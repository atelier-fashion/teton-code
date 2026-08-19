//! `/provider test <id>` and `teton provider test <id>` — one consented call,
//! reported typed (REQ-581 BR-2/BR-3/BR-7).
//!
//! The user asks *"is it working?"* on purpose, this module shows them what the
//! answer will cost in shape, waits for a `y`, and then asks the daemon to make
//! the smallest **real** call that provider serves. Nothing here dials anything:
//! the CLI has no network path of its own (project BR-1), so the whole flow is
//! `config/get` → a question → `provider/test` → the daemon's own words.
//!
//! ## What is *not* decided here
//!
//! The outcome. [`ProviderTestOutcome`] is a tagged enum and this module
//! branches on the **variant**, never on the sentence inside it (BR-3,
//! LESSON-456) — a client that read "401" out of a `reason` to decide what
//! happened would be a second classifier drifting from the daemon's. Every
//! `reason` is rendered verbatim and composed nowhere but the daemon: the only
//! vendor-adjacent text that reaches the surface is a sentence built from the
//! status, the dial host, the configured model and the credential *reference*
//! (ADR-3), so no *report* line can carry a credential value.
//!
//! ## The preview echoes, and redacts exactly one thing (LESSON-529)
//!
//! The preview line carries the endpoint **as the config snapshot stores it**,
//! and the confirm question says "that endpoint" rather than naming a host of
//! its own. A display helper that extracted an authority here would be a second
//! parser of the string the daemon is about to POST to, and every divergence
//! between the two would be a lie on the one line a user reads before consenting
//! to spend. The *report*'s host is the daemon's own dial-time reading
//! ([`ProviderTestResult::dial_host`]) for the same reason, travelling as data
//! rather than being re-derived.
//!
//! The one exception is [`crate::displayed_endpoint`], which replaces a
//! `user:password@` with `***@`. A stored endpoint may carry userinfo — the
//! product permits it and dials it as typed (REQ-578) — so an endpoint echoed
//! byte for byte is a password in the scrollback, the session recording and
//! whatever the user pastes into a bug report. That is not a second parser: it
//! is *the* renderer, the same one the registration echo, `provider list` and
//! doctor go through, and it is written to end the authority where the request
//! builder ends it, backslash included. The redaction is visible rather than
//! silent, so the line still says a credential was there.
//!
//! ## Everything but the bytes is testable without a terminal
//!
//! The flow reaches the world through one seam, [`TestIo`]: two RPCs, the
//! [`Surface`] and the [`Prompter`] — [`crate::provider_setup_ui`]'s
//! arrangement, for its reason. That is what lets AC-4's claim ("a decline sends
//! nothing") be a unit assertion on a call counter instead of an e2e that has to
//! prove a negative over a socket.

use teton_protocol::jsonrpc::{error_code, RpcError};
use teton_protocol::methods::{
    ConfigGetParams, ConfigGetResult, ConfigSnapshot, ProviderConfig, ProviderHealth,
    ProviderTestOutcome, ProviderTestParams, ProviderTestResult,
};
use teton_protocol::{ProviderKind, SessionId};

use crate::client::{Connection, UiContext};
use crate::cost_ui::format_usd;
use crate::prompt::{is_yes, Prompter};
use crate::render::{LineKind, Surface};

/// What `/provider test` says when there is no session to act on.
///
/// Reachable only from a context that owns no session, the same guard
/// `/provider setup`'s is — it keeps the id from being fabricated rather than
/// being a line users meet. `provider/test` is session-gated (ADR-5), so a
/// fabricated id would be refused anyway; this is the legible half of that.
const TEST_NEEDS_A_SESSION: &str =
    "`/provider test` needs a session to act on, and this command owns none.";

/// What the flow says to a daemon built before REQ-581.
///
/// A version fact, not a failure, so it wears no `error:` prefix (BUG-152).
const TEST_UNAVAILABLE: &str =
    "this daemon build does not serve provider connection tests — restart it after upgrading. \
     Nothing was sent.";

/// What the flow says to a daemon that does not serve `config/get`.
///
/// Distinct from [`TEST_UNAVAILABLE`] because it names a **different missing
/// method**, and a line that reported the wrong one would send a user looking
/// for a feature that may well be there. The two are far apart in age: this one
/// means a daemon older than the config snapshot itself, which is old enough
/// that nothing else in the session works either.
const CONFIG_UNAVAILABLE: &str =
    "this daemon build does not serve `config/get`, so there is no provider list to check the id \
     against — restart it after upgrading. Nothing was sent.";

/// What a session whose input is not a terminal is told when it did not consent
/// up front (BR-2 / AC-4).
///
/// A refusal rather than `/provider setup`'s degradation, and the difference is
/// what the two commands do: setup can hand a script a recipe to run later,
/// while this command's whole body is an outbound request. There is no reduced
/// version of it to offer, so the answer is the flag that consents in advance.
const NOT_A_TERMINAL: &str = "`/provider test` asks before it sends, and this session's input is \
                              not a terminal — re-run it with `--yes` to consent up front. \
                              Nothing was sent.";

/// The decline at the confirm step, and the EOF that reads as one.
///
/// The fact the user needs is that the machine is exactly as they left it and
/// their provider was not billed.
const TEST_DECLINED: &str = "nothing was sent — the provider was not called and nothing was \
                             recorded.";

/// The confirm question (BR-2, LESSON-470's default-no).
///
/// It says "that endpoint" and names no host of its own: the address is on the
/// preview line directly above, from the snapshot, and a second reading of it
/// here would be the display-vs-dial divergence LESSON-529 is about.
///
/// The size is stated as the two facts the product actually owns rather than as
/// one round number. The probe is a single fixed sentence in — a handful of
/// tokens, and the exact count is the vendor's tokenizer's business, not this
/// binary's — and asks for at most 8 back, which is the daemon's own
/// `PROBE_MAX_TOKENS`. The "≈ 20 tokens" this line used to claim was a guess:
/// the one figure on screen that no measurement backed, on the line whose whole
/// job is to be true before a user consents to spend.
///
/// The 8 is quoted rather than imported — it is a constant in a crate this one
/// does not depend on, and putting a number on the wire to render one sentence
/// would be the wrong trade. It stays honest because it is a *ceiling* the
/// report measures against: a `reached` line prints the tokens the vendor
/// actually billed, so a daemon that raised its budget shows up in the output
/// rather than only in this comment.
const CONFIRM_QUESTION: &str =
    "  this sends one minimal request (a few tokens in, at most 8 out) to that endpoint. \
     proceed?  [y/N] ";

// ---------------------------------------------------------------------------
// The world seam
// ---------------------------------------------------------------------------

/// Everything the flow can reach outside itself: two RPCs, a place to render,
/// and a place to ask.
///
/// The accessors hand out short-lived borrows rather than the seams themselves
/// because the production implementation holds the session's `UiContext` — the
/// same context [`Connection::call`] needs in order to pump events while a
/// request is in flight.
pub(crate) trait TestIo {
    /// Where the flow's lines go.
    fn surface(&mut self) -> &mut dyn Surface;
    /// Where the flow's one question goes.
    fn prompter(&mut self) -> &mut dyn Prompter;
    /// `config/get` — the snapshot the preview and the routing line are read
    /// from.
    ///
    /// # Errors
    /// Propagates a transport failure; a daemon that *answers* with an error
    /// returns it in the inner `Result`.
    fn config_get(
        &mut self,
        params: ConfigGetParams,
    ) -> anyhow::Result<Result<ConfigGetResult, RpcError>>;
    /// `provider/test` — the one call that sends.
    ///
    /// # Errors
    /// As [`Self::config_get`].
    fn provider_test(
        &mut self,
        params: ProviderTestParams,
    ) -> anyhow::Result<Result<ProviderTestResult, RpcError>>;
}

/// The production seam: the session's own connection and context.
pub(crate) struct DaemonIo<'a, 'ctx> {
    conn: &'a mut Connection,
    ctx: &'a mut UiContext<'ctx>,
}

impl<'a, 'ctx> DaemonIo<'a, 'ctx> {
    /// Wire the flow to an open connection and the context it renders through.
    pub(crate) fn new(conn: &'a mut Connection, ctx: &'a mut UiContext<'ctx>) -> Self {
        Self { conn, ctx }
    }
}

