//! Where HighVi writes: a fixed output directory, one private job directory
//! per run inside it, and an atomic no-replace publish of the finished MP4.

use crate::PublicError;
use std::{
    ffi::CString,
    fs::File,
    os::{
        fd::AsRawFd,
        unix::{ffi::OsStrExt, fs::OpenOptionsExt, fs::PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const JOB_PREFIX: &str = ".highvi-job-";
const JOB_MARKER: &str = ".highvi-owned-job";
/// Present in a job directory kept for recovery; cleanup never removes it.
const KEEP_MARKER: &str = ".highvi-kept";
/// Long enough to cover a job directory being created and locked; a live
/// job keeps its marker locked, so its age alone never gets it removed.
const STALE_JOB_AGE: Duration = Duration::from_secs(10 * 60);
const MAX_NAME_ATTEMPTS: u32 = 1_000;
static JOB_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Atomically moves `source` to `destination`, failing with `AlreadyExists`
/// instead of replacing a file created there by anyone else.
pub fn rename_without_replacement(source: &Path, destination: &Path) -> std::io::Result<()> {
    let nul = |_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL");
    let source = CString::new(source.as_os_str().as_bytes()).map_err(nul)?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(nul)?;

    // SAFETY: both pointers are valid NUL-terminated path strings for the
    // duration of the call. RENAME_NOREPLACE provides atomic publication
    // without clobbering a destination created by another process.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Publishes `source` as `<stem>.mp4` in `directory`, or `<stem> (2).mp4`,
/// `<stem> (3).mp4`… when the name is taken. Never replaces a file.
/// `cancelled` is checked immediately before each rename; when it returns
/// true nothing is published and the error kind is `Interrupted`.
pub fn publish_unique(
    source: &Path,
    directory: &Path,
    stem: &str,
    cancelled: &dyn Fn() -> bool,
) -> std::io::Result<PathBuf> {
    for attempt in 1..=MAX_NAME_ATTEMPTS {
        if cancelled() {
            return Err(std::io::ErrorKind::Interrupted.into());
        }
        let name = if attempt == 1 {
            format!("{stem}.mp4")
        } else {
            format!("{stem} ({attempt}).mp4")
        };
        let destination = directory.join(name);
        match rename_without_replacement(source, &destination) {
            Ok(()) => return Ok(destination),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::ErrorKind::AlreadyExists.into())
}

/// Bytes available to unprivileged users on the filesystem holding `path`.
pub fn free_bytes(path: &Path) -> Option<u64> {
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is NUL-terminated and `stats` is a valid out-pointer.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: statvfs succeeded and filled the struct.
    let stats = unsafe { stats.assume_init() };
    stats.f_bavail.checked_mul(stats.f_frsize)
}

/// Space promised to running jobs, so jobs that start together do not all
/// count the same free space. One per process (or per scheduler).
#[derive(Debug, Default)]
pub struct DiskBudget {
    ledger: std::sync::Mutex<Ledger>,
    /// Woken whenever space is given back or a waiter leaves the line.
    released: tokio::sync::Notify,
}

#[derive(Debug, Default)]
struct Ledger {
    reserved: u64,
    /// Jobs waiting for space, oldest first. Only the oldest may take
    /// space, so small jobs that start later cannot keep a big one waiting.
    waiters: std::collections::VecDeque<u64>,
    /// Merges waiting for space, oldest first. They go before any download:
    /// a merge always gives space back when it is done, so a download must
    /// never be able to keep one waiting (that would deadlock the queue).
    merge_waiters: std::collections::VecDeque<u64>,
    next_ticket: u64,
    /// Room each downloading job will need to merge, by reservation. Merges
    /// are what need two copies at once; keeping room for the largest one
    /// (not for every one) lets several downloads run beside each other,
    /// since each merge frees its tracks before the next needs the room.
    merges: Vec<(u64, u64)>,
    next_hold: u64,
}

/// What a job asks the budget for.
#[derive(Clone, Copy, Debug)]
pub enum Need {
    /// Downloading `bytes`, then merging them into a copy of about `merge`
    /// bytes. Starts only if, beside everything already held, there is
    /// still room for the largest merge any downloading job will need.
    Download { bytes: u64, merge: u64 },
    /// Writing the merged copy now. This is what the merge room was kept
    /// for, so only what is actually held counts against it.
    Merge { bytes: u64 },
}

impl Need {
    /// All the room it needs when nothing else is running.
    pub fn total(self) -> u64 {
        match self {
            Need::Download { bytes, merge } => bytes.saturating_add(merge),
            Need::Merge { bytes } => bytes,
        }
    }
}

/// What [`DiskBudget::admit`] decided.
#[derive(Debug)]
pub enum Admission {
    Granted(Reservation),
    /// Other downloads hold space (or an older waiter is ahead): wait for
    /// them to give it back.
    Wait,
    /// The video does not fit, and no other download holds space.
    Refused,
}

/// A place in the line of jobs waiting for space; leaving it (by drop)
/// lets the next one try.
#[derive(Debug)]
pub struct WaitTicket {
    budget: std::sync::Arc<DiskBudget>,
    id: u64,
    merge: bool,
}

impl Drop for WaitTicket {
    fn drop(&mut self) {
        let mut ledger = self.budget.ledger();
        let line = if self.merge {
            &mut ledger.merge_waiters
        } else {
            &mut ledger.waiters
        };
        line.retain(|waiter| *waiter != self.id);
        drop(ledger);
        self.budget.released.notify_waiters();
    }
}

/// Returned to the budget when dropped.
#[derive(Debug)]
pub struct Reservation {
    budget: std::sync::Arc<DiskBudget>,
    /// Its entry in the merge room, while it downloads.
    hold: Option<u64>,
    /// What the job asked for.
    total: u64,
    /// Free space when it asked, if known.
    free: Option<u64>,
    /// What it still holds: `total` less what it has written.
    bytes: u64,
}

impl DiskBudget {
    fn ledger(&self) -> std::sync::MutexGuard<'_, Ledger> {
        self.ledger
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Reserves `bytes` if `free`, less what other jobs reserved, still
    /// leaves `floor` and no job is waiting for space. Unknown free space
    /// admits the job; the floor is still watched while it writes.
    pub fn reserve(
        self: &std::sync::Arc<Self>,
        free: Option<u64>,
        bytes: u64,
        floor: u64,
    ) -> Option<Reservation> {
        let need = Need::Download { bytes, merge: 0 };
        match self.admit(free, need, floor, &mut None).0 {
            Admission::Granted(reservation) => Some(reservation),
            Admission::Wait | Admission::Refused => None,
        }
    }

    /// Decides, in one look at the ledger, whether a job needing `bytes`
    /// may start, must wait (other jobs hold space, or an older waiter is
    /// first), or cannot fit at all (nothing else is held and it still
    /// does not fit). A waiting job keeps `ticket` for its place in the line and
    /// passes it back on the next try. Also returns what other jobs held
    /// at that moment.
    pub fn admit(
        self: &std::sync::Arc<Self>,
        free: Option<u64>,
        need: Need,
        floor: u64,
        ticket: &mut Option<WaitTicket>,
    ) -> (Admission, u64) {
        let mut ledger = self.ledger();
        let held = ledger.reserved;
        let mine = ticket.as_ref().map(|ticket| ticket.id);
        let is_merge = matches!(need, Need::Merge { .. });
        let first_in = |line: &std::collections::VecDeque<u64>| {
            line.front().is_some_and(|front| Some(*front) != mine)
        };
        // Merges answer only to older merges; downloads to older downloads
        // and to every waiting merge.
        let ahead = if is_merge {
            first_in(&ledger.merge_waiters)
        } else {
            first_in(&ledger.waiters) || !ledger.merge_waiters.is_empty()
        };
        let (bytes, merge_room, merge) = match need {
            Need::Download { bytes, merge } => {
                let largest = ledger.merges.iter().map(|(_, merge)| *merge).max();
                (bytes, largest.unwrap_or(0).max(merge), Some(merge))
            }
            Need::Merge { bytes } => (bytes, 0, None),
        };
        let needed = bytes.saturating_add(merge_room).saturating_add(floor);
        let available = free.unwrap_or(u64::MAX).saturating_sub(held);
        if available >= needed && !ahead {
            ledger.reserved = held.saturating_add(bytes);
            let hold = merge.filter(|merge| *merge > 0).map(|merge| {
                let id = ledger.next_hold;
                ledger.next_hold += 1;
                ledger.merges.push((id, merge));
                id
            });
            drop(ledger);
            // Leaves the line (and wakes the next waiter).
            drop(ticket.take());
            let reservation = Reservation {
                budget: std::sync::Arc::clone(self),
                hold,
                total: bytes,
                free,
                bytes,
            };
            return (Admission::Granted(reservation), held);
        }
        // While others hold space, what is free now says little: a job that
        // is merging briefly needs room for two copies and then frees its
        // tracks. So wait while something that will move on holds space;
        // only then is a video that still does not fit refused. A merge
        // waits only on downloads and merges in progress (what `reserved`
        // counts; a job that finished downloading holds no bytes there):
        // merge room kept for others is no space taken, and those jobs wait
        // behind it, so waiting on them could never end.
        let others_active = if is_merge {
            held > 0
        } else {
            held > 0 || !ledger.merges.is_empty()
        };
        if others_active || ahead {
            if ticket.is_none() {
                let id = ledger.next_ticket;
                ledger.next_ticket += 1;
                if is_merge {
                    ledger.merge_waiters.push_back(id);
                } else {
                    ledger.waiters.push_back(id);
                }
                *ticket = Some(WaitTicket {
                    budget: std::sync::Arc::clone(self),
                    id,
                    merge: is_merge,
                });
            }
            return (Admission::Wait, held);
        }
        drop(ledger);
        drop(ticket.take());
        (Admission::Refused, held)
    }

    /// Resolves after space is given back or a waiter leaves the line.
    /// Create it before checking the budget (and `enable` it), so a
    /// release in between is not missed.
    pub fn released(&self) -> tokio::sync::futures::Notified<'_> {
        self.released.notified()
    }

    pub fn reserved(&self) -> u64 {
        self.ledger().reserved
    }

    /// Holds `bytes` outright, as another job would, whatever is free.
    #[cfg(test)]
    pub(crate) fn hold(self: &std::sync::Arc<Self>, bytes: u64) -> Reservation {
        let mut ledger = self.ledger();
        ledger.reserved = ledger.reserved.saturating_add(bytes);
        Reservation {
            budget: std::sync::Arc::clone(self),
            hold: None,
            total: bytes,
            free: None,
            bytes,
        }
    }

    /// Room kept for merges: the largest any downloading job will need.
    pub fn merge_room(&self) -> u64 {
        self.ledger()
            .merges
            .iter()
            .map(|(_, merge)| *merge)
            .max()
            .unwrap_or(0)
    }
}

impl Reservation {
    /// The download is done: what it reserved but did not write (the
    /// estimate was high, or free space was reported late) goes back now.
    /// Only its merge room stays, until its merge has space of its own.
    pub fn finish_download(&mut self) {
        if self.bytes == 0 {
            return;
        }
        let mut ledger = self.budget.ledger();
        ledger.reserved = ledger.reserved.saturating_sub(self.bytes);
        self.bytes = 0;
        drop(ledger);
        self.budget.released.notify_waiters();
    }

    /// The job has `written` bytes on the disk. Free space already counts
    /// them, so the promise shrinks by as much; it never grows back. Only
    /// as far as `free` has fallen since the reservation, though: some
    /// filesystems report writes late, and the count comes from yt-dlp.
    /// Each job compares with its own reading, so jobs writing at once on
    /// such a filesystem may each count the same fall for a moment; the
    /// floor watched while writing still holds. Waiters are not woken:
    /// free space fell by as much, so nothing opened up for them.
    pub fn written(&mut self, written: u64, free: Option<u64>) {
        let (Some(before), Some(free)) = (self.free, free) else {
            return;
        };
        let shown = written.min(before.saturating_sub(free));
        let outstanding = self.total.saturating_sub(shown);
        if outstanding >= self.bytes {
            return;
        }
        let mut ledger = self.budget.ledger();
        ledger.reserved = ledger.reserved.saturating_sub(self.bytes - outstanding);
        self.bytes = outstanding;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut ledger = self.budget.ledger();
        ledger.reserved = ledger.reserved.saturating_sub(self.bytes);
        if let Some(hold) = self.hold {
            ledger.merges.retain(|(id, _)| *id != hold);
        }
        drop(ledger);
        self.budget.released.notify_waiters();
    }
}

/// Creates the output directory and refuses to use it through a symlink.
pub async fn prepare_output_dir(directory: &Path) -> Result<(), PublicError> {
    tokio::fs::create_dir_all(directory)
        .await
        .map_err(|_| output_dir_error())?;
    let metadata = tokio::fs::symlink_metadata(directory)
        .await
        .map_err(|_| output_dir_error())?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(output_dir_error());
    }
    Ok(())
}

pub fn output_dir_error() -> PublicError {
    PublicError::new(
        "download_directory_unavailable",
        "HighVi could not safely use its folder inside Downloads.",
    )
}

/// A private working directory for one job. It is removed when dropped,
/// unless [`JobDir::keep`] was called to leave a file for recovery.
///
/// While it exists, the job holds an exclusive `flock` on its marker file,
/// so another HighVi process can tell a running job from a crashed one.
#[derive(Debug)]
pub struct JobDir {
    path: PathBuf,
    keep: bool,
    _lock: File,
}

/// Takes an exclusive lock without waiting. The kernel drops it when the
/// file is closed, including when the process dies.
fn try_lock(file: &File) -> bool {
    // SAFETY: plain syscall on a descriptor owned by `file`.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

impl JobDir {
    pub async fn create(parent: &Path) -> Result<Self, PublicError> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for _ in 0..32 {
            let counter = JOB_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(
                "{JOB_PREFIX}{}-{timestamp}-{counter}",
                std::process::id()
            ));
            match tokio::fs::create_dir(&path).await {
                Ok(()) => {
                    let created = create_marker(&path);
                    return match created {
                        Some(lock) => Ok(Self {
                            path,
                            keep: false,
                            _lock: lock,
                        }),
                        None => {
                            let _ = std::fs::remove_dir_all(&path);
                            Err(output_dir_error())
                        }
                    };
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => return Err(output_dir_error()),
            }
        }
        Err(output_dir_error())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn keep(&mut self) {
        self.keep = true;
        let _ = std::fs::write(self.path.join(KEEP_MARKER), b"");
    }
}

/// Makes `directory` private and creates its locked marker file.
fn create_marker(directory: &Path) -> Option<File> {
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).ok()?;
    let marker = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(directory.join(JOB_MARKER))
        .ok()?;
    try_lock(&marker).then_some(marker)
}

impl Drop for JobDir {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// Removes job directories left by a crash. Only directories with the job
/// prefix and a regular marker file that no live process holds locked,
/// untouched for a day, are removed, and symlinks are never followed.
pub async fn cleanup_stale_jobs(parent: &Path) {
    // Never through a symlinked or missing output directory: that could
    // remove directories somewhere else. Only a process running as the
    // user could swap the folder between this check and the removals, and
    // such a process can delete these files itself; what is removed is
    // still limited to unlocked, stale job directories carrying HighVi's
    // marker.
    match tokio::fs::symlink_metadata(parent).await {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        _ => return,
    }
    let Ok(mut entries) = tokio::fs::read_dir(parent).await else {
        return;
    };
    let now = SystemTime::now();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        if !name
            .to_str()
            .is_some_and(|name| name.starts_with(JOB_PREFIX))
        {
            continue;
        }
        let path = entry.path();
        let Ok(metadata) = tokio::fs::symlink_metadata(&path).await else {
            continue;
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if now.duration_since(modified).unwrap_or_default() < STALE_JOB_AGE {
            continue;
        }
        let marker = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path.join(JOB_MARKER));
        let Ok(marker) = marker else { continue };
        let is_file = marker.metadata().is_ok_and(|metadata| metadata.is_file());
        // Held until the directory is gone, so no new job can claim it.
        let kept = std::fs::symlink_metadata(path.join(KEEP_MARKER)).is_ok();
        if is_file && !kept && try_lock(&marker) {
            let _ = tokio::fs::remove_dir_all(path).await;
        }
    }
}

#[cfg(test)]
pub(crate) fn temp_directory(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "highvi-test-{label}-{}-{}",
        std::process::id(),
        JOB_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir(&path).unwrap();
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn publishes_without_replacing_an_existing_destination() {
        let directory = temp_directory("rename");
        let source = directory.join("job.mp4");
        let destination = directory.join("final.mp4");
        fs::write(&source, b"new").unwrap();
        fs::write(&destination, b"old").unwrap();

        assert_eq!(
            rename_without_replacement(&source, &destination)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&source).unwrap(), b"new");
        assert_eq!(fs::read(&destination).unwrap(), b"old");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn collisions_get_a_numbered_name_and_existing_files_survive() {
        let directory = temp_directory("unique");
        fs::write(directory.join("Clip.mp4"), b"first").unwrap();
        fs::write(directory.join("Clip (2).mp4"), b"second").unwrap();
        let source = directory.join("new.mp4");
        fs::write(&source, b"third").unwrap();

        let published = publish_unique(&source, &directory, "Clip", &|| false).unwrap();
        assert_eq!(published, directory.join("Clip (3).mp4"));
        assert_eq!(fs::read(directory.join("Clip.mp4")).unwrap(), b"first");
        assert_eq!(fs::read(directory.join("Clip (2).mp4")).unwrap(), b"second");
        assert_eq!(fs::read(&published).unwrap(), b"third");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_cancel_before_the_rename_publishes_nothing() {
        let directory = temp_directory("cancelled");
        let source = directory.join("new.mp4");
        fs::write(&source, b"data").unwrap();
        let error = publish_unique(&source, &directory, "Clip", &|| true).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
        assert!(source.exists());
        assert!(!directory.join("Clip.mp4").exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn free_space_is_reported_for_real_paths_only() {
        assert!(free_bytes(&std::env::temp_dir()).is_some_and(|free| free > 0));
        assert!(free_bytes(Path::new("/nonexistent/highvi")).is_none());
    }

    /// A download with no merge room of its own.
    fn plain(bytes: u64) -> Need {
        Need::Download { bytes, merge: 0 }
    }
    fn download(size: u64) -> Need {
        Need::Download {
            bytes: size,
            merge: size,
        }
    }

    /// The case that was refused: ~4.2 GB videos, 17 GB free (in MiB).
    #[test]
    fn two_large_downloads_run_side_by_side() {
        let (size, free, floor) = (4_247, 17_007, 512);
        let budget = std::sync::Arc::new(DiskBudget::default());
        let (first, _) = budget.admit(Some(free), download(size), floor, &mut None);
        assert!(matches!(first, Admission::Granted(_)));
        // Beside it: its own size, one merge room, the floor.
        let (second, _) = budget.admit(Some(free), download(size), floor, &mut None);
        assert!(matches!(second, Admission::Granted(_)), "{second:?}");
        assert_eq!(budget.reserved(), 2 * size);
        assert_eq!(budget.merge_room(), size);
        // A third would not leave room for a merge: it waits.
        let mut third = None;
        assert!(waits(&budget.admit(
            Some(free),
            download(size),
            floor,
            &mut third
        )));
        drop((first, second));
        assert_eq!(budget.merge_room(), 0);
    }

    #[test]
    fn a_lone_download_still_needs_room_to_merge() {
        let budget = std::sync::Arc::new(DiskBudget::default());
        assert!(refused(&budget.admit(
            Some(2 * 100 + 9),
            download(100),
            10,
            &mut None
        )));
        assert!(granted(&budget.admit(
            Some(2 * 100 + 10),
            download(100),
            10,
            &mut None
        )));
    }

    #[test]
    fn merge_room_follows_the_largest_downloading_job() {
        let budget = std::sync::Arc::new(DiskBudget::default());
        let big = budget.admit(Some(1_000), download(300), 0, &mut None).0;
        assert!(matches!(big, Admission::Granted(_)));
        // 300 held + 300 merge room: a small one fits beside it.
        let small = budget.admit(Some(1_000), download(100), 0, &mut None).0;
        assert!(matches!(small, Admission::Granted(_)));
        assert_eq!(budget.merge_room(), 300);
        // 400 held + room 300 leaves 300: a 301-byte download must wait.
        assert!(waits(&budget.admit(Some(1_000), plain(301), 0, &mut None)));
        drop(big);
        assert_eq!(budget.merge_room(), 100);
    }

    #[test]
    fn a_merge_uses_the_room_kept_for_it() {
        let budget = std::sync::Arc::new(DiskBudget::default());
        let mut downloading = match budget.admit(Some(1_000), download(400), 0, &mut None).0 {
            Admission::Granted(reservation) => reservation,
            other => panic!("{other:?}"),
        };
        // Everything downloaded: free fell by 400, nothing is held, but the
        // merge room still counts against new downloads, which wait.
        downloading.written(400, Some(600));
        assert_eq!(budget.reserved(), 0);
        assert!(waits(&budget.admit(Some(600), plain(300), 0, &mut None)));
        // The merge itself is not charged the room kept for it.
        let merge = budget.admit(Some(600), Need::Merge { bytes: 425 }, 0, &mut None);
        assert!(granted(&merge));
        drop(downloading);
        assert_eq!(budget.merge_room(), 0);
    }

    fn take(admission: (Admission, u64)) -> Reservation {
        match admission.0 {
            Admission::Granted(reservation) => reservation,
            other => panic!("{other:?}"),
        }
    }

    /// Breaker repro: three 4247 MiB videos, 17007 MiB free. Two download,
    /// the third waits first in line; the two merges must not queue behind
    /// it (it waits for them), or nothing ever moves again.
    #[test]
    fn merges_never_wait_behind_a_waiting_download() {
        let (size, floor) = (4_247, 512);
        let merge = size + size / 16;
        let budget = std::sync::Arc::new(DiskBudget::default());
        let need = Need::Download { bytes: size, merge };
        let mut a = take(budget.admit(Some(17_007), need, floor, &mut None));
        let mut b = take(budget.admit(Some(17_007), need, floor, &mut None));
        let mut c = None;
        assert!(waits(&budget.admit(Some(17_007), need, floor, &mut c)));
        // Both downloaded: their bytes are on the disk.
        a.written(size, Some(17_007 - size));
        b.written(size, Some(17_007 - 2 * size));
        a.finish_download();
        b.finish_download();
        let free = 17_007 - 2 * size;
        assert!(waits(&budget.admit(Some(free), need, floor, &mut c)));
        let merge_a = budget.admit(Some(free), Need::Merge { bytes: merge }, floor, &mut None);
        assert!(granted(&merge_a), "{:?}", merge_a.0);
        drop(a);
        // B's merge waits for A's (which is writing), not for C.
        let mut b_merge = None;
        assert!(waits(&budget.admit(
            Some(free - merge),
            Need::Merge { bytes: merge },
            floor,
            &mut b_merge
        )));
        drop(merge_a);
        assert!(granted(&budget.admit(
            Some(free),
            Need::Merge { bytes: merge },
            floor,
            &mut b_merge
        )));
    }

    /// Breaker repro: a lone job whose tracks came out larger than the
    /// estimate. Its merge does not fit and nothing else will free space:
    /// refused, not a wait that never ends (and blocks every later job).
    #[test]
    fn a_merge_that_cannot_fit_is_refused() {
        let budget = std::sync::Arc::new(DiskBudget::default());
        let need = Need::Download {
            bytes: 1_000,
            merge: 1_062,
        };
        let mut job = take(budget.admit(Some(2_300), need, 100, &mut None));
        job.written(1_400, Some(900));
        job.finish_download();
        assert_eq!(budget.reserved(), 0);
        let mut ticket = None;
        let merge = budget.admit(Some(900), Need::Merge { bytes: 1_487 }, 100, &mut ticket);
        assert!(refused(&merge));
        assert!(ticket.is_none(), "a refused merge leaves the line");
        // Its leftover bytes no longer block anyone else.
        drop(job);
        assert!(granted(&budget.admit(
            Some(900),
            plain(700),
            100,
            &mut None
        )));
    }

    #[test]
    fn a_merge_first_in_line_that_cannot_fit_lets_the_next_go() {
        let budget = std::sync::Arc::new(DiskBudget::default());
        let writing = budget.hold(500);
        let mut big = None;
        let mut small = None;
        assert!(waits(&budget.admit(
            Some(1_000),
            Need::Merge { bytes: 900 },
            0,
            &mut big
        )));
        assert!(waits(&budget.admit(
            Some(1_000),
            Need::Merge { bytes: 300 },
            0,
            &mut small
        )));
        drop(writing);
        // Nothing in progress, and the big one still does not fit.
        assert!(refused(&budget.admit(
            Some(800),
            Need::Merge { bytes: 900 },
            0,
            &mut big
        )));
        assert!(granted(&budget.admit(
            Some(800),
            Need::Merge { bytes: 300 },
            0,
            &mut small
        )));
    }

    #[test]
    fn downloads_wait_behind_a_waiting_merge() {
        let budget = std::sync::Arc::new(DiskBudget::default());
        let writing = budget.hold(600);
        let mut merge = None;
        assert!(waits(&budget.admit(
            Some(1_000),
            Need::Merge { bytes: 500 },
            0,
            &mut merge
        )));
        // Would fit beside what is held, but the merge is first.
        assert!(waits(&budget.admit(Some(1_000), plain(100), 0, &mut None)));
        drop(writing);
        let merged = budget.admit(Some(1_000), Need::Merge { bytes: 500 }, 0, &mut merge);
        assert!(granted(&merged));
        assert!(granted(&budget.admit(
            Some(1_000),
            plain(100),
            0,
            &mut None
        )));
    }

    fn granted(admission: &(Admission, u64)) -> bool {
        matches!(admission.0, Admission::Granted(_))
    }
    fn waits(admission: &(Admission, u64)) -> bool {
        matches!(admission.0, Admission::Wait)
    }
    fn refused(admission: &(Admission, u64)) -> bool {
        matches!(admission.0, Admission::Refused)
    }

    #[test]
    fn admission_at_the_boundaries() {
        let floor = 10;
        let budget = std::sync::Arc::new(DiskBudget::default());
        // Nothing else held: an exact fit starts, one byte short is refused.
        assert!(granted(&budget.admit(
            Some(110),
            plain(100),
            floor,
            &mut None
        )));
        assert!(refused(&budget.admit(
            Some(109),
            plain(100),
            floor,
            &mut None
        )));
        assert!(granted(&budget.admit(None, plain(100), floor, &mut None)));
        // An unknown size still keeps the floor.
        assert!(refused(&budget.admit(Some(9), plain(0), floor, &mut None)));
        assert!(granted(&budget.admit(Some(10), plain(0), floor, &mut None)));
        assert_eq!(budget.reserved(), 0);

        let _held = budget.reserve(Some(1_000), 500, 0).unwrap();
        // Exactly fits beside what is held.
        let beside = budget.admit(Some(610), plain(100), floor, &mut None);
        assert!(granted(&beside));
        assert_eq!(beside.1, 500);
        drop(beside);
        // One byte short beside it: waits, in line.
        let mut ticket = None;
        assert!(waits(&budget.admit(
            Some(609),
            plain(100),
            floor,
            &mut ticket
        )));
        assert!(ticket.is_some());
        // Even a video larger than what is free now waits while others
        // hold space: a merging job frees its tracks when it is done.
        let mut big = None;
        assert!(waits(&budget.admit(Some(50), plain(100), floor, &mut big)));
    }

    #[test]
    fn waiters_take_space_in_order() {
        let budget = std::sync::Arc::new(DiskBudget::default());
        let held = budget.reserve(Some(1_000), 900, 0).unwrap();
        let mut big = None;
        assert!(waits(&budget.admit(Some(1_000), plain(800), 0, &mut big)));
        // Would fit beside the holder, but the big one is first in line.
        let mut small = None;
        assert!(waits(&budget.admit(Some(1_000), plain(50), 0, &mut small)));
        assert!(budget.reserve(Some(1_000), 50, 0).is_none());
        drop(held);
        assert!(waits(&budget.admit(Some(1_000), plain(50), 0, &mut small)));
        let (first, _) = budget.admit(Some(1_000), plain(800), 0, &mut big);
        assert!(matches!(first, Admission::Granted(_)));
        assert!(big.is_none(), "a granted job leaves the line");
        // What is left fits the small one.
        let (second, held) = budget.admit(Some(1_000), plain(50), 0, &mut small);
        assert!(matches!(second, Admission::Granted(_)));
        assert_eq!(held, 800);
        // Only one of two waiters fits: the other keeps waiting.
        let mut third = None;
        assert!(waits(&budget.admit(Some(1_000), plain(300), 0, &mut third)));
        drop((first, second));
        assert_eq!(budget.reserved(), 0);
        assert!(granted(&budget.admit(
            Some(1_000),
            plain(300),
            0,
            &mut third
        )));
    }

    #[test]
    fn a_waiter_that_gives_up_lets_the_next_one_go() {
        let budget = std::sync::Arc::new(DiskBudget::default());
        let held = budget.reserve(Some(1_000), 900, 0).unwrap();
        let mut first = None;
        let mut second = None;
        assert!(waits(&budget.admit(Some(1_000), plain(500), 0, &mut first)));
        assert!(waits(&budget.admit(
            Some(1_000),
            plain(500),
            0,
            &mut second
        )));
        drop(held);
        assert!(waits(&budget.admit(
            Some(1_000),
            plain(500),
            0,
            &mut second
        )));
        drop(first);
        assert!(granted(&budget.admit(
            Some(1_000),
            plain(500),
            0,
            &mut second
        )));
        assert!(second.is_none());
    }

    #[tokio::test]
    async fn giving_space_back_wakes_waiters() {
        let budget = std::sync::Arc::new(DiskBudget::default());
        let mut held = budget.reserve(Some(1_000), 900, 0).unwrap();
        let released = budget.released();
        tokio::pin!(released);
        released.as_mut().enable();
        // Writing moves bytes from the promise to the disk; nothing opens
        // up for a waiter, so it is not woken.
        held.written(400, Some(600));
        assert_eq!(budget.reserved(), 500);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), released.as_mut())
                .await
                .is_err()
        );
        drop(held);
        tokio::time::timeout(Duration::from_secs(5), released)
            .await
            .expect("not woken when space came back");
    }

    #[test]
    fn reservations_share_free_space_and_return_it() {
        let budget = std::sync::Arc::new(DiskBudget::default());
        let first = budget.reserve(Some(1_000), 600, 100).unwrap();
        // 1 000 free − 600 promised leaves 400: not enough for 400 + floor.
        assert!(budget.reserve(Some(1_000), 400, 100).is_none());
        let second = budget.reserve(Some(1_000), 300, 100).unwrap();
        assert_eq!(budget.reserved(), 900);
        drop(first);
        drop(second);
        assert_eq!(budget.reserved(), 0);
        assert!(budget.reserve(Some(1_000), 900, 100).is_some());
    }

    #[test]
    fn concurrent_reservations_never_overcommit() {
        let budget = std::sync::Arc::new(DiskBudget::default());
        let granted = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        std::thread::scope(|scope| {
            for _ in 0..16 {
                let budget = std::sync::Arc::clone(&budget);
                let granted = std::sync::Arc::clone(&granted);
                scope.spawn(move || {
                    if let Some(reservation) = budget.reserve(Some(10_000), 1_000, 500) {
                        granted.lock().unwrap().push(reservation);
                    }
                });
            }
        });
        // (10 000 − 500 floor) / 1 000 = 9 jobs fit.
        assert_eq!(granted.lock().unwrap().len(), 9);
        assert_eq!(budget.reserved(), 9_000);
    }

    #[test]
    fn written_bytes_are_not_counted_twice() {
        let budget = std::sync::Arc::new(DiskBudget::default());
        let mut first = budget.reserve(Some(1_000), 600, 100).unwrap();
        // The first job writes 300 bytes, but free space does not show them
        // yet (btrfs, ZFS and network mounts report late), or is unknown:
        // nothing is given back. A forged progress line gets no further.
        first.written(300, Some(1_000));
        first.written(300, None);
        assert_eq!(budget.reserved(), 600);
        // Free space shows 200 of them, then all 300: the disk has 700 free,
        // and the first job needs only 300 more.
        first.written(300, Some(800));
        assert_eq!(budget.reserved(), 400);
        first.written(300, Some(700));
        assert_eq!(budget.reserved(), 300);
        let mut second = budget.reserve(Some(700), 300, 100).unwrap();
        assert_eq!(budget.reserved(), 600);
        // Another job's 250 bytes lower free space too; the second job
        // wrote only 50 of them and gets back only those.
        second.written(50, Some(400));
        assert_eq!(budget.reserved(), 550);
        // Progress past the promise gives back all of it...
        first.written(900, Some(100));
        assert_eq!(budget.reserved(), 250);
        // ...and going backwards never grows it.
        second.written(10, Some(400));
        assert_eq!(budget.reserved(), 250);
        drop(first);
        drop(second);
        assert_eq!(budget.reserved(), 0);
    }

    #[test]
    fn unknown_free_space_at_reserve_gives_nothing_back() {
        let budget = std::sync::Arc::new(DiskBudget::default());
        let mut job = budget.reserve(None, 600, 100).unwrap();
        // Later readings have nothing to compare with.
        job.written(600, Some(1_000));
        assert_eq!(budget.reserved(), 600);
    }

    #[tokio::test]
    async fn job_dir_is_private_and_removed_unless_kept() {
        let parent = temp_directory("jobdir");
        let job = JobDir::create(&parent).await.unwrap();
        let path = job.path().to_path_buf();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        fs::write(path.join("video.mp4"), b"partial").unwrap();
        drop(job);
        assert!(!path.exists());

        let mut kept = JobDir::create(&parent).await.unwrap();
        kept.keep();
        let kept_path = kept.path().to_path_buf();
        drop(kept);
        assert!(kept_path.exists());
        // A recovery directory survives cleanup even once it is old.
        age(&kept_path);
        cleanup_stale_jobs(&parent).await;
        assert!(kept_path.exists());
        fs::remove_dir_all(parent).unwrap();
    }

    #[tokio::test]
    async fn output_dir_symlink_is_refused() {
        let parent = temp_directory("symlink");
        let real = parent.join("real");
        fs::create_dir(&real).unwrap();
        let link = parent.join("HighVi");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(
            prepare_output_dir(&link).await.unwrap_err().code,
            "download_directory_unavailable"
        );
        assert!(prepare_output_dir(&parent.join("fresh")).await.is_ok());
        fs::remove_dir_all(parent).unwrap();
    }

    fn age(path: &Path) {
        let old = SystemTime::now() - Duration::from_secs(3 * 24 * 60 * 60);
        fs::File::open(path).unwrap().set_modified(old).unwrap();
    }

    #[tokio::test]
    async fn stale_cleanup_never_removes_a_running_job() {
        let parent = temp_directory("live");
        let running = JobDir::create(&parent).await.unwrap();
        age(running.path());
        cleanup_stale_jobs(&parent).await;
        assert!(
            running.path().exists(),
            "an active job directory was deleted"
        );
        drop(running);
        fs::remove_dir_all(parent).unwrap();
    }

    #[tokio::test]
    async fn stale_cleanup_removes_crashed_jobs() {
        let parent = temp_directory("crashed");
        let crashed = parent.join(format!("{JOB_PREFIX}1-2-3"));
        fs::create_dir(&crashed).unwrap();
        fs::write(crashed.join(JOB_MARKER), b"").unwrap();
        fs::write(crashed.join("video.mp4.part"), b"partial").unwrap();
        age(&crashed);
        cleanup_stale_jobs(&parent).await;
        assert!(!crashed.exists());
        fs::remove_dir_all(parent).unwrap();
    }

    #[tokio::test]
    async fn stale_cleanup_never_follows_a_symlinked_output_directory() {
        let elsewhere = temp_directory("elsewhere");
        let crashed = elsewhere.join(format!("{JOB_PREFIX}1-2-3"));
        fs::create_dir(&crashed).unwrap();
        fs::write(crashed.join(JOB_MARKER), b"").unwrap();
        age(&crashed);
        let parent = temp_directory("linked");
        let link = parent.join("HighVi");
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
        cleanup_stale_jobs(&link).await;
        assert!(
            crashed.exists(),
            "cleanup followed a symlinked output directory"
        );
        fs::remove_dir_all(parent).unwrap();
        fs::remove_dir_all(elsewhere).unwrap();
    }

    #[tokio::test]
    async fn stale_cleanup_skips_unmarked_and_fresh_directories() {
        let parent = temp_directory("stale");
        let unmarked = parent.join(format!("{JOB_PREFIX}user-dir"));
        fs::create_dir(&unmarked).unwrap();
        let fresh = JobDir::create(&parent).await.unwrap();
        cleanup_stale_jobs(&parent).await;
        assert!(unmarked.exists());
        assert!(fresh.path().exists());
        drop(fresh);
        fs::remove_dir_all(parent).unwrap();
    }
}
