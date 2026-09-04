//! REQ-598 AC-1 / TASK-299 — the suppression ratchet, and AC-9's both-halves check.
//!
//! # Why this walks the source tree and not clippy's output (ADR-6)
//!
//! Two of the workspace's remaining suppressions live in
//! `teton-inference/src/engine.rs` behind `#[cfg(feature = "llama")]`, which
//! neither CI nor AC-3's command compiles. A ratchet driven by clippy output
//! would silently stop counting them and the number would drift down for the
//! wrong reason — reading as progress. LESSON-515 is precisely this failure: a
//! `SessionId` parameter added in REQ-564 shipped broken in 0.1.14 because the
//! gated call site was never type-checked.
//!
//! # Why the pattern is anchored
//!
//! A bare substring search for the attribute also matches **prose**. This REQ's
//! own `turn_context.rs` opens by explaining what the twenty-five attributes
//! were, and that sentence contains the literal. Counting it inflated an early
//! measurement from 16 to 17. The count therefore matches only a line whose
//! first non-space characters are the attribute itself, so a doc comment
//! discussing suppressions can never be mistaken for one.
//!
//! This matters more than it sounds: a ratchet that counts its own commentary
//! drifts *upward* as the rationale is documented, which is the exact opposite
//! of what it exists to enforce.

use std::path::{Path, PathBuf};

/// What this REQ actually reached, measured rather than predicted.
///
/// The requirement's baseline was 25. The architecture's expected arithmetic
/// bounded the remainder at 11–13 and said in as many words that the final
/// number is measured; this is that measurement.
///
/// Breakdown of the 13:
///
/// | file | count | why it stays |
/// |---|---|---|
/// | `harness/turn_loop.rs` | 4 | a third cluster, ADR-2 — out of scope, REQ-599 inherits it |
/// | `runtime.rs` | 2 | `run_prompt_turn` (constructor site, parameters off the wire) and `run_one_attempt` (10 args after dropping 5) |
/// | `teton-inference/src/engine.rs` | 2 | feature-gated; carries none of the five fields |
/// | `tools/skill.rs`, `harness/budget.rs`, `carry.rs`, `teton/src/main.rs`, `teton-core/src/category.rs` | 1 each | measured in Phase 1 as carrying none of the cluster (OQ-3) |
const REACHED: usize = 13;

/// The vocabulary of the count. Built at runtime so this file's own text can
/// never be mistaken for a suppression by a tool grepping it.
fn needle() -> String {
    format!("#[{}(clippy::too_many_arguments)]", "allow")
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/<crate> has a workspace root two levels up")
        .to_path_buf()
}

fn rust_sources(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
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
    walk(&root.join("crates"), &mut out);
    out.sort();
    out
}

/// Every `(file, count)` where the count is of **attribute lines**, never prose.
fn counts() -> Vec<(String, usize)> {
    let root = workspace_root();
    let needle = needle();
    let mut out = Vec::new();
    for path in rust_sources(&root) {
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        let n = src
            .lines()
            .filter(|line| line.trim_start().starts_with(&needle))
            .count();
        if n > 0 {
            let rel = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            out.push((rel, n));
        }
    }
    out
}

/// **AC-1 — the ratchet, bounded on both sides.**
///
/// The upper bound is the obvious half: the count must not climb back. The
/// lower bound is the half that is easy to leave out and matters just as much.
/// A count that *drops* without this constant being updated is not evidence the
/// code improved — the likelier explanation is that a suppression was deleted
/// while its lint was disabled, or that the selector stopped matching (the
/// anchoring bug this file's module docs describe would have done exactly
/// that). Either way the number stops meaning what it says, so a drop is a
/// failure that asks for a deliberate update.
#[test]
fn the_suppression_count_is_exactly_what_this_req_reached() {
    let counts = counts();
    let total: usize = counts.iter().map(|(_, n)| n).sum();
    let breakdown = counts
        .iter()
        .map(|(f, n)| format!("{n:>3}  {f}"))
        .collect::<Vec<_>>()
        .join("\n  ");

    assert!(
        total <= REACHED,
        "suppressions climbed to {total}, above the {REACHED} REQ-598 reached. A new \
         `too_many_arguments` suppression is a new unnamed parameter cluster; name it \
         instead.\n  {breakdown}"
    );
    assert!(
        total >= REACHED,
        "suppressions dropped to {total}, below the {REACHED} REQ-598 reached. That is good \
         news only if it was deliberate — update REACHED and say what collapsed. If it was \
         not deliberate, the selector has stopped matching, which is the failure this \
         bound exists to catch.\n  {breakdown}"
    );
}

/// The ratchet counts attributes, never the prose that discusses them.
///
/// A direct guard on the anchoring bug that inflated an early measurement of
/// this very REQ from 16 to 17: `turn_context.rs`'s module documentation opens
/// by naming `#[allow(clippy::too_many_arguments)]` in a sentence, and an
/// unanchored search counts it.
#[test]
fn prose_that_names_the_attribute_is_not_counted_as_one() {
    let needle = needle();
    let turn_context =
        std::fs::read_to_string(workspace_root().join("crates/tetond/src/turn_context.rs"))
            .expect("turn_context.rs is readable");

    let anywhere = turn_context.matches(&needle).count();
    let as_attributes = turn_context
        .lines()
        .filter(|l| l.trim_start().starts_with(&needle))
        .count();

    assert!(
        anywhere > 0,
        "non-vacuity: this test is pointless unless turn_context.rs really does mention the \
         attribute in prose — it opens by explaining what the 25 of them were"
    );
    assert_eq!(
        as_attributes, 0,
        "turn_context.rs introduces no suppression of its own (BR-9); every occurrence is \
         commentary, and an unanchored count would read {anywhere} of them as real"
    );
}

