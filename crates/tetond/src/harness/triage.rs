//! The `triage` duty: ranking a `grep` result before it enters context
//! (REQ-561 BR-1/BR-3/BR-7/BR-8, TASK-060).
//!
//! ## What it does
//!
//! [`GrepTool::run`](super::tools::GrepTool) walks the repo, collects every
//! matching line, and hands back the **first 200 in file order** — an ordering
//! that has nothing to do with what the user asked for. On a weak model those
//! 200 lines are most of the context window, and the one match that mattered is
//! as likely to be 187th as 2nd. The duty reads the same 200 lines against the
//! turn's request and answers with the order they should be read in, dropping
//! the ones that cannot help.
//!
//! ## What is `triage`-specific, and what is not
//!
//! Everything routable lives on the shared [`duty`](super::duty) seam. What is
//! genuinely this category's is exactly the three things ADR-3 allows: the
//! one-line resolver in [`crate::runtime`] that names the category literally,
//! [`TRIAGE_DUTY`] with its [`TRIAGE_OUTPUT_MAX_BYTES`] ceiling, and the prompt
//! builder below with its [`TRIAGE_OUTPUT_CONTRACT`].
//!
//! (On writing about that resolver: describe it, never spell it. The
//! `declared, no call site yet` marker in [`crate::call_sites`] is derived by
//! scanning the daemon's source as **text**, so the spelling of a
//! category-resolving router call inside a doc comment registers as a call site
//! and turns the derived-marker test red. ADR-9, learned the hard way in
//! TASK-058.)
//!
//! ## The answer is numbers, not text (and that is a safety property)
//!
//! The duty is asked for **match numbers**, and [`rank_matches`] resolves them
//! against the list it offered. A number the duty invents out of range is
//! discarded and a line it invents wholesale has nowhere to land, so the ranked
//! result is always a subset — in some order — of what `grep` actually found.
//! Asking for the lines back verbatim would make a confused (or prompt-injected)
//! model able to write *new* "matches" into context, naming files and line
//! numbers that do not exist. Numbers cannot do that, and they cost a few
//! hundred bytes instead of tens of kilobytes, which is why the ceiling here is
//! two orders of magnitude below `digest`'s.
//!
//! Duplicates are dropped as they are read, so the ranking can never be longer
//! than the list it ranks: a duty answering `1 1 1 …` five hundred times cannot
//! inflate a 200-match result past 200.
//!
//! ## Failure is today's behaviour, verbatim (BR-3, LESSON-447)
//!
//! The invariant this call site guards is "the model sees the matches". Ranking
//! is an improvement on the *order*, never a precondition for having them — so
//! every failure (unresolvable route, provider error, refusal at the egress
//! choke point, an answer that is not a ranking) returns the tool's own
//! `ToolOutcome` **unchanged**, and reports why on
//! [`RefinedOutcome::duty_error`](super::tools::RefinedOutcome). That is the
//! whole reason [`rank_matches`] returns an ordering rather than a rebuilt
//! string: the fallback is not a code path anyone has to write correctly, it is
//! the value the caller already had.

use teton_protocol::Category;

use crate::egress::Provenance;

use super::context::{floor_char_boundary, truncate_middle};
use super::duty::{DutyKind, DutyRoute};

/// Byte ceiling on what a `triage` duty may return (BR-8).
///
/// A ranking of the tool's own 200-match cap is at most ~700 bytes of decimal
/// numbers and separators, so 2 KiB is roomy for every legitimate answer and
/// still three orders of magnitude below what an unbounded stream could push at
/// the harness. Enforced in the duty implementation rather than requested of the
/// provider (LESSON-484); `max_tokens` is a request, and a request is not a
/// bound.
pub const TRIAGE_OUTPUT_MAX_BYTES: usize = 2_048;

/// The `triage` duty on the shared seam: its category and its output ceiling.
///
/// One `const` per category, stated once and read by every construction site —
/// the resolver in [`crate::runtime`], the transport-free offline entry point in
/// [`super::turn_loop`], and the tests.
pub const TRIAGE_DUTY: DutyKind = DutyKind::new(Category::Triage, TRIAGE_OUTPUT_MAX_BYTES);

