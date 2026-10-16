//! Running external tools with bounded output, timeouts and cancellation.
//!
//! Each child gets its own process group, and every exit path that does not
//! wait for a normal exit kills the whole group before reaping the leader, so
//! the group ID cannot be reused while it is being signalled.

use crate::{CappedOutput, read_capped};
use std::{collections::VecDeque, ffi::OsStr, path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    sync::{mpsc, watch},
    time::{Instant, MissedTickBehavior, interval, timeout},
};

const MAX_LINE_BYTES: usize = 16 * 1024;
const WATCH_TICK: Duration = Duration::from_secs(1);
/// Progress samples closer together than this are merged, which bounds the
/// sample buffer to `window / MIN_SAMPLE_GAP` entries.
const MIN_SAMPLE_GAP: Duration = Duration::from_millis(250);

/// Requests cancellation of the job that holds the matching [`CancelToken`].
#[derive(Debug)]
pub struct CancelHandle(watch::Sender<bool>);

#[derive(Clone, Debug)]
pub struct CancelToken(watch::Receiver<bool>);

pub fn cancel_pair() -> (CancelHandle, CancelToken) {
    let (sender, receiver) = watch::channel(false);
    (CancelHandle(sender), CancelToken(receiver))
}

impl CancelHandle {
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }
}

impl CancelToken {
    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }

    /// Resolves once cancellation is requested. Never resolves if the handle
    /// was dropped without cancelling.
    pub async fn cancelled(&mut self) {
        if self.0.wait_for(|cancelled| *cancelled).await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

#[derive(Debug)]
pub enum RunError {
    /// The executable could not be started (`NotFound` when it is missing).
    Spawn(std::io::Error),
    Io,
    TimedOut,
    Cancelled,
    /// Progress stayed below the stall policy's minimum for its window.
    Stalled,
    /// The disk's free space fell below the floor.
    NoSpace,
}

/// When a transfer counts as stalled: fewer than `min_bytes` of progress in
/// any `window`. A rate floor, not a silence timer, so a server that sends a
/// byte every few seconds cannot hold a job forever, while a slow but steady
/// one is never cut off.
#[derive(Clone, Copy, Debug)]
pub struct StallPolicy {
    pub window: Duration,
    pub min_bytes: u64,
}

/// What [`run_streaming`] watches besides the process itself.
pub struct Watch<'a> {
    /// Hard limit for the whole run.
    pub deadline: Duration,
    pub stall: Option<StallPolicy>,
    /// Stop when `free()` reports fewer than `floor` bytes.
    pub disk: Option<DiskFloor<'a>>,
}

pub struct DiskFloor<'a> {
    pub free: &'a (dyn Fn() -> Option<u64> + Send + Sync),
    pub floor: u64,
}

/// Progress over a sliding window.
struct Meter {
    policy: StallPolicy,
    started: Instant,
    /// `(when, cumulative bytes)`, oldest first; at most one sample is older
    /// than the window (the baseline).
    samples: VecDeque<(Instant, u64)>,
    /// The tool's last reported counter, and what earlier counters reached
    /// before it restarted (yt-dlp restarts a transfer the server would not
    /// resume): bytes moved again are still bytes moved.
    reported: u64,
    carried: u64,
}

impl Meter {
    fn new(policy: StallPolicy, started: Instant) -> Self {
        Self {
            policy,
            started,
            samples: VecDeque::new(),
            reported: 0,
            carried: 0,
        }
    }

    fn observe(&mut self, at: Instant, reported: u64) {
        if reported < self.reported {
            self.carried = self.carried.saturating_add(self.reported);
        }
        self.reported = reported;
        let bytes = self.carried.saturating_add(reported);
        match self.samples.back_mut() {
            Some((_, last)) if bytes <= *last => return,
            Some((last_at, last)) if at.duration_since(*last_at) < MIN_SAMPLE_GAP => {
                *last = bytes;
            }
            _ => self.samples.push_back((at, bytes)),
        }
        let horizon = at.checked_sub(self.policy.window);
        while let (Some(horizon), Some((second, _))) = (horizon, self.samples.get(1))
            && *second <= horizon
        {
            self.samples.pop_front();
        }
    }

    fn stalled(&self, now: Instant) -> bool {
        if now.duration_since(self.started) < self.policy.window {
            return false;
        }
        let current = self.samples.back().map_or(0, |(_, bytes)| *bytes);
        let horizon = now - self.policy.window;
        let baseline = self
            .samples
            .iter()
            .rev()
            .find(|(at, _)| *at <= horizon)
            .map_or(0, |(_, bytes)| *bytes);
        current.saturating_sub(baseline) < self.policy.min_bytes
    }
}

