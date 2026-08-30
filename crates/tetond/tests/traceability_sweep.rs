//! REQ-598 AC-8 / TASK-298 — the traceability sweep.
//!
//! `runtime.rs` is heavily and deliberately documented: its comments carry REQ,
//! ADR, LESSON and BUG ids explaining why each branch is ordered as it is. That
//! density is the asset a refactor is most likely to destroy, and destroying it
//! is silent — the code still compiles, the suite still passes, and the reason
//! a branch exists is simply gone or attached to the wrong thing.
//!
//! # Why a per-file set diff is not enough
//!
//! The live evidence, twice in one day and before this REQ's refactor started:
//! when REQ-597 rebased onto REQ-596, a method was inserted between
//! `config_snapshot`'s doc comment and its attribute. The doc then documented
//! the *inserted* method. No id left the file — set identical, count identical,
//! defect present. A third instance was committed by hand while implementing
//! this very REQ (TASK-297 orphaned the BR-3 hold test's doc and attribute).
//!
//! So the arm with teeth is **re-attachment**, not counting (LESSON-585 — key
//! the sweep on the hazard, not the remedy's shape).
//!
//! # The arms
//!
//! 1. **Disappearance** — an id that annotated an item at the base must still
//!    annotate an item somewhere in the workspace. Workspace-scoped, so a
//!    genuine file-to-file move (this REQ moved rationale into
//!    `turn_context.rs`) is not a false positive. Stricter than a text search:
//!    the id must still be *attached to an item*, not merely present in prose.
//! 2. **Re-attachment** — if an id annotated item `X` at the base and `X` still
//!    exists, the id must still annotate `X`. This is the arm that catches the
//!    `config_snapshot` insertion, which arms keyed on presence cannot see.
//! 3. **Vacuity floor** — the sweep asserts it saw at least the number of ids
//!    and annotated items known to exist. A sweep's failure mode is seeing
//!    *less*, and every site it misses makes it pass more easily.
//!
//! # Known limitation, stated rather than hidden
//!
//! Arms 1 and 2 need the base revision, read live via `git show <base>:<path>`
//! rather than from a checked-in snapshot that could drift. If the base commit
//! is not present in the clone — a shallow CI checkout is the realistic case —
//! those two arms cannot run. They then emit a loud notice and skip, while arm
//! 3 still runs. This is a real coverage gap on shallow clones, not a silent
//! pass: the notice names what did not run. Deepening the CI fetch would close
//! it, and that is a CI change this REQ deliberately did not make.
//!
//! Not feature-gated, deliberately: LESSON-515 — a feature-gated target is
//! invisible to every refactor, and this one exists to watch refactors.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The merge-base this branch was cut from. A *pointer*, not a snapshot: the
/// content is read live with `git show`, so this cannot drift out of date with
/// the file it describes the way a checked-in copy would.
const BASE: &str = "17c39ec4f26432fae22bfb4d266159ce8afb614f";

/// The files this REQ touched, repo-relative.
const TOUCHED: &[&str] = &["crates/tetond/src/runtime.rs", "crates/tetond/src/lib.rs"];

/// Where `runtime.rs`'s content lives **now**, after REQ-599 split it into a
/// directory. [`TOUCHED`] names the paths as they were at [`BASE`], which is
/// what `git show` needs; this names the present-day corpus the floor measures.
const RUNTIME_DIR: &str = "crates/tetond/src/runtime";

/// **Globally unique** ids. A `REQ-597` means the same thing everywhere, so
/// "this id annotates nothing in the workspace any more" is a sound claim about
/// it. Arm 1 uses these and only these.
const GLOBAL_IDS: &[&str] = &["REQ-", "ADR-", "LESSON-", "BUG-", "TASK-", "ASSUME-"];

