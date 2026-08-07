//! The `shell` duty: saying what a command's output *means*, but only when
//! reading it unaided is the hard part (REQ-561 BR-1/BR-3/BR-4b/BR-7/BR-8,
//! TASK-061).
//!
//! Named `shell_duty` rather than `shell` because
//! [`tools::shell`](super::tools::shell) — the tool this duty hangs off — already
//! owns that name.
//!
//! ## When it runs, and why that is the whole design
//!
//! `shell` is the highest-frequency tool call in a coding session: every `ls`,
//! every `cargo check`, every `git status`. A duty that fired on all of them
//! would put a model call behind every command the agent runs, which is a cost
//! nobody asked for and a latency everybody would feel. So it fires on exactly
//! the two results a weak model cannot read for itself (BR-4b):
//!
//! - **the command failed** — a non-zero exit, a timeout, a call that never
//!   started. The output is a stack trace or a compiler wall, and what the model
//!   needs out of it is one sentence: what broke.
//! - **the output was capped** — the raw stdout+stderr ran past
//!   [`SHELL_TRIGGER_OUTPUT_CHARS`], so the tool threw the rest away and what
//!   entered context is a fragment of a thing. Interpretation is the only way to
//!   recover what the fragment was part of.
//!
//! A short successful command is returned **verbatim, with no model call at
//! all**. That negative case is the cost argument, and it is asserted by call
//! count rather than by output shape ([`worth_interpreting`], and the table in
//! [`tools::shell`](super::tools::shell)'s tests).
//!
//! ## The size arm is decided on the RAW output (ADR-5, LESSON-443)
//!
//! [`SHELL_TRIGGER_OUTPUT_CHARS`] **is** the tool's own output cap, read from it
//! rather than restated. That is not a convenience: the reason to spend a model
//! call on a *successful* command is precisely that the cap threw information
//! away, so the trigger and the cap are one number by construction.
//!
//! It also means the comparison has to happen on the length the command really
//! produced, before the cap is applied. A post-truncation length is clamped to
//! the cap by the very code being tested against it, so a guard written that way
//! could never fire — it would disable itself. The tool captures the raw length
//! at the one moment it exists and records it, and this module's predicate reads
//! that number.
//!
//! ## What is `shell`-specific, and what is not
//!
//! Everything routable lives on the shared [`duty`](super::duty) seam. What is
//! genuinely this category's is exactly the three things ADR-3 allows: the
//! one-line resolver in [`crate::runtime`] that names the category literally,
//! [`SHELL_DUTY`] with its [`SHELL_OUTPUT_MAX_BYTES`] ceiling, and the prompt
//! builder below with its [`SHELL_OUTPUT_CONTRACT`].
//!
//! (On writing about that resolver: describe it, never spell it. The
//! `declared, no call site yet` marker in [`crate::call_sites`] is derived by
//! scanning the daemon's source as **text**, so the spelling of a
//! category-resolving router call inside a doc comment registers as a call site
//! and turns the derived-marker test red. ADR-9, learned the hard way in
//! TASK-058.)
//!
//! ## `shell` output has no provenance, and that is the point (BR-7)
//!
//! A shell command runs arbitrary code, so the daemon cannot know which files
//! its output came from — every `shell` result is tagged
//! [`ToolProvenance::Unknown`](super::context::ToolProvenance::Unknown), which
//! the egress choke point fail-closes on before it looks at a single boundary
//! glob. So on a machine with **any** privacy boundary configured, a remotely
//! bound `shell` duty is **refused**, and the command output comes back
//! uninterpreted with the refusal on the outcome.
//!
//! That is BR-3 operating correctly, not a gap to be patched. The alternative —
//! attributing shell output to "no files" so it could be sent — would ship
//! `cat secrets/prod.env` to a frontier provider, which is the exact thing
//! REQ-544 C-1 exists to prevent. The duty degrades; the boundary holds.
//!
//! The "**any** boundary" in that sentence is universal because
//! [`tools::shell`](super::tools::shell)'s `refine` fires only on results a
//! command actually produced. Its two pre-spawn failures — a missing `command`
//! argument, a repo root that does not exist — carry *empty* provenance rather
//! than `Unknown`, which is the honest tag (nothing ran, so nothing came off the
//! machine) but also the one the egress choke point fast-paths without
//! consulting a boundary. Letting those reach the duty would have made this
//! module's claim true of most `shell` duties instead of all of them; gating on
//! [`ToolOutcome::measured`](super::tools::ToolOutcome::measured) is what closes
//! it, and it closes it without tagging harness-authored strings `Unknown` and
//! pinning the entire session to the local tier (REQ-544 C-2) over a typo.
//!
//! ## Failure is today's behaviour, verbatim (BR-3, LESSON-447)
//!
//! The invariant this call site guards is "the model sees the command's output".
//! Interpretation is an addition to it, never a precondition for having it — so
//! every failure (unresolvable route, provider error, refusal at the choke
//! point, an empty answer) returns the tool's own `ToolOutcome` **unchanged**,
//! and reports why on
//! [`RefinedOutcome::duty_error`](super::tools::RefinedOutcome). That is why
//! [`interpret_output`] returns only the interpretation rather than a rebuilt
//! result: the fallback is not a code path anyone has to write correctly, it is
//! the value the caller already had.

