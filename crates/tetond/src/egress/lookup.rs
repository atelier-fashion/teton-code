//! The web-lookup seam at the choke point (REQ-563, architecture D-2/D-4/D-6).
//!
//! [`Egress::lookup`] is to a web lookup what [`Egress::send`](super::Egress::send)
//! is to a provider payload: the one place the bytes are inspected, the one
//! place they reach the wire, and the one place the attempt is recorded. The web
//! *tool* owns no HTTP client — it composes a [`LookupRequest`] and hands it
//! here — which is what makes "a lookup cannot bypass egress" the same
//! compile-time property provider calls already have (`deny_http_client`).
//!
//! ## The gates, in order
//!
//! 1. **Tier ceiling and allowlist are the caller's** (architecture D-5/D-9).
//!    This seam knows nothing about config shapes; the harness refuses an
//!    over-ceiling or off-allowlist destination before it ever gets here. What
//!    the seam does take is a per-hop [host check](LookupContext::new), because
//!    a *redirect* target is chosen by the destination rather than by the
//!    caller, and there is no earlier moment at which it could be checked.
//! 2. **Authorship / taint** (BR-13, D-4). A `ModelComposed` lookup in a
//!    session that has touched boundary content is refused with
//!    [`WebLookupOutcome::TaintRestricted`] and **zero** transport calls. A
//!    `UserPasted` URL proceeds: the user authored those bytes. The refusal
//!    lifts only through the session override, which is a client RPC — see
//!    [`TaintView::is_overridden`].
//! 3. **The search redaction gate** (BR-14, D-6). Every `Search` query is
//!    scanned before a byte leaves, whatever `[privacy] redact` says, and a
//!    search with **no gate installed at all** is refused rather than sent
//!    unscanned. `Fetch` is never scanned by this gate (BR-2's parity clause:
//!    fetch tiers get provider-payload parity, search gets the unconditional
//!    scan).
//! 4. **The wire**, through the transport this choke point already owns.
//!
//! The order is the same load-bearing one `send()` uses: the cheap refusal
//! returns before the model call, so a lookup nobody was going to allow costs
//! zero inferences (AC-11's argument, applied to the lookup path).
//!
//! ## What this seam does not do
//!
//! It runs **no provenance inspection**. For a provider payload the outgoing
//! bytes are an assembled context whose provenance is known; for a lookup they
//! are either the user's own paste (BR-13 exempts it) or text the model
//! composed — and the model's only route to boundary content is a context that
//! has already tainted the session, which gate 2 refuses. The taint split *is*
//! the provenance answer here (D-4), and adding a second one predicated on a
//! `Provenance` no caller can honestly supply would be a guard on a distinction
//! that cannot occur. [`WebLookupOutcome::BlockedPrivacy`] is still minted, but
//! from the one place it can genuinely arise: an inner transport that is itself
//! guarded and refuses.
//!
//! ## A blocked lookup does not taint the session
//!
//! Stated here and again at the emission site, because it is the kind of rule
//! that gets "fixed" into symmetry: a web `blocked_redact` publishes a
//! `web_lookup` event and nothing else. It does **not** publish a
//! `privacy_block`, and so it does not reach `TaintingPrivacySink` and does not
//! pin the session to the local tier. Taint semantics stay owned by that sink's
//! existing rules (REQ-544 C-2, REQ-562's cause gate). A query the user typed
//! and the daemon refused to send establishes nothing about the *context* the
//! session is holding — which is the fact a pin is a claim about.

use std::time::Instant;

use futures::StreamExt;

use teton_core::config::WebTier as CoreWebTier;
use teton_protocol::events::{BlockCause, WebLookupKind, WebLookupOutcome, WebTier as WireWebTier};
use teton_protocol::SessionId;
use teton_providers::transport::{
    ByteStream, HttpMethod, Transport, TransportError, TransportRequest,
};

use super::redact;
use super::{block_cause, Egress};

/// The most redirect hops a `Fetch` follows before it gives up (BR-2).
///
/// Three, and it is a bound rather than a preference: the transport refuses to
/// follow redirects itself (ADR-004), so every hop here is one this module
/// decided to take, and each one is a fresh destination the caller's host check
/// has to agree to. An unbounded loop against a server that redirects to itself
/// is a lookup that never ends; three is more than any documentation host uses
/// and small enough that the whole chain is legible in a ledger row.
pub const MAX_REDIRECT_HOPS: usize = 3;

/// The most content bytes a single lookup keeps (BR-10).
///
/// A cap on what is read off the wire, not a cap on what a scan may look at —
/// the scan caps are REQ-562's and are measured on the rendered prompt
/// (LESSON-491), which is a different quantity with a different reason. This one
/// exists because a `ByteStream` is a remote party's decision about how much
/// memory this daemon spends, and 2 MiB is far past any page the reducer will
/// keep while still being a bound.
pub const LOOKUP_MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Who authored the bytes about to leave (BR-13, architecture D-4).
///
/// The whole of the taint split rests on this one value, so it is a caller's
/// explicit claim rather than something this seam infers: only the harness knows
/// whether the URL appeared verbatim in a user message of this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Authorship {
    /// The user typed or pasted it. Their act is its own authorization, so a
    /// tainted session may still perform it — the bytes leaving are the user's
    /// own, not a model's paraphrase of something it read.
    UserPasted,
    /// The model composed it: a search query, or a URL that did not appear in
    /// this session's user messages. Refused for the rest of a tainted session
    /// unless the user lifts the restriction.
    ModelComposed,
}

/// What to look up.
///
/// `Debug` is **hand-written** for this type and for [`LookupRequest`]: a
/// derived one would print the query text and the full URL, and BR-7's rule that
/// neither ever reaches a log or an event is much easier to keep when the
/// obvious way to violate it does not compile into anything. The rendering names
/// the kind and nothing else.
#[derive(Clone, PartialEq, Eq)]
pub enum LookupKind {
    /// Retrieve one URL.
    Fetch {
        /// The absolute URL to retrieve.
        url: String,
    },
    /// Query the configured search backend.
    Search {
        /// The free-text query. Scanned before it leaves, always (BR-14).
        query: String,
    },
}

impl std::fmt::Debug for LookupKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LookupKind::Fetch { .. } => f.write_str("Fetch { url: <redacted> }"),
            LookupKind::Search { .. } => f.write_str("Search { query: <redacted> }"),
        }
    }
}

/// One lookup, as the harness hands it to the choke point.
#[derive(Clone, PartialEq, Eq)]
pub struct LookupRequest {
    /// Fetch or search.
    pub kind: LookupKind,
    /// Who authored the outgoing bytes (BR-13).
    pub authorship: Authorship,
}

impl std::fmt::Debug for LookupRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LookupRequest")
            .field("kind", &self.kind)
            .field("authorship", &self.authorship)
            .finish()
    }
}

impl LookupRequest {
    /// Fetch `url`, authored by `authorship`.
    #[must_use]
    pub fn fetch(url: impl Into<String>, authorship: Authorship) -> Self {
        Self {
            kind: LookupKind::Fetch { url: url.into() },
            authorship,
        }
    }

    /// Search for `query`, authored by `authorship`.
    #[must_use]
    pub fn search(query: impl Into<String>, authorship: Authorship) -> Self {
        Self {
            kind: LookupKind::Search {
                query: query.into(),
            },
            authorship,
        }
    }

    /// The wire twin of this request's kind.
    #[must_use]
    pub fn wire_kind(&self) -> WebLookupKind {
        match self.kind {
            LookupKind::Fetch { .. } => WebLookupKind::Fetch,
            LookupKind::Search { .. } => WebLookupKind::Search,
        }
    }
}

/// The two session flags the taint gate reads (BR-13).
///
/// A trait rather than a concrete handle for the same reason
/// [`PrivacyEventSink`](super::PrivacyEventSink) and
/// [`CostMeter`](crate::cost::CostMeter) are: the choke point must not depend on
/// the daemon runtime that owns the session state, and a test needs to state
/// "tainted, not overridden" without building one.
pub trait TaintView: Send + Sync {
    /// Whether `session` has touched `local-only` or unknown-provenance content
    /// (REQ-544 C-2's flag, read rather than written here — this gate never
    /// marks).
    fn is_tainted(&self, session: &SessionId) -> bool;

