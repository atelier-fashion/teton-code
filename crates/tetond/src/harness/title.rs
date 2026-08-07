//! The `title` duty: naming a session once, from the first thing the user asked
//! for (REQ-561 BR-1/BR-2/BR-3/BR-8/BR-9a, TASK-062).
//!
//! ## Not a tool's duty — the session's
//!
//! `triage` and `shell` hang off the tool that owns them, through
//! [`Tool::refine`](super::tools::Tool::refine) (ADR-10). `title` has no tool: a
//! session is named because it *is* a session, not because anything was grepped
//! or run. So its call site is the daemon's own prompt-turn entry point, which is
//! the one place that knows a session both exists and has now been asked for
//! something.
//!
//! ## It runs at most once per session, and a failure spends that once
//!
//! "Runs once per session" and "runs once per session *unless it failed*" are
//! different rules with very different bills. The second is the expensive one: a
//! duty that retries whenever the last attempt produced no title fires again on
//! every subsequent turn, which turns the cheapest category in the table into a
//! per-turn model call — and it does it precisely on the machines where the duty
//! is failing, i.e. where the calls are already not working.
//!
//! So the attempt is **claimed before it is made**
//! ([`SessionRegistry::claim_title`](crate::sessions::SessionRegistry::claim_title)),
//! not recorded after it succeeds. A session that failed to be named stays
//! unnamed, which is exactly the state every session was in before this REQ —
//! BR-3's degraded means is the pre-REQ behaviour, and here it costs nothing to
//! hold.
//!
//! The complementary guard is BR-9's: the title is written only when the session
//! has none, so a re-derivation cannot overwrite a name a session already
//! answers to.
//!
//! ## Always local, and not because this module says so
//!
//! `title` is a `reflex` category, and REQ-558 settled that an unbound `reflex`
//! tier inherits the **local** tier and never `default_provider` — "sub-second,
//! every turn, never leaves the machine". So a machine whose turns go to a
//! frontier provider still names its sessions on the local engine, and no check
//! here is what makes that true: it is the resolver's answer, arrived at through
//! the same table every other category reads (LESSON-484). This module has no
//! locality branch at all, and the shared [`duty`](super::duty) seam is what
//! would carry an explicitly bound `reflex` tier remotely if a user ever asked
//! for one — scoped, metered and refusable like any other duty.
//!
//! ## What is `title`-specific, and what is not
//!
//! Exactly the three things ADR-3 allows: the one-line resolver in
//! [`crate::runtime`] that names the category literally, [`TITLE_DUTY`] with its
//! [`TITLE_OUTPUT_MAX_BYTES`] ceiling, and the prompt builder below with its
//! [`TITLE_OUTPUT_CONTRACT`]. Routing, egress scoping, ceiling enforcement,
//! `route_decided` emission and the failure shape are the seam's, once, for all
//! five duties.
//!
//! (On writing about that resolver: describe it, never spell it. The
//! `declared, no call site yet` marker in [`crate::call_sites`] is derived by
//! scanning the daemon's source as **text**, so the spelling of a
//! category-resolving router call inside a doc comment registers as a call site
//! and turns the derived-marker test red. ADR-9, learned the hard way in
//! TASK-058.)

use teton_protocol::Category;

use crate::egress::Provenance;

use super::context::truncate_middle;
use super::duty::{DutyKind, DutyRoute};

/// How many words [`TITLE_OUTPUT_CONTRACT`] asks a title to be, at most.
///
/// Kept as a number so the ceiling below can be *derived* from the promise the
/// prompt makes rather than picked beside it — two numbers describing one budget
/// are two numbers that can drift.
const TITLE_CONTRACT_WORDS: usize = 8;

/// Generous bytes per word, for sizing the ceiling.
///
/// Generous on purpose: a title may be multi-byte (a non-English session name is
/// an ordinary session name), and the ceiling is a *safety* bound on an
/// untrusted stream, not a style rule. The contract is what asks for brevity;
/// this only stops an unbounded answer.
const TITLE_BYTES_PER_WORD: usize = 16;

