# REQ-578 — Architecture

## Approach

One pure composition module in `teton-core` becomes the authoritative home of
the per-kind canonical request paths; the CLI's registration flow calls it
before anything credential-shaped happens; `teton doctor` reuses its
classifier for the advisory; and a new tetond-side **bridge test** pins the
module against the REQ-577 recipe catalog without touching any AC-6-protected
file. No protocol, daemon, adapter, or persisted-schema change anywhere —
the stored `ProviderConfig.endpoint` remains the absolute request URL and the
verbatim-POST contract stays exactly as pinned.

Explorer findings that shaped the design (post-BUG-171 state):

- `run_provider_add` (crates/teton/src/main.rs:1295) already pre-checks
  `--model` (l.1306) and duplicate id (l.1332) **before** `read_secret`
  (l.1347) — BR-5 is a third pre-check in an existing pattern, not a new one.
- `build_provider_registration` (main.rs:1820) stores `endpoint` verbatim —
  it stays a pure assembler; composition runs earlier, in `run_provider_add`.
- The canonical suffix facts currently exist only inside the protected seam
  test's hand-written match (provider_recipes.rs:664) and the recipe values
  themselves — the new module becomes the *source*, the bridge test makes
  drift between the two spellings a failure.
- BUG-171's take-back machinery (`PriorKey`, `report_registration_outcome`,
  tests at main.rs:3025/3058) sits downstream of the reorder point and is
  untouched.
- Test utilities exist for every needed fixture: `MockKeychain`,
  `ScriptedPrompter`, `RecordingSurface`, `TestDaemon` (tests/common/mod.rs).
- REQ-576 presence attestation is orthogonal: config/set degrades to allow
  on no-presence builds, so the e2e path needs no new seams.

## Key Decisions

### ADR-1: Composition lives in `teton-core` as a pure, dependency-free module

New `crates/teton-core/src/endpoint_composition.rs`:

- `pub const OPENAI_COMPATIBLE_REQUEST_PATH: &str = "/chat/completions"`,
  `pub const ANTHROPIC_REQUEST_PATH: &str = "/v1/messages"`,
  `pub const ANTHROPIC_DEFAULT_ENDPOINT: &str = "https://api.anthropic.com/v1/messages"`.
- `pub fn compose_endpoint(kind: ProviderKind, input: Option<&str>) ->
  ComposedEndpoint` where `ComposedEndpoint { stored: Option<String>,
  changed: bool }` implements the BR-2 classes: canonical-suffix input →
  verbatim (`changed: false`); no path / bare `/` / bare `/v1(/)` → append
  the kind's path; any other explicit path → verbatim; `None` + Anthropic →
  the default endpoint (`changed: true`); `None` otherwise → `None`
  (validation downstream still owns the missing-endpoint refusal for
  openai-compatible).
- Path inspection is deliberate string work (find `://`, then the first `/`
  after the authority), no `url` crate: teton-core carries no HTTP
  dependencies today and the classifier needs only "does a path exist and is
  it one of three trivial shapes". Malformed input (no scheme) is class (c)
  — stored verbatim, refused later by the same validation that refuses it
  today (BR-6: no new fatal class).

*Rationale for placement:* `ProviderKind` lives in `teton-core::entities`;
the module is pure (LESSON-481's testable-without-environment posture); both
the CLI today and any future client of the registration path share it (OQ-1
resolution). *Rejected:* `teton-protocol` (wire types, not policy; no TS
client yet), CLI-local (unshareable), daemon-side (OQ-1 keeps raw
`config/set` verbatim per the REQ-574 durable-document posture).

### ADR-2: The bridge test pins module ↔ catalog without touching protected files

AC-6 freezes `provider_recipes.rs`, `conformance.rs`, and
`web_setup_contracts.rs`. The seam test's hand-written suffix match therefore
*stays*, and a new `crates/tetond/tests/endpoint_composition_bridge.rs`
asserts, for every `recipe_catalog()` entry: (a) **idempotence** —
`compose_endpoint(kind, Some(recipe.endpoint))` returns it unchanged with
`changed: false`; (b) **base→full agreement** — composing the recipe
endpoint's origin (and its bare-`/v1` form where the canonical path begins
with `/v1`) yields exactly the recipe's endpoint; (c) the Anthropic default
constant equals the Anthropic recipe's endpoint. Now the two spellings of the
contract fail together on drift, and (b) is AC-7's mutation target: stub the
composition to identity and this test fails on the missing path.

### ADR-3: Flow order in `run_provider_add`

`--model` pre-check → **compose + Anthropic default** → **echo when
`changed`** (via `surface.line`, before any prompt) → duplicate-id pre-check
→ `read_secret` → `build_provider_registration(composed_endpoint)` → RPC.
The echo lands before the credential prompt so the user sees what will be
stored before committing a key (BR-4/BR-5 together); BUG-171's rejection
take-back stays byte-untouched downstream. The existing e2e
`provider_add_without_a_model_refuses_before_asking_for_a_credential`
(cli_e2e.rs:1706) is the pattern for the new ordering test.

### ADR-4: Doctor advisory is CLI-side, reusing the classifier

`run_doctor` (main.rs:1154) already fetches the config snapshot; a new pass
over remote providers applies the classifier's "would compose" predicate and
emits one `LineKind::Notice` per class-(b)-shaped endpoint naming the exact
full form (`compose_endpoint`'s output). Custom paths are silent; exit
status unchanged (BR-6). *Rejected:* a daemon-side doctor RPC — new protocol
surface for an advisory that only needs data doctor already has.

### ADR-5: Test economy

- **Unit table** in the new module: every (kind × class) cell plus trailing
  slash, `/v1/` spelling, missing-scheme, and `None` inputs.
- **CLI unit tests** (main.rs test module): echo on composed/defaulted
  endpoints and silence on verbatim ones (RecordingSurface); Anthropic
  default applied; `stored_registration` helper reads
  `ANTHROPIC_DEFAULT_ENDPOINT` instead of its hardcoded copy; prompt-order
  test via ScriptedPrompter asserting the echo precedes the secret prompt.
- **E2E** (cli_e2e.rs, TestDaemon): AC-1 base-URL composition through real
  `config/set`; AC-2 idempotence; AC-3 Anthropic default + BR-5 ordering;
  AC-4 custom path verbatim; AC-5 doctor advisory (extending the existing
  doctor e2e at cli_e2e.rs:393).
- **Bridge test** (ADR-2) doubles as the AC-7 mutation vehicle; record the
  mutation demonstration in the commit body.
- **Must stay green, unmodified**: the ~20 enumerated registration/BUG-171/
  REQ-576/REQ-577 tests, most critically `a_configured_endpoint_is_the_request_url_verbatim`,
  `every_recipe_is_a_registration_the_daemon_accepts_and_an_adapter_can_post`,
  `a_rejected_registration_takes_back_the_key_it_stored`, and
  `a_rejected_registration_restores_the_credential_it_displaced`.
- Gated sweep (LESSON-515) after workspace changes; no feature-gated surface
  is expected to move, but the sweep is cheap and mandatory.

## Data Model Changes

None. No protocol changes, no config schema changes, no new events.

## Proposed Additions to `.adlc/context/architecture.md`

At wrapup: one sentence extending the "durable document" / registration
pattern family — "a forgiving input surface normalizes at the write seam and
echoes what it stored; the stored value is always the literal contract value,
so every downstream consumer (validation, adapters, egress) stays
verbatim" (REQ-578).