/// **AC-9 — the three typed outcomes still have both halves.**
///
/// A typed outcome needs `failure_class() -> None`, so the retry / fallback /
/// degrade machinery leaves it alone, **and** a dedicated arm on the turn path,
/// so the user is told something true. One half without the other is the
/// LESSON-557 shape: the outcome silently falls through to the generic remote
/// arm, whose sentence names a provider failure — wrong about the cause and
/// naming no remedy.
///
/// This is a region check over the source rather than a call, because
/// `failure_class`'s `None` group is a *grouped match arm* and an integration
/// test cannot reach the private turn-path arms at all. It asserts the three
/// variants sit inside the `return None` group, which is the half a refactor
/// could quietly drop by moving one variant up into the classified list.
///
/// ## Mutation, run and recorded (conventions.md)
///
/// Moving `ProviderError::SpendCeilingReached` out of the `return None` group
/// and giving it `FailureClass::Transport` turns this red:
///
/// ```text
/// SpendCeilingReached must return None from failure_class
/// ```
///
/// and — the half that matters to a user — makes a spend-ceiling stop degrade a
/// healthy provider and fail over, spending *more* money rather than less.
/// Reverted after observing.
#[test]
fn the_typed_outcomes_still_return_none_from_failure_class() {
    let src = std::fs::read_to_string(workspace_root().join("crates/teton-providers/src/lib.rs"))
        .expect("teton-providers/src/lib.rs is readable");

    let body = src
        .split_once("pub fn failure_class")
        .expect("failure_class exists")
        .1;
    // Isolate the grouped arm itself, not everything that precedes it. Slicing
    // at `=> return None` and keeping the front half swallows the *classified*
    // arms too, which would make every assertion below pass regardless — the
    // non-vacuity check at the foot of this test caught exactly that while this
    // was being written. Walk back from the `=>` over the contiguous run of
    // pattern lines instead.
    let front = body
        .split_once("=> return None")
        .expect("failure_class has a `return None` group — the half AC-9 guards")
        .0;
    let group: String = front
        .lines()
        .rev()
        .take_while(|l| {
            let t = l.trim_start();
            t.starts_with('|') || t.starts_with("ProviderError::")
        })
        .collect::<Vec<_>>()
        .join("\n");

    for variant in [
        "PrivacyBlocked",
        "ContextLengthExceeded",
        "SpendCeilingReached",
    ] {
        assert!(
            group.contains(variant),
            "{variant} must return None from failure_class: without it the retry/fallback/\
             degrade machinery acts on a provider that is working fine, and the user is \
             told a provider failed when it did not (LESSON-557, conventions.md's \
             both-halves rule)"
        );
    }

    // Non-vacuity: the group must not be so wide that it contains everything.
    assert!(
        !group.contains("FailureClass::Timeout"),
        "the split found more than the None group — a classified variant is inside it, so \
         the assertions above would pass no matter what"
    );
}

// ---------------------------------------------------------------------------
// REQ-618 BR-3 — only the context manager assigns an anchor
// ---------------------------------------------------------------------------

/// **BR-3 / TASK-001.** An anchor is harness-assigned, and "the harness" is one
/// file: `harness/context.rs`. Everywhere else states `Anchor::None` and lets
/// `ContextManager::restate_anchors` decide the rest.
///
/// # Why a region check and not a count
///
/// A count of `anchor:` initializers would stay identical if a site *moved*
/// from `None` to `UserAsk` (LESSON-568 — relocating a call keeps a count the
/// same). What has to hold is a property of every site outside the manager, so
/// the check reads every site and asserts the value.
///
/// # Why this is the enforcement and not the doc comment
///
/// `Anchor`'s own doc says block text is never an input to the assignment.
/// That is true of `restate_anchors` today and would stay true of it after a
/// future push path started writing `anchor: Anchor::UserAsk` from a field the
/// model can influence. This check is what makes the promise survive that
/// author.
///
/// # Inversion
///
/// Changing any one of the workspace's `anchor: Anchor::None` initializers to
/// `Anchor::UserAsk` fails this test naming that file and line; deleting the
/// `Anchor` field entirely fails the vacuity floor below instead of passing
/// silently.
#[test]
fn only_the_context_manager_assigns_an_anchor() {
    let root = workspace_root();
    let owner = root.join("crates/tetond/src/harness/context.rs");
    let mut sites = 0usize;
    let mut offenders = Vec::new();
    for path in rust_sources(&root) {
        if path == owner {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        // Cut at the first column-0 `#[cfg(test)]`? No — deliberately not.
        // Test helpers build `ContextBlock`s too, and a test that hand-anchored
        // a block would be asserting against a state production cannot reach.
        for (n, line) in src.lines().enumerate() {
            let trimmed = line.trim_start();
            if !trimmed.starts_with("anchor:") {
                continue;
            }
            sites += 1;
            if !trimmed.contains("Anchor::None") {
                offenders.push(format!(
                    "{}:{}: {}",
                    path.strip_prefix(&root).unwrap_or(&path).display(),
                    n + 1,
                    trimmed
                ));
            }
        }
    }
    // The vacuity floor. Without it, renaming the field turns this test into a
    // loop over nothing that exits green — the shape LESSON-598 is about.
    assert!(
        sites >= 8,
        "expected the workspace to hold anchor initializers outside the manager; found {sites}. \
         Either the field was renamed or this check has stopped covering its subject."
    );
    assert!(
        offenders.is_empty(),
        "an anchor is assigned by ContextManager::restate_anchors and nowhere else (REQ-618 BR-3); \
         these sites name something other than Anchor::None:\n{}",
        offenders.join("\n")
    );
}
