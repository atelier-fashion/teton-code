//! The create-new write of `TETON.md` and its `--force` replacement (REQ-613
//! BR-6, ADR-5).
//!
//! Two entry points and one shared tail: [`write_new`] is the offer's path and
//! never clobbers; [`replace`] is `--force`'s and always replaces, by rename.
//! Both write the buffer whole, and both remove what they created if any part of
//! that fails.
//!
//! # The no-clobber *is* the open
//!
//! [`write_new`] opens with `create_new(true)`, so "is there already a
//! `TETON.md`?" is answered by the very syscall that would otherwise create one.
//! A `stat`-then-open would leave a window, and the thing that fits in that
//! window is the case BR-6 exists for: a checkout landing, another session
//! writing, or the user creating the file themselves while the model drafts one.
//! `AlreadyExists` is therefore an *outcome*, not a fault — the offer's normal
//! second answer.
//!
//! # Mode `0o644`, deliberately not the transcript's `0o600`
//!
//! `transcript::writer` forces `0o600` and re-`chmod`s to defeat the umask,
//! because a transcript is a private record. This file is the opposite kind of
//! thing: repository content, meant to be committed, read by every tool and
//! every collaborator who checks the repository out. So `0o644` — and it is a
//! *request*, left to be masked by the umask exactly as `git checkout`'s files
//! are, rather than forced. A user whose umask is `077` gets a `0o600`
//! `TETON.md` and that is the answer they asked their shell for; nothing here
//! should override it, because nothing here is protecting a secret.
//!
//! # What `O_NOFOLLOW` still buys under `O_EXCL`, and what it does not
//!
//! POSIX resolves `O_EXCL` **first**: `open(O_CREAT|O_EXCL|O_NOFOLLOW)` on a path
//! that is a symlink fails with `EEXIST` on macOS and Linux alike and never
//! reports `ELOOP` — measured on both spellings of the flag set, not assumed.
//! So `O_NOFOLLOW` is not what refuses a symlink here; `O_EXCL` is, and it
//! refuses a **broken** one too, which is the shape that would otherwise have
//! created a file wherever the dangling link pointed — outside the jail, under
//! any name.
//!
//! The flag stays for two reasons. It is the discipline every other open in this
//! daemon keeps (`transcript::writer` REQ-611 BR-9, the loader's read in the
//! parent module), and one spelling of a rule is cheaper to keep right than two.
//! And it is what would still refuse the link on the day one of these opens
//! loses its `O_EXCL` — the flag that is redundant today is the one standing
//! between a future edit and a write through a symlink. `ELOOP`/`EMLINK` are
//! mapped anyway, the pair `install::is_symlink_refusal` and the parent module's
//! `map_open` already match, so a platform that reports the link before the
//! existence reaches a user with the same sentence.
//!
//! `O_NONBLOCK` rides along for the same belt-and-braces reason: a FIFO at the
//! path is already `EEXIST` under `O_EXCL`, so no open here can block on a
//! writer, and the flag costs nothing while keeping this open the same shape as
//! the loader's.
//!
//! Because `EEXIST` covers both cases, the **verdict** is named after the
//! refusal rather than by it: on `AlreadyExists` the path is `lstat`ed to choose
//! which of two true sentences the user reads. That `lstat` decides nothing —
//! the write is already refused, and no answer it gives can make one happen — so
//! the window it opens is a window on wording, not on bytes.
//!
//! # `--force` is a rename, never a truncate
//!
//! [`replace`] writes `TETON.md.<pid>.tmp` with the same open and `rename`s it
//! over the target. `rename(2)` is atomic within a directory: a reader between
//! the two sees the old file or the new one, never a half-written one, and there
//! is no moment at which `TETON.md` exists at zero length. A
//! `truncate`-then-write would have exactly that moment, and REQ-612's loader
//! runs every session start — it would load the hole as if the repository had
//! authored it.
//!
//! The rename is also what keeps `--force` from writing *through* a symlink:
//! `rename` replaces the entry, so a `TETON.md` that is a link to somewhere else
//! becomes a regular file here and the link's target is untouched.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use teton_protocol::methods::RepoContextSource;

use super::file_name;

