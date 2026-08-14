//! The `/web setup` walkthrough — collect at the edge, commit at the core
//! (REQ-572 ADR-1/ADR-3).
//!
//! The daemon holds no step state. This module asks the questions, the daemon
//! answers three stateless RPCs — `web/setup_plan`, `web/setup_preview`,
//! `web/setup_commit` — and until the commit lands nothing durable exists
//! anywhere (BR-11). An abort is therefore not an operation: it is this module
//! deciding to stop asking.
//!
//! ## What is *not* decided here
//!
//! The `[web]` TOML shown at the confirm step and the host it would reach are
//! the **daemon's own bytes**, rendered verbatim. Nothing in this file composes
//! a config table or forms an opinion about what a valid candidate is: a
//! client-side re-derivation would be a second answer to a question the daemon
//! has already answered, and the two would agree right up until the one that
//! mattered (LESSON-494, BR-7). What the user confirms is what the commit
//! re-derives from the same answers.
//!
//! The one thing this file *does* read out of a typed endpoint is its host, and
//! only to decide which auth-header template the next **prompt offers as its
//! default** ([`offered_auth`]). That is a question about what to put in front of
//! a person, not about what is valid — the daemon still validates, and a user who
//! types a template over the offer is obeyed. Offering the generic template to
//! somebody who just typed the endpoint of a backend that wants its own header is
//! how a walkthrough hands out a config that 401s.
//!
//! ## The suggestions are the daemon's list, rendered (REQ-573 BR-1)
//!
//! Which backends the walkthrough names, what their endpoints look like and which
//! header each one wants all arrive as data on the plan, in
//! [`WebSetupCatalog`] — this file holds no copy. A second list here would agree
//! with the daemon's right up until one of them was edited, and the person who
//! met the drift would read the 401 as a bad key. A daemon that sends no catalog
//! is answered by naming no backend at all rather than by falling back to a list
//! of this file's own (BR-3): the offer degrades to
//! [`GENERIC_SEARCH_AUTH_TEMPLATE`], the one spelling both binaries share.
//!
//! ## The secret's whole life is in this process (ADR-3)
//!
//! The key is read echo-off into memory, written to the OS keychain by
//! [`Keychain::store`] **after** the user has confirmed the preview, and only
//! its reference — `keychain://teton/web-search` — travels to the daemon.
//!
//! The account is fixed, so that store is **destructive**: on a rotation it
//! overwrites a credential the live config still references. The flow therefore
//! reads the account before it writes ([`Keychain::read`]) and, when the commit
//! the write was made for is refused, puts back exactly what it displaced rather
//! than deleting — BR-11 removes "any keychain entry the aborted flow run itself
//! created", and a rotation created none. A *transport* failure is the third
//! case and is left alone entirely: the commit may have landed, so both undos
//! could be the destructive one, and the honest answer is a notice rather than a
//! guess ([`ambiguous_commit_line`]).
//!
//! ## Everything but the bytes is testable without a terminal
//!
//! The flow reaches the world through one seam, [`SetupIo`]: the two RPCs, the
//! [`Surface`] and the [`Prompter`]. Production wires it to a real connection
//! and the session's own context; the tests wire it to a recording surface, a
//! scripted prompter and canned answers, which is what lets the store-then-commit
//! ordering, the delete-on-failure and every abort point be pinned with no
//! socket, no keychain and no tty (REQ-556/REQ-560 BR-8's pattern).

use teton_protocol::events::{WebCapabilityState, WebTier};
use teton_protocol::jsonrpc::{error_code, RpcError};
use teton_protocol::methods::{
    WebBackendSuggestion, WebSetupCatalog, WebSetupCommitParams, WebSetupCommitResult,
    WebSetupPlanParams, WebSetupPlanResult, WebSetupPreviewParams, WebSetupPreviewResult,
    WebTableSummary,
};
use teton_protocol::{SessionId, GENERIC_SEARCH_AUTH_TEMPLATE};

use crate::client::{Connection, UiContext};
use crate::keychain::{auth_ref_for, Cleanup, Keychain, PriorKey};
use crate::prompt::Prompter;
use crate::render::{LineKind, Surface};
use crate::session_ui::web_tier_name;

/// The keychain account the search credential is filed under.
///
/// One spelling, used by the store, by the delete, and by the reference the
/// daemon is handed — a second copy is how a flow comes to delete an entry it
/// did not write.
pub(crate) const SEARCH_KEY_ACCOUNT: &str = "web-search";

/// What `/web setup` says when there is no session to set anything up for.
///
/// Reachable only from a context that owns no session, the same guard
/// `/web allow`'s is — it keeps the id from being fabricated rather than being a
/// line users meet.
const SETUP_NEEDS_A_SESSION: &str =
    "`/web setup` needs a session to act on, and this command owns none.";

/// What `/web setup` says to a daemon built before REQ-572.
///
/// A version fact, not a failure, so it wears no `error:` prefix (BUG-152) — and
/// it names the way to enable the capability on such a daemon rather than only
/// reporting that the walkthrough is missing.
const SETUP_UNAVAILABLE: &str =
    "this daemon build does not serve the guided web setup — restart it after upgrading, or \
     write the `[web]` table into your config by hand.";

/// What every abort renders: the fact the user needs, which is that the machine
/// is exactly as they left it.
const SETUP_ABORTED: &str =
    "web setup cancelled — nothing was written to your config and no key was stored.";

/// The decline at the confirm step. Distinct from [`SETUP_ABORTED`] because the
/// user got as far as reading the bytes and said no to *them*.
const SETUP_DECLINED: &str =
    "not written. Nothing was changed and no key was stored; run `/web setup` again to start over.";

/// What the flow says when the daemon offered no preview digest (BUG-166
/// residual (a)).
///
/// A daemon built before BR-7's digest field degrades the commit to the
/// protocol's own "do not check" — correct, and previously silent, which made
/// the loss of the confirmed-bytes guard a fact only the source code knew.
/// A version fact, not a failure, so it wears no `error:` prefix (BUG-152) —
/// the commit still lands; what changed is which guard rides it.
const DIGEST_CHECK_UNAVAILABLE: &str =
    "this daemon build offers no preview digest, so the commit is not pinned to the previewed \
     bytes — upgrade and restart the daemon to restore that check.";

// ---------------------------------------------------------------------------
// The world seam
// ---------------------------------------------------------------------------

/// Everything the flow can reach outside itself: two RPCs, a place to render,
/// and a place to ask.
///
/// The accessors hand out short-lived borrows rather than the seams themselves
/// because the production implementation holds the session's `UiContext` — the
/// same context [`Connection::call`] needs in order to pump events while a
/// request is in flight — so a design that handed the surface out for the
/// duration would have to take the context apart.
pub(crate) trait SetupIo {
    /// Where the flow's lines go.
    fn surface(&mut self) -> &mut dyn Surface;
    /// Where the flow's questions go.
    fn prompter(&mut self) -> &mut dyn Prompter;
    /// `web/setup_plan`.
    ///
    /// # Errors
    /// Propagates a transport failure; a daemon that *answers* with an error
    /// returns it in the inner `Result`.
    fn plan(
        &mut self,
        params: WebSetupPlanParams,
    ) -> anyhow::Result<Result<WebSetupPlanResult, RpcError>>;
    /// `web/setup_preview`.
    ///
    /// # Errors
    /// As [`Self::plan`].
    fn preview(
        &mut self,
        params: WebSetupPreviewParams,
    ) -> anyhow::Result<Result<WebSetupPreviewResult, RpcError>>;
    /// `web/setup_commit`.
    ///
    /// # Errors
    /// As [`Self::plan`].
    fn commit(
        &mut self,
        params: WebSetupCommitParams,
    ) -> anyhow::Result<Result<WebSetupCommitResult, RpcError>>;
}

/// The production seam: the session's own connection and context (D-4).
struct DaemonIo<'a, 'ctx> {
    conn: &'a mut Connection,
    ctx: &'a mut UiContext<'ctx>,
}

impl SetupIo for DaemonIo<'_, '_> {
    fn surface(&mut self) -> &mut dyn Surface {
        &mut *self.ctx.surface
    }

    fn prompter(&mut self) -> &mut dyn Prompter {
        &mut *self.ctx.prompter
    }

    fn plan(
        &mut self,
        params: WebSetupPlanParams,
    ) -> anyhow::Result<Result<WebSetupPlanResult, RpcError>> {
        self.conn.call(params, self.ctx)
    }

    fn preview(
        &mut self,
        params: WebSetupPreviewParams,
    ) -> anyhow::Result<Result<WebSetupPreviewResult, RpcError>> {
        self.conn.call(params, self.ctx)
    }

    fn commit(
        &mut self,
        params: WebSetupCommitParams,
    ) -> anyhow::Result<Result<WebSetupCommitResult, RpcError>> {
        self.conn.call(params, self.ctx)
    }
}

// ---------------------------------------------------------------------------
// The typed-input gate
// ---------------------------------------------------------------------------

/// What [`gate`] decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gate {
    /// Ask the questions.
    Walk,
    /// Print what enabling the capability involves and ask nothing.
    Instructions,
}

/// Whether `/web setup` may put questions to this session's stdin.
///
/// The same shape as `slash::model_set_gate`, and pure for its reason — the
/// branch that matters is the one a test process cannot otherwise reach — but a
/// **different answer** to a refusal: this command degrades rather than refuses
/// (BR-12/AC-10). A piped session is told what the capability needs and what to
/// type where, and no prompt is drawn, so the flow consumes no line that was
/// meant for the session (LESSON-470's is-terminal rule: an interactive offer
/// must first check that somebody is there to take it).
///
/// `typed_input` is the session's own flag, read once at the edge and carried on
/// the context like every other world-fact a handler needs. `seams_allowed` is
/// the debug-build test switch, which makes this **looser** — the polarity
/// `slash::test_seams_allowed` documents as the safe one, since a release build
/// that ignores the switch can only decline to walk.
#[must_use]
pub(crate) fn gate(typed_input: bool, seams_allowed: bool) -> Gate {
    if typed_input || seams_allowed {
        Gate::Walk
    } else {
        Gate::Instructions
    }
}

// ---------------------------------------------------------------------------
// The answers
// ---------------------------------------------------------------------------

/// What the user typed, held in this process and nowhere else.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Answers {
    /// The ceiling to write.
    tier: WebTier,
    /// The search backend, as typed. `None` below the `search` tier.
    search_endpoint: Option<String>,
    /// The auth-header template, or `None` for the daemon's default.
    search_auth: Option<String>,
    /// The credential itself — memory only, never rendered, never serialized.
    search_key: Option<String>,
}