/// The triage duty's output contract, verbatim: the last sentence of the
/// instruction, before the numbered matches it embeds.
///
/// Exported because it is also how the CI/offline stand-in engine
/// ([`crate::runtime::ScriptedFileEngine`]) recognizes a `triage` duty and
/// answers it *without consuming a scripted turn* — **a duty is not a turn**
/// (BR-10). One constant, used both to write the sentence and to recognize it,
/// so the seam cannot drift out of step with the prompt. A duty with no
/// recognition arm eats a scripted block and shifts every fixture's turn
/// sequence by one, which REQ-558 shipped twice before it was caught.
///
/// A full, distinctive sentence rather than a short phrase, for the reason
/// [`SUMMARIZER_OUTPUT_CONTRACT`](super::context::SUMMARIZER_OUTPUT_CONTRACT)
/// is one: the recognizer sees the *whole* rendered prompt, and a generic phrase
/// could plausibly appear inside a tool result riding an ordinary turn.
pub const TRIAGE_OUTPUT_CONTRACT: &str = "Output only the match numbers, most useful first, \
     separated by spaces — no prose, no commentary.";

/// Fewest matches worth spending a model call on.
///
/// Below this there is no ordering to choose: one match is already first, and
/// zero matches are not a list. Stated as a constant so the "when does this
/// cost anything" question has one answer a reviewer can read (BR-4b's shape,
/// applied to the cheaper duty).
pub const TRIAGE_MIN_MATCHES: usize = 2;

/// Display bound on one offered match line, in bytes.
///
/// A `grep` hit is a whole source line, and a minified or generated file has
/// lines in the tens of kilobytes. The prompt is bounded by construction —
/// `matches.len() × (TRIAGE_MATCH_MAX_BYTES + a small prefix)` — so no repository
/// can make this duty's prompt unbounded. Only the *display* is bounded: the
/// ranking answers in numbers, and a number resolves to the whole original line.
const TRIAGE_MATCH_MAX_BYTES: usize = 160;

/// Display bound on the request and the search description, in bytes.
const TRIAGE_REQUEST_MAX_BYTES: usize = 1_024;

/// The header that opens the numbered match list in a triage prompt.
///
/// Written once and read once ([`offered_match_count`]), for the same reason the
/// output contract is: two places describing one prompt shape must not be able
/// to drift.
const MATCH_LIST_HEADER: &str = "\nMatches:\n";

/// Whether a `grep` result of `matches` lines is worth a `triage` model call.
///
/// The negative case is the cost argument: a session that greps for a unique
/// symbol pays nothing.
#[must_use]
pub const fn worth_ranking(matches: usize) -> bool {
    matches >= TRIAGE_MIN_MATCHES
}

/// The duty prompt: the request, the search that produced the matches, and the
/// matches themselves, numbered from 1.
///
/// The matches come **last** so that [`offered_match_count`] can find the list
/// by its header without a request that happens to contain numbered lines
/// confusing it.
#[must_use]
pub fn triage_prompt(request: &str, search: &str, matches: &[&str]) -> String {
    let mut prompt = String::new();
    prompt.push_str(
        "Below is a numbered list of `grep` matches from one repository. Rank them by how \
         likely each one is to help with the request below, most useful first, and leave out \
         the ones that cannot help. ",
    );
    prompt.push_str(TRIAGE_OUTPUT_CONTRACT);
    prompt.push_str("\n\nRequest: ");
    prompt.push_str(&truncate_middle(request, TRIAGE_REQUEST_MAX_BYTES));
    prompt.push_str("\nSearch: ");
    prompt.push_str(&truncate_middle(search, TRIAGE_REQUEST_MAX_BYTES));
    prompt.push_str(MATCH_LIST_HEADER);
    for (i, line) in matches.iter().enumerate() {
        prompt.push_str(&format!("{}. {}\n", i + 1, one_line(line)));
    }
    prompt
}

