//! The `web` tool — the model-facing half of opt-in web lookup (REQ-563,
//! architecture D-1/D-3/D-5).
//!
//! It owns no HTTP client and no transport. What it owns is the **order the
//! gates run in**, and that order is the whole of this module:
//!
//! 1. **Authorship** ([`UserUrls`]) — did this URL appear verbatim in a user
//!    message of this session (BR-3)? A search query is always model-composed.
//! 2. **The tier ceiling** (`[web] tier`). Answered *after* authorship because
//!    the tier a fetch needs *is* its authorship: a pasted URL needs
//!    `fetch_user_url`, a composed one needs `fetch_any_url`. Refused here
//!    names the missing tier and prompts nobody (AC-4).
//! 3. **The allowlist** (BR-11), on the **initial** destination of a
//!    model-composed fetch only — a user-pasted URL is exempt, and the choke
//!    point's own host check covers redirect hops, which nobody chose.
//! 4. **The cache** (BR-12). A fresh hit is served with zero egress *and no
//!    prompt*: there is nothing to consent to when nothing leaves the machine.
//! 5. **Consent** ([`PermissionGate`]), showing the verbatim query or URL and
//!    the destination host (BR-4), under a **per-tier key** so an
//!    allow-for-session on a fetch never grants a search (BR-3).
//! 6. **The seam** — [`Egress::lookup`](crate::egress::Egress::lookup), which
//!    owns the taint gate, the search redaction gate, the wire, the ledger row
//!    and the event. This module records none of those a second time.
//!
//! Steps 1–4 all end in a refusal *before* step 5, which is what makes "a
//! lookup nobody was going to allow costs zero prompts and zero packets" a
//! property of the control flow rather than of a reviewer's attention.
//!
//! ## Why the result carries empty provenance
//!
//! A web result is [`ToolProvenance::Sources`]`(∅)` — **not**
//! [`ToolProvenance::Unknown`], which is `shell`'s answer. Provenance is what a
//! tool *touched* (LESSON-432), and this tool touched a URL: no repo file, no
//! boundary path, nothing a `local-only` glob could ever match. `Unknown` would
//! fail-close egress for the rest of the session over content the daemon knows
//! the origin of exactly, which is the opposite of what fail-closed is for.
//!
//! Containment of a hostile page is the untrusted envelope's job, not
//! provenance's: the result is framed by the loop's existing
//! `frame_untrusted_builtin` — the *same* `<tool-result trust="untrusted">`
//! spelling `read` and `shell` get — so no new envelope alphabet exists and the
//! ADR-009 two-sided marker sets are untouched (BR-5, BUG-149/151).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::runtime::Handle;

use teton_core::config::{WebConfig, WebTier};
use teton_protocol::events::{WebLookupKind, WebLookupOutcome};

use crate::egress::{Authorship, LookupDetail, LookupOutcome, LookupRecord, LookupRequest};
use crate::harness::permissions::{PermissionDecision, PermissionGate, RememberedGrant};
use crate::web::{
    canonical_host_of, canonical_lookup_url, reduce, DomainAllowlist, UserUrls, WebCache,
    REDUCED_MAX_BYTES,
};

use super::{Tool, ToolContext, ToolError, ToolOutcome, ToolRegistry};

/// The name the model calls this tool by.
pub const WEB_TOOL_NAME: &str = "web";

/// The permission subject a fetch of a **user-pasted** URL is authorized under.
///
/// Deliberately not the tool name, and deliberately **one key per tier**: BR-3
/// grades the capability and consents each tier separately, and
/// [`PermissionGate`] remembers an `allow_always` under exactly the string it
/// was asked about. Keyed on the tool name, one "allow for this session" on a
/// page fetch would silently have granted every search for the rest of the
/// session; keyed on the tool *kind*, one grant on a URL the user pasted would
/// have granted every URL the model composes — the mixed-authorship case, and
/// the one BR-3 names first. The grant must never be wider than the question the
/// user answered, so the key is a function of
/// [`Call::needed_tier`] and nothing else.
pub const PERMISSION_KEY_FETCH_USER_URL: &str = "web_fetch_user_url";

/// The permission subject a fetch of a **model-composed** URL is authorized
/// under — see [`PERMISSION_KEY_FETCH_USER_URL`] for why the tiers do not share.
pub const PERMISSION_KEY_FETCH_ANY_URL: &str = "web_fetch_any_url";

/// The permission subject a **search** is authorized under.
pub const PERMISSION_KEY_SEARCH: &str = "web_search";

/// Every web permission key, so a sweep over them — the gate's `is_web_*`
/// predicate, the `permissive()` exclusion, the config-driven policy mapping —
/// cannot miss one a later tier adds.
///
/// Ordered like [`WebTier::ALL`] above `off`, because that is the order the keys
/// *are*: [`permission_key_for`] is the total function from tier to key, and a
/// list that disagreed with it would be a second mapping to keep in step.
pub const WEB_PERMISSION_KEYS: [&str; 3] = [
    PERMISSION_KEY_FETCH_USER_URL,
    PERMISSION_KEY_FETCH_ANY_URL,
    PERMISSION_KEY_SEARCH,
];

/// The consent key a lookup needing `tier` is authorized under (BR-3).
///
/// The **one** definition of the tier→key mapping, so the tool, the gate and any
/// config-driven policy row cannot disagree about which grant answers which
/// lookup. [`WebTier::Off`] has no key — nothing is ever consented at a tier
/// that names the absence of the capability — and it answers `None` rather than
/// borrowing a neighbouring key, which would be a grant at a tier nobody holds.
#[must_use]
pub fn permission_key_for(tier: WebTier) -> Option<&'static str> {
    match tier {
        WebTier::Off => None,
        WebTier::FetchUserUrl => Some(PERMISSION_KEY_FETCH_USER_URL),
        WebTier::FetchAnyUrl => Some(PERMISSION_KEY_FETCH_ANY_URL),
        WebTier::Search => Some(PERMISSION_KEY_SEARCH),
    }
}

/// The two schemes a fetch may name.
///
/// A closed list rather than "whatever the transport accepts": `file://`,
/// `data:` and friends are not lookups, and the place to refuse them is the
/// gate that decides what a lookup *is*, not the client that would otherwise
/// happily read a local path.
const FETCHABLE_SCHEMES: [&str; 2] = ["http://", "https://"];

/// Model-facing description at a fetch-only ceiling (< 100 chars — tool docs
/// are resident prompt bytes, LESSON-493).
const DESCRIPTION_FETCH: &str = "Fetch one web page by URL. Opt-in; every lookup asks the user.";

/// Model-facing description when the `search` tier is configured.
const DESCRIPTION_SEARCH: &str =
    "Fetch a web page by URL, or search the web. Opt-in; every lookup asks the user.";

/// The daemon-side seam one lookup crosses.
///
/// A trait rather than a handle to the runtime for the reason
/// [`TaintView`](crate::egress::TaintView) and
/// [`CostMeter`](crate::cost::CostMeter) are traits: the harness must not
/// depend on the daemon runtime that owns the router, the config and the
/// keychain — and a test must be able to state "this call reached the wire" or,
/// far more often here, "this call reached **nothing**" without building one.
/// Every zero-egress claim in this module's tests is a count on a fake
/// implementation of this trait.
#[async_trait]
pub trait WebLookupSeam: Send + Sync {
    /// Perform one lookup through the egress choke point.
    ///
    /// The implementation builds the choke point **per call** — the search
    /// key is resolved from the keychain as it is built (ADR-007), so a cached
    /// one would be holding a credential resolved at some earlier time.
    ///
    /// `hop_allowed` is consulted for **redirect hops only**, never for the
    /// initial destination: that one this module has already cleared against
    /// the tier and the allowlist, and BR-11 exempts a user-pasted URL from the
    /// allowlist entirely. A hop, by contrast, was chosen by the destination,
    /// so it is checked whoever authored the original request.
    async fn lookup(
        &self,
        request: &LookupRequest,
        hop_allowed: &(dyn for<'h> Fn(&'h str) -> bool + Send + Sync),
    ) -> Result<LookupOutcome, SeamError>;

    /// Record an attempt the choke point never saw.
    ///
    /// The seam's own "exactly one row and one event per attempt" rule can only
    /// cover attempts that reach it, and three kinds do not: a cache hit
    /// (BR-12 requires it in the ledger precisely *because* it is free), a tier
    /// refusal (AC-4 — the one refusal with no `permission_request` and no
    /// packet to observe it by), and an allowlist refusal on the initial
    /// destination. Routing them here keeps one recorder, and therefore one
    /// answer to "how many lookups did this session perform".
    fn record_without_egress(&self, record: &LookupRecord);
}

/// The lookup path could not be opened at all.
///
/// Distinct from every [`LookupOutcome`], which describes an attempt that was
/// *made*: this is the choke point failing to be built, so no gate ran, nothing
/// left, and there is no attempt to record.
#[derive(Debug, thiserror::Error)]
pub enum SeamError {
    /// The choke point could not be constructed (no HTTP client).
    #[error("the web lookup path could not be opened ({0})")]
    Unavailable(String),
}

/// What the model asked for, with authorship already resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    /// Retrieve one URL.
    Fetch {
        /// The URL as the model wrote it — echoed verbatim at the prompt (BR-4).
        url: String,
        /// The destination host, extracted once.
        host: String,
        /// Whether the user pasted this URL in the current session (BR-3).
        authorship: Authorship,
    },
    /// Query the configured search backend.
    Search {
        /// The query as the model wrote it — echoed verbatim at the prompt.
        query: String,
    },
}

impl Call {
    /// The wire kind, for the ledger row a local refusal still owes.
    fn wire_kind(&self) -> WebLookupKind {
        match self {
            Call::Fetch { .. } => WebLookupKind::Fetch,
            Call::Search { .. } => WebLookupKind::Search,
        }
    }

    /// The tier this call needs — BR-3's ladder, read off the call itself.
    ///
    /// A fetch's required tier *is* its authorship, which is why authorship is
    /// resolved before the ceiling is consulted and not after.
    fn needed_tier(&self) -> WebTier {
        match self {
            Call::Fetch {
                authorship: Authorship::UserPasted,
                ..
            } => WebTier::FetchUserUrl,
            Call::Fetch { .. } => WebTier::FetchAnyUrl,
            Call::Search { .. } => WebTier::Search,
        }
    }

