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
const STALE_JOB_AGE: Duration = Duration::from_secs(24 * 60 * 60);
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
pub fn publish_unique(source: &Path, directory: &Path, stem: &str) -> std::io::Result<PathBuf> {
    for attempt in 1..=MAX_NAME_ATTEMPTS {
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
        if is_file && try_lock(&marker) {
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

        let published = publish_unique(&source, &directory, "Clip").unwrap();
        assert_eq!(published, directory.join("Clip (3).mp4"));
        assert_eq!(fs::read(directory.join("Clip.mp4")).unwrap(), b"first");
        assert_eq!(fs::read(directory.join("Clip (2).mp4")).unwrap(), b"second");
        assert_eq!(fs::read(&published).unwrap(), b"third");
        fs::remove_dir_all(directory).unwrap();
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
