//! **AC-3 — the cross-duty failure matrix** (REQ-561 BR-3, LESSON-447).
//!
//! Every duty in this REQ is an *improvement* on something the harness already
//! did correctly: `triage` improves the order `grep` returns matches in, `shell`
//! improves how legible a failed command is, `digest` improves how an oversized
//! result is shrunk, `title` improves a session's name from "nothing", and
//! `compact` improves *which* blocks a pressured conversation drops. None of
//! them is the reason its call site's invariant holds — and this file is the
//! table that says so, over all five duties and every way a duty can fail.
//!
//! ## Why this is one table rather than five tests
//!
//! Each duty task already ships its own degradation test at its own layer. What
//! none of them can show is that the *shape* is the same across all five, which
//! is BR-6's whole claim and the thing a sixth duty would break. A table makes
//! adding a duty without its failure arms impossible to do quietly: the new row
//! is missing, and the row count is asserted.
//!
//! ## The four conditions, and what the fourth one is
//!
//! AC-3 names them `resolves / unresolvable / provider error / tainted session`.
//! The first three are literal. The fourth needs stating precisely, because **a
//! call site cannot see a session** — it sees content and a route, and nothing
//! else. A tainted session reaches a duty in exactly one of two shapes:
//!
//! 1. the resolver hands it a *local* or *unresolved* route instead of the
//!    remote one its binding names — which is rows 1 and 2, already here; or
//! 2. the content the call site is about to hand over is itself refusable, which
//!    is what a taint is made of (REQ-544 C-1: unknown or boundary-derived
//!    provenance is what marks a session in the first place).
//!
//! [`Condition::ContentRefused`] is (2): the duty's own content is refused at
//! the egress choke point before a byte leaves. The resolver-side half — a
//! tainted session resolving *local* however its tier is bound — is asserted
//! where the resolver lives (`runtime::tests::dispatch::{digest,triage,shell,
//! title,compact}::a_tainted_session_*`, one per duty) and by captured bytes end
//! to end in `tests/e2e/duty_taint.rs` (AC-5). Neither is reachable from an
//! integration test, which can build routes but not a `DaemonRuntime`.
//!
//! ## Non-vacuity (LESSON-485)
//!
//! A matrix of failure rows proves nothing on its own: "the invariant held" is
//! satisfied by a duty that never ran. So every duty's `Resolves` row must show
//! the duty **genuinely changed the outcome** — a different ordering, an added
//! interpretation, a real title, a real compaction — and that is asserted as
//! hard as the failures are.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream;
use serde_json::json;

use teton_core::entities::{BoundaryMode, PrivacyBoundary};
use teton_inference::{Engine, MockEngine};
use teton_providers::transport::{HttpMethod, TransportError, TransportRequest, TransportResponse};
use teton_providers::{
    CapabilityProfile, Provider, ProviderError, StopReason, TokenUsage, Transport, TurnCompletion,
    TurnEvent, TurnRequest, TurnStream,
};

use tetond::egress::{Egress, NoopSink, Provenance};
use tetond::harness::context::{summarize_if_large, APPROX_BYTES_PER_TOKEN};
use tetond::harness::{
    title, ContextManager, DutyKind, DutyRoute, RefinedOutcome, ToolDuties, ToolOutcome,
    ToolProvenance, ToolRegistry, COMPACT_DUTY, DIGEST_DUTY, SHELL_DUTY, TITLE_DUTY, TRIAGE_DUTY,
};

/// A sentinel that exists only inside boundary-protected content, so a leak is
/// identifiable in a captured payload rather than inferred.
const SECRET: &str = "sk-live-DO-NOT-LEAK-duty-matrix-Zx9";

/// The boundary the `ContentRefused` row is refused by.
fn boundaries() -> Vec<PrivacyBoundary> {
    vec![PrivacyBoundary {
        path_glob: "secrets/**".to_owned(),
        mode: BoundaryMode::LocalOnly,
    }]
}

// ===========================================================================
// The two axes
// ===========================================================================

/// The five duties on the shared seam.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Duty {
    Digest,
    Triage,
    Shell,
    Title,
    Compact,
}

