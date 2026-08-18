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
//! `reason` is rendered verbatim and composed nowhere but the daemon, which is
//! also why no line here can leak a credential: the only vendor-adjacent text
//! that reaches the surface is a sentence the daemon built from the status, the
//! dial host, the configured model and the credential *reference* (ADR-3).
//!
//! ## The preview echoes, it does not parse (LESSON-529)
//!
//! The preview line carries the endpoint **as the config snapshot stores it**,
//! byte for byte, and the confirm question says "that endpoint" rather than
//! naming a host of its own. A display helper that extracted an authority here
//! would be a second parser of the string the daemon is about to POST to, and
//! every divergence between the two would be a lie on the one line a user reads
//! before consenting to spend. The *report*'s host is the daemon's own
//! dial-time reading ([`ProviderTestResult::dial_host`]) for the same reason,
//! travelling as data rather than being re-derived.
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
use crate::prompt::Prompter;
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
/// preview line directly above, verbatim from the snapshot, and a second reading
/// of it here would be the display-vs-dial divergence LESSON-529 is about.
const CONFIRM_QUESTION: &str =
    "  this sends one minimal request (≈ 20 tokens) to that endpoint. proceed?  [y/N] ";

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
        self.conn.call(params, self.ctx)
    }
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
/// The **order** is load-bearing. Every path that would reach
/// [`TestIo::provider_test`] passes the gate first, so "a decline sends nothing"
/// is a property of the control flow rather than of four separate branches
/// remembering to check (AC-4).
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
            io.surface().line(LineKind::Notice, TEST_UNAVAILABLE);
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

    // The gate, ahead of everything that could reach the wire. A session that
    // cannot be asked and did not consent in advance is answered here and
    // nowhere else, so no later branch can send on its behalf.
    if !typed_input && !auto_yes {
        io.surface().line(LineKind::Error, NOT_A_TERMINAL);
        return Ok(());
    }

    if provider.kind == ProviderKind::Local {
        // BR-8: there is no host to dial and therefore nothing to preview or
        // consent to. The *refusal* is the daemon's — it carries the local
        // tier's current state (REQ-580's classification), which is the actual
        // answer to "does it work", and inventing a sentence for it here would
        // be this module forming an opinion about a tier it cannot see.
        //
        // The branch is a client-side reading of the snapshot, so it decides
        // only which sentence comes first: the daemon refuses a `kind = local`
        // provider whatever this thinks, and a config edited between the two
        // calls is bounded by that refusal rather than by this check.
        return report_refusal(io, &provider, session_id);
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

    for line in report_lines(&result, &snapshot) {
        io.surface().line(LineKind::Notice, &line);
    }
    Ok(())
}

/// Ask the daemon about a `kind = "local"` provider and render whatever it says
/// (BR-8 / AC-7).
///
/// Split out so the "no preview, no question, still one RPC" shape is one
/// readable thing rather than an early-return tangle in the middle of [`run`].
fn report_refusal(
    io: &mut dyn TestIo,
    provider: &ProviderConfig,
    session_id: &SessionId,
) -> anyhow::Result<()> {
    match io.provider_test(ProviderTestParams {
        session_id: session_id.clone(),
        provider_id: provider.id.clone(),
    })? {
        // Structurally unreachable — the daemon refuses a local provider — and
        // rendered rather than asserted, because a client that panicked on a
        // daemon's legitimate answer would be the worse of the two bugs.
        Ok(result) => {
            for line in report_lines(&result, &ConfigSnapshot::default()) {
                io.surface().line(LineKind::Notice, &line);
            }
        }
        Err(err) if err.code == error_code::METHOD_NOT_FOUND => {
            io.surface().line(LineKind::Notice, TEST_UNAVAILABLE);
        }
        Err(err) => {
            io.surface().line(LineKind::Notice, &err.message);
        }
    }
    Ok(())
}

/// An explicit yes, and nothing else (LESSON-470). Empty and EOF are both no.
fn is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}

// ---------------------------------------------------------------------------
// Content (pure)
// ---------------------------------------------------------------------------

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