    /// Whether the user has lifted the restriction for `session`.
    ///
    /// **User-only by construction, not by check** (AC-12): the flag behind this
    /// is set from the `web/override` client RPC and from nowhere else, and tool
    /// dispatch has no path to a client RPC. A model-issued override is not
    /// rejected at runtime, it is unrepresentable.
    fn is_overridden(&self, session: &SessionId) -> bool;
}

/// Per-call context the [`LookupRequest`] cannot carry.
///
/// The host check is a **required** constructor argument rather than an optional
/// builder step. It is the only thing standing between a redirect and an
/// arbitrary destination, and a security check that can be left off by writing
/// less code is one that will be.
pub struct LookupContext<'a> {
    session_id: SessionId,
    taint: &'a dyn TaintView,
    host_check: &'a (dyn Fn(&str) -> bool + Send + Sync),
    search_endpoint: Option<&'a str>,
}

impl<'a> LookupContext<'a> {
    /// Context for `session_id`.
    ///
    /// `host_check` is consulted for **every redirect hop and no other URL**.
    /// Not for the initial destination: that one the caller already cleared
    /// against the tier and the allowlist, and BR-11 exempts a user-pasted URL
    /// from the allowlist entirely — so re-running the check here would refuse
    /// exactly the case the requirement exempts. A redirect target has no such
    /// exemption, because the user did not choose it and the model did not
    /// either: the *destination* did.
    pub fn new(
        session_id: impl Into<SessionId>,
        taint: &'a dyn TaintView,
        host_check: &'a (dyn Fn(&str) -> bool + Send + Sync),
    ) -> Self {
        Self {
            session_id: session_id.into(),
            taint,
            host_check,
            search_endpoint: None,
        }
    }

    /// The search backend's endpoint, from config.
    ///
    /// The backend's **key is deliberately not here**. It rides where every
    /// other credential in this daemon rides: resolved from its keychain
    /// reference as the choke point is built, attached by
    /// [`HttpTransport::with_endpoint_auth`](super::HttpTransport::with_endpoint_auth),
    /// and bound to this endpoint's origin so it cannot travel to any other host
    /// (BR-7, REQ-544 M-3). This module therefore never holds the secret, never
    /// logs it, and could not put it in an event if it tried.
    #[must_use]
    pub fn with_search_endpoint(mut self, endpoint: &'a str) -> Self {
        self.search_endpoint = Some(endpoint);
        self
    }

    /// The session this lookup belongs to.
    #[must_use]
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }
}

/// What ended the lookup, in finer grain than the wire outcome.
///
/// The wire [`WebLookupOutcome`] answers "did bytes leave, and what stopped
/// them" — it is a ledger vocabulary, fixed by the protocol. This answers "what
/// should the user be told", which is a different question with more answers.
/// The fold between them is stated once, at [`Ending`], and pinned by a test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LookupDetail {
    /// A 2xx answer with content.
    Delivered,
    /// The destination answered, and answered with an error status.
    ///
    /// **Not offline** (BUG-152's taxonomy). A 404 or a 503 is a settled fact
    /// about a host that is reachable; reporting it as "offline" sends the user
    /// to debug a network that is working.
    HttpStatus {
        /// The status the destination returned.
        status: u16,
    },
    /// The chain ran past [`MAX_REDIRECT_HOPS`].
    RedirectLimit,
    /// A redirect pointed somewhere the caller's host check refused, or
    /// somewhere with no readable host at all.
    RedirectRefused,
    /// Nothing answered: DNS, connect, or timeout.
    Unreachable {
        /// The transport's failure class. Carries no URL and no query — the
        /// taxonomy is closed and content-free by construction.
        error: TransportError,
    },
    /// A model-composed lookup in a tainted session, with no override (BR-13).
    TaintRestricted,
    /// An inspection refused the outgoing text.
    Blocked {
        /// Which inspection, in the same vocabulary a `privacy_block` uses —
        /// so the notice this becomes cannot disagree with the one a provider
        /// payload would have produced.
        cause: BlockCause,
    },
    /// The URL, or the configured search endpoint, could not be read as a URL
    /// with a host.
    Malformed,
    /// A `Search` arrived with no endpoint configured for this call.
    ///
    /// Folds onto [`WebLookupOutcome::RefusedTier`]: with no backend there is no
    /// search tier to perform, which is the same thing the ceiling says when it
    /// refuses one. Config validation makes this near-unreachable in production
    /// — `tier = "search"` without a `search_endpoint` is a startup error — so
    /// this is the seam refusing to assume that validation ran.
    SearchUnconfigured,
}

/// How a lookup ended, with the content when there is any.
///
/// `Debug` is hand-written: `body` is a remote document and `host` is the only
/// part of a destination that may be printed (BR-7).
#[derive(Clone, PartialEq, Eq)]
pub struct LookupOutcome {
    kind: WebLookupKind,
    host: String,
    outcome: WebLookupOutcome,
    bytes_in: u64,
    duration_ms: u64,
    detail: LookupDetail,
    body: Vec<u8>,
    truncated: bool,
}

impl std::fmt::Debug for LookupOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LookupOutcome")
            .field("kind", &self.kind)
            .field("host", &self.host)
            .field("outcome", &self.outcome)
            .field("bytes_in", &self.bytes_in)
            .field("duration_ms", &self.duration_ms)
            .field("detail", &self.detail)
            .field("truncated", &self.truncated)
            .field("body", &"<content>")
            .finish()
    }
}

impl LookupOutcome {
    /// The wire outcome — the same value the event and the ledger row carry.
    #[must_use]
    pub fn outcome(&self) -> WebLookupOutcome {
        self.outcome
    }

    /// Fetch or search.
    #[must_use]
    pub fn kind(&self) -> WebLookupKind {
        self.kind
    }

    /// The **last host this lookup actually sent bytes to**, or the empty string
    /// when it never reached one (a malformed URL, a refusal before the wire).
    ///
    /// "Last contacted" rather than "originally asked for" is the reading that
    /// stays true across a redirect chain in both directions: on a delivered
    /// fetch it is the host that served the content, and on a refused hop it is
    /// the host that did the redirecting — which is the one this session
    /// actually talked to.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Content bytes brought back; `0` for every ending that transferred none.
    #[must_use]
    pub fn bytes_in(&self) -> u64 {
        self.bytes_in
    }

    /// Wall-clock duration of the attempt, refusals included.
    #[must_use]
    pub fn duration_ms(&self) -> u64 {
        self.duration_ms
    }

    /// What ended it, in the seam's own vocabulary.
    #[must_use]
    pub fn detail(&self) -> &LookupDetail {
        &self.detail
    }

    /// The retrieved content, empty unless the lookup was delivered.
    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// Take the retrieved content.
    #[must_use]
    pub fn into_body(self) -> Vec<u8> {
        self.body
    }

    /// Whether [`LOOKUP_MAX_BODY_BYTES`] cut the content short.
    #[must_use]
    pub fn truncated(&self) -> bool {
        self.truncated
    }
}

/// One row's worth of what a lookup did — the *whole* of what leaves this seam
/// for the ledger and the event stream (BR-7).
///
/// There is no field here that could hold a query, a path, a full URL, or a
/// credential. That is the guarantee, expressed as a type rather than as a rule
/// a recorder is trusted to follow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookupRecord {
    /// Fetch or search.
    pub kind: WebLookupKind,
    /// The destination host, and nothing finer.
    pub host: String,
    /// How it ended.
    pub outcome: WebLookupOutcome,
    /// Content bytes brought back.
    pub bytes_in: u64,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
}

/// The seam the choke point calls once per lookup, whatever the ending.
///
/// One trait for both obligations — the `web_lookups` row (BR-7) and the
/// `web_lookup` event (D-8) — because "exactly one row and exactly one event per
/// attempt" is one invariant, and splitting it across two installable hooks
/// would make it two things that can be installed separately and disagree.
pub trait LookupRecorder: Send + Sync {
    /// Record and announce one completed attempt.
    fn web_lookup(&self, session_id: &SessionId, record: &LookupRecord);
}

/// A recorder that drops everything — for contexts with no ledger and no
/// subscribers.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopLookupRecorder;

impl LookupRecorder for NoopLookupRecorder {
    fn web_lookup(&self, _session_id: &SessionId, _record: &LookupRecord) {}
}

