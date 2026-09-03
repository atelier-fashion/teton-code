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
//!
//! # The base directory and the *data* directory are not the same question
//!
//! [`resolve_base_dir`] answers "where does this daemon's **runtime** state
//! live" — a socket, a lock, a log, a rebuildable cache. On Linux that is
//! `$XDG_RUNTIME_DIR`, a tmpfs cleared at logout, which is the correct place
//! for exactly those things and the wrong place for anything the user expects
//! to still be there tomorrow. [`resolve_data_dir`] answers the other question
//! (REQ-611 ADR-4), and it is deliberately a **second** resolver rather than a
//! redefinition of the first: relocating the socket would break every running
//! client, and relocating `cost.db` in the same change would silently migrate a
//! store whose tests assume the current place. On macOS the two agree, so the
//! default install is unchanged either way.

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

/// Where user data that must outlive a logout is kept (REQ-611 ADR-4).
///
/// The composition sibling of [`daemon_paths`], reading `$XDG_DATA_HOME` and
/// `$HOME` and handing them to [`resolve_data_dir`]. Returns the directory
/// itself rather than a path *inside* it, because its only consumer today
/// derives its own subdirectory from it
/// (`TranscriptConfig::effective_dir`) and a second store added later must be
/// able to do the same.
#[must_use]
pub fn data_dir() -> PathBuf {
    resolve_data_dir(
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

/// The `$HOME`-relative data directory for this platform.
///
/// `cfg` rather than a runtime branch because the answer is a property of the
/// build target, and one constant rather than two literals because the whole
/// point of a resolver is that the location has a single spelling.
#[cfg(target_os = "macos")]
const HOME_DATA_SUFFIX: &str = "Library/Application Support/teton";
/// The `$HOME`-relative data directory for this platform — the XDG default,
/// which is what `$XDG_DATA_HOME` means when it is unset.
#[cfg(not(target_os = "macos"))]
const HOME_DATA_SUFFIX: &str = ".local/share/teton";

/// Chooses the **data** directory from the two environment inputs (REQ-611
/// ADR-4).
///
/// Pure, for [`resolve_base_dir`]'s reason: the precedence rule is unit-testable
/// without mutating process-global state.
///
/// # Why this is not `resolve_base_dir`
///
/// The precedence *shape* is the same — an explicit XDG variable, then a
/// home-relative default, then the OS temp dir — and the middle step is where
/// they diverge. `resolve_base_dir` falls back to the macOS location on every
/// platform, which is right for a socket (on Linux the variable it prefers is
/// effectively always set) and wrong here: a Linux box with `$XDG_DATA_HOME`
/// unset is the *ordinary* case, since XDG says an unset variable means
/// `~/.local/share`. Falling back to `Library/Application Support` there would
/// put a 30-day retention policy in a directory no Linux tool knows about.
///
/// The temp-dir last resort is shared with [`resolve_base_dir`] and is worth
/// naming as the compromise it is: with neither variable set there is no
/// durable place this function can name, and a daemon that still runs — writing
/// records that may not survive a reboot — beats one that panics at startup.
#[must_use]
pub fn resolve_data_dir(xdg_data_home: Option<PathBuf>, home: Option<PathBuf>) -> PathBuf {
    if let Some(xdg) = xdg_data_home {
        return xdg.join("teton");
    }
    if let Some(home) = home {
        return home.join(HOME_DATA_SUFFIX);
    }
    std::env::temp_dir().join("teton")
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

    /// **REQ-611 ADR-4 / TASK-360.** The data directory's precedence, in the
    /// table shape `resolve_base_dir`'s own cases have one row of each.
    ///
    /// The home-fallback row is platform-selected because the resolver is, and
    /// it is written as a **literal per platform** rather than by joining
    /// `HOME_DATA_SUFFIX`: an expectation computed from the subject's own
    /// constant would agree with any value that constant ever held
    /// (LESSON-569).
    ///
    /// **Mutation** (LESSON-441): swap the two `HOME_DATA_SUFFIX` definitions
    /// so the `cfg(target_os = "macos")` arm carries `.local/share/teton` —
    /// the third row goes red on macOS and on Linux alike. Restored.
    #[test]
    fn resolve_data_dir_prefers_xdg_data_home_then_the_home_form_then_temp() {
        let home_form = if cfg!(target_os = "macos") {
            PathBuf::from("/Users/x/Library/Application Support/teton")
        } else {
            PathBuf::from("/Users/x/.local/share/teton")
        };
        let cases: [(Option<PathBuf>, Option<PathBuf>, PathBuf); 4] = [
            // The variable wins even where a home is available to fall back to.
            (
                Some(PathBuf::from("/home/x/.local/share")),
                Some(PathBuf::from("/Users/x")),
                PathBuf::from("/home/x/.local/share/teton"),
            ),
            (
                Some(PathBuf::from("/home/x/.local/share")),
                None,
                PathBuf::from("/home/x/.local/share/teton"),
            ),
            (None, Some(PathBuf::from("/Users/x")), home_form),
            // Neither set: somewhere to write beats refusing to start.
            (None, None, std::env::temp_dir().join("teton")),
        ];
        for (xdg, home, expected) in cases {
            assert_eq!(
                resolve_data_dir(xdg.clone(), home.clone()),
                expected,
                "xdg_data_home={xdg:?}, home={home:?}"
            );
        }
    }

    /// **REQ-611 ADR-4.** The whole reason for a second resolver: on a
    /// Linux-style environment the data directory is *not* the runtime base,
    /// which is a tmpfs cleared at logout and would contradict a 30-day
    /// retention policy.
    ///
    /// Asserted as a relationship between the two resolvers rather than against
    /// a literal, so it keeps holding if either location is ever moved.
    #[test]
    fn the_data_dir_is_not_the_logout_cleared_runtime_base() {
        let home = PathBuf::from("/home/x");
        let runtime = resolve_base_dir(Some(PathBuf::from("/run/user/1000")), Some(home.clone()));
        let data = resolve_data_dir(Some(PathBuf::from("/home/x/.local/share")), Some(home));
        assert_ne!(
            runtime, data,
            "transcripts would be cleared at logout with the socket"
        );
        assert!(
            !data.starts_with("/run/user"),
            "the data directory resolved under the runtime directory: {data:?}"
        );
    }

    /// The composition reads the environment and still names the one directory
    /// every teton store hangs off — the property `daemon_paths`'s own tests
    /// assert about the socket's parent.
    #[test]
    fn data_dir_composes_from_the_environment_and_names_teton() {
        assert_eq!(
            data_dir().file_name().and_then(|n| n.to_str()),
            Some("teton")
        );
        assert!(data_dir().is_absolute() || data_dir().starts_with(std::env::temp_dir()));
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
