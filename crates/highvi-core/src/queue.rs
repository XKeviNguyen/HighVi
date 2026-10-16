//! The download queue: many sources, at most [`MAX_ACTIVE_JOBS`] jobs at
//! once, with a target the user can change while jobs run.
//!
//! One mutex-protected [`State`] is the only source of truth. It is never
//! held across an await: every method takes the lock, changes the state,
//! decides which jobs to launch, and releases it before spawning anything or
//! emitting events. Events still leave in state order: a second lock, taken
//! before the state lock is released, orders emissions. Each launch carries `(id, attempt)`; whatever arrives
//! from an older attempt is ignored, and every launch frees its slot exactly
//! once, even when the job panics.

use crate::{
    Discovery, JobOutcome, JobUpdate, PublicError, SourceKind, SourceRequest, Stage,
    ValidatedSource,
    process::{CancelHandle, CancelToken, cancel_pair},
    validate_source,
};
use serde::Serialize;
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};
use tokio::sync::watch;

/// Hard limit on video jobs running at once, whatever the UI asks for. A job
/// counts from discovery to cleanup; its video and audio tracks are one job.
pub const MAX_ACTIVE_JOBS: usize = 4;
pub const DEFAULT_CONCURRENCY: usize = 2;
/// Items kept in the queue, finished ones included, to bound memory.
pub const MAX_QUEUE_ITEMS: usize = 200;
/// Sources accepted in one request.
pub const MAX_BATCH: usize = 100;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ItemStatus {
    Queued,
    Running,
    /// Cancel was requested; the job is stopping its tools.
    Cancelling,
    Completed,
    Failed,
    Cancelled,
}

impl ItemStatus {
    fn is_active(self) -> bool {
        matches!(self, Self::Running | Self::Cancelling)
    }
}

/// What a job runner returns: the outcome and, for failures, a report.
pub type JobResult = (Result<JobOutcome, PublicError>, Option<String>);
pub type JobFuture = Pin<Box<dyn Future<Output = JobResult> + Send>>;
pub type Report = Arc<dyn Fn(JobUpdate) + Send + Sync>;
/// Starts one job. Production wraps [`crate::run_job`]; tests use fakes.
pub type Runner =
    Arc<dyn Fn(ValidatedSource, Discovery, CancelToken, Report) -> JobFuture + Send + Sync>;

