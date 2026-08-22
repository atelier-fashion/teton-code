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
//! right vehicle for the always-needed surface, and it is nearly full: the
//! system prompt sits under a pinned byte ceiling with little headroom — BUG-168
//! had to shorten one phrase to pay for another, and BUG-181 had to move the
//! ceiling (with its arithmetic re-checked) to land one capability sentence.
//! Depth cannot live there. It lives here instead, and the only resident cost is
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
/// A topic is a *tool result*, not resident prompt, so the constraint is not the
/// prompt budget but the conversation it lands in: against the local engine's
/// 16,384-token window, 4 KiB is roughly a thousand tokens, which keeps a full
/// docs read a small fraction of the window. A topic that grew past this could
/// evict the very turn it was fetched to serve — the failure that reads to a
/// user as the agent forgetting what they asked (LESSON-482).
pub const MAX_TOPIC_BYTES: usize = 4096;

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
    ("doctor", include_str!("../docs/doctor.md")),
];

/// The topic index as the model reads it, in [`DESCRIPTION`] and in the
/// unknown-topic error.
///
/// Written out rather than joined from [`TOPICS`] because both places it appears
/// are `const` — and because a hand-written second spelling is what
/// `the_description_indexes_every_bundled_topic` can compare against, the same
/// golden posture the recipe catalog takes.
const TOPIC_INDEX: &str = "providers, policy, context, web, skills, doctor";

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
/// the subject it was asked about against these six words. BUG-168's lesson
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
/// moved. **Spent: 102. Left: 18** — room for one more short name.
const DESCRIPTION: &str = concat!(
    "Read Teton's own docs, bundled in this binary. ",
    "topics: providers, policy, context, web, skills, doctor"
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
    #[test]
    fn every_topic_serves_its_whole_bundled_body() {
        assert_eq!(
            TOPICS.len(),
            6,
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
                 Trim it, or split it into a second topic and add that topic to `TOPICS` \
                 and to the index in `DESCRIPTION`. Do not raise the ceiling and do not \
                 delete this assertion: the ceiling exists so one docs read can never \
                 evict the conversation it was fetched to serve (REQ-577 BR-9).",
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

    /// **The ceiling is only a zero-eviction claim while it stays under the
    /// threshold that would summarize a result instead of delivering it.**
    ///
    /// [`MAX_TOPIC_BYTES`] is justified against the conversation a topic lands
    /// in, and the number that decides whether a tool result lands whole is
    /// `HarnessConfig::summarize_threshold_tokens` — above it, a result is sent
    /// to the summarizer rather than delivered. A docs read that crossed that
    /// line would stop being the zero-egress, zero-model-call lookup this
    /// module's header promises: summarizing a result is a model call, and on a
    /// remote-bound tier it is a model call carrying the body. The two
    /// constants are set in different files by different rationales, so the
    /// coupling is written down here rather than left to hold by luck.
    #[test]
    fn the_topic_ceiling_stays_under_the_summarize_threshold() {
        use crate::harness::turn_loop::HarnessConfig;

        // The byte twin is read off the config rather than recomputed from the
        // word threshold (REQ-586 BR-6, gotcha #3): the two thresholds scale
        // from two different currencies on a remote route, so a topic must clear
        // the byte one the harness will actually apply.
        let threshold_bytes = HarnessConfig::default().summarize_threshold_bytes;
        assert!(
            MAX_TOPIC_BYTES < threshold_bytes,
            "the per-topic ceiling is {MAX_TOPIC_BYTES} bytes and the default harness \
             summarizes a tool result past {threshold_bytes} bytes \
             ({} tokens). A topic at the ceiling would be \
             summarized instead of served — a model call, and on a remote tier a model call \
             carrying the body, which is not what `teton_docs` promises. Lower \
             `MAX_TOPIC_BYTES`, or raise the threshold deliberately and say why here.",
            HarnessConfig::default().summarize_threshold_tokens
        );
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
}
