#[cfg(target_os = "linux")]
use std::{ffi::CString, os::unix::ffi::OsStrExt, path::Path};

#[cfg(target_os = "linux")]
pub fn rename_without_replacement(source: &Path, destination: &Path) -> std::io::Result<()> {
    let source = CString::new(source.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "source path contains NUL")
    })?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "destination path contains NUL",
        )
    })?;

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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::{fs, time::SystemTime};

    fn temp_directory() -> std::path::PathBuf {
        let unique = format!(
            "highvi-filesystem-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let path = std::env::temp_dir().join(unique);
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn publishes_without_replacing_an_existing_destination() {
        let directory = temp_directory();
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
    fn publishes_completed_file_atomically_when_destination_is_free() {
        let directory = temp_directory();
        let source = directory.join("job.mp4");
        let destination = directory.join("final.mp4");
        fs::write(&source, b"complete").unwrap();

        rename_without_replacement(&source, &destination).unwrap();
        assert!(!source.exists());
        assert_eq!(fs::read(&destination).unwrap(), b"complete");
        fs::remove_dir_all(directory).unwrap();
    }
}
