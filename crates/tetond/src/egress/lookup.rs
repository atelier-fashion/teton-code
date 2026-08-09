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
//! 3. **Destination policy** — the two things about *where* a lookup points
//!    that no earlier gate can have checked:
//!    * the [address class](AddressClass) of the destination. A model-composed
//!      fetch may not aim at loopback, link-local, private or unique-local
//!      space, and **no** hop of any chain may, whoever authored the original
//!      URL. This is the SSRF floor: an allowlist is a statement about names,
//!      and `127.0.0.1` defeats one by not being a name anybody thought to
//!      list.
//!    * the **search endpoint's origin**, which a `Fetch` may not target at
//!      all. The search key is bound to that origin by the transport
//!      ([`LookupContext::with_search_endpoint`]), so a fetch aimed there would
//!      carry the credential *and* skip the unconditional search scan — the
//!      search tier through the fetch tier's door.
//! 4. **The redaction gates** (BR-2, BR-13, BR-14, D-6). Every `Search` query
//!    is scanned before a byte leaves, whatever `[privacy] redact` says, and a
//!    search with **no gate installed at all** is refused rather than sent
//!    unscanned. Every `Fetch` URL is scanned by the *parity* gate — the one
//!    `[privacy] redact` installs, on the same switch and with the same gate
//!    the provider payload path uses — and when `redact` is off, neither the
//!    provider payload nor the fetch URL is scanned. Parity means parity.
//! 5. **The wire**, through the transport this choke point already owns, under
//!    a total wall-clock bound ([`LOOKUP_TOTAL_TIMEOUT`]).
//!
//! The order is the same load-bearing one `send()` uses: the cheap refusal
//! returns before the model call, so a lookup nobody was going to allow costs
//! zero inferences (AC-11's argument, applied to the lookup path). That is why
//! destination policy sits *above* the scan and not below it: a destination this
//! seam will refuse is refused before an inference is spent deciding whether its
//! URL contains a secret.
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
//! guarded and refuses — see [`outcome_for`] for why the variant survives with
//! no production producer.
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

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

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

/// How long a lookup may spend getting a connection up.
///
/// Applied by [`HttpTransport::for_lookup`](super::HttpTransport::for_lookup)
/// and by nothing else, so the provider `send()` path keeps the behavior it has
/// always had: a long completion is not a stalled connection, and a bound tuned
/// for a page fetch would cut one off. Ten seconds is far past any reachable
/// host's TCP+TLS handshake and short enough that a black-holed destination
/// fails while the user is still watching.
pub const LOOKUP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The wall-clock bound on **one whole lookup**: redirects, headers and body
/// read included.
///
/// [`LOOKUP_MAX_BODY_BYTES`] bounds how many bytes a destination may make this
/// daemon hold; it does not bound how *long* it may take to send them. A server
/// that accepts the connection, returns a 200, and then emits one byte a minute
/// stays inside every byte cap in this module while parking the turn forever —
/// a slow loris, aimed at a turn rather than at a socket. So the bound is on
/// elapsed time, it is enforced at the seam rather than in the client (it has to
/// hold for every transport this choke point is built over, test doubles
/// included), and it covers the redirect loop as a whole rather than each hop,
/// because three hops of 59 seconds each is the same attack.
///
/// Expiry on the **wire** is reported as [`TransportError::Timeout`] and
/// therefore as [`WebLookupOutcome::Offline`]: nothing answered in the time
/// allowed, which is what "offline" means in this seam's taxonomy (BUG-152).
/// Sixty seconds is generous for a document fetch and finite, which is the only
/// property that matters here.
///
/// Expiry during the **redaction scan** is a different fact and gets a different
/// name — see [`ScanPhase`]. The clock wraps every gate as well as the wire, so a
/// stalled local scanner would otherwise be announced as "the destination could
/// not be reached", which is BUG-152's mislabel pointing the other way: a user
/// sent to check their network because the thing that hung was on their own
/// machine and had nothing to do with the network at all.
pub const LOOKUP_TOTAL_TIMEOUT: Duration = Duration::from_secs(60);

/// Which phase of an attempt the [`LOOKUP_TOTAL_TIMEOUT`] deadline fired in.
///
/// The deadline wraps the whole of [`Egress::attempt`] — deliberately, since the
/// thing being bounded is the turn's total exposure and not any one hop — but
/// when it fires, the future is dropped and takes its own record of where it had
/// got to with it. So the phase is written down *as it is entered*, into a flag
/// the timing wrapper still holds after the cancellation.
///
/// Two phases, because two honest answers exist:
///
/// * **scan** — the redaction gate was in flight. Nothing had left the machine,
///   and nothing was going to until the scan answered. A guard that cannot finish
///   is a guard that did not run, which is a block and not a skip (LESSON-492),
///   so this folds to [`WebLookupOutcome::BlockedRedact`] with
///   [`BlockCause::ScanUnavailable`] — the same ending an absent gate produces,
///   because it is the same fact.
/// * **wire** — everything else: the gates that cost no I/O, the request, the
///   redirect chain, the body read. A deadline here really is "nothing answered
///   in the time allowed", so it stays [`WebLookupOutcome::Offline`].
///
/// A plain [`AtomicBool`] rather than a channel or a shared future state: the
/// only writer is the attempt itself, the only reader runs after the attempt has
/// been dropped, and there is no ordering to establish beyond "the write happened
/// before the drop".
#[derive(Debug, Default)]
struct ScanPhase(AtomicBool);

impl ScanPhase {
    /// Record that the redaction scan is now in flight.
    fn entered(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Record that the scan has answered and the attempt is back on the wire
    /// path. Called on the *forwarding* side only — a scan that blocked returns
    /// straight out of `attempt`, and the flag it leaves behind is never read
    /// because the deadline did not fire.
    fn left(&self) {
        self.0.store(false, Ordering::SeqCst);
    }

    /// The ending a deadline fired in this phase produces.
    fn expiry_ending(&self) -> Ending {
        if self.0.load(Ordering::SeqCst) {
            Ending::refused(
                WebLookupOutcome::BlockedRedact,
                LookupDetail::Blocked {
                    cause: BlockCause::ScanUnavailable,
                },
            )
        } else {
            Ending::refused(
                outcome_for(TransportError::Timeout),
                detail_for(TransportError::Timeout),
            )
        }
    }
}

/// A destination address class this seam will not send to.
///
/// The names are the classes' own (RFC 1122, 1918, 3927, 4193, 4291), not
/// invented ones, because the refusal a user reads should be checkable against
/// the thing it names. There is deliberately no `Global` variant: this enum
/// exists to answer "which non-global class is this", and the global case is
/// `None` from [`address_class_of_host`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AddressClass {
    /// `127.0.0.0/8`, `::1`, `localhost`, and the `.localhost` TLD (RFC 6761).
    /// The daemon's own machine — every service bound to a "safe because it is
    /// only reachable locally" port.
    Loopback,
    /// `169.254.0.0/16`, `fe80::/10`. The cloud metadata endpoint
    /// (`169.254.169.254`) lives here, which is the single most valuable target
    /// an SSRF has.
    LinkLocal,
    /// RFC 1918: `10/8`, `172.16/12`, `192.168/16`. The user's LAN — routers,
    /// NAS boxes, printers, an internal wiki.
    Private,
    /// RFC 4193 `fc00::/7`: IPv6's answer to RFC 1918.
    UniqueLocal,
    /// `0.0.0.0`, `::`. Not a destination; on several stacks it resolves to
    /// loopback, which is exactly the case a naive check misses.
    Unspecified,
}