    /// The permission subject (BR-3: one per tier, never one per tool).
    ///
    /// Derived from [`Self::needed_tier`] through the single
    /// [`permission_key_for`] mapping rather than matched on the call shape: the
    /// two fetch tiers are *different capabilities*, and a match on
    /// `Call::Fetch { .. }` collapses them back into one grant. `Off` is
    /// unreachable here — `needed_tier` never yields it — and the fallback picks
    /// the **narrowest** key rather than panicking, so a tier added to the ladder
    /// without a key fails closed instead of sharing a broader one.
    fn permission_key(&self) -> &'static str {
        permission_key_for(self.needed_tier()).unwrap_or(PERMISSION_KEY_FETCH_USER_URL)
    }

    /// The request handed to the choke point.
    fn request(&self) -> LookupRequest {
        match self {
            Call::Fetch {
                url, authorship, ..
            } => LookupRequest::fetch(url.clone(), *authorship),
            // A query is composed by the model even when it paraphrases the
            // user — BR-13's split is about who authored the bytes that leave,
            // and nobody typed this string.
            Call::Search { query } => {
                LookupRequest::search(query.clone(), Authorship::ModelComposed)
            }
        }
    }
}

/// The opt-in web lookup tool.
///
/// Constructed only where a `[web] tier` above `off` has been read — see
/// [`register_web_tool`], which is the one place that condition is expressed.
pub struct WebTool {
    /// The configured ceiling. Never a grant: it bounds what consent may be
    /// asked for, and `off` never reaches here at all.
    tier: WebTier,
    /// BR-11's matcher, or `None` for the unrestricted (and perfectly valid)
    /// configuration. `Option` and not an empty list, because an allowlist that
    /// lists nothing is the *most* restrictive posture, not the absence of one.
    allowlist: Option<DomainAllowlist>,
    /// The search backend's **host** — the destination a search prompt names
    /// (BR-4). The endpoint's path and its key stay with the choke point.
    search_host: Option<String>,
    /// The local document cache (BR-12).
    cache: WebCache,
    /// This session's user-pasted URLs (BR-3). Shared with the prompt-ingestion
    /// path, which fills it before the turn runs.
    user_urls: Arc<Mutex<UserUrls>>,
    /// The consent authority. Held by the tool rather than consulted by the
    /// loop — see [`Tool::gates_itself`].
    gate: Arc<PermissionGate>,
    /// The choke point, behind a trait.
    seam: Arc<dyn WebLookupSeam>,
    /// The runtime the sync [`Tool::run`] bridges into (same shape as `mcp`).
    runtime: Handle,
    /// Tier-appropriate model-facing description: a fetch-only ceiling does not
    /// advertise a search it would only refuse.
    description: &'static str,
}

impl WebTool {
    /// A tool for `config`'s ceiling, backed by `cache`, `gate` and `seam`.
    #[must_use]
    pub fn new(
        config: &WebConfig,
        cache: WebCache,
        user_urls: Arc<Mutex<UserUrls>>,
        gate: Arc<PermissionGate>,
        seam: Arc<dyn WebLookupSeam>,
        runtime: Handle,
    ) -> Self {
        Self {
            tier: config.tier,
            allowlist: config
                .allowed_domains
                .as_ref()
                .map(|patterns| DomainAllowlist::new(patterns)),
            // The same parser the seam reads the endpoint with, so the host the
            // prompt names is the host the request reaches (C-1).
            search_host: config
                .search_endpoint
                .as_deref()
                .and_then(canonical_host_of),
            cache,
            user_urls,
            gate,
            seam,
            runtime,
            description: if config.tier.allows(WebTier::Search) {
                DESCRIPTION_SEARCH
            } else {
                DESCRIPTION_FETCH
            },
        }
    }

    /// The whole orchestration, async because both the consent round-trip and
    /// the lookup are.
    ///
    /// Separate from [`Tool::run`] — which is only the sync→async bridge — so
    /// the gate order can be tested without a `block_in_place`, and so a caller
    /// already on the async path never pays for one.
    pub(crate) async fn lookup(&self, args: &Value) -> ToolOutcome {
        let call = match self.parse(args) {
            Ok(call) => call,
            Err(err) => return err.into(),
        };

        // --- gate: the tier ceiling (AC-4). Before any prompt, by design. ---
        let needed = call.needed_tier();
        if !self.tier.allows(needed) {
            self.record_local(&call, WebLookupOutcome::RefusedTier);
            return ToolOutcome::error(format!(
                "web lookup refused: this needs the `{}` tier, and `[web] tier` is set to \
                 `{}`. Do not retry it — say the lookup is not available at the granted \
                 tier and continue.",
                tier_name(needed),
                tier_name(self.tier),
            ));
        }

        // --- gate: the allowlist, initial destination only (BR-11, AC-9) ---
        if let Some(refusal) = self.allowlist_refusal(&call) {
            self.record_local(&call, WebLookupOutcome::RefusedDomain);
            return ToolOutcome::error(refusal);
        }

        // --- gate: the cache (BR-12). A hit performs no egress, so it asks no
        // consent: there is nothing for the user to authorize. It is still a
        // lookup, so it is still a ledger row. ---
        //
        // "Asks no consent" is not "ignores consent". A standing
        // `reject_always` is an answer about the **capability**, not about the
        // packet — the user said "stop doing web lookups this session" — and
        // serving a cached page past it would be the one path around their own
        // answer, invisible precisely because it is free. So the remembered
        // grant is *read* (never prompted for, never recorded) before the entry
        // is handed over.
        if let Call::Fetch { url, host, .. } = &call {
            if let Some(entry) = self.cache.get(url) {
                if self.gate.remembered(call.permission_key())
                    == Some(RememberedGrant::RejectAlways)
                {
                    return ToolOutcome::error(
                        "Permission denied: the user declined web fetches for this session. Do \
                         not retry it; take a different approach or finish."
                            .to_owned(),
                    );
                }
                self.record_local(&call, WebLookupOutcome::CacheHit);
                return ToolOutcome::ok(format!(
                    "web fetch {url} (host {host}; served from the local cache, nothing \
                     left this machine)\n\n{}",
                    entry.content
                ));
            }
        }

        // --- gate: consent, under a per-tier key (BR-3, BR-4) ---
        //
        // `needed` rides along with the key because the two answer different
        // questions: the key is what a session grant is remembered under, and
        // the tier is what an `enable_permanent` answer writes to config. It is
        // the tier already checked against the ceiling above, so the option can
        // never offer to persist a tier this lookup was not entitled to.
        if self
            .gate
            .authorize_web(call.permission_key(), Some(self.describe(&call)), needed)
            .await
            == PermissionDecision::Denied
        {
            // No ledger row: the outcome vocabulary has no "the user said no"
            // value, and the `permission_request` this answered is itself the
            // record. Minting a neighbouring outcome to fill the gap would put
            // a wrong word in the one place BR-7 is read from.
            return ToolOutcome::error(format!(
                "Permission denied: the user declined this web {}. Do not retry it; take a \
                 different approach or finish.",
                match call {
                    Call::Fetch { .. } => "fetch",
                    Call::Search { .. } => "search",
                }
            ));
        }

        // --- the seam. Everything past here is recorded by the choke point. ---
        let allowlist = self.allowlist.clone();
        let hop_allowed =
            move |host: &str| allowlist.as_ref().is_none_or(|list| list.matches(host));
        let outcome = match self.seam.lookup(&call.request(), &hop_allowed).await {
            Ok(outcome) => outcome,
            // Nothing was attempted, so nothing is recorded — an unopened choke
            // point is not a lookup that happened and failed.
            Err(err) => {
                return ToolOutcome::error(format!(
                    "web lookup unavailable: {err}. State that the lookup did not happen \
                     and continue."
                ));
            }
        };
        self.render(&call, outcome)
    }

    /// Read the model's arguments into a [`Call`], resolving authorship.
    ///
    /// ## One parser, and the URL that leaves is the URL that was checked
    ///
    /// The `url` argument is parsed by [`canonical_lookup_url`] — `reqwest::Url`,
    /// the transport's own parser — and everything downstream uses its
    /// **re-serialized** output: the host the allowlist and the consent prompt
    /// name, the string the prompt echoes, the string the cache keys, the string
    /// handed to the seam. A model-supplied spelling whose re-serialization
    /// differs is not a hazard, because the re-serialized form is what is sent;
    /// what *was* a hazard is checking one reading and sending another, which is
    /// what `https://evil.example\@allowed.example/x` does to any splitter that
    /// is not the socket's (REQ-563 verify, C-1).
    ///
    /// The scheme check runs on the parsed scheme rather than a prefix of the
    /// raw string, for the same reason: a lowercase prefix test on the model's
    /// bytes is a second reading of the input.
    fn parse(&self, args: &Value) -> Result<Call, ToolError> {
        let url = super::opt_str_arg(args, "url").filter(|s| !s.trim().is_empty());
        let query = super::opt_str_arg(args, "query").filter(|s| !s.trim().is_empty());
        match (url, query) {
            (Some(raw), None) => {
                let (url, host) = canonical_lookup_url(&raw)
                    .ok_or_else(|| ToolError::args("`url` has no host to look up"))?;
                // A closed scheme list, checked here rather than at the
                // transport: `file:///etc/passwd` is not a lookup that fails, it
                // is not a lookup.
                if !FETCHABLE_SCHEMES
                    .iter()
                    .any(|scheme| url.starts_with(scheme))
                {
                    return Err(ToolError::args(
                        "`url` must be an absolute http:// or https:// URL",
                    ));
                }
                // Authorship is asked of the canonical spelling *and* of what
                // the model wrote. The first is the ordinary case (the paste is
                // ingested through the same canonicalization); the second keeps
                // a URL whose raw spelling the user typed verbatim from losing
                // its exemption to a re-serialization difference. Neither
                // widens: two strings that normalize alike differ only in
                // scheme/host case and fragment, so they name the same
                // destination — the substitution BR-3 forbids is not reachable
                // from here.
                let authorship = if self.user_pasted(&url) || self.user_pasted(&raw) {
                    Authorship::UserPasted
                } else {
                    Authorship::ModelComposed
                };
                Ok(Call::Fetch {
                    url,
                    host,
                    authorship,
                })
            }
            (None, Some(query)) => Ok(Call::Search {
                query: query.trim().to_owned(),
            }),
            (Some(_), Some(_)) => Err(ToolError::args(
                "give either `url` or `query`, never both — the kind of lookup is which one \
                 you supply",
            )),
            (None, None) => Err(ToolError::args(
                "give exactly one of `url` (fetch a page) or `query` (search the web)",
            )),
        }
    }

