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
endpoint's **vendor-documented base form** yields exactly the recipe's
endpoint: the bare-`/v1` form for recipes whose endpoint carries a `/v1`
segment (OpenAI, Moonshot, Ollama, xAI), the bare origin otherwise
(DeepSeek, Anthropic). Derive the base form from the recipe endpoint by
stripping the kind's `canonical_request_path()` suffix — never hand-write
base URLs; (c) the Anthropic default constant equals the Anthropic recipe's
endpoint.

**Known limit (recorded 2026-08-15, TASK-148):** a bare *origin* for a
`/v1`-family vendor (`https://api.openai.com`) composes to
`…/chat/completions` without `/v1` — a URL that vendor does not serve. This
is BR-2 as specified: the kind-level rule cannot know whether `/v1` belongs
(DeepSeek's documented base has none), and per-vendor variants are out of
scope. Mitigations already in the design: vendors document their base URLs
*with* `/v1` where it belongs, so the common paste composes correctly; BR-4's
echo shows the composed value before any key is read; the doctor advisory
names the full form for bare shapes. TASK-149's echo tests must include a
bare-origin case so the visibility claim is itself tested. Now the two spellings of the
contract fail together on drift, and (b) is AC-7's mutation target: stub the
composition to identity and this test fails on the missing path.

*Amended (2026-08-15, verify pass):* the doctor advisory no longer asserts
what the vendor serves. For a bare origin on an OpenAI-compatible provider it
names the composed form **and** the `/v1` alternative and points at the
vendor's docs; the unambiguous shapes (bare `/v1`, and Anthropic's bare
origin, whose canonical path carries its own version segment) keep the single
plain form. The old wording was live-observed advising
`https://api.openai.com/chat/completions`, which 404s.

**Second known limit (recorded 2026-08-15, verify pass):** a *versioned but
non-`/v1`* base — `https://host/v2`, `https://host/v1beta`,
`https://host/openai/v1` — is BR-2 class (c): an explicit path, so it is
stored verbatim, with **no echo and no doctor advisory**. That is correct per
BR-2 (class (b) is an exhaustive list of three bare shapes, deliberately not
"roughly empty-looking"), and it is silent by design rather than by oversight:
a rule that guessed at arbitrary version segments would start rewriting the
gateway paths class (c) exists to protect. The cost is that this shape gets
neither of the two mitigations above, so it fails the way the pre-REQ product
failed — a 404 on the first turn. Recorded rather than fixed; a per-vendor
recipe lookup is the shape of an answer, and it is out of scope here.

### ADR-3: Flow order in `run_provider_add`

**Decision (amended 2026-08-15, TASK-149):** `--model` pre-check →
duplicate-id pre-check → **compose + Anthropic default** → **echo when
`changed`** (via `surface.line`, before any prompt) → `read_secret` →
`build_provider_registration(composed_endpoint)` → RPC. The echo lands before
the credential prompt so the user sees what will be stored before committing a
key (BR-4/BR-5 together); BUG-171's rejection take-back stays byte-untouched
downstream. The existing e2e
`provider_add_without_a_model_refuses_before_asking_for_a_credential`
(cli_e2e.rs:1706) is the pattern for the new ordering test.

The amendment is the position of the duplicate-id probe, which this ADR
originally sketched *after* the composition step. Two consequences moved it
back in front, and neither weakens the decision's own reason — everything the
user needs in order to decide whether to type a key is still on screen before
they are asked for one:

- A command already refused for a better reason must keep being refused for
  that reason. `provider add deepseek …` on an existing id answered "already
  registered" before this REQ and answers it still (BR-7); a
  missing-`--endpoint` message there would be true, unhelpful, and a change to
  a shipped refusal.
- An echo printed before a refusal would say "endpoint stored as …" about a
  registration that is not happening.

**Further amended (2026-08-15, verify pass):** `settle_endpoint` also refuses
an endpoint containing TAB/LF/CR *before* composing — those bytes are deleted
by URL parsers and rendered as spacing by a terminal, so the echoed string
would not be the dialled string and BR-4's mitigation would be defeated — and
emits a cleartext-credential notice (`http://` to a non-loopback host) after
the echo, so it sits immediately above the prompt it is about. Both are inside
the same pre-credential window and change no ordering already claimed.

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
