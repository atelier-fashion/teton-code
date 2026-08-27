//! Redaction: the verdict data model, the deterministic credential pattern
//! pass, and the pure forward/block decision (REQ-562, ADR-4/ADR-5/ADR-6).
//!
//! This module is the **foundation** the redaction duty (`harness::redact`) and
//! the egress gate hook (`egress::Egress::send`) consume. Every line of *code*
//! in it is pure: no async body, no engine, no I/O, no events. That is what
//! makes AC-10's decision table exhaustively testable at unit cost, and it keeps
//! the one security-relevant rule of the feature — *which verdicts block* — in a
//! function with no collaborators to mock.
//!
//! [`RedactionGate`] is the one exception, and it is a *declaration* rather than
//! an implementation: the choke point needs a name for "the thing that produces
//! a verdict" and the verdict type lives here, so the trait lives beside it.
//! Nothing in this module implements it — `runtime::RedactionGateImpl` does,
//! where the router, the engine slot and the duty seam already are.
//!
//! ## The two passes and where confidence comes from (ADR-4, BR-4)
//!
//! Confidence is **derived structurally, never self-reported**. A 3B model's own
//! estimate of its certainty is the least trustworthy value in the pipeline, so
//! it is never consulted:
//!
//! - **Pattern pass** (this module): a deterministic sweep for five credential
//!   shapes. A hit is [`Confidence::High`] *by construction* —
//!   [`Finding::pattern`] is the only way to build one and it hard-codes the
//!   level.
//! - **Model pass** (`harness::redact`, TASK-069): a hit the model reported and
//!   the redactor could locate in the payload. It is [`Confidence::Low`] by
//!   construction — [`Finding::model`] hard-codes that level.
//!
//! There is no `Finding::new(kind, span, confidence)`: a caller cannot choose a
//! confidence level, so "a pattern hit is high and a model hit is low" is a
//! property of the constructors rather than a convention someone maintains.
//!
//! ## A finding never quotes what it matched (BR-6, ADR-5)
//!
//! [`Finding`] has **no text field at all**. Not a private one, not an
//! `Option<String>`, none. A secret detector that echoes the secret into an
//! event, a log line, or a turn-failure sentence has *moved* the secret, not
//! caught it. Because the type cannot carry the matched bytes, "a finding never
//! quotes the matched text" is structural — every downstream consumer inherits
//! it for free, and `a_finding_never_carries_the_matched_text` below turns red
//! if anyone adds one back (AC-8 mutation (c)).
//!
//! ## Fail closed, and say which failure it was (ADR-6, BR-3, BR-7)
//!
//! [`Outcome::Unavailable`] is the single "the scan could not run" state, and
//! [`decide`] maps it to [`EgressDecision::Block`]. A guard that cannot run does
//! not become a guard that passes everything. It rides with `scanned: false` so
//! the *reason* stays legible: the user is told the scan could not run, never
//! that it found something — different problems with different fixes.
//!
//! An over-cap payload ([`REDACT_INPUT_MAX_BYTES`]) is that case, **not** a
//! silent pass and **not** a truncate-and-scan: a partial scan reporting itself
//! as complete is precisely the lie BR-7 forbids. ADR-6 permits running the
//! pattern pass first on an over-cap payload so a real finding could outrank
//! "too large" as the reported reason; [`pattern_verdict`] deliberately does not
//! take that option. The terminal outcome is Block either way, and reporting
//! `Findings` (which carries `scanned: true`) would claim a completed scan of a
//! payload the model pass never saw.
//!
//! **Over-cap is not the same event as over-window.** The model pass is chunked
//! ([`REDACT_CHUNK_MAX_BYTES`], `harness::redact::scan`), so a payload larger
//! than one engine window is scanned in several calls rather than refused. What
//! [`REDACT_INPUT_MAX_BYTES`] bounds is how many calls one send may buy —
//! chunking spreads a scan, it does not make it unbounded (BR-7).
//!
//! ## No regex crate
//!
//! The REQ forbids new crates and `regex` is not a declared dependency of any
//! crate in this workspace (it appears in `Cargo.lock` only transitively). The
//! five shapes are anchored on a literal prefix or a literal suffix, so they are
//! hand-rolled byte scans below.
//!
//! ## Why byte offsets from those scans are always char boundaries
//!
//! Not because "every shape is pure ASCII" — the assignment shape's `\S+` value
//! arm consumes *any* non-whitespace byte, continuation bytes included, so a
//! match on `AWS_API_KEY=café` really does span multi-byte content. The
//! guarantee comes from where each run can **stop**:
//!
//! - every span *start* is at a literal ASCII prefix, or at the start of an
//!   ASCII `[A-Z_]+` run extended leftward one ASCII byte at a time;
//! - every span *end* is at a byte that failed an ASCII-only predicate
//!   (`is_body`, `is_shape_space`) or at the end of the input.
//!
//! An ASCII byte can never occur inside a multi-byte UTF-8 sequence, so a run
//! that begins and ends at ASCII decisions can never stop in the middle of one.
//! `spans_are_byte_offsets_that_stay_on_char_boundaries` and
//! `a_multibyte_assignment_value_still_slices` pin both halves.

use std::ops::Range;

use async_trait::async_trait;

use crate::harness::duty::DUTY_REQUEST_BYTES_PER_TOKEN;
use crate::harness::redact::{
    REDACT_DEFUSE_GROWTH_DIVISOR, REDACT_DUTY, REDACT_PROMPT_OVERHEAD_BYTES,
};
use crate::harness::render::CHATML_DUTY_ENVELOPE_BYTES;
use crate::runtime::LOCAL_ENGINE_N_CTX;

/// Bytes the duty seam assumes per BPE token, sizing a prompt against a context
/// window.
///
/// **The seam's own constant, read rather than restated.** The output side
/// (`DutyKind::max_tokens`) and the input side (the cap below) are two uses of
/// one convention, and a second copy here would be the second number LESSON-446
/// is about.
///
/// It is an **estimate, not a bound**. Real BPE averages nearer four bytes per
/// token on prose and code, so two usually over-counts a payload's tokens and
/// the cap lands under the window. *Usually*: dense punctuation, base64 and CJK
/// can all tokenize under two bytes per token, and for those the arithmetic
/// below under-states the real token count. That is why the cap is a cheap
/// first filter and the actual bound is measured — see
/// [`crate::harness::redact::scan`]'s render guard — with the engine's own
/// typed over-window refusal as the last backstop.
const DUTY_BYTES_PER_TOKEN: usize = DUTY_REQUEST_BYTES_PER_TOKEN;

/// The prompt budget in bytes: what the local engine will accept **after** the
/// duty's own generation reservation.
///
/// `LlamaEngine::complete` refuses — with a typed error, before llama.cpp's
/// `GGML_ASSERT` can abort the daemon — any prompt whose token count exceeds
/// `n_ctx - max_tokens`. That refusal is an engine error, which the scan turns
/// into `Unavailable`, which blocks. Correct, but with the *wrong reason*: the
/// user is told the scan could not run when what actually happened is that the
/// payload was too large, and the fix for those is not the same.
///
/// This is the budget the **rendered** prompt has to fit, which is what
/// [`crate::harness::redact::scan`] measures against before it calls the model.
pub(crate) const REDACT_PROMPT_BUDGET_BYTES: usize =
    (LOCAL_ENGINE_N_CTX as usize - REDACT_DUTY.max_tokens() as usize) * DUTY_BYTES_PER_TOKEN;

/// The largest payload the redactor will hand to a **single** model call, in
/// bytes (ADR-6, BR-7).
///
/// **Derived from the engine window, not picked beside it** (LESSON-446). The
/// per-call cap and the window are two descriptions of one budget, and they used
/// to be two independently chosen numbers:
///
/// ```text
///   engine window            16,384 tokens   (LOCAL_ENGINE_N_CTX)
///   − the duty's generation   1,024 tokens   (REDACT_DUTY.max_tokens())
///   = prompt budget          15,360 tokens
///   × 2 bytes/token          30,720 bytes    (the seam's convention — an
///                                             ESTIMATE, see below)
///   − the ChatML envelope         55 bytes   (CHATML_DUTY_ENVELOPE_BYTES:
///                                             33 message delimiters + 22 cue,
///                                             added by render_duty AFTER the
///                                             prompt is built)
///   − prompt overhead            586 bytes   (REDACT_PROMPT_OVERHEAD_BYTES:
///                                             318 instruction + 257 contract
///                                             + 11 header)
///   − 1 byte                                 (the frame-defusing bound's
///                                             constant term)
///   = 30,078 bytes for payload + its worst-case defusing
///   × 9/10                   30,078 → 27,070 (ADR-009 frame defusing inserts at
///                                             most one byte per 9 bytes of
///                                             payload — REDACT_DEFUSE_GROWTH_DIVISOR)
///   = REDACT_CHUNK_MAX_BYTES  27,070 bytes
/// ```
///
/// Every term is *measured*, not stated: the numbers above are what the
/// constants happen to be today, and editing the instruction — or the frame
/// label the defusing looks for, or the chat template's delimiters — moves the
/// cap rather than eating into the window.
///
/// The `× 9/10` is the ADR-009 term. [`redact_prompt`](crate::harness::redact::redact_prompt)
/// defuses line-anchored `Payload:` labels inside the payload before embedding
/// it, and an insertion-only transform makes the prompt longer than the
/// payload. A cap sized against the *raw* payload would let a payload built
/// entirely of `Payload:\n` lines push the prompt back over the window — the
/// same failure this derivation exists to remove, reintroduced by the fix for a
/// different one.
///
/// ## What this arithmetic does **not** cover, and what does (LESSON-488)
///
/// Two terms are deliberately absent, and they are the reason the real bound is
/// a *measurement* — [`crate::harness::redact::scan`] renders the prompt and
/// refuses before the model call when it exceeds
/// [`REDACT_PROMPT_BUDGET_BYTES`] — rather than this constant:
///
/// 1. **Control-token neutralization.** `render_duty` defuses every `<|…|>` run
///    (`<|` → `<_|`) on both arms, which is insertion-only and worst-cased at
///    **one byte per two** of payload: a payload of `<|`-runs closed by a `|>`
///    within the 64-byte span window grows by ~48%. Folding a `× 2/3` term in
///    here would cost every user a third of the cap — dropping it to ~18 KiB,
///    below a single large file — to pre-reject a payload the render guard
///    rejects for free. So the term lives in the guard, not in the constant.
/// 2. **The bytes-per-token estimate itself.** [`DUTY_BYTES_PER_TOKEN`] is an
///    estimate, not a bound; base64 or CJK content can tokenize under two bytes
///    per token, and no byte arithmetic can fix that. The engine's typed
///    over-window refusal remains the last backstop, as it is for every other
///    duty.
///
/// So: this constant is the **cheap first filter** on each chunk — it is what
/// sizes the window handed to one model call, before a prompt is built — and
/// the render guard is what makes "the prompt fits the window" true rather
/// than estimated.
///
/// The old value was a flat 64 KiB, which is **more than twice** the prompt
/// budget. Every payload between roughly 30 KiB and 64 KiB therefore passed the
/// cap, was rendered into a prompt, and was refused by the engine as
/// over-window — reported as "the redaction scan could not run" when the true
/// reason was "this payload is too large to scan", which is BR-3's distinction
/// collapsed by arithmetic rather than by wording.
///
/// ## It is a *per-call* cap now, not the bound on what can be scanned
///
/// It used to be both, and being both is what collided with the harness's own
/// context budget: `HarnessConfig::context_budget_bytes` is **32,768** —
/// *larger* than this number — so a turn that filled its context budget
/// assembled a body the scan refused outright, and with `[privacy] redact =
/// true` every such remote turn blocked as `ScanUnavailable`. Fail-closed and
/// honest about its reason, and unusable on any repository where long turns are
/// normal.
///
/// [`crate::harness::redact::scan`] therefore **chunks** the model pass: a
/// payload longer than this is scanned in overlapping windows of at most this
/// size, one model call each, and the verdict claims a completed scan only when
/// *every* chunk's call completed. The pattern pass is unchanged — it has no
/// window and still sweeps the whole payload in one go.
///
/// What bounds the scan as a whole is [`REDACT_INPUT_MAX_BYTES`], which is a
/// stated multiple of this.
///
/// ## Why this module reaches upward for four constants
///
/// It is otherwise the pure foundation the duty and the choke point consume,
/// and the imports below run the other way. That is the price of there being
/// **one** number: the cap belongs to the engine that has to hold the prompt
/// and to the prompt that has to fit in it, and a copy of any of its inputs
/// here would be the second number LESSON-446 is about.
pub const REDACT_CHUNK_MAX_BYTES: usize =
    (REDACT_PROMPT_BUDGET_BYTES - CHATML_DUTY_ENVELOPE_BYTES - REDACT_PROMPT_OVERHEAD_BYTES - 1)
        * REDACT_DEFUSE_GROWTH_DIVISOR
        / (REDACT_DEFUSE_GROWTH_DIVISOR + 1);

/// The escaping factor an outbound body is modelled with: the JSON escaping of
/// the assembled context costs **one part in this many** of the context's own
/// bytes — `context / REDACT_ESCAPING_DIVISOR` on top of the context.
///
/// One home for the term (REQ-586 BR-11), because it sits in two places that
/// have to agree: the margin tests
/// (`the_total_cap_clears_the_harness_context_budget_with_margin` here and
/// `harness::tools::web::tests::the_web_tool_docs_clear_the_outbound_body_overhead`)
/// charge `budget / REDACT_ESCAPING_DIVISOR` against
/// [`REDACT_BODY_OVERHEAD_BYTES`], and [`REDACT_SCANNABLE_CONTEXT_BYTES`] is
/// solved for the context that, escaped by the same factor, still fits under the
/// cap. A body's escaping is overwhelmingly the quotes, backslashes and newlines
/// of code and tool output, and a tenth is the allowance REQ-562's margin test
/// has charged since the cap was first derived against it; were it to move,
/// the bound and the measurement would move together or not at all.
pub(crate) const REDACT_ESCAPING_DIVISOR: usize = 10;

/// Bytes a remote request body carries **beyond** the harness's assembled
/// context: the system prompt (tool descriptions included), the JSON envelope,
/// and the escaping of the context itself.
///
/// It exists so that [`REDACT_INPUT_MAX_BYTES`]'s "with room to spare" is a
/// number a test can check rather than a claim in prose. It is an
/// **assumption** about the outbound body, and
/// `the_total_cap_clears_the_harness_context_budget_with_margin` checks the half
/// of it that is measurable: the real
/// [`build_system_prompt`](crate::harness::turn_loop::build_system_prompt) with
/// every builtin tool registered, plus a tenth of the context budget
/// ([`REDACT_ESCAPING_DIVISOR`]) for JSON escaping, must fit inside it. If a
/// later REQ adds enough tools to overflow that, the assumption turns red
/// instead of silently eating the margin.
///
/// It was `#[cfg(test)]` until REQ-586 (ADR-6): nothing in the send path read
/// it, and a production constant nothing reads is a number that drifts
/// unnoticed. It now has a production reader —
/// [`REDACT_SCANNABLE_CONTEXT_BYTES`] subtracts it from the cap to find how much
/// *context* a scanned route may assemble — so it is the one definition of
/// "what the body carries beyond the assembled context" that both the margin
/// tests and the bound read. The value is unchanged by the promotion; the two
/// claims the margin test makes about it are unchanged too.
///
/// Raised 8→9 KiB by REQ-577 when `teton_docs` (a sixth, cap-exempt builtin)
/// made the 8 KiB assumption red; chunk caps and [`REDACT_INPUT_MAX_BYTES`] are
/// unchanged (verify: 2×(32768+9216)=83968 ≤ 108280, chunks stay 4). That is
/// this doc's own prescription being followed rather than worked around: the
/// tool's docs are 274 bytes and the 8 KiB assumption had 19 bytes of slack
/// above [`MIN_PROMPT_HEADROOM_BYTES`], so no trim of any description could
/// have closed it. The floor itself is untouched — it is the line the margin is
/// never traded away across (REQ-577 BR-4).
///
/// Raised 9→10 KiB by BUG-181, for the same shape of reason: the capability
/// sentence the bug adds to the bundled guide (the session's commands are the
/// ones `/help` lists; nothing is loaded from `.claude/`) is 186 bytes, and the
/// 9 KiB assumption had **1 byte** of slack above [`MIN_PROMPT_HEADROOM_BYTES`]
/// — every other line of the guide is pinned by whole-line or per-segment
/// assertions tuned by live A/B (REQ-579), so paying for it by trimming would
/// have meant re-tuning a sentence that works. Chunk caps and
/// [`REDACT_INPUT_MAX_BYTES`] are again unchanged (verify:
/// 2×(32768+10240)=86016 ≤ 108280, chunks stay 4). The floor is again untouched.
///
/// Raised 10→11 KiB by REQ-587, and this one has a **second consequence the
/// two above did not**. BR-2 puts a `skill` tool in the prompt whose
/// description carries a roster bounded at
/// [`ROSTER_MAX_BYTES`](crate::harness::tools::skill::ROSTER_MAX_BYTES) — 1,010
/// bytes of docs and schema at that ceiling — and BR-8 amends the guide's
/// capability sentence for the third time (+82). Together that is 1,092 against
/// 826 of margin, so no trim of a description could have closed it and the
/// assumption moved, exactly as it did for `teton_docs` and for BUG-181's
/// sentence. Chunk caps and [`REDACT_INPUT_MAX_BYTES`] are unchanged a third
/// time (verify: 2×(32768+11264)=88064 ≤ 108280, 88064/27070=3.25, chunks stay
/// 4) and the floor is untouched.
///
/// **What is new is the other direction.** Since REQ-586 this constant has a
/// production reader, so raising it *narrows* every `[privacy] redact = true`
/// route's byte budget: [`REDACT_SCANNABLE_CONTEXT_BYTES`] drops 89,127 →
/// 88,196, a **931-byte** cut to the budget BR-7's `bound: redact scan`
/// refusal measures against. Every assertion about that bound is an inequality
/// that holds either way, so the cut is silent unless it is stated —
/// `the_overhead_raise_restates_the_chunk_count_and_the_scannable_bound` is
/// what states it, and it is the test the *next* raise has to come back and
/// re-state too.
///
/// `pub(crate)` because the *other* prompt shape has to clear it too and cannot
/// be built from here: with `[web] tier` above `off` the system prompt carries
/// the web tool's docs instead of REQ-563's BR-6 opt-in clause, and building
/// that tool needs a permission gate and a choke-point seam. Its half of the
/// assertion lives beside the tool
/// (`harness::tools::web::tests::the_web_tool_docs_clear_the_outbound_body_overhead`);
/// this is the number both of them measure against, so the two shapes cannot
/// come to disagree about the budget.
pub(crate) const REDACT_BODY_OVERHEAD_BYTES: usize = 11 * 1024;

