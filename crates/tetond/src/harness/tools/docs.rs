//! The `teton_docs` tool — Teton's own product knowledge, bundled (REQ-577
//! BR-6..BR-10, ADR-3).
//!
//! BUG-160 is the shape this closes: asked how to hook up an external model, the
//! agent searched the *user's* repository. Not because it chose badly, but
//! because the only knowledge source it could reach was the repository — Teton's
//! own setup facts lived in a README the daemon does not ship and in weights that
//! do not reliably hold an endpoint URL. A prompt ending is only reachable if its
//! knowledge source exists (LESSON-493), so this module is that source.
//!
//! # Why a tool and not more prompt
//!
//! The bundled guide ([`SELF_CONFIG_GUIDE`](crate::harness::turn_loop)) is the
//! right vehicle for the always-needed surface, and the room it has is decided
//! rather than found: the system prompt sits under a pinned byte ceiling
//! ([`REDACT_BODY_OVERHEAD_BYTES`](crate::egress::redact), 23 KiB) with a
//! measured margin above it, pinned in turn so it cannot drift (BUG-193).
//! BUG-168 had to shorten one phrase to pay for another; BUG-181 had to move
//! the ceiling, with its arithmetic re-checked, to land one capability
//! sentence; and REQ-612 moved it again — 14 → 23 KiB — because the
//! repository's own notes are now resident, 8 KiB of them, and that raise took
//! the redact chunk count with it. Every one of those was a reviewed diff, and
//! that is the posture: headroom here is spent on purpose, never discovered.
//! Depth still cannot live in the guide. It lives here instead, and the only
//! resident cost is
//! [`DESCRIPTION`] — one line naming the topic index — so adding a sixth topic
//! later costs the prompt one word rather than a page (BR-10).
//!
//! # What a call touches
//!
//! Nothing. The bodies are [`include_str!`]d at compile time and served from
//! process memory: no filesystem read (so the jail has nothing to say), no
//! transport (so a session that reads every topic emits zero egress events), and
//! provenance identical to a tool that touched no paths — `Sources(∅)`, not
//! `Unknown`, because "touched no repo file" is a fact this tool knows exactly
//! and fail-closing on it would close the web channel over the daemon's own
//! documentation (BR-6, LESSON-432).
//!
//! Versioned with the binary is the freshness posture, deliberately: a topic can
//! be a release behind, and it can never be *unreachable* — which is what an
//! offline machine, a first run, or a degraded provider would otherwise make
//! every other retrieval design.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{str_arg, Tool, ToolContext, ToolOutcome};

/// The name the model calls this tool by.
///
/// Namespaced with `teton_` because the subject is Teton itself: `docs` alone
/// reads, to a model holding a repository, as "the docs in this project", which
/// is precisely the confusion BUG-160 is made of.
pub const DOCS_TOOL_NAME: &str = "teton_docs";

/// The per-topic byte ceiling (BR-9), pinned by
/// `every_bundled_topic_is_under_the_ceiling` and its fencepost twin in this
/// module's test block.
///
/// A topic is a *tool result*, not resident prompt: nothing here is in the
/// system prompt except [`DESCRIPTION`], so the ceiling has never been about
/// the prompt budget. It is about the conversation the body lands in.
///
/// # Raised 4,096 → 50,000 (REQ-612 decision, 2026-09-03)
///
/// The old figure was chosen so a full docs read could never be condensed: it
/// sat under `HarnessConfig`'s digest threshold, and
/// `the_topic_ceiling_stays_under_the_summarize_threshold` pinned that. The
/// cost was paid in the pages themselves — REQ-577, BUG-181, REQ-585 and
/// TASK-378 each had to cut a true sentence out of one topic to land another,
/// and TASK-378 cut four facts out of `context.md` to make room for the
/// repository-notes section. The product owner's decision reverses that trade:
/// **a topic may say everything it knows**, and the condensing machinery the
/// daemon already has is what bounds the cost.
///
/// **The mechanism, read rather than assumed.** [`DocsTool::run`] answers with
/// `ToolOutcome::ok(body)`, whose `disposition` is the default
/// [`ResultDisposition::Data`](super::ResultDisposition::Data). The turn loop
/// bypasses the digest for `ResultDisposition::Expansion` **only** — a skill
/// body, which must be carried whole or refused (REQ-587 BR-7) — so a
/// `teton_docs` result goes through
/// [`summarize_if_large`](crate::harness::context::summarize_if_large) exactly
/// as a large `read` does: under the route's `digest` threshold it is delivered
/// verbatim; over it, the `digest` duty condenses it, and if that duty cannot
/// be served the result is mechanically truncated under a marker. **The docs
/// tool is not exempt**, and `the_topic_ceiling_is_bounded_by_the_digest_duty`
/// pins both halves of that sentence.
///
/// So the honest statement of what this ceiling now buys is narrower than the
/// one it used to make, and it is the one that matters: a topic can never
/// evict the conversation it was fetched to serve, because the digest duty
/// bounds it at the route's own threshold — not because the ceiling sits under
/// that threshold. What the ceiling still does is bound the *worst case* the
/// duty has to handle, and keep a bundled page from silently becoming a book.
///
/// The price, stated: a topic past the route's digest threshold (23,250 bytes
/// on the local pair) costs one model call and reaches the model as a summary
/// of itself rather than verbatim. That is the same bargain every large `read`
/// makes, and it is why the pages are still written to be read, not skimmed.
pub const MAX_TOPIC_BYTES: usize = 50_000;

/// Every bundled topic, as `(name, body)`, in the order a reader should meet
/// them: how to connect a provider, where work is then routed, what each turn
/// is assembled under, the separate web opt-in, the user's own `/` commands, and
/// how to read the diagnostic when one of those is wrong.
///
/// A `&[(&str, &str)]` rather than a constant apiece because every rule below —
/// the ceiling sweep, the topic index, the unknown-topic error — is a statement
/// about the *set*, and a set spelled out once is one a further topic can be
/// added to without touching a rule. `context` (REQ-586 ADR-11) and `skills`
/// (REQ-585 BR-13) are the design paying off: a page of budget vocabulary cost
/// the resident prompt nine bytes, and a page on user-defined commands —
/// including the half that says what Teton does *not* run — cost it eight.
const TOPICS: &[(&str, &str)] = &[
    ("providers", include_str!("../docs/providers.md")),
    ("policy", include_str!("../docs/policy.md")),
    ("context", include_str!("../docs/context.md")),
    ("web", include_str!("../docs/web.md")),
    ("skills", include_str!("../docs/skills.md")),
    // REQ-617 BR-2. The two the 2026-09-04 transcript proved were missing: asked
    // "is transcript on?", the model had no topic to fetch and no command name in
    // its prompt, so it searched a repository for seven tool calls and reported
    // another tool's setting as Teton's. `commands` is the roster with the shell
    // twins; `transcript` is the two switches, the directory, and the fact that
    // the model's own tools are refused there.
    ("commands", include_str!("../docs/commands.md")),
    ("transcript", include_str!("../docs/transcript.md")),
    ("doctor", include_str!("../docs/doctor.md")),
    ("cost", include_str!("../docs/cost.md")),
];

