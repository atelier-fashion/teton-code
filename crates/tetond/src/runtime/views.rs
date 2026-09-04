//! REQ-599 step 5: the config snapshot and web-setup views.
//!
//! What the daemon *reports* rather than what it decides: `BoundaryPosture`,
//! `WebSetupAnswers` and the setup warnings, the capability and table
//! summaries, and the `snapshot_from_config` / `tier_route_view` /
//! `category_route_view` trio that `config/get` renders from.
//!
//! A view layer, and it reads as one now that it is not interleaved with the
//! machinery it reports on.
//!
//! # Its tests (REQ-602 TASK-304)
//!
//! This module shipped from REQ-599 with **zero** `#[cfg(test)]` content and no
//! note saying why, while the four tests describing `snapshot_from_config` —
//! the projection that lives here — stayed behind in `runtime/mod.rs`. BR-7
//! asks that a test not be left pointing at a module it no longer describes,
//! and one of those tests says the quiet part outright: *"the projection is the
//! step that could drop it."* They are here now.
//!
//! The distinction BR-7 turns on is whether a test's **subject** moved or it
//! merely uses a moved item as a **fixture**. `engine.rs` and `duty.rs` both
//! record which of theirs stayed on those grounds. This module's silence was
//! the defect more than the placement — a reader could not tell a decision from
//! an oversight. 489 contiguous lines; the census measured its items
//! spanning 467 of them, the tightest grouping left after step 4.

use super::*;

/// The two facts REQ-597's session-start events report, derived together.
///
/// A struct rather than two accessors so a caller cannot read one and forget the
/// other, and so the pair is guaranteed to describe the same instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundaryPosture {
    /// Whether the effective set is empty — reachable only via
    /// `[privacy] disable_default_boundaries` with no user rows (BR-3). This,
    /// not the opt-out flag, is what BR-5's warning keys on.
    pub effective_is_empty: bool,
    /// How many builtin rows the effective set carries; `0` when opted out.
    pub builtin_count: usize,
}

// ---------------------------------------------------------------------------
// Config <-> protocol conversions
// ---------------------------------------------------------------------------

/// The four answers a `/web setup` flow collects, borrowed out of whichever of
/// the two identical param structs carried them (REQ-572 ADR-2).
///
/// It exists so preview and commit cannot build their candidates differently:
/// [`DaemonRuntime::web_setup_candidate`] takes this and nothing else, and the
/// only two ways to make one are the two constructors below. `WebSetupPreviewParams`
/// and `WebSetupCommitParams` are deliberately separate wire types — the commit
/// re-asks rather than trusting a blob — and this is where the two stop being
/// two things.
///
/// **A blank answer is an absent one.** `Config::validate` already reads a
/// blank `search_endpoint` and a blank `search_auth` as unset ("not configured
/// is one state, not two"), so trimming to `None` here means the document the
/// flow writes says it the same way — rather than persisting `search_endpoint = ""`,
/// which validates and then reads as nothing.
pub(super) struct WebSetupAnswers<'a> {
    pub(super) tier: WebTier,
    pub(super) search_endpoint: Option<&'a str>,
    pub(super) search_key_ref: Option<&'a str>,
    pub(super) search_auth: Option<&'a str>,
}

impl<'a> WebSetupAnswers<'a> {
    /// The **one** reading of the four wire fields: the tier mapping, the trim,
    /// and blank-as-absent all happen here and nowhere else.
    ///
    /// The two constructors below are the two wire types calling it with their
    /// own fields. They stay separate because the *types* are deliberately
    /// separate (the commit re-asks rather than trusting a preview's blob), but
    /// what they do with those fields was byte-identical prose in two places —
    /// which is one place for a trim rule to be tightened and missed.
    pub(super) fn new(
        tier: WireWebTier,
        search_endpoint: Option<&'a str>,
        search_key_ref: Option<&'a str>,
        search_auth: Option<&'a str>,
    ) -> Self {
        Self {
            tier: from_protocol_web_tier(tier),
            search_endpoint: setup_answer(search_endpoint),
            search_key_ref: setup_answer(search_key_ref),
            search_auth: setup_answer(search_auth),
        }
    }

    pub(super) fn from_preview(params: &'a WebSetupPreviewParams) -> Self {
        Self::new(
            params.tier,
            params.search_endpoint.as_deref(),
            params.search_key_ref.as_deref(),
            params.search_auth.as_deref(),
        )
    }

    pub(super) fn from_commit(params: &'a WebSetupCommitParams) -> Self {
        Self::new(
            params.tier,
            params.search_endpoint.as_deref(),
            params.search_key_ref.as_deref(),
            params.search_auth.as_deref(),
        )
    }
}

