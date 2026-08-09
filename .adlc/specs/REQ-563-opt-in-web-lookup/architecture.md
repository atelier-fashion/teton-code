# REQ-563 — Architecture: Opt-in Web Lookup Through the Egress Choke Point

Grounded in three codebase surveys (feature-tracer, architecture-mapper,
integration-explorer, 2026-08-08). File:line references are to the REQ-563
worktree at branch point `origin/main` (8b78f89).

## Approach

The web tool is a **harness tool that is an egress consumer** — it composes a
`LookupRequest` and hands it to the egress module; it never owns an HTTP
client (the tree-wide `deny_http_client` check already makes this mechanical —
egress/mod.rs:1-12). Outgoing lookup text takes the same two ordered
inspections as a provider payload (provenance-shaped gate first, redaction
second — egress/mod.rs:474-623), inbound content is framed by the existing
untrusted-envelope machinery, and the whole capability is **absent by
construction** when not opted in: the tool is simply not registered, so AC-1's
"zero lookup traffic" is structural, not policed.

## Key Decisions

### D-1: Conditional registration, not conditional refusal (BR-1, BR-6)

`ToolRegistry::with_builtins()` gains a web tool **only when** config enables a
tier and the session's gate conditions hold. When absent, `build_system_prompt`
(turn_loop.rs:795-819) names the opt-in in the no-tool-ending clause (BUG-154
pattern) so the model has a legal ending instead of a futile repo hunt
(LESSON-482/493). Registration order: **after `shell` (last)** — under a
degraded provider's `max_tools` cap the web tool is cut first (BR-6 of the
charter; tools/mod.rs:469-494). The tool description stays terse (<100 chars)
because tool docs burn prompt-budget bytes (integration finding, Issue C).

### D-2: One egress seam, two request kinds (BR-2)

`Egress` gains a `lookup()` entry alongside `send()`: `LookupRequest::Fetch
{ url }` and `LookupRequest::Search { query }`. Both flow through the existing
transport (no redirect-following: egress's `redirect::Policy::none()` protects
credential headers — ADR-004; fetch redirects are handled as a bounded manual
loop in the web module, re-running the domain gate per hop). The search
endpoint + keychain-referenced key ride the request like a provider credential.
Inspection order matches `send()`: authorship/taint gate first (D-4), then the
search redaction gate (D-6), then wire. Cost/event emission hooks sit where
`send()`'s are (egress/mod.rs:605-622).

### D-3: Inbound content is empty-provenance data in the existing envelope (BR-5, BR-10)

Fetched content enters context via `frame_untrusted_builtin` — the SAME
`<tool-result trust="untrusted">` envelope as built-in tools (turn_loop.rs:884)
— so **no new envelope spelling exists** and the ADR-009 two-sided marker sets
are untouched (a new spelling would demand input+output alphabet changes and
coverage tests — BUG-149/151). Provenance is `Sources(∅)` (empty): the daemon
knows exactly what the tool touched (a URL, not repo files), so the
shell-style `Unknown` fail-close is wrong here (LESSON-432 — provenance from
what was touched). Injection containment comes from the envelope + sanitizers,
not provenance. Reduction (BR-10): a deterministic, dependency-free HTML→text
extractor in the web module (tags/scripts stripped, entities decoded, byte-
capped); condensation beyond that rides the EXISTING local-pinned
`summarize_if_large` (context.rs) — already LESSON-447-hardened, already
local-tier — so raw page bytes never reach a remote model by construction.

### D-4: Taint split rides the existing SessionTaint flag (BR-13)