/// The topic index as the model reads it, in [`DESCRIPTION`] and in the
/// unknown-topic error.
///
/// Written out rather than joined from [`TOPICS`] because both places it appears
/// are `const` — and because a hand-written second spelling is what
/// `the_description_indexes_every_bundled_topic` can compare against, the same
/// golden posture the recipe catalog takes.
const TOPIC_INDEX: &str =
    "providers, policy, context, web, skills, commands, transcript, doctor, cost";

/// The longest echo of a caller-supplied topic any message here will carry.
///
/// The argument comes from a model, and a model that emits a runaway string —
/// a pasted file, a repeated token — would otherwise have it copied verbatim
/// into a tool result and an event title, i.e. straight back into the context
/// window the next turn has to fit. Sixty-four characters is far past any real
/// topic name (the longest is nine) and far short of anything that costs
/// context.
///
/// **The bound is characters, not bytes**, and that is deliberate rather than
/// overlooked. Truncating by byte would split a multi-byte codepoint and panic
/// on an argument a model chose, which turns a malformed tool call into a
/// crashed turn. The cost of counting the other unit is that the worst case is
/// 64 four-byte codepoints plus the one-character ellipsis: **259 bytes**, not
/// 64. That is the number to reason about for context, and it is still three
/// orders of magnitude below anything that matters against the window.
pub(crate) const MAX_ECHOED_TOPIC_CHARS: usize = 64;

/// `topic`, bounded to [`MAX_ECHOED_TOPIC_CHARS`] and marked when it was cut.
///
/// Shared by the unknown-topic error and the `tool_call` title
/// (`describe_call`) because they echo the same untrusted value for the same
/// reason, and a bound applied in one place only is a bound on nothing.
#[must_use]
pub(crate) fn bounded_topic_echo(topic: &str) -> String {
    if topic.chars().count() <= MAX_ECHOED_TOPIC_CHARS {
        return topic.to_owned();
    }
    let mut out: String = topic.chars().take(MAX_ECHOED_TOPIC_CHARS).collect();
    out.push('…');
    out
}

/// The model-facing description, budgeted against `MAX_DESCRIPTION_CHARS` in
/// this module's test block — it is resident prompt on every turn of every
/// session, so its length is a decision somebody makes rather than a side
/// effect (LESSON-493).
///
/// It ends with the topic index because the index *is* the affordance: a model
/// deciding whether this tool answers the question in front of it is matching
/// the subject it was asked about against these seven words. BUG-168's lesson
/// applies — a capability the prompt does not name outright is one the local
/// tier does not reach for. The fifth word, `context`, cost nine characters
/// here for a 3.4 KB topic (REQ-586 ADR-11); the sixth, `skills`, cost eight for
/// a 4.0 KB one (REQ-585) — the trade this module exists to make.
///
/// The sentence in front of the index paid for both. `setup and troubleshooting`
/// named nothing `docs` does not already imply, and it was the only clause
/// here that was not a topic name; dropping it took the description from the
/// ceiling to 26 characters under it. **Spend that on names**: the index is
/// the affordance, the sentence is the frame around it, and `skills` came out
/// of that margin rather than out of `MAX_DESCRIPTION_CHARS`, which has not
/// moved. The seventh, `cost`, took six of what was left for a 2.6 KB topic
/// (REQ-588 BR-5) — a page a user consults about their own money, which is not
/// a page to make them find by guessing. **Spent: 108. Left: 12.**
///
/// The eighth and ninth, `commands` and `transcript` (REQ-617 BR-2), cost 22
/// between them against the 12 that were left, so **the frame paid a third
/// time**: `Read Teton's own docs, bundled in this binary. ` →
/// `Teton's own bundled docs. `, recovering 20. `bundled` is the load-bearing
/// word and it survives — it is the sentence that says these pages are not read
/// from the repository, which is precisely the mistake the two new topics exist
/// to stop (LESSON-493). `Read` went because the tool is named `teton_docs` and
/// its schema has one string field; `in this binary` went because it elaborates
/// `bundled` and elaboration is what a resident description cannot afford.
/// **Spent: 109. Left: 11.** `MAX_DESCRIPTION_CHARS` has still not moved.
///
/// That leaves roughly one more name at today's spelling and then this frame is
/// out of road. The next topic either shortens `topics: ` (7 characters that say
/// nothing the list does not), or makes the case for a bigger ceiling out loud —
/// which is a decision, and this ledger is where it gets written down.
const DESCRIPTION: &str = concat!(
    "Teton's own bundled docs. ",
    "topics: providers, policy, context, web, skills, commands, transcript, doctor, cost"
);

/// The body of `topic`, or `None` when nothing by that name is bundled.
///
/// The lookup is **normalized** — surrounding whitespace trimmed, ASCII case
/// folded — because the caller is a language model filling a string slot, and
/// `"Providers"`, `" providers"` and `"providers\n"` are the same request in
/// every sense but `==`. Refusing them would spend a whole turn teaching a
/// weak model a capitalization rule that carries no meaning (BR-8's posture,
/// one level below the didactic error). Topic names are ASCII lowercase single
/// tokens by the rule pinned in the tests, so ASCII folding is the whole of
/// the case question here; the raw spelling still reaches the error message,
/// so a caller who typed something genuinely wrong sees what they typed.
#[must_use]
fn body_of(topic: &str) -> Option<&'static str> {
    let wanted = topic.trim().to_ascii_lowercase();
    TOPICS
        .iter()
        .find(|(name, _)| *name == wanted)
        .map(|(_, body)| *body)
}

/// Teton's bundled documentation, one topic per call.
///
/// A unit struct: there is no state to hold, which is the same thing as saying
/// there is nothing a call can depend on but its argument.
pub struct DocsTool;

#[async_trait]
impl Tool for DocsTool {
    fn name(&self) -> &str {
        DOCS_TOOL_NAME
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        // One required string, and deliberately no second argument. A `section`
        // or `query` parameter would be a filter this tool cannot honestly
        // implement — the bodies are prose, not records — and a weak model
        // filling two keys correctly is a harder ask than it filling one.
        //
        // The argument's own description is built from `TOPIC_INDEX` rather
        // than spelled a fourth time. This method returns an **owned** `Value`,
        // so nothing here has to be `const` — and a hand-written list that only
        // the schema carried would be the one copy no golden compares, which is
        // exactly how a new topic ends up reachable from the description and
        // invisible in the schema the model actually fills.
        json!({
            "type": "object",
            "properties": {
                "topic": {
                    "type": "string",
                    "description": format!("One of: {TOPIC_INDEX}")
                }
            },
            "required": ["topic"]
        })
    }