/// Where the queue reports changes, in the order they happened. Called
/// without the queue's state lock held, but with its emit lock: an
/// implementation must not call back into the [`Scheduler`].
pub trait QueueEvents: Send + Sync {
    fn changed(&self, snapshot: QueueSnapshot);
    fn progress(&self, progress: ProgressEvent);
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressEvent {
    pub id: u64,
    pub attempt: u32,
    #[serde(flatten)]
    pub update: JobUpdate,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemView {
    pub id: u64,
    pub attempt: u32,
    pub status: ItemStatus,
    pub kind: &'static str,
    /// The site's host name, until the title is known.
    pub host: String,
    pub stage: Option<Stage>,
    pub title: Option<String>,
    pub percent: Option<String>,
    pub speed: Option<String>,
    pub eta: Option<String>,
    pub error: Option<PublicError>,
    pub has_diagnostics: bool,
    pub saved_to: Option<String>,
    /// This attempt uses a visible HighVi browser window.
    pub interactive: bool,
    /// A visible HighVi browser window may get past this failure.
    pub can_open_in_browser: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Counts {
    pub queued: usize,
    pub running: usize,
    pub completed: usize,
    pub failed: usize,
    pub cancelled: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueSnapshot {
    /// Increases with every change, so a late snapshot can be discarded.
    pub revision: u64,
    pub desired: usize,
    pub max: usize,
    /// Jobs holding a slot, which may exceed `desired` after it was lowered.
    pub active: usize,
    pub started: bool,
    /// Why the queue paused itself, e.g. `missing_dependency`.
    pub paused_by: Option<String>,
    pub counts: Counts,
    pub items: Vec<ItemView>,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AddReport {
    pub added: Vec<u64>,
    /// `(index in the request, error)` for sources that were not valid.
    pub rejected: Vec<(usize, PublicError)>,
    /// Indexes of sources already in the queue (or repeated in the request).
    pub duplicates: Vec<usize>,
}

struct Item {
    id: u64,
    attempt: u32,
    source: ValidatedSource,
    discovery: Discovery,
    /// Identity for duplicate detection: the full URL(s), query included.
    key: String,
    host: String,
    status: ItemStatus,
    stage: Option<Stage>,
    title: Option<String>,
    progress: Option<JobUpdate>,
    error: Option<PublicError>,
    diagnostics: Option<String>,
    saved_to: Option<String>,
    cancel: Option<CancelHandle>,
}

impl Item {
    fn view(&self) -> ItemView {
        let progress = self.progress.as_ref();
        ItemView {
            id: self.id,
            attempt: self.attempt,
            status: self.status,
            kind: match self.source.kind() {
                SourceKind::Page => "page",
                SourceKind::BarePlaylist => "bare",
                SourceKind::DirectHls => "direct",
            },
            host: self.host.clone(),
            stage: self.stage,
            title: self.title.clone(),
            percent: progress.and_then(|update| update.percent.clone()),
            speed: progress.and_then(|update| update.speed.clone()),
            eta: progress.and_then(|update| update.eta.clone()),
            error: self.error.clone(),
            has_diagnostics: self.diagnostics.is_some(),
            saved_to: self.saved_to.clone(),
            interactive: self.discovery == Discovery::Interactive,
            can_open_in_browser: self.can_open_in_browser(),
        }
    }

    /// Only for pages, after an attempt ended without a video that a
    /// visible window might find (or the user cancelled).
    fn can_open_in_browser(&self) -> bool {
        self.source.kind() == SourceKind::Page
            && match self.status {
                ItemStatus::Cancelled => true,
                ItemStatus::Failed => self
                    .error
                    .as_ref()
                    .is_some_and(|error| crate::job::browser_can_help(&error.code)),
                _ => false,
            }
    }

    /// Back to the queue as a fresh attempt.
    fn requeue(&mut self) {
        self.attempt += 1;
        self.status = ItemStatus::Queued;
        self.stage = None;
        self.progress = None;
        self.error = None;
        self.diagnostics = None;
        self.saved_to = None;
        self.cancel = None;
    }
}

struct State {
    items: Vec<Item>,
    desired: usize,
    started: bool,
    accepting: bool,
    paused_by: Option<String>,
    /// Launches that have not finished. Changed only by `launch_ready` and
    /// `finish`, never by item edits.
    active: usize,
    next_id: u64,
    revision: u64,
}

impl State {
    fn snapshot(&mut self) -> QueueSnapshot {
        self.revision += 1;
        let mut counts = Counts::default();
        for item in &self.items {
            match item.status {
                ItemStatus::Queued => counts.queued += 1,
                ItemStatus::Running | ItemStatus::Cancelling => counts.running += 1,
                ItemStatus::Completed => counts.completed += 1,
                ItemStatus::Failed => counts.failed += 1,
                ItemStatus::Cancelled => counts.cancelled += 1,
            }
        }
        QueueSnapshot {
            revision: self.revision,
            desired: self.desired,
            max: MAX_ACTIVE_JOBS,
            active: self.active,
            started: self.started,
            paused_by: self.paused_by.clone(),
            counts,
            items: self.items.iter().map(Item::view).collect(),
        }
    }

    fn item(&mut self, id: u64) -> Option<&mut Item> {
        self.items.iter_mut().find(|item| item.id == id)
    }

    /// Marks the next queued items running while a slot is free. FIFO: the
    /// oldest queued item always goes first (a retried item keeps its place
    /// in the list), and finished or failed items never hold a place.
    fn launch_ready(&mut self) -> Vec<Launch> {
        let mut launches = Vec::new();
        let limit = self.desired.min(MAX_ACTIVE_JOBS);
        while self.accepting && self.started && self.active < limit {
            let Some(item) = self
                .items
                .iter_mut()
                .find(|item| item.status == ItemStatus::Queued)
            else {
                break;
            };
            let (handle, token) = cancel_pair();
            item.status = ItemStatus::Running;
            item.stage = Some(Stage::Discovering);
            item.cancel = Some(handle);
            self.active += 1;
            launches.push(Launch {
                id: item.id,
                attempt: item.attempt,
                source: item.source.clone(),
                discovery: item.discovery,
                token,
            });
        }
        debug_assert!(self.active <= MAX_ACTIVE_JOBS);
        launches
    }
}

struct Launch {
    id: u64,
    attempt: u32,
    source: ValidatedSource,
    discovery: Discovery,
    token: CancelToken,
}

struct Inner {
    state: Mutex<State>,
    runtime: tokio::runtime::Handle,
    runner: Runner,
    events: Arc<dyn QueueEvents>,
    /// Held while emitting; taken before the state lock is released, so
    /// events leave in the order the state changed. Never taken first.
    emitting: Mutex<()>,
    /// Mirrors `State::active`, so shutdown can wait for zero.
    active: watch::Sender<usize>,
}

/// The queue. Cheap to clone; all clones share one state.
#[derive(Clone)]
pub struct Scheduler {
    inner: Arc<Inner>,
}

impl Scheduler {
    /// Jobs are spawned on `runtime`, so methods may be called from any
    /// thread.
    pub fn new(
        runtime: tokio::runtime::Handle,
        runner: Runner,
        events: Arc<dyn QueueEvents>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    items: Vec::new(),
                    desired: DEFAULT_CONCURRENCY,
                    started: false,
                    accepting: true,
                    paused_by: None,
                    active: 0,
                    next_id: 1,
                    revision: 0,
                }),
                runtime,
                runner,
                events,
                emitting: Mutex::new(()),
                active: watch::channel(0).0,
            }),
        }
    }

    /// Validates each source on its own: an invalid or duplicate one never
    /// stops the others.
    pub fn add(&self, sources: Vec<SourceRequest>) -> AddReport {
        let mut report = AddReport::default();
        let validated: Vec<_> = sources
            .iter()
            .take(MAX_BATCH)
            .map(validate_source)
            .collect();
        self.update(|state| {
            for (index, source) in validated.into_iter().enumerate() {
                let source = match source {
                    Ok(source) => source,
                    Err(error) => {
                        report.rejected.push((index, error));
                        continue;
                    }
                };
                let key = identity(&source);
                if state.items.iter().any(|item| item.key == key) {
                    report.duplicates.push(index);
                    continue;
                }
                if !state.accepting {
                    report
                        .rejected
                        .push((index, PublicError::new("queue_closed", "HighVi is closing.")));
                    continue;
                }
                if state.items.len() >= MAX_QUEUE_ITEMS {
                    report.rejected.push((
                        index,
                        PublicError::new(
                            "queue_full",
                            format!("The queue holds at most {MAX_QUEUE_ITEMS} videos. Clear finished ones first."),
                        ),
                    ));
                    continue;
                }
                let id = state.next_id;
                state.next_id += 1;
                state.items.push(Item {
                    id,
                    attempt: 1,
                    host: host(&source),
                    source,
                    discovery: Discovery::Automatic,
                    key,
                    status: ItemStatus::Queued,
                    stage: None,
                    title: None,
                    progress: None,
                    error: None,
                    diagnostics: None,
                    saved_to: None,
                    cancel: None,
                });
                report.added.push(id);
            }
        });
        for index in MAX_BATCH..sources.len() {
            report.rejected.push((
                index,
                PublicError::new(
                    "batch_too_large",
                    format!("Add at most {MAX_BATCH} links at a time."),
                ),
            ));
        }
        report
    }