/// Byte ceiling on what a `title` duty may return (BR-8).
///
/// The **tightest of the five**, by an order of magnitude, and deliberately so:
/// a session title is a handful of words rendered into a list, so anything that
/// could not fit on one line of a session picker is not a title but an essay
/// with a title's job. `digest` and `shell` are sized for prose about kilobytes
/// of input; this one is sized for the thing it produces — which is why it is
/// computed from [`TITLE_CONTRACT_WORDS`] rather than written down as a number
/// that happens to look about right.
///
/// Enforced in the duty implementation rather than requested of the provider
/// (LESSON-484): `max_tokens` is a request, and a request is not a bound.
pub const TITLE_OUTPUT_MAX_BYTES: usize = TITLE_CONTRACT_WORDS * TITLE_BYTES_PER_WORD;

/// The `title` duty on the shared seam: its category and its output ceiling.
///
/// One `const` per category, stated once and read by every construction site —
/// the resolver in [`crate::runtime`] and the tests.
pub const TITLE_DUTY: DutyKind = DutyKind::new(Category::Title, TITLE_OUTPUT_MAX_BYTES);

/// The title duty's output contract, verbatim: the last sentence of the
/// instruction, before the request it embeds.
///
/// Exported because it is also how the CI/offline stand-in engine
/// ([`crate::runtime::ScriptedFileEngine`]) recognizes a `title` duty and answers
/// it *without consuming a scripted turn* — **a duty is not a turn** (BR-10). One
/// constant, used both to write the sentence and to recognize it, so the seam
/// cannot drift out of step with the prompt. A duty with no recognition arm eats
/// a scripted block and shifts every fixture's turn sequence by one, which
/// REQ-558 shipped twice before it was caught — and `title` is the duty that
/// would do it to *every* scripted session, because it fires on the first turn of
/// each one rather than on some particular tool result.
///
/// A full, distinctive sentence rather than a short phrase, for the reason
/// [`SUMMARIZER_OUTPUT_CONTRACT`](super::context::SUMMARIZER_OUTPUT_CONTRACT) is
/// one: the recognizer sees the *whole* rendered prompt, and this duty's prompt
/// embeds the user's own words — so a generic phrase could plausibly arrive
/// inside the very request being named.
pub const TITLE_OUTPUT_CONTRACT: &str = "Reply with the title alone, on one line, in at most \
     eight words — no quotation marks, no trailing period, and no explanation.";

/// Fewest bytes of request worth spending a model call to name a session after
/// (ADR-11).
///
/// `"hi"`, `"ok"`, `"continue"` and `"go on"` are the openings of a great many
/// real sessions and they name nothing: a title derived from one of them is
/// noise that the BR-9 guard then makes **permanent**, because a session is
/// never renamed. Below this the duty declines, the session's one attempt is
/// **not** spent, and the next turn that says something substantive gets it
/// instead.
///
/// Stated as a constant with a zero-call test rather than as an inline literal,
/// because a hidden threshold is a cost surprise in one direction and a quality
/// surprise in the other (ADR-11). It is disclosed rather than smuggled: it is
/// this task's addition, not the spec's.
pub const TITLE_MIN_REQUEST_BYTES: usize = 16;

/// Display bound on the embedded request in a duty prompt, in bytes.
///
/// The classifier's bound, for the classifier's reason: what a session is *about*
/// lives in the opening and closing sentences of the first request, so head +
/// tail ([`truncate_middle`]) is the shape that keeps it, and a pasted stack
/// trace cannot make this prompt unbounded.
const TITLE_REQUEST_MAX_BYTES: usize = 2_048;

/// Whether a session's first substantive request is worth naming (ADR-11).
///
/// Trims before measuring, so the threshold is about how much was *said* rather
/// than how much whitespace was sent — and so no call site can forget to trim
/// and quietly get a different answer than the tests do.
///
/// The negative case is the cost argument: a session opened with `"hi"` pays
/// nothing, and stays eligible for the turn that actually asks for something.
#[must_use]
pub fn worth_titling(request: &str) -> bool {
    request.trim().len() >= TITLE_MIN_REQUEST_BYTES
}