impl TestIo for DaemonIo<'_, '_> {
    fn surface(&mut self) -> &mut dyn Surface {
        &mut *self.ctx.surface
    }

    fn prompter(&mut self) -> &mut dyn Prompter {
        &mut *self.ctx.prompter
    }

    fn config_get(
        &mut self,
        params: ConfigGetParams,
    ) -> anyhow::Result<Result<ConfigGetResult, RpcError>> {
        self.conn.call(params, self.ctx)
    }

    fn provider_test(
        &mut self,
        params: ProviderTestParams,
    ) -> anyhow::Result<Result<ProviderTestResult, RpcError>> {
        while_this_clients_probe_is_out(self.ctx, |ctx| self.conn.call(params, ctx))
    }
}

/// Run `call` with the session's "**my** `provider/test` is out" flag raised,
/// and lower it whatever the call answers (verify G2).
///
/// The one writer of the flag [`crate::session_ui::render_event`]'s
/// `provider_tested` arm reads. [`Connection::call`] *is* the event pump — it
/// renders every envelope that arrives while it waits — so the daemon's own
/// `provider_tested` for this call is drawn from inside `call`, which is why the
/// flag is raised around it rather than derived from the answer: there is no
/// answer yet at the moment the event lands.
///
/// A helper rather than three lines inline, because the claim worth pinning is
/// the unhappy path. Lowered on **both** endings — a transport error and a
/// daemon's `Err` included — and by holding the result rather than returning
/// early, since a flag left raised silences this session's *next* notice, which
/// would be about a test some other client ran and this one has no report for.
fn while_this_clients_probe_is_out<T>(
    ctx: &mut UiContext,
    call: impl FnOnce(&mut UiContext) -> T,
) -> T {
    ctx.state.provider_test_in_flight = true;
    let answered = call(ctx);
    ctx.state.provider_test_in_flight = false;
    answered
}

// ---------------------------------------------------------------------------
// The flow
// ---------------------------------------------------------------------------

/// Preview, confirm, call, report — the whole of `/provider test <id>`
/// (BR-2/BR-3).
///
/// `auto_yes` is the session's `--yes`, the one flag REQ-555 BR-4b established
/// for exactly this ("the user consented at the command line"), and
/// `typed_input` is the world-fact that says whether a question can be asked at
/// all. Between them they decide the gate; nothing here reads stdin or a
/// terminal itself.
///
/// The **order** is load-bearing. There is exactly one call to
/// [`TestIo::provider_test`] in this function and the gate is above it, so "a
/// decline sends nothing" is a property of the control flow rather than of
/// several branches remembering to check (AC-4). The local-tier answer sits
/// *above* the gate on purpose: it makes no call, so there is nothing for a
/// pipe to consent to, and answering a question about a provider that dials
/// nothing does not need a terminal.
///
/// # Errors
///
/// Propagates a transport error. A daemon that *answers* — with an error, or
/// with "no such method" — is reported on the surface and returns `Ok`: a
/// refused test ends the command, never the session.
pub(crate) fn run(
    io: &mut dyn TestIo,
    session_id: &SessionId,
    provider_id: &str,
    auto_yes: bool,
    typed_input: bool,
) -> anyhow::Result<()> {
    let snapshot = match io.config_get(ConfigGetParams::default())? {
        Ok(result) => result.snapshot,
        Err(err) if err.code == error_code::METHOD_NOT_FOUND => {
            io.surface().line(LineKind::Notice, CONFIG_UNAVAILABLE);
            return Ok(());
        }
        Err(err) => {
            io.surface().line(
                LineKind::Error,
                &format!("provider test could not read your config: {}", err.message),
            );
            return Ok(());
        }
    };

    // The id is checked against the daemon's own list rather than sent
    // hopefully, because the useful answer to a typo is the set of ids that
    // would have worked — which only the snapshot has.
    let Some(provider) = snapshot
        .providers
        .iter()
        .find(|candidate| candidate.id.0 == provider_id)
    else {
        io.surface()
            .line(LineKind::Error, &unknown_id_line(provider_id, &snapshot));
        return Ok(());
    };
    // Cloned out of the snapshot so the borrow ends here: the report reads the
    // snapshot again for its routing line, after `io` has been borrowed mutably.
    let provider = provider.clone();

    // BR-8 / AC-7, and it sends nothing — so it is answered before the gate.
    //
    // A `kind = "local"` provider has no host, so there is no preview to show,
    // no question to ask and no call to make; asking the daemon in order to be
    // told that would spend a round trip to learn a fact this snapshot already
    // carries, and — on a pipe — would print the `--yes` remedy for a request
    // that was never going to be made. What this line deliberately does *not*
    // do is describe the tier: `teton doctor` owns that state (REQ-580's
    // classification), and a sentence composed here would be this module
    // forming an opinion about a tier it cannot see.
    //
    // AC-7's guarantee does not rest on this branch. The daemon refuses a local
    // provider whatever the client thinks, which is what covers a config edited
    // between the snapshot and a call — there simply is no call here to race
    // with, so the refusal is the tetond-side test's to prove.
    if provider.kind == ProviderKind::Local {
        io.surface()
            .line(LineKind::Notice, &local_tier_line(provider_id));
        return Ok(());
    }

    // Verify G3, and the local branch's reasoning applied to the second row that
    // cannot be dialed: a provider with no endpoint has no address, so there is
    // no preview to show, no question to ask and no call to make.
    //
    // Above the gate for the local branch's reason, and answered *client-side*
    // for the reason the old preview clause got wrong: nothing composes an
    // address at this point. Composition happens at registration and is written
    // to config; `provider_test` takes `unwrap_or_default()` and refuses
    // ("endpoint has no host the transport could dial"). So the round trip buys
    // a fact this snapshot already carries — and on a pipe it would print the
    // `--yes` remedy for a call that was never going to be made.
    //
    // See [`no_endpoint_line`] on reachability: `Config::validate` refuses this
    // shape at load, so it is the snapshot this client is *handed* that is being
    // defended against, not a config a running daemon holds. The daemon's own
    // refusal is unaffected and stays where it is tested — this is the legible
    // half, never the enforcing one.
    if provider
        .endpoint
        .as_deref()
        .is_none_or(|endpoint| endpoint.trim().is_empty())
    {
        io.surface()
            .line(LineKind::Notice, &no_endpoint_line(provider_id));
        return Ok(());
    }

    // The gate, ahead of everything that could reach the wire. A session that
    // cannot be asked and did not consent in advance is answered here and
    // nowhere else, so no later branch can send on its behalf.
    if !typed_input && !auto_yes {
        io.surface().line(LineKind::Error, NOT_A_TERMINAL);
        return Ok(());
    }

    for line in preview_lines(&provider) {
        io.surface().line(LineKind::Info, &line);
    }

    // LESSON-470: the call is the costly wrong answer, so silence declines —
    // an empty answer and EOF are both "no", and only an explicit yes sends.
    if !auto_yes {
        let confirmed =
            matches!(io.prompter().ask(CONFIRM_QUESTION), Some(answer) if is_yes(&answer));
        if !confirmed {
            io.surface().line(LineKind::Notice, TEST_DECLINED);
            return Ok(());
        }
    }

    let result = match io.provider_test(ProviderTestParams {
        session_id: session_id.clone(),
        provider_id: provider.id.clone(),
    })? {
        Ok(result) => result,
        Err(err) if err.code == error_code::METHOD_NOT_FOUND => {
            io.surface().line(LineKind::Notice, TEST_UNAVAILABLE);
            return Ok(());
        }
        Err(err) => {
            // The daemon answered "no" — an unknown provider it does not hold, a
            // credential reference it could not resolve, a session this
            // connection may not drive. Its sentence is carried verbatim,
            // because it is the end that knows why.
            io.surface().line(
                LineKind::Error,
                &format!("provider test failed: {}", err.message),
            );
            return Ok(());
        }
    };

    // The routing line is read from a snapshot taken **before** the call, and a
    // `reached` outcome is exactly the ending that can have invalidated it: the
    // daemon screens an `Unavailable` provider out of its routing resolution
    // (`teton_core::category`), so a provider the health map had written off
    // shows up in the pre-test snapshot with nothing dispatching to it — and
    // this test is what just restored it (BR-4, AC-5). Re-read on that one
    // outcome, so the "what now routes here" sentence describes the machine the
    // user is left with rather than the one they started with.
    //
    // Only on `reached`: a failure changed no routing, and a second `config/get`
    // after every outcome would be a round trip spent to re-read an unchanged
    // answer.
    let snapshot = match &result.outcome {
        ProviderTestOutcome::Reached { .. } => refreshed_snapshot(io, snapshot),
        ProviderTestOutcome::Refused { .. }
        | ProviderTestOutcome::UnknownModel { .. }
        | ProviderTestOutcome::RateLimited { .. }
        | ProviderTestOutcome::ServerError { .. }
        | ProviderTestOutcome::Unreachable { .. }
        | ProviderTestOutcome::NotACompletion { .. }
        | ProviderTestOutcome::TimedOut { .. } => snapshot,
    };

    for line in report_lines(&result, &snapshot) {
        io.surface().line(LineKind::Notice, &line);
    }
    Ok(())
}

