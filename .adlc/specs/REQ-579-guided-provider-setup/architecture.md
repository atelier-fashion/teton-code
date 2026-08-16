# REQ-579 — Architecture: Guided in-session provider setup

## Approach

Mirror REQ-572's `/web setup` in shape, reuse its parts by reference, and add
nothing the precedent did not already need. Concretely:

- **Client (`teton`)** — a new `provider_setup_ui.rs` walkthrough behind a
  `provider setup` row in the `COMMANDS` table. It owns step state, asks the
  questions through the existing `SetupIo`/prompter seam, reads the key
  echo-off, stores it via the shared `Keychain` + `PriorKey` undo machinery, and
  drives three stateless daemon RPCs. On a non-TTY surface it prints the CLI
  recipe and exits.
- **Daemon (`tetond`)** — three new methods, `provider/setup_plan`,
  `provider/setup_preview`, `provider/setup_commit`, dispatched exactly as the
  `web/setup_*` trio is: `refuse_unmintable_session_id` + `may_drive` on all
  three; commit additionally async-spawned and behind
  `refuse_unattested_commitment`. The catalog on `plan` is
  `provider_recipes::recipe_catalog()` mapped to a protocol type. `preview`
  builds a candidate `Config` from the answers (composing the endpoint through
  `teton_core::compose_endpoint`), validates it, renders the exact TOML delta,
  and returns bytes + dial host + warnings + digest. `commit` rebuilds the same
  candidate, checks the digest, writes the digested bytes once, re-derives
  routing, and emits `provider_setup_completed`.
- **Protocol** — `ProviderSetup{Plan,Preview,Commit}{Params,Result}`,
  `ProviderRecipeEntry`, `TierSummary`, two events, one new wire error code.
- **Model** — one guide-line edit in `self_config.md`: point at
  `/provider setup <vendor> [tier]` first, `teton provider add` second. No new
  capability clause (ADR-3).

Nothing here is a new capability for the model. It gains a sentence; it loses
nothing; it still never sees a key.

## Data model changes

