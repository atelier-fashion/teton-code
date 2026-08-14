---
id: REQ-573
title: "Daemon-owned web-setup suggestion catalog in web/setup_plan"
status: draft
deployable: true
created: 2026-08-14
updated: 2026-08-14
component: "protocol"
domain: "clients"
stack: ["rust", "json-rpc", "daemon", "cli"]
concerns: ["extensibility", "developer-experience"]
tags: ["web-setup", "suggestion-catalog", "setup-plan", "contract-tests", "single-source", "web-search"]
---

## Description

REQ-572's guided web-setup flow ships its suggestion catalog inside the CLI:
`ENDPOINT_HELP` (the three backend suggestions printed above the endpoint
prompt), `KNOWN_BACKEND_AUTH` (host → auth-header-template mapping), and
`DEFAULT_SEARCH_AUTH` (the generic Bearer default) all live in
`crates/teton/src/web_setup_ui.rs`. The same backend strings exist in two more
places — the bundled self-configuration guide
(`crates/tetond/src/harness/self_config.md`, line 7) and the README's backend
rows — held together by a three-way drift comment in README.md and by the AC-8
contract suite (`crates/tetond/tests/web_setup_contracts.rs`), which today
`include_str!`-parses the *source text* of the CLI crate and the bundled guide
to enumerate what is being suggested.

REQ-572's reflection (finding M5) named the consequence: the phase-2 VS Code
surface — which REQ-572 explicitly anticipates (its out-of-scope note: "BR-12's
degradation rule is what phase 2 builds on") — would have to reimplement the
question flow *and* carry its own copy of the suggestion list, and that copy
would be invisible to the AC-8 CI gate. Every future client multiplies the
drift surface.

This REQ moves the catalog to the daemon and returns it in the `web/setup_plan`
RPC result as a new, additive field on `WebSetupPlanResult`. The daemon becomes
the single owner of "which backends we suggest and what shape each one has"
(endpoint example, auth-header template, needs-key default, notes); every
client — CLI today, VS Code in phase 2 — renders the one daemon-owned list; the
contract suite enumerates one typed source instead of parsing two files of
source text; and the README's three-way drift comment collapses to a single
in-tree source of truth. This further discharges REQ-572 ADR-1's "clients stay
thin" intent: the catalog is static product knowledge, and product knowledge
belongs to the daemon, not to each client's source code (informed by
LESSON-493).

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| WebSetupPlanResult | suggestion_catalog | WebSetupCatalog (optional) | new field; additive — absent on older daemons, ignored by older clients |
| WebSetupCatalog | backends | array of WebBackendSuggestion | required, non-empty; array order is display order |
| WebSetupCatalog | default_auth_template | string | required; must contain the `{key}` marker; today `Authorization: Bearer {key}` |
| WebBackendSuggestion | id | string | required, unique within catalog, stable (e.g. `searxng`, `brave`, `kagi`) |
| WebBackendSuggestion | label | string | required, human display name |
| WebBackendSuggestion | endpoint | string | required; absolute URL example including any required query (e.g. `/search?format=json`) |
| WebBackendSuggestion | host | string (optional) | hostname used to match a typed endpoint to this entry (e.g. `api.search.brave.com`); absent for self-hosted backends whose host varies |
| WebBackendSuggestion | auth_template | string (optional) | present iff `needs_key`; must contain `{key}`; a header *shape*, never a secret |
| WebBackendSuggestion | needs_key | boolean | required |
| WebBackendSuggestion | notes | string (optional) | short hint (e.g. "self-hosted, no key") |

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| read suggestion_catalog | same as `web/setup_plan` today — no new authority; the catalog is static, non-secret product data |

_Events: none — no new events; the REQ-572 setup events are unchanged._

## Business Rules

- [ ] BR-1: **One catalog, daemon-owned.** Exactly one definition in `tetond` is the source of backend suggestions (endpoint shape, auth template, needs-key, notes). The CLI crate contains no backend endpoint or auth-header literals; `ENDPOINT_HELP`, `KNOWN_BACKEND_AUTH`, and `DEFAULT_SEARCH_AUTH` are removed, and every *client* rendering surface consumes the RPC catalog (the daemon-side bundled guide is governed by BR-5, not this rule)
- [ ] BR-2: **The protocol change is additive.** `suggestion_catalog` follows the existing additive-field convention (`#[serde(default)]` / skip-if-none); no `PROTOCOL_VERSION` bump. An older client against a newer daemon ignores the field; a newer client against an older daemon sees it absent and must not fail on deserialization (informed by BUG-158)
- [ ] BR-3: **Absence degrades, never errors.** When the catalog is absent (older daemon), the guided flow still completes: no named suggestions are shown, the offered auth default is the generic `Authorization: Bearer {key}`, and needs-key defaults to yes. This mirrors REQ-572 BR-12's rule that the guided flow's enhancements are never the only path (informed by BUG-158, REQ-572)
- [ ] BR-4: **Every catalog entry is a contract-tested vector.** The AC-8 suite enumerates the daemon catalog directly — typed iteration over the one definition, not source-text parsing of other files — and for each entry drives the production request builder: GET via the egress lookup path preserving the endpoint's path/query, the documented header produced by the production auth-shape code, and a config shape `Config::validate()` accepts whose rendered TOML contains no raw secret. An entry without a passing contract fails CI (informed by LESSON-512, BUG-165)
- [ ] BR-5: **Derived surfaces cannot drift.** The bundled guide's backend suggestions and the README's backend rows are either generated from the catalog or mechanically checked against it in CI; a mismatch fails the build. The bundled guide keeps its existing prompt-size ceiling test green (informed by LESSON-493, BUG-160)
- [ ] BR-6: **The catalog is static, non-secret, and pure.** It contains no keys, no keychain references, and no user configuration; it is a pure function of the daemon build, computable and unit-testable without environment gates, TTY state, or user state (informed by LESSON-481)
- [ ] BR-7: **Rendering parity for today's users.** The CLI's interactive help lines and the piped-session `instruction_lines` render from the catalog, and the shipped catalog carries the same three backends with today's exact strings: SearxNG keyless (`/search?format=json`), Brave `X-Subscription-Token: {key}`, Kagi `Authorization: Bot {key}` (informed by BUG-165)
- [ ] BR-8: **Offer logic is data-driven.** The auth-header default offered for a typed endpoint derives only from catalog data: match the parsed host against entries' `host`, offer that entry's `auth_template`, else offer `default_auth_template`. Clients implement the matching; the data comes only from the catalog