    /// BR-3's question: did the user paste this URL in this session?
    fn user_pasted(&self, url: &str) -> bool {
        self.user_urls
            .lock()
            .expect("user url set mutex poisoned")
            .contains(url)
    }

    /// The BR-11 refusal for this call, or `None` when the allowlist has no say.
    ///
    /// It has no say in three cases, and each is a requirement rather than a
    /// shortcut: there is no allowlist (unrestricted is valid, BR-11); the URL
    /// is one the user pasted (their act is its own authorization); or the call
    /// is a search, whose destination is the endpoint *the user configured* and
    /// so was never model-chosen.
    fn allowlist_refusal(&self, call: &Call) -> Option<String> {
        let Call::Fetch {
            host,
            authorship: Authorship::ModelComposed,
            ..
        } = call
        else {
            return None;
        };
        let list = self.allowlist.as_ref()?;
        if list.matches(host) {
            return None;
        }
        let named = if list.is_empty() {
            "it lists nothing, so it permits nothing".to_owned()
        } else {
            format!("it permits {}", list.patterns().join(", "))
        };
        Some(format!(
            "web lookup refused: `{host}` is outside the `[web] allowed_domains` allowlist \
             ({named}). The allowlist constrains destinations the model chose; a URL the \
             user pasted is exempt. Do not retry this host."
        ))
    }

    /// The consent prompt's description: the **verbatim** query or URL and the
    /// destination host (BR-4, AC-2).
    ///
    /// Verbatim is the point. A normalized, shortened or host-only rendering
    /// would ask the user to approve something other than what would leave, and
    /// the whole of BR-4 is that consent is concrete.
    fn describe(&self, call: &Call) -> String {
        match call {
            Call::Fetch { url, host, .. } => format!("web fetch {url} (host {host})"),
            Call::Search { query } => format!(
                "web search {query} (host {})",
                self.search_host.as_deref().unwrap_or("<none configured>")
            ),
        }
    }

    /// Write the one ledger row + event an attempt owes when the choke point
    /// never saw it (see [`WebLookupSeam::record_without_egress`]).
    fn record_local(&self, call: &Call, outcome: WebLookupOutcome) {
        let host = match call {
            Call::Fetch { host, .. } => host.clone(),
            Call::Search { .. } => self.search_host.clone().unwrap_or_default(),
        };
        self.seam.record_without_egress(&LookupRecord {
            kind: call.wire_kind(),
            host,
            outcome,
            bytes_in: 0,
            duration_ms: 0,
            // No inspection ran: these are the endings decided *before* the
            // choke point, so there is no block and no cause to name.
            cause: None,
        });
    }

    /// Turn a [`LookupOutcome`] into what the model sees.
    ///
    /// Delivered content is reduced here (BR-10) — deterministic, dependency-free,
    /// and byte-capped — and the reduction is what the cache stores and what
    /// enters context. Raw page bytes reach nothing else by construction: they
    /// are dropped with the outcome at the end of this function.
    fn render(&self, call: &Call, outcome: LookupOutcome) -> ToolOutcome {
        if *outcome.detail() != LookupDetail::Delivered {
            return ToolOutcome::error(self.failure_sentence(outcome.detail()));
        }
        let bytes_in = outcome.bytes_in();
        let truncated = outcome.truncated();
        self.deliver(call, &outcome.into_body(), bytes_in, truncated)
    }

    /// Reduce delivered `body` and fold it into the result the model sees.
    ///
    /// Split from [`Self::render`] so that the reduction, the cache write and
    /// the header can be tested without minting a [`LookupOutcome`], whose
    /// fields are the choke point's to set and have no public constructor —
    /// deliberately, and not a gap to widen from this side.
    fn deliver(&self, call: &Call, body: &[u8], bytes_in: u64, truncated: bool) -> ToolOutcome {
        let raw = String::from_utf8_lossy(body);
        // BR-10's reduction is an **HTML→text** extractor, and it is applied to
        // the one body that is HTML. A search backend answers JSON: run through
        // `reduce`, `<script` inside a snippet is treated as a tag to strip,
        // `Vec<T>` in a code result loses `<T>`, and `&amp;` in a URL field is
        // decoded — the result is neither the JSON that was received nor a
        // faithful rendering of it. A search body therefore takes the same byte
        // cap and nothing else; the cap is the same constant, so what enters
        // context is bounded identically either way (REQ-563 verify).
        let text = match call {
            Call::Fetch { .. } => reduce(&raw, REDUCED_MAX_BYTES),
            Call::Search { .. } => cap_bytes(&raw, REDUCED_MAX_BYTES),
        };
        let cut = if truncated { ", truncated" } else { "" };
        match call {
            Call::Fetch { url, host, .. } => {
                // A write failure is not a lookup failure: the bytes are in
                // hand, and the only cost of an uncached document is fetching
                // it again. Named on stderr, never folded into the model's
                // context as if the page had a problem.
                if let Err(err) = self.cache.put(url, &text) {
                    eprintln!("teton: a fetched page could not be cached ({err})");
                }
                ToolOutcome::ok(format!(
                    "web fetch {url} (host {host}, {bytes_in} bytes{cut})\n\n{text}"
                ))
            }
            Call::Search { query } => ToolOutcome::ok(format!(
                "web search {query} (host {}, {bytes_in} bytes{cut})\n\n{text}",
                self.search_host.as_deref().unwrap_or("")
            )),
        }
    }

    /// The sentence a non-delivered ending reads as to the **model**.
    ///
    /// Every privacy cause collapses to one sentence naming no cause, for the
    /// reason [`mcp::tool_error_sentence`](super::mcp) does it: folded back into
    /// context, a cause-distinct sentence is a three-way oracle on an input the
    /// model controls, and the model can do nothing with the distinction except
    /// probe it. The precise cause still reaches the surfaces built for a
    /// person — the `web_lookup` event and the ledger row.
    fn failure_sentence(&self, detail: &LookupDetail) -> String {
        let what = match detail {
            // BR-9 / BUG-152's taxonomy: unreachable is transient-shaped and
            // says so in those words. It is never a turn error.
            LookupDetail::Unreachable { .. } => {
                "web lookup unavailable: offline — the destination could not be reached".to_owned()
            }
            LookupDetail::HttpStatus { status } => {
                format!("the destination answered HTTP {status}")
            }
            LookupDetail::RedirectLimit => "the destination redirected too many times".to_owned(),
            LookupDetail::RedirectRefused => {
                "the destination redirected outside the configured allowlist".to_owned()
            }
            // `..` rather than naming the class, deliberately. *Which*
            // non-global range a destination fell in — loopback, link-local,
            // private, unspecified — is a fact for the ledger row and the
            // `web_lookup` event, where a person can act on it. Handed back to
            // the model it is a four-way oracle on an input the model controls:
            // sweep an address space, read which class each answer names, and
            // the refusal has mapped the host's network for you. The one thing
            // the model can act on is "stop trying to reach addresses like this
            // one", and that is what the sentence says.
            LookupDetail::RefusedAddress { .. } => {
                "this destination is not reachable from web lookup — it is not a public \
                 internet address. Do not try other addresses in the same family"
                    .to_owned()
            }
            // This one *does* name its cause, and the contrast with the arm
            // above is the rule rather than an exception to it: the search
            // endpoint is a value out of the user's own config, so naming it
            // discloses nothing the model could not read off `[web]`, and the
            // fact is directly actionable — the same destination is reachable
            // by supplying `query` instead of `url`. Saying only "refused"
            // would leave the model retrying a fetch that can never succeed.
            LookupDetail::SearchEndpointFetch => {
                "that host is the configured search backend, which is reached with the \
                 `query` argument (the search tier scans every query and carries the \
                 backend's credential) and never with `url`"
                    .to_owned()
            }
            // BR-13's notice names the cause and the effect, because a
            // restriction the user cannot see is one they cannot lift.
            LookupDetail::TaintRestricted => "this session has read privacy-boundary content, so \
                 model-composed web lookups are restricted for the rest of it — a URL the user \
                 pasted still works, and only the user can lift the restriction"
                .to_owned(),
            // Deliberately one sentence for every cause; `..` so a cause added
            // later cannot leak by defaulting into its own arm.
            LookupDetail::Blocked { .. } => "it was refused by the local privacy gate".to_owned(),
            LookupDetail::Malformed => "the destination could not be read as a URL".to_owned(),
            LookupDetail::SearchUnconfigured => {
                "no search backend is configured (`[web] search_endpoint`)".to_owned()
            }
            LookupDetail::Delivered => "it returned nothing".to_owned(),
        };
        format!(
            "web lookup did not complete: {what}. State that and continue; do not retry blindly."
        )
    }
}

impl Tool for WebTool {
    fn name(&self) -> &str {
        WEB_TOOL_NAME
    }

    fn description(&self) -> &str {
        self.description
    }

    fn input_schema(&self) -> Value {
        // The kind is derived from **which argument is present**, not from a
        // separate `kind` field: two encodings of one fact are two encodings
        // that can disagree (`kind:"search"` beside a `url`), and a weak model
        // filling three keys correctly is a harder ask than it filling one.
        //
        // There is deliberately **no `refresh` argument**. BR-12's "an explicit
        // user refresh bypasses the cache" is a *user* act, and it is served by
        // evicting the entry ([`WebCache::evict`], a client surface). Offered
        // here it would be a flag with which the model could turn a free,
        // consent-free cache hit into a network request — a way to manufacture
        // egress out of a lookup that was not going to perform any.
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "Absolute http(s) URL to fetch. Supply this OR `query`, \
                                    never both — supplying `url` is what makes this a fetch."
                },
                "query": {
                    "type": "string",
                    "description": "Free-text search terms for the configured search backend. \
                                    Supply this OR `url`, never both — supplying `query` is \
                                    what makes this a search."
                }
            }
        })
    }

    /// This tool prompts for itself — see [`Tool::gates_itself`] for why.
    fn gates_itself(&self) -> bool {
        true
    }

    fn run(&self, _ctx: &ToolContext, args: &Value) -> ToolOutcome {
        // The same sync→async bridge the MCP tools use, on the multi-threaded
        // runtime the daemon runs on. No repo path is involved, so the tool
        // context's jail has nothing to say about this call.
        tokio::task::block_in_place(|| self.runtime.block_on(self.lookup(args)))
    }
}

