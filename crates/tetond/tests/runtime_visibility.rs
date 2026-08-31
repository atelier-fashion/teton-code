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
//! | **demote all, build, read the errors** | **5, exactly** |
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
//! # Why `mod.rs` is excluded
//!
//! `pub(super)` in `mod.rs` **is** `pub(crate)`: `mod.rs` is the `runtime`
//! module and its parent is the crate root. Counting its qualifiers as part of
//! the surface treats a semantic no-op as work — which is how an earlier draft
//! of AC-1 reached "143 → 8" instead of the submodules' real 130 → 5.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The submodules of `runtime/`. `mod.rs` is deliberately absent — see the
/// module docs.
const SUBMODULES: &[&str] = &[
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
/// | `RenderedProviderSetup` | must match its own `pub(crate)` accessor |
const CRATE_WIDE: &[&str] = &[
    "LOCAL_ENGINE_N_CTX",
    "RenderedProviderSetup",
    "TAINT_BY_CONTEXT",
    "endpoint_query_names_a_credential",
    "taint_pin_line",
];

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
    // Anchored: the qualifier must be the line's first non-space text, so
    // `/// … pub(crate) …` in a doc comment is never a declaration.
    let rest = line.trim_start().strip_prefix("pub(crate) ")?;
    // `async` is a modifier; `const` is BOTH a modifier (`const fn`) and an item
    // kind (`const NAME: T`). Getting that wrong is what made the first draft of
    // this parser miss `LOCAL_ENGINE_N_CTX` and `TAINT_BY_CONTEXT` — the same
    // two-meanings-one-keyword slip that produced an earlier wrong count.
    let rest = rest.strip_prefix("async ").unwrap_or(rest);
    let after = if let Some(r) = rest.strip_prefix("const ") {
        r.strip_prefix("fn ").unwrap_or(r)
    } else {
        ["fn ", "struct ", "enum ", "trait ", "type ", "static "]
            .iter()
            .find_map(|kw| rest.strip_prefix(kw))?
    };
    let n: String = after
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!n.is_empty()).then_some(n)
}

fn declared_pub_crate() -> BTreeSet<String> {
    let dir = runtime_dir();
    let mut found = BTreeSet::new();
    for name in SUBMODULES {
        let path = dir.join(name);
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
/// ## Mutations, run and recorded **as observed** — two predictions were wrong
///
/// 1. Promoting `views::setup_answer` from `pub(super)` to `pub(crate)` →
///    `unexpectedly crate-visible: ["setup_answer"]`. As predicted.
/// 2. Deleting `"taint_pin_line"` from `CRATE_WIDE` → also
///    `unexpectedly crate-visible`, **not** the `missing` message predicted.
///    Removing a name makes the item *found but unexpected*, so it trips the
///    upper bound. The prediction had the branch backwards.
/// 3. Narrowing `taint_pin_line` to `pub(super)` in source while leaving it in
///    `CRATE_WIDE` — intended to exercise the lower bound — **does not compile**
///    (`carry.rs` needs it). The lower bound cannot be reached that way at all:
///    the compiler refuses first. That is worth knowing rather than papering
///    over — the `missing` arm is belt-and-braces *behind* the build, and only
///    fires when the item is genuinely no longer needed (in which case
///    `CRATE_WIDE` should be updated) or when this parser stops matching a
///    declaration it used to see.
/// 4. So the lower bound was exercised by simulating exactly that: renaming a
///    `CRATE_WIDE` entry to `taint_pin_line_RENAMED`, standing in for a parser
///    that has gone blind →
///    `expected to be crate-visible but is not: ["taint_pin_line_RENAMED"]`,
///    with the upper bound firing alongside it.
///
/// All reverted after observing. Recording mutation 3's compile error rather
/// than dropping it is the point: a mutation that "passes" because the build
/// never ran is the false-green this REQ line has already shipped once.
#[test]
fn the_crate_wide_surface_is_exactly_the_five_items_that_earn_it() {
    let found = declared_pub_crate();
    let expected: BTreeSet<String> = CRATE_WIDE.iter().map(|s| (*s).to_owned()).collect();

    assert!(
        !found.is_empty(),
        "vacuity floor: the parser found no `pub(crate)` declarations at all under {}. \
         A parser that matches nothing agrees with any tree, which is the one way this \
         ratchet could pass while telling you nothing (LESSON-585).",
        runtime_dir().display()
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
