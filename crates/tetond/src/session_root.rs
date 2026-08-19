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
//! module only answers the questions that need the filesystem — *is a marker
//! present*, *what does `.git/HEAD` say*, and *is this path the home folder by
//! identity* (one `metadata` read of each side, compared by device and inode,
//! with a `canonicalize` of each as the fallback when a side cannot be stat'ed)
//! — and hands the answers over. Nothing here shells out: the branch is read
//! from `HEAD` directly through the crate's one bounded reader
//! ([`crate::fs_util::read_regular_file_bounded`]), and a value that cannot be
//! read is `None`, never a guess (BR-1).

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use teton_core::session_root::{
    bounded_field, classify, display_for, DISPLAY_MAX_CHARS, NAME_MAX_CHARS, PROJECT_MARKERS,
};
use teton_protocol::methods::{RootKind, SessionRoot};

/// The most bytes read from `.git` (the worktree pointer file) or `HEAD`. Both
/// are one short line in every checkout git writes; anything longer is not the
/// file this reader is for, and the bound is what keeps a `HEAD` that is a
/// multi-gigabyte file from holding a probe — which runs on every turn —
/// hostage (a `HEAD` that is a FIFO is refused by the reader's type check
/// before any open).
const GIT_FILE_MAX_BYTES: u64 = 4096;

/// The daemon's `HOME`, read at the call site of every session-root probe
/// (REQ-583 ADR-1): the daemon runs as the user, so this is the home the display
/// spelling is relative to and the home the `home` kind is judged against.
/// `None` when unset or empty — never a guess (the CLI's `home_dir` applies the
/// same rule) — and then the probe displays absolute paths and never classifies
/// a root as `home`, which also leaves BR-12's home-tree pruning inert: a
/// daemon started with no `HOME` (a bare service environment) walks a real
/// home folder as a `plain` root, so `HOME` is part of what makes the daemon's
/// launch environment correct.
#[must_use]
pub(crate) fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| !home.as_os_str().is_empty())
}

/// The probe's answer together with the path it was asked about — one value,
/// so a jail built from `path` and a prompt built from `view` are two readings
/// of one probe and cannot disagree about which directory they describe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbedRoot {
    /// The path the session's tools are jailed to, as the registry stores it.
    pub path: PathBuf,
    /// [`probe`]'s answer for `path`: the wire view every surface renders.
    pub view: SessionRoot,
}

impl ProbedRoot {
    /// [`probe`] `path` under `home`, keeping the path beside the answer.
    #[must_use]
    pub fn probe(path: PathBuf, home: Option<&Path>) -> Self {
        let view = probe(&path, home);
        Self { path, view }
    }
}

/// Derive the session root for `path`, given the caller's `HOME` (or `None`).
///
/// - `kind`: [`classify`] over whether any [`PROJECT_MARKERS`] entry exists at
///   `path` as a file **or** a directory (a linked git worktree's `.git` is a
///   file naming its `gitdir:`). Whether `path` *is* the home folder is judged
///   by filesystem identity ([`same_directory`]: device and inode), so an alias
///   of it — a symlink to it, macOS's `/private` prefix or its
///   `/System/Volumes/Data` firmlink, a `..` in the middle — is `home` and not
///   a `plain` directory that happens to hold every media tree BR-12 prunes;
///   the filesystem-root check is the pure one on the path as given.
/// - `project_name`: the bounded basename, present iff `kind == Project`.
/// - `vcs_branch`: from `.git/HEAD` (following a `gitdir:` file), present iff
///   `kind == Project` and `HEAD` names a branch — a detached SHA, an unreadable
///   `HEAD`, or a non-git project all yield `None`. A home-kind root with a
///   `~/.git` says no branch: home wins over the marker (BR-4), so the branch is
///   not read there at all.
/// - `display`: [`display_for`] over the path **as given** (the spelling the
///   user chose, not the canonical one), bounded to [`DISPLAY_MAX_CHARS`].
///
/// The three user-controlled strings are bounded here, once, so the environment
/// block, the jail refusal, the launch notice and the `/cd` line all print the
/// same bytes (ADR-2 "built once").
#[must_use]
pub fn probe(path: &Path, home: Option<&Path>) -> SessionRoot {
    let has_marker = PROJECT_MARKERS
        .iter()
        .any(|marker| path.join(marker).exists());
    // "Is this the home folder" is a question of identity, not spelling: when
    // `path` and `home` are one directory the classifier is handed `home`'s
    // own spelling for the path, so its `==` says so; otherwise the path as
    // given. The display keeps the user's spelling either way.
    let is_home = home.is_some_and(|home| same_directory(path, home));
    let classified_path = if is_home { home.unwrap_or(path) } else { path };
    let kind = classify(classified_path, home, has_marker);
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

/// Whether `a` and `b` name one directory — by filesystem identity (device and
/// inode off `metadata`, which follows symlinks), falling back to comparing
/// their canonical spellings when either side cannot be stat'ed.
///
/// Identity rather than spelling because the home folder has many spellings
/// that canonicalization does not always unify: a symlink to it, macOS's
/// `/private` prefix on a `/var` path, the `/System/Volumes/Data` firmlink
/// (a *firmlink* is not a symlink, so `canonicalize` leaves it as spelled),
/// a `..` in the middle. Two `metadata` reads per probe, one per side.
fn same_directory(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => {
            let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
            canonical(a) == canonical(b)
        }
    }
}