impl Duty {
    /// Every duty wired in REQ-558 and REQ-561. A sixth added without a row here
    /// fails [`the_matrix_covers_every_duty_the_seam_serves`].
    const ALL: [Duty; 5] = [
        Duty::Digest,
        Duty::Triage,
        Duty::Shell,
        Duty::Title,
        Duty::Compact,
    ];

    fn kind(self) -> DutyKind {
        match self {
            Duty::Digest => DIGEST_DUTY,
            Duty::Triage => TRIAGE_DUTY,
            Duty::Shell => SHELL_DUTY,
            Duty::Title => TITLE_DUTY,
            Duty::Compact => COMPACT_DUTY,
        }
    }

    fn name(self) -> &'static str {
        self.kind().category().as_str()
    }

    /// A well-formed answer for **this** duty's own parser — the thing that
    /// makes the `Resolves` row a real success rather than a parse failure
    /// wearing one.
    fn good_answer(self) -> &'static str {
        match self {
            Duty::Digest => "the file defines three functions and one constant.",
            // Reverses the three matches offered below, so "the duty took
            // effect" is observable as a different ordering.
            Duty::Triage => "3 2 1",
            Duty::Shell => "the build failed on a type mismatch in src/lib.rs.",
            Duty::Title => "Retry the download client",
            Duty::Compact => "FORGET: 1 2\nSUMMARY: the agent read three files.",
        }
    }

    /// A fragment that appears in what the call site delivers **only** when the
    /// duty ran and its answer was applied — the non-vacuity probe.
    ///
    /// `triage`'s is the frame `render_ranked` writes rather than a match line,
    /// because every match line is present either way: the whole point of the
    /// BR-3 fallback is that they survive. The frame is what only a ranking
    /// produces.
    fn effect_marker(self) -> &'static str {
        match self {
            Duty::Digest => "three functions",
            Duty::Triage => "[triage: 3 of 3 matches",
            Duty::Shell => "type mismatch in src/lib.rs",
            Duty::Title => "Retry the download client",
            Duty::Compact => "the agent read three files",
        }
    }
}

/// The four states a duty's call site has to survive (AC-3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Condition {
    /// The duty is routed and answers.
    Resolves,
    /// Nothing can serve this duty this turn; the resolver said why.
    Unresolvable,
    /// The duty is routed remotely and the provider fails.
    ProviderError,
    /// The duty is routed remotely and its **own content** is refused at the
    /// egress choke point before a byte leaves — the shape a session taint takes
    /// at a call site (see the module docs).
    ContentRefused,
}

impl Condition {
    const ALL: [Condition; 4] = [
        Condition::Resolves,
        Condition::Unresolvable,
        Condition::ProviderError,
        Condition::ContentRefused,
    ];

    /// Whether this condition must leave a BR-3 degradation report on the
    /// outcome. Only [`Condition::Resolves`] must not.
    fn must_degrade(self) -> bool {
        self != Condition::Resolves
    }
}

// ===========================================================================
// Routes
// ===========================================================================

/// A transport that records every request body it is asked to send, so "zero
/// boundary bytes left" is a fact about captured bytes rather than about a code
/// path (LESSON-432).
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

/// What the stand-in remote provider does when a duty calls it.
#[derive(Clone)]
enum Answer {
    /// Put the request on the wire, then stream `0` back as one delta.
    Says(String),
    /// Fail before anything is sent — an auth error, a 500, a dead endpoint.
    Fails,
}

struct StubProvider {
    answer: Answer,
}

#[async_trait]
impl Provider for StubProvider {
    fn id(&self) -> &str {
        "frontier"
    }

    fn capabilities(&self) -> CapabilityProfile {
        CapabilityProfile::default()
    }

