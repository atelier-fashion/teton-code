//! Shared binary-resolution helpers for the `teton` crate's e2e suites.
//!
//! Both `cli_e2e` and `pty_e2e` spawn the real CLI against a real daemon, so
//! both need the same answer to "which binaries am I testing?". That answer is
//! not symmetric between the two, which is the whole point of this module.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

/// Path to the `teton` CLI under test.
///
/// Cargo guarantees this is rebuilt for every run, because `teton` is a binary
/// of *this* package and the variable is set by Cargo itself.
pub fn teton_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_teton"))
}

/// Path to the `teton-code` daemon, refusing to return a stale one.
///
/// The daemon cannot be reached the way `teton` is (BUG-164). Cargo sets
/// `CARGO_BIN_EXE_<name>` only for binaries belonging to the *same package* as
/// the integration test, and `teton-code` is a binary of `tetond`; referencing
/// `env!("CARGO_BIN_EXE_teton-code")` here is a compile error, not a fallback.
/// `teton` also declares no dependency on `tetond`, so nothing in the dependency
/// graph obliges Cargo to build the daemon for a `-p teton` run.
///
/// The path therefore has to be derived by joining onto `teton`'s directory, and
/// that yields a valid-looking path to whatever daemon was built last —
/// arbitrarily older than the change under test. The previous guard only checked
/// that the file *existed*, so a targeted run reported PASS against a daemon
/// that did not contain the change.
///
/// Existence and freshness are different properties. This checks the one that
/// matters: if the daemon is older than any source it is built from, the suite
/// refuses to run and says how to fix it, rather than passing against the wrong
/// binary. Under `cargo test --workspace` the daemon is always current and this
/// is a few `stat` calls.
///
/// Freshness is checked rather than *repaired* (by shelling out to `cargo build`)
/// deliberately: invoking Cargo from inside a Cargo-spawned test process
/// perturbed the pty suite in ways that were not worth carrying in a test
/// harness. Refusing is honest and has no such interaction.
pub fn daemon_bin() -> PathBuf {
    static DAEMON: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    match DAEMON.get_or_init(resolve_daemon) {
        Ok(path) => path.clone(),
        Err(why) => panic!("{why}"),
    }
}

fn resolve_daemon() -> Result<PathBuf, String> {
    let daemon = teton_bin()
        .parent()
        .ok_or("the `teton` binary has no parent directory")?
        .join("teton-code");

    let Ok(built) = mtime(&daemon) else {
        return Err(format!(
            "the `teton-code` daemon is not built at {}.\n\
             Run `cargo build --workspace` (or `cargo test --workspace`) first: a \
             `-p teton` run does not build the daemon, because `teton` declares no \
             dependency on `tetond` (BUG-164).",
            daemon.display()
        ));
    };

    if let Some((newest, source)) = newest_daemon_input() {
        if newest > built {
            return Err(format!(
                "the `teton-code` daemon at {} is older than {}.\n\
                 The suite refuses to run rather than report a pass against a stale \
                 daemon (BUG-164) — a `-p teton` run does not rebuild it.\n\
                 Run `cargo build --workspace` (or `cargo test --workspace`).",
                daemon.display(),
                source.display()
            ));
        }
    }

    Ok(daemon)
}

/// Newest mtime among the sources `teton-code` is built from, with the file it
/// came from (for the error message).
///
/// Scoped to the daemon's own inputs — the `tetond` crate and the library crates
/// it links — so editing only the CLI does not read as a stale daemon. A source
/// tree that cannot be walked yields `None`: this guard fails open on its own
/// bookkeeping, because refusing to run over an unreadable directory would be
/// worse than the staleness it protects against.
fn newest_daemon_input() -> Option<(SystemTime, PathBuf)> {
    const DAEMON_CRATES: [&str; 5] = [
        "tetond",
        "teton-core",
        "teton-protocol",
        "teton-providers",
        "teton-inference",
    ];

    let crates_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()?
        .to_path_buf();
    let workspace = crates_dir.parent()?.to_path_buf();

    let mut newest: Option<(SystemTime, PathBuf)> = None;
    let mut consider = |path: PathBuf| {
        if let Ok(t) = mtime(&path) {
            if newest.as_ref().is_none_or(|(best, _)| t > *best) {
                newest = Some((t, path));
            }
        }
    };

    consider(workspace.join("Cargo.lock"));
    consider(workspace.join("Cargo.toml"));
    for name in DAEMON_CRATES {
        let root = crates_dir.join(name);
        consider(root.join("Cargo.toml"));
        collect_rs(&root.join("src"), &mut consider);
    }

    newest
}

/// Recursively feed every `.rs` file under `dir` to `consider`.
fn collect_rs(dir: &Path, consider: &mut impl FnMut(PathBuf)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => collect_rs(&path, consider),
            Ok(t) if t.is_file() && path.extension().is_some_and(|e| e == "rs") => {
                consider(path);
            }
            _ => {}
        }
    }
}

fn mtime(path: &Path) -> std::io::Result<SystemTime> {
    std::fs::metadata(path)?.modified()
}