/// Register the web tool into `reg` — and **only** when a tier is enabled
/// (BR-1, D-1). Returns whether it was registered.
///
/// The condition lives here, once, rather than at the call site: "absent by
/// construction" is the whole of AC-1's structural half, and a condition
/// duplicated at two call sites is a condition that will eventually hold at one
/// of them.
///
/// Call it **last** — after the built-ins and after any MCP tools — so a
/// degraded provider's `max_tools` cap cuts the opt-in capability first (BR-6).
pub fn register_web_tool(
    reg: &mut ToolRegistry,
    config: &WebConfig,
    cache: WebCache,
    user_urls: Arc<Mutex<UserUrls>>,
    gate: Arc<PermissionGate>,
    seam: Arc<dyn WebLookupSeam>,
    runtime: Handle,
) -> bool {
    if config.tier == WebTier::Off {
        return false;
    }
    reg.register(Arc::new(WebTool::new(
        config, cache, user_urls, gate, seam, runtime,
    )));
    true
}

/// `text` truncated to at most `cap` **bytes**, never mid-character.
///
/// The whole of what a non-HTML body gets instead of [`reduce`]: the point of
/// the cap is to bound what enters context, and every other thing `reduce` does
/// is an HTML transform that a JSON document is only damaged by. Cutting on a
/// `char_indices` boundary rather than slicing at `cap` is what keeps the result
/// a `String` at all — a byte slice through a multi-byte character panics.
fn cap_bytes(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_owned();
    }
    let end = text
        .char_indices()
        .map(|(at, _)| at)
        .take_while(|at| *at <= cap)
        .last()
        .unwrap_or(0);
    text[..end].to_owned()
}