impl AddressClass {
    /// A short, content-free name for the class — safe for any surface, since
    /// it names a *class* and never the destination.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            AddressClass::Loopback => "loopback",
            AddressClass::LinkLocal => "link-local",
            AddressClass::Private => "private",
            AddressClass::UniqueLocal => "unique-local",
            AddressClass::Unspecified => "unspecified",
        }
    }

    /// Whether a **user-pasted** initial URL in this class is let through.
    ///
    /// The question is not "did the user type this" — `UserPasted` only means the
    /// URL appeared in a user message, and a pasted stack trace, issue thread or
    /// log file is full of URLs a third party wrote. The question is whether the
    /// class has a legitimate story a paste could be an instance of:
    ///
    /// * **yes** for loopback, private and unique-local. "Fetch my dev server on
    ///   `localhost:3000`" and "read the wiki on the box in the next room" are
    ///   ordinary requests, and refusing them would make the floor tier useless
    ///   to the people it is for.
    /// * **no** for link-local and unspecified. `169.254.169.254` is the cloud
    ///   metadata service — the single most valuable SSRF target there is — and
    ///   `0.0.0.0` is not a destination at all. Neither has a version where a
    ///   user meant to fetch it, so neither gets an exemption to hide behind.
    ///
    /// Only the *initial* URL asks this. A redirect hop is the destination's
    /// choice, never the user's, and is refused in every class (see
    /// [`Egress::fetch`]).
    #[must_use]
    pub fn is_paste_exemptible(self) -> bool {
        match self {
            AddressClass::Loopback | AddressClass::Private | AddressClass::UniqueLocal => true,
            AddressClass::LinkLocal | AddressClass::Unspecified => false,
        }
    }
}

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
    /// `host_check` is consulted for **redirect hops and no other URL**. Not
    /// for the initial destination: that one the caller already cleared against
    /// the tier and the allowlist, and BR-11 exempts a user-pasted URL from the
    /// allowlist entirely — so re-running the check here would refuse exactly
    /// the case the requirement exempts. A redirect target has no such
    /// exemption, because the user did not choose it and the model did not
    /// either: the *destination* did.
    ///
    /// ## Two hops it is not consulted for
    ///
    /// * A hop the [address-class policy](AddressClass) already refused. That
    ///   one is refused whatever the check would have said — a closure bound to
    ///   an allowlist cannot be asked to have an opinion about `127.0.0.1`,
    ///   and a permissive closure must not be able to grant one.
    /// * A hop that stays on the **user's own pasted host** — same host, or a
    ///   dotted parent or child of it (`example.com` → `www.example.com`). The
    ///   caller binds this closure to the allowlist, and BR-11 exempts a pasted
    ///   URL from the allowlist; without this the exemption would survive hop
    ///   zero and die at the `http → https → www` redirect every second site
    ///   performs. The bypass is deliberately narrow: it is the *user's own
    ///   host*, not the user's own registrable domain (no public-suffix list
    ///   here, so `example.com → evil.example.com` is allowed only because a
    ///   subdomain of the pasted host is the operator the user chose to trust,
    ///   while `example.com → evil.com` still goes to the closure).
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
    /// The destination is in an address class this seam does not send to
    /// (SSRF floor — see [`AddressClass`]).
    ///
    /// Folds onto [`WebLookupOutcome::RefusedDomain`] rather than adding a wire
    /// variant: the wire vocabulary is fixed at eight values (D-8) and this is
    /// a destination refusal, which is what `RefusedDomain` already names. The
    /// finer reading — *which* class, and so what the user should change —
    /// lives here, which is the split [`Ending`] documents.
    RefusedAddress {
        /// The class that refused it. Names a class, never the destination.
        class: AddressClass,
    },
    /// A `Fetch` aimed at the configured search endpoint's origin.
    ///
    /// Its own detail because the thing to say is specific: that origin is
    /// reachable through the **search tier**, which scans every query and
    /// carries the endpoint-bound key, and reaching it through the fetch tier
    /// would have both skipped the scan and taken the credential along.
    /// Also folds onto [`WebLookupOutcome::RefusedDomain`].
    SearchEndpointFetch,
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
    ///
    /// **One exception, and it is structural.** When [`LOOKUP_TOTAL_TIMEOUT`]
    /// fires, the in-flight future is dropped and its record of how far down the
    /// chain it had got goes with it, so there is no "last contacted" left to
    /// report. Such a row names the host the lookup *intended*
    /// ([`Egress::intended_host`]): the initial URL's host for a fetch, the
    /// configured endpoint's for a search. On a chain that had already taken a
    /// hop, that is the first host rather than the last — a deliberate
    /// under-report, because the alternative is naming no host at all.
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
    /// Which inspection refused a blocked attempt, `None` for every other
    /// ending (REQ-563 BR-14's honesty half).
    ///
    /// The one part of [`LookupDetail`] the recorder is given, and it is here
    /// for a reason the rest are not: `BlockedRedact` folds "the scan refused
    /// the text" together with "the scan could not run", and those two send a
    /// user to two different places. The wire outcome stays at its eight fixed
    /// values (D-8); this rides beside it so the notice can name the cause the
    /// way a `privacy_block` notice already does.
    pub cause: Option<BlockCause>,
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
    ///
    /// ## And exactly one clock
    ///
    /// [`LOOKUP_TOTAL_TIMEOUT`] wraps the whole of `attempt` — every gate, every
    /// redirect hop and the body read — rather than any single request, because
    /// the thing being bounded is *the turn's exposure to a remote party's
    /// pace*, and that is a property of the chain and not of a hop. Expiry
    /// drops the in-flight future, which cancels whatever was in flight. The
    /// host such a row names is the destination the lookup *intended*, since the
    /// cancelled chain took its own record of where it had got to with it.
    ///
    /// **Which ending an expiry produces depends on where it fired** — see
    /// [`ScanPhase`]. A deadline that lands on the wire is
    /// [`WebLookupOutcome::Offline`], through the same [`outcome_for`] fold a
    /// connect failure takes; a deadline that lands while the redaction gate is
    /// still thinking is a guard that did not finish, which is a block. The flag
    /// is held *here*, outside the future being timed, precisely because the
    /// future is gone by the time the answer is needed.
    pub async fn lookup(&self, request: &LookupRequest, ctx: &LookupContext<'_>) -> LookupOutcome {
        let started = Instant::now();
        let kind = request.wire_kind();
        let phase = ScanPhase::default();
        let (host, ending) =
            match tokio::time::timeout(LOOKUP_TOTAL_TIMEOUT, self.attempt(request, ctx, &phase))
                .await
            {
                Ok(reached) => reached,
                Err(_elapsed) => (self.intended_host(request, ctx), phase.expiry_ending()),
            };
        let bytes_in = ending.body.len() as u64;
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

        let record = LookupRecord {
            kind,
            host: host.clone(),
            outcome: ending.outcome,
            bytes_in,
            duration_ms,
            cause: block_cause_of(&ending.detail),
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
    ///
    /// `phase` is written *through* this call rather than returned from it: a
    /// cancelled future returns nothing at all, and where it got to is exactly
    /// what the caller needs when the deadline fires (see [`ScanPhase`]).
    async fn attempt(
        &self,
        request: &LookupRequest,
        ctx: &LookupContext<'_>,
        phase: &ScanPhase,
    ) -> (String, Ending) {
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
            LookupKind::Search { query } => self.search(query, ctx, phase).await,
            LookupKind::Fetch { url } => self.fetch(url, request.authorship, ctx, phase).await,
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
    ///
    /// No [address-class](AddressClass) check either, and for the same reason
    /// the initial fetch destination is exempt when the user pasted it: the
    /// search endpoint is a value out of the user's own config file. A user who
    /// writes `search_endpoint = "http://localhost:8888/search"` is running a
    /// local backend, which is a configuration to support rather than a
    /// destination to refuse. There is also no model-composed spelling of this
    /// destination to guard against — the model supplies the query, never the
    /// endpoint.
    async fn search(
        &self,
        query: &str,
        ctx: &LookupContext<'_>,
        phase: &ScanPhase,
    ) -> (String, Ending) {
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
        //
        // Bracketed by the phase flag: a gate that never answers must be
        // reported as the block it is rather than as an unreachable
        // destination, and the deadline that fires here takes the future — and
        // its own idea of where it was — with it (see [`ScanPhase`]).
        phase.entered();
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
        phase.left();
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

    /// Fetch: a bounded, destination-checked redirect loop over the transport.
    ///
    /// The transport never follows a redirect itself (ADR-004), so each hop is
    /// taken here, deliberately, and every check runs **before** the request
    /// that would act on it. That ordering is the whole point: a check run after
    /// the request has gone out is a report, not a gate.
    ///
    /// ## Whose choice was this destination
    ///
    /// The loop's checks split on one question, and only that question:
    ///
    /// | | initial URL | every redirect hop |
    /// |---|---|---|
    /// | address class | always refused; `UserPasted` is exempt for the two classes with a dev-server story | always refused |
    /// | search endpoint origin | always refused | always refused |
    /// | caller's host check | never | unless the hop stays on a user-pasted host |
    ///
    /// A user pointing this daemon at `http://localhost:3000` is pointing it at
    /// their own dev server, which is a thing people do on purpose; a *model*
    /// composing that URL is the SSRF, and a *redirect* to it is the SSRF
    /// wearing a legitimate first hop, which is why the hop row has no
    /// exemption at all. Nothing about who typed the first URL is evidence
    /// about a destination the first URL's server picked.
    ///
    /// ### Why the paste exemption stops at two classes
    ///
    /// `UserPasted` means the URL appeared in the text of a user message, which
    /// is a weaker claim than "the user chose this destination": a pasted stack
    /// trace, a quoted GitHub issue, a copied log line all carry URLs somebody
    /// *else* wrote, and pasting the log is not choosing them. The exemption is
    /// therefore sized to the story that justifies it. [`AddressClass::Loopback`],
    /// [`AddressClass::Private`] and [`AddressClass::UniqueLocal`] have one — "my
    /// own dev server", "the box on my LAN" — and keep it.
    /// [`AddressClass::LinkLocal`] and [`AddressClass::Unspecified`] have none:
    /// `169.254.169.254` is the cloud metadata service and `0.0.0.0` is not a
    /// destination at all, so no paste makes either of them a thing a user meant
    /// to fetch. Those two are refused at hop zero whoever typed them.
    async fn fetch(
        &self,
        url: &str,
        authorship: Authorship,
        ctx: &LookupContext<'_>,
        phase: &ScanPhase,
    ) -> (String, Ending) {
        let Some(mut host) = host_of(url) else {
            return (
                String::new(),
                Ending::refused(WebLookupOutcome::RefusedDomain, LookupDetail::Malformed),
            );
        };
        // Gate 3 — destination policy on the initial URL. Before the scan, so a
        // destination this seam was never going to reach costs zero inferences
        // (AC-11's argument again).
        if let Some(class) = address_class_of_host(&host) {
            if authorship == Authorship::ModelComposed || !class.is_paste_exemptible() {
                return (host, refused_address(class));
            }
        }
        if targets_search_endpoint(url, ctx) {
            return (
                host,
                Ending::refused(
                    WebLookupOutcome::RefusedDomain,
                    LookupDetail::SearchEndpointFetch,
                ),
            );
        }

        // Gate 4 — the provider-parity redaction scan (BR-2, BR-13).
        //
        // The scanned string is the URL that goes on the wire, character for
        // character: `TransportRequest.url` below is `current`, which starts as
        // this value (LESSON-485 — no projection that could drift from what is
        // sent). Authorship is irrelevant here and deliberately so: BR-13 says
        // a user-pasted fetch is "still redact-scanned", which is the whole
        // point of parity — the provider path does not ask who typed the
        // context either.
        //
        // Absent gate means `[privacy] redact` is off, and then nothing is
        // scanned — not this URL and not a provider payload. That is the same
        // "off", which is what parity means; contrast `search()`, where an
        // absent gate is a refusal because BR-14 conditions the search tier's
        // existence on the scan.
        //
        // Only the initial URL is scanned, not each hop: a hop's bytes are the
        // *destination's* composition, not the user's or the model's, and the
        // outgoing text this gate exists to inspect is the text a party in this
        // session wrote. Redirect targets are governed by the destination
        // policy and the host check above instead.
        //
        // Bracketed by the phase flag for the same reason `search` is: an
        // expiry while this gate is thinking is a guard that did not finish,
        // and a guard that did not finish is a block (see [`ScanPhase`]).
        if let Some(gate) = &self.fetch_redaction {
            phase.entered();
            let verdict = gate.scan(url).await;
            if redact::decide(&verdict) == redact::EgressDecision::Block {
                return (
                    host,
                    Ending::refused(
                        WebLookupOutcome::BlockedRedact,
                        LookupDetail::Blocked {
                            cause: block_cause(&verdict),
                        },
                    ),
                );
            }
            phase.left();
            for line in redact::forwarded_findings_report(&verdict) {
                eprintln!("{line}");
            }
        }

        // The host the *user* named, kept for the hop exemption below. Fixed at
        // hop zero rather than tracked across the chain: the exemption is about
        // the destination the user chose, and a chain that has already left it
        // has left it.
        let pasted_host = host.clone();
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
            // The gates, before the hop. The destination chose this host, so
            // neither the tier grant nor BR-11's user-paste exemption covers
            // it — with the one narrow exception spelled out below.
            //
            // Address class first, and unconditionally: an allowlist is a
            // statement about names, and a redirect to `169.254.169.254` is not
            // a name anybody listed. A permissive host check must not be able
            // to grant this, so the class check is not an argument to it.
            if let Some(class) = address_class_of_host(&next_host) {
                return (host, refused_address(class));
            }
            if targets_search_endpoint(&next, ctx) {
                return (
                    host,
                    Ending::refused(
                        WebLookupOutcome::RefusedDomain,
                        LookupDetail::SearchEndpointFetch,
                    ),
                );
            }
            // The user-pasted host exemption (BR-11, carried past hop zero).
            // Short-circuits *before* the closure, which is the observable
            // property: the caller binds the closure to the allowlist, so a
            // pasted `example.com` redirecting to `www.example.com` would
            // otherwise be refused by an allowlist BR-11 exempts it from.
            let stays_on_pasted_host =
                authorship == Authorship::UserPasted && same_host_family(&pasted_host, &next_host);
            if !stays_on_pasted_host && !(ctx.host_check)(&next_host) {
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

/// The ending an address-class refusal produces — stated once so the initial
/// destination and every hop cannot drift into reporting it differently.
fn refused_address(class: AddressClass) -> Ending {
    Ending::refused(
        WebLookupOutcome::RefusedDomain,
        LookupDetail::RefusedAddress { class },
    )
}

/// Whether `url` targets the origin the search endpoint lives on.
///
/// Origin, not host: `scheme://host:port`, computed by the same `origin_of` the
/// transport's endpoint-auth binding uses to decide whether to attach the search
/// key. That the two agree is the point — this refusal exists to make the case
/// where the credential *would* attach unreachable, and a laxer or stricter
/// comparison here would leave a gap on one side or refuse innocent destinations
/// on the other. A different port or a different scheme is a different origin,
/// carries no credential, and is not refused.
fn targets_search_endpoint(url: &str, ctx: &LookupContext<'_>) -> bool {
    let Some(endpoint) = ctx.search_endpoint else {
        return false;
    };
    match (super::origin_of(endpoint), super::origin_of(url)) {
        (Some(bound), Some(target)) => bound == target,
        // No tuple origin on one side or the other: nothing to match, and the
        // credential would not attach either (`origin_of` returning `None` is
        // what makes `EndpointAuth` fail closed). Let the other gates speak.
        _ => false,
    }
}

/// Whether `hop` stays inside the family of the user's pasted `original` host.
///
/// Two relations, and they are deliberately **not** symmetric.
///
/// * **Downward** — `hop` is any subdomain of `original`. The user named a
///   name; everything under it is served by whoever controls that name, so a
///   redirect deeper into it stays inside the thing they pointed at.
/// * **Upward** — `hop` is `original` with exactly one leading `www.` removed,
///   and nothing else.
///
/// The asymmetry is the whole of this function's safety. Going *up* a level
/// leaves the name the user chose and lands on one they did not, and one level
/// up from a hosting domain is the hosting *provider*: a pasted
/// `alice.blogspot.com` one-label-stripped is `blogspot.com`, and calling those
/// two the same family would let one tenant's page redirect to the shared apex
/// with the user's paste exemption still attached. The case that genuinely
/// breaks users is `www.example.com → example.com`, which is not "one label up"
/// in general — it is that one label. So that one label is what is allowed, by
/// name.
///
/// Nothing here resolves a registrable domain: that needs a public-suffix list
/// this daemon does not carry, and guessing at one is how `foo.co.uk` and
/// `bar.co.uk` become the same trust decision. Every hop these two relations do
/// not cover is referred to the caller's check.
///
/// Host strings arrive from `Url::host_str`, which lower-cases domains, so the
/// comparison is exact rather than case-insensitive by accident.
fn same_host_family(original: &str, hop: &str) -> bool {
    if original.is_empty() || hop.is_empty() {
        return false;
    }
    if original == hop {
        return true;
    }
    // Downward: `hop` is `<something>.original`. The dot is part of the test on
    // purpose — without it `evilexample.com` would end with `example.com` and
    // pass.
    if hop
        .strip_suffix(original)
        .is_some_and(|prefix| prefix.ends_with('.'))
    {
        return true;
    }
    // Upward: exactly `www.` and no other label. `original.strip_prefix` rather
    // than a general one-label strip, because "the label that was removed" is
    // the entire question and any other answer to it is a hop to a name the
    // user did not choose.
    original.strip_prefix("www.") == Some(hop)
}

/// The non-global [`AddressClass`] of `host`, or `None` when it names something
/// globally routable (or something this seam cannot classify).
///
/// ## What is checked, and what is not
///
/// `host` comes from `Url::host_str`, which has already run the WHATWG host
/// parser: `http://2130706433/` arrives here as `127.0.0.1` and `http://[::1]/`
/// as `[::1]`, so the decimal, octal and hex spellings of an IPv4 literal are
/// one case rather than four. The bare-integer fallback below is belt and
/// braces for a host string that reached this function without that
/// normalization.
///
/// **Names that resolve into these ranges are not caught here**, and cannot be:
/// this seam sees a host string, the transport does the DNS, and no API on that
/// transport exposes the resolved addresses. `localhost` and the `.localhost`
/// TLD are special-cased because RFC 6761 makes them loopback *by definition*
/// rather than by resolution, but an attacker-controlled `evil.example` with an
/// `A` record of `127.0.0.1` — and the rebinding variant, where the record is
/// global at check time and loopback at connect time — is a **residual**. The
/// closure of it is a resolving transport that refuses non-global answers at
/// connect time (a `reqwest` custom resolver, or a pre-resolve + connect-to-IP
/// pass), which belongs in the transport and not at this seam. Recorded here so
/// the gap is a known one rather than an assumed absence.
fn address_class_of_host(host: &str) -> Option<AddressClass> {
    // `host_str` brackets an IPv6 literal; `IpAddr` does not want the brackets.
    let literal = host.strip_prefix('[').and_then(|h| h.strip_suffix(']'));
    if let Ok(ip) = literal.unwrap_or(host).parse::<IpAddr>() {
        return address_class_of_ip(ip);
    }
    if literal.is_some() {
        // Bracketed and not an IP: not a host any resolver will make sense of.
        // Nothing to classify, and the URL parser would not have produced it.
        return None;
    }
    // A host that is all digits is an IPv4 literal per the URL standard, so a
    // parser that did not fold it is still describing 127.0.0.1 when it says
    // `2130706433`.
    if let Ok(packed) = host.parse::<u32>() {
        return address_class_of_ip(IpAddr::V4(Ipv4Addr::from(packed)));
    }
    // RFC 6761: `localhost` and anything under `.localhost` are loopback by
    // definition. The trailing-dot spelling (`localhost.`) is the same name.
    let name = host.strip_suffix('.').unwrap_or(host);
    if name.eq_ignore_ascii_case("localhost") || ends_with_label(name, "localhost") {
        return Some(AddressClass::Loopback);
    }
    None
}

/// Whether `name` is a subdomain of `suffix` (ASCII-case-insensitively).
fn ends_with_label(name: &str, suffix: &str) -> bool {
    name.len() > suffix.len() + 1
        && name.as_bytes()[name.len() - suffix.len() - 1] == b'.'
        && name[name.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
}

/// The non-global class of a resolved address, or `None` when it is global.
fn address_class_of_ip(ip: IpAddr) -> Option<AddressClass> {
    match ip {
        IpAddr::V4(v4) => address_class_of_ipv4(v4),
        IpAddr::V6(v6) => address_class_of_ipv6(v6),
    }
}

fn address_class_of_ipv4(ip: Ipv4Addr) -> Option<AddressClass> {
    if ip.is_unspecified() {
        return Some(AddressClass::Unspecified);
    }
    if ip.is_loopback() {
        return Some(AddressClass::Loopback);
    }
    if ip.is_link_local() {
        return Some(AddressClass::LinkLocal);
    }
    if ip.is_private() {
        return Some(AddressClass::Private);
    }
    // `0.0.0.0/8` — "this network" (RFC 1122). `is_unspecified` covers only the
    // single address; the rest of the block is not a destination either.
    let octets = ip.octets();
    if octets[0] == 0 {
        return Some(AddressClass::Unspecified);
    }
    // `100.64.0.0/10` — carrier-grade NAT shared address space (RFC 6598).
    // Not in RFC 1918 and so not caught by `is_private`, but it is exactly as
    // internal: on any CGNAT-ed network these addresses name the provider's own
    // infrastructure and the subscriber boxes beside this one. `Private` is the
    // class it belongs to — same story, later RFC.
    if octets[0] == 100 && (64..128).contains(&octets[1]) {
        return Some(AddressClass::Private);
    }
    // `198.18.0.0/15` — benchmarking (RFC 2544). Reserved for test equipment on
    // a lab network, never globally routed, and present on real networks often
    // enough to be worth naming rather than leaving to fall through as "global".
    if octets[0] == 198 && (octets[1] == 18 || octets[1] == 19) {
        return Some(AddressClass::Private);
    }
    None
}

/// The IPv4 address a pair of IPv6 segments carries.
fn embedded_ipv4(hi: u16, lo: u16) -> Ipv4Addr {
    Ipv4Addr::from((u32::from(hi) << 16) | u32::from(lo))
}

fn address_class_of_ipv6(ip: Ipv6Addr) -> Option<AddressClass> {
    // Before the IPv4 folds below, because `::1` and `::` both sit inside the
    // IPv4-compatible block and folding them first would call `::1` "0.0.0.1".
    if ip.is_unspecified() {
        return Some(AddressClass::Unspecified);
    }
    if ip.is_loopback() {
        return Some(AddressClass::Loopback);
    }
    // Written as masks rather than as `is_unique_local` / `is_unicast_link_local`
    // so the check does not depend on when those stabilized, and so the RFC each
    // one comes from is readable at the point of use.
    let first = ip.segments()[0];
    if first & 0xffc0 == 0xfe80 {
        // fe80::/10 (RFC 4291).
        return Some(AddressClass::LinkLocal);
    }
    if first & 0xfe00 == 0xfc00 {
        // fc00::/7 (RFC 4193).
        return Some(AddressClass::UniqueLocal);
    }
    // An IPv4-**mapped** address (`::ffff:127.0.0.1`) *is* the IPv4 destination
    // it embeds; classifying it as "some IPv6 nobody listed" is how
    // `::ffff:169.254.169.254` reaches the metadata service.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return address_class_of_ipv4(v4);
    }
    // The two other transition formats that embed an IPv4 destination in an
    // IPv6 address. Both are the same hazard as the mapped form wearing a
    // different prefix: the packet ends up at the embedded address, so the
    // embedded address is what has to be classified. A fold that stopped at
    // `::ffff:` would refuse `::ffff:169.254.169.254` and wave
    // `64:ff9b::a9fe:a9fe` through to the same metadata service.
    //
    // `64:ff9b::/96` — the NAT64 well-known prefix (RFC 6052 §2.1). The low 32
    // bits are the IPv4 address a NAT64 gateway will translate to.
    let segments = ip.segments();
    if segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2..6].iter().all(|&seg| seg == 0)
    {
        return address_class_of_ipv4(embedded_ipv4(segments[6], segments[7]));
    }
    // `2002::/16` — 6to4 (RFC 3056). The next 32 bits are the IPv4 address of
    // the 6to4 relay endpoint the traffic is encapsulated to; `2002:7f00:1::`
    // is a route to 127.0.0.1.
    if segments[0] == 0x2002 {
        return address_class_of_ipv4(embedded_ipv4(segments[1], segments[2]));
    }
    // An IPv4-**compatible** address (`::127.0.0.1`) is deprecated outright
    // (RFC 4291 §2.5.5.1) and routes nowhere, so it is refused whatever it
    // embeds rather than folded onto a class it might not have.
    if ip.to_ipv4().is_some() {
        return Some(AddressClass::Unspecified);
    }
    None
}

/// The wire outcome a transport failure folds onto.
///
/// ## `BlockedPrivacy` has no production producer, and the variant stays
///
/// [`TransportError::PrivacyBlocked`] is the one failure here that is not a
/// network fault: it means an *inner* transport is itself guarded and refused.
/// The production lookup transport
/// ([`HttpTransport::for_lookup`](super::HttpTransport::for_lookup)) is
/// unguarded, so **no production lookup can reach this arm** — it is reachable
/// only when this choke point is composed over another guarded one, which today
/// happens only in tests.
///
/// The variant is kept rather than removed for three reasons, in descending
/// order of force:
///
/// 1. [`WebLookupOutcome`] is a **wire** enum (D-8). Removing a variant is a
///    protocol change, and a client that already understands eight values
///    should not have to learn seven.
/// 2. Composition is the property, not the current wiring: the arm is what
///    makes "a guarded inner transport's refusal is reported as a refusal"
///    true of the *seam* rather than of one construction of it. Folding it into
///    `Offline` would be BUG-152's mislabel — a settled refusal wearing a
///    transient network fault's name — reintroduced for the sake of deleting
///    one line.
/// 3. It costs one match arm.
///
/// **What a production lookup does with AC-3's boundary case** is therefore not
/// this: a lookup that would carry boundary-derived content is refused by the
/// taint gate as [`WebLookupOutcome::TaintRestricted`], per architecture D-4,
/// because the model's only route to boundary content is a context that has
/// already tainted the session. That is the whole of the lookup path's answer to
/// AC-3, and it is recorded as a deviation in the architecture document's
/// Deviations section — see it for the argument that the two are the same
/// guarantee reached by a different gate.
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

/// The block cause a detail carries, or `None` when the ending was not a block.
///
/// A one-arm projection rather than a `From` impl, because that is all the
/// ledger and the event are entitled to: the rest of [`LookupDetail`] — which
/// address class, which HTTP status, which transport error — stays daemon-side,
/// where BR-7's "a host and nothing finer" rule governs what may be recorded.
fn block_cause_of(detail: &LookupDetail) -> Option<BlockCause> {
    match detail {
        LookupDetail::Blocked { cause } => Some(*cause),
        _ => None,
    }
}

/// The host of `url`, or `None` when it does not parse to one.
///
/// Delegates to [`crate::web::canonical_host_of`] so the seam, the tool's gates
/// and the consent prompt read **one** parser (REQ-563 verify, C-1) rather than
/// three copies of the same call that can be edited apart.
fn host_of(url: &str) -> Option<String> {
    crate::web::canonical_host_of(url)
}

/// Resolve `location` against `base`, absolute or relative — and only when what
/// comes out is still a URL this seam fetches.
///
/// The **scheme gate is stated here** rather than left to the transport. A
/// `Location: ftp://evil.example/x` joins perfectly well against an `https`
/// base, and so do `file:///etc/passwd` and a `data:` URL; today `reqwest`
/// happens to refuse to execute the result, but that is a property of the
/// current client and not of this choke point, and a transport that was more
/// accommodating — or a test double, which is most of what this seam is
/// exercised over — would turn a redirect into a local file read with no gate
/// having had an opinion. The initial URL is held to the same closed list by
/// [`FETCHABLE_SCHEMES`](crate::harness::tools::web), one layer up; this is that
/// rule surviving a hop the destination chose.
///
/// `None` reads to the caller as `RedirectRefused`, which is what a hop to a
/// scheme this daemon does not fetch is.
fn join(base: &str, location: &str) -> Option<String> {
    let base = reqwest::Url::parse(base).ok()?;
    let next = base.join(location).ok()?;
    matches!(next.scheme(), "http" | "https").then(|| String::from(next))
}

/// The search request: the configured endpoint carrying the query as `q`.
///
/// There is no blessed search backend (BR-8), so a shape has to be assumed, and
/// this is the common one — a GET whose query string carries the terms. The
/// endpoint's own query parameters (an API version, a result count) survive, in
/// their configured order, so a user can configure `…/search?count=5` and get
/// both.
///
/// An endpoint that already carries a `q` has it **replaced** rather than
/// duplicated. Config validation rejects such an endpoint before it reaches
/// here, so this is the seam declining to assume validation ran — and the
/// failure mode it forecloses is not cosmetic: `?q=preset&q=<query>` is read as
/// the *first* value by some backends and the last by others, so a duplicate
/// turns "which query did this session send" into a question about the
/// backend's parser.
///
/// No credential header is built here. The key is attached by the transport,
/// bound to this endpoint's origin (see
/// [`LookupContext::with_search_endpoint`]).
fn search_request(endpoint: &str, query: &str) -> Option<TransportRequest> {
    let mut url = reqwest::Url::parse(endpoint).ok()?;
    url.host_str()?;
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(name, _)| name != "q")
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect();
    url.query_pairs_mut()
        .clear()
        .extend_pairs(kept)
        .append_pair("q", query);
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
        // Strictly greater, not `>=`: a body of *exactly* `cap` bytes fits, and
        // reporting it as truncated would tell the user content was dropped
        // when none was. `truncated` is a claim about bytes this seam threw
        // away, so it is true only when there were some.
        if bytes.len() + chunk.len() > cap {
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

    /// A redaction gate that is asked and never answers.
    ///
    /// Distinct from an *absent* gate and from one that answers
    /// `RedactionVerdict::unavailable()`: those are both decisions, taken
    /// promptly. This is a local scanner that has stalled — an engine still
    /// loading, a model call that will not return — which is the case
    /// [`LOOKUP_TOTAL_TIMEOUT`] has to attribute to the right phase.
    struct HangingGate;

    #[async_trait]
    impl RedactionGate for HangingGate {
        async fn scan(&self, _payload: &str) -> RedactionVerdict {
            std::future::pending().await
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

    /// A transport that accepts the call and then never answers — the slow
    /// loris, in the smallest form that reproduces it.
    struct SleepingTransport {
        calls: Arc<Mutex<usize>>,
    }

    #[async_trait]
    impl Transport for SleepingTransport {
        async fn execute(
            &self,
            _request: TransportRequest,
        ) -> Result<TransportResponse, TransportError> {
            *self.calls.lock().unwrap() += 1;
            // Far past the total bound. Under `start_paused` the runtime jumps
            // to the earliest deadline, so this costs no real time.
            tokio::time::sleep(Duration::from_secs(3_600)).await;
            Ok(TransportResponse {
                status: 200,
                location: None,
                body: Box::pin(futures::stream::empty()),
            })
        }
    }

    /// A transport that answers 200 promptly and then stalls **mid-body** —
    /// the case a byte cap cannot see, because no further byte ever arrives.
    struct StallingBodyTransport;

    #[async_trait]
    impl Transport for StallingBodyTransport {
        async fn execute(
            &self,
            _request: TransportRequest,
        ) -> Result<TransportResponse, TransportError> {
            let body: ByteStream = Box::pin(
                futures::stream::once(async { Ok(b"partial".to_vec()) })
                    .chain(futures::stream::pending()),
            );
            Ok(TransportResponse {
                status: 200,
                location: None,
                body,
            })
        }
    }

    /// A host check that records every host it was asked about.
    ///
    /// The instrument for the hop-exemption tests: "allowed without consulting
    /// the closure" is a claim about a call that did *not* happen, and a
    /// boolean-returning function cannot express it.
    #[derive(Clone)]
    struct RecordingHostCheck {
        seen: Arc<Mutex<Vec<String>>>,
        answer: bool,
    }

    impl RecordingHostCheck {
        fn answering(answer: bool) -> Self {
            Self {
                seen: Arc::new(Mutex::new(Vec::new())),
                answer,
            }
        }

        fn asked(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }

        fn as_check(&self) -> impl Fn(&str) -> bool + Send + Sync + '_ {
            move |host: &str| {
                self.seen.lock().unwrap().push(host.to_owned());
                self.answer
            }
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
    async fn a_search_is_always_scanned_and_a_fetch_is_scanned_iff_the_parity_gate_is_installed() {
        // The two clauses BR-2 and BR-14 make, side by side, distinguished by
        // gate call count on one choke point:
        //
        // * search scans through its own fail-closed slot, whatever
        //   `[privacy] redact` says;
        // * fetch scans through the **parity** slot, which is exactly what
        //   `[privacy] redact` installs — so `redact` off means the fetch URL
        //   is unscanned, the same "off" a provider payload gets, and `redact`
        //   on means it is scanned (BR-13: a user-pasted URL "is still
        //   redact-scanned").
        //
        // A test that asserted "no fetch is ever scanned" would pass on an
        // implementation that has no parity gate at all, which is the bug this
        // replaces.
        let redact_off = CaptureTransport::new(vec![
            Ok((200, None, b"{}".to_vec())),
            Ok((200, None, b"<html/>".to_vec())),
        ]);
        let search_gate = CountingGate::new(RedactionVerdict::clean());
        let egress =
            egress_over(redact_off.clone()).with_search_redaction_gate(search_gate.clone());
        let flags = Flags::clean();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host)
            .with_search_endpoint("https://search.example/api");

        egress
            .lookup(
                &LookupRequest::search("rust lifetimes", Authorship::ModelComposed),
                &ctx,
            )
            .await;
        assert_eq!(search_gate.calls(), 1);
        assert_eq!(
            search_gate.payloads(),
            vec!["rust lifetimes".to_owned()],
            "the scanned string is the query itself, not a wrapper around it"
        );

        let unscanned = egress
            .lookup(
                &LookupRequest::fetch("https://docs.rs/x", Authorship::UserPasted),
                &ctx,
            )
            .await;
        assert_eq!(unscanned.outcome(), WebLookupOutcome::Completed);
        assert_eq!(
            search_gate.calls(),
            1,
            "the search slot is not the fetch slot: a fetch never reaches it"
        );
        assert_eq!(redact_off.calls(), 2, "and both lookups went out");

        // Now the same fetch on a choke point built the way the daemon builds
        // it when `[privacy] redact` is on.
        let redact_on = CaptureTransport::answering(200, "<html/>");
        let fetch_gate = CountingGate::new(RedactionVerdict::clean());
        let guarded = egress_over(redact_on.clone())
            .with_search_redaction_gate(CountingGate::new(RedactionVerdict::clean()))
            .with_fetch_redaction_gate(fetch_gate.clone());

        let scanned = guarded
            .lookup(
                &LookupRequest::fetch("https://docs.rs/x", Authorship::UserPasted),
                &ctx,
            )
            .await;

        assert_eq!(scanned.outcome(), WebLookupOutcome::Completed);
        assert_eq!(
            fetch_gate.calls(),
            1,
            "parity means the fetch URL is scanned"
        );
        assert_eq!(
            fetch_gate.payloads(),
            vec!["https://docs.rs/x".to_owned()],
            "and the scanned string is the URL that goes on the wire"
        );
        assert_eq!(redact_on.urls(), vec!["https://docs.rs/x".to_owned()]);
    }

    // -- the fetch parity gate (BR-2, BR-13) -------------------------------

    #[tokio::test]
    async fn a_high_finding_in_a_fetch_url_blocks_it_before_the_wire() {
        // A secret pasted into a URL's query string is a secret leaving, and
        // BR-2's parity clause is the promise that the fetch path treats it the
        // way the provider path treats one in a payload.
        for authorship in [Authorship::UserPasted, Authorship::ModelComposed] {
            let inner = CaptureTransport::answering(200, "<html/>");
            let gate = CountingGate::new(a_high_finding());
            let egress = egress_over(inner.clone()).with_fetch_redaction_gate(gate.clone());
            let flags = Flags::clean();
            let ctx = LookupContext::new("sess-1", &flags, &allow_any_host);

            let outcome = egress
                .lookup(
                    &LookupRequest::fetch("https://docs.rs/x?key=sk-ant-0000000", authorship),
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
            assert_eq!(
                inner.calls(),
                0,
                "{authorship:?}: and not a byte of it left — authorship does not \
                 exempt a URL from the scan (BR-13)"
            );
        }
    }

    #[tokio::test]
    async fn an_unavailable_verdict_blocks_a_fetch_rather_than_skipping_it() {
        // LESSON-492 on the fetch path: a guard that ran and could not decide
        // is a block, exactly as it is for a search query and for a provider
        // payload.
        let inner = CaptureTransport::answering(200, "<html/>");
        let egress = egress_over(inner.clone())
            .with_fetch_redaction_gate(CountingGate::new(RedactionVerdict::unavailable()));
        let flags = Flags::clean();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host);

        let outcome = egress
            .lookup(
                &LookupRequest::fetch("https://docs.rs/x", Authorship::UserPasted),
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
    async fn the_fetch_gate_does_not_satisfy_the_search_gate() {
        // Two slots, not one: installing the parity gate must not make a
        // search look scanned. If this ever passes as `Completed`, BR-14's
        // coupling has become bypassable by turning `[privacy] redact` on and
        // the search tier's own gate off.
        let inner = CaptureTransport::answering(200, "{}");
        let fetch_gate = CountingGate::new(RedactionVerdict::clean());
        let egress = egress_over(inner.clone()).with_fetch_redaction_gate(fetch_gate.clone());
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
        assert_eq!(fetch_gate.calls(), 0);
        assert_eq!(inner.calls(), 0);
    }

    #[tokio::test]
    async fn the_fetch_scan_runs_before_the_wire_not_after_it() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let egress = Egress::new(
            OrderingTransport { log: log.clone() },
            boundaries(),
            Arc::new(NoopSink),
        )
        .with_fetch_redaction_gate(Arc::new(OrderingGate { log: log.clone() }));
        let flags = Flags::clean();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host);

        egress
            .lookup(
                &LookupRequest::fetch("https://docs.rs/x", Authorship::UserPasted),
                &ctx,
            )
            .await;

        assert_eq!(log.lock().unwrap().as_slice(), ["scan", "wire"]);
    }

    #[tokio::test]
    async fn a_destination_the_seam_refuses_costs_zero_scanner_calls() {
        // The gate order, as AC-11's argument states it: a lookup nobody was
        // going to allow must not spend an inference deciding whether its URL
        // held a secret.
        let inner = CaptureTransport::answering(200, "<html/>");
        let gate = CountingGate::new(RedactionVerdict::clean());
        let egress = egress_over(inner.clone()).with_fetch_redaction_gate(gate.clone());
        let flags = Flags::clean();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host)
            .with_search_endpoint("https://search.example/api");

        for url in [
            "http://169.254.169.254/latest",
            "https://search.example/api",
        ] {
            let outcome = egress
                .lookup(&LookupRequest::fetch(url, Authorship::ModelComposed), &ctx)
                .await;
            assert_eq!(outcome.outcome(), WebLookupOutcome::RefusedDomain);
        }
        assert_eq!(gate.calls(), 0);
        assert_eq!(inner.calls(), 0);
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

    /// **A hop to a scheme this seam does not fetch is refused here, by this
    /// seam.**
    ///
    /// The initial URL is held to `http`/`https` one layer up, in the tool. A
    /// redirect target is not, and `Location: ftp://…` joins against an `https`
    /// base perfectly well — as do `file:///etc/passwd` and a `data:` URL. The
    /// current client happens to refuse to execute the result, which is a
    /// property of `reqwest` and not of this choke point: every test double this
    /// seam is exercised over would have taken the hop, and so would a
    /// transport that was more accommodating. So the gate is stated rather than
    /// inherited.
    #[tokio::test]
    async fn a_redirect_to_a_scheme_this_seam_does_not_fetch_is_refused() {
        for location in [
            "ftp://evil.example/x",
            "file:///etc/passwd",
            "data:text/html,<p>hi</p>",
            "javascript:alert(1)",
        ] {
            let inner = CaptureTransport::new(vec![
                Ok((302, Some(location.to_owned()), Vec::new())),
                Ok((200, None, b"secrets".to_vec())),
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

            assert_eq!(
                outcome.detail(),
                &LookupDetail::RedirectRefused,
                "Location: {location}"
            );
            assert_eq!(
                inner.calls(),
                1,
                "Location: {location} — the hop must never be requested"
            );
        }

        // Non-vacuity: the two schemes it *does* fetch still redirect.
        let inner = CaptureTransport::new(vec![
            Ok((302, Some("http://docs.rs/plain".to_owned()), Vec::new())),
            Ok((200, None, b"the page".to_vec())),
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
        assert_eq!(inner.calls(), 2);
    }

    // -- the address-class policy (SSRF floor) -----------------------------

    /// Every spelling of "somewhere that is not on the public internet" this
    /// seam is expected to recognize, with the class it should name.
    ///
    /// The decimal form is in here because it is the one a reviewer's mental
    /// model of "check for 127." misses, and the `.localhost` TLD because RFC
    /// 6761 makes it loopback by definition rather than by resolution.
    const NON_GLOBAL_URLS: &[(&str, AddressClass)] = &[
        ("http://127.0.0.1/x", AddressClass::Loopback),
        ("http://127.1.2.3/x", AddressClass::Loopback),
        ("http://[::1]/x", AddressClass::Loopback),
        ("http://localhost/x", AddressClass::Loopback),
        ("http://localhost:3000/x", AddressClass::Loopback),
        ("http://api.localhost/x", AddressClass::Loopback),
        ("http://2130706433/x", AddressClass::Loopback),
        ("http://[::ffff:127.0.0.1]/x", AddressClass::Loopback),
        (
            "http://169.254.169.254/latest/meta-data/",
            AddressClass::LinkLocal,
        ),
        ("http://[fe80::1]/x", AddressClass::LinkLocal),
        ("http://10.0.0.1/x", AddressClass::Private),
        ("http://172.16.0.1/x", AddressClass::Private),
        ("http://192.168.1.1/x", AddressClass::Private),
        ("http://[fc00::1]/x", AddressClass::UniqueLocal),
        ("http://[fd00::1]/x", AddressClass::UniqueLocal),
        ("http://0.0.0.0/x", AddressClass::Unspecified),
        ("http://[::]/x", AddressClass::Unspecified),
    ];

    #[tokio::test]
    async fn a_model_composed_fetch_to_a_non_global_address_never_reaches_the_wire() {
        // The SSRF floor. An allowlist is a statement about *names*; none of
        // these is a name anybody thought to list, and the metadata endpoint in
        // the middle of the table is the single most valuable thing an SSRF
        // reaches.
        for (url, class) in NON_GLOBAL_URLS {
            let inner = CaptureTransport::answering(200, "secrets");
            let egress = egress_over(inner.clone());
            let flags = Flags::clean();
            let ctx = LookupContext::new("sess-1", &flags, &allow_any_host);

            let outcome = egress
                .lookup(&LookupRequest::fetch(*url, Authorship::ModelComposed), &ctx)
                .await;

            assert_eq!(
                outcome.outcome(),
                WebLookupOutcome::RefusedDomain,
                "{url} should be refused"
            );
            assert_eq!(
                outcome.detail(),
                &LookupDetail::RefusedAddress { class: *class },
                "{url} should be refused as {}",
                class.as_str()
            );
            assert_eq!(inner.calls(), 0, "{url}: a refusal that reached the wire");
        }
    }

    #[tokio::test]
    async fn a_user_pasted_fetch_to_a_local_address_is_allowed() {
        // The other half of the split, and the reason the initial check is
        // authorship-gated at all: pointing this daemon at your own dev server
        // is a thing people do on purpose, and BR-11 is the requirement that
        // says the user's own destination is the user's own business.
        for url in [
            "http://127.0.0.1:3000/x",
            "http://localhost:8080/docs",
            "http://192.168.1.10/wiki",
        ] {
            let inner = CaptureTransport::answering(200, "my dev server");
            let egress = egress_over(inner.clone());
            let flags = Flags::clean();
            let ctx = LookupContext::new("sess-1", &flags, &allow_any_host);

            let outcome = egress
                .lookup(&LookupRequest::fetch(url, Authorship::UserPasted), &ctx)
                .await;

            assert_eq!(outcome.outcome(), WebLookupOutcome::Completed, "{url}");
            assert_eq!(inner.calls(), 1, "{url}");
        }
    }

    /// **The paste exemption stops at the two classes with no story** (BR-11,
    /// REQ-563 verify).
    ///
    /// `UserPasted` means "this URL appeared in the text of a user message",
    /// which is weaker than "the user chose this destination" — a pasted stack
    /// trace, a quoted issue thread or a copied log line all carry URLs somebody
    /// else wrote. So the exemption is sized to the story that justifies it. "My
    /// dev server on localhost" and "the box on my LAN" are real; there is no
    /// version of `http://169.254.169.254/latest/meta-data/` that a user meant
    /// to fetch, and `0.0.0.0` is not a destination at all. Those two are
    /// refused at hop zero whoever typed them.
    ///
    /// The failure this closes is concrete: get a metadata URL in front of the
    /// user in text they paste back — an error message to debug, a log line to
    /// explain — and the exemption carried it straight through the SSRF floor.
    #[tokio::test]
    async fn a_pasted_url_gets_no_exemption_for_link_local_or_unspecified() {
        for (url, class) in [
            (
                "http://169.254.169.254/latest/meta-data/",
                AddressClass::LinkLocal,
            ),
            ("http://[fe80::1]/x", AddressClass::LinkLocal),
            ("http://0.0.0.0:8000/x", AddressClass::Unspecified),
            ("http://[::]/x", AddressClass::Unspecified),
        ] {
            let inner = CaptureTransport::answering(200, "iam credentials");
            let egress = egress_over(inner.clone());
            let flags = Flags::clean();
            let ctx = LookupContext::new("sess-1", &flags, &allow_any_host);

            let outcome = egress
                .lookup(&LookupRequest::fetch(url, Authorship::UserPasted), &ctx)
                .await;

            assert_eq!(
                outcome.detail(),
                &LookupDetail::RefusedAddress { class },
                "{url} was let through because it appeared in a user message"
            );
            assert_eq!(inner.calls(), 0, "{url}: a refusal that reached the wire");
        }

        // Non-vacuity in the other direction: the classes that *do* have a
        // story keep it, so this narrowing did not simply refuse everything.
        assert!(AddressClass::Loopback.is_paste_exemptible());
        assert!(AddressClass::Private.is_paste_exemptible());
        assert!(AddressClass::UniqueLocal.is_paste_exemptible());
        assert!(!AddressClass::LinkLocal.is_paste_exemptible());
        assert!(!AddressClass::Unspecified.is_paste_exemptible());
    }

    #[tokio::test]
    async fn every_hop_to_a_non_global_address_is_refused_whoever_pasted_the_original() {
        // A redirect target is *always* chosen by the destination, so the
        // user-paste exemption cannot reach it: `https://docs.rs/x` →
        // `http://169.254.169.254/` is the metadata endpoint wearing a
        // legitimate first hop.
        for (url, class) in NON_GLOBAL_URLS {
            for authorship in [Authorship::UserPasted, Authorship::ModelComposed] {
                let inner = CaptureTransport::new(vec![
                    Ok((302, Some((*url).to_owned()), Vec::new())),
                    Ok((200, None, b"secrets".to_vec())),
                ]);
                let egress = egress_over(inner.clone());
                let flags = Flags::clean();
                let ctx = LookupContext::new("sess-1", &flags, &allow_any_host);

                let outcome = egress
                    .lookup(&LookupRequest::fetch("https://docs.rs/x", authorship), &ctx)
                    .await;

                assert_eq!(
                    outcome.detail(),
                    &LookupDetail::RefusedAddress { class: *class },
                    "{url} as a hop, {authorship:?}"
                );
                assert_eq!(
                    inner.urls(),
                    vec!["https://docs.rs/x".to_owned()],
                    "{url} as a hop, {authorship:?}: the hop must never be requested"
                );
                assert_eq!(outcome.host(), "docs.rs");
            }
        }
    }

    #[tokio::test]
    async fn a_permissive_host_check_cannot_grant_a_hop_to_loopback() {
        // The class check is not an argument to the caller's closure, and this
        // is why: a closure bound to a permissive allowlist must not be able to
        // hand out the local network.
        let inner = CaptureTransport::new(vec![
            Ok((
                302,
                Some("http://127.0.0.1:9200/_all".to_owned()),
                Vec::new(),
            )),
            Ok((200, None, b"your search index".to_vec())),
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

        assert_eq!(
            outcome.detail(),
            &LookupDetail::RefusedAddress {
                class: AddressClass::Loopback
            }
        );
        assert_eq!(inner.calls(), 1);
    }

    #[test]
    fn the_address_classifier_lets_globally_routable_hosts_through() {
        // The negative half — a classifier that refused everything would pass
        // every test above and break every real lookup.
        for host in [
            "docs.rs",
            "example.com",
            "localhosting.example",
            "notlocalhost",
            "8.8.8.8",
            "93.184.216.34",
            "172.32.0.1",  // just outside 172.16/12
            "169.253.0.1", // just outside 169.254/16
            "128.0.0.1",
            "[2606:4700::1111]",
        ] {
            assert_eq!(address_class_of_host(host), None, "{host} is global");
        }
    }

    /// **An IPv4 destination wrapped in an IPv6 transition prefix is still that
    /// destination.**
    ///
    /// `::ffff:` is the fold everybody remembers. There are two more, and each
    /// is a working route to the embedded address on a real network: a NAT64
    /// gateway translates `64:ff9b::/96` and forwards to the low 32 bits, and a
    /// 6to4 relay encapsulates to the IPv4 address in `2002:V4::`. A floor that
    /// folded only the mapped form would refuse `::ffff:169.254.169.254` and
    /// wave `64:ff9b::a9fe:a9fe` through to the same metadata service — a
    /// rewrite of one address, not a different destination.
    #[test]
    fn the_transition_prefixes_fold_onto_the_ipv4_address_they_carry() {
        for (host, class) in [
            // NAT64 well-known prefix (RFC 6052). `a9fe:a9fe` = 169.254.169.254.
            ("[64:ff9b::a9fe:a9fe]", AddressClass::LinkLocal),
            ("[64:ff9b::7f00:1]", AddressClass::Loopback),
            ("[64:ff9b::c0a8:1]", AddressClass::Private),
            // 6to4 (RFC 3056). `2002:7f00:1::` is a route to 127.0.0.1.
            ("[2002:7f00:1::1]", AddressClass::Loopback),
            ("[2002:a9fe:a9fe::1]", AddressClass::LinkLocal),
            ("[2002:c0a8:101::1]", AddressClass::Private),
        ] {
            assert_eq!(
                address_class_of_host(host),
                Some(class),
                "{host} did not fold onto the IPv4 destination it carries"
            );
        }

        // The prefixes are not blanket refusals: one wrapping a *global* IPv4
        // address is a globally routable destination and stays one.
        for global in ["[64:ff9b::808:808]", "[2002:0808:0808::1]"] {
            assert_eq!(
                address_class_of_host(global),
                None,
                "{global} carries 8.8.8.8, which is on the public internet"
            );
        }

        // And a neighbour of the NAT64 prefix is not the NAT64 prefix: the
        // check is `64:ff9b::/96`, so the bits past it matter.
        assert_eq!(address_class_of_host("[64:ff9c::7f00:1]"), None);
        assert_eq!(address_class_of_host("[64:ff9b:1::7f00:1]"), None);
    }

    /// **Two IPv4 ranges that are internal without being RFC 1918.**
    ///
    /// `100.64.0.0/10` is carrier-grade NAT shared space (RFC 6598): on any
    /// CGNAT-ed network these addresses name the provider's own infrastructure
    /// and the subscriber boxes beside this one, which is the same story RFC
    /// 1918 tells with a later number. `198.18.0.0/15` is benchmarking (RFC
    /// 2544) — lab equipment, never globally routed. `is_private` knows neither.
    #[test]
    fn the_classifier_knows_the_two_private_ranges_that_are_not_rfc_1918() {
        for host in [
            "100.64.0.1",
            "100.100.100.100",
            "100.127.255.254",
            "198.18.0.1",
            "198.19.255.254",
        ] {
            assert_eq!(
                address_class_of_host(host),
                Some(AddressClass::Private),
                "{host} is not on the public internet"
            );
        }

        // The edges, so the masks are the masks and not "starts with 100".
        for global in [
            "100.63.255.255",
            "100.128.0.1",
            "198.17.255.255",
            "198.20.0.1",
        ] {
            assert_eq!(
                address_class_of_host(global),
                None,
                "{global} is outside the reserved range and must stay reachable"
            );
        }
    }

    #[test]
    fn the_address_classifier_names_the_class_it_refuses_on() {
        for (url, class) in NON_GLOBAL_URLS {
            let host = reqwest::Url::parse(url)
                .expect("fixture URL parses")
                .host_str()
                .expect("fixture URL has a host")
                .to_owned();
            assert_eq!(
                address_class_of_host(&host),
                Some(*class),
                "{url} (host `{host}`)"
            );
        }
    }

    // -- the search endpoint is not a fetch destination --------------------

    #[tokio::test]
    async fn a_fetch_at_the_search_endpoint_origin_is_refused() {
        // The bypass this closes: the search key is bound to the endpoint's
        // *origin*, so a `Fetch` aimed there would carry the credential and
        // skip the unconditional search scan — the search tier reached through
        // the fetch tier's door. Authorship is irrelevant: the credential does
        // not care who typed the URL.
        for authorship in [Authorship::UserPasted, Authorship::ModelComposed] {
            for url in [
                "https://search.example/api",
                "https://search.example/api?q=leak",
                "https://search.example/",
                "https://search.example/some/other/path",
            ] {
                let inner = CaptureTransport::answering(200, "{}");
                let egress = egress_over(inner.clone());
                let flags = Flags::clean();
                let ctx = LookupContext::new("sess-1", &flags, &allow_any_host)
                    .with_search_endpoint("https://search.example/api");

                let outcome = egress
                    .lookup(&LookupRequest::fetch(url, authorship), &ctx)
                    .await;

                assert_eq!(
                    outcome.outcome(),
                    WebLookupOutcome::RefusedDomain,
                    "{url} ({authorship:?})"
                );
                assert_eq!(
                    outcome.detail(),
                    &LookupDetail::SearchEndpointFetch,
                    "{url} ({authorship:?})"
                );
                assert_eq!(inner.calls(), 0, "{url} ({authorship:?})");
            }
        }
    }

    #[tokio::test]
    async fn a_different_origin_on_the_same_search_host_is_still_fetchable() {
        // Origin, not host — the same comparison that decides whether the
        // credential attaches. A different port or scheme carries no key, so
        // refusing it would be refusing an innocent destination.
        for url in [
            "https://search.example:8443/docs",
            "http://search.example/docs",
        ] {
            let inner = CaptureTransport::answering(200, "docs");
            let egress = egress_over(inner.clone());
            let flags = Flags::clean();
            let ctx = LookupContext::new("sess-1", &flags, &allow_any_host)
                .with_search_endpoint("https://search.example/api");

            let outcome = egress
                .lookup(&LookupRequest::fetch(url, Authorship::UserPasted), &ctx)
                .await;

            assert_eq!(outcome.outcome(), WebLookupOutcome::Completed, "{url}");
            assert_eq!(inner.calls(), 1, "{url}");
        }
    }

    #[tokio::test]
    async fn a_redirect_onto_the_search_origin_is_refused_too() {
        // The hop form of the same bypass: a destination that redirects to the
        // search endpoint would otherwise walk the credential there.
        let inner = CaptureTransport::new(vec![
            Ok((
                302,
                Some("https://search.example/api?q=leak".to_owned()),
                Vec::new(),
            )),
            Ok((200, None, b"{}".to_vec())),
        ]);
        let egress = egress_over(inner.clone());
        let flags = Flags::clean();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host)
            .with_search_endpoint("https://search.example/api");

        let outcome = egress
            .lookup(
                &LookupRequest::fetch("https://docs.rs/x", Authorship::UserPasted),
                &ctx,
            )
            .await;

        assert_eq!(outcome.detail(), &LookupDetail::SearchEndpointFetch);
        assert_eq!(inner.urls(), vec!["https://docs.rs/x".to_owned()]);
    }

    #[tokio::test]
    async fn with_no_search_endpoint_configured_no_fetch_is_refused_for_one() {
        let inner = CaptureTransport::answering(200, "docs");
        let egress = egress_over(inner.clone());
        let flags = Flags::clean();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host);

        let outcome = egress
            .lookup(
                &LookupRequest::fetch("https://search.example/api", Authorship::UserPasted),
                &ctx,
            )
            .await;

        assert_eq!(outcome.outcome(), WebLookupOutcome::Completed);
        assert_eq!(inner.calls(), 1);
    }

    // -- the user-pasted hop exemption (BR-11, past hop zero) --------------

    #[tokio::test]
    async fn a_user_pasted_hop_that_stays_on_the_pasted_host_skips_the_closure() {
        // The `apex → www` redirect every second site performs. The caller
        // binds the closure to the allowlist, and BR-11 exempts a pasted URL
        // from the allowlist — so consulting it here would kill the exemption
        // at hop one. "Without consulting the closure" is the observable claim,
        // which is why the check records rather than merely answers.
        for (from, to) in [
            ("https://example.com/x", "https://www.example.com/x"),
            ("https://www.example.com/x", "https://example.com/x"),
            ("https://example.com/x", "https://example.com/y"),
            ("http://example.com/x", "https://example.com/x"),
        ] {
            let inner = CaptureTransport::new(vec![
                Ok((301, Some(to.to_owned()), Vec::new())),
                Ok((200, None, b"landed".to_vec())),
            ]);
            let check = RecordingHostCheck::answering(false);
            let closure = check.as_check();
            let egress = egress_over(inner.clone());
            let flags = Flags::clean();
            let ctx = LookupContext::new("sess-1", &flags, &closure);

            let outcome = egress
                .lookup(&LookupRequest::fetch(from, Authorship::UserPasted), &ctx)
                .await;

            assert_eq!(
                outcome.outcome(),
                WebLookupOutcome::Completed,
                "{from} -> {to}"
            );
            assert_eq!(outcome.body(), b"landed", "{from} -> {to}");
            assert!(
                check.asked().is_empty(),
                "{from} -> {to}: the closure was consulted anyway: {:?}",
                check.asked()
            );
        }
    }

    #[tokio::test]
    async fn a_user_pasted_hop_that_leaves_the_pasted_host_goes_through_the_closure() {
        // The exemption is the user's *own host*, not a licence. `evilexample.com`
        // is in the table because a suffix test without the dot would let it
        // through.
        for to in [
            "https://evil.example/x",
            "https://evilexample.com/x",
            "https://example.com.evil.test/x",
        ] {
            let inner = CaptureTransport::new(vec![
                Ok((302, Some(to.to_owned()), Vec::new())),
                Ok((200, None, b"never".to_vec())),
            ]);
            let check = RecordingHostCheck::answering(false);
            let closure = check.as_check();
            let egress = egress_over(inner.clone());
            let flags = Flags::clean();
            let ctx = LookupContext::new("sess-1", &flags, &closure);

            let outcome = egress
                .lookup(
                    &LookupRequest::fetch("https://example.com/x", Authorship::UserPasted),
                    &ctx,
                )
                .await;

            assert_eq!(
                outcome.outcome(),
                WebLookupOutcome::RefusedDomain,
                "-> {to}"
            );
            assert_eq!(outcome.detail(), &LookupDetail::RedirectRefused, "-> {to}");
            assert_eq!(check.asked().len(), 1, "-> {to}: the closure decided it");
            assert_eq!(inner.calls(), 1, "-> {to}");
        }
    }

    #[tokio::test]
    async fn a_model_composed_hop_always_goes_through_the_closure() {
        // No exemption for a model-composed original, even onto its own host:
        // there is no user paste to be exempt on behalf of.
        let inner = CaptureTransport::new(vec![
            Ok((
                301,
                Some("https://www.example.com/x".to_owned()),
                Vec::new(),
            )),
            Ok((200, None, b"landed".to_vec())),
        ]);
        let check = RecordingHostCheck::answering(true);
        let closure = check.as_check();
        let egress = egress_over(inner.clone());
        let flags = Flags::clean();
        let ctx = LookupContext::new("sess-1", &flags, &closure);

        let outcome = egress
            .lookup(
                &LookupRequest::fetch("https://example.com/x", Authorship::ModelComposed),
                &ctx,
            )
            .await;

        assert_eq!(outcome.outcome(), WebLookupOutcome::Completed);
        assert_eq!(check.asked(), vec!["www.example.com".to_owned()]);
    }

    /// The relation is asymmetric, and the asymmetry is the security property.
    ///
    /// Downward — deeper into the name the user pasted — is unrestricted: they
    /// named it, and everything under it is served by whoever holds it. Upward
    /// is `www.` and only `www.`, because one label up from a *hosting* domain
    /// is the hosting provider: `alice.blogspot.com` stripped of one label is
    /// `blogspot.com`, and a general one-label rule would have carried the
    /// user's paste exemption from a single tenant's page onto the shared apex
    /// — a redirect the tenant chose, to a name the user never did.
    #[test]
    fn the_pasted_host_family_goes_down_freely_and_up_only_past_www() {
        assert!(same_host_family("example.com", "example.com"));

        // Down: any depth.
        assert!(same_host_family("example.com", "www.example.com"));
        assert!(same_host_family("example.com", "a.b.example.com"));
        assert!(same_host_family(
            "alice.blogspot.com",
            "cdn.alice.blogspot.com"
        ));

        // Up: the one label that breaks users, and no other.
        assert!(same_host_family("www.example.com", "example.com"));
        assert!(
            !same_host_family("alice.blogspot.com", "blogspot.com"),
            "one tenant's page redirected to the shared apex with a paste exemption attached"
        );
        assert!(!same_host_family("docs.example.com", "example.com"));
        assert!(!same_host_family("a.b.example.com", "b.example.com"));
        // Two labels up is not one label up, even when the first of them is
        // `www` — the relation is a single named strip, not a loop.
        assert!(!same_host_family("www.docs.example.com", "example.com"));
        // And `www` as an ordinary *label* is not the prefix strip: nothing is
        // removed from the front here.
        assert!(!same_host_family("evil.www.example.com", "example.com"));

        assert!(!same_host_family("example.com", "evilexample.com"));
        assert!(!same_host_family("example.com", "example.com.evil.test"));
        assert!(!same_host_family("example.com", "evil.com"));
        // No public-suffix list here, deliberately: two unrelated sites under
        // one registry must not become one trust decision.
        assert!(!same_host_family("foo.co.uk", "bar.co.uk"));
        assert!(!same_host_family("", "example.com"));
        assert!(!same_host_family("example.com", ""));
    }

    // -- the wall-clock bound (slow loris) ---------------------------------

    #[tokio::test(start_paused = true)]
    async fn a_destination_that_never_answers_is_bounded_by_the_total_timeout() {
        // Without this the turn parks forever: `drain_capped` bounds *bytes*,
        // and a destination that accepts the connection and then says nothing
        // sends no bytes at all.
        let calls = Arc::new(Mutex::new(0));
        let recorder = CapturingRecorder::new();
        let egress = Egress::new(
            SleepingTransport {
                calls: calls.clone(),
            },
            boundaries(),
            Arc::new(NoopSink),
        )
        .with_lookup_recorder(recorder.clone());
        let flags = Flags::clean();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host);

        let started = tokio::time::Instant::now();
        let outcome = egress
            .lookup(
                &LookupRequest::fetch("https://docs.rs/x", Authorship::UserPasted),
                &ctx,
            )
            .await;

        assert_eq!(outcome.outcome(), WebLookupOutcome::Offline);
        assert_eq!(
            outcome.detail(),
            &LookupDetail::Unreachable {
                error: TransportError::Timeout
            },
            "expiry is reported as the timeout it is, not as a new ending"
        );
        assert!(started.elapsed() >= LOOKUP_TOTAL_TIMEOUT);
        assert!(
            started.elapsed() < LOOKUP_TOTAL_TIMEOUT * 2,
            "the bound is a bound, not a suggestion"
        );
        assert_eq!(*calls.lock().unwrap(), 1);
        assert_eq!(
            outcome.host(),
            "docs.rs",
            "and the row still names where it was headed"
        );
        assert_eq!(
            recorder.records().len(),
            1,
            "one row for a timed-out attempt, like every other ending"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_body_that_stalls_mid_stream_is_bounded_too() {
        // The byte cap cannot see this one: the destination answered 200,
        // delivered seven bytes, and then stopped. `LOOKUP_MAX_BODY_BYTES` is
        // never reached, so only a clock ends it.
        let egress = Egress::new(StallingBodyTransport, boundaries(), Arc::new(NoopSink));
        let flags = Flags::clean();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host);

        let outcome = egress
            .lookup(
                &LookupRequest::fetch("https://docs.rs/x", Authorship::UserPasted),
                &ctx,
            )
            .await;

        assert_eq!(outcome.outcome(), WebLookupOutcome::Offline);
        assert_eq!(
            outcome.bytes_in(),
            0,
            "a partial body the seam abandoned is not content it brought back"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_chain_of_individually_legal_hops_cannot_outlast_the_bound_by_multiplying() {
        // Why the clock wraps the *attempt* and not each request: four hops of
        // 40 seconds each are individually unremarkable and together park the
        // turn for nearly three minutes. A per-hop bound would pass every one
        // of them.
        struct SlowRedirects {
            calls: Arc<Mutex<usize>>,
        }

        #[async_trait]
        impl Transport for SlowRedirects {
            async fn execute(
                &self,
                _request: TransportRequest,
            ) -> Result<TransportResponse, TransportError> {
                *self.calls.lock().unwrap() += 1;
                tokio::time::sleep(Duration::from_secs(40)).await;
                Ok(TransportResponse {
                    status: 302,
                    location: Some("https://docs.rs/next".to_owned()),
                    body: Box::pin(futures::stream::empty()),
                })
            }
        }

        let calls = Arc::new(Mutex::new(0));
        let egress = Egress::new(
            SlowRedirects {
                calls: calls.clone(),
            },
            boundaries(),
            Arc::new(NoopSink),
        );
        let flags = Flags::clean();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host);

        let started = tokio::time::Instant::now();
        let outcome = egress
            .lookup(
                &LookupRequest::fetch("https://docs.rs/x", Authorship::UserPasted),
                &ctx,
            )
            .await;

        assert_eq!(outcome.outcome(), WebLookupOutcome::Offline);
        assert!(started.elapsed() >= LOOKUP_TOTAL_TIMEOUT);
        assert!(
            started.elapsed() < LOOKUP_TOTAL_TIMEOUT * 2,
            "the chain is cut mid-flight, not allowed to finish its hops"
        );
        assert!(
            *calls.lock().unwrap() < MAX_REDIRECT_HOPS + 1,
            "and the hops it never got to were never requested"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_refusal_before_the_wire_does_not_wait_on_the_clock() {
        // The wrapper bounds the attempt; it must not delay one. A gate refusal
        // returns at once, which is the property AC-11's "costs nothing"
        // argument rests on.
        let egress = Egress::new(
            SleepingTransport {
                calls: Arc::new(Mutex::new(0)),
            },
            boundaries(),
            Arc::new(NoopSink),
        );
        let flags = Flags::tainted();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host);

        let started = tokio::time::Instant::now();
        let outcome = egress
            .lookup(
                &LookupRequest::fetch("https://docs.rs/x", Authorship::ModelComposed),
                &ctx,
            )
            .await;

        assert_eq!(outcome.outcome(), WebLookupOutcome::TaintRestricted);
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    // -- which phase the deadline fired in ---------------------------------

    /// **A scan that never finishes is a block, not an unreachable host.**
    ///
    /// The clock wraps every gate as well as the wire, so a stalled *local*
    /// scanner used to come back as `offline` — "the destination could not be
    /// reached" — about a destination this daemon never dialled. That is
    /// BUG-152's mislabel pointing the other way: a settled local refusal
    /// wearing a transient network fault's name, sending the user to check
    /// their network for a thing that hung on their own machine.
    ///
    /// A guard that cannot finish is a guard that did not run, which is a block
    /// (LESSON-492) — the same ending an absent gate produces, because it is the
    /// same fact.
    #[tokio::test(start_paused = true)]
    async fn a_fetch_scan_that_never_answers_is_reported_as_a_block() {
        // The transport answers instantly, so nothing but the gate can be what
        // ran out of time — and `calls() == 0` proves it never got that far.
        let inner = CaptureTransport::answering(200, "<p>hi</p>");
        let egress = egress_over(inner.clone()).with_fetch_redaction_gate(Arc::new(HangingGate));
        let flags = Flags::clean();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host);

        let started = tokio::time::Instant::now();
        let outcome = egress
            .lookup(
                &LookupRequest::fetch("https://docs.rs/x", Authorship::UserPasted),
                &ctx,
            )
            .await;

        assert_eq!(
            outcome.outcome(),
            WebLookupOutcome::BlockedRedact,
            "a stalled local scan was announced as an unreachable destination"
        );
        assert_eq!(
            outcome.detail(),
            &LookupDetail::Blocked {
                cause: BlockCause::ScanUnavailable
            },
            "and it names the cause an absent gate names, because it is the same fact"
        );
        assert_eq!(inner.calls(), 0, "nothing left the machine");
        assert!(started.elapsed() >= LOOKUP_TOTAL_TIMEOUT);
        assert!(started.elapsed() < LOOKUP_TOTAL_TIMEOUT * 2);
    }

    /// The same, on the search path — where the gate is unconditional (BR-14)
    /// and so the stall is the more likely of the two to be met.
    #[tokio::test(start_paused = true)]
    async fn a_search_scan_that_never_answers_is_reported_as_a_block() {
        let inner = CaptureTransport::answering(200, "{}");
        let egress = egress_over(inner.clone()).with_search_redaction_gate(Arc::new(HangingGate));
        let flags = Flags::clean();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host)
            .with_search_endpoint("https://search.example/api");

        let outcome = egress
            .lookup(
                &LookupRequest::search("my api key", Authorship::ModelComposed),
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
        assert_eq!(
            inner.calls(),
            0,
            "the query reached the endpoint while its own scan was still running"
        );
        assert_eq!(
            outcome.host(),
            "search.example",
            "and the row names the endpoint it was going to"
        );
    }

    /// **The flag is cleared when the scan answers.** The other half, and the
    /// one a missing `ScanPhase::left` would break silently: a scan that
    /// finished cleanly and a wire that then stalled is an *offline* lookup, and
    /// reporting it as `blocked_redact` would tell a user their privacy gate
    /// refused a request it had in fact approved.
    #[tokio::test(start_paused = true)]
    async fn a_wire_stall_after_a_clean_scan_is_still_offline() {
        let gate = CountingGate::new(RedactionVerdict::clean());
        let egress = Egress::new(
            SleepingTransport {
                calls: Arc::new(Mutex::new(0)),
            },
            boundaries(),
            Arc::new(NoopSink),
        )
        .with_fetch_redaction_gate(gate.clone());
        let flags = Flags::clean();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host);

        let outcome = egress
            .lookup(
                &LookupRequest::fetch("https://docs.rs/x", Authorship::UserPasted),
                &ctx,
            )
            .await;

        assert_eq!(gate.calls(), 1, "non-vacuity: the scan really did run");
        assert_eq!(
            outcome.outcome(),
            WebLookupOutcome::Offline,
            "a destination that never answered was blamed on the local scanner"
        );
        assert_eq!(
            outcome.detail(),
            &LookupDetail::Unreachable {
                error: TransportError::Timeout
            }
        );
    }

    /// The same for search: gate answers, endpoint stalls, ending is `offline`.
    #[tokio::test(start_paused = true)]
    async fn a_search_endpoint_that_stalls_after_a_clean_scan_is_offline() {
        let gate = CountingGate::new(RedactionVerdict::clean());
        let egress = Egress::new(
            SleepingTransport {
                calls: Arc::new(Mutex::new(0)),
            },
            boundaries(),
            Arc::new(NoopSink),
        )
        .with_search_redaction_gate(gate.clone());
        let flags = Flags::clean();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host)
            .with_search_endpoint("https://search.example/api");

        let outcome = egress
            .lookup(
                &LookupRequest::search("rust lifetimes", Authorship::ModelComposed),
                &ctx,
            )
            .await;

        assert_eq!(gate.calls(), 1);
        assert_eq!(outcome.outcome(), WebLookupOutcome::Offline);
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
            fetch_gate: Option<RedactionVerdict>,
            endpoint: Option<&'static str>,
            search: bool,
            /// Overrides the default fetch URL. The default carries a path and
            /// a query string precisely so the BR-7 leak assertion has
            /// something to catch; an override should carry the same shape.
            fetch_url: Option<&'static str>,
            tainted: bool,
            host_check: fn(&str) -> bool,
            expected: WebLookupOutcome,
        }

        impl Case {
            /// The common case: a fetch with no gates and no endpoint.
            fn fetch(name: &'static str, script: Vec<ScriptedAnswer>) -> Self {
                Self {
                    name,
                    script,
                    gate: None,
                    fetch_gate: None,
                    endpoint: None,
                    search: false,
                    fetch_url: None,
                    tainted: false,
                    host_check: allow_any_host,
                    expected: WebLookupOutcome::Completed,
                }
            }

            fn expecting(mut self, expected: WebLookupOutcome) -> Self {
                self.expected = expected;
                self
            }

            fn at(mut self, url: &'static str) -> Self {
                self.fetch_url = Some(url);
                self
            }
        }

        let cases = vec![
            Case::fetch("delivered fetch", vec![Ok((200, None, b"hello".to_vec()))]),
            Case::fetch("http error", vec![Ok((500, None, Vec::new()))]),
            Case::fetch("offline", vec![Err(TransportError::Connect)])
                .expecting(WebLookupOutcome::Offline),
            Case {
                tainted: true,
                ..Case::fetch("taint restricted", vec![])
                    .expecting(WebLookupOutcome::TaintRestricted)
            },
            Case {
                host_check: deny_any_host,
                ..Case::fetch(
                    "refused redirect",
                    vec![Ok((
                        302,
                        Some("https://evil.example/x".to_owned()),
                        Vec::new(),
                    ))],
                )
                .expecting(WebLookupOutcome::RefusedDomain)
            },
            // The refusal kinds this seam gained on review: each is a fresh
            // return path through `attempt`, and each has to come back through
            // the one emission site like every other ending.
            Case::fetch("refused address", vec![])
                .at("http://169.254.169.254/secret/path?token=abc")
                .expecting(WebLookupOutcome::RefusedDomain),
            Case {
                endpoint: Some("https://search.example/api"),
                ..Case::fetch("fetch at the search origin", vec![])
                    .at("https://search.example/secret/path?token=abc")
                    .expecting(WebLookupOutcome::RefusedDomain)
            },
            Case {
                fetch_gate: Some(a_high_finding()),
                ..Case::fetch("blocked fetch", vec![]).expecting(WebLookupOutcome::BlockedRedact)
            },
            Case {
                gate: Some(a_high_finding()),
                endpoint: Some("https://search.example/api"),
                search: true,
                ..Case::fetch("blocked search", vec![]).expecting(WebLookupOutcome::BlockedRedact)
            },
            Case {
                gate: Some(RedactionVerdict::clean()),
                search: true,
                ..Case::fetch("unconfigured search", vec![])
                    .expecting(WebLookupOutcome::RefusedTier)
            },
            Case {
                gate: Some(RedactionVerdict::clean()),
                endpoint: Some("https://search.example/api"),
                search: true,
                ..Case::fetch("delivered search", vec![Ok((200, None, b"{}".to_vec()))])
            },
        ];

        for case in cases {
            let inner = CaptureTransport::new(case.script);
            let recorder = CapturingRecorder::new();
            let mut egress = egress_over(inner).with_lookup_recorder(recorder.clone());
            if let Some(verdict) = case.gate {
                egress = egress.with_search_redaction_gate(CountingGate::new(verdict));
            }
            if let Some(verdict) = case.fetch_gate {
                egress = egress.with_fetch_redaction_gate(CountingGate::new(verdict));
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
                    case.fetch_url
                        .unwrap_or("https://docs.rs/secret/path?token=abc"),
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

    #[tokio::test]
    async fn a_body_of_exactly_the_cap_is_kept_whole_and_not_called_truncated() {
        // The off-by-one. `truncated` is a claim about bytes this seam threw
        // away, and a body that fits exactly had none thrown away — reporting
        // it as truncated tells the user content is missing when none is, and
        // the reducer downstream renders that as a caveat on a complete page.
        for (len, expected_truncation) in [
            (LOOKUP_MAX_BODY_BYTES - 1, false),
            (LOOKUP_MAX_BODY_BYTES, false),
            (LOOKUP_MAX_BODY_BYTES + 1, true),
        ] {
            let inner = CaptureTransport::new(vec![Ok((200, None, vec![b'x'; len]))]);
            let egress = egress_over(inner);
            let flags = Flags::clean();
            let ctx = LookupContext::new("sess-1", &flags, &allow_any_host);

            let outcome = egress
                .lookup(
                    &LookupRequest::fetch("https://docs.rs/x", Authorship::UserPasted),
                    &ctx,
                )
                .await;

            assert_eq!(
                outcome.truncated(),
                expected_truncation,
                "a {len}-byte body against a {LOOKUP_MAX_BODY_BYTES}-byte cap"
            );
            assert_eq!(
                outcome.bytes_in(),
                len.min(LOOKUP_MAX_BODY_BYTES) as u64,
                "a {len}-byte body against a {LOOKUP_MAX_BODY_BYTES}-byte cap"
            );
        }
    }

    #[tokio::test]
    async fn an_endpoint_that_already_carries_a_q_has_it_replaced_not_duplicated() {
        // Config validation rejects such an endpoint, so this is the seam
        // declining to assume validation ran. A duplicate `q` is not cosmetic:
        // some backends read the first value and some the last, so it turns
        // "which query did this session send" into a question about the
        // backend's parser.
        let inner = CaptureTransport::answering(200, "{}");
        let egress = egress_over(inner.clone())
            .with_search_redaction_gate(CountingGate::new(RedactionVerdict::clean()));
        let flags = Flags::clean();
        let ctx = LookupContext::new("sess-1", &flags, &allow_any_host)
            .with_search_endpoint("https://search.example/api?q=preset&count=5");

        egress
            .lookup(
                &LookupRequest::search("rust lifetimes", Authorship::ModelComposed),
                &ctx,
            )
            .await;

        let sent = &inner.urls()[0];
        let parsed = reqwest::Url::parse(sent).expect("the request URL parses");
        let qs: Vec<String> = parsed
            .query_pairs()
            .filter(|(name, _)| name == "q")
            .map(|(_, value)| value.into_owned())
            .collect();
        assert_eq!(qs, vec!["rust lifetimes".to_owned()], "sent: {sent}");
        assert!(
            parsed
                .query_pairs()
                .any(|(name, value)| name == "count" && value == "5"),
            "the endpoint's other parameters survive: {sent}"
        );
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