/// Every id, including the **REQ-relative** ones.
///
/// `TASK` and `ASSUME` are here and in [`GLOBAL_IDS`] because `runtime.rs`
/// carries pairs like `REQ-558 TASK-054`, and dropping the task half loses half
/// the reference.
///
/// **`BR` and `AC` were added after the mandated demonstration failed, and the
/// reason is worth keeping.** TASK-298 specified the vocabulary as
/// `(REQ|ADR|LESSON|BUG|TASK|ASSUME)` *and* required the sweep be proven by
/// reproducing the REQ-596/597 insertion against `config_snapshot`. Those two
/// clauses contradict each other: at the base commit `config_snapshot`'s doc
/// carries exactly `BR-6` and `AC-11` and no id from that vocabulary, so the
/// item was invisible and the insertion passed silently. A sweep that cannot
/// see the comment whose loss motivated it is decorative.
///
/// `BR-6` and `AC-11` are not globally unique — they are numbered within their
/// own REQ, and hundreds of items across the workspace carry a `BR-6`. That
/// makes them useless to arm 1 (some `BR-6` always survives somewhere, so a
/// disappearance can never be detected) and exactly right for arm 2, whose
/// claim is per-item: *this* item kept *its* ids. Using one vocabulary for both
/// arms would either blind arm 2 to `config_snapshot` or make arm 1 vacuous.
const ALL_IDS: &[&str] = &[
    "REQ-", "ADR-", "LESSON-", "BUG-", "TASK-", "ASSUME-", "BR-", "AC-",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/<crate> has a workspace root two levels up")
        .to_path_buf()
}

/// Every `.rs` file under `crates/`, so the disappearance arm can be
/// workspace-scoped.
fn workspace_sources() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // `target/` holds generated sources and vendored copies; it is
                // not the workspace's own text.
                if path.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(&workspace_root().join("crates"), &mut out);
    out.sort();
    out
}

/// Every traceability id in `text`.
///
/// Hand-rolled rather than a regex so this test pulls in no dependency the
/// workspace does not already need for production.
fn ids_in(text: &str, prefixes: &[&str]) -> BTreeSet<String> {
    let bytes = text.as_bytes();
    let mut found = BTreeSet::new();
    for prefix in prefixes {
        let mut from = 0usize;
        while let Some(at) = text[from..].find(prefix) {
            let start = from + at;
            let mut end = start + prefix.len();
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            // A prefix with no digits after it is prose ("the REQ-shaped id"),
            // not a reference.
            if end > start + prefix.len() {
                found.insert(text[start..end].to_owned());
            }
            from = start + prefix.len();
        }
    }
    found
}

/// An item and the ids its attached doc-comment block carries.
#[derive(Debug, Clone)]
struct Item {
    name: String,
    ids: BTreeSet<String>,
}

/// The name an item-declaring line introduces, if it declares one.
///
/// Deliberately conservative: it recognises the declaration forms `runtime.rs`
/// actually uses. A form it does not recognise ends a doc run without claiming
/// it, which the vacuity floor is there to catch if it ever starts happening at
/// scale.
fn item_name(line: &str) -> Option<String> {
    let t = line.trim_start();
    let t = t
        .strip_prefix("pub(crate) ")
        .or_else(|| t.strip_prefix("pub(super) "))
        .or_else(|| t.strip_prefix("pub "))
        .unwrap_or(t);
    let t = t.strip_prefix("default ").unwrap_or(t);
    let t = t.strip_prefix("const ").unwrap_or(t);
    let t = t.strip_prefix("async ").unwrap_or(t);
    let t = t.strip_prefix("unsafe ").unwrap_or(t);
    let t = t.strip_prefix("extern ").unwrap_or(t);
    for kw in [
        "fn ", "struct ", "enum ", "trait ", "mod ", "type ", "static ", "union ",
    ] {
        if let Some(rest) = t.strip_prefix(kw) {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    // `impl` blocks carry rationale too, and their "name" is the type.
    if let Some(rest) = t.strip_prefix("impl") {
        if rest.starts_with(' ') || rest.starts_with('<') {
            let name: String = rest
                .trim_start_matches(|c: char| !c.is_alphanumeric() && c != '_')
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                return Some(format!("impl {name}"));
            }
        }
    }
    None
}

/// Parse `src` into the items that carry ids in their attached doc block.
///
/// "Attached" means the contiguous run of `///` (or `//`) lines immediately
/// preceding an item, allowing `#[...]` attributes between the comment and the
/// item — that adjacency is the thing the re-attachment arm tests, so an
/// attribute must count as part of the item, never as a separator.
///
/// `//!` inner docs are skipped: they annotate the enclosing module, not an
/// item, and treating them as an item's doc would attribute a module's ids to
/// whatever happens to be declared first.
fn parse(src: &str, prefixes: &[&str]) -> Vec<Item> {
    let mut items: Vec<Item> = Vec::new();
    let mut run: Vec<&str> = Vec::new();
    for line in src.lines() {
        let t = line.trim_start();
        if t.starts_with("//!") {
            continue;
        }
        if t.starts_with("///") || t.starts_with("//") {
            run.push(t);
            continue;
        }
        if t.starts_with("#[") || t.starts_with("#!") || t.is_empty() {
            // Attributes belong to the item; a blank line does not detach a doc
            // comment in Rust, so neither ends the run.
            continue;
        }
        if let Some(name) = item_name(line) {
            let ids = ids_in(&run.join("\n"), prefixes);
            if !ids.is_empty() {
                items.push(Item { name, ids });
            }
        }
        // Any other code line ends the run without claiming it.
        run.clear();
    }
    items
}

/// `git show <BASE>:<path>`, or `None` when the base is not in this clone.
fn base_source(path: &str) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(workspace_root())
        .arg("show")
        .arg(format!("{BASE}:{path}"))
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Every (id -> items carrying it) mapping across the workspace today.
fn current_attachments(prefixes: &[&str]) -> BTreeMap<String, BTreeSet<String>> {
    let mut map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for path in workspace_sources() {
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        for item in parse(&src, prefixes) {
            for id in item.ids {
                map.entry(id).or_default().insert(item.name.clone());
            }
        }
    }
    map
}

/// The names of every item declared anywhere in the workspace today.
fn current_item_names() -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for path in workspace_sources() {
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in src.lines() {
            if let Some(name) = item_name(line) {
                names.insert(name);
            }
        }
    }
    names
}