/// The branch `<root>/.git/HEAD` names, without invoking git.
///
/// `<root>/.git` is either the git directory itself or — in a linked worktree —
/// a file whose first line is `gitdir: <path>` (relative paths are relative to
/// `root`); the pointer is followed one hop. `HEAD` reading `ref: refs/heads/<b>`
/// yields `Some(b)`; a detached SHA, a ref outside `refs/heads/`, an empty
/// branch name, an unreadable, missing, oversize or non-UTF-8 file, or no `.git`
/// at all yields `None`. Both files are opened only when they are regular files
/// and read to at most [`GIT_FILE_MAX_BYTES`] — see [`read_git_file`].
fn read_git_branch(root: &Path) -> Option<String> {
    let dot_git = root.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        gitdir_pointer(root, &read_git_file(&dot_git)?)?
    };
    let head = read_git_file(&git_dir.join("HEAD"))?;
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

/// The contents of the **regular** file at `path` (`.git` or `HEAD`), read to
/// at most [`GIT_FILE_MAX_BYTES`]; `None` for anything else — a directory, a
/// FIFO (whose open would block for a writer that never comes), a device, a
/// file past the bound, an unreadable one, or one that is not UTF-8. The
/// crate's one bounded reader, at this module's bound.
fn read_git_file(path: &Path) -> Option<String> {
    crate::fs_util::read_regular_file_bounded(path, GIT_FILE_MAX_BYTES)
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

    fn write_head(git_dir: &Path, contents: impl AsRef<[u8]>) {
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
            format!("ref: refs/heads/{long_branch}\u{1b}\n"),
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

    /// **An alias of the home folder is `home`.** A symlink to it, a `..` in
    /// the middle, or (on macOS) the `/private` spelling of a `/var` path all
    /// name the same directory, and BR-12's pruning keys on the kind — so the
    /// kind is judged on the canonical spellings. The display keeps the
    /// spelling the caller gave: that is what the user typed and what they
    /// will recognise.
    #[test]
    fn an_alias_of_the_home_folder_is_home_and_keeps_its_own_spelling() {
        let base = temp_root("alias");
        let home = base.join("home");
        std::fs::create_dir_all(home.join("Music")).unwrap();
        std::fs::write(home.join("Cargo.toml"), "[package]\n").unwrap();
        let link = base.join("home-link");
        std::os::unix::fs::symlink(&home, &link).unwrap();

        // Through the link, with `HOME` spelled plainly.
        let root = probe(&link, Some(&home));
        assert_eq!(root.kind, RootKind::Home, "{root:?}");
        assert_eq!(root.project_name, None, "home wins over the marker");
        assert_eq!(root.vcs_branch, None);
        assert_eq!(
            root.display,
            bounded_field(&link.display().to_string(), DISPLAY_MAX_CHARS),
            "the display is the caller's spelling, not the canonical one"
        );

        // Plainly, with `HOME` spelled through the link.
        assert_eq!(probe(&home, Some(&link)).kind, RootKind::Home);

        // With a `..` in the middle of either side.
        let dotted = home.join("Music/..");
        assert_eq!(probe(&dotted, Some(&home)).kind, RootKind::Home);
        assert_eq!(probe(&home, Some(&dotted)).kind, RootKind::Home);

        // A different directory that merely looks like a home is not one.
        let other = base.join("other");
        std::fs::create_dir_all(&other).unwrap();
        assert_eq!(probe(&other, Some(&home)).kind, RootKind::Plain);
        std::fs::remove_dir_all(&base).ok();
    }

    /// **Home is judged by identity, not spelling.** macOS mounts the data
    /// volume at `/System/Volumes/Data` through a *firmlink*, which
    /// `canonicalize` does not resolve — so `/System/Volumes/Data/Users/<n>`
    /// and `/Users/<n>` are one directory with two canonical spellings. The
    /// probe compares `(dev, ino)` off `metadata`, which sees through a
    /// firmlink, a symlink and a `..` alike. The firmlink itself cannot be
    /// built in a test, so its shape is: a *parent* reached through a link,
    /// with the home a plain child under it — the alias and the home differ
    /// in every path component, and only the identity says they are one.
    /// When one side cannot be stat'ed the canonical-spelling compare is the
    /// fallback, asserted both ways round.
    #[test]
    fn home_identity_sees_through_an_aliased_parent_and_falls_back_to_spelling() {
        let base = temp_root("identity");
        let volume = base.join("Volumes/Data");
        let home = volume.join("Users/ada");
        std::fs::create_dir_all(home.join("Music")).unwrap();
        std::fs::write(home.join("Cargo.toml"), "[package]\n").unwrap();
        // `base/Users` -> `base/Volumes/Data/Users`: the alias names the home
        // as `base/Users/ada`, which shares no component with the data-volume
        // spelling past `base`.
        std::os::unix::fs::symlink(volume.join("Users"), base.join("Users")).unwrap();
        let alias = base.join("Users/ada");
        assert!(same_directory(&alias, &home));
        assert!(!same_directory(&alias, &volume));

        let root = probe(&alias, Some(&home));
        assert_eq!(root.kind, RootKind::Home, "{root:?}");
        assert_eq!(root.project_name, None, "home wins over the marker");
        assert_eq!(
            root.display,
            bounded_field(&alias.display().to_string(), DISPLAY_MAX_CHARS),
            "the display is the caller's spelling"
        );
        assert_eq!(probe(&home, Some(&alias)).kind, RootKind::Home);
        // Under the alias, not at it: not home, and the display is
        // home-relative only when the spelling says so.
        let under = alias.join("Music");
        assert_eq!(probe(&under, Some(&home)).kind, RootKind::Plain);

        // The fallback: a side that cannot be stat'ed is compared by
        // canonical spelling, so a missing home still never matches a real
        // directory, and a missing path matches only its own spelling.
        let missing = base.join("missing");
        assert!(!same_directory(&home, &missing));
        assert!(!same_directory(&missing, &home));
        assert!(same_directory(&missing, &base.join("missing")));
        assert_eq!(probe(&home, Some(&missing)).kind, RootKind::Project);
        std::fs::remove_dir_all(&base).ok();
    }

    /// The branch reader refuses every degenerate `HEAD` and pointer without a
    /// guess: an empty branch name, a `gitdir:` naming a directory that is not
    /// there, a `HEAD` that is not UTF-8, and one past the size bound.
    #[test]
    fn a_degenerate_head_or_pointer_yields_no_branch() {
        let base = temp_root("degenerate-empty");
        write_head(&base.join(".git"), "ref: refs/heads/\n");
        assert_eq!(probe(&base, None).vcs_branch, None, "an empty branch name");
        std::fs::remove_dir_all(&base).ok();

        let base = temp_root("degenerate-pointer");
        std::fs::write(base.join(".git"), "gitdir: ../nowhere/.git/worktrees/x\n").unwrap();
        let root = probe(&base, None);
        assert_eq!(root.kind, RootKind::Project, "the pointer file is a marker");
        assert_eq!(root.vcs_branch, None, "a pointer to a missing directory");
        std::fs::remove_dir_all(&base).ok();

        let base = temp_root("degenerate-utf8");
        write_head(&base.join(".git"), b"ref: refs/heads/\xff\xfe\n");
        assert_eq!(probe(&base, None).vcs_branch, None, "invalid UTF-8");
        std::fs::remove_dir_all(&base).ok();

        let base = temp_root("degenerate-huge");
        let huge = format!(
            "ref: refs/heads/{}\n",
            "b".repeat(GIT_FILE_MAX_BYTES as usize + 16)
        );
        write_head(&base.join(".git"), huge);
        assert_eq!(probe(&base, None).vcs_branch, None, "past the size bound");
        // Just under the bound still reads (and is then bounded for display).
        let base2 = temp_root("degenerate-under");
        write_head(&base2.join(".git"), "ref: refs/heads/main\n");
        assert_eq!(probe(&base2, None).vcs_branch.as_deref(), Some("main"));
        std::fs::remove_dir_all(&base).ok();
        std::fs::remove_dir_all(&base2).ok();
    }

    /// A `HEAD` that is a FIFO does not hold the probe: the type is read off
    /// `metadata` before any open, so the open that would block for a writer
    /// never happens. On a worker thread with a deadline, so a regression fails
    /// rather than hangs.
    #[test]
    fn a_fifo_named_head_does_not_block_the_probe() {
        let base = temp_root("fifo-head");
        std::fs::create_dir_all(base.join(".git")).unwrap();
        crate::mkfifo(&base.join(".git/HEAD"));

        let probed = base.clone();
        let root = crate::with_deadline("the probe with a FIFO HEAD", move || probe(&probed, None));
        assert_eq!(root.kind, RootKind::Project);
        assert_eq!(root.vcs_branch, None);
        std::fs::remove_dir_all(&base).ok();
    }

    /// The one `HOME` reader: the variable's value, or `None` when it is unset
    /// or empty. Read-only against the process environment — the same rule the
    /// CLI's `home_dir` applies.
    #[test]
    fn home_is_the_variable_or_none_when_empty() {
        let raw = std::env::var_os("HOME").map(PathBuf::from);
        let expected = raw.filter(|h| !h.as_os_str().is_empty());
        assert_eq!(home(), expected);
    }

    /// `ProbedRoot` keeps the path beside the probe's answer for it — one value
    /// for the jail and the prompt to be built from.
    #[test]
    fn a_probed_root_carries_the_path_and_the_probe_of_that_path() {
        let base = temp_root("probed");
        write_head(&base.join(".git"), "ref: refs/heads/main\n");
        let probed = ProbedRoot::probe(base.clone(), None);
        assert_eq!(probed.path, base);
        assert_eq!(probed.view, probe(&base, None));
        assert_eq!(probed.view.vcs_branch.as_deref(), Some("main"));
        std::fs::remove_dir_all(&base).ok();
    }
}
