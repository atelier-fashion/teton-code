//! The `route` classifier: one small local model call that assigns a **freeform**
//! turn to one of the four judgment categories (REQ-558 BR-3, ADR-C).
//!
//! ## What it replaces
//!
//! Nothing classified a prompt before this module existed. What decided where a
//! freeform turn went was `AUXILIARY_SIGNALS` — a ten-word substring list that
//! sent *"explain the tradeoffs between these two architectures"* to a 3B local
//! model for containing the word `explain`, and never consulted the user's
//! configured table at all. TASK-050 deleted that list; this is the thing that
//! stands in its place (BR-2: deleted, not relocated).
//!
//! ## Three properties, each held by construction rather than by care
//!
//! **The router stays prompt-blind.** No routing function can see prompt text —
//! `Router::freeform_category` and `Router::resolve` both take none, and
//! `no_routing_function_can_see_prompt_text` pins their types. The classifier
//! therefore runs *outside* the router, here, on the turn path; its
//! [`JudgmentCategory`] answer is what goes in. A prompt parameter on any
//! routing function is precisely where a substring list reappears.
//!
//! **It cannot name a harness-known category.** [`JudgmentCategory`] has four
//! variants, and [`JudgmentCategory::from_str`] is the only parse from text into
//! the category space. A model that answers `"digest"` produces a parse failure,
//! not a category (AC-3, BR-2). Nothing here converts `&str` → `Category`.
//!
//! **A failure degrades to a real category, loudly.** LESSON-447: when a
//! best-effort step guards an invariant, its fallback must still hold that
//! invariant by a degraded means, and the degradation must be visible. The
//! invariant here is "every freeform turn dispatches on a category resolved
//! through the configured table". So an engine error, a task that did not
//! complete, or output naming no judgment category all fall back to the BR-9
//! **declared default** — a real category, resolved through the same chain — and
//! say so in the sentence `route_decided` carries. `Err(_) => pick something` is
//! the shape that lesson forbids.
//!
//! ## The bypass costs nothing (BR-5)
//!
//! When the local tier cannot serve, classification is **skipped entirely**:
//! [`ClassifierPlan::Bypass`] issues no call, and there is no arm anywhere in
//! this module that reaches a remote provider. REQ-544 BR-8 is the reason — the
//! local tier's value is latency, and a remote call to decide where to send a
//! call has already spent what the decision was meant to save. The bypass is
//! asserted by call count rather than by output text, because "no network
//! request was made" is not a property of a string.
//!
//! Note what this module does *not* do: it never asks whether a provider is
//! local. `Category::Route` has no [`ConfigurableCategory`](teton_core::category::ConfigurableCategory)
//! counterpart, so the resolver reaches it through a branch that consults no
//! binding and returns the local tier or nothing — the pin is the resolver's, and
//! a second locality check here would be a guard placed where it is convenient
//! rather than where the decision is made (LESSON-484). What this module owns is
//! only the decision *not to classify at all*.
//!
//! ## Latency
//!
//! This is a new model call on a path that had none: every freeform judgment turn
//! now waits on it before the real call starts. The budget is sized for that —
//! [`CLASSIFIER_INPUT_MAX_BYTES`] of prompt (an eighth of the summarizer's) and
//! [`CLASSIFIER_MAX_TOKENS`] of output, greedy, with a one-word answer contract
//! instead of free text.

use std::sync::{Arc, Mutex};

use teton_core::category::{Category, CategoryResolution, JudgmentCategory};
use teton_inference::{ChatFormat, Engine, GenParams};

use crate::harness::context::truncate_middle;
use crate::harness::render::render_duty;

/// Most prompt bytes the classifier ever reads, head + tail
/// ([`truncate_middle`]).
///
/// Bounding the guard's own input is the other half of LESSON-447: the
/// summarizer's fallback fired precisely on the inputs it existed for, because
/// the same oversized text that triggered it also blew its own prompt past the
/// engine window. A classifier that cannot classify long prompts would fail over
/// to the declared default on exactly the turns most likely to be `design` or
/// `debug`. Intent lives in the opening and closing sentences of a request, so
/// head + tail is the shape that keeps it.
pub const CLASSIFIER_INPUT_MAX_BYTES: usize = 2_048;

