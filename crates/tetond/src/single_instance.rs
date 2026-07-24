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
    use std::time::{Duration, Instant};

    use super::*;

    /// How long the test lets a *transient* holder of the lock's open file
    /// description get out of the way. See [`acquire_within`].
    const RELEASE_WINDOW: Duration = Duration::from_secs(2);

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

    /// Acquire, retrying until `window` elapses, and report the attempts spent.
    ///
    /// `flock(2)` binds the lock to the **open file description**, not to the
    /// process, and releases it only once *every* descriptor referring to that
    /// description is closed. Every `std::process::Command` this test binary
    /// spawns — the shell tool's `sh -c`, the MCP client's subprocess tests —
    /// duplicates the whole descriptor table into the child at fork, so a fork
    /// that lands between this test's `acquire` and its `drop` leaves the child
    /// holding a copy of the lock's description. `O_CLOEXEC` closes it at the
    /// child's `exec`, but not before: for the few milliseconds of that
    /// fork→exec window the lock stays held by a process that has no idea it
    /// owns it, and the acquire that should have succeeded sees `EWOULDBLOCK`.
    /// That window is why this test is green alone and flaky in a parallel run
    /// of the whole binary — the fork has to land inside it.
    ///
    /// Retrying is the honest fix rather than a mask: the borrowed descriptor is
    /// *transient*, so what the test loses to it is time, not the property under
    /// test. A lock that is genuinely never released still fails, one attempt
    /// per 25ms until the window is out.
    fn acquire_within(path: &std::path::Path, window: Duration) -> (Option<SingleInstance>, u32) {
        let deadline = Instant::now() + window;
        let mut attempts = 0;
        loop {
            attempts += 1;
            match SingleInstance::acquire(path).expect("acquiring the lock must not error") {
                Some(instance) => return (Some(instance), attempts),
                None if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                None => return (None, attempts),
            }
        }
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

        // Retried, not asserted on the first try: another test in this binary can
        // have forked a child that transiently holds an inherited copy of the
        // lock's open file description — see [`acquire_within`].
        let (third, attempts) = acquire_within(&path, RELEASE_WINDOW);
        assert!(
            third.is_some(),
            "acquire should succeed after the lock frees (gave up after {attempts} \
             attempts over {RELEASE_WINDOW:?})"
        );

        drop(third);
        let _ = std::fs::remove_file(&path);
    }
}