    /// Starts (or resumes) launching queued jobs.
    pub fn start(&self) {
        self.update(|state| {
            state.started = true;
            state.paused_by = None;
        });
    }

    /// Stops launching new jobs. Running jobs continue.
    pub fn pause(&self) {
        self.update(|state| state.started = false);
    }

    /// Sets how many jobs may run at once (1 to [`MAX_ACTIVE_JOBS`]).
    /// Raising it starts queued jobs now; lowering it lets running jobs
    /// finish and starts nothing until fewer than `desired` remain.
    pub fn set_concurrency(&self, desired: usize) -> Result<(), PublicError> {
        if !(1..=MAX_ACTIVE_JOBS).contains(&desired) {
            return Err(PublicError::new(
                "invalid_concurrency",
                format!("Choose between 1 and {MAX_ACTIVE_JOBS} downloads at once."),
            ));
        }
        self.update(|state| state.desired = desired);
        Ok(())
    }

    /// Cancels one item. A queued item is cancelled at once; a running one
    /// is asked to stop and reports `Cancelled` when its tools are gone.
    /// Finished items are left alone. Idempotent.
    pub fn cancel(&self, id: u64) {
        self.update(|state| {
            if let Some(item) = state.item(id) {
                cancel_item(item);
            }
        });
    }

    /// Cancels everything queued or running. Completed files are kept.
    pub fn cancel_all(&self) {
        self.update(|state| state.items.iter_mut().for_each(cancel_item));
    }

    /// Queues a failed or cancelled item again as a new automatic attempt.
    pub fn retry(&self, id: u64) {
        self.update(|state| {
            if let Some(item) = state.item(id)
                && matches!(item.status, ItemStatus::Failed | ItemStatus::Cancelled)
            {
                item.discovery = Discovery::Automatic;
                item.requeue();
            }
        });
    }

    pub fn retry_failed(&self) {
        self.update(|state| {
            for item in &mut state.items {
                if item.status == ItemStatus::Failed {
                    item.discovery = Discovery::Automatic;
                    item.requeue();
                }
            }
        });
    }

    /// Queues a page again for a visible HighVi browser window, when its
    /// last attempt ended in a way such a window might get past. The window
    /// opens when the item's turn comes and the browser is free.
    pub fn open_in_browser(&self, id: u64) {
        self.update(|state| {
            if let Some(item) = state.item(id)
                && item.can_open_in_browser()
            {
                item.discovery = Discovery::Interactive;
                item.requeue();
            }
        });
    }

    /// Removes an item that is not running.
    pub fn remove(&self, id: u64) {
        self.update(|state| {
            state
                .items
                .retain(|item| item.id != id || item.status.is_active());
        });
    }

    pub fn clear_completed(&self) {
        self.update(|state| {
            state
                .items
                .retain(|item| item.status != ItemStatus::Completed);
        });
    }

    pub fn snapshot(&self) -> QueueSnapshot {
        self.lock().snapshot()
    }

    /// The diagnostic report of a failed item's last attempt.
    pub fn diagnostics(&self, id: u64) -> Option<String> {
        self.lock()
            .item(id)
            .and_then(|item| item.diagnostics.clone())
    }

    /// Stops accepting and launching, cancels everything, and resolves once
    /// no job holds a slot. Callers bound the wait.
    pub async fn shutdown(&self) {
        self.update(|state| {
            state.accepting = false;
            state.started = false;
            state.items.iter_mut().for_each(cancel_item);
        });
        let mut active = self.inner.active.subscribe();
        let _ = active.wait_for(|active| *active == 0).await;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Applies `change`, launches what the new state allows, and reports the
    /// result — all decisions under the lock, all effects after it.
    fn update(&self, change: impl FnOnce(&mut State)) {
        let (launches, snapshot, emitting) = {
            let mut state = self.lock();
            change(&mut state);
            let launches = state.launch_ready();
            self.inner.active.send_replace(state.active);
            (launches, state.snapshot(), self.emit_lock())
        };
        for launch in launches {
            self.inner.runtime.spawn(run_launch(self.clone(), launch));
        }
        self.inner.events.changed(snapshot);
        drop(emitting);
    }

    fn emit_lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.inner
            .emitting
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn progress(&self, id: u64, attempt: u32, update: JobUpdate) {
        let emitting = {
            let mut state = self.lock();
            match state.item(id) {
                Some(item) if item.attempt == attempt && item.status.is_active() => {
                    item.stage = Some(update.stage);
                    if let Some(title) = &update.title {
                        item.title = Some(title.clone());
                    }
                    item.progress = Some(update.clone());
                    Some(self.emit_lock())
                }
                _ => None,
            }
        };
        if emitting.is_some() {
            self.inner.events.progress(ProgressEvent {
                id,
                attempt,
                update,
            });
        }
    }

    /// Records a launch's end and frees its slot. Runs exactly once per
    /// launch.
    fn finish(&self, id: u64, attempt: u32, result: JobResult) {
        let (outcome, diagnostics) = result;
        self.update(|state| {
            state.active -= 1;
            let mut doomed = None;
            if let Some(item) = state.item(id)
                && item.attempt == attempt
            {
                // Failures every other queued job would repeat. A stale
                // attempt's result never pauses the queue.
                if let Err(error) = &outcome
                    && matches!(error.code.as_str(), "missing_dependency" | "ytdlp_too_old")
                {
                    doomed = Some(error.code.clone());
                }
                let was_cancelled = item.status == ItemStatus::Cancelling;
                item.cancel = None;
                item.stage = None;
                item.progress = None;
                match outcome {
                    Ok(outcome) => {
                        // Published before the cancel took effect: the file
                        // exists, so say so.
                        item.status = ItemStatus::Completed;
                        item.title = Some(outcome.title);
                        item.saved_to = Some(outcome.saved_to.to_string_lossy().into_owned());
                    }
                    Err(error) if was_cancelled || error.code == "cancelled" => {
                        item.status = ItemStatus::Cancelled;
                    }
                    Err(error) => {
                        item.status = ItemStatus::Failed;
                        item.error = Some(error);
                        item.diagnostics = diagnostics;
                    }
                }
            }
            if let Some(code) = doomed {
                state.started = false;
                state.paused_by = Some(code);
            }
        });
    }
}

fn cancel_item(item: &mut Item) {
    match item.status {
        ItemStatus::Queued => item.status = ItemStatus::Cancelled,
        ItemStatus::Running => {
            item.status = ItemStatus::Cancelling;
            if let Some(handle) = &item.cancel {
                handle.cancel();
            }
        }
        _ => {}
    }
}

/// Runs one launch in its own task, so a panicking job becomes a failed
/// item instead of a lost slot.
async fn run_launch(scheduler: Scheduler, launch: Launch) {
    let Launch {
        id,
        attempt,
        source,
        discovery,
        token,
    } = launch;
    let reporter = scheduler.clone();
    let report: Report = Arc::new(move |update| reporter.progress(id, attempt, update));
    // The runner is called inside the task too, so even a runner that
    // panics before returning its future cannot keep the slot.
    let runner = Arc::clone(&scheduler.inner.runner);
    let job = async move { runner(source, discovery, token, report).await };
    let result = match scheduler.inner.runtime.spawn(job).await {
        Ok(result) => result,
        Err(_) => (
            Err(PublicError::new(
                "internal_error",
                "This download stopped because of an internal error in HighVi.",
            )),
            None,
        ),
    };
    scheduler.finish(id, attempt, result);
}

/// The full URL(s): two links differing only in their query are different
/// videos, and must not be merged.
fn identity(source: &ValidatedSource) -> String {
    match source {
        ValidatedSource::Page { page } => page.as_str().to_string(),
        ValidatedSource::DirectHls {
            playlist,
            referer_page,
        } => format!(
            "{} {}",
            playlist.as_str(),
            crate::referer_origin(referer_page)
        ),
    }
}

fn host(source: &ValidatedSource) -> String {
    let url = match source {
        ValidatedSource::Page { page } => page,
        ValidatedSource::DirectHls { playlist, .. } => playlist,
    };
    url.host_str()
        .unwrap_or_default()
        .chars()
        .take(253)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::HashMap,
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };
    use tokio::sync::oneshot;

