//! REQ-611 — age-based retention over the daemon's own transcript files
//! (BR-13, AC-16).
//!
//! # A stated policy, applied only to the daemon's own files
//!
//! `prune` deletes. That is the whole reason its scope is drawn as narrowly as
//! it is: it runs in a directory the *user* may have chosen (`[transcript]
//! dir`), which may be a directory they keep other things in, and a retention
//! policy that removed one of those would be a data-loss bug with a
//! configuration file for a trigger.
//!
//! Three rules bound it, and each is a separate refusal rather than a clause of
//! one check:
//!
//! 1. **The name must be one this daemon mints** —
//!    `^\d{8}T\d{6}Z-sess-[0-9a-hjkmnp-tv-z]{26}\.jsonl$`. The body alphabet is
//!    Crockford's base32 **lowercased**, which is what `sessions.rs` mints
//!    (`SESSION_ID_ALPHABET`); it is not RFC 4648's, and a matcher written
//!    against RFC 4648 would both accept names this daemon cannot produce and
//!    reject ones it does.
//! 2. **`symlink_metadata`, never `metadata`** — a symlink is skipped whole. A
//!    followed symlink would let a name inside the directory decide the fate of
//!    a file outside it, which is the classic shape of a deletion primitive
//!    pointed at somebody else's data.
//! 3. **One level, no recursion, no path the caller did not name** — every
//!    candidate is `dir.join(entry.file_name())`, and an entry whose name is not
//!    a single component is skipped. `prune` never leaves `dir`.
//!
//! `retain_days = 0` is "keep everything", a policy rather than an error, and it
//! returns before the directory is even read.

use std::path::Path;
use std::time::SystemTime;

use super::writer::TRANSCRIPT_EXTENSION;

/// Seconds in a retention day.
const SECONDS_PER_DAY: u64 = 86_400;

/// The `sess-` prefix every transcript file name carries after its stamp.
const SESSION_PREFIX: &str = "sess-";

/// How many base32 characters follow `sess-` (`sessions.rs`: 128 bits of
/// entropy is 26 characters).
const SESSION_BODY_LEN: usize = 26;

/// What one pruning pass did (BR-13).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneReport {
    /// Files removed.
    pub removed: usize,
    /// Matching files that were young enough to keep.
    pub kept: usize,
    /// Entries skipped because the name, the type or the metadata disqualified
    /// them — every non-transcript in the directory lands here, which is the
    /// number that says the scope stayed narrow.
    pub skipped: usize,
    /// Matching, old-enough files whose removal failed.
    ///
    /// Not an error return: a file another process holds, or one the daemon
    /// cannot unlink, means this pass tidied less than it hoped and nothing
    /// more. Reporting it upward would hand the caller an error whose only
    /// honest handling is to ignore it (the posture `web::cache::enforce_bounds`
    /// takes for the same reason).
    pub failed: usize,
}

/// Remove transcripts older than `retain_days` from `dir` (BR-13, AC-16).
///
/// Called at daemon start and at every transcript open. `now` is a parameter so
/// a test can state an age exactly rather than sleep out a window.
///
/// Writes one stderr line naming the count **when it removed anything** (BR-13).
/// The line lives here, where the count is known, rather than at the two call
/// sites — a rule with two homes is a rule one of them will forget.
#[must_use]
pub fn prune(dir: &Path, retain_days: u32, now: SystemTime) -> PruneReport {
    let mut report = PruneReport::default();
    if retain_days == 0 {
        // "Never prune" is a setting, and it is answered before the directory is
        // read: a pass that walked the directory to delete nothing would still
        // be a pass that could delete something after a later edit.
        return report;
    }
    let Ok(listing) = std::fs::read_dir(dir) else {
        return report;
    };
    let max_age = u64::from(retain_days).saturating_mul(SECONDS_PER_DAY);

    for entry in listing.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            report.skipped += 1;
            continue;
        };
        if !is_transcript_file_name(name) {
            report.skipped += 1;
            continue;
        }
        let path = dir.join(name);
        // `symlink_metadata`, so a symlink reports as a symlink rather than as
        // whatever it points at. Rule 2 of the module doc.
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            report.skipped += 1;
            continue;
        };
        if meta.file_type().is_symlink() || !meta.is_file() {
            report.skipped += 1;
            continue;
        }
        let Ok(modified) = meta.modified() else {
            // A filesystem that reports no mtime cannot be asked how old the
            // file is, and "keep it" is the only safe answer to a question with
            // no data behind it.
            report.skipped += 1;
            continue;
        };
        let age = now
            .duration_since(modified)
            .map_or(0, |elapsed| elapsed.as_secs());
        if age <= max_age {
            report.kept += 1;
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            report.removed += 1;
        } else {
            report.failed += 1;
        }
    }

    if report.removed > 0 {
        eprintln!(
            "transcript: pruned {} file(s) older than {retain_days} day(s) from {}",
            report.removed,
            dir.display()
        );
    }
    report
}