/// How many matches a triage prompt offered.
///
/// The stand-in engine's way to answer a `triage` duty with the *identity*
/// ranking (BR-10): it cannot judge relevance, and inventing an order would
/// silently reorder every fixture's `grep` output, but it can keep every match
/// in the order it was offered. Reading the count off the prompt is what lets it
/// do that without the fixture author having to declare it.
///
/// Reads the list the prompt actually carries — after rendering, and after the
/// last header — so a prompt edit that changed the numbering would show up here
/// rather than silently producing an unusable answer.
#[must_use]
pub fn offered_match_count(prompt: &str) -> usize {
    let Some(at) = prompt.rfind(MATCH_LIST_HEADER) else {
        return 0;
    };
    let mut offered = 0usize;
    for line in prompt[at + MATCH_LIST_HEADER.len()..].lines() {
        if line.starts_with(&format!("{}. ", offered + 1)) {
            offered += 1;
        }
    }
    offered
}

/// Rank `matches` through the resolved `triage` route, most useful first.
///
/// Returns **indices into `matches`**, not text: the caller keeps ownership of
/// what the model finally sees, so a failure here costs the ordering and nothing
/// else (BR-3).
///
/// `provenance` is the egress provenance of the *matched files* — the content
/// this duty is about to send — so a remote binding is refused at the choke
/// point when a match came out of a `local-only` file, while the rest of the
/// turn proceeds (BR-7, ADR-2). Computing it is the call site's job, because the
/// call site is what knows where its content came from (LESSON-432).
///
/// # Errors
/// The route's own sentence when it resolved to nothing or the duty failed, or a
/// sentence naming an answer that could not be read as a ranking. Every one of
/// them leaves the caller holding an explanation it can degrade with.
pub async fn rank_matches(
    route: &DutyRoute,
    request: &str,
    search: &str,
    matches: &[&str],
    provenance: &Provenance,
) -> Result<Vec<usize>, String> {
    // Taken before the prompt is built, not after: an unresolvable route has
    // nothing to send, so rendering tens of kilobytes of matches into a prompt no
    // model will ever see is work done for a call that cannot happen.
    if let DutyRoute::Unresolved { reason } = route {
        return Err(reason.clone());
    }
    let prompt = triage_prompt(request, search, matches);
    let answer = route.perform(&prompt, provenance).await?;
    let order = read_ranking(&answer, matches.len());
    if order.is_empty() {
        // Not a ranking. Never "keep everything in the order the duty implied",
        // which is indistinguishable from the duty having run — the caller must
        // be able to report that ranking did not happen (LESSON-447).
        return Err("the `triage` duty answered with no usable match numbers".to_owned());
    }
    Ok(order)
}

/// Read a ranking out of `answer`: every in-range match number, first mention
/// wins, everything else ignored.
///
/// Total by construction. A duty is a third party — a small local model, or a
/// remote provider — so "the answer is a clean list of numbers" is a hope, not
/// an invariant. Prose around the numbers is fine, a number out of range is
/// dropped rather than clamped (clamping would silently promote a match the duty
/// never chose), and a repeated number is taken once so the ranking cannot grow
/// past the list it ranks.
fn read_ranking(answer: &str, offered: usize) -> Vec<usize> {
    let mut order = Vec::new();
    let mut taken = vec![false; offered];
    for token in answer.split(|c: char| !c.is_ascii_digit()) {
        // A non-numeric run splits to `""`, and an absurdly long digit run
        // overflows — both parse to `Err` and are skipped rather than special-cased.
        let Ok(n) = token.parse::<usize>() else {
            continue;
        };
        if n == 0 || n > offered || taken[n - 1] {
            continue;
        }
        taken[n - 1] = true;
        order.push(n - 1);
    }
    order
}