/// The local tier as the classifier receives it: the engine handle and the
/// [`ChatFormat`] it was installed with.
///
/// The two travel together because they are read together, in one look at the
/// daemon's engine slot — asking the engine its own format on the async path
/// would park a tokio worker behind whatever completion currently owns its mutex
/// (LESSON-448).
pub type LocalEngine = (Arc<Mutex<dyn Engine>>, ChatFormat);

/// The output contract, verbatim: the last sentence of every classifier prompt.
///
/// Exported because it is also how the CI/offline stand-in engine
/// ([`crate::runtime::ScriptedFileEngine`]) recognizes a classification and
/// answers it *without consuming a scripted turn* — a duty is not a turn. One
/// constant, used to write the sentence and to recognize it, so the seam cannot
/// drift out of step with the prompt.
pub const CLASSIFIER_OUTPUT_CONTRACT: &str =
    "Reply with exactly one word: edit, design, debug, or review.";

/// Generation budget for a one-word answer.
///
/// Small on purpose (BR-5's latency premise): a category name is one or two BPE
/// tokens, and the slack covers a leading newline or a stray quote. A model that
/// wants to write an essay instead is cut off here and its output fails the
/// parse, which is the declared-default path — never a silent miscategorization.
pub const CLASSIFIER_MAX_TOKENS: u32 = 8;

/// Why a freeform turn's judgment category is the one it is (BR-3, BR-5, BR-9).
///
/// Carried onto `route_decided` through [`Classification::sentence`], because
/// REQ-544 BR-5's legibility promise applies to classification as it does to
/// policy: a user who sees a turn go somewhere surprising must be able to read
/// which signal sent it there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassificationSignal {
    /// The local classifier ran and named this category.
    Classified,
    /// Classification was skipped: the local tier could not serve it. **No call
    /// was issued at all** — not locally, and never remotely (BR-5). The reason
    /// is the resolver's own sentence for the `route` category, so the bypass is
    /// explained by the same function that made the decision (BR-6).
    Bypassed {
        /// Why the local tier could not serve the classification.
        reason: String,
    },
    /// The classifier ran and failed — an engine error, a task that did not
    /// complete, or output naming no judgment category. The declared default
    /// stands in (BR-9) and the failure is reported rather than swallowed
    /// (LESSON-447).
    Failed {
        /// What went wrong, in a form safe to broadcast: an engine error message
        /// (which by [`teton_inference::EngineError`]'s contract carries no
        /// prompt content) or a fixed sentence. **Never the model's own output**
        /// — a classifier that echoes the prompt back would put prompt text on
        /// the event bus by way of a routing reason.
        error: String,
    },
}

/// The category a freeform judgment turn dispatches on, and the signal that
/// chose it.
///
/// There is no `category: Option` here and no "unclassified" variant: every
/// construction path produces a real [`JudgmentCategory`], because the caller's
/// next step is to resolve one through the configured table. The *signal* is what
/// records whether that category came from the model or from the declared
/// default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    /// The category to dispatch on.
    pub category: JudgmentCategory,
    /// How it was chosen.
    pub signal: ClassificationSignal,
}

impl Classification {
    /// The sentence `route_decided` carries, naming the signal that fired
    /// (BR-3, REQ-544 BR-5).
    ///
    /// It is prepended to the resolver's own sentence by
    /// [`Router::resolve_judgment`](crate::router::Router::resolve_judgment), so
    /// one reason answers both halves of the question: why *this* category, and
    /// why *that* provider.
    #[must_use]
    pub fn sentence(&self) -> String {
        let category = self.category;
        match &self.signal {
            ClassificationSignal::Classified => format!(
                "The 'route' classifier read this turn's prompt on the local tier and \
                 classified it as '{category}'."
            ),
            ClassificationSignal::Bypassed { reason } => format!(
                "Classification was bypassed for this turn, so the declared default category \
                 '{category}' stands in; no classification call was issued, locally or \
                 remotely. {reason}"
            ),
            ClassificationSignal::Failed { error } => format!(
                "The 'route' classifier failed ({error}), so the declared default category \
                 '{category}' stands in."
            ),
        }
    }
}

