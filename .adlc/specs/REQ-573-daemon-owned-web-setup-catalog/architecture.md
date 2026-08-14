# REQ-573 — Architecture: Daemon-owned web-setup suggestion catalog

## Approach

Move the suggestion catalog to the daemon as a typed factory, carry it to
clients as a new optional field on `WebSetupPlanResult`, and re-point every
surface at it:

```
crates/tetond/src/web_setup_catalog.rs      ← THE definition (pure fn, typed)
        │
        ├─ runtime.web_setup_plan()          → suggestion_catalog on the wire
        │       └─ CLI web_setup_ui.rs       → help lines, instruction_lines,
        │                                      offered_auth (data-driven)
        ├─ tests/web_setup_contracts.rs      → typed enumeration (AC-8 gate)
        │       └─ self_config.md sync check → guide ↔ catalog (bidirectional)
        └─ README drift comment              → names the one source
```

The protocol precedent is `ModelListResult` + `model_consent::list_entries`:
a protocol struct carrying a `Vec` of entries, populated by a daemon factory.
The additive-field precedent is `WebSetupPlanResult`'s own `search_gap` /
`current_web` (`#[serde(default, skip_serializing_if = "Option::is_none")]`,
no `PROTOCOL_VERSION` bump — min == max == 2 stays).

## Protocol changes (`crates/teton-protocol`)

```rust
/// One backend `/web setup` suggests, as data: shapes and templates only,
/// never a secret (REQ-573 BR-6).
pub struct WebBackendSuggestion {
    pub id: String,              // stable: "searxng" | "brave" | "kagi"
    pub label: String,           // display name
    pub endpoint: String,        // absolute URL example incl. required query
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,    // match key for typed endpoints; None = self-hosted
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_template: Option<String>, // present iff needs_key; carries {key}
    pub needs_key: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

pub struct WebSetupCatalog {
    pub default_auth_template: String,  // GENERIC_SEARCH_AUTH_TEMPLATE
    pub backends: Vec<WebBackendSuggestion>,
}

// On WebSetupPlanResult:
#[serde(default, skip_serializing_if = "Option::is_none")]
pub suggestion_catalog: Option<WebSetupCatalog>,
```

Plus one shared constant:

```rust
/// The generic auth-header shape offered when no suggestion matches, and the
/// BR-3 degraded default when a pre-573 daemon returns no catalog.
pub const GENERIC_SEARCH_AUTH_TEMPLATE: &str = "Authorization: Bearer {key}";
```

Tests: extend `the_web_setup_methods_round_trip` for the populated case; add
an absent-field deserialization test (a result JSON without the field parses,
`suggestion_catalog == None`) — both directions of BR-2/AC-1.

## Daemon changes (`crates/tetond`)

New module `web_setup_catalog.rs`: `pub fn suggestion_catalog() ->
WebSetupCatalog`, a pure function of nothing (static product data — LESSON-481:
no TTY/env/user-state gates, unit-testable in isolation). Three entries with
today's exact strings (BR-7, preserving the BUG-165 shapes):

| id | endpoint | host | auth_template | needs_key |
|---|---|---|---|---|
| searxng | `http://localhost:8888/search?format=json` | None | None | false |
| brave | `https://api.search.brave.com/res/v1/web/search` | `api.search.brave.com` | `X-Subscription-Token: {key}` | true |
| kagi | `https://kagi.com/api/v0/search` | `kagi.com` | `Authorization: Bot {key}` | true |

`default_auth_template` is `GENERIC_SEARCH_AUTH_TEMPLATE`. `runtime.rs`
`web_setup_plan()` adds `suggestion_catalog: Some(suggestion_catalog())` to its
struct literal. `server.rs` handler unchanged.

## Contract suite redesign (`crates/tetond/tests/web_setup_contracts.rs`)

- Delete `FLOW_SUGGESTIONS = include_str!("../../teton/src/web_setup_ui.rs")`
  and the `suggested_endpoints` source-text parser: the suite enumerates
  `tetond::web_setup_catalog::suggestion_catalog()` — typed iteration over the
  one definition (BR-4/AC-3).
- **Expectations stay independent of the code under test** (LESSON-512): a
  test-local table keyed by catalog `id` pins the expected header name/value
  per entry. Zipping is exhaustive both ways — a catalog entry with no
  expectation row fails ("suggestion with no contract"), and an expectation
  row with no catalog entry fails (stale table). Deriving expectations by
  parsing `auth_template` with production `search_auth_shape` would test the
  code against itself.
- The three production-builder assertions run per catalog entry, unchanged in
  substance: request shape via `Egress::lookup` (GET, terms as `q`, endpoint
  path/query preserved), documented header via `search_auth_shape()` /
  `header_value()`, and `Config::validate()` acceptance with no raw secret in
  rendered TOML.
- `BUNDLED_GUIDE` (`self_config.md`) parsing stays, re-pointed: every
  backtick auth template in the guide must be a catalog `auth_template` (or
  the generic default), AND every catalog `auth_template` must appear in the
  guide; the SearxNG endpoint-shape string (`/search?format=json`) must appear
  in both. Bidirectional, so drift fails whichever side moves (BR-5/AC-4).

## CLI changes (`crates/teton/src/web_setup_ui.rs`)

- Delete `ENDPOINT_HELP` (816), `KNOWN_BACKEND_AUTH` (824–843),
  `DEFAULT_SEARCH_AUTH` (79).
