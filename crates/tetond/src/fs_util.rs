//! The daemon's one bounded file reader (REQ-583 verify pass).
//!
//! Two places open a file whose size and type they do not control and must not
//! be held hostage by: the session-root probe (`.git` and `HEAD`, a few KiB at
//! most, read on every turn) and `grep` (every candidate file under the root,
//! up to its per-file cap). Both used to carry their own copy of the same
//! three-step read — type check, size check, `take`n read — and the copies had
//! already drifted in *order*: one checked the type off `metadata` before the
//! open (so a FIFO was never opened), the other opened first and checked size
//! off the handle (so a FIFO would have blocked had the walker not screened it
//! out a step earlier). [`read_regular_file_bounded`] is the one reader, in the
//! safe order, documented once.

use std::io::Read;
use std::path::Path;

/// The contents of the **regular** file at `path`, read to at most `max_bytes`;
/// `None` for anything else — a directory, a FIFO, a device, a socket, a file
/// past the bound, an unreadable one, or one that is not UTF-8
/// (`read_to_string` refuses invalid UTF-8, the cheap binary heuristic both
/// callers always had).
///
/// The type and size are checked off `metadata` — following a symlink, which a
/// `.git` file or a `HEAD` may legitimately be — **before** the open, so a FIFO
/// (whose open for reading blocks until a writer appears) is never opened, and
/// the read is `take`n as well as size-checked so a file that grows under the
/// read stays bounded.
pub(crate) fn read_regular_file_bounded(path: &Path, max_bytes: u64) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        return None;
    }
    let mut contents = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(max_bytes)
        .read_to_string(&mut contents)
        .ok()?;
    Some(contents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_root(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "teton-fsutil-{tag}-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_regular_file_within_the_bound_is_read_whole() {
        let root = temp_root("ok");
        let file = root.join("f.txt");
        std::fs::write(&file, "alpha\nbeta\n").unwrap();
        assert_eq!(
            read_regular_file_bounded(&file, 64).as_deref(),
            Some("alpha\nbeta\n")
        );
        // Exactly at the bound still reads.
        assert_eq!(
            read_regular_file_bounded(&file, 11).as_deref(),
            Some("alpha\nbeta\n")
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// A file past the bound, a directory, a missing path and invalid UTF-8
    /// are each `None` — no partial read, no error.
    #[test]
    fn an_oversize_missing_non_regular_or_non_utf8_file_is_none() {
        let root = temp_root("none");
        let big = root.join("big");
        std::fs::write(&big, "x".repeat(100)).unwrap();
        assert_eq!(read_regular_file_bounded(&big, 99), None);
        assert_eq!(read_regular_file_bounded(&root, 1 << 20), None);
        assert_eq!(read_regular_file_bounded(&root.join("nope"), 1 << 20), None);
        let binary = root.join("bin");
        std::fs::write(&binary, b"\xff\xfe").unwrap();
        assert_eq!(read_regular_file_bounded(&binary, 1 << 20), None);
        std::fs::remove_dir_all(&root).ok();
    }

    /// A FIFO is refused off `metadata` before any open, so the call returns
    /// rather than blocking for a writer that never comes.
    #[test]
    fn a_fifo_is_none_without_blocking() {
        let root = temp_root("fifo");
        let fifo = root.join("pipe");
        crate::mkfifo(&fifo);
        let read = crate::with_deadline("read_regular_file_bounded on a FIFO", move || {
            read_regular_file_bounded(&fifo, 1 << 20)
        });
        assert_eq!(read, None);
        std::fs::remove_dir_all(&root).ok();
    }
}