/// The smallest headroom [`REDACT_BODY_OVERHEAD_BYTES`] may be left with after
/// the largest system prompt this build produces (REQ-572 verify).
///
/// The two headroom tests already refused a prompt that *exceeded* the
/// assumption and a prompt that filled it *exactly*. Between those lies the case
/// that actually happens: a prompt that clears it by a handful of bytes, which
/// passes, tells nobody, and leaves the next sentence anyone adds unable to
/// land. The floor turns that from a discovery into a **decision** — a change
/// that eats the last of the margin has to either shorten something or move this
/// number, and moving it is a diff someone reviews.
///
/// Forty-eight bytes: about one short clause, which is the unit these prompts
/// actually grow in. Small enough that it is not a second budget competing with
/// the overhead it guards, large enough that "there is room for one more
/// sentence" is true rather than nearly true.
#[cfg(test)]
pub(crate) const MIN_PROMPT_HEADROOM_BYTES: usize = 48;

/// The margin the two headroom sweeps **currently measure**, pinned so that it
/// cannot drift without someone deciding it should (BUG-193).
///
/// # Why a pin and not just a floor
///
/// [`MIN_PROMPT_HEADROOM_BYTES`] is an *inequality*, and an inequality holds
/// identically at 710 bytes of margin, at 476, and at 49. That is exactly how
/// this budget's own doc comment came to be wrong by ~234 bytes across six REQs
/// (REQ-583, REQ-585, REQ-587, REQ-589, REQ-590, REQ-591) while the test stayed
/// green the entire time: the number a human reads lived in prose, and nothing
/// compared the prose to reality. REQ-592 was sized against the stale 710 and
/// had to be corrected mid-task.
///
/// The floor answers "is there room at all". This answers "did the resident
/// prompt move without anyone noticing", which is the question that actually
/// went unasked.
///
/// # The cost, accepted deliberately
///
/// Every intentional prompt edit now fails a test until this number is updated.
/// That is churn, and it is the point: with 129 bytes of margin against a
/// 48-byte floor there are **81 bytes of usable room**, so an edit that costs 20
/// of them is not a detail — it is a fifth of what is left, and it should
/// announce itself. When the margin was 710 this pin would have been noise;
/// at 81 it is the cheapest possible alarm.
///
/// # Updating it
///
/// Re-measure, do not reason — that correction is what reasoning about it cost
/// last time. Add a ledger line to [`REDACT_BODY_OVERHEAD_BYTES`] saying which
/// REQ spent the bytes, then move this number in the same diff.
#[cfg(test)]
pub(crate) const RECORDED_PROMPT_MARGIN_BYTES: usize = 129;

/// The same pin for the **web-enabled** prompt shape measured by
/// `harness::tools::web::tests::the_web_tool_docs_clear_the_outbound_body_overhead`.
///
/// Two shapes, two numbers: with `[web] tier` above `off` the prompt carries the
/// web tool's docs instead of REQ-563's opt-in clause, so it spends a different
/// amount and must be pinned separately. It lives here rather than beside its
/// test for the same reason [`REDACT_BODY_OVERHEAD_BYTES`] does — one place
/// holds the budget vocabulary, so the two shapes cannot come to disagree about
/// which constant they are measuring against.
#[cfg(test)]
pub(crate) const RECORDED_WEB_PROMPT_MARGIN_BYTES: usize = 176;

/// How many per-chunk windows the total cap is worth — the multiple that turns
/// [`REDACT_CHUNK_MAX_BYTES`] into [`REDACT_INPUT_MAX_BYTES`].
///
/// **Four, and the arithmetic is the reason** (the ADR-6 derivation's shape:
/// state the terms, do not pick the answer):
///
/// ```text
///   HarnessConfig::context_budget_bytes   32,768 bytes  (turn_loop.rs)
///   + `REDACT_BODY_OVERHEAD_BYTES`        11,264 bytes  (system prompt, tool
///                                                        descriptions, JSON
///                                                        envelope + escaping —
///                                                        the assumption, and
///                                                        the test below checks
///                                                        it against the real
///                                                        system prompt)
///   = the largest ordinary outbound body  44,032 bytes
///   × 2 (the margin — a cap that only just clears the body it has to hold is
///        one context-budget bump away from being the old collision again)
///   = 88,064 bytes to clear
///   ÷ REDACT_CHUNK_MAX_BYTES              88,064 / 27,070 = 3.25
///   → the next whole chunk up                        4
/// ```
///
/// The overhead term moved 8→9 KiB in REQ-577 (quotient 3.03→3.10), 9→10 KiB in
/// BUG-181 (3.10→3.18) and 10→11 KiB in REQ-587 (3.18→3.25), which is the point
/// of writing the arithmetic out rather than the answer: the chunk count is
/// unchanged all three times, so [`REDACT_INPUT_MAX_BYTES`] is unchanged, and a
/// reader can see that rather than take it on trust.
///
/// **REQ-586: the remote budget is bounded by this when the scan applies.**
/// The arithmetic above runs from the *default* context budget up to the cap;
/// [`REDACT_SCANNABLE_CONTEXT_BYTES`] runs it the other way, from the cap down
/// to the largest context a `[privacy] redact = true` route may assemble, with
/// the same two terms (the overhead and the escaping) and without the 2×
/// margin — the margin exists so a budget bumped *elsewhere* cannot drift past
/// the cap, and a bound derived *from* the cap cannot drift by construction. So
/// the one thing a reader should take from this block is that there is still
/// only one cap: a route's budget on a scanned route is a function of the same
/// constants this chunk count is, and moving either of them re-derives both.
///
/// The other half of the choice is the chunk **count**, because every chunk is
/// a model call and ADR-8's budget is per call. With the
/// [`REDACT_CHUNK_OVERLAP_BYTES`](crate::harness::redact::REDACT_CHUNK_OVERLAP_BYTES)
/// overlap the stride is 26,814 bytes, so a payload at the total cap is at most
/// **five** chunks
/// ([`REDACT_MAX_CHUNKS`](crate::harness::redact::REDACT_MAX_CHUNKS), asserted
/// against the real chunker rather than restated, and enforced by
/// [`scan`](crate::harness::redact::scan) before the first call) — p50 ≤ 10 s
/// and p95 ≤ 25 s at ADR-8's per-chunk budget, against a context-budget-full
/// turn's expected **two**. Five is the number that has to stay small because
/// it multiplies that budget, and a cap twice this size would double the
/// ordinary latency. It no longer multiplies the *worst case*: the scan as a
/// whole is bounded at one `DUTY_DEADLINE`, not one per chunk.
const REDACT_TOTAL_CAP_CHUNKS: usize = 4;

/// The largest payload the redactor will scan **at all**, in bytes (BR-7,
/// ADR-6).
///
/// A payload above this is [`Outcome::Unavailable`] (→ Block), never truncated
/// and scanned, and it costs **zero** model calls — the refusal is
/// [`pattern_verdict`]'s, before any chunk is cut.
///
/// The bound still exists for the reason BR-7 gives — a scan has to be bounded
/// or "scanned" means nothing — but it is no longer the engine window wearing a
/// second hat. The window bounds one *call*
/// ([`REDACT_CHUNK_MAX_BYTES`]); this bounds how many calls one send may buy,
/// and [`REDACT_TOTAL_CAP_CHUNKS`] carries the arithmetic that picked it.
///
/// **This is the number that used to sit under the harness's context budget**
/// (32,768) and block every context-budget-full remote turn. It is now
/// **108,280** — 3.3× that budget, 2.46× a full body with the system prompt and
/// JSON overhead on top of it (2.6× before REQ-577 widened the overhead term,
/// 2.58× before BUG-181 widened it again, 2.52× before REQ-587 did).
/// The collision is closed rather than measured; what
/// `docs/manual-verification.md` now records is the *chunk-count distribution*,
/// which is where the cost went.
pub const REDACT_INPUT_MAX_BYTES: usize = REDACT_TOTAL_CAP_CHUNKS * REDACT_CHUNK_MAX_BYTES;

/// The largest assembled **context** a `[privacy] redact = true` route may
/// carry, in bytes — the bound the redact scan places on a remote route's byte
/// budget (REQ-586 BR-4, ADR-6).
///
/// The model redact scan runs on the whole outbound body, and a body above
/// [`REDACT_INPUT_MAX_BYTES`] is refused unscanned (→ Block). A scanned route
/// therefore cannot be allowed to assemble a body the redactor will not scan:
/// its budget is `min(window-derived, this)`, with the byte guard binding
/// (`harness::budget::derive` applies it last and names the bound
/// `redact_scan` when it bites). The word component stays window-derived — the
/// scan is byte-denominated, so this is a byte bound.
///
/// **Derived, never a literal** — the same constants as
/// [`REDACT_TOTAL_CAP_CHUNKS`], run from the cap downward (BR-11, LESSON-491):
///
/// ```text
///   REDACT_INPUT_MAX_BYTES              108,280 bytes  (the cap: 4 × 27,070)
///   − REDACT_BODY_OVERHEAD_BYTES         11,264 bytes  (system prompt, JSON
///                                                       envelope — what the
///                                                       body carries beyond
///                                                       the context)
///   = room for the escaped context       97,016 bytes
///   ÷ (1 + 1/REDACT_ESCAPING_DIVISOR)    × 10 / 11     (the context plus its
///                                                       own escaping has to fit
///                                                       in that room)
///   = the scannable context               88,196 bytes
/// ```
///
/// So a body at the bound is `88,196 + 8,819 (escaping) + 11,264 (overhead) =
/// 108,279 ≤ 108,280`: four chunk-widths, up to five chunks with the overlap
/// ([`REDACT_MAX_CHUNKS`](crate::harness::redact::REDACT_MAX_CHUNKS)) — inside
/// the envelope REQ-562 measured, and ≈ 2.7× the default context budget
/// (32,768), which admits every ADLC skill.
///
/// Two things about the shape of the expression, both decisions rather than
/// arithmetic:
///
/// - **A floor.** The overhead term already folds the *default* budget's
///   escaping (the margin test charges `budget / REDACT_ESCAPING_DIVISOR`
///   against it), so a body at this bound escapes its context twice over — by
///   the divisor here and by the allowance inside the overhead. The bound is
///   conservative by a few KB; it is never optimistic.
/// - **Cap minus overhead, not the cap with the 2× margin inverted.** The
///   margin in [`REDACT_TOTAL_CAP_CHUNKS`]'s derivation exists so a context
///   budget bumped *elsewhere* cannot drift past the cap. This bound is
///   derived *from* the cap: move [`REDACT_BODY_OVERHEAD_BYTES`] or the chunk
///   count and it re-derives, and
///   `the_scannable_bound_plus_overhead_and_escaping_fits_under_the_cap` is
///   what says so. Halving it for a margin it cannot need would bound a
///   200k-window route to ≈ 44 KB of context for no collision it prevents.
///
/// `pub`, like the cap and the per-chunk window beside it: `harness::budget`
/// reads it, and the egress-capture integration tests (the redact bound's own,
/// AC-6) read the caps the same way from outside the crate. It is the *only*
/// place the redact chain's arithmetic leaves this module — a second copy of
/// the number anywhere (TASK-192's one-home grep: `89_127`/`89127` must hit
/// comments only) would be the drift LESSON-446 is about.
pub const REDACT_SCANNABLE_CONTEXT_BYTES: usize =
    (REDACT_INPUT_MAX_BYTES - REDACT_BODY_OVERHEAD_BYTES) * REDACT_ESCAPING_DIVISOR
        / (REDACT_ESCAPING_DIVISOR + 1);

/// How much the pipeline trusts a finding — **derived, never self-reported**
/// (BR-4, ADR-4).
///
/// The two levels come from *which pass produced the finding*, not from any
/// judgment about the content: see [`Finding::pattern`] and [`Finding::model`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Confidence {
    /// A deterministic pattern hit. A string matching one of the five shapes
    /// essentially *is* the credential it looks like, so precision is near
    /// perfect — this level blocks.
    High,
    /// A model-only hit. Moderate recall, uncertain precision: the model catches
    /// what patterns structurally cannot, and can also invent. Reported, not
    /// blocking (BR-4) — blocking every low hit makes the feature unusable and
    /// trains users to disable it.
    Low,
}

impl Confidence {
    /// A stable, content-free name for reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Confidence::High => "high-confidence",
            Confidence::Low => "low-confidence",
        }
    }
}

/// What kind of sensitive content a finding identifies.
///
/// The deterministic pattern pass only ever emits [`FindingKind::Credential`] —
/// all five of its shapes are credentials. The other variants exist for the
/// model pass, which is the half of the feature that can recognize a secret
/// paraphrased into prose or PII with no fixed shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FindingKind {
    /// A secret value with no credential-specific shape.
    Secret,
    /// An API key, access key, or bearer token.
    Credential,
    /// Personally identifying information.
    Pii,
    /// Sensitive, but the pass could not classify it further.
    Unknown,
}

impl FindingKind {
    /// A stable, content-free name for reports and events.
    ///
    /// Safe to render anywhere: it names the *class* of thing found, never the
    /// thing itself (BR-6).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            FindingKind::Secret => "secret",
            FindingKind::Credential => "credential",
            FindingKind::Pii => "pii",
            FindingKind::Unknown => "unknown",
        }
    }
}

/// One located piece of sensitive content: what class it is, where it is, and
/// how much the pipeline trusts the detection.
///
/// **There is no text field on this type, by design** (BR-6, ADR-5). A finding
/// carries a byte range into the scanned payload and nothing else, so no
/// consumer — event, log, error, CLI renderer — can echo the matched bytes even
/// by accident. A model-reported string that cannot be located in the payload is
/// a fabrication and is dropped rather than reported: a finding with no span is
/// not a finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    kind: FindingKind,
    span: Range<usize>,
    confidence: Confidence,
}

impl Finding {
    /// A finding from the deterministic pattern pass: [`Confidence::High`] **by
    /// construction** (ADR-4). There is no way to build a high-confidence
    /// finding from anywhere else.
    #[must_use]
    pub fn pattern(kind: FindingKind, span: Range<usize>) -> Self {
        Self {
            kind,
            span,
            confidence: Confidence::High,
        }
    }

    /// A finding from the local-model pass: [`Confidence::Low`] **by
    /// construction** (ADR-4). The model's own certainty is never consulted, so
    /// no argument for it exists.
    #[must_use]
    pub fn model(kind: FindingKind, span: Range<usize>) -> Self {
        Self {
            kind,
            span,
            confidence: Confidence::Low,
        }
    }

    /// The class of sensitive content.
    #[must_use]
    pub fn kind(&self) -> FindingKind {
        self.kind
    }

    /// The byte range in the scanned payload. Always a valid `str` char-boundary
    /// range for the text it was derived from.
    #[must_use]
    pub fn span(&self) -> &Range<usize> {
        &self.span
    }

    /// How much the pipeline trusts this finding.
    #[must_use]
    pub fn confidence(&self) -> Confidence {
        self.confidence
    }