use teton_protocol::Category;

use crate::egress::Provenance;

use super::context::truncate_middle;
use super::duty::{DutyKind, DutyRoute};
use super::tools::shell::MAX_OUTPUT_CHARS;

/// Byte ceiling on what a `shell` duty may return (BR-8).
///
/// The duty is asked for at most three sentences about one command, so 2 KiB is
/// roomy for every legitimate answer while being far below what an unbounded
/// stream could push at the harness. Enforced in the duty implementation rather
/// than requested of the provider (LESSON-484); `max_tokens` is a request, and a
/// request is not a bound.
///
/// Deliberately two orders of magnitude below the output it is interpreting: an
/// "interpretation" longer than the 8,000 characters it explains is not an
/// interpretation, and folding one into context would grow the very budget this
/// duty exists to make legible.
pub const SHELL_OUTPUT_MAX_BYTES: usize = 2_048;

/// The `shell` duty on the shared seam: its category and its output ceiling.
///
/// One `const` per category, stated once and read by every construction site —
/// the resolver in [`crate::runtime`], the transport-free offline entry point in
/// [`super::turn_loop`], and the tests.
pub const SHELL_DUTY: DutyKind = DutyKind::new(Category::Shell, SHELL_OUTPUT_MAX_BYTES);

/// The shell duty's output contract, verbatim: the last sentence of the
/// instruction, before the command and output it embeds.
///
/// Exported because it is also how the CI/offline stand-in engine
/// ([`crate::runtime::ScriptedFileEngine`]) recognizes a `shell` duty and
/// answers it *without consuming a scripted turn* — **a duty is not a turn**
/// (BR-10). One constant, used both to write the sentence and to recognize it,
/// so the seam cannot drift out of step with the prompt. A duty with no
/// recognition arm eats a scripted block and shifts every fixture's turn
/// sequence by one, which REQ-558 shipped twice before it was caught.
///
/// A full, distinctive sentence rather than a short phrase, for the reason
/// [`SUMMARIZER_OUTPUT_CONTRACT`](super::context::SUMMARIZER_OUTPUT_CONTRACT)
/// is one: the recognizer sees the *whole* rendered prompt, and this duty's
/// prompt embeds arbitrary command output — so a generic phrase could plausibly
/// arrive inside the very text being interpreted.
pub const SHELL_OUTPUT_CONTRACT: &str = "Answer in at most three sentences of plain prose — \
     no code fences, no bullet list, and no verbatim restatement of the output.";

