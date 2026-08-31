//! REQ-602 AC-2 — a ratchet on the `runtime/` submodule visibility surface.
//!
//! # Why a ratchet and not a search
//!
//! The obvious guard is a test that decides, per item, whether anything outside
//! `runtime/` needs it. That is what this REQ tried, three times, and got three
//! different wrong answers:
//!
//! | method | answer |
//! |---|---:|
//! | a review sample, rule unstated | ~24 needed |
//! | bare name appears outside `runtime/` | 24–30 needed |
//! | qualified `crate::runtime::<item>` reference | 5 needed |
//! | demote all, build, read the errors | 5 |
//! | **the same, re-run under adversarial review** | **4, exactly** |
//!
//! The bare-name search counted **prose** — `startup_lifecycle`, `seam_policy`,
//! `cause_taints_the_session`, `derive_provider_setup` and
//! `LOCAL_ENGINE_N_CTX` appear outside the tree only inside doc comments like
//! `// see \`runtime::startup_lifecycle\``. The qualified-path rule fixed that
//! and broke differently: it missed `LOCAL_ENGINE_N_CTX`, imported on its own
//! line rather than in a group.
//!
//! So this file does **not** re-implement the decision. It pins the *result*,
//! and records how to re-derive it. A search-shaped test here would encode the
//! fourth wrong answer.
//!
//! **The fifth wrong answer was this file's own.** The first demote-all pass
//! kept `RenderedProviderSetup` crate-wide on the reasoning that it "must match
//! its `pub(crate)` accessor", `derive_provider_setup`. Review asked why *that*
//! was `pub(crate)`, and the answer was that nothing outside `runtime/` names
//! it — every hit was prose, including one this file's own table cites as a
//! false positive. Narrowing the accessor to private drops the type to
//! `pub(super)`, and the build is clean. The demote-all method was right; it
//! had been applied to the submodules and not to the `mod.rs` item holding one
//! of them open. **The surface is four.**
//!
//! # Why `mod.rs` is excluded
//!
//! `pub(super)` in `mod.rs` **is** `pub(crate)`: `mod.rs` is the `runtime`
//! module and its parent is the crate root. Rewriting its qualifiers *to
//! `pub(super)`* is a semantic no-op, and counting those as work is how an
//! earlier draft of AC-1 reached "143 → 8".
//!
//! **`mod.rs` is excluded from the count, not from the rule.** Narrowing to
//! *private* there is real, and BR-2 asks for it: `derive_provider_setup` was
//! made private in exactly this way, which is what reduced the surface to four.
//! A future item in `mod.rs` that nothing outside `runtime/` names should go
//! private too — this ratchet will not tell you so, because it does not read
//! `mod.rs`, and that is a stated limit rather than an oversight.
//!
//! # The figures, each with its rule
//!
//! Two rules, and the branch previously paired the first's "before" with the
//! second's "after" — which is the bug this REQ exists to correct, committed
//! inside the guard written to prevent it:
//!
//! | rule | before (`8902439`) | after |
//! |---|---:|---:|
//! | occurrences of the token `pub(crate)` in the seven submodules | 131 | 5 |
//! | **item declarations** carrying `pub(crate)` there | **88** | **4** |
//!
//! The occurrence figures exceed the declaration figures because they include
//! struct fields, `use` re-exports, and prose. One prose line survives at
//! `views.rs:205`, a doc comment discussing the visibility of the function
//! declared two lines below it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The submodule files that must be present, whatever else the walk finds.
///
/// Not the corpus — [`submodules`] enumerates that from disk. This is the
/// **shape floor**: a walk that returned the wrong directory, or returned
/// nothing, would otherwise produce an empty surface and agree with any tree
/// (LESSON-585). Keyed on the hazard rather than on a bare count, because a
/// count floor is cleared by a walk that finds only the wrong files.
const MUST_BE_PRESENT: &[&str] = &[
    "config_document.rs",
    "duty.rs",
    "engine.rs",
    "provider.rs",
    "taint.rs",
    "testsupport.rs",
    "views.rs",
];