    /// How a fake job ends.
    enum End {
        Done,
        Fail(&'static str),
        Panic,
    }

    /// Fake jobs that run until the test ends them (or they are cancelled),
    /// recording how many ran at once.
    #[derive(Default)]
    struct Fake {
        running: AtomicUsize,
        peak: AtomicUsize,
        started: AtomicUsize,
        enders: Mutex<HashMap<String, oneshot::Sender<End>>>,
        reports: Mutex<Vec<Report>>,
        /// `(key, mode)` of every launch, in order.
        modes: Mutex<Vec<(String, Discovery)>>,
    }

    #[derive(Default)]
    struct Recorder {
        snapshots: Mutex<Vec<QueueSnapshot>>,
        progress: Mutex<Vec<ProgressEvent>>,
    }

    impl QueueEvents for Recorder {
        fn changed(&self, snapshot: QueueSnapshot) {
            assert!(snapshot.active <= MAX_ACTIVE_JOBS);
            self.snapshots.lock().unwrap().push(snapshot);
        }
        fn progress(&self, progress: ProgressEvent) {
            self.progress.lock().unwrap().push(progress);
        }
    }

    fn setup() -> (Scheduler, Arc<Fake>, Arc<Recorder>) {
        let fake = Arc::new(Fake::default());
        let recorder = Arc::new(Recorder::default());
        let runner_fake = Arc::clone(&fake);
        let runner: Runner = Arc::new(move |source, discovery, mut cancel, report| {
            let fake = Arc::clone(&runner_fake);
            let (sender, receiver) = oneshot::channel();
            let key = source.discovery_target().path().to_string();
            fake.modes.lock().unwrap().push((key.clone(), discovery));
            fake.enders.lock().unwrap().insert(key.clone(), sender);
            fake.reports.lock().unwrap().push(Arc::clone(&report));
            let now = fake.running.fetch_add(1, Ordering::SeqCst) + 1;
            fake.peak.fetch_max(now, Ordering::SeqCst);
            fake.started.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                let end = tokio::select! {
                    end = receiver => end.unwrap_or(End::Done),
                    () = cancel.cancelled() => End::Fail("cancelled"),
                };
                fake.enders.lock().unwrap().remove(&key);
                fake.running.fetch_sub(1, Ordering::SeqCst);
                match end {
                    End::Done => (
                        Ok(JobOutcome {
                            saved_to: "/out/v.mp4".into(),
                            title: "Video".into(),
                        }),
                        None,
                    ),
                    End::Fail(code) => {
                        (Err(PublicError::new(code, "x")), Some("report".to_string()))
                    }
                    End::Panic => panic!("job bug"),
                }
            }) as JobFuture
        });
        (
            Scheduler::new(
                tokio::runtime::Handle::current(),
                runner,
                Arc::clone(&recorder) as Arc<dyn QueueEvents>,
            ),
            fake,
            recorder,
        )
    }

    fn pages(range: std::ops::Range<usize>) -> Vec<SourceRequest> {
        range
            .map(|index| SourceRequest::Page {
                page_url: format!("https://site.example/v{index}"),
            })
            .collect()
    }

    /// Ends the fake job for `/v{index}`, once it has reached the runner.
    async fn end(fake: &Fake, index: usize, how: End) {
        let key = format!("/v{index}");
        for _ in 0..500 {
            if let Some(sender) = fake.enders.lock().unwrap().remove(&key) {
                let _ = sender.send(how);
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("job v{index} never started");
    }

    /// Waits until the queue reaches a state, without fixed sleeps.
    async fn until(scheduler: &Scheduler, check: impl Fn(&QueueSnapshot) -> bool) {
        for _ in 0..500 {
            if check(&scheduler.snapshot()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("queue never reached the state: {:?}", scheduler.snapshot());
    }

    fn status(snapshot: &QueueSnapshot, id: u64) -> ItemStatus {
        snapshot
            .items
            .iter()
            .find(|item| item.id == id)
            .unwrap()
            .status
    }

    /// The fake jobs running now, once every launch the queue counts has
    /// reached its runner (launches start in their own tasks).
    async fn running_ids(scheduler: &Scheduler, fake: &Fake) -> Vec<String> {
        for _ in 0..500 {
            let active = scheduler.snapshot().active;
            let keys: Vec<String> = fake.enders.lock().unwrap().keys().cloned().collect();
            if keys.len() == active && fake.running.load(Ordering::SeqCst) == active {
                let mut keys = keys;
                keys.sort();
                return keys;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!(
            "launches never reached the runner: {:?}",
            scheduler.snapshot()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn never_more_than_the_desired_number_and_never_more_than_four() {
        for desired in 1..=MAX_ACTIVE_JOBS {
            let (scheduler, fake, _) = setup();
            scheduler.set_concurrency(desired).unwrap();
            scheduler.add(pages(0..10));
            scheduler.start();
            for finished in 0..10 {
                until(&scheduler, |snapshot| {
                    snapshot.active == desired.min(10 - finished)
                })
                .await;
                let running = running_ids(&scheduler, &fake).await;
                assert_eq!(running.len(), desired.min(10 - finished));
                let next = running[0].trim_start_matches("/v").parse().unwrap();
                end(&fake, next, End::Done).await;
            }
            until(&scheduler, |snapshot| snapshot.counts.completed == 10).await;
            assert_eq!(fake.peak.load(Ordering::SeqCst), desired);
            assert_eq!(fake.started.load(Ordering::SeqCst), 10);
        }
        let (scheduler, _, _) = setup();
        for invalid in [0, 5, 100] {
            assert_eq!(
                scheduler.set_concurrency(invalid).unwrap_err().code,
                "invalid_concurrency"
            );
        }
        assert_eq!(scheduler.snapshot().desired, DEFAULT_CONCURRENCY);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn queued_jobs_wait_for_start_and_run_in_order() {
        let (scheduler, fake, _) = setup();
        scheduler.add(pages(0..5));
        tokio::task::yield_now().await;
        assert_eq!(scheduler.snapshot().active, 0, "nothing runs before Start");
        scheduler.start();
        until(&scheduler, |snapshot| snapshot.active == 2).await;
        assert_eq!(running_ids(&scheduler, &fake).await, ["/v0", "/v1"]);
        end(&fake, 1, End::Done).await;
        until(&scheduler, |snapshot| {
            snapshot.counts.completed == 1 && snapshot.active == 2
        })
        .await;
        assert_eq!(running_ids(&scheduler, &fake).await, ["/v0", "/v2"]);
        // Pause: running jobs continue, nothing new starts.
        scheduler.pause();
        end(&fake, 0, End::Done).await;
        until(&scheduler, |snapshot| snapshot.counts.completed == 2).await;
        assert_eq!(running_ids(&scheduler, &fake).await, ["/v2"]);
        scheduler.start();
        until(&scheduler, |snapshot| snapshot.active == 2).await;
        assert_eq!(running_ids(&scheduler, &fake).await, ["/v2", "/v3"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn raising_the_target_starts_more_without_touching_running_jobs() {
        let (scheduler, fake, _) = setup();
        scheduler.add(pages(0..8));
        scheduler.start();
        until(&scheduler, |snapshot| snapshot.active == 2).await;
        scheduler.set_concurrency(4).unwrap();
        until(&scheduler, |snapshot| snapshot.active == 4).await;
        assert_eq!(
            running_ids(&scheduler, &fake).await,
            ["/v0", "/v1", "/v2", "/v3"]
        );
        assert_eq!(
            fake.started.load(Ordering::SeqCst),
            4,
            "a running job was restarted"
        );
        let snapshot = scheduler.snapshot();
        assert!(snapshot.items.iter().all(|item| item.attempt == 1));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn lowering_the_target_drains_instead_of_cancelling() {
        let (scheduler, fake, _) = setup();
        scheduler.set_concurrency(4).unwrap();
        scheduler.add(pages(0..8));
        scheduler.start();
        until(&scheduler, |snapshot| snapshot.active == 4).await;
        scheduler.set_concurrency(2).unwrap();
        let snapshot = scheduler.snapshot();
        assert_eq!((snapshot.active, snapshot.desired), (4, 2), "draining");
        assert_eq!(snapshot.counts.running, 4, "nothing was cancelled");
        assert_eq!(running_ids(&scheduler, &fake).await.len(), 4);
        // 4 → 3 → 2 running: no replacement starts while above the target.
        end(&fake, 0, End::Done).await;
        until(&scheduler, |snapshot| snapshot.counts.completed == 1).await;
        end(&fake, 1, End::Done).await;
        until(&scheduler, |snapshot| snapshot.counts.completed == 2).await;
        tokio::task::yield_now().await;
        assert_eq!(running_ids(&scheduler, &fake).await, ["/v2", "/v3"]);
        // Below the target again: one starts.
        end(&fake, 2, End::Done).await;
        until(&scheduler, |snapshot| {
            snapshot.counts.completed == 3 && snapshot.active == 2
        })
        .await;
        assert_eq!(running_ids(&scheduler, &fake).await, ["/v3", "/v4"]);
        assert_eq!(fake.peak.load(Ordering::SeqCst), 4);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn rapid_target_changes_keep_the_invariants() {
        let (scheduler, fake, recorder) = setup();
        scheduler.add(pages(0..40));
        scheduler.start();
        for round in 0..200 {
            scheduler
                .set_concurrency(1 + round % MAX_ACTIVE_JOBS)
                .unwrap();
            if round % 7 == 0
                && let Some(first) = running_ids(&scheduler, &fake).await.first().cloned()
            {
                end(
                    &fake,
                    first.trim_start_matches("/v").parse().unwrap(),
                    End::Done,
                )
                .await;
            }
            tokio::task::yield_now().await;
        }
        scheduler.set_concurrency(4).unwrap();
        while scheduler.snapshot().counts.completed < 40 {
            if let Some(first) = running_ids(&scheduler, &fake).await.first().cloned() {
                end(
                    &fake,
                    first.trim_start_matches("/v").parse().unwrap(),
                    End::Done,
                )
                .await;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(fake.peak.load(Ordering::SeqCst) <= MAX_ACTIVE_JOBS);
        assert_eq!(
            fake.started.load(Ordering::SeqCst),
            40,
            "a job ran twice or never"
        );
        let snapshots = recorder.snapshots.lock().unwrap();
        assert!(
            snapshots
                .iter()
                .all(|snapshot| snapshot.active <= MAX_ACTIVE_JOBS)
        );
        assert!(
            snapshots
                .windows(2)
                .all(|pair| pair[0].revision < pair[1].revision)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancelling_queued_running_and_all_is_idempotent() {
        let (scheduler, fake, _) = setup();
        let ids = scheduler.add(pages(0..6)).added;
        scheduler.start();
        until(&scheduler, |snapshot| snapshot.active == 2).await;
        // A queued item is cancelled at once and never starts.
        scheduler.cancel(ids[5]);
        scheduler.cancel(ids[5]);
        assert_eq!(status(&scheduler.snapshot(), ids[5]), ItemStatus::Cancelled);
        // A running item stops its job; its slot goes to the next item.
        scheduler.cancel(ids[0]);
        scheduler.cancel(ids[0]);
        until(&scheduler, |snapshot| {
            status(snapshot, ids[0]) == ItemStatus::Cancelled
        })
        .await;
        until(&scheduler, |snapshot| snapshot.active == 2).await;
        assert_eq!(running_ids(&scheduler, &fake).await, ["/v1", "/v2"]);
        end(&fake, 1, End::Done).await;
        until(&scheduler, |snapshot| {
            status(snapshot, ids[1]) == ItemStatus::Completed
        })
        .await;
        // Cancel All: everything else stops; the completed item stays.
        scheduler.cancel_all();
        scheduler.cancel_all();
        until(&scheduler, |snapshot| snapshot.active == 0).await;
        let snapshot = scheduler.snapshot();
        assert_eq!(status(&snapshot, ids[1]), ItemStatus::Completed);
        assert_eq!(snapshot.counts.cancelled, 5);
        assert_eq!(fake.started.load(Ordering::SeqCst), 4);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn failures_do_not_stop_the_queue_and_retry_is_a_new_attempt() {
        let (scheduler, fake, recorder) = setup();
        let ids = scheduler.add(pages(0..3)).added;
        scheduler.start();
        until(&scheduler, |snapshot| snapshot.active == 2).await;
        running_ids(&scheduler, &fake).await;
        let old_report = Arc::clone(&fake.reports.lock().unwrap()[0]);
        end(&fake, 0, End::Fail("access_denied")).await;
        until(&scheduler, |snapshot| {
            status(snapshot, ids[0]) == ItemStatus::Failed
        })
        .await;
        until(&scheduler, |snapshot| snapshot.active == 2).await;
        assert_eq!(running_ids(&scheduler, &fake).await, ["/v1", "/v2"]);
        assert_eq!(scheduler.diagnostics(ids[0]).as_deref(), Some("report"));
        let failed = scheduler.snapshot();
        let item = failed.items.iter().find(|item| item.id == ids[0]).unwrap();
        assert_eq!(item.error.as_ref().unwrap().code, "access_denied");
        assert!(item.has_diagnostics);

        scheduler.retry(ids[0]);
        let retried = scheduler.snapshot();
        let item = retried.items.iter().find(|item| item.id == ids[0]).unwrap();
        assert_eq!((item.status, item.attempt), (ItemStatus::Queued, 2));
        assert!(item.error.is_none() && !item.has_diagnostics);
        end(&fake, 1, End::Done).await;
        until(&scheduler, |snapshot| {
            status(snapshot, ids[0]) == ItemStatus::Running
        })
        .await;
        // A late update from attempt 1 cannot touch attempt 2.
        old_report(JobUpdate {
            stage: Stage::Merging,
            title: Some("stale".into()),
            percent: Some("99%".into()),
            speed: None,
            eta: None,
        });
        let snapshot = scheduler.snapshot();
        let item = snapshot
            .items
            .iter()
            .find(|item| item.id == ids[0])
            .unwrap();
        assert_ne!(item.title.as_deref(), Some("stale"));
        assert!(
            recorder
                .progress
                .lock()
                .unwrap()
                .iter()
                .all(|event| event.attempt != 1
                    || event.id != ids[0]
                    || event.update.title.as_deref() != Some("stale"))
        );
        // Retrying something that is not failed or cancelled does nothing.
        scheduler.retry(ids[1]);
        assert_eq!(status(&scheduler.snapshot(), ids[1]), ItemStatus::Completed);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_panicking_job_fails_alone_and_frees_its_slot() {
        let (scheduler, fake, _) = setup();
        let ids = scheduler.add(pages(0..3)).added;
        scheduler.set_concurrency(1).unwrap();
        scheduler.start();
        until(&scheduler, |snapshot| snapshot.active == 1).await;
        end(&fake, 0, End::Panic).await;
        until(&scheduler, |snapshot| {
            status(snapshot, ids[0]) == ItemStatus::Failed
        })
        .await;
        let snapshot = scheduler.snapshot();
        let item = snapshot
            .items
            .iter()
            .find(|item| item.id == ids[0])
            .unwrap();
        assert_eq!(item.error.as_ref().unwrap().code, "internal_error");
        until(&scheduler, |snapshot| snapshot.active == 1).await;
        assert_eq!(running_ids(&scheduler, &fake).await, ["/v1"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_runner_that_panics_before_returning_frees_its_slot() {
        let recorder = Arc::new(Recorder::default());
        let runner: Runner = Arc::new(|source, _discovery, _cancel, _report| {
            assert!(source.discovery_target().path() != "/v0", "runner bug");
            Box::pin(async {
                (
                    Ok(JobOutcome {
                        saved_to: "/out/v.mp4".into(),
                        title: "Video".into(),
                    }),
                    None,
                )
            }) as JobFuture
        });
        let scheduler = Scheduler::new(
            tokio::runtime::Handle::current(),
            runner,
            Arc::clone(&recorder) as Arc<dyn QueueEvents>,
        );
        scheduler.set_concurrency(1).unwrap();
        let ids = scheduler.add(pages(0..2)).added;
        scheduler.start();
        until(&scheduler, |snapshot| snapshot.counts.completed == 1).await;
        let snapshot = scheduler.snapshot();
        let failed = snapshot
            .items
            .iter()
            .find(|item| item.id == ids[0])
            .unwrap();
        assert_eq!(failed.status, ItemStatus::Failed);
        assert_eq!(failed.error.as_ref().unwrap().code, "internal_error");
        assert_eq!(snapshot.active, 0);
        tokio::time::timeout(Duration::from_secs(5), scheduler.shutdown())
            .await
            .expect("a lost slot blocked shutdown");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_too_old_yt_dlp_pauses_the_queue_like_a_missing_tool() {
        let (scheduler, fake, _) = setup();
        scheduler.set_concurrency(1).unwrap();
        scheduler.add(pages(0..3));
        scheduler.start();
        until(&scheduler, |snapshot| snapshot.active == 1).await;
        end(&fake, 0, End::Fail("ytdlp_too_old")).await;
        until(&scheduler, |snapshot| !snapshot.started).await;
        assert_eq!(
            scheduler.snapshot().paused_by.as_deref(),
            Some("ytdlp_too_old")
        );
        assert_eq!(fake.started.load(Ordering::SeqCst), 1);
    }

    /// Waits until the last launch was `key` in `mode`.
    async fn launched(fake: &Fake, key: &str, mode: Discovery) {
        for _ in 0..500 {
            if fake.modes.lock().unwrap().last() == Some(&(key.to_string(), mode)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!(
            "{key} never launched as {mode:?}: {:?}",
            fake.modes.lock().unwrap()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn only_pages_a_window_might_help_can_open_in_the_highvi_browser() {
        let (scheduler, fake, _) = setup();
        scheduler.set_concurrency(1).unwrap();
        let mut sources = pages(0..2);
        sources.push(SourceRequest::DirectHls {
            playlist_url: "https://cdn.example/v2.m3u8".into(),
            referer_page_url: "https://site.example/watch".into(),
        });
        let added = scheduler.add(sources);
        assert!(added.rejected.is_empty(), "{:?}", added.rejected);
        let ids = added.added;
        scheduler.start();
        end(&fake, 0, End::Fail("host_firewall_block")).await;
        end(&fake, 1, End::Fail("rate_limited")).await;
        let direct = loop {
            if let Some(sender) = fake.enders.lock().unwrap().remove("/v2.m3u8") {
                break sender;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        };
        let _ = direct.send(End::Fail("host_firewall_block"));
        until(&scheduler, |snapshot| snapshot.counts.failed == 3).await;
        let view = |id| {
            scheduler
                .snapshot()
                .items
                .into_iter()
                .find(|item| item.id == id)
                .unwrap()
        };
        // A refused page: yes. Rate limiting, or a direct playlist: no.
        assert!(view(ids[0]).can_open_in_browser);
        assert!(!view(ids[1]).can_open_in_browser);
        assert!(!view(ids[2]).can_open_in_browser);
        scheduler.open_in_browser(ids[1]);
        scheduler.open_in_browser(ids[2]);
        assert_eq!(scheduler.snapshot().counts.failed, 3, "ignored");

        scheduler.open_in_browser(ids[0]);
        until(&scheduler, |snapshot| snapshot.counts.running == 1).await;
        let opened = view(ids[0]);
        assert!(opened.interactive && opened.status == ItemStatus::Running);
        assert_eq!(opened.attempt, 2);
        launched(&fake, "/v0", Discovery::Interactive).await;
        // Closed by the user: it may be opened again. A plain retry is
        // automatic again.
        end(&fake, 0, End::Fail("browser_closed")).await;
        until(&scheduler, |snapshot| snapshot.counts.failed == 3).await;
        assert!(view(ids[0]).can_open_in_browser);
        scheduler.retry(ids[0]);
        until(&scheduler, |snapshot| snapshot.counts.running == 1).await;
        assert!(!view(ids[0]).interactive);
        launched(&fake, "/v0", Discovery::Automatic).await;
        end(&fake, 0, End::Done).await;
        until(&scheduler, |snapshot| snapshot.counts.completed == 1).await;
        // Finished items cannot be reopened.
        assert!(!view(ids[0]).can_open_in_browser);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_missing_tool_pauses_the_queue() {
        let (scheduler, fake, _) = setup();
        scheduler.set_concurrency(1).unwrap();
        scheduler.add(pages(0..4));
        scheduler.start();
        until(&scheduler, |snapshot| snapshot.active == 1).await;
        end(&fake, 0, End::Fail("missing_dependency")).await;
        until(&scheduler, |snapshot| !snapshot.started).await;
        let snapshot = scheduler.snapshot();
        assert_eq!(snapshot.paused_by.as_deref(), Some("missing_dependency"));
        assert_eq!((snapshot.active, snapshot.counts.queued), (0, 3));
        assert_eq!(
            fake.started.load(Ordering::SeqCst),
            1,
            "doomed jobs were started"
        );
        scheduler.start();
        until(&scheduler, |snapshot| snapshot.active == 1).await;
        assert_eq!(scheduler.snapshot().paused_by, None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn many_jobs_ending_together_start_each_queued_job_once() {
        let (scheduler, fake, _) = setup();
        scheduler.set_concurrency(4).unwrap();
        scheduler.add(pages(0..12));
        scheduler.start();
        for batch in 0..3 {
            until(&scheduler, |snapshot| {
                snapshot.active == 4 && snapshot.counts.completed == batch * 4
            })
            .await;
            running_ids(&scheduler, &fake).await;
            let senders: Vec<_> = fake
                .enders
                .lock()
                .unwrap()
                .drain()
                .map(|(_, sender)| sender)
                .collect();
            for sender in senders {
                let _ = sender.send(End::Done);
            }
        }
        until(&scheduler, |snapshot| snapshot.counts.completed == 12).await;
        assert_eq!(fake.started.load(Ordering::SeqCst), 12);
        assert_eq!(fake.peak.load(Ordering::SeqCst), 4);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn adding_validates_each_source_and_detects_duplicates() {
        let (scheduler, _, _) = setup();
        let report = scheduler.add(vec![
            SourceRequest::Page {
                page_url: "https://site.example/a?x=1".into(),
            },
            SourceRequest::Page {
                page_url: "not a url".into(),
            },
            SourceRequest::Page {
                page_url: "https://site.example/a?x=2".into(),
            },
            SourceRequest::Page {
                page_url: "https://site.example/a?x=1".into(),
            },
            SourceRequest::Page {
                page_url: "http://site.example/plain".into(),
            },
            SourceRequest::DirectHls {
                playlist_url: "https://cdn.example/p.m3u8?token=1".into(),
                referer_page_url: "https://site.example/watch".into(),
            },
        ]);
        // Query strings are part of a video's identity.
        assert_eq!(report.added.len(), 3);
        assert_eq!(report.duplicates, [3]);
        let rejected: Vec<_> = report
            .rejected
            .iter()
            .map(|(index, error)| (*index, error.code.as_str()))
            .collect();
        assert_eq!(rejected, [(1, "invalid_page_url"), (4, "invalid_page_url")]);
        assert_eq!(scheduler.add(pages(0..1)).added.len(), 1);
        assert_eq!(scheduler.add(pages(0..1)).duplicates, [0]);
        let snapshot = scheduler.snapshot();
        assert_eq!(snapshot.items[2].kind, "direct");
        assert_eq!(snapshot.items[2].host, "cdn.example");
        let text = serde_json::to_string(&snapshot).unwrap();
        assert!(!text.contains("token"), "{text}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_queue_and_each_batch_are_bounded() {
        let (scheduler, _, _) = setup();
        let report = scheduler.add(pages(0..MAX_BATCH + 5));
        assert_eq!(report.added.len(), MAX_BATCH);
        assert_eq!(report.rejected.len(), 5);
        assert!(
            report
                .rejected
                .iter()
                .all(|(_, error)| error.code == "batch_too_large")
        );
        scheduler.add(pages(MAX_BATCH..MAX_QUEUE_ITEMS));
        let full = scheduler.add(pages(MAX_QUEUE_ITEMS..MAX_QUEUE_ITEMS + 1));
        assert_eq!(full.rejected[0].1.code, "queue_full");
        assert_eq!(scheduler.snapshot().items.len(), MAX_QUEUE_ITEMS);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn remove_and_clear_leave_running_jobs_alone() {
        let (scheduler, fake, _) = setup();
        let ids = scheduler.add(pages(0..4)).added;
        scheduler.set_concurrency(1).unwrap();
        scheduler.start();
        until(&scheduler, |snapshot| snapshot.active == 1).await;
        scheduler.remove(ids[0]);
        scheduler.remove(ids[3]);
        let snapshot = scheduler.snapshot();
        assert!(
            snapshot.items.iter().any(|item| item.id == ids[0]),
            "a running job was removed"
        );
        assert!(!snapshot.items.iter().any(|item| item.id == ids[3]));
        end(&fake, 0, End::Done).await;
        until(&scheduler, |snapshot| {
            status(snapshot, ids[0]) == ItemStatus::Completed
        })
        .await;
        scheduler.clear_completed();
        let snapshot = scheduler.snapshot();
        assert!(!snapshot.items.iter().any(|item| item.id == ids[0]));
        assert_eq!(snapshot.items.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shutdown_cancels_everything_and_accepts_nothing_new() {
        let (scheduler, fake, _) = setup();
        scheduler.set_concurrency(3).unwrap();
        scheduler.add(pages(0..6));
        scheduler.start();
        until(&scheduler, |snapshot| snapshot.active == 3).await;
        end(&fake, 0, End::Done).await;
        until(&scheduler, |snapshot| {
            snapshot.counts.completed == 1 && snapshot.active == 3
        })
        .await;
        tokio::time::timeout(Duration::from_secs(5), scheduler.shutdown())
            .await
            .expect("shutdown did not finish");
        let snapshot = scheduler.snapshot();
        assert_eq!(snapshot.active, 0);
        assert_eq!(snapshot.counts.completed, 1);
        assert_eq!(snapshot.counts.cancelled, 5);
        assert_eq!(
            scheduler.add(pages(10..11)).rejected[0].1.code,
            "queue_closed"
        );
        scheduler.start();
        tokio::task::yield_now().await;
        assert_eq!(scheduler.snapshot().active, 0);
        assert_eq!(fake.started.load(Ordering::SeqCst), 4);
    }
}