    fn run(&self, _ctx: &ToolContext, args: &Value) -> ToolOutcome {
        let topic = match str_arg(args, "topic") {
            Ok(t) => t,
            Err(e) => return e.into(),
        };
        match body_of(&topic) {
            // `ToolOutcome::ok` carries `ToolProvenance::none()` — `Sources(∅)`,
            // the reading a tool that touched no repo file gets. Nothing here
            // opens a path, so there is no identity to mint and none to hide.
            Some(body) => ToolOutcome::ok(body),
            // The `dispatch` posture for an unknown tool name (mod.rs), applied
            // one level down: a weak model that guessed a topic is told what the
            // topics are, in the same reply, and can spend its next turn on the
            // right one. Never a crash, never an empty result (BR-8).
            //
            // What is echoed back is the caller's **raw** spelling — normalized
            // matching is a convenience for reaching a topic, not a licence to
            // tell somebody they asked for something other than what they typed
            // — bounded by `bounded_topic_echo` so a runaway argument cannot
            // spend the window on its own repetition.
            None => ToolOutcome::error(format!(
                "unknown topic `{}`; valid topics: {TOPIC_INDEX}",
                bounded_topic_echo(&topic)
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeSet;

    use crate::harness::context::ToolProvenance;
    use crate::provider_recipes::recipe_catalog;

    /// The ceiling on [`DESCRIPTION`], which is resident prompt on every turn of
    /// every session (LESSON-493).
    ///
    /// Not a style rule: both prompt-margin tests measure the real assembled
    /// prompt against a 48-byte floor, so a description written without a number
    /// in front of it spends headroom the bundled guide is competing for.
    ///
    /// It lives in this module and not beside [`DESCRIPTION`] because only the
    /// tests read it. It once *had* to: `boundary_coverage.rs` used to truncate
    /// its source scan at each file's first `#[cfg(test)]` line, so a
    /// `cfg(test)` item above the `impl Tool` block hid the tool from the
    /// derived universe. The scan now anchors on the test module itself and
    /// pins that with a regression test, so the placement is convention, not a
    /// constraint.
    ///
    /// **Spent and left, as of REQ-585.** The `context` topic's nine characters
    /// (`, context`) first took [`DESCRIPTION`] to exactly 120 — legal, and no
    /// headroom at all with a `projects` tool (REQ-584) and a `skill` tool
    /// (REQ-587) queued behind this one. So the room was bought where the
    /// review said to buy it: the sentence in front of the index lost `setup
    /// and troubleshooting`, 26 characters that named nothing the index does
    /// not already carry, leaving 94 spent and 26 free.
    ///
    /// REQ-585's `skills` spent eight of those 26 out of the margin, exactly as
    /// that note said the next topic should, and this number did not move.
    /// **Spent: 102. Left: 18.** The margin, not the ceiling, is what a seventh
    /// topic buys its name from: the two margin tests measure the prompt this
    /// description sits in, and their headroom is what the bundled guide is
    /// competing for. When the margin is gone, shorten the frame again or make
    /// the case for a bigger ceiling out loud.
    pub(super) const MAX_DESCRIPTION_CHARS: usize = 120;

    fn ctx() -> ToolContext {
        // Every call ignores it: the jail has nothing to say about a body that
        // never leaves this binary. Pointed at the temp dir rather than a real
        // repo so a test that started depending on the filesystem would be
        // visible as one.
        ToolContext::new(std::env::temp_dir())
    }

    fn call(topic: &str) -> ToolOutcome {
        DocsTool.run(&ctx(), &json!({ "topic": topic }))
    }

    /// **AC-3, the positive half.** Every bundled topic is served, whole, with
    /// the provenance of a tool that touched nothing.
    ///
    /// Asserted through `run` rather than against [`TOPICS`] directly, because
    /// the claim is about what a model *receives*: a lookup that quietly
    /// truncated, or an outcome flagged `is_error` while carrying a body, would
    /// satisfy an equality against the constant and fail the user.
    /// **AC-5 (REQ-588 BR-5).** The `cost` topic states what a fresh install
    /// actually does, rather than being trusted to have said it.
    ///
    /// Three claims, each pinned separately because each is a different way the
    /// page could mislead someone about their own money:
    ///
    /// - **No ceiling until one is configured.** The mirror of the silently
    ///   recorded window: a limit nobody was told about is a limit nobody can
    ///   plan around, and the absence of one has to be stated rather than
    ///   inferred from the page not mentioning it.
    /// - **The key is named.** A page that says a ceiling can be set without
    ///   saying where leaves the reader to guess at a config file.
    /// - **The one-call overshoot.** The load-bearing one. A user reading
    ///   `$5.00` will believe they are promised $5.00, and they are not —
    ///   output tokens cannot be priced before the model has written them, so
    ///   the check is between calls and a prompt can finish over by the cost of
    ///   the call already in flight. Leaving that out would make the page
    ///   technically true and practically a lie.
    ///
    /// Asserted on the served body, not the file, so a topic that stopped being
    /// bundled or served would fail here too.
    #[test]
    fn the_cost_topic_states_what_a_fresh_install_does() {
        let outcome = call("cost");
        assert!(!outcome.is_error, "the cost topic must serve");
        let body = outcome.content.to_lowercase();

        assert!(
            body.contains("no cap") || body.contains("no spend ceiling until"),
            "the page must say a fresh install has no ceiling"
        );
        assert!(
            body.contains("prompt_ceiling_usd"),
            "the page must name the key that sets one"
        );
        assert!(
            body.contains("[cost]"),
            "the page must name the table the key lives in"
        );
        // ADR-2's overshoot, in substance rather than by a single word: the
        // check is between calls, and the excess is bounded by one of them.
        assert!(
            body.contains("between calls"),
            "the page must say when the ceiling is checked"
        );
        assert!(
            body.contains("one call"),
            "the page must bound the overshoot at one call"
        );
        // And it must not promise what it cannot deliver.
        assert!(
            body.contains("not a promise never to exceed it"),
            "the page must say plainly that the ceiling can be exceeded"
        );
    }

    #[test]
    fn every_topic_serves_its_whole_bundled_body() {
        assert_eq!(
            TOPICS.len(),
            9,
            "the topic roster changed: {:?}",
            TOPICS.iter().map(|(name, _)| *name).collect::<Vec<_>>()
        );
        for (name, body) in TOPICS {
            let outcome = call(name);
            assert!(!outcome.is_error, "`{name}` failed: {}", outcome.content);
            assert_eq!(&outcome.content, body, "`{name}` was not served whole");
            assert_eq!(
                outcome.provenance,
                ToolProvenance::none(),
                "`{name}` claimed to have touched a repo file; this tool opens no path \
                 and `Sources(∅)` is what says so (BR-6)"
            );
            assert!(
                outcome.dead_end.is_none(),
                "`{name}` reported a capability dead end; reading bundled bytes runs out \
                 of no capability"
            );
        }
    }

    /// **BR-2 at the tool's own altitude: what the model is handed carries every
    /// vendor fact the catalog ships.**
    ///
    /// `the_providers_topic_and_the_recipe_catalog_agree` (web_setup_contracts)
    /// gates the *file* in both directions. This gates the *served result* — the
    /// one thing a session actually sees — so a `run` that ever answered from
    /// somewhere other than the bundled body could not hide behind that file
    /// being correct.
    #[test]
    fn the_served_providers_topic_carries_every_catalog_recipe() {
        let served = call("providers");
        assert!(!served.is_error, "{}", served.content);
        let catalog = recipe_catalog();
        assert!(
            !catalog.is_empty(),
            "the catalog is empty; nothing is checked"
        );
        for recipe in catalog {
            if let Some(endpoint) = &recipe.endpoint {
                assert!(
                    served.content.contains(endpoint.as_str()),
                    "the served `providers` topic never names `{endpoint}`, which is the \
                     endpoint `{}` needs. Update crates/tetond/src/harness/docs/providers.md.",
                    recipe.id_suggestion
                );
            }
            assert!(
                served.content.contains(recipe.example_model.as_str()),
                "the served `providers` topic never names `{}`, the example model for \
                 `{}`. Update crates/tetond/src/harness/docs/providers.md.",
                recipe.example_model,
                recipe.id_suggestion
            );
        }
    }

    /// The `skills` topic as a model receives it, with every whitespace run
    /// collapsed to one space and markdown emphasis markers dropped.
    ///
    /// The needle tests below match against this rather than the raw body, for
    /// two reasons that are both about what the needles are *for*. A claim
    /// written across a line break is one substring here, so a phrase can be
    /// pinned as it reads rather than as it happens to wrap; and re-wrapping a
    /// paragraph at 80 columns — or bolding a word — is a formatting change,
    /// which must not fail a test about what the topic *asserts*, nor let a
    /// stale assertion back in wearing `**` (`**stalls**` and `stalls` are the
    /// same claim).
    ///
    /// Served through `run`, like the sweep above, because the claim under test
    /// is about what reaches the model rather than what is on disk.
    fn skills_topic() -> String {
        let served = call("skills");
        assert!(!served.is_error, "{}", served.content);
        served
            .content
            .replace('*', "")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// A phrase REQ-587 made false, which must therefore not be in the topic.
    fn assert_no_stale_claim(topic: &str, phrase: &str, why: &str) {
        assert!(
            !topic.contains(phrase),
            "the `skills` topic still says `{phrase}`, which this build makes false: \
             {why}. The topic is compiled into the same binary that hands the model a \
             `skill` tool, so a model reading it is told the opposite of what it can do \
             (AC-16) — fix crates/tetond/src/harness/docs/skills.md, not this assertion, \
             and pay for the words by cutting elsewhere in the topic."
        );
    }

    /// A phrase that carries a claim the topic has to make.
    fn assert_states(topic: &str, phrase: &str, claim: &str) {
        assert!(
            topic.contains(phrase),
            "the `skills` topic no longer says `{phrase}`, so it no longer states that \
             {claim}. The byte ceiling above cannot see this and neither can the length \
             floor (LESSON-481): if the wording changed on purpose, move the needle with \
             it — deleting the needle deletes the only thing that notices when the topic \
             and the product disagree."
        );
    }

    /// **AC-16, first passage: the topic no longer denies model invocation.**
    ///
    /// It said *"The model cannot invoke a skill: name it and let the user type
    /// it"*, compiled into the same binary that registers a `skill` tool. A
    /// model that reads its own documentation and believes it hands the turn
    /// back to the user instead of calling the tool sitting in its own schema —
    /// which is BUG-181's defect with the sign flipped, on the surface REQ-577
    /// shipped so the model would stop guessing.
    #[test]
    fn the_skills_topic_does_not_deny_model_invocation() {
        let topic = skills_topic();
        assert_no_stale_claim(
            &topic,
            "cannot invoke a skill",
            "BR-1 gives the model a second door into the same expander",
        );
        assert_no_stale_claim(
            &topic,
            "let the user type it",
            "a hand-off to the user is exactly the stall this REQ removes",
        );
        assert_states(
            &topic,
            "the model's `skill { name, args }` is a tool result",
            "the model invokes a skill by calling the tool, and the expansion comes back \
             inside the turn it is already in (BR-1, OQ-2)",
        );
    }

    /// **AC-16, second passage: two frontmatter flags are no longer inert.**
    ///
    /// BR-3 shrinks REQ-585 BR-5's inert list by exactly two keys. The topic has
    /// to name both and what they do, and — the half a positive needle would
    /// miss — must not go on listing them among the keys it calls inert.
    #[test]
    fn the_skills_topic_does_not_call_the_two_invocation_flags_inert() {
        let topic = skills_topic();
        assert_no_stale_claim(
            &topic,
            "Frontmatter reads only",
            "BR-3 makes two more keys meaningful, so the set of keys that are read is no \
             longer closed at three",
        );
        assert_states(
            &topic,
            "`disable-model-invocation: true` hides the skill from the model",
            "the first flag hides a skill from the model completely (BR-3)",
        );
        assert_states(
            &topic,
            "`user-invocable: false` makes it model-only",
            "the second flag makes a skill model-only (BR-3)",
        );
        assert_states(
            &topic,
            "A non-boolean value is safe per key",
            "a value that is not `true`/`false` takes the safe reading, so a typo can \
             never widen what the model may run (BR-3). Which reading is safe depends on \
             the flag, and the eighth passage below pins that half",
        );

        // The inert list itself, read out of the sentence that makes the claim:
        // a topic that named both flags above and then left them in this
        // parenthesis would satisfy every needle and still tell the model they
        // do nothing.
        let inert = topic
            .split_once("Every other key (")
            .and_then(|(_, rest)| rest.split_once(") is inert"))
            .map(|(list, _)| list)
            .expect(
                "the topic no longer names what stays inert; BR-3 shrinks REQ-585's inert \
                 list by exactly two keys and leaves the rest inert, which is a claim the \
                 topic still has to carry",
            );
        for flag in ["disable-model-invocation", "user-invocable"] {
            assert!(
                !inert.contains(flag),
                "the `skills` topic lists `{flag}` among the inert keys — `{inert}` — and \
                 BR-3 is precisely that this key is now read. A model told the flag is \
                 inert will not believe a skill can be hidden from it."
            );
        }
    }

    /// **AC-16, third passage: a skill that invokes skills no longer stalls.**
    ///
    /// *"stalls at its first 'invoke the skill' step"* was REQ-585's honest
    /// limit and is this REQ's entire subject: `/proceed` reaching its first
    /// gate is the reason the tool exists. What still degrades is a skill that
    /// dispatches subagents, and the topic keeps saying so.
    #[test]
    fn the_skills_topic_does_not_say_a_skill_invoking_skill_stalls() {
        let topic = skills_topic();
        assert_no_stale_claim(
            &topic,
            " stalls",
            "the `skill` tool is what unstalls a skill written as skill invocations",
        );
        assert_no_stale_claim(
            &topic,
            "at its first \"invoke the skill\" step",
            "the first such step is now a tool call that resolves",
        );
        assert_states(
            &topic,
            "invokes other skills now runs them",
            "a skill whose phases are skill invocations reaches them (BR-1)",
        );
        assert_states(
            &topic,
            "dispatches subagents degrades",
            "what genuinely still degrades is named, so the model does not pretend a \
             subagent step ran",
        );
    }

    /// **AC-16, fourth passage: BR-10's provenance is two rules, not one.**
    ///
    /// REQ-585 ADR-9 refused to widen the id minter, so a project skill and a
    /// user skill pin a turn by different rules — and the second is stricter
    /// than a `read` of the same bytes. The consequence for a model invocation
    /// on a boundary-configured machine is the part BR-10 says to state plainly
    /// rather than leave to be discovered in a runbook.
    #[test]
    fn the_skills_topic_states_both_provenance_rules() {
        let topic = skills_topic();
        assert_no_stale_claim(
            &topic,
            "the stricter unknown rule",
            "the pre-BR-10 paragraph folded both rules into one clause about a typed \
             `/name`, and said nothing about a model invocation",
        );
        assert_states(
            &topic,
            "Two rules.",
            "there are two provenance rules and the topic counts them (BR-10)",
        );
        assert_states(
            &topic,
            "A project skill mints a root-relative source",
            "rule one: a project skill has a root-relative identity and pins the turn as \
             reading that file would (BR-10)",
        );
        assert_states(
            &topic,
            "block is `Unknown` and pins the turn wherever any boundary is configured",
            "rule two: a user skill has no root-relative identity, so it is `Unknown` and \
             pins wherever any boundary is set — related to it or not (BR-10)",
        );
        assert_states(
            &topic,
            "a model invocation of a `~/.claude` skill runs local",
            "the consequence of rule two for the seventeen `~/.claude` skills, which is \
             what a runbook would otherwise discover the hard way (BR-10)",
        );
    }

    /// **AC-16, fifth passage: the model's toolbox is named, not counted.**
    ///
    /// *"one model with five tools"* was REQ-585's shorthand for the five
    /// built-ins, and this build adds a sixth door the model can open. A bare
    /// count is what goes stale the next time a tool is registered — and it
    /// was already silent about `teton_docs` and an opted-in `web` — so the
    /// topic names the tool that matters here instead.
    #[test]
    fn the_skills_topic_does_not_pin_a_tool_count_that_moved() {
        let topic = skills_topic();
        assert_no_stale_claim(
            &topic,
            "five tools",
            "the model's toolbox gained `skill`, and the count was never the whole set \
             anyway (`teton_docs`, and `web` when opted in)",
        );
        assert_states(
            &topic,
            "tools, `skill` among them",
            "the tool a skill body may reach for is named rather than counted",
        );
    }

    /// **AC-16, sixth passage: an invocation has two callers.**
    ///
    /// *"`/name <rest>` is exactly one user-role prompt turn"* is now true of
    /// one caller of two. The other lands as a tool result inside the turn
    /// already running, over identical body bytes (AC-2) behind a different
    /// frame (BR-4, ADR-6) — and a topic silent about the second caller is a
    /// topic the model cannot act on.
    #[test]
    fn the_skills_topic_names_both_callers_of_the_expander() {
        let topic = skills_topic();
        assert_no_stale_claim(
            &topic,
            "is exactly one user-role prompt turn",
            "an invocation is no longer only a prompt turn — the model's call lands as a \
             tool result inside the turn already running",
        );
        assert_states(
            &topic,
            "Two callers, one expander.",
            "one registry and one expander are reached by `/name` and by the tool (BR-1)",
        );
        assert_states(
            &topic,
            "then the same bytes",
            "the body bytes are identical on both paths; only the frame in front differs \
             (AC-2, BR-4)",
        );
    }

    /// **AC-16, seventh passage: the `skill` tool is not always there.**
    ///
    /// TASK-220 could not state this for want of room — the topic was 16 bytes
    /// under the ceiling — and left it reading as though `skill` is present in
    /// every session. BR-2 registers it only when the registry holds at least
    /// one model-invocable skill, so on a machine with none the model is
    /// otherwise told about a tool that is not in its own schema, which is
    /// BUG-181's defect in the other direction: the topic and the tool set
    /// disagreeing, with the topic winning.
    ///
    /// The clause was paid for by cutting elsewhere in the same file. The
    /// ceiling did not move and `every_bundled_topic_is_under_the_ceiling` still
    /// guards it.
    #[test]
    fn the_skills_topic_says_the_tool_is_registered_conditionally() {
        let topic = skills_topic();
        assert_states(
            &topic,
            "The `skill` tool exists only when some skill is model-invocable",
            "the tool is conditionally registered, so a session with no \
             model-invocable skill has no `skill` tool at all (BR-2)",
        );
        assert_states(
            &topic,
            "with none it is absent",
            "the consequence is stated as absence rather than as an empty list — a \
             model that reads about a tool missing from its own schema guesses (BR-2)",
        );
    }

    /// **AC-16, eighth passage: the safe reading is per flag, not "user only".**
    ///
    /// The flag paragraph used to end *"A non-boolean value reads as the safe
    /// one — user only"*, which is true of exactly one of the two flags.
    /// `disable-model-invocation: bogus` does read user-only: `boolean` returns
    /// `None`, the key is named in `ignored_keys`, and `model_invocable` takes
    /// `false` while the user keeps `/name`. `user-invocable: bogus` does not:
    /// the safe reading in that direction is the *unchanged* one, so
    /// `user_invocable` stays `true` and `model_invocable` is never touched —
    /// that row is invocable by **both**.
    ///
    /// The asymmetry is deliberate and is spelled out on `skills::frontmatter`'s
    /// module docs: an unreadable value lands on the narrower capability for the
    /// model and the unchanged one for the user, so a typo can hide a skill from
    /// the model and can never hand it one the user meant to keep to themselves.
    /// A topic that flattens that into one answer tells the model it cannot
    /// invoke a row it can in fact invoke — BUG-181's defect in the shape AC-16
    /// exists to keep out, on the surface the model reads to find out what it
    /// is allowed to do.
    ///
    /// Paid for by cutting elsewhere in the same file; the ceiling did not move
    /// and `every_bundled_topic_is_under_the_ceiling` still guards it.
    #[test]
    fn the_skills_topic_states_the_safe_reading_is_per_flag() {
        let topic = skills_topic();
        assert_no_stale_claim(
            &topic,
            "reads as the safe one — user only",
            "the safe reading is per flag — user-only for `disable-model-invocation`, and \
             for `user-invocable` the unchanged reading, which leaves that row invocable \
             by both",
        );
        assert_states(
            &topic,
            "unreadable `disable-model-invocation` hides it",
            "an unreadable value on the negative flag takes the narrower reading for the \
             model, and only for the model (BR-3)",
        );
        assert_states(
            &topic,
            "unreadable `user-invocable` changes nothing, so it stays invocable by both",
            "an unreadable value on the positive flag moves neither capability, so that \
             row is invocable by the user *and* the model — the arm the old wording got \
             backwards (BR-3)",
        );
    }

    /// **AC-3, the didactic half (BR-8).** An unknown topic names every valid
    /// one, so the recovery is the model's next turn rather than the user's.
    #[test]
    fn an_unknown_topic_is_answered_with_the_valid_ones() {
        let outcome = call("pricing");
        assert!(outcome.is_error, "an unknown topic is a failed call");
        assert!(
            outcome.content.contains("unknown topic `pricing`"),
            "the error must repeat what was asked for: {}",
            outcome.content
        );
        for (name, _) in TOPICS {
            assert!(
                outcome.content.contains(name),
                "the error omits `{name}`, so a model reading it cannot reach that topic: \
                 {}",
                outcome.content
            );
        }
        assert_eq!(
            outcome.provenance,
            ToolProvenance::none(),
            "a refusal touched no file either"
        );
    }

    /// A missing argument is the same class of answer: the model is told what it
    /// left out, never handed an empty success.
    #[test]
    fn a_missing_topic_argument_is_a_failed_call_that_says_so() {
        let outcome = DocsTool.run(&ctx(), &json!({}));
        assert!(outcome.is_error);
        assert!(outcome.content.contains("topic"), "{}", outcome.content);
    }

    /// **A topic the model spelled with a capital or a stray space is the topic
    /// it asked for.**
    ///
    /// The argument is filled by a language model, and the four spellings below
    /// are one request in every sense but `==`. Refusing them costs a whole
    /// turn — the didactic error, then a retry — to teach a casing rule that
    /// carries no meaning. Swept over every bundled topic rather than spot-
    /// checked on one, so a new topic inherits the tolerance.
    #[test]
    fn a_topic_is_matched_past_case_and_surrounding_space() {
        for (name, body) in TOPICS {
            for spelling in [
                name.to_ascii_uppercase(),
                format!(" {name}"),
                format!("{name}\n"),
                format!("  {}  ", name.to_ascii_uppercase()),
            ] {
                let outcome = call(&spelling);
                assert!(
                    !outcome.is_error,
                    "`{spelling:?}` is `{name}` with different whitespace or case and was \
                     refused: {}",
                    outcome.content
                );
                assert_eq!(
                    &outcome.content, body,
                    "`{spelling:?}` served the wrong body"
                );
            }
        }
    }

    /// **An unknown topic is echoed back as typed, and bounded.**
    ///
    /// Two claims that pull in opposite directions and are both wanted. The
    /// echo is the caller's *raw* spelling, because an error that silently
    /// lowercased what it quotes tells somebody they asked for something they
    /// did not — and the normalization above makes that a live possibility
    /// rather than a hypothetical. The bound is `MAX_ECHOED_TOPIC_CHARS`,
    /// because the value is model-supplied and a runaway argument copied
    /// verbatim into a tool result is context spent on a model's own
    /// repetition.
    #[test]
    fn an_unknown_topic_is_echoed_as_typed_and_bounded() {
        let outcome = call("  Pricing  ");
        assert!(outcome.is_error);
        assert!(
            outcome.content.contains("unknown topic `  Pricing  `"),
            "the error must quote what was actually passed: {}",
            outcome.content
        );

        let runaway = "z".repeat(4_000);
        let outcome = call(&runaway);
        assert!(outcome.is_error);
        assert!(
            !outcome.content.contains(&runaway),
            "the whole {}-char argument was copied into the result; a topic echo is \
             bounded at {MAX_ECHOED_TOPIC_CHARS} chars so a model cannot spend the window \
             on its own repetition",
            runaway.len()
        );
        assert!(
            outcome
                .content
                .contains(&"z".repeat(MAX_ECHOED_TOPIC_CHARS)),
            "the echo was cut shorter than the {MAX_ECHOED_TOPIC_CHARS}-char bound, so the \
             caller cannot see enough of what they typed to fix it: {}",
            outcome.content
        );
        assert!(
            outcome.content.contains(TOPIC_INDEX),
            "a bounded echo must still carry the topic index — that is what makes the \
             error didactic (BR-8): {}",
            outcome.content
        );
    }

    /// The bound is applied by `char`, so a multi-byte spelling is never split
    /// mid-codepoint — the panic a naive byte slice would produce, on an
    /// argument a model chose.
    #[test]
    fn the_topic_echo_is_bounded_by_characters_not_bytes() {
        let multibyte = "é".repeat(MAX_ECHOED_TOPIC_CHARS * 2);
        let echoed = bounded_topic_echo(&multibyte);
        assert_eq!(
            echoed.chars().count(),
            MAX_ECHOED_TOPIC_CHARS + 1,
            "the bound counts characters, plus the one marking the cut: {echoed}"
        );
        assert_eq!(bounded_topic_echo("providers"), "providers");
    }

    /// Whether a body of this many bytes fits under [`MAX_TOPIC_BYTES`].
    ///
    /// One spelling of the comparison, called by the sweep over the real topics
    /// and by the fencepost test below. Written as a function precisely so the
    /// fencepost can reach *the* check rather than a copy of it: a sweep whose
    /// `<=` slipped to `<` would still pass on every bundled topic (the largest
    /// is well under the ceiling), so the boundary is only defended if the
    /// boundary is what gets exercised.
    fn fits_ceiling(len: usize) -> bool {
        len <= MAX_TOPIC_BYTES
    }

    /// **BR-9 / AC-8: every bundled topic is under the ceiling.**
    ///
    /// Swept over [`TOPICS`] rather than written per topic, so a new topic is
    /// covered the moment it is added rather than the moment someone remembers.
    /// The floor below is the other half of BUG-159's lesson: an empty or
    /// stub-length body would pass a ceiling check by having nothing in it.
    ///
    /// **The remedy in the message changed with REQ-612's raise.** While the
    /// ceiling was 4,096 the instruction was "trim it, never raise the
    /// ceiling", because the ceiling's whole job was to sit under the digest
    /// threshold. It no longer does (see [`MAX_TOPIC_BYTES`]), so the honest
    /// remedy for a page that has outgrown 50,000 bytes is to *split* it —
    /// which is also the only remedy that keeps a single read useful.
    #[test]
    fn every_bundled_topic_is_under_the_ceiling() {
        for (name, body) in TOPICS {
            assert!(
                body.len() > 500,
                "`{name}` is {} bytes, which is not a topic — the sweep below would pass \
                 vacuously",
                body.len()
            );
            assert!(
                fits_ceiling(body.len()),
                "the `{name}` topic is {} bytes against a ceiling of {MAX_TOPIC_BYTES}. \
                 Split it into a second topic and add that topic to `TOPICS` and to the \
                 index in `DESCRIPTION`. Do not raise the ceiling and do not delete this \
                 assertion: a page this long is one no read returns from usefully, and \
                 past the route's digest threshold it reaches the model as a summary of \
                 itself (REQ-577 BR-9, REQ-612's raise).",
                body.len()
            );
        }
    }

    /// **The ceiling's fencepost, exercised at the byte it is written on.**
    ///
    /// Every bundled topic today is a kilobyte or more short of
    /// [`MAX_TOPIC_BYTES`], so the sweep above would stay green if `<=` became
    /// `<` — the mutation survives on the real corpus and would then reject a
    /// topic that fits exactly, with a failure message telling its author to
    /// trim a body that was already legal. Synthetic bodies at exactly the
    /// ceiling and exactly one byte past it pin the comparison itself.
    #[test]
    fn the_ceiling_admits_its_own_byte_and_refuses_the_next_one() {
        let at_ceiling = "x".repeat(MAX_TOPIC_BYTES);
        let one_over = "x".repeat(MAX_TOPIC_BYTES + 1);
        assert_eq!(at_ceiling.len(), MAX_TOPIC_BYTES);
        assert_eq!(one_over.len(), MAX_TOPIC_BYTES + 1);
        assert!(
            fits_ceiling(at_ceiling.len()),
            "a topic of exactly {MAX_TOPIC_BYTES} bytes is inside the ceiling; the check is \
             `<=`, and a `<` here would reject a body whose author did the arithmetic right"
        );
        assert!(
            !fits_ceiling(one_over.len()),
            "a topic of {} bytes is over a ceiling of {MAX_TOPIC_BYTES} and the check let it \
             through",
            one_over.len()
        );
    }

    /// **What bounds a docs read is the `digest` duty, not the ceiling
    /// (REQ-612 decision, 2026-09-03).**
    ///
    /// This test used to assert the opposite relation — `MAX_TOPIC_BYTES <
    /// summarize_threshold_bytes`, so a full docs read could never be
    /// condensed. Raising the ceiling to 50,000 so a topic may say everything
    /// it knows reverses it, and the reversal is a decision rather than a
    /// regression **only if the condensing machinery really does apply here**.
    /// So that is what is asserted now, in the two halves the claim is made of:
    ///
    /// 1. **The ceiling is above the threshold**, deliberately — the fact that
    ///    used to be forbidden, pinned in its new direction so nobody
    ///    "restores" the old invariant by lowering the ceiling in silence.
    /// 2. **The docs tool is not exempt from the digest.** The turn loop
    ///    bypasses `summarize_if_large` for [`ResultDisposition::Expansion`]
    ///    alone, and [`DocsTool`]'s outcome is
    ///    [`ResultDisposition::Data`](super::ResultDisposition::Data) — so an
    ///    over-threshold topic is condensed exactly as a large `read` is. This
    ///    half is the load-bearing one: were the docs tool ever given
    ///    `Expansion` (to stop its pages being summarized, say), the ceiling
    ///    would become the *only* bound on what a docs read can push into a
    ///    conversation, and 50,000 bytes is not a bound anyone chose for that.
    ///
    /// **Mutation, run and observed:** giving the `providers` outcome
    /// `.with_disposition(ResultDisposition::Expansion)` fails the second
    /// assertion; lowering `MAX_TOPIC_BYTES` back to 4,096 fails the first.
    #[test]
    fn the_topic_ceiling_is_bounded_by_the_digest_duty() {
        use crate::harness::tools::ResultDisposition;
        use crate::harness::turn_loop::HarnessConfig;

        // The byte twin is read off the config rather than recomputed from the
        // word threshold (REQ-586 BR-6, gotcha #3): the two thresholds scale
        // from two different currencies on a remote route, so this is the one
        // the harness will actually apply.
        let config = HarnessConfig::default();
        assert!(
            MAX_TOPIC_BYTES > config.summarize_threshold_bytes,
            "the per-topic ceiling is {MAX_TOPIC_BYTES} bytes and the default harness \
             summarizes a tool result past {} bytes ({} words). REQ-612 raised the \
             ceiling *past* the threshold on purpose — a topic may say everything it \
             knows, and the `digest` duty is what bounds the result. If the ceiling has \
             come back under the threshold, say why here rather than leaving the old \
             invariant to look like it was never abandoned.",
            config.summarize_threshold_bytes,
            config.summarize_threshold_tokens
        );

        // And the duty really is what bounds it: `Data`, so the turn loop's
        // `disposition == Expansion` bypass does not apply.
        for (name, _) in TOPICS {
            let outcome = call(name);
            assert_eq!(
                outcome.disposition,
                ResultDisposition::Data,
                "`{name}` is served with a disposition the turn loop exempts from the \
                 `digest` duty, which would leave `MAX_TOPIC_BYTES` as the only bound on \
                 what one docs read can push into a conversation"
            );
        }
        // The unknown-topic error takes the same path, so a runaway page can
        // never be smuggled in through the didactic arm either.
        assert_eq!(call("nope").disposition, ResultDisposition::Data);
    }

    /// **The resident cost of this tool, pinned.**
    ///
    /// [`DESCRIPTION`] is the only part of `teton_docs` that is in the system
    /// prompt on every turn, so it is the only part that competes with the
    /// bundled guide for the margin the two prompt-size tests measure. Growing
    /// it is a decision; making it here means the decision gets made on purpose.
    #[test]
    fn the_description_stays_inside_its_prompt_budget() {
        assert!(
            DESCRIPTION.chars().count() <= MAX_DESCRIPTION_CHARS,
            "the tool description is {} characters against a budget of \
             {MAX_DESCRIPTION_CHARS}. Tool docs are resident prompt bytes on every turn \
             (LESSON-493): shorten it rather than raising the budget, and re-run the two \
             margin tests either way.\n{DESCRIPTION}",
            DESCRIPTION.chars().count()
        );
    }

    /// The index the model reads and the topics that exist are two spellings of
    /// one list, and they must agree in both directions — a topic missing from
    /// the index is unreachable, and an index naming a topic that does not exist
    /// sends the model to a didactic error it cannot escape.
    #[test]
    fn the_description_indexes_every_bundled_topic() {
        assert!(
            DESCRIPTION.ends_with(&format!("topics: {TOPIC_INDEX}")),
            "the description must end with the topic index, which is what a model \
             matches a subject against: {DESCRIPTION}"
        );
        let indexed: BTreeSet<&str> = TOPIC_INDEX.split(", ").collect();
        let bundled: BTreeSet<&str> = TOPICS.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            indexed, bundled,
            "`TOPIC_INDEX` and `TOPICS` disagree. Both the description the model reads \
             and the unknown-topic error are built from the index, so a topic missing \
             from it is one nothing will ever ask for."
        );
        assert_eq!(
            bundled.len(),
            TOPICS.len(),
            "two topics share a name, so `body_of` answers with whichever comes first: \
             {:?}",
            TOPICS.iter().map(|(name, _)| *name).collect::<Vec<_>>()
        );
        for (name, _) in TOPICS {
            assert!(
                !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "`{name}` is not a lowercase single token, and the argument a weak model \
                 has to type is exactly this string"
            );
        }
    }

    /// One topic, whitespace-collapsed and un-emphasised, for the phrase
    /// assertions below — the same normalization [`skills_topic`] does, which
    /// exists so a line wrap in the Markdown cannot break a needle.
    fn topic(name: &str) -> String {
        let served = call(name);
        assert!(!served.is_error, "{}", served.content);
        served
            .content
            .replace('*', "")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// A phrase a topic has to carry, with the reason it exists in the failure.
    fn states(body: &str, name: &str, phrase: &str, claim: &str) {
        assert!(
            body.contains(phrase),
            "the `{name}` topic no longer says `{phrase}`, so it no longer states that \
             {claim}. Fix crates/tetond/src/harness/docs/{name}.md, or — if the wording \
             changed on purpose — move the needle with it. Deleting the needle deletes \
             the only thing that notices when the topic and the product disagree."
        );
    }

    /// **BR-2, the `commands` topic.** Its whole job is to stop the seven-tool-call
    /// repository search, so the three claims that do that are pinned.
    #[test]
    fn the_commands_topic_refuses_to_run_a_command_and_says_where_config_is_not() {
        let body = topic("commands");
        states(
            &body,
            "commands",
            "You cannot run any of them",
            "the model does not dispatch a built-in command (BR-1's sentence, on the \
             page as well as in the guide)",
        );
        states(
            &body,
            "commands",
            "never inside the repository you are working in",
            "Teton's configuration is not in the tree, which is the fact whose absence \
             cost seven tool calls and a wrong answer off another tool's file \
             (LESSON-493)",
        );
        states(
            &body,
            "commands",
            "Type `/transcript`",
            "the page shows the exact reply shape for a session-state question, worked \
             on the question that was actually asked",
        );
    }

    /// **BR-2, the `transcript` topic.** The two switches, the directory, and the
    /// refusal — the last being the one a model cannot learn by trying, because
    /// trying is what the refusal stops.
    #[test]
    fn the_transcript_topic_names_both_switches_and_the_tool_refusal() {
        let body = topic("transcript");
        states(
            &body,
            "transcript",
            "[transcript] enabled",
            "the durable switch is named with the table it lives in",
        );
        states(
            &body,
            "transcript",
            "`/transcript on` / `/transcript off`",
            "the session switch is named beside the durable one, since the whole \
             difficulty is that there are two with different lifetimes",
        );
        states(
            &body,
            "transcript",
            "denied prefix",
            "the topic says why `read`, `glob` and `grep` refuse the directory, rather \
             than leaving the model to discover it one refusal at a time (REQ-611 ADR-7)",
        );
        states(
            &body,
            "transcript",
            "I cannot run it",
            "the topic dictates the honest half of the answer: naming the command is not \
             the same as being able to run it",
        );
    }

    /// **AC-2: a new command cannot be added without its docs line.**
    ///
    /// The enumeration source is `teton_protocol::commands::SESSION_COMMANDS`,
    /// not the CLI's `slash::COMMANDS` — the `teton` crate is not a dependency of
    /// this one (the arrow runs the other way, and `cli_rows.rs` reaches the
    /// bundled guide by `include_str!` rather than by a crate edge), so a test
    /// here cannot see that table.
    ///
    /// **The guarantee is therefore a composition of two guards, and it is worth
    /// naming as such rather than glossing:** `slash.rs`'s
    /// `the_protocol_roster_and_the_command_table_are_the_same_set` pins the
    /// roster to the dispatch table in *both* directions, and this test pins the
    /// page to the roster. A command added to `COMMANDS` with no roster row fails
    /// there; a roster row with no docs line fails here. Neither guard alone is
    /// AC-2, and the chain has no gap only because the first one asserts equality
    /// rather than containment.
    ///
    /// # Mutation
    ///
    /// Deleting any single `- **`/name`**` line from `commands.md` goes red here
    /// naming that command. Run before trusting it.
    #[test]
    fn the_commands_topic_names_every_registered_command() {
        let body = topic("commands");
        let missing: Vec<&str> = teton_protocol::commands::SESSION_COMMANDS
            .iter()
            .filter(|c| !body.contains(&format!("`/{}`", c.name)))
            .map(|c| c.name)
            .collect();
        assert!(
            missing.is_empty(),
            "`teton_docs commands` does not name {missing:?}. A command the page \
             omits is one the model will never offer the user, which is the whole \
             defect REQ-617 exists to close. Add a line to \
             crates/tetond/src/harness/docs/commands.md."
        );

        // The count too, because `contains` on `/model` is satisfied by the line
        // for `/model set`. Without this, deleting the `/model` line alone would
        // leave the loop above green.
        let named = body.matches("- `/").count() + body.matches("- **`/").count();
        assert_eq!(
            named,
            teton_protocol::commands::SESSION_COMMANDS.len(),
            "the page carries {named} command lines for {} roster rows. A prefix \
             match hides a missing line when one name is a prefix of another \
             (`/model` inside `/model set`), so the count is what actually \
             catches it.",
            teton_protocol::commands::SESSION_COMMANDS.len()
        );
    }

    /// **BR-2, the two completed topics.** `context` must name all three switch
    /// forms and the config key; `skills` must name the four load globs and say
    /// that `skill` is the model's only route.
    #[test]
    fn the_context_and_skills_topics_carry_what_br_2_completes() {
        let ctx = topic("context");
        states(
            &ctx,
            "context",
            "/context on|off",
            "the session switch is named in its typed form",
        );
        states(
            &ctx,
            "context",
            "/context init",
            "the generation command is named, since it is the one that writes",
        );
        states(
            &ctx,
            "context",
            "[context] repo_file",
            "the durable key is named with its table",
        );

        let sk = topic("skills");
        states(
            &sk,
            "skills",
            "skills/<name>/SKILL.md",
            "the directory-form glob is named",
        );
        states(
            &sk,
            "skills",
            "commands/<name>.md",
            "the file-form glob is named",
        );
        states(
            &sk,
            "skills",
            "under `~/.claude/` and `<root>/.claude/`",
            "both roots are named, which with the two forms above is the four locations",
        );
        states(
            &sk,
            "skills",
            "The `skill` tool is the only way you run one",
            "the model's single route is stated outright — the transcript's model called \
             `skill` correctly four times and still hunted for another way in",
        );
    }
}