pub struct Captured {
    pub success: bool,
    pub stdout: CappedOutput,
    pub stderr: CappedOutput,
}

/// Makes the child die with HighVi: after a crash or SIGKILL, a tool must
/// not keep transferring on its own. Runs between fork and exec. The kernel
/// ties the signal to the spawning *thread*, so tools are only started from
/// the runtime's worker threads, never from `spawn_blocking`.
pub(crate) fn die_with_parent(command: &mut Command) {
    let parent = libc::pid_t::try_from(std::process::id()).unwrap_or(0);
    // SAFETY: prctl and getppid are async-signal-safe and touch no memory
    // shared with the parent.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // HighVi died before the setting took effect.
            if libc::getppid() != parent {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
}

/// A command with no shell, no stdin, a cleared environment (plus `env`), its
/// own process group, and a lifetime tied to HighVi's.
pub fn tool_command<'a>(
    program: &Path,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
    env: impl IntoIterator<Item = (&'a str, std::ffi::OsString)>,
) -> Command {
    let mut command = Command::new(program);
    command
        .kill_on_drop(true)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear()
        .envs(env)
        .args(args);
    die_with_parent(&mut command);
    command
}

/// Runs to completion and keeps at most `stdout_limit`/`stderr_limit` bytes.
pub async fn run_captured(
    mut command: Command,
    stdout_limit: usize,
    stderr_limit: usize,
    limit: Duration,
    cancel: &mut CancelToken,
) -> Result<Captured, RunError> {
    let mut child = command.spawn().map_err(RunError::Spawn)?;
    let stdout = child.stdout.take().ok_or(RunError::Io)?;
    let stderr = child.stderr.take().ok_or(RunError::Io)?;

    let outcome = {
        let work = async {
            tokio::join!(
                child.wait(),
                read_capped(stdout, stdout_limit),
                read_capped(stderr, stderr_limit)
            )
        };
        tokio::select! {
            result = timeout(limit, work) => Some(result),
            () = cancel.cancelled() => None,
        }
    };
    match outcome {
        None => {
            kill_group(&mut child).await;
            Err(RunError::Cancelled)
        }
        Some(Err(_)) => {
            kill_group(&mut child).await;
            Err(RunError::TimedOut)
        }
        Some(Ok((status, stdout, stderr))) => {
            let status = status.map_err(|_| RunError::Io)?;
            Ok(Captured {
                success: status.success(),
                stdout: stdout.map_err(|_| RunError::Io)?,
                stderr: stderr.map_err(|_| RunError::Io)?,
            })
        }
    }
}

/// Runs to completion, passing each stdout/stderr line (lossily decoded,
/// oversized lines dropped) to `on_line`, which returns the cumulative
/// progress in bytes when the line reports any. Returns whether the tool
/// exited with 0; the watch can end the run early, killing its group.
pub async fn run_streaming(
    mut command: Command,
    watch: Watch<'_>,
    cancel: &mut CancelToken,
    mut on_line: impl FnMut(&str, Stream) -> Option<u64>,
) -> Result<bool, RunError> {
    let mut child = command.spawn().map_err(RunError::Spawn)?;
    let stdout = child.stdout.take().ok_or(RunError::Io)?;
    let stderr = child.stderr.take().ok_or(RunError::Io)?;
    let started = Instant::now();
    let (progress_sender, progress) = watch::channel(0_u64);

    let outcome = {
        let (sender, mut receiver) = mpsc::channel::<(Stream, String)>(256);
        let readers = async {
            tokio::join!(
                read_lines(stdout, Stream::Stdout, sender.clone()),
                read_lines(stderr, Stream::Stderr, sender)
            )
        };
        let consumer = async {
            while let Some((stream, line)) = receiver.recv().await {
                if let Some(bytes) = on_line(&line, stream) {
                    progress_sender.send_replace(bytes);
                }
            }
        };
        let work = async {
            let (status, (), ()) = tokio::join!(
                child.wait(),
                async {
                    readers.await;
                },
                consumer
            );
            status
        };
        tokio::select! {
            result = timeout(watch.deadline, work) => result.map_err(|_| RunError::TimedOut),
            error = watchdog(&watch, started, progress) => Err(error),
            () = cancel.cancelled() => Err(RunError::Cancelled),
        }
    };
    match outcome {
        Ok(status) => Ok(status.map_err(|_| RunError::Io)?.success()),
        Err(error) => {
            kill_group(&mut child).await;
            Err(error)
        }
    }
}

/// Resolves with the reason to stop, if the stall policy or the disk floor
/// is ever violated; otherwise never.
async fn watchdog(
    watch: &Watch<'_>,
    started: Instant,
    mut progress: watch::Receiver<u64>,
) -> RunError {
    if watch.stall.is_none() && watch.disk.is_none() {
        return std::future::pending().await;
    }
    let mut meter = watch.stall.map(|policy| Meter::new(policy, started));
    let mut tick = interval(WATCH_TICK);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut progress_open = true;
    loop {
        tokio::select! {
            changed = progress.changed(), if progress_open => match changed {
                Ok(()) => {
                    let bytes = *progress.borrow_and_update();
                    if let Some(meter) = meter.as_mut() {
                        meter.observe(Instant::now(), bytes);
                    }
                }
                Err(_) => progress_open = false,
            },
            _ = tick.tick() => {
                if meter.as_ref().is_some_and(|meter| meter.stalled(Instant::now())) {
                    return RunError::Stalled;
                }
                if let Some(disk) = &watch.disk
                    && (disk.free)().is_some_and(|free| free < disk.floor)
                {
                    return RunError::NoSpace;
                }
            }
        }
    }
}

/// Which output a line came from: progress is trusted only from stdout,
/// where yt-dlp's progress template writes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stream {
    Stdout,
    Stderr,
}

