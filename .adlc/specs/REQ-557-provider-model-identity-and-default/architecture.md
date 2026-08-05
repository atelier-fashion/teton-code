# REQ-557 — Architecture

## Approach

The change is a **separation of two identities that are currently conflated**: the
model a provider *calls* (a routing fact) and the model a call is *billed as* (a
pricing fact). Today only the second exists, and the first is derived from it by
`billing_model()` searching the price table by provider id. Reversing that
dependency — provider declares the model, price table consumes it — is the whole
REQ; everything else follows.

Three seams, in dependency order:

1. **Entity + config (`teton-core`)** — `ModelProvider` gains `model`;
   `Config` gains `default_provider`; `Config::validate()` enforces both. The
   one-shot migration lives here too, because this is the layer that already owns
   load, validation, and serialization.
2. **Router (`tetond`)** — `build_router` reads `p.model` rather than calling
   `billing_model`, and takes the default from config rather than array position.
   `billing_model()` is deleted.
3. **Surfaces (`teton` CLI, `teton-protocol`, `tetond/cost`)** —
   `provider add --model`, `ProviderConfig.model` in the projection, and the
   unpriced bucket gaining model identity.

### What the exploration changed about the plan

Two findings narrowed the work materially, and both are recorded because they
contradict what a reading of the spec alone would suggest:

- **The cost meter already refuses to invent a price.** `report.rs` carries an
  `UnpricedTotals` bucket and its module doc states the meter "never invents a
  cost for a model it has no price for." BR-9's *"never a `$0` record"* half is
  therefore **already satisfied** — no work. What is genuinely missing is
  identity: `UnpricedTotals` is `{calls, input_tokens, output_tokens}` with no
  model name, so AC-7b's *"a user can read off which model needs a price"* is the
  only new behavior in that area.
- **The config write path is daemon-side.** `run_provider_add` does not write the
  config file; it sends `ConfigUpdate::RegisterProvider` over RPC and the daemon
  persists (`runtime.rs:2820`). Migration therefore belongs in the daemon's load
  path, and the CLI change is purely threading a new parameter — the two are
  independent tasks rather than one.

## Data Model Changes

| Type | Location | Change |
|---|---|---|
| `ModelProvider` | `teton-core/src/entities.rs:74` | `+ model: Option<String>` with `#[serde(default, skip_serializing_if = "Option::is_none")]`, matching the existing optional-field convention on `endpoint` / `auth_ref` |
| `Config` | `teton-core/src/config.rs:92` | `+ default_provider: Option<String>`, `#[serde(default)]` |
| `ModelPrice` | `tetond/src/cost/prices.rs:38` | lookup re-oriented to `model`; `provider_id` retained for the baseline label and back-compat of the bundled table |
| `UnpricedTotals` | `tetond/src/cost/report.rs:51` | `+ models: BTreeSet<String>` (or equivalent ordered set) so the bucket names what it could not price |
| `ProviderConfig` | `teton-protocol` | `+ model: Option<String>` in the `config/get` projection |

No persisted-schema migration is required for the price table: it is
`PriceTable::bundled()`, embedded in the binary and never read from disk.

## Key Decisions

### ADR-A: The provider declares its model; the price table is a pure consumer

**Decision**: `ModelProvider.model` is the single source of the model identifier.
`billing_model()` is **deleted**, not reduced — the router reads `p.model`
directly, and pricing looks up by that model string.