/// The config as it is **after** a test that moved health, falling back to the
/// reading taken before it.
///
/// A failed re-read is not worth a word on screen: `before` is a true reading of
/// the routing table a moment ago, and a report that dropped its routing line
/// because a follow-up round trip hiccuped would lose more than it saved. The
/// call that mattered already happened and is already reported.
fn refreshed_snapshot(io: &mut dyn TestIo, before: ConfigSnapshot) -> ConfigSnapshot {
    match io.config_get(ConfigGetParams::default()) {
        Ok(Ok(result)) => result.snapshot,
        Ok(Err(_)) | Err(_) => before,
    }
}

// ---------------------------------------------------------------------------
// Content (pure)
// ---------------------------------------------------------------------------

/// What `/provider test <id>` says about a provider that dials nothing (BR-8).
///
/// Two sentences and no third: what the id *is*, and where its state is
/// reported. It says nothing about whether the local tier is ready, because this
/// module cannot see that and `teton doctor` can.
fn local_tier_line(id: &str) -> String {
    format!(
        "`{id}` is the local tier: a connection test dials nothing. `teton doctor` reports its \
         state."
    )
}

/// What a typo is answered with: the ids that would have worked.
fn unknown_id_line(typed: &str, snapshot: &ConfigSnapshot) -> String {
    if snapshot.providers.is_empty() {
        return format!(
            "no provider is registered as `{typed}` — this machine has none at all. \
             `/provider setup` registers one."
        );
    }
    let registered: Vec<&str> = snapshot
        .providers
        .iter()
        .map(|provider| provider.id.0.as_str())
        .collect();
    format!(
        "no provider is registered as `{typed}` — this machine has `{}`. \
         `/provider setup` registers another.",
        registered.join("`, `")
    )
}

/// What a provider with no stored endpoint is answered with (verify G3).
///
/// This used to be a clause on the preview line — "no endpoint stored; this kind
/// composes its own" — and it was not true at the moment it was shown.
/// `teton_core::compose_endpoint` does default an `anthropic` row to the
/// vendor's address, but it runs at **registration**, and what it composes is
/// *written to config*; by the time this flow reads the snapshot, an endpoint
/// either was stored or never will be. Nothing composes one afterwards:
/// `provider_test`, `build_provider` and `build_remote_transport` all take
/// `provider.endpoint.clone().unwrap_or_default()`, and the daemon refuses the
/// empty string with "endpoint has no host the transport could dial". So the old
/// sentence promised a fill-in that nothing performs, on the one line whose whole
/// job is to be true before a user consents to spend.
///
/// It is answered here instead, [`local_tier_line`]'s way: there is nothing to
/// dial, so there is nothing to preview, nothing to consent to and no call to
/// make. It names the command that fixes it, because "no endpoint stored" is a
/// state the user can leave.
///
/// Reachability, stated rather than implied: `Config::validate` refuses a remote
/// provider with a blank endpoint outright (`ConfigError::MissingEndpoint`), so
/// a daemon that *started* cannot serve this shape in a snapshot today. The
/// branch is defence against the snapshot the client is handed — a daemon of
/// another version, a field that stops being echoed — and its worth is that the
/// honest answer costs a line, while the old one spent a round trip to be
/// contradicted by the daemon's own refusal.
fn no_endpoint_line(id: &str) -> String {
    format!(
        "`{id}` has no endpoint stored, so there is nothing to dial. \
         `teton provider add {id} --endpoint <url> --model <model>` sets one."
    )
}

/// The one line the user reads before consenting (BR-2).
///
/// Id, kind, model and the **stored** endpoint, echoed rather than parsed
/// (LESSON-529). A provider with no endpoint at all never reaches here — the
/// flow answers it with [`no_endpoint_line`] and stops — so the fallback below
/// is unreachable in production and says only what it can defend: that the
/// config holds no address.
///
/// The endpoint goes through [`crate::displayed_endpoint`] on the way out, which
/// is the single thing between the stored bytes and the screen: a stored
/// endpoint may carry `user:password@` (REQ-578 stores and dials one as typed),
/// and this line is printed to a terminal, a scrollback and a pasted bug report.
/// Every other CLI line that prints an endpoint goes through the same helper —
/// the registration echo, `provider list`, doctor's advisory — and this was the
/// one that did not.
fn preview_lines(provider: &ProviderConfig) -> Vec<String> {
    let model = provider.model.as_deref().unwrap_or("no model configured");
    let endpoint = match provider.endpoint.as_deref() {
        Some(endpoint) if !endpoint.trim().is_empty() => crate::displayed_endpoint(endpoint),
        _ => "no endpoint stored".to_owned(),
    };
    vec![format!(
        "  provider:  {} ({}, {model}) — {endpoint}",
        provider.id,
        crate::kind_label(provider.kind),
    )]
}

/// The verdict word for an outcome, and nothing else.
///
/// Exhaustive rather than a lookup, so a new [`ProviderTestOutcome`] variant
/// cannot reach a surface without somebody deciding what it is called.
///
/// A `String` rather than a `&'static str` for exactly one variant's sake:
/// [`ProviderTestOutcome::TimedOut`]'s verdict *is* the bound it stopped at, and
/// that figure is a typed field on the outcome. Composing it here is the
/// opposite of the prose-reading BR-3 forbids — the number travelled as a
/// number, and this is the one place it becomes words.
fn outcome_verb(outcome: &ProviderTestOutcome) -> String {
    match outcome {
        ProviderTestOutcome::Reached { .. } => "reachable".to_owned(),
        ProviderTestOutcome::Refused { .. } => "refused".to_owned(),
        ProviderTestOutcome::UnknownModel { .. } => "model unknown".to_owned(),
        ProviderTestOutcome::RateLimited { .. } => "rate limited".to_owned(),
        ProviderTestOutcome::ServerError { .. } => "server error".to_owned(),
        ProviderTestOutcome::Unreachable { .. } => "unreachable".to_owned(),
        // Three verdicts where there used to be one, because they are three
        // different next moves: check the address, check the path, wait or
        // check whether the vendor is up.
        ProviderTestOutcome::NotACompletion { .. } => {
            "answered, but not with a completion".to_owned()
        }
        ProviderTestOutcome::TimedOut { after_secs, .. } => {
            format!("no answer within {after_secs} s")
        }
    }
}

