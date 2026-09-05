//! The one-time move of durable state out of the runtime directory (BUG-211).
//!
//! Until v0.1.31 the daemon kept everything beside its socket, under
//! `teton_protocol::socket_path::resolve_base_dir`: on Linux that is
//! `$XDG_RUNTIME_DIR/teton`, a tmpfs the login session owns and removes at
//! logout. The socket, the lock and the startup log belong there. `cost.db`,
//! `config.toml`, the project registry, the model decision, the web cache and
//! the multi-gigabyte weights the user consented to download do not — and on
//! Linux every one of them was lost at logout.
//!
//! REQ-611 added the second resolver, `resolve_data_dir`, and used it for
//! transcripts alone, filing the rest as this bug. This module is the rest: the
//! durable stores now open under the data directory, and a daemon that finds
//! them under the old runtime directory **moves them once**, before anything
//! opens them.
//!
//! # Rules the move keeps
//!
//! - **Never overwrite.** An entry present at both paths is left where it is
//!   and reported as [`Outcome::KeptBoth`]; the daemon then opens the data-dir
//!   copy, which is the one it will keep writing. A user who wants the older
//!   file can still find it.
//! - **Rename first, copy second.** The two directories are on different
//!   filesystems whenever the runtime one is a tmpfs, so `rename` fails with
//!   `CrossesDevices` and the entry is copied and then removed. A copy that
//!   fails leaves the source untouched ([`Outcome::Failed`]) — a weights file
//!   the copy could not finish is still where the loader used to find it, and
//!   the loader still looks there through the migration's second half: the
//!   daemon never re-downloads a file it can copy.
//! - **The verification receipt survives a copy.** `install::deep_status`
//!   re-digests the bytes before trusting a receipt, so a copied `.gguf` whose
//!   `.verified` sidecar names an older mtime is re-verified, not re-fetched.
//! - **Same directory, no-op.** On macOS the two resolvers agree by default, so
//!   [`migrate_durable_state`] returns nothing and touches nothing.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// Every entry the daemon keeps that must outlive a login session, by the name
/// it has under the state directory.
///
/// The SQLite sidecars ride with `cost.db`: a daemon that stopped without a
/// clean checkpoint leaves a `-wal` beside the database, and moving the
/// database without it would silently drop the last committed rows.
pub const DURABLE_ENTRIES: &[&str] = &[
    "config.toml",
    "cost.db",
    "cost.db-wal",
    "cost.db-shm",
    "cost.db-journal",
    "projects.json",
    "model-selection.toml",
    "service-declined",
    "web-cache",
    "models",
];

