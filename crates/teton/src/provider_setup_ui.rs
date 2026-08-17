//! The `/provider setup` walkthrough — collect at the edge, commit at the core
//! (REQ-579 ADR-1/ADR-3, mirroring REQ-572's `/web setup`).
//!
//! The daemon holds no step state. This module asks the four questions a
//! provider registration needs — vendor, model, key, and which tier(s) to route
//! — and the daemon answers three stateless RPCs: `provider/setup_plan`,
//! `provider/setup_preview`, `provider/setup_commit`. Until the commit lands
//! nothing durable exists anywhere (BR-8), so an abort is not an operation: it
//! is this module deciding to stop asking.
//!
//! ## What is *not* decided here
//!
//! The TOML shown at the confirm step, the host it would reach, and the
//! `replaces` fact are the **daemon's own** answers, rendered. Nothing in this
//! file composes a config table or forms an opinion about what a valid candidate
//! is (BR-3, LESSON-494). The one piece of URL logic the flow runs is
//! [`crate::settle_endpoint_text`] — `teton provider add`'s own compose-refuse-echo
//! core, called rather than copied (ADR-8, LESSON-528/529), so a pasted base URL
//! composes exactly as the shell command composes it and a backslash-in-authority
//! URL is refused with the same sentence.
//!
//! ## The catalog is the daemon's list, rendered (BR-4)
//!
//! Which vendors the walkthrough names, what each one's endpoint looks like and
//! which model it offers as an example all arrive as data on the plan, in
//! [`ProviderRecipeEntry`] — this file holds no copy, so the entry a user picks
//! is the entry the model's inline guide would have named (ADR-4). What this
//! file *does* own is the **lenient resolution** of a vendor argument against
//! that list (ADR-2): the model teaches a user `Moonshot/Kimi`, a user types
//! `kimi`, and both have to land on the same row without a second spelling table
//! living here.
//!
//! ## The secret's whole life is in this process (BR-2, ADR-5)
//!
//! The key is read echo-off into memory, written to the OS keychain under the
//! **provider id** — the same account `teton provider add` uses, so a rotation
//! from a session hits the row a shell registration created — **after** the user
//! has confirmed the preview, and only its reference (`keychain://teton/<id>`)
//! travels to the daemon.
//!
//! The account is the id, so that store is **destructive** on a rotation: the
//! flow reads the account before it writes ([`PriorKey::read`]) and, when the
//! commit the write was made for is refused, puts back exactly what it displaced
//! rather than deleting (BUG-171 / LESSON-514). A *transport* failure is the
//! third case and is left alone entirely: the commit may have landed, so both
//! undos could be the destructive one, and the honest answer is a notice rather
//! than a guess.
//!
//! ## Everything but the bytes is testable without a terminal
//!
//! The flow reaches the world through one seam, [`SetupIo`]: the three RPCs, the
//! [`Surface`] and the [`Prompter`]. Production wires it to a real connection
//! and the session's own context; the tests wire it to a recording surface, a
//! scripted prompter and canned answers, which is what lets the
//! store-then-commit ordering, the undo on refusal and every abort point be
//! pinned with no socket, no keychain and no tty.

use teton_protocol::jsonrpc::{error_code, RpcError};
use teton_protocol::methods::{
    ExistingProvider, ProviderRecipeEntry, ProviderSetupCandidate, ProviderSetupCommitParams,
    ProviderSetupCommitResult, ProviderSetupPlanParams, ProviderSetupPlanResult,
    ProviderSetupPreviewParams, ProviderSetupPreviewResult, TierBinding,
};
use teton_protocol::{ProviderId, ProviderKind, SessionId, Tier};

use crate::client::{Connection, UiContext};
use crate::keychain::{auth_ref_for, Cleanup, Keychain, PriorKey};
use crate::prompt::Prompter;
use crate::render::{LineKind, Surface};
use crate::settle_endpoint_text;
// The gate is `/web setup`'s, imported rather than restated: "may this command
// put a question to this session's stdin" is one predicate with one answer, and
// a second `enum Gate { Walk, Instructions }` here would be the mirrored
// predicate LESSON-528 is about — identical today, and identical only until one
// of them is edited.
use crate::web_setup_ui::{gate, Gate};

/// What `/provider setup` says when there is no session to act on.
///
/// Reachable only from a context that owns no session, the same guard
/// `/web setup`'s is — it keeps the id from being fabricated rather than being a
/// line users meet.
const SETUP_NEEDS_A_SESSION: &str =
    "`/provider setup` needs a session to act on, and this command owns none.";

/// What `/provider setup` says to a daemon built before REQ-579.
///
/// A version fact, not a failure, so it wears no `error:` prefix (BUG-152) — and
/// it names the shell command that does the same job on such a daemon rather
/// than only reporting that the walkthrough is missing.
const SETUP_UNAVAILABLE: &str =
    "this daemon build does not serve the guided provider setup — restart it after upgrading, or \
     register the provider from a shell with `teton provider add`.";

/// What every abort renders: the fact the user needs, which is that the machine
/// is exactly as they left it.
const SETUP_ABORTED: &str =
    "provider setup cancelled — nothing was written to your config and no key was stored.";

/// The decline at the confirm step. Distinct from [`SETUP_ABORTED`] because the
/// user got as far as reading the bytes and said no to *them*.
const SETUP_DECLINED: &str = "not written. Nothing was changed and no key was stored; run \
                              `/provider setup` again to start over.";

/// What the flow says when the daemon offered no preview digest.
///
/// A daemon that predates the digest field degrades the commit to the protocol's
/// own "do not check". Rendered **before the confirm question**, because a guard
/// that turns itself off on version skew must say so where the user can still
/// act on it, and declining is the one act this flow offers (BUG-166 residual).
const DIGEST_CHECK_UNAVAILABLE: &str =
    "this daemon build offers no preview digest, so the commit is not pinned to the previewed \
     bytes — upgrade and restart the daemon to restore that check.";

/// What a session whose input is not a terminal is told, above the recipe
/// (BR-11 / AC-9).
///
/// It is a *degradation*, not a refusal, and it says why: the key prompt is the
/// part that needs a terminal. Nothing is read from stdin — the line the user
/// typed for the session stays theirs.
const NOT_A_TERMINAL: &str =
    "`/provider setup` reads an API key without echoing it, which needs a terminal — this \
     session's input is not one, so nothing was read and nothing was changed.";

/// What a session on a platform with **no OS keychain** is told, above the same
/// recipe a non-TTY session gets (requirement Assumptions, BR-11).
///
/// The posture is inherited from `teton provider add`: this flow does not invent
/// a fallback store, and it does not walk a user through five questions and a
/// typed credential before admitting there is nowhere to put it. Availability is
/// asked at the gate, next to the TTY question, because both are the same kind of
/// fact — a property of the world this session is running in, known before the
/// first prompt.
const NO_KEYCHAIN: &str =
    "`/provider setup` files the API key in your OS keychain, and this build has no keychain \
     backend for this platform — so nothing was read and nothing was changed.";

/// What the flow says when the daemon served an empty catalog.
///
/// Structurally impossible per the protocol (`catalog` is required and the
/// recipes ship with the method), which is exactly why it gets a sentence rather
/// than an empty menu and a prompt nobody can answer.
const EMPTY_CATALOG: &str =
    "this daemon build named no vendors, so there is nothing to walk through — \
     `teton provider add <id> --kind <kind> --endpoint <url> --model <name>` registers one by hand.";

// ---------------------------------------------------------------------------
// The world seam
// ---------------------------------------------------------------------------

/// Everything the flow can reach outside itself: three RPCs, a place to render,
/// and a place to ask.
///
/// The accessors hand out short-lived borrows rather than the seams themselves
/// because the production implementation holds the session's `UiContext` — the
/// same context [`Connection::call`] needs in order to pump events while a
/// request is in flight.
pub(crate) trait SetupIo {
    /// Where the flow's lines go.
    fn surface(&mut self) -> &mut dyn Surface;
    /// Where the flow's questions go.
    fn prompter(&mut self) -> &mut dyn Prompter;
    /// `provider/setup_plan`.
    ///
    /// # Errors
    /// Propagates a transport failure; a daemon that *answers* with an error
    /// returns it in the inner `Result`.
    fn plan(
        &mut self,
        params: ProviderSetupPlanParams,
    ) -> anyhow::Result<Result<ProviderSetupPlanResult, RpcError>>;
    /// `provider/setup_preview`.
    ///
    /// # Errors
    /// As [`Self::plan`].
    fn preview(
        &mut self,
        params: ProviderSetupPreviewParams,
    ) -> anyhow::Result<Result<ProviderSetupPreviewResult, RpcError>>;
    /// `provider/setup_commit`.
    ///
    /// # Errors
    /// As [`Self::plan`].
    fn commit(
        &mut self,
        params: ProviderSetupCommitParams,
    ) -> anyhow::Result<Result<ProviderSetupCommitResult, RpcError>>;
}

/// The production seam: the session's own connection and context.
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
        params: ProviderSetupPlanParams,
    ) -> anyhow::Result<Result<ProviderSetupPlanResult, RpcError>> {
        self.conn.call(params, self.ctx)
    }

    fn preview(
        &mut self,
        params: ProviderSetupPreviewParams,
    ) -> anyhow::Result<Result<ProviderSetupPreviewResult, RpcError>> {
        self.conn.call(params, self.ctx)
    }

    fn commit(
        &mut self,
        params: ProviderSetupCommitParams,
    ) -> anyhow::Result<Result<ProviderSetupCommitResult, RpcError>> {
        self.conn.call(params, self.ctx)
    }
}

// ---------------------------------------------------------------------------
// The answers
// ---------------------------------------------------------------------------

/// What the user typed, held in this process and nowhere else.
#[derive(Clone, PartialEq, Eq)]
struct Answers {
    /// The id to register under — also the keychain account (ADR-5).
    id: String,
    /// Which adapter the provider speaks.
    kind: ProviderKind,
    /// The **composed** absolute request URL, as
    /// [`crate::settle_endpoint_text`] settled it — never the raw paste.
    endpoint: Option<String>,
    /// The model to pin. Required and asked before the key (BR-6, REQ-557 BR-1).
    model: String,
    /// The credential itself — memory only, never rendered, never serialized.
    key: String,
    /// The tiers to route, zero or more (BR-7).
    bindings: Vec<TierBinding>,
}

/// Redacts the key.
///
/// Hand-written rather than derived because a derived `Debug` puts the
/// credential into any `dbg!`, any `unwrap` panic, and any test failure message
/// that formats these answers.
impl std::fmt::Debug for Answers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Answers")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .field("key", &"<redacted>")
            .field("bindings", &self.bindings)
            .finish()
    }
}

impl Answers {
    /// The reference the daemon will be given for the collected key.
    ///
    /// Known **before** the key is stored, because it is a name and not a value:
    /// service and account are both fixed by the id, so the preview can show the
    /// exact `key_ref` the commit will write without a credential having been
    /// written anywhere yet. That is what keeps the confirmed bytes and the
    /// committed bytes the same bytes (BR-9).
    fn key_ref(&self) -> String {
        auth_ref_for(&self.id)
    }

    /// The candidate these answers describe, carrying `key_ref` rather than the
    /// key (BR-2).
    fn candidate(&self, key_ref: String) -> ProviderSetupCandidate {
        ProviderSetupCandidate {
            id: ProviderId::from(self.id.as_str()),
            kind: self.kind,
            endpoint: self.endpoint.clone(),
            model: self.model.clone(),
            key_ref,
            bindings: self.bindings.clone(),
        }
    }

    /// The preview request these answers describe.
    fn preview_params(&self, session_id: &SessionId) -> ProviderSetupPreviewParams {
        ProviderSetupPreviewParams {
            session_id: session_id.clone(),
            candidate: self.candidate(self.key_ref()),
        }
    }