/// `text` bounded to [`TRIAGE_MATCH_MAX_BYTES`] and guaranteed to stay one line.
///
/// Deliberately **not**
/// [`truncate_middle`](super::context::truncate_middle): its elision marker
/// carries newlines, which would split one match across two numbered entries and
/// desynchronize every number after it from the line it names.
fn one_line(text: &str) -> String {
    const ELLIPSIS: &str = " […]";
    if text.len() <= TRIAGE_MATCH_MAX_BYTES {
        return text.to_owned();
    }
    let keep = TRIAGE_MATCH_MAX_BYTES.saturating_sub(ELLIPSIS.len());
    format!("{}{ELLIPSIS}", &text[..floor_char_boundary(text, keep)])
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

    fn matches() -> Vec<&'static str> {
        vec![
            "src/a.rs:12: fn parse(input: &str)",
            "src/b.rs:40: // parse the header",
            "src/c.rs:7: let parse = 1;",
        ]
    }

    /// The output contract is the *whole* of the sentence the stand-in engine
    /// recognizes, and it really is in the prompt.
    ///
    /// Written out here rather than reused from the constant for the same reason
    /// `the_duty_prompt_carries_the_output_contract_verbatim` spells the
    /// summarizer's out: changing it must be a deliberate two-place edit rather
    /// than something that silently desynchronizes `ScriptedFileEngine` from the
    /// duty it is meant to answer off-script (BR-10).
    #[test]
    fn the_duty_prompt_carries_the_output_contract_verbatim() {
        assert_eq!(
            TRIAGE_OUTPUT_CONTRACT,
            "Output only the match numbers, most useful first, separated by spaces — no prose, \
             no commentary."
        );
        assert!(triage_prompt("find the parser", "grep `parse`", &matches())
            .contains(TRIAGE_OUTPUT_CONTRACT));
    }

    /// The stand-in engine reads the offered count straight off the prompt, so
    /// the numbering and the reader are pinned against each other.
    #[test]
    fn the_offered_matches_are_counted_off_the_prompt() {
        let prompt = triage_prompt("find the parser", "grep `parse`", &matches());
        assert_eq!(offered_match_count(&prompt), 3);
        // And a prompt that offers nothing counts nothing rather than panicking.
        assert_eq!(offered_match_count("no matches here"), 0);
    }

    /// A request full of numbered lines must not be mistaken for the match list.
    /// The header is what locates it, and the matches come last.
    #[test]
    fn a_numbered_request_does_not_inflate_the_offered_count() {
        let request = "Do this:\n1. read the file\n2. fix it\n3. verify\n4. ship";
        let prompt = triage_prompt(request, "grep `parse`", &matches());
        assert_eq!(offered_match_count(&prompt), 3);
    }

    /// A pathological repository cannot make the prompt unbounded: the offered
    /// display of each match is capped and stays on one line, so the numbering
    /// survives a 50 KB minified hit.
    #[test]
    fn one_enormous_match_is_bounded_and_stays_one_line() {
        let huge = format!("src/bundle.min.js:1: {}", "x".repeat(50_000));
        let offered = vec![huge.as_str(), "src/a.rs:1: x"];
        let prompt = triage_prompt("find it", "grep `x`", &offered);
        assert!(
            prompt.len() < 2_000,
            "one 50 KB match produced a {} byte prompt",
            prompt.len()
        );
        assert_eq!(offered_match_count(&prompt), 2);
    }

    // -- reading the answer --------------------------------------------------

    #[test]
    fn a_ranking_is_read_out_of_whatever_the_duty_says() {
        assert_eq!(read_ranking("3 1 2", 3), vec![2, 0, 1]);
        assert_eq!(read_ranking("3, 1", 3), vec![2, 0]);
        assert_eq!(read_ranking("The most useful is 2, then 1.", 3), vec![1, 0]);
    }

    /// Out of range is dropped, not clamped: clamping would promote a match the
    /// duty never chose and present it as the duty's judgment.
    #[test]
    fn an_out_of_range_number_is_dropped_rather_than_clamped() {
        assert_eq!(read_ranking("0 9 2 400", 3), vec![1]);
        assert_eq!(read_ranking("99999999999999999999999999 1", 3), vec![0]);
    }

    /// **The ranking can never be longer than the list it ranks.** A duty
    /// repeating itself five hundred times cannot inflate a three-match result.
    #[test]
    fn a_repeating_duty_cannot_inflate_the_result() {
        let answer = "1 ".repeat(500);
        assert_eq!(read_ranking(&answer, 3), vec![0]);
        assert_eq!(read_ranking(&"2 3 2 3 ".repeat(200), 3), vec![1, 2]);
    }

    #[test]
    fn an_answer_with_no_numbers_reads_as_no_ranking() {
        assert!(read_ranking("all of them look relevant", 3).is_empty());
        assert!(read_ranking("", 3).is_empty());
    }

    // -- the route legs ------------------------------------------------------

    /// An unresolvable route is a routing failure carrying the resolver's own
    /// sentence, and it never reaches a model.
    #[tokio::test]
    async fn an_unresolved_route_returns_the_resolvers_reason() {
        let route = DutyRoute::unresolved(
            "The 'triage' category inherits the 'scan' tier, which is not bound to any provider.",
        );
        let err = rank_matches(&route, "find it", "grep `parse`", &matches(), &empty())
            .await
            .expect_err("an unresolved route cannot rank");
        assert!(err.contains("not bound to any provider"), "{err}");
    }

    /// A local route really does rank, and the ordering is the duty's.
    #[tokio::test]
    async fn a_local_route_ranks_the_matches() {
        let engine: Arc<Mutex<dyn Engine>> =
            Arc::new(Mutex::new(MockEngine::with_response("mock", "3 1")));
        let route = DutyRoute::local(TRIAGE_DUTY, "local", engine);
        let order = rank_matches(&route, "find it", "grep `parse`", &matches(), &empty())
            .await
            .expect("the local duty ranks");
        assert_eq!(order, vec![2, 0]);
    }

    /// An answer that is not a ranking is a failure, not an ordering nobody
    /// chose (LESSON-447).
    #[tokio::test]
    async fn an_unreadable_answer_is_a_failure_not_an_ordering() {
        let engine: Arc<Mutex<dyn Engine>> = Arc::new(Mutex::new(MockEngine::with_response(
            "mock",
            "they all look useful to me",
        )));
        let route = DutyRoute::local(TRIAGE_DUTY, "local", engine);
        let err = rank_matches(&route, "find it", "grep `parse`", &matches(), &empty())
            .await
            .expect_err("an unreadable answer must not pass as a ranking");
        assert!(err.contains("no usable match numbers"), "{err}");
    }

    fn empty() -> Provenance {
        Provenance::empty()
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

    /// A remote `triage` route over a capturing transport, with `boundaries`
    /// enforced at the choke point.
    fn remote_route(
        boundaries: Vec<PrivacyBoundary>,
        reply: &str,
        repeat: usize,
    ) -> (DutyRoute, Arc<Mutex<Vec<Vec<u8>>>>) {
        let transport = CaptureTransport::default();
        let sent = Arc::clone(&transport.sent);
        let egress = Egress::new(transport, boundaries, Arc::new(NoopSink));
        let route = DutyRoute::remote(
            TRIAGE_DUTY,
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

    /// **AC-11.** A provider ignoring `max_tokens` cannot grow the triage
    /// buffer without limit. The assertion reads
    /// [`TRIAGE_OUTPUT_MAX_BYTES`] rather than a literal, so raising the ceiling
    /// cannot silently un-test the bound.
    ///
    /// Asserted on the *accepted* answer rather than on the ranking, because a
    /// ranking is bounded by the match list whatever the provider does — the
    /// buffer is the thing an unbounded stream grows, so the buffer is what the
    /// ceiling has to be measured on.
    #[tokio::test]
    async fn a_remote_triage_is_bounded_however_much_the_provider_streams() {
        // 1 KiB per delta × 64 deltas = 64 KiB offered, 2 KiB accepted.
        let chunk = format!("{} ", "7".repeat(1023));
        let (route, _sent) = remote_route(Vec::new(), &chunk, 64);
        let DutyRoute::Serves { duty, .. } = &route else {
            panic!("the fixture must resolve, or the ceiling is never exercised");
        };

        let answer = duty
            .perform("rank these", &Provenance::empty())
            .await
            .expect("the remote duty served");

        assert!(
            answer.len() <= TRIAGE_OUTPUT_MAX_BYTES,
            "a triage accepted {} bytes from a provider ignoring its token budget; the \
             ceiling is {TRIAGE_OUTPUT_MAX_BYTES}",
            answer.len()
        );
        // Non-vacuity, both directions: the provider really did offer far more
        // than the bound, and the duty really did accumulate up to it — so this
        // is a cap and not a refusal.
        assert!(
            chunk.len() * 64 > TRIAGE_OUTPUT_MAX_BYTES * 4,
            "the fixture must offer well over the bound"
        );
        assert!(
            answer.len() > TRIAGE_OUTPUT_MAX_BYTES / 2,
            "the cap must let a real ranking through: {} bytes",
            answer.len()
        );
    }

    /// The ceiling is a **band** around the widest legitimate answer, not just a
    /// floor under it.
    ///
    /// The lower half is the obvious one: a bound that truncated a full ranking
    /// of the tool's own cap would be a silent filter. The upper half is the one
    /// that makes the bound mean anything — a ceiling is a safety bound on an
    /// untrusted stream, so a ceiling many times larger than any answer the duty
    /// could legitimately produce is not bounding the duty, it is bounding
    /// nothing. Without it, widening this constant kills no test: AC-11's
    /// enforcement test reads the constant, so it moves with it (the disclosed
    /// limitation this closes; `title` closes the same gap by deriving its
    /// ceiling from its contract's word budget).
    ///
    /// [`CEILING_HEADROOM`] is the multiple, stated rather than implied.
    #[test]
    fn the_ceiling_is_a_band_around_a_ranking_of_every_capped_match() {
        /// How much bigger than the widest legitimate answer a ceiling may be.
        ///
        /// Room for a different separator, a stray token, a slightly larger tool
        /// cap — and not room for a different order of magnitude.
        const CEILING_HEADROOM: usize = 4;

        let full: String = (1..=200)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            full.len() < TRIAGE_OUTPUT_MAX_BYTES,
            "a full ranking is {} bytes and the ceiling is {TRIAGE_OUTPUT_MAX_BYTES}",
            full.len()
        );
        assert!(
            TRIAGE_OUTPUT_MAX_BYTES < CEILING_HEADROOM * full.len(),
            "the ceiling is {TRIAGE_OUTPUT_MAX_BYTES} bytes against a widest legitimate \
             answer of {} — more than {CEILING_HEADROOM}× the largest thing this duty \
             can honestly say is a bound on nothing",
            full.len()
        );
        assert_eq!(TRIAGE_DUTY.ceiling_bytes(), TRIAGE_OUTPUT_MAX_BYTES);
        assert_eq!(TRIAGE_DUTY.category(), Category::Triage);
    }

    // -- the boundary interaction (BR-7) ------------------------------------

    fn boundaries() -> Vec<PrivacyBoundary> {
        vec![PrivacyBoundary {
            path_glob: "secrets/**".to_owned(),
            mode: BoundaryMode::LocalOnly,
        }]
    }

    /// **The non-vacuity half.** A remotely-bound `triage` over clean matches
    /// genuinely sends: the prompt reaches the transport and the provider's
    /// ranking comes back.
    #[tokio::test]
    async fn a_remotely_bound_triage_sends_the_duty_prompt_and_returns_its_ranking() {
        let (route, sent) = remote_route(boundaries(), "2 1", 1);
        let order = rank_matches(
            &route,
            "find the parser",
            "grep `parse`",
            &matches(),
            &Provenance::tainted_by("src/a.rs".to_owned()),
        )
        .await
        .expect("clean matches rank remotely");
        assert_eq!(order, vec![1, 0]);
        assert!(
            wire(&sent).contains("Rank them by how"),
            "the duty prompt never reached the transport"
        );
    }

    /// **BR-7, asserted by capture.** The same route, the same boundaries — but
    /// a match came out of a `local-only` file, so the choke point refuses before
    /// a byte leaves and the caller is told why. The test above proves this route
    /// *does* send when the content is clean, which is what makes this one
    /// evidence rather than a fixture that never reached the sending state
    /// (LESSON-485).
    #[tokio::test]
    async fn a_local_only_match_is_never_sent_to_a_remote_triage() {
        let (route, sent) = remote_route(boundaries(), "2 1", 1);
        let secret = "secrets/prod.env:1: API_KEY=super-secret-xyzzy";
        let offered = vec![secret, "src/a.rs:12: fn parse()"];

        let err = rank_matches(
            &route,
            "find the key",
            "grep `API_KEY`",
            &offered,
            &Provenance::tainted_by("secrets/prod.env".to_owned()),
        )
        .await
        .expect_err("boundary content must not be ranked remotely");

        assert!(
            wire(&sent).is_empty(),
            "boundary content reached the transport"
        );
        assert!(!wire(&sent).contains("super-secret-xyzzy"));
        assert!(err.contains("privacy boundary"), "{err}");
    }
}