No new taint machinery. The lookup gate **reads** `SessionTaint::is_tainted`
(runtime.rs:503-558): tainted + model-composed lookup (Search, or Fetch of a
URL not found verbatim in this session's user messages) → blocked with a
notice; tainted + user-pasted URL → allowed (user authored the egressing
bytes; still scanned when a gate is installed). The session tracks user-pasted
URLs by extracting them from each user prompt into a per-session set. The
override is a client RPC (`web/override`, surfaced as a CLI command) that sets
a session-scoped flag — the model cannot reach it because tool dispatch and
client RPCs are structurally distinct channels, which gives AC-12's
"override rejected when issued by the model" for free. Cache hits skip the
taint gate (BR-12: the restriction guards egress; a hit performs none).

### D-5: Consent rides the PermissionGate, plus one new option (BR-3, BR-4)

The web tool authorizes through `PermissionGate::authorize` (permissions.rs:
209-251) with the verbatim query/URL + host in the prompt description. The
existing options give allow-once and allow-for-session (session-scoped grants,
never persisted — permissions.rs:19-20). One NEW option id, `enable_permanent`,
is handled daemon-side by writing the granted tier to config (REQ-547
precedent: consent decisions persist via the daemon, model_ui.rs:55-114).
Tier ceilings are enforced BEFORE the permission prompt: config `tier` is the
outer bound; a lookup above the ceiling is refused naming the missing tier
(AC-4) without prompting.

### D-6: Search's redaction gate is the same composite scanner, always installed for search (BR-14)

The lookup path holds its OWN `RedactionGate` instance (the REQ-562 trait,
redact.rs:739-743), installed unconditionally whenever `tier = search` —
independent of `[privacy] redact`, which continues to govern provider-payload
scanning only (BR-2's parity clause for fetch tiers). The composite semantics
are inherited: pattern pass carries blocking power, model pass adds paraphrase
recall (ASSUME-003), verdict `Unavailable` on a query **blocks that query**
(LESSON-492: a guard that cannot run is a block, not a skip). Engine absent →
the search tier is not offered at consent time (BR-14). Scan caps follow
LESSON-491: measured on the rendered scan prompt, not the raw query.

### D-7: Ledger gets a sibling table, not overloaded provider rows (BR-7)

The cost ledger's provider-call schema (session/phase/category/provider/model/
tokens/usd — ledger.rs:49-87) does not fit lookups. A second append-only table
`web_lookups` (same SQLite file, same UPDATE/DELETE-denying trigger pattern)
records kind, host, bytes_in, duration_ms, outcome (completed/blocked/
cache_hit/refused_domain/offline), usd_micros (0 for MVP). `/cost` aggregates
both tables. No query text, no full URL beyond host, no credentials (BR-7 of
the charter; conventions).

### D-8: Event vocabulary — one lookup family, blocked-reasons folded

Protocol gains three variants (events.rs:75-127 pattern — enum + `Event::name`
+ index comment): `web_lookup` (kind, host, outcome, bytes_in — where outcome
∈ {completed, cache_hit, blocked_privacy, blocked_redact, refused_domain,
refused_tier, taint_restricted, offline}), `web_consent_decided` (scope, tier,
granted), `web_taint_overridden` (tiers restored). The spec's ten-event table
is realized with less enum surface: consent PROMPTS reuse the existing
`permission_request` event; the folding is recorded here deliberately (events
are an index, not decoration). `web_lookup_requested` is not a wire event —
the request is only observable at Ask-time (permission_request) or at outcome.

### D-9: Config is tier-only — no separate `enabled` bool

`[web] tier = "off" | "fetch_user_url" | "fetch_any_url" | "search"` (serde
default `off`), plus `search_endpoint`, `search_key_ref` (keychain name, never
a value — config.rs:8-17 validation pattern), `allowed_domains` (optional
list; when set, constrains model-chosen destinations only — BR-11),
`cache_ttl_secs` (default 900). The spec's System Model carried both `enabled`
and `tier`; the /validate Info flagged the double-encoding and this resolves
it: `tier` is the single source of truth, `off` means disabled, nothing can
disagree. Placed in its own `[web]` table (REQ-562's `[privacy]` placement
rationale, config.rs:92).

## Data Model Changes

- `teton-core::config`: `WebConfig` as D-9; validation — `search` tier without
  `search_endpoint` is a config error naming the missing field; allowlist
  patterns charset-checked; `search_key_ref` must be a keychain ref, never a
  raw credential.
- `tetond::web` (new module): `WebCache` (content-addressed by URL hash under
  the daemon data dir; entries = reduced text + fetched_at + ttl; local-only),
  `reduce()` (deterministic extractor), `UserUrls` (per-session verbatim-URL
  set), `DomainAllowlist` matcher.
- `tetond::cost`: `web_lookups` table (D-7).
- `teton-protocol`: three event variants (D-8).
- Session state: web grant scopes (once/session per tier), taint-override
  flag, user-URL set — all session-scoped, never persisted (permissions.rs
  precedent).

### Egress behaviour the seam owns (added at verify — load-bearing, not detail)

Two properties of `egress::lookup` are part of the design rather than
implementation colour, and are recorded here because a later change that drops
either would look local while removing a security floor. **Address-class
policy (the SSRF floor):** a destination in a non-global address class —
loopback (including `localhost` and anything under `.localhost`), link-local,
private (RFC 1918 plus CGNAT `100.64.0.0/10` and benchmarking
`198.18.0.0/15`), unspecified/`0.0.0.0/8`, and their IPv6 equivalents, with
all-digit hosts folded to the IPv4 literal they are and every IPv6 transition
prefix that embeds an IPv4 address (`::ffff:` mapped, `64:ff9b::/96` NAT64,
`2002::/16` 6to4) folded onto the address it carries — is refused. The **only**
exemption is a user-pasted *initial* URL, and it covers only loopback, private
and unique-local: those three have a story a paste can be an instance of
("fetch my dev server on `localhost:3000`", "read the wiki on the box in the
next room"), while link-local and unspecified have none — `169.254.169.254` is
the cloud metadata service and `0.0.0.0` is not a destination — so those two are
refused at hop zero whoever typed them. This matters because `UserPasted` means
only "the URL appeared in a user message", which a pasted stack trace or log
line satisfies. A **redirect hop** carries no exemption at all and is checked
unconditionally, ahead of the caller's allowlist closure, so a permissive
allowlist cannot grant `169.254.169.254`; a hop is also held to the `http`/`https`
scheme list explicitly at this seam rather than relying on the current client
happening to refuse `ftp://`. The configured `search_endpoint` is exempt for the
same reason a pasted URL is — it is a value out of the user's own config — and a
`Fetch` aimed at its origin is refused outright so the endpoint-bound key can
never ride one. **Timeouts:** the transport carries a *connect* bound
(`LOOKUP_CONNECT_TIMEOUT`) because a lookup destination is an arbitrary host
that may accept a connection and then say nothing; the whole attempt — every
gate, every redirect hop and the body read — is bounded by
`LOOKUP_TOTAL_TIMEOUT` at the seam, so the bound holds for every transport the
choke point is built over and not only for the real client. Expiry is attributed
to the **phase** it fired in: on the wire it is the same `offline` outcome a
connect failure produces (BR-9/BUG-152 taxonomy), but while the redaction gate
is still thinking it is `blocked_redact` / `scan_unavailable` — a guard that
cannot finish is a guard that did not run (LESSON-492), and calling a stalled
local scanner "the destination could not be reached" is BUG-152's mislabel
pointing the other way. Neither is ever a turn error.

**Residual: name-based non-global destinations.** The floor above reads a *host
string*; the transport does the DNS, and no API on it exposes the resolved
addresses. So `127.0.0.1.nip.io`, or any attacker-controlled name with an `A`
record inside a refused range, passes the literal check and is dialled — as does
the rebinding variant, where the record is global when this seam looks and
loopback when the socket connects. `localhost` and the `.localhost` TLD are
special-cased only because RFC 6761 makes them loopback *by definition* rather
than by resolution; that is not a general answer. **The closure is a resolving
transport that refuses non-global answers at connect time** — a `reqwest` custom
resolver, or a pre-resolve-then-connect-to-IP pass — which belongs in the
transport, not at this seam, because only the thing that opens the socket knows
the address it opened to. Recorded here as a known gap rather than an assumed
absence; the cross-reference lives on `address_class_of_host` in
`egress/lookup.rs`, where anyone tightening the floor would be reading.

## AC → Decision Map

| AC | Covered by |
|----|-----------|
| AC-1 | D-1 (absent by construction) + egress-capture test |
| AC-2 | D-5 (gate options + enable_permanent) |
| AC-3 | D-2/D-4 (gate order) + D-6 (scan) + egress-capture |
| AC-4 | D-5 (ceiling before prompt) |
| AC-5 | D-3 (existing envelope/sanitizers; tests extend fixtures, no new markers) |
| AC-6 | D-7 (ledger) + D-8 (events) + CLI surfaces |
| AC-7 | D-9 (config validation, keychain ref) |
| AC-8 | D-2 (offline mapped to notice outcome, turn continues — BUG-152 shape) |
| AC-9 | D-9 + D-4 (allowlist constrains model-chosen only) |
| AC-10 | D-3 cache (hit = zero egress, ledger row) |
| AC-11 | D-3 (extractor + local-pinned summarize; egress-capture asserts) |
| AC-12 | D-4 (RPC-only override; structural rejection of model calls) |
| AC-13 | D-6 (always-installed composite gate; Unavailable blocks) |

## Task Graph (TASK-072..078)

```
Tier 1:  TASK-072 (config)          TASK-073 (protocol events + ledger table)
Tier 2:  TASK-074 (web module)      TASK-075 (egress lookup seam)
Tier 3:  TASK-076 (harness tool + prompt + registration)
Tier 4:  TASK-077 (CLI: consent, override, status line, /cost)
Tier 5:  TASK-078 (egress-capture + e2e acceptance suite)
```

TASK-074 depends on 072; TASK-075 on 072+073; TASK-076 on 074+075; TASK-077 on
073+076; TASK-078 on 076+077.

## Deviations From the Spec's System Model (deliberate, recorded)

1. `enabled` bool dropped — `tier=off` is the single disabled state (D-9).
2. Ten wire events folded to three variants + reuse of `permission_request`
   (D-8); every spec event's information survives as an outcome/field.
3. Consent scopes ride the PermissionGate rather than a parallel mechanism
   (D-5) — "separately consented tiers" = ceiling in config + per-tier session
   grants, exactly the spec's Permissions table semantics.

The rest were recorded at verify, once implementation showed what the spec's
wording actually resolves to.

4. **AC-3's boundary case is realized as `taint_restricted`, not
   `blocked_privacy`.** The spec's "a lookup in a session that has touched
   boundary content is blocked" is BR-13's session restriction, and BR-13 is
   asymmetric — a URL the *user* pasted still goes. So the ending that fires is
   `taint_restricted`, which names the restriction and the way out of it
   (`/web allow`), where `blocked_privacy` would name neither. The
   `blocked_privacy` wire variant is **retained** — the outcome vocabulary is a
   fixed protocol enum (D-8) and removing a variant is a wire change — but it
   has **no production producer**; the cross-reference lives on the variant
   itself in `egress/lookup.rs`, which is where anyone adding a producer would
   be reading. A lookup does not publish `privacy_block` either, deliberately:
   refusing to send establishes nothing about the context this session holds,
   so it must not become a session pin.
5. **BR-14's "the search tier is not offered at consent time" is realized as
   always-blocked-with-a-kind-aware-notice.** Not offering a tier at consent
   time would mean a consent surface that reads the engine slot and hides an
   option, and the slot changes mid-session (an install can finish thirty
   seconds in). Instead the search gate is installed whenever `tier = search`
   and a scan that cannot run **blocks the query** (LESSON-492), with a notice
   that names the missing *local model* rather than a generic refusal — so the
   user is told the thing they can act on. The effect BR-14 asks for holds
   (no query leaves unscanned); the mechanism differs. **Pending a product
   decision:** whether `[web] tier = "search"` should additionally be refused at
   *config load* on a machine with no local tier, which would move the failure
   from per-query to startup. Left open because it would make an engine
   download a precondition for a config file to load.
6. **`[web] permission_allow = [tier, …]` added to D-9's config surface.** D-9
   described the `[web]` table as tier-only. `enable_permanent` (D-5) has to
   have somewhere durable to land the *permission* half of the answer:
   persisting only the tier left the next daemon start re-prompting for a
   capability the user had already enabled permanently, which is the one thing
   that option promises not to do — and worse, the tier write is raise-only and
   the ceiling is checked *before* any prompt exists, so it was a guaranteed
   no-op for every prompt a user could actually reach. `permission_allow` is
   therefore a second `[web]` key, defaulting to the empty list, mapped onto the
   gate's policy rows by `PermissionConfig::apply_web_permission`.
   It is a **list of tiers and not a two-valued switch**, because BR-3 grades the
   capability into three separately-consented tiers: a single `permission =
   "allow"` fanned onto all three keys, so one "enable permanently" answered at a
   prompt about a URL *the user pasted* permanently stopped the prompts for URLs
   the model composes and for searches too — the breadth violation BR-3 forbids,
   made durable in a file the user never re-reads. Each listed tier maps to **its
   own** key through `permission_key_for` and no other; `"off"` is refused at
   config load, since it names the absence of a tier and no prompt can produce
   it. The `enable_permanent` option label names `[web] permission_allow +=
   "<tier>"` — the key that is actually written, with the append visible.
7. **Per-tier permission keys replace D-5's single `web` row.** D-5's sketch
   implied one consent subject. Three ship — `web_fetch_user_url`,
   `web_fetch_any_url`, `web_search` — because a grant is remembered under
   exactly the string it was asked about: one key would have made "allow for
   this session" on a pasted link silently grant every model-composed URL and
   every search, which is the mixed-authorship case BR-3 names first.
   `permission_key_for(tier)` is the single tier→key mapping, and it is *total*
   over the tiers a lookup can need: `Call::permission_key` treats a `None` from
   it as `unreachable!` rather than falling back on the narrowest key, because a
   fallback reads as failing-closed and is not — a new tier added to the ladder
   without a key would be silently authorized under `web_fetch_user_url`, a real
   grant under a question about a different capability.
   **Forward note:** REQ-560's named permission levels must map through
   `PermissionConfig::apply_web_permission`, which reads a *set of tiers* and
   sets one key per member — mapping a level to a single `web` row would silently
   re-collapse the three keys, and mapping it to a fan-out over all three would
   re-introduce the breadth violation deviation 6 records.
8. **The degraded-profile cap is a floor the web tool always loses.**
   `DEGRADED_MAX_TOOLS` is 5 and the builtin set is 5, and the web tool
   registers last precisely so a cap cuts it first (D-1) — so on any provider
   that is not a `Native` tool-caller the tool is registered and never exposed.
   That is signalled, not hidden: the model is told by `WEB_CAPPED_CLAUSE` in
   its own prompt and the user by the status row's `web: unavailable (profile)`,
   read from one function (`web_tool_is_exposed`) so the two cannot disagree.
   Changing the cap policy itself — a reserved slot for opt-in capabilities, or
   a larger degraded budget — is **deferred to a follow-up**; it is a change to
   BR-6's degradation contract, not to this requirement.
9. **AC-6's `/cost` section shows count and bytes; the host is ledger-side.**
   `/cost` grows a `web lookups:` section, one row per session, carrying the
   lookup count (every ending — cache hits and refusals included, BR-7) and
   bytes-in. It carries **no host**. Per-lookup hosts live in the `web_lookups`
   table and on the `web_lookup` event, and reach a person through the ledger
   and the `/verbose` notices — a cost *summary* that enumerated destinations
   would be a browsing history in the one surface a user shows someone else.
   The section is silent when empty, so a machine that never opted in sees the
   `/cost` output it always saw (BR-1).
10. **BR-6's ungranted-tier affordance is realized as a runtime refusal that
    names the missing tier, with the prompt clause beside it and not instead of
    it.** BR-6 asks that the model be *told* the state and given a legal
    no-tool ending. The prompt does that (and names `[web] tier`, the key the
    user would change — LESSON-493), and the tool's description is tier-shaped
    so a fetch-only ceiling never advertises a search. But a prompt sentence is
    a claim the model can ignore, so the ceiling is *also* enforced when the
    call arrives — ahead of any consent prompt — with a refusal naming both the
    tier the lookup needed and the tier `[web] tier` is set to, and telling the
    model not to retry (AC-4). The affordance is the pair; the refusal is what
    makes it true.

## Risks / Notes for Implementation

- Redaction-gate latency on search (integration Issue D): queries are short;
  cap per LESSON-491 on the rendered prompt; the existing chunk caps apply.
- Prompt-budget: web tool docs + BR-6 opt-in sentence must clear the budget
  headroom test the same way SELF_CONFIG_GUIDE does (BUG-160/LESSON-493).
- The e2e suite must use the scripted-engine harness (cli_e2e.rs:149-165) and
  `CaptureTransport`/`CountingGate` fixtures (egress tests) rather than new
  test scaffolding.
- Offline detection maps transport connect errors to the `offline` outcome —
  a settled failure of the endpoint (DNS/timeout) must NOT be reported as a
  turn error (BR-9, BUG-152 taxonomy).