    async fn stream_turn(
        &self,
        request: TurnRequest,
        transport: &dyn Transport,
    ) -> Result<TurnStream, ProviderError> {
        let reply = match &self.answer {
            Answer::Says(reply) => reply.clone(),
            // Before the transport, deliberately: a provider that cannot reach
            // its endpoint sends nothing, which is also what makes this row
            // distinguishable from `ContentRefused` by captured bytes.
            Answer::Fails => return Err(ProviderError::Transport),
        };
        let body = serde_json::to_vec(&request).map_err(|e| ProviderError::Build(e.to_string()))?;
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
        Ok(Box::pin(stream::iter(vec![
            Ok(TurnEvent::TextDelta(reply)),
            Ok(TurnEvent::Completed(TurnCompletion {
                usage: TokenUsage::default(),
                stop_reason: StopReason::EndTurn,
            })),
        ])))
    }
}

/// The route `duty` gets under `condition`, plus the wire it would send on.
fn route_for(duty: Duty, condition: Condition) -> (DutyRoute, Arc<Mutex<Vec<Vec<u8>>>>) {
    let transport = CaptureTransport::default();
    let sent = Arc::clone(&transport.sent);
    let route = match condition {
        Condition::Resolves => {
            let engine: Arc<Mutex<dyn Engine>> = Arc::new(Mutex::new(MockEngine::with_response(
                "mock-3b",
                duty.good_answer(),
            )));
            DutyRoute::local(duty.kind(), "local", engine)
        }
        // The resolver's own sentence, verbatim — BR-6: this layer mints no
        // explanation of its own.
        Condition::Unresolvable => DutyRoute::unresolved(format!(
            "The '{}' category resolves to 'frontier', which is not a configured provider.",
            duty.name()
        )),
        Condition::ProviderError => DutyRoute::remote(
            duty.kind(),
            "frontier",
            Box::new(StubProvider {
                answer: Answer::Fails,
            }),
            Egress::new(transport, Vec::new(), Arc::new(NoopSink)),
            "claude-opus-4",
            "sess-matrix",
        ),
        // Boundaries configured and the content is boundary-derived: the choke
        // point refuses. Everything else about this route is the working remote
        // one, which is what makes the refusal the guard holding rather than a
        // fixture that could not send.
        Condition::ContentRefused => DutyRoute::remote(
            duty.kind(),
            "frontier",
            Box::new(StubProvider {
                answer: Answer::Says(duty.good_answer().to_owned()),
            }),
            Egress::new(transport, boundaries(), Arc::new(NoopSink)),
            "claude-opus-4",
            "sess-matrix",
        ),
    };
    (route, sent)
}

// ===========================================================================
// Driving each call site
// ===========================================================================

/// What one call site did, in the only four terms the matrix asks about.
struct Observed {
    /// Whether the invariant that call site guards still holds.
    invariant: bool,
    /// The BR-3 degradation report, on the **value** rather than in a log.
    degraded: Option<String>,
    /// Whether the duty demonstrably changed the outcome — the non-vacuity leg.
    took_effect: bool,
    /// What the call site delivered, for the failure message.
    delivered: String,
}

/// Digest's threshold, small enough that the fixture below is comfortably over
/// it in both currencies.
const DIGEST_THRESHOLD_TOKENS: usize = 64;

/// The byte budget the compaction fixture works against.
const COMPACT_BUDGET_BYTES: usize = 4_000;

/// A tool result far over the `digest` threshold, carrying `marker` on every
/// line.
fn oversized(marker: &str) -> String {
    std::iter::repeat_n(format!("{marker} padding padding padding\n"), 400).collect()
}

/// The three `grep` matches `triage` is offered.
fn matches(tainted: bool) -> Vec<String> {
    if tainted {
        vec![
            format!("secrets/prod.env:1: API_KEY={SECRET}"),
            "src/b.rs:9: fn parse_two()".to_owned(),
            "src/c.rs:3: fn parse_three()".to_owned(),
        ]
    } else {
        vec![
            "src/a.rs:12: fn parse_one()".to_owned(),
            "src/b.rs:9: fn parse_two()".to_owned(),
            "src/c.rs:3: fn parse_three()".to_owned(),
        ]
    }
}

