//! The `PATH` a daemon-spawned child receives (BUG-174).
//!
//! The daemon's own `PATH` is only as good as whatever started it. A daemon the
//! CLI spawned inherits the user's login-shell `PATH` and everything resolves. A
//! daemon **launchd** started inherits launchd's default —
//! `/usr/bin:/bin:/usr/sbin:/sbin` — which names no package-manager prefix at
//! all. Every Homebrew-installed binary then vanishes from every subprocess the
//! daemon spawns: `gh`, `rg`, `jq`, brew's `python3`, an MCP server invoked as
//! `npx …`, and `teton` itself. The reported symptom was an in-session agent
//! getting exit 127 and telling the user Teton was not installed, while running
//! inside Teton.
//!
//! Both spawn sites share this floor: the `shell` tool
//! ([`harness::tools::shell`](crate::harness::tools::shell)) and the stdio MCP
//! client ([`mcp::client`](crate::mcp::client)), which passes `PATH` through on
//! its allowlist and so inherits the same starved value.
//!
//! This is a **usability** floor, not a security control. It cannot grant a
//! child anything the daemon's own user could not already run, and the ordering
//! rule below keeps it from changing the meaning of a `PATH` that already works.

use std::path::Path;

/// Directories holding user-installed binaries that a supervisor-started
/// daemon's `PATH` routinely lacks.
pub(crate) const PATH_FLOOR: &[&str] = &[
    "/opt/homebrew/bin", // Homebrew, Apple Silicon
    "/opt/homebrew/sbin",
    "/usr/local/bin", // Homebrew on Intel, and the conventional local prefix
    "/usr/local/sbin",
    "/home/linuxbrew/.linuxbrew/bin", // Linuxbrew (the Linux CI leg, and Linux users)
];

/// The POSIX fallback, used only when flooring would otherwise yield nothing.
const POSIX_DEFAULT_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// `inherited`, plus any `floor` directory that exists here and is not already
/// named.
///
/// Floor entries are **appended, never prepended**. Where a directory already
/// appears in the inherited `PATH`, the inherited position is kept, so this can
/// only make more commands resolvable — it can never change which binary an
/// already-working `PATH` selects. That ordering is the whole safety argument: a
/// daemon started from a real login shell gets byte-identical behaviour.
///
/// `exists` is injected so the choice is testable without depending on what
/// happens to be installed on the machine running the test.
pub(crate) fn floored_path(
    inherited: Option<&str>,
    floor: &[&str],
    exists: &dyn Fn(&str) -> bool,
) -> String {
    let mut entries: Vec<&str> = inherited
        .unwrap_or_default()
        .split(':')
        .filter(|e| !e.is_empty())
        .collect();
    for dir in floor {
        if !entries.contains(dir) && exists(dir) {
            entries.push(dir);
        }
    }
    // An empty result would hand the child no `PATH` at all, which is a worse
    // failure than the one being fixed.
    if entries.is_empty() {
        return POSIX_DEFAULT_PATH.to_owned();
    }
    entries.join(":")
}

/// Apply [`floored_path`] to a set of environment pairs, in place, leaving
/// exactly one `PATH` behind.
///
/// Call this on the *inherited* pairs, before any explicitly declared per-child
/// variables are layered on: a child that declares its own `PATH` has said what
/// it wants and must override this untouched.
pub(crate) fn apply_path_floor(env: &mut Vec<(String, String)>) {
    let inherited = env
        .iter()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| v.as_str());
    let floored = floored_path(inherited, PATH_FLOOR, &|dir| Path::new(dir).is_dir());
    env.retain(|(k, _)| k != "PATH");
    env.push(("PATH".to_owned(), floored));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything exists, so the floor is purely additive.
    fn all_exist(_: &str) -> bool {
        true
    }

    /// The reported failure: launchd's default `PATH` names no package-manager
    /// prefix, so a Homebrew `teton` cannot be found from inside a session.
    #[test]
    fn a_launchd_path_gains_the_package_manager_prefixes() {
        let floored = floored_path(
            Some("/usr/bin:/bin:/usr/sbin:/sbin"),
            &["/opt/homebrew/bin", "/usr/local/bin"],
            &all_exist,
        );
        assert!(floored.contains("/opt/homebrew/bin"), "{floored}");
        assert!(floored.contains("/usr/local/bin"), "{floored}");
        // The inherited entries are still there, and still first.
        assert!(
            floored.starts_with("/usr/bin:/bin:/usr/sbin:/sbin"),
            "{floored}"
        );
    }

    /// The safety property: a daemon started from a real login shell must get
    /// byte-identical behaviour. Floor entries already present are not moved,
    /// so the floor can never change which binary an existing `PATH` selects.
    #[test]
    fn an_inherited_entry_keeps_its_position_and_is_not_duplicated() {
        let floored = floored_path(
            Some("/opt/homebrew/bin:/usr/bin"),
            &["/usr/local/bin", "/opt/homebrew/bin"],
            &all_exist,
        );
        assert_eq!(floored, "/opt/homebrew/bin:/usr/bin:/usr/local/bin");
        assert_eq!(
            floored.matches("/opt/homebrew/bin").count(),
            1,
            "no duplicate entry: {floored}"
        );
    }

    /// A floor directory that does not exist on this machine is not added — an
    /// Apple Silicon prefix must not be pasted onto a Linux box's `PATH`.
    #[test]
    fn a_missing_floor_directory_is_not_added() {
        let floored = floored_path(Some("/usr/bin"), &["/opt/homebrew/bin"], &|_| false);
        assert_eq!(floored, "/usr/bin");
    }

    /// Handing the child an empty `PATH` would be a worse break than the one
    /// being fixed, so the POSIX default is the floor of the floor.
    #[test]
    fn an_absent_path_falls_back_to_the_posix_default() {
        assert_eq!(
            floored_path(None, &["/opt/homebrew/bin"], &|_| false),
            POSIX_DEFAULT_PATH
        );
        assert_eq!(floored_path(Some(""), &[], &all_exist), POSIX_DEFAULT_PATH);
    }

    /// The floor is applied to the pairs a child actually receives, and leaves
    /// exactly one `PATH` behind.
    #[test]
    fn apply_path_floor_replaces_rather_than_appends_a_second_path() {
        let mut env = vec![
            ("PATH".to_owned(), "/usr/bin".to_owned()),
            ("EDITOR".to_owned(), "vi".to_owned()),
        ];
        apply_path_floor(&mut env);
        assert_eq!(env.iter().filter(|(k, _)| k == "PATH").count(), 1);
        assert!(env.iter().any(|(k, v)| k == "EDITOR" && v == "vi"));
        let path = env.iter().find(|(k, _)| k == "PATH").unwrap().1.clone();
        assert!(path.starts_with("/usr/bin"), "{path}");
    }
}