    /// Whether this finding is high-confidence — the predicate [`decide`] blocks
    /// on (BR-4).
    #[must_use]
    pub fn is_high(&self) -> bool {
        matches!(self.confidence, Confidence::High)
    }
}

/// The three terminal states of a redaction scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Outcome {
    /// The scan ran to completion and found nothing.
    Clean,
    /// The scan ran to completion and found at least one thing.
    Findings,
    /// The scan could **not** run (no local tier, over-cap payload, engine
    /// error, or deadline). Fails closed (ADR-6, BR-3).
    Unavailable,
}

/// The result of a redaction scan, with its invariants enforced by construction.
///
/// The spec's two invariants are not maintained by discipline — the fields are
/// private and the only four constructors ([`RedactionVerdict::clean`],
/// [`RedactionVerdict::from_findings`], [`RedactionVerdict::unavailable`],
/// [`RedactionVerdict::unavailable_with_evidence`]) each establish both:
///
/// - `findings` is non-empty **iff** the outcome is [`Outcome::Findings`];
/// - `scanned` is `false` **iff** the outcome is [`Outcome::Unavailable`].
///
/// The second is a biconditional on purpose: `scanned: false` is exactly the
/// claim "no scan happened here", so a completed scan can never carry it and an
/// aborted one can never omit it. That is what makes AC-3's "no configuration
/// makes the scan appear to have run when it did not" checkable.
///
/// ## Why `evidence` is a third field rather than more findings
///
/// The deterministic pattern pass sweeps the **whole** payload in one piece and
/// has no window to be cut by, so it can complete on a payload whose *model*
/// pass then fails. That leaves a real, deterministically-earned High finding on
/// the floor of an `Unavailable` verdict — and until 2026-08-08 it was dropped
/// outright: a transient engine stall erased the finding and reported "the scan
/// could not run" for a payload in which a credential *had* been found.
///
/// Putting it back into `findings` is not available: that would break the
/// non-empty-iff-`Findings` invariant above, and it would make `scanned: false`
/// ride beside a populated finding list — the two claims AC-3 is about. So it
/// rides in its own field, which the invariants say nothing about and
/// [`RedactionVerdict::evidence`] reads. `findings()` still means "what the
/// completed scan found"; `evidence()` means "what the deterministic pass had
/// already established when the scan stopped".
///
/// It is empty for every constructor but
/// [`RedactionVerdict::unavailable_with_evidence`], and always
/// [`Confidence::High`] — see that constructor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactionVerdict {
    outcome: Outcome,
    findings: Vec<Finding>,
    scanned: bool,
    /// What the deterministic pass established before the scan failed. Empty on
    /// every outcome but [`Outcome::Unavailable`], and empty there too unless
    /// the pattern pass had actually completed.
    evidence: Vec<Finding>,
}

impl RedactionVerdict {
    /// The scan ran and found nothing.
    #[must_use]
    pub fn clean() -> Self {
        Self {
            outcome: Outcome::Clean,
            findings: Vec::new(),
            scanned: true,
            evidence: Vec::new(),
        }
    }

    /// The scan ran and produced `findings`.
    ///
    /// An **empty** `findings` collapses to [`RedactionVerdict::clean`] rather
    /// than producing an `Outcome::Findings` with nothing in it: "the scan ran
    /// and found no findings" is precisely `Clean`, and allowing the degenerate
    /// pair would break the non-empty-iff-`Findings` invariant. Composing the
    /// two passes is therefore just concatenation — the caller does not branch
    /// on emptiness.
    #[must_use]
    pub fn from_findings(findings: Vec<Finding>) -> Self {
        if findings.is_empty() {
            return Self::clean();
        }
        Self {
            outcome: Outcome::Findings,
            findings,
            scanned: true,
            evidence: Vec::new(),
        }
    }

    /// The scan could not run, and nothing was established about the payload
    /// first. Carries no findings, no evidence, and `scanned: false`, and
    /// [`decide`] blocks on it (ADR-6, BR-3).
    ///
    /// This is the constructor for the failures that happen *before* the
    /// deterministic pass has swept the payload or before a model pass exists to
    /// fail: an over-cap payload (the pattern pass never ran) and an unresolved
    /// route (no local tier, so the actionable fact is the configuration, not
    /// the content). Use [`RedactionVerdict::unavailable_with_evidence`] for a
    /// model pass that failed on a payload the pattern pass had already swept.
    #[must_use]
    pub fn unavailable() -> Self {
        Self {
            outcome: Outcome::Unavailable,
            findings: Vec::new(),
            scanned: false,
            evidence: Vec::new(),
        }
    }

    /// The **model** pass could not run to completion on a payload the
    /// deterministic pass had already swept whole — carrying what that pass
    /// established (2026-08-08, on review evidence).
    ///
    /// Same outcome, same `scanned: false`, same `Block` from [`decide`] as
    /// [`RedactionVerdict::unavailable`]: what changes is what the block is
    /// allowed to *say*. A payload whose pattern pass found `sk-…` and whose
    /// second chunk hit a stalled engine is a payload in which a credential was
    /// found, by a pass that completed, over every byte. Reporting that as "the
    /// scan could not run" throws away the one deterministic fact anyone
    /// established and tells the user to go debug their engine.
    ///
    /// `established` is filtered to [`Confidence::High`] here rather than
    /// trusted: evidence is the thing a block sentence and a **session taint**
    /// are derived from (see `egress::block_cause`), and BR-4's rule that a
    /// low-confidence finding never blocks must not be reachable through a side
    /// door. A caller that passes low findings gets them dropped, not honoured.
    #[must_use]
    pub fn unavailable_with_evidence(established: Vec<Finding>) -> Self {
        Self {
            outcome: Outcome::Unavailable,
            findings: Vec::new(),
            scanned: false,
            evidence: established.into_iter().filter(Finding::is_high).collect(),
        }
    }

    /// Which terminal state the scan reached.
    #[must_use]
    pub fn outcome(&self) -> Outcome {
        self.outcome
    }

    /// The located findings — empty unless the outcome is [`Outcome::Findings`].
    #[must_use]
    pub fn findings(&self) -> &[Finding] {
        &self.findings
    }

    /// What the deterministic pass had established when the scan stopped —
    /// empty except on an [`RedactionVerdict::unavailable_with_evidence`]
    /// verdict, and always [`Confidence::High`] there.
    ///
    /// It is **not** `findings()`: these are not the result of a completed scan
    /// and nothing here makes `scanned()` true. They are the reason a blocking
    /// `Unavailable` may still name what was found rather than only that it
    /// stopped (`egress::block_cause`).
    #[must_use]
    pub fn evidence(&self) -> &[Finding] {
        &self.evidence
    }

    /// Whether a scan actually ran. `false` **only** for
    /// [`Outcome::Unavailable`], so a report can distinguish "found nothing"
    /// from "could not look" (BR-3).
    #[must_use]
    pub fn scanned(&self) -> bool {
        self.scanned
    }
}

/// What the egress choke point does with a payload after the scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EgressDecision {
    /// Hand the payload to the inner transport, **byte-for-byte unmodified**
    /// (BR-5, AC-9 — v1 detects, it never substitutes).
    Forward,
    /// Refuse the payload. The bytes are dropped, never sent, never logged.
    Block,
}

/// Map a verdict to the egress action (BR-4, ADR-4, ADR-6).
///
/// The whole rule, in one pure function:
///
/// | verdict                         | decision  | why                                |
/// |---------------------------------|-----------|------------------------------------|
/// | [`Outcome::Unavailable`]        | `Block`   | fail closed — BR-3                 |
/// | [`Outcome::Clean`]              | `Forward` | nothing found                      |
/// | `Findings` with any `High`      | `Block`   | a pattern hit essentially is a key |
/// | `Findings`, all `Low`           | `Forward` | the user's call — BR-4             |
///
/// A single high finding blocks regardless of how many low ones accompany it,
/// and the order findings arrive in is irrelevant. This is the rule that decides
/// whether the feature is usable or decorative, and AC-10 calls it the one a
/// later change is most likely to quietly retune — hence the table-driven test
/// below rather than a handful of examples.
#[must_use]
pub fn decide(verdict: &RedactionVerdict) -> EgressDecision {
    match verdict.outcome() {
        // A guard that cannot run does not become a guard that passes
        // everything (BR-3, ADR-6).
        Outcome::Unavailable => EgressDecision::Block,
        Outcome::Clean => EgressDecision::Forward,
        Outcome::Findings => {
            if verdict.findings().iter().any(Finding::is_high) {
                EgressDecision::Block
            } else {
                EgressDecision::Forward
            }
        }
    }
}

/// The daemon-log lines a **forwarded** payload's findings owe (BR-4, ADR-4).
///
/// ## Why this exists
///
/// [`decide`] forwards a `Findings` verdict whose findings are all
/// [`Confidence::Low`] — that is BR-4's rule, and AC-10 requires it. But a
/// finding that is computed and then dropped on the floor is indistinguishable
/// from a finding that was never made: nothing tells the user, nothing tells
/// the operator, and OQ-2's *"what did the model catch that patterns did
/// not?"* — the question that decides whether the model call earns its latency
/// — has no observable answer at all. ADR-4's wiring line says these are
/// "logged as kind+span only", and this is what produces those lines.
///
/// ## What a line may say, and what it structurally cannot
///
/// Kind, confidence, byte span, and the non-secret locus. Never the matched
/// text — a [`Finding`] has no text field to draw it from (BR-6), so this is
/// not a rule the formatter keeps but a fact about its inputs.
///
/// The confidence word is read off the finding rather than written as a
/// literal. Every finding on a forwarded verdict is `Low` today, by
/// construction; a literal would quietly become a lie the day that changes.
///
/// ## Total, and empty for everything else
///
/// A blocked verdict reports through `privacy_block` and the turn-failure
/// sentence — this returns nothing for it, so the two reporting paths cannot
/// double-report one payload. `Clean` and `Unavailable` carry no findings and
/// so produce no lines. One line per finding: a payload with two located
/// low-confidence strings has two places to look.
#[must_use]
pub fn forwarded_findings_report(verdict: &RedactionVerdict) -> Vec<String> {
    if decide(verdict) != EgressDecision::Forward {
        return Vec::new();
    }
    verdict
        .findings()
        .iter()
        .map(|finding| {
            format!(
                "tetond: redact — {} {} at bytes {}-{} of the outbound payload; forwarded \
                 (a low-confidence finding is reported, not blocked — BR-4).",
                finding.confidence().as_str(),
                finding.kind().as_str(),
                finding.span().start,
                finding.span().end,
            )
        })
        .collect()
}

/// Something that can scan an outbound payload and return a verdict (ADR-1,
/// ADR-2).
///
/// The collaborator [`Egress`](crate::egress::Egress) holds, and the whole
/// interface between the choke point and the redactor: text in, verdict out.
/// The choke point knows nothing about routers, engines, prompts or contracts —
/// it knows [`decide`].
///
/// ## Total, like the scan behind it
///
/// There is no `Result`. Every way a scan can fail — no local tier, an over-cap
/// payload, an engine error, a deadline, an unreadable reply — is already
/// [`Outcome::Unavailable`], which [`decide`] blocks on (ADR-6, BR-3). An error
/// arm here would be a *second* spelling of "the scan could not run", and the
/// two spellings would eventually disagree about whether that means block.
///
/// ## Absence is the off state (ADR-2)
///
/// The gate is installed only when `[privacy] redact` is true, so there is no
/// `enabled` flag on this trait and no `if enabled` branch behind it. "Off" is
/// `Option::None` at the choke point: zero calls, no model interaction, nothing
/// that could claim a scan ran (AC-13). A flag *inside* an installed gate would
/// be a switch a mutation could flip to "on and permissive".
#[async_trait]
pub trait RedactionGate: Send + Sync {
    /// Scan `payload` — the lossy-UTF-8 text of the exact bytes that would go on
    /// the wire (ADR-1) — and say what was found.
    async fn scan(&self, payload: &str) -> RedactionVerdict;
}

/// The deterministic credential pattern pass over `text` (ADR-4).
///
/// Always runs to completion and always yields [`Confidence::High`],
/// [`FindingKind::Credential`] findings — all five shapes are credentials.
/// Returns an empty vector for a clean payload; that empty vector is the *result
/// of a scan*, which is what distinguishes it from [`Outcome::Unavailable`].
///
/// Spans are byte ranges into `text`, sorted ascending by start and free of
/// containment: when two shapes match the same secret (`AWS_API_KEY=AKIA…`
/// matches both the assignment shape and the AWS shape), the outer span is kept
/// and the nested one dropped, so one secret produces one finding.
#[must_use]
pub fn pattern_pass(text: &str) -> Vec<Finding> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    for shape in PREFIX_SHAPES {
        scan_prefix_shape(bytes, shape, &mut spans);
    }
    scan_env_assignment(bytes, &mut spans);
    suppress_contained(spans)
        .into_iter()
        .map(|span| Finding::pattern(FindingKind::Credential, span))
        .collect()
}

/// The bounded pattern-pass entry point: cap check, then the pass (BR-7, ADR-6).
///
/// A payload longer than [`REDACT_INPUT_MAX_BYTES`] is
/// [`RedactionVerdict::unavailable`] — the scan does not run, is not truncated,
/// and does not report a partial result as a complete one. Everything at or
/// under the cap is scanned, and the resulting verdict carries `scanned: true`
/// whether or not anything was found.
///
/// **This pass is never chunked**, and that is the asymmetry worth stating: the
/// model pass is windowed because an engine has a context window, and this one
/// is a byte scan with no window at all. Chunking it would buy nothing and cost
/// the one thing chunking has to be careful about — a credential lying across a
/// boundary. So the total cap is the only bound it needs, and it is the cap the
/// whole scan is refused by.
#[must_use]
pub fn pattern_verdict(text: &str) -> RedactionVerdict {
    if text.len() > REDACT_INPUT_MAX_BYTES {
        return RedactionVerdict::unavailable();
    }
    RedactionVerdict::from_findings(pattern_pass(text))
}

// ---------------------------------------------------------------------------
// The five shapes, hand-rolled (no regex crate — see the module docs).
// ---------------------------------------------------------------------------

/// A credential shape anchored on a literal prefix followed by a run of body
/// bytes — four of the five shapes have this form.
///
/// There is no per-shape left-boundary alphabet: [`is_left_boundary`] is one
/// predicate for all four, for the reason recorded there.
struct PrefixShape {
    /// The literal prefix that anchors the match.
    prefix: &'static [u8],
    /// Minimum body length for a match (the `{20,}` / `{16}` in the shape).
    min_body: usize,
    /// `Some(n)` ends the match after `n` body bytes (the exact `{16}` in
    /// `AKIA[A-Z0-9]{16}`); `None` is greedy (`{20,}`).
    body_cap: Option<usize>,
    /// Which bytes may appear in the body.
    is_body: fn(u8) -> bool,
}

/// `sk-[A-Za-z0-9_-]{20,}` — OpenAI/Anthropic-style secret keys.
fn is_sk_body(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// `AKIA[A-Z0-9]{16}` — AWS access key ids.
fn is_upper_alnum(b: u8) -> bool {
    b.is_ascii_uppercase() || b.is_ascii_digit()
}

/// `ghp_[A-Za-z0-9]{36,}` — GitHub personal access tokens.
fn is_alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}

/// `Bearer [A-Za-z0-9._-]{20,}` — an `Authorization` bearer token, including the
/// dots of a JWT.
fn is_bearer_body(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-'
}

const PREFIX_SHAPES: &[PrefixShape] = &[
    PrefixShape {
        prefix: b"sk-",
        min_body: 20,
        body_cap: None,
        is_body: is_sk_body,
    },
    PrefixShape {
        prefix: b"AKIA",
        min_body: 16,
        body_cap: Some(16),
        is_body: is_upper_alnum,
    },
    PrefixShape {
        prefix: b"ghp_",
        min_body: 36,
        body_cap: None,
        is_body: is_alnum,
    },
    PrefixShape {
        prefix: b"Bearer ",
        min_body: 20,
        body_cap: None,
        is_body: is_bearer_body,
    },
];

