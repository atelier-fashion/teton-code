//! Single-instance enforcement via an advisory `flock(2)` on a lock file.
//!
//! On startup the daemon opens a lock file and takes a non-blocking exclusive
//! `flock`. If another live daemon already holds it, the lock is denied and the
//! second process exits cleanly with an "already running" notice. The lock is
//! owned by the open file description, so it is released automatically when the
//! process exits (the [`SingleInstance`] guard keeps the file — and thus the
//! lock — alive for the process lifetime).

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

/// RAII guard proving this process holds the single-instance lock.
///
/// Dropping it (at process exit) closes the file descriptor and releases the
/// `flock`, letting a future daemon start.
pub struct SingleInstance {
    // Held solely to keep the flocked file descriptor open for the process
    // lifetime; never read.
    #[allow(dead_code)]
    file: File,
}

impl SingleInstance {
    /// Attempts to acquire the lock.
    ///
    /// Returns `Ok(Some(guard))` when this process now holds the lock, or
    /// `Ok(None)` when another live daemon already holds it (the caller should
    /// report "already running" and exit).
    ///
    /// # Errors
    ///
    /// Returns an OS error if the lock directory or file cannot be created, or
    /// if `flock` fails for a reason other than the lock being held.
    pub fn acquire(lock_path: &Path) -> io::Result<Option<Self>> {
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(lock_path)?;

        // SAFETY: `file.as_raw_fd()` is a valid open descriptor for the call.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(Some(Self { file }));
        }

        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            // The lock is held by another process — not a hard error.
            Some(code) if code == libc::EWOULDBLOCK => Ok(None),
            _ => Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_lock() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "teton-lock-{}-{}.lock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn second_acquire_is_refused_until_the_first_is_dropped() {
        let path = temp_lock();

        let first = SingleInstance::acquire(&path).unwrap();
        assert!(first.is_some(), "first acquire should succeed");

        let second = SingleInstance::acquire(&path).unwrap();
        assert!(
            second.is_none(),
            "second acquire should report already-running"
        );

        drop(first);

        let third = SingleInstance::acquire(&path).unwrap();
        assert!(
            third.is_some(),
            "acquire should succeed after the lock frees"
        );

        drop(third);
        let _ = std::fs::remove_file(&path);
    }
}
