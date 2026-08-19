//! The session-root probe (REQ-583 ADR-1): the I/O half of "what ground does
//! this session stand on".
//!
//! [`probe`] turns the one stored fact — the session's path — into the wire view
//! every surface renders ([`SessionRoot`]): kind, display, project name, branch.
//! It is called *per use* (each turn, at `session/create`, on `/cd`) and never
//! cached, so a `/cd` that rewrites the path or a checkout between turns moves
//! every consumer on the next probe with no second source of truth. The cost is
//! a handful of `exists()` calls and one small file read.
//!
//! The decisions live in [`teton_core::session_root`] (pure, no I/O); this
//! module only answers the two questions that need the filesystem — *is a
//! marker present* and *what does `.git/HEAD` say* — and hands the answers
//! over. Nothing here shells out: the branch is read from `HEAD` directly, and
//! a value that cannot be read is `None`, never a guess (BR-1).

use std::path::{Path, PathBuf};

use teton_core::session_root::{
    bounded_field, classify, display_for, DISPLAY_MAX_CHARS, NAME_MAX_CHARS, PROJECT_MARKERS,
};
use teton_protocol::methods::{RootKind, SessionRoot};

/// Derive the session root for `path`, given the caller's `HOME` (or `None`).
///
/// - `kind`: [`classify`] over whether any [`PROJECT_MARKERS`] entry exists at
///   `path` as a file **or** a directory (a linked git worktree's `.git` is a
///   file naming its `gitdir:`).
/// - `project_name`: the bounded basename, present iff `kind == Project`.
/// - `vcs_branch`: from `.git/HEAD` (following a `gitdir:` file), present iff
///   `kind == Project` and `HEAD` names a branch — a detached SHA, an unreadable
///   `HEAD`, or a non-git project all yield `None`. A home-kind root with a
///   `~/.git` says no branch: home wins over the marker (BR-4), so the branch is
///   not read there at all.
/// - `display`: [`display_for`], bounded to [`DISPLAY_MAX_CHARS`].
///
/// The three user-controlled strings are bounded here, once, so the environment
/// block, the jail refusal, the launch notice and the `/cd` line all print the
/// same bytes (ADR-2 "built once").
#[must_use]
pub fn probe(path: &Path, home: Option<&Path>) -> SessionRoot {
    let has_marker = PROJECT_MARKERS
        .iter()
        .any(|marker| path.join(marker).exists());
    let kind = classify(path, home, has_marker);
    let is_project = kind == RootKind::Project;
    let project_name = if is_project {
        path.file_name()
            .map(|name| bounded_field(&name.to_string_lossy(), NAME_MAX_CHARS))
    } else {
        None
    };
    let vcs_branch = if is_project {
        read_git_branch(path).map(|branch| bounded_field(&branch, NAME_MAX_CHARS))
    } else {
        None
    };
    SessionRoot {
        display: bounded_field(&display_for(path, home), DISPLAY_MAX_CHARS),
        kind,
        project_name,
        vcs_branch,
    }
}

/// The branch `<root>/.git/HEAD` names, without invoking git.
///
/// `<root>/.git` is either the git directory itself or — in a linked worktree —
/// a file whose first line is `gitdir: <path>` (relative paths are relative to
/// `root`); the pointer is followed one hop. `HEAD` reading `ref: refs/heads/<b>`
/// yields `Some(b)`; a detached SHA, a ref outside `refs/heads/`, an unreadable
/// or missing file, or no `.git` at all yields `None`.
fn read_git_branch(root: &Path) -> Option<String> {
    let dot_git = root.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        gitdir_pointer(root, &std::fs::read_to_string(&dot_git).ok()?)?
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let branch = head
        .trim()
        .strip_prefix("ref:")?
        .trim()
        .strip_prefix("refs/heads/")?;
    if branch.is_empty() {
        None
    } else {
        Some(branch.to_owned())
    }
}