/// Whether a credential prefix starting at `at` begins a word — the left word
/// boundary every prefix shape requires (see [`scan_prefix_shape`]).
///
/// ## One predicate for every shape, and it is "alphanumeric"
///
/// It used to be the shape's *own body alphabet*, which is not a word boundary
/// but a statement about the alphabet, and the two came apart in both
/// directions:
///
/// - `sk-`'s body alphabet includes `-` and `_`, so a unified-diff removal line
///   (`-sk-…`) and an underscore-prefixed key (`_sk-…`) were **skipped**. Both
///   are real credentials at a real word start.
/// - `AKIA`'s body alphabet is upper-alnum, so `prefixAKIA…` — the prefix
///   glued to the tail of a lowercase word — still **matched**.
///
/// A real credential is never preceded by a letter or a digit, and `disk-` /
/// `risk-` are rejected by exactly that: the byte before the prefix is a
/// letter. So the predicate is `is_ascii_alphanumeric` for all four shapes,
/// which is what "starts a word" actually means.
///
/// ## The JSON-escape exception, and why it is not optional
///
/// This scan runs on the **serialized** outbound body (`Egress::send` scans
/// `request.body`), where a newline inside message content is not byte `0x0a`
/// but the two bytes `\` `n` — and `n` is a letter. Every credential written at
/// the start of a content line was therefore preceded by a "word byte" and
/// skipped: a JSON body with four line-start credentials in it detected only
/// the `AKIA` one, whose lowercase-`n` predecessor its old upper-alnum
/// alphabet happened to reject. That is the whole feature failing on the exact
/// shape of the payload it is installed to scan.
///
/// So a preceding byte that is the last byte of a **string escape** counts as a
/// boundary: `\n`, `\t`, `\r`, `\b`, `\f`, and `\uXXXX` when what it decodes to
/// is not itself alphanumeric — the escape for a newline is a boundary, the
/// escape for `A` is not, so an escaped `A` before `sk-…` stays mid-word and
/// stays rejected. "Escape" means an
/// **odd** run of backslashes before it — `\\nsk-…` is a literal backslash then
/// the letter `n`, which is mid-word, and this says so.
///
/// `disk-` is untouched by all of it: `i` is not one of the escape letters.
fn is_left_boundary(bytes: &[u8], at: usize) -> bool {
    if at == 0 {
        return true;
    }
    let prev = bytes[at - 1];
    if !prev.is_ascii_alphanumeric() {
        return true;
    }
    ends_a_string_escape(bytes, at - 1)
}

/// Whether the byte at `at` is the final byte of a string escape whose decoded
/// character is not alphanumeric (see [`is_left_boundary`]).
fn ends_a_string_escape(bytes: &[u8], at: usize) -> bool {
    // `\n` `\t` `\r` `\b` `\f` — two bytes, the second a letter.
    if matches!(bytes[at], b'n' | b't' | b'r' | b'b' | b'f') && is_escaped(bytes, at) {
        return true;
    }
    // `\uXXXX` — six bytes, `at` being the last hex digit.
    if at >= 5 && bytes[at - 4] == b'u' && is_escaped(bytes, at - 4) {
        if let Some(code) = hex4(&bytes[at - 3..=at]) {
            // Only what it *decodes to* decides: an escape standing for a
            // letter or a digit is a word byte like any other.
            return !(code < 128 && (code as u8).is_ascii_alphanumeric());
        }
    }
    false
}

/// Whether the byte at `at` is escaped — preceded by an **odd** run of
/// backslashes.
///
/// The walk is bounded in aggregate rather than per call: a caller only reaches
/// it when a literal prefix matched at `at + 1`, and the backslash runs behind
/// two distinct prefix occurrences cannot overlap (the escape letter and the
/// prefix itself sit between them), so every byte of the payload is walked at
/// most once across the whole scan.
fn is_escaped(bytes: &[u8], at: usize) -> bool {
    let mut backslashes = 0usize;
    let mut i = at;
    while i > 0 && bytes[i - 1] == b'\\' {
        backslashes += 1;
        i -= 1;
    }
    backslashes % 2 == 1
}

/// The value of exactly four hex digits, or `None` if they are not all hex.
fn hex4(digits: &[u8]) -> Option<u32> {
    let mut value = 0u32;
    for &byte in digits {
        value = value * 16 + char::from(byte).to_digit(16)?;
    }
    Some(value)
}

/// Collect every non-overlapping, leftmost-longest match of `shape` in `bytes`.
///
/// ## The left word boundary, and why it is not optional
///
/// A prefix these shapes anchor on is only a credential when it *starts* a
/// word. Without that check `sk-` matches inside `disk-encryption-configuration`
/// and `risk-assessment-and-mitigation`: both carry a ≥20-byte run of body
/// bytes after the `sk-`, so both scored a `High`-confidence credential and
/// **blocked the turn** — a pattern pass whose precision was supposed to be the
/// thing that keeps this feature usable (BR-4, OQ-2) refusing ordinary English.
///
/// So a match at `i` is accepted only when [`is_left_boundary`] holds there.
/// **No true positive is lost**: a real credential is preceded by a quote, an
/// `=`, a `:`, whitespace, a diff marker, an escape, or the start of the payload
/// — never by a letter or a digit, because those would be part of the word the
/// prefix belongs to.
fn scan_prefix_shape(bytes: &[u8], shape: &PrefixShape, out: &mut Vec<Range<usize>>) {
    let plen = shape.prefix.len();
    let mut i = 0usize;
    while i + plen <= bytes.len() {
        if &bytes[i..i + plen] != shape.prefix {
            i += 1;
            continue;
        }
        // The left word boundary: a prefix in the middle of a longer word is
        // that word, not a credential.
        if !is_left_boundary(bytes, i) {
            i += 1;
            continue;
        }
        let body_start = i + plen;
        // A capped shape stops at exactly `cap` body bytes, mirroring `{16}`.
        let limit = match shape.body_cap {
            Some(cap) => bytes.len().min(body_start + cap),
            None => bytes.len(),
        };
        let mut j = body_start;
        while j < limit && (shape.is_body)(bytes[j]) {
            j += 1;
        }
        if j - body_start >= shape.min_body {
            out.push(i..j);
            i = j;
        } else {
            i += 1;
        }
    }
}

/// The literal suffixes of the assignment shape
/// `[A-Z_]+_(API_KEY|TOKEN)\s*[=:]\s*\S+`.
const ENV_SUFFIXES: &[&[u8]] = &[b"_API_KEY", b"_TOKEN"];

/// A byte of the `[A-Z_]+` name that must precede the suffix.
fn is_name_byte(b: u8) -> bool {
    b.is_ascii_uppercase() || b == b'_'
}

/// A byte matching the shape's `\s` (`[\t\n\v\f\r ]`). `u8::is_ascii_whitespace`
/// omits the vertical tab, so it is added explicitly.
fn is_shape_space(b: u8) -> bool {
    b.is_ascii_whitespace() || b == 0x0b
}

/// Collect every match of `[A-Z_]+_(API_KEY|TOKEN)\s*[=:]\s*\S+`.
///
/// Anchored on the literal suffix, then extended left over the greedy `[A-Z_]+`
/// (which must contribute at least one byte, so a bare `API_KEY=x` is *not* a
/// match — the shape requires a qualified name) and right over the separator and
/// the value.
///
/// ## The left extension is memoized, and it is defence in depth rather than a
/// live exposure
///
/// Every byte of the suffix is a name byte, so a suffix occurrence always lies
/// inside a maximal `[A-Z_]+` run — and every occurrence *in the same run*
/// extends left to the same place. Re-walking the run per occurrence is
/// quadratic: `_TOKEN_TOKEN_TOKEN…` is one run with thousands of occurrences,
/// and the scan measured **123 ms** on a 64 KiB version of it.
///
/// **In production that 64 KiB input never arrives here.**
/// [`pattern_verdict`] refuses anything over [`REDACT_INPUT_MAX_BYTES`] —
/// 108,280 bytes — before this function is called, so the reachable worst case
/// is the cap, not an unbounded body an attacker sizes. Calling this "a
/// quadratic scan on the send path from a payload an attacker chooses"
/// overstated it: the cap is what bounds the payload, and it is checked first.
///
/// What remains, and is worth keeping, is the **complexity class**. A bound
/// that holds because a *different* constant happens to be small is a bound
/// that moves when that constant does — and this cap is derived from an engine
/// window, so it is exactly the kind of number a later REQ raises. The
/// memoization makes the scan amortized linear in its own right: the run
/// containing the current position is computed once and cached, `i` never
/// decreases, and runs are maximal and therefore disjoint, so each run is
/// walked at most once in each direction. Behaviour is byte-for-byte what it
/// was — the cache answers the same question, it just stops asking it
/// repeatedly.
///
/// `the_assignment_scans_left_extension_does_not_go_quadratic` therefore feeds
/// it half a mebibyte **deliberately past the cap**, calling `pattern_pass`
/// rather than `pattern_verdict`: it is measuring the class, not simulating a
/// reachable production input.
fn scan_env_assignment(bytes: &[u8], out: &mut Vec<Range<usize>>) {
    for suffix in ENV_SUFFIXES {
        let slen = suffix.len();
        let mut i = 0usize;
        // The maximal `[A-Z_]+` run last computed, as `start..end`.
        let mut run: Range<usize> = 0..0;
        while i + slen <= bytes.len() {
            if &bytes[i..i + slen] != *suffix {
                i += 1;
                continue;
            }
            // `[A-Z_]+` immediately left of the suffix's leading underscore —
            // i.e. the start of the maximal name run `i` sits in. Recomputed
            // only when `i` has left the cached run.
            if !run.contains(&i) {
                let mut start = i;
                while start > 0 && is_name_byte(bytes[start - 1]) {
                    start -= 1;
                }
                let mut end = i;
                while end < bytes.len() && is_name_byte(bytes[end]) {
                    end += 1;
                }
                run = start..end;
            }
            let start = run.start;
            if start == i {
                i += 1;
                continue;
            }
            let mut j = i + slen;
            while j < bytes.len() && is_shape_space(bytes[j]) {
                j += 1;
            }
            // `[=:]`
            if j >= bytes.len() || (bytes[j] != b'=' && bytes[j] != b':') {
                i += slen;
                continue;
            }
            j += 1;
            while j < bytes.len() && is_shape_space(bytes[j]) {
                j += 1;
            }
            // `\S+`
            let value_start = j;
            while j < bytes.len() && !is_shape_space(bytes[j]) {
                j += 1;
            }
            if j == value_start {
                i += slen;
                continue;
            }
            out.push(start..j);
            i = j;
        }
    }
}

