# REQ-577 — Architecture

## Approach

Two deliverables share one data spine. A **typed vendor-recipe catalog** is the
single source for every provider fact this REQ ships; two prose surfaces (the
bundled self-config guide and the README's "Hooking up an external model"
section) stay hand-written and are CI-gated bidirectionally against it; and the
new **`teton_docs` tool** serves bundled markdown topics — one of which
(`providers`) carries the same recipes at depth, gated against the same
catalog. This is the REQ-573 "suggestion data is daemon-owned, typed,
seam-pinned" pattern applied to provider recipes, plus the REQ-563 cap-exempt
builtin-tool pattern applied to a knowledge tool.

Explorer findings that shaped the design:

- `ToolRegistry::register_cap_exempt` (tools/mod.rs:663) already exists; the
  cap machinery (`exposed_tools`, tools/mod.rs:762) needs no change.
- `DEGRADED_MAX_TOOLS = 5` (teton-providers/src/capability.rs:17) equals the
  builtin count — a non-exempt sixth tool would be silently absent on every
  degraded profile (the LESSON-496 trap; forbidden by BR-7).
- The README **already ships unpinned vendor facts** (`api.moonshot.ai/v1`,
  `kimi-k2` at README.md ~l.246) — the exact BUG-165 drift class; this REQ
  converts them into pinned copies.
- The prompt-size contract is `REDACT_BODY_OVERHEAD_BYTES = 8192` with
  `MIN_PROMPT_HEADROOM_BYTES = 48` floor, measured against the real
  `build_system_prompt` by `the_total_cap_clears_the_harness_context_budget_with_margin`
  (egress/redact.rs:1938) and `the_web_tool_docs_clear_the_outbound_body_overhead`
  (tools/web.rs:2227). Guide is currently 1,645 bytes.

## Key Decisions

### ADR-1: The recipe catalog is a pure daemon-owned factory reusing `ProviderKind`

`crates/tetond/src/provider_recipes.rs`, modeled byte-for-byte on
`web_setup_catalog.rs` (REQ-573): a pure `#[must_use]` factory
`recipe_catalog() -> Vec<ProviderRecipe>` taking nothing, reading nothing.
Fields: `id_suggestion: String` (the `teton provider add <id>` example id),
`label: String` (vendor display name), `kind: ProviderKind` (reused from
`teton_core::entities` so a recipe cannot name a kind `provider add` does not
accept — the illegal state is unrepresentable), `endpoint: Option<String>`
(`None` for `anthropic`, whose endpoint is built in), `example_model: String`,
`notes: Option<String>` (bounded; e.g. "local, keyless").

Roster (OQ-1 resolution): Anthropic, OpenAI, Moonshot (Kimi), DeepSeek,
Ollama, Grok (xAI). Each entry's facts are verified against the vendor's
current public docs at implementation time (BR-3, LESSON-512) and pinned by a
**golden verbatim test** in the module — hand-written second spellings, the
`the_catalog_ships_the_three_backends_verbatim` posture: a reworded label is a
one-line diff, a changed endpoint is a failure.

*Rejected:* protocol types in `teton-protocol` (the web catalog needed them
because `web/setup_plan` hands the list to clients; no RPC exposes recipes in
this REQ — out of scope — and a later RPC can lift the types then).

### ADR-2: Prose stays hand-written and CI-gated; never runtime-rendered

The guide and README keep hand-written recipe lines, and new contract tests in
`crates/tetond/tests/web_setup_contracts.rs` (the file that already owns the
guide↔catalog gates) pin them bidirectionally:

- `the_bundled_guide_and_the_recipe_catalog_agree` — every catalog entry's
  endpoint (where present) and example `provider add` shape appears in
  `self_config.md`, and every endpoint the guide names is in the catalog.
- `the_readme_recipes_and_the_catalog_agree` — same for the README's bash
  block (which today carries unpinned `kimi` facts).

