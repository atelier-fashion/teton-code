//! Where the daemon's Unix socket and single-instance lock live.
//!
//! The base directory is `$XDG_RUNTIME_DIR/teton` when the variable is set
//! (Linux, and anyone who opts in), else the macOS per-user location
//! `~/Library/Application Support/teton`. Both the socket and the lock file sit
//! side by side under that directory so a single lock guards a single socket.

use std::path::PathBuf;

/// The concrete paths the daemon uses at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonPaths {
    /// Path the daemon binds its `UnixListener` to.
    pub socket: PathBuf,
    /// Path of the advisory single-instance lock file.
    pub lock: PathBuf,
}

/// Resolves the socket and lock paths from the current environment.
#[must_use]
pub fn daemon_paths() -> DaemonPaths {
    let base = resolve_base_dir(
        std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    );
    DaemonPaths {
        socket: base.join("tetond.sock"),
        lock: base.join("tetond.lock"),
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

    #[test]
    fn falls_back_to_macos_app_support_without_xdg() {
        let base = resolve_base_dir(None, Some(PathBuf::from("/Users/x")));
        assert_eq!(
            base,
            PathBuf::from("/Users/x/Library/Application Support/teton")
        );
    }

    #[test]
    fn daemon_paths_share_a_base_directory() {
        let paths = daemon_paths();
        assert_eq!(paths.socket.parent(), paths.lock.parent());
        assert_eq!(paths.socket.file_name().unwrap(), "tetond.sock");
        assert_eq!(paths.lock.file_name().unwrap(), "tetond.lock");
    }
}