/// The mode the file is created with — see the module docs for why it is not
/// the transcript's `0o600`.
const MODE: u32 = 0o644;

/// A `TETON.md` that is now on disk.
///
/// Carries the path rather than leaving the caller to rebuild it: the next thing
/// that happens is REQ-612's loader reading this very file (BR-7), and a second
/// spelling of the path is a second chance for the write and the read to
/// disagree about which bytes were generated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    /// Where it was written — `<root>/TETON.md` for both entry points, never
    /// the scratch name.
    pub path: PathBuf,
    /// How many bytes were written. The body's length, so the caller reports
    /// what it asked for rather than re-`stat`ing to find out.
    pub bytes: usize,
}

/// Why a write did not happen, or did not finish.
///
/// Three answers because they lead a user to three different next moves: keep
/// the file that is already there, look at what the link points to, or read an
/// I/O error. Collapsing the first into the third would tell someone whose
/// checkout simply landed first that something went wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteFailure {
    /// A regular `TETON.md` is already there and nothing was written (BR-6).
    ///
    /// The no-clobber outcome and the offer's normal second answer, never
    /// reported by [`replace`], whose whole purpose is to overwrite.
    AlreadyExists,
    /// The entry at the path is a symlink; nothing was written and nothing at
    /// the far end of the link was touched.
    Symlink,
    /// Anything else the filesystem said, by kind.
    ///
    /// The **kind** rather than the `io::Error`: this type is `Copy` and
    /// comparable so a caller can match on the verdict, and the kind is all the
    /// sentence the daemon prints needs.
    Io(std::io::ErrorKind),
}

/// Write `body` to `<root>/TETON.md`, refusing to clobber anything (BR-6, AC-8).
///
/// Returns [`WriteFailure::AlreadyExists`] when a file is already there and
/// [`WriteFailure::Symlink`] when the entry is a link, in both cases having
/// written nothing at all. A failure *after* the file is created removes it, so
/// no path through this function leaves a partial `TETON.md` for the next
/// session to load as if the repository had authored it (AC-9).
pub fn write_new(root: &Path, body: &str) -> Result<Written, WriteFailure> {
    let path = root.join(file_name(RepoContextSource::TetonMd));
    let file = create_new(&path)?;
    fill(&path, file, body)
}

/// Replace `<root>/TETON.md` with `body` — the `--force` path (BR-8, ADR-5).
///
/// The new bytes go to `TETON.md.<pid>.tmp` first and are `rename`d over the
/// target, so the target holds the old bytes or the new ones and never a
/// truncated file. The scratch file is removed on every failing path.
///
/// [`WriteFailure::AlreadyExists`] is not among this function's answers: it
/// means "a `TETON.md` is already there", which is the case `--force` is *for*.
/// A scratch name that cannot be claimed is reported as the I/O error it is.
pub fn replace(root: &Path, body: &str) -> Result<Written, WriteFailure> {
    let name = file_name(RepoContextSource::TetonMd);
    let target = root.join(name);
    // Beside the target and inside the root, so the `rename` is within one
    // directory and therefore atomic; a scratch file in `/tmp` would make it a
    // cross-device copy, which is the truncate window under another name.
    let temp = root.join(format!("{name}.{}.tmp", std::process::id()));

    let file = match create_new(&temp) {
        Ok(file) => file,
        // Only this process can have written that name, so an entry there is a
        // leftover from a run of ours that died — or, once pids wrap, of a
        // long-dead stranger's. Unlink it and claim the name once more.
        // `remove_file` unlinks the entry itself and never follows, so this is
        // safe for the symlink case too.
        Err(WriteFailure::AlreadyExists | WriteFailure::Symlink) => {
            remove(&temp);
            create_new(&temp).map_err(|failure| match failure {
                WriteFailure::AlreadyExists => WriteFailure::Io(std::io::ErrorKind::AlreadyExists),
                other => other,
            })?
        }
        Err(other) => return Err(other),
    };

    let written = fill(&temp, file, body)?;
    match std::fs::rename(&temp, &target) {
        Ok(()) => Ok(Written {
            path: target,
            bytes: written.bytes,
        }),
        Err(error) => {
            remove(&temp);
            Err(classify(&error))
        }
    }
}