*Rejected:* rendering recipe lines into the prompt from the catalog at
`build_system_prompt` time. It would kill the drift class by construction, but
it forks the guide into static-file + generated halves, complicates the
margin tests' "measure the real static shape" property, and departs from the
proven REQ-573 seam for no failure mode the gates don't already catch.

**Guide format budget:** one compact line per vendor inside the existing
numbered recipe (step 1) of `self_config.md`, e.g.
`(endpoints: Moonshot/Kimi https://api.moonshot.ai/v1, DeepSeek …, Grok
https://api.x.ai/v1, Ollama http://localhost:11434/v1 keyless; OpenAI/Anthropic
built-in kinds)` — target ≤ 450 added bytes. **Fallback posture (spec
Assumptions):** if either margin test's 48-byte floor breaks, recipes live
only in the `providers` topic, the guide keeps the generic shape plus its
existing pointer budget, and the guide↔catalog gate retargets the topic file.
The margins are never traded away.

### ADR-3: `teton_docs` is a sixth builtin, registered cap-exempt inside `with_builtins()`

New `crates/tetond/src/harness/tools/docs.rs` implementing `Tool`
(tools/mod.rs:518): `name() = "teton_docs"`, one-string `topic` argument,
`gates_itself() = false` (no permission prompt — read-only, no user data, like
read/grep), default `refine`. Topics live as markdown files under
`crates/tetond/src/harness/docs/{providers,policy,web,doctor}.md`, compiled in
via `include_str!` (the `structured/templates.rs` precedent), served from
memory — no filesystem read, no transport, provenance identical to a tool that
touched no paths (BR-6).

Registration happens **inside `ToolRegistry::with_builtins()`** via
`register_cap_exempt`, unlike web's call-site registration: web is opt-in
config-gated (its constructor comment explains why it must NOT live there);
`teton_docs` is unconditional product surface, so the constructor is the seam
that makes "present in every session" true by construction — offline sessions,
template smoke, and every prompt-measure test inherit it with no fixture edits.

**Cap posture (OQ-3 resolution): cap-exempt.** The integration explorer
recommended non-exempt; rejected — with `DEGRADED_MAX_TOOLS == 5 ==` builtin
count, non-exempt means *never exposed on any degraded profile*, which BR-7
names as the one unacceptable outcome (LESSON-496). The
`register_cap_exempt`/`with_builtins` doc comments' "exactly one tool
registers this way" sentence is rewritten to enumerate both exempt tools with
their **distinct** rationales: web = user opt-in must survive the cap;
teton_docs = self-serving product knowledge, most needed exactly where the cap
bites (BR-7), so the exempt set stays a checked rule, not a dumping ground.

Error posture: unknown topic returns the didactic
`ToolOutcome::error("unknown topic \`{t}\`; valid topics: providers, policy,
web, doctor")` — mirroring `dispatch`'s unknown-tool posture (tools/mod.rs:701).
`describe_call` (turn_loop.rs:1214) gains a `"teton_docs"` arm rendering
`teton_docs <topic>` for the tool_call event title.

**Per-topic ceiling (BR-9): 4,096 bytes per topic**, pinned by a test that
iterates every bundled topic. Rationale: a topic is a *tool result* (not
resident prompt); against `LOCAL_ENGINE_N_CTX = 16,384` tokens and the
byte-denominated harness budgets, 4 KiB ≈ 1K tokens keeps a full docs read a
small fraction of the window so it can never evict the conversation it
serves. Failure message instructs trim-or-split, never delete (BUG-160
posture).

No ADR-009 impact: topics are plain markdown with no new envelopes or
delimiters; results ride the existing `frame_untrusted_builtin` framing and
render-time neutralization untouched.

### ADR-4: The referral clause is dictated, pinned, and placed with the key rule

