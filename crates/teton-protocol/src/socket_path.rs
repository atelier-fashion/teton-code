//! Where the daemon's Unix socket and single-instance lock live.
//!
//! This lives in the shared `teton-protocol` crate (not in either binary) because
//! the daemon and every client MUST resolve the socket to the *same* path — a
//! binary cannot depend on another binary, so before REQ-544 both `tetond` and
//! the `teton` CLI carried byte-identical copies of this logic that had to be
//! kept in sync by hand. One shared resolver removes that drift risk.
//!
//! The base directory is `$XDG_RUNTIME_DIR/teton` when the variable is set
//! (Linux, and anyone who opts in), else the macOS per-user location
//! `~/Library/Application Support/teton`, else the OS temp dir. Both the socket
//! and the lock file sit side by side under that directory so a single lock
//! guards a single socket.

use std::path::PathBuf;

/// The concrete socket, lock, and log paths the daemon uses and every client
/// dials or reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonPaths {
    /// Path the daemon binds its `UnixListener` to.
    pub socket: PathBuf,
    /// Path of the advisory single-instance lock file.
    pub lock: PathBuf,
    /// Where an autostarted daemon's stderr is captured (H-1 / E-4).
    ///
    /// A daemon the CLI spawned has no terminal, so a startup diagnostic written
    /// to stderr — a refused config, a failed bind — would go to `/dev/null` and
    /// the user would see only "could not reach the daemon". Capturing it to a
    /// file beside the socket is what lets `teton` quote the actual cause back.
    pub log: PathBuf,
    /// The known-project registry (REQ-584 BR-5, ADR-2).
    ///
    /// **Here rather than computed where it is used**, for the reason the three
    /// paths above are here: `resolve_base_dir` is the one home for "where this
    /// daemon keeps its things", and a second derivation could disagree with it.
    /// It also means every test is isolated for free — the harness already sets
    /// `XDG_RUNTIME_DIR`, so a suite can never read or clobber the real
    /// machine's project list.
    ///
    /// Unlike the socket and lock, this one is **persistent state** rather than
    /// runtime state. On a Linux box whose `XDG_RUNTIME_DIR` is a tmpfs the
    /// registry is therefore forgotten on reboot. That is acceptable and not
    /// worth a second base directory: BR-1 rebuilds it from use, and BR-3's
    /// scan rebuilds it on demand — the registry is a cache, and losing a cache
    /// costs one scan.
    pub projects: PathBuf,
}

/// The known-project registry inside `base` (REQ-584 ADR-2).
///
/// **One spelling, two callers.** `daemon_paths()` uses it, and so does the
/// runtime — which is handed the base directory rather than a `DaemonPaths` and
/// would otherwise have to join the filename itself. Two joins is two answers
/// to a question ADR-2 says has one, and the one that drifts is the one no test
/// covers.
#[must_use]
pub fn projects_path(base: &std::path::Path) -> PathBuf {
    base.join("projects.json")
}

/// Resolves the socket, lock, and log paths from the current environment.
#[must_use]
pub fn daemon_paths() -> DaemonPaths {
    let base = resolve_base_dir(
        std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    );
    DaemonPaths {
        socket: base.join("tetond.sock"),
        lock: base.join("tetond.lock"),
        log: base.join("tetond.log"),
        projects: projects_path(&base),
    }
}

/// Chooses the base directory from the two environment inputs.
///
/// Kept pure (no direct env reads) so the precedence rule is unit-testable
/// without mutating process-global state.
#[must_use]
pub fn resolve_base_dir(xdg_runtime_dir: Option<PathBuf>, home: Option<PathBuf>) -> PathBuf {
    if let Some(xdg) = xdg_runtime_dir {
        return xdg.join("teton");
    }
    if let Some(home) = home {
        return home.join("Library/Application Support/teton");
    }
    // Neither variable is set (unusual); fall back to the OS temp dir so the
    // daemon still has somewhere to bind rather than panicking.
    std::env::temp_dir().join("teton")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_runtime_dir_wins_when_set() {
        let base = resolve_base_dir(
            Some(PathBuf::from("/run/user/1000")),
            Some(PathBuf::from("/home/x")),
        );
        assert_eq!(base, PathBuf::from("/run/user/1000/teton"));
    }

    /// REQ-584 ADR-2: the registry shares the daemon's one base directory.
    ///
    /// Asserted as a *relationship* to the socket rather than against a literal
    /// path — a test that re-derived the expected path would be the second
    /// derivation this field exists to prevent.
    #[test]
    fn the_project_registry_sits_in_the_same_base_as_the_socket() {
        let paths = daemon_paths();
        assert_eq!(
            paths.projects.parent(),
            paths.socket.parent(),
            "the registry must live beside the socket, lock and log"
        );
        assert_eq!(
            paths.projects.file_name().and_then(|n| n.to_str()),
            Some("projects.json")
        );
    }

    #[test]
    fn falls_back_to_macos_app_support_without_xdg() {
        let base = resolve_base_dir(None, Some(PathBuf::from("/Users/x")));
        assert_eq!(
            base,
            PathBuf::from("/Users/x/Library/Application Support/teton")
        );
    }

    #[test]
    fn daemon_paths_share_a_base_and_name_socket_and_lock() {
        let paths = daemon_paths();
        assert_eq!(paths.socket.parent(), paths.lock.parent());
        // The startup log lives beside them, so a CLI that knows where to dial
        // also knows where the daemon's own diagnostics landed (H-1 / E-4).
        assert_eq!(paths.socket.parent(), paths.log.parent());
        assert_eq!(paths.socket.file_name().unwrap(), "tetond.sock");
        assert_eq!(paths.lock.file_name().unwrap(), "tetond.lock");
        assert_eq!(paths.log.file_name().unwrap(), "tetond.log");
    }
}