/// Raw output length, in characters, past which a **successful** command is
/// still worth interpreting.
///
/// It is the tool's own cap, read rather than restated. The reason to spend a
/// model call on a command that succeeded is that the cap threw information
/// away, so the trigger and the cap are necessarily one number: two spellings
/// could drift into a threshold above the cap, which no capped output could ever
/// reach — a guard that disables itself (LESSON-443).
pub const SHELL_TRIGGER_OUTPUT_CHARS: usize = MAX_OUTPUT_CHARS;

/// Display bound on the command line in a duty prompt, in bytes.
const SHELL_COMMAND_MAX_BYTES: usize = 512;

/// Display bound on the embedded command output in a duty prompt, in bytes.
///
/// The tool already caps its result at [`SHELL_TRIGGER_OUTPUT_CHARS`]
/// *characters*, which is up to four times that many **bytes** for non-ASCII
/// output — so the prompt is bounded here in the unit that actually determines
/// how much is sent. Generous enough that an all-ASCII capped result passes
/// through whole, which is the case that matters.
const SHELL_OUTPUT_PROMPT_MAX_BYTES: usize = 12_288;

/// Whether a finished `shell` result is worth a model call (BR-4b).
///
/// `failed` is the tool's own error flag — a non-zero exit, a timeout, a command
/// that never started. `raw_output_chars` is the length of the stdout+stderr the
/// command really produced, **before** the tool capped it (ADR-5); a result the
/// cap never touched supplies a length that cannot be oversize, which is the
/// honest answer for it.
///
/// The negative case is the cost argument: a session running `ls`, `git status`
/// and a passing `cargo check` pays nothing at all.
#[must_use]
pub const fn worth_interpreting(failed: bool, raw_output_chars: usize) -> bool {
    failed || raw_output_chars > SHELL_TRIGGER_OUTPUT_CHARS
}

/// The duty prompt: what was run, what came back, and what to answer with.
///
/// The output comes **last** so that the instruction cannot be pushed out of a
/// small model's attention by a wall of compiler noise — and so that command
/// output containing something instruction-shaped is read as the material rather
/// than as the task.
#[must_use]
pub fn shell_prompt(command: &str, output: &str) -> String {
    let mut prompt = String::new();
    prompt.push_str(
        "Below is one shell command an agent ran in a repository, and the output it \
         produced. Say what happened: whether it succeeded, what failed if anything, and \
         what that means for what the agent should do next. ",
    );
    prompt.push_str(SHELL_OUTPUT_CONTRACT);
    prompt.push_str("\n\nCommand: ");
    prompt.push_str(&truncate_middle(command, SHELL_COMMAND_MAX_BYTES));
    prompt.push_str("\nOutput:\n");
    prompt.push_str(&truncate_middle(output, SHELL_OUTPUT_PROMPT_MAX_BYTES));
    prompt
}

/// Interpret `output` through the resolved `shell` route.
///
/// Returns **only the interpretation**, not a rebuilt tool result: the caller
/// keeps ownership of what the model finally sees, so a failure here costs the
/// explanation and nothing else (BR-3).
///
/// `provenance` is the egress provenance of the command output — the content
/// this duty is about to send. For `shell` that is always
/// [`Provenance::unknown`], so a remotely bound duty is refused at the choke
/// point on any machine with a boundary configured, and the caller degrades to
/// the uninterpreted output (BR-7, ADR-2). Computing it is the call site's job,
/// because the call site is what knows where its content came from
/// (LESSON-432).
///
/// # Errors
/// The route's own sentence when it resolved to nothing or the duty failed, or a
/// sentence naming an answer with nothing in it. Every one of them leaves the
/// caller holding an explanation it can degrade with.
pub async fn interpret_output(
    route: &DutyRoute,
    command: &str,
    output: &str,
    provenance: &Provenance,
) -> Result<String, String> {
    // Taken before the prompt is built, not after: an unresolvable route has
    // nothing to send, so rendering kilobytes of command output into a prompt no
    // model will ever see is work done for a call that cannot happen.
    if let DutyRoute::Unresolved { reason } = route {
        return Err(reason.clone());
    }
    let prompt = shell_prompt(command, output);
    let answer = route.perform(&prompt, provenance).await?;
    let interpretation = one_line(&answer);
    if interpretation.is_empty() {
        // Never "carry on with an empty explanation", which is indistinguishable
        // from the duty having declined to say anything — the caller must be able
        // to report that interpretation did not happen (LESSON-447).
        return Err("the `shell` duty answered with nothing to interpret".to_owned());
    }
    Ok(interpretation)
}