/// What a commit is told when the document moved under its preview (BR-7).
///
/// Names the fact and the remedy and **echoes nothing** — not the digests, not
/// the field that changed, not the document: a client renders this into a
/// transcript, and the thing that moved may be another session's answer.
pub(super) const SETUP_DIGEST_STALE: &str =
    "the configuration changed since the preview, so this would write bytes you did not \
     confirm — run `/web setup` again";

/// What a provider-setup commit is told when the document moved under its
/// preview (REQ-579 BR-9).
///
/// [`SETUP_DIGEST_STALE`]'s sibling rather than a share of it, because the
/// remedy is a different command and the remedy is the half of this sentence
/// that does any work. It **echoes nothing** for that constant's reason — not
/// the digests, not the field that changed, not the document, and above all not
/// the candidate: the thing that moved may be another session's answer, and the
/// candidate carries a credential reference.
pub(super) const PROVIDER_SETUP_DIGEST_STALE: &str =
    "the preview you confirmed no longer matches what this daemon would write, so committing \
     would write bytes you did not see — run `/provider setup` again";

/// One setup answer, trimmed, with blank read as absent — see
/// [`WebSetupAnswers`] for why.
pub(super) fn setup_answer(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// The semantic capability state as it travels on the wire.
///
/// The daemon's boundary conversion, and the only one: TASK-128 left it here
/// deliberately, because this is where the wire type is actually named. The
/// gap's sentence comes from [`SearchGap::as_str`] rather than being written
/// again, so the status line, the prompt clause and the setup flow cannot each
/// invent a phrasing of one fact (BR-3, LESSON-456).
pub(super) fn to_protocol_capability_state(state: WebCapabilityState) -> WireWebCapabilityState {
    match state {
        WebCapabilityState::Ready(tier) => WireWebCapabilityState::Ready {
            tier: to_protocol_web_tier(tier),
        },
        WebCapabilityState::OffAvailable => WireWebCapabilityState::OffAvailable,
        WebCapabilityState::SearchUnavailable { reason } => {
            WireWebCapabilityState::SearchUnavailable {
                reason: reason.as_str().to_owned(),
            }
        }
    }
}

/// The `[web]` table as the setup flow shows it — a tier, a host, and two
/// references (REQ-572 AC-7).
///
/// Every field is non-secret by construction: the endpoint appears as its
/// **host** only (REQ-563 BR-7, the rule the whole web event family follows),
/// and the key appears as the reference config holds rather than the value the
/// keychain holds (BR-6).
pub(super) fn web_table_summary(web: &WebConfig) -> WebTableSummary {
    WebTableSummary {
        tier: to_protocol_web_tier(web.tier),
        search_host: web
            .search_endpoint
            .as_deref()
            .and_then(crate::web::canonical_host_of),
        search_key_ref: web.search_key_ref.clone(),
        search_auth: web.search_auth.clone(),
    }
}

/// Query-parameter **names** that mean a credential is riding in the URL.
///
/// Lowercased and compared whole, so `key` matches and `keyword` does not. The
/// list is the four spellings the search backends this flow suggests actually
/// use; it is a heuristic and says so in its sentence, because the alternative
/// — saying nothing — is how a key ends up in a config file, a shell history and
/// every `web_lookup` destination string that endpoint ever produces.
///
/// The catalog's BR-6 sweep (`web_setup_catalog`'s tests) shares this rule via
/// [`endpoint_query_names_a_credential`] — the whole function, not just this
/// list, so the extraction cannot drift either (REQ-573 verify: a `://`-gated
/// re-derivation of the split let scheme-less shapes sweep clean).
pub(super) const CREDENTIAL_QUERY_KEYS: [&str; 4] = ["api_key", "apikey", "key", "token"];

/// Whether `endpoint`'s query string carries a parameter whose **name** says it
/// holds a credential (REQ-572 verify).
///
/// Name-based, and the value is never read, let alone echoed: what the warning
/// needs to say is "there is a key in this URL", and reading the value to say it
/// would put a secret in a `Vec<String>` that travels to a client and into a
/// transcript (BR-6's rule, which the whole web event family follows).
///
/// A hand-split rather than a URL parse, for the reason the whole function is a
/// warning and not a gate: an endpoint that does not parse has its own arm
/// below, and this one has to work on the string the user typed.
///
/// `pub(crate)` for one reader beyond this module: the catalog's BR-6 sweep,
/// which refuses to ship a suggestion whose own query names a credential.
pub(crate) fn endpoint_query_names_a_credential(endpoint: &str) -> bool {
    endpoint
        .split_once('?')
        .map(|(_, query)| query)
        .into_iter()
        .flat_map(|query| query.split(['&', ';']))
        .filter_map(|pair| pair.split('=').next())
        .any(|name| {
            let name = name.trim().to_ascii_lowercase();
            CREDENTIAL_QUERY_KEYS.contains(&name.as_str())
        })
}

/// Non-fatal notes about a candidate the validator already accepted.
///
/// Warnings, never errors — a candidate the validator **refuses** is a
/// `WEB_SETUP_INVALID` response, not a preview with a note attached. What is
/// left for this function is the set of things that are legitimate
/// configurations and still probably not what the user meant, each stated as
/// the consequence rather than as a scolding.
///
/// Every note is **conditional**. There used to be an unconditional first one,
/// disclosing that a save rewrote the whole file and took the user's comments
/// and unrecognized keys with it. REQ-574 removed the behavior, so BR-7 removes
/// the disclosure: a warning that no longer describes what happens is worse than
/// no warning, because it teaches a user to fear a save that is now surgical.
/// A candidate with nothing to say about it therefore draws an empty list.
///
/// `current` is the **delta's base**, not the live config's table
/// ([`DaemonRuntime::web_setup_preview`] passes
/// [`RenderedCandidate::base_web`]). Only the removal note reads it, and what a
/// removal note is about is the document the answer is being applied to: with
/// the live table instead, a file that had drifted drew a sentence contradicting
/// the bytes printed directly beside it.
pub(super) fn web_setup_warnings(current: &WebConfig, candidate: &WebConfig) -> Vec<String> {
    let mut warnings: Vec<String> = Vec::new();
    if candidate.tier < WebTier::Search && candidate.search_endpoint.is_some() {
        warnings.push(format!(
            "`search_endpoint` is written, but `[web] tier` is \"{}\", which does not reach \
             search — the backend will not be queried until the tier is raised.",
            tier_name(candidate.tier)
        ));
    }
    if candidate.tier == WebTier::Search && candidate.search_key_ref.is_none() {
        warnings.push(
            "no `search_key_ref`, so searches go out with no credential — right for a \
             self-hosted backend, and refused by one that requires a key."
                .to_owned(),
        );
    }
    // A credential with nothing to bind to. The second arm is the exact
    // condition `DaemonRuntime::search_auth` fails closed on, asked here so the
    // user learns it at the confirm step rather than from a 401 — and it is
    // genuinely reachable past the validator, whose `is_absolute_http_url` is a
    // looser reading of a URL than the `reqwest::Url` parse the transport binds
    // the key with.
    if candidate.search_key_ref.is_some() {
        match candidate.search_endpoint.as_deref() {
            None => warnings.push(
                "`search_key_ref` is written with no `search_endpoint`, so there is no request \
                 for the key to ride — it stays inert until a backend is named."
                    .to_owned(),
            ),
            Some(endpoint) if origin_of(endpoint).is_none() => warnings.push(
                "`search_endpoint` does not parse to a network origin, so the resolved key has \
                 nothing to be bound to and searches would go out with no credential."
                    .to_owned(),
            ),
            Some(_) => {}
        }
    }
    // A key in the URL itself. Legitimate for a backend that takes no header —
    // which is why it is a note and not a refusal — and worth saying out loud
    // because `search_endpoint` is the one `[web]` field that is *not* treated
    // as a secret anywhere: it goes in the config in clear, and its host travels
    // in every `web_lookup` event. The name is matched and the value is never
    // read, so this note cannot itself become the leak it is warning about.
    if candidate
        .search_endpoint
        .as_deref()
        .is_some_and(endpoint_query_names_a_credential)
    {
        warnings.push(
            "the endpoint's query string looks like it carries a credential; keys belong in \
             the keychain (`search_key_ref`), not in a config file."
                .to_owned(),
        );
    }
    // The candidate is a re-derivation, not a patch (BR-8), so an answer that
    // omits a key the current table has is an answer that removes it. Said out
    // loud, because the preview is where a user can still say no.
    let dropped: Vec<&str> = [
        (
            "search_endpoint",
            &current.search_endpoint,
            &candidate.search_endpoint,
        ),
        (
            "search_key_ref",
            &current.search_key_ref,
            &candidate.search_key_ref,
        ),
        ("search_auth", &current.search_auth, &candidate.search_auth),
    ]
    .into_iter()
    .filter(|(_, before, after)| before.is_some() && after.is_none())
    .map(|(key, _, _)| key)
    .collect();
    if !dropped.is_empty() {
        let mut removal = format!(
            "this replaces the current `[web]` table: {} will be removed.",
            dropped.join(", ")
        );
        // Dropping the *reference* does not drop the secret. The key's whole
        // lifecycle lives in the client process (ADR-3), so the daemon has no
        // way to delete it and would not be the right holder if it had — but it
        // is the party that knows the reference is about to stop existing, and a
        // user who is never told is left with a live credential nothing points
        // at. The entry named is the one `/web setup` writes (service `teton`,
        // account `web-search`), not one parsed out of the reference: building a
        // shell command out of a config string is how a note becomes an
        // instruction to run something the user did not write.
        if dropped.contains(&"search_key_ref") {
            removal.push_str(
                " The stored key remains in the keychain; remove it with: \
                 security delete-generic-password -s teton -a web-search",
            );
        }
        warnings.push(removal);
    }
    warnings
}

/// Whether any remote provider is configured at all.
///
/// One spelling, read by [`DaemonRuntime::unserved_turn_error`]'s remote half
/// and by the dead-end announcement that keys on the same fact — two readings
/// of one question is how the message and the event come to disagree about
/// which state the machine is in (LESSON-456).
pub(super) fn has_remote_provider(config: &Config) -> bool {
    config.providers.iter().any(|p| p.kind.is_remote())
}

/// Project a [`Config`] into the protocol [`ConfigSnapshot`] for `config/get`.
///
/// The phase table that used to fill `routing` is gone (AC-9). What replaced it
/// is not a reverse projection of the category table — that map is one-way
/// (`design` came from either `spec` or `architect`, and nothing records which)
/// — but the resolver's own answer for each of the eleven categories, taken from
/// `router` so that `teton policy show` and `route_decided` are two renderings of
/// one value rather than two computations of one question (ADR-D, BR-6, AC-11).
///
/// `local_model_present` is the one runtime fact the projection cannot read off
/// the config: the web capability state depends on whether a local model is
/// live (REQ-563 BR-14), and that lives in the daemon's engine slot. Passed in
/// rather than reached for, so the whole projection stays a function of its
/// arguments and every cell of it is testable without a daemon.
///
/// `transcript_dir` is the second such fact and arrives the same way (REQ-611
/// AC-20). It is composed by [`super::turn::effective_transcript_dir`], the one
/// function that pairs `TranscriptConfig::effective_dir` with the machine's
/// data directory — the same composition the sink is constructed from, so the
/// directory `doctor` prints and the directory the daemon writes to cannot come
/// from two readings (LESSON-456). Reaching for the environment here instead
/// would buy one fewer argument and cost this projection the property the
/// paragraph above is about.
pub(super) fn snapshot_from_config(
    config: &Config,
    router: &Router,
    local_model_present: bool,
    transcript_dir: &Path,
) -> ConfigSnapshot {
    // REQ-559 BR-9 / AC-8: every row comes from `Router::effort_for`, the SAME
    // function the router calls per model call. The surfaces therefore cannot
    // describe a provider differently from the request that goes to it — which
    // a second, surface-local computation could, and would do silently.
    let effort = Some(teton_protocol::methods::EffortView {
        level: router.effort(),
        providers: config
            .providers
            .iter()
            .filter_map(|p| {
                router.effort_for(Some(&p.id)).map(|resolved| {
                    teton_protocol::methods::ProviderEffortView {
                        provider_id: ProviderId::from(p.id.as_str()),
                        resolved,
                    }
                })
            })
            .collect(),
    });
    ConfigSnapshot {
        effort,
        providers: config
            .providers
            .iter()
            .map(|p| ProviderConfig {
                id: ProviderId::from(p.id.as_str()),
                kind: to_proto_kind(p.kind),
                endpoint: p.endpoint.clone(),
                model: p.model.clone(),
                auth_ref: p.auth_ref.clone(),
                // REQ-586 ADR-7: always populated — `Some(0)` is "unknown" /
                // "no cap", stated rather than hidden (BR-3), and `None` is
                // reserved for a snapshot from a daemon predating the fields,
                // so a live daemon never emits it.
                max_context: Some(p.capabilities.max_context),
                context_budget_cap: Some(p.capabilities.context_budget_cap),
                // BUG-205: a snapshot states the posture rather than leaving it
                // invisible, so `provider list` and `doctor` can show that a
                // provider is deliberately talking in the clear. Always
                // populated for the `max_context` reason directly above — `None`
                // is reserved for a snapshot from a daemon predating the field.
                allow_cleartext: Some(p.allow_cleartext),
                // TASK-194 2b: whether this provider's declaration actually
                // survives the derivation, answered by the **router's** budget
                // for it — the same `budget_for` every route attempt runs
                // through, so `/doctor` reports the floor a turn would really
                // get rather than a client's guess at one (BR-8, AC-12).
                // `Some` only when the floor bit, so a snapshot of ordinary
                // providers is byte-identical to today's.
                floored_budget: {
                    let budget = router.budget_for(Some(p.id.as_str()));
                    budget
                        .floored
                        .then_some(teton_protocol::methods::FlooredBudget {
                            budget_tokens: budget.budget_tokens as u64,
                            budget_bytes: budget.budget_bytes as u64,
                        })
                },
            })
            .collect(),
        tiers: Tier::ALL
            .into_iter()
            .map(|tier| tier_route_view(&router.tier_report(tier)))
            .collect(),
        routing: router
            .table_report()
            .iter()
            .map(category_route_view)
            .collect(),
        // AC-12: the BR-9 default is configuration, so it is readable as
        // configuration — not only visible in the CLI's rendering of it.
        judgment_default: Some(to_protocol_category(Category::from(
            router.judgment_default(),
        ))),
        // REQ-597 BR-6: the **effective** set, not the user's table — a report
        // that named only the rows the user wrote would be answering a
        // different question from the one the enforcement path asks, which is
        // exactly the drift `boundary list` exists to prevent. Composed order
        // is preserved (user rows first, builtins appended), and each row
        // carries its origin so a reader can tell which of their protections
        // they authored and which they were shipped.
        privacy: config
            .effective_boundaries()
            .iter()
            .map(|b| PrivacyBoundaryConfig {
                path_glob: b.path_glob.clone(),
                mode: to_proto_mode(b.mode),
                origin: to_proto_origin(b.origin),
            })
            .collect(),
        // REQ-562: the `[privacy] redact` opt-in, projected so `policy show`
        // can *report* it. Read from the config rather than from the presence
        // of a gate, because this is the same question `redaction_gate` asks —
        // one switch, one reader, no second answer to drift from the first.
        redact_enabled: config.privacy.redact,
        // REQ-572 BR-3/BR-10: the derived web capability state, from the same
        // classifier that governs whether the web tool is registered at all —
        // never a second reading of `config.web.tier` here, which is the one
        // thing BR-3 forbids (LESSON-456). `Some` on every daemon that can
        // answer, which is every daemon since TASK-129; the field stays
        // optional because a *client* may be talking to an older one.
        web_capability: Some(to_protocol_capability_state(web_capability_state(
            &config.web,
            local_model_present,
        ))),
        // REQ-611 AC-20: the posture `doctor` renders in one line. `enabled` is
        // the **durable default** — the config's own key, which is what a
        // machine-wide report is about — and never a session's effective state;
        // BR-2's two lifetimes are two different questions, and `/transcript`
        // answers the other one on the connection that asked.
        //
        // The directory is stated even when nothing is recording, because it is
        // where last week's files still are: a user asking "where are my
        // transcripts" after `/transcript off` is asking about the directory,
        // not about the switch (the same reason ADR-7 composes the tool denial
        // on every turn rather than only while recording).
        transcript: Some(teton_protocol::methods::TranscriptPosture {
            enabled: config.transcript.enabled,
            dir: transcript_dir.display().to_string(),
            retain_days: config.transcript.retain_days,
        }),
        // REQ-612 BR-2/BR-7: the durable `[context] repo_file` default, read
        // from the config's own key for `redact_enabled`'s reason — this is the
        // same question `store_session_repo_context` asks when a session starts,
        // and a second reading here could answer it differently.
        //
        // The cap is the daemon's pinned constant rather than a figure a client
        // could hold, so `doctor`'s worst-case sentence and the truncation
        // marker are two readings of one number (ADR-5's one-derivation rule).
        // It is `REPO_CONTEXT_MAX_BYTES` and not a route's effective cap on
        // purpose: `config/get` reports configuration, and no route is in scope
        // here — the per-route quarter is `SessionContextResult::cap`'s to say,
        // on the connection that asked about a session.
        repo_context: Some(teton_protocol::methods::RepoContextPosture {
            enabled: config.context.repo_file,
            max_bytes: crate::repo_context::REPO_CONTEXT_MAX_BYTES as u64,
        }),
    }
}

/// One tier row of the snapshot.
pub(super) fn tier_route_view(report: &TierReport) -> TierRouteView {
    TierRouteView {
        tier: to_protocol_tier(report.tier),
        provider_id: report.provider_id.as_deref().map(ProviderId::from),
        fallback_id: report.fallback_id.as_deref().map(ProviderId::from),
        source: match report.origin {
            TierOrigin::Configured => TierBindingSource::Configured,
            TierOrigin::DefaultProvider => TierBindingSource::DefaultProvider,
            TierOrigin::LocalTier => TierBindingSource::LocalTier,
            TierOrigin::Unbound => TierBindingSource::Unbound,
        },
    }
}

/// One category row of the snapshot, read **off** a [`CategoryResolution`].
///
/// Every routing field is copied, none is derived: the provider, the tier, which
/// row the binding came from, and the sentence all belong to the resolver. Two
/// fields are about the category rather than about its routing:
/// [`CategoryRouteView::reached`], a fact about the daemon's call sites, from
/// [`crate::call_sites::has_call_site`] (ADR-A); and
/// [`CategoryRouteView::content_class`], what the category sends to a model,
/// from [`ContentClass::for_category`] (REQ-561 BR-11).
pub(super) fn category_route_view(resolution: &CategoryResolution) -> CategoryRouteView {
    CategoryRouteView {
        category: to_protocol_category(resolution.category),
        tier: to_protocol_tier(resolution.tier),
        provider_id: resolution.provider_id.as_deref().map(ProviderId::from),
        fallback_id: resolution.fallback_id.as_deref().map(ProviderId::from),
        source: match resolution.source {
            CoreBindingSource::Override => BindingSource::Override,
            CoreBindingSource::TierInheritance => BindingSource::TierInheritance,
            CoreBindingSource::PinnedLocal => BindingSource::PinnedLocal,
            CoreBindingSource::Unbound => BindingSource::Unbound,
        },
        reached: has_call_site(resolution.category),
        content_class: ContentClass::for_category(to_protocol_category(resolution.category)),
        reason: resolution.reason.clone(),
    }
}

// ---------------------------------------------------------------------------
// REQ-602 TASK-304 — these moved with their subject.
//
// BR-7 of REQ-599 asks that a test not be left "pointing at a module it no
// longer describes". These four are *about* `snapshot_from_config` — the doc on
// the redaction one says so in as many words: "the projection is the step that
// could drop it." The projection lives here, so they do too.
//
// `views.rs` shipped from REQ-599 with zero `#[cfg(test)]` content and no note
// saying why, which is what made the placement unreviewable. `engine.rs` and
// `duty.rs` both record which of their tests deliberately stayed behind; this
// module now has tests of its own instead.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::testsupport::{a_transcript_dir, config_with_remote, router_for_config};
    use teton_protocol::Category as ProtoCategory;

    #[test]
    fn config_snapshot_round_trips_kinds_and_modes() {
        let mut config = Config::default();
        apply_update(
            &mut config,
            ConfigUpdate::RegisterProvider(ProviderConfig {
                id: ProviderId::from("deepseek"),
                kind: ProtoProviderKind::OpenaiCompatible,
                endpoint: Some("https://api.deepseek.com/v1/chat/completions".to_owned()),
                model: Some("deepseek-chat".to_owned()),
                auth_ref: Some("keychain:deepseek".to_owned()),
                max_context: None,
                context_budget_cap: None,
                allow_cleartext: None,
                floored_budget: None,
            }),
        );
        apply_update(
            &mut config,
            ConfigUpdate::SetTierBinding(TierBindingConfig {
                tier: ProtoTier::Build,
                provider_id: ProviderId::from("deepseek"),
                fallback_id: None,
            }),
        );
        apply_update(
            &mut config,
            ConfigUpdate::SetPrivacyBoundary(PrivacyBoundaryConfig {
                path_glob: "secrets/**".to_owned(),
                mode: PrivacyMode::LocalOnly,
                origin: Default::default(),
            }),
        );
        config.validate().expect("valid");

        assert_eq!(config.tiers.len(), 1);
        assert_eq!(config.tiers[0].tier, Tier::Build);
        assert_eq!(config.tiers[0].provider_id, "deepseek");
        // AC-9: no phase-keyed routing row is written by any config op.
        assert!(config.legacy_routing.is_empty());

        let snap = snapshot_from_config(
            &config,
            &router_for_config(&config),
            false,
            a_transcript_dir(),
        );
        assert_eq!(snap.providers.len(), 1);
        assert_eq!(snap.providers[0].kind, ProtoProviderKind::OpenaiCompatible);
        assert_eq!(snap.privacy[0].mode, PrivacyMode::LocalOnly);
        // REQ-586 ADR-7: the snapshot ALWAYS populates the window fields — a
        // provider with no capabilities table reads `Some(0)` ("unknown" /
        // "no cap"), never `None`, which is reserved for a daemon predating
        // the fields.
        assert_eq!(snap.providers[0].max_context, Some(0));
        assert_eq!(snap.providers[0].context_budget_cap, Some(0));

        // And a declared window round-trips: registered over the wire,
        // projected back out as the same figures.
        apply_update(
            &mut config,
            ConfigUpdate::RegisterProvider(ProviderConfig {
                id: ProviderId::from("deepseek"),
                kind: ProtoProviderKind::OpenaiCompatible,
                endpoint: Some("https://api.deepseek.com/v1/chat/completions".to_owned()),
                model: Some("deepseek-chat".to_owned()),
                auth_ref: Some("keychain:deepseek".to_owned()),
                max_context: Some(131_072),
                context_budget_cap: Some(65_536),
                allow_cleartext: None,
                floored_budget: None,
            }),
        );
        let snap = snapshot_from_config(
            &config,
            &router_for_config(&config),
            false,
            a_transcript_dir(),
        );
        assert_eq!(snap.providers[0].max_context, Some(131_072));
        assert_eq!(snap.providers[0].context_budget_cap, Some(65_536));
    }

    /// **The snapshot reports whether the redaction scan is enabled** (REQ-562;
    /// user decision, 2026-08-08) — the daemon half of making the `[privacy]`
    /// switch visible.
    ///
    /// Both states from one projection, so each is the other's discrimination
    /// (LESSON-485): a field wired to a constant, or one projected from
    /// something that merely correlates with the switch, fails a leg. The
    /// absent-table leg is the one that matters most, because it is what almost
    /// every machine sends: no `[privacy]` table at all must reach a client as
    /// `false` rather than as a missing answer a renderer has to guess at.
    ///
    /// It reads `config.privacy.redact` — the same field `redaction_gate`
    /// consults before installing the gate — so `policy show` and the gate
    /// cannot disagree about whether anything is scanning. Asserted here rather
    /// than at the wire, because the projection is the step that could drop it.
    #[test]
    fn the_snapshot_reports_whether_the_redaction_scan_is_enabled() {
        // The overwhelmingly common config: no `[privacy]` table written at all.
        let absent = Config::default();
        assert!(
            !absent.privacy.redact,
            "the fixture must model the default, or the `false` leg proves nothing"
        );
        assert!(
            !snapshot_from_config(
                &absent,
                &router_for_config(&absent),
                false,
                a_transcript_dir()
            )
            .redact_enabled,
            "an un-opted-in daemon must report the scan as off"
        );

        // And the opt-in, on the same projection.
        let opted_in = Config {
            privacy: teton_core::config::PrivacyConfig {
                redact: true,
                ..Default::default()
            },
            ..Config::default()
        };
        assert!(
            snapshot_from_config(
                &opted_in,
                &router_for_config(&opted_in),
                false,
                a_transcript_dir()
            )
            .redact_enabled,
            "`[privacy] redact = true` must reach the client that asked for the config"
        );
    }

    /// **REQ-612 BR-2/BR-7 (verify).** `config/get` reports the durable
    /// `[context] repo_file` default, read from the config's own key.
    ///
    /// [`the_snapshot_reports_whether_the_redaction_scan_is_enabled`]'s twin,
    /// for its reason: this is the same question `store_session_repo_context`
    /// asks when a session starts, and a projection that answered it from
    /// anywhere else — a constant, a router field, a session's own switch —
    /// would put `/doctor` and `teton context` at odds with what the daemon
    /// actually does when it loads a file.
    ///
    /// The cap is asserted as the pinned constant on both legs, because it is
    /// configuration this projection reports and **not** a route's effective
    /// quarter: no route is in scope at `config/get`, and the per-route figure
    /// is `SessionContextResult::cap`'s to say.
    ///
    /// Mutation: pinning `enabled` to a literal, or reading it off anything but
    /// `config.context.repo_file`, fails whichever leg disagrees with it — the
    /// default leg for `true`, the opted-in leg for `false`.
    #[test]
    fn the_snapshot_reports_whether_the_repository_notes_are_on_by_default() {
        // The default a machine with no `[context]` table has.
        let absent = Config::default();
        let posture = snapshot_from_config(
            &absent,
            &router_for_config(&absent),
            false,
            a_transcript_dir(),
        )
        .repo_context
        .expect("the snapshot always states the notes posture");
        assert_eq!(
            posture.enabled, absent.context.repo_file,
            "the snapshot must report the config's own key, not a second opinion"
        );
        assert_eq!(
            posture.max_bytes,
            crate::repo_context::REPO_CONTEXT_MAX_BYTES as u64
        );

        // And the other value of the same key, on the same projection — so the
        // assertion above cannot pass by reporting a constant that happens to
        // equal the default.
        let flipped = Config {
            context: teton_core::config::ContextConfig {
                repo_file: !absent.context.repo_file,
            },
            ..Config::default()
        };
        let posture = snapshot_from_config(
            &flipped,
            &router_for_config(&flipped),
            false,
            a_transcript_dir(),
        )
        .repo_context
        .expect("the snapshot always states the notes posture");
        assert_eq!(
            posture.enabled, flipped.context.repo_file,
            "`[context] repo_file` did not reach the client that asked for the config"
        );
        assert_ne!(
            posture.enabled, absent.context.repo_file,
            "the two legs must disagree, or neither of them is reading the key"
        );
        assert_eq!(
            posture.max_bytes,
            crate::repo_context::REPO_CONTEXT_MAX_BYTES as u64,
            "the cap this projection reports is configuration, not a route's quarter"
        );
    }

    /// ADR-A + AC-12, on the projection a client actually reads.
    ///
    /// The snapshot carries one row per category — all twelve — with the ones
    /// that no model call reaches marked, and the BR-9 judgment default beside
    /// them. Both are things `teton policy show` renders and nothing else
    /// computes.
    #[test]
    fn the_snapshot_marks_the_unreached_categories_and_the_judgment_default() {
        let mut config = config_with_remote("cheap");
        config.default_provider = Some("cheap".to_owned());
        config.judgment_default = teton_core::category::JudgmentCategory::Debug;
        let snap = snapshot_from_config(
            &config,
            &router_for_config(&config),
            false,
            a_transcript_dir(),
        );

        // REQ-613 TASK-381: twelve since `draft` joined them.
        assert_eq!(snap.routing.len(), 12, "every category gets a row");
        let unreached: Vec<&str> = snap
            .routing
            .iter()
            .filter(|r| !r.reached)
            .map(|r| r.category.as_str())
            .collect();
        // Empty since REQ-562 TASK-070 wired `redact`, and still empty after
        // REQ-613 added `draft` with its duty in the same change.
        // Stated as the census rather than dropped: the loop below is the
        // invariant (every row agrees with `has_call_site`), and this line is
        // what makes a *change* to the set show up as a diff a reviewer reads.
        assert_eq!(
            unreached,
            Vec::<&str>::new(),
            "the marker in the projection must agree with `call_sites::has_call_site`"
        );
        for row in &snap.routing {
            assert_eq!(
                row.reached,
                has_call_site(
                    Category::ALL
                        .into_iter()
                        .find(|c| c.as_str() == row.category.as_str())
                        .expect("category")
                ),
                "{} disagrees with the registry",
                row.category
            );
            assert!(!row.reason.is_empty(), "{}", row.category);
        }

        // AC-12: the declared default is readable as configuration, not only as
        // a rendered sentence.
        assert_eq!(snap.judgment_default, Some(ProtoCategory::Debug));

        // The two pinned categories report the pin, whatever the table says.
        for pinned in [ProtoCategory::Route, ProtoCategory::Redact] {
            let row = snap
                .routing
                .iter()
                .find(|r| r.category == pinned)
                .expect("pinned row");
            assert_eq!(row.source, BindingSource::PinnedLocal, "{pinned}");
            assert_ne!(
                row.provider_id.as_ref().map(|p| p.0.as_str()),
                Some("cheap"),
                "{pinned} must never resolve to the remote default"
            );
        }
    }

    /// The tier rows report the fill an unbound tier takes, and the two
    /// **local-by-default** tiers take a different one —
    /// `Tier::inherits_default_provider`'s fact, reported rather than restated.
    ///
    /// `build` inherits `default_provider`; `reflex` and `scan` inherit the
    /// local tier. This row is where a user sees that asymmetry rather than
    /// discovering it by watching where their file contents go.
    #[test]
    fn an_unbound_tier_reports_what_it_inherits_and_the_local_tiers_differ() {
        let mut config = config_with_remote("cheap");
        config.default_provider = Some("cheap".to_owned());
        apply_update(
            &mut config,
            ConfigUpdate::SetTierBinding(TierBindingConfig {
                tier: ProtoTier::Think,
                provider_id: ProviderId::from("cheap"),
                fallback_id: None,
            }),
        );
        let snap = snapshot_from_config(
            &config,
            &router_for_config(&config),
            false,
            a_transcript_dir(),
        );
        let row = |tier: ProtoTier| {
            snap.tiers
                .iter()
                .find(|t| t.tier == tier)
                .unwrap_or_else(|| panic!("{tier} row"))
        };
        assert_eq!(snap.tiers.len(), 4);
        assert_eq!(row(ProtoTier::Think).source, TierBindingSource::Configured);
        assert_eq!(
            row(ProtoTier::Build).source,
            TierBindingSource::DefaultProvider,
            "a turn tier inherits the declared default — the non-vacuity leg, \
             without which the two below prove nothing"
        );
        for local_by_default in [ProtoTier::Reflex, ProtoTier::Scan] {
            assert_eq!(
                row(local_by_default).source,
                TierBindingSource::LocalTier,
                "`{local_by_default}` never inherits a remote default: its work \
                 was already local before this REQ, and this row is where the \
                 user sees that rather than discovering it by watching where \
                 their file contents go"
            );
            assert_ne!(
                row(local_by_default).provider_id,
                row(ProtoTier::Build).provider_id
            );
        }
    }
}