/// Whether `name` is `^\d{8}T\d{6}Z-sess-[0-9a-hjkmnp-tv-z]{26}\.jsonl$`
/// (BR-13).
///
/// Hand-matched rather than compiled: the workspace carries no regex crate, and
/// the REQ's External Dependencies say none is added. The alphabet check is the
/// load-bearing half — see the module doc on why it is Crockford's and not RFC
/// 4648's.
#[must_use]
pub fn is_transcript_file_name(name: &str) -> bool {
    let Some(rest) = name.strip_suffix(&format!(".{TRANSCRIPT_EXTENSION}")) else {
        return false;
    };
    // `\d{8}T\d{6}Z`
    let Some((stamp, rest)) = rest.split_once('-') else {
        return false;
    };
    if stamp.len() != 16 {
        return false;
    }
    let stamp = stamp.as_bytes();
    if stamp[8] != b'T' || stamp[15] != b'Z' {
        return false;
    }
    if !stamp[..8].iter().all(u8::is_ascii_digit) || !stamp[9..15].iter().all(u8::is_ascii_digit) {
        return false;
    }
    // `sess-` + 26 lowercased Crockford base32 characters.
    let Some(body) = rest.strip_prefix(SESSION_PREFIX) else {
        return false;
    };
    body.len() == SESSION_BODY_LEN && body.bytes().all(is_crockford_lower)
}