**Rationale**: the current direction makes a *billing* table load-bearing for a
*routing* decision, which is why the same provider cannot serve two models. It
also produces the fallback-identifier defect: a provider absent from the price
table reports its own id as its model. Reversing the dependency fixes both with
one change. (REQ-557 BR-1, BR-2; LESSON-456 — "a fallback identifier is not
'none'".)

**Consequences**: two providers may share `kind`, `endpoint`, and `auth_ref`
while differing in `model` — verified viable, because `provider_transport`
(`runtime.rs:2596`) binds a credential to the endpoint **origin**, not the
provider id. Pricing gains a genuine "model I have no entry for" state, which the
report already models.

### ADR-B: `model` is serde-optional in the struct, required in validation

**Decision**: the field deserializes as absent and required-ness is enforced in
`Config::validate()`, alongside the existing raw-key-shaped `auth_ref` rejection
(`config.rs:269`).

**Rationale**: a bare required `String` makes every pre-REQ config fail to
*deserialize*, and a config that cannot be opened cannot be migrated — the
requirement would defeat its own migration (REQ-557 BR-7). Enforcing after load
also lets the error name the offending provider by id, which a serde error
cannot.

**Consequences**: the type system no longer guarantees a remote provider has a
model; the usability pass of ADR-E is the guarantee. See ADR-E for *which* pass —
putting it in `validate()` is the obvious reading and it is wrong.

### ADR-E: A missing model is a **usability** condition, not a **validity** condition

**Decision**: `Config::validate()` does **not** reject a remote provider whose
`model` is `None`. Instead a separate, non-fatal **usability** pass marks such
providers unusable, reports them by id, and lets the daemon start. The router
refuses to route to an unusable provider and names it.

**Rationale — this ADR exists because the obvious design bricks the daemon.**
`Config::load` is `from_toml` then `validate` (`config.rs:251`), and
`load_config` (`runtime.rs:1532`) converts any load error into *"Refusing to
start rather than fall back to an empty config that would silently drop your
privacy boundaries."* That refusal is correct and must stay. But if
`model: None` were a validation error:

- a **pre-REQ config** — every provider `model: None` — fails validation, so the
  daemon refuses to start **before migration can run**. ADR-B made the field
  *deserialize*; validation still gated startup one layer down.
- post-migration, a **single** unresolvable provider fails validation and the
  daemon refuses to start — contradicting BR-7's *"the daemon starts with that
  provider unusable rather than silently routing."*

BR-7's own vocabulary is the tell: it says **unusable**, not **invalid**. A
config naming a provider we cannot yet price is not corrupt; it is incomplete in
one entry.

**Consequences**: `validate()` keeps its fail-closed startup posture for
structural errors (duplicate ids, raw keys in `auth_ref`, a dangling
`default_provider`) — a dangling default **is** a validity error, because it
names something that does not exist rather than omitting something that does.
The unusable-provider report must reach the user at startup, or a provider
silently stops working after upgrade; it rides the same surface as the
migration's unresolvable-provider report. `teton provider add` still rejects a
missing `--model` at the CLI (TASK-046), so this path is reached by migration and
hand-edited configs, not by the normal registration flow.

### ADR-C: Migration is a one-shot, load-time, daemon-side pass keyed on absence

**Decision**: on load, a provider with `model: None` has its model resolved once
through the **legacy** provider-id price lookup, written back, and recorded as
migrated. A provider the legacy lookup cannot resolve is reported by id and left
unusable rather than defaulted.

**Rationale**: the daemon owns the config write path (`ConfigUpdate`), so this is
the only layer that can complete the write. Keying on `model.is_none()` rather
than a version stamp keeps the migration idempotent without new state.

**Consequences — and the trap this ADR exists to name**: "migrate when the field
is absent" is a guard whose condition is the absence of the feature being added.
LESSON-443 documents exactly this shape becoming a silent no-op once the feature
lands. It is safe *here* only because absence is the migration's genuine subject
rather than a proxy for something else — but the legacy lookup helper MUST be
deleted in the same change that stops needing it, or it becomes a live path that
can re-derive a model from a price table after ADR-A forbade it.

### ADR-D: An unset default is `None`, surfaced through the existing precedence

**Decision**: `Router`'s default provider is `Option<ProviderId>`. Both halves of
the current fallback chain are removed — the positional `.find(|p|
p.kind.is_remote())` **and** its fallback to `local_provider`, which itself falls
back to the literal `"local"`. An unresolvable default is reported through
`DaemonRuntime::unserved_turn_error`'s existing precedence.

**Rationale**: BUG-146's root cause #1 is this exact doubled fallback. LESSON-456's
second rule — "when a component already classifies a state for one surface, reuse
that classifier for every surface" — means no new classifier: the turn-failure
sentence and the lifecycle stream must keep agreeing.

**Consequences**: a fresh install with no providers now fails a turn with an
actionable "no default provider configured" rather than routing to a provider id
registered nowhere.

## Applicable Lessons

| Lesson | How it binds |
|---|---|
| **LESSON-456** (matched by tag grep on `daemon/router`) | Drives ADR-A's deletion of the id fallback and ADR-D's reuse of `unserved_turn_error` rather than a second classifier |
| **LESSON-443** | Named explicitly in ADR-C: the migration's guard is keyed on the absence of the field being added; the legacy helper must die with it |
| **LESSON-441** | TASK-047's mutation checks — restoring either deleted fallback must turn a test red, or the removal is unverified |
| **LESSON-432** | TASK-047's egress-capture leg: BR-8 claims this REQ changes nothing about boundary enforcement, and that is a claim tests must make, not prose |

## Proposed addition to `.adlc/context/architecture.md`

Under **Key Patterns**, after "Workflow-aware routing":

> - **Declared identity over derived identity** — a provider states the model it
>   calls; pricing, routing, and attribution all consume that declaration. No
>   subsystem re-derives an identifier from another subsystem's table, and an
>   absent identifier stays `None` rather than becoming a plausible literal.

To be applied at `/wrapup`, not now.

## Task Graph

```
        TASK-043  (teton-core: schema + validation + migration)
             │
   ┌─────────┼─────────┐
   ▼         ▼         ▼
TASK-044  TASK-045  TASK-046      ← parallel tier
(router)  (cost)    (CLI+proto)
   └─────────┼─────────┘
             ▼
         TASK-047  (e2e + egress-capture + mutation)
```

Three tiers; the middle tier is fully parallel. No task exceeds three
dependencies.
