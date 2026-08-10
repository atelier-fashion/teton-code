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
use std::time::{Duration, Instant};

/// How long [`SingleInstance::acquire_within`] waits for a predecessor to
/// finish releasing the lock before concluding another daemon is genuinely
/// live (REQ-565 D-6).
pub const DEFAULT_ACQUIRE_WINDOW: Duration = Duration::from_secs(5);

/// How often the wait re-attempts inside that window.
const RETRY_INTERVAL: Duration = Duration::from_millis(25);

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

    /// [`Self::acquire`], retried until `window` elapses (REQ-565 D-6).
    ///
    /// # Why a successor has to wait
    ///
    /// Exit-on-last-client puts a *departing* daemon and an *arriving* one in
    /// the same instant. The departing one holds this lock until after it has
    /// unlinked the socket — that ordering is what stops a racing autostart
    /// from finding a stale socket (BR-3) — but it also means a successor
    /// spawned during that teardown sees `EWOULDBLOCK`.
    ///
    /// Without a wait, that successor prints "already running" and exits 0,
    /// after which the CLI polls a socket path that nobody will ever bind and
    /// reports "could not reach the daemon after autostart". The user sees a
    /// failure caused entirely by good timing.
    ///
    /// A predecessor mid-teardown is transient by construction, so waiting for
    /// it costs milliseconds. A daemon that is genuinely alive still yields
    /// "already running", just `window` later — and in that case the CLI's
    /// *first* connect would have succeeded, so this slow path is unreachable
    /// on the common route.
    ///
    /// # Errors
    ///
    /// Returns an OS error if the lock file cannot be created or `flock` fails
    /// for a reason other than the lock being held.
    pub fn acquire_within(lock_path: &Path, window: Duration) -> io::Result<Option<Self>> {
        let deadline = Instant::now() + window;
        loop {
            match Self::acquire(lock_path)? {
                Some(instance) => return Ok(Some(instance)),
                None if Instant::now() < deadline => std::thread::sleep(RETRY_INTERVAL),
                None => return Ok(None),
            }
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

    /// A lock path no other test in this binary can be handed.
    ///
    /// The counter is not belt-and-braces. `SystemTime::now()` is not
    /// guaranteed to advance between two calls — its granularity is coarser
    /// than a nanosecond on macOS — so two tests calling this from different
    /// threads inside one tick used to receive the *same* path, and then
    /// genuinely contended for one lock. That reads as a flaky
    /// "acquire returned None" in whichever test lost, which is a bug in the
    /// fixture masquerading as a bug in the code under test. A monotonic
    /// counter makes the path unique by construction rather than by timing.
    fn temp_lock() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "teton-lock-{}-{}-{}.lock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed),
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

    // -----------------------------------------------------------------------
    // acquire_within — REQ-565 D-6
    // -----------------------------------------------------------------------

    /// The successor case. A predecessor still finishing its teardown holds the
    /// lock for a moment; without the wait, the successor would report "already
    /// running" and exit, leaving the CLI polling a socket nobody will bind.
    #[test]
    fn a_successor_wins_the_lock_once_a_departing_predecessor_releases_it() {
        let path = temp_lock();
        let predecessor = SingleInstance::acquire(&path).unwrap();
        assert!(predecessor.is_some());

        // Release it shortly, as a teardown would.
        let releaser = {
            let predecessor = predecessor;
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                drop(predecessor);
            })
        };

        let started = Instant::now();
        let successor = SingleInstance::acquire_within(&path, Duration::from_secs(5))
            .expect("acquiring must not error");
        assert!(
            successor.is_some(),
            "the successor must get the lock once the predecessor releases it"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(100),
            "the successor should have waited rather than succeeding instantly"
        );

        releaser.join().unwrap();
        drop(successor);
        let _ = std::fs::remove_file(&path);
    }

    /// The wait must not turn a genuinely live daemon into a second one. A
    /// holder that never lets go still yields "already running" — just later.
    #[test]
    fn a_lock_that_is_never_released_still_reports_already_running() {
        let path = temp_lock();
        let held = SingleInstance::acquire(&path).unwrap();
        assert!(held.is_some());

        let second = SingleInstance::acquire_within(&path, Duration::from_millis(150))
            .expect("acquiring must not error");
        assert!(
            second.is_none(),
            "a live holder must still be reported as already running"
        );

        drop(held);
        let _ = std::fs::remove_file(&path);
    }

    /// A free lock costs nothing — the wait is a fallback, not a delay on the
    /// common path (every ordinary autostart takes this branch).
    #[test]
    fn an_uncontended_lock_is_acquired_without_waiting() {
        let path = temp_lock();
        let started = Instant::now();
        let instance = SingleInstance::acquire_within(&path, Duration::from_secs(5)).unwrap();
        assert!(instance.is_some());
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "an uncontended acquire must not pay the retry window"
        );
        drop(instance);
        let _ = std::fs::remove_file(&path);
    }
}
