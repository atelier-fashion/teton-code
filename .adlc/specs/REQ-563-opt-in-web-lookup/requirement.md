---
id: REQ-563
title: "Opt-in web lookup through the egress choke point"
status: approved
deployable: true
created: 2026-08-08
updated: 2026-08-08
component: "daemon/egress"
domain: "harness"
stack: ["rust", "daemon", "cli", "keychain"]
concerns: ["privacy", "security", "cost", "developer-experience"]
tags: ["web-search", "web-fetch", "opt-in", "consent", "egress", "prompt-injection", "permission-levels"]
---

## Description

Teton's harness exposes a deliberately closed toolset — `read`, `edit`, `glob`,
`grep`, `shell`, MCP — with no way to consult the web. That is the privacy
posture expressed as capability design, but it caps the product's usefulness:
a question the local model cannot answer from its weights or from files on the
machine simply dead-ends (today it hunts the repository fruitlessly instead —
the same failure shape as BUG-160/BUG-154).

This REQ adds an **opt-in web lookup capability** as a harness tool that is a
client of the egress choke point. It is disabled by default, graded into three
separately-consented tiers (fetch a user-pasted URL → fetch a model-chosen URL
→ free-text search), and inherits the full egress discipline: outgoing queries
and URLs receive the same provenance inspection and redact scan as any payload
bound for a remote provider, and inbound page content enters context as
untrusted data under the existing envelope/sanitizer machinery.

Why this shape: the product promise is *control*, not isolation. The user
should be able to buy exactly as much web capability as they want, see every
utterance before it leaves the machine (at Ask-level permission), and trust
that the same choke point that guards provider calls guards lookups. The
feature also aligns privacy with cost: a knowledge question resolved by a
cheap local lookup-and-summarize is a turn that never escalated to a frontier
model.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| WebConfig | enabled | boolean | required, default `false` |
| WebConfig | tier | enum(off, fetch_user_url, fetch_any_url, search) | required, default `off`; ordered — each tier includes the ones below it |
| WebConfig | search_endpoint | url | optional; required before the `search` tier can be offered |
| WebConfig | search_key_ref | string | optional; name of an OS-keychain entry, never a key value |
| WebConfig | allowed_domains | list of domain patterns | optional; when set, constrains model-chosen destinations only; absent = tier grants alone govern |
| WebCache | entry | url hash, reduced content, fetched_at, ttl | local-only data; content-addressed; never syncs or egresses |
| SessionState | web_grant | enum(none, once, session) × tier | session-scoped; resets every session; never written back to config |
| PermissionConfig | web (new tool row) | Allow/Ask/Deny | joins the existing per-tool table; named levels (REQ-560) map it — `guarded` → Ask, `full` → Allow |
| CostRecord | web lookup entry | kind, destination host, bytes in, duration, cost | recorded per lookup, including zero-cost lookups |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| web_lookup_requested | model emits a web tool call | kind (fetch/search), verbatim query or URL, session id |
| web_consent_prompted | lookup requested while tier not granted | tier requested, verbatim query/URL, destination host |
| web_consent_granted | user approves | scope (once/session/persistent), tier |
| web_consent_denied | user declines | tier requested |
| web_lookup_blocked | egress provenance check or redact scan blocks the outgoing text | reason (privacy_block/redact_finding), no payload content |
| web_lookup_completed | egress returns | destination host, bytes in, duration |
| web_cache_hit | granted lookup served from local cache | url hash, age; no egress occurred |
| web_lookup_refused_domain | model-chosen destination outside the configured allowlist | destination host, matched-nothing note |
| web_taint_restricted | first model-composed lookup attempt after the session touched boundary content | cause summary (boundary read), tiers affected |
| web_taint_overridden | user lifts the taint restriction via the session command | tiers restored, session id |

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| fetch a URL the user pasted this session | tier ≥ fetch_user_url AND permission table row permits |
| fetch a model-chosen URL | tier ≥ fetch_any_url AND permission table row permits |
| free-text search | tier = search AND search_endpoint configured AND permission table row permits |
| grant/deny/opt-in at any scope | user only — in-session consent prompt or config edit; never the model, never observed content |