/// One outcome as a sentence — the shared vocabulary the report line and the
/// `provider_tested` event notice are both built from (REQ-581 BR-3).
///
/// One function for both, which is what the protocol's own note asks for: the
/// event's `outcome` is byte-identical to the RPC answer's, and two renderers
/// would be two spellings of one value for a reader to find subtly different.
///
/// Every failure variant's detail is the daemon's `reason`, verbatim. The two
/// that carry none — [`ProviderTestOutcome::Reached`] and
/// [`ProviderTestOutcome::RateLimited`] — get a sentence composed from their own
/// typed fields here, which is not the re-reading BR-3 forbids: nothing is being
/// inferred from prose, the variant *is* the classification.
pub(crate) fn outcome_sentence(outcome: &ProviderTestOutcome) -> String {
    let verb = outcome_verb(outcome);
    match outcome {
        ProviderTestOutcome::Reached {
            latency_ms,
            input_tokens,
            output_tokens,
            usd_micros,
        } => {
            // "unpriced", never "$0.000000": a cost is recorded or it is not, and
            // a zero standing in for "no price on file" shows an estimate as an
            // actual (REQ-544 BR-2).
            let cost = match usd_micros {
                Some(micros) => format!("{} recorded", format_usd(*micros)),
                None => "unpriced".to_owned(),
            };
            format!(
                "{verb} — answered in {} ({input_tokens} in / {output_tokens} out, {cost})",
                format_latency(*latency_ms),
            )
        }
        ProviderTestOutcome::Refused { reason, .. }
        | ProviderTestOutcome::UnknownModel { reason, .. }
        | ProviderTestOutcome::ServerError { reason, .. }
        | ProviderTestOutcome::Unreachable { reason }
        | ProviderTestOutcome::NotACompletion { reason }
        | ProviderTestOutcome::TimedOut { reason, .. } => {
            format!("{verb} — {reason}. Nothing else was sent")
        }
        ProviderTestOutcome::RateLimited { retry_after_secs } => {
            // v1's transport surfaces no `Retry-After` by design (ADR-2 / OQ-5,
            // deferred rather than dropped), so the absent case is the one users
            // meet and it says something actionable rather than nothing.
            let wait = match retry_after_secs {
                Some(secs) => format!("try again in {secs}s"),
                None => "try again shortly".to_owned(),
            };
            format!("{verb} — the vendor is holding calls off; {wait}. Nothing else was sent")
        }
    }
}

/// The health a test left a provider in, in the router's own words (BR-4).
pub(crate) fn health_name(health: ProviderHealth) -> &'static str {
    match health {
        ProviderHealth::Healthy => "healthy",
        ProviderHealth::Degraded => "degraded",
        ProviderHealth::Unavailable => "unavailable",
    }
}

/// Wall time as a person reads it. Integer arithmetic — a latency is a measured
/// figure and rounding it through a float would be a second value.
fn format_latency(ms: u64) -> String {
    if ms < 1_000 {
        return format!("{ms} ms");
    }
    format!("{}.{} s", ms / 1_000, (ms % 1_000) / 100)
}

/// The report a finished test renders (BR-3/BR-4).
///
/// One line for what came back and where health landed, plus — on the outcome
/// that changed nothing about the configuration — one line for what now routes
/// there, and — on the outcome a credential explains — the two ways to replace
/// it.
fn report_lines(result: &ProviderTestResult, snapshot: &ConfigSnapshot) -> Vec<String> {
    let mut lines = vec![format!(
        "  {} {}: {}; provider health: {}.",
        result.provider_id,
        result.model,
        outcome_sentence(&result.outcome),
        health_name(result.health_after),
    )];
    match &result.outcome {
        // BR-4's other half: the test answered "yes", and the useful next fact is
        // what that provider is on the hook for.
        ProviderTestOutcome::Reached { .. } => {
            lines.push(routing_line(snapshot, result.provider_id.0.as_str()));
        }
        // The credential is what a refusal is usually about, so the remedy names
        // both ways to replace one — the in-session walkthrough and the shell
        // command — rather than assuming which surface the reader is on.
        ProviderTestOutcome::Refused { .. } => {
            lines.push(format!(
                "  `/provider setup {}` stores a new key, or `teton provider add {} --model {}` \
                 from a shell.",
                result.provider_id, result.provider_id, result.model,
            ));
        }
        ProviderTestOutcome::UnknownModel { .. }
        | ProviderTestOutcome::RateLimited { .. }
        | ProviderTestOutcome::ServerError { .. }
        | ProviderTestOutcome::Unreachable { .. }
        | ProviderTestOutcome::NotACompletion { .. }
        | ProviderTestOutcome::TimedOut { .. } => {}
    }
    lines
}