/// Sort spans ascending and drop any span fully contained in another.
///
/// Two shapes matching the same secret (`AWS_API_KEY=AKIA…`, or
/// `Bearer sk-…`) must report *one* finding, not two — otherwise a single
/// credential inflates the report and the nested span points at part of a thing
/// already named. Sorting by start ascending then end descending means a kept
/// span always starts at or before any span that follows it, so a candidate
/// ending at or before the widest end seen so far is contained in it.
fn suppress_contained(mut spans: Vec<Range<usize>>) -> Vec<Range<usize>> {
    spans.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
    let mut kept: Vec<Range<usize>> = Vec::with_capacity(spans.len());
    let mut max_end = 0usize;
    for span in spans {
        if span.end <= max_end {
            continue;
        }
        max_end = span.end;
        kept.push(span);
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Verdict invariants — enforced by construction, not by discipline.
    // -----------------------------------------------------------------------

    fn a_high() -> Finding {
        Finding::pattern(FindingKind::Credential, 10..30)
    }

    fn a_low() -> Finding {
        Finding::model(FindingKind::Pii, 40..52)
    }

    /// The two invariants, checked as the biconditionals the spec states.
    fn assert_invariants(verdict: &RedactionVerdict) {
        assert_eq!(
            !verdict.findings().is_empty(),
            verdict.outcome() == Outcome::Findings,
            "findings must be non-empty iff the outcome is Findings"
        );
        assert_eq!(
            !verdict.scanned(),
            verdict.outcome() == Outcome::Unavailable,
            "scanned must be false iff the outcome is Unavailable"
        );
    }

    #[test]
    fn every_constructor_establishes_both_verdict_invariants() {
        let verdicts = [
            RedactionVerdict::clean(),
            RedactionVerdict::from_findings(vec![a_high()]),
            RedactionVerdict::from_findings(vec![a_low(), a_high()]),
            // The degenerate input: an empty findings list.
            RedactionVerdict::from_findings(Vec::new()),
            RedactionVerdict::unavailable(),
        ];
        for verdict in &verdicts {
            assert_invariants(verdict);
        }
    }

    #[test]
    fn an_empty_findings_list_collapses_to_clean_never_to_an_empty_findings_outcome() {
        // The discriminating pair: the same constructor, one row with findings
        // and one without. If `from_findings` ever produced `Findings` with an
        // empty vector, the first assertion fires.
        let empty = RedactionVerdict::from_findings(Vec::new());
        assert_eq!(empty.outcome(), Outcome::Clean);
        assert!(empty.scanned(), "an empty result is still a completed scan");

        let non_empty = RedactionVerdict::from_findings(vec![a_low()]);
        assert_eq!(non_empty.outcome(), Outcome::Findings);
        assert_eq!(non_empty.findings().len(), 1);
    }

    #[test]
    fn unavailable_is_the_only_verdict_that_reports_no_scan() {
        // AC-3: no construction makes the scan appear to have run when it did
        // not, and none makes a completed scan look like it did not run.
        assert!(!RedactionVerdict::unavailable().scanned());
        assert!(RedactionVerdict::clean().scanned());
        assert!(RedactionVerdict::from_findings(vec![a_high()]).scanned());
        assert!(RedactionVerdict::unavailable().findings().is_empty());
    }

    #[test]
    fn confidence_is_fixed_by_which_pass_built_the_finding() {
        // ADR-4/BR-4: derived, never self-reported. There is no constructor that
        // takes a confidence argument, so these are the only two levels
        // obtainable and each comes from exactly one pass.
        assert_eq!(
            Finding::pattern(FindingKind::Credential, 0..4).confidence(),
            Confidence::High
        );
        assert!(Finding::pattern(FindingKind::Credential, 0..4).is_high());
        assert_eq!(
            Finding::model(FindingKind::Secret, 0..4).confidence(),
            Confidence::Low
        );
        assert!(!Finding::model(FindingKind::Secret, 0..4).is_high());
    }

    #[test]
    fn finding_kinds_render_as_stable_content_free_names() {
        let names = [
            (FindingKind::Secret, "secret"),
            (FindingKind::Credential, "credential"),
            (FindingKind::Pii, "pii"),
            (FindingKind::Unknown, "unknown"),
        ];
        for (kind, expected) in names {
            assert_eq!(kind.as_str(), expected);
        }
    }

    // -----------------------------------------------------------------------
    // The pattern pass: five shapes, their near-misses, and a clean payload.
    // -----------------------------------------------------------------------

    /// One row of the pattern table. `expect` is the exact substring the pass
    /// must locate — the expected span is derived from its position in `text`,
    /// so a row pins *where* the finding is, not merely that there was one.
    struct ShapeCase {
        name: &'static str,
        text: &'static str,
        expect: Option<&'static str>,
    }

    const SHAPE_CASES: &[ShapeCase] = &[
        ShapeCase {
            name: "sk- secret key",
            text: "here is a key sk-ABCDEFGHIJKLMNOPQRSTUVWX and more prose",
            expect: Some("sk-ABCDEFGHIJKLMNOPQRSTUVWX"),
        },
        ShapeCase {
            name: "AKIA access key id",
            text: "aws id AKIAIOSFODNN7EXAMPLE trailing text",
            expect: Some("AKIAIOSFODNN7EXAMPLE"),
        },
        ShapeCase {
            name: "ghp_ personal access token",
            text: "token ghp_abcdefghijklmnopqrstuvwxyz0123456789 end",
            expect: Some("ghp_abcdefghijklmnopqrstuvwxyz0123456789"),
        },
        ShapeCase {
            name: "Bearer authorization token",
            text: "authorization: Bearer eyJhbGciOiJIUzI1NiJ9.abc",
            expect: Some("Bearer eyJhbGciOiJIUzI1NiJ9.abc"),
        },
        ShapeCase {
            name: "*_API_KEY assignment",
            text: "export AWS_API_KEY=wJalrXUtnFEMI/K7MDENG",
            expect: Some("AWS_API_KEY=wJalrXUtnFEMI/K7MDENG"),
        },
        ShapeCase {
            name: "*_TOKEN assignment with a colon and spaces",
            text: "config GITHUB_TOKEN : hunter2seventeen",
            expect: Some("GITHUB_TOKEN : hunter2seventeen"),
        },
        // The clean row runs through the *same* function as every row above.
        // That is the non-vacuity: the call that returns nothing here is the
        // call that returned a located span there, so "found nothing" cannot be
        // "was never invoked" (LESSON-485).
        ShapeCase {
            name: "clean prose",
            text: "the quick brown fox reads src/main.rs and returns ok",
            expect: None,
        },
        // Near-misses: each is one shape with its run length below the minimum,
        // which is the state that discriminates "the pass matches this shape"
        // from "the pass matches anything vaguely similar".
        ShapeCase {
            name: "sk- below the 20-byte minimum",
            text: "sk-tooshort here",
            expect: None,
        },
        ShapeCase {
            name: "AKIA below the 16-byte body",
            text: "AKIA123 here",
            expect: None,
        },
        ShapeCase {
            name: "ghp_ below the 36-byte body",
            text: "ghp_abcdefghijklmnop here",
            expect: None,
        },
        ShapeCase {
            name: "Bearer below the 20-byte token",
            text: "Bearer shorttoken",
            expect: None,
        },
        ShapeCase {
            name: "assignment with no value",
            text: "AWS_API_KEY=",
            expect: None,
        },
        ShapeCase {
            name: "unqualified API_KEY has no [A-Z_]+ prefix",
            text: "API_KEY=value",
            expect: None,
        },
        // The left word boundary. Both of these carry a ≥20-byte run of `sk-`
        // body bytes after an `sk-` that is the tail of an ordinary English
        // word, and both blocked turns before the boundary check existed.
        ShapeCase {
            name: "sk- inside `disk-` is a word, not a key",
            text: "we should review the disk-encryption-configuration before shipping",
            expect: None,
        },
        ShapeCase {
            name: "sk- inside `risk-` is a word, not a key",
            text: "see the risk-assessment-and-mitigation doc",
            expect: None,
        },
        // The positive twin, and the reason the boundary check loses no true
        // positive: a real key is preceded by a quote, an `=`, a `:` or
        // whitespace — never by a letter or a digit.
        ShapeCase {
            name: "sk- after an equals and a quote still matches",
            text: "let key = \"sk-ABCDEFGHIJKLMNOPQRSTUVWX\";",
            expect: Some("sk-ABCDEFGHIJKLMNOPQRSTUVWX"),
        },
        // The rows the *alphabet-relative* boundary got wrong in both
        // directions. `-` and `_` are `sk-` body bytes, so a unified-diff
        // removal line and an underscore-prefixed name were skipped; and
        // `AKIA`'s upper-alnum alphabet let a lowercase word's tail through.
        ShapeCase {
            name: "a diff removal line's `-` is a boundary, not a body byte",
            text: "-sk-ABCDEFGHIJKLMNOPQRSTUVWX",
            expect: Some("sk-ABCDEFGHIJKLMNOPQRSTUVWX"),
        },
        ShapeCase {
            name: "an underscore before the prefix is a boundary too",
            text: "_sk-ABCDEFGHIJKLMNOPQRSTUVWX",
            expect: Some("sk-ABCDEFGHIJKLMNOPQRSTUVWX"),
        },
        ShapeCase {
            name: "AKIA glued to a lowercase word is that word's tail",
            text: "prefixAKIAIOSFODNN7EXAMPLE",
            expect: None,
        },
    ];

    #[test]
    fn the_pattern_pass_locates_each_shape_and_nothing_else() {
        for case in SHAPE_CASES {
            let findings = pattern_pass(case.text);
            match case.expect {
                Some(expected) => {
                    assert_eq!(
                        findings.len(),
                        1,
                        "{}: expected exactly one finding, got {findings:?}",
                        case.name
                    );
                    let start = case
                        .text
                        .find(expected)
                        .unwrap_or_else(|| panic!("{}: fixture is malformed", case.name));
                    assert_eq!(
                        *findings[0].span(),
                        start..start + expected.len(),
                        "{}: span must cover exactly the credential",
                        case.name
                    );
                    // Confidence and kind are structural for this pass.
                    assert_eq!(findings[0].confidence(), Confidence::High, "{}", case.name);
                    assert_eq!(findings[0].kind(), FindingKind::Credential, "{}", case.name);
                }
                None => assert!(
                    findings.is_empty(),
                    "{}: expected no finding, got {findings:?}",
                    case.name
                ),
            }
        }
    }

    /// **Every prefix shape requires a left word boundary**, and the same
    /// prefix one byte to the right of a separator still matches.
    ///
    /// Each row is a pair over the *same* prefix and the *same* body run: the
    /// only difference between the negative and the positive is the byte
    /// immediately left of the prefix. That is what makes this a discrimination
    /// rather than four assertions that the pass sometimes finds nothing —
    /// deleting the boundary check turns every negative red while every
    /// positive stays green (LESSON-485).
    #[test]
    fn every_prefix_shape_requires_a_left_word_boundary() {
        struct BoundaryCase {
            /// The prefix, embedded mid-word: must NOT match.
            mid_word: &'static str,
            /// The same prefix and body after a separator: must match.
            at_boundary: &'static str,
            /// The exact credential `at_boundary` carries.
            expect: &'static str,
        }

        const CASES: &[BoundaryCase] = &[
            BoundaryCase {
                mid_word: "the disk-encryption-configuration file",
                at_boundary: "key=sk-encryption-configuration-x",
                expect: "sk-encryption-configuration-x",
            },
            BoundaryCase {
                mid_word: "PREFIXAKIAIOSFODNN7EXAMPLE",
                at_boundary: "PREFIX AKIAIOSFODNN7EXAMPLE",
                expect: "AKIAIOSFODNN7EXAMPLE",
            },
            BoundaryCase {
                mid_word: "xghp_abcdefghijklmnopqrstuvwxyz0123456789",
                at_boundary: "x ghp_abcdefghijklmnopqrstuvwxyz0123456789",
                expect: "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            },
            BoundaryCase {
                mid_word: "xBearer eyJhbGciOiJIUzI1NiJ9.abc",
                at_boundary: "x Bearer eyJhbGciOiJIUzI1NiJ9.abc",
                expect: "Bearer eyJhbGciOiJIUzI1NiJ9.abc",
            },
        ];

        for case in CASES {
            assert!(
                pattern_pass(case.mid_word).is_empty(),
                "a prefix inside a longer word is that word, not a credential: {:?} \
                 produced {:?}",
                case.mid_word,
                pattern_pass(case.mid_word)
            );
            let found = pattern_pass(case.at_boundary);
            assert_eq!(
                found.len(),
                1,
                "the twin must still match: {:?} produced {found:?}",
                case.at_boundary
            );
            let start = case
                .at_boundary
                .find(case.expect)
                .expect("the fixture is malformed");
            assert_eq!(*found[0].span(), start..start + case.expect.len());
        }
    }

    /// **The boundary survives JSON escaping** — the shape this scan actually
    /// meets in production.
    ///
    /// `Egress::send` scans `request.body`, which is a *serialized* request: a
    /// newline inside message content is not byte `0x0a` but the two bytes
    /// backslash + `n`, and `n`/`t`/`r`/`b`/`f` are all letters. Under the old
    /// alphabet-relative boundary every credential written at the start of a
    /// content line was preceded by a "word byte" and skipped — this body
    /// carries four and only the `AKIA` one was found, because its upper-alnum
    /// alphabet happened to reject a lowercase `n`.
    ///
    /// So the fixture is serialized with `serde_json` rather than hand-written:
    /// what is scanned is the encoding an adapter really produces.
    #[test]
    fn a_credential_at_the_start_of_a_json_encoded_line_is_still_found() {
        const CONTENT: &str = "here is the deploy config:\n\
                               sk-ABCDEFGHIJKLMNOPQRSTUVWX\n\
                               AKIAIOSFODNN7EXAMPLE\n\
                               ghp_abcdefghijklmnopqrstuvwxyz0123456789\n\
                               Bearer eyJhbGciOiJIUzI1NiJ9.abcdefghij\n";
        let body = serde_json::json!({
            "model": "claude-opus-4",
            "messages": [{ "role": "user", "content": CONTENT }],
        });
        let serialized = serde_json::to_string(&body).expect("a serializable body");

        // Non-vacuity: the fixture really is the escaped shape. If serialization
        // ever stopped escaping newlines, the rows below would be testing the
        // raw-text case that already worked.
        assert!(
            !serialized.contains('\n'),
            "a serialized body is one line; its content newlines are escapes"
        );
        assert!(
            serialized.contains(r"\nsk-"),
            "the fixture must put a credential immediately after an escape: {serialized}"
        );

        let expected = [
            "sk-ABCDEFGHIJKLMNOPQRSTUVWX",
            "AKIAIOSFODNN7EXAMPLE",
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            "Bearer eyJhbGciOiJIUzI1NiJ9.abcdefghij",
        ];
        let findings = pattern_pass(&serialized);
        assert_eq!(
            findings.len(),
            expected.len(),
            "every shape at a JSON line start must be found: {findings:?}"
        );
        for (finding, needle) in findings.iter().zip(expected) {
            let at = serialized.find(needle).expect("the fixture carries it");
            assert_eq!(
                *finding.span(),
                at..at + needle.len(),
                "{needle}: the span must cover exactly the credential"
            );
        }
    }

    /// The escape exception is about what the escape **decodes to**, and about
    /// the backslash really being an escape rather than a literal.
    ///
    /// Each row is the same prefix and the same body run; only the two bytes in
    /// front of it change. That is what keeps the exception from being "a
    /// backslash anywhere nearby makes it a credential".
    #[test]
    fn only_an_escape_that_decodes_to_a_non_word_character_is_a_boundary() {
        const KEY: &str = "sk-ABCDEFGHIJKLMNOPQRSTUVWX";

        for escape in ["n", "t", "r", "b", "f"] {
            let text = format!("word\\{escape}{KEY}");
            assert_eq!(
                pattern_pass(&text).len(),
                1,
                "an escaped whitespace byte is a line start, not a word: {text}"
            );
        }
        // An EVEN run of backslashes is a literal backslash followed by the
        // letter `n` — mid-word, and the credential shape does not start there.
        let literal = format!("word\\\\n{KEY}");
        assert!(
            pattern_pass(&literal).is_empty(),
            "a literal backslash then `n` is a word byte: {literal}"
        );
        // And an odd run is an escape again, however long.
        let odd = format!("word\\\\\\n{KEY}");
        assert_eq!(pattern_pass(&odd).len(), 1, "{odd}");

        // The six-byte escape. Written through `format!` so the four hex digits
        // are the only thing that varies between the rows.
        let hex_escape = |hex: &str| format!("word\\u{hex}{KEY}");
        assert_eq!(
            pattern_pass(&hex_escape("000a")).len(),
            1,
            "the escape for a newline is a boundary"
        );
        assert_eq!(
            pattern_pass(&hex_escape("2014")).len(),
            1,
            "the escape for an em dash is a boundary"
        );
        assert!(
            pattern_pass(&hex_escape("0041")).is_empty(),
            "the escape for `A` decodes to a word byte, so this is mid-word"
        );
        assert!(
            pattern_pass(&hex_escape("zzzz")).is_empty(),
            "four non-hex bytes are not an escape at all"
        );
    }

    #[test]
    fn a_clean_payload_yields_a_scanned_clean_verdict_not_a_skipped_one() {
        // AC-2's non-vacuity pairing: the clean payload's verdict must prove the
        // pass RAN (`scanned: true`, outcome Clean) rather than being the
        // "could not look" state, and the identical entry point must still find
        // a planted credential.
        let clean = pattern_verdict("the quick brown fox reads src/main.rs and returns ok");
        assert_eq!(clean.outcome(), Outcome::Clean);
        assert!(clean.scanned(), "a clean verdict must claim the scan ran");
        assert_eq!(decide(&clean), EgressDecision::Forward);

        let dirty = pattern_verdict("prose then sk-ABCDEFGHIJKLMNOPQRSTUVWX");
        assert_eq!(dirty.outcome(), Outcome::Findings);
        assert!(dirty.scanned());
        assert_eq!(decide(&dirty), EgressDecision::Block);
    }

    #[test]
    fn a_multibyte_assignment_value_still_slices() {
        // The half the "every shape is pure ASCII" claim got wrong: the
        // assignment shape's `\S+` arm consumes continuation bytes, so this
        // match genuinely spans multi-byte content. It stays sliceable because
        // the run stops at an ASCII whitespace byte (or the end), never inside
        // a sequence.
        let text = "export SOME_API_KEY=café-au-lait☕ and then more prose";
        let findings = pattern_pass(text);
        assert_eq!(findings.len(), 1, "{findings:?}");
        let span = findings[0].span().clone();
        // Would panic if either offset were not a char boundary.
        assert_eq!(&text[span.clone()], "SOME_API_KEY=café-au-lait☕");
        assert!(
            text[span].len() > "SOME_API_KEY=cafe-au-lait".len(),
            "the fixture must really carry multi-byte bytes inside the span"
        );
    }

    #[test]
    fn spans_are_byte_offsets_that_stay_on_char_boundaries() {
        // Every shape is ASCII, and an ASCII byte never occurs inside a
        // multi-byte UTF-8 sequence — so a span derived from a byte scan must
        // still slice a payload containing multi-byte characters.
        let text = "héllo wörld sk-ABCDEFGHIJKLMNOPQRSTUVWX ünd mehr";
        let findings = pattern_pass(text);
        assert_eq!(findings.len(), 1);
        let span = findings[0].span().clone();
        let expected = "sk-ABCDEFGHIJKLMNOPQRSTUVWX";
        assert_eq!(
            span,
            text.find(expected).unwrap()..text.find(expected).unwrap() + expected.len()
        );
        // Would panic if the offsets were not char boundaries.
        assert_eq!(&text[span], expected);
    }

    #[test]
    fn one_secret_matched_by_two_shapes_reports_one_finding() {
        // `AWS_API_KEY=AKIA…` matches the assignment shape and the AWS shape.
        // The nested span is dropped so a single credential is named once.
        let text = "AWS_API_KEY=AKIAIOSFODNN7EXAMPLE";
        let findings = pattern_pass(text);
        assert_eq!(findings.len(), 1, "got {findings:?}");
        assert_eq!(*findings[0].span(), 0..text.len());
    }

    #[test]
    fn several_distinct_credentials_are_reported_in_ascending_span_order() {
        let text = "AKIAIOSFODNN7EXAMPLE and sk-ABCDEFGHIJKLMNOPQRSTUVWX and more";
        let findings = pattern_pass(text);
        assert_eq!(findings.len(), 2, "got {findings:?}");
        assert!(
            findings[0].span().start < findings[1].span().start,
            "findings must be ordered by position"
        );
        assert_eq!(*findings[0].span(), 0..20);
        let sk = text.find("sk-").unwrap();
        assert_eq!(*findings[1].span(), sk..text.len() - " and more".len());
    }

    /// **The assignment scan's left extension is bounded** — a payload that is
    /// one enormous `[A-Z_]+` run full of suffix occurrences does not cost
    /// quadratic time.
    ///
    /// The fixture is a single unbroken name run of `_TOKEN` repeated. Half a
    /// mebibyte of it is ~87,000 suffix occurrences in **one** run: linear it is
    /// milliseconds, quadratic it is on the order of 10^10 byte comparisons.
    ///
    /// **It is deliberately past the input cap, and calls `pattern_pass` rather
    /// than `pattern_verdict` to get there.** In production a payload this size
    /// is refused by [`REDACT_INPUT_MAX_BYTES`] before this scan runs at all, so
    /// this is not a reachable input — which is the point. The claim under test
    /// is the *complexity class* of the scan itself, which has to hold on its
    /// own rather than because a cap derived from an engine window happens to
    /// be small today. A later REQ that raises the cap must not silently
    /// reintroduce a quadratic scan.
    ///
    /// The wall-clock bound is a complexity-class discriminator, not a
    /// performance target. Measured, debug build: **0.04 s memoized, 52.9 s
    /// with the memoization deleted** — three orders of magnitude between pass
    /// and fail, so the 10-second bound is nowhere near either. This is not the
    /// flaky shape LESSON-450 warns about: nothing is polled and nothing is
    /// raced, it is one pure call over a fixed input.
    #[test]
    fn the_assignment_scans_left_extension_does_not_go_quadratic() {
        let adversarial = "A".to_owned() + &"_TOKEN".repeat(87_000);
        assert!(adversarial.len() > 500 * 1024, "{}", adversarial.len());
        assert!(
            adversarial.len() > REDACT_INPUT_MAX_BYTES,
            "the fixture bypasses the cap on purpose: this measures the scan's \
             complexity class, not a payload production can deliver"
        );
        assert!(
            adversarial.bytes().all(is_name_byte),
            "the fixture must be ONE unbroken name run, or it bounds nothing"
        );

        let started = std::time::Instant::now();
        let findings = pattern_pass(&adversarial);
        let elapsed = started.elapsed();

        // No `[=:]` follows any occurrence, so the shape never completes.
        assert!(findings.is_empty(), "{findings:?}");
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "the left extension went quadratic: {elapsed:?}"
        );
    }

    #[test]
    fn the_pattern_pass_is_deterministic() {
        let text =
            "AWS_API_KEY=wJalrXUtnFEMI and AKIAIOSFODNN7EXAMPLE and Bearer eyJhbGciOiJIUzI1NiJ9";
        let first = pattern_pass(text);
        for _ in 0..8 {
            assert_eq!(pattern_pass(text), first);
        }
    }

    #[test]
    fn a_finding_never_carries_the_matched_text() {
        // BR-6 / ADR-5, and AC-8's mutation (c): adding a text field to
        // `Finding` — even a private one — turns this red, because `Debug` is
        // derived and would start rendering the secret.
        const SENTINEL: &str = "sk-ZZQUUXSENTINELCREDENTIAL0123";
        let verdict = pattern_verdict(&format!("please summarize {SENTINEL} for me"));
        // Non-vacuity: the sentinel really was detected, so the absence below is
        // about the type and not about an empty result.
        assert_eq!(verdict.outcome(), Outcome::Findings);
        assert_eq!(verdict.findings().len(), 1);

        let rendered = format!("{verdict:?}");
        assert!(
            !rendered.contains("QUUXSENTINEL"),
            "a finding must never carry the matched text: {rendered}"
        );
        assert!(
            !rendered.contains(SENTINEL),
            "a finding must never carry the matched text: {rendered}"
        );
    }

    // -----------------------------------------------------------------------
    // The input cap (BR-7, ADR-6).
    // -----------------------------------------------------------------------

    /// **The per-chunk cap and the engine window are one budget**
    /// (LESSON-446), and the thing measured against it is what the engine is
    /// **handed** (LESSON-488).
    ///
    /// The claim that matters is the inequality, not the number: a chunk **at**
    /// [`REDACT_CHUNK_MAX_BYTES`] must still fit inside what the local engine
    /// will accept, which is `(n_ctx - max_tokens)` tokens, read at the seam's
    /// two-bytes-per-token convention. That is what makes the chunker's window
    /// the right size rather than a number beside the right one — every chunk
    /// the model pass cuts is one of these.
    ///
    /// Every assertion below is on the **rendered** string
    /// ([`rendered_prompt_bytes`](crate::harness::redact::rendered_prompt_bytes)),
    /// not on `redact_prompt`'s output. Asserting on the prompt was the round-1
    /// mistake: `render_duty` runs afterwards and adds the ChatML envelope and
    /// any control-token defusing, so a prompt that fit could still be handed
    /// to the engine over-window — the exact failure the derivation exists to
    /// remove, one transform further down.
    ///
    /// Before this derivation the cap was a flat 64 KiB against a 30,720-byte
    /// prompt budget, so every payload from ~30 KiB to 64 KiB passed the cap and
    /// was then refused by the engine as over-window — a block that said "the
    /// scan could not run" when the true reason was "this payload is too large".
    /// Setting `REDACT_CHUNK_MAX_BYTES` back to `64 * 1024` turns this red.
    #[test]
    fn a_chunk_at_the_per_chunk_cap_still_fits_the_engines_window() {
        use crate::harness::redact::{redact_prompt, rendered_prompt_bytes};

        let budget = (LOCAL_ENGINE_N_CTX as usize - REDACT_DUTY.max_tokens() as usize)
            * DUTY_BYTES_PER_TOKEN;
        assert_eq!(
            REDACT_PROMPT_BUDGET_BYTES, budget,
            "the budget is n_ctx minus the duty's generation reservation"
        );
        // The worst case the ARITHMETIC covers, not the typical one: the prompt
        // builder defuses line-anchored frame labels inside the payload
        // (ADR-009), which is an insertion, and the renderer adds a fixed
        // envelope. Both are terms in the cap's derivation.
        let worst_case_growth = REDACT_CHUNK_MAX_BYTES / REDACT_DEFUSE_GROWTH_DIVISOR + 1;
        assert!(
            REDACT_CHUNK_MAX_BYTES
                + worst_case_growth
                + REDACT_PROMPT_OVERHEAD_BYTES
                + CHATML_DUTY_ENVELOPE_BYTES
                <= budget,
            "a payload at the cap, worst-case defused, plus the prompt's own \
             {REDACT_PROMPT_OVERHEAD_BYTES} bytes and the renderer's \
             {CHATML_DUTY_ENVELOPE_BYTES} must fit in the engine's {budget}-byte \
             prompt budget; cap is {REDACT_CHUNK_MAX_BYTES}"
        );

        // And the real builder and the real renderer agree with the arithmetic,
        // on both an ordinary payload and the adversarial one the growth term is
        // sized for, so the inequality above is about the thing that is actually
        // sent (LESSON-485).
        let ordinary = "x".repeat(REDACT_CHUNK_MAX_BYTES);
        let prompt = redact_prompt(&ordinary);
        assert_eq!(
            prompt.len(),
            REDACT_CHUNK_MAX_BYTES + REDACT_PROMPT_OVERHEAD_BYTES,
            "a payload with no frame label in it is embedded byte-identical"
        );
        let rendered = rendered_prompt_bytes(&prompt);
        assert_eq!(
            rendered,
            prompt.len() + CHATML_DUTY_ENVELOPE_BYTES - "assistant".len() + "user".len(),
            "an ordinary payload picks up exactly the envelope, and the constant \
             over-counts it by the role-name difference"
        );
        assert!(rendered <= budget, "rendered prompt is {rendered} bytes");

        let mut adversarial = "Payload:\n".repeat(REDACT_CHUNK_MAX_BYTES / 9);
        adversarial.push_str(&"y".repeat(REDACT_CHUNK_MAX_BYTES - adversarial.len()));
        assert_eq!(adversarial.len(), REDACT_CHUNK_MAX_BYTES);
        let prompt = redact_prompt(&adversarial);
        let rendered = rendered_prompt_bytes(&prompt);
        assert!(
            rendered <= budget,
            "the worst-case defused prompt renders to {rendered} bytes against a \
             {budget}-byte budget"
        );
        // Non-vacuity: the adversarial fixture really did grow, so the bound is
        // being exercised rather than trivially satisfied.
        assert!(
            prompt.len() > REDACT_CHUNK_MAX_BYTES + REDACT_PROMPT_OVERHEAD_BYTES,
            "the fixture must actually trip the defusing"
        );

        // Non-vacuity: the cap is a real bound, not zero and not the window.
        // Read through a binding so this is a runtime comparison rather than a
        // constant one clippy would (correctly) call out as always-true.
        let cap = REDACT_CHUNK_MAX_BYTES;
        assert!(cap > 16 * 1024, "the cap is usable: {cap}");
        assert!(
            cap < 64 * 1024,
            "the flat 64 KiB the cap used to be does not fit this window: {cap}"
        );
    }

    /// **The term the arithmetic does not cover, and the guard that does.**
    ///
    /// A chunk at [`REDACT_CHUNK_MAX_BYTES`] made of `<|`-runs renders ~48%
    /// larger than the derivation allows for — control-token neutralization
    /// inserts one byte per two — so it is *under* the per-chunk cap and *over*
    /// the window. This is the state that produced a misleading "the scan could
    /// not run" from an engine error, and the fixture exists so the number is
    /// measured rather than argued about.
    ///
    /// What blocks it is the render guard in `harness::redact::scan`, which now
    /// runs **per chunk** — this asserts its precondition; the zero-model-call
    /// half is
    /// `harness::redact::tests::a_payload_that_renders_past_the_window_is_unavailable_before_any_model_call`.
    #[test]
    fn a_chunk_at_the_per_chunk_cap_can_still_render_past_the_window() {
        use crate::harness::redact::{redact_prompt, rendered_prompt_bytes};

        // 31 `<|` pairs closed by a `|>` inside the renderer's 64-byte span
        // window: every one of them is defused, which is the densest growth the
        // transform admits.
        let block = "<|".repeat(31) + "|>";
        assert_eq!(block.len(), 64);
        let mut adversarial = block.repeat(REDACT_CHUNK_MAX_BYTES / 64);
        adversarial.push_str(&"z".repeat(REDACT_CHUNK_MAX_BYTES - adversarial.len()));
        assert_eq!(adversarial.len(), REDACT_CHUNK_MAX_BYTES);

        // Under the cap — so the cheap first filter passes it.
        assert_eq!(
            pattern_verdict(&adversarial).outcome(),
            Outcome::Clean,
            "the fixture must pass the input cap, or it tests the wrong guard"
        );
        // And over the window once rendered.
        let rendered = rendered_prompt_bytes(&redact_prompt(&adversarial));
        assert!(
            rendered > REDACT_PROMPT_BUDGET_BYTES,
            "the fixture must really render past the {REDACT_PROMPT_BUDGET_BYTES}-byte \
             budget, or the guard behind it is untested; it rendered to {rendered}"
        );
    }

    /// **The total cap clears the harness's own context budget with room to
    /// spare** — the collision ADR-6 recorded as deliberate follow-up, closed
    /// rather than measured.
    ///
    /// `HarnessConfig::context_budget_bytes` bounds the *context* a turn
    /// assembles; the body that reaches the wire is that plus the system prompt
    /// (which carries every exposed tool's description) plus the JSON envelope
    /// and the escaping of the context itself. The cap has to clear all of it,
    /// and clear it by enough that a later bump to the context budget does not
    /// quietly restore the old failure.
    ///
    /// Two claims, and the first is the one that can rot:
    ///
    /// 1. [`REDACT_BODY_OVERHEAD_BYTES`] really does cover the non-context part
    ///    — measured from the real
    ///    [`build_system_prompt`](crate::harness::turn_loop::build_system_prompt)
    ///    with every builtin registered and the tool cap lifted, plus a tenth of
    ///    the budget for escaping. An assumption nobody checks is a number that
    ///    drifts.
    /// 2. The total cap is at least **twice** that whole body.
    ///
    /// Since REQ-572 the prompt's capability clause is **per state**, and the
    /// states are different lengths — so the measurement is the worst of them,
    /// swept rather than sampled. The failure message states the remaining
    /// margin, because "it fits" and "it fits by nine bytes" are different
    /// facts, and only one of them survives the next sentence somebody adds to
    /// the bundled guide (AC-9).
    ///
    /// **Recorded headroom at REQ-577:** the worst prompt is 5,847 bytes, so
    /// `spent` is 9,123 against a 9,216-byte overhead — **93 bytes of margin**
    /// over the 48-byte floor. It was 67 before this REQ, against an 8 KiB
    /// overhead; `teton_docs`'s 274 bytes of tool docs did not fit in the 19
    /// bytes that left, which is why the assumption moved rather than the floor.
    /// TASK-144 then spent 493 of the 817 that bought: 379 bytes of vendor
    /// recipe line and a 114-byte referral sentence in the bundled guide, which
    /// left 324. TASK-147's live A/B spent 95 more on the routing step's tier
    /// purposes: the guide named the four tiers and said what none of them was
    /// *for*, and the model composed "deep reasoning" onto `reflex` in 4 of 4
    /// trials, handing the user a command that binds their think-tier provider
    /// to the always-on duties, leaving 229.
    ///
    /// The phase-5 endpoint correction spent 136 of that 229. Every recipe
    /// endpoint gained the request path Teton actually POSTs to
    /// (`/chat/completions`, or `/v1/messages` for Anthropic, which had no
    /// endpoint at all), and the step's own shape line lost the two-form
    /// `anthropic`-without-`--endpoint` spelling that was wrong. The bytes were
    /// paid out of the margin rather than out of the assumption: **the 9,216 is
    /// not to be raised again to make room for prose** — the ADR-2 fallback
    /// (recipes move into the `providers` topic, which is a tool result rather
    /// than resident prompt) is what the next sentence buys itself with, and 93
    /// bytes is close enough to the floor that the next one probably has to.
    ///
    /// **Recorded headroom at REQ-579:** the worst prompt is 5,868 bytes,
    /// `spent` is 9,144, and the margin is **72**. The guide gained the
    /// `/provider setup` hand-off (BR-1, ADR-3) and grew **21** bytes net doing
    /// it, 2,369 → 2,390 on the file: the credential sentence shrank (168 →
    /// 140), the "you cannot run these commands" line shrank (113 → 62), and
    /// step 1 took the hand-off and grew (737 → 837). It was paid the way the
    /// paragraph above says it must be — out of the margin, with every sentence
    /// shortened until it fit — rather than out of the 9,216. The opted-in web
    /// shape moved the same 21, 140 → 119, and stays the looser of the two.
    /// Twenty-four bytes above the floor now: the ADR-2 fallback is no longer a
    /// prediction about the next sentence, it is nearly the only room left.
    ///
    /// **Recorded headroom at REQ-581:** the worst prompt is 5,890 bytes,
    /// `spent` is 9,166, and the margin is **50** — two bytes above the floor.
    /// The guide grew **22** bytes net, 2,390 → 2,412, naming
    /// `/provider test <id>` in the inspect step (BR-6, AC-8a): the step's own
    /// prose paid most of it back (`Inspect with` → `Inspect:`, `Config is …
    /// state directory` → `Config: … state dir`), so one clause of new fact
    /// cost 22 rather than 41. What it does **not** carry is the shell form or
    /// the "do not probe with shell commands" sentence the task asked for —
    /// together another 60-odd bytes, which do not exist here. Those live in
    /// the CLI surface line (`session_ui::CONNECTION_TEST_LINE`, ADR-4), which
    /// costs no prompt bytes and is where LESSON-532 says the guarantee belongs.
    ///
    /// So the paragraph above is now the operative one rather than a warning:
    /// **the next sentence has to buy itself with the ADR-2 fallback** — depth
    /// into a `teton_docs` topic, a resident line only for the fact a model must
    /// have without a tool call. Two bytes is not room for a clause, and neither
    /// this floor nor the 9,216 is to be moved to make some.
    ///
    /// The figures are the guide **as it ships** — round 2's wording,
    /// `verification.md` §24 — not TASK-156's intermediate 2,402-byte draft, on
    /// which an earlier version of this note was measured. And what the extra
    /// bytes did *not* buy is on the record too: three live A/B rounds, 0/9
    /// replies volunteering the command (`verification.md` §4/§14/§19). ADR-9
    /// moved that guarantee onto the CLI surface, where it costs zero prompt
    /// bytes — the reason the next person here should not expect to buy
    /// behaviour with this margin.
    ///
    /// **Recorded headroom at REQ-583:** the worst prompt is 5,891 bytes,
    /// `spent` is 9,167, and the margin is **49** — one byte above the floor,
    /// with the floor and the 9,216 exactly where they were. This is the
    /// paragraph above being followed rather than worked around. The prompt
    /// gained the environment block (BR-1, ADR-2): one line after the opener —
    /// `Session root: <display> (project <name>, branch <branch>). Platform:
    /// macOS.` — which is precisely "the fact a model must have without a tool
    /// call", because the tools are jailed to the answer. Its worst case is
    /// **203 bytes**: a 200-character root elided to the 80-character display
    /// ceiling, a name and a branch elided to 32, kind `project` (the longest
    /// phrase), which is the row this sweep and its twin now cross with every
    /// capability state (`turn_loop::worst_case_session_root`). It was bought
    /// with **234 bytes of guide**, 2,406 → 2,172 on the file, none of it an
    /// instruction (ASSUME-008's split): the web paragraph's *reference data*
    /// — the `[web]` key list, the keychain-reference example, the "search also
    /// needs the local model" sentence — went behind "the rest: `teton_docs
    /// web`" (179 bytes; the `web` topic already carried every fact, so
    /// nothing was added to it), and step 2's `--fallback`/`set-category`
    /// clause was compressed to `[--fallback <id>]` inside the `set-tier`
    /// command's own syntax and "`set-category` binds a category" (55 bytes;
    /// the `policy` topic carries both in full). Every pinned string stayed:
    /// the three auth templates and the keyless SearxNG line, the recipes and
    /// their URLs, the inspect step's spellings, step 1's hand-off, the
    /// prohibition and the tier purposes. The same trim also paid for the 32
    /// bytes TASK-176's reworded tool descriptions ("repository" → "session
    /// root") had already spent — 5,890 → 5,922 left this test **red at 18**
    /// before this task, by design (ADR-2: measure the docs actually in the
    /// tree, then pay), and it was paid the same way rather than by moving
    /// either number. The opted-in web shape is 5,844 / 9,120 / **96**, and
    /// stays the looser of the two.
    ///
    /// **Re-measured at the merged tip (TASK-180): 5,891 / 9,167 / margin
    /// 49, unchanged** — the integration task added no resident byte, and the
    /// opted-in twin is likewise still 5,844 / 9,120 / **96**. What did change
    /// is the row's standing. TASK-177 recorded that `bounded_field` bounded
    /// *characters* while this ceiling counts bytes, so an all-multibyte root
    /// (up to four bytes a character) rendered longer than the 200-character
    /// ASCII row AC-4 names — 240 bytes of display against the row's 82. That
    /// is closed in `teton-core`, where the bound belongs: every value in the
    /// block is also held to `byte_ceiling(max_chars)` bytes — the cost of an
    /// ASCII value cut to the character ceiling, `max_chars + 2` — eliding
    /// further at character boundaries around one mark, so the row's three
    /// values sit exactly at their byte ceilings and no script can render past
    /// them (`turn_loop::the_worst_case_root_is_the_byte_worst_for_multibyte_roots_too`
    /// drives a 200-character CJK path and 33-character CJK name and branch,
    /// and their astral-plane twins, through the block and asserts none renders
    /// longer than the row). The bound was chosen at the ASCII cost rather than
    /// wider — twice the character ceiling, say — because a wider one would
    /// have made the row 138 bytes longer, and one byte of margin does not pay
    /// for that; the ASCII rendering is what it always was. And one byte is
    /// still not room for a clause: the next resident sentence buys itself the
    /// way this one did, out of a topic.
    ///
    /// **Re-measured after the verify pass (finding S): 5,891 / 9,167 / margin
    /// 49, unchanged**, and the opted-in twin still 5,844 / 9,120 / **96** —
    /// the row is byte-identical. What moved is *where* the byte bound lives:
    /// it is now `bounded_field_bytes`, applied inside `environment_block`
    /// alone (display, name and branch, before the shared kind phrase is
    /// built), because the resident prompt is the one surface a root value is
    /// paid for in bytes. `bounded_field` — the probe's, the CLI banner's, the
    /// notice's, `/cd`'s and every jail refusal's — went back to bounding
    /// characters (plus control-character *and* bidi/zero-width
    /// neutralisation), so a person reading a CJK path sees its full eighty
    /// characters rather than a third of them. The multibyte test
    /// (`turn_loop::tests::the_worst_case_root_is_the_byte_worst_for_multibyte_roots_too`)
    /// now asserts on `environment_block(..).len()` and pins the row's exact
    /// byte cost, not the probe's strings; and this sweep now checks that the widest
    /// prompt it measured carries `Session root: ` at all, so the row cannot be
    /// dropped and leave the sweep passing on the smaller shape.
    ///
    /// **Recorded headroom at REQ-585:** the worst prompt is 6,138 bytes,
    /// `spent` is 9,414, and the margin is **826** — against BUG-181's
    /// 10,240-byte overhead, with the floor still 48 and the overhead still
    /// 10 KiB (neither moved for REQ-585 or REQ-586). Every figure above this
    /// paragraph predates BUG-181 and was measured against the 9,216 it
    /// replaced.
    ///
    /// REQ-585 spent **68** of it. Fifty-two went on one sentence: BUG-181's
    /// guide line said Teton loads nothing from `.claude/`, which REQ-585 made
    /// false, so BR-9 amended it to name what *is* loaded (skills and commands)
    /// and what still is not (CLAUDE.md, agents, hooks). The other sixteen are
    /// the `skills` topic's name, which `teton_docs` renders twice — once in
    /// its description and once in the schema's `One of: …`.
    ///
    /// **REQ-586's recorded pair was 26 B high on both shapes** (6,096 /
    /// 6,049 against a measured 6,070 / 6,023 at the same tree). Corrected
    /// here, in `harness::tools::web`'s twin, and in
    /// `docs/manual-verification.md`, which records how to re-measure: these
    /// tests print only on failure, so pad the guide by a known amount, read
    /// the `N-byte system prompt` the failure names, and subtract.
    ///
    /// This REQ spent **18** of that, and it is the ADR-11 trade working: the
    /// budget vocabulary is 3.4 KB of prose and it entered the prompt as one
    /// word. `teton_docs`'s topic list gained `context` — nine bytes in its
    /// description and nine more in the tool schema's `One of: …`, which is
    /// rendered too — and nothing else resident moved. The `context` topic
    /// itself, the `window:` column, the doctor advisories and the pressure
    /// lines are tool results and CLI surfaces, which cost the prompt nothing.
    /// The opted-in twin is 6,049 / 9,325 / **915** and stays the looser of
    /// the two.
    ///
    /// **Recorded headroom at REQ-587:** the worst prompt is 7,230 bytes,
    /// `spent` is 10,506, and the margin is **758** — against an overhead
    /// raised 10 → 11 KiB by this REQ, with the floor still 48. The opted-in
    /// twin is 7,183 / 10,459 / **805** and stays the looser of the two by the
    /// same 47 bytes it always has.
    ///
    /// REQ-587 spent **1,092**, which is 266 more than the 826 it inherited —
    /// hence the raise, and the account of it (including the 931-byte cut it
    /// makes to every scanned route's budget) is on
    /// [`REDACT_BODY_OVERHEAD_BYTES`]. Where it went: **1,010** on the `skill`
    /// tool's docs and schema at BR-2's worst case — a roster at
    /// [`ROSTER_MAX_BYTES`](crate::harness::tools::skill::ROSTER_MAX_BYTES),
    /// which is what a session with sixty installed skills renders — and **82**
    /// on BR-8's third amendment to the guide's capability sentence, which now
    /// scopes the who-runs claim to the built-in commands and names the `skill`
    /// tool as the model's only door. The roster is the first resident line
    /// that **grows with the user's tree**, which is why it is bounded by a byte
    /// cap and why both sweeps register it at that cap rather than at whatever
    /// the developer's `~/.claude` happens to hold (ADR-9, LESSON-540).
    ///
    /// Everything else this REQ added is a tool result or a CLI surface and
    /// costs the prompt nothing: the listing reply, the frame, the consent
    /// text, the typed refusals and the echo line.
    ///
    /// Measured, not reasoned — the correction above is what reasoning about it
    /// cost last time.
    ///
    /// **Corrected 2026-08-26 (BUG-193). The figure this comment used to carry
    /// was wrong by ~234 bytes and had been wrong for six REQs.** It read: "710
    /// bytes of filler leaves this shape at exactly the 48-byte floor and
    /// passes, 711 fails." That was true when written, at REQ-587. The prompt
    /// then grew through REQ-583, REQ-585, REQ-587, REQ-589, REQ-590 and
    /// REQ-591 without the constant moving, so nothing failed and nothing was
    /// restated — the assertion below is `margin >= 48`, which holds identically
    /// at 710, at 476, and at 49. REQ-592 sized a task against the stale 710 and
    /// had to be corrected mid-implementation.
    ///
    /// **Measured now:** `worst` 7,859 + `escaping` 3,276 = `spent` 11,135
    /// against an assumed 11,264, leaving a margin of **129** — that is
    /// [`RECORDED_PROMPT_MARGIN_BYTES`], and it is now *asserted*, not narrated.
    /// Against the 48-byte floor that is **81 bytes of usable room**.
    ///
    /// Do not restore a prose-only figure here. Prose drifts silently; the pin
    /// is what makes the next drift a red test.
    #[test]
    fn the_total_cap_clears_the_harness_context_budget_with_margin() {
        use std::sync::Arc;

        use teton_core::capability::{SearchGap, WebCapabilityState};
        use teton_core::config::WebTier;

        use crate::harness::tools::{Tool, ToolRegistry};
        use crate::harness::turn_loop::{
            build_system_prompt, worst_case_session_root, HarnessConfig, SkillToolDocs,
        };

        // The strong-model shape (`max_tools: None`), so every builtin's
        // description is in the prompt: the larger of the two harness configs
        // is the one the overhead has to cover.
        //
        // This registry is the **opted-out** shape (REQ-563 D-1): no web tool.
        // The opted-in shape — web tool docs in place of the off clause — is
        // measured against this same constant beside the tool, because building
        // one needs a permission gate and a choke-point seam that do not belong
        // in this module.
        // **The `skill` tool is registered here, or this sweep never sees it**
        // (REQ-587 ADR-9). `ToolRegistry::with_builtins()` cannot hold it:
        // `register_skill_tool` is conditional on the session's registry
        // holding a model-invocable skill and runs per turn, so a sweep built
        // from the constructor alone would keep passing while BR-2's roster
        // grew the resident prompt on every real turn — LESSON-481's shape in
        // the one test guarding a budget three REQs contend for.
        //
        // `SkillToolDocs` rather than the real `SkillTool`, for the reason the
        // comment below gives about the web tool: the real one holds a
        // permission gate and a `tokio::runtime::Handle`, and this is a sync
        // `#[test]`. It is not a hand-typed stand-in — its description and its
        // schema come from the functions the shipped tool reaches for, pinned
        // byte-identical by
        // `harness::tools::skill::tests::the_doc_only_tool_and_the_real_one_render_one_set_of_prompt_bytes`.
        // The roster it carries is at `ROSTER_MAX_BYTES`: the ceiling, so the
        // margin below is the one a session with sixty installed skills has.
        let skill_docs = Arc::new(SkillToolDocs::worst_case());
        let mut tools = ToolRegistry::with_builtins();
        tools.register_cap_exempt(Arc::clone(&skill_docs) as Arc<dyn Tool>);

        let base = HarnessConfig::for_strong_model();
        let budget = base.context_budget_bytes;
        // The escaping allowance is the one the scannable bound is solved with
        // (`REDACT_ESCAPING_DIVISOR`), read rather than restated: this test
        // measures the *default* budget against the overhead, and the bound is
        // the cap minus that overhead for a context escaped by the same factor.
        let escaping = budget / REDACT_ESCAPING_DIVISOR;

        // Every state a clause can be built from, plus the unsupplied case that
        // falls back to the registry. A state added to the classifier without a
        // row here would go unmeasured, which is how a prompt grows past a
        // ceiling one capability at a time.
        let states = [
            None,
            Some(WebCapabilityState::OffAvailable),
            Some(WebCapabilityState::SearchUnavailable {
                reason: SearchGap::NoLocalModel,
            }),
            Some(WebCapabilityState::Ready(WebTier::Search)),
        ];
        // Since REQ-583 the prompt also carries the environment block when the
        // caller supplies a session root, and the block's length is the root's
        // — so the sweep crosses every capability state with the **largest**
        // root the block can render (a 200-character path elided to the display
        // ceiling, name and branch elided to theirs, kind `project`, which is
        // the longest kind phrase) as well as with no root at all. A root that
        // pushed the block past what this row measures would be a bounding
        // failure in `environment_block`, not a prompt that quietly grew.
        let roots = [None, Some(worst_case_session_root())];
        let widest = states
            .into_iter()
            .flat_map(|web_capability| {
                roots
                    .clone()
                    .into_iter()
                    .map(move |session_root| (web_capability, session_root))
            })
            .map(|(web_capability, session_root)| {
                let config = HarnessConfig {
                    web_capability,
                    session_root,
                    ..base.clone()
                };
                build_system_prompt(&tools, &config)
            })
            .max_by_key(String::len)
            .expect("the state sweep is not empty");
        // Self-check: the widest prompt is one that carries the block. Were the
        // root row dropped from the sweep (or the block from the prompt), the
        // measurement below would quietly become the smaller, root-less shape
        // and pass on the wrong number.
        assert!(
            widest.contains("Session root: "),
            "the widest prompt measured carries no environment block, so the sweep \
             is not measuring the row it claims to:\n{widest}"
        );
        // The same self-check for the `skill` tool, and it is the one that makes
        // ADR-9's registration a *guard* rather than a courtesy: dropping the
        // registration above shrinks the prompt, so every arithmetic assertion
        // below would still pass and this sweep would go back to measuring a
        // budget the product does not have. Matched on the rendered entry
        // (`- skill:`) and on the roster's own bytes, so neither the docs line
        // nor the roster can go missing on its own.
        assert!(
            widest.contains("- skill: "),
            "the widest prompt measured carries no `skill` tool docs, so the sweep is \
             measuring a resident prompt the daemon never builds (REQ-587 ADR-9). \
             Register `SkillToolDocs::worst_case()` — do not delete this \
             check:\n{widest}"
        );
        assert!(
            widest.contains(skill_docs.description()),
            "the `skill` tool's docs are in the prompt but not its worst-case roster, so \
             the sweep is measuring the description without the bytes that grow with the \
             user's installed skills (REQ-587 BR-2):\n{widest}"
        );
        let worst = widest.len();

        let spent = worst + escaping;
        // Strictly under, and asserted **before** the subtraction below: a
        // `spent` at or above the overhead would make that subtraction panic
        // with an arithmetic message instead of this sentence, which is the one
        // that says what to do about it.
        assert!(
            spent < REDACT_BODY_OVERHEAD_BYTES,
            "the assumed body overhead no longer covers what a body carries: a \
             {worst}-byte system prompt (the largest capability clause and the \
             largest session root) plus {escaping} bytes of escaping against an \
             assumed {REDACT_BODY_OVERHEAD_BYTES}. Shorten the bundled guide or a \
             clause; do not raise the overhead without re-checking the two claims \
             below."
        );
        // The margin is asserted against a **floor**, not merely against zero: a
        // prompt that cleared the overhead by three bytes would pass a `> 0`
        // check, say nothing, and leave the next sentence anyone writes with
        // nowhere to go. Spending the last of it is a decision, so it fails here
        // and gets made on purpose.
        let margin = REDACT_BODY_OVERHEAD_BYTES - spent;
        assert!(
            margin >= MIN_PROMPT_HEADROOM_BYTES,
            "the system prompt leaves {margin} bytes of headroom against a floor \
             of {MIN_PROMPT_HEADROOM_BYTES} ({worst} bytes of prompt + {escaping} \
             of escaping against an assumed {REDACT_BODY_OVERHEAD_BYTES}). \
             Eroding the last of the margin is a decision, not a side effect: \
             shorten the bundled guide or a clause, or move \
             `MIN_PROMPT_HEADROOM_BYTES` deliberately."
        );
        // BUG-193. The floor above is an inequality and therefore cannot see
        // drift: it held identically while the recorded figure went stale by
        // ~234 bytes over six REQs. This pin is what turns "the prompt moved"
        // from a comment nobody re-read into a failing test.
        assert_eq!(
            margin,
            RECORDED_PROMPT_MARGIN_BYTES,
            "the resident system prompt moved: the margin is now {margin} bytes \
             where {RECORDED_PROMPT_MARGIN_BYTES} was recorded, a change of {}. \
             This is not a failure — it is the alarm working. If you meant to \
             spend (or reclaim) those bytes: re-measure, add a ledger line to \
             `REDACT_BODY_OVERHEAD_BYTES`'s doc comment naming the REQ that \
             spent them, and move `RECORDED_PROMPT_MARGIN_BYTES` in the same \
             diff. With only {} bytes of usable room above the {MIN_PROMPT_HEADROOM_BYTES}-byte \
             floor, a change this size is worth a deliberate decision. Do NOT \
             widen this into an inequality — that is precisely the shape that \
             let the figure drift for six REQs (BUG-193).",
            (margin as i64) - (RECORDED_PROMPT_MARGIN_BYTES as i64),
            RECORDED_PROMPT_MARGIN_BYTES.saturating_sub(MIN_PROMPT_HEADROOM_BYTES),
        );

        let body = budget + REDACT_BODY_OVERHEAD_BYTES;
        assert!(
            REDACT_INPUT_MAX_BYTES >= 2 * body,
            "the total cap ({REDACT_INPUT_MAX_BYTES}) must clear a full outbound \
             body ({body}) twice over; it used to sit UNDER it, which is what \
             blocked every context-budget-full remote turn"
        );

        // And the shape of the fix, asserted rather than described: the cap
        // that is still *under* the budget is the per-chunk one, which is fine
        // precisely because a payload larger than one chunk is now scanned in
        // several.
        assert!(
            REDACT_CHUNK_MAX_BYTES < budget,
            "the collision was real: one engine window does not hold a \
             context-budget-full turn"
        );
        assert_eq!(
            REDACT_INPUT_MAX_BYTES,
            REDACT_TOTAL_CAP_CHUNKS * REDACT_CHUNK_MAX_BYTES,
            "the total cap is a stated multiple of the per-chunk cap, not a \
             number chosen beside it"
        );
    }

    /// **A body at the scannable bound fits under the cap** — REQ-586 BR-4 /
    /// BR-11 / AC-7, the second assertion ADR-6 puts beside the margin test.
    ///
    /// The test above runs the arithmetic *up* from the default budget to the
    /// cap; this one runs it *down* from the cap to the largest context a
    /// `[privacy] redact = true` route may assemble, and checks the two agree
    /// on the terms: a context at [`REDACT_SCANNABLE_CONTEXT_BYTES`], escaped
    /// by the same factor the margin test charges
    /// (`/ REDACT_ESCAPING_DIVISOR`), plus the same overhead the margin test
    /// measures ([`REDACT_BODY_OVERHEAD_BYTES`]), is a body the redactor will
    /// scan rather than refuse.
    ///
    /// What it catches, named so the next reader knows which failure is
    /// which:
    ///
    /// - **A literal copy of the bound** (TASK-192's one-home grep, mutation
    ///   (h)): `89_127` written where the expression belongs *passes* every
    ///   assertion here today — which is why the grep exists — but the moment
    ///   the cap or the overhead moves, the literal stays put and this test is
    ///   the one that says the bound no longer fits (or, moving the other way,
    ///   that it stopped spending the cap).
    /// - **An inverted 2× margin**: a bound halved "for safety" — `(cap −
    ///   overhead) / 2 …`, ≈ 48 KB — still fits under the cap, so the first
    ///   assertion lets it through; the *third* is the one it trips, because
    ///   the bound is then at most half the room under the cap, which is the
    ///   margin the cap already carries being charged a second time. The
    ///   second assertion is the floor the same mistake breaches once the cap
    ///   has no slack left: at the margin test's minimum (cap = 2 × a default
    ///   body) an inverted bound sits *under* the default budget, and a
    ///   scanned remote route would be budgeted below the local engine — the
    ///   collision REQ-562 closed, back by another door.
    /// - **A dropped escaping term**: `cap − overhead` with no divisor is
    ///   97,016, and a body at it escapes to 97,016 + 9,701 + 11,264 = 117,981
    ///   — over the cap by nearly ten KB, refused unscanned, every
    ///   context-full turn blocked. That is the first assertion.
    ///
    /// And what it is **meant to pass**: moving [`REDACT_BODY_OVERHEAD_BYTES`]
    /// or [`REDACT_TOTAL_CAP_CHUNKS`] alone. The bound is an expression over
    /// those constants, so it re-derives with them and every inequality here
    /// holds by construction; a change to either shows up in the margin test
    /// (which measures the overhead against the real prompt, and the cap
    /// against the default body) rather than here. This test pins the *shape*
    /// of the derivation, not its inputs.
    #[test]
    fn the_scannable_bound_plus_overhead_and_escaping_fits_under_the_cap() {
        use crate::harness::turn_loop::HarnessConfig;

        let escaping = REDACT_SCANNABLE_CONTEXT_BYTES / REDACT_ESCAPING_DIVISOR;
        let body = REDACT_SCANNABLE_CONTEXT_BYTES + escaping + REDACT_BODY_OVERHEAD_BYTES;
        assert!(
            body <= REDACT_INPUT_MAX_BYTES,
            "a body at the scannable bound does not fit under the cap: \
             {REDACT_SCANNABLE_CONTEXT_BYTES} of context + {escaping} of escaping + \
             {REDACT_BODY_OVERHEAD_BYTES} of overhead = {body} > \
             {REDACT_INPUT_MAX_BYTES}. The bound must be derived from the cap \
             (`(cap − overhead) × divisor / (divisor + 1)`), never copied or \
             written without its escaping term"
        );

        // The bound is a floor on what a scanned route may carry, and it has
        // to sit above the default pair: the local shape is what REQ-562
        // measured the cap against (2× a default body), so a bound that fell
        // below the default budget would have inverted that margin rather than
        // subtracted the overhead.
        let default_budget = HarnessConfig::default().context_budget_bytes;
        assert!(
            REDACT_SCANNABLE_CONTEXT_BYTES > default_budget,
            "the scannable bound ({REDACT_SCANNABLE_CONTEXT_BYTES}) does not clear \
             the default context budget ({default_budget}): a scanned remote route \
             would be budgeted below the local engine. The bound is cap minus \
             overhead, not the cap with the 2× margin inverted"
        );

        // And the bound *spends* the room under the cap rather than half of
        // it. The 2× in the cap's derivation is the cap's margin against a
        // budget bumped elsewhere; a bound derived from the cap cannot drift,
        // and charging the margin again here would halve every scanned route
        // for no collision it prevents. This is the line a `/ 2` trips: a
        // halved bound is at most half the room (the divisions floor), and the
        // derived one is ten elevenths of it.
        let room = REDACT_INPUT_MAX_BYTES - REDACT_BODY_OVERHEAD_BYTES;
        assert!(
            REDACT_SCANNABLE_CONTEXT_BYTES > room / 2,
            "the scannable bound ({REDACT_SCANNABLE_CONTEXT_BYTES}) is no more than \
             half the room under the cap ({room}): the bound is carrying the 2× \
             margin a second time. It is cap minus overhead, not the cap with the \
             margin inverted"
        );
    }

    /// **What a move of [`REDACT_BODY_OVERHEAD_BYTES`] costs, stated rather
    /// than implied** — REQ-587 AC-3's "arithmetic re-stated", in both
    /// directions.
    ///
    /// The test above pins the *shape* of the derivation and is meant to pass
    /// when the overhead moves; every assertion it makes is an inequality with
    /// kilobytes of slack. That is the right posture for a shape, and it is why
    /// a raise is otherwise silent: the overhead went 8→9→10→11 KiB across four
    /// REQs and nothing in the tree would have gone red if the second
    /// consequence below had been wrong.
    ///
    /// The two claims a raise has to re-state:
    ///
    /// 1. **The chunk count is still four.** Re-derived here from the same
    ///    terms [`REDACT_TOTAL_CAP_CHUNKS`]'s doc block runs — twice a full
    ///    default body, rounded up to whole chunks — rather than compared
    ///    against `REDACT_INPUT_MAX_BYTES / REDACT_CHUNK_MAX_BYTES`, which is
    ///    that constant's own definition and would be a tautology. At 11 KiB
    ///    the quotient is 3.25; the count is unchanged, so the cap is unchanged
    ///    and no `Unavailable` threshold moved.
    /// 2. **Every `[privacy] redact = true` route's byte budget got smaller.**
    ///    [`REDACT_SCANNABLE_CONTEXT_BYTES`] is `(cap − overhead) × 10 / 11`, so
    ///    the raise cut it 89,127 → **88,196**: 931 bytes off the budget BR-7's
    ///    `bound: redact scan` refusal measures against, on every scanned route,
    ///    with no test above able to see it.
    ///
    /// The literal here is the **assertion**, not a second home for the number
    /// (TASK-192's one-home grep is about the constant's definition, which is
    /// still the expression). Its whole job is to be updated by hand in the
    /// same diff that moves the overhead, so that narrowing a live budget is a
    /// line a reviewer reads.
    #[test]
    fn the_overhead_raise_restates_the_chunk_count_and_the_scannable_bound() {
        use crate::harness::turn_loop::HarnessConfig;

        let body = HarnessConfig::default().context_budget_bytes + REDACT_BODY_OVERHEAD_BYTES;
        assert_eq!(
            REDACT_TOTAL_CAP_CHUNKS,
            (2 * body).div_ceil(REDACT_CHUNK_MAX_BYTES),
            "the overhead moved and the chunk count no longer follows from it: twice a \
             full default body is {} bytes, which is {} whole {REDACT_CHUNK_MAX_BYTES}-byte \
             chunks, and `REDACT_TOTAL_CAP_CHUNKS` says {REDACT_TOTAL_CAP_CHUNKS}. The cap \
             is that count times the per-chunk window, so this is the total cap moving — \
             re-state the arithmetic in `REDACT_TOTAL_CAP_CHUNKS`'s doc block and check \
             what `REDACT_MAX_CHUNKS` does with it.",
            2 * body,
            (2 * body).div_ceil(REDACT_CHUNK_MAX_BYTES)
        );

        assert_eq!(
            REDACT_SCANNABLE_CONTEXT_BYTES,
            88_196,
            "the scannable bound moved to {REDACT_SCANNABLE_CONTEXT_BYTES}. It is derived, \
             so this is not a bug — it is the *cost*: every `[privacy] redact = true` \
             route's byte budget just changed by {} bytes, and that budget is what BR-7's \
             `bound: redact scan` refusal measures against. Update this figure in the same \
             diff that moved `REDACT_BODY_OVERHEAD_BYTES`, and say in that diff which way \
             the budget went.",
            88_196i64 - REDACT_SCANNABLE_CONTEXT_BYTES as i64
        );
    }

    #[test]
    fn an_over_cap_payload_is_unavailable_and_blocks_never_forwards() {
        // BR-7 / AC-8 mutation (b): a payload too large to scan is the "cannot
        // run" case. The payload here is deliberately CLEAN — that is the
        // discriminating state. A clean payload is the one case that would
        // Forward if the bound were removed, so removing the cap turns this red
        // rather than leaving it accidentally green.
        let over = "x".repeat(REDACT_INPUT_MAX_BYTES + 1);
        let verdict = pattern_verdict(&over);
        assert_eq!(verdict.outcome(), Outcome::Unavailable);
        assert!(
            !verdict.scanned(),
            "an over-cap payload must not claim a scan ran"
        );
        assert!(verdict.findings().is_empty());
        assert_eq!(
            decide(&verdict),
            EgressDecision::Block,
            "an over-cap payload must never forward"
        );
    }

    #[test]
    fn a_payload_at_exactly_the_cap_is_scanned() {
        // The non-vacuity twin: one byte under the over-cap fixture, the same
        // clean content, and the scan runs. Without this, the test above would
        // also pass if `pattern_verdict` returned `Unavailable` for everything.
        let at_cap = "x".repeat(REDACT_INPUT_MAX_BYTES);
        let verdict = pattern_verdict(&at_cap);
        assert_eq!(verdict.outcome(), Outcome::Clean);
        assert!(verdict.scanned());
        assert_eq!(decide(&verdict), EgressDecision::Forward);
    }

    #[test]
    fn an_over_cap_payload_carrying_a_credential_still_reports_could_not_scan() {
        // ADR-6: the block's cause must say the scan could not run, never that
        // it found something — different problems with different fixes. A
        // truncate-and-scan would report `Findings` (and `scanned: true`) here,
        // claiming a completed scan of bytes the model pass never saw.
        let mut over = String::from("sk-ABCDEFGHIJKLMNOPQRSTUVWX ");
        over.push_str(&"x".repeat(REDACT_INPUT_MAX_BYTES));
        let verdict = pattern_verdict(&over);
        assert_eq!(verdict.outcome(), Outcome::Unavailable);
        assert!(!verdict.scanned());
        assert_eq!(decide(&verdict), EgressDecision::Block);
    }

    // -----------------------------------------------------------------------
    // AC-10: confidence drives the action.
    // -----------------------------------------------------------------------

    /// One row of the decision table. `findings` is built from a confidence
    /// list so each row names its discriminating state explicitly (LESSON-485):
    /// what changes between a Block row and a Forward row is the presence of a
    /// `High`, and nothing else.
    struct DecisionCase {
        name: &'static str,
        verdict: fn() -> RedactionVerdict,
        expected: EgressDecision,
    }

    fn findings_of(levels: &[Confidence]) -> RedactionVerdict {
        let findings = levels
            .iter()
            .enumerate()
            .map(|(i, level)| {
                let span = i * 10..i * 10 + 8;
                match level {
                    Confidence::High => Finding::pattern(FindingKind::Credential, span),
                    Confidence::Low => Finding::model(FindingKind::Pii, span),
                }
            })
            .collect();
        RedactionVerdict::from_findings(findings)
    }

    const DECISION_CASES: &[DecisionCase] = &[
        DecisionCase {
            name: "clean",
            verdict: RedactionVerdict::clean,
            expected: EgressDecision::Forward,
        },
        DecisionCase {
            name: "unavailable",
            verdict: RedactionVerdict::unavailable,
            expected: EgressDecision::Block,
        },
        DecisionCase {
            name: "single high",
            verdict: || findings_of(&[Confidence::High]),
            expected: EgressDecision::Block,
        },
        DecisionCase {
            // The row AC-10 names explicitly: a low-confidence-only payload is
            // NOT blocked.
            name: "single low",
            verdict: || findings_of(&[Confidence::Low]),
            expected: EgressDecision::Forward,
        },
        DecisionCase {
            name: "mixed high then low",
            verdict: || findings_of(&[Confidence::High, Confidence::Low]),
            expected: EgressDecision::Block,
        },
        DecisionCase {
            // Order must not matter: the same multiset as the row above.
            name: "mixed low then high",
            verdict: || findings_of(&[Confidence::Low, Confidence::High]),
            expected: EgressDecision::Block,
        },
        DecisionCase {
            name: "mixed all low",
            verdict: || findings_of(&[Confidence::Low, Confidence::Low, Confidence::Low]),
            expected: EgressDecision::Forward,
        },
        DecisionCase {
            name: "mixed all high",
            verdict: || findings_of(&[Confidence::High, Confidence::High]),
            expected: EgressDecision::Block,
        },
        DecisionCase {
            name: "many low around one high",
            verdict: || {
                findings_of(&[
                    Confidence::Low,
                    Confidence::Low,
                    Confidence::High,
                    Confidence::Low,
                ])
            },
            expected: EgressDecision::Block,
        },
    ];

    #[test]
    fn confidence_drives_the_egress_decision() {
        for case in DECISION_CASES {
            let verdict = (case.verdict)();
            assert_invariants(&verdict);
            assert_eq!(
                decide(&verdict),
                case.expected,
                "{}: verdict {verdict:?}",
                case.name
            );
        }
    }

    #[test]
    fn the_decision_table_covers_both_outcomes() {
        // Guards the table itself: a table where every row expects the same
        // decision would pass vacuously no matter what `decide` did.
        assert!(DECISION_CASES
            .iter()
            .any(|c| c.expected == EgressDecision::Block));
        assert!(DECISION_CASES
            .iter()
            .any(|c| c.expected == EgressDecision::Forward));
    }

    // -----------------------------------------------------------------------
    // The forwarded-findings report (BR-4, ADR-4's "logged as kind+span only").
    // -----------------------------------------------------------------------

    /// **The discrimination.** A Low-only forward reports exactly one line per
    /// finding, carrying its kind and its span; a Clean forward — the same
    /// entry point, the same decision, one finding fewer — reports none.
    ///
    /// Without the pair this would pass for a function that always returned
    /// nothing (LESSON-485): "no line" is only meaningful beside "a line".
    #[test]
    fn a_low_only_forward_is_reported_and_a_clean_forward_is_not() {
        let low =
            RedactionVerdict::from_findings(vec![Finding::model(FindingKind::Pii, 1_400..1_436)]);
        assert_eq!(decide(&low), EgressDecision::Forward);
        let lines = forwarded_findings_report(&low);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("pii"), "the kind: {}", lines[0]);
        assert!(
            lines[0].contains("low-confidence"),
            "the confidence: {}",
            lines[0]
        );
        assert!(
            lines[0].contains("1400-1436"),
            "the span, so the user can find it: {}",
            lines[0]
        );
        // **The documented grep target, as a literal.** `docs/manual-verification.md`
        // tells a dogfooder to run `grep 'redact — low-confidence' tetond.log`,
        // which is the only way the model half of this feature is observable at
        // all (OQ-2). The words are asserted individually above; this pins them
        // as one contiguous substring, because a reworded line that still
        // contains both words separately would leave that procedure finding
        // nothing and reporting it as "the model caught nothing".
        assert!(
            lines[0].contains("redact — low-confidence"),
            "the documented grep target must survive verbatim: {}",
            lines[0]
        );

        let clean = RedactionVerdict::clean();
        assert_eq!(decide(&clean), EgressDecision::Forward);
        assert!(
            forwarded_findings_report(&clean).is_empty(),
            "a clean scan has nothing to report"
        );
    }

    /// A blocked payload is reported by `privacy_block` and the turn-failure
    /// sentence, so this path stays silent for it — one payload, one report.
    #[test]
    fn a_blocked_verdict_produces_no_forward_report() {
        for verdict in [
            RedactionVerdict::unavailable(),
            findings_of(&[Confidence::High]),
            findings_of(&[Confidence::High, Confidence::Low]),
        ] {
            assert_eq!(decide(&verdict), EgressDecision::Block);
            assert!(
                forwarded_findings_report(&verdict).is_empty(),
                "a blocked verdict must not also report as forwarded: {verdict:?}"
            );
        }
    }

    /// One line per finding, each naming its own span — a payload with two
    /// located strings gives the user two places to look.
    #[test]
    fn each_forwarded_finding_gets_its_own_line() {
        let verdict = findings_of(&[Confidence::Low, Confidence::Low, Confidence::Low]);
        let lines = forwarded_findings_report(&verdict);
        assert_eq!(lines.len(), 3, "{lines:?}");
        for (line, finding) in lines.iter().zip(verdict.findings()) {
            assert!(
                line.contains(&format!("{}-{}", finding.span().start, finding.span().end)),
                "{line}"
            );
        }
    }

    /// **BR-6 at the new surface.** The report is built from a `Finding`, which
    /// has no text field — so the sentinel cannot reach a log line even when the
    /// payload it was located in is right there.
    #[test]
    fn the_forward_report_never_carries_the_matched_text() {
        const SENTINEL: &str = "orange-walrus-9-QUUXSENTINEL";
        let payload = format!("the deploy password is {SENTINEL} — please rotate it");
        let at = payload.find(SENTINEL).expect("the fixture contains it");
        let verdict = RedactionVerdict::from_findings(vec![Finding::model(
            FindingKind::Secret,
            at..at + SENTINEL.len(),
        )]);
        let lines = forwarded_findings_report(&verdict);
        // Non-vacuity: there really is a line, and it really does describe this
        // finding.
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("secret"));
        for line in &lines {
            assert!(
                !line.contains("QUUXSENTINEL") && !line.contains(SENTINEL),
                "BR-6 VIOLATION — the matched text reached the daemon log: {line}"
            );
        }
    }

    #[test]
    fn adding_a_low_finding_never_changes_a_forward_into_a_block() {
        // The property behind BR-4, stated once rather than per-row: low
        // findings are reportable, not blocking.
        let lows = findings_of(&[Confidence::Low; 5]);
        assert_eq!(decide(&lows), EgressDecision::Forward);
        assert_eq!(lows.outcome(), Outcome::Findings);
        assert_eq!(lows.findings().len(), 5);
    }
}