/// The failed command `shell` is offered. `shell` output is always
/// [`ToolProvenance::Unknown`], so it has only ever one provenance — the
/// `tainted` axis does not apply to it, and the refusal comes from the boundary
/// being configured at all (ADR-5 note in `harness::shell_duty`).
const FAILED_COMMAND: &str =
    "$ cargo build\n(exit 101)\n[stderr] error[E0308]: mismatched types at src/lib.rs:12";

/// A conversation over its byte budget, with a real choice in it.
fn pressured(tainted: bool) -> ContextManager {
    let mut ctx = ContextManager::new("sys", 1_000_000).with_budget_bytes(COMPACT_BUDGET_BYTES);
    for i in 0..5 {
        let text = format!("block {i} {}", "x".repeat(1_000));
        if tainted {
            ctx.push_tool_result_prov(
                "read",
                ToolProvenance::path("secrets/prod.env"),
                format!("{text} {SECRET}"),
            );
        } else {
            ctx.push_user(text);
        }
    }
    assert!(
        ctx.under_compaction_pressure(),
        "the compaction fixture must be pressured, or the duty is never asked"
    );
    ctx
}

/// A route no duty in this file uses, for the `ToolDuties` field the tool under
/// test is not.
fn spare() -> DutyRoute {
    DutyRoute::unresolved("not the duty under test")
}

/// Refine `outcome` through `tool`'s own duty, exactly as the turn loop does.
async fn refine(
    tool: &str,
    args: serde_json::Value,
    route: &DutyRoute,
    outcome: ToolOutcome,
) -> RefinedOutcome {
    let registry = ToolRegistry::with_builtins();
    let spare = spare();
    let duties = match tool {
        "grep" => ToolDuties {
            triage: route,
            shell: &spare,
        },
        _ => ToolDuties {
            triage: &spare,
            shell: route,
        },
    };
    registry
        .refine(
            tool,
            &args,
            "find the parser and fix the build",
            &duties,
            outcome,
        )
        .await
}