/// Create `path`, refusing an existing entry of any kind.
///
/// The one open in this module; see the module docs for the flags and for why
/// the symlink verdict is chosen after the refusal rather than by it.
fn create_new(path: &Path) -> Result<File, WriteFailure> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(MODE)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists
                && std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
            {
                return WriteFailure::Symlink;
            }
            classify(&error)
        })
}

/// Write `body` whole into the just-created `file`, or remove `path` (AC-9).
///
/// `sync_all` is part of the write, not a durability flourish: without it the
/// last error a buffered write can hit is discovered when the descriptor is
/// closed, and `File`'s `Drop` has nowhere to report a close error, so an
/// `ENOSPC` that arrives late would be dropped on the floor and this function
/// would return `Ok` over a truncated file. Syncing moves that error to a place
/// where it can be seen and the file can be removed.
fn fill(path: &Path, mut file: File, body: &str) -> Result<Written, WriteFailure> {
    match file
        .write_all(body.as_bytes())
        .and_then(|()| file.sync_all())
    {
        Ok(()) => Ok(Written {
            path: path.to_path_buf(),
            bytes: body.len(),
        }),
        Err(error) => {
            remove(path);
            Err(classify(&error))
        }
    }
}

/// Unlink `path` on a failing path, ignoring `NotFound` and only that.
///
/// There is no error to return from here — the caller is already returning the
/// failure that brought it here — so anything other than "it is already gone"
/// is said out loud. A file left behind is the one outcome AC-9 rules out, and
/// silence about it would leave a partial `TETON.md` for the next session's
/// loader with nothing in the log to explain it.
fn remove(path: &Path) {
    if let Err(error) = std::fs::remove_file(path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            eprintln!(
                "repo-context: could not remove the partial {}: {error}",
                path.display()
            );
        }
    }
}