/// Map a `teton_core::config::WebTier` (the configured ceiling) to the
/// `teton_protocol::events::WebTier` carried on `web_consent_decided` and
/// `web_taint_overridden`.
///
/// Total, and the same boundary bridge
/// [`to_protocol_category`](crate::router::to_protocol_category) is — two enums,
/// one meaning, and the map stated once. Deliberately **one-way** for the same
/// reason that one is: the core ladder is what config and the gates read, and
/// nothing on the wire should be able to name a ceiling.
///
/// Both directions of drift are build failures rather than review catches: the
/// match below is exhaustive over the core ladder, so a variant added or renamed
/// there fails to compile here, and
/// [`tests::the_two_web_tier_ladders_mirror_each_other`] sweeps
/// [`WireWebTier::ALL`] through an exhaustive inverse, so a variant added or
/// renamed on the wire fails there.
#[must_use]
pub fn to_protocol_web_tier(tier: CoreWebTier) -> WireWebTier {
    match tier {
        CoreWebTier::Off => WireWebTier::Off,
        CoreWebTier::FetchUserUrl => WireWebTier::FetchUserUrl,
        CoreWebTier::FetchAnyUrl => WireWebTier::FetchAnyUrl,
        CoreWebTier::Search => WireWebTier::Search,
    }
}

/// How the attempt ended, before it becomes a [`LookupOutcome`].
///
/// ## The fold onto the wire vocabulary, stated once
///
/// [`WebLookupOutcome`] is fixed by the protocol at eight values (D-8), and the
/// seam distinguishes more endings than that. The rule is: **the wire outcome
/// answers whether bytes left and what stopped them, not whether the user got
/// what they wanted.**
///
/// * `Completed` — the destination answered. A 200 with content, a 404, a 500,
///   and a redirect chain that ran past its bound are all *answers*: the packet
///   went out, a host replied. `bytes_in` distinguishes the useful one.
/// * `Offline` — nothing answered (DNS, connect, timeout).
/// * `TaintRestricted`, `BlockedRedact`, `BlockedPrivacy`, `RefusedDomain`,
///   `RefusedTier` — nothing left at all, and each names which gate said so.
///
/// Folding an HTTP error into `Offline` would be the BUG-152 mistake in
/// miniature: a settled failure wearing a transient one's name, sending a user
/// to check their network because a page 404'd. The finer reading survives on
/// [`LookupOutcome::detail`], which is what the harness renders.
struct Ending {
    outcome: WebLookupOutcome,
    detail: LookupDetail,
    body: Vec<u8>,
    truncated: bool,
}

impl Ending {
    /// An ending that transferred nothing.
    fn refused(outcome: WebLookupOutcome, detail: LookupDetail) -> Self {
        Self {
            outcome,
            detail,
            body: Vec::new(),
            truncated: false,
        }
    }

    /// A 2xx answer with content.
    fn delivered(body: Vec<u8>, truncated: bool) -> Self {
        Self {
            outcome: WebLookupOutcome::Completed,
            detail: LookupDetail::Delivered,
            body,
            truncated,
        }
    }

    /// The destination answered with an error status — see the fold above for
    /// why this is `Completed` and not `Offline`.
    fn http_status(status: u16) -> Self {
        Self::refused(
            WebLookupOutcome::Completed,
            LookupDetail::HttpStatus { status },
        )
    }
}

impl<T: Transport> Egress<T> {
    /// Perform one web lookup (architecture D-2).
    ///
    /// Never returns an error: **a lookup failure is never a turn error**
    /// (BR-9). Every ending — delivered, refused by a gate, refused by the
    /// allowlist on a redirect, unreachable — comes back as a [`LookupOutcome`]
    /// the harness states to the model, and exactly one of them produces exactly
    /// one ledger row and one `web_lookup` event.
    ///
    /// ## Exactly one emission, structurally
    ///
    /// The gates and the wire live in [`Self::attempt`], which returns a host
    /// and an [`Ending`] and records nothing. The single call to the recorder is
    /// below, on the one path out of this function. "Exactly one row and one
    /// event per attempt" is therefore a property of the control flow rather
    /// than of remembering to call a helper on each of nine return sites.
    pub async fn lookup(&self, request: &LookupRequest, ctx: &LookupContext<'_>) -> LookupOutcome {
        let started = Instant::now();
        let kind = request.wire_kind();
        let (host, ending) = self.attempt(request, ctx).await;
        let bytes_in = ending.body.len() as u64;
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

        let record = LookupRecord {
            kind,
            host: host.clone(),
            outcome: ending.outcome,
            bytes_in,
            duration_ms,
        };
        // The one emission. A `BlockedRedact` here publishes a `web_lookup` and
        // NOTHING else — in particular no `privacy_block`, which is the event
        // `TaintingPrivacySink` turns into a session pin (REQ-544 C-2, REQ-562's
        // cause gate). Refusing to send a query establishes nothing about the
        // context this session is holding, and a pin is a claim about exactly
        // that; taint stays owned by the rules that already own it.
        if let Some(recorder) = &self.lookup_recorder {
            recorder.web_lookup(&ctx.session_id, &record);
        }

        LookupOutcome {
            kind,
            host,
            outcome: ending.outcome,
            bytes_in,
            duration_ms,
            detail: ending.detail,
            body: ending.body,
            truncated: ending.truncated,
        }
    }

    /// The gates and the wire. Records nothing — see [`Self::lookup`].
    ///
    /// Returns the last host this attempt actually contacted alongside the
    /// ending, because on a redirect chain those two are decided at different
    /// points and only the loop knows both.
    async fn attempt(&self, request: &LookupRequest, ctx: &LookupContext<'_>) -> (String, Ending) {
        // Gate 1 — authorship / taint (BR-13, D-4). First, and before anything
        // that costs a model call or a packet: a lookup this session may not
        // perform is refused for free. `UserPasted` never reaches the taint
        // read at all, which is the requirement's own asymmetry rather than an
        // optimization.
        if request.authorship == Authorship::ModelComposed
            && ctx.taint.is_tainted(&ctx.session_id)
            && !ctx.taint.is_overridden(&ctx.session_id)
        {
            return (
                self.intended_host(request, ctx),
                Ending::refused(
                    WebLookupOutcome::TaintRestricted,
                    LookupDetail::TaintRestricted,
                ),
            );
        }

        match &request.kind {
            LookupKind::Search { query } => self.search(query, ctx).await,
            LookupKind::Fetch { url } => self.fetch(url, ctx).await,
        }
    }

    /// The host a refusal *before the wire* names — the destination this lookup
    /// was going to reach, or the empty string when there is not one to name.
    fn intended_host(&self, request: &LookupRequest, ctx: &LookupContext<'_>) -> String {
        match &request.kind {
            LookupKind::Fetch { url } => host_of(url).unwrap_or_default(),
            LookupKind::Search { .. } => ctx.search_endpoint.and_then(host_of).unwrap_or_default(),
        }
    }

    /// Search: scan, then one request to the configured endpoint.
    ///
    /// No redirect loop. A search endpoint is a configured API, not a document
    /// tree, and following a redirect off it would carry the endpoint-bound key
    /// toward a host the user never named — the exact hazard ADR-004 closed for
    /// providers.
    async fn search(&self, query: &str, ctx: &LookupContext<'_>) -> (String, Ending) {
        let endpoint_host = ctx.search_endpoint.and_then(host_of).unwrap_or_default();

        // Gate 2 — the search redaction gate (BR-14, D-6), unconditional.
        //
        // The `None` arm is the load-bearing one. `[privacy] redact` does not
        // reach this gate: the daemon installs it whenever the configured tier
        // is `search`, and if it is somehow absent the query does **not** go out
        // unscanned. A guard that cannot run is a block, not a skip
        // (LESSON-492) — and here the guard is not merely stalled, it is
        // missing, which is the strongest version of the same fact.
        let Some(gate) = &self.search_redaction else {
            return (
                endpoint_host,
                Ending::refused(
                    WebLookupOutcome::BlockedRedact,
                    LookupDetail::Blocked {
                        cause: BlockCause::ScanUnavailable,
                    },
                ),
            );
        };

        // The scanned string and the string that goes on the wire are the same
        // `&str` — `search_request` percent-encodes *this* value and adds
        // nothing else the user authored — so there is no separate projection
        // that could drift from what is sent (LESSON-485). Percent-encoding is
        // a lossless transform of the same bytes, so a finding located here
        // locates the same secret leaving.
        //
        // The scan's own caps are REQ-562's and are measured on the *rendered*
        // prompt rather than on this text (LESSON-491); nothing here re-derives
        // them, which is why there is no second cap to keep in step.
        let verdict = gate.scan(query).await;
        if redact::decide(&verdict) == redact::EgressDecision::Block {
            return (
                endpoint_host,
                Ending::refused(
                    WebLookupOutcome::BlockedRedact,
                    LookupDetail::Blocked {
                        cause: block_cause(&verdict),
                    },
                ),
            );
        }
        // A forwarding verdict with low-confidence findings still goes to the
        // daemon log — kind, confidence and span, never text — for the same
        // reason `send()` does it: a finding computed and discarded is
        // indistinguishable from one never made (REQ-562 BR-4, ADR-4).
        for line in redact::forwarded_findings_report(&verdict) {
            eprintln!("{line}");
        }

        let Some(endpoint) = ctx.search_endpoint else {
            return (
                endpoint_host,
                Ending::refused(
                    WebLookupOutcome::RefusedTier,
                    LookupDetail::SearchUnconfigured,
                ),
            );
        };
        let Some(request) = search_request(endpoint, query) else {
            return (
                endpoint_host,
                Ending::refused(WebLookupOutcome::RefusedDomain, LookupDetail::Malformed),
            );
        };
        (endpoint_host, self.wire(request).await)
    }