async fn read_lines<R: AsyncRead + Unpin>(
    mut reader: R,
    stream: Stream,
    sender: mpsc::Sender<(Stream, String)>,
) {
    let mut chunk = vec![0_u8; 8 * 1024];
    let mut line = Vec::with_capacity(512);
    let mut oversized = false;
    loop {
        let read = match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        for byte in &chunk[..read] {
            if *byte == b'\n' {
                if !oversized && sender.send((stream, decode(&line))).await.is_err() {
                    return;
                }
                line.clear();
                oversized = false;
            } else if !oversized {
                if line.len() < MAX_LINE_BYTES {
                    line.push(*byte);
                } else {
                    line.clear();
                    oversized = true;
                }
            }
        }
    }
    if !line.is_empty() && !oversized {
        let _ = sender.send((stream, decode(&line))).await;
    }
}

fn decode(line: &[u8]) -> String {
    String::from_utf8_lossy(line)
        .trim_end_matches('\r')
        .to_string()
}

/// SIGKILLs the child's process group, then reaps the leader.
pub async fn kill_group(child: &mut Child) {
    if let Some(pid) = child.id()
        && let Ok(pid) = libc::pid_t::try_from(pid)
    {
        // SAFETY: plain syscall; the leader has not been reaped yet, so
        // `pid` still names the group this child created.
        unsafe {
            libc::killpg(pid, libc::SIGKILL);
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn limited(deadline: Duration) -> Watch<'static> {
        Watch {
            deadline,
            stall: None,
            disk: None,
        }
    }

    /// Emits `bytes` cumulative progress every `period`, `count` times.
    fn progress_script(period: &str, step: u64, count: u32) -> String {
        format!(
            "i=0; while [ $i -lt {count} ]; do i=$((i+1)); echo P$((i*{step})); sleep {period}; done"
        )
    }

    fn parse(line: &str) -> Option<u64> {
        line.strip_prefix('P')?.parse().ok()
    }

    fn stall(window_ms: u64, min_bytes: u64) -> Option<StallPolicy> {
        Some(StallPolicy {
            window: Duration::from_millis(window_ms),
            min_bytes,
        })
    }

    #[tokio::test]
    async fn a_silent_tool_is_stopped_as_stalled_and_its_group_killed() {
        let (_handle, mut token) = cancel_pair();
        let marker = std::env::temp_dir().join(format!("highvi-stall-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let script = format!("(sleep 5; touch '{}') & sleep 30", marker.display());
        let started = std::time::Instant::now();
        let result = run_streaming(
            sh(&script),
            Watch {
                deadline: Duration::from_secs(60),
                stall: stall(1_500, 1),
                disk: None,
            },
            &mut token,
            |line, _| parse(line),
        )
        .await;
        assert!(matches!(result, Err(RunError::Stalled)), "{result:?}");
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "{:?}",
            started.elapsed()
        );
        tokio::time::sleep(
            Duration::from_millis(5_500) - started.elapsed().min(Duration::from_millis(5_500)),
        )
        .await;
        assert!(!marker.exists(), "a grandchild survived the stall kill");
    }

    #[tokio::test]
    async fn a_trickle_below_the_rate_floor_is_a_stall() {
        let (_handle, mut token) = cancel_pair();
        // 10 bytes every 0.2 s keeps "making progress", but far below
        // 1 000 bytes per 1.5 s.
        let result = run_streaming(
            sh(&progress_script("0.2", 10, 100)),
            Watch {
                deadline: Duration::from_secs(60),
                stall: stall(1_500, 1_000),
                disk: None,
            },
            &mut token,
            |line, _| parse(line),
        )
        .await;
        assert!(matches!(result, Err(RunError::Stalled)), "{result:?}");
    }

    #[tokio::test]
    async fn a_slow_but_steady_transfer_and_chatty_output_are_not_stalls() {
        let (_handle, mut token) = cancel_pair();
        // 400 bytes every 0.2 s = 3 000 per 1.5 s, above the floor, for 4 s.
        let ok = run_streaming(
            sh(&progress_script("0.2", 400, 20)),
            Watch {
                deadline: Duration::from_secs(60),
                stall: stall(1_500, 1_000),
                disk: None,
            },
            &mut token,
            |line, _| parse(line),
        )
        .await
        .unwrap();
        assert!(ok);
        // Lines that are not progress do not count as progress.
        let result = run_streaming(
            sh("while true; do echo 'WARNING: retrying'; sleep 0.1; done"),
            Watch {
                deadline: Duration::from_secs(60),
                stall: stall(1_500, 1),
                disk: None,
            },
            &mut token,
            |line, _| parse(line),
        )
        .await;
        assert!(matches!(result, Err(RunError::Stalled)), "{result:?}");
    }

    #[tokio::test]
    async fn progress_on_stderr_does_not_count() {
        let (_handle, mut token) = cancel_pair();
        let script = format!("({}) 1>&2", progress_script("0.1", 100_000, 100));
        let result = run_streaming(
            sh(&script),
            Watch {
                deadline: Duration::from_secs(60),
                stall: stall(1_500, 1),
                disk: None,
            },
            &mut token,
            |line, stream| (stream == Stream::Stdout).then(|| parse(line)).flatten(),
        )
        .await;
        assert!(matches!(result, Err(RunError::Stalled)), "{result:?}");
    }

    #[tokio::test]
    async fn the_deadline_and_the_disk_floor_stop_a_run() {
        let (_handle, mut token) = cancel_pair();
        let result = run_streaming(
            sh(&progress_script("0.1", 1_000_000, 1_000)),
            Watch {
                deadline: Duration::from_millis(1_500),
                stall: stall(60_000, 1),
                disk: None,
            },
            &mut token,
            |line, _| parse(line),
        )
        .await;
        assert!(matches!(result, Err(RunError::TimedOut)), "{result:?}");

        let free = std::sync::atomic::AtomicU64::new(10_000);
        let probe = || Some(free.load(std::sync::atomic::Ordering::SeqCst));
        let run = run_streaming(
            sh(&progress_script("0.1", 1_000, 1_000)),
            Watch {
                deadline: Duration::from_secs(60),
                stall: None,
                disk: Some(DiskFloor {
                    free: &probe,
                    floor: 5_000,
                }),
            },
            &mut token,
            |line, _| {
                let bytes = parse(line)?;
                free.store(
                    10_000u64.saturating_sub(bytes),
                    std::sync::atomic::Ordering::SeqCst,
                );
                Some(bytes)
            },
        );
        let result = run.await;
        assert!(matches!(result, Err(RunError::NoSpace)), "{result:?}");
    }

    #[test]
    fn the_meter_keeps_a_bounded_window() {
        let start = tokio::time::Instant::now();
        let mut meter = Meter::new(
            StallPolicy {
                window: Duration::from_secs(10),
                min_bytes: 100,
            },
            start,
        );
        for step in 0..10_000_u64 {
            meter.observe(start + Duration::from_millis(step * 10), step * 1_000);
        }
        // 100 s of samples every 10 ms: merged to >= 250 ms apart and pruned
        // to the last 10 s (plus one baseline).
        assert!(meter.samples.len() <= 42, "{}", meter.samples.len());
        let now = start + Duration::from_millis(99_990);
        assert!(!meter.stalled(now));
        assert!(meter.stalled(now + Duration::from_secs(11)));
        // A restarted counter (the transfer began again) keeps counting:
        // new bytes after the restart are progress, not a stall.
        let restarted = now + Duration::from_secs(11);
        meter.observe(restarted, 5);
        for step in 1..=50_u64 {
            meter.observe(
                restarted + Duration::from_millis(step * 300),
                5 + step * 1_000,
            );
        }
        let later = restarted + Duration::from_secs(15);
        assert!(
            !meter.stalled(later),
            "transfer after a restart looked stalled"
        );
        assert!(meter.stalled(later + Duration::from_secs(11)));
    }

    fn sh(script: &str) -> Command {
        tool_command(
            Path::new("/bin/sh"),
            ["-c", script],
            [("PATH", std::ffi::OsString::from("/usr/bin:/bin"))],
        )
    }

    #[tokio::test]
    async fn streaming_delivers_lines_from_both_streams_and_drops_oversized_ones() {
        let (_handle, mut token) = cancel_pair();
        let mut lines = Vec::new();
        let ok = run_streaming(
            sh("echo one; echo two >&2; head -c 20000 /dev/zero | tr '\\0' x; echo; printf 'last\\r'"),
            limited(Duration::from_secs(10)),
            &mut token,
            |line, _| {
                lines.push(line.to_string());
                None
            },
        )
        .await
        .unwrap();
        assert!(ok);
        lines.sort();
        assert_eq!(lines, ["last", "one", "two"]);
    }

    #[test]
    fn a_tool_dies_with_the_thread_that_started_it() {
        // HighVi's death is the same signal as its spawning thread's: the
        // kernel sends SIGKILL to the child.
        let pid = std::thread::spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let mut command = tool_command(
                    Path::new("/bin/sleep"),
                    ["30"],
                    Vec::<(&str, std::ffi::OsString)>::new(),
                );
                command.kill_on_drop(false);
                command.spawn().unwrap().id().unwrap()
            })
        })
        .join()
        .unwrap();
        let alive = || {
            std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
                !stat
                    .rsplit(')')
                    .next()
                    .unwrap_or("")
                    .trim_start()
                    .starts_with('Z')
            })
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while alive() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if alive() {
            // SAFETY: plain kill of the child this test started.
            unsafe { libc::kill(libc::pid_t::try_from(pid).unwrap(), libc::SIGKILL) };
            panic!("the tool outlived the thread that started it");
        }
    }

    #[tokio::test]
    async fn cancellation_kills_the_whole_process_group() {
        let (handle, mut token) = cancel_pair();
        let marker = std::env::temp_dir().join(format!("highvi-orphan-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let script = format!(
            "(sleep 2; touch '{}') & echo started; wait",
            marker.display()
        );
        let started = Instant::now();
        let run = run_streaming(
            sh(&script),
            limited(Duration::from_secs(60)),
            &mut token,
            |line, _| {
                if line == "started" {
                    handle.cancel();
                }
                None
            },
        );
        assert!(matches!(run.await, Err(RunError::Cancelled)));
        assert!(started.elapsed() < Duration::from_secs(2));
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(!marker.exists(), "a grandchild survived cancellation");
    }

    #[tokio::test]
    async fn timeouts_and_missing_programs_are_reported() {
        let (_handle, mut token) = cancel_pair();
        let result = run_captured(
            sh("sleep 5"),
            10,
            10,
            Duration::from_millis(100),
            &mut token,
        )
        .await;
        assert!(matches!(result, Err(RunError::TimedOut)));

        let missing = tool_command(Path::new("/nonexistent/highvi-tool"), ["x"], []);
        let result = run_captured(missing, 10, 10, Duration::from_secs(1), &mut token).await;
        assert!(
            matches!(result, Err(RunError::Spawn(ref error)) if error.kind() == std::io::ErrorKind::NotFound)
        );
    }

    #[tokio::test]
    async fn captured_output_is_bounded() {
        let (_handle, mut token) = cancel_pair();
        let captured = run_captured(
            sh("head -c 100000 /dev/zero; echo err >&2; exit 3"),
            1024,
            1024,
            Duration::from_secs(10),
            &mut token,
        )
        .await
        .unwrap();
        assert!(!captured.success);
        assert!(captured.stdout.overflowed);
        assert_eq!(captured.stdout.bytes.len(), 1024);
        assert_eq!(captured.stderr.bytes, b"err\n");
    }
}
