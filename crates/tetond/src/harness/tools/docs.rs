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
//! system prompt sits under a pinned byte ceiling with tens of bytes of
//! headroom, and BUG-168 already had to shorten one phrase to pay for another.
//! Depth cannot live there. It lives here instead, and the only resident cost is
//! [`DESCRIPTION`] — one line naming the topic index — so adding a fifth topic
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
/// [`every_bundled_topic_is_under_the_ceiling`](tests::every_bundled_topic_is_under_the_ceiling).
///
/// A topic is a *tool result*, not resident prompt, so the constraint is not the
/// prompt budget but the conversation it lands in: against the local engine's
/// 16,384-token window, 4 KiB is roughly a thousand tokens, which keeps a full
/// docs read a small fraction of the window. A topic that grew past this could
/// evict the very turn it was fetched to serve — the failure that reads to a
/// user as the agent forgetting what they asked (LESSON-482).
pub const MAX_TOPIC_BYTES: usize = 4096;

/// Every bundled topic, as `(name, body)`, in the order a reader should meet
/// them: how to connect a provider, where work is then routed, the separate
/// web opt-in, and how to read the diagnostic when one of those is wrong.
///
/// A `&[(&str, &str)]` rather than four constants because every rule below —
/// the ceiling sweep, the topic index, the unknown-topic error — is a statement
/// about the *set*, and a set spelled out four times is one a fifth topic can be
/// added to without.
const TOPICS: &[(&str, &str)] = &[
    ("providers", include_str!("../docs/providers.md")),
    ("policy", include_str!("../docs/policy.md")),
    ("web", include_str!("../docs/web.md")),
    ("doctor", include_str!("../docs/doctor.md")),
];

/// The topic index as the model reads it, in [`DESCRIPTION`] and in the
/// unknown-topic error.
///
/// Written out rather than joined from [`TOPICS`] because both places it appears
/// are `const` — and because a hand-written second spelling is what
/// [`the_description_indexes_every_bundled_topic`](tests::the_description_indexes_every_bundled_topic)
/// can compare against, the same golden posture the recipe catalog takes.
const TOPIC_INDEX: &str = "providers, policy, web, doctor";

/// The model-facing description, budgeted against
/// [`MAX_DESCRIPTION_CHARS`](tests::MAX_DESCRIPTION_CHARS) — it is resident
/// prompt on every turn of every session, so its length is a decision somebody
/// makes rather than a side effect (LESSON-493).
///
/// It ends with the topic index because the index *is* the affordance: a model
/// deciding whether this tool answers the question in front of it is matching
/// the subject it was asked about against these four words. BUG-168's lesson
/// applies — a capability the prompt does not name outright is one the local
/// tier does not reach for.
const DESCRIPTION: &str = concat!(
    "Read Teton's own setup and troubleshooting docs, bundled in this binary. ",
    "topics: providers, policy, web, doctor"
);

/// The body of `topic`, or `None` when nothing by that name is bundled.
#[must_use]
fn body_of(topic: &str) -> Option<&'static str> {
    TOPICS
        .iter()
        .find(|(name, _)| *name == topic)
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
        json!({
            "type": "object",
            "properties": {
                "topic": {
                    "type": "string",
                    "description": "One of: providers, policy, web, doctor"
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
            None => ToolOutcome::error(format!(
                "unknown topic `{topic}`; valid topics: {TOPIC_INDEX}"
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
    /// It lives in this module and not beside [`DESCRIPTION`] for a mechanical
    /// reason worth knowing: `boundary_coverage.rs` derives the universe of
    /// tools from the source text *before* each file's first `#[cfg(test)]`
    /// line, so a `cfg(test)` item above the `impl Tool` block hides the tool
    /// from that scan entirely — narrowing the universe, which is the one
    /// direction that fail-safe is not safe in.
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
            4,
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

    /// **AC-3, the didactic half (BR-8).** An unknown topic names all four valid
    /// ones, so the recovery is the model's next turn rather than the user's.
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

    /// **BR-9 / AC-8: every bundled topic is under the ceiling.**
    ///
    /// Swept over [`TOPICS`] rather than written per topic, so a fifth topic is
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
                body.len() <= MAX_TOPIC_BYTES,
                "the `{name}` topic is {} bytes against a ceiling of {MAX_TOPIC_BYTES}. \
                 Trim it, or split it into a second topic and add that topic to `TOPICS` \
                 and to the index in `DESCRIPTION`. Do not raise the ceiling and do not \
                 delete this assertion: the ceiling exists so one docs read can never \
                 evict the conversation it was fetched to serve (REQ-577 BR-9).",
                body.len()
            );
        }
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