/// Drive `duty`'s real call site over `route`, with content that is
/// boundary-derived when `tainted`.
async fn exercise(duty: Duty, route: &DutyRoute, tainted: bool) -> Observed {
    match duty {
        // -- `summarize_if_large`: the result that enters context is bounded ---
        Duty::Digest => {
            let text = oversized(if tainted { SECRET } else { "ordinary" });
            let provenance = if tainted {
                ToolProvenance::path("secrets/prod.env")
            } else {
                ToolProvenance::path("src/lib.rs")
            };
            let out =
                summarize_if_large(route, "read", &text, DIGEST_THRESHOLD_TOKENS, &provenance)
                    .await;
            // The invariant: an oversized result is never folded raw. Both the
            // summary and the mechanical fallback are bounded by the threshold
            // the caller set (plus the frame each wraps it in).
            let bound = DIGEST_THRESHOLD_TOKENS * APPROX_BYTES_PER_TOKEN + 1_024;
            Observed {
                invariant: out.text.len() < text.len() && out.text.len() <= bound,
                took_effect: out.text.contains(duty.effect_marker()),
                degraded: out.engine_error,
                delivered: out.text,
            }
        }

        // -- `GrepTool::refine`: the model still sees every match --------------
        Duty::Triage => {
            let lines = matches(tainted);
            let content = lines.join("\n");
            let paths: Vec<String> = if tainted {
                vec!["secrets/prod.env".to_owned(), "src/b.rs".to_owned()]
            } else {
                vec!["src/a.rs".to_owned(), "src/b.rs".to_owned()]
            };
            // `measuring` is how `run` tells `refine` how many matches it
            // found — a fact that no longer travels in the rendered text
            // (REQ-561 verify M3). A hand-built outcome has to supply it, and an
            // outcome that supplies nothing is one the duty correctly declines.
            let outcome = ToolOutcome::ok(&content)
                .with_paths(paths)
                .measuring(lines.len());
            let refined = refine("grep", json!({ "pattern": "parse" }), route, outcome).await;
            Observed {
                invariant: lines.iter().all(|m| refined.outcome.content.contains(m)),
                took_effect: refined.outcome.content.contains(duty.effect_marker())
                    && refined.outcome.content != content,
                degraded: refined.duty_error,
                delivered: refined.outcome.content,
            }
        }

        // -- `ShellTool::refine`: the model still sees the command's output ----
        Duty::Shell => {
            // `measuring` is how `run` says a command actually ran: the duty is
            // for command output, and a call that never spawned one has none to
            // interpret (REQ-561 verify). The length is the failed command's own
            // output, which is well under the cap — the `is_error` arm is what
            // fires here.
            let outcome = ToolOutcome::error(FAILED_COMMAND)
                .with_unknown_provenance()
                .measuring(FAILED_COMMAND.chars().count());
            let refined =
                refine("shell", json!({ "command": "cargo build" }), route, outcome).await;
            Observed {
                // Both halves: the output survives, and a command that failed
                // still reads as failed after being explained (REQ-544 MED-4).
                invariant: refined.outcome.content.contains(FAILED_COMMAND)
                    && refined.outcome.is_error,
                took_effect: refined.outcome.content.contains(duty.effect_marker()),
                degraded: refined.duty_error,
                delivered: refined.outcome.content,
            }
        }

        // -- `name_session`: a failure leaves nothing to store -----------------
        Duty::Title => {
            let (request, provenance) = if tainted {
                (
                    format!("Rotate the production key in secrets/prod.env — it is {SECRET}."),
                    Provenance::tainted_by("secrets/prod.env"),
                )
            } else {
                (
                    "Add retry-with-backoff to the download client and cover it with tests."
                        .to_owned(),
                    Provenance::empty(),
                )
            };
            let (delivered, degraded) =
                match title::name_session(route, &request, &provenance).await {
                    Ok(title) => (title, None),
                    Err(error) => (String::new(), Some(error)),
                };
            Observed {
                // The invariant is the pairing itself: a failure yields nothing
                // to write into a session record that is never revised — never a
                // blank name, which every client renders worse than no name.
                invariant: degraded.is_some() == delivered.is_empty(),
                took_effect: delivered == duty.effect_marker(),
                degraded,
                delivered,
            }
        }

        // -- `compact_if_pressured` + the hard gate: under budget, always ------
        Duty::Compact => {
            let mut ctx = pressured(tainted);
            let out = ctx.compact_if_pressured(route).await;
            // ADR-4: unconditional, unmodified, and never made conditional on
            // anything the duty did. This line is what enforces the budget.
            ctx.truncate_to_budget();
            // Only the *replacement* block a compaction inserts is reported as
            // delivered content, beside the shape of the surviving context — the
            // 5 KB of block padding would drown every failure message.
            let summary: String = ctx
                .blocks()
                .iter()
                .filter(|b| b.text.starts_with("[earlier conversation compacted"))
                .map(|b| b.text.clone())
                .collect();
            Observed {
                invariant: ctx.estimated_bytes() <= COMPACT_BUDGET_BYTES,
                took_effect: out.dropped_blocks > 0
                    && !out.degraded
                    && summary.contains(duty.effect_marker()),
                degraded: out.reason,
                delivered: format!(
                    "{} blocks / {} bytes, dropped {}; summary: {summary}",
                    ctx.blocks().len(),
                    ctx.estimated_bytes(),
                    out.dropped_blocks
                ),
            }
        }
    }
}

// ===========================================================================
// AC-3
// ===========================================================================