/// What routes to a provider, from the snapshot's **resolved** routing.
///
/// The daemon already answered this question — `tiers` carries each tier's
/// effective provider and `routing` carries every category's — so this reads
/// those answers rather than re-deriving inheritance, which would be a second
/// resolver in a client (BR-3's rule, applied to routing).
///
/// Categories are filtered on `reached`, so the parenthetical names the ones
/// that actually dispatch today. A category declared with no call site yet would
/// otherwise pad the line with work this provider will not be asked for.
fn routing_line(snapshot: &ConfigSnapshot, id: &str) -> String {
    let tiers: Vec<&str> = snapshot
        .tiers
        .iter()
        .filter(|row| row.provider_id.as_ref().is_some_and(|bound| bound.0 == id))
        .map(|row| row.tier.as_str())
        .collect();
    let categories: Vec<&str> = snapshot
        .routing
        .iter()
        .filter(|row| row.reached && row.provider_id.as_ref().is_some_and(|bound| bound.0 == id))
        .map(|row| row.category.as_str())
        .collect();

    match (tiers.is_empty(), categories.is_empty()) {
        (true, true) => format!(
            "  nothing routes to `{id}` yet — `teton policy set-tier <tier> {id}` binds it."
        ),
        (true, false) => format!(
            "  no tier routes here, but these categories do: {}.",
            categories.join(", ")
        ),
        (false, true) => format!(
            "  `{}` routes here, and no category dispatches on it yet.",
            tiers.join("`, `")
        ),
        (false, false) => format!(
            "  `{}` routes here ({}).",
            tiers.join("`, `"),
            categories.join(", ")
        ),
    }
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Run `/provider test <id>` on the session's own connection and context.
///
/// The session-id guard lives here rather than in [`run`] because it is a
/// property of the *caller* — the shell subcommand creates a session precisely
/// so it has one to name (ADR-5), and only a slash handler can be reached
/// without one.
///
/// # Errors
///
/// Propagates a transport error, as [`run`] does.
pub(crate) fn run_in_session(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    provider_id: &str,
) -> anyhow::Result<()> {
    let Some(session_id) = ctx.session_id.clone() else {
        ctx.surface.line(LineKind::Error, TEST_NEEDS_A_SESSION);
        return Ok(());
    };
    // Read before the context is borrowed by the seam: the two world-facts the
    // gate needs belong to the session and are never re-derived by a handler.
    let auto_yes = ctx.auto_accept_model;
    let typed_input = ctx.typed_input;
    let mut io = DaemonIo::new(conn, ctx);
    run(&mut io, &session_id, provider_id, auto_yes, typed_input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::ScriptedPrompter;
    use crate::render::{RecordingSurface, Rendered};
    use teton_protocol::methods::{CategoryRouteView, ContentClass, TierRouteView};
    use teton_protocol::{BindingSource, Category, ProviderId, Tier, TierBindingSource};

    /// The credential value that must never reach a rendered line. Distinctive
    /// enough that a sweep over the surface means something (LESSON-519).
    ///
    /// It is **planted in the fixture's own endpoint** rather than merely being
    /// absent from it, because a "never the key" assertion over a fixture that
    /// holds no key is a test of nothing: it passed on a preview that printed
    /// the stored endpoint byte for byte, which is precisely how a userinfo
    /// credential reached the screen. A stored `user:password@` is a shape the
    /// product permits and dials as typed (REQ-578), so the fixture carries one
    /// and every sweep below is answering a real question.
    const PLANTED_KEY: &str = "sk-planted-provider-test-key";

    /// The fixture endpoint, credential and all — as `config/get` would report a
    /// provider registered with userinfo in its URL.
    const ENDPOINT_WITH_KEY: &str =
        "https://u:sk-planted-provider-test-key@api.moonshot.ai/v1/chat/completions";

    /// The same endpoint as it may be **shown**: the authority's userinfo
    /// replaced, the rest untouched (`crate::displayed_endpoint`).
    const ENDPOINT_SHOWN: &str = "https://***@api.moonshot.ai/v1/chat/completions";

    /// The credential *reference*, which the daemon's own `refused` sentence
    /// does name — and which AC-2 asserts is what prints in its place.
    const KEY_REF: &str = "keychain://teton/kimi";

    /// The seam, wired to canned answers and a recording surface.
    struct FakeIo {
        surface: RecordingSurface,
        prompter: ScriptedPrompter,
        snapshot: Result<ConfigSnapshot, RpcError>,
        /// What the **second** `config/get` answers, when the flow re-reads the
        /// config after a `reached` outcome. `None` means "the same as the
        /// first", which is every test that is not about the re-read.
        snapshot_after: Option<Result<ConfigSnapshot, RpcError>>,
        /// How many `config/get` calls the flow made. The re-read is a round
        /// trip on a user's daemon, so "only after `reached`" is asserted as a
        /// count rather than inferred from a rendered line.
        snapshots_read: usize,
        outcome: Result<ProviderTestResult, RpcError>,
        /// Every `provider/test` this flow sent, as sent. The whole of AC-4 is
        /// an assertion that this is empty.
        tests: Vec<ProviderTestParams>,
    }

    impl FakeIo {
        fn new(answers: &[&str]) -> Self {
            Self {
                surface: RecordingSurface::new(),
                prompter: ScriptedPrompter::new(answers),
                snapshot: Ok(snapshot()),
                snapshot_after: None,
                snapshots_read: 0,
                outcome: Ok(result(reached())),
                tests: Vec::new(),
            }
        }

        /// Every line rendered, in order, as one string.
        fn rendered(&self) -> String {
            self.surface
                .calls
                .iter()
                .filter_map(|call| match call {
                    Rendered::Line(_, text) => Some(text.as_str()),
                    Rendered::Fragment(_) | Rendered::Repaint(..) => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
    }

    impl TestIo for FakeIo {
        fn surface(&mut self) -> &mut dyn Surface {
            &mut self.surface
        }

        fn prompter(&mut self) -> &mut dyn Prompter {
            &mut self.prompter
        }

        fn config_get(
            &mut self,
            _params: ConfigGetParams,
        ) -> anyhow::Result<Result<ConfigGetResult, RpcError>> {
            self.snapshots_read += 1;
            if self.snapshots_read > 1 {
                if let Some(after) = self.snapshot_after.clone() {
                    return Ok(after.map(|snapshot| ConfigGetResult { snapshot }));
                }
            }
            Ok(self
                .snapshot
                .clone()
                .map(|snapshot| ConfigGetResult { snapshot }))
        }

        fn provider_test(
            &mut self,
            params: ProviderTestParams,
        ) -> anyhow::Result<Result<ProviderTestResult, RpcError>> {
            self.tests.push(params);
            Ok(self.outcome.clone())
        }
    }

    fn session() -> SessionId {
        SessionId::from("sess-provider-test")
    }

    fn provider(id: &str, kind: ProviderKind, endpoint: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            id: ProviderId::from(id),
            kind,
            endpoint: endpoint.map(str::to_owned),
            model: Some("kimi-k3".to_owned()),
            auth_ref: Some(format!("keychain://teton/{id}")),
            max_context: None,
            context_budget_cap: None,
        }
    }

    /// A machine with one remote provider bound to `build`, one local tier, and
    /// two categories dispatching there — the shape the report's routing line is
    /// read from.
    fn snapshot() -> ConfigSnapshot {
        ConfigSnapshot {
            providers: vec![
                provider(
                    "kimi",
                    ProviderKind::OpenaiCompatible,
                    Some(ENDPOINT_WITH_KEY),
                ),
                ProviderConfig {
                    model: None,
                    ..provider("onlocal", ProviderKind::Local, None)
                },
            ],
            tiers: vec![
                TierRouteView {
                    tier: Tier::Build,
                    provider_id: Some(ProviderId::from("kimi")),
                    fallback_id: None,
                    source: TierBindingSource::Configured,
                },
                TierRouteView {
                    tier: Tier::Think,
                    provider_id: None,
                    fallback_id: None,
                    source: TierBindingSource::Unbound,
                },
            ],
            routing: vec![
                category(Category::Edit, Some("kimi"), true),
                category(Category::Shell, Some("kimi"), true),
                // Declared, no call site yet: bound to the same provider and
                // deliberately absent from the rendered line.
                category(Category::Design, Some("kimi"), false),
                category(Category::Triage, None, true),
            ],
            ..ConfigSnapshot::default()
        }
    }

    fn category(name: Category, provider_id: Option<&str>, reached: bool) -> CategoryRouteView {
        CategoryRouteView {
            category: name,
            tier: Tier::Build,
            provider_id: provider_id.map(ProviderId::from),
            fallback_id: None,
            source: BindingSource::TierInheritance,
            reached,
            content_class: ContentClass::TurnContext,
            reason: "the 'build' tier is bound.".to_owned(),
        }
    }

    fn reached() -> ProviderTestOutcome {
        ProviderTestOutcome::Reached {
            latency_ms: 1_400,
            input_tokens: 2_040,
            output_tokens: 21,
            usd_micros: Some(6_400),
        }
    }

    fn result(outcome: ProviderTestOutcome) -> ProviderTestResult {
        ProviderTestResult {
            provider_id: ProviderId::from("kimi"),
            model: "kimi-k3".to_owned(),
            dial_host: "api.moonshot.ai".to_owned(),
            outcome,
            health_after: ProviderHealth::Healthy,
        }
    }

    // -------------------------------------------------------------------
    // BR-2 — the preview and the gate
    // -------------------------------------------------------------------

    /// BR-2: the line a user reads before consenting names the provider, the
    /// kind, the model and the endpoint **as stored** — and the question that
    /// follows says what it will cost in shape.
    ///
    /// The endpoint is asserted whole rather than by its host: the claim the
    /// preview makes is "this is the string Teton POSTs to" (REQ-578), and a
    /// test that checked only a substring would pass for a helper that had
    /// started parsing the URL (LESSON-529). Whole, with exactly one difference
    /// — the userinfo is `***@`, through the same `displayed_endpoint` every
    /// other endpoint-bearing line in this CLI goes through. Path, query and
    /// host are untouched, which is what makes this a redaction rather than a
    /// second reading of the address.
    #[test]
    fn the_preview_names_the_provider_and_the_stored_endpoint_with_userinfo_masked() {
        let mut io = FakeIo::new(&["y"]);
        run(&mut io, &session(), "kimi", false, true).expect("the flow renders");

        let rendered = io.rendered();
        assert!(
            rendered.contains("provider:  kimi (openai-compatible, kimi-k3)"),
            "{rendered}"
        );
        assert!(
            rendered.contains(&format!("— {ENDPOINT_SHOWN}")),
            "the stored endpoint must be echoed whole but for its userinfo: {rendered}"
        );
        assert!(
            rendered.contains("***@"),
            "the redaction has to be visible, or the line hides that a credential is stored: \
             {rendered}"
        );
        assert!(
            !rendered.contains(PLANTED_KEY),
            "a credential value reached the preview: {rendered}"
        );
        assert!(
            io.prompter.any_question_contains(
                "one minimal request (a few tokens in, at most 8 out) to that endpoint"
            ),
            "the question must state the size honestly — the daemon's own budget, not a \
             guessed total: {:?}",
            io.prompter.questions
        );
        assert!(
            io.prompter.any_question_contains("[y/N]"),
            "the default must be visibly no: {:?}",
            io.prompter.questions
        );
    }

    /// AC-4: `n` sends nothing. Asserted on the seam's call log, which is the
    /// only place "nothing was sent" is a fact rather than a sentence.
    #[test]
    fn a_decline_makes_no_call_at_all() {
        for answer in ["n", "N", "no", "", " ", "yes please"] {
            let mut io = FakeIo::new(&[answer]);
            run(&mut io, &session(), "kimi", false, true).expect("the flow renders");
            assert!(
                io.tests.is_empty(),
                "`{answer}` reached provider/test: {:?}",
                io.tests
            );
            assert!(
                io.rendered().contains("nothing was sent"),
                "{}",
                io.rendered()
            );
        }
    }

    /// AC-4: EOF at the question is a decline, not a default-yes. A prompter
    /// with no answers left returns `None`, which is what Ctrl-D produces.
    #[test]
    fn eof_at_the_question_sends_nothing() {
        let mut io = FakeIo::new(&[]);
        run(&mut io, &session(), "kimi", false, true).expect("the flow renders");
        assert!(io.tests.is_empty(), "{:?}", io.tests);
        assert_eq!(
            io.prompter.asked, 1,
            "the question was put and not answered"
        );
        assert!(
            io.rendered().contains("nothing was sent"),
            "{}",
            io.rendered()
        );
    }

    /// AC-4's second half: a piped session that did not consent up front sends
    /// nothing and says what would have let it.
    #[test]
    fn a_piped_session_without_yes_sends_nothing_and_says_why() {
        let mut io = FakeIo::new(&["y"]);
        run(&mut io, &session(), "kimi", false, false).expect("the flow renders");

        assert!(io.tests.is_empty(), "{:?}", io.tests);
        assert_eq!(
            io.prompter.asked, 0,
            "a session that cannot be asked is not asked"
        );
        let rendered = io.rendered();
        assert!(rendered.contains("--yes"), "{rendered}");
        assert!(rendered.contains("Nothing was sent"), "{rendered}");
    }

    /// BR-2's other side: `--yes` is the consent, so the question is not put and
    /// the call goes — including on a pipe, which is the whole point of the flag.
    #[test]
    fn the_yes_flag_consents_up_front_and_asks_nothing() {
        for typed_input in [true, false] {
            let mut io = FakeIo::new(&[]);
            run(&mut io, &session(), "kimi", true, typed_input).expect("the flow renders");
            assert_eq!(
                io.tests.len(),
                1,
                "exactly one call, typed_input={typed_input}"
            );
            assert_eq!(io.tests[0].provider_id, ProviderId::from("kimi"));
            assert_eq!(io.tests[0].session_id, session());
            assert_eq!(io.prompter.asked, 0, "--yes must consume no prompt");
        }
    }

    /// A typo is answered with the ids that would have worked, and reaches the
    /// wire not at all.
    #[test]
    fn an_unknown_id_names_the_registered_ones_and_sends_nothing() {
        let mut io = FakeIo::new(&["y"]);
        run(&mut io, &session(), "kimu", true, true).expect("the flow renders");

        assert!(io.tests.is_empty(), "{:?}", io.tests);
        let rendered = io.rendered();
        assert!(
            rendered.contains("no provider is registered as `kimu`"),
            "{rendered}"
        );
        assert!(rendered.contains("`kimi`"), "{rendered}");
        assert!(rendered.contains("`onlocal`"), "{rendered}");

        // A machine with none at all says that, rather than naming an empty list.
        let mut io = FakeIo::new(&["y"]);
        io.snapshot = Ok(ConfigSnapshot::default());
        run(&mut io, &session(), "kimi", true, true).expect("the flow renders");
        assert!(io.tests.is_empty(), "{:?}", io.tests);
        assert!(io.rendered().contains("none at all"), "{}", io.rendered());
    }

    /// BR-8 / AC-7: a local provider is answered from the snapshot and **no
    /// call is made at all** — no preview, no question, and no `provider/test`.
    ///
    /// The RPC used to be sent anyway, so that the daemon's own refusal could be
    /// rendered. Two things were wrong with that. It relied on the daemon still
    /// reading `kind = local` at the moment it answered, which is a race the
    /// client has no need to enter: nothing here needs the daemon's opinion to
    /// know that a provider with no host has no connection to test. And it sat
    /// *below* the non-terminal gate, so a piped session was told to re-run with
    /// `--yes` — consent advice for a call that was never going to be made.
    ///
    /// AC-7's own claim (the daemon refuses `kind = local`) is unaffected and is
    /// tested where it lives, in `tetond`.
    #[test]
    fn a_local_provider_is_answered_without_a_call() {
        for typed_input in [true, false] {
            let mut io = FakeIo::new(&["y"]);
            run(&mut io, &session(), "onlocal", false, typed_input).expect("the flow renders");

            assert!(
                io.tests.is_empty(),
                "a local provider must reach provider/test not at all (typed_input={typed_input}): \
                 {:?}",
                io.tests
            );
            assert_eq!(io.prompter.asked, 0, "there is nothing to consent to");
            let rendered = io.rendered();
            assert!(
                rendered.contains("`onlocal` is the local tier: a connection test dials nothing."),
                "{rendered}"
            );
            assert!(
                rendered.contains("teton doctor"),
                "the answer to \"does it work\" for a local tier is doctor's: {rendered}"
            );
            assert!(
                !rendered.contains("one minimal request"),
                "a provider with no host must not be previewed as one that has: {rendered}"
            );
            assert!(
                !rendered.contains("--yes"),
                "a call that will not be made must not ask for consent to make it: {rendered}"
            );
        }
    }

    // -------------------------------------------------------------------
    // BR-3 — the typed report
    // -------------------------------------------------------------------

    /// BR-3: every variant renders a distinct verdict, and the daemon's `reason`
    /// is carried verbatim. Asserted by variant, never by parsing prose.
    #[test]
    fn every_outcome_variant_renders_its_own_line() {
        let cases: [(ProviderTestOutcome, &str, &str); 8] = [
            (
                reached(),
                "reachable",
                "answered in 1.4 s (2040 in / 21 out, $0.006400 recorded)",
            ),
            (
                ProviderTestOutcome::Refused {
                    status: 401,
                    reason: "HTTP 401 from api.moonshot.ai — the vendor did not accept the \
                             credential at keychain://teton/kimi"
                        .to_owned(),
                },
                "refused",
                "HTTP 401 from api.moonshot.ai",
            ),
            (
                ProviderTestOutcome::UnknownModel {
                    status: 404,
                    reason: "HTTP 404 from api.moonshot.ai — no model `kimi-k3` there".to_owned(),
                },
                "model unknown",
                "no model `kimi-k3` there",
            ),
            (
                ProviderTestOutcome::RateLimited {
                    retry_after_secs: None,
                },
                "rate limited",
                "try again shortly",
            ),
            (
                ProviderTestOutcome::ServerError {
                    status: 503,
                    reason: "HTTP 503 from api.moonshot.ai".to_owned(),
                },
                "server error",
                "HTTP 503 from api.moonshot.ai",
            ),
            (
                ProviderTestOutcome::Unreachable {
                    reason: "could not reach api.moonshot.ai: a transport failure".to_owned(),
                },
                "unreachable",
                "could not reach api.moonshot.ai: a transport failure",
            ),
            (
                ProviderTestOutcome::NotACompletion {
                    reason: "api.moonshot.ai answered, but not with a completion (no tokens, no \
                             text)"
                        .to_owned(),
                },
                "answered, but not with a completion",
                "api.moonshot.ai answered, but not with a completion",
            ),
            (
                ProviderTestOutcome::TimedOut {
                    after_secs: 30,
                    // The figure the verdict states comes from `after_secs`, not
                    // from this sentence — which is why the sentence does not
                    // carry one (BR-3).
                    reason: "nothing came back from api.moonshot.ai before the test stopped \
                             waiting"
                        .to_owned(),
                },
                "no answer within 30 s",
                "nothing came back from api.moonshot.ai",
            ),
        ];

        let mut verdicts = Vec::new();
        for (outcome, verb, detail) in cases {
            let mut io = FakeIo::new(&[]);
            io.outcome = Ok(result(outcome));
            run(&mut io, &session(), "kimi", true, true).expect("the flow renders");

            let rendered = io.rendered();
            assert!(
                rendered.contains(&format!("kimi kimi-k3: {verb} —")),
                "{rendered}"
            );
            assert!(rendered.contains(detail), "{rendered}");
            // BR-4: what the next turn will do, on every outcome.
            assert!(rendered.contains("provider health: healthy."), "{rendered}");
            verdicts.push(verb);
        }

        // Distinct, which is the claim: eight outcomes must not collapse into one
        // word a reader cannot act on differently. The last three are the point
        // of the split — "nothing answered", "something answered wrongly" and
        // "nothing answered in time" are three different next moves, and one
        // verdict word covering them would put the reader back to parsing prose.
        let mut unique = verdicts.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), verdicts.len(), "{verdicts:?}");
    }

    /// AC-2: the flow can name the credential's **reference** and can never name
    /// its value — over the *whole* run, preview included.
    ///
    /// Non-vacuous by construction: the fixture provider's endpoint carries
    /// [`PLANTED_KEY`] as userinfo, this run renders the preview (`--yes` skips
    /// the question, not the preview) and then the report, and the sweep is over
    /// every line both produced. Before the preview went through
    /// `displayed_endpoint` this assertion failed on the preview line — which is
    /// the only reason it is worth writing down.
    #[test]
    fn the_report_names_the_key_reference_and_never_the_key() {
        let mut io = FakeIo::new(&[]);
        io.outcome = Ok(result(ProviderTestOutcome::Refused {
            status: 401,
            reason: format!(
                "HTTP 401 from api.moonshot.ai — the vendor did not accept the credential at \
                 {KEY_REF}"
            ),
        }));
        run(&mut io, &session(), "kimi", true, true).expect("the flow renders");

        let rendered = io.rendered();
        assert!(rendered.contains(KEY_REF), "{rendered}");
        assert!(
            !rendered.contains(PLANTED_KEY),
            "a credential value reached the surface: {rendered}"
        );
        // The remedy names both ways to replace the key it is about.
        assert!(rendered.contains("`/provider setup kimi`"), "{rendered}");
        assert!(
            rendered.contains("teton provider add kimi --model kimi-k3"),
            "{rendered}"
        );
    }

    /// An unpriced model records no cost, and the line says so rather than
    /// showing `$0.000000` (REQ-544 BR-2).
    #[test]
    fn an_unpriced_reach_says_unpriced_and_never_a_zero_dollar_figure() {
        let mut io = FakeIo::new(&[]);
        io.outcome = Ok(result(ProviderTestOutcome::Reached {
            latency_ms: 820,
            input_tokens: 19,
            output_tokens: 2,
            usd_micros: None,
        }));
        run(&mut io, &session(), "kimi", true, true).expect("the flow renders");

        let rendered = io.rendered();
        assert!(rendered.contains("820 ms"), "{rendered}");
        assert!(rendered.contains("unpriced"), "{rendered}");
        assert!(!rendered.contains("$0.000000"), "{rendered}");
    }

    /// BR-4: the report says what the next turn will do with this provider, read
    /// from the snapshot's own resolved routing rather than re-derived.
    #[test]
    fn a_reached_report_names_what_routes_there() {
        let mut io = FakeIo::new(&[]);
        run(&mut io, &session(), "kimi", true, true).expect("the flow renders");

        let rendered = io.rendered();
        assert!(
            rendered.contains("`build` routes here (edit, shell)"),
            "{rendered}"
        );
        // A category with no call site yet is bound to the same provider and
        // stays off the line: it is work this provider will not be asked for.
        assert!(!rendered.contains("design"), "{rendered}");
    }

    /// The unrouted registration BR-7 of REQ-579 permits says so plainly, and
    /// names the command that binds it — a line that trailed off would leave a
    /// user with a working provider and no idea why nothing uses it.
    #[test]
    fn a_reached_report_on_an_unrouted_provider_says_nothing_routes_there() {
        let mut io = FakeIo::new(&[]);
        io.snapshot = Ok(ConfigSnapshot {
            providers: snapshot().providers,
            ..ConfigSnapshot::default()
        });
        run(&mut io, &session(), "kimi", true, true).expect("the flow renders");

        let rendered = io.rendered();
        assert!(
            rendered.contains("nothing routes to `kimi` yet"),
            "{rendered}"
        );
        assert!(rendered.contains("teton policy set-tier"), "{rendered}");
    }

    /// **BR-4 / AC-5: the routing line describes the machine the test left.**
    ///
    /// The snapshot the preview was read from is a reading taken *before* the
    /// call, and a `reached` outcome is the one ending that can invalidate it:
    /// the daemon screens an `Unavailable` provider out of its routing
    /// resolution, so a provider the health map had written off comes back with
    /// no categories dispatching to it — and a `reached` test is what restores
    /// it. Reporting the stale reading would tell a user whose connection just
    /// came back that nothing routes to it.
    ///
    /// Asserted on both halves: the second `config/get` happens, and the line is
    /// composed from *its* answer rather than the first's.
    #[test]
    fn a_reached_report_re_reads_the_config_before_naming_what_routes_there() {
        let mut io = FakeIo::new(&[]);
        // Before: `kimi` was unavailable, so the resolver routed nothing to it.
        io.snapshot = Ok(ConfigSnapshot {
            routing: Vec::new(),
            ..snapshot()
        });
        // After: the test restored it, and the categories are back.
        io.snapshot_after = Some(Ok(snapshot()));
        run(&mut io, &session(), "kimi", true, true).expect("the flow renders");

        assert_eq!(
            io.snapshots_read, 2,
            "a reached test must re-read the config it is about to report on"
        );
        let rendered = io.rendered();
        assert!(
            rendered.contains("`build` routes here (edit, shell)"),
            "the routing line must come from the reading taken *after* the test: {rendered}"
        );
        assert!(
            !rendered.contains("no category dispatches on it yet"),
            "the pre-test reading must not be what the user is left with: {rendered}"
        );
    }

    /// And **only** on `reached`: a failure changed no routing, so a second
    /// round trip would buy an unchanged answer.
    #[test]
    fn a_failed_report_does_not_re_read_the_config() {
        for outcome in [
            ProviderTestOutcome::Unreachable {
                reason: "could not reach api.moonshot.ai: timeout".to_owned(),
            },
            ProviderTestOutcome::Refused {
                status: 401,
                reason: "HTTP 401 from api.moonshot.ai".to_owned(),
            },
            ProviderTestOutcome::RateLimited {
                retry_after_secs: None,
            },
            ProviderTestOutcome::NotACompletion {
                reason: "api.moonshot.ai answered, but not with a completion".to_owned(),
            },
            ProviderTestOutcome::TimedOut {
                after_secs: 30,
                reason: "nothing came back from api.moonshot.ai".to_owned(),
            },
        ] {
            let mut io = FakeIo::new(&[]);
            io.outcome = Ok(result(outcome));
            run(&mut io, &session(), "kimi", true, true).expect("the flow renders");
            assert_eq!(
                io.snapshots_read,
                1,
                "a failed test must not spend a second config/get: {}",
                io.rendered()
            );
        }
    }

    /// A re-read that **fails** costs the report nothing: the pre-test snapshot
    /// is still a true reading of the routing table a moment ago, the call this
    /// report is about already happened, and a dropped routing line would lose
    /// more than the staleness it avoided.
    #[test]
    fn a_failed_re_read_falls_back_to_the_reading_it_had() {
        let mut io = FakeIo::new(&[]);
        io.snapshot_after = Some(Err(RpcError {
            code: error_code::INTERNAL_ERROR,
            message: "the config could not be read".to_owned(),
            data: None,
        }));
        run(&mut io, &session(), "kimi", true, true).expect("the flow renders");

        assert_eq!(io.snapshots_read, 2);
        let rendered = io.rendered();
        assert!(
            rendered.contains("`build` routes here (edit, shell)"),
            "the first reading must still be reported: {rendered}"
        );
        assert!(
            !rendered.contains("could not read your config"),
            "a failed re-read is not news the user can act on: {rendered}"
        );
    }

    /// A failure renders no routing line: what routes there did not change, and
    /// the reader's next move is the remedy, not the routing table.
    #[test]
    fn a_failed_report_says_nothing_about_routing() {
        let mut io = FakeIo::new(&[]);
        io.outcome = Ok(result(ProviderTestOutcome::Unreachable {
            reason: "could not reach api.moonshot.ai: timeout".to_owned(),
        }));
        run(&mut io, &session(), "kimi", true, true).expect("the flow renders");
        assert!(!io.rendered().contains("routes here"), "{}", io.rendered());
    }

    // -------------------------------------------------------------------
    // Version skew and refusals
    // -------------------------------------------------------------------

    /// A daemon that never heard of a method says so as a version fact, not as
    /// an error — and names the method it actually lacks.
    ///
    /// The two are separate lines because they are separate facts: a daemon
    /// without `config/get` predates the config snapshot itself, and telling its
    /// user that "provider connection tests" are unavailable would send them
    /// looking for a feature while the older, larger problem went unnamed.
    #[test]
    fn a_daemon_without_a_method_names_the_method_it_lacks() {
        let absent = || RpcError {
            code: error_code::METHOD_NOT_FOUND,
            message: "no such method".to_owned(),
            data: None,
        };

        let mut io = FakeIo::new(&[]);
        io.snapshot = Err(absent());
        run(&mut io, &session(), "kimi", true, true).expect("the flow renders");
        let rendered = io.rendered();
        assert!(
            rendered.contains("does not serve `config/get`"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("provider connection tests"),
            "a missing config/get must not be reported as a missing provider/test: {rendered}"
        );
        assert!(rendered.contains("Nothing was sent"), "{rendered}");
        assert!(io.tests.is_empty(), "{:?}", io.tests);

        let mut io = FakeIo::new(&[]);
        io.outcome = Err(absent());
        run(&mut io, &session(), "kimi", true, true).expect("the flow renders");
        let rendered = io.rendered();
        assert!(
            rendered.contains("does not serve provider connection tests"),
            "{rendered}"
        );
        assert!(!rendered.contains("`config/get`"), "{rendered}");
    }

    /// A daemon that answers "no" — an unresolvable credential reference, a
    /// session this connection may not drive — is reported with its own
    /// sentence, because it is the end that knows why.
    #[test]
    fn a_refused_call_carries_the_daemons_sentence() {
        let mut io = FakeIo::new(&[]);
        io.outcome = Err(RpcError {
            code: error_code::INVALID_PARAMS,
            message: format!("the credential at {KEY_REF} could not be resolved on this machine"),
            data: None,
        });
        run(&mut io, &session(), "kimi", true, true).expect("the flow renders");

        let rendered = io.rendered();
        assert!(rendered.contains("provider test failed"), "{rendered}");
        assert!(rendered.contains(KEY_REF), "{rendered}");
        assert!(!rendered.contains(PLANTED_KEY), "{rendered}");
    }

    // -------------------------------------------------------------------
    // Pure helpers
    // -------------------------------------------------------------------

    /// Sub-second latencies read in milliseconds and longer ones in seconds; the
    /// arithmetic is integral, so the figure on screen is the figure measured.
    #[test]
    fn latency_reads_in_the_unit_a_person_would_use() {
        assert_eq!(format_latency(0), "0 ms");
        assert_eq!(format_latency(999), "999 ms");
        assert_eq!(format_latency(1_000), "1.0 s");
        assert_eq!(format_latency(1_449), "1.4 s");
        assert_eq!(format_latency(60_000), "60.0 s");
    }

    /// **A provider with no stored endpoint is answered without a call, and
    /// without the claim that something will fill the address in** (verify G3).
    ///
    /// The old preview line said "no endpoint stored; this kind composes its
    /// own", and nothing does. `build_provider` and `build_remote_transport`
    /// both take `unwrap_or_default()`, so an empty string reaches the adapter
    /// and the daemon refuses with "endpoint has no host the transport could
    /// dial" — which means the sentence was false on the one line whose job is
    /// to be true before a user consents to spend, and the round trip it invited
    /// bought nothing.
    ///
    /// Asserted on the call log, not on the wording: "no call" is the half a
    /// re-worded preview would have left broken. The blank and whitespace-only
    /// forms go the same way — a config carrying `endpoint = " "` has no address
    /// either, and `unwrap_or_default()` cannot tell the two apart.
    #[test]
    fn a_provider_with_no_stored_endpoint_is_answered_without_a_call() {
        for stored in [None, Some(""), Some("   ")] {
            for typed_input in [true, false] {
                let mut io = FakeIo::new(&["y"]);
                io.snapshot = Ok(ConfigSnapshot {
                    providers: vec![provider("anth", ProviderKind::Anthropic, stored)],
                    ..snapshot()
                });
                run(&mut io, &session(), "anth", false, typed_input).expect("the flow renders");

                assert!(
                    io.tests.is_empty(),
                    "a provider with no address must reach provider/test not at \
                     all (stored={stored:?}, typed_input={typed_input}): {:?}",
                    io.tests
                );
                assert_eq!(io.prompter.asked, 0, "there is nothing to consent to");
                let rendered = io.rendered();
                assert!(
                    rendered
                        .contains("`anth` has no endpoint stored, so there is nothing to dial."),
                    "{rendered}"
                );
                assert!(
                    rendered.contains("teton provider add anth --endpoint <url> --model <model>"),
                    "the state is one the user can leave, so the line says how: {rendered}"
                );
                assert!(
                    !rendered.contains("composes its own"),
                    "nothing composes an endpoint — the daemon refuses this row: {rendered}"
                );
                assert!(
                    !rendered.contains("--yes"),
                    "a call that will not be made must not ask for consent to make it: {rendered}"
                );
            }
        }
    }

    /// **The flag that suppresses this session's own `provider_tested` notice is
    /// raised only across the call, and is lowered however the call ends**
    /// (verify G2).
    ///
    /// The notice exists for the *other* clients attached to the session, and the
    /// only client that must not see it is the one holding the report — which is
    /// to say, the one whose `provider/test` is out. So the flag is a window,
    /// and both of its edges are the assertion: raised while the pump can render
    /// the event (the first row), and down again afterwards on **every** ending
    /// (the rest). A flag left raised by a failed call would silence the next
    /// notice this session gets, one some other client's test produced and this
    /// one has no report for — a silence nobody could account for later.
    ///
    /// Asserted on the helper rather than on [`DaemonIo`] because the seam under
    /// test is the window, not the socket: `Connection::call` is what stands
    /// between them, and it needs a daemon.
    #[test]
    fn the_in_flight_flag_is_raised_only_across_the_call_and_always_lowered() {
        for ending in ["ok", "rpc-error", "transport-error"] {
            let mut surface = RecordingSurface::new();
            let mut state = crate::session_ui::SessionState::new();
            let mut prompter = ScriptedPrompter::new(&[]);
            let mut ctx = UiContext {
                surface: &mut surface,
                state: &mut state,
                prompter: &mut prompter,
                answer_permissions: true,
                answer_model_proposals: true,
                auto_accept_model: false,
                typed_input: true,
                session_id: Some(session()),
            };
            assert!(
                !ctx.state.provider_test_in_flight,
                "a fresh session has no probe out"
            );

            let mut seen_inside = false;
            let answered: anyhow::Result<Result<ProviderTestResult, RpcError>> =
                while_this_clients_probe_is_out(&mut ctx, |ctx| {
                    // Where the pump runs, and therefore where the daemon's
                    // `provider_tested` for *this* call is rendered.
                    seen_inside = ctx.state.provider_test_in_flight;
                    match ending {
                        "ok" => Ok(Ok(result(reached()))),
                        "rpc-error" => Ok(Err(RpcError {
                            code: error_code::INVALID_PARAMS,
                            message: "no".to_owned(),
                            data: None,
                        })),
                        _ => Err(anyhow::anyhow!("the socket went away")),
                    }
                });

            assert!(seen_inside, "the flag must be up while the call is out");
            assert!(
                !ctx.state.provider_test_in_flight,
                "and down again after a `{ending}` ending — a flag left raised \
                 silences a notice about somebody else's test"
            );
            assert_eq!(answered.is_ok(), ending != "transport-error");
        }
    }

    /// Nothing this module composes carries an escape sequence of its own — the
    /// surface defuses what it is handed (REQ-573), and a renderer that
    /// contributed its own would be defeating that at the source.
    #[test]
    fn nothing_this_module_composes_carries_an_escape_sequence() {
        let mut composed = vec![
            TEST_NEEDS_A_SESSION.to_owned(),
            TEST_UNAVAILABLE.to_owned(),
            CONFIG_UNAVAILABLE.to_owned(),
            NOT_A_TERMINAL.to_owned(),
            TEST_DECLINED.to_owned(),
            CONFIRM_QUESTION.to_owned(),
            unknown_id_line("x", &snapshot()),
            routing_line(&snapshot(), "kimi"),
            local_tier_line("onlocal"),
            no_endpoint_line("anth"),
        ];
        composed.extend(preview_lines(&provider(
            "kimi",
            ProviderKind::OpenaiCompatible,
            Some(ENDPOINT_WITH_KEY),
        )));
        composed.extend(report_lines(&result(reached()), &snapshot()));
        for line in composed {
            assert!(!line.contains('\x1b'), "{line:?}");
            assert!(!line.contains('\r'), "{line:?}");
        }
    }
}