/// Whether this turn classifies at all, decided **before** any call is issued.
///
/// The engine handle rides on the [`ClassifierPlan::Classify`] arm rather than
/// beside the plan, so "we decided to classify" and "we have something to
/// classify with" cannot disagree — there is no state where the plan says go and
/// the caller has no engine.
pub enum ClassifierPlan {
    /// Classify with this engine, rendering for this [`ChatFormat`].
    Classify {
        /// The local tier's engine.
        engine: Arc<Mutex<dyn Engine>>,
        /// The rendering family it was installed with (read from the engine slot,
        /// never by locking the engine on the async path — LESSON-448).
        format: ChatFormat,
    },
    /// Skip classification. No call is issued, locally or remotely (BR-5).
    Bypass {
        /// Why, in the words of whichever check declined.
        reason: String,
    },
}

/// Decide whether to classify this turn (BR-5) — pure, and taken before any call.
///
/// `route` is the **`route` category's own resolution**, and it is the whole
/// availability question. `Category::Route` has no `ConfigurableCategory`
/// counterpart, so `teton_core::category::resolve` reaches it through the pinned
/// branch: it returns the local tier or it returns nothing. A resolution that
/// selected a provider therefore *is* the statement that the local tier can
/// serve, and this function asks no locality question of its own (LESSON-484,
/// BR-4/BR-5's by-construction pin).
///
/// `local_engine` is the daemon's engine slot as read for this turn. It can be
/// empty while the resolution still selects — the tier is registered but its
/// weights are still loading — and that is a bypass too, for the same reason and
/// with the same cost: nothing.
#[must_use]
pub fn plan(route: &CategoryResolution, local_engine: Option<LocalEngine>) -> ClassifierPlan {
    debug_assert_eq!(
        route.category,
        Category::Route,
        "the classifier plans from the `route` category's own resolution"
    );

    let Some(provider) = route.provider_id.as_deref() else {
        // The resolver's sentence, verbatim. It already names the category, the
        // pin, and what is missing — re-describing it here would be the second
        // surface BR-6 exists to prevent.
        return ClassifierPlan::Bypass {
            reason: route.reason.clone(),
        };
    };

    let Some((engine, format)) = local_engine else {
        return ClassifierPlan::Bypass {
            reason: format!(
                "The 'route' category resolves to '{provider}', but no local engine is loaded \
                 to serve it yet."
            ),
        };
    };

    ClassifierPlan::Classify { engine, format }
}

/// Classify `prompt` into a judgment category, or take `default` (BR-9).
///
/// Every path returns a [`Classification`] carrying a real category: the model's
/// answer when it parsed, and `default` when the plan was a bypass or the call
/// failed. Nothing here can fail the turn, and nothing here is silent — the
/// signal says which happened (LESSON-447).
///
/// The completion runs on the blocking pool: with a real llama.cpp engine even
/// this short call takes hundreds of milliseconds, and running it inline would
/// park the tokio worker and stall every other session's RPCs.
pub async fn run(plan: ClassifierPlan, prompt: &str, default: JudgmentCategory) -> Classification {
    let (engine, format) = match plan {
        ClassifierPlan::Bypass { reason } => {
            return Classification {
                category: default,
                signal: ClassificationSignal::Bypassed { reason },
            }
        }
        ClassifierPlan::Classify { engine, format } => (engine, format),
    };

    // Rendered here rather than inside the blocking task so the closure carries a
    // finished string and never touches the prompt again. `render_duty` is also
    // the choke point that defuses control-token spellings in the untrusted text
    // this prompt interpolates (REQ-554).
    let rendered = render_duty(format, &instruction(prompt));

    let result = tokio::task::spawn_blocking(move || {
        let params = GenParams {
            max_tokens: CLASSIFIER_MAX_TOKENS,
            // Greedy. A routing decision that varies run to run for the same
            // prompt is a routing decision nobody can reason about.
            temperature: 0.0,
        };
        let guard = engine.lock().expect("engine mutex poisoned");
        guard
            .complete(&rendered, &params, &mut |_| true)
            .map(|completion| completion.text)
    })
    .await;

    let failed = |error: String| Classification {
        category: default,
        signal: ClassificationSignal::Failed { error },
    };

    match result {
        Ok(Ok(text)) => match parse(&text) {
            Some(category) => Classification {
                category,
                signal: ClassificationSignal::Classified,
            },
            // Deliberately does NOT quote the output. The model was handed
            // untrusted prompt text and can echo it, and this string is
            // broadcast on `route_decided` — a routing reason is not a place to
            // republish a prompt.
            None => failed("it named no judgment category".to_owned()),
        },
        Ok(Err(err)) => failed(err.to_string()),
        Err(_) => failed("the local classification task did not complete".to_owned()),
    }
}