/// Whether `byte` is in Crockford's base32 alphabet, lowercased — the alphabet
/// `sessions.rs` mints a session id body from.
///
/// `i`, `l`, `o` and `u` are absent by design there (they are the characters a
/// human transcribes wrongly), so they are absent here.
fn is_crockford_lower(byte: u8) -> bool {
    matches!(byte, b'0'..=b'9' | b'a'..=b'h' | b'j' | b'k' | b'm' | b'n' | b'p'..=b't' | b'v'..=b'z')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{Duration, UNIX_EPOCH};

    /// A scratch directory no other test in this process collides with; see
    /// `writer::tests::scratch`.
    fn scratch(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "teton-transcript-retention-{}-{tag}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    /// Backdate a file's mtime, which is what [`prune`] reads as its age. The
    /// house pattern (`web::cache::tests::set_written_at`): exact, and no test
    /// sleeps out a real window.
    fn set_age(path: &Path, at: SystemTime) {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for set_times");
        file.set_times(std::fs::FileTimes::new().set_modified(at))
            .expect("set mtime");
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read dir")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    /// AC-16 / BR-13 — with `retain_days = 1` and three two-day-old entries,
    /// only the matching regular file goes; `retain_days = 0` removes nothing.
    ///
    /// The three entries are the three ways a pass can overreach: a name it does
    /// not own, a symlink it must not follow, and (in the second half) a policy
    /// it must obey.
    ///
    /// # What the symlink leg actually catches, having been checked
    ///
    /// The fixture's first draft planted a **young** target, and the mutation
    /// below stayed green: a following `prune` read the target's fresh mtime and
    /// kept the link for the wrong reason. The target is therefore backdated
    /// too, which is what makes the age question reach the code under test
    /// (LESSON-569 — verify the failure *mechanism* before building a fixture
    /// around it).
    ///
    /// The damage a followed symlink does is that the **link's** fate is decided
    /// by a file outside the directory: the daemon deletes an entry the user
    /// placed, on a schedule derived from data it has no business reading. The
    /// target itself survives either way, because `remove_file` on a symlink
    /// path unlinks the link — so the "target untouched" assertion is AC-16's
    /// wording honoured rather than the leg with teeth, and the leg with teeth
    /// is the directory listing and the count.
    ///
    /// **Shown to fail** (mutation, restored): replacing
    /// `std::fs::symlink_metadata(&path)` with `std::fs::metadata(&path)` and
    /// dropping the `is_symlink()` arm makes this red on
    /// `only the matching regular file goes`, with `removed` at 2 instead of 1,
    /// and the link gone from the listing behind it.
    #[test]
    fn prune_removes_only_matching_old_files_and_never_follows_symlinks() {
        let dir = scratch("prune");
        let outside = scratch("prune-target");
        let now = UNIX_EPOCH + Duration::from_secs(1_756_900_272);
        let two_days_ago = now - Duration::from_secs(2 * SECONDS_PER_DAY);

        let matching = dir.join("20250901T115112Z-sess-0123456789abcdefghjkmnpqrs.jsonl");
        std::fs::write(&matching, b"{}\n").expect("write matching");
        set_age(&matching, two_days_ago);

        // Same age, same directory, a name this daemon never mints.
        let other = dir.join("notes.jsonl");
        std::fs::write(&other, b"{}\n").expect("write non-matching");
        set_age(&other, two_days_ago);

        // A symlink whose *name* matches the pattern, pointing outside the
        // directory — and a target as old as everything else here, so that a
        // pass which followed the link would find an expired file and unlink it.
        let target = outside.join("precious.txt");
        std::fs::write(&target, b"precious\n").expect("write target");
        set_age(&target, two_days_ago);
        let link = dir.join("20250901T115112Z-sess-zyxwvtsrqpnmkjhgfedcba9876.jsonl");
        std::os::unix::fs::symlink(&target, &link).expect("plant a symlink");

        let report = prune(&dir, 1, now);

        assert_eq!(report.removed, 1, "only the matching regular file goes");
        assert_eq!(
            names(&dir),
            vec![
                "20250901T115112Z-sess-zyxwvtsrqpnmkjhgfedcba9876.jsonl".to_owned(),
                "notes.jsonl".to_owned(),
            ],
            "a non-matching name and a symlink both survive"
        );
        assert_eq!(
            std::fs::read(&target).expect("the symlink target survives"),
            b"precious\n",
            "a symlink is never followed, and its target is untouched"
        );

        // retain_days = 0 is "keep everything".
        let survivor = dir.join("20250901T115112Z-sess-0123456789abcdefghjkmnpqrt.jsonl");
        std::fs::write(&survivor, b"{}\n").expect("write survivor");
        set_age(&survivor, two_days_ago);
        let report = prune(&dir, 0, now);
        assert_eq!(report, PruneReport::default(), "0 means never prune");
        assert!(survivor.exists(), "0 removes nothing, however old");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// BR-13 — a file inside the window is kept, and the boundary is inclusive.
    #[test]
    fn prune_keeps_a_file_inside_the_window() {
        let dir = scratch("window");
        let now = UNIX_EPOCH + Duration::from_secs(1_756_900_272);
        let young = dir.join("20250903T115112Z-sess-0123456789abcdefghjkmnpqrs.jsonl");
        std::fs::write(&young, b"{}\n").expect("write young");
        set_age(&young, now - Duration::from_secs(SECONDS_PER_DAY));

        let report = prune(&dir, 1, now);
        assert_eq!(report.kept, 1);
        assert_eq!(report.removed, 0, "exactly at the window is kept");
        assert!(young.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// BR-13 — the name matcher accepts what `sessions.rs` mints and refuses
    /// every neighbouring shape.
    ///
    /// The `i`/`l`/`o`/`u` row is the one that separates Crockford's alphabet
    /// from RFC 4648's: those four characters are exactly what a session id
    /// cannot contain, so a name carrying one was not minted by this daemon.
    #[test]
    fn the_name_matcher_accepts_only_ids_this_daemon_mints() {
        assert!(is_transcript_file_name(
            "20250903T115112Z-sess-0123456789abcdefghjkmnpqrs.jsonl"
        ));
        for rejected in [
            // uppercase base32
            "20250903T115112Z-sess-0123456789ABCDEFGHJKMNPQRS.jsonl",
            // `i`, `l`, `o`, `u` — present in RFC 4648, absent from Crockford's
            "20250903T115112Z-sess-0123456789abcdefghijkmnpqr.jsonl",
            "20250903T115112Z-sess-0123456789abcdefghjklmnpqr.jsonl",
            "20250903T115112Z-sess-0123456789abcdefghjkmnopqr.jsonl",
            "20250903T115112Z-sess-0123456789abcdefghjkmnpqru.jsonl",
            // 25 and 27 body characters
            "20250903T115112Z-sess-0123456789abcdefghjkmnpqr.jsonl",
            "20250903T115112Z-sess-0123456789abcdefghjkmnpqrst.jsonl",
            // no `sess-` prefix
            "20250903T115112Z-0123456789abcdefghjkmnpqrs.jsonl",
            // a stamp that is not a stamp
            "2025-09-03T115112Z-sess-0123456789abcdefghjkmnpqrs.jsonl",
            "20250903X115112Z-sess-0123456789abcdefghjkmnpqrs.jsonl",
            "20250903T115112z-sess-0123456789abcdefghjkmnpqrs.jsonl",
            // wrong extension, and no extension
            "20250903T115112Z-sess-0123456789abcdefghjkmnpqrs.json",
            "20250903T115112Z-sess-0123456789abcdefghjkmnpqrs",
            // a path, not a name
            "../20250903T115112Z-sess-0123456789abcdefghjkmnpqrs.jsonl",
            "notes.jsonl",
            "",
        ] {
            assert!(
                !is_transcript_file_name(rejected),
                "{rejected:?} is not a name this daemon mints"
            );
        }
    }
}