## Business Rules

- [ ] BR-1: Web lookup is off by default. On a fresh install, no code path in the daemon performs a web lookup; enabling requires an explicit user act (config edit or in-session consent). Egress-capture tests prove zero lookup traffic in the default state.
- [ ] BR-2: The web tool owns no HTTP client. Every fetch and search flows through the egress module, and the outgoing query/URL text is subject to the same provenance inspection and redact scan as a remote-provider payload: text derived from privacy-boundary content fails closed with a `privacy_block`, and the redact scan runs on the outgoing text whenever the scan is enabled — parity with provider payloads for fetch tiers, unconditionally for search (BR-14). (informed by LESSON-432, LESSON-490, LESSON-492)
- [ ] BR-3: Capability is graded and each tier is separately consented: `fetch_user_url` < `fetch_any_url` < `search`. A grant at a lower tier never implies a higher tier, and a "user-pasted URL" means a URL that appeared verbatim in a user message of the current session — not one the model composed.
- [ ] BR-4: Consent is concrete, not abstract. At Ask, every lookup with no standing grant shows the verbatim query or URL and the destination host, and offers: allow once / allow for this session / enable permanently / no. Persistent enablement is the only path that writes config; once/session grants reset with the session.
- [ ] BR-5: Fetched content enters context as untrusted data: framed by the existing untrusted-content envelope, neutralized by the same authoring-layer sanitizers as tool results, and any new envelope spelling this feature introduces is added to BOTH the input neutralizer alphabet and the output fabrication-marker sets, with bidirectional coverage tests. (informed by LESSON-474, LESSON-477, BUG-148, BUG-149, BUG-151)
- [ ] BR-6: When web lookup is off or the needed tier is not granted, the system prompt names that state and gives the model a legal no-tool ending: answer from knowledge if it can, otherwise say the question needs the web and name the opt-in. It must not hunt project files for knowledge that cannot be in them. (informed by LESSON-493, BUG-160, BUG-154, LESSON-482)
- [ ] BR-7: Every lookup — including zero-cost ones — lands in the cost ledger with destination host and bytes, and is visible in `/cost` and `/verbose`. The status line reflects the session's web capability state alongside permission and effort levels.
- [ ] BR-8: The search backend is user-configured. No default endpoint ships, no bundled key exists, and the user's key lives in the OS keychain referenced by name. With no endpoint configured, the `search` tier is simply not offered — its absence is not an error.
- [ ] BR-9: Network-unreachable is a distinct, transient-shaped notice ("web lookup unavailable: offline"), not a generic turn error, and a lookup failure never fails the turn — the model continues with the lookup's absence stated. (informed by BUG-152)
- [ ] BR-10: Raw fetched page bytes never egress to a remote provider. HTML-to-text extraction is deterministic daemon code; any model-based condensation of fetched content is pinned to the local tier by property, not by provider name (the REQ-558 privacy-pin pattern). When the local tier is absent, the deterministic extraction alone (truncated as needed) enters context — the raw page is never shipped to a remote model for reduction.
- [ ] BR-11: The domain allowlist is optional and constrains only model-chosen destinations. When `allowed_domains` is set, a `fetch_any_url` destination or followed search-result URL outside it is refused with the allowlist named; user-pasted URLs are exempt (the user's explicit act is its own authorization). When unset, tier grants alone govern — the absence of an allowlist is a valid, unrestricted configuration, not a warning state.
- [ ] BR-12: Fetched documents are cached locally, content-addressed, on-machine only. A granted lookup whose target is cached and fresh is served from cache with zero egress and no Ask prompt (nothing leaves the machine), still requires the tier grant, and is recorded in the ledger as a cache hit. The cache never syncs anywhere and an explicit user refresh bypasses it. Cache hits are exempt from the BR-13 taint restriction — the restriction guards egress, and a cache hit performs none.
- [ ] BR-13: Session taint is handled by the authoring-layer split (informed by LESSON-432, LESSON-477): once a session has touched privacy-boundary content, **model-composed** lookups (`fetch_any_url` destinations, search queries) fail closed for the rest of the session, while **user-pasted-URL** fetches survive (the user authored those bytes; the redact scan still runs on them). The restriction is never silent: when it first trips, the user sees a notice naming the cause (boundary content was read) and the effect (model-composed web lookup disabled), and the status line reflects the restricted state for the rest of the session. An explicit, user-only session command lifts the restriction and restores model-composed lookups at the tiers already granted — it grants no new tiers, is recorded as an event and in the ledger, cannot be invoked by the model or by observed content, and resets with the session (never written to config).
- [ ] BR-14: The `search` tier is hard-coupled to the redact scan: enabling search enables the scan for lookup egress as one decision — there is no configuration that yields search without the scan. Every search query is scanned before egress; a transient scan failure fails closed for that query (a guard that cannot run is a block, not a skip — informed by LESSON-492). On a machine where the local tier is absent (so the scan cannot run), the search tier is not offered — like the no-endpoint case, its absence is a stated notice, not an error. Fetch tiers are unaffected by this coupling.

## Acceptance Criteria

- [ ] AC-1: Fresh-install default: with no opt-in, a question that needs the web gets an answer naming the opt-in (no repository hunt), and an egress-capture test records zero lookup traffic for the session.
- [ ] AC-2: Consent flow: at Ask, the prompt shows the verbatim query/URL and destination host; deny → no packet leaves (egress-capture verified); allow-once permits exactly one lookup; allow-session persists to session end and not beyond; enable-permanently writes config and survives a daemon restart.
- [ ] AC-3: Privacy: a lookup whose query text derives from privacy-boundary content is blocked at egress with a `privacy_block` event and no packet (egress-capture verified); a query containing a planted credential shape is caught by the redact scan, with fixtures built through the production encoder. (informed by LESSON-490)
- [ ] AC-4: Tier gradation: with only `fetch_user_url` granted, a model-composed URL is refused and the refusal names the missing tier; with `fetch_any_url` granted, search is refused likewise.
- [ ] AC-5: Injection: a fetched page containing frame markers, role labels, and fabricated tool-result envelopes (built-in and MCP spellings) is neutralized before entering context; marker-coverage tests assert input/output alphabets bidirectionally and fail when a marker is removed. (informed by LESSON-479, BUG-151)
- [ ] AC-6: Legibility: each lookup produces a cost-ledger entry with host and bytes; `/cost` and `/verbose` show it; the status line shows the web capability state.
- [ ] AC-7: Search configuration: the key is stored in and read from the OS keychain by reference; config holds endpoint + key name only; with no endpoint configured the consent prompt never offers the search tier.
- [ ] AC-8: Offline: with the network unreachable, a granted lookup yields the transient-shaped notice, the turn completes, and no error status is reported for the session.
- [ ] AC-9: Allowlist: with `allowed_domains` configured, a model-chosen fetch outside it is refused naming the allowlist while a user-pasted URL outside it proceeds (subject to its tier grant); with no allowlist configured, the same model-chosen fetch proceeds on the tier grant alone.
- [ ] AC-10: Cache: a second granted lookup of the same URL within TTL produces a `web_cache_hit` ledger entry and zero network traffic (egress-capture verified); an explicit refresh re-fetches.
- [ ] AC-11: Local reduction: after a fetch, egress-capture shows no raw page bytes in any remote-provider payload for that turn — only the locally-produced reduction enters context; with the local tier absent, the deterministic extraction path is used and the same capture assertion holds.
- [ ] AC-12: Taint and override: after a boundary read, the next model-composed lookup is blocked with a user-visible notice naming cause and effect, and the status line shows the restricted state; a user-pasted-URL fetch still proceeds in the same session; the session override command restores model-composed lookups at previously granted tiers only, emits `web_taint_overridden`, and a fresh session starts restricted-on-taint again (the override never persists). The override is rejected when issued by the model (tool call) rather than the user (client command).
- [ ] AC-13: Search–redact coupling: enabling the search tier activates the redact scan for lookup egress with no decoupling configuration accepted; a search query with a planted secret shape is blocked (fixtures built through the production encoder); a transient scan failure blocks that query while the turn completes; with the local tier absent, the consent prompt never offers the search tier and a notice names the reason.

## External Dependencies

- A user-supplied search backend for the `search` tier (e.g., Brave Search API, Kagi, or a self-hosted SearxNG instance). None is bundled or defaulted.
- No new bundled dependencies for fetch: the egress module's existing HTTP transport carries lookups.

## Assumptions

- The egress module can host a non-provider egress consumer (the web tool) without weakening the D-2 single-owner invariant — the tool is handed transport, it never constructs one.
- The REQ-560 permission table accepts a new tool row without protocol changes, and the named levels can map it without a new mechanism.
- The redact scan (REQ-562) can run on short query strings with acceptable latency on the local tier.
- Product direction (stated during spec review): a local model is expected to be present on supported machines. The BR-14 hard-couple consequence — machines without a local tier (below hardware floor, declined download, memory-pressure absence) do not get the search tier — is accepted by design, not an oversight. If the "always present" expectation is ever promoted to a product guarantee, the charter's graceful-absence rules (REQ-544 BR-8/BR-9) need reconciling with it.
- HTML-to-text reduction of fetched pages can be done well enough locally that raw page bytes need not enter context wholesale.

## Open Questions

- None remaining — all resolved during spec review: local-tier page reduction (BR-10), optional allowlist (BR-11), local cache (BR-12), taint hybrid with visible notice and user-only session override (BR-13), hard search–redact coupling (BR-14).

## Out of Scope

- Offline docset bundles (man pages, rustup doc, vendored crate docs) — a companion REQ with a one-consent-download shape like the model flow (REQ-547); deliberately separate so the private path and the egress path are independently adoptable.
- Multi-page crawling, JS rendering, or anything browser-shaped; fetch is single-URL, static content.
- Provider-native/server-side web search tools (e.g., a remote provider's own search tool) — different egress semantics, separate REQ if ever.
- Bundling or defaulting a search backend or API key.
- VS Code extension surface (phase 2 client work).
- `redact-then-remote` privacy mode (charter out-of-scope; unchanged here).

## Retrieved Context

- LESSON-432 (lesson, score 9): Provenance must derive from what a tool touches, not from an argument name
- LESSON-490 (lesson, score 7): A guard that runs on an encoded form is tested against the encoder's output
- LESSON-492 (lesson, score 7): A composite guard's failure path must not discard evidence a completed pass established
- LESSON-493 (lesson, score 7): A prompt ending is only reachable if its knowledge source exists — bundle what only the product knows
- BUG-160 (bug, score 7): Asked how to hook up external models, the agent searches the user's repo — Teton's own setup instructions are not bundled
- LESSON-482 (lesson, score 7): A prompt that enumerates a turn's legal endings must name every one
- BUG-154 (bug, score 7): The system prompt describes no ending for a question that needs no files, so the model searches the repo instead of answering
- BUG-152 (bug, score 7): A prompt typed while the local tier is still loading is reported as an error, not as a wait
- LESSON-479 (lesson, score 6): A subset invariant is only tested in the direction your loop iterates
- BUG-151 (bug, score 6): The frame-marker coverage invariant only holds in one direction
- BUG-153 (bug, score 6): /exit is not a command, so asking to leave gets an answer instead of an exit
- LESSON-474 (lesson, score 6): If the tokenizer treats a string as frame, so must your renderer
- LESSON-477 (lesson, score 6): Harness-authored frame inside content is indistinguishable from forged frame — split the sanitizer by authoring layer
- BUG-148 (bug, score 6): Untrusted content can forge turn boundaries in the flat prompt frame
- BUG-149 (bug, score 6): A fabricated MCP tool-result envelope is not cut as fabrication