One imperative sentence added to `self_config.md` beside the existing "Never
ask the user to type an API key" rule: the agent cannot run Teton's own setup
commands itself and must give the user the exact commands to run. BUG-168
wording rules apply — stated outright, no em-dash aside, no meta-instruction
prefix. The BUG-160-lineage content-pin tests in `turn_loop.rs` are extended
to pin the clause on both harness profiles, with the update-don't-delete
failure message. Live wording verification rides AC-1/AC-2 (ADR-5).

### ADR-5: Test economy and the weights-gated acceptance run

- **Inherited automatically** (via constructor registration): both margin
  tests re-measure the real prompt including the new tool docs; the tool's
  `description()` is budgeted ≤ ~120 chars.
- **Updated invariants:** `docs_are_capped_by_max_tools_for_degraded_providers`
  (tools/mod.rs:997) — at `Some(5)` the exposure is now the 5 builtins *plus*
  `teton_docs`; `a_cap_exempt_tool_is_never_displaced_by_the_max_tools_cut`
  (tools/mod.rs:1034) — fixture now starts from a registry already carrying
  one exempt tool.
- **New tests:** catalog goldens (ADR-1); the two prose gates (ADR-2); topic
  ceiling sweep, unknown-topic error, degraded+offline exposure
  (`exposed_names(Some(DEGRADED_MAX_TOOLS))` contains `teton_docs`; offline
  session serves a `teton_docs` call with zero egress events via the capture
  transport) (ADR-3).
- **Gated-surface sweep** after the `with_builtins` change:
  `cargo check -p tetond -p teton-inference --features
  tetond/llama,teton-inference/llama --tests` (LESSON-515) — `template_smoke`
  consumes `with_builtins()` and must still compile; its runtime contract is
  unchanged (one templated turn, `#[ignore]`d without weights).
- **AC-1/AC-2 live A/B** needs a `tetond/llama` release build plus the 17 GiB
  local weights (LESSON-482 isolation method: short `XDG_RUNTIME_DIR`,
  symlinked weights, fresh base dir). Weights may be absent on this machine
  (validation warning). If so, the run is recorded as **deferred manual
  verification** in the task and CHANGELOG-adjacent notes with the exact
  commands — stated plainly, never claimed as done.

## Data Model Changes

None persisted. No config keys, no protocol changes, no events beyond the
existing `tool_call` shape (title via `describe_call`).

## Deviations

**ADR-5 amendment (TASK-145): `REDACT_BODY_OVERHEAD_BYTES` raised 8 → 9 KiB.**
ADR-5 assumed the margin tests would simply re-measure and stay green. They did
not: the system prompt was 4,849 bytes against an effective ceiling of 4,868
(8,192 overhead − 3,276 escaping − the 48-byte floor), leaving **19 bytes of
slack**, while `teton_docs` costs 274 bytes of tool docs and ~156 even stripped
to a bare description and schema. No trim could close a 255-byte deficit, and
the deficit predates any guide growth, so ADR-2's fallback posture — move the
recipes into the topic — does not reach it either; BR-7 forbids dropping the
tool. The budgeting *assumption* moved instead, exactly as its own doc comment
anticipates ("if a later REQ adds enough tools to overflow that, the assumption
turns red"). `MIN_PROMPT_HEADROOM_BYTES` is untouched: that floor is the line
BR-4 forbids trading. Chunk arithmetic is unchanged — 2×(32,768+9,216) = 83,968
≤ 108,280, and 83,968/27,070 = 3.10 still rounds to 4 chunks, so
`REDACT_INPUT_MAX_BYTES` does not move. Recorded margins are now 817 bytes
(opted-out shape) and 865 (opted-in), leaving roughly **769 usable bytes** for
TASK-144's guide recipe line and referral sentence against its ≤450-byte target.

## Proposed Additions to `.adlc/context/architecture.md`

At wrapup, extend the "Suggestion data is daemon-owned, typed, and seam-pinned"
bullet with the recipe catalog as a second instance, and note the cap-exempt
set's membership rule ("exempt for a *stated* reason, each rationale distinct
and doc-commented").