/// The git directory a `.git` *file* points at, or `None` when the file is not
/// a `gitdir:` pointer.
fn gitdir_pointer(root: &Path, dot_git_file: &str) -> Option<PathBuf> {
    let target = dot_git_file.lines().next()?.strip_prefix("gitdir:")?.trim();
    if target.is_empty() {
        return None;
    }
    let target = Path::new(target);
    Some(if target.is_absolute() {
        target.to_path_buf()
    } else {
        root.join(target)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique scratch directory per call — the counter, not the timestamp,
    /// guarantees uniqueness across two calls within one clock tick.
    fn temp_root(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "teton-sessionroot-{tag}-{}-{}-{}",
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

    fn write_head(git_dir: &Path, contents: &str) {
        std::fs::create_dir_all(git_dir).unwrap();
        std::fs::write(git_dir.join("HEAD"), contents).unwrap();
    }

    /// AC-1's fixture: a `.git` directory whose `HEAD` names `main`.
    #[test]
    fn a_git_repo_on_main_is_a_project_named_after_its_directory_with_branch_main() {
        let base = temp_root("git");
        let repo = base.join("repo");
        write_head(&repo.join(".git"), "ref: refs/heads/main\n");
        let home = base.join("home");

        let root = probe(&repo, Some(&home));
        assert_eq!(root.kind, RootKind::Project);
        assert_eq!(root.project_name.as_deref(), Some("repo"));
        assert_eq!(root.vcs_branch.as_deref(), Some("main"));
        // Not under `home`, so absolute — through the same bounding every
        // display gets (a macOS temp dir is longer than the ceiling).
        assert_eq!(
            root.display,
            bounded_field(&repo.display().to_string(), DISPLAY_MAX_CHARS)
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_branch_with_slashes_in_its_name_is_read_whole() {
        let base = temp_root("slashes");
        write_head(&base.join(".git"), "ref: refs/heads/feat/REQ-583-x\n");
        let root = probe(&base, None);
        assert_eq!(root.vcs_branch.as_deref(), Some("feat/REQ-583-x"));
        std::fs::remove_dir_all(&base).ok();
    }

    /// AC-3: a detached `HEAD` is a project with no branch — never a guess.
    #[test]
    fn a_detached_head_is_a_project_with_no_branch() {
        let base = temp_root("detached");
        write_head(
            &base.join(".git"),
            "0123456789abcdef0123456789abcdef01234567\n",
        );
        let root = probe(&base, None);
        assert_eq!(root.kind, RootKind::Project);
        assert!(root.project_name.is_some());
        assert_eq!(root.vcs_branch, None);
        std::fs::remove_dir_all(&base).ok();
    }

    /// AC-3: an unreadable `HEAD` (here: a directory where the file should be,
    /// which fails the read on every platform and for every uid) is a project
    /// with no branch. So is a `HEAD` that names something outside
    /// `refs/heads/`, and a `.git` with no `HEAD` at all.
    #[test]
    fn an_unreadable_or_foreign_head_is_a_project_with_no_branch() {
        let base = temp_root("unreadable");
        std::fs::create_dir_all(base.join(".git/HEAD")).unwrap();
        let root = probe(&base, None);
        assert_eq!(root.kind, RootKind::Project);
        assert_eq!(root.vcs_branch, None);
        std::fs::remove_dir_all(&base).ok();

        let base = temp_root("foreign-ref");
        write_head(&base.join(".git"), "ref: refs/remotes/origin/main\n");
        let root = probe(&base, None);
        assert_eq!(root.kind, RootKind::Project);
        assert_eq!(root.vcs_branch, None);
        std::fs::remove_dir_all(&base).ok();

        let base = temp_root("no-head");
        std::fs::create_dir_all(base.join(".git")).unwrap();
        let root = probe(&base, None);
        assert_eq!(root.kind, RootKind::Project);
        assert_eq!(root.vcs_branch, None);
        std::fs::remove_dir_all(&base).ok();
    }

    /// A linked worktree: `.git` is a **file** reading `gitdir: <path>`; the
    /// pointer is followed (relative to the root) and the branch read from
    /// there. This repository is itself such a worktree.
    #[test]
    fn a_linked_worktree_git_file_is_followed_to_its_head() {
        let base = temp_root("worktree");
        let worktree = base.join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        // The relative pointer, as git writes it for a worktree beside its
        // main checkout: `gitdir: ../main/.git/worktrees/wt`.
        write_head(
            &base.join("main/.git/worktrees/wt"),
            "ref: refs/heads/feat/REQ-583-root\n",
        );
        std::fs::write(worktree.join(".git"), "gitdir: ../main/.git/worktrees/wt\n").unwrap();

        let root = probe(&worktree, None);
        assert_eq!(root.kind, RootKind::Project, "a `.git` file is a marker");
        assert_eq!(root.project_name.as_deref(), Some("wt"));
        assert_eq!(root.vcs_branch.as_deref(), Some("feat/REQ-583-root"));

        // An absolute pointer works the same way.
        let abs_git_dir = base.join("elsewhere/.git/worktrees/wt2");
        write_head(&abs_git_dir, "ref: refs/heads/dev\n");
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", abs_git_dir.display()),
        )
        .unwrap();
        assert_eq!(probe(&worktree, None).vcs_branch.as_deref(), Some("dev"));

        // A `.git` file that is not a pointer: still a marker, no branch.
        std::fs::write(worktree.join(".git"), "not a pointer\n").unwrap();
        let root = probe(&worktree, None);
        assert_eq!(root.kind, RootKind::Project);
        assert_eq!(root.vcs_branch, None);
        std::fs::remove_dir_all(&base).ok();
    }

    /// AC-2 / BR-4: `$HOME` is `home` — with no project name and no branch —
    /// even when a `.git` sits in it. Home wins over the marker.
    #[test]
    fn home_is_home_even_with_a_dot_git_in_it_and_says_no_branch() {
        let home = temp_root("home");
        write_head(&home.join(".git"), "ref: refs/heads/main\n");
        let root = probe(&home, Some(&home));
        assert_eq!(root.kind, RootKind::Home);
        assert_eq!(root.project_name, None);
        assert_eq!(root.vcs_branch, None);
        assert_eq!(root.display, "~");
        std::fs::remove_dir_all(&home).ok();
    }

    /// AC-2: the filesystem root and a marker-less directory say no branch and
    /// carry no project name.
    #[test]
    fn the_filesystem_root_and_a_plain_directory_carry_no_project_facts() {
        let root = probe(Path::new("/"), Some(Path::new("/nonexistent-home")));
        assert_eq!(root.kind, RootKind::FilesystemRoot);
        assert_eq!(root.project_name, None);
        assert_eq!(root.vcs_branch, None);
        assert_eq!(root.display, "/");

        let plain = temp_root("plain");
        let root = probe(&plain, None);
        assert_eq!(root.kind, RootKind::Plain);
        assert_eq!(root.project_name, None);
        assert_eq!(root.vcs_branch, None);
        std::fs::remove_dir_all(&plain).ok();
    }

    /// AC-7 (the I/O half): every marker in the table, present as a file,
    /// makes the directory a project — the probe reads the same table the
    /// classifier's test iterates, so a name added there is found here.
    #[test]
    fn every_marker_in_the_table_is_found_as_a_file_or_a_directory() {
        for marker in PROJECT_MARKERS {
            let base = temp_root("marker");
            std::fs::write(base.join(marker), "").unwrap();
            assert_eq!(
                probe(&base, None).kind,
                RootKind::Project,
                "marker {marker} as a file"
            );
            std::fs::remove_dir_all(&base).ok();

            let base = temp_root("marker-dir");
            std::fs::create_dir_all(base.join(marker)).unwrap();
            assert_eq!(
                probe(&base, None).kind,
                RootKind::Project,
                "marker {marker} as a directory"
            );
            std::fs::remove_dir_all(&base).ok();
        }
    }

    #[test]
    fn a_non_git_project_has_a_name_and_no_branch() {
        let base = temp_root("cargo");
        std::fs::write(base.join("Cargo.toml"), "[package]\n").unwrap();
        let root = probe(&base, None);
        assert_eq!(root.kind, RootKind::Project);
        assert!(root.project_name.is_some());
        assert_eq!(root.vcs_branch, None);
        std::fs::remove_dir_all(&base).ok();
    }

    /// ADR-2 bounding, applied at the probe: a long branch name and a long
    /// display both come back within their ceilings, and a control character
    /// in a branch name cannot break the line it will sit on.
    #[test]
    fn the_probe_bounds_every_user_controlled_field() {
        let base = temp_root("bound");
        let long_branch = "b".repeat(200);
        write_head(
            &base.join(".git"),
            &format!("ref: refs/heads/{long_branch}\u{1b}\n"),
        );
        let root = probe(&base, None);
        let branch = root.vcs_branch.expect("a long branch is still a branch");
        assert!(branch.chars().count() <= NAME_MAX_CHARS, "{branch}");
        assert!(!branch.chars().any(char::is_control), "{branch}");
        assert!(root.display.chars().count() <= DISPLAY_MAX_CHARS);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn display_is_home_relative_when_the_root_is_under_home() {
        let home = temp_root("home-rel");
        let repo = home.join("Documents/GitHub/teton-code");
        std::fs::create_dir_all(&repo).unwrap();
        let root = probe(&repo, Some(&home));
        assert_eq!(root.display, "~/Documents/GitHub/teton-code");
        std::fs::remove_dir_all(&home).ok();
    }
}