/// This module's named answer for an `io::Error`.
///
/// `ELOOP`/`EMLINK` first, the pair `install::is_symlink_refusal` and the parent
/// module's `map_open` match, so the one kernel answer does not reach a user as
/// two different sentences depending on which of this daemon's `O_NOFOLLOW`
/// opens hit it. `libc`'s constants rather than either literal: the number
/// differs by platform and `io::ErrorKind` has no stable value for it.
fn classify(error: &std::io::Error) -> WriteFailure {
    if matches!(error.raw_os_error(), Some(libc::ELOOP) | Some(libc::EMLINK)) {
        return WriteFailure::Symlink;
    }
    match error.kind() {
        std::io::ErrorKind::AlreadyExists => WriteFailure::AlreadyExists,
        kind => WriteFailure::Io(kind),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    /// The body every leg writes — long enough that a truncated file is visibly
    /// not this one.
    const BODY: &str = "> Generated by Teton\n\n# teton-code\n\nA daemon and a CLI.\n";

    /// A scratch directory no other test in this process collides with — the
    /// house pattern (`transcript::writer::tests::scratch`): a counter, not a
    /// timestamp, because two calls inside one clock tick return the same
    /// instant. Created, so every leg starts from a real, empty root.
    fn scratch(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "teton-repo-context-write-{}-{tag}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the scratch root is created");
        dir
    }

    /// The mode bits of `path`, without following a symlink.
    fn mode_of(path: &Path) -> u32 {
        std::fs::symlink_metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    }

    /// `(dev, ino)` — the identity that tells a rename from a truncate.
    fn identity(path: &Path) -> (u64, u64) {
        let meta = std::fs::symlink_metadata(path).expect("metadata");
        (meta.dev(), meta.ino())
    }

    /// The entry names directly under `dir`, sorted.
    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("the root lists")
            .map(|entry| {
                entry
                    .expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    /// This file's source, cut at the first column-0 `#[cfg(test)]` so a check
    /// whose own patterns appear in the tests below cannot match itself
    /// (conventions: bound the span, key on the hazard).
    fn source() -> &'static str {
        let whole = include_str!("write.rs");
        &whole[..whole
            .find("\n#[cfg(test)]\n")
            .expect("this file has a test module")]
    }

    /// The body of the top-level function named `name`, bounded to that item.
    fn body_of(name: &str) -> &'static str {
        let source = source();
        let start = source
            .find(&format!("fn {name}("))
            .unwrap_or_else(|| panic!("{name} is defined in this file"));
        let end = source[start..]
            .find("\n}\n")
            .unwrap_or_else(|| panic!("{name}'s body closes"));
        &source[start..start + end]
    }

    /// BR-6 / AC-8: a fresh root takes the write; a `TETON.md` already there is
    /// not clobbered — same bytes, same inode, same mtime; a symlink at the path
    /// is refused and its target is untouched, dangling or not; and a create
    /// that cannot happen at all leaves the root as it found it.
    ///
    /// The broken-link leg is the pointed one: `O_EXCL` refuses a dangling
    /// symlink with `EEXIST`, so the file the link names is *not* created —
    /// which is the only way this write could have put bytes outside the jail.
    ///
    /// Mutation, each one run and watched go red: `create_new(true)` →
    /// `create(true).truncate(true)` in `create_new` lets the second write
    /// through and fails on the `AlreadyExists` verdict, before the bytes,
    /// inode and mtime legs standing behind it; dropping the `symlink_metadata`
    /// arm from `create_new`'s mapping answers `AlreadyExists` for both link
    /// legs; deleting `.mode(MODE)` fails the structural leg with "the open
    /// dropped `.mode(MODE)`" — and *only* that leg, since a `0o666` create
    /// under the usual `022` umask still lands on `0o644`, which is why the
    /// mode is pinned structurally and not by its bits alone.
    #[test]
    fn write_new_refuses_an_existing_file_and_a_symlink_and_leaves_nothing_on_failure() {
        // A fresh root: the file is written whole.
        let root = scratch("fresh");
        let written = write_new(&root, BODY).expect("a fresh root takes the write");
        assert_eq!(written.path, root.join("TETON.md"));
        assert_eq!(written.bytes, BODY.len());
        assert_eq!(
            std::fs::read_to_string(&written.path).expect("the file reads back"),
            BODY
        );
        assert_eq!(entries(&root), vec!["TETON.md".to_owned()]);
        // `OpenOptions::mode` is a request the umask masks, and masking only
        // clears bits — so `0o644`'s zeros are the half of the mode that holds
        // on every machine, whatever the umask, and the half worth asserting.
        assert_eq!(
            mode_of(&written.path) & 0o022,
            0,
            "the generated file is group- or world-writable"
        );

        // BR-6: a second write finds the file and changes nothing.
        let before = std::fs::symlink_metadata(&written.path).expect("metadata");
        assert_eq!(
            write_new(&root, "a second draft"),
            Err(WriteFailure::AlreadyExists)
        );
        let after = std::fs::symlink_metadata(&written.path).expect("metadata");
        assert_eq!(
            std::fs::read_to_string(&written.path).expect("the file reads back"),
            BODY,
            "the refused write clobbered the file"
        );
        assert_eq!(
            after.modified().ok(),
            before.modified().ok(),
            "the refused write touched the mtime"
        );
        assert_eq!(
            (after.dev(), after.ino()),
            (before.dev(), before.ino()),
            "the refused write replaced the inode"
        );

        // A symlink at the path: refused, and the far end is untouched.
        let root = scratch("symlink");
        let elsewhere = root.join("elsewhere.md");
        std::fs::write(&elsewhere, "the link's target").expect("the target is written");
        std::os::unix::fs::symlink(&elsewhere, root.join("TETON.md")).expect("the link is made");
        assert_eq!(write_new(&root, BODY), Err(WriteFailure::Symlink));
        assert_eq!(
            std::fs::read_to_string(&elsewhere).expect("the target reads back"),
            "the link's target",
            "the write followed the link"
        );
        assert!(
            std::fs::symlink_metadata(root.join("TETON.md"))
                .expect("metadata")
                .file_type()
                .is_symlink(),
            "the link itself was replaced"
        );

        // A *dangling* symlink: refused, and the path it names stays absent.
        let root = scratch("broken-symlink");
        let never = root.join("never-created.md");
        std::os::unix::fs::symlink(&never, root.join("TETON.md")).expect("the link is made");
        assert_eq!(write_new(&root, BODY), Err(WriteFailure::Symlink));
        assert!(
            std::fs::symlink_metadata(&never).is_err(),
            "the write followed a dangling link and created its target"
        );

        // A create that cannot happen: nothing is left behind. Skipped as root,
        // who can write a mode-`0o555` directory — the house rule
        // (`walk::running_as_root`).
        if !crate::harness::tools::walk::running_as_root() {
            let root = scratch("read-only");
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o555))
                .expect("the root is made read-only");
            let failure = write_new(&root, BODY);
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755))
                .expect("the root is made writable again");
            assert_eq!(
                failure,
                Err(WriteFailure::Io(std::io::ErrorKind::PermissionDenied))
            );
            assert!(entries(&root).is_empty(), "a refused create left an entry");
        }

        // ADR-5's flags and mode, structurally. `O_EXCL` is what refuses the
        // symlink above, so `O_NOFOLLOW`'s presence is not observable from the
        // outside at all and this is the only place that can hold it — the
        // module docs say why it is kept. Bounded to `create_new`'s own body,
        // with a floor so a slice that stopped containing the function cannot
        // pass vacuously.
        let body = body_of("create_new");
        assert!(
            body.contains("OpenOptions::new()") && body.contains(".open(path)"),
            "the extracted slice is not `create_new`"
        );
        for required in [
            ".create_new(true)",
            ".mode(MODE)",
            "libc::O_NOFOLLOW",
            "libc::O_NONBLOCK",
        ] {
            assert!(body.contains(required), "the open dropped {required}");
        }
        assert_eq!(MODE, 0o644, "ADR-5's mode for a committed file");
    }

    /// AC-9: a write that fails after the file is created leaves no file.
    ///
    /// The failure is injected the only way a real filesystem offers portably —
    /// a handle opened read-only, so `write(2)` returns `EBADF`. It is a real
    /// descriptor on a real file in a real directory, and it enters `fill`
    /// exactly where `write_new`'s working handle does, so the cleanup under
    /// test is the same code a full disk would run. The mechanism is verified
    /// before the fixture is built on it (LESSON-569): the first half of this
    /// test asserts the handle actually refuses a write, because a fixture whose
    /// failure never fires is a green test that asserts nothing.
    ///
    /// The verdict is `Io(_)` rather than a named kind: `EBADF` has no stable
    /// `io::ErrorKind`, and naming the one it maps to today would pin an
    /// unstable detail rather than the rule. What the file is left as — empty
    /// here, half-written on a full disk — does not change the path: `fill`
    /// removes what it was given either way.
    ///
    /// Mutation, run: deleting the `remove(path)` call from `fill`'s error arm
    /// leaves the file behind and fails on "the failed write left a file
    /// behind".
    #[test]
    fn a_failed_write_leaves_no_partial_file() {
        let root = scratch("failed-write");
        let path = root.join("TETON.md");
        // Created exactly as `write_new` creates it and then reopened without
        // write access — `OpenOptions` refuses `read(true).create_new(true)`
        // outright, and the fixture needs a descriptor on a file that is
        // genuinely there, which is the state `fill` is always called in.
        let read_only = || {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(MODE)
                .open(&path)
                .expect("the file is created as `write_new` creates it");
            OpenOptions::new()
                .read(true)
                .open(&path)
                .expect("it reopens read-only")
        };

        let mut probe = read_only();
        assert!(
            probe.write_all(b"x").is_err(),
            "the fixture's failure mechanism does not fire: a read-only handle took a write"
        );
        drop(probe);
        std::fs::remove_file(&path).expect("the probe file is removed");

        let failure = fill(&path, read_only(), BODY);
        assert!(
            matches!(failure, Err(WriteFailure::Io(_))),
            "a failed write did not report an I/O failure: {failure:?}"
        );
        assert!(
            std::fs::symlink_metadata(&path).is_err(),
            "the failed write left a file behind"
        );
        assert!(entries(&root).is_empty(), "the failed write left an entry");
    }

    /// AC (BR-8): `--force` writes `TETON.md.<pid>.tmp` and renames it over the
    /// target last, so the target is only ever the old bytes or the new ones.
    ///
    /// The atomicity claim is made three ways, because no test can observe the
    /// instant between the two syscalls:
    ///
    /// - **The inode changes.** A `truncate`-then-write keeps the target's
    ///   inode; a rename replaces it. This is the whole difference, and it is
    ///   visible from outside.
    /// - **The scratch name is exactly `TETON.md.<pid>.tmp`.** A *directory*
    ///   planted there can be neither claimed by `create_new` nor cleared by
    ///   `remove_file`, so a `replace` that used any other name would have
    ///   sailed past it and replaced the target.
    /// - **The rename is textually last** in `replace`'s body, which inside one
    ///   function body is execution order (conventions).
    ///
    /// Mutation, both run: replacing the temp-and-rename with a direct
    /// `create(true).truncate(true)` write to the target keeps the inode and
    /// fails on "the target kept its inode"; hoisting the `rename` above the
    /// `fill` fails the order leg.
    #[test]
    fn replace_writes_a_temp_beside_the_target_and_renames_it_last() {
        // The old file is replaced, and by a different inode.
        let root = scratch("replace");
        let target = root.join("TETON.md");
        std::fs::write(&target, "the old notes").expect("the old file is written");
        let before = identity(&target);
        let written = replace(&root, BODY).expect("--force replaces");
        assert_eq!(written.path, target);
        assert_eq!(written.bytes, BODY.len());
        assert_eq!(
            std::fs::read_to_string(&target).expect("the file reads back"),
            BODY
        );
        assert_ne!(
            identity(&target),
            before,
            "the target kept its inode: it was truncated in place, not renamed over"
        );
        assert_eq!(
            entries(&root),
            vec!["TETON.md".to_owned()],
            "the scratch file outlived the rename"
        );

        // The scratch name is the one ADR-5 specifies, and a failure to claim it
        // leaves the target alone.
        let root = scratch("temp-name");
        let target = root.join("TETON.md");
        std::fs::write(&target, "the old notes").expect("the old file is written");
        std::fs::create_dir(root.join(format!("TETON.md.{}.tmp", std::process::id())))
            .expect("the scratch name is occupied");
        let failure = replace(&root, BODY);
        assert!(
            matches!(failure, Err(WriteFailure::Io(_))),
            "an unclaimable scratch name did not report an I/O failure: {failure:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&target).expect("the file reads back"),
            "the old notes",
            "a replace that never wrote its scratch file still changed the target"
        );

        // A symlink at the target is replaced, never written through — the
        // property `rename` gives for free and a truncating write would not.
        let root = scratch("replace-symlink");
        let elsewhere = root.join("elsewhere.md");
        std::fs::write(&elsewhere, "the link's target").expect("the target is written");
        std::os::unix::fs::symlink(&elsewhere, root.join("TETON.md")).expect("the link is made");
        let written = replace(&root, BODY).expect("--force replaces a link too");
        assert_eq!(
            std::fs::read_to_string(&elsewhere).expect("the target reads back"),
            "the link's target",
            "--force wrote through the link"
        );
        assert!(
            !std::fs::symlink_metadata(&written.path)
                .expect("metadata")
                .file_type()
                .is_symlink(),
            "the link is still a link"
        );
        assert_eq!(
            std::fs::read_to_string(&written.path).expect("the file reads back"),
            BODY
        );

        // The order, inside one body: the scratch file is created and filled,
        // and the rename is the last thing that happens.
        let body = body_of("replace");
        assert!(
            body.contains("std::fs::rename(&temp, &target)") && body.contains(".tmp"),
            "the extracted slice is not `replace`"
        );
        assert!(
            !body.contains("create_new(&target)"),
            "`replace` opens the target directly"
        );
        let created = body
            .find("create_new(&temp)")
            .expect("the scratch file is created");
        let filled = body
            .find("fill(&temp,")
            .expect("the scratch file is filled");
        let renamed = body
            .find("std::fs::rename(&temp, &target)")
            .expect("the scratch file is renamed");
        assert!(
            created < filled && filled < renamed,
            "`replace` does not create, fill, then rename: {created} {filled} {renamed}"
        );
        for after in ["create_new(", "fill(", "write_all"] {
            assert!(
                !body[renamed + "std::fs::rename(&temp, &target)".len()..].contains(after),
                "`{after}` runs after the rename"
            );
        }
    }
}