/// The duty prompt: what to produce, and the request to produce it from.
///
/// The request comes **last** so that the instruction cannot be pushed out of a
/// small model's attention by a long paste — and so that a request containing
/// something instruction-shaped is read as the material rather than as the task.
#[must_use]
pub fn title_prompt(request: &str) -> String {
    let mut prompt = String::new();
    prompt.push_str(
        "Below is the first thing someone asked an AI coding agent to do in a new session. \
         Give the session a short name after it — the label a person would use to pick this \
         session out of a list of sessions a week from now. ",
    );
    prompt.push_str(TITLE_OUTPUT_CONTRACT);
    prompt.push_str("\n\nRequest: ");
    prompt.push_str(&truncate_middle(request, TITLE_REQUEST_MAX_BYTES));
    prompt
}

/// Name a session after `request`, through the resolved `title` route.
///
/// Returns **only the title**, not a stored session: the caller owns the session
/// record and the once-only guard on it, so a failure here costs the name and
/// nothing else (BR-3). A session with no title is the state every session was in
/// before this REQ.
///
/// `provenance` is the egress provenance of the request text — the content this
/// duty is about to send. Computing it is the call site's job, because the call
/// site is what knows where its content came from (LESSON-432).
///
/// # Errors
/// The route's own sentence when it resolved to nothing or the duty failed, or a
/// sentence naming an answer with no title in it. Every one of them leaves the
/// caller holding an explanation rather than a silent absence (LESSON-447).
pub async fn name_session(
    route: &DutyRoute,
    request: &str,
    provenance: &Provenance,
) -> Result<String, String> {
    // Taken before the prompt is built, not after: an unresolvable route has
    // nothing to send, so rendering the request into a prompt no model will ever
    // see is work done for a call that cannot happen.
    if let DutyRoute::Unresolved { reason } = route {
        return Err(reason.clone());
    }
    let prompt = title_prompt(request);
    let answer = route.perform(&prompt, provenance).await?;
    let title = read_title(&answer);
    if title.is_empty() {
        // Never "name it the empty string", which a client renders as a session
        // with a blank name rather than as a session with none — an outcome
        // worse than the absence it is standing in for (LESSON-447).
        return Err("the `title` duty answered with nothing to name the session".to_owned());
    }
    Ok(title)
}