**Config on disk** — none. The commit writes the same `[[providers]]` row
(`auth_ref = "keychain://teton/<id>"`) and `[[tiers]]` rows (`tier`,
`provider_id`) that `teton provider add` and `teton policy set-tier` write
today, through the same comment-preserving writer (REQ-574). (An earlier draft
of the spec's mock spelled these `[policy.tiers.<tier>]` / `api_key`; the
schema has no `[policy]` table — TASK-153 built against the real one, and the
spec's example was corrected to match.)

**Protocol (teton-protocol/src/methods.rs, events.rs)** — additive:

| Type | Direction | Fields |
|---|---|---|
| `ProviderSetupPlanParams` | C→D | `session_id` |
| `ProviderSetupPlanResult` | D→C | `catalog: Vec<ProviderRecipeEntry>`, `existing: Vec<ExistingProvider {id, kind, model}>`, `tiers: Vec<TierSummary {tier, provider_id: Option<String>}>` |
| `ProviderRecipeEntry` | D→C | `id_suggestion, label, guide_spelling, kind, endpoint: Option<String>, example_model, notes: Option<String>` — 1:1 with `ProviderRecipe` |
| `ProviderSetupCandidate` | C→D | `id, kind, endpoint: Option<String>, model, key_ref, bindings: Vec<TierBinding {tier, provider_id}>` |
| `ProviderSetupPreviewParams` | C→D | `session_id, candidate` |
| `ProviderSetupPreviewResult` | D→C | `toml, dial_host, warnings: Vec<String>, digest, replaces: Option<ExistingProvider>` |
| `ProviderSetupCommitParams` | C→D | `session_id, candidate, expect_digest: Option<String>` |
| `ProviderSetupCommitResult` | D→C | `applied, provider_id, bindings` |
| `Event::ProviderSetupCompleted` | D→clients | `provider_id, kind, model, bindings` — session scope comes from the `EventEnvelope`, which flattens over the payload; a payload `session_id` would be a duplicate wire key (the web precedent's test asserts this) |
| `Event::ProviderSetupRejected` | D→clients | `method` — wire name `provider_setup_rejected_nonuser` (serde rename; scope from the envelope) |
| error code | | `PROVIDER_SETUP_INVALID` (candidate refused by validation / digest mismatch). The caller gate answers a foreign connection with the existing `NOT_ATTACHED` code — exactly as `web/setup_*` does — and the commit additionally announces `provider_setup_rejected_nonuser`. (There is no `SETUP_REJECTED_NONUSER` code in the codebase; an earlier draft of this table assumed one.) |

`key_ref` is a keychain reference (`keychain://teton/<id>`); the daemon rejects
any candidate whose `key_ref` does not parse as a reference — the same
structural rule `Config::validate` already applies to `api_key`.

## API changes

Three JSON-RPC methods, session-scoped, mirroring `web/setup_*` in every gate.
No change to `config/set`, `RegisterProvider`, or `SetTierBinding`; the commit
*composes* them into one candidate rather than calling them (ADR-1).

## Service layer

**tetond/src/runtime.rs** — three new `DaemonRuntime` methods:

- `provider_setup_plan(&self) -> ProviderSetupPlanResult` — pure over the
  current config snapshot + `recipe_catalog()`.
- `provider_setup_preview(&self, candidate) -> Result<Rendered, RpcError>` —
  builds candidate `Config` = current + `RegisterProvider(candidate)` +
  `SetTierBinding` × bindings, runs `Config::validate()`, renders the delta
  through the REQ-574 writer *in memory* (as `web_setup_preview` does), digests
  it, computes `dial_host` with the dial-time authority parser, collects
  warnings (`replaces existing provider …`, unpriced model, cleartext endpoint).
- `provider_setup_commit(&self, candidate, expect_digest) -> Result<…, RpcError>`
  — re-derives exactly as preview, compares digest, writes the digested bytes
  as-is (not `persist_config`, per the `web_setup_commit` comment block), then
  swaps the already-validated `candidate_config` into memory — not a re-load
  from disk, which would be a second read that can disagree with the bytes
  just validated (LESSON-451); `build_router` clones the config per turn, so
  the next routing decision sees it with no restart (REQ-572 BR-8). Returns
  `applied: false` when the config already matches.

**tetond/src/server.rs** — `handle_provider_setup_{plan,preview,commit}`; the
commit is added to the async-spawn router and to `COMMITMENT_METHODS`.

**teton/src/session_ui.rs** — `render_event` is an exhaustive match, so the two
new `Event` variants require render arms; TASK-152 added them (a swallowed
`provider_setup_completed` would be a BR-15 defect). TASK-155 may reword.

**teton/src/provider_setup_ui.rs** — `run(conn, ctx, keychain, vendor_arg,
tier_arg)`; a `Gate` (walk vs instructions); a lenient vendor resolver over the
plan's catalog (ADR-2); reuse of `settle_endpoint`'s pure part for
compose+echo; `ask_secret` for the key; `PriorKey::read` before store; preview
render; confirm (default N); commit; undo on refusal; instruction lines on
non-TTY.

## Key decisions

### ADR-1 — A dedicated `provider/setup_*` trio, whose commit composes `ConfigUpdate`s into one write (resolves OQ-1)

**Decision.** Three dedicated methods, not `config/set`. The commit builds a
single candidate `Config` carrying both the provider row and every tier
binding, validates it once, and writes it once.

**Why.** Two facts settle it. (1) `DaemonRuntime::apply_config_update`
validates and **persists one `ConfigUpdate` per call** — riding `config/set`
would make "register + route" two durable writes with a window between them in
which the provider exists unrouted and a crash leaves it so; the spec says
atomic. (2) Preview needs to return typed state and the exact bytes a digest
was taken over; `config/set` has no preview and no digest, and adding one to a
general mutation RPC is a bigger surface than adding a trio the codebase
already has a template for. The trio also inherits `web/setup_*`'s gate
wiring unchanged — `may_drive`, `refuse_unmintable_session_id`,
`refuse_unattested_commitment`, the `COMMITMENT_METHODS` enumeration test — so
there is one presence posture, not two.

**Cost.** Three method names instead of zero. Accepted: they are the same three
the precedent has, and a second general-purpose config RPC would have been the
larger novelty.

### ADR-2 — The vendor argument is resolved leniently, client-side, against the daemon-served catalog (resolves OQ-4)

**Decision.** `/provider setup <vendor>` accepts `id_suggestion`, `label`, or
`guide_spelling`, case-insensitively and ignoring punctuation/whitespace
(`kimi`, `Kimi`, `moonshot`, `Moonshot (Kimi)`, `Moonshot/Kimi` all resolve to
the same entry). Zero matches → the flow lists the catalog and asks; more than
one → the flow lists the matches and asks.

**Why.** The resident guide teaches the model vendors by `guide_spelling`
(`Moonshot/Kimi`), not by id, and prompt margins are thin (ASSUME-008) — teaching
ids would cost bytes the ceiling test does not have. Resolving leniently means
the model can hand off with whatever spelling it already knows and the flow
still lands on the right entry, so AC-1 does not depend on the model emitting a
canonical id. Resolution happens against the *plan's* catalog, so there is no
CLI-side vendor list to drift (BR-4).

### ADR-3 — The steer is a guide-line edit, not a capability clause

**Decision.** Edit `self_config.md` line 2 from "point them at
`teton provider add` or `/web setup`" to name `/provider setup <vendor> [tier]`
first for an interactive session and `teton provider add` second for a shell.
Keep the per-vendor recipe list (line 5) verbatim. No new
`*_capability_clause` in `turn_loop.rs`.

**Why.** The web clause exists because web has an *off / partial / unavailable*
state the model can hit mid-turn and must explain (REQ-572 BR-1/BR-10);
"connect Kimi" is not a refusal, it is a front-door question, and REQ-577
established that front-door provider questions are answered from the resident
guide (ASSUME-008 recorded that the model reaches the guide, not the docs
tool). Editing the sentence the model already reads is the smallest change
that reaches every session; the recipe list stays because it is the BR-11
non-TTY answer and the vendor names the model uses. Size: the edit is
byte-neutral-or-smaller and `the_total_cap_clears_the_harness_context_budget_with_margin`
pins it.

### ADR-4 — Catalog served from the same typed source the model reads

**Decision.** `plan.catalog` is `recipe_catalog()` mapped field-for-field to
`ProviderRecipeEntry`. A contract test asserts the mapping is total (every
recipe, every field). The existing guide↔catalog and README↔catalog gates
(REQ-577 ADR-2) are unchanged and now transitively pin the client's view.

### ADR-5 — Keychain identity is the provider id, exactly as `teton provider add`

**Decision.** Account = `<id>`, reference `keychain://teton/<id>`. `PriorKey::read`
before store; the shared undo (BUG-171 / LESSON-514) on refusal; store happens
after the user confirms the preview and before commit, as `/web setup` orders
it.

**Why.** A user who registered `kimi` from the shell and later rotates it from
a session must hit the same keychain row; two naming schemes would leave the
old key orphaned and the config pointing at whichever won.

### ADR-6 — Tier is an optional argument; the default offer is the argument, else `think`; bindings are a list; no fallback in v1 (resolves OQ-2)

`/provider setup kimi think` → offer `think` first. `/provider setup kimi` →
offer `think`. The routing question is a checklist over routable tiers; zero
selections is allowed and reported ("registered but unrouted; `teton policy
show` / `/provider setup` to route it later"). `--fallback` is named in the
completion message as a `teton policy set-tier` capability, not asked.

### ADR-7 — Anthropic skips the endpoint prompt (resolves OQ-3)

`kind = anthropic` → no endpoint question; the composed default is echoed in
the preview line so the user still sees what will be dialed.

### ADR-8 — Endpoint composition and URL shape are `teton_core`'s, called, not copied

`compose_endpoint(kind, input)` and the dial-time authority parser are the only
URL logic; `settle_endpoint`'s echo-before-key ordering is reused by making its
pure core `pub(crate)`. LESSON-528/529 are the reason this is an ADR and not a
note.

### ADR-9 — The hand-off is guaranteed by the surface, not begged from the model (added after verification.md rounds 1–3)

**Decision.** At the end of every typed-prompt turn on a TTY surface, if the
model's reply text for that turn contains `teton provider add` or
`teton policy set-tier`, the CLI appends exactly one `LineKind::Notice`:
*"in this session, `/provider setup <vendor> [tier]` does this without leaving
it — no key in chat."* Deterministic string match on the model's own output;
zero model calls; visibly the harness's voice (`>>`), not the model's.

**Why.** Three live rounds against the shipped local model (verification.md):
hand-off in the preamble, hand-off inside the numbered step, and the competing
recipe removed entirely — 0/9 replies volunteered the command, while the
endpoint and model transferred exactly every time. The data crosses; the
instruction does not, and round 3 showed that pushing further on the prompt
regresses other behaviour (shell probes, doubled model calls, hallucinated
command). REQ-572 already established that a dead-end is answered by the
*product* naming the enablement path; this is that rule applied to a
front-door answer the model got wrong. It fires only when the model reached
for the CLI, so a session that never mentions providers never sees it, and a
model that one day volunteers the command makes it dormant without a code
change.

**Cost.** One prose match on the model's output. Accepted: the strings matched
are Teton's own command names, which the model can only have got from the
guide, and a false positive costs one true sentence.

## Proposed additions to `.adlc/context/architecture.md`

Under "Enablement is collection at the edge, commitment at the core", add one
sentence: *the pattern now has two instances — `/web setup` (REQ-572) and
`/provider setup` (REQ-579) — and a third should be built by copying the trio,
not by generalising `config/set`, because a preview-then-digest-bound-commit is
what makes the flow safe and `config/set` has neither.*

## Lessons applied

- LESSON-514 / BUG-171 — read-before-store, restore-on-refusal; the shared undo.
- LESSON-517 — the seam owns the styling; the flow hands plain text to the
  prompter and the sanitizer strips nothing it should not.
- LESSON-519 — AC-4/AC-7/AC-8 assert on the real keychain and the real config
  bytes; the tests use the in-memory keychain seam and read the written file.
- LESSON-522 — the candidate `Config` is built by identity (`id`), never by
  index; replacing an existing provider is a matched edit.
- LESSON-523 — at least one real end-to-end registration through the trio in
  the e2e suite, not just unit tests over the pure functions.
- LESSON-525 — every credential concern (rollback, echo-off, reference-only) is
  applied at this second surface, not assumed inherited.
- LESSON-528 / LESSON-529 — no mirrored predicate; `dial_host` is rendered by
  the parser that dials.
- LESSON-513 — plan/preview refusals are in-response only; the event is emitted
  for the commit refusal alone.
- LESSON-470 — the confirm defaults to **N** (durable write); the routing offer
  defaults to **Y** (the user asked for it and it is reversible).