/// What the surface is allowed to be, and who needs each member.
///
/// **How to re-derive this list** (the only method that has been right):
/// rewrite every `pub(crate)` under `crates/tetond/src/runtime/` to
/// `pub(super)`, run `cargo check --workspace --all-targets`, and read the
/// `E0603 … is private` errors. Those are the items something outside the tree
/// genuinely reaches. Restore exactly those and no others.
///
/// | item | reached from |
/// |---|---|
/// | `LOCAL_ENGINE_N_CTX` | `egress/redact.rs`, `harness/budget.rs`, `harness/compact.rs` |
/// | `TAINT_BY_CONTEXT`, `taint_pin_line` | `carry.rs` |
/// | `endpoint_query_names_a_credential` | `provider_recipes.rs`, `web_setup_catalog.rs` |
///
/// `RenderedProviderSetup` was on this list and is not any more: see the module
/// docs. Nothing outside `runtime/` names it, and the accessor that appeared to
/// require it did not need crate reach either.
const CRATE_WIDE: &[&str] = &[
    "LOCAL_ENGINE_N_CTX",
    "TAINT_BY_CONTEXT",
    "endpoint_query_names_a_credential",
    "taint_pin_line",
];

/// The submodule items that are **`pub`** — wider than `pub(crate)`, and
/// therefore part of the crate's public surface.
///
/// This list exists because the ratchet was blind to bare `pub` and review
/// proved the blindness exploitable: changing `taint.rs`'s `pub(super) fn lift`
/// to `pub fn lift` compiles, makes the lift reachable from
/// `crate::harness::tools` — which `taint.rs`'s own header says "does not
/// compile", it being where a model's tool call lands — and leaves the entire
/// suite, this ratchet included, green. Guarding only the narrower spelling
/// while the wider one goes unwatched is the worst shape a ratchet can have.
///
/// Entries are `file.rs::name`. Adding one means arguing that something outside
/// this crate needs it.
const PUBLIC: &[&str] = &[
    "engine.rs::test_seams_enabled",
    // REQ-600. **Not a widening** — `run_prompt_turn` was already `pub`, in
    // `mod.rs`, where it was reachable exactly as far. What changed is the
    // *corpus*: `mod.rs` is excluded from this scan and `turn.rs` is not, so
    // relocating the method brought an existing public item into view. The
    // count below moves 13 -> 14 for that reason and no other.
    //
    // It earns `pub` rather than `pub(crate)`: demoting it fails to compile
    // against the `provenance_egress` integration test, which links the lib
    // from outside the crate. Established by demoting and building, never by
    // grepping for the name (LESSON-596).
    "turn.rs::run_prompt_turn",
    "taint.rs::SessionTaint",
    "taint.rs::SessionTaintView",
    "taint.rs::WebTaintOverride",
    "taint.rs::is_lifted",
    "taint.rs::is_tainted",
    "taint.rs::mark",
    "taint.rs::new",
    "taint.rs::try_mark",
    "views.rs::BoundaryPosture",
    "views.rs::builtin_count",
    "views.rs::effective_is_empty",
];

/// How many `pub` declarations there are, counting duplicates.
///
/// One more than `PUBLIC.len()`: `taint.rs` declares `pub fn new` twice, on
/// `SessionTaint` and on `WebTaintOverride`. Pinned separately so a *third*
/// `pub fn new` in that file cannot hide inside a name that is already allowed.
///
/// 13 -> 14 at REQ-600, when `run_prompt_turn` moved from `mod.rs` (outside
/// this scan) into `turn.rs` (inside it). The item's visibility did not change.
const PUBLIC_DECLARATIONS: usize = 14;

fn runtime_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/runtime")
}

/// Every `pub(crate)` **declaration** in the submodules, by item name.
///
/// Anchored on the line's first non-space characters, so a doc comment
/// discussing `pub(crate)` is never counted. That is not hypothetical: the same
/// miscount inflated the suppression ratchet from 16 to 17, and produced this
/// REQ's own 48–52 estimate.
/// The item name a single line declares as `pub(crate)`, if it declares one.
///
/// Split out so the anchoring can be tested against strings rather than against
/// whatever the corpus happens to contain — a non-vacuity check that depends on
/// the tree is a check that quietly weakens as the tree changes.
fn declared_on_line(line: &str) -> Option<String> {
    declared_with(line, "pub(crate) ")
}