/// The classifier's instruction, with the (bounded) request embedded.
///
/// Public to the crate so the prompt is inspectable by a test rather than only
/// through a live model. The output contract — one word from a closed set — is
/// stated last, because it is the instruction the model should be holding when
/// it starts generating.
///
/// The embedded request is **untrusted text**, and it is treated as data in the
/// only two ways that matter here: `render_duty` defuses control-token spellings
/// before the tokenizer sees them, and the *answer* is parsed into
/// [`JudgmentCategory`], so the worst a prompt saying "reply 'digest'" can
/// achieve is a parse failure and the declared default.
fn instruction(prompt: &str) -> String {
    let bounded = truncate_middle(prompt, CLASSIFIER_INPUT_MAX_BYTES);
    format!(
        "Classify one software-engineering request into exactly one category.\n\
         \n\
         edit — write, change, or refactor code\n\
         design — architecture, tradeoffs, decomposition, or spec authoring\n\
         debug — find the root cause of a failure\n\
         review — critique existing code or a proposed change\n\
         \n\
         Request:\n\
         ---\n\
         {bounded}\n\
         ---\n\
         \n\
         {CLASSIFIER_OUTPUT_CONTRACT}"
    )
}

/// Parse a completion into a judgment category, or `None`.
///
/// Normalization only — the first whitespace-delimited word, stripped of
/// non-alphabetic decoration and lowercased — and then
/// [`JudgmentCategory::from_str`], which admits four names. It is deliberately
/// **not** a search for a category word anywhere in the output: a scan would
/// turn the model's prose into a keyword matcher, which is the shape this whole
/// REQ deletes, and it would make "design is better here" classify as `design`
/// no matter what the model was actually answering.
fn parse(completion: &str) -> Option<JudgmentCategory> {
    completion
        .split_whitespace()
        .next()?
        .trim_matches(|c: char| !c.is_ascii_alphabetic())
        .to_ascii_lowercase()
        .parse()
        .ok()
}