/// A title read out of `answer`: one line, unquoted, no trailing period.
///
/// Total by construction. A duty is a third party — a small local model — so "the
/// answer is a bare title" is a hope, not an invariant, and the three things a
/// model reliably adds anyway are a wrapping pair of quotes, a full stop, and a
/// second line of explanation it was asked not to write. Each is stripped here
/// rather than only asked for in the contract, because the result is written into
/// a session record that is never revised.
///
/// Collapsing to one line is the load-bearing half: the title is rendered inline
/// in a session list, and an embedded newline there is a rendering bug in every
/// client at once. Bounded already by the seam's ceiling; this is about shape,
/// not size.
fn read_title(answer: &str) -> String {
    answer
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|c| c == '"' || c == '\'' || c == '`')
        .trim_end_matches('.')
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use futures::stream;
    use teton_core::entities::{BoundaryMode, PrivacyBoundary};
    use teton_inference::{Engine, MockEngine};
    use teton_providers::transport::{
        HttpMethod, TransportError, TransportRequest, TransportResponse,
    };
    use teton_providers::{
        CapabilityProfile, Provider, ProviderError, StopReason, TokenUsage, Transport,
        TurnCompletion, TurnEvent, TurnRequest, TurnStream,
    };

    use crate::egress::{Egress, NoopSink};

    const REQUEST: &str = "Add retry-with-backoff to the download client and cover it with tests.";

    // -- the decline threshold (ADR-11) --------------------------------------

    /// **The threshold, stated once**, both arms — and the load-bearing half is
    /// the one that costs nothing: an opener too short to name a session by.
    #[test]
    fn only_a_substantive_first_request_is_worth_titling() {
        for nothing in ["", "   ", "hi", "ok", "continue", "go on", "\n\t "] {
            assert!(
                !worth_titling(nothing),
                "{nothing:?} must not buy a model call"
            );
        }
        assert!(worth_titling(REQUEST));
        // Exactly at the bound, and one byte under it — so the comparison cannot
        // silently drift by one without this failing.
        assert!(worth_titling(&"x".repeat(TITLE_MIN_REQUEST_BYTES)));
        assert!(!worth_titling(&"x".repeat(TITLE_MIN_REQUEST_BYTES - 1)));
    }

    /// The threshold measures what was *said*, not what was sent: padding a short
    /// opener with whitespace must not buy it a title.
    #[test]
    fn whitespace_does_not_make_a_short_request_worth_titling() {
        let padded = format!("{}hi{}", " ".repeat(64), " ".repeat(64));
        assert!(padded.len() > TITLE_MIN_REQUEST_BYTES);
        assert!(!worth_titling(&padded));
    }

    // -- the prompt ----------------------------------------------------------

    /// The output contract is the *whole* of the sentence the stand-in engine
    /// recognizes, and it really is in the prompt.
    ///
    /// Written out here rather than reused from the constant for the reason
    /// `triage`'s and `shell`'s equivalents are: changing it must be a deliberate
    /// two-place edit rather than something that silently desynchronizes
    /// `ScriptedFileEngine` from the duty it is meant to answer off-script
    /// (BR-10).
    #[test]
    fn the_duty_prompt_carries_the_output_contract_verbatim() {
        assert_eq!(
            TITLE_OUTPUT_CONTRACT,
            "Reply with the title alone, on one line, in at most eight words — no quotation \
             marks, no trailing period, and no explanation."
        );
        assert!(title_prompt(REQUEST).contains(TITLE_OUTPUT_CONTRACT));
    }

    /// A pasted stack trace cannot make the prompt unbounded, and the request is
    /// bounded in **bytes** — the unit the wire is measured in.
    #[test]
    fn an_enormous_request_is_bounded() {
        let huge = "é".repeat(50_000);
        let prompt = title_prompt(&huge);
        assert!(
            prompt.len() < TITLE_REQUEST_MAX_BYTES + 1_024,
            "a 100 KB request produced a {} byte prompt",
            prompt.len()
        );
        assert!(prompt.contains(TITLE_OUTPUT_CONTRACT));
    }

    // -- the route legs ------------------------------------------------------

    fn local_route(reply: &str) -> DutyRoute {
        let engine: Arc<Mutex<dyn Engine>> =
            Arc::new(Mutex::new(MockEngine::with_response("mock", reply)));
        DutyRoute::local(TITLE_DUTY, "local", engine)
    }

    fn user_text() -> Provenance {
        Provenance::empty()
    }

    /// An unresolvable route is a routing failure carrying the resolver's own
    /// sentence, and it never reaches a model.
    #[tokio::test]
    async fn an_unresolved_route_returns_the_resolvers_reason() {
        let route = DutyRoute::unresolved(
            "The 'title' category resolves to 'local', but no local engine is loaded to serve \
             it yet.",
        );
        let err = name_session(&route, REQUEST, &user_text())
            .await
            .expect_err("an unresolved route cannot name a session");
        assert!(err.contains("no local engine is loaded"), "{err}");
    }

    /// A local route really does name the session, and the name is the duty's.
    #[tokio::test]
    async fn a_local_route_names_the_session() {
        let route = local_route("Retry with backoff in the download client");
        let named = name_session(&route, REQUEST, &user_text())
            .await
            .expect("the local duty names the session");
        assert_eq!(named, "Retry with backoff in the download client");
    }

    /// An empty answer is a failure, not a session named the empty string
    /// (LESSON-447). A blank name renders worse than no name at all.
    #[tokio::test]
    async fn an_empty_answer_is_a_failure_not_a_blank_title() {
        for empty in ["", "   ", "\n\n", "\"\"", "."] {
            let err = name_session(&local_route(empty), REQUEST, &user_text())
                .await
                .expect_err("an empty answer must not pass as a title");
            assert!(err.contains("nothing to name the session"), "{err}");
        }
    }

    /// The three things a small model adds despite being asked not to — quotes, a
    /// full stop, and a second line — are stripped, because the result is written
    /// into a record that is never revised.
    #[tokio::test]
    async fn a_chatty_answer_is_read_back_as_a_bare_one_line_title() {
        for (answer, expected) in [
            ("\"Retry the download client\"", "Retry the download client"),
            ("Retry the download client.", "Retry the download client"),
            (
                "Retry the download client\nHope that helps!",
                "Retry the download client Hope that helps!",
            ),
            (
                "  `Retry the download client`  ",
                "Retry the download client",
            ),
        ] {
            let named = name_session(&local_route(answer), REQUEST, &user_text())
                .await
                .expect("the duty answers");
            assert_eq!(named, expected);
            assert!(!named.contains('\n'), "{named}");
        }
    }

    // -- the local leg: the harness-owned ceiling (BR-8, AC-11) --------------

    /// **AC-11, BR-8, on the leg `title` actually runs on.** An engine that runs
    /// on cannot grow the title duty's answer without limit either.
    ///
    /// This is the sharper of the two ceiling tests and it was the missing one.
    /// `title` is `reflex`-tier and REQ-558 settled that `reflex` never inherits
    /// `default_provider`, so on every ordinary configuration it has **no remote
    /// leg at all** — a bound enforced only where the provider streams is a
    /// bound on none of this duty's production paths. And the answer is written
    /// into a session record that is never revised (BR-9) and announced on the
    /// wire as `session_titled`, so an unbounded one is permanent.
    ///
    /// The fixture is one unbroken run of bytes on purpose: a token budget is a
    /// request measured in *tokens*, and a single token carries as many bytes as
    /// the model felt like putting in it. That is why the ceiling is stated in
    /// bytes and enforced after generation rather than asked for before it.
    #[tokio::test]
    async fn a_local_title_duty_is_bounded_however_much_the_engine_generates() {
        let runaway = "e".repeat(TITLE_OUTPUT_MAX_BYTES * 32);
        let route = local_route(&runaway);
        let DutyRoute::Serves { duty, .. } = &route else {
            panic!("the fixture must resolve, or the ceiling is never exercised");
        };

        let answer = duty
            .perform("name this", &user_text())
            .await
            .expect("the local duty served");

        assert!(
            answer.len() <= TITLE_OUTPUT_MAX_BYTES,
            "a local title duty accepted {} bytes from an engine that ran on; the \
             ceiling is {TITLE_OUTPUT_MAX_BYTES}",
            answer.len()
        );
        // Non-vacuity, both directions: the engine really did offer far more than
        // the bound, and the duty really did accumulate up to it — so this is a
        // cap and not a refusal.
        assert!(runaway.len() > TITLE_OUTPUT_MAX_BYTES * 4);
        assert!(
            answer.len() > TITLE_OUTPUT_MAX_BYTES / 2,
            "the cap must let a real title through: {} bytes",
            answer.len()
        );
    }

    /// And the same bound reached through the duty's own call site, because that
    /// is where an unbounded title would actually land.
    #[tokio::test]
    async fn a_runaway_local_answer_still_names_the_session_within_the_ceiling() {
        let named = name_session(
            &local_route(&"unbounded ".repeat(TITLE_OUTPUT_MAX_BYTES)),
            REQUEST,
            &user_text(),
        )
        .await
        .expect("a bounded answer is still an answer");
        assert!(
            named.len() <= TITLE_OUTPUT_MAX_BYTES,
            "a session was named with {} bytes: {named}",
            named.len()
        );
    }

    // -- the remote leg: the harness-owned ceiling (BR-8, AC-11) -------------

    /// A transport that records every request body it is asked to send.
    #[derive(Default, Clone)]
    struct CaptureTransport {
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    #[async_trait]
    impl Transport for CaptureTransport {
        async fn execute(
            &self,
            request: TransportRequest,
        ) -> Result<TransportResponse, TransportError> {
            self.sent
                .lock()
                .expect("capture poisoned")
                .push(request.body);
            Ok(TransportResponse {
                status: 200,
                body: Box::pin(stream::empty()),
            })
        }
    }

    /// A provider that puts its request on the wire before answering, and can be
    /// told to stream its reply `repeat` times — i.e. to ignore `max_tokens`.
    struct WireProvider {
        reply: String,
        repeat: usize,
    }

    #[async_trait]
    impl Provider for WireProvider {
        fn id(&self) -> &str {
            "wire"
        }
        fn capabilities(&self) -> CapabilityProfile {
            CapabilityProfile::default()
        }
        async fn stream_turn(
            &self,
            request: TurnRequest,
            transport: &dyn Transport,
        ) -> Result<TurnStream, ProviderError> {
            let body =
                serde_json::to_vec(&request).map_err(|e| ProviderError::Build(e.to_string()))?;
            transport
                .execute(TransportRequest {
                    method: HttpMethod::Post,
                    url: "https://api.example.com/v1/chat/completions".to_owned(),
                    headers: Vec::new(),
                    body,
                })
                .await
                .map_err(|err| match err {
                    TransportError::PrivacyBlocked => ProviderError::PrivacyBlocked,
                    _ => ProviderError::Transport,
                })?;
            let mut events: Vec<Result<TurnEvent, ProviderError>> = (0..self.repeat)
                .map(|_| Ok(TurnEvent::TextDelta(self.reply.clone())))
                .collect();
            events.push(Ok(TurnEvent::Completed(TurnCompletion {
                usage: TokenUsage::default(),
                stop_reason: StopReason::EndTurn,
            })));
            Ok(Box::pin(stream::iter(events)))
        }
    }

    /// A remote `title` route over a capturing transport, with `boundaries`
    /// enforced at the choke point.
    ///
    /// `title` resolves local on every ordinary config, so this fixture exists to
    /// exercise the *seam's* remote leg for the ceiling — the bound must hold on
    /// the path where an unbounded stream is even possible.
    fn remote_route(
        boundaries: Vec<PrivacyBoundary>,
        reply: &str,
        repeat: usize,
    ) -> (DutyRoute, Arc<Mutex<Vec<Vec<u8>>>>) {
        let transport = CaptureTransport::default();
        let sent = Arc::clone(&transport.sent);
        let egress = Egress::new(transport, boundaries, Arc::new(NoopSink));
        let route = DutyRoute::remote(
            TITLE_DUTY,
            "frontier",
            Box::new(WireProvider {
                reply: reply.to_owned(),
                repeat,
            }),
            egress,
            "claude-opus-4",
            "sess-1",
        );
        (route, sent)
    }

    fn wire(sent: &Arc<Mutex<Vec<Vec<u8>>>>) -> String {
        sent.lock()
            .expect("capture poisoned")
            .iter()
            .map(|body| String::from_utf8_lossy(body).into_owned())
            .collect()
    }

    /// **AC-11, BR-8.** A provider ignoring `max_tokens` cannot grow the title
    /// duty's buffer without limit. The assertion reads
    /// [`TITLE_OUTPUT_MAX_BYTES`] rather than a literal, so raising the ceiling
    /// cannot silently un-test the bound.
    #[tokio::test]
    async fn a_remote_title_duty_is_bounded_however_much_the_provider_streams() {
        // 1 KiB per delta × 64 deltas = 64 KiB offered, 128 bytes accepted.
        let chunk = format!("{} ", "e".repeat(1023));
        let (route, _sent) = remote_route(Vec::new(), &chunk, 64);
        let DutyRoute::Serves { duty, .. } = &route else {
            panic!("the fixture must resolve, or the ceiling is never exercised");
        };

        let answer = duty
            .perform("name this", &Provenance::empty())
            .await
            .expect("the remote duty served");

        assert!(
            answer.len() <= TITLE_OUTPUT_MAX_BYTES,
            "a title duty accepted {} bytes from a provider ignoring its token budget; the \
             ceiling is {TITLE_OUTPUT_MAX_BYTES}",
            answer.len()
        );
        // Non-vacuity, both directions: the provider really did offer far more
        // than the bound, and the duty really did accumulate up to it — so this
        // is a cap and not a refusal.
        assert!(
            chunk.len() * 64 > TITLE_OUTPUT_MAX_BYTES * 4,
            "the fixture must offer well over the bound"
        );
        assert!(
            answer.len() > TITLE_OUTPUT_MAX_BYTES / 2,
            "the cap must let a real title through: {} bytes",
            answer.len()
        );
    }

    /// The tightest of the five ceilings, and the one derived from its own
    /// contract rather than picked — a title that could hold a `shell`
    /// interpretation is not a title.
    ///
    /// The neighbour comparisons are compile-time because the relationship
    /// between two constants is a compile-time fact: raising this ceiling past a
    /// neighbour's stops the build rather than waiting for a test run. What they
    /// cannot catch is a change *within* the range — that is what tying the
    /// number to [`TITLE_CONTRACT_WORDS`] is for, since enlarging it there means
    /// writing down a claim about words that a reviewer can read.
    #[test]
    fn the_title_ceiling_is_derived_from_its_contract_and_is_the_tightest() {
        const {
            assert!(TITLE_OUTPUT_MAX_BYTES < crate::harness::shell_duty::SHELL_OUTPUT_MAX_BYTES);
            assert!(TITLE_OUTPUT_MAX_BYTES < crate::harness::triage::TRIAGE_OUTPUT_MAX_BYTES);
        }
        assert_eq!(
            TITLE_OUTPUT_MAX_BYTES,
            TITLE_CONTRACT_WORDS * TITLE_BYTES_PER_WORD,
            "the ceiling is the contract's word budget, not a number beside it"
        );
        assert!(
            TITLE_OUTPUT_CONTRACT.contains("eight words"),
            "the contract must ask for the word budget the ceiling is sized from"
        );
        assert_eq!(TITLE_DUTY.ceiling_bytes(), TITLE_OUTPUT_MAX_BYTES);
        assert_eq!(TITLE_DUTY.category(), Category::Title);
    }

    // -- the boundary interaction (BR-7) ------------------------------------

    /// **The non-vacuity half** (LESSON-485). The same remote route with **no**
    /// boundary configured genuinely sends: the prompt reaches the transport and
    /// the provider's title comes back. Without it, the refusal below would be
    /// equally satisfied by a fixture that could never send anything at all.
    #[tokio::test]
    async fn an_unbounded_machine_sends_the_title_prompt_and_returns_its_answer() {
        let (route, sent) = remote_route(Vec::new(), "Retry the download client", 1);
        let named = name_session(&route, REQUEST, &user_text())
            .await
            .expect("with no boundary configured the duty sends");
        assert_eq!(named, "Retry the download client");
        assert!(
            wire(&sent).contains("Give the session a short name"),
            "the duty prompt never reached the transport"
        );
        // And it asked for **this duty's** budget, not a default shared with the
        // four duties whose ceilings are an order of magnitude looser.
        assert!(
            wire(&sent).contains(&format!("\"max_tokens\":{}", TITLE_DUTY.max_tokens())),
            "the request carried a generation budget that is not this duty's: {}",
            wire(&sent)
        );
        assert!(
            TITLE_DUTY.max_tokens() < crate::harness::COMPACT_DUTY.max_tokens(),
            "non-vacuity: the budget really is per-duty, so this is not one number \
             spelled two ways"
        );
    }

    /// **BR-7 through the one choke point.** The request text a session is named
    /// from is scoped by the provenance the call site computed for it, like every
    /// other duty — so a request carrying `local-only` material is refused before
    /// a byte leaves and the session simply stays unnamed.
    ///
    /// Only the *provenance* differs from the test above; the boundaries and the
    /// route are otherwise identical, which is what makes this the guard holding
    /// rather than a fixture that could not send.
    #[tokio::test]
    async fn a_title_over_boundary_content_is_refused_before_a_byte_leaves() {
        let boundaries = vec![PrivacyBoundary {
            path_glob: "secrets/**".to_owned(),
            mode: BoundaryMode::LocalOnly,
        }];
        let (route, sent) = remote_route(boundaries, "Rotate the production key", 1);
        let secret_bearing = "Rotate the key in secrets/prod.env — it is sk-live-DO-NOT-LEAK.";

        let err = name_session(
            &route,
            secret_bearing,
            &Provenance::tainted_by("secrets/prod.env"),
        )
        .await
        .expect_err("boundary content must not be titled remotely");

        assert!(
            wire(&sent).is_empty(),
            "boundary-derived request text reached the transport"
        );
        assert!(!wire(&sent).contains("sk-live-DO-NOT-LEAK"));
        assert!(err.contains("privacy boundary"), "{err}");
    }
}