    /// The commit request these answers describe, carrying the reference the
    /// keychain actually returned rather than the one this struct predicted, and
    /// the previewed document's digest so the daemon refuses to write bytes the
    /// user never confirmed (BR-9).
    fn commit_params(
        &self,
        session_id: &SessionId,
        key_ref: String,
        expect_digest: Option<String>,
    ) -> ProviderSetupCommitParams {
        ProviderSetupCommitParams {
            session_id: session_id.clone(),
            candidate: self.candidate(key_ref),
            expect_digest,
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run `/provider setup [vendor] [tier]` on the session's own connection and
/// context.
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
    vendor: Option<&str>,
    tier: Option<&str>,
) -> anyhow::Result<()> {
    let Some(session_id) = ctx.session_id.clone() else {
        ctx.surface.line(LineKind::Error, SETUP_NEEDS_A_SESSION);
        return Ok(());
    };
    let gate = gate(ctx.typed_input, crate::slash::test_seams_allowed());
    let mut io = DaemonIo { conn, ctx };
    drive(&mut io, keychain, &session_id, gate, vendor, tier)
}

/// The flow itself, over the seam.
///
/// # Errors
///
/// Propagates a transport error from the plan or preview call. A transport
/// failure on the **commit** is bound rather than propagated: the write may have
/// landed, and that is the one path where the user most needs to be told what
/// state their machine is in.
pub(crate) fn drive(
    io: &mut dyn SetupIo,
    keychain: &dyn Keychain,
    session_id: &SessionId,
    gate: Gate,
    vendor_arg: Option<&str>,
    tier_arg: Option<&str>,
) -> anyhow::Result<()> {
    let plan = match io.plan(ProviderSetupPlanParams {
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
                &format!("provider setup could not start: {}", err.message),
            );
            return Ok(());
        }
    };

    // Two world-facts, one degradation. A session that cannot be asked a
    // question and a platform with nowhere to put the answer both end in BR-11's
    // recipe, and both are settled **here** — before a vendor menu, before a
    // model, and above all before a credential is typed.
    let degraded = if gate == Gate::Instructions {
        Some(NOT_A_TERMINAL)
    } else if keychain.is_available() {
        None
    } else {
        Some(NO_KEYCHAIN)
    };
    if let Some(reason) = degraded {
        io.surface().line(LineKind::Notice, reason);
        // What was silently normalised on the way to the recipe, said out loud.
        // The walk reports both of these (`unknown_vendor_line`,
        // `unknown_tier_argument_line`) because the user is about to act on the
        // answer; on this path they are about to *run* it, which is the stronger
        // reason, not a weaker one.
        for line in argument_notices(&plan, vendor_arg, tier_arg) {
            io.surface().line(LineKind::Notice, &line);
        }
        for line in instruction_lines(&plan.catalog, vendor_arg, tier_arg) {
            io.surface().line(LineKind::Info, &line);
        }
        // Last, because it corrects the line above it: the recipe says
        // `teton provider add` files the key in the OS keychain, which is the
        // one sentence in it that is not true here.
        //
        // Asked of the keychain rather than of `reason`, so a *piped* session on
        // a platform with no backend gets the correction too. It reached this
        // block by the other door, and the recipe it is being handed is just as
        // wrong about where the key goes.
        if !keychain.is_available() {
            io.surface().line(LineKind::Notice, &out_of_band_key_line());
        }
        return Ok(());
    }

    let Some(answers) = collect(&plan, io, vendor_arg, tier_arg) else {
        io.surface().line(LineKind::Notice, SETUP_ABORTED);
        return Ok(());
    };

    let preview = match io.preview(answers.preview_params(session_id))? {
        Ok(preview) => preview,
        Err(err) => {
            // Including `PROVIDER_SETUP_INVALID`, whose message is the
            // validator's own sentence — carried verbatim, because the daemon is
            // the one that knows why a candidate would not load. No key has been
            // stored at this point (BR-8): the store happens past the confirm.
            io.surface()
                .line(LineKind::Error, &refused_line(&err, "previewed"));
            return Ok(());
        }
    };
    for line in render_preview(&preview, &answers.model) {
        io.surface().line(LineKind::Info, &line);
    }
    // The daemon's own sentences, rendered verbatim and in its own order —
    // "replaces existing provider", an unpriced model, a cleartext endpoint.
    for warning in &preview.warnings {
        io.surface().line(LineKind::Notice, warning);
    }

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

    // ADR-5's residual-minimizing order: the store happens here — after the
    // human said yes, immediately before the commit it was collected for — so
    // the window in which an orphan can exist is one RPC wide rather than the
    // length of the flow.
    //
    // `prior` is read in the same breath and for the same reason the store is
    // late: the account is the provider id, so this write displaces whatever a
    // previous registration put there, and after it there is no way left to find
    // out what that was (BUG-171).
    let prior = PriorKey::read(keychain, &answers.id);
    let key_ref = match keychain.store(&answers.id, &answers.key) {
        Ok(reference) => reference,
        Err(err) => {
            io.surface().line(
                LineKind::Error,
                &format!(
                    "the key could not be stored in the OS keychain ({err}); nothing was written \
                     to your config."
                ),
            );
            return Ok(());
        }
    };

    // Bound rather than `?`-ed. A transport failure here is not the same event
    // as a daemon that answered "no": the commit may have landed, and letting
    // the error out would end the session on the one path where the user most
    // needs the flow to tell them what state their machine is in.
    match io.commit(answers.commit_params(session_id, key_ref, expect_digest)) {
        Ok(Ok(result)) => {
            // A successful, routed commit renders nothing of its own: the daemon
            // publishes `provider_setup_completed` for this session and
            // `Connection::call` has already pumped it through `session_ui` by
            // the time this returns (BR-15). What is added here is only what
            // that event does not carry — that the config was already exactly
            // this, and the remedy for the unrouted outcome.
            if !result.applied {
                // The daemon publishes `provider_setup_completed` only for a
                // commit that *changed* something, so on this path there is no
                // event line naming the host — which makes this the one place
                // the unchanged outcome can say where the registration that
                // stands will be dialed.
                io.surface().line(
                    LineKind::Notice,
                    &unchanged_line(&result.provider_id, &result.dial_host),
                );
            }
            if result.bindings.is_empty() {
                io.surface()
                    .line(LineKind::Notice, &unrouted_line(&result.provider_id));
            }
        }
        Ok(Err(err)) => {
            // The entry that was written a moment ago exists only for this
            // commit, and this commit did not happen (BR-8). A flow that
            // displaced a credential owes the machine what it displaced, not a
            // delete — which is exactly the three-way decision `PriorKey` holds.
            let cleanup = prior.undo(keychain);
            io.surface()
                .line(LineKind::Error, &refused_line(&err, "written"));
            io.surface()
                .line(LineKind::Notice, &cleanup_line(&answers.id, &cleanup));
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
            io.surface().line(
                LineKind::Notice,
                &ambiguous_commit_line(&answers.id, &prior),
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Vendor resolution (ADR-2) — pure
// ---------------------------------------------------------------------------

/// What a vendor argument resolved to against the plan's catalog.
#[derive(Debug)]
pub(crate) enum Resolution<'a> {
    /// Exactly one entry. The flow uses it and asks nothing.
    One(&'a ProviderRecipeEntry),
    /// Several entries answer to the same spelling. The flow lists these and
    /// asks.
    Many(Vec<&'a ProviderRecipeEntry>),
    /// Nothing matched — including the no-argument case, which is the same
    /// actionable state: the flow lists the whole catalog and asks (AC-3).
    None,
}

/// Fold a spelling to the form both sides of a comparison are held to: ASCII
/// lowercase, with every non-alphanumeric character dropped.
///
/// So `Moonshot (Kimi)`, `Moonshot/Kimi` and `moonshotkimi` are one string, and
/// `kimi` is not `kimi-k3`. Deliberately **not** a fuzzy match: `deep` must not
/// resolve to `deepseek`, because the value decides which vendor a user is about
/// to type a credential for, and a near-miss resolved silently is the spelling
/// nobody tests.
fn normalised(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Every spelling that reaches one catalog entry (ADR-2).
///
/// The three fields the protocol carries, each folded whole — and, for the two
/// *display* spellings, each of their words on its own. The word split is what
/// makes `moonshot` reach `Moonshot (Kimi)`: the vendor's own name is the half a
/// user remembers, and the parenthesised half is the product. It cannot make the
/// resolution looser than an exact word, so `deep` still reaches nothing.
fn spellings(entry: &ProviderRecipeEntry) -> Vec<String> {
    let mut keys = vec![
        normalised(&entry.id_suggestion),
        normalised(&entry.label),
        normalised(&entry.guide_spelling),
    ];
    for source in [&entry.label, &entry.guide_spelling] {
        keys.extend(
            source
                .split(|c: char| !c.is_alphanumeric())
                .filter(|word| !word.is_empty())
                .map(str::to_ascii_lowercase),
        );
    }
    keys.retain(|key| !key.is_empty());
    keys
}

/// The entries a normalised needle answers to.
fn pick<'a>(
    entries: impl Iterator<Item = &'a ProviderRecipeEntry>,
    needle: &str,
) -> Resolution<'a> {
    let matched: Vec<&ProviderRecipeEntry> = entries
        .filter(|entry| spellings(entry).iter().any(|key| key == needle))
        .collect();
    match matched.first() {
        None => Resolution::None,
        Some(one) if matched.len() == 1 => Resolution::One(one),
        Some(_) => Resolution::Many(matched),
    }
}

/// Resolve a vendor argument leniently against the daemon-served catalog
/// (ADR-2). Pure.
///
/// `None` for the argument is `Resolution::None` on purpose: "you did not say"
/// and "what you said matches nothing" put the caller in the same place — list
/// the catalog and ask — and giving them one spelling means there is one path to
/// test rather than two that must agree.
pub(crate) fn resolve_vendor<'a>(
    catalog: &'a [ProviderRecipeEntry],
    arg: Option<&str>,
) -> Resolution<'a> {
    let Some(needle) = arg.map(normalised).filter(|needle| !needle.is_empty()) else {
        return Resolution::None;
    };
    pick(catalog.iter(), &needle)
}

// ---------------------------------------------------------------------------
// Collection
// ---------------------------------------------------------------------------

/// The vendor question, asked only when the argument did not settle it.
const VENDOR_QUESTION: &str = "  vendor [number or name, Enter to cancel]: ";

/// The endpoint question for an entry that has none to offer.
const ENDPOINT_QUESTION_BARE: &str = "  endpoint [Enter to cancel]: ";

/// The one confirmation, default-**no** (LESSON-470).
const CONFIRM_QUESTION: &str = "  write this to your config? [y/N] ";

/// The model question, naming what Enter would take — and naming it as an
/// *example* rather than a recommendation (BR-6).
fn model_question(example: &str) -> String {
    format!("  model [Enter for `{example}`, an example rather than a recommendation]: ")
}

/// The endpoint question, naming the recipe's own request URL as what Enter
/// takes.
fn endpoint_question(offered: &str) -> String {
    format!("  endpoint [Enter for `{offered}`]: ")
}

/// The credential question. It says where the key goes, because that is the part
/// the user is agreeing to.
fn key_question(id: &str) -> String {
    format!("  API key (not shown; stored in your OS keychain as `{id}`): ")
}

/// The routing question, naming the tier Enter would take (ADR-6, LESSON-470 —
/// the routing offer defaults to yes, because the user asked for it and it is
/// reversible).
fn routing_question(default: Tier) -> String {
    format!(
        "  route [Enter for `{default}`, or tier names/numbers, or `none`]: ",
        default = default.as_str()
    )
}

/// Ask every question a registration needs, or `None` for any abort.
///
/// `None` is EOF, an empty answer at a question that needs one, an unreadable
/// answer, or Ctrl-C (which ends the process and therefore this). Every one of
/// them leaves this function having sent nothing and stored nothing —
/// collection is buffering, and a buffer nobody submits is not state.
fn collect(
    plan: &ProviderSetupPlanResult,
    io: &mut dyn SetupIo,
    vendor_arg: Option<&str>,
    tier_arg: Option<&str>,
) -> Option<Answers> {
    let entry = choose_vendor(plan, io, vendor_arg)?;
    let id = entry.id_suggestion.clone();
    io.surface().line(LineKind::Info, &vendor_line(entry));
    // BR-14: an id that is already taken is said out loud *before* a key is
    // typed for it. What actually changes is the daemon's `replaces`, shown at
    // the preview — this is the early warning, not the authority.
    if let Some(existing) = plan.existing.iter().find(|held| held.id.0 == id) {
        io.surface()
            .line(LineKind::Notice, &already_registered_line(existing));
    }

    // REQ-557 BR-1: the model is asked and validated BEFORE the key. A candidate
    // without one is refused, and refusing it after a credential has been typed
    // is the sequence that command was fixed to avoid.
    let typed = io.prompter().ask(&model_question(&entry.example_model))?;
    let model = match typed.trim() {
        "" => entry.example_model.trim(),
        answered => answered,
    };
    if model.is_empty() {
        return None;
    }
    let model = model.to_owned();

    let endpoint = settle(entry, io)?;

    // REQ-578 BR-5 / AC-6: the composed URL has been echoed by `settle` above,
    // so what the user is about to type a key for is on screen first.
    let key = io.prompter().ask_secret(&key_question(&id))?;
    let key = key.trim();
    if key.is_empty() {
        return None;
    }

    let bindings = choose_bindings(plan, io, &id, tier_arg)?;

    Some(Answers {
        id,
        kind: entry.kind,
        endpoint,
        model,
        key: key.to_owned(),
        bindings,
    })
}

/// Settle the endpoint through `teton provider add`'s own core (ADR-8), asking
/// for one unless the kind carries its own address (ADR-7).
///
/// Returns `None` for a cancel *and* for a refusal — the refusal is rendered
/// here because it is the seam's own sentence, and the caller's `None` handling
/// then reports that nothing was written, which is true of both.
///
/// `anthropic` is asked nothing: the composed default is settled with `None` and
/// echoed, so the address is still on screen before the key prompt even though
/// there was no question.
fn settle(entry: &ProviderRecipeEntry, io: &mut dyn SetupIo) -> Option<Option<String>> {
    let supplied = if matches!(entry.kind, ProviderKind::Anthropic) {
        None
    } else {
        let offered = entry.endpoint.as_deref();
        let question = offered.map_or_else(|| ENDPOINT_QUESTION_BARE.to_owned(), endpoint_question);
        let typed = io.prompter().ask(&question)?;
        let typed = typed.trim();
        // Empty here is an answer — "take what was offered" — and the question
        // says so. With nothing offered, empty is the cancel the question named.
        Some(if typed.is_empty() {
            offered?.to_owned()
        } else {
            typed.to_owned()
        })
    };

    match settle_endpoint_text(&entry.id_suggestion, entry.kind, supplied.as_deref()) {
        Ok(settled) => {
            if let Some(echo) = &settled.echo {
                io.surface().line(LineKind::Info, echo);
            }
            if let Some(warning) = &settled.cleartext_warning {
                io.surface().line(LineKind::Notice, warning);
            }
            Some(settled.stored)
        }
        Err(message) => {
            io.surface().line(LineKind::Error, &message);
            None
        }
    }
}

/// Settle which vendor this is: the argument when it resolves, the catalog and a
/// question when it does not (AC-3).
fn choose_vendor<'a>(
    plan: &'a ProviderSetupPlanResult,
    io: &mut dyn SetupIo,
    arg: Option<&str>,
) -> Option<&'a ProviderRecipeEntry> {
    let offered: Vec<&ProviderRecipeEntry> = match resolve_vendor(&plan.catalog, arg) {
        Resolution::One(entry) => return Some(entry),
        Resolution::Many(entries) => {
            io.surface().line(
                LineKind::Notice,
                &ambiguous_vendor_line(arg.unwrap_or_default()),
            );
            entries
        }
        Resolution::None => {
            // An argument that matched nothing is said out loud; no argument at
            // all is not an error and gets no line.
            if let Some(arg) = arg.map(str::trim).filter(|arg| !arg.is_empty()) {
                io.surface()
                    .line(LineKind::Notice, &unknown_vendor_line(arg));
            }
            plan.catalog.iter().collect()
        }
    };

    if offered.is_empty() {
        io.surface().line(LineKind::Error, EMPTY_CATALOG);
        return None;
    }
    for line in catalog_lines(&offered) {
        io.surface().line(LineKind::Info, &line);
    }
    let typed = io.prompter().ask(VENDOR_QUESTION)?;
    let typed = typed.trim();
    if typed.is_empty() {
        return None;
    }
    // A number is read as a position in the list that was just printed, and only
    // as that: a catalog spelled entirely in digits is not a thing, and reading
    // `2` as a name would make the menu's own numbering ambiguous.
    if let Ok(position) = typed.parse::<usize>() {
        let picked = position.checked_sub(1).and_then(|index| offered.get(index));
        return match picked {
            Some(entry) => Some(entry),
            None => {
                io.surface()
                    .line(LineKind::Error, &out_of_range_line(typed, offered.len()));
                None
            }
        };
    }
    match pick(offered.iter().copied(), &normalised(typed)) {
        Resolution::One(entry) => Some(entry),
        Resolution::Many(_) => {
            io.surface()
                .line(LineKind::Error, &ambiguous_vendor_line(typed));
            None
        }
        Resolution::None => {
            io.surface()
                .line(LineKind::Error, &unknown_vendor_line(typed));
            None
        }
    }
}

/// Ask which tiers should route to the new provider (BR-7, ADR-6).
///
/// One question over the daemon's own list of routable tiers, with the command's
/// tier argument — or `think` — pre-selected. Zero selections is a legal answer
/// and is spelled `none`; it is not an abort, and the flow says plainly what it
/// leaves behind (AC-13).
fn choose_bindings(
    plan: &ProviderSetupPlanResult,
    io: &mut dyn SetupIo,
    id: &str,
    tier_arg: Option<&str>,
) -> Option<Vec<TierBinding>> {
    let tiers: Vec<Tier> = plan.tiers.iter().map(|summary| summary.tier).collect();
    // A daemon that reports no routable tier has nothing to ask about; the
    // registration is still worth making, and the unrouted sentence covers it.
    let Some(default) = preferred_tier(&tiers, tier_arg, io) else {
        return Some(Vec::new());
    };

    for line in routing_lines(plan, id, default) {
        io.surface().line(LineKind::Info, &line);
    }
    let typed = io.prompter().ask(&routing_question(default))?;
    let typed = typed.trim();
    if typed.is_empty() {
        return Some(vec![binding(default, id)]);
    }
    if matches!(typed.to_ascii_lowercase().as_str(), "none" | "no" | "n") {
        return Some(Vec::new());
    }

    let mut chosen: Vec<Tier> = Vec::new();
    for token in typed
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|token| !token.is_empty())
    {
        let Some(tier) = read_tier(&tiers, token) else {
            // A near-miss is answered with the list rather than resolved to
            // whichever tier is closest: the value decides what this provider
            // will be asked to do.
            io.surface()
                .line(LineKind::Error, &unknown_tier_line(token, &tiers));
            return None;
        };
        if !chosen.contains(&tier) {
            chosen.push(tier);
        }
    }
    Some(chosen.into_iter().map(|tier| binding(tier, id)).collect())
}