/// `text` collapsed onto a single line and trimmed.
///
/// The answer is folded into the model's context beside the command output it
/// describes, and that output has a **line-oriented** shape the model reads
/// structurally (`$ command`, `(exit 3)`, `[stderr] …`). A duty answering with
/// embedded newlines could otherwise write lines of that shape itself — a second
/// `(exit 0)` under a command that failed, say — so the interpretation is held
/// to one line and cannot forge one. Bounded already by the seam's ceiling; this
/// is about shape, not size.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    use teton_core::entities::{BoundaryMode, PrivacyBoundary};
    use teton_inference::{Engine, MockEngine};

    use super::super::duty::testing::{remote_duty_route, wire, Sent};

    fn output() -> String {
        "$ cargo test\n(exit 101)\n[stderr] error[E0308]: mismatched types\n".to_owned()
    }

    // -- the trigger (BR-4b, ADR-11) -----------------------------------------

    /// **The trigger, stated once.** Both arms, and — the load-bearing half —
    /// the case that costs nothing: a command that succeeded and stayed under
    /// the cap.
    #[test]
    fn only_a_failure_or_a_capped_output_is_worth_interpreting() {
        assert!(
            !worth_interpreting(false, 12),
            "a short successful command must cost nothing"
        );
        assert!(
            !worth_interpreting(false, SHELL_TRIGGER_OUTPUT_CHARS),
            "output that exactly fills the cap lost nothing, so it buys nothing"
        );
        assert!(worth_interpreting(false, SHELL_TRIGGER_OUTPUT_CHARS + 1));
        assert!(worth_interpreting(true, 12));
        assert!(worth_interpreting(true, SHELL_TRIGGER_OUTPUT_CHARS + 1));
    }

    /// The trigger is the tool's own cap, not a number beside it.
    ///
    /// A threshold *above* the cap could never be reached by a capped result, so
    /// the size arm would be a guard that disabled itself (LESSON-443). Pinning
    /// them equal is what makes that unrepresentable.
    #[test]
    fn the_trigger_is_the_tools_own_output_cap() {
        assert_eq!(SHELL_TRIGGER_OUTPUT_CHARS, MAX_OUTPUT_CHARS);
    }

    // -- the prompt ----------------------------------------------------------

    /// The output contract is the *whole* of the sentence the stand-in engine
    /// recognizes, and it really is in the prompt.
    ///
    /// Written out here rather than reused from the constant for the same reason
    /// `triage`'s equivalent spells its own out: changing it must be a deliberate
    /// two-place edit rather than something that silently desynchronizes
    /// `ScriptedFileEngine` from the duty it is meant to answer off-script
    /// (BR-10).
    #[test]
    fn the_duty_prompt_carries_the_output_contract_verbatim() {
        assert_eq!(
            SHELL_OUTPUT_CONTRACT,
            "Answer in at most three sentences of plain prose — no code fences, no bullet \
             list, and no verbatim restatement of the output."
        );
        assert!(shell_prompt("cargo test", &output()).contains(SHELL_OUTPUT_CONTRACT));
    }

    /// A pathological command cannot make the prompt unbounded: both halves are
    /// bounded in **bytes**, which is the unit the wire is measured in.
    #[test]
    fn an_enormous_command_and_output_are_both_bounded() {
        let huge_cmd = format!("echo {}", "x".repeat(50_000));
        let huge_out = "é".repeat(50_000);
        let prompt = shell_prompt(&huge_cmd, &huge_out);
        assert!(
            prompt.len() < SHELL_COMMAND_MAX_BYTES + SHELL_OUTPUT_PROMPT_MAX_BYTES + 1_024,
            "a 50 KB command and a 100 KB output produced a {} byte prompt",
            prompt.len()
        );
        assert!(prompt.contains(SHELL_OUTPUT_CONTRACT));
    }

    // -- the route legs ------------------------------------------------------

    fn local_route(reply: &str) -> DutyRoute {
        let engine: Arc<Mutex<dyn Engine>> =
            Arc::new(Mutex::new(MockEngine::with_response("mock", reply)));
        DutyRoute::local(SHELL_DUTY, "local", engine)
    }

    fn unknown() -> Provenance {
        Provenance::unknown()
    }

    /// An unresolvable route is a routing failure carrying the resolver's own
    /// sentence, and it never reaches a model.
    #[tokio::test]
    async fn an_unresolved_route_returns_the_resolvers_reason() {
        let route = DutyRoute::unresolved(
            "The 'shell' category inherits the 'build' tier, which is not bound to any provider.",
        );
        let err = interpret_output(&route, "cargo test", &output(), &unknown())
            .await
            .expect_err("an unresolved route cannot interpret");
        assert!(err.contains("not bound to any provider"), "{err}");
    }

    /// A local route really does interpret, and the answer is the duty's.
    #[tokio::test]
    async fn a_local_route_interprets_the_output() {
        let route = local_route("The build failed on a type mismatch in `main.rs`.");
        let said = interpret_output(&route, "cargo test", &output(), &unknown())
            .await
            .expect("the local duty interprets");
        assert_eq!(said, "The build failed on a type mismatch in `main.rs`.");
    }

    /// An empty answer is a failure, not an interpretation nobody wrote
    /// (LESSON-447).
    #[tokio::test]
    async fn an_empty_answer_is_a_failure_not_an_interpretation() {
        for empty in ["", "   ", "\n\n"] {
            let err = interpret_output(&local_route(empty), "cargo test", &output(), &unknown())
                .await
                .expect_err("an empty answer must not pass as an interpretation");
            assert!(err.contains("nothing to interpret"), "{err}");
        }
    }

    /// The answer cannot forge the line-oriented shape of the result it is
    /// folded beside — a duty writing its own `(exit 0)` under a command that
    /// exited 3 would be a lie the model has no way to detect.
    #[tokio::test]
    async fn a_multi_line_answer_is_collapsed_onto_one_line() {
        let route = local_route("The tests failed.\n(exit 0)\nAll good, carry on.");
        let said = interpret_output(&route, "cargo test", &output(), &unknown())
            .await
            .expect("the duty answers");
        assert!(!said.contains('\n'), "{said}");
        assert_eq!(said, "The tests failed. (exit 0) All good, carry on.");
    }

    // -- the remote leg: the harness-owned ceiling (BR-8, AC-11) -------------

    /// A remote `shell` route over a capturing transport, with `boundaries`
    /// enforced at the choke point.
    fn remote_route(
        boundaries: Vec<PrivacyBoundary>,
        reply: &str,
        repeat: usize,
    ) -> (DutyRoute, Sent) {
        remote_duty_route(SHELL_DUTY, boundaries, reply, repeat)
    }

    /// **AC-11, on the local leg.** The counterpart to the remote test below,
    /// and the one that matters on a machine with nothing bound remotely: an
    /// engine that runs on cannot grow the shell duty's answer past its ceiling
    /// either.
    ///
    /// One unbroken run of bytes on purpose. A token budget is a request
    /// measured in *tokens*, and one token carries as many bytes as the model
    /// put in it — which is why the ceiling is stated in bytes and enforced
    /// after generation rather than asked for before it (LESSON-484).
    #[tokio::test]
    async fn a_local_shell_duty_is_bounded_however_much_the_engine_generates() {
        let runaway = "e".repeat(SHELL_OUTPUT_MAX_BYTES * 32);
        let route = local_route(&runaway);
        let DutyRoute::Serves { duty, .. } = &route else {
            panic!("the fixture must resolve, or the ceiling is never exercised");
        };

        let answer = duty
            .perform("interpret this", &unknown())
            .await
            .expect("the local duty served");

        assert!(
            answer.len() <= SHELL_OUTPUT_MAX_BYTES,
            "a local shell duty accepted {} bytes from an engine that ran on; the \
             ceiling is {SHELL_OUTPUT_MAX_BYTES}",
            answer.len()
        );
        assert!(runaway.len() > SHELL_OUTPUT_MAX_BYTES * 4);
        assert!(
            answer.len() > SHELL_OUTPUT_MAX_BYTES / 2,
            "the cap must let a real interpretation through: {} bytes",
            answer.len()
        );
    }

    /// **AC-11.** A provider ignoring `max_tokens` cannot grow the shell duty's
    /// buffer without limit. The assertion reads [`SHELL_OUTPUT_MAX_BYTES`]
    /// rather than a literal, so raising the ceiling cannot silently un-test the
    /// bound.
    #[tokio::test]
    async fn a_remote_shell_duty_is_bounded_however_much_the_provider_streams() {
        // 1 KiB per delta × 64 deltas = 64 KiB offered, 2 KiB accepted.
        let chunk = format!("{} ", "e".repeat(1023));
        let (route, _sent) = remote_route(Vec::new(), &chunk, 64);
        let DutyRoute::Serves { duty, .. } = &route else {
            panic!("the fixture must resolve, or the ceiling is never exercised");
        };

        let answer = duty
            .perform("interpret this", &Provenance::empty())
            .await
            .expect("the remote duty served");

        assert!(
            answer.len() <= SHELL_OUTPUT_MAX_BYTES,
            "a shell duty accepted {} bytes from a provider ignoring its token budget; the \
             ceiling is {SHELL_OUTPUT_MAX_BYTES}",
            answer.len()
        );
        // Non-vacuity, both directions: the provider really did offer far more
        // than the bound, and the duty really did accumulate up to it — so this
        // is a cap and not a refusal.
        assert!(
            chunk.len() * 64 > SHELL_OUTPUT_MAX_BYTES * 4,
            "the fixture must offer well over the bound"
        );
        assert!(
            answer.len() > SHELL_OUTPUT_MAX_BYTES / 2,
            "the cap must let a real interpretation through: {} bytes",
            answer.len()
        );
    }

    /// The ceiling is smaller than the output it explains — this module's own
    /// stated rationale, pinned so it cannot quietly stop being true.
    ///
    /// An "interpretation" longer than the 8,000 characters it interprets is not
    /// an interpretation, and folding one into context would grow the very
    /// budget this duty exists to make legible. Asserted at compile time because
    /// the relationship between two constants is a compile-time fact.
    ///
    /// **What this does not catch**, stated rather than left to be discovered:
    /// a widening *within* the range — 2 KiB to 4 KiB — survives it, because
    /// AC-11's enforcement test reads the constant and moves with it. `title`
    /// closes that by deriving its number from its contract's word budget and
    /// `triage` by banding its against the widest answer it can produce; neither
    /// derivation is available here, because "three sentences of prose" has no
    /// honest byte size. The neighbour pins in
    /// [`crate::harness::duty`](super::duty)'s
    /// `every_duty_declares_a_ceiling_and_the_five_are_ordered` bound the other
    /// end.
    #[test]
    fn the_ceiling_is_smaller_than_the_output_it_interprets() {
        const {
            assert!(SHELL_OUTPUT_MAX_BYTES < SHELL_TRIGGER_OUTPUT_CHARS);
            assert!(SHELL_OUTPUT_MAX_BYTES < crate::harness::context::SUMMARIZER_INPUT_MAX_BYTES);
        }
        assert_eq!(SHELL_TRIGGER_OUTPUT_CHARS, MAX_OUTPUT_CHARS);
        assert_eq!(SHELL_DUTY.ceiling_bytes(), SHELL_OUTPUT_MAX_BYTES);
        assert_eq!(SHELL_DUTY.category(), Category::Shell);
    }

    // -- the boundary interaction (BR-7) ------------------------------------

    fn boundaries() -> Vec<PrivacyBoundary> {
        vec![PrivacyBoundary {
            path_glob: "secrets/**".to_owned(),
            mode: BoundaryMode::LocalOnly,
        }]
    }

    /// **The non-vacuity half.** The same remote route, on a machine with **no**
    /// boundary configured, genuinely sends: the prompt reaches the transport and
    /// the provider's interpretation comes back.
    ///
    /// Without this, the refusal below would be equally satisfied by a fixture
    /// that could never send anything at all (LESSON-485). Note what differs
    /// between the two: only the *boundaries*. The provenance is
    /// [`Provenance::unknown`] in both, because that is the only provenance a
    /// `shell` result ever has.
    #[tokio::test]
    async fn an_unbounded_machine_sends_the_shell_duty_prompt_and_returns_its_answer() {
        let (route, sent) = remote_route(Vec::new(), "The tests failed to compile.", 1);
        let said = interpret_output(&route, "cargo test", &output(), &unknown())
            .await
            .expect("with no boundary configured the duty sends");
        assert_eq!(said, "The tests failed to compile.");
        assert!(
            wire(&sent).contains("Say what happened"),
            "the duty prompt never reached the transport"
        );
    }

    /// **BR-7, asserted rather than worked around.** A `shell` result carries
    /// unknown provenance — the daemon cannot know which files a command read —
    /// and the choke point fail-closes on unknown provenance *before* it looks at
    /// a boundary glob. So on a machine with any boundary configured, a remotely
    /// bound `shell` duty is refused before a byte leaves.
    ///
    /// This is the correct outcome, not a defect: the alternative would send the
    /// output of `cat secrets/prod.env` to a frontier provider. The duty
    /// degrades, the caller keeps the command's own output, and the boundary
    /// holds.
    #[tokio::test]
    async fn a_shell_duty_is_refused_remotely_on_any_machine_with_a_boundary() {
        let (route, sent) = remote_route(boundaries(), "The tests failed to compile.", 1);
        let secret_bearing = "$ cat secrets/prod.env\n(exit 0)\nAPI_KEY=super-secret-xyzzy\n";

        let err = interpret_output(&route, "cat secrets/prod.env", secret_bearing, &unknown())
            .await
            .expect_err("unattributable output must not be interpreted remotely");

        assert!(
            wire(&sent).is_empty(),
            "unattributable command output reached the transport"
        );
        assert!(!wire(&sent).contains("super-secret-xyzzy"));
        assert!(err.contains("privacy boundary"), "{err}");
    }

    /// **BR-11 / AC-16: the disclosure must cover the whole payload.**
    ///
    /// A `shell` row that discloses only "command output" omits the command
    /// itself — and a command line is routinely the more revealing half, since
    /// it carries paths, hostnames, branch and ticket names that the output may
    /// never print. The prompt is read here rather than described, so a builder
    /// that starts carrying something else turns this red rather than widening
    /// the egress under a stale sentence.
    #[test]
    fn the_disclosed_content_class_names_everything_the_shell_prompt_carries() {
        use teton_protocol::methods::ContentClass;

        let command = "cargo test -p tetond --features internal-staging";
        let prompt = shell_prompt(command, &output());

        // Non-vacuity: the prompt really does carry both halves.
        assert!(prompt.contains(command), "the command rides in the prompt");
        assert!(
            prompt.contains("mismatched types"),
            "and so does its output"
        );

        let disclosed = ContentClass::for_category(teton_protocol::Category::Shell).describe();
        assert!(
            disclosed.contains("output"),
            "the stdout and stderr are sent: {disclosed}"
        );
        // `contains("command")` alone would not discriminate: the understating
        // wording this replaced was the single noun phrase "command output",
        // which contains the word and still names only one thing. The claim is
        // that the command is disclosed as an item *conjoined with* the output.
        assert!(
            disclosed.contains("command and"),
            "the command line is sent in its own right, not merely as the thing \
             whose output is sent, and a row that does not say so understates \
             what leaves: {disclosed}"
        );
    }
}