/// Every declaration in the submodules at least as wide as `pub(crate)`,
/// as `file.rs::name`, **with duplicates preserved**.
///
/// Keyed by file because a bare name is ambiguous — `taint.rs` has two
/// `pub fn new` — and returned as a `Vec` so the count is checkable: two items
/// with the same `file::name` collapse in a set, and a set-only guard would let
/// a third `pub fn new` in `taint.rs` arrive unseen.
fn declared_at_least_crate_wide(vis: &str) -> Vec<String> {
    let mut found = Vec::new();
    for path in submodules() {
        let file = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("unreadable {}: {err}", path.display()));
        for line in text.lines() {
            if let Some(name) = declared_with(line, vis).or_else(|| field_with(line, vis)) {
                found.push(format!("{file}::{name}"));
            }
        }
    }
    found.sort();
    found
}

/// A struct or enum **field** carrying `vis` — `pub(crate) taint: bool`.
///
/// Fields were outside the surface until review pointed out that the diff
/// narrowed roughly twenty-five of them (`SessionTaintView.taint`,
/// `TaintingPrivacySink.*`, `EngineSlot.*`, …). The thing this REQ actually
/// changed was the thing the ratchet could not re-check: re-promoting
/// `SessionTaintView`'s two fields to `pub(crate)` left every test green, and
/// that type is `pub use`-exported, so the fields become crate-wide readable
/// from `harness::tools`. The doc-block's own re-derivation method — demote
/// all, build, read E0603 — covers fields, so the parser has to as well.
fn field_with(line: &str, vis: &str) -> Option<String> {
    let rest = line.trim_start().strip_prefix(vis)?;
    let (name, tail) = rest.split_once(':')?;
    let name = name.trim();
    // A field is `name: Type`, never `name(` or `name<`; and `::` is a path.
    if name.is_empty()
        || tail.starts_with(':')
        || !name.chars().all(|c| c.is_alphanumeric() || c == '_')
        || name.starts_with(|c: char| c.is_uppercase())
    {
        return None;
    }
    Some(name.to_owned())
}

/// The same, for any visibility qualifier — so the parser's own health can be
/// measured against a population that does not move when `CRATE_WIDE` does.
fn declared_with(line: &str, vis: &str) -> Option<String> {
    // Anchored: the qualifier must be the line's first non-space text, so
    // `/// … pub(crate) …` in a doc comment is never a declaration.
    let rest = line.trim_start().strip_prefix(vis)?;
    // `async` is a modifier; `const` is BOTH a modifier (`const fn`) and an item
    // kind (`const NAME: T`). Getting that wrong is what made the first draft of
    // this parser miss `LOCAL_ENGINE_N_CTX` and `TAINT_BY_CONTEXT` — the same
    // two-meanings-one-keyword slip that produced an earlier wrong count.
    let rest = rest.strip_prefix("async ").unwrap_or(rest);
    let after = if let Some(r) = rest.strip_prefix("const ") {
        r.strip_prefix("fn ").unwrap_or(r)
    } else {
        // Kept in step with `runtime_doc_paths.rs`'s parser, written the same
        // day with a wider list. A `pub(crate) unsafe fn` or `pub(crate) union`
        // that only one of the two recognises is a declaration this ratchet
        // cannot see, which is the same silent-blindness this file exists to
        // prevent.
        let rest = ["unsafe ", "default "]
            .iter()
            .find_map(|m| rest.strip_prefix(m))
            .unwrap_or(rest);
        [
            "fn ", "struct ", "enum ", "trait ", "type ", "static ", "union ",
        ]
        .iter()
        .find_map(|kw| rest.strip_prefix(kw))?
    };
    let n: String = after
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!n.is_empty()).then_some(n)
}