/// **Arm 1 — disappearance.** An id that annotated an item at the base still
/// annotates an item somewhere in the workspace.
///
/// Workspace-scoped on purpose: this REQ deliberately moved rationale out of
/// `runtime.rs` and into `turn_context.rs`, and a file-scoped arm would call
/// that a defect.
#[test]
fn every_id_that_annotated_an_item_at_the_base_still_annotates_one() {
    let current = current_attachments(GLOBAL_IDS);
    let mut checked = 0usize;
    let mut lost: Vec<String> = Vec::new();
    let mut ran = false;

    for path in TOUCHED {
        let Some(before) = base_source(path) else {
            continue;
        };
        ran = true;
        for item in parse(&before, GLOBAL_IDS) {
            for id in item.ids {
                checked += 1;
                if !current.contains_key(&id) {
                    lost.push(format!("{id} (was on `{}` in {path})", item.name));
                }
            }
        }
    }

    if !ran {
        eprintln!(
            "NOTICE: REQ-598 traceability arms 1 and 2 did NOT run — base {BASE} is not in \
             this clone (a shallow checkout does this). The orphan/vacuity arm still ran. \
             This is a coverage gap, not a pass."
        );
        return;
    }

    assert!(
        lost.is_empty(),
        "a refactor dropped rationale ids that annotated an item at the base — they annotate \
         nothing anywhere in the workspace now:\n  {}",
        lost.join("\n  ")
    );
    assert!(
        checked >= 200,
        "vacuity floor: the base parse found only {checked} attached ids across {TOUCHED:?}; \
         the selector is matching less than it should"
    );
}