/// The one line the user reads before consenting (BR-2).
///
/// Id, kind, model and the **stored** endpoint, echoed rather than parsed
/// (LESSON-529). A provider whose config carries no endpoint — an `anthropic`
/// row taking the vendor's default — says so instead of showing a URL this
/// module composed, because a composed address is a claim about what will be
/// dialed that only the daemon can make.
fn preview_lines(provider: &ProviderConfig) -> Vec<String> {
    let model = provider.model.as_deref().unwrap_or("no model configured");
    let endpoint = match provider.endpoint.as_deref() {
        Some(endpoint) if !endpoint.trim().is_empty() => endpoint.to_owned(),
        _ => "no endpoint stored; this kind composes its own".to_owned(),
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
fn outcome_verb(outcome: &ProviderTestOutcome) -> &'static str {
    match outcome {
        ProviderTestOutcome::Reached { .. } => "reachable",
        ProviderTestOutcome::Refused { .. } => "refused",
        ProviderTestOutcome::UnknownModel { .. } => "model unknown",
        ProviderTestOutcome::RateLimited { .. } => "rate limited",
        ProviderTestOutcome::ServerError { .. } => "server error",
        ProviderTestOutcome::Unreachable { .. } => "unreachable",
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
        | ProviderTestOutcome::Unreachable { reason } => {
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
        | ProviderTestOutcome::Unreachable { .. } => {}
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
    const PLANTED_KEY: &str = "sk-planted-provider-test-key";

    /// The credential *reference*, which the daemon's own `refused` sentence
    /// does name — and which AC-2 asserts is what prints in its place.
    const KEY_REF: &str = "keychain://teton/kimi";

    /// The seam, wired to canned answers and a recording surface.
    struct FakeIo {
        surface: RecordingSurface,
        prompter: ScriptedPrompter,
        snapshot: Result<ConfigSnapshot, RpcError>,
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
                    Some("https://api.moonshot.ai/v1/chat/completions"),
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
    /// preview makes is "this exact string is what Teton POSTs to" (REQ-578),
    /// and a test that checked only a substring would pass for a helper that had
    /// started parsing the URL (LESSON-529).
    #[test]
    fn the_preview_names_the_provider_and_the_stored_endpoint_verbatim() {
        let mut io = FakeIo::new(&["y"]);
        run(&mut io, &session(), "kimi", false, true).expect("the flow renders");

        let rendered = io.rendered();
        assert!(
            rendered.contains("provider:  kimi (openai-compatible, kimi-k3)"),
            "{rendered}"
        );
        assert!(
            rendered.contains("— https://api.moonshot.ai/v1/chat/completions"),
            "the stored endpoint must be echoed whole: {rendered}"
        );
        assert!(
            io.prompter
                .any_question_contains("one minimal request (≈ 20 tokens) to that endpoint"),
            "{:?}",
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

    /// BR-8 / AC-7: a local provider is refused with the **daemon's** sentence,
    /// and the flow neither previews an endpoint it does not have nor asks a
    /// question about a request that will not be made.
    #[test]
    fn a_local_provider_renders_the_daemons_own_refusal() {
        let mut io = FakeIo::new(&["y"]);
        io.outcome = Err(RpcError {
            code: error_code::INVALID_PARAMS,
            message: "`onlocal` is the local tier and has nothing to dial — it is still \
                      downloading qwen2.5-coder-7b; `teton doctor` reports its state."
                .to_owned(),
            data: None,
        });
        run(&mut io, &session(), "onlocal", false, true).expect("the flow renders");

        assert_eq!(io.prompter.asked, 0, "there is nothing to consent to");
        let rendered = io.rendered();
        assert!(rendered.contains("nothing to dial"), "{rendered}");
        assert!(rendered.contains("teton doctor"), "{rendered}");
        assert!(
            !rendered.contains("one minimal request"),
            "a provider with no host must not be previewed as one that has: {rendered}"
        );
    }

    // -------------------------------------------------------------------
    // BR-3 — the typed report
    // -------------------------------------------------------------------

    /// BR-3: every variant renders a distinct verdict, and the daemon's `reason`
    /// is carried verbatim. Asserted by variant, never by parsing prose.
    #[test]
    fn every_outcome_variant_renders_its_own_line() {
        let cases: [(ProviderTestOutcome, &str, &str); 6] = [
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
                    reason: "could not reach api.moonshot.ai: timeout".to_owned(),
                },
                "unreachable",
                "could not reach api.moonshot.ai: timeout",
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

        // Distinct, which is the claim: six outcomes must not collapse into one
        // word a reader cannot act on differently.
        let mut unique = verdicts.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), verdicts.len(), "{verdicts:?}");
    }

    /// AC-2: the report can name the credential's **reference** and can never
    /// name its value — because the only vendor-adjacent text on screen is the
    /// daemon's own `reason`, rendered.
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

    /// A daemon that never heard of the method says so as a version fact, not as
    /// an error, and neither call is retried into a second line.
    #[test]
    fn a_daemon_without_the_method_reports_a_version_fact() {
        for missing in ["config", "test"] {
            let mut io = FakeIo::new(&[]);
            let absent = RpcError {
                code: error_code::METHOD_NOT_FOUND,
                message: "no such method".to_owned(),
                data: None,
            };
            if missing == "config" {
                io.snapshot = Err(absent);
            } else {
                io.outcome = Err(absent);
            }
            run(&mut io, &session(), "kimi", true, true).expect("the flow renders");
            assert!(
                io.rendered()
                    .contains("does not serve provider connection tests"),
                "{missing}: {}",
                io.rendered()
            );
        }
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

    /// A provider whose config stores no endpoint says so rather than showing an
    /// address this module composed: the destination is the daemon's claim to
    /// make, and a composed one is exactly the display-vs-dial divergence
    /// LESSON-529 is about.
    #[test]
    fn a_provider_with_no_stored_endpoint_previews_no_url() {
        let lines = preview_lines(&provider("anth", ProviderKind::Anthropic, None));
        let rendered = lines.join("\n");
        assert!(rendered.contains("no endpoint stored"), "{rendered}");
        assert!(!rendered.contains("https://"), "{rendered}");
    }

    /// Nothing this module composes carries an escape sequence of its own — the
    /// surface defuses what it is handed (REQ-573), and a renderer that
    /// contributed its own would be defeating that at the source.
    #[test]
    fn nothing_this_module_composes_carries_an_escape_sequence() {
        let mut composed = vec![
            TEST_NEEDS_A_SESSION.to_owned(),
            TEST_UNAVAILABLE.to_owned(),
            NOT_A_TERMINAL.to_owned(),
            TEST_DECLINED.to_owned(),
            CONFIRM_QUESTION.to_owned(),
            unknown_id_line("x", &snapshot()),
            routing_line(&snapshot(), "kimi"),
        ];
        composed.extend(preview_lines(&provider(
            "kimi",
            ProviderKind::OpenaiCompatible,
            Some("https://api.moonshot.ai/v1/chat/completions"),
        )));
        composed.extend(report_lines(&result(reached()), &snapshot()));
        for line in composed {
            assert!(!line.contains('\x1b'), "{line:?}");
            assert!(!line.contains('\r'), "{line:?}");
        }
    }
}