/// One tier→provider binding, spelled out rather than implied by position
/// (LESSON-522).
fn binding(tier: Tier, id: &str) -> TierBinding {
    TierBinding {
        tier,
        provider_id: ProviderId::from(id),
    }
}

/// The tier to pre-select: the command's argument when it names one the daemon
/// reports as routable, else `think`, else whatever the daemon listed first
/// (ADR-6).
///
/// An argument that names no routable tier is reported and then ignored — it
/// decides a *default offer*, not a write, and the user still reads the menu and
/// confirms a preview. Refusing the whole command over it would turn a typo in
/// the hand-off the model composed into a dead end.
fn preferred_tier(tiers: &[Tier], arg: Option<&str>, io: &mut dyn SetupIo) -> Option<Tier> {
    if let Some(arg) = arg.map(str::trim).filter(|arg| !arg.is_empty()) {
        if let Some(tier) = tiers
            .iter()
            .copied()
            .find(|tier| tier.as_str().eq_ignore_ascii_case(arg))
        {
            return Some(tier);
        }
        io.surface()
            .line(LineKind::Notice, &unknown_tier_argument_line(arg, tiers));
    }
    tiers
        .iter()
        .copied()
        .find(|tier| *tier == Tier::Think)
        .or_else(|| tiers.first().copied())
}

/// Read one routing answer: the menu number, or the tier's own config spelling.
///
/// Both are accepted because both are in front of the user — the menu numbers it
/// and the row names it, and the name is what they will later see in their
/// config file.
fn read_tier(tiers: &[Tier], token: &str) -> Option<Tier> {
    if let Ok(position) = token.parse::<usize>() {
        return position
            .checked_sub(1)
            .and_then(|index| tiers.get(index))
            .copied();
    }
    tiers
        .iter()
        .copied()
        .find(|tier| tier.as_str().eq_ignore_ascii_case(token))
}

/// An explicit yes, and nothing else (LESSON-470). Empty and EOF are both no.
fn is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}

// ---------------------------------------------------------------------------
// Content (pure)
// ---------------------------------------------------------------------------

/// The `--kind` value a recipe's kind is spelled as on `teton provider add`.
///
/// An exhaustive match rather than a lookup table, so a new [`ProviderKind`]
/// cannot reach an instruction line without somebody deciding how it is typed —
/// and `every_kind_spells_itself_the_way_the_cli_parses_it` runs each of these
/// through the CLI's own argument parser, so the two cannot drift.
fn kind_flag(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Local => "local",
        ProviderKind::OpenaiCompatible => "openai-compatible",
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::Custom => "custom",
    }
}

/// The address a catalog entry would register, including the one a kind composes
/// for itself.
///
/// Read through [`crate::settle_endpoint_text`] rather than off the field, so an
/// `anthropic` row whose recipe carries no endpoint still shows the URL Teton
/// would dial (ADR-7/ADR-8) instead of a blank.
fn offered_endpoint(entry: &ProviderRecipeEntry) -> Option<String> {
    entry.endpoint.clone().or_else(|| {
        settle_endpoint_text(&entry.id_suggestion, entry.kind, None)
            .ok()
            .and_then(|settled| settled.stored)
    })
}

/// The one line naming the vendor the walk settled on.
fn vendor_line(entry: &ProviderRecipeEntry) -> String {
    let mut line = format!(
        "vendor: {} — id `{}`, kind `{}`",
        entry.label,
        entry.id_suggestion,
        kind_flag(entry.kind)
    );
    if let Some(endpoint) = offered_endpoint(entry) {
        line.push_str(&format!(", {endpoint}"));
    }
    line.push('.');
    if let Some(notes) = &entry.notes {
        line.push(' ');
        line.push_str(notes);
    }
    line
}

/// The catalog as a menu: `n) label — endpoint` (AC-3).
///
/// Every vendor fact in the output arrives as data. The one thing composed here
/// is the layout — labels pad to the widest one so the addresses line up, which
/// is a property of the list rather than of any entry in it.
fn catalog_lines(entries: &[&ProviderRecipeEntry]) -> Vec<String> {
    let width = entries
        .iter()
        .map(|entry| entry.label.chars().count())
        .max()
        .unwrap_or(0);
    let mut lines =
        vec!["which vendor? (a number, or the vendor's name in any spelling)".to_owned()];
    for (index, entry) in entries.iter().enumerate() {
        let address =
            offered_endpoint(entry).unwrap_or_else(|| "no address in this recipe".to_owned());
        lines.push(format!(
            "  {n}) {label:width$}  — {address}",
            n = index + 1,
            label = entry.label,
        ));
    }
    lines
}

/// What a vendor argument that matched nothing gets back.
fn unknown_vendor_line(typed: &str) -> String {
    format!(
        "`{}` is not a vendor this build knows — pick one from the list.",
        crate::slash::echoed(typed)
    )
}

/// What a vendor argument that matched several entries gets back.
fn ambiguous_vendor_line(typed: &str) -> String {
    format!(
        "`{}` names more than one vendor — pick one from the list.",
        crate::slash::echoed(typed)
    )
}

/// What a menu number outside the list gets back.
fn out_of_range_line(typed: &str, len: usize) -> String {
    format!(
        "`{}` is not one of the {len} listed vendors. Nothing was changed.",
        crate::slash::echoed(typed)
    )
}

/// BR-14's early warning: the id the walk is about is already taken.
fn already_registered_line(existing: &ExistingProvider) -> String {
    format!(
        "`{}` is already registered (kind `{}`, model `{}`) — going on replaces it, and the \
         preview says exactly what changes. Nothing is written until you confirm.",
        existing.id,
        kind_flag(existing.kind),
        existing.model.as_deref().unwrap_or("none"),
    )
}

/// The routing menu: every routable tier the daemon reported, what it points at
/// today, and which one Enter takes.
fn routing_lines(plan: &ProviderSetupPlanResult, id: &str, default: Tier) -> Vec<String> {
    let mut lines = vec![format!("which tiers should route to `{id}`?")];
    for (index, summary) in plan.tiers.iter().enumerate() {
        let bound = match &summary.provider_id {
            Some(provider) => format!("now `{provider}`"),
            None => "unbound".to_owned(),
        };
        let marker = if summary.tier == default {
            "  (Enter takes this one)"
        } else {
            ""
        };
        lines.push(format!(
            "  {n}) {tier:6}  {bound}{marker}",
            n = index + 1,
            tier = summary.tier.as_str(),
        ));
    }
    // ADR-6: a fallback binding is a real capability and is not a sixth
    // question, so the flow names where it lives instead of asking.
    lines.push(
        "  a backup provider is `teton policy set-tier <tier> <id> --fallback <other>`, and is \
         not asked here."
            .to_owned(),
    );
    lines
}

/// The rejection an unrecognised routing token gets, quoting what was typed
/// through the same bounded, sanitised echo a bad command name goes through.
fn unknown_tier_line(typed: &str, tiers: &[Tier]) -> String {
    format!(
        "`{}` is not one of the tiers — answer with a number, or one of {}, or `none`. Nothing \
         was changed.",
        crate::slash::echoed(typed),
        tier_names(tiers),
    )
}

/// The notice a `/provider setup <vendor> <tier>` argument that names no routable
/// tier gets. It changes a default offer and nothing else, so it is a notice.
fn unknown_tier_argument_line(typed: &str, tiers: &[Tier]) -> String {
    format!(
        "`{}` is not a routable tier ({}), so the routing question offers the usual default \
         instead.",
        crate::slash::echoed(typed),
        tier_names(tiers),
    )
}