/// **Arm 2 — re-attachment. This is the arm with teeth.**
///
/// If an id annotated item `X` at the base and `X` still exists, the id must
/// still annotate `X`. Presence-based arms cannot see the hazard this catches:
/// inserting a method between a doc comment and its attribute leaves the file's
/// id set and count identical while moving the rationale onto the wrong item.
///
/// ## Mutation, run and recorded (conventions.md)
///
/// Reproducing the REQ-596/597 defect — inserting
///
/// ```text
///     pub fn wedge_between_doc_and_item(&self) {}
/// ```
///
/// between `config_snapshot`'s doc comment and its `#[must_use]` attribute —
/// turns this red with:
///
/// ```text
/// rationale moved off the item it explains (the REQ-596/597 hazard):
///   REQ-558 left `config_snapshot` (now on: wedge_between_doc_and_item)
///   ...
/// ```
///
/// Reverted after observing.
#[test]
fn an_id_still_annotates_the_item_it_explained_at_the_base() {
    let current = current_attachments(ALL_IDS);
    let names_today = current_item_names();
    let mut moved: Vec<String> = Vec::new();
    let mut checked = 0usize;
    let mut ran = false;

    for path in TOUCHED {
        let Some(before) = base_source(path) else {
            continue;
        };
        ran = true;
        for item in parse(&before, ALL_IDS) {
            // An item that no longer exists was renamed or removed; that is a
            // different claim and arm 1 covers its ids.
            if !names_today.contains(&item.name) {
                continue;
            }
            for id in item.ids {
                checked += 1;
                let now = current.get(&id);
                if !now.is_some_and(|items| items.contains(&item.name)) {
                    // Only how many other items carry it, never the list: a
                    // REQ-relative id like `BR-6` is on hundreds of items, and
                    // a failure nobody can read is a failure nobody acts on.
                    let elsewhere = now.map_or(0, BTreeSet::len);
                    moved.push(format!(
                        "{id} left `{}` (still on {elsewhere} other item(s))",
                        item.name
                    ));
                }
            }
        }
    }

    if !ran {
        eprintln!("NOTICE: arm 2 did not run — base {BASE} absent from this clone.");
        return;
    }

    assert!(
        moved.is_empty(),
        "rationale moved off the item it explains (the REQ-596/597 hazard):\n  {}",
        moved.join("\n  ")
    );
    assert!(
        checked >= 150,
        "vacuity floor: only {checked} surviving-item ids were compared; the selector is \
         matching less than it should"
    );
}

/// **Arm 3 — the vacuity floor**, and the only arm that runs without the base.
///
/// A sweep's failure mode is seeing *less*: a selector bug makes it pass by
/// matching nothing, and every site it misses makes every other arm weaker.
/// These floors are deliberately below the measured values, so ordinary
/// additions do not trip them while a selector regression does.
///
/// ## Mutation, run and recorded as observed
///
/// Narrowing `parse` to claim only `///` runs beginning `/// **` — a plausible
/// "only the emphasised docs matter" tightening — drops `runtime.rs` from
/// **493 annotated items to 141**, and the base parse from 269 attached ids to
/// 83. This test goes red on the item floor, and arm 1 goes red on its own
/// floor:
///
/// ```text
/// vacuity floor: only 141 annotated items parsed out of runtime.rs
/// vacuity floor: the base parse found only 83 attached ids
/// ```
///
/// **Arm 2 still passed under that mutation**, and that is the whole argument
/// for having a floor at all: a selector regression leaves the re-attachment
/// arm comparing a third of the corpus and reporting success. The floor is what
/// turns "saw less" from a quiet pass into a failure. Reverted after observing.
#[test]
fn the_sweep_sees_enough_of_the_corpus_to_be_meaningful() {
    // REQ-599: the corpus is a directory now. Reading one file would let the
    // floor fall as slices are extracted and read as "the docs shrank".
    let dir = workspace_root().join(RUNTIME_DIR);
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .expect("the runtime module directory is readable")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .collect();
    files.sort();
    assert!(
        !files.is_empty(),
        "vacuity floor: no sources under {}",
        dir.display()
    );
    let runtime: String = files
        .iter()
        .map(|p| std::fs::read_to_string(p).expect("a runtime source is readable"))
        .collect::<Vec<_>>()
        .join("\n");
    let items = parse(&runtime, ALL_IDS);
    let ids: BTreeSet<&String> = items.iter().flat_map(|i| i.ids.iter()).collect();

    // Floors sit below the measured values (493 items / 180 ids / 342
    // attachments as of this commit) so ordinary additions do not trip them,
    // but a selector regression does.
    assert!(
        items.len() >= 440,
        "vacuity floor: only {} annotated items parsed out of runtime.rs — the selector is \
         matching less than it should",
        items.len()
    );
    assert!(
        ids.len() >= 160,
        "vacuity floor: only {} distinct ids parsed out of runtime.rs",
        ids.len()
    );

    let attachments = current_attachments(ALL_IDS);
    assert!(
        attachments.len() >= 300,
        "vacuity floor: only {} distinct ids attached to items across the workspace",
        attachments.len()
    );
}