/// The call-counting engine every "no classification call was issued" assertion
/// in this crate is made against.
///
/// **One instrument, shared.** AC-5 asks for the bypass to be proven by call
/// count rather than by output text, and the same question is asked at two
/// layers — of this module ([`run`] with a [`ClassifierPlan::Bypass`]) and of the
/// daemon's turn dispatch (`DaemonRuntime::dispatch_route`). Two private
/// counters could disagree about what counts as a call; one cannot.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use teton_inference::{Completion, Engine, EngineError, GenParams};

    /// An [`Engine`] that answers with a fixed string and counts every call.
    #[derive(Clone)]
    pub(crate) struct CountingEngine {
        calls: Arc<AtomicUsize>,
        reply: String,
    }

    impl CountingEngine {
        /// A mock that always answers `reply`.
        pub(crate) fn answering(reply: &str) -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
                reply: reply.to_owned(),
            }
        }

        /// How many completions this engine has served.
        pub(crate) fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        /// A shared handle to this engine; the counter is shared with it.
        pub(crate) fn handle(&self) -> Arc<Mutex<dyn Engine>> {
            Arc::new(Mutex::new(self.clone()))
        }
    }

    impl Engine for CountingEngine {
        fn model_id(&self) -> &str {
            "counting"
        }

        fn complete(
            &self,
            _prompt: &str,
            _params: &GenParams,
            _on_token: &mut dyn FnMut(&str) -> bool,
        ) -> Result<Completion, EngineError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Completion::cold(self.reply.clone(), 0, 1))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::CountingEngine;
    use super::*;

    use teton_core::category::BindingSource;
    use teton_core::policy::RouteOutcome;
    use teton_inference::{Completion, EngineError, MockEngine};

    /// An engine that always errors, for the LESSON-447 fallback leg.
    struct FailingEngine;

    impl Engine for FailingEngine {
        fn model_id(&self) -> &str {
            "failing"
        }

        fn complete(
            &self,
            _prompt: &str,
            _params: &GenParams,
            _on_token: &mut dyn FnMut(&str) -> bool,
        ) -> Result<Completion, EngineError> {
            Err(EngineError::Backend("the tier fell over".to_owned()))
        }
    }

    fn handle(engine: impl Engine + 'static) -> Arc<Mutex<dyn Engine>> {
        Arc::new(Mutex::new(engine))
    }

    /// A `route` resolution that selected `provider`.
    fn route_resolved(provider: &str) -> CategoryResolution {
        CategoryResolution {
            category: Category::Route,
            tier: Category::Route.tier(),
            provider_id: Some(provider.to_owned()),
            fallback_id: None,
            source: BindingSource::PinnedLocal,
            reason: "The 'route' category is pinned to the local tier by construction, so it \
                     routes to 'local' regardless of any tier or per-category binding."
                .to_owned(),
            outcome: RouteOutcome::Primary,
        }
    }

    /// A `route` resolution that selected nothing — the local tier cannot serve.
    fn route_unresolved() -> CategoryResolution {
        CategoryResolution {
            category: Category::Route,
            tier: Category::Route.tier(),
            provider_id: None,
            fallback_id: None,
            source: BindingSource::PinnedLocal,
            reason: "The 'route' category is pinned to the local tier and 'local' is \
                     unavailable. It is never routed to a remote provider — a classifier that \
                     calls a remote model has spent what the routing decision saves — so \
                     'route' cannot be routed."
                .to_owned(),
            outcome: RouteOutcome::NoHealthyProvider,
        }
    }

    // -- the output contract -------------------------------------------------

    #[test]
    fn every_judgment_category_round_trips_through_the_parse() {
        for category in JudgmentCategory::ALL {
            assert_eq!(parse(category.as_str()), Some(category));
        }
    }

    #[test]
    fn the_parse_normalizes_decoration_but_does_not_search() {
        // Decoration a small model adds around a one-word answer.
        assert_eq!(parse(" Design\n"), Some(JudgmentCategory::Design));
        assert_eq!(parse("**debug**"), Some(JudgmentCategory::Debug));
        assert_eq!(parse("\"review\","), Some(JudgmentCategory::Review));
        assert_eq!(parse("edit — write code"), Some(JudgmentCategory::Edit));

        // But it reads the ANSWER, not the prose. A scan-anywhere parse would
        // classify both of these, and both would be wrong.
        assert_eq!(parse("I think this is a design question"), None);
        assert_eq!(parse("none of edit, design, debug, review apply"), None);
        assert_eq!(parse(""), None);
    }

    /// **AC-3 at runtime.** The type makes `digest` unassignable at compile
    /// time; this is what a model that answers it anyway actually gets.
    #[test]
    fn a_model_naming_a_harness_known_category_gets_a_parse_failure() {
        for harness_known in ["digest", "redact", "title", "compact", "triage", "route"] {
            assert_eq!(
                parse(harness_known),
                None,
                "'{harness_known}' is not reachable from model output"
            );
        }
    }

    // -- the prompt ----------------------------------------------------------

    #[test]
    fn the_instruction_names_the_four_categories_and_the_one_word_contract() {
        let text = instruction("make the button blue");
        for category in JudgmentCategory::ALL {
            assert!(
                text.contains(category.as_str()),
                "the instruction offers '{category}'"
            );
        }
        assert!(text.contains("make the button blue"));
        assert!(
            text.trim_end().ends_with(CLASSIFIER_OUTPUT_CONTRACT),
            "the output contract is the last thing the model reads: {text}"
        );
        // The contract names exactly the four the parse accepts, and nothing the
        // parse would reject.
        for category in JudgmentCategory::ALL {
            assert!(CLASSIFIER_OUTPUT_CONTRACT.contains(category.as_str()));
        }
    }

    /// The guard's own input is bounded, so the classifier cannot be broken by
    /// the prompt it is classifying (LESSON-447's second half).
    #[test]
    fn a_huge_prompt_is_bounded_before_it_reaches_the_engine() {
        let huge = "x".repeat(CLASSIFIER_INPUT_MAX_BYTES * 40);
        let text = instruction(&huge);
        assert!(
            text.len() < CLASSIFIER_INPUT_MAX_BYTES + 1_024,
            "instruction is {} bytes",
            text.len()
        );
    }

    // -- the bypass (AC-5) ---------------------------------------------------

    /// **AC-5 / BR-5.** The local tier cannot serve, so the turn takes the
    /// declared default and **no call is issued** — asserted on the counter of
    /// an engine that was perfectly reachable, not on the text of anything.
    #[tokio::test]
    async fn a_bypassed_turn_issues_no_call_at_all() {
        let engine = CountingEngine::answering("design");
        let plan = plan(
            &route_unresolved(),
            Some((engine.handle(), ChatFormat::Flat)),
        );
        let classification = run(plan, "explain the tradeoffs", JudgmentCategory::Edit).await;

        assert_eq!(engine.calls(), 0, "the bypass issues no call (BR-5)");
        assert_eq!(classification.category, JudgmentCategory::Edit);
        assert!(matches!(
            classification.signal,
            ClassificationSignal::Bypassed { .. }
        ));
        // And it says so, carrying the resolver's own explanation forward.
        let sentence = classification.sentence();
        assert!(sentence.contains("bypassed"), "{sentence}");
        assert!(
            sentence.contains("no classification call was issued, locally or remotely"),
            "{sentence}"
        );
        assert!(
            sentence.contains("never routed to a remote provider"),
            "{sentence}"
        );
    }

    /// **The other half of AC-5**, and worth being exact about the split.
    ///
    /// A counter on the local engine proves the *local* classifier did not run.
    /// It cannot prove a *remote* one did not, because a remote call would not
    /// touch that counter — so the no-remote-call half is held structurally
    /// instead: [`plan`] takes a local [`Engine`] or nothing, and
    /// [`ClassifierPlan`] has exactly two arms, neither of which carries a
    /// provider, an endpoint, or an egress handle. [`run`] can therefore only
    /// reach what the plan carries.
    ///
    /// This is a compile-level assertion, which is the point: adding a `Remote`
    /// arm, or a provider parameter to `plan`, stops compiling here rather than
    /// quietly passing a runtime check that was only ever watching the local
    /// engine.
    #[test]
    fn the_classifier_can_reach_a_local_engine_and_nothing_else() {
        let _plan: fn(&CategoryResolution, Option<LocalEngine>) -> ClassifierPlan = plan;

        // Exhaustive, with no wildcard: a third arm fails to compile.
        let bypass = ClassifierPlan::Bypass {
            reason: "unavailable".to_owned(),
        };
        match bypass {
            ClassifierPlan::Classify {
                engine: _,
                format: _,
            } => unreachable!(),
            ClassifierPlan::Bypass { reason } => assert_eq!(reason, "unavailable"),
        }
    }

    /// A registered local tier whose weights are still loading is the same
    /// bypass: no call, declared default, and a sentence naming the cause.
    #[tokio::test]
    async fn an_empty_engine_slot_bypasses_rather_than_failing_the_turn() {
        let plan = plan(&route_resolved("local"), None);
        let classification = run(plan, "anything", JudgmentCategory::Review).await;

        assert_eq!(classification.category, JudgmentCategory::Review);
        assert!(classification
            .sentence()
            .contains("no local engine is loaded"));
    }

    // -- the happy path ------------------------------------------------------

    #[tokio::test]
    async fn a_classified_turn_issues_exactly_one_call_and_reports_the_category() {
        let engine = CountingEngine::answering("design");
        let plan = plan(
            &route_resolved("local"),
            Some((engine.handle(), ChatFormat::Flat)),
        );
        let classification = run(
            plan,
            "explain the tradeoffs between these two architectures",
            JudgmentCategory::Edit,
        )
        .await;

        assert_eq!(engine.calls(), 1, "one classification, not two");
        assert_eq!(classification.category, JudgmentCategory::Design);
        assert_eq!(classification.signal, ClassificationSignal::Classified);
        assert!(classification.sentence().contains("'design'"));
    }

    /// The classifier renders through the same duty path the summarizer uses, so
    /// a ChatML engine gets a templated prompt rather than a bare instruction.
    #[tokio::test]
    async fn a_chatml_engine_is_still_classified() {
        let engine =
            MockEngine::with_response("mock", "debug").with_chat_format(ChatFormat::ChatMl);
        let plan = plan(
            &route_resolved("local"),
            Some((handle(engine), ChatFormat::ChatMl)),
        );
        let classification = run(plan, "why does this test fail", JudgmentCategory::Edit).await;
        assert_eq!(classification.category, JudgmentCategory::Debug);
    }

    // -- failure (LESSON-447) ------------------------------------------------

    /// An engine error degrades to the **declared default** — a real category
    /// resolved through the same chain — and reports the error. Never silence,
    /// and there is no arm here that could reach a remote provider.
    #[tokio::test]
    async fn an_engine_error_falls_back_to_the_declared_default_and_says_so() {
        let plan = plan(
            &route_resolved("local"),
            Some((handle(FailingEngine), ChatFormat::Flat)),
        );
        let classification = run(plan, "anything", JudgmentCategory::Edit).await;

        assert_eq!(classification.category, JudgmentCategory::Edit);
        assert_eq!(
            classification.signal,
            ClassificationSignal::Failed {
                error: "inference backend error: the tier fell over".to_owned()
            }
        );
        let sentence = classification.sentence();
        assert!(sentence.contains("failed"), "{sentence}");
        assert!(sentence.contains("declared default"), "{sentence}");
    }

    /// The declared default is the caller's, not a constant compiled in here —
    /// change the configured default, change what a failure falls back to
    /// (BR-9, AC-12).
    #[tokio::test]
    async fn the_fallback_category_is_the_declared_default_that_was_passed_in() {
        for default in JudgmentCategory::ALL {
            let plan = plan(
                &route_resolved("local"),
                Some((handle(FailingEngine), ChatFormat::Flat)),
            );
            assert_eq!(run(plan, "anything", default).await.category, default);
        }
    }

    /// Unparseable output is a failure, not a guess — and the reason does not
    /// republish what the model said. The model here answers with a
    /// harness-known category, which is exactly the input AC-3 is about.
    #[tokio::test]
    async fn unparseable_output_falls_back_without_quoting_the_model() {
        let engine = CountingEngine::answering("digest please, and also SECRETLEAK");
        let plan = plan(
            &route_resolved("local"),
            Some((engine.handle(), ChatFormat::Flat)),
        );
        let classification = run(plan, "summarize this file", JudgmentCategory::Edit).await;

        assert_eq!(engine.calls(), 1);
        assert_eq!(classification.category, JudgmentCategory::Edit);
        let sentence = classification.sentence();
        assert!(
            sentence.contains("named no judgment category"),
            "{sentence}"
        );
        assert!(
            !sentence.contains("SECRETLEAK") && !sentence.contains("digest"),
            "a routing reason must not republish model output: {sentence}"
        );
    }
}