- `collect()` renders help lines from `plan.suggestion_catalog`;
  `instruction_lines()` (piped/BR-12 path) builds from the same catalog;
  `offered_auth(endpoint, catalog)` matches the parsed host against
  `backends[].host`, falling back to `catalog.default_auth_template` (BR-8).
- **Degraded path (BR-3/AC-7)**: catalog `None` (pre-573 daemon) → no named
  suggestion lines, offered default is `GENERIC_SEARCH_AUTH_TEMPLATE` (the
  shared protocol const — not a re-declared literal), needs-key default stays
  yes, flow completes. Covered by a test, not inspection.
- Fixtures: `plan_ready_for_search()` / `plan_without_search()` gain a
  catalog; the AC-2 test feeds a synthetic catalog with obviously-sentinel
  values (LESSON-497 house style, e.g. `sentinel-backend`,
  `X-Sentinel-Header: {key}`) and asserts help lines, `instruction_lines`,
  and the offered default all track the data — proving no residual
  client-side copy.
- The two constant-consistency unit tests
  (`the_offered_auth_header_follows_the_endpoints_host`,
  `every_offered_template_belongs_to_a_backend_the_help_names`) are rewritten
  against catalog data.

## README + docs

The three-way drift comment (README.md:334) collapses: it names
`crates/tetond/src/web_setup_catalog.rs` as the single in-tree source, notes
that the contract suite enumerates it typed, that the bundled guide is
CI-checked against it, and that the README rows are prose synced by the same
comment (AC-5 wording states exactly where sync is enforced).

## ADRs

### ADR-A: The catalog is a pure daemon factory returning protocol types

**Decision**: one `pub fn suggestion_catalog() -> WebSetupCatalog` in a
dedicated `tetond` module; the contract suite imports it as a lib item.

**Rationale**: BR-1 wants exactly one definition, BR-6 wants it pure and
unit-testable (LESSON-481: logic behind gates hides from tests), and the
suite already links the tetond lib for `Egress`/`Config` — typed enumeration
replaces `include_str!` source parsing, which was the fragile part of AC-8
(a rename of `ENDPOINT_HELP` would have silently emptied the parsed set;
`suggested_endpoints` only survived because `.expect()` guarded the marker).

**Rejected**: defining the data in `teton-protocol` (a shared crate invites
clients to read it compile-time, recreating the client-side copy the REQ
removes); a const table of structs with `&'static str` (forces `String`
allocation at the seam anyway; a factory keeps the protocol types clean).

### ADR-B: The generic default template is a shared protocol constant

**Decision**: `GENERIC_SEARCH_AUTH_TEMPLATE` lives in `teton-protocol`,
consumed by the daemon catalog (as `default_auth_template`) and by the CLI's
BR-3 degraded path.

**Rationale**: BR-3 requires the CLI to offer `Authorization: Bearer {key}`
when no catalog arrives, which would otherwise re-introduce exactly one
client-side literal — the drift class this REQ exists to remove. A shared
const gives the Rust surfaces one definition with zero cross-crate source
parsing; the wire field still carries the value for non-Rust clients.
Degraded-mode fidelity is bounded: a pre-573 daemon's era had Bearer as its
default, so the constant is correct for every daemon that can trigger the
degraded path.

### ADR-C: Guide stays hand-written, CI-checked — not generated (resolves OQ-1)

**Decision**: `self_config.md` remains authored prose; the contract suite
enforces bidirectional template/endpoint agreement with the catalog.

**Rationale**: the guide line is prompt prose under a hard byte ceiling
(BUG-160 sized the guide at 1,012 bytes against `REDACT_BODY_OVERHEAD_BYTES`,
pinned by regression tests on both model profiles); generating prose from
structured data needs a template as carefully worded as the prose itself and
adds a build step for three lines. The bidirectional check gives the same
drift guarantee (either side moving fails CI). Revisit if the backend list
grows past a handful.

### ADR-D: Catalog only on `setup_plan` (resolves OQ-2)

**Decision**: no second discovery surface. **Rationale**: `setup_plan` is
stateless and cheap; any client wanting suggestions can call it — REQ-572
ADR-1's shape ("clients collect, daemon validates") already makes it the
entry point of the flow. Additive later if a real client needs pre-flow
suggestions.

### ADR-E: Parity is pinned by tests at both altitudes

**Decision**: AC-6 byte-parity is asserted (a) in the daemon catalog unit
test (golden strings for the three entries) and (b) by the existing e2e
tests (`the_walkthrough_collects_every_answer_and_the_daemon_announces_the_write`
asserts the rendered SearxNG text end-to-end against the real daemon;
`a_piped_web_setup_prints_the_instructions_and_asks_nothing` covers the
piped/no-prompt path). **Note** (LESSON-510):
e2e verification must build the workspace first — a targeted
`-p teton --test cli_e2e` run against a stale `tetond` binary would test the
old daemon's catalog.

## Lessons applied

- LESSON-481 — catalog is pure, no gates between it and its tests (BR-6).
- LESSON-512 / BUG-165 — expectation table independent of production parsing;
  every named example is a contract vector.
- LESSON-497 — synthetic-catalog fixtures use obvious sentinels.
- LESSON-493 / BUG-160 — the bundled guide stays resident and under its byte
  ceiling; sync is checked, not regenerated.
- BUG-158 — additive-within-v2 skew handled by serde defaults; absent-field
  deserialization is tested, not assumed.
- LESSON-510 — final verification builds the whole workspace before e2e.
- LESSON-514 — checked, not applicable: commit/undo semantics untouched.