/// The config spelling of a tier, for a refusal that has to name it (AC-4).
///
/// A match rather than a `Display` impl on the core enum: this is the `[web]
/// tier` *config* vocabulary, and the exhaustive match means a tier added to
/// the ladder fails to compile here rather than rendering as a debug string in
/// a sentence the user reads.
///
/// `pub(crate)` so the consent prompt's `enable_permanent` label names the tier
/// with the same spelling this refusal does — the label promises to write a
/// config value, and a second mapping would be a second chance to write a
/// different one.
pub(crate) fn tier_name(tier: WebTier) -> &'static str {
    match tier {
        WebTier::Off => "off",
        WebTier::FetchUserUrl => "fetch_user_url",
        WebTier::FetchAnyUrl => "fetch_any_url",
        WebTier::Search => "search",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use teton_protocol::events::Event;
    use teton_protocol::methods::PermissionOutcome;
    use teton_protocol::SessionId;

    use crate::broadcast::EventBus;
    use crate::harness::context::ToolProvenance;
    use crate::harness::permissions::{PendingPermissions, PermissionConfig, PermissionPolicy};

    /// A seam that reaches no network and counts everything asked of it.
    ///
    /// Every "zero egress" assertion in this module is `seam.calls == 0` on one
    /// of these — a count, not an inspection of what a real transport would
    /// have done.
    ///
    /// It cannot return a delivered [`LookupOutcome`]: that type has no public
    /// constructor, deliberately, because its fields are the choke point's to
    /// set. The delivered path is therefore reached through [`WebTool::deliver`]
    /// (the half that does not need one) here, and end-to-end through a real
    /// capture transport in TASK-078's acceptance suite.
    struct FakeSeam {
        calls: AtomicUsize,
        records: Mutex<Vec<LookupRecord>>,
        /// The URL (or query) of every request that reached the seam — what
        /// would have gone on the wire, which is the string C-1 is about.
        requests: Mutex<Vec<String>>,
    }

    impl FakeSeam {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                records: Mutex::new(Vec::new()),
                requests: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn recorded(&self) -> Vec<LookupRecord> {
            self.records.lock().unwrap().clone()
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl WebLookupSeam for FakeSeam {
        async fn lookup(
            &self,
            request: &LookupRequest,
            _hop_allowed: &(dyn for<'h> Fn(&'h str) -> bool + Send + Sync),
        ) -> Result<LookupOutcome, SeamError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.requests.lock().unwrap().push(match &request.kind {
                crate::egress::LookupKind::Fetch { url } => url.clone(),
                crate::egress::LookupKind::Search { query } => query.clone(),
            });
            Err(SeamError::Unavailable("test seam".to_owned()))
        }

        fn record_without_egress(&self, record: &LookupRecord) {
            self.records.lock().unwrap().push(record.clone());
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "teton-webtool-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A gate whose policy for every subject is `policy`, plus the bus so a
    /// test can watch for (or assert the absence of) a prompt.
    fn gate(
        policy: PermissionPolicy,
    ) -> (Arc<EventBus>, Arc<PendingPermissions>, Arc<PermissionGate>) {
        let bus = Arc::new(EventBus::new());
        let pending = Arc::new(PendingPermissions::new());
        let gate = Arc::new(PermissionGate::new(
            SessionId::from("web-tool-test"),
            PermissionConfig::with_default(policy),
            Arc::clone(&bus),
            Arc::clone(&pending),
        ));
        (bus, pending, gate)
    }

    struct Fixture {
        tool: WebTool,
        seam: Arc<FakeSeam>,
        bus: Arc<EventBus>,
        pending: Arc<PendingPermissions>,
        dir: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    /// A tool at `config`'s ceiling, whose consent answer is `policy`, with the
    /// user having pasted `pasted`.
    fn fixture(tag: &str, config: WebConfig, policy: PermissionPolicy, pasted: &[&str]) -> Fixture {
        let dir = temp_dir(tag);
        let seam = FakeSeam::new();
        let (bus, pending, gate) = gate(policy);
        let mut urls = UserUrls::new();
        for url in pasted {
            urls.insert(url);
        }
        let tool = WebTool::new(
            &config,
            WebCache::from_config(&dir, &config),
            Arc::new(Mutex::new(urls)),
            gate,
            Arc::clone(&seam) as Arc<dyn WebLookupSeam>,
            Handle::current(),
        );
        Fixture {
            tool,
            seam,
            bus,
            pending,
            dir,
        }
    }

    fn web_config(tier: WebTier) -> WebConfig {
        WebConfig {
            tier,
            ..WebConfig::default()
        }
    }

    // -----------------------------------------------------------------------
    // Arguments
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn the_kind_is_which_argument_was_supplied() {
        let fx = fixture(
            "kind",
            web_config(WebTier::Search),
            PermissionPolicy::Deny,
            &[],
        );
        // Neither, and both, are argument errors — never a guess.
        for args in [json!({}), json!({ "url": "https://a.test/", "query": "x" })] {
            let out = fx.tool.lookup(&args).await;
            assert!(out.is_error, "{args} was accepted");
            assert_eq!(fx.seam.calls(), 0);
        }
    }

    #[tokio::test]
    async fn a_non_http_scheme_is_not_a_lookup() {
        let fx = fixture(
            "scheme",
            web_config(WebTier::Search),
            PermissionPolicy::Allow,
            &[],
        );
        for url in ["file:///etc/passwd", "ftp://example.com/x", "/etc/passwd"] {
            let out = fx.tool.lookup(&json!({ "url": url })).await;
            assert!(out.is_error, "{url} was accepted as a lookup");
            assert_eq!(fx.seam.calls(), 0, "{url} reached the seam");
        }
    }

    // -----------------------------------------------------------------------
    // The tier ceiling (AC-4)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn a_model_composed_fetch_is_refused_at_the_user_url_tier_naming_the_missing_tier() {
        let fx = fixture(
            "ceiling-fetch",
            web_config(WebTier::FetchUserUrl),
            // Allow, so the refusal cannot be the permission gate's.
            PermissionPolicy::Allow,
            &[],
        );
        let out = fx
            .tool
            .lookup(&json!({ "url": "https://docs.rs/tokio" }))
            .await;
        assert!(out.is_error);
        assert!(
            out.content.contains("fetch_any_url"),
            "the refusal must name the missing tier: {}",
            out.content
        );
        assert!(out.content.contains("fetch_user_url"), "{}", out.content);
        assert_eq!(fx.seam.calls(), 0, "a refused lookup reached the wire");
        assert_eq!(
            fx.seam.recorded().first().map(|r| r.outcome),
            Some(WebLookupOutcome::RefusedTier),
            "AC-4's refusal is invisible unless it is recorded"
        );
    }

    #[tokio::test]
    async fn search_is_refused_at_the_fetch_any_url_tier_naming_the_missing_tier() {
        let fx = fixture(
            "ceiling-search",
            web_config(WebTier::FetchAnyUrl),
            PermissionPolicy::Allow,
            &[],
        );
        let out = fx
            .tool
            .lookup(&json!({ "query": "tokio task pinning" }))
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("search"), "{}", out.content);
        assert!(out.content.contains("fetch_any_url"), "{}", out.content);
        assert_eq!(fx.seam.calls(), 0);
    }

    #[tokio::test]
    async fn a_pasted_url_clears_the_floor_tier_that_refuses_a_composed_one() {
        // Non-vacuity for the test above: the same ceiling, the same tool, one
        // difference — the user pasted this URL — and the ceiling no longer
        // refuses. Without this, the refusal above could be the tool refusing
        // everything.
        let fx = fixture(
            "floor-ok",
            web_config(WebTier::FetchUserUrl),
            PermissionPolicy::Allow,
            &["https://docs.rs/tokio"],
        );
        let out = fx
            .tool
            .lookup(&json!({ "url": "https://docs.rs/tokio" }))
            .await;
        assert!(
            !out.content.contains("tier"),
            "the ceiling refused a pasted URL: {}",
            out.content
        );
        assert_eq!(
            fx.seam.calls(),
            1,
            "the pasted fetch never reached the seam"
        );
    }

    // -----------------------------------------------------------------------
    // The allowlist (BR-11, AC-9)
    // -----------------------------------------------------------------------

    fn allowlisted(tier: WebTier, domains: &[&str]) -> WebConfig {
        WebConfig {
            tier,
            allowed_domains: Some(domains.iter().map(|s| (*s).to_owned()).collect()),
            ..WebConfig::default()
        }
    }

    #[tokio::test]
    async fn an_out_of_list_model_composed_fetch_is_refused_naming_the_allowlist() {
        let fx = fixture(
            "allow-no",
            allowlisted(WebTier::FetchAnyUrl, &["docs.rs", "*.rust-lang.org"]),
            PermissionPolicy::Allow,
            &[],
        );
        let out = fx
            .tool
            .lookup(&json!({ "url": "https://evil.test/x" }))
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("allowed_domains"), "{}", out.content);
        assert!(out.content.contains("docs.rs"), "{}", out.content);
        assert!(out.content.contains("evil.test"), "{}", out.content);
        assert_eq!(fx.seam.calls(), 0);
        assert_eq!(
            fx.seam.recorded().first().map(|r| r.outcome),
            Some(WebLookupOutcome::RefusedDomain)
        );
    }

    #[tokio::test]
    async fn a_user_pasted_url_is_exempt_from_the_allowlist() {
        // BR-11's exemption, and the discriminating case for AC-9: the same
        // out-of-list host, the same allowlist, allowed because the user typed
        // it.
        let fx = fixture(
            "allow-exempt",
            allowlisted(WebTier::FetchUserUrl, &["docs.rs"]),
            PermissionPolicy::Allow,
            &["https://evil.test/x"],
        );
        let out = fx
            .tool
            .lookup(&json!({ "url": "https://evil.test/x" }))
            .await;
        assert!(
            !out.content.contains("allowed_domains"),
            "the allowlist refused a pasted URL: {}",
            out.content
        );
        assert_eq!(fx.seam.calls(), 1);
    }

    #[tokio::test]
    async fn an_in_list_model_composed_fetch_proceeds() {
        let fx = fixture(
            "allow-yes",
            allowlisted(WebTier::FetchAnyUrl, &["*.rust-lang.org"]),
            PermissionPolicy::Allow,
            &[],
        );
        let out = fx
            .tool
            .lookup(&json!({ "url": "https://doc.rust-lang.org/std" }))
            .await;
        assert!(!out.content.contains("allowed_domains"), "{}", out.content);
        assert_eq!(fx.seam.calls(), 1);
    }

    #[tokio::test]
    async fn no_allowlist_means_the_tier_grant_alone_governs() {
        // BR-11: an absent allowlist is a valid, unrestricted configuration —
        // not a warning state and not an empty one.
        let fx = fixture(
            "allow-none",
            web_config(WebTier::FetchAnyUrl),
            PermissionPolicy::Allow,
            &[],
        );
        let out = fx
            .tool
            .lookup(&json!({ "url": "https://anywhere.test/x" }))
            .await;
        assert!(!out.content.contains("allowed_domains"), "{}", out.content);
        assert_eq!(fx.seam.calls(), 1);
    }

    #[tokio::test]
    async fn an_empty_allowlist_permits_nothing() {
        // The state deliberately not collapsed into "unrestricted", asserted so
        // that collapsing it later turns this red.
        let fx = fixture(
            "allow-empty",
            allowlisted(WebTier::FetchAnyUrl, &[]),
            PermissionPolicy::Allow,
            &[],
        );
        let out = fx.tool.lookup(&json!({ "url": "https://docs.rs/x" })).await;
        assert!(out.is_error);
        assert!(out.content.contains("permits nothing"), "{}", out.content);
        assert_eq!(fx.seam.calls(), 0);
    }

    // -----------------------------------------------------------------------
    // The cache (BR-12, AC-10)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn a_fresh_cache_hit_serves_the_document_with_no_egress_and_no_prompt() {
        const URL: &str = "https://docs.rs/tokio/latest";
        let config = web_config(WebTier::FetchAnyUrl);
        let dir = temp_dir("cache-hit");
        let cache = WebCache::from_config(&dir, &config);
        cache.put(URL, "the reduced page text").unwrap();

        let seam = FakeSeam::new();
        // `Ask` with **no client attached**: any prompt at all would hang this
        // test rather than fail it quietly, and the assertion below on
        // `pending_count` names the property directly.
        let (bus, pending, gate) = gate(PermissionPolicy::Ask);
        let mut sub = bus.subscribe(16);
        let tool = WebTool::new(
            &config,
            cache,
            Arc::new(Mutex::new(UserUrls::new())),
            gate,
            Arc::clone(&seam) as Arc<dyn WebLookupSeam>,
            Handle::current(),
        );

        let out = tool.lookup(&json!({ "url": URL })).await;

        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("the reduced page text"));
        assert!(out.content.contains("local cache"), "{}", out.content);
        assert_eq!(seam.calls(), 0, "a cache hit reached the network");
        assert_eq!(
            pending.pending_count(),
            0,
            "a cache hit asked the user for permission (BR-12: nothing left the machine)"
        );
        assert!(
            !std::iter::from_fn(|| sub.try_recv())
                .any(|env| matches!(env.event, Event::PermissionRequest(_))),
            "a cache hit published a permission_request"
        );
        assert_eq!(
            seam.recorded().first().map(|r| r.outcome),
            Some(WebLookupOutcome::CacheHit),
            "a cache hit is still a ledger row (BR-12)"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_cache_miss_takes_the_normal_flow() {
        // Non-vacuity for the test above: the same tool, a URL that is not
        // cached, and the seam is reached.
        let fx = fixture(
            "cache-miss",
            web_config(WebTier::FetchAnyUrl),
            PermissionPolicy::Allow,
            &[],
        );
        let out = fx
            .tool
            .lookup(&json!({ "url": "https://docs.rs/never-cached" }))
            .await;
        assert_eq!(fx.seam.calls(), 1);
        assert!(!out.content.contains("local cache"));
    }

    #[tokio::test]
    async fn the_cache_is_consulted_after_the_gates_not_before_them() {
        // A cached document must not become a way to exercise a capability the
        // ceiling refuses: the tier answer comes first, whatever is on disk.
        const URL: &str = "https://docs.rs/tokio/latest";
        let config = web_config(WebTier::FetchUserUrl);
        let dir = temp_dir("cache-order");
        let cache = WebCache::from_config(&dir, &config);
        cache.put(URL, "the reduced page text").unwrap();

        let seam = FakeSeam::new();
        let (_bus, _pending, gate) = gate(PermissionPolicy::Allow);
        let tool = WebTool::new(
            &config,
            cache,
            // Not pasted, so this is a model-composed fetch above the ceiling.
            Arc::new(Mutex::new(UserUrls::new())),
            gate,
            Arc::clone(&seam) as Arc<dyn WebLookupSeam>,
            Handle::current(),
        );

        let out = tool.lookup(&json!({ "url": URL })).await;
        assert!(out.is_error, "the cache served an over-ceiling lookup");
        assert!(out.content.contains("fetch_any_url"), "{}", out.content);
        assert!(!out.content.contains("the reduced page text"));
        std::fs::remove_dir_all(&dir).ok();
    }

    // -----------------------------------------------------------------------
    // Consent (BR-3, BR-4)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn the_prompt_shows_the_verbatim_url_and_the_destination_host() {
        const URL: &str = "https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html?x=1";
        let fx = fixture(
            "desc-fetch",
            web_config(WebTier::FetchAnyUrl),
            PermissionPolicy::Ask,
            &[],
        );
        let mut sub = fx.bus.subscribe(16);

        let args = json!({ "url": URL });
        let ask = fx.tool.lookup(&args);
        let answer = async {
            let env = sub.recv().await.unwrap();
            let Event::PermissionRequest(request) = env.event else {
                panic!("expected a permission_request");
            };
            let description = request.description.clone().unwrap_or_default();
            assert!(
                description.contains(URL),
                "the prompt must show the URL verbatim: {description}"
            );
            assert!(
                description.contains("docs.rs"),
                "the prompt must name the destination host: {description}"
            );
            assert_eq!(
                request.tool_name, PERMISSION_KEY_FETCH_ANY_URL,
                "a fetch must be authorized under its own tier's key"
            );
            fx.pending.resolve(
                &request.request_id,
                PermissionOutcome::Selected {
                    option_id: "reject_once".to_owned(),
                },
            );
        };
        let (out, ()) = tokio::join!(ask, answer);
        assert!(out.is_error);
        assert_eq!(fx.seam.calls(), 0, "a declined lookup reached the wire");
    }

    #[tokio::test]
    async fn the_prompt_shows_the_verbatim_query_and_the_search_host() {
        const QUERY: &str = "how do I pin a tokio task to a thread";
        let config = WebConfig {
            tier: WebTier::Search,
            search_endpoint: Some("https://api.search.test/v1/search".to_owned()),
            ..WebConfig::default()
        };
        let fx_dir = temp_dir("desc-search");
        let seam = FakeSeam::new();
        let (bus, pending, gate) = gate(PermissionPolicy::Ask);
        let mut sub = bus.subscribe(16);
        let tool = WebTool::new(
            &config,
            WebCache::from_config(&fx_dir, &config),
            Arc::new(Mutex::new(UserUrls::new())),
            gate,
            Arc::clone(&seam) as Arc<dyn WebLookupSeam>,
            Handle::current(),
        );

        let args = json!({ "query": QUERY });
        let ask = tool.lookup(&args);
        let answer = async {
            let env = sub.recv().await.unwrap();
            let Event::PermissionRequest(request) = env.event else {
                panic!("expected a permission_request");
            };
            let description = request.description.clone().unwrap_or_default();
            assert!(description.contains(QUERY), "{description}");
            assert!(
                description.contains("api.search.test"),
                "the prompt must name the search backend's host: {description}"
            );
            assert_eq!(
                request.tool_name, PERMISSION_KEY_SEARCH,
                "a search must be authorized under its own tier's key"
            );
            // And the endpoint's *path* is not the user's question — the host
            // is what BR-4 asks for.
            pending.resolve(
                &request.request_id,
                PermissionOutcome::Selected {
                    option_id: "reject_once".to_owned(),
                },
            );
        };
        let (out, ()) = tokio::join!(ask, answer);
        assert!(out.is_error);
        assert_eq!(seam.calls(), 0);
        std::fs::remove_dir_all(&fx_dir).ok();
    }

    #[tokio::test]
    async fn a_session_grant_on_fetch_does_not_grant_search() {
        // BR-3's "separately consented" clause, at the one place it can be
        // violated silently: `PermissionGate` remembers an `allow_always` under
        // the string it was asked about, so a shared key would have made one
        // answer grant both capabilities.
        let config = WebConfig {
            tier: WebTier::Search,
            search_endpoint: Some("https://api.search.test/v1".to_owned()),
            ..WebConfig::default()
        };
        let dir = temp_dir("separate-grants");
        let seam = FakeSeam::new();
        let (bus, pending, gate) = gate(PermissionPolicy::Ask);
        let mut sub = bus.subscribe(16);
        let tool = WebTool::new(
            &config,
            WebCache::from_config(&dir, &config),
            Arc::new(Mutex::new(UserUrls::new())),
            gate,
            Arc::clone(&seam) as Arc<dyn WebLookupSeam>,
            Handle::current(),
        );

        // Grant fetch for the session.
        let fetch_args = json!({ "url": "https://docs.rs/a" });
        let ask = tool.lookup(&fetch_args);
        let answer = async {
            let env = sub.recv().await.unwrap();
            let Event::PermissionRequest(request) = env.event else {
                panic!("expected a permission_request");
            };
            assert_eq!(request.tool_name, PERMISSION_KEY_FETCH_ANY_URL);
            pending.resolve(
                &request.request_id,
                PermissionOutcome::Selected {
                    option_id: "allow_always".to_owned(),
                },
            );
        };
        let (_, ()) = tokio::join!(ask, answer);

        // A second fetch rides the grant with no prompt...
        let out = tool.lookup(&json!({ "url": "https://docs.rs/b" })).await;
        assert_eq!(pending.pending_count(), 0, "{}", out.content);

        // ...and a search still asks.
        //
        // Skipping past non-prompt events rather than asserting the *next* event
        // is one: since REQ-563's TASK-077 the fetch decision also publishes a
        // `web_consent_decided`, and this test's claim is that a search raises
        // its own prompt — not that nothing else is ever said in between.
        let search_args = json!({ "query": "anything" });
        let ask = tool.lookup(&search_args);
        let answer = async {
            let request = loop {
                let env = sub
                    .recv()
                    .await
                    .expect("a session grant on fetch silently granted search");
                if let Event::PermissionRequest(request) = env.event {
                    break request;
                }
            };
            assert_eq!(request.tool_name, PERMISSION_KEY_SEARCH);
            pending.resolve(&request.request_id, PermissionOutcome::Cancelled);
        };
        let (out, ()) = tokio::join!(ask, answer);
        assert!(out.is_error);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_denied_lookup_records_nothing_and_sends_nothing() {
        let fx = fixture(
            "denied",
            web_config(WebTier::FetchAnyUrl),
            PermissionPolicy::Deny,
            &[],
        );
        let out = fx.tool.lookup(&json!({ "url": "https://docs.rs/x" })).await;
        assert!(out.is_error);
        assert!(out.content.contains("declined"), "{}", out.content);
        assert_eq!(fx.seam.calls(), 0);
        assert!(
            fx.seam.recorded().is_empty(),
            "a declined lookup has no outcome vocabulary and must not borrow a neighbouring one"
        );
    }

    // -----------------------------------------------------------------------
    // Shape
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn every_outcome_carries_empty_provenance() {
        // LESSON-432 / D-3: a web result touched no repo file, so it is
        // `Sources(∅)` and never `Unknown`. `Unknown` would fail-close provider
        // egress for the rest of a session over content whose origin the daemon
        // knows exactly.
        let fx = fixture(
            "prov",
            web_config(WebTier::Search),
            PermissionPolicy::Allow,
            &[],
        );
        for args in [
            json!({}),                              // argument error
            json!({ "url": "https://docs.rs/x" }),  // reaches the seam
            json!({ "query": "anything" }),         // reaches the seam
            json!({ "url": "file:///etc/passwd" }), // refused
        ] {
            let out = fx.tool.lookup(&args).await;
            assert_eq!(
                out.provenance,
                ToolProvenance::none(),
                "{args} carried provenance"
            );
        }
    }

    #[test]
    fn the_description_is_terse_and_tier_appropriate() {
        // Tool docs are resident prompt bytes on every turn (LESSON-493), and a
        // fetch-only ceiling must not advertise a search it would only refuse.
        for tier in [WebTier::FetchUserUrl, WebTier::FetchAnyUrl] {
            let description = if web_config(tier).tier.allows(WebTier::Search) {
                DESCRIPTION_SEARCH
            } else {
                DESCRIPTION_FETCH
            };
            assert!(!description.contains("search"), "{tier:?}: {description}");
        }
        for description in [DESCRIPTION_FETCH, DESCRIPTION_SEARCH] {
            assert!(
                description.len() < 100,
                "a tool description over 100 bytes: {description}"
            );
        }
    }

    #[tokio::test]
    async fn registration_happens_only_above_off() {
        let dir = temp_dir("register");
        let seam = FakeSeam::new();
        let (_bus, _pending, gate) = gate(PermissionPolicy::Ask);
        let make = |config: &WebConfig, reg: &mut ToolRegistry| {
            register_web_tool(
                reg,
                config,
                WebCache::from_config(&dir, config),
                Arc::new(Mutex::new(UserUrls::new())),
                Arc::clone(&gate),
                Arc::clone(&seam) as Arc<dyn WebLookupSeam>,
                Handle::current(),
            )
        };

        let mut off = ToolRegistry::with_builtins();
        assert!(!make(&web_config(WebTier::Off), &mut off));
        assert!(off.get(WEB_TOOL_NAME).is_none());

        for tier in [WebTier::FetchUserUrl, WebTier::FetchAnyUrl, WebTier::Search] {
            let mut reg = ToolRegistry::with_builtins();
            assert!(make(&web_config(tier), &mut reg), "{tier:?}");
            assert!(reg.get(WEB_TOOL_NAME).is_some(), "{tier:?}");
            assert_eq!(
                reg.names().last().copied(),
                Some(WEB_TOOL_NAME),
                "the web tool must register LAST so a max_tools cap cuts it first (BR-6)"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A connector that never connects — enough to construct an
    /// [`McpRegistry`](crate::mcp::McpRegistry) for a handle whose `run` is never
    /// called. The sweep below asks each tool a question about itself, not about
    /// its backend.
    struct UnusedConnector;

    #[async_trait]
    impl crate::mcp::McpConnector for UnusedConnector {
        async fn connect(
            &self,
            config: &crate::mcp::McpServerConfig,
        ) -> Result<Arc<dyn crate::mcp::McpConnection>, crate::mcp::McpError> {
            Err(crate::mcp::McpError::Startup(config.id.clone()))
        }
    }

    /// One MCP tool, built exactly as `register_mcp_tools` builds them.
    fn mcp_tool_handle() -> Arc<dyn Tool> {
        let registry = Arc::new(crate::mcp::McpRegistry::new(
            Arc::new(UnusedConnector),
            Vec::new(),
        ));
        let discovered = crate::mcp::DiscoveredTool {
            server_id: "fs".to_owned(),
            tool: crate::mcp::McpTool {
                name: "read_file".to_owned(),
                description: "read a file".to_owned(),
                input_schema: json!({ "type": "object" }),
            },
        };
        Arc::new(crate::harness::tools::McpToolHandle::new(
            &discovered,
            registry,
            Handle::current(),
            false,
        ))
    }

    /// **`gates_itself` is the web tool's alone — across every kind of tool the
    /// registry can hold.**
    ///
    /// The surface it opens is fail-open: a tool that answers `true` is *not*
    /// authorized by the loop, so a second one that started answering `true`
    /// would run unprompted. The sweep is therefore only as good as the tool set
    /// it sweeps, and `with_builtins()` is not that set — the daemon also
    /// registers an [`McpToolHandle`](crate::harness::tools::McpToolHandle) per
    /// discovered MCP tool, from a *different* `Tool` impl, and a self-gating
    /// one would be an unprompted subprocess or an unprompted remote call.
    /// Registering one here means a future `gates_itself` on that impl fails this
    /// test rather than shipping.
    #[tokio::test]
    async fn the_web_tool_is_the_only_tool_that_gates_itself() {
        // The fail-open surface `gates_itself` opens, bounded by an assertion:
        // every other tool is authorized by the loop, by name, before it runs.
        let dir = temp_dir("gates-itself");
        let seam = FakeSeam::new();
        let (_bus, _pending, gate) = gate(PermissionPolicy::Ask);
        let mut reg = ToolRegistry::with_builtins();
        assert!(register_web_tool(
            &mut reg,
            &web_config(WebTier::Search),
            WebCache::from_config(&dir, &web_config(WebTier::Search)),
            Arc::new(Mutex::new(UserUrls::new())),
            gate,
            Arc::clone(&seam) as Arc<dyn WebLookupSeam>,
            Handle::current(),
        ));
        reg.register(mcp_tool_handle());

        let names = reg.names();
        // Non-vacuity, both ends: the sweep really did see the web tool and
        // really did see a second `Tool` impl.
        assert!(names.contains(&WEB_TOOL_NAME));
        assert!(
            names.iter().any(|n| n.starts_with("mcp__")),
            "the sweep must include an MCP-shaped tool: {names:?}"
        );

        for name in names {
            let tool = reg.get(name).unwrap();
            assert_eq!(
                tool.gates_itself(),
                name == WEB_TOOL_NAME,
                "`{name}` disagrees with the loop about who raises its permission prompt"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The sync→async bridge itself, which no other test in this module
    /// exercises: `run` is what the loop calls, and it needs the multi-threaded
    /// runtime `block_in_place` requires.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_bridges_into_the_async_orchestration() {
        let fx = fixture(
            "bridge",
            web_config(WebTier::FetchUserUrl),
            PermissionPolicy::Allow,
            &[],
        );
        let ctx = ToolContext::new(std::env::temp_dir());
        let out = fx
            .tool
            .run(&ctx, &json!({ "url": "https://docs.rs/tokio" }));
        // Above the ceiling, so this is the tier refusal — reached through
        // `run`, which is the point.
        assert!(out.is_error);
        assert!(out.content.contains("fetch_any_url"), "{}", out.content);
    }

    #[test]
    fn every_tier_has_its_config_spelling() {
        // The sweep exists so a tier added to the ladder cannot render as a
        // debug string in a sentence a user reads (AC-4's "names the tier").
        for tier in WebTier::ALL {
            let name = tier_name(tier);
            assert!(!name.is_empty());
            assert_eq!(
                name.to_ascii_lowercase(),
                name,
                "{tier:?} renders in something other than the config spelling"
            );
        }
        assert_eq!(tier_name(WebTier::FetchAnyUrl), "fetch_any_url");
    }

    // -----------------------------------------------------------------------
    // Delivery (BR-10, BR-12)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn a_delivered_page_is_reduced_locally_and_cached() {
        const URL: &str = "https://docs.rs/tokio/latest";
        let fx = fixture(
            "deliver",
            web_config(WebTier::FetchAnyUrl),
            PermissionPolicy::Allow,
            &[],
        );
        let call = Call::Fetch {
            url: URL.to_owned(),
            host: "docs.rs".to_owned(),
            authorship: Authorship::ModelComposed,
        };
        let html = "<html><head><style>b{}</style></head><body><h1>Spawn</h1>\
                    <script>alert(1)</script><p>Use spawn_blocking &amp; wait.</p></body></html>";

        let out = fx
            .tool
            .deliver(&call, html.as_bytes(), html.len() as u64, false);

        assert!(!out.is_error);
        // BR-10: what enters context is the *local* reduction — tags gone,
        // scripts and styles gone with their content, entities decoded.
        assert!(out.content.contains("Spawn"), "{}", out.content);
        assert!(
            out.content.contains("Use spawn_blocking & wait."),
            "{}",
            out.content
        );
        assert!(!out.content.contains("<h1>"), "{}", out.content);
        assert!(!out.content.contains("alert(1)"), "{}", out.content);
        // The header names the destination the user consented to.
        assert!(out.content.contains(URL), "{}", out.content);
        assert!(out.content.contains("docs.rs"), "{}", out.content);
        assert_eq!(out.provenance, ToolProvenance::none());

        // BR-12: the reduction — not the raw page — is what the cache holds,
        // so the next lookup of this URL is a zero-egress hit.
        let entry = fx.tool.cache.get(URL).expect("the page was cached");
        assert!(entry.content.contains("Spawn"));
        assert!(!entry.content.contains("<h1>"));
        assert_eq!(fx.seam.calls(), 0, "delivery is not a second lookup");
    }

    /// **The opted-in prompt shape clears the outbound-body budget**
    /// (LESSON-493 / BUG-160's pattern, REQ-563's half).
    ///
    /// `the_total_cap_clears_the_harness_context_budget_with_margin`
    /// (`egress::redact`) measures the *opted-out* shape: no web tool, the BR-6
    /// clause in its place. This measures the other one — the web tool's
    /// description **and its schema**, rendered by the real
    /// [`build_system_prompt`](crate::harness::turn_loop::build_system_prompt)
    /// with the tool cap lifted, against the same constant.
    ///
    /// Asserted on the real rendered prompt rather than on the description's
    /// length, because the schema is the larger half and a terse description
    /// beside a verbose schema is exactly the shape that would pass a
    /// description-only check.
    #[tokio::test]
    async fn the_web_tool_docs_clear_the_outbound_body_overhead() {
        use crate::egress::redact::REDACT_BODY_OVERHEAD_BYTES;
        use crate::harness::turn_loop::{build_system_prompt, HarnessConfig};

        let dir = temp_dir("budget");
        // The largest shape: `search` (the longer description) and
        // `max_tools: None`, so every tool's docs are resident.
        let config = WebConfig {
            tier: WebTier::Search,
            search_endpoint: Some("https://api.search.test/v1".to_owned()),
            ..WebConfig::default()
        };
        let (_bus, _pending, gate) = gate(PermissionPolicy::Ask);
        let mut tools = ToolRegistry::with_builtins();
        assert!(register_web_tool(
            &mut tools,
            &config,
            WebCache::from_config(&dir, &config),
            Arc::new(Mutex::new(UserUrls::new())),
            gate,
            FakeSeam::new() as Arc<dyn WebLookupSeam>,
            Handle::current(),
        ));

        let harness = HarnessConfig::for_strong_model();
        let system = build_system_prompt(&tools, &harness);
        // Non-vacuity: the tool's docs really are in what is being measured.
        assert!(system.contains(WEB_TOOL_NAME), "{system}");
        assert!(system.contains(DESCRIPTION_SEARCH), "{system}");
        assert!(
            system.contains("\"query\""),
            "the schema is missing:\n{system}"
        );

        let escaping = harness.context_budget_bytes / 10;
        assert!(
            system.len() + escaping <= REDACT_BODY_OVERHEAD_BYTES,
            "the web tool's docs pushed the outbound body past its assumed \
             overhead: a {}-byte system prompt plus {escaping} bytes of escaping \
             against an assumed {REDACT_BODY_OVERHEAD_BYTES}. Shorten the \
             description or the schema; do not raise the assumption without \
             raising the cap it feeds.",
            system.len()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn offline_reads_as_transient_and_a_privacy_block_names_no_cause() {
        use teton_protocol::events::{BlockCause, ByteSpan, FindingKind};
        use teton_providers::transport::TransportError;

        let dir = temp_dir("sentences");
        let config = web_config(WebTier::Search);
        let (_bus, _pending, gate) = gate(PermissionPolicy::Allow);
        let tool = WebTool::new(
            &config,
            WebCache::from_config(&dir, &config),
            Arc::new(Mutex::new(UserUrls::new())),
            gate,
            FakeSeam::new() as Arc<dyn WebLookupSeam>,
            Handle::current(),
        );

        // BR-9 / BUG-152: unreachable is transient-shaped and says so, and is
        // never dressed up as a fault in the destination.
        let offline = tool.failure_sentence(&LookupDetail::Unreachable {
            error: TransportError::Connect,
        });
        assert!(offline.contains("offline"), "{offline}");
        assert!(offline.contains("unavailable"), "{offline}");

        // A 404 is a settled fact about a host that IS reachable — reporting it
        // as offline sends the user to debug a working network.
        let status = tool.failure_sentence(&LookupDetail::HttpStatus { status: 404 });
        assert!(status.contains("404"), "{status}");
        assert!(!status.contains("offline"), "{status}");

        // Every privacy cause reads the same to the model: a cause-distinct
        // sentence is a multi-way oracle on an input the model controls (the
        // `mcp` bridge's argument, and the same conclusion).
        let boundary = tool.failure_sentence(&LookupDetail::Blocked {
            cause: BlockCause::Boundary,
        });
        let redaction = tool.failure_sentence(&LookupDetail::Blocked {
            cause: BlockCause::Redaction {
                kind: FindingKind::Secret,
                span: ByteSpan { start: 0, end: 8 },
            },
        });
        let unavailable = tool.failure_sentence(&LookupDetail::Blocked {
            cause: BlockCause::ScanUnavailable,
        });
        assert_eq!(boundary, redaction, "the cause leaked to the model");
        assert_eq!(redaction, unavailable, "the cause leaked to the model");
        for word in ["boundary", "redaction", "scan", "credential"] {
            assert!(
                !boundary.to_lowercase().contains(word),
                "`{word}` reached the model: {boundary}"
            );
        }

        // BR-13's notice names both the cause and the effect, because a
        // restriction the user cannot see is one they cannot lift.
        let tainted = tool.failure_sentence(&LookupDetail::TaintRestricted);
        assert!(tainted.contains("boundary"), "{tainted}");
        assert!(tainted.contains("restrict"), "{tainted}");
        assert!(tainted.contains("pasted"), "{tainted}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The address-class refusal names no class, and the search-endpoint one
    /// names the routing — the asymmetry is deliberate (REQ-563 verify).
    #[tokio::test]
    async fn a_refused_address_names_no_class_and_a_search_endpoint_fetch_names_the_tier() {
        use crate::egress::AddressClass;

        let fx = fixture(
            "sentences-2",
            web_config(WebTier::Search),
            PermissionPolicy::Allow,
            &[],
        );

        // A four-way oracle on an input the model controls is a network map;
        // every class must read identically.
        let sentences: Vec<String> = [
            AddressClass::Loopback,
            AddressClass::LinkLocal,
            AddressClass::Private,
            AddressClass::Unspecified,
        ]
        .into_iter()
        .map(|class| {
            fx.tool
                .failure_sentence(&LookupDetail::RefusedAddress { class })
        })
        .collect();
        for sentence in &sentences {
            assert_eq!(sentence, &sentences[0], "the address class leaked");
            for word in ["loopback", "link", "private", "unspecified", "range"] {
                assert!(
                    !sentence.to_lowercase().contains(word),
                    "`{word}` reached the model: {sentence}"
                );
            }
        }
        assert!(
            sentences[0].contains("public internet address"),
            "{}",
            sentences[0]
        );

        // The search endpoint, by contrast, is a value out of the user's own
        // config and the fact is directly actionable — so it names the route.
        let endpoint = fx.tool.failure_sentence(&LookupDetail::SearchEndpointFetch);
        assert!(endpoint.contains("query"), "{endpoint}");
        assert!(endpoint.contains("search backend"), "{endpoint}");
        assert!(
            endpoint.contains("url"),
            "the model must be told which argument it used wrongly: {endpoint}"
        );
    }

    // -----------------------------------------------------------------------
    // One parser (REQ-563 verify, C-1)
    // -----------------------------------------------------------------------

    /// Drive one `Ask` prompt to `option_id`, returning the outcome and the
    /// request the client actually received.
    async fn ask(
        fx: &Fixture,
        args: Value,
        option_id: &str,
    ) -> (ToolOutcome, teton_protocol::events::PermissionRequest) {
        let mut sub = fx.bus.subscribe(16);
        let run = fx.tool.lookup(&args);
        let answer = async {
            let request = loop {
                let env = sub.recv().await.expect("no permission_request was raised");
                if let Event::PermissionRequest(request) = env.event {
                    break request;
                }
            };
            fx.pending.resolve(
                &request.request_id,
                PermissionOutcome::Selected {
                    option_id: option_id.to_owned(),
                },
            );
            request
        };
        tokio::join!(run, answer)
    }

    /// **The gate, the prompt and the wire read one parser.**
    ///
    /// Each of these spellings has an authority that a hand-written splitter
    /// reads differently from the socket. The tool must gate on — and show, and
    /// send — the socket's reading, so an allowlist naming the *decoy* refuses
    /// them all and the refusal names the host the bytes would actually reach.
    #[tokio::test]
    async fn a_spoofed_authority_is_gated_on_the_host_the_socket_would_reach() {
        let cases = [
            // WHATWG treats `\` as a path separator, so the `@` is a path byte
            // and the authority ends at the backslash.
            ("https://evil.example\\@allowed.example/x", "evil.example"),
            // Userinfo is not a host, however much it is spelled like one.
            ("https://allowed.example@evil.example/x", "evil.example"),
            // A decimal integer is an IPv4 address.
            ("http://2130706433/admin", "127.0.0.1"),
            // An IPv6 literal keeps its brackets, which is the wire spelling.
            ("http://[::1]:8080/admin", "[::1]"),
        ];
        for (raw, real_host) in cases {
            let fx = fixture(
                "spoof",
                allowlisted(WebTier::FetchAnyUrl, &["allowed.example"]),
                // Allow, so a refusal can only be the allowlist's.
                PermissionPolicy::Allow,
                &[],
            );
            let out = fx.tool.lookup(&json!({ "url": raw })).await;
            assert!(out.is_error, "{raw} was not refused");
            assert!(
                out.content.contains(real_host),
                "{raw}: the refusal must name the host the request would reach, got {}",
                out.content
            );
            assert!(
                out.content.contains("allowed_domains"),
                "{raw}: {}",
                out.content
            );
            assert_eq!(fx.seam.calls(), 0, "{raw} reached the seam");
        }
    }

    /// …and the consent prompt asks about the same string that would be sent.
    ///
    /// The half the allowlist test above cannot cover: with no allowlist the
    /// spoof proceeds to the prompt, and what the user is shown has to be the
    /// re-serialized URL and the real host — not the decoy the model wrote.
    #[tokio::test]
    async fn the_prompt_shows_the_reserialized_url_and_the_real_host() {
        let fx = fixture(
            "spoof-prompt",
            web_config(WebTier::FetchAnyUrl),
            PermissionPolicy::Ask,
            &[],
        );
        let (out, request) = ask(
            &fx,
            json!({ "url": "https://evil.example\\@allowed.example/x" }),
            "reject_once",
        )
        .await;
        let description = request.description.clone().unwrap_or_default();
        assert!(
            description.contains("(host evil.example)"),
            "the prompt named the decoy host: {description}"
        );
        assert!(
            description.contains("https://evil.example/@allowed.example/x"),
            "the prompt must echo the URL as it would be sent: {description}"
        );
        assert!(out.is_error);
        assert_eq!(fx.seam.calls(), 0);
    }

    /// The string handed to the seam is the re-serialized one, so "checked",
    /// "shown" and "sent" are one value rather than three readings.
    #[tokio::test]
    async fn the_seam_receives_the_reserialized_url() {
        let fx = fixture(
            "reserialized",
            web_config(WebTier::FetchAnyUrl),
            PermissionPolicy::Allow,
            &[],
        );
        // No allowlist, so this reaches the seam; the fake seam records the
        // request it was handed.
        let _ = fx
            .tool
            .lookup(&json!({ "url": "HTTPS://Docs.RS:443/tokio" }))
            .await;
        assert_eq!(fx.seam.requests(), vec!["https://docs.rs/tokio".to_owned()]);
    }

    /// A URL the user pasted keeps its exemption when the re-serializer moves
    /// it — the ordinary case, not an edge one: `https://docs.rs` gains a `/`.
    #[tokio::test]
    async fn a_pasted_url_whose_reserialization_moves_is_still_pasted() {
        let fx = fixture(
            "reserialize-pasted",
            web_config(WebTier::FetchUserUrl),
            PermissionPolicy::Allow,
            &["https://docs.rs"],
        );
        let out = fx.tool.lookup(&json!({ "url": "https://docs.rs" })).await;
        assert!(
            !out.content.contains("tier"),
            "the ceiling refused the user's own link: {}",
            out.content
        );
        assert_eq!(fx.seam.calls(), 1);
    }

    // -----------------------------------------------------------------------
    // Per-tier permission keys (REQ-563 verify, C-2)
    // -----------------------------------------------------------------------

    /// **Mixed authorship: a grant on a pasted URL does not cover a composed
    /// one.**
    ///
    /// The case BR-3 names first, and the one a shared `web_fetch` key made
    /// silently wrong: the user pastes a link, allows fetches "for this
    /// session", and the model then composes a URL of its own — which must
    /// raise its own prompt, because the question the user answered was about a
    /// URL they chose.
    #[tokio::test]
    async fn a_session_grant_on_a_pasted_fetch_does_not_grant_a_composed_one() {
        let fx = fixture(
            "mixed-authorship",
            web_config(WebTier::FetchAnyUrl),
            PermissionPolicy::Ask,
            &["https://docs.rs/pasted"],
        );

        let (_, first) = ask(
            &fx,
            json!({ "url": "https://docs.rs/pasted" }),
            "allow_always",
        )
        .await;
        assert_eq!(first.tool_name, PERMISSION_KEY_FETCH_USER_URL);

        // A second pasted fetch rides the grant with no prompt…
        let mut sub = fx.bus.subscribe(16);
        let _ = fx
            .tool
            .lookup(&json!({ "url": "https://docs.rs/pasted" }))
            .await;
        assert!(
            !std::iter::from_fn(|| sub.try_recv())
                .any(|env| matches!(env.event, Event::PermissionRequest(_))),
            "the grant did not answer a second lookup at its own tier"
        );

        // …and a model-composed one asks again, under its own key.
        let (out, second) = ask(
            &fx,
            json!({ "url": "https://docs.rs/composed" }),
            "reject_once",
        )
        .await;
        assert_eq!(
            second.tool_name, PERMISSION_KEY_FETCH_ANY_URL,
            "a composed fetch must be authorized under the tier it needs"
        );
        assert!(out.is_error, "the reject was not honoured");
    }

    /// The mapping itself: one key per tier above `off`, all distinct, and
    /// nothing for `off`.
    #[test]
    fn every_tier_above_off_has_its_own_distinct_permission_key() {
        assert_eq!(permission_key_for(WebTier::Off), None);
        let keys: Vec<&str> = WebTier::ALL
            .into_iter()
            .filter_map(permission_key_for)
            .collect();
        assert_eq!(keys, WEB_PERMISSION_KEYS.to_vec());
        assert_eq!(
            keys.iter().collect::<std::collections::HashSet<_>>().len(),
            keys.len(),
            "two tiers share a consent key, so one grant answers both"
        );
    }

    // -----------------------------------------------------------------------
    // A cache hit honours a standing denial (REQ-563 verify)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn a_cache_hit_is_refused_after_a_reject_for_this_session() {
        const URL: &str = "https://docs.rs/tokio/latest";
        let config = web_config(WebTier::FetchAnyUrl);
        let dir = temp_dir("cache-denied");
        let cache = WebCache::from_config(&dir, &config);
        cache.put(URL, "the reduced page text").unwrap();

        let seam = FakeSeam::new();
        let (bus, pending, gate) = gate(PermissionPolicy::Ask);
        let tool = WebTool::new(
            &config,
            cache,
            Arc::new(Mutex::new(UserUrls::new())),
            Arc::clone(&gate),
            Arc::clone(&seam) as Arc<dyn WebLookupSeam>,
            Handle::current(),
        );

        // The user refuses the capability for the session, on an uncached URL.
        let mut sub = bus.subscribe(16);
        let other = json!({ "url": "https://docs.rs/other" });
        let run = tool.lookup(&other);
        let answer = async {
            let request = loop {
                let env = sub.recv().await.unwrap();
                if let Event::PermissionRequest(request) = env.event {
                    break request;
                }
            };
            pending.resolve(
                &request.request_id,
                PermissionOutcome::Selected {
                    option_id: "reject_always".to_owned(),
                },
            );
        };
        let (denied, ()) = tokio::join!(run, answer);
        assert!(denied.is_error);

        // The cached URL must not become the way around that answer — and it
        // must not raise a prompt either: the answer is already on file.
        let out = tool.lookup(&json!({ "url": URL })).await;
        assert!(out.is_error, "a cache hit served past a standing denial");
        assert!(
            !out.content.contains("the reduced page text"),
            "{}",
            out.content
        );
        assert_eq!(pending.pending_count(), 0, "a cache hit raised a prompt");
        assert_eq!(seam.calls(), 0);
        assert!(
            !seam
                .recorded()
                .iter()
                .any(|r| r.outcome == WebLookupOutcome::CacheHit),
            "a refused cache hit was recorded as one"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // -----------------------------------------------------------------------
    // Reduction applies to pages, not to search bodies (REQ-563 verify)
    // -----------------------------------------------------------------------

    /// A search backend answers JSON, and `reduce` is an HTML→text extractor:
    /// run over a result body it eats `<script` as a tag, eats `<T>` out of
    /// `Vec<T>`, and decodes entities inside URL fields. The search half takes
    /// the byte cap and nothing else.
    #[tokio::test]
    async fn a_search_body_is_capped_but_never_html_reduced() {
        let fx = fixture(
            "search-body",
            web_config(WebTier::Search),
            PermissionPolicy::Allow,
            &[],
        );
        const BODY: &str = r#"{"results":[{"title":"Vec<T> and <script> in Rust",
"url":"https://docs.rs/x?a=1&amp;b=2"}]}"#;

        let out = fx.tool.deliver(
            &Call::Search {
                query: "vec".to_owned(),
            },
            BODY.as_bytes(),
            BODY.len() as u64,
            false,
        );
        assert!(!out.is_error);
        assert!(
            out.content.contains(BODY),
            "the search body was rewritten:\n{}",
            out.content
        );

        // Non-vacuity: the same bytes through the fetch path really are mangled,
        // which is why the two are split.
        let mangled = fx.tool.deliver(
            &Call::Fetch {
                url: "https://docs.rs/x".to_owned(),
                host: "docs.rs".to_owned(),
                authorship: Authorship::ModelComposed,
            },
            BODY.as_bytes(),
            BODY.len() as u64,
            false,
        );
        assert!(
            !mangled.content.contains(BODY),
            "the fetch path left the JSON intact, so this test proves nothing"
        );
    }

    /// The cap is a byte cap and it never cuts a character in half.
    #[test]
    fn the_search_cap_stops_on_a_character_boundary() {
        // Three-byte characters against a cap that lands mid-character.
        let text = "日".repeat(10);
        for cap in 0..text.len() + 2 {
            let capped = cap_bytes(&text, cap);
            assert!(
                capped.len() <= cap || cap >= text.len(),
                "cap {cap} produced {} bytes",
                capped.len()
            );
            assert!(
                text.starts_with(&capped),
                "cap {cap} produced foreign bytes"
            );
        }
        assert_eq!(cap_bytes("abc", 10), "abc");
    }
}