    /// Fetch: a bounded, host-checked redirect loop over the transport.
    ///
    /// The transport never follows a redirect itself (ADR-004), so each hop is
    /// taken here, deliberately, and the caller's host check runs **before**
    /// every one of them. That ordering is the whole point: a check run after
    /// the request has gone out is a report, not a gate.
    async fn fetch(&self, url: &str, ctx: &LookupContext<'_>) -> (String, Ending) {
        let Some(mut host) = host_of(url) else {
            return (
                String::new(),
                Ending::refused(WebLookupOutcome::RefusedDomain, LookupDetail::Malformed),
            );
        };
        let mut current = url.to_owned();

        for hop in 0..=MAX_REDIRECT_HOPS {
            let request = TransportRequest {
                method: HttpMethod::Get,
                url: current.clone(),
                headers: vec![("accept".to_owned(), "text/html, text/*, */*".to_owned())],
                body: Vec::new(),
            };
            let response = match self.inner.execute(request).await {
                Ok(response) => response,
                Err(error) => {
                    return (host, Ending::refused(outcome_for(error), detail_for(error)))
                }
            };

            if !is_redirect(response.status) {
                if !(200..300).contains(&response.status) {
                    return (host, Ending::http_status(response.status));
                }
                return match drain_capped(response.body, LOOKUP_MAX_BODY_BYTES).await {
                    Ok((body, truncated)) => (host, Ending::delivered(body, truncated)),
                    // The stream broke after the status was known. Still an
                    // ending with no content, and still not a turn error.
                    Err(error) => (host, Ending::refused(outcome_for(error), detail_for(error))),
                };
            }

            // A redirect. `hop == MAX_REDIRECT_HOPS` means this response is the
            // (MAX+1)-th and following it would be one hop too many.
            if hop == MAX_REDIRECT_HOPS {
                return (
                    host,
                    Ending::refused(WebLookupOutcome::Completed, LookupDetail::RedirectLimit),
                );
            }
            // No `Location`, or one that does not resolve to a URL with a host:
            // there is nowhere to go. Reported as the redirect refusal it is
            // rather than as the status, because "this went somewhere
            // unreadable" and "this returned 302" are different things to be
            // told.
            let Some(next) = response
                .location
                .as_deref()
                .and_then(|loc| join(&current, loc))
            else {
                return (
                    host,
                    Ending::refused(
                        WebLookupOutcome::RefusedDomain,
                        LookupDetail::RedirectRefused,
                    ),
                );
            };
            let Some(next_host) = host_of(&next) else {
                return (
                    host,
                    Ending::refused(
                        WebLookupOutcome::RefusedDomain,
                        LookupDetail::RedirectRefused,
                    ),
                );
            };
            // The gate, before the hop. The destination chose this host, so
            // neither the tier grant nor BR-11's user-paste exemption covers it.
            if !(ctx.host_check)(&next_host) {
                return (
                    host,
                    Ending::refused(
                        WebLookupOutcome::RefusedDomain,
                        LookupDetail::RedirectRefused,
                    ),
                );
            }
            host = next_host;
            current = next;
        }

        // Unreachable: the loop returns on every path within `MAX_REDIRECT_HOPS
        // + 1` iterations. Stated as a refusal rather than an `unreachable!` so
        // a later edit to the loop bound degrades to a refusal instead of
        // panicking the daemon.
        (
            host,
            Ending::refused(WebLookupOutcome::Completed, LookupDetail::RedirectLimit),
        )
    }

    /// Execute one prepared request and read its answer (no redirect handling).
    async fn wire(&self, request: TransportRequest) -> Ending {
        let response = match self.inner.execute(request).await {
            Ok(response) => response,
            Err(error) => return Ending::refused(outcome_for(error), detail_for(error)),
        };
        if !(200..300).contains(&response.status) {
            return Ending::http_status(response.status);
        }
        match drain_capped(response.body, LOOKUP_MAX_BODY_BYTES).await {
            Ok((body, truncated)) => Ending::delivered(body, truncated),
            Err(error) => Ending::refused(outcome_for(error), detail_for(error)),
        }
    }
}

/// Whether `status` is a redirect this loop would follow.
fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// The wire outcome a transport failure folds onto.
///
/// [`TransportError::PrivacyBlocked`] is the one that is not a network fault:
/// it means an inner transport is itself guarded and refused, which is
/// `BlockedPrivacy` and not `Offline`. The production lookup transport is
/// unguarded, so this arm is a correctness statement about composition rather
/// than a path today — but reporting a refusal as "offline" is precisely the
/// mislabel BUG-152 is about, and the arm costs one line.
fn outcome_for(error: TransportError) -> WebLookupOutcome {
    match error {
        TransportError::PrivacyBlocked(_) => WebLookupOutcome::BlockedPrivacy,
        TransportError::Timeout | TransportError::Connect | TransportError::Io => {
            WebLookupOutcome::Offline
        }
    }
}

/// The seam-level detail a transport failure folds onto.
fn detail_for(error: TransportError) -> LookupDetail {
    match error {
        TransportError::PrivacyBlocked(detail) => LookupDetail::Blocked {
            cause: match detail {
                teton_providers::transport::BlockDetail::Boundary => BlockCause::Boundary,
                teton_providers::transport::BlockDetail::ScanUnavailable
                | teton_providers::transport::BlockDetail::Redaction => BlockCause::ScanUnavailable,
            },
        },
        other => LookupDetail::Unreachable { error: other },
    }
}

/// The host of `url`, or `None` when it does not parse to one.
fn host_of(url: &str) -> Option<String> {
    reqwest::Url::parse(url).ok()?.host_str().map(str::to_owned)
}

/// Resolve `location` against `base`, absolute or relative.
fn join(base: &str, location: &str) -> Option<String> {
    let base = reqwest::Url::parse(base).ok()?;
    base.join(location).ok().map(String::from)
}

/// The search request: the configured endpoint with the query appended as `q`.
///
/// There is no blessed search backend (BR-8), so a shape has to be assumed, and
/// this is the common one — a GET whose query string carries the terms. The
/// endpoint's own query parameters (an API version, a result count) survive:
/// `append_pair` adds to them rather than replacing them, so a user can
/// configure `…/search?count=5` and get both.
///
/// No credential header is built here. The key is attached by the transport,
/// bound to this endpoint's origin (see
/// [`LookupContext::with_search_endpoint`]).
fn search_request(endpoint: &str, query: &str) -> Option<TransportRequest> {
    let mut url = reqwest::Url::parse(endpoint).ok()?;
    url.host_str()?;
    url.query_pairs_mut().append_pair("q", query);
    Some(TransportRequest {
        method: HttpMethod::Get,
        url: String::from(url),
        headers: vec![("accept".to_owned(), "application/json".to_owned())],
        body: Vec::new(),
    })
}