/// Every `.rs` file under `runtime/` except `mod.rs`, **recursively**.
///
/// Enumerated from disk rather than listed. A hardcoded list is a corpus that
/// stops growing while the tree does not: REQ-600 extracts `runtime/turn.rs`
/// and REQ-603 extracts a session module, and against a frozen list this ratchet
/// would stay green while the crate-wide surface grew without bound — the
/// LESSON-594 failure, arriving through the guard rather than through the code.
/// The whole stated case for landing REQ-602 before REQ-600 is that REQ-600's
/// new slices inherit whatever this holds, so it has to be able to see them.
///
/// `mod.rs` is excluded because `pub(super)` there *is* `pub(crate)`; see the
/// module docs for why that is an exclusion from the *count*, not from the rule.
fn submodules() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let listing = std::fs::read_dir(dir)
            .unwrap_or_else(|err| panic!("unreadable {}: {err}", dir.display()));
        for entry in listing.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs")
                && path.file_name().is_some_and(|n| n != "mod.rs")
            {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(&runtime_dir(), &mut out);
    out.sort();
    out
}

fn declared_pub_crate() -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for path in submodules() {
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("unreadable {}: {err}", path.display()));
        found.extend(text.lines().filter_map(declared_on_line));
    }
    found
}

/// **The ratchet, bounded on both sides.**
///
/// A climb is a new item reaching out of the module tree — name it here with
/// its consumer, or narrow it. A **drop** is equally suspicious: it more often
/// means this parser stopped matching than that the code improved, which is the
/// failure mode every sweep in this REQ line has had at least once.
///
/// ## Mutations, re-run and recorded **as observed**
///
/// Every line below is output that actually appeared. Where a prediction was
/// wrong, the prediction is kept beside it — an earlier version of this table
/// recorded an outcome for mutation 4 that **cannot occur**, and review caught
/// it: two `assert!`s in one test cannot both fire, so "with the upper bound
/// firing alongside it" described something no run had produced.
///
/// | # | mutation | observed |
/// |---|---|---|
/// | 1 | promote `views::setup_answer` to `pub(crate)` | `unexpectedly crate-visible: ["setup_answer"]` |
/// | 2 | delete `"taint_pin_line"` from `CRATE_WIDE` | `unexpectedly crate-visible: ["taint_pin_line"]` — **not** the `missing` message predicted; removing a name makes the item found-but-unexpected, tripping the *upper* bound |
/// | 3 | narrow `taint_pin_line` to `pub(super)` in source, keep it in `CRATE_WIDE` | **does not compile** — `carry.rs` needs it. The lower bound is unreachable this way; the compiler refuses first |
/// | 4 | add `"an_item_that_does_not_exist"` to `CRATE_WIDE` | `expected to be crate-visible but is not: ["an_item_that_does_not_exist"]` — the lower bound, finally observed |
/// | 5 | point the walk at `src/projects` | the **presence** floor: `did not find \`config_document.rs\`` |
/// | 6 | make the parser stop recognising `const` | `expected to be crate-visible but is not: ["LOCAL_ENGINE_N_CTX", "TAINT_BY_CONTEXT"]` |
///
/// **Mutation 4 also caught a defect in the floor written above it.** The
/// declaration floor was first expressed as `found.len() >= CRATE_WIDE.len()`.
/// Adding a bogus name raises `CRATE_WIDE.len()`, so that floor fired *instead
/// of* the `missing` arm it was meant to exercise — leaving the lower bound
/// unobserved for a second time, one commit after review pointed out it had
/// never been observed at all. The floor now measures `pub(super)`
/// declarations, a population that does not move when this ratchet's
/// expectations do.
///
/// All reverted after observing. Recording mutation 3's compile error rather
/// than dropping it is the point: a mutation that "passes" because the build
/// never ran is the false-green this REQ line has already shipped once.
#[test]
fn the_crate_wide_surface_is_exactly_the_four_items_that_earn_it() {
    let found = declared_pub_crate();
    let expected: BTreeSet<String> = CRATE_WIDE.iter().map(|s| (*s).to_owned()).collect();

    // Vacuity floors, keyed on the hazards that can actually make this blind.
    //
    // `!found.is_empty()` was the floor here and it could never fire: `expected`
    // always holds four names, so an empty `found` fails the `missing` assert
    // below regardless. A floor that cannot fire is not a floor. The two ways
    // this ratchet can go quiet while staying green are a corpus that lost
    // files and a parser that stopped matching — so those are what is floored.
    let corpus = submodules();
    let names: BTreeSet<String> = corpus
        .iter()
        .filter_map(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .collect();
    for required in MUST_BE_PRESENT {
        assert!(
            names.contains(*required),
            "vacuity floor: the walk over {} did not find `{required}`. It returned \
             {} file(s): {names:?}. A walk pointed at the wrong directory, or one that \
             stopped matching, yields an empty surface — which agrees with any tree \
             (LESSON-585). Keyed on files that must exist rather than on a count, \
             because a count is cleared by finding the wrong files.",
            runtime_dir().display(),
            corpus.len()
        );
    }
    // The parser's own health, measured on a population that does not move when
    // `CRATE_WIDE` does. Floored against `CRATE_WIDE.len()` at first, which was
    // wrong in a way only a mutation showed: adding a bogus name to `CRATE_WIDE`
    // raised the bar and tripped *this* assert instead of the `missing` arm it
    // was meant to exercise, leaving the lower bound unobserved for a second
    // time. `pub(super)` declarations are the right population — there are ~114
    // and nothing about this ratchet's expectations changes their count.
    let recognised: usize = corpus
        .iter()
        .map(|path| {
            let text = std::fs::read_to_string(path).expect("a submodule is readable");
            text.lines()
                .filter(|l| declared_with(l, "pub(super) ").is_some())
                .count()
        })
        .sum();
    assert!(
        recognised >= 50,
        "vacuity floor: the parser recognised only {recognised} `pub(super)` declaration(s) \
         across {} file(s), where the tree has over a hundred. The parser has stopped \
         matching a declaration shape — which reports a shrinking crate-wide surface as \
         an improvement.",
        corpus.len()
    );

    let extra: Vec<_> = found.difference(&expected).cloned().collect();
    assert!(
        extra.is_empty(),
        "unexpectedly crate-visible: {extra:?}\n\
         Something outside `runtime/` may genuinely need these — but establish that the \
         way the surface was established in the first place: demote all `pub(crate)` under \
         `runtime/` to `pub(super)`, build, and read the E0603 errors. Do NOT grep for the \
         name; three searches gave three wrong answers, all of them counting prose in doc \
         comments as usage. If it earns crate visibility, add it to CRATE_WIDE with its \
         consumer."
    );

    let missing: Vec<_> = expected.difference(&found).cloned().collect();
    assert!(
        missing.is_empty(),
        "expected to be crate-visible but is not: {missing:?}\n\
         A drop is not automatically good news. Either the item was narrowed deliberately \
         — in which case remove it from CRATE_WIDE and say so — or this parser stopped \
         matching its declaration, which is how a sweep starts seeing less and passing."
    );
}

/// **The `pub` surface, watched the same way.**
///
/// `pub` is wider than `pub(crate)`, so a ratchet that guards only the narrower
/// spelling watches the wrong door. Review demonstrated the gap by promoting
/// `taint.rs`'s `lift` to `pub` — it compiled, became callable from
/// `crate::harness::tools`, and every test stayed green.
///
/// Fields are included. The diff this REQ shipped narrowed about twenty-five of
/// them, and re-promoting `SessionTaintView`'s two left the suite green: the
/// type is `pub use`-exported, so its fields become crate-wide readable from
/// exactly the module `taint.rs` says must not reach them.
#[test]
fn nothing_in_the_submodules_is_public_but_the_dozen_that_are_meant_to_be() {
    let found = declared_at_least_crate_wide("pub ");
    let unique: BTreeSet<&String> = found.iter().collect();
    let expected: BTreeSet<String> = PUBLIC.iter().map(|s| (*s).to_owned()).collect();

    assert_eq!(
        found.len(),
        PUBLIC_DECLARATIONS,
        "the number of `pub` declarations under `runtime/` changed: {} found, \
         {PUBLIC_DECLARATIONS} pinned.\nfound: {found:?}\n\
         The count is pinned alongside the names because `taint.rs` declares \
         `pub fn new` twice; without it a third would hide inside an allowed name.",
        found.len()
    );

    let extra: Vec<_> = unique.iter().filter(|n| !expected.contains(**n)).collect();
    assert!(
        extra.is_empty(),
        "unexpectedly `pub`: {extra:?}\n\
         `pub` puts an item on the crate's public surface, which is wider than \
         anything BR-2 discusses. If that is intended, add it to `PUBLIC` with the \
         reason. If it is a slip, it is the one the review of this REQ demonstrated: \
         `pub fn lift` in `taint.rs` compiles, reaches `crate::harness::tools`, and \
         breaks the invariant that file's header states — silently."
    );

    let missing: Vec<_> = expected.iter().filter(|n| !unique.contains(n)).collect();
    assert!(
        missing.is_empty(),
        "pinned as `pub` but not found: {missing:?}\n\
         Either it was narrowed deliberately — remove it from `PUBLIC` and say so — \
         or this parser stopped matching a declaration it used to see."
    );
}

/// No submodule field is `pub(crate)`.
///
/// A separate, empty-set ratchet rather than a line in `CRATE_WIDE`: there are
/// none today, and "none" is the strongest form this can take. A field that
/// earns crate visibility should have to argue for it here.
#[test]
fn no_submodule_field_reaches_beyond_the_module_tree() {
    let fields: Vec<String> = submodules()
        .iter()
        .flat_map(|path| {
            let file = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let text = std::fs::read_to_string(path).expect("a submodule is readable");
            text.lines()
                .filter_map(|l| field_with(l, "pub(crate) "))
                .map(|n| format!("{file}::{n}"))
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(
        fields.is_empty(),
        "these struct fields are `pub(crate)`: {fields:?}\n\
         A field is as reachable as a function. `SessionTaintView` is \
         `pub use`-exported, so a `pub(crate)` field on it is readable from \
         `crate::harness::tools` — which `taint.rs`'s header says must not happen."
    );
}

/// The parser counts declarations, never the prose that discusses them.
///
/// A direct guard on the miscount that produced this REQ's own wrong estimate,
/// and on the one that inflated the suppression ratchet from 16 to 17. Tested
/// on strings rather than on the tree: a non-vacuity check that depends on what
/// the corpus happens to contain weakens silently as the corpus changes, which
/// is how this test failed its first draft.
#[test]
fn only_a_declaration_counts_never_a_mention() {
    // Real declarations, including the two shapes the first parser missed.
    for (line, want) in [
        (
            "pub(crate) fn taint_pin_line(cause: &str) -> String {",
            "taint_pin_line",
        ),
        (
            "pub(crate) const LOCAL_ENGINE_N_CTX: u32 = 16_384;",
            "LOCAL_ENGINE_N_CTX",
        ),
        (
            "    pub(crate) struct RenderedProviderSetup {",
            "RenderedProviderSetup",
        ),
        ("pub(crate) const fn wire_stage() -> u8 {", "wire_stage"),
        ("pub(crate) async fn probe() {", "probe"),
    ] {
        assert_eq!(
            declared_on_line(line).as_deref(),
            Some(want),
            "should have been read as a declaration of `{want}`: {line}"
        );
    }

    // Prose and non-item forms, which must all be invisible.
    for line in [
        "/// something outside the tree reaches it, so pub(crate) fn ghost() stays",
        "// pub(crate) const GHOST: u32 = 1;",
        "//! the module used to carry pub(crate) struct Ghost {",
        "pub(crate) use engine::*;",
        "pub(crate) mod testsupport;",
        "    pub(crate) tainted: Mutex<HashSet<SessionId>>,",
        "pub fn genuinely_public() {",
    ] {
        assert_eq!(
            declared_on_line(line),
            None,
            "must NOT be read as a declaration: {line}"
        );
    }
}