/// Redacts the key.
///
/// Hand-written rather than derived because a derived `Debug` puts the
/// credential into any `dbg!`, any `unwrap` panic, and any test failure message
/// that formats these answers — which is a plaintext key in a CI log for
/// nobody's benefit.
impl std::fmt::Debug for Answers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Answers")
            .field("tier", &self.tier)
            .field("search_endpoint", &self.search_endpoint)
            .field("search_auth", &self.search_auth)
            .field(
                "search_key",
                &self.search_key.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl Answers {
    /// The reference the daemon will be given for the collected key.
    ///
    /// Known **before** the key is stored, because it is a name and not a value:
    /// service and account are fixed, so the preview can show the exact
    /// `search_key_ref` the commit will write without a credential having been
    /// written anywhere yet. That is what keeps the confirmed bytes and the
    /// committed bytes the same bytes (BR-7).
    fn key_ref(&self) -> Option<String> {
        self.search_key
            .as_ref()
            .map(|_| auth_ref_for(SEARCH_KEY_ACCOUNT))
    }

    /// The preview request these answers describe.
    fn preview_params(&self, session_id: &SessionId) -> WebSetupPreviewParams {
        WebSetupPreviewParams {
            session_id: session_id.clone(),
            tier: self.tier,
            search_endpoint: self.search_endpoint.clone(),
            search_key_ref: self.key_ref(),
            search_auth: self.search_auth.clone(),
        }
    }

    /// The commit request these answers describe, carrying the reference the
    /// keychain actually returned rather than the one this struct predicted,
    /// and the previewed document's digest so the daemon refuses to write
    /// bytes the user never confirmed (BR-7; the daemon-side check is
    /// `render_persisted_document`, whose digest covers the whole edited
    /// document — REQ-574 BR-3, so even a comment-only hand edit refuses).
    fn commit_params(
        &self,
        session_id: &SessionId,
        key_ref: Option<String>,
        expect_digest: Option<String>,
    ) -> WebSetupCommitParams {
        WebSetupCommitParams {
            session_id: session_id.clone(),
            tier: self.tier,
            search_endpoint: self.search_endpoint.clone(),
            search_key_ref: key_ref,
            search_auth: self.search_auth.clone(),
            expect_digest,
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run `/web setup` on the session's own connection and context.
///
/// # Errors
///
/// Propagates a transport error. A daemon that *answers* — with an error, or
/// with "no such method" — is reported on the surface and returns `Ok`: a
/// refused setup ends the command, never the session.
pub(crate) fn run(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    keychain: &dyn Keychain,
) -> anyhow::Result<()> {
    let Some(session_id) = ctx.session_id.clone() else {
        ctx.surface.line(LineKind::Error, SETUP_NEEDS_A_SESSION);
        return Ok(());
    };
    let gate = gate(ctx.typed_input, crate::slash::test_seams_allowed());
    let mut io = DaemonIo { conn, ctx };
    drive(&mut io, keychain, &session_id, gate)
}

/// The flow itself, over the seam.
///
/// # Errors
///
/// Propagates a transport error from any of the three calls.
pub(crate) fn drive(
    io: &mut dyn SetupIo,
    keychain: &dyn Keychain,
    session_id: &SessionId,
    gate: Gate,
) -> anyhow::Result<()> {
    let plan = match io.plan(WebSetupPlanParams {
        session_id: session_id.clone(),
    })? {
        Ok(plan) => plan,
        Err(err) if err.code == error_code::METHOD_NOT_FOUND => {
            io.surface().line(LineKind::Notice, SETUP_UNAVAILABLE);
            return Ok(());
        }
        Err(err) => {
            io.surface().line(
                LineKind::Error,
                &format!("web setup could not start: {}", err.message),
            );
            return Ok(());
        }
    };

    for (kind, text) in plan_lines(&plan) {
        io.surface().line(kind, &text);
    }

    if gate == Gate::Instructions {
        for (kind, text) in instruction_lines(plan.suggestion_catalog.as_ref()) {
            io.surface().line(kind, &text);
        }
        return Ok(());
    }

    let Some(answers) = collect(&plan, io) else {
        io.surface().line(LineKind::Notice, SETUP_ABORTED);
        return Ok(());
    };

    let preview = match io.preview(answers.preview_params(session_id))? {
        Ok(preview) => preview,
        Err(err) => {
            // Including `WEB_SETUP_INVALID`, whose message is the validator's
            // own sentence — carried verbatim, because the daemon is the one
            // that knows why a candidate would not load.
            io.surface()
                .line(LineKind::Error, &refused_line(&err, "previewed"));
            return Ok(());
        }
    };
    for (kind, text) in preview_lines(&preview) {
        io.surface().line(kind, &text);
    }

    // The previewed document's digest rides the commit so the daemon refuses
    // to write bytes the user never confirmed (BR-7). An empty digest is a
    // daemon that predates the field — degrade to the protocol's own
    // "do not check" rather than sending a value that can only mismatch, and
    // say so **before the confirm question**: a guard that turns itself off on
    // version skew must do it where the user can still act on the fact, and
    // declining is the one act this flow offers (BUG-166 residual (a)).
    let expect_digest = Some(preview.digest.clone()).filter(|digest| !digest.is_empty());
    if expect_digest.is_none() {
        io.surface()
            .line(LineKind::Notice, DIGEST_CHECK_UNAVAILABLE);
    }

    // LESSON-470: the write is the costly wrong answer, so silence declines.
    let confirmed = matches!(io.prompter().ask(CONFIRM_QUESTION), Some(answer) if is_yes(&answer));
    if !confirmed {
        io.surface().line(LineKind::Notice, SETUP_DECLINED);
        return Ok(());
    }

    // ADR-3's residual-minimizing order: the store happens here — after the
    // human said yes, immediately before the commit it was collected for — so
    // the window in which an orphan can exist is one RPC wide rather than the
    // length of the flow.
    //
    // `prior` is read in the same breath and for the same reason the store is
    // late: the account is fixed, so this write displaces whatever a previous
    // run put there, and after it there is no way left to find out what that was.
    let (key_ref, prior) = match answers.search_key.as_deref() {
        Some(secret) => {
            let prior = PriorKey::read(keychain, SEARCH_KEY_ACCOUNT);
            match keychain.store(SEARCH_KEY_ACCOUNT, secret) {
                Ok(reference) => (Some(reference), Some(prior)),
                Err(err) => {
                    io.surface().line(
                        LineKind::Error,
                        &format!(
                            "the key could not be stored in the OS keychain ({err}); nothing was \
                             written to your config."
                        ),
                    );
                    return Ok(());
                }
            }
        }
        None => (None, None),
    };

    // Bound rather than `?`-ed. A transport failure here is not the same event
    // as a daemon that answered "no": the commit may have landed, and letting
    // the error out would end the session on the one path where the user most
    // needs the flow to tell them what state their machine is in.
    match io.commit(answers.commit_params(session_id, key_ref.clone(), expect_digest)) {
        // Nothing is rendered here on purpose. The daemon publishes
        // `web_setup_completed` for this session, and `Connection::call` has
        // already pumped it through `session_ui` by the time this returns — the
        // daemon fences a request's events ahead of its response. A second line
        // composed here would say the same thing twice to the one person who
        // already knew, which is the drift `/clear` avoids the same way.
        Ok(Ok(result)) if result.applied => {}
        Ok(Ok(result)) => io
            .surface()
            .line(LineKind::Notice, &unchanged_line(result.tier, &prior)),
        Ok(Err(err)) => {
            // The entry that was written a moment ago exists only for this
            // commit, and this commit did not happen (ADR-3). A flow that stored
            // nothing has nothing to take back; a flow that *displaced* one owes
            // the machine what it displaced, not a delete.
            let cleanup = prior.as_ref().map(|prior| prior.undo(keychain));
            io.surface()
                .line(LineKind::Error, &refused_line(&err, "written"));
            if let Some(cleanup) = cleanup {
                io.surface().line(LineKind::Notice, &cleanup_line(&cleanup));
            }
        }
        Err(transport) => {
            // Deliberately **no** keychain mutation. Either undo is destructive
            // in one of the two states this error is consistent with, and there
            // is nothing here that can tell them apart — so the user gets the
            // ambiguity itself, with the command that resolves it.
            io.surface().line(
                LineKind::Error,
                &format!("the daemon did not answer the commit: {transport}"),
            );
            io.surface()
                .line(LineKind::Notice, &ambiguous_commit_line(&prior));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The undo (ADR-3, BR-11)
//
// The three-state read-before-write machinery this flow was built on —
// `PriorKey` and `Cleanup` — moved to `crate::keychain` when `teton provider
// add` became its second caller (BUG-171). What stays here is what is this
// flow's own: the account it stores into, and the sentences it renders about
// each outcome.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Collection
// ---------------------------------------------------------------------------

/// The tier question. `Enter` cancels, which is stated rather than implied.
const TIER_QUESTION: &str = "  tier [1-3, or Enter to cancel]: ";

/// The endpoint question.
const ENDPOINT_QUESTION: &str = "  search endpoint [Enter to cancel]: ";

/// Whether the backend needs a credential at all — the question that decides
/// whether the echo-off prompt is asked (a self-hosted SearxNG needs no key).
const KEY_NEEDED_QUESTION: &str = "  does this backend need an API key? [Y/n] ";

/// The advanced auth-header question, naming the template Enter would take.
///
/// The offer is part of the question because it is what an empty answer means,
/// and it is not always the same offer — see [`offered_auth`].
fn auth_question(offered: &str) -> String {
    format!("  auth header template [Enter for `{offered}`]: ")
}

/// The credential question. It says where the key goes, because that is the part
/// the user is agreeing to.
const KEY_QUESTION: &str = "  API key (not shown; stored in your OS keychain as `web-search`): ";

/// The one confirmation, default-**no** (LESSON-470).
const CONFIRM_QUESTION: &str = "  write this to your config? [y/N] ";

/// Ask every question the tier needs, or `None` for any abort.
///
/// `None` is EOF, an empty answer at a question that needs one, Ctrl-C (which
/// ends the process and therefore this), or a `search` selection the plan says
/// cannot serve. Every one of them leaves this function having sent nothing and
/// stored nothing — collection is buffering, and a buffer nobody submits is not
/// state (ADR-1).
fn collect(plan: &WebSetupPlanResult, io: &mut dyn SetupIo) -> Option<Answers> {
    for line in tier_menu_lines(plan) {
        io.surface().line(LineKind::Info, &line);
    }
    let typed = io.prompter().ask(TIER_QUESTION)?;
    if typed.trim().is_empty() {
        return None;
    }
    let tier = match parse_tier(typed.trim()) {
        TierChoice::Tier(tier) => tier,
        TierChoice::Off => {
            io.surface().line(LineKind::Notice, TURNING_IT_OFF);
            return None;
        }
        TierChoice::Unknown => {
            io.surface()
                .line(LineKind::Error, &unknown_tier_line(typed.trim()));
            return None;
        }
    };
    // AC-7: the menu marks it and the selection is refused, from the one fact
    // the daemon reported — the client forms no opinion about whether search can
    // serve, it only declines to walk someone into a tier that would refuse
    // every query.
    if tier == WebTier::Search && !plan.search_available {
        io.surface().line(
            LineKind::Error,
            &search_refused_line(plan.search_gap.as_deref()),
        );
        return None;
    }

    if tier < WebTier::Search {
        return Some(Answers {
            tier,
            search_endpoint: None,
            search_auth: None,
            search_key: None,
        });
    }

    // The suggestions are the daemon's, rendered (BR-1) — and a daemon that sent
    // none leaves this file naming no backend rather than inventing a list.
    let catalog = plan.suggestion_catalog.as_ref();
    for line in suggestion_lines(catalog) {
        io.surface().line(LineKind::Info, &line);
    }
    let endpoint = io.prompter().ask(ENDPOINT_QUESTION)?;
    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        return None;
    }

    let needs_key = is_yes_by_default(&io.prompter().ask(KEY_NEEDED_QUESTION)?);
    let (search_auth, search_key) = if needs_key {
        let offered = offered_auth(endpoint, catalog);
        let auth = io.prompter().ask(&auth_question(&offered))?;
        let auth = auth.trim();
        // Empty here is an answer — "take what was offered" — and not an abort,
        // which is why the question says what Enter means.
        //
        // What that resolves to differs by offer, and the difference is the
        // point. For the catalog's own generic template it is `None`: the daemon
        // writes no `search_auth` key at all and applies exactly that default,
        // so the default stays one value in one place instead of being copied
        // into every config this flow writes. For a backend whose own header was
        // offered, it is that template on the wire — a `None` there would write
        // a generic config against a backend that answers 401 to it, which is a
        // walkthrough handing out a broken setup and blaming the key.
        let auth = if auth.is_empty() {
            (offered != default_auth(catalog)).then(|| offered.clone())
        } else {
            Some(auth.to_owned())
        };
        let key = io.prompter().ask_secret(KEY_QUESTION)?;
        let key = key.trim();
        if key.is_empty() {
            return None;
        }
        (auth, Some(key.to_owned()))
    } else {
        (None, None)
    };

    Some(Answers {
        tier,
        search_endpoint: Some(endpoint.to_owned()),
        search_auth,
        search_key,
    })
}

/// What a tier answer resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TierChoice {
    /// One of the three tiers this flow can enable.
    Tier(WebTier),
    /// The user asked to turn the capability *off*, which this flow does not do.
    Off,
    /// Not a tier.
    Unknown,
}

/// Read a tier answer: the menu number, or the tier's config spelling.
///
/// Both spellings are accepted because both are in front of the user — the menu
/// numbers it and the line names it, and the name is what they will later see in
/// their config file. Nothing else is guessed at: a near-miss is answered with
/// the list rather than resolved to whichever tier is closest, since the value
/// decides what the machine may fetch.
fn parse_tier(answer: &str) -> TierChoice {
    match answer.to_lowercase().as_str() {
        "1" | "fetch_user_url" => TierChoice::Tier(WebTier::FetchUserUrl),
        "2" | "fetch_any_url" => TierChoice::Tier(WebTier::FetchAnyUrl),
        "3" | "search" => TierChoice::Tier(WebTier::Search),
        "off" | "0" => TierChoice::Off,
        _ => TierChoice::Unknown,
    }
}

/// An explicit yes, and nothing else (LESSON-470). Empty and EOF are both no.
fn is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}

/// A yes unless the user actually said no — for the one question where the
/// costly wrong answer is the *other* way round.
///
/// Answering "does this backend need a key?" wrongly as "no" skips the credential
/// prompt and produces a search config that 401s; answering it wrongly as "yes"
/// asks one more question the user can leave by pressing Enter. Nothing is
/// written either way — the consent that matters is the default-no confirm.
fn is_yes_by_default(answer: &str) -> bool {
    !matches!(answer.trim().to_lowercase().as_str(), "n" | "no")
}

// ---------------------------------------------------------------------------
// Content (pure)
// ---------------------------------------------------------------------------

/// What the plan says, as lines: the capability state, the table as it stands,
/// and the search gap when there is one.
fn plan_lines(plan: &WebSetupPlanResult) -> Vec<(LineKind, String)> {
    let mut lines = vec![(LineKind::Info, capability_line(&plan.state))];
    lines.push(match &plan.current_web {
        Some(current) => (LineKind::Info, current_table_line(current)),
        None => (
            LineKind::Info,
            "there is no `[web]` table in your config yet.".to_owned(),
        ),
    });
    if !plan.search_available {
        lines.push((
            LineKind::Notice,
            format!(
                "the `search` tier cannot be offered on this machine: {}.",
                plan.search_gap.as_deref().unwrap_or(UNNAMED_GAP)
            ),
        ));
    }
    lines
}

/// What a `search_gap` the daemon left empty is called. It should not happen —
/// the protocol pairs the flag with the reason — but a blank in the middle of a
/// sentence is worse than a word that admits the daemon said nothing.
const UNNAMED_GAP: &str = "the daemon named no reason";

/// One line for the capability state the daemon derived. Rendered, never
/// re-derived: the state is a typed value precisely so a client branches on the
/// variant rather than on prose (BR-10).
fn capability_line(state: &WebCapabilityState) -> String {
    match state {
        WebCapabilityState::Ready { tier } => format!(
            "web lookup is on, with `{}` as the configured ceiling.",
            web_tier_name(*tier)
        ),
        WebCapabilityState::OffAvailable => {
            "web lookup is available on this machine and currently off.".to_owned()
        }
        WebCapabilityState::SearchUnavailable { reason } => format!(
            "web lookup is configured for search, but the search leg cannot serve: {reason}."
        ),
    }
}

/// The `[web]` table as it stands, in one line. Every field here is non-secret
/// by construction — a tier, a host, and two references.
fn current_table_line(current: &WebTableSummary) -> String {
    let mut parts = vec![format!("tier `{}`", web_tier_name(current.tier))];
    if let Some(host) = &current.search_host {
        parts.push(format!("search host {host}"));
    }
    if let Some(key_ref) = &current.search_key_ref {
        parts.push(format!("key {key_ref}"));
    }
    if let Some(auth) = &current.search_auth {
        parts.push(format!("auth `{auth}`"));
    }
    format!("current `[web]` table: {}.", parts.join(", "))
}

/// The tier menu. The `search` row carries the daemon's own reason when the
/// machine cannot serve it, so the entry is marked where it is read (AC-7).
fn tier_menu_lines(plan: &WebSetupPlanResult) -> Vec<String> {
    let search_note = if plan.search_available {
        String::new()
    } else {
        format!(
            "  (unavailable: {})",
            plan.search_gap.as_deref().unwrap_or(UNNAMED_GAP)
        )
    };
    vec![
        "each tier includes the ones before it; consent is still asked per lookup.".to_owned(),
        "  1) fetch_user_url  fetch a URL you pasted into this session".to_owned(),
        "  2) fetch_any_url   also fetch a URL the model composed".to_owned(),
        format!("  3) search          also search through a backend you name{search_note}"),
    ]
}

/// What the endpoint question is preceded by when the daemon sent a list to
/// name. The shapes themselves are the catalog's; only the sentence is here.
const ENDPOINT_HELP_HEADER: &str = "name the search backend to query. Shapes that are known to \
                                    work:";

/// The same preamble with nothing to list — a daemon that predates the catalog
/// (BR-3). It asks the question and names no backend, because a client that
/// filled the silence with a list of its own is the drift REQ-573 removes.
const ENDPOINT_HELP_BARE: &str = "name the search backend to query.";

/// The suggestions to show above the endpoint question, rendered from the
/// catalog the daemon sent (BR-1).
///
/// Every backend fact in the output — the name, the endpoint, the header it
/// wants, any note — arrives as data. The one thing composed here is the layout:
/// the labels are padded to the widest one so the endpoints line up in a column,
/// which is a property of the list rather than of any entry in it.
///
/// A note gets its own line under the entry it belongs to, indented to the same
/// column. It is a sentence, and a sentence hung off the end of an already-long
/// row is where a terminal wraps it into the next backend's line — this way the
/// row a user reads is the same row whether the daemon sent a note or not.
fn suggestion_lines(catalog: Option<&WebSetupCatalog>) -> Vec<String> {
    let backends = catalog.map_or(&[][..], |catalog| catalog.backends.as_slice());
    let Some(label_width) = backends
        .iter()
        .map(|entry| entry.label.chars().count())
        .max()
    else {
        return vec![ENDPOINT_HELP_BARE.to_owned()];
    };
    let mut lines = vec![ENDPOINT_HELP_HEADER.to_owned()];
    for entry in backends {
        lines.push(suggestion_line(entry, label_width));
        if let Some(notes) = &entry.notes {
            lines.push(format!("  {blank:label_width$}  {notes}", blank = ""));
        }
    }
    lines
}

/// One suggestion as the row a user reads.
///
/// **`needs_key` decides whether there is a credential; the template only says
/// how one would ride.** The protocol declares the two present together (see
/// `WebBackendSuggestion`), so a template beside `needs_key: false` is a
/// malformed entry — and the row states the field that is *about* the question
/// the row answers, rather than inferring the answer back out of the template.
/// The alternative reads a keyless backend to the user as one that wants a
/// header, which is the wrong half of a contradiction to believe.
///
/// (What [`offered_auth`] does with that same template is a different question —
/// "if you do have a key, how does it ride?" — asked only of a user who has
/// already said, at the prompt after this row, that they have one.)
fn suggestion_line(entry: &WebBackendSuggestion, label_width: usize) -> String {
    let credential = match (&entry.auth_template, entry.needs_key) {
        (Some(template), true) => format!("(header `{template}`)"),
        // A backend that wants a key and names no header of its own: the generic
        // template is what the next prompt will offer, and saying it needs a key
        // beats saying nothing.
        (None, true) => "(needs a key)".to_owned(),
        (_, false) => "(no key)".to_owned(),
    };
    format!(
        "  {label:label_width$}  {endpoint}  {credential}",
        label = entry.label,
        endpoint = entry.endpoint,
    )
}

/// The template to offer when nothing more specific applies: the catalog's own
/// default, or the shared protocol constant when no catalog arrived (ADR-B).
///
/// The constant is imported rather than spelled out because both binaries need
/// the *same* string, and a second literal here is exactly the copy this REQ
/// deletes.
fn default_auth(catalog: Option<&WebSetupCatalog>) -> &str {
    catalog.map_or(GENERIC_SEARCH_AUTH_TEMPLATE, |catalog| {
        catalog.default_auth_template.as_str()
    })
}

/// The auth-header template to offer for `endpoint`.
///
/// A host the catalog names gets that backend's own header; everything else — a
/// self-hosted instance, a proxy, anything unparseable, or a daemon that sent no
/// catalog at all — gets the generic default. This decides a *default*, never a
/// value: an answer typed over the offer wins, and the daemon validates either
/// way.
fn offered_auth(endpoint: &str, catalog: Option<&WebSetupCatalog>) -> String {
    catalog
        .and_then(|catalog| matched_backend(endpoint, catalog))
        .and_then(|entry| entry.auth_template.clone())
        .unwrap_or_else(|| default_auth(catalog).to_owned())
}

/// The catalog entry a typed endpoint belongs to, by host (BR-8).
///
/// Only entries that named a host **and** a header can match. The host half is
/// BR-8's: a self-hosted backend's host is wherever the user runs it, so there
/// is nothing to recognise it by. The header half is a degradation rule — an
/// entry that names a host but no template has nothing to offer, and matching it
/// would let a malformed first entry shadow a later, well-formed one for the
/// same host and silently swap that backend's header for the generic default.
/// Skipping it falls through to the catalog's own default, which is what a host
/// nobody claimed already gets.
fn matched_backend<'a>(
    endpoint: &str,
    catalog: &'a WebSetupCatalog,
) -> Option<&'a WebBackendSuggestion> {
    let host = endpoint_host(endpoint)?;
    catalog.backends.iter().find(|entry| {
        entry.auth_template.is_some()
            && entry
                .host
                .as_deref()
                .is_some_and(|known| host.eq_ignore_ascii_case(known))
    })
}

/// The host of an absolute `http(s)` endpoint, or `None` when it is not one.
///
/// Deliberately small and deliberately not a validator: whether the endpoint is
/// acceptable is the daemon's answer (BR-7), and this only has to be right
/// enough to recognise a host the catalog names. Anything it cannot read falls
/// back to the generic offer, which is the pre-existing behaviour.
///
/// **Residual, and the condition it rests on** (LESSON-494). This splits only on
/// `/`, `?` and `#`, so an authority containing a backslash reads differently
/// here than it would in a WHATWG-shaped parser — `https://evil.test\@kagi.com/`
/// is the shape of the disagreement. That is tolerable for exactly two reasons,
/// both outside this function: `Config::validate` refuses such an endpoint
/// outright (`teton_core::ConfigError::InvalidWebSearchEndpoint`), so no config
/// this offer leads to can ever load; and the host the *user* is shown before
/// confirming comes from the daemon's canonical parse, on
/// `WebSetupPreviewResult::search_host`, not from this. A disagreement therefore
/// costs at most a wrong *default* on a config that will be refused. **If that
/// refusal is ever softened, this parse has to move to the canonical parser** —
/// the loose read is only safe while the strict one is the gate behind it.
fn endpoint_host(endpoint: &str) -> Option<&str> {
    let endpoint = endpoint.trim();
    let rest = endpoint
        .strip_prefix("https://")
        .or_else(|| endpoint.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    // Userinfo first (everything before the last `@`), then the port.
    let host = authority.rsplit('@').next()?.split(':').next()?;
    (!host.is_empty()).then_some(host)
}

/// The refusal a `search` selection gets when the plan says the machine cannot
/// serve it. It names the missing piece — the daemon's sentence — and the tier
/// that *is* reachable, so the answer is not a dead end.
fn search_refused_line(gap: Option<&str>) -> String {
    format!(
        "`search` cannot be enabled on this machine: {}. Nothing was changed — choose \
         `fetch_any_url` or lower, or set the local model up first.",
        gap.unwrap_or(UNNAMED_GAP)
    )
}

/// The rejection an unrecognised tier answer gets, quoting what was typed
/// through the same bounded, sanitised echo a bad command name goes through.
fn unknown_tier_line(typed: &str) -> String {
    format!(
        "`{}` is not one of the tiers — answer 1, 2 or 3, or the tier's own name. Nothing was \
         changed.",
        crate::slash::echoed(typed)
    )
}

/// What an answer of `off` gets. This flow enables a capability; disabling one is
/// a config edit, and saying so is better than writing `tier = "off"` under a
/// command whose completion notice announces an enablement.
const TURNING_IT_OFF: &str =
    "`/web setup` turns web lookup on. To turn it off, set `[web] tier = \"off\"` in your \
     config — nothing here was changed.";

/// The preview, as lines: the daemon's `[web]` bytes, the host its parse
/// produced, and its warnings. All three are rendered verbatim (BR-7,
/// LESSON-494) — this function decides layout and nothing else.
fn preview_lines(preview: &WebSetupPreviewResult) -> Vec<(LineKind, String)> {
    let mut lines = vec![(
        LineKind::Info,
        "this is what would be written to your config:".to_owned(),
    )];
    for line in preview.toml.lines() {
        lines.push((LineKind::Info, format!("  {line}")));
    }
    if let Some(host) = &preview.search_host {
        lines.push((
            LineKind::Info,
            format!("searches would go to: {host} (and nowhere else without a further consent)"),
        ));
    }
    for warning in &preview.warnings {
        lines.push((LineKind::Notice, warning.clone()));
    }
    lines
}

/// One line for a call the daemon refused, carrying its own sentence.
///
/// `stage` is what did not happen — "previewed" or "written" — so the user
/// learns which side of the commit point they are on without the message having
/// to say it twice.
fn refused_line(err: &RpcError, stage: &str) -> String {
    format!("nothing was {stage}: {}", err.message)
}

/// What the undo did, said out loud — including when it did nothing.
///
/// A failure to clean up is reported rather than swallowed: the user is the only
/// one who can act on the keychain by hand, and a credential left in a state
/// they were never told about is exactly the residue ADR-3 exists to avoid. Each
/// arm therefore ends in the command that finishes the job it could not.
fn cleanup_line(cleanup: &Cleanup) -> String {
    match cleanup {
        Cleanup::Deleted(Ok(())) => {
            "the key that was stored for this attempt has been removed from your keychain."
                .to_owned()
        }
        Cleanup::Deleted(Err(err)) => format!(
            "the key stored for this attempt could not be removed from your keychain ({err}) — \
             it is unreferenced, and `security delete-generic-password -s teton -a \
             {SEARCH_KEY_ACCOUNT}` clears it."
        ),
        Cleanup::Restored(Ok(())) => format!(
            "the key stored for this attempt has been replaced with the one that was there \
             before, so the `{SEARCH_KEY_ACCOUNT}` entry your config already points at is \
             unchanged."
        ),
        Cleanup::Restored(Err(err)) => format!(
            "the key that was in your keychain as `{SEARCH_KEY_ACCOUNT}` before this attempt \
             could not be put back ({err}) — the entry now holds the key you just typed, so a \
             config pointing at it is using the new key. Run `/web setup` again, or restore the \
             entry with `security add-generic-password -U -s teton -a {SEARCH_KEY_ACCOUNT} -w`."
        ),
        Cleanup::LeftInPlace(why) => format!(
            "your keychain could not be read before this attempt ({why}), so the key you typed \
             was left in `{SEARCH_KEY_ACCOUNT}` rather than risk removing a credential your \
             config still uses — `security find-generic-password -s teton -a \
             {SEARCH_KEY_ACCOUNT}` shows what is there."
        ),
    }
}

/// What a commit that applied nothing says.
///
/// "Nothing changed" is true of the config and **false of the keychain** when
/// this run stored a key: the table already matched, so no write was needed, but
/// the credential behind `search_key_ref` is the one just typed. A user rotating
/// a key against an unchanged config would otherwise be told their rotation did
/// not happen, and go looking for the reason.
fn unchanged_line(tier: WebTier, prior: &Option<PriorKey>) -> String {
    let tier = web_tier_name(tier);
    match prior {
        None => format!(
            "web lookup was already configured exactly this way (`{tier}`), so nothing changed."
        ),
        Some(_) => format!(
            "web lookup was already configured exactly this way (`{tier}`), so your config is \
             unchanged — the key stored in your keychain as `{SEARCH_KEY_ACCOUNT}` was updated to \
             the one you just typed."
        ),
    }
}

/// What a commit the daemon never answered says.
///
/// The one honest sentence available: the write either landed or did not, this
/// process cannot tell, and there is a command that can. Everything about the
/// keychain is reported as *left alone* because that is what happened — the
/// alternative was to guess which of two destructive undos was the right one and
/// be wrong half the time (a delete orphans a landed config; a restore
/// resurrects the key the user was replacing).
fn ambiguous_commit_line(prior: &Option<PriorKey>) -> String {
    let mut line = String::from(
        "your config may or may not have been written — run `/web setup` again to see which: its \
         first lines report the `[web]` table as it now stands.",
    );
    match prior {
        None => {}
        Some(prior) if prior.displaced() => line.push_str(&format!(
            " The key you typed is in your keychain as `{SEARCH_KEY_ACCOUNT}`, in place of the one \
             that was there before, and was left there: taking it back out would break the setup \
             if the write did land."
        )),
        // Absent or unreadable: nothing known to be displaced, so the removal
        // command is safe to offer once the user has confirmed nothing landed.
        Some(_) => line.push_str(&format!(
            " The key you typed is in your keychain as `{SEARCH_KEY_ACCOUNT}` and was left there: \
             taking it back out would break the setup if the write did land. If it did not, \
             `security delete-generic-password -s teton -a {SEARCH_KEY_ACCOUNT}` removes it."
        )),
    }
    line
}

/// What a piped session is told instead of being asked (BR-12 / AC-10).
///
/// It is a *degradation*, not a refusal: everything the walkthrough would have
/// collected is named here, in the vocabulary the config file uses, so the same
/// outcome is reachable by hand. Nothing is read from stdin — the line the user
/// typed for the session stays theirs.
///
/// The two facts a backend owns — an endpoint worth pasting and the header a key
/// rides — come from the same catalog the walkthrough draws (BR-1). The example
/// is the catalog's first entry, which is the order a client is told to show
/// them in; a daemon that sent no catalog gets the field named without an
/// example rather than one this file made up.
fn instruction_lines(catalog: Option<&WebSetupCatalog>) -> Vec<(LineKind, String)> {
    let example_endpoint = catalog
        .and_then(|catalog| catalog.backends.first())
        .map(|entry| entry.endpoint.as_str());
    [
        (
            LineKind::Notice,
            "`/web setup` collects an endpoint and a key without echoing it, which needs a \
             terminal — this session's input is not one, so nothing was read and nothing was \
             changed."
                .to_owned(),
        ),
        (
            LineKind::Info,
            "run `teton` in a terminal and type `/web setup`, or write the `[web]` table into \
             your config yourself:"
                .to_owned(),
        ),
        (
            LineKind::Info,
            "  tier             \"fetch_user_url\" | \"fetch_any_url\" | \"search\" (each \
             includes the ones before it)"
                .to_owned(),
        ),
        (
            LineKind::Info,
            match example_endpoint {
                Some(endpoint) => format!(
                    "  search_endpoint  the backend the `search` tier queries, e.g. {endpoint}"
                ),
                None => "  search_endpoint  the backend the `search` tier queries".to_owned(),
            },
        ),
        (
            LineKind::Info,
            format!(
                "  search_key_ref   a keychain reference — `{}` — never a raw key",
                auth_ref_for(SEARCH_KEY_ACCOUNT)
            ),
        ),
        (
            LineKind::Info,
            format!(
                "  search_auth      the header the key rides, `{{key}}` marking the secret \
                 (default `{}`)",
                default_auth(catalog)
            ),
        ),
        // The reference above names an entry that does not exist yet, and the
        // walkthrough is the only thing that creates one — which on a piped
        // session is precisely what is unavailable. Without this line the
        // instructions describe a config that cannot be made to work.
        (
            LineKind::Info,
            format!(
                "the keychain entry itself is written with `security add-generic-password -s \
                 {} -a {SEARCH_KEY_ACCOUNT} -w` — put `-w` last and it prompts for the key \
                 instead of leaving it in your shell history.",
                crate::keychain::SERVICE
            ),
        ),
        (
            LineKind::Info,
            "the `search` tier also needs the local model, which scans every query before it \
             leaves the machine."
                .to_owned(),
        ),
    ]
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keychain::MockKeychain;
    use crate::prompt::ScriptedPrompter;
    use crate::render::{RecordingSurface, Rendered};

    /// A planted credential: distinctive enough that a sweep over rendered lines
    /// and serialized frames means something.
    const PLANTED_KEY: &str = "sk-planted-web-search-key";

    /// The credential a rotation displaces — the one the live config already
    /// references, and the one a refused commit owes the machine back.
    const PREVIOUS_KEY: &str = "sk-previous-web-search-key";

    /// The seam, wired to canned answers and a recording surface.
    struct FakeIo {
        surface: RecordingSurface,
        prompter: ScriptedPrompter,
        plan: Result<WebSetupPlanResult, RpcError>,
        preview: Result<WebSetupPreviewResult, RpcError>,
        commit: Result<WebSetupCommitResult, RpcError>,
        /// When set, `commit` fails at the **transport** level instead of
        /// answering: the socket broke, or the daemon died, after the frame went
        /// out.
        ///
        /// A different event from `commit = Err(RpcError)`, and the difference is
        /// the whole point: a daemon that answers "no" has certainly not
        /// written, while a daemon that does not answer may have written and
        /// died. One of those licenses an undo and the other does not.
        commit_transport_error: Option<&'static str>,
        /// Every frame that crossed, kept as sent — the capture AC-5's "the
        /// secret appears in no RPC params" is asserted against.
        previews: Vec<WebSetupPreviewParams>,
        commits: Vec<WebSetupCommitParams>,
    }

    impl FakeIo {
        fn new(answers: &[&str]) -> Self {
            Self {
                surface: RecordingSurface::new(),
                prompter: ScriptedPrompter::new(answers),
                plan: Ok(plan_ready_for_search()),
                preview: Ok(preview_result()),
                commit: Ok(WebSetupCommitResult {
                    applied: true,
                    tier: WebTier::Search,
                }),
                commit_transport_error: None,
                previews: Vec::new(),
                commits: Vec::new(),
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

        /// Every frame this flow sent, as JSON — the shape a socket would have
        /// carried.
        fn frames(&self) -> String {
            let previews = serde_json::to_string(&self.previews).expect("params serialize");
            let commits = serde_json::to_string(&self.commits).expect("params serialize");
            format!("{previews}{commits}")
        }
    }

    impl SetupIo for FakeIo {
        fn surface(&mut self) -> &mut dyn Surface {
            &mut self.surface
        }

        fn prompter(&mut self) -> &mut dyn Prompter {
            &mut self.prompter
        }

        fn plan(
            &mut self,
            _params: WebSetupPlanParams,
        ) -> anyhow::Result<Result<WebSetupPlanResult, RpcError>> {
            Ok(self.plan.clone())
        }

        fn preview(
            &mut self,
            params: WebSetupPreviewParams,
        ) -> anyhow::Result<Result<WebSetupPreviewResult, RpcError>> {
            self.previews.push(params);
            Ok(self.preview.clone())
        }

        fn commit(
            &mut self,
            params: WebSetupCommitParams,
        ) -> anyhow::Result<Result<WebSetupCommitResult, RpcError>> {
            // Recorded before the failure: the frame went out either way, which
            // is exactly why a transport failure leaves the outcome unknown.
            self.commits.push(params);
            match self.commit_transport_error {
                Some(message) => Err(anyhow::anyhow!(message)),
                None => Ok(self.commit.clone()),
            }
        }
    }

    fn session() -> SessionId {
        SessionId::from("sess-web-setup")
    }

    fn plan_ready_for_search() -> WebSetupPlanResult {
        WebSetupPlanResult {
            state: WebCapabilityState::OffAvailable,
            search_available: true,
            search_gap: None,
            current_web: None,
            suggestion_catalog: Some(shipped_catalog()),
        }
    }

    fn plan_without_search() -> WebSetupPlanResult {
        WebSetupPlanResult {
            state: WebCapabilityState::OffAvailable,
            search_available: false,
            search_gap: Some("search needs the local model".to_owned()),
            current_web: None,
            // A daemon that cannot serve search still sends its catalog: what
            // this fixture varies is the gap, and nothing else.
            suggestion_catalog: Some(shipped_catalog()),
        }
    }

    /// The catalog as the daemon ships it, at the values the product uses.
    ///
    /// **This is the parity fixture** (AC-6). These are the only lines in this
    /// crate where the shipped backends are spelled out, and they are *test
    /// data*: the flow reads none of them, which is the point —
    /// `the_shipped_catalog_renders_the_req_572_wording_byte_for_byte` feeds them
    /// in and pins the bytes that come out, so a rendering change that moved the
    /// wording fails here rather than in a user's terminal.
    ///
    /// **Nothing checks this copy against the daemon's.** It is a hand-written
    /// transcription of `tetond::web_setup_catalog::suggestion_catalog()`, and if
    /// the daemon changed a label tomorrow this fixture would keep asserting the
    /// old one, green. Two things are load-bearing instead of that: the daemon's
    /// own golden test is the authority for what the shipped strings *are*
    /// (ADR-E), and `cli_e2e`'s walkthrough — which runs the real daemon's
    /// catalog through the real renderer — is the one assertion in CI where the
    /// two ends meet, so a drift between them fails there. What this fixture pins
    /// is the renderer, given a catalog.
    fn shipped_catalog() -> WebSetupCatalog {
        WebSetupCatalog {
            default_auth_template: GENERIC_SEARCH_AUTH_TEMPLATE.to_owned(),
            backends: vec![
                WebBackendSuggestion {
                    id: "searxng".to_owned(),
                    label: "self-hosted SearxNG".to_owned(),
                    endpoint: "http://localhost:8888/search?format=json".to_owned(),
                    host: None,
                    auth_template: None,
                    needs_key: false,
                    notes: None,
                },
                WebBackendSuggestion {
                    id: "brave".to_owned(),
                    label: "Brave Search API".to_owned(),
                    endpoint: "https://api.search.brave.com/res/v1/web/search".to_owned(),
                    host: Some("api.search.brave.com".to_owned()),
                    auth_template: Some("X-Subscription-Token: {key}".to_owned()),
                    needs_key: true,
                    notes: None,
                },
                WebBackendSuggestion {
                    id: "kagi".to_owned(),
                    label: "Kagi Search API".to_owned(),
                    endpoint: "https://kagi.com/api/v0/search".to_owned(),
                    host: Some("kagi.com".to_owned()),
                    auth_template: Some("Authorization: Bot {key}".to_owned()),
                    needs_key: true,
                    notes: None,
                },
            ],
        }
    }

    /// A catalog no daemon would ever ship (LESSON-497): every field says
    /// "sentinel" out loud, so a rendered line carrying one of these words can
    /// only have come off the wire.
    fn sentinel_catalog() -> WebSetupCatalog {
        WebSetupCatalog {
            default_auth_template: "X-Sentinel-Default: {key}".to_owned(),
            backends: vec![
                WebBackendSuggestion {
                    id: "sentinel-keyless".to_owned(),
                    label: "Sentinel Keyless".to_owned(),
                    endpoint: "http://localhost:9999/sentinel?format=json".to_owned(),
                    host: None,
                    auth_template: None,
                    needs_key: false,
                    notes: None,
                },
                WebBackendSuggestion {
                    id: "sentinel-backend".to_owned(),
                    label: "Sentinel Backend With A Long Label".to_owned(),
                    endpoint: "https://sentinel.example/search".to_owned(),
                    host: Some("sentinel.example".to_owned()),
                    auth_template: Some("X-Sentinel-Header: {key}".to_owned()),
                    needs_key: true,
                    notes: Some("a sentinel note".to_owned()),
                },
            ],
        }
    }

    /// A catalog whose entries contradict the shape the protocol declares — the
    /// wire states a daemon should never produce and this client must not be
    /// steered by.
    ///
    /// Deliberately **not** folded into [`sentinel_catalog`]. That fixture is a
    /// *well-formed* catalog nothing would ship, and the properties asserted over
    /// it — every hosted suggestion names the header it wants, every template
    /// offered is one the list the user just read names — are true of any
    /// well-formed catalog and false of this one. Merging them would have bought
    /// this coverage by taking the teeth out of those.
    fn malformed_catalog() -> WebSetupCatalog {
        WebSetupCatalog {
            default_auth_template: "X-Sentinel-Default: {key}".to_owned(),
            backends: vec![
                // Says it needs a key and names no header to carry one.
                WebBackendSuggestion {
                    id: "sentinel-headerless".to_owned(),
                    label: "Sentinel Headerless".to_owned(),
                    endpoint: "https://headerless.sentinel.example/search".to_owned(),
                    host: Some("headerless.sentinel.example".to_owned()),
                    auth_template: None,
                    needs_key: true,
                    notes: None,
                },
                // Names a header while saying it needs no key.
                WebBackendSuggestion {
                    id: "sentinel-vestigial".to_owned(),
                    label: "Sentinel Vestigial".to_owned(),
                    endpoint: "https://vestigial.sentinel.example/search".to_owned(),
                    host: Some("vestigial.sentinel.example".to_owned()),
                    auth_template: Some("X-Sentinel-Vestigial: {key}".to_owned()),
                    needs_key: false,
                    notes: None,
                },
                // Two entries for one host, the template-less one first: the
                // shadowing case the host match has to see past.
                WebBackendSuggestion {
                    id: "sentinel-shadow-empty".to_owned(),
                    label: "Sentinel Shadow Empty".to_owned(),
                    endpoint: "https://shadow.sentinel.example/search".to_owned(),
                    host: Some("shadow.sentinel.example".to_owned()),
                    auth_template: None,
                    needs_key: true,
                    notes: None,
                },
                WebBackendSuggestion {
                    id: "sentinel-shadow-real".to_owned(),
                    label: "Sentinel Shadow Real".to_owned(),
                    endpoint: "https://shadow.sentinel.example/v1/search".to_owned(),
                    host: Some("shadow.sentinel.example".to_owned()),
                    auth_template: Some("X-Sentinel-Shadowed: {key}".to_owned()),
                    needs_key: true,
                    notes: None,
                },
            ],
        }
    }

    /// The piped instructions as one string — the BR-12 path, rendered.
    fn rendered_instructions(catalog: Option<&WebSetupCatalog>) -> String {
        instruction_lines(catalog)
            .into_iter()
            .map(|(_, text)| text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The plan a daemon that predates the catalog answers with (BR-3).
    fn plan_without_a_catalog() -> WebSetupPlanResult {
        WebSetupPlanResult {
            suggestion_catalog: None,
            ..plan_ready_for_search()
        }
    }

    fn preview_result() -> WebSetupPreviewResult {
        WebSetupPreviewResult {
            toml: "[web]\ntier = \"search\"\nsearch_endpoint = \"https://example.test/search\"\n\
                   search_key_ref = \"keychain://teton/web-search\"\n"
                .to_owned(),
            search_host: Some("example.test".to_owned()),
            warnings: vec![
                "this replaces the current `[web]` table: search_auth will be removed.".to_owned(),
            ],
            // A daemon that predates the field sends exactly this, so the
            // fixture doubles as the compatibility case: the flow degrades an
            // empty digest to "do not check" (pinned by
            // `a_previewed_digest_rides_the_commit_and_an_absent_one_does_not`).
            digest: String::new(),
        }
    }

    fn preview_result_with_digest(digest: &str) -> WebSetupPreviewResult {
        WebSetupPreviewResult {
            digest: digest.to_owned(),
            ..preview_result()
        }
    }

    /// The header template the full walk *types* — a value of the user's own,
    /// deliberately not a backend's, so what it pins is that a typed answer rides
    /// the wire as typed.
    const TYPED_AUTH: &str = "X-Typed-Header: {key}";

    /// The answers of a complete search walk with a key.
    const FULL_WALK: &[&str] = &[
        "3",
        "https://example.test/search",
        "y",
        TYPED_AUTH,
        PLANTED_KEY,
        "y",
    ];

    /// BR-7's guard is only real if the previewed digest actually rides the
    /// commit — and only when the daemon offered one: an empty digest is a
    /// daemon that predates the field, and inventing a value for it could
    /// only ever mismatch.
    ///
    /// The degrade must also be **said** (BUG-166 residual (a)): silently
    /// dropping the confirmed-bytes check on version skew made the guard's
    /// absence a fact only the source code knew, and it is rendered before
    /// the confirm question because declining is the one act the flow offers
    /// a user who minds. Both directions are pinned — the notice when the
    /// check is off, its absence when the check is on — or the line would
    /// drift into constant noise the first time someone hoisted it.
    #[test]
    fn a_previewed_digest_rides_the_commit_and_an_absent_one_does_not() {
        let mut io = FakeIo::new(FULL_WALK);
        io.preview = Ok(preview_result_with_digest("abc123"));
        let keychain = MockKeychain::new();
        drive(&mut io, &keychain, &session(), Gate::Walk).unwrap();
        assert_eq!(io.commits[0].expect_digest.as_deref(), Some("abc123"));
        assert!(
            !io.rendered().contains(DIGEST_CHECK_UNAVAILABLE),
            "a daemon that offered a digest must not be reported as lacking one"
        );

        let mut io = FakeIo::new(FULL_WALK);
        let keychain = MockKeychain::new();
        drive(&mut io, &keychain, &session(), Gate::Walk).unwrap();
        assert_eq!(
            io.commits[0].expect_digest, None,
            "an empty digest from an old daemon must degrade to the protocol's own do-not-check"
        );
        assert!(
            io.rendered().contains(DIGEST_CHECK_UNAVAILABLE),
            "and the degrade must be rendered, not silent: the user is about to \
             confirm bytes the daemon will not be held to"
        );
    }

    /// The gate is the one thing that decides whether a question is ever put to
    /// a stdin nobody is typing into (LESSON-470).
    #[test]
    fn the_gate_walks_at_a_terminal_and_degrades_on_a_pipe() {
        assert_eq!(gate(true, false), Gate::Walk);
        assert_eq!(gate(true, true), Gate::Walk);
        // The e2e suite drives this command over pipes; the seam is the only way
        // in, and it makes the gate looser, never stricter.
        assert_eq!(gate(false, true), Gate::Walk);
        assert_eq!(gate(false, false), Gate::Instructions);
    }

    /// AC-10: a piped session is told what to do, is asked nothing at all, and
    /// consumes no line that was meant for the session.
    #[test]
    fn a_piped_session_is_told_what_to_type_and_asked_nothing() {
        let mut io = FakeIo::new(&["3", "https://example.test/search"]);
        let keychain = MockKeychain::new();
        drive(&mut io, &keychain, &session(), Gate::Instructions).unwrap();

        assert_eq!(
            io.prompter.asked, 0,
            "the instruction path must not read stdin"
        );
        assert!(io.previews.is_empty() && io.commits.is_empty());
        assert!(keychain.is_empty() && keychain.deletes().is_empty());
        let rendered = io.rendered();
        for needle in ["[web]", "tier", "search_endpoint", "search_key_ref"] {
            assert!(rendered.contains(needle), "{rendered}");
        }
        assert!(
            rendered.contains("keychain://teton/web-search"),
            "the instructions must name the reference, not leave it to be guessed: {rendered}"
        );
        // …and the way to make the thing that reference names. The walkthrough
        // is the only other way to create the entry, and it is exactly what this
        // session cannot run — instructions for a config that cannot be made to
        // work are not a degradation, they are a dead end (BR-12/AC-10).
        assert!(
            rendered.contains("security add-generic-password -s teton -a web-search -w"),
            "the instructions must say how to create the entry, not only how to \
             reference it: {rendered}"
        );
    }

    /// **BR-3 on the piped path, driven through [`drive`] rather than through
    /// [`instruction_lines`].**
    ///
    /// The two degradations compose: no terminal *and* no catalog. Calling
    /// `instruction_lines(None)` proves what that function renders; it does not
    /// prove that `drive` hands it the plan's `None` rather than a catalog
    /// synthesised on the way, nor that the path still asks nothing. Both are
    /// exactly the sort of thing an edit to the gate could quietly change, so the
    /// combination is asserted where a user would meet it.
    #[test]
    fn a_piped_session_without_a_catalog_names_no_backend_and_still_asks_nothing() {
        let mut io = FakeIo::new(&["3", "https://example.test/search"]);
        io.plan = Ok(plan_without_a_catalog());
        let keychain = MockKeychain::new();
        drive(&mut io, &keychain, &session(), Gate::Instructions).unwrap();

        assert_eq!(
            io.prompter.asked, 0,
            "the instruction path must not read stdin"
        );
        assert!(io.previews.is_empty() && io.commits.is_empty());
        assert!(keychain.is_empty() && keychain.deletes().is_empty());

        let rendered = io.rendered();
        // The field is still named; no example is invented for it.
        assert!(
            rendered.contains("  search_endpoint  the backend the `search` tier queries")
                && !rendered.contains("e.g."),
            "no catalog means the field is named without an example:\n{rendered}"
        );
        assert!(
            rendered.contains(GENERIC_SEARCH_AUTH_TEMPLATE),
            "the header default degrades to the shared protocol constant:\n{rendered}"
        );
        for shipped in shipped_catalog().backends {
            assert!(
                !rendered.contains(&shipped.label) && !rendered.contains(&shipped.endpoint),
                "no catalog arrived, so no backend may be named:\n{rendered}"
            );
        }
    }

    /// The whole walk: the key reaches the keychain, and only its **reference**
    /// reaches the wire (AC-5, ADR-3).
    #[test]
    fn a_full_walk_stores_the_key_and_sends_only_its_reference() {
        let mut io = FakeIo::new(FULL_WALK);
        let keychain = MockKeychain::new();
        drive(&mut io, &keychain, &session(), Gate::Walk).unwrap();

        assert_eq!(
            keychain.stored_secret(SEARCH_KEY_ACCOUNT).as_deref(),
            Some(PLANTED_KEY)
        );
        assert!(
            keychain.deletes().is_empty(),
            "a successful commit takes nothing back out"
        );
        assert_eq!(io.commits.len(), 1, "exactly one commit");
        assert_eq!(
            io.commits[0].search_key_ref.as_deref(),
            Some("keychain://teton/web-search")
        );
        assert_eq!(io.commits[0].tier, WebTier::Search);
        assert_eq!(io.commits[0].search_auth.as_deref(), Some(TYPED_AUTH));
        // The preview carried the same reference, which is what makes the bytes
        // the user confirmed the bytes the commit writes.
        assert_eq!(io.previews[0].search_key_ref, io.commits[0].search_key_ref);

        // The sweep: not in any frame, and not on the screen either.
        let frames = io.frames();
        assert!(frames.contains("keychain://teton/web-search"), "{frames}");
        assert!(
            !frames.contains(PLANTED_KEY),
            "the credential crossed the socket: {frames}"
        );
        assert!(
            !io.rendered().contains(PLANTED_KEY),
            "the credential was echoed back to the screen"
        );
        // And it was asked for through the hiding path, not the echoing one.
        assert_eq!(io.prompter.secrets.len(), 1);
        assert!(io.prompter.secrets[0].contains("API key"));

        // A successful commit renders nothing of its own: the daemon's
        // `web_setup_completed` event is the completion notice.
        assert!(
            !io.rendered().contains("already configured"),
            "the unchanged-line must not fire on an applied commit"
        );
    }

    /// AC-6: aborting at *any* prompt leaves the keychain empty and sends no
    /// commit. Driven by truncating the script one answer at a time, so a prompt
    /// added later is covered the moment its answer joins the list.
    #[test]
    fn an_abort_at_every_prompt_stores_nothing_and_commits_nothing() {
        for stop in 0..FULL_WALK.len() {
            let script: Vec<&str> = FULL_WALK[..stop].to_vec();
            let mut io = FakeIo::new(&script);
            let keychain = MockKeychain::new();
            drive(&mut io, &keychain, &session(), Gate::Walk).unwrap();

            assert!(
                keychain.is_empty(),
                "an abort after {stop} answer(s) left a credential behind"
            );
            assert!(
                keychain.deletes().is_empty(),
                "nothing was stored, so nothing should have been deleted ({stop})"
            );
            assert!(
                io.commits.is_empty(),
                "an abort after {stop} answer(s) committed anyway"
            );
        }

        // The same for an empty answer at each question rather than EOF, which is
        // the other way a user backs out.
        for stop in 0..FULL_WALK.len() {
            let mut script: Vec<&str> = FULL_WALK[..stop].to_vec();
            script.push("");
            let mut io = FakeIo::new(&script);
            let keychain = MockKeychain::new();
            drive(&mut io, &keychain, &session(), Gate::Walk).unwrap();
            assert!(keychain.is_empty(), "empty answer {stop} stored something");
            assert!(io.commits.is_empty(), "empty answer {stop} committed");
        }
    }

    /// The confirm is default-no: an empty answer declines, and the store never
    /// happens (LESSON-470 — the write is the costly wrong answer).
    #[test]
    fn the_confirm_defaults_to_no_and_only_an_explicit_yes_writes() {
        for (answer, writes) in [
            ("y", true),
            ("yes", true),
            ("Y", true),
            ("n", false),
            ("", false),
            ("sure", false),
        ] {
            let mut script: Vec<&str> = FULL_WALK[..FULL_WALK.len() - 1].to_vec();
            script.push(answer);
            let mut io = FakeIo::new(&script);
            let keychain = MockKeychain::new();
            drive(&mut io, &keychain, &session(), Gate::Walk).unwrap();

            assert_eq!(
                !keychain.is_empty(),
                writes,
                "confirm answer {answer:?} stored the wrong thing"
            );
            assert_eq!(
                !io.commits.is_empty(),
                writes,
                "confirm answer {answer:?} committed the wrong thing"
            );
            // A preview was still shown either way — declining is a decision
            // about bytes the user was allowed to read first.
            assert_eq!(io.previews.len(), 1);
        }
    }

    /// AC-6's other half: a commit the daemon refuses takes the stored key back
    /// out and shows the daemon's own sentence.
    #[test]
    fn a_refused_commit_deletes_the_key_it_stored_and_renders_the_daemons_sentence() {
        let mut io = FakeIo::new(FULL_WALK);
        io.commit = Err(RpcError::new(
            error_code::WEB_SETUP_INVALID,
            "web.search_endpoint must be an absolute http(s) URL",
        ));
        let keychain = MockKeychain::new();
        drive(&mut io, &keychain, &session(), Gate::Walk).unwrap();

        assert_eq!(
            keychain.deletes(),
            vec![SEARCH_KEY_ACCOUNT.to_owned()],
            "the entry written for this commit must be taken back out"
        );
        assert!(
            keychain.is_empty(),
            "the delete must have actually removed it"
        );
        let rendered = io.rendered();
        assert!(
            rendered.contains("web.search_endpoint must be an absolute http(s) URL"),
            "the validator's own sentence must reach the user: {rendered}"
        );
        assert!(rendered.contains("nothing was written"), "{rendered}");
        assert!(!rendered.contains(PLANTED_KEY), "{rendered}");
    }

    /// The credential a **rotation** displaces belongs to the live config, and a
    /// refused commit puts it back rather than deleting it.
    ///
    /// This is BR-11 read exactly: the undo removes "any keychain entry the
    /// aborted flow run itself created", and a run that overwrote an existing
    /// entry created none. Deleting here destroys a working setup — the user
    /// tried to rotate a key, the daemon refused the new table, and they are
    /// left with a `search_key_ref` pointing at nothing.
    #[test]
    fn a_refused_commit_after_a_rotation_puts_the_previous_key_back() {
        let mut io = FakeIo::new(FULL_WALK);
        io.commit = Err(RpcError::new(
            error_code::WEB_SETUP_INVALID,
            "web.search_endpoint must be an absolute http(s) URL",
        ));
        let keychain = MockKeychain::new();
        keychain.store(SEARCH_KEY_ACCOUNT, PREVIOUS_KEY).unwrap();

        drive(&mut io, &keychain, &session(), Gate::Walk).unwrap();

        assert_eq!(
            keychain.stored_secret(SEARCH_KEY_ACCOUNT).as_deref(),
            Some(PREVIOUS_KEY),
            "the credential the live config references must survive a refused rotation"
        );
        assert!(
            keychain.deletes().is_empty(),
            "an entry this run did not create must not be deleted"
        );
        let rendered = io.rendered();
        assert!(
            rendered.contains("the one that was there before"),
            "the user must be told a restore happened, not a removal: {rendered}"
        );
        assert!(
            !rendered.contains(PLANTED_KEY) && !rendered.contains(PREVIOUS_KEY),
            "neither credential may be rendered: {rendered}"
        );
    }

    /// The other half of the same decision: with nothing there before, the undo
    /// really is a delete, and the line says so.
    #[test]
    fn a_refused_commit_on_a_fresh_account_deletes_and_says_removed() {
        let mut io = FakeIo::new(FULL_WALK);
        io.commit = Err(RpcError::new(error_code::WEB_SETUP_INVALID, "no."));
        let keychain = MockKeychain::new();

        drive(&mut io, &keychain, &session(), Gate::Walk).unwrap();

        assert!(keychain.is_empty(), "a fresh entry must be taken back out");
        assert_eq!(keychain.deletes(), vec![SEARCH_KEY_ACCOUNT.to_owned()]);
        let rendered = io.rendered();
        assert!(
            rendered.contains("has been removed from your keychain"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("the one that was there before"),
            "nothing was displaced, so nothing was restored: {rendered}"
        );
    }

    /// A store that could not find out what it was displacing does **neither**
    /// undo, and says which of the two it declined to guess at.
    #[test]
    fn a_refused_commit_after_an_unreadable_keychain_leaves_the_entry_alone() {
        let mut io = FakeIo::new(FULL_WALK);
        io.commit = Err(RpcError::new(error_code::WEB_SETUP_INVALID, "no."));
        let keychain = MockKeychain::new();
        keychain.fail_read_with("the keychain is locked");

        drive(&mut io, &keychain, &session(), Gate::Walk).unwrap();

        assert!(
            keychain.deletes().is_empty(),
            "a delete against an unknown prior state is the destructive guess"
        );
        assert_eq!(
            keychain.stored_secret(SEARCH_KEY_ACCOUNT).as_deref(),
            Some(PLANTED_KEY),
            "the store still happened; it is the undo that was declined"
        );
        let rendered = io.rendered();
        assert!(
            rendered.contains("could not be read") && rendered.contains("the keychain is locked"),
            "the reason must reach the user: {rendered}"
        );
        assert!(
            rendered.contains("security find-generic-password"),
            "and so must the way to look: {rendered}"
        );
    }

    /// FIX 3's case: the daemon refuses **and** the cleanup the refusal triggers
    /// fails too. Both facts are the user's, and only the second comes with
    /// something they can do about it.
    #[test]
    fn a_refused_commit_whose_own_cleanup_also_fails_reports_both_failures() {
        let mut io = FakeIo::new(FULL_WALK);
        io.commit = Err(RpcError::new(
            error_code::WEB_SETUP_INVALID,
            "web.search_endpoint must be an absolute http(s) URL",
        ));
        let keychain = MockKeychain::new();
        keychain.fail_delete_with("the keychain is locked");

        drive(&mut io, &keychain, &session(), Gate::Walk).unwrap();

        // The attempt was made and refused, so the entry is still there — which
        // is precisely why the user has to be told.
        assert_eq!(keychain.deletes(), vec![SEARCH_KEY_ACCOUNT.to_owned()]);
        assert_eq!(
            keychain.stored_secret(SEARCH_KEY_ACCOUNT).as_deref(),
            Some(PLANTED_KEY)
        );

        let rendered = io.rendered();
        assert!(
            rendered.contains("web.search_endpoint must be an absolute http(s) URL"),
            "the daemon's own refusal must still reach the user: {rendered}"
        );
        assert!(
            rendered.contains("could not be removed from your keychain")
                && rendered.contains("the keychain is locked"),
            "the cleanup failure must be reported, not swallowed: {rendered}"
        );
        assert!(
            rendered.contains("security delete-generic-password -s teton -a web-search"),
            "the user is the only one who can finish this, so they get the command: {rendered}"
        );
        assert!(!rendered.contains(PLANTED_KEY), "{rendered}");
    }

    /// FIX 2: a commit the daemon never *answered* ends the command, not the
    /// session, and touches the keychain not at all.
    ///
    /// The write may have landed. A delete would then orphan a live config, and
    /// a restore would resurrect the key the user was replacing — so both undos
    /// are the wrong one half the time, and the honest move is to say so and
    /// name the command that resolves it.
    #[test]
    fn a_commit_that_never_answered_leaves_the_keychain_alone_and_says_so() {
        let mut io = FakeIo::new(FULL_WALK);
        io.commit_transport_error = Some("connection reset by peer");
        let keychain = MockKeychain::new();

        // The whole point: `Ok`, so the session survives to run the next command.
        drive(&mut io, &keychain, &session(), Gate::Walk)
            .expect("a transport failure must end the command, never the session");

        assert_eq!(io.commits.len(), 1, "the frame did go out");
        assert_eq!(
            keychain.stored_secret(SEARCH_KEY_ACCOUNT).as_deref(),
            Some(PLANTED_KEY),
            "the store stands; nothing beyond it may have happened"
        );
        assert!(
            keychain.deletes().is_empty(),
            "no undo may be guessed at on an unknown outcome"
        );

        let rendered = io.rendered();
        assert!(
            rendered.contains("connection reset by peer"),
            "the transport failure itself must be named: {rendered}"
        );
        assert!(
            rendered.contains("may or may not have been written"),
            "the ambiguity is the fact: {rendered}"
        );
        assert!(
            rendered.contains("web-search"),
            "the notice must name the account the key sits in: {rendered}"
        );
        assert!(
            rendered.contains("/web setup"),
            "and the command that shows the current state: {rendered}"
        );
        assert!(!rendered.contains(PLANTED_KEY), "{rendered}");
    }

    /// The same unanswered commit over a **rotation**: still no mutation, and the
    /// notice says which key is now in the account rather than implying the old
    /// one survived.
    #[test]
    fn an_unanswered_commit_after_a_rotation_says_the_previous_key_was_replaced() {
        let mut io = FakeIo::new(FULL_WALK);
        io.commit_transport_error = Some("connection reset by peer");
        let keychain = MockKeychain::new();
        keychain.store(SEARCH_KEY_ACCOUNT, PREVIOUS_KEY).unwrap();

        drive(&mut io, &keychain, &session(), Gate::Walk).unwrap();

        assert_eq!(
            keychain.stored_secret(SEARCH_KEY_ACCOUNT).as_deref(),
            Some(PLANTED_KEY)
        );
        assert!(keychain.deletes().is_empty());
        let rendered = io.rendered();
        assert!(
            rendered.contains("in place of the one that was there before"),
            "{rendered}"
        );
        // No delete instruction here: the previous key is already gone, so
        // removing the entry would leave a rotation half-done either way.
        assert!(
            !rendered.contains("security delete-generic-password"),
            "a rotation has no clean removal to suggest: {rendered}"
        );
    }

    /// A refused **preview** never gets as far as the keychain: the store is
    /// after the confirm, and there is nothing to confirm.
    #[test]
    fn a_refused_preview_asks_for_no_confirmation_and_stores_nothing() {
        let mut io = FakeIo::new(FULL_WALK);
        io.preview = Err(RpcError::new(
            error_code::WEB_SETUP_INVALID,
            "this machine cannot serve the `search` tier: search needs the local model",
        ));
        let keychain = MockKeychain::new();
        drive(&mut io, &keychain, &session(), Gate::Walk).unwrap();

        assert!(keychain.is_empty() && keychain.deletes().is_empty());
        assert!(io.commits.is_empty());
        assert!(
            io.rendered().contains("search needs the local model"),
            "{}",
            io.rendered()
        );
    }

    /// AC-7 at the client: the menu marks the unavailable tier with the daemon's
    /// reason, and selecting it is refused before any RPC.
    #[test]
    fn search_is_marked_unavailable_and_its_selection_is_refused() {
        let mut io = FakeIo::new(&["3"]);
        io.plan = Ok(plan_without_search());
        let keychain = MockKeychain::new();
        drive(&mut io, &keychain, &session(), Gate::Walk).unwrap();

        let rendered = io.rendered();
        assert!(
            rendered.contains("(unavailable: search needs the local model)"),
            "the menu entry must carry the reason: {rendered}"
        );
        assert!(
            rendered.contains("`search` cannot be enabled on this machine"),
            "{rendered}"
        );
        assert!(
            io.previews.is_empty(),
            "a refused tier must not reach the daemon"
        );
        assert!(keychain.is_empty());
        // The other tiers still work on the same machine.
        let mut io = FakeIo::new(&["2", "y"]);
        io.plan = Ok(plan_without_search());
        io.commit = Ok(WebSetupCommitResult {
            applied: true,
            tier: WebTier::FetchAnyUrl,
        });
        drive(&mut io, &keychain, &session(), Gate::Walk).unwrap();
        assert_eq!(io.commits.len(), 1);
        assert_eq!(io.commits[0].tier, WebTier::FetchAnyUrl);
        assert!(io.commits[0].search_endpoint.is_none());
        assert!(
            keychain.is_empty(),
            "a fetch tier collects no credential at all"
        );
    }

    /// A keyless backend is a first-class answer: no key prompt, no keychain
    /// write, and no `search_key_ref` on the wire (AC-8's SearxNG shape).
    #[test]
    fn a_keyless_backend_stores_nothing_and_sends_no_reference() {
        let mut io = FakeIo::new(&["3", "http://localhost:8888/search?format=json", "n", "y"]);
        let keychain = MockKeychain::new();
        drive(&mut io, &keychain, &session(), Gate::Walk).unwrap();

        assert_eq!(io.commits.len(), 1);
        assert!(io.commits[0].search_key_ref.is_none());
        assert!(io.commits[0].search_auth.is_none());
        assert_eq!(
            io.commits[0].search_endpoint.as_deref(),
            Some("http://localhost:8888/search?format=json")
        );
        assert!(
            keychain.is_empty(),
            "a keyless backend writes no credential"
        );
        assert!(
            io.prompter.secrets.is_empty(),
            "no key prompt should have been drawn"
        );
    }

    /// The preview is the daemon's bytes. Nothing here composes a `[web]` table,
    /// so the confirm step cannot show a candidate the commit would not write
    /// (BR-7, LESSON-494).
    #[test]
    fn the_preview_renders_the_daemons_own_bytes_and_host() {
        let preview = preview_result();
        let lines = preview_lines(&preview);
        let rendered = lines
            .iter()
            .map(|(_, text)| text.trim().to_owned())
            .collect::<Vec<_>>()
            .join("\n");
        for line in preview.toml.lines() {
            assert!(
                rendered.contains(line),
                "the previewed bytes must appear verbatim: {rendered}"
            );
        }
        assert!(rendered.contains("example.test"), "{rendered}");
        assert!(
            rendered.contains("search_auth will be removed"),
            "warnings are the daemon's too: {rendered}"
        );
    }

    /// A daemon too old to serve the flow says so as a version fact, and asks
    /// nothing (BUG-152's class).
    #[test]
    fn a_daemon_without_the_method_says_so_and_asks_nothing() {
        let mut io = FakeIo::new(FULL_WALK);
        io.plan = Err(RpcError::new(
            error_code::METHOD_NOT_FOUND,
            "no such method",
        ));
        let keychain = MockKeychain::new();
        drive(&mut io, &keychain, &session(), Gate::Walk).unwrap();

        assert_eq!(io.prompter.asked, 0);
        assert!(io.previews.is_empty() && io.commits.is_empty());
        assert!(keychain.is_empty());
        assert!(
            io.surface.lines_of(LineKind::Error).is_empty(),
            "a version fact must not wear an error prefix"
        );
    }

    /// A commit that changed nothing says so, because no event announces a
    /// change that did not happen — and it says it about the **config**, which
    /// is the only thing `applied: false` is a fact about.
    ///
    /// The keychain is the other half of this setup and it did move: the same
    /// table with a rotated key is exactly the case where the commit applies
    /// nothing and the credential behind it is new. "Nothing changed" there is
    /// false, and sends a user who just rotated a key looking for the reason it
    /// did not take.
    #[test]
    fn a_commit_that_applied_nothing_says_which_half_of_the_setup_moved() {
        // A key was stored: the config is unchanged, the credential is not.
        let mut io = FakeIo::new(FULL_WALK);
        io.commit = Ok(WebSetupCommitResult {
            applied: false,
            tier: WebTier::Search,
        });
        let keychain = MockKeychain::new();
        keychain.store(SEARCH_KEY_ACCOUNT, PREVIOUS_KEY).unwrap();
        drive(&mut io, &keychain, &session(), Gate::Walk).unwrap();

        let rendered = io.rendered();
        assert!(
            rendered.contains("already configured exactly this way"),
            "{rendered}"
        );
        assert!(
            rendered.contains("your config is unchanged")
                && rendered.contains("was updated to the one you just typed"),
            "a rotation against an unchanged table must not be reported as a no-op: {rendered}"
        );
        assert!(
            !rendered.contains("so nothing changed"),
            "that sentence is false of the keychain here: {rendered}"
        );
        assert_eq!(
            keychain.stored_secret(SEARCH_KEY_ACCOUNT).as_deref(),
            Some(PLANTED_KEY),
            "and the new key really is what the account now holds"
        );

        // No key at all (a fetch tier): nothing changed anywhere, and the line
        // is the plain one.
        let mut io = FakeIo::new(&["2", "y"]);
        io.commit = Ok(WebSetupCommitResult {
            applied: false,
            tier: WebTier::FetchAnyUrl,
        });
        let keychain = MockKeychain::new();
        drive(&mut io, &keychain, &session(), Gate::Walk).unwrap();
        let rendered = io.rendered();
        assert!(
            rendered.contains("so nothing changed"),
            "with no key collected, nothing changed is the whole truth: {rendered}"
        );
        assert!(
            !rendered.contains("was updated to the one you just typed"),
            "no key was collected, so no key was updated: {rendered}"
        );
        // The line itself, away from the preview fixture's own `keychain://`
        // reference: with nothing collected it does not mention the store.
        assert!(
            !unchanged_line(WebTier::FetchAnyUrl, &None).contains("keychain"),
            "{}",
            unchanged_line(WebTier::FetchAnyUrl, &None)
        );
    }

    /// AC-8's trap, closed: the walkthrough offers a suggested backend's *own*
    /// header when the user types that backend's endpoint, and pressing Enter
    /// sends that header on the wire.
    ///
    /// The generic template against a backend that wants its own is a 401 the
    /// user reads as a bad key — a suggestion list that names the right header in
    /// one line and offers the wrong one at the next prompt is worse than no
    /// suggestion. Which backend it is, the catalog decides.
    #[test]
    fn a_known_backend_is_offered_its_own_auth_header_and_enter_takes_it() {
        let catalog = shipped_catalog();
        let entry = catalog
            .backends
            .iter()
            .find(|entry| entry.host.is_some())
            .expect("the shipped catalog names a backend with a host");
        let template = entry
            .auth_template
            .as_deref()
            .expect("a hosted suggestion names the header it wants");
        let mut io = FakeIo::new(&[
            "3",
            &entry.endpoint,
            "y",
            "", // Enter: take what was offered.
            PLANTED_KEY,
            "y",
        ]);
        let keychain = MockKeychain::new();
        drive(&mut io, &keychain, &session(), Gate::Walk).unwrap();

        assert!(
            io.prompter.any_question_contains(template),
            "the offer must be stated in the prompt, since Enter is how it is taken: {:?}",
            io.prompter.questions
        );
        assert_eq!(io.commits.len(), 1);
        assert_eq!(
            io.commits[0].search_auth.as_deref(),
            Some(template),
            "an empty answer must put the offered template on the wire, not the generic default"
        );

        // An unknown host is unchanged: the generic template is offered, and an
        // empty answer still means "no key at all", so the daemon's one default
        // stays one value in one place.
        let mut io = FakeIo::new(&[
            "3",
            "https://example.test/search",
            "y",
            "",
            PLANTED_KEY,
            "y",
        ]);
        let keychain = MockKeychain::new();
        drive(&mut io, &keychain, &session(), Gate::Walk).unwrap();
        assert!(
            io.prompter
                .any_question_contains(&catalog.default_auth_template),
            "{:?}",
            io.prompter.questions
        );
        assert_eq!(
            io.commits[0].search_auth, None,
            "the generic default is the daemon's to apply, not this flow's to copy"
        );
    }

    /// The offer is decided from the endpoint's host against the catalog the
    /// daemon sent, and everything it cannot read falls back to that catalog's
    /// own default — the behaviour that was there when the table lived in this
    /// file, now a property of the data.
    ///
    /// Driven over both catalogs, so what is asserted is the rule rather than
    /// the three backends that happen to be shipped today.
    #[test]
    fn the_offered_auth_header_follows_the_endpoints_host() {
        for catalog in [shipped_catalog(), sentinel_catalog()] {
            let hosted: Vec<_> = catalog
                .backends
                .iter()
                .filter(|entry| entry.host.is_some())
                .collect();
            assert!(
                !hosted.is_empty(),
                "the fixture must carry a backend there is a host to match"
            );
            for entry in hosted {
                let host = entry.host.as_deref().expect("filtered on it");
                let template = entry
                    .auth_template
                    .as_deref()
                    .expect("a hosted suggestion names the header it wants");
                for endpoint in [
                    entry.endpoint.clone(),
                    // Case and port are not part of the answer.
                    format!("https://{}/some/path", host.to_uppercase()),
                    format!("https://{host}:443/some/path?q=x"),
                ] {
                    assert_eq!(
                        offered_auth(&endpoint, Some(&catalog)),
                        template,
                        "{endpoint}"
                    );
                }
                for endpoint in [
                    // A host that merely *contains* a catalogued one is not it.
                    format!("https://{host}.evil.test/some/path"),
                    // Nor is one behind a scheme the parse does not read.
                    format!("ftp://{host}/x"),
                ] {
                    assert_eq!(
                        offered_auth(&endpoint, Some(&catalog)),
                        catalog.default_auth_template,
                        "{endpoint}"
                    );
                }
            }

            // A keyless suggestion has no host to be recognised by, so its own
            // endpoint takes the default too — as does anything unparseable.
            let keyless = catalog
                .backends
                .iter()
                .find(|entry| entry.host.is_none())
                .expect("the fixture must carry a self-hosted suggestion");
            for endpoint in [
                keyless.endpoint.as_str(),
                "https://example.test/search",
                "not-a-url",
                "",
            ] {
                assert_eq!(
                    offered_auth(endpoint, Some(&catalog)),
                    catalog.default_auth_template,
                    "{endpoint}"
                );
            }
        }

        // The host parse itself, including the parts that are not the host.
        assert_eq!(
            endpoint_host("https://user:pw@host.example:443/api/v0/search#frag"),
            Some("host.example")
        );
        assert_eq!(endpoint_host("https:///search"), None);
        assert_eq!(endpoint_host("host.example/api"), None);
    }

    /// Every template this flow offers is one the list it just drew actually
    /// names — which used to be a promise between two constants kept side by
    /// side and is now a property of whatever a daemon sends.
    #[test]
    fn every_offered_template_belongs_to_a_backend_the_help_names() {
        for catalog in [shipped_catalog(), sentinel_catalog()] {
            let help = suggestion_lines(Some(&catalog)).join("\n");
            for entry in &catalog.backends {
                assert!(
                    help.contains(&entry.endpoint),
                    "`{}` is a suggestion the user is never shown:\n{help}",
                    entry.endpoint
                );
                let Some(host) = &entry.host else { continue };
                let offered = offered_auth(&entry.endpoint, Some(&catalog));
                assert!(
                    help.contains(&offered),
                    "`{offered}` is offered for {host} and named nowhere the user can read it:\n\
                     {help}"
                );
            }
        }
    }

    /// **AC-2: every backend fact the walkthrough shows came off the wire.**
    ///
    /// Fed a catalog nothing would ever ship, the help lines, the offer at the
    /// auth prompt and the piped instructions all say the sentinel's words — and
    /// the shipped backends appear nowhere, which is what proves this file kept
    /// no list of its own (BR-1).
    #[test]
    fn the_suggestions_the_walkthrough_shows_are_the_daemons_catalog() {
        let catalog = sentinel_catalog();
        let mut io = FakeIo::new(&[
            "3",
            "https://sentinel.example/search",
            "y",
            "", // Enter: take the offer, which must be the sentinel's header.
            PLANTED_KEY,
            "y",
        ]);
        io.plan = Ok(WebSetupPlanResult {
            suggestion_catalog: Some(catalog.clone()),
            ..plan_ready_for_search()
        });
        let keychain = MockKeychain::new();
        drive(&mut io, &keychain, &session(), Gate::Walk).unwrap();

        let rendered = io.rendered();
        for needle in [
            "Sentinel Keyless",
            "http://localhost:9999/sentinel?format=json",
            "(no key)",
            "Sentinel Backend With A Long Label",
            "https://sentinel.example/search",
            "X-Sentinel-Header: {key}",
            "a sentinel note",
        ] {
            assert!(
                rendered.contains(needle),
                "`{needle}` was sent and never shown:\n{rendered}"
            );
        }
        for shipped in shipped_catalog().backends {
            assert!(
                !rendered.contains(&shipped.label),
                "`{}` came from nowhere but this file:\n{rendered}",
                shipped.label
            );
        }

        // The offer at the auth prompt is the matched entry's own header, and
        // Enter puts it on the wire rather than a default it would 401 to.
        assert!(
            io.prompter
                .any_question_contains("X-Sentinel-Header: {key}"),
            "{:?}",
            io.prompter.questions
        );
        assert_eq!(
            io.commits[0].search_auth.as_deref(),
            Some("X-Sentinel-Header: {key}")
        );

        // An endpoint the catalog does not name gets the catalog's *own*
        // default, and an empty answer to it still means "no `search_auth` key".
        let mut io = FakeIo::new(&[
            "3",
            "https://unmatched.example/search",
            "y",
            "",
            PLANTED_KEY,
            "y",
        ]);
        io.plan = Ok(WebSetupPlanResult {
            suggestion_catalog: Some(catalog.clone()),
            ..plan_ready_for_search()
        });
        let keychain = MockKeychain::new();
        drive(&mut io, &keychain, &session(), Gate::Walk).unwrap();
        assert!(
            io.prompter
                .any_question_contains("X-Sentinel-Default: {key}"),
            "{:?}",
            io.prompter.questions
        );
        assert_eq!(
            io.commits[0].search_auth, None,
            "the catalog's default is the daemon's to apply, not this flow's to copy"
        );

        // And the piped path names the same data: the first suggestion as the
        // example endpoint, and the catalog's default as the header default.
        let instructions = rendered_instructions(Some(&catalog));
        assert!(
            instructions.contains(&catalog.backends[0].endpoint),
            "{instructions}"
        );
        assert!(
            instructions.contains(&catalog.default_auth_template),
            "{instructions}"
        );
    }

    /// A catalog entry that contradicts the protocol's shape degrades — it does
    /// not steer the row it draws, and it does not shadow a well-formed entry for
    /// the same host.
    ///
    /// The list arrives over RPC, so "no daemon would send that" is a statement
    /// about the daemon this client happens to be talking to and not about the
    /// wire. Three degradations, each of which used to have a sharper failure:
    ///
    /// - a backend that wants a key and names no header reads as needing one,
    ///   and the offer falls through to the catalog's own default;
    /// - a header beside `needs_key: false` does not make the row claim a
    ///   credential the entry says it does not want;
    /// - a template-less entry no longer wins the host match ahead of the entry
    ///   that names the header — which would have swapped that backend's real
    ///   header for the generic default, and produced a config it answers 401 to.
    #[test]
    fn a_malformed_catalog_entry_neither_steers_its_row_nor_shadows_a_real_one() {
        let catalog = malformed_catalog();
        let lines = suggestion_lines(Some(&catalog));
        let row = |label: &str| {
            lines
                .iter()
                .find(|line| line.contains(label))
                .unwrap_or_else(|| panic!("`{label}` is a suggestion nobody is shown:\n{lines:#?}"))
        };
        assert!(
            row("Sentinel Headerless").ends_with("(needs a key)"),
            "a backend that wants a key and names no header still says so: {:?}",
            row("Sentinel Headerless")
        );
        assert!(
            row("Sentinel Vestigial").ends_with("(no key)"),
            "`needs_key` decides whether there is a credential, not the template: {:?}",
            row("Sentinel Vestigial")
        );
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("X-Sentinel-Vestigial")),
            "a header on a keyless entry is not a header the row may advertise:\n{lines:#?}"
        );

        // A host matched to an entry with nothing to offer takes the catalog's
        // default — the same answer a host nobody claimed gets.
        assert_eq!(
            offered_auth("https://headerless.sentinel.example/search", Some(&catalog)),
            catalog.default_auth_template
        );

        // And the shadowing case: the template-less entry for this host is first
        // in the list, and the one that names a header still wins.
        for endpoint in [
            "https://shadow.sentinel.example/search",
            "https://shadow.sentinel.example/v1/search",
        ] {
            assert_eq!(
                offered_auth(endpoint, Some(&catalog)),
                "X-Sentinel-Shadowed: {key}",
                "{endpoint}"
            );
        }
    }

    /// **An empty catalog is its own wire state, and it is not `None`.**
    ///
    /// A daemon that sent `suggestion_catalog: {"backends": [], ...}` is not a
    /// daemon that predates the field: it declared a default header and named no
    /// backend. So the two states agree about the *list* — there is nothing to
    /// show, and both collapse to the bare preamble — and disagree about the
    /// *default*, where the present catalog's own value wins over the shared
    /// protocol constant.
    ///
    /// **That asymmetry is deliberate.** A catalog is the daemon's declaration,
    /// and `default_auth_template` is a field of it: a daemon that sends one is
    /// authoritative about the header its own validator expects, whether or not
    /// it also had backends to suggest. Only the `None` case — no catalog at all
    /// — is the client's to fill, and it fills it with the constant both binaries
    /// share (ADR-B). Pinned here because "empty list" and "no catalog" look
    /// alike enough that collapsing them would be an easy, silent edit.
    #[test]
    fn an_empty_catalog_shows_no_backend_and_still_declares_its_own_default() {
        let empty = WebSetupCatalog {
            default_auth_template: "X-Sentinel-Default: {key}".to_owned(),
            backends: Vec::new(),
        };
        assert_ne!(
            empty.default_auth_template, GENERIC_SEARCH_AUTH_TEMPLATE,
            "the fixture is only worth anything while the two differ"
        );

        // The list half: identical to the no-catalog path, down to the bytes.
        assert_eq!(suggestion_lines(Some(&empty)), suggestion_lines(None));
        assert_eq!(suggestion_lines(Some(&empty)), [ENDPOINT_HELP_BARE]);

        // The default half: the daemon's declaration, not the shared constant.
        assert_eq!(default_auth(Some(&empty)), empty.default_auth_template);
        assert_eq!(
            offered_auth("https://anything.example/search", Some(&empty)),
            empty.default_auth_template
        );

        // And the piped instructions say both halves: no example endpoint to
        // name, and the catalog's own header default.
        let instructions = rendered_instructions(Some(&empty));
        assert!(
            !instructions.contains("e.g."),
            "an empty catalog has no first entry to hold up as an example:\n{instructions}"
        );
        assert!(
            instructions.contains(&empty.default_auth_template)
                && !instructions.contains(GENERIC_SEARCH_AUTH_TEMPLATE),
            "the declared default is the one shown:\n{instructions}"
        );
    }

    /// **AC-7/BR-3: a daemon that predates the catalog is not a broken
    /// walkthrough.**
    ///
    /// The list nobody sent is not invented, the offer degrades to the shared
    /// protocol constant rather than to a literal spelled out here, the
    /// needs-a-key default is still yes, and the walk still writes.
    #[test]
    fn a_daemon_that_sends_no_catalog_still_walks_with_the_generic_offer() {
        let mut io = FakeIo::new(&[
            "3",
            "https://example.test/search",
            "", // Enter at "does this backend need an API key?": still yes.
            "", // Enter at the auth question: take the generic offer.
            PLANTED_KEY,
            "y",
        ]);
        io.plan = Ok(plan_without_a_catalog());
        let keychain = MockKeychain::new();
        drive(&mut io, &keychain, &session(), Gate::Walk).unwrap();

        let rendered = io.rendered();
        assert!(
            rendered.contains(ENDPOINT_HELP_BARE),
            "the endpoint is still asked for:\n{rendered}"
        );
        for shipped in shipped_catalog().backends {
            assert!(
                !rendered.contains(&shipped.label),
                "no catalog arrived, so no backend may be named:\n{rendered}"
            );
        }
        assert!(
            io.prompter
                .any_question_contains(GENERIC_SEARCH_AUTH_TEMPLATE),
            "the degraded offer is the protocol's own constant: {:?}",
            io.prompter.questions
        );

        // The flow completed: the key was collected because the default answer
        // to the key question is yes, stored, and only its reference sent.
        assert_eq!(
            keychain.stored_secret(SEARCH_KEY_ACCOUNT).as_deref(),
            Some(PLANTED_KEY),
            "an empty answer to the key question must still ask for a key"
        );
        assert_eq!(io.commits.len(), 1);
        assert_eq!(
            io.commits[0].search_key_ref.as_deref(),
            Some("keychain://teton/web-search")
        );
        assert_eq!(
            io.commits[0].search_auth, None,
            "the generic default is the daemon's to apply"
        );

        // The piped path degrades the same way: the field is named, no example
        // is invented for it, and the header default is the shared constant.
        let instructions = rendered_instructions(None);
        assert!(
            instructions.contains(GENERIC_SEARCH_AUTH_TEMPLATE),
            "{instructions}"
        );
        assert!(
            !instructions.contains("e.g."),
            "no catalog means no example endpoint to name:\n{instructions}"
        );
    }

    /// **AC-6: the wording did not move.** Fed the catalog the daemon ships, the
    /// walkthrough renders the same lines byte for byte — the suggestions a user
    /// reads and the instructions a piped session gets are the same strings they
    /// were when this file held the list itself.
    ///
    /// The baseline is **REQ-572, not a release**: `web/setup_plan` and this
    /// walkthrough first shipped there and are still unreleased, so the wording
    /// pinned below has never been in a user's hands under a version number. It
    /// is named for the REQ that wrote it rather than for v0.1.14, which predates
    /// it.
    ///
    /// The literals are the pin, captured from that implementation. They are test
    /// data and nothing reads them but this assertion, which is what makes the
    /// refactor provable rather than merely plausible.
    #[test]
    fn the_shipped_catalog_renders_the_req_572_wording_byte_for_byte() {
        assert_eq!(
            suggestion_lines(Some(&shipped_catalog())),
            [
                "name the search backend to query. Shapes that are known to work:",
                "  self-hosted SearxNG  http://localhost:8888/search?format=json  (no key)",
                "  Brave Search API     https://api.search.brave.com/res/v1/web/search  \
                 (header `X-Subscription-Token: {key}`)",
                "  Kagi Search API      https://kagi.com/api/v0/search  \
                 (header `Authorization: Bot {key}`)",
            ]
        );

        // A note the daemon chooses to send rides *under* its entry, so the rows
        // above stay the rows pinned here. No entry the daemon ships today
        // carries a note — this is the shape a note a daemon *may* send takes,
        // and AC-6 is about the row, not the count.
        let mut annotated = shipped_catalog();
        let note = "a note the daemon added";
        annotated.backends[0].notes = Some(note.to_owned());
        let annotated = suggestion_lines(Some(&annotated));
        assert_eq!(
            annotated
                .iter()
                .filter(|line| !line.contains(note))
                .cloned()
                .collect::<Vec<_>>(),
            suggestion_lines(Some(&shipped_catalog())),
            "a note may add a line and must not touch one"
        );
        let note_line = annotated
            .iter()
            .find(|line| line.contains(note))
            .expect("the note is shown at all");
        let column = suggestion_lines(Some(&shipped_catalog()))[1]
            .find("http://")
            .expect("the row names its endpoint");
        assert_eq!(
            *note_line,
            format!("{}{note}", " ".repeat(column)),
            "the note lines up under the endpoint column"
        );

        let instructions: Vec<String> = instruction_lines(Some(&shipped_catalog()))
            .into_iter()
            .map(|(_, text)| text)
            .collect();
        assert_eq!(
            instructions,
            [
                "`/web setup` collects an endpoint and a key without echoing it, which needs a \
                 terminal — this session's input is not one, so nothing was read and nothing was \
                 changed.",
                "run `teton` in a terminal and type `/web setup`, or write the `[web]` table into \
                 your config yourself:",
                "  tier             \"fetch_user_url\" | \"fetch_any_url\" | \"search\" (each \
                 includes the ones before it)",
                "  search_endpoint  the backend the `search` tier queries, e.g. \
                 http://localhost:8888/search?format=json",
                "  search_key_ref   a keychain reference — `keychain://teton/web-search` — never \
                 a raw key",
                "  search_auth      the header the key rides, `{key}` marking the secret (default \
                 `Authorization: Bearer {key}`)",
                "the keychain entry itself is written with `security add-generic-password -s \
                 teton -a web-search -w` — put `-w` last and it prompts for the key instead of \
                 leaving it in your shell history.",
                "the `search` tier also needs the local model, which scans every query before it \
                 leaves the machine.",
            ]
        );
    }

    /// Both tier spellings reach the same tier, and nothing else resolves to
    /// one — a near-miss decides what the machine may fetch, so it is answered
    /// with the list rather than guessed at.
    #[test]
    fn tier_answers_are_read_exactly() {
        for (typed, expected) in [
            ("1", WebTier::FetchUserUrl),
            ("fetch_user_url", WebTier::FetchUserUrl),
            ("2", WebTier::FetchAnyUrl),
            ("FETCH_ANY_URL", WebTier::FetchAnyUrl),
            ("3", WebTier::Search),
            ("search", WebTier::Search),
        ] {
            assert_eq!(parse_tier(typed), TierChoice::Tier(expected), "{typed}");
        }
        assert_eq!(parse_tier("off"), TierChoice::Off);
        for typed in ["4", "fetch", "sea rch", "yes", "-1"] {
            assert_eq!(parse_tier(typed), TierChoice::Unknown, "{typed}");
        }
    }

    /// Asking to turn the capability *off* is answered, not obeyed: this flow's
    /// completion notice announces an enablement, and `tier = "off"` is not one.
    #[test]
    fn an_answer_of_off_is_pointed_at_the_config_and_changes_nothing() {
        let mut io = FakeIo::new(&["off"]);
        let keychain = MockKeychain::new();
        drive(&mut io, &keychain, &session(), Gate::Walk).unwrap();
        assert!(io.previews.is_empty() && io.commits.is_empty());
        assert!(
            io.rendered().contains("tier = \"off\""),
            "{}",
            io.rendered()
        );
    }

    /// The state the daemon derived is rendered per variant, never re-derived —
    /// and the off-but-available case, the one this REQ exists for, says both
    /// halves out loud.
    #[test]
    fn every_capability_state_has_its_own_sentence() {
        let ready = capability_line(&WebCapabilityState::Ready {
            tier: WebTier::Search,
        });
        assert!(ready.contains("search"), "{ready}");
        let off = capability_line(&WebCapabilityState::OffAvailable);
        assert!(off.contains("available") && off.contains("off"), "{off}");
        let gap = capability_line(&WebCapabilityState::SearchUnavailable {
            reason: "search needs the local model".to_owned(),
        });
        assert!(gap.contains("search needs the local model"), "{gap}");
    }

    /// A daemon that reports a gap without naming it still produces a readable
    /// sentence — a blank in the middle of a line is worse than admitting the
    /// daemon said nothing.
    #[test]
    fn an_unnamed_gap_still_reads() {
        let plan = WebSetupPlanResult {
            state: WebCapabilityState::OffAvailable,
            search_available: false,
            search_gap: None,
            current_web: None,
            suggestion_catalog: None,
        };
        let menu = tier_menu_lines(&plan).join("\n");
        assert!(menu.contains(UNNAMED_GAP), "{menu}");
        assert!(!menu.contains("()"), "{menu}");
        assert!(search_refused_line(None).contains(UNNAMED_GAP));
    }

    /// The summary of the current table names every non-secret field and no
    /// others — the endpoint appears as a host (REQ-563 BR-7) because that is
    /// what the daemon sent.
    #[test]
    fn the_current_table_summary_names_what_is_there() {
        let line = current_table_line(&WebTableSummary {
            tier: WebTier::Search,
            search_host: Some("search.example".to_owned()),
            search_key_ref: Some("keychain://teton/web-search".to_owned()),
            search_auth: Some("X-Configured-Header: {key}".to_owned()),
        });
        for needle in [
            "search",
            "search.example",
            "keychain://teton/web-search",
            "X-Configured-Header: {key}",
        ] {
            assert!(line.contains(needle), "{line}");
        }

        let bare = current_table_line(&WebTableSummary {
            tier: WebTier::Off,
            search_host: None,
            search_key_ref: None,
            search_auth: None,
        });
        assert!(bare.contains("off"), "{bare}");
        assert!(!bare.contains("key"), "{bare}");
    }

    /// The answers never carry a credential into a formatted string, however
    /// they are formatted.
    #[test]
    fn debugging_the_answers_does_not_print_the_key() {
        let answers = Answers {
            tier: WebTier::Search,
            search_endpoint: Some("https://example.test/search".to_owned()),
            search_auth: None,
            search_key: Some(PLANTED_KEY.to_owned()),
        };
        let printed = format!("{answers:?}");
        assert!(!printed.contains(PLANTED_KEY), "{printed}");
        assert!(printed.contains("redacted"), "{printed}");
        // And the reference it predicts is the one the store would return.
        assert_eq!(
            answers.key_ref().as_deref(),
            Some("keychain://teton/web-search")
        );
    }
}