/// **AC-3.** Five duties × four conditions: on every failure path the call
/// site's own invariant still holds, and the failure is reported on the value.
#[tokio::test]
async fn every_duty_holds_its_call_sites_invariant_on_every_failure_path() {
    let mut rows = 0usize;
    for duty in Duty::ALL {
        for condition in Condition::ALL {
            let tainted = condition == Condition::ContentRefused;
            let (route, sent) = route_for(duty, condition);
            let observed = exercise(duty, &route, tainted).await;
            rows += 1;

            assert!(
                observed.invariant,
                "BR-3 VIOLATION: `{}` under {condition:?} broke the invariant its call site \
                 guards. Delivered: {}",
                duty.name(),
                observed.delivered
            );
            assert_eq!(
                observed.degraded.is_some(),
                condition.must_degrade(),
                "`{}` under {condition:?} must {}report a degradation on its outcome \
                 (LESSON-447 — a routing failure that is only a log line is a silent one). \
                 Got: {:?}",
                duty.name(),
                if condition.must_degrade() { "" } else { "not " },
                observed.degraded
            );

            // Each failure row must fail for **its own** reason, or one of them
            // is standing in for another and the matrix has three conditions
            // wearing four names. The refusal names the boundary; the provider
            // error does not.
            let reason = observed.degraded.clone().unwrap_or_default();
            match condition {
                Condition::Resolves => {}
                Condition::Unresolvable => assert!(
                    reason.contains("not a configured provider"),
                    "`{}` must degrade with the resolver's own sentence: {reason}",
                    duty.name()
                ),
                Condition::ProviderError => assert!(
                    !reason.contains("privacy boundary"),
                    "`{}` degraded at the choke point on the row that has no boundaries \
                     configured, so the provider-error row is not exercising a provider \
                     error: {reason}",
                    duty.name()
                ),
                Condition::ContentRefused => assert!(
                    reason.contains("privacy boundary"),
                    "`{}` must be refused at the choke point rather than failing some other \
                     way — otherwise this row proves nothing about BR-7: {reason}",
                    duty.name()
                ),
            }

            // The two conditions that must not put a byte on the wire, told
            // apart from each other by *why*: an unresolvable route never built
            // a prompt, and a refused one built it and was stopped at the choke
            // point. Neither may have sent.
            if matches!(
                condition,
                Condition::Unresolvable | Condition::ContentRefused
            ) {
                let wire: String = sent
                    .lock()
                    .expect("capture poisoned")
                    .iter()
                    .map(|b| String::from_utf8_lossy(b).into_owned())
                    .collect();
                assert!(
                    wire.is_empty(),
                    "`{}` under {condition:?} put {} bytes on the wire",
                    duty.name(),
                    wire.len()
                );
                assert!(
                    !wire.contains(SECRET),
                    "BR-7 VIOLATION: boundary content reached the transport for `{}`",
                    duty.name()
                );
            }
        }
    }
    assert_eq!(
        rows,
        Duty::ALL.len() * Condition::ALL.len(),
        "the matrix must be complete"
    );
}

/// **The non-vacuity half of AC-3** (LESSON-485). Every duty's `Resolves` row
/// genuinely changed the outcome, so the failure rows above are a guard holding
/// rather than a fixture that never reached the duty at all.
///
/// Without this, the whole matrix is satisfied by five duties that are never
/// invoked: "the matches survived" and "the context is under budget" are both
/// trivially true of a call site that did nothing.
#[tokio::test]
async fn every_duty_that_resolves_genuinely_changes_its_call_sites_outcome() {
    for duty in Duty::ALL {
        let (route, _sent) = route_for(duty, Condition::Resolves);
        let observed = exercise(duty, &route, false).await;
        assert!(
            observed.took_effect,
            "`{}` resolved and answered, but its call site's outcome is indistinguishable \
             from the duty never having run — so its failure rows prove nothing. \
             Delivered: {}",
            duty.name(),
            observed.delivered
        );
        assert!(observed.degraded.is_none(), "{:?}", observed.degraded);
    }
}

/// The matrix's own coverage claim: it exercises every duty the shared seam
/// serves, named by the seam rather than by this file.
///
/// A sixth duty added to `harness/mod.rs` without a row here leaves its
/// `DutyKind` out of this set and fails — which is the prompt.
#[test]
fn the_matrix_covers_every_duty_the_seam_serves() {
    let covered: Vec<&str> = Duty::ALL.iter().map(|d| d.name()).collect();
    assert_eq!(
        covered,
        vec!["digest", "triage", "shell", "title", "compact"],
        "the five duties on the shared seam (REQ-558 `digest` + REQ-561's four)"
    );
    // And every one of them declares a ceiling, which is what makes AC-11's
    // per-duty bound assertions have something to read (BR-8).
    for duty in Duty::ALL {
        assert!(
            duty.kind().ceiling_bytes() > 0,
            "`{}` declares no output ceiling",
            duty.name()
        );
    }
}