/// The routable tiers, as the config file spells them.
fn tier_names(tiers: &[Tier]) -> String {
    tiers
        .iter()
        .map(|tier| format!("`{}`", tier.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The preview, as plain lines: the daemon's own TOML bytes, the host its parse
/// produced, and the provider this would replace (BR-9, BR-14).
///
/// All of it is rendered verbatim — this function decides layout and nothing
/// else (LESSON-494/529). It carries **no styling**: the `Surface` owns that, and
/// a flow that reached for SGR by hand is the drift LESSON-517 is about.
///
/// The daemon's `warnings` are deliberately not folded in here. They are its own
/// sentences and ride the `Notice` class, which this `Vec<String>` cannot carry;
/// [`drive`] renders them immediately after these lines, in the daemon's order.
/// `model` is the candidate's — the one fact in the replace line that the
/// preview result does not carry. The daemon reports what is being *replaced*
/// (`replaces`), and AC-12 asks the user to be shown the transition, which is
/// two models: the one on disk and the one they just typed. Passed in rather
/// than parsed back out of `toml`, because reading the answer out of the bytes
/// would be this file forming an opinion about the daemon's document.
pub(crate) fn render_preview(preview: &ProviderSetupPreviewResult, model: &str) -> Vec<String> {
    let mut lines = vec!["this is what would be written to your config:".to_owned()];
    for line in preview.toml.lines() {
        lines.push(format!("  {line}"));
    }
    lines.push(format!(
        "requests would go to: {} (and nowhere else)",
        preview.dial_host
    ));
    if let Some(replaced) = &preview.replaces {
        lines.push(replace_line(replaced, model));
    }
    lines
}

/// AC-12's sentence: which provider is being replaced, and the model change it
/// amounts to, old on the left.
///
/// The **transition** is the point. Rendering only the model already on disk
/// left the user reading a line about the thing they were replacing with no
/// statement of what it was becoming — the one number they are here to change,
/// and the one a rotation gets wrong silently. The kind is deliberately not
/// repeated: the previewed TOML directly above names the new one, and the
/// early warning before the key prompt named the old one.
///
/// A prior registration with no model is a real state (a row written by hand),
/// and `none → kimi-k3` reads as a model called "none", so it gets its own
/// phrasing rather than a placeholder.
fn replace_line(replaced: &ExistingProvider, model: &str) -> String {
    match replaced.model.as_deref() {
        Some(prior) => format!(
            "this replaces existing provider `{}` (model `{prior}` → `{model}`).",
            replaced.id
        ),
        None => format!(
            "this replaces existing provider `{}`, which names no model, with one pinned to \
             `{model}`.",
            replaced.id
        ),
    }
}

/// One line for a call the daemon refused, carrying its own sentence.
///
/// `stage` is what did not happen — "previewed" or "written" — so the user
/// learns which side of the commit point they are on without the message having
/// to say it twice.
fn refused_line(err: &RpcError, stage: &str) -> String {
    format!("nothing was {stage}: {}", err.message)
}

/// What the undo did, said out loud — including when it did nothing (BR-8/AC-8).
///
/// A failure to clean up is reported rather than swallowed: the user is the only
/// one who can act on the keychain by hand, and a credential left in a state they
/// were never told about is exactly the residue this ordering exists to avoid.
/// Each arm therefore ends in the command that finishes the job it could not.
fn cleanup_line(id: &str, cleanup: &Cleanup) -> String {
    let service = crate::keychain::SERVICE;
    match cleanup {
        Cleanup::Deleted(Ok(())) => format!(
            "the key that was stored for this attempt has been removed from your keychain, so \
             `{id}` is not left holding a credential nothing references."
        ),
        Cleanup::Deleted(Err(err)) => format!(
            "the key stored for this attempt could not be removed from your keychain ({err}) — it \
             is unreferenced, and `security delete-generic-password -s {service} -a {id}` clears \
             it."
        ),
        Cleanup::Restored(Ok(())) => format!(
            "the key stored for this attempt has been replaced with the one that was there \
             before, so the `{id}` entry your config already points at is unchanged."
        ),
        Cleanup::Restored(Err(err)) => format!(
            "the key that was in your keychain as `{id}` before this attempt could not be put \
             back ({err}) — the entry now holds the key you just typed, so a config pointing at \
             it is using the new key. Run `/provider setup {id}` again, or restore the entry with \
             `security add-generic-password -U -s {service} -a {id} -w`."
        ),
        Cleanup::LeftInPlace(why) => format!(
            "your keychain could not be read before this attempt ({why}), so the key you typed \
             was left in `{id}` rather than risk removing a credential your config still uses — \
             `security find-generic-password -s {service} -a {id}` shows what is there."
        ),
    }
}

/// What a commit that applied nothing says.
///
/// "Nothing changed" is true of the config and **false of the keychain**: this
/// run always stores a key, so a user rotating a credential against an otherwise
/// identical registration would otherwise be told their rotation did not happen.
///
/// It names the host for a reason the applied path does not need: the daemon
/// announces `provider_setup_completed` — the event that carries `dial_host` —
/// only for a commit that changed something, so this is the sole line an
/// unchanged commit prints and the only chance to say where the registration
/// that stands will be dialed. Empty means an older daemon did not say, and the
/// clause is dropped rather than rendered as a blank.
fn unchanged_line(id: &ProviderId, dial_host: &str) -> String {
    let dialed = if dial_host.is_empty() {
        String::new()
    } else {
        format!(" (dialed at `{dial_host}`)")
    };
    format!(
        "`{id}` was already registered exactly this way{dialed}, so your config is unchanged — the \
         key in your keychain as `{id}` was updated to the one you just typed."
    )
}

/// AC-13: the provider landed and nothing calls it, said plainly, with both ways
/// to route it later.
fn unrouted_line(id: &ProviderId) -> String {
    format!(
        "`{id}` is registered but unrouted: no tier calls it yet. `teton policy show` lists what \
         each tier routes to now, and `/provider setup {id}` — or `teton policy set-tier <tier> \
         {id}`, which also takes `--fallback` — routes it later."
    )
}

/// What a commit the daemon never answered says (BR-8).
///
/// The one honest sentence available: the write either landed or did not, this
/// process cannot tell, and there is a command that can. Everything about the
/// keychain is reported as *left alone* because that is what happened — the
/// alternative was to guess which of two destructive undos was right and be
/// wrong half the time.
fn ambiguous_commit_line(id: &str, prior: &PriorKey) -> String {
    let mut line = format!(
        "your config may or may not have been written — `teton provider list` says whether `{id}` \
         is registered."
    );
    if prior.displaced() {
        line.push_str(&format!(
            " The key you typed is in your keychain as `{id}`, in place of the one that was there \
             before, and was left there: taking it back out would break the registration if the \
             write did land."
        ));
    } else {
        line.push_str(&format!(
            " The key you typed is in your keychain as `{id}` and was left there: taking it back \
             out would break the registration if the write did land. If it did not, `security \
             delete-generic-password -s {service} -a {id}` removes it.",
            service = crate::keychain::SERVICE
        ));
    }
    line
}

/// The exact CLI recipe a non-TTY session gets instead of the walk (BR-11 /
/// AC-9). Pure.
///
/// Every line that *starts* with `teton ` is a command to run verbatim; the
/// placeholder forms live inside prose so nothing on screen looks runnable and
/// is not. `instructions_are_commands_the_cli_itself_parses` puts every runnable
/// line through the binary's own argument parser, which is what keeps this from
/// becoming a recipe that no longer exists.
pub(crate) fn instruction_lines(
    catalog: &[ProviderRecipeEntry],
    vendor: Option<&str>,
    tier: Option<&str>,
) -> Vec<String> {
    let tier = offered_tier_name(tier);
    match resolve_vendor(catalog, vendor) {
        Resolution::One(entry) => {
            let id = entry.id_suggestion.as_str();
            let mut lines = vec![format!(
                "run `teton` in a terminal and type `/provider setup {id} {tier}`, or run these \
                 two from a shell:"
            )];
            lines.push(format!("  {}", provider_add_line(entry)));
            lines.push(format!("  teton policy set-tier {tier} {id}"));
            lines.push(key_handling_line(id));
            lines
        }
        Resolution::Many(entries) => bare_recipe_lines(&entries, tier),
        Resolution::None => bare_recipe_lines(&catalog.iter().collect::<Vec<_>>(), tier),
    }
}

/// What the recipe quietly did with an argument it could not use (BR-11).
///
/// [`instruction_lines`] is total by construction: a vendor it cannot resolve
/// becomes the whole catalog and a tier it does not recognise becomes `think`.
/// Both are the right *output* and the wrong silence — the walk says each of
/// these out loud before it acts on them, and a user who is about to paste a
/// command has more need of the correction, not less, because nothing further
/// will ask them to confirm.
///
/// Pure, and asked of the same authorities the recipe itself consults: the
/// plan's catalog for the vendor, [`offered_tier_name`] for the tier.
fn argument_notices(
    plan: &ProviderSetupPlanResult,
    vendor: Option<&str>,
    tier: Option<&str>,
) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(typed) = vendor.map(str::trim).filter(|typed| !typed.is_empty()) {
        match resolve_vendor(&plan.catalog, Some(typed)) {
            Resolution::One(_) => {}
            // Both sentences end in "pick one from the list", and on this path
            // the list is the recipe printed directly below — every vendor with
            // its own runnable line.
            Resolution::Many(_) => lines.push(ambiguous_vendor_line(typed)),
            Resolution::None => lines.push(unknown_vendor_line(typed)),
        }
    }
    if let Some(typed) = tier.map(str::trim).filter(|typed| !typed.is_empty()) {
        let offered = offered_tier_name(tier);
        if !typed.eq_ignore_ascii_case(offered) {
            lines.push(unknown_tier_recipe_line(typed, offered));
        }
    }
    lines
}

/// The tier notice for the recipe path.
///
/// Distinct from [`unknown_tier_argument_line`] in one respect that matters:
/// there is no routing question here to offer a default *to*, so the sentence
/// names the command the user is about to run instead of a prompt they will
/// never see.
fn unknown_tier_recipe_line(typed: &str, offered: &str) -> String {
    format!(
        "`{}` is not a tier, so the commands below route `{offered}` instead.",
        crate::slash::echoed(typed)
    )
}

/// The one line a platform with no keychain gets that a piped session does not:
/// where the key goes instead (requirement Assumptions, BR-11).
///
/// The `env:<VAR>` form is not invented here — it is one of the three reference
/// schemes `teton_core::is_recognized_auth_ref` accepts and `Config::validate`
/// names in its own refusal, so a config written this way loads. The opening
/// clause is [`crate::keychain::KeychainError::Unsupported`]'s own words rather
/// than a second phrasing of them (LESSON-528).
fn out_of_band_key_line() -> String {
    format!(
        "{}; supply the key out of band instead — put `auth_ref = \"env:<VAR>\"` on the provider \
         in your config and export that variable in the daemon's environment.",
        crate::keychain::KeychainError::Unsupported
    )
}

/// The recipe list for a session that did not name one vendor: every entry's
/// registration command, and the routing step named once with placeholders.
fn bare_recipe_lines(entries: &[&ProviderRecipeEntry], tier: &str) -> Vec<String> {
    let mut lines = vec![
        "run `teton` in a terminal and type `/provider setup <vendor> [tier]`, or register one \
         from a shell:"
            .to_owned(),
    ];
    for entry in entries {
        lines.push(format!("  {}", provider_add_line(entry)));
    }
    lines.push(format!(
        "then route a tier to it with `teton policy set-tier {tier} <id>` — `teton policy show` \
         lists what each tier points at now."
    ));
    lines.push(key_handling_line("<id>"));
    lines
}

/// One `teton provider add` line, spelled exactly as the binary parses it.
fn provider_add_line(entry: &ProviderRecipeEntry) -> String {
    let mut line = format!(
        "teton provider add {id} --kind {kind}",
        id = entry.id_suggestion,
        kind = kind_flag(entry.kind),
    );
    if let Some(endpoint) = offered_endpoint(entry) {
        line.push_str(&format!(" --endpoint {endpoint}"));
    }
    line.push_str(&format!(" --model {}", entry.example_model));
    line
}

/// The line that says where the key goes and what must never carry it.
fn key_handling_line(id: &str) -> String {
    format!(
        "`teton provider add` reads the key itself, without echoing it, and files it in your OS \
         keychain as `{}` — never pass a key on the command line.",
        auth_ref_for(id)
    )
}

/// The tier an instruction line should name: the argument when it spells a
/// routable tier, else `think` (ADR-6).
///
/// The roster is `teton_core`'s own `Tier::ALL` — the list the daemon resolves
/// routing against and `teton policy show` renders — rather than a fourth copy
/// of four names living here (LESSON-528). The walk itself never reaches this:
/// there the roster is the *plan's* `tiers`, which is the daemon's live answer.
/// This is only for the non-TTY recipe, which has no plan to read.
fn offered_tier_name(tier: Option<&str>) -> &'static str {
    let Some(typed) = tier.map(str::trim).filter(|typed| !typed.is_empty()) else {
        return teton_core::Tier::Think.as_str();
    };
    teton_core::Tier::ALL
        .into_iter()
        .find(|tier| tier.as_str().eq_ignore_ascii_case(typed))
        .unwrap_or(teton_core::Tier::Think)
        .as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keychain::MockKeychain;
    use crate::prompt::ScriptedPrompter;
    use crate::render::{RecordingSurface, Rendered};
    // The recipe lines claim to be commands this binary runs, and the only way
    // to make that claim true is to hand them to the binary's own parser.
    use clap::Parser as _;
    use teton_protocol::methods::TierSummary;

    /// A planted credential: distinctive enough that a sweep over rendered lines
    /// and serialized frames means something (LESSON-519).
    const PLANTED_KEY: &str = "sk-planted-provider-setup-key";

    /// The credential a rotation displaces — the one the live config already
    /// references, and the one a refused commit owes the machine back.
    const PREVIOUS_KEY: &str = "sk-previous-provider-setup-key";

    /// The digest the canned preview offers, and therefore the one the commit
    /// must carry back.
    const PREVIEW_DIGEST: &str = "sha256:previewed-provider-document";

    /// The seam, wired to canned answers and a recording surface.
    struct FakeIo {
        surface: RecordingSurface,
        prompter: ScriptedPrompter,
        plan: Result<ProviderSetupPlanResult, RpcError>,
        preview: Result<ProviderSetupPreviewResult, RpcError>,
        commit: Result<ProviderSetupCommitResult, RpcError>,
        /// When set, `commit` fails at the **transport** level instead of
        /// answering. A different event from `commit = Err(RpcError)`: a daemon
        /// that answers "no" has certainly not written, while a daemon that does
        /// not answer may have written and died.
        commit_transport_error: Option<&'static str>,
        /// Every frame that crossed, kept as sent — the capture the "no key on
        /// the wire" sweep is asserted against.
        previews: Vec<ProviderSetupPreviewParams>,
        commits: Vec<ProviderSetupCommitParams>,
    }

    impl FakeIo {
        fn new(answers: &[&str]) -> Self {
            Self {
                surface: RecordingSurface::new(),
                prompter: ScriptedPrompter::new(answers),
                plan: Ok(fresh_plan()),
                preview: Ok(preview_result()),
                commit: Ok(commit_result(true, &[Tier::Think])),
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
            _params: ProviderSetupPlanParams,
        ) -> anyhow::Result<Result<ProviderSetupPlanResult, RpcError>> {
            Ok(self.plan.clone())
        }

        fn preview(
            &mut self,
            params: ProviderSetupPreviewParams,
        ) -> anyhow::Result<Result<ProviderSetupPreviewResult, RpcError>> {
            self.previews.push(params);
            Ok(self.preview.clone())
        }

        fn commit(
            &mut self,
            params: ProviderSetupCommitParams,
        ) -> anyhow::Result<Result<ProviderSetupCommitResult, RpcError>> {
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
        SessionId::from("sess-provider-setup")
    }

    /// The catalog as the daemon ships it, at the values REQ-577 pinned.
    ///
    /// **Nothing checks this copy against the daemon's.** It is a hand-written
    /// transcription of `tetond::provider_recipes::recipe_catalog()`; the
    /// daemon's own golden test is the authority for what the shipped strings
    /// *are*, and the e2e walkthrough is where the two ends meet. What this
    /// fixture pins is the resolver and the renderer, given a catalog.
    fn shipped_catalog() -> Vec<ProviderRecipeEntry> {
        vec![
            ProviderRecipeEntry {
                id_suggestion: "anthropic".to_owned(),
                label: "Anthropic".to_owned(),
                guide_spelling: "Anthropic".to_owned(),
                kind: ProviderKind::Anthropic,
                endpoint: None,
                example_model: "claude-opus-5".to_owned(),
                notes: None,
            },
            ProviderRecipeEntry {
                id_suggestion: "kimi".to_owned(),
                label: "Moonshot (Kimi)".to_owned(),
                guide_spelling: "Moonshot/Kimi".to_owned(),
                kind: ProviderKind::OpenaiCompatible,
                endpoint: Some("https://api.moonshot.ai/v1/chat/completions".to_owned()),
                example_model: "kimi-k3".to_owned(),
                notes: None,
            },
            ProviderRecipeEntry {
                id_suggestion: "deepseek".to_owned(),
                label: "DeepSeek".to_owned(),
                guide_spelling: "DeepSeek".to_owned(),
                kind: ProviderKind::OpenaiCompatible,
                endpoint: Some("https://api.deepseek.com/chat/completions".to_owned()),
                example_model: "deepseek-v4-pro".to_owned(),
                notes: Some("a sentence about pricing".to_owned()),
            },
        ]
    }

    /// A catalog whose two entries answer to the same word — the ambiguity the
    /// resolver has to report rather than resolve.
    fn colliding_catalog() -> Vec<ProviderRecipeEntry> {
        vec![
            ProviderRecipeEntry {
                id_suggestion: "sentinel-one".to_owned(),
                label: "Sentinel One".to_owned(),
                guide_spelling: "Sentinel/One".to_owned(),
                kind: ProviderKind::OpenaiCompatible,
                endpoint: Some("https://one.sentinel.example/v1/chat/completions".to_owned()),
                example_model: "sentinel-model-1".to_owned(),
                notes: None,
            },
            ProviderRecipeEntry {
                id_suggestion: "sentinel-two".to_owned(),
                label: "Sentinel Two".to_owned(),
                guide_spelling: "Sentinel/Two".to_owned(),
                kind: ProviderKind::OpenaiCompatible,
                endpoint: Some("https://two.sentinel.example/v1/chat/completions".to_owned()),
                example_model: "sentinel-model-2".to_owned(),
                notes: None,
            },
        ]
    }

    fn tiers() -> Vec<TierSummary> {
        vec![
            TierSummary {
                tier: Tier::Scan,
                provider_id: None,
                fallback_id: None,
            },
            // The one row with both a binding and a fallback — the shape the
            // daemon now preserves across a re-bind, and therefore the shape a
            // renderer has to survive being handed.
            TierSummary {
                tier: Tier::Build,
                provider_id: Some(ProviderId::from("opus")),
                fallback_id: Some(ProviderId::from("deepseek")),
            },
            TierSummary {
                tier: Tier::Think,
                provider_id: None,
                fallback_id: None,
            },
        ]
    }

    /// The plan a machine with no remote providers answers with — the fresh
    /// install this REQ exists for.
    fn fresh_plan() -> ProviderSetupPlanResult {
        ProviderSetupPlanResult {
            catalog: shipped_catalog(),
            existing: Vec::new(),
            tiers: tiers(),
        }
    }

    /// The host the canned preview and commit both report — the dial-time
    /// parser's reading, never the endpoint.
    const DIAL_HOST: &str = "api.moonshot.ai";

    /// The daemon's document, in the shape this project's config actually has:
    /// a `[[providers]]` row keyed by `auth_ref`, and a `[[tiers]]` row with
    /// `tier`/`provider_id`.
    ///
    /// It is rendered verbatim by [`render_preview`], so a fixture in a schema
    /// the daemon does not write would put an invented config on screen in the
    /// one test that claims to show the user what will be written.
    fn preview_result() -> ProviderSetupPreviewResult {
        ProviderSetupPreviewResult {
            toml: "[[providers]]\nid = \"kimi\"\nkind = \"openai-compatible\"\n\
                   endpoint = \"https://api.moonshot.ai/v1/chat/completions\"\n\
                   model = \"kimi-k3\"\nauth_ref = \"keychain://teton/kimi\"\n\
                   \n[[tiers]]\ntier = \"think\"\nprovider_id = \"kimi\"\n"
                .to_owned(),
            dial_host: DIAL_HOST.to_owned(),
            warnings: vec!["the price table does not know `kimi-k3`.".to_owned()],
            digest: PREVIEW_DIGEST.to_owned(),
            replaces: None,
        }
    }

    fn commit_result(applied: bool, bound: &[Tier]) -> ProviderSetupCommitResult {
        ProviderSetupCommitResult {
            applied,
            provider_id: ProviderId::from("kimi"),
            bindings: bound
                .iter()
                .map(|tier| TierBinding {
                    tier: *tier,
                    provider_id: ProviderId::from("kimi"),
                })
                .collect(),
            dial_host: DIAL_HOST.to_owned(),
        }
    }

    /// The daemon's refusal at the validator, carrying the code it actually
    /// answers with — a fixture that invented one would let a client that
    /// branched on the wrong number keep passing.
    fn refusal() -> RpcError {
        RpcError {
            code: error_code::PROVIDER_SETUP_INVALID,
            message: "the candidate provider would not validate: `kimi` has no model".to_owned(),
            data: None,
        }
    }

    /// The answers of a complete walk on a vendor the argument resolved: model,
    /// endpoint, key, routing, confirm. Five prompts, in that order.
    const FULL_WALK: &[&str] = &["", "", PLANTED_KEY, "", "y"];

    // -------------------------------------------------------------------
    // Vendor resolution (ADR-2)
    // -------------------------------------------------------------------

    /// ADR-2: the spellings a user and a model each already know all land on one
    /// entry, and a near-miss lands on none.
    #[test]
    fn every_spelling_of_one_vendor_resolves_to_that_vendor() {
        let catalog = shipped_catalog();
        for spelling in [
            "kimi",
            "Kimi",
            "KIMI",
            "moonshot",
            "Moonshot",
            "Moonshot (Kimi)",
            "Moonshot/Kimi",
            "moonshotkimi",
            "  kimi  ",
        ] {
            match resolve_vendor(&catalog, Some(spelling)) {
                Resolution::One(entry) => assert_eq!(
                    entry.id_suggestion, "kimi",
                    "`{spelling}` resolved to the wrong vendor"
                ),
                other => panic!("`{spelling}` did not resolve to one vendor: {other:?}"),
            }
        }
    }

    /// The resolver is lenient about *spelling*, never about *identity*: a
    /// prefix is not a match, because the value decides which vendor a key is
    /// about to be typed for.
    #[test]
    fn a_near_miss_resolves_to_nothing_rather_than_to_the_closest_vendor() {
        let catalog = shipped_catalog();
        for typed in ["deep", "moon", "anthro", "k", "kimi-k3", "zzz"] {
            assert!(
                matches!(resolve_vendor(&catalog, Some(typed)), Resolution::None),
                "`{typed}` must not resolve to a vendor"
            );
        }
    }

    /// AC-3's precondition: no argument is the same actionable state as an
    /// argument that matched nothing — the caller lists the catalog and asks.
    #[test]
    fn no_vendor_argument_resolves_to_none_so_the_caller_lists_the_catalog() {
        let catalog = shipped_catalog();
        assert!(matches!(resolve_vendor(&catalog, None), Resolution::None));
        assert!(matches!(
            resolve_vendor(&catalog, Some("")),
            Resolution::None
        ));
        assert!(matches!(
            resolve_vendor(&catalog, Some("   ")),
            Resolution::None
        ));
    }

    /// An ambiguous spelling is reported, never resolved to whichever entry came
    /// first — a silent pick here registers a key against the wrong vendor.
    #[test]
    fn a_spelling_two_vendors_answer_to_is_ambiguous_not_first_wins() {
        let catalog = colliding_catalog();
        match resolve_vendor(&catalog, Some("Sentinel")) {
            Resolution::Many(entries) => assert_eq!(entries.len(), 2),
            other => panic!("`Sentinel` must not resolve to one entry: {other:?}"),
        }
        // …and the unambiguous halves still resolve on their own.
        assert!(matches!(
            resolve_vendor(&catalog, Some("Sentinel/One")),
            Resolution::One(_)
        ));
    }

    // -------------------------------------------------------------------
    // The non-TTY recipe (BR-11 / AC-9)
    // -------------------------------------------------------------------

    /// BR-11: the recipe names the exact two commands, with the tier the
    /// hand-off carried.
    #[test]
    fn the_recipe_for_a_named_vendor_is_the_two_commands_that_do_the_job() {
        let rendered =
            instruction_lines(&shipped_catalog(), Some("kimi"), Some("think")).join("\n");
        assert!(
            rendered.contains(
                "teton provider add kimi --kind openai-compatible --endpoint \
                 https://api.moonshot.ai/v1/chat/completions --model kimi-k3"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains("teton policy set-tier think kimi"),
            "{rendered}"
        );
        // The key never rides a command line, and the recipe says where it does
        // go — instructions for a config that cannot be made to work are a dead
        // end, not a degradation.
        assert!(rendered.contains("keychain://teton/kimi"), "{rendered}");
        assert!(
            rendered.contains("never pass a key on the command line"),
            "{rendered}"
        );

        // The tier argument is carried through; absent, `think` is the default
        // offer (ADR-6).
        let build = instruction_lines(&shipped_catalog(), Some("kimi"), Some("build")).join("\n");
        assert!(
            build.contains("teton policy set-tier build kimi"),
            "{build}"
        );
        let bare = instruction_lines(&shipped_catalog(), Some("kimi"), None).join("\n");
        assert!(bare.contains("teton policy set-tier think kimi"), "{bare}");
        // A tier nobody routes is not invented into the line.
        let bogus = instruction_lines(&shipped_catalog(), Some("kimi"), Some("bogus")).join("\n");
        assert!(
            bogus.contains("teton policy set-tier think kimi"),
            "{bogus}"
        );
    }

    /// AC-3's degraded twin: with no vendor named, every catalog entry gets its
    /// own registration line rather than one invented default.
    #[test]
    fn the_recipe_without_a_vendor_names_every_vendor_the_daemon_sent() {
        let catalog = shipped_catalog();
        let rendered = instruction_lines(&catalog, None, None).join("\n");
        for entry in &catalog {
            assert!(
                rendered.contains(&format!("teton provider add {}", entry.id_suggestion)),
                "`{}` is in the catalog and not in the recipe:\n{rendered}",
                entry.id_suggestion
            );
            assert!(rendered.contains(&entry.example_model), "{rendered}");
        }
        // `anthropic` carries no endpoint of its own, and the line still names
        // the address Teton composes for it (ADR-7/ADR-8).
        assert!(
            rendered.contains("--kind anthropic --endpoint https://api.anthropic.com/v1/messages"),
            "{rendered}"
        );
    }

    /// **The recipe has to be a recipe.** Every runnable line is put through the
    /// binary's own argument parser, so a flag rename or a kind spelling that
    /// drifted fails here rather than in a user's shell (AC-9).
    #[test]
    fn instructions_are_commands_the_cli_itself_parses() {
        for (vendor, tier) in [
            (Some("kimi"), Some("think")),
            (Some("anthropic"), Some("build")),
            (Some("deepseek"), None),
            (None, None),
        ] {
            for line in instruction_lines(&shipped_catalog(), vendor, tier) {
                let line = line.trim();
                if !line.starts_with("teton ") {
                    continue;
                }
                let argv: Vec<&str> = line.split_whitespace().collect();
                crate::Cli::try_parse_from(&argv).unwrap_or_else(|err| {
                    panic!("the recipe line `{line}` is not a command this binary parses: {err}")
                });
            }
        }
    }

    /// Every kind the catalog can carry spells itself the way the CLI's own
    /// `--kind` parses it. Without this the exhaustive match is a table nobody
    /// checks.
    #[test]
    fn every_kind_spells_itself_the_way_the_cli_parses_it() {
        for kind in [
            ProviderKind::Local,
            ProviderKind::OpenaiCompatible,
            ProviderKind::Anthropic,
            ProviderKind::Custom,
        ] {
            let argv = [
                "teton",
                "provider",
                "add",
                "x",
                "--kind",
                kind_flag(kind),
                "--model",
                "m",
                "--endpoint",
                "https://x.example/v1/chat/completions",
            ];
            crate::Cli::try_parse_from(argv)
                .unwrap_or_else(|err| panic!("`--kind {}` does not parse: {err}", kind_flag(kind)));
        }
    }

    /// LESSON-517: the flow hands the seam plain text. A styled class composed by
    /// hand here would be printed literally by `defused`, or would put an escape
    /// sequence somewhere the sanitizer is not.
    #[test]
    fn nothing_this_module_composes_carries_an_escape_sequence() {
        let mut composed: Vec<String> = render_preview(&preview_result(), "kimi-k3");
        composed.extend(instruction_lines(
            &shipped_catalog(),
            Some("kimi"),
            Some("think"),
        ));
        composed.extend(instruction_lines(&shipped_catalog(), None, None));
        composed.extend(catalog_lines(&shipped_catalog().iter().collect::<Vec<_>>()));
        composed.extend(routing_lines(&fresh_plan(), "kimi", Tier::Think));
        composed.extend(argument_notices(&fresh_plan(), Some("nope"), Some("nope")));
        composed.push(out_of_band_key_line());
        composed.push(unrouted_line(&ProviderId::from("kimi")));
        composed.push(unchanged_line(&ProviderId::from("kimi"), DIAL_HOST));
        for line in composed {
            assert!(
                !line.chars().any(|c| c.is_control()),
                "a control character reached the surface: {line:?}"
            );
        }
    }

    /// The preview is the daemon's bytes and the daemon's host, laid out and
    /// otherwise untouched (BR-9, LESSON-529).
    #[test]
    fn the_preview_renders_the_daemons_own_bytes_and_host() {
        let mut preview = preview_result();
        preview.replaces = Some(ExistingProvider {
            id: ProviderId::from("kimi"),
            kind: ProviderKind::OpenaiCompatible,
            model: Some("kimi-k2".to_owned()),
        });
        let rendered = render_preview(&preview, "kimi-k3").join("\n");
        for line in preview.toml.lines().filter(|line| !line.is_empty()) {
            assert!(rendered.contains(line), "{rendered}");
        }
        assert!(rendered.contains("api.moonshot.ai"), "{rendered}");
        // BR-14: the replace is stated, never silent.
        assert!(
            rendered.contains("replaces existing provider `kimi`") && rendered.contains("kimi-k2"),
            "{rendered}"
        );
    }

    /// **AC-12's literal.** The replace line is the *transition*, old to new.
    ///
    /// Pinned as an exact string rather than by two `contains`: the criterion is
    /// about what a user reads before they confirm a rotation, and "both model
    /// names appear somewhere in the output" is satisfied by a line that says
    /// neither which is which nor which direction it goes.
    #[test]
    fn the_replace_line_names_both_models_old_to_new() {
        let mut preview = preview_result();
        preview.replaces = Some(ExistingProvider {
            id: ProviderId::from("kimi"),
            kind: ProviderKind::OpenaiCompatible,
            model: Some("kimi-k2".to_owned()),
        });
        let rendered = render_preview(&preview, "kimi-k3");
        let expected = "this replaces existing provider `kimi` (model `kimi-k2` → `kimi-k3`).";
        assert!(
            rendered.iter().any(|line| line == expected),
            "AC-12's line is not on screen: {rendered:#?}"
        );

        // The new model is the candidate's — the one the user just typed — and
        // not whatever the daemon's document happens to spell, which is what a
        // line built by parsing `toml` back out would have rendered.
        let renamed = render_preview(&preview, "kimi-k3-turbo").join("\n");
        assert!(
            renamed.contains("(model `kimi-k2` → `kimi-k3-turbo`)"),
            "{renamed}"
        );

        // A prior row with no model of its own is a real state, and `none →
        // kimi-k3` would read as a model called "none".
        preview.replaces = Some(ExistingProvider {
            id: ProviderId::from("kimi"),
            kind: ProviderKind::OpenaiCompatible,
            model: None,
        });
        let modelless = render_preview(&preview, "kimi-k3").join("\n");
        assert!(
            modelless.contains("which names no model") && modelless.contains("`kimi-k3`"),
            "{modelless}"
        );
        assert!(!modelless.contains("`none`"), "{modelless}");

        // And a preview that replaces nothing says nothing about a replacement.
        let fresh = render_preview(&preview_result(), "kimi-k3").join("\n");
        assert!(!fresh.contains("replaces"), "{fresh}");
    }

    // -------------------------------------------------------------------
    // The walk
    // -------------------------------------------------------------------

    /// AC-2 and AC-4, in one walk: the key reaches the keychain under the
    /// provider id, only its **reference** reaches the wire, and the previewed
    /// digest rides the commit.
    #[test]
    fn a_full_walk_stores_the_key_after_the_confirm_and_sends_only_its_reference() {
        let mut io = FakeIo::new(FULL_WALK);
        let keychain = MockKeychain::new();
        drive(
            &mut io,
            &keychain,
            &session(),
            Gate::Walk,
            Some("kimi"),
            Some("think"),
        )
        .unwrap();

        assert_eq!(keychain.stored_secret("kimi").as_deref(), Some(PLANTED_KEY));
        assert!(
            keychain.deletes().is_empty(),
            "a successful commit takes nothing back out"
        );
        assert_eq!(io.commits.len(), 1, "exactly one commit");
        let committed = &io.commits[0];
        assert_eq!(committed.candidate.key_ref, "keychain://teton/kimi");
        assert_eq!(committed.candidate.id, ProviderId::from("kimi"));
        assert_eq!(committed.candidate.model, "kimi-k3");
        assert_eq!(
            committed.candidate.endpoint.as_deref(),
            Some("https://api.moonshot.ai/v1/chat/completions")
        );
        assert_eq!(
            committed.candidate.bindings,
            vec![TierBinding {
                tier: Tier::Think,
                provider_id: ProviderId::from("kimi"),
            }]
        );
        // BR-9: the commit is pinned to the bytes the user just read.
        assert_eq!(committed.expect_digest.as_deref(), Some(PREVIEW_DIGEST));
        // The preview carried the same reference, which is what makes the bytes
        // the user confirmed the bytes the commit writes.
        assert_eq!(
            io.previews[0].candidate.key_ref,
            committed.candidate.key_ref
        );

        // And it was asked for through the hiding path, not the echoing one.
        assert_eq!(io.prompter.secrets.len(), 1);
        assert!(io.prompter.secrets[0].contains("API key"));

        let rendered = io.rendered();
        // Every daemon warning reaches the screen verbatim. They are the sentences
        // the client cannot compose — an unpriced model, a cleartext endpoint, the
        // replace — and a flow that dropped one would hide the very fact the
        // confirm is being asked about (BR-9).
        for warning in &preview_result().warnings {
            assert!(rendered.contains(warning.as_str()), "{rendered}");
        }
        // BR-7: the routing question is asked against the truth the daemon
        // reported, so a tier that already points somewhere says where.
        assert!(rendered.contains("build   now `opus`"), "{rendered}");
        assert!(rendered.contains("Enter takes this one"), "{rendered}");
    }

    /// BR-2 / AC-4, asserted by sweeping the real artifacts rather than by the
    /// absence of an error (LESSON-519): the credential is in no rendered line
    /// and in no RPC parameter, and what *is* on the wire is the reference.
    #[test]
    fn the_key_reaches_the_keychain_and_nothing_else() {
        let mut io = FakeIo::new(FULL_WALK);
        let keychain = MockKeychain::new();
        keychain.store("kimi", PREVIOUS_KEY).unwrap();
        drive(
            &mut io,
            &keychain,
            &session(),
            Gate::Walk,
            Some("kimi"),
            Some("think"),
        )
        .unwrap();

        let frames = io.frames();
        assert!(frames.contains("keychain://teton/kimi"), "{frames}");
        assert!(
            !frames.contains(PLANTED_KEY),
            "the credential crossed the socket: {frames}"
        );
        assert!(
            !frames.contains(PREVIOUS_KEY),
            "the displaced credential crossed the socket: {frames}"
        );
        let rendered = io.rendered();
        assert!(
            !rendered.contains(PLANTED_KEY) && !rendered.contains(PREVIOUS_KEY),
            "a credential was echoed back to the screen:\n{rendered}"
        );
        // Every captured question, too: a prompt that quoted the answer back
        // would be a leak the line sweep above cannot see.
        for question in &io.prompter.questions {
            assert!(!question.contains(PLANTED_KEY), "{question}");
        }
        // The redacting `Debug` is the last line of defence for a panic message.
        let answers = format!(
            "{:?}",
            Answers {
                id: "kimi".to_owned(),
                kind: ProviderKind::OpenaiCompatible,
                endpoint: None,
                model: "kimi-k3".to_owned(),
                key: PLANTED_KEY.to_owned(),
                bindings: Vec::new(),
            }
        );
        assert!(!answers.contains(PLANTED_KEY), "{answers}");
        assert!(answers.contains("redacted"), "{answers}");
    }

    /// AC-7: aborting at *any* prompt leaves the keychain empty and sends no
    /// commit. Driven by truncating the script one answer at a time, so a prompt
    /// added later is covered the moment its answer joins the list.
    #[test]
    fn an_abort_at_every_prompt_stores_nothing_and_commits_nothing() {
        for stop in 0..FULL_WALK.len() {
            let script: Vec<&str> = FULL_WALK[..stop].to_vec();
            let mut io = FakeIo::new(&script);
            let keychain = MockKeychain::new();
            drive(
                &mut io,
                &keychain,
                &session(),
                Gate::Walk,
                Some("kimi"),
                Some("think"),
            )
            .unwrap();

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

        // The prompts an empty answer *cancels*, rather than answering: the
        // model with no example to fall back on, and the key.
        for (stop, script) in [(0usize, vec![""]), (2, vec!["", "", ""])] {
            let mut io = FakeIo::new(&script);
            let mut plan = fresh_plan();
            // Blank the example so Enter has nothing to take at the model step.
            plan.catalog[1].example_model = String::new();
            io.plan = Ok(plan);
            let keychain = MockKeychain::new();
            drive(
                &mut io,
                &keychain,
                &session(),
                Gate::Walk,
                Some("kimi"),
                Some("think"),
            )
            .unwrap();
            assert!(keychain.is_empty(), "empty answer {stop} stored something");
            assert!(io.commits.is_empty(), "empty answer {stop} committed");
            assert!(io.rendered().contains(SETUP_ABORTED), "{}", io.rendered());
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
            let script: Vec<&str> = vec!["", "", PLANTED_KEY, "", answer];
            let mut io = FakeIo::new(&script);
            let keychain = MockKeychain::new();
            drive(
                &mut io,
                &keychain,
                &session(),
                Gate::Walk,
                Some("kimi"),
                Some("think"),
            )
            .unwrap();
            assert_eq!(
                !keychain.is_empty(),
                writes,
                "`{answer}` at the confirm stored the wrong thing"
            );
            assert_eq!(!io.commits.is_empty(), writes, "`{answer}` at the confirm");
            if !writes {
                assert!(io.rendered().contains(SETUP_DECLINED), "{}", io.rendered());
            }
        }
    }

    /// AC-8, fresh key: a refused commit removes the entry this run created, and
    /// says so.
    #[test]
    fn a_refused_commit_on_a_fresh_key_deletes_it_and_says_so() {
        let mut io = FakeIo::new(FULL_WALK);
        io.commit = Err(refusal());
        let keychain = MockKeychain::new();
        drive(
            &mut io,
            &keychain,
            &session(),
            Gate::Walk,
            Some("kimi"),
            Some("think"),
        )
        .unwrap();

        assert!(
            keychain.is_empty(),
            "the entry this run created must be gone"
        );
        assert_eq!(keychain.deletes(), vec!["kimi".to_owned()]);
        let rendered = io.rendered();
        assert!(rendered.contains("nothing was written"), "{rendered}");
        assert!(rendered.contains(&refusal().message), "{rendered}");
        assert!(
            rendered.contains("has been removed from your keychain"),
            "the outcome of the undo must be reported, not merely performed: {rendered}"
        );
    }

    /// AC-8, rotation: a refused commit puts back exactly the credential it
    /// displaced — a delete here would destroy a working registration the user
    /// never agreed to give up (BUG-171).
    #[test]
    fn a_refused_commit_after_a_rotation_puts_the_previous_key_back() {
        let mut io = FakeIo::new(FULL_WALK);
        io.commit = Err(refusal());
        let keychain = MockKeychain::new();
        keychain.store("kimi", PREVIOUS_KEY).unwrap();
        drive(
            &mut io,
            &keychain,
            &session(),
            Gate::Walk,
            Some("kimi"),
            Some("think"),
        )
        .unwrap();

        assert_eq!(
            keychain.stored_secret("kimi").as_deref(),
            Some(PREVIOUS_KEY),
            "the displaced credential must be back, byte for byte"
        );
        assert!(
            keychain.deletes().is_empty(),
            "a restore is a store, not a delete"
        );
        let rendered = io.rendered();
        assert!(
            rendered.contains("replaced with the one that was there before"),
            "{rendered}"
        );
        assert!(!rendered.contains(PREVIOUS_KEY), "{rendered}");
    }

    /// AC-8's unhappy half, fresh key: the daemon refuses **and** the cleanup
    /// the refusal triggers fails too.
    ///
    /// Both facts are the user's, and only the second comes with something they
    /// can do about it: they are the only one who can remove an entry this
    /// process could not, and an unreferenced credential they were never told
    /// about is exactly the residue the store-late ordering exists to avoid.
    #[test]
    fn a_refused_commit_whose_own_cleanup_also_fails_reports_both_failures() {
        let mut io = FakeIo::new(FULL_WALK);
        io.commit = Err(refusal());
        let keychain = MockKeychain::new();
        keychain.fail_delete_with("the keychain is locked");

        drive(
            &mut io,
            &keychain,
            &session(),
            Gate::Walk,
            Some("kimi"),
            Some("think"),
        )
        .unwrap();

        // The attempt was made and refused, so the entry is still there — which
        // is precisely why the user has to be told.
        assert_eq!(keychain.deletes(), vec!["kimi".to_owned()]);
        assert_eq!(keychain.stored_secret("kimi").as_deref(), Some(PLANTED_KEY));

        let rendered = io.rendered();
        assert!(
            rendered.contains(&refusal().message),
            "the daemon's own refusal must still reach the user: {rendered}"
        );
        assert!(
            rendered.contains("could not be removed from your keychain")
                && rendered.contains("the keychain is locked"),
            "the cleanup failure must be reported, not swallowed: {rendered}"
        );
        assert!(
            rendered.contains("security delete-generic-password -s teton -a kimi"),
            "the user is the only one who can finish this, so they get the command and the \
             account: {rendered}"
        );
        assert!(!rendered.contains(PLANTED_KEY), "{rendered}");
    }

    /// AC-8's unhappy half, rotation: the store could not find out what it was
    /// displacing, so a refused commit does **neither** undo and says which two
    /// it declined to guess between.
    ///
    /// A delete here might take out a credential the live config still uses, and
    /// there is nothing to restore — so the entry stays, and the sentence that
    /// says so carries the account and the command that shows what is in it.
    #[test]
    fn a_refused_commit_after_an_unreadable_keychain_leaves_the_entry_alone() {
        let mut io = FakeIo::new(FULL_WALK);
        io.commit = Err(refusal());
        let keychain = MockKeychain::new();
        keychain.store("kimi", PREVIOUS_KEY).unwrap();
        keychain.fail_read_with("the keychain is locked");

        drive(
            &mut io,
            &keychain,
            &session(),
            Gate::Walk,
            Some("kimi"),
            Some("think"),
        )
        .unwrap();

        assert!(
            keychain.deletes().is_empty(),
            "a delete against an unknown prior state is the destructive guess"
        );
        assert_eq!(
            keychain.stored_secret("kimi").as_deref(),
            Some(PLANTED_KEY),
            "the store still happened; it is the undo that was declined"
        );

        let rendered = io.rendered();
        assert!(
            rendered.contains("could not be read") && rendered.contains("the keychain is locked"),
            "the reason must reach the user: {rendered}"
        );
        assert!(
            rendered.contains("security find-generic-password -s teton -a kimi"),
            "and so must the way to look, naming the account: {rendered}"
        );
        assert!(
            !rendered.contains(PLANTED_KEY) && !rendered.contains(PREVIOUS_KEY),
            "{rendered}"
        );
    }

    /// BR-8's third state: a commit the daemon never answered licenses neither
    /// undo, and the user gets the ambiguity itself with the command that
    /// resolves it.
    #[test]
    fn a_commit_that_never_answered_leaves_the_keychain_alone_and_says_so() {
        let mut io = FakeIo::new(FULL_WALK);
        io.commit_transport_error = Some("the socket closed");
        let keychain = MockKeychain::new();
        drive(
            &mut io,
            &keychain,
            &session(),
            Gate::Walk,
            Some("kimi"),
            Some("think"),
        )
        .unwrap();

        assert_eq!(keychain.stored_secret("kimi").as_deref(), Some(PLANTED_KEY));
        assert!(keychain.deletes().is_empty());
        let rendered = io.rendered();
        assert!(rendered.contains("did not answer the commit"), "{rendered}");
        assert!(rendered.contains("teton provider list"), "{rendered}");
    }

    /// A refused **preview** never reaches the confirm, and therefore never
    /// stores anything: the store is past the commit point on purpose (BR-8).
    #[test]
    fn a_refused_preview_asks_for_no_confirmation_and_stores_nothing() {
        let mut io = FakeIo::new(FULL_WALK);
        io.preview = Err(refusal());
        let keychain = MockKeychain::new();
        drive(
            &mut io,
            &keychain,
            &session(),
            Gate::Walk,
            Some("kimi"),
            Some("think"),
        )
        .unwrap();

        assert!(keychain.is_empty() && keychain.deletes().is_empty());
        assert!(io.commits.is_empty());
        assert!(
            !io.prompter
                .questions
                .iter()
                .any(|question| question.contains("write this to your config")),
            "the confirm must not be asked about a preview the daemon refused"
        );
        assert!(
            io.rendered().contains("nothing was previewed"),
            "{}",
            io.rendered()
        );
    }

    /// ADR-7: an `anthropic` vendor is asked nothing about its address, and the
    /// composed default still reaches the candidate and the screen.
    #[test]
    fn an_anthropic_vendor_is_not_asked_for_an_endpoint() {
        let mut io = FakeIo::new(&["", PLANTED_KEY, "", "y"]);
        let keychain = MockKeychain::new();
        drive(
            &mut io,
            &keychain,
            &session(),
            Gate::Walk,
            Some("anthropic"),
            Some("think"),
        )
        .unwrap();

        assert_eq!(
            io.prompter.asked, 4,
            "an anthropic walk asks four questions, not five: {:?}",
            io.prompter.questions
        );
        assert!(
            !io.prompter
                .questions
                .iter()
                .any(|question| question.contains("endpoint")),
            "{:?}",
            io.prompter.questions
        );
        assert_eq!(
            io.commits[0].candidate.endpoint.as_deref(),
            Some("https://api.anthropic.com/v1/messages"),
            "the composed default is what would be persisted"
        );
        assert!(
            io.rendered()
                .contains("https://api.anthropic.com/v1/messages"),
            "the address is on screen before the key prompt: {}",
            io.rendered()
        );

        // And the openai-compatible walk *does* ask, which is what makes the
        // count above mean something.
        let mut io = FakeIo::new(FULL_WALK);
        let keychain = MockKeychain::new();
        drive(
            &mut io,
            &keychain,
            &session(),
            Gate::Walk,
            Some("kimi"),
            Some("think"),
        )
        .unwrap();
        assert_eq!(io.prompter.asked, 5);
    }

    /// REQ-578 BR-5 / AC-6: a pasted base URL is composed, echoed **before** the
    /// key prompt, and it is the composed value that is registered.
    #[test]
    fn a_pasted_base_url_is_composed_and_echoed_before_the_key_is_asked_for() {
        let mut io = FakeIo::new(&["", "https://api.moonshot.ai/v1", PLANTED_KEY, "", "y"]);
        let keychain = MockKeychain::new();
        drive(
            &mut io,
            &keychain,
            &session(),
            Gate::Walk,
            Some("kimi"),
            Some("think"),
        )
        .unwrap();

        assert_eq!(
            io.commits[0].candidate.endpoint.as_deref(),
            Some("https://api.moonshot.ai/v1/chat/completions")
        );
        let lines: Vec<&str> = io
            .surface
            .calls
            .iter()
            .filter_map(|call| match call {
                Rendered::Line(_, text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let echo = lines
            .iter()
            .position(|line| line.contains("endpoint stored as"))
            .expect("the composed endpoint must be echoed");
        assert!(
            lines[echo].contains("https://api.moonshot.ai/v1/chat/completions"),
            "{:?}",
            lines[echo]
        );
        // The echo is content this flow got from the shared seam, and it lands
        // before the key question — which is the whole of BR-5's ordering claim.
        assert!(
            io.prompter.questions[..2]
                .iter()
                .all(|question| !question.contains("API key")),
            "{:?}",
            io.prompter.questions
        );
        assert!(io.prompter.secrets.len() == 1);
    }

    /// AC-6's refusal half: a URL whose rendering and dialling would differ is
    /// refused at the **same seam** with the **same sentence** `teton provider
    /// add` gives, and the key is never asked for.
    #[test]
    fn a_backslash_authority_is_refused_with_the_shell_commands_own_sentence() {
        let mut io = FakeIo::new(&["", "https://evil.example\\@127.0.0.1/v1", PLANTED_KEY]);
        let keychain = MockKeychain::new();
        drive(
            &mut io,
            &keychain,
            &session(),
            Gate::Walk,
            Some("kimi"),
            Some("think"),
        )
        .unwrap();

        assert!(keychain.is_empty() && io.previews.is_empty() && io.commits.is_empty());
        assert!(
            io.prompter.secrets.is_empty(),
            "the key must not be asked for after the address was refused"
        );
        let rendered = io.rendered();
        assert!(
            rendered.contains("is not an absolute `http://` or `https://` URL with a host"),
            "{rendered}"
        );
        assert!(rendered.contains("no credential was read"), "{rendered}");
    }

    /// AC-3: with no vendor named, every catalog entry is listed and either a
    /// number or a lenient spelling selects one.
    #[test]
    fn a_walk_without_a_vendor_lists_the_catalog_and_takes_a_number_or_a_name() {
        for answer in ["2", "moonshot", "Moonshot/Kimi"] {
            let mut script = vec![answer];
            script.extend_from_slice(FULL_WALK);
            let mut io = FakeIo::new(&script);
            let keychain = MockKeychain::new();
            drive(
                &mut io,
                &keychain,
                &session(),
                Gate::Walk,
                None,
                Some("think"),
            )
            .unwrap();

            let rendered = io.rendered();
            for entry in shipped_catalog() {
                assert!(
                    rendered.contains(&entry.label),
                    "`{}` is in the catalog and not on screen:\n{rendered}",
                    entry.label
                );
            }
            assert!(rendered.contains("2) Moonshot (Kimi)"), "{rendered}");
            assert_eq!(
                io.commits.first().map(|c| c.candidate.id.0.as_str()),
                Some("kimi"),
                "`{answer}` did not select Moonshot (Kimi)"
            );
        }
    }

    /// An unresolvable argument falls back to the same list, and says why
    /// (AC-3's second sentence).
    #[test]
    fn an_unresolvable_vendor_argument_falls_back_to_the_catalog() {
        let mut script = vec!["kimi"];
        script.extend_from_slice(FULL_WALK);
        let mut io = FakeIo::new(&script);
        let keychain = MockKeychain::new();
        drive(
            &mut io,
            &keychain,
            &session(),
            Gate::Walk,
            Some("deep"),
            Some("think"),
        )
        .unwrap();

        let rendered = io.rendered();
        assert!(
            rendered.contains("`deep` is not a vendor this build knows"),
            "{rendered}"
        );
        assert!(rendered.contains("1) Anthropic"), "{rendered}");
        assert_eq!(io.commits[0].candidate.id, ProviderId::from("kimi"));
    }

    /// ADR-6: the tier argument is the pre-selected offer, and with no argument
    /// the offer is `think`.
    #[test]
    fn the_tier_argument_is_what_enter_takes_and_think_is_the_default() {
        for (arg, expected) in [
            (Some("build"), Tier::Build),
            (Some("scan"), Tier::Scan),
            (None, Tier::Think),
            // A tier the daemon does not list is reported and falls back.
            (Some("reflex"), Tier::Think),
            (Some("bogus"), Tier::Think),
        ] {
            let mut io = FakeIo::new(FULL_WALK);
            let keychain = MockKeychain::new();
            drive(
                &mut io,
                &keychain,
                &session(),
                Gate::Walk,
                Some("kimi"),
                arg,
            )
            .unwrap();

            assert_eq!(
                io.commits[0].candidate.bindings,
                vec![TierBinding {
                    tier: expected,
                    provider_id: ProviderId::from("kimi"),
                }],
                "the {arg:?} argument did not pre-select {expected}"
            );
            let routing = io
                .prompter
                .questions
                .iter()
                .find(|question| question.starts_with("  route"))
                .expect("the routing question is asked");
            assert!(
                routing.contains(expected.as_str()),
                "the question must name what Enter takes: {routing}"
            );
        }
    }

    /// The routing answer is read exactly: numbers, names, several at once, and
    /// a near-miss that is refused rather than guessed at.
    #[test]
    fn routing_answers_are_read_exactly() {
        for (answer, expected) in [
            ("1", vec![Tier::Scan]),
            ("build", vec![Tier::Build]),
            ("1,3", vec![Tier::Scan, Tier::Think]),
            ("scan think", vec![Tier::Scan, Tier::Think]),
            // A repeat is one binding, not two.
            ("think,3", vec![Tier::Think]),
        ] {
            let script: Vec<&str> = vec!["", "", PLANTED_KEY, answer, "y"];
            let mut io = FakeIo::new(&script);
            let keychain = MockKeychain::new();
            drive(
                &mut io,
                &keychain,
                &session(),
                Gate::Walk,
                Some("kimi"),
                Some("think"),
            )
            .unwrap();
            let bound: Vec<Tier> = io.commits[0]
                .candidate
                .bindings
                .iter()
                .map(|binding| binding.tier)
                .collect();
            assert_eq!(bound, expected, "`{answer}` was read wrongly");
        }

        // A tier nobody listed is refused, and refusing it aborts rather than
        // registering an unrouted provider the user did not ask for.
        let script: Vec<&str> = vec!["", "", PLANTED_KEY, "reflex", "y"];
        let mut io = FakeIo::new(&script);
        let keychain = MockKeychain::new();
        drive(
            &mut io,
            &keychain,
            &session(),
            Gate::Walk,
            Some("kimi"),
            Some("think"),
        )
        .unwrap();
        assert!(io.commits.is_empty() && keychain.is_empty());
        assert!(
            io.rendered().contains("is not one of the tiers"),
            "{}",
            io.rendered()
        );
    }

    /// AC-13: declining every binding still registers the provider, and the one
    /// line about it names both ways to route it later.
    #[test]
    fn declining_every_tier_registers_the_provider_and_says_it_is_unrouted() {
        let script: Vec<&str> = vec!["", "", PLANTED_KEY, "none", "y"];
        let mut io = FakeIo::new(&script);
        io.commit = Ok(commit_result(true, &[]));
        let keychain = MockKeychain::new();
        drive(
            &mut io,
            &keychain,
            &session(),
            Gate::Walk,
            Some("kimi"),
            Some("think"),
        )
        .unwrap();

        assert_eq!(io.commits.len(), 1, "the registration still happens");
        assert!(
            io.commits[0].candidate.bindings.is_empty(),
            "no binding may be invented for a user who declined every one"
        );
        assert_eq!(keychain.stored_secret("kimi").as_deref(), Some(PLANTED_KEY));

        let rendered = io.rendered();
        assert!(rendered.contains("registered but unrouted"), "{rendered}");
        assert!(rendered.contains("teton policy show"), "{rendered}");
        assert!(rendered.contains("/provider setup kimi"), "{rendered}");
    }

    /// A commit that changed nothing is not a failure, and the sentence has to
    /// be honest about the half that *did* move: the keychain.
    #[test]
    fn a_commit_that_applied_nothing_says_the_key_was_still_rotated() {
        let mut io = FakeIo::new(FULL_WALK);
        io.commit = Ok(commit_result(false, &[Tier::Think]));
        let keychain = MockKeychain::new();
        drive(
            &mut io,
            &keychain,
            &session(),
            Gate::Walk,
            Some("kimi"),
            Some("think"),
        )
        .unwrap();

        let rendered = io.rendered();
        assert!(
            rendered.contains("already registered exactly this way"),
            "{rendered}"
        );
        assert!(
            rendered.contains("was updated to the one you just typed"),
            "{rendered}"
        );
        // The daemon announces a completed setup only when the commit *changed*
        // something, so this line is the whole of what an unchanged commit
        // prints — and the destination belongs in it.
        assert!(
            rendered.contains("(dialed at `api.moonshot.ai`)"),
            "{rendered}"
        );

        // An older daemon that does not report a host renders no empty clause.
        let mut io = FakeIo::new(FULL_WALK);
        io.commit = Ok(ProviderSetupCommitResult {
            dial_host: String::new(),
            ..commit_result(false, &[Tier::Think])
        });
        let keychain = MockKeychain::new();
        drive(
            &mut io,
            &keychain,
            &session(),
            Gate::Walk,
            Some("kimi"),
            Some("think"),
        )
        .unwrap();
        let rendered = io.rendered();
        assert!(
            rendered.contains("already registered exactly this way, so your config is unchanged"),
            "{rendered}"
        );
        assert!(!rendered.contains("dialed at"), "{rendered}");
    }

    /// BR-14's early warning: an id that already exists is named before a key is
    /// typed for it.
    #[test]
    fn an_id_that_already_exists_is_named_before_the_key_prompt() {
        let mut io = FakeIo::new(FULL_WALK);
        io.plan = Ok(ProviderSetupPlanResult {
            existing: vec![ExistingProvider {
                id: ProviderId::from("kimi"),
                kind: ProviderKind::OpenaiCompatible,
                model: Some("kimi-k2".to_owned()),
            }],
            ..fresh_plan()
        });
        let keychain = MockKeychain::new();
        drive(
            &mut io,
            &keychain,
            &session(),
            Gate::Walk,
            Some("kimi"),
            Some("think"),
        )
        .unwrap();

        let rendered = io.rendered();
        assert!(
            rendered.contains("`kimi` is already registered"),
            "{rendered}"
        );
        assert!(rendered.contains("kimi-k2"), "{rendered}");
        assert!(
            rendered.contains("Nothing is written until you confirm"),
            "{rendered}"
        );
    }

    /// BR-9's guard is only real if the previewed digest actually rides the
    /// commit — and only when the daemon offered one. The degrade must also be
    /// **said**, before the confirm, because declining is the one act the flow
    /// offers a user who minds.
    #[test]
    fn an_absent_preview_digest_degrades_to_do_not_check_and_says_so() {
        let mut io = FakeIo::new(FULL_WALK);
        io.preview = Ok(ProviderSetupPreviewResult {
            digest: String::new(),
            ..preview_result()
        });
        let keychain = MockKeychain::new();
        drive(
            &mut io,
            &keychain,
            &session(),
            Gate::Walk,
            Some("kimi"),
            Some("think"),
        )
        .unwrap();
        assert_eq!(io.commits[0].expect_digest, None);
        assert!(io.rendered().contains(DIGEST_CHECK_UNAVAILABLE));

        // And the notice must not fire on a daemon that did offer one, or it
        // becomes noise nobody reads.
        let mut io = FakeIo::new(FULL_WALK);
        let keychain = MockKeychain::new();
        drive(
            &mut io,
            &keychain,
            &session(),
            Gate::Walk,
            Some("kimi"),
            Some("think"),
        )
        .unwrap();
        assert!(!io.rendered().contains(DIGEST_CHECK_UNAVAILABLE));
    }

    /// AC-9: a session whose input is not a terminal is told what to type, is
    /// asked nothing at all, and consumes no line that was meant for the session.
    #[test]
    fn a_piped_session_is_told_what_to_type_and_asked_nothing() {
        let mut io = FakeIo::new(&["kimi-k3", PLANTED_KEY]);
        let keychain = MockKeychain::new();
        drive(
            &mut io,
            &keychain,
            &session(),
            Gate::Instructions,
            Some("kimi"),
            Some("think"),
        )
        .unwrap();

        assert_eq!(
            io.prompter.asked, 0,
            "the instruction path must not read stdin"
        );
        assert!(io.prompter.secrets.is_empty());
        assert!(io.previews.is_empty() && io.commits.is_empty());
        assert!(keychain.is_empty() && keychain.deletes().is_empty());

        let rendered = io.rendered();
        assert!(rendered.contains(NOT_A_TERMINAL), "{rendered}");
        assert!(
            rendered.contains(
                "teton provider add kimi --kind openai-compatible --endpoint \
                 https://api.moonshot.ai/v1/chat/completions --model kimi-k3"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains("teton policy set-tier think kimi"),
            "{rendered}"
        );
    }

    /// The recipe path normalises what it cannot use — and says so.
    ///
    /// A vendor that resolves to nothing becomes the whole catalog and a tier
    /// that names no tier becomes `think`; both are the right commands and the
    /// wrong silence, because the user is about to *run* the answer with nothing
    /// further to confirm. The walk has always said both; this is the same two
    /// sentences on the path that has no prompt.
    #[test]
    fn a_piped_session_is_told_what_its_arguments_were_read_as() {
        let mut io = FakeIo::new(&[]);
        let keychain = MockKeychain::new();
        drive(
            &mut io,
            &keychain,
            &session(),
            Gate::Instructions,
            Some("kimmi"),
            Some("thonk"),
        )
        .unwrap();

        let rendered = io.rendered();
        assert_eq!(io.prompter.asked, 0);
        assert!(
            rendered.contains("`kimmi` is not a vendor this build knows"),
            "{rendered}"
        );
        assert!(
            rendered
                .contains("`thonk` is not a tier, so the commands below route `think` instead."),
            "{rendered}"
        );
        // Having said so, it still prints the recipe it settled on — every
        // vendor, and `think` in the routing line.
        assert!(
            rendered.contains("teton provider add kimi")
                && rendered.contains("teton provider add deepseek"),
            "{rendered}"
        );
        assert!(
            rendered.contains("teton policy set-tier think"),
            "{rendered}"
        );

        // A spelling that answers to two entries is the third case, and it is
        // reported rather than resolved to whichever came first.
        let mut io = FakeIo::new(&[]);
        io.plan = Ok(ProviderSetupPlanResult {
            catalog: colliding_catalog(),
            ..fresh_plan()
        });
        drive(
            &mut io,
            &keychain,
            &session(),
            Gate::Instructions,
            Some("sentinel"),
            Some("think"),
        )
        .unwrap();
        assert!(
            io.rendered()
                .contains("`sentinel` names more than one vendor"),
            "{}",
            io.rendered()
        );

        // And arguments the recipe *can* use draw no correction at all, or the
        // notice becomes noise on the path that prints it every time.
        let mut io = FakeIo::new(&[]);
        drive(
            &mut io,
            &keychain,
            &session(),
            Gate::Instructions,
            Some("Moonshot/Kimi"),
            Some("BUILD"),
        )
        .unwrap();
        let rendered = io.rendered();
        assert!(!rendered.contains("is not a tier"), "{rendered}");
        assert!(!rendered.contains("is not a vendor"), "{rendered}");
        assert!(
            rendered.contains("teton policy set-tier build kimi"),
            "{rendered}"
        );
    }

    /// **The requirement's keychain-availability posture.** A platform with no
    /// credential store is told so *before* the first question, gets the same
    /// CLI recipe a pipe gets, and is told the one thing the recipe cannot be
    /// true about: where the key goes instead.
    ///
    /// The order this is asked in is the whole fix. Availability was previously
    /// discovered by `store` failing — which is after the vendor, the model, the
    /// endpoint, the key and the confirm — so a Linux user typed a credential
    /// into a flow that could never have kept it. Asked at the gate, the walk
    /// never starts.
    #[test]
    fn a_platform_without_a_keychain_gets_the_recipe_and_the_env_form_and_is_asked_nothing() {
        let mut io = FakeIo::new(FULL_WALK);
        let keychain = MockKeychain::unavailable();
        drive(
            &mut io,
            &keychain,
            &session(),
            // The walk gate: this session *is* a terminal. The degradation is
            // the platform's, not the surface's.
            Gate::Walk,
            Some("kimi"),
            Some("think"),
        )
        .unwrap();

        assert_eq!(
            io.prompter.asked, 0,
            "no question may be put to a user whose answer has nowhere to go"
        );
        assert!(
            io.prompter.secrets.is_empty(),
            "and above all not the one that reads a credential"
        );
        assert!(io.previews.is_empty() && io.commits.is_empty());
        assert!(keychain.is_empty() && keychain.deletes().is_empty());

        let rendered = io.rendered();
        assert!(rendered.contains(NO_KEYCHAIN), "{rendered}");
        // BR-11's recipe, unchanged: the way this provider is registered on a
        // machine like this one.
        assert!(
            rendered.contains(
                "teton provider add kimi --kind openai-compatible --endpoint \
                 https://api.moonshot.ai/v1/chat/completions --model kimi-k3"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains("teton policy set-tier think kimi"),
            "{rendered}"
        );
        // And the correction the recipe needs here, in the reference form the
        // daemon's own validator accepts.
        assert!(
            rendered.contains("no OS keychain is available on this platform"),
            "{rendered}"
        );
        assert!(
            rendered.contains("auth_ref = \"env:<VAR>\""),
            "the out-of-band form must be spelled the way a config file takes it: {rendered}"
        );

        // The gate is a gate, not a ceiling: an available keychain still walks.
        let mut io = FakeIo::new(FULL_WALK);
        let keychain = MockKeychain::new();
        drive(
            &mut io,
            &keychain,
            &session(),
            Gate::Walk,
            Some("kimi"),
            Some("think"),
        )
        .unwrap();
        assert_eq!(io.commits.len(), 1, "an available keychain still commits");
        let rendered = io.rendered();
        assert!(!rendered.contains(NO_KEYCHAIN));
        assert!(
            !rendered.contains("env:<VAR>"),
            "a machine with a keychain is not told to work around one: {rendered}"
        );

        // A *piped* session on the same platform reaches the recipe by the other
        // door and is just as badly served by a line promising the key goes in a
        // keychain, so it gets the correction too.
        let mut io = FakeIo::new(&[]);
        let keychain = MockKeychain::unavailable();
        drive(
            &mut io,
            &keychain,
            &session(),
            Gate::Instructions,
            Some("kimi"),
            Some("think"),
        )
        .unwrap();
        let rendered = io.rendered();
        assert!(rendered.contains(NOT_A_TERMINAL), "{rendered}");
        assert!(rendered.contains("auth_ref = \"env:<VAR>\""), "{rendered}");

        // And a piped session on a machine that *has* one is unchanged from
        // before this fix — BR-11's output is a script's output.
        let mut io = FakeIo::new(&[]);
        let keychain = MockKeychain::new();
        drive(
            &mut io,
            &keychain,
            &session(),
            Gate::Instructions,
            Some("kimi"),
            Some("think"),
        )
        .unwrap();
        assert!(!io.rendered().contains("env:<VAR>"), "{}", io.rendered());
    }

    /// A daemon that predates the method says so and asks nothing, rather than
    /// walking a user into a flow with nowhere to commit.
    #[test]
    fn a_daemon_without_the_method_says_so_and_asks_nothing() {
        let mut io = FakeIo::new(FULL_WALK);
        io.plan = Err(RpcError {
            code: error_code::METHOD_NOT_FOUND,
            message: "no such method".to_owned(),
            data: None,
        });
        let keychain = MockKeychain::new();
        drive(
            &mut io,
            &keychain,
            &session(),
            Gate::Walk,
            Some("kimi"),
            Some("think"),
        )
        .unwrap();

        assert_eq!(io.prompter.asked, 0);
        assert!(keychain.is_empty() && io.previews.is_empty() && io.commits.is_empty());
        assert!(io.rendered().contains(SETUP_UNAVAILABLE));
        assert!(io.rendered().contains("teton provider add"));
    }

    /// A daemon that answered the plan with an error ends the command, not the
    /// session, and asks nothing.
    #[test]
    fn a_refused_plan_ends_the_command_and_asks_nothing() {
        let mut io = FakeIo::new(FULL_WALK);
        io.plan = Err(RpcError {
            code: -32001,
            message: "this connection did not open that session".to_owned(),
            data: None,
        });
        let keychain = MockKeychain::new();
        drive(
            &mut io,
            &keychain,
            &session(),
            Gate::Walk,
            Some("kimi"),
            Some("think"),
        )
        .unwrap();

        assert_eq!(io.prompter.asked, 0);
        assert!(keychain.is_empty() && io.commits.is_empty());
        assert!(
            io.rendered()
                .contains("provider setup could not start: this connection did not open"),
            "{}",
            io.rendered()
        );
    }

    /// A keychain that refuses the write stops the flow before the commit: a
    /// registration whose credential does not exist is a row that fails on first
    /// use, and writing it anyway would be worse than not registering.
    #[test]
    fn a_keychain_that_cannot_store_the_key_writes_no_config() {
        struct RefusingKeychain;
        impl Keychain for RefusingKeychain {
            fn store(
                &self,
                _account: &str,
                _secret: &str,
            ) -> Result<String, crate::keychain::KeychainError> {
                Err(crate::keychain::KeychainError::Backend(
                    "the keychain is locked".to_owned(),
                ))
            }
            fn read(
                &self,
                _account: &str,
            ) -> Result<Option<String>, crate::keychain::KeychainError> {
                Ok(None)
            }
            fn delete(&self, _account: &str) -> Result<(), crate::keychain::KeychainError> {
                Ok(())
            }
        }

        let mut io = FakeIo::new(FULL_WALK);
        drive(
            &mut io,
            &RefusingKeychain,
            &session(),
            Gate::Walk,
            Some("kimi"),
            Some("think"),
        )
        .unwrap();
        assert!(
            io.commits.is_empty(),
            "no config write without a stored key"
        );
        assert!(
            io.rendered()
                .contains("could not be stored in the OS keychain"),
            "{}",
            io.rendered()
        );
    }
}