/// What happened to one entry.
#[derive(Debug)]
pub enum Outcome {
    /// Moved by a single `rename`.
    Renamed { entry: &'static str },
    /// Copied across filesystems and then removed from the runtime directory.
    Copied { entry: &'static str },
    /// Present at both paths; neither was touched.
    KeptBoth { entry: &'static str },
    /// The move failed and the source was left in place.
    Failed {
        entry: &'static str,
        error: io::Error,
    },
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Outcome::Renamed { entry } => write!(f, "moved {entry}"),
            Outcome::Copied { entry } => write!(f, "copied {entry} across filesystems"),
            Outcome::KeptBoth { entry } => write!(
                f,
                "left {entry} in place: the data directory already holds one, which is the one \
                 the daemon will use"
            ),
            Outcome::Failed { entry, error } => {
                write!(f, "could not move {entry} ({error}); it stays where it was")
            }
        }
    }
}

/// Move every [`DURABLE_ENTRIES`] item that exists under `runtime_dir` and not
/// under `data_dir` into `data_dir`. Returns one [`Outcome`] per entry that was
/// found under `runtime_dir`; an empty vector means there was nothing to move.
///
/// A no-op when the two directories are the same place, which is the macOS
/// default and every configuration where the caller chose to keep them
/// together.
pub fn migrate_durable_state(runtime_dir: &Path, data_dir: &Path) -> Vec<Outcome> {
    if same_place(runtime_dir, data_dir) || !runtime_dir.is_dir() {
        return Vec::new();
    }
    let mut outcomes = Vec::new();
    for entry in DURABLE_ENTRIES {
        let source = runtime_dir.join(entry);
        if !source.exists() {
            continue;
        }
        let destination = data_dir.join(entry);
        if destination.exists() {
            outcomes.push(Outcome::KeptBoth { entry });
            continue;
        }
        outcomes.push(match move_entry(&source, &destination, false) {
            Ok(false) => Outcome::Renamed { entry },
            Ok(true) => Outcome::Copied { entry },
            Err(error) => Outcome::Failed { entry, error },
        });
    }
    outcomes
}

/// Whether two directories name the same place. Canonicalized when both
/// exist, so a symlinked home does not read as a second directory; compared
/// textually otherwise.
fn same_place(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Move one entry. Returns `Ok(false)` after a rename, `Ok(true)` after a copy.
///
/// `force_copy` is the test seam: a cross-device rename cannot be provoked on
/// one filesystem, and the copy half is the half that carries gigabytes.
pub(crate) fn move_entry(source: &Path, destination: &Path, force_copy: bool) -> io::Result<bool> {
    if let Some(parent) = destination.parent() {
        crate::auth::secure_socket_dir(parent)?;
    }
    if !force_copy {
        match std::fs::rename(source, destination) {
            Ok(()) => return Ok(false),
            Err(err) if err.kind() == io::ErrorKind::CrossesDevices => {}
            Err(err) => return Err(err),
        }
    }
    copy_recursively(source, destination)?;
    if source.is_dir() {
        std::fs::remove_dir_all(source)?;
    } else {
        std::fs::remove_file(source)?;
    }
    Ok(true)
}

/// Copy a file, or a directory tree, preserving nothing but bytes and the
/// owner-only mode the state directory is held at.
fn copy_recursively(source: &Path, destination: &Path) -> io::Result<()> {
    if source.is_dir() {
        std::fs::create_dir_all(destination)?;
        for child in std::fs::read_dir(source)? {
            let child = child?;
            copy_recursively(&child.path(), &destination.join(child.file_name()))?;
        }
        return Ok(());
    }
    // Copy into a sibling temp name and rename, so a copy that dies mid-way
    // never leaves a truncated file under the real name for the loader to
    // find (the same shape `install.rs` uses for `.part`).
    let partial = partial_name(destination);
    let copied = std::fs::copy(source, &partial);
    if let Err(err) = copied {
        let _ = std::fs::remove_file(&partial);
        return Err(err);
    }
    std::fs::rename(&partial, destination)
}

fn partial_name(destination: &Path) -> PathBuf {
    let mut name = destination.as_os_str().to_owned();
    name.push(".moving");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A temp pair no other test in this process can collide with (the
    /// counter, not the clock, guarantees it — `auth.rs`'s idiom).
    fn dirs(tag: &str) -> (PathBuf, PathBuf) {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "teton-state-dir-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        let runtime = root.join("run");
        let data = root.join("data");
        std::fs::create_dir_all(&runtime).unwrap();
        (runtime, data)
    }

    fn plant(runtime: &Path) {
        std::fs::write(
            runtime.join("config.toml"),
            "[transcript]\nenabled = true\n",
        )
        .unwrap();
        std::fs::write(runtime.join("cost.db"), b"sqlite bytes").unwrap();
        std::fs::write(runtime.join("cost.db-wal"), b"wal bytes").unwrap();
        std::fs::write(runtime.join("model-selection.toml"), "model = \"x\"\n").unwrap();
        std::fs::create_dir_all(runtime.join("models")).unwrap();
        std::fs::write(runtime.join("models/x.gguf"), vec![7u8; 4096]).unwrap();
        std::fs::write(runtime.join("models/x.gguf.verified"), "receipt").unwrap();
        std::fs::create_dir_all(runtime.join("web-cache/ab")).unwrap();
        std::fs::write(runtime.join("web-cache/ab/cd.json"), "{}").unwrap();
        // Runtime-only things that must **stay**.
        std::fs::write(runtime.join("tetond.lock"), "").unwrap();
        std::fs::write(runtime.join("tetond.log"), "log").unwrap();
    }

    /// **BUG-211.** Every durable entry crosses; the socket's siblings do not;
    /// and the bytes are the same bytes.
    ///
    /// Mutation: drop `"models"` from `DURABLE_ENTRIES` and the weights
    /// assertion goes red; drop the `-wal` sidecar and the WAL assertion does.
    #[test]
    fn durable_entries_move_and_runtime_entries_stay() {
        let (runtime, data) = dirs("move");
        plant(&runtime);
        let outcomes = migrate_durable_state(&runtime, &data);
        // Six top-level entries: two trees (`models`, `web-cache`) and four
        // files (`config.toml`, `cost.db`, its `-wal`, `model-selection.toml`).
        assert_eq!(outcomes.len(), 6, "{outcomes:?}");
        assert!(
            outcomes
                .iter()
                .all(|o| matches!(o, Outcome::Renamed { .. } | Outcome::Copied { .. })),
            "{outcomes:?}"
        );
        for entry in [
            "config.toml",
            "cost.db",
            "cost.db-wal",
            "model-selection.toml",
            "models/x.gguf",
            "models/x.gguf.verified",
            "web-cache/ab/cd.json",
        ] {
            assert!(data.join(entry).exists(), "{entry} did not arrive");
            assert!(!runtime.join(entry).exists(), "{entry} was left behind");
        }
        assert_eq!(
            std::fs::read(data.join("models/x.gguf")).unwrap(),
            vec![7u8; 4096]
        );
        assert_eq!(
            std::fs::read(data.join("cost.db-wal")).unwrap(),
            b"wal bytes"
        );
        assert!(
            runtime.join("tetond.lock").exists(),
            "the lock is runtime state"
        );
        assert!(
            runtime.join("tetond.log").exists(),
            "the log is runtime state"
        );
        // Idempotent: a second start finds nothing to move.
        assert!(migrate_durable_state(&runtime, &data).is_empty());
        let _ = std::fs::remove_dir_all(runtime.parent().unwrap());
    }

    /// An entry already present at the destination is never overwritten — and
    /// the source is not deleted either, so nothing is lost in either direction.
    #[test]
    fn an_entry_present_at_both_paths_is_kept_at_both() {
        let (runtime, data) = dirs("both");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(runtime.join("cost.db"), b"old").unwrap();
        std::fs::write(data.join("cost.db"), b"new").unwrap();
        let outcomes = migrate_durable_state(&runtime, &data);
        assert!(
            matches!(
                outcomes.as_slice(),
                [Outcome::KeptBoth { entry: "cost.db" }]
            ),
            "{outcomes:?}"
        );
        assert_eq!(std::fs::read(data.join("cost.db")).unwrap(), b"new");
        assert_eq!(std::fs::read(runtime.join("cost.db")).unwrap(), b"old");
        let _ = std::fs::remove_dir_all(runtime.parent().unwrap());
    }

    /// The same place — the macOS default — is a no-op, whichever spelling
    /// names it.
    #[test]
    fn the_same_directory_moves_nothing() {
        let (runtime, _data) = dirs("same");
        plant(&runtime);
        assert!(migrate_durable_state(&runtime, &runtime).is_empty());
        let dotted = runtime.join(".");
        assert!(migrate_durable_state(&runtime, &dotted).is_empty());
        assert!(runtime.join("cost.db").exists());
        let _ = std::fs::remove_dir_all(runtime.parent().unwrap());
    }

    /// The copy half, forced: a directory tree crosses byte for byte and the
    /// source is removed only after the whole tree landed. This is the arm a
    /// tmpfs runtime directory takes for the weights.
    #[test]
    fn the_copy_arm_moves_a_tree_and_removes_the_source() {
        let (runtime, data) = dirs("copy");
        plant(&runtime);
        let copied = move_entry(&runtime.join("models"), &data.join("models"), true).unwrap();
        assert!(copied, "the seam forces the copy arm");
        assert_eq!(
            std::fs::read(data.join("models/x.gguf")).unwrap(),
            vec![7u8; 4096]
        );
        assert!(data.join("models/x.gguf.verified").exists());
        assert!(
            !runtime.join("models").exists(),
            "the source tree is removed after the copy"
        );
        assert!(
            !data.join("models/x.gguf.moving").exists(),
            "no partial name survives a completed copy"
        );
        let _ = std::fs::remove_dir_all(runtime.parent().unwrap());
    }

    /// A runtime directory that does not exist is the first-run case, not an
    /// error.
    #[test]
    fn a_missing_runtime_directory_moves_nothing() {
        let (runtime, data) = dirs("missing");
        std::fs::remove_dir_all(&runtime).unwrap();
        assert!(migrate_durable_state(&runtime, &data).is_empty());
        assert!(!data.exists(), "nothing was created for nothing");
    }
}