/// Read at most `cap` bytes off `body`, reporting whether the cap cut it short.
///
/// Stops reading at the cap rather than reading on and discarding: the point is
/// to bound the memory a remote party can make this daemon spend, and a cap
/// enforced after the allocation is not one.
async fn drain_capped(mut body: ByteStream, cap: usize) -> Result<(Vec<u8>, bool), TransportError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        if bytes.len() + chunk.len() >= cap {
            let room = cap.saturating_sub(bytes.len());
            bytes.extend_from_slice(&chunk[..room.min(chunk.len())]);
            return Ok((bytes, true));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok((bytes, false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::redact::RedactionGate;
    use crate::egress::{NoopSink, RedactionVerdict};
    use async_trait::async_trait;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};
    use teton_core::entities::{BoundaryMode, PrivacyBoundary};
    use teton_providers::transport::TransportResponse;

    // -- fixtures ----------------------------------------------------------

    /// A transport that records every request and answers from a script.
    ///
    /// The **count** is the instrument for most of these tests: "zero transport
    /// calls" is the whole content of the taint and scan refusals, and a mock
    /// that only returned a canned answer could not tell a refusal from a
    /// permissive round trip (LESSON-485, the `CountingGate` argument applied to
    /// the wire).
    type ScriptedAnswer = Result<(u16, Option<String>, Vec<u8>), TransportError>;

    #[derive(Default, Clone)]
    struct CaptureTransport {
        sent: Arc<Mutex<Vec<TransportRequest>>>,
        script: Arc<Mutex<Vec<ScriptedAnswer>>>,
    }

    impl CaptureTransport {
        fn new(script: Vec<ScriptedAnswer>) -> Self {
            Self {
                sent: Arc::new(Mutex::new(Vec::new())),
                script: Arc::new(Mutex::new(script)),
            }
        }

        fn answering(status: u16, body: &str) -> Self {
            Self::new(vec![Ok((status, None, body.as_bytes().to_vec()))])
        }

        fn calls(&self) -> usize {
            self.sent.lock().unwrap().len()
        }

        fn urls(&self) -> Vec<String> {
            self.sent
                .lock()
                .unwrap()
                .iter()
                .map(|r| r.url.clone())
                .collect()
        }
    }

    #[async_trait]
    impl Transport for CaptureTransport {
        async fn execute(
            &self,
            request: TransportRequest,
        ) -> Result<TransportResponse, TransportError> {
            self.sent.lock().unwrap().push(request);
            let next = {
                let mut script = self.script.lock().unwrap();
                if script.is_empty() {
                    Ok((200, None, Vec::new()))
                } else {
                    script.remove(0)
                }
            };
            let (status, location, body) = next?;
            Ok(TransportResponse {
                status,
                location,
                body: Box::pin(futures::stream::once(async move { Ok(body) })),
            })
        }
    }

    /// A redaction gate that answers with a canned verdict and counts calls.
    struct CountingGate {
        verdict: RedactionVerdict,
        seen: Mutex<Vec<String>>,
    }

    impl CountingGate {
        fn new(verdict: RedactionVerdict) -> Arc<Self> {
            Arc::new(Self {
                verdict,
                seen: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> usize {
            self.seen.lock().unwrap().len()
        }

        fn payloads(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl RedactionGate for CountingGate {
        async fn scan(&self, payload: &str) -> RedactionVerdict {
            self.seen.lock().unwrap().push(payload.to_owned());
            self.verdict.clone()
        }
    }

    /// A gate that records the order in which it ran relative to the wire.
    struct OrderingGate {
        log: Arc<Mutex<Vec<&'static str>>>,
    }

    #[async_trait]
    impl RedactionGate for OrderingGate {
        async fn scan(&self, _payload: &str) -> RedactionVerdict {
            self.log.lock().unwrap().push("scan");
            RedactionVerdict::clean()
        }
    }

    /// A transport that appends to the same log the [`OrderingGate`] writes to.
    struct OrderingTransport {
        log: Arc<Mutex<Vec<&'static str>>>,
    }

    #[async_trait]
    impl Transport for OrderingTransport {
        async fn execute(
            &self,
            _request: TransportRequest,
        ) -> Result<TransportResponse, TransportError> {
            self.log.lock().unwrap().push("wire");
            Ok(TransportResponse {
                status: 200,
                location: None,
                body: Box::pin(futures::stream::empty()),
            })
        }
    }

    #[derive(Default)]
    struct Flags {
        tainted: HashSet<SessionId>,
        overridden: HashSet<SessionId>,
    }

    impl Flags {
        fn clean() -> Self {
            Self::default()
        }

        fn tainted() -> Self {
            Self {
                tainted: HashSet::from([SessionId::from("sess-1")]),
                overridden: HashSet::new(),
            }
        }

        fn tainted_and_overridden() -> Self {
            Self {
                tainted: HashSet::from([SessionId::from("sess-1")]),
                overridden: HashSet::from([SessionId::from("sess-1")]),
            }
        }
    }

    impl TaintView for Flags {
        fn is_tainted(&self, session: &SessionId) -> bool {
            self.tainted.contains(session)
        }

        fn is_overridden(&self, session: &SessionId) -> bool {
            self.overridden.contains(session)
        }
    }

    #[derive(Default)]
    struct CapturingRecorder {
        records: Mutex<Vec<(SessionId, LookupRecord)>>,
    }

    impl CapturingRecorder {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        fn records(&self) -> Vec<LookupRecord> {
            self.records
                .lock()
                .unwrap()
                .iter()
                .map(|(_, r)| r.clone())
                .collect()
        }
    }

    impl LookupRecorder for CapturingRecorder {
        fn web_lookup(&self, session_id: &SessionId, record: &LookupRecord) {
            self.records
                .lock()
                .unwrap()
                .push((session_id.clone(), record.clone()));
        }
    }

    fn allow_any_host(_host: &str) -> bool {
        true
    }

    fn deny_any_host(_host: &str) -> bool {
        false
    }

    fn boundaries() -> Vec<PrivacyBoundary> {
        vec![PrivacyBoundary {
            path_glob: "secrets/**".to_owned(),
            mode: BoundaryMode::LocalOnly,
        }]
    }

    fn egress_over(inner: CaptureTransport) -> Egress<CaptureTransport> {
        Egress::new(inner, boundaries(), Arc::new(NoopSink))
    }

    fn a_high_finding() -> RedactionVerdict {
        RedactionVerdict::from_findings(vec![redact::Finding::pattern(
            redact::FindingKind::Credential,
            0..12,
        )])
    }

    // -- the taint gate (BR-13, AC-12) -------------------------------------

    #[tokio::test]
    async fn a_model_composed_lookup_in_a_tainted_session_never_reaches_the_wire() {
        let inner = CaptureTransport::answering(200, "<html/>");
        let egress = egress_over(inner.clone());
        let flags = Flags::tainted();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host);

        let outcome = egress
            .lookup(
                &LookupRequest::fetch("https://docs.rs/x", Authorship::ModelComposed),
                &ctx,
            )
            .await;

        assert_eq!(outcome.outcome(), WebLookupOutcome::TaintRestricted);
        assert_eq!(outcome.detail(), &LookupDetail::TaintRestricted);
        assert_eq!(
            inner.calls(),
            0,
            "a refusal that reached the transport is not a refusal"
        );
        assert_eq!(
            outcome.host(),
            "docs.rs",
            "the refusal still names where it was headed"
        );
    }

    #[tokio::test]
    async fn a_user_pasted_url_survives_the_same_tainted_session() {
        // The asymmetry BR-13 is entirely about, in one session: the model's
        // destination is refused and the user's own is not.
        let inner = CaptureTransport::answering(200, "<html/>");
        let egress = egress_over(inner.clone());
        let flags = Flags::tainted();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host);

        let refused = egress
            .lookup(
                &LookupRequest::fetch("https://docs.rs/x", Authorship::ModelComposed),
                &ctx,
            )
            .await;
        assert_eq!(refused.outcome(), WebLookupOutcome::TaintRestricted);

        let allowed = egress
            .lookup(
                &LookupRequest::fetch("https://docs.rs/x", Authorship::UserPasted),
                &ctx,
            )
            .await;
        assert_eq!(allowed.outcome(), WebLookupOutcome::Completed);
        assert_eq!(inner.calls(), 1, "and exactly one of the two went out");
    }

    #[tokio::test]
    async fn the_session_override_restores_model_composed_lookups() {
        let inner = CaptureTransport::answering(200, "ok");
        let egress = egress_over(inner.clone());
        let flags = Flags::tainted_and_overridden();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host);

        let outcome = egress
            .lookup(
                &LookupRequest::fetch("https://docs.rs/x", Authorship::ModelComposed),
                &ctx,
            )
            .await;
        assert_eq!(outcome.outcome(), WebLookupOutcome::Completed);
        assert_eq!(inner.calls(), 1);
    }

    #[tokio::test]
    async fn an_untainted_session_is_never_restricted() {
        let inner = CaptureTransport::answering(200, "ok");
        let egress = egress_over(inner.clone());
        let flags = Flags::clean();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host);

        let outcome = egress
            .lookup(
                &LookupRequest::fetch("https://docs.rs/x", Authorship::ModelComposed),
                &ctx,
            )
            .await;
        assert_eq!(outcome.outcome(), WebLookupOutcome::Completed);
    }

    #[tokio::test]
    async fn a_taint_refused_search_costs_zero_scanner_calls() {
        // The order gate 1 / gate 2 is written in: the cheap refusal returns
        // before the model call, exactly as `send()`'s provenance check returns
        // before its scan (AC-11's argument on the lookup path).
        let inner = CaptureTransport::answering(200, "{}");
        let gate = CountingGate::new(RedactionVerdict::clean());
        let egress = egress_over(inner.clone()).with_search_redaction_gate(gate.clone());
        let flags = Flags::tainted();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host)
            .with_search_endpoint("https://search.example/api");

        let outcome = egress
            .lookup(
                &LookupRequest::search("anything", Authorship::ModelComposed),
                &ctx,
            )
            .await;

        assert_eq!(outcome.outcome(), WebLookupOutcome::TaintRestricted);
        assert_eq!(gate.calls(), 0);
        assert_eq!(inner.calls(), 0);
    }

    // -- the search redaction gate (BR-14, AC-13) --------------------------

    #[tokio::test]
    async fn every_search_query_is_scanned_and_no_fetch_is() {
        // BR-2's parity clause, by call count on one gate: search scans, fetch
        // does not, and the same installed gate proves both.
        let inner = CaptureTransport::new(vec![
            Ok((200, None, b"{}".to_vec())),
            Ok((200, None, b"<html/>".to_vec())),
        ]);
        let gate = CountingGate::new(RedactionVerdict::clean());
        let egress = egress_over(inner.clone()).with_search_redaction_gate(gate.clone());
        let flags = Flags::clean();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host)
            .with_search_endpoint("https://search.example/api");

        egress
            .lookup(
                &LookupRequest::search("rust lifetimes", Authorship::ModelComposed),
                &ctx,
            )
            .await;
        assert_eq!(gate.calls(), 1);
        assert_eq!(
            gate.payloads(),
            vec!["rust lifetimes".to_owned()],
            "the scanned string is the query itself, not a wrapper around it"
        );

        egress
            .lookup(
                &LookupRequest::fetch("https://docs.rs/x", Authorship::UserPasted),
                &ctx,
            )
            .await;
        assert_eq!(
            gate.calls(),
            1,
            "fetch is not this gate's business (BR-2 parity clause)"
        );
    }

    #[tokio::test]
    async fn a_high_finding_blocks_the_query_before_the_wire() {
        let inner = CaptureTransport::answering(200, "{}");
        let gate = CountingGate::new(a_high_finding());
        let egress = egress_over(inner.clone()).with_search_redaction_gate(gate.clone());
        let flags = Flags::clean();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host)
            .with_search_endpoint("https://search.example/api");

        let outcome = egress
            .lookup(
                &LookupRequest::search("sk-ant-0000000", Authorship::ModelComposed),
                &ctx,
            )
            .await;

        assert_eq!(outcome.outcome(), WebLookupOutcome::BlockedRedact);
        assert!(matches!(
            outcome.detail(),
            LookupDetail::Blocked {
                cause: BlockCause::Redaction { .. }
            }
        ));
        assert_eq!(gate.calls(), 1);
        assert_eq!(inner.calls(), 0, "and not a byte of it left");
    }

    #[tokio::test]
    async fn an_unavailable_verdict_blocks_the_query_rather_than_skipping_it() {
        // LESSON-492: a guard that cannot run is a block. The stalled engine
        // says nothing about the query, so the cause says `ScanUnavailable`
        // and never that something was found.
        let inner = CaptureTransport::answering(200, "{}");
        let gate = CountingGate::new(RedactionVerdict::unavailable());
        let egress = egress_over(inner.clone()).with_search_redaction_gate(gate);
        let flags = Flags::clean();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host)
            .with_search_endpoint("https://search.example/api");

        let outcome = egress
            .lookup(
                &LookupRequest::search("anything", Authorship::ModelComposed),
                &ctx,
            )
            .await;

        assert_eq!(outcome.outcome(), WebLookupOutcome::BlockedRedact);
        assert_eq!(
            outcome.detail(),
            &LookupDetail::Blocked {
                cause: BlockCause::ScanUnavailable
            }
        );
        assert_eq!(inner.calls(), 0);
    }

    #[tokio::test]
    async fn a_search_with_no_gate_installed_is_refused_rather_than_sent_unscanned() {
        // The strongest form of LESSON-492 and the one BR-14 turns on: search
        // egress is coupled to the scan by construction, so an absent gate is a
        // block. If this ever passes as `Completed`, a query has left unseen.
        let inner = CaptureTransport::answering(200, "{}");
        let egress = egress_over(inner.clone());
        let flags = Flags::clean();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host)
            .with_search_endpoint("https://search.example/api");

        let outcome = egress
            .lookup(
                &LookupRequest::search("anything", Authorship::UserPasted),
                &ctx,
            )
            .await;

        assert_eq!(outcome.outcome(), WebLookupOutcome::BlockedRedact);
        assert_eq!(
            outcome.detail(),
            &LookupDetail::Blocked {
                cause: BlockCause::ScanUnavailable
            }
        );
        assert_eq!(inner.calls(), 0);
    }

    #[tokio::test]
    async fn the_scan_runs_before_the_wire_not_after_it() {
        // Order, observed rather than argued: two participants writing to one
        // log. "Scanned before it leaves" is a claim about sequence, and a test
        // that only checked the block case would pass on an implementation that
        // sent first and scanned the copy.
        let log = Arc::new(Mutex::new(Vec::new()));
        let egress = Egress::new(
            OrderingTransport { log: log.clone() },
            boundaries(),
            Arc::new(NoopSink),
        )
        .with_search_redaction_gate(Arc::new(OrderingGate { log: log.clone() }));
        let flags = Flags::clean();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host)
            .with_search_endpoint("https://search.example/api");

        egress
            .lookup(
                &LookupRequest::search("rust", Authorship::ModelComposed),
                &ctx,
            )
            .await;

        assert_eq!(log.lock().unwrap().as_slice(), ["scan", "wire"]);
    }

    #[tokio::test]
    async fn a_search_with_no_endpoint_is_refused_after_the_scan_and_before_the_wire() {
        let inner = CaptureTransport::answering(200, "{}");
        let gate = CountingGate::new(RedactionVerdict::clean());
        let egress = egress_over(inner.clone()).with_search_redaction_gate(gate.clone());
        let flags = Flags::clean();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host);

        let outcome = egress
            .lookup(&LookupRequest::search("rust", Authorship::UserPasted), &ctx)
            .await;

        assert_eq!(outcome.outcome(), WebLookupOutcome::RefusedTier);
        assert_eq!(outcome.detail(), &LookupDetail::SearchUnconfigured);
        assert_eq!(inner.calls(), 0);
    }

    #[tokio::test]
    async fn the_query_rides_the_configured_endpoint_and_keeps_its_parameters() {
        let inner = CaptureTransport::answering(200, "{}");
        let gate = CountingGate::new(RedactionVerdict::clean());
        let egress = egress_over(inner.clone()).with_search_redaction_gate(gate);
        let flags = Flags::clean();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host)
            .with_search_endpoint("https://search.example/api?count=5");

        egress
            .lookup(
                &LookupRequest::search("a b&c", Authorship::ModelComposed),
                &ctx,
            )
            .await;

        let urls = inner.urls();
        assert_eq!(urls.len(), 1);
        assert!(urls[0].starts_with("https://search.example/api?count=5&q="));
        assert!(
            urls[0].contains("a+b%26c") || urls[0].contains("a%20b%26c"),
            "the query is percent-encoded onto the endpoint: {}",
            urls[0]
        );
    }

    // -- the redirect loop --------------------------------------------------

    #[tokio::test]
    async fn a_redirect_is_followed_only_after_the_host_check_agrees() {
        let inner = CaptureTransport::new(vec![
            Ok((
                301,
                Some("https://elsewhere.example/doc".to_owned()),
                Vec::new(),
            )),
            Ok((200, None, b"landed".to_vec())),
        ]);
        let egress = egress_over(inner.clone());
        let flags = Flags::clean();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host);

        let outcome = egress
            .lookup(
                &LookupRequest::fetch("https://docs.rs/x", Authorship::UserPasted),
                &ctx,
            )
            .await;

        assert_eq!(outcome.outcome(), WebLookupOutcome::Completed);
        assert_eq!(outcome.body(), b"landed");
        assert_eq!(
            outcome.host(),
            "elsewhere.example",
            "the recorded host is the one that answered"
        );
        assert_eq!(inner.urls().len(), 2);
    }

    #[tokio::test]
    async fn a_refused_hop_is_never_requested() {
        // The check is a gate, not a report: the second URL must not appear in
        // the transport's record at all.
        let inner = CaptureTransport::new(vec![Ok((
            302,
            Some("https://evil.example/doc".to_owned()),
            Vec::new(),
        ))]);
        let egress = egress_over(inner.clone());
        let flags = Flags::clean();
        let ctx = LookupContext::new("sess-1", &flags, &deny_any_host);

        let outcome = egress
            .lookup(
                &LookupRequest::fetch("https://docs.rs/x", Authorship::UserPasted),
                &ctx,
            )
            .await;

        assert_eq!(outcome.outcome(), WebLookupOutcome::RefusedDomain);
        assert_eq!(outcome.detail(), &LookupDetail::RedirectRefused);
        assert_eq!(inner.urls(), vec!["https://docs.rs/x".to_owned()]);
        assert_eq!(
            outcome.host(),
            "docs.rs",
            "and the host recorded is the one actually contacted"
        );
    }

    #[tokio::test]
    async fn the_initial_url_is_not_subject_to_the_per_hop_host_check() {
        // BR-11: a user-pasted URL is exempt from the allowlist. Re-running the
        // caller's check on hop zero would refuse exactly that case.
        let inner = CaptureTransport::answering(200, "ok");
        let egress = egress_over(inner.clone());
        let flags = Flags::clean();
        let ctx = LookupContext::new("sess-1", &flags, &deny_any_host);

        let outcome = egress
            .lookup(
                &LookupRequest::fetch("https://docs.rs/x", Authorship::UserPasted),
                &ctx,
            )
            .await;

        assert_eq!(outcome.outcome(), WebLookupOutcome::Completed);
        assert_eq!(inner.calls(), 1);
    }

    #[tokio::test]
    async fn a_relative_redirect_resolves_against_the_current_url() {
        let inner = CaptureTransport::new(vec![
            Ok((302, Some("/moved".to_owned()), Vec::new())),
            Ok((200, None, b"ok".to_vec())),
        ]);
        let egress = egress_over(inner.clone());
        let flags = Flags::clean();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host);

        egress
            .lookup(
                &LookupRequest::fetch("https://docs.rs/a/b", Authorship::UserPasted),
                &ctx,
            )
            .await;

        assert_eq!(inner.urls()[1], "https://docs.rs/moved");
    }

    #[tokio::test]
    async fn the_redirect_chain_is_bounded_at_three_hops() {
        let inner = CaptureTransport::new(vec![
            Ok((302, Some("https://a.example/1".to_owned()), Vec::new())),
            Ok((302, Some("https://a.example/2".to_owned()), Vec::new())),
            Ok((302, Some("https://a.example/3".to_owned()), Vec::new())),
            Ok((302, Some("https://a.example/4".to_owned()), Vec::new())),
            Ok((200, None, b"never".to_vec())),
        ]);
        let egress = egress_over(inner.clone());
        let flags = Flags::clean();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host);

        let outcome = egress
            .lookup(
                &LookupRequest::fetch("https://docs.rs/x", Authorship::UserPasted),
                &ctx,
            )
            .await;

        assert_eq!(outcome.detail(), &LookupDetail::RedirectLimit);
        assert_eq!(
            inner.calls(),
            MAX_REDIRECT_HOPS + 1,
            "the original request plus at most three hops"
        );
        assert_eq!(outcome.bytes_in(), 0);
    }

    #[tokio::test]
    async fn a_redirect_with_no_location_stops_the_chain() {
        let inner = CaptureTransport::new(vec![Ok((302, None, Vec::new()))]);
        let egress = egress_over(inner.clone());
        let flags = Flags::clean();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host);

        let outcome = egress
            .lookup(
                &LookupRequest::fetch("https://docs.rs/x", Authorship::UserPasted),
                &ctx,
            )
            .await;

        assert_eq!(outcome.detail(), &LookupDetail::RedirectRefused);
        assert_eq!(inner.calls(), 1);
    }

    // -- offline and HTTP status (BR-9, AC-8, BUG-152) ---------------------

    #[tokio::test]
    async fn a_connect_failure_is_offline_and_not_a_turn_error() {
        for error in [
            TransportError::Connect,
            TransportError::Timeout,
            TransportError::Io,
        ] {
            let inner = CaptureTransport::new(vec![Err(error)]);
            let egress = egress_over(inner.clone());
            let flags = Flags::clean();
            let ctx = LookupContext::new("sess-1", &flags, &allow_any_host);

            let outcome = egress
                .lookup(
                    &LookupRequest::fetch("https://docs.rs/x", Authorship::UserPasted),
                    &ctx,
                )
                .await;

            assert_eq!(outcome.outcome(), WebLookupOutcome::Offline);
            assert_eq!(outcome.detail(), &LookupDetail::Unreachable { error });
            assert_eq!(outcome.bytes_in(), 0);
        }
    }

    #[tokio::test]
    async fn an_http_error_status_is_not_reported_as_offline() {
        // BUG-152's taxonomy. A host that answered 404 is reachable; calling it
        // "offline" sends the user to debug a network that is working.
        let inner = CaptureTransport::new(vec![Ok((404, None, b"nope".to_vec()))]);
        let egress = egress_over(inner.clone());
        let flags = Flags::clean();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host);

        let outcome = egress
            .lookup(
                &LookupRequest::fetch("https://docs.rs/x", Authorship::UserPasted),
                &ctx,
            )
            .await;

        assert_ne!(outcome.outcome(), WebLookupOutcome::Offline);
        assert_eq!(outcome.detail(), &LookupDetail::HttpStatus { status: 404 });
        assert_eq!(
            outcome.bytes_in(),
            0,
            "an error page is not content this lookup brought back"
        );
    }

    // -- recording (BR-7, AC-6) --------------------------------------------

    #[tokio::test]
    async fn every_ending_records_exactly_one_row_and_names_only_the_host() {
        // The sweep is the point: each of these is a different return path
        // through `attempt`, and every one of them has to come back through the
        // single emission site.
        let flags_clean = Flags::clean();
        let flags_tainted = Flags::tainted();

        struct Case {
            name: &'static str,
            script: Vec<ScriptedAnswer>,
            gate: Option<RedactionVerdict>,
            endpoint: Option<&'static str>,
            search: bool,
            tainted: bool,
            host_check: fn(&str) -> bool,
            expected: WebLookupOutcome,
        }

        let cases = vec![
            Case {
                name: "delivered fetch",
                script: vec![Ok((200, None, b"hello".to_vec()))],
                gate: None,
                endpoint: None,
                search: false,
                tainted: false,
                host_check: allow_any_host,
                expected: WebLookupOutcome::Completed,
            },
            Case {
                name: "http error",
                script: vec![Ok((500, None, Vec::new()))],
                gate: None,
                endpoint: None,
                search: false,
                tainted: false,
                host_check: allow_any_host,
                expected: WebLookupOutcome::Completed,
            },
            Case {
                name: "offline",
                script: vec![Err(TransportError::Connect)],
                gate: None,
                endpoint: None,
                search: false,
                tainted: false,
                host_check: allow_any_host,
                expected: WebLookupOutcome::Offline,
            },
            Case {
                name: "taint restricted",
                script: vec![],
                gate: None,
                endpoint: None,
                search: false,
                tainted: true,
                host_check: allow_any_host,
                expected: WebLookupOutcome::TaintRestricted,
            },
            Case {
                name: "refused redirect",
                script: vec![Ok((
                    302,
                    Some("https://evil.example/x".to_owned()),
                    Vec::new(),
                ))],
                gate: None,
                endpoint: None,
                search: false,
                tainted: false,
                host_check: deny_any_host,
                expected: WebLookupOutcome::RefusedDomain,
            },
            Case {
                name: "blocked search",
                script: vec![],
                gate: Some(a_high_finding()),
                endpoint: Some("https://search.example/api"),
                search: true,
                tainted: false,
                host_check: allow_any_host,
                expected: WebLookupOutcome::BlockedRedact,
            },
            Case {
                name: "unconfigured search",
                script: vec![],
                gate: Some(RedactionVerdict::clean()),
                endpoint: None,
                search: true,
                tainted: false,
                host_check: allow_any_host,
                expected: WebLookupOutcome::RefusedTier,
            },
            Case {
                name: "delivered search",
                script: vec![Ok((200, None, b"{}".to_vec()))],
                gate: Some(RedactionVerdict::clean()),
                endpoint: Some("https://search.example/api"),
                search: true,
                tainted: false,
                host_check: allow_any_host,
                expected: WebLookupOutcome::Completed,
            },
        ];

        for case in cases {
            let inner = CaptureTransport::new(case.script);
            let recorder = CapturingRecorder::new();
            let mut egress = egress_over(inner).with_lookup_recorder(recorder.clone());
            if let Some(verdict) = case.gate {
                egress = egress.with_search_redaction_gate(CountingGate::new(verdict));
            }
            let flags: &dyn TaintView = if case.tainted {
                &flags_tainted
            } else {
                &flags_clean
            };
            let check: &(dyn Fn(&str) -> bool + Send + Sync) = &case.host_check;
            let mut ctx = LookupContext::new("sess-1", flags, check);
            if let Some(endpoint) = case.endpoint {
                ctx = ctx.with_search_endpoint(endpoint);
            }
            let request = if case.search {
                LookupRequest::search("some query text", Authorship::ModelComposed)
            } else {
                LookupRequest::fetch(
                    "https://docs.rs/secret/path?token=abc",
                    Authorship::ModelComposed,
                )
            };

            let outcome = egress.lookup(&request, &ctx).await;
            assert_eq!(outcome.outcome(), case.expected, "case: {}", case.name);

            let records = recorder.records();
            assert_eq!(records.len(), 1, "case {}: exactly one row", case.name);
            let record = &records[0];
            assert_eq!(record.outcome, case.expected, "case: {}", case.name);
            // BR-7, on the whole record and not just the host field: no path,
            // no query string, no query text anywhere in what leaves the seam.
            let rendered = format!("{record:?}");
            for forbidden in ["secret", "token=abc", "some query text", "/api"] {
                assert!(
                    !rendered.contains(forbidden),
                    "case {}: the record leaked `{forbidden}`: {rendered}",
                    case.name
                );
            }
        }
    }

    #[tokio::test]
    async fn a_blocked_lookup_records_a_row_and_publishes_no_privacy_block() {
        // The rule that must not be "fixed" into symmetry: web refusals are
        // observable as `web_lookup` and nothing else, so `TaintingPrivacySink`
        // never sees one and the session is never pinned by a blocked query.
        struct CountingSink {
            calls: Mutex<usize>,
        }
        impl super::super::PrivacyEventSink for CountingSink {
            fn privacy_block(
                &self,
                _session_id: Option<SessionId>,
                _block: teton_protocol::events::PrivacyBlock,
            ) {
                *self.calls.lock().unwrap() += 1;
            }
        }

        let sink = Arc::new(CountingSink {
            calls: Mutex::new(0),
        });
        let inner = CaptureTransport::answering(200, "{}");
        let recorder = CapturingRecorder::new();
        let egress = Egress::new(inner.clone(), boundaries(), sink.clone())
            .with_search_redaction_gate(CountingGate::new(a_high_finding()))
            .with_lookup_recorder(recorder.clone());
        let flags = Flags::clean();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host)
            .with_search_endpoint("https://search.example/api");

        let outcome = egress
            .lookup(
                &LookupRequest::search("sk-ant-0000", Authorship::ModelComposed),
                &ctx,
            )
            .await;

        assert_eq!(outcome.outcome(), WebLookupOutcome::BlockedRedact);
        assert_eq!(recorder.records().len(), 1);
        assert_eq!(
            *sink.calls.lock().unwrap(),
            0,
            "a web block must not travel the privacy_block path, which taints"
        );
    }

    #[tokio::test]
    async fn a_lookup_with_no_recorder_installed_still_answers() {
        // Additive, like every other slot on this choke point: no recorder
        // means no row and no event, never a changed decision.
        let inner = CaptureTransport::answering(200, "ok");
        let egress = egress_over(inner);
        let flags = Flags::clean();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host);
        let outcome = egress
            .lookup(
                &LookupRequest::fetch("https://docs.rs/x", Authorship::UserPasted),
                &ctx,
            )
            .await;
        assert_eq!(outcome.outcome(), WebLookupOutcome::Completed);
    }

    // -- caps, shapes, conversions -----------------------------------------

    #[tokio::test]
    async fn a_body_past_the_cap_is_cut_rather_than_read_whole() {
        let oversize = vec![b'x'; LOOKUP_MAX_BODY_BYTES + 4_096];
        let inner = CaptureTransport::new(vec![Ok((200, None, oversize))]);
        let egress = egress_over(inner);
        let flags = Flags::clean();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host);

        let outcome = egress
            .lookup(
                &LookupRequest::fetch("https://docs.rs/x", Authorship::UserPasted),
                &ctx,
            )
            .await;

        assert!(outcome.truncated());
        assert_eq!(outcome.bytes_in(), LOOKUP_MAX_BODY_BYTES as u64);
    }

    #[test]
    fn debug_renderings_never_carry_the_query_or_the_url() {
        // BR-7 as a property of the types rather than of every call site that
        // might one day log one.
        let fetch =
            LookupRequest::fetch("https://docs.rs/secret?token=abc", Authorship::UserPasted);
        let search = LookupRequest::search("my private question", Authorship::ModelComposed);
        for rendered in [format!("{fetch:?}"), format!("{search:?}")] {
            assert!(!rendered.contains("docs.rs"));
            assert!(!rendered.contains("token=abc"));
            assert!(!rendered.contains("my private question"));
            assert!(rendered.contains("redacted"));
        }
    }

    #[test]
    fn a_malformed_url_is_refused_without_a_host() {
        assert_eq!(host_of("not a url"), None);
        assert_eq!(host_of("https://docs.rs/x"), Some("docs.rs".to_owned()));
        // `file:` parses but has no host, which is exactly the case a naive
        // `Url::parse(..).is_ok()` check would wave through.
        assert_eq!(host_of("file:///etc/passwd"), None);
    }

    #[test]
    fn the_two_web_tier_ladders_mirror_each_other() {
        // The ALL sweep, over **both** ladders rather than one of them:
        //
        // * a variant added to either side changes one `ALL`'s length and the
        //   `zip` below stops covering it, which the length assertion catches;
        // * a variant *renamed* on the core side fails `to_protocol_web_tier`'s
        //   own exhaustive match — a compile error, not a test failure;
        // * a variant renamed on the wire side fails the inverse match here,
        //   likewise at compile time.
        //
        // So neither direction of drift is a review catch, which is the whole
        // point of a twin enum living in two crates.
        assert_eq!(
            CoreWebTier::ALL.len(),
            WireWebTier::ALL.len(),
            "the two ladders must have the same rungs"
        );
        let mut previous: Option<(CoreWebTier, WireWebTier)> = None;
        for (core, wire) in CoreWebTier::ALL.into_iter().zip(WireWebTier::ALL) {
            assert_eq!(to_protocol_web_tier(core), wire);
            // The inverse, exhaustive over the wire ladder, so a wire-side
            // rename or addition cannot compile past this point.
            let back = match wire {
                WireWebTier::Off => CoreWebTier::Off,
                WireWebTier::FetchUserUrl => CoreWebTier::FetchUserUrl,
                WireWebTier::FetchAnyUrl => CoreWebTier::FetchAnyUrl,
                WireWebTier::Search => CoreWebTier::Search,
            };
            assert_eq!(back, core);
            // The ladder is ordered and BR-3 rests on the order, so the two
            // orderings mirror too — not merely the variant names.
            if let Some((prev_core, prev_wire)) = previous {
                assert!(prev_core < core);
                assert!(prev_wire < wire);
            }
            previous = Some((core, wire));
        }
    }
}