## Acceptance Criteria

- [ ] AC-1: `WebSetupPlanResult` carries `suggestion_catalog` with the entity shape above; a serde round-trip test covers the populated case and a deserialization test covers a result JSON with the field absent (both directions of BR-2)
- [ ] AC-2: `crates/teton/src/web_setup_ui.rs` no longer defines `ENDPOINT_HELP`, `KNOWN_BACKEND_AUTH`, or `DEFAULT_SEARCH_AUTH`; a CLI unit test feeding a synthetic catalog (obviously-sentinel values per LESSON-497) shows the rendered help lines, the piped `instruction_lines`, and the offered auth default all change with the catalog — proving no residual client-side copy
- [ ] AC-3: `web_setup_contracts.rs` enumerates the daemon catalog as its single source: the `include_str!` of `../../teton/src/web_setup_ui.rs` is deleted, and the existing production-builder assertions (request shape, documented header, loadable config) run per catalog entry
- [ ] AC-4: CI fails if the bundled guide's backend suggestions disagree with the catalog; the existing prompt-size ceiling test still passes
- [ ] AC-5: The README drift comment names the daemon catalog as the single in-tree source (the three-way comment collapses); the README backend rows are covered by the BR-5 check or the comment states exactly where sync is enforced
- [ ] AC-6: Interactive parity: `/web setup` against the new daemon shows the same three suggestions with byte-identical endpoint and template strings as v0.1.14, and the host-match offer behaves as before (Brave host → `X-Subscription-Token: {key}`, unknown host → Bearer default)
- [ ] AC-7: Degradation: against a plan result without the catalog field, the guided flow completes with generic defaults per BR-3 — covered by a test, not by inspection

## External Dependencies

- None

## Assumptions

- The additive-field convention within protocol v2 is sufficient — this is
  additive skew, not a structural reshape, so no `PROTOCOL_VERSION_MIN` bump is
  needed (the BUG-158 distinction)
- The suggested backend *content* is unchanged: same three backends, same
  strings; this REQ moves ownership, not content
- The bundled guide remains compiled into the system prompt and within its size
  ceiling whether generated from the catalog or checked against it
- REQ-572's three setup endpoints remain stateless; a static catalog on the
  plan result does not introduce server-held flow state (ADR-1 preserved)

## Open Questions

- [ ] OQ-1: Should the bundled guide's backend sentence be *generated* from the
  catalog at build time, or stay hand-written prose with a CI check against the
  catalog? (Architecture decision; BR-5 permits either)
- [ ] OQ-2: Does the catalog belong only on `web/setup_plan`, or should a
  discovery surface (e.g. the capability snapshot) also carry it for clients
  that want to show suggestions before starting setup? Default: `setup_plan`
  only, additive later if needed

## Out of Scope

- The VS Code extension itself — still phase-2 client work; this REQ only makes
  its suggestion list daemon-fed and CI-gated when it arrives
- Adding, removing, or altering suggested backends
- Any server-held per-session flow state (REQ-572 ADR-1 stands)
- Changes to the question flow's order, wording, or consent semantics beyond
  sourcing suggestion data from the catalog
- Localization of catalog labels/notes

## Retrieved Context

- LESSON-496 (lesson, score 6): "Cut first under pressure" means "never available" when the limit equals the count
- LESSON-481 (lesson, score 6): A gate that hides a feature from users also hides it from the test suite
- BUG-152 (bug, score 6): A prompt typed while the local tier is still loading is reported as an error
- LESSON-512 (lesson, score 5): A spec's named example is a test case, not decoration
- BUG-165 (bug, score 5): The search credential only speaks Bearer, and the spec's own example backends do not
- LESSON-495 (lesson, score 5): A remembered grant answers every question its key matches
- BUG-146 (bug, score 5): First prompt after install fails blaming the local engine for a config/timing problem
- LESSON-510 (lesson, score 4): A harness that checked a binary exists has not checked it is the one under test
- BUG-153 (bug, score 4): /exit is not a command
- LESSON-456 (lesson, score 4): A `_`-discarded error is a silent downgrade
- BUG-158 (bug, score 3): A new CLI cannot read a running v0.1.10 daemon's config
- BUG-162 (bug, score 3): model/confirm can be answered by any connection
- LESSON-497 (lesson, score 3): A test fixture that looks like a real credential blocks the push that ships it
- LESSON-493 (lesson, score 3): Bundle what only the product knows
- BUG-160 (bug, score 3): Teton cannot answer how to hook up external models — setup instructions not bundled
