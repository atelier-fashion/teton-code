---
id: BUG-155
title: "REQ-557's deleted provider-id fallback was only relocated, and three other defects it shipped"
status: resolved
severity: high
created: 2026-08-05
updated: 2026-08-05
component: "tetond/router"
domain: "routing"
stack: ["rust"]
concerns: ["correctness", "billing-honesty", "privacy", "data-loss"]
tags: ["fallback-identifier", "mutation-testing", "post-merge-review", "atomic-write", "price-table"]
req: REQ-557
---

## Description

A post-merge review panel on REQ-557 (merged as `3aefb6b`) found that the REQ's
central claim does not hold, plus three further defects the same change shipped.
Three independent reviewers converged on the first; each finding below was then
reproduced by hand before being accepted.

### C1 — the provider-id fallback was relocated, not deleted

REQ-557 BR-1 says the `map_or_else(|| provider_id.to_owned(), …)` fallback in
`billing_model()` is "deleted, not relocated". `billing_model` did go — but
`run_one_attempt` carried its own copy:

```rust
let model = route.model.clone().unwrap_or_else(|| provider_cfg.id.clone());
```

Pre-REQ that line was unreachable: `default_provider` was always derived from a
provider the router had registered, so `route.model` was never `None`. **The
redesign made it live.** `build_router` skips a remote provider that declares no
model (ADR-E), so it never enters the router's provider map — but three paths
read a provider id straight out of config without consulting that map:

1. **`resolve_freeform`** trusts `default_provider` unconditionally.
   `Config::validate` accepts a default naming a registered-but-*unusable*
   provider, because BR-6 checks registration and ADR-E deliberately keeps
   usability out of validation.
2. **`fallback_for`** reads `policy.fallback_id` with no health screen, so a
   mid-turn failure of a healthy primary could fail over to an unusable one.
3. **`config/set` `register_provider`** accepted a remote provider with no
   model, persisted it, and logged nothing. AC-2's guard lived only in
   `teton provider add`, so every non-`teton` ACP client bypassed it — including
   this suite's own `register_provider` helper.

In each case the turn egressed with the provider's **id** as its model, the call
was billed under that id, and `teton cost` then named the id as "a model needing
a price". Reproduced end to end: `"model":"mystery"`, `"model":"broken"`,
`"model":"gpu-box"` on the wire.

### C2 — paid remote calls billed as $0, with fabricated savings

Re-keying the price table on `model` alone made the bundled `provider_id =
"local"` zero-cost rows apply to **any** provider declaring those model names —
precisely the self-hosted-gateway shape BR-3 exists to enable. A rented-GPU
endpoint declaring `qwen2.5-coder-7b` was reported as costing $0 *and* credited
with the full Opus baseline as savings. Pre-REQ the `(provider_id, model)` key
gave the honest answer (unpriced).

Those rows were also dead for their stated purpose: local turns are never
metered, because the cost meter is attached to the egress transport and only the
remote path constructs one.

### C3 — the migration's config rewrite could silently drop privacy boundaries

`migrate_and_report_provider_models` rewrote the user's config with
`std::fs::write`, which truncates on open — and REQ-557 turned that from a write
behind an explicit user action into an **unattended write on the first start
after upgrade, for every existing install**.

This is fail-*open*. Every `Config` field is `#[serde(default)]`, and `providers`
serializes before `boundaries`, so a partial write is very likely to be valid
TOML carrying the user's remote providers and none of their `local-only`
boundaries. Verified: a truncated prefix loads cleanly with `providers=1,
boundaries=0`. The daemon then starts, reports nothing, and routes remotely with
boundary enforcement silently gone — the outcome `load_config`'s
refusal-to-start exists to prevent, reached through a different door.

### Also fixed

- **AC-1's duplicate-id clause had no implementation.** `apply_update` is
  replace-or-insert, so `teton provider add opus --model claude-sonnet-5`
  silently overwrote the Opus entry — the exact command BR-3's headline invites
  running twice.
- **Classifier drift on a blank model.** `unusable_providers()` trimmed;
  `build_router` matched `Some(_)`. A provider with `model = " "` was reported
  unusable *and* served turns with `"model": ""`.
- **`unserved_turn_error`'s unusable arm over-fired**, so any leftover
  unmigrated provider hijacked the message for unrelated causes.
- **`default_provider` was never migrated.** Every pre-REQ config arrived with it
  unset; on a machine with a local tier that is silent, not loud — freeform
  coding turns quietly went local instead of the remote they used to.
- **Re-registration wiped `capabilities`**, so a provider pinned to the degraded
  tool-calling tier came back Native.

## Root Cause

One shape, four times: **a rule enforced where it was convenient rather than
where the decision is made.**

The model requirement lived in the CLI, so the RPC bypassed it. The usability
screen lived in `build_router`'s map construction, so the two paths that read
config directly bypassed it. The "declares a model" predicate was written out at
three call sites, so one drifted. And `billing_model`'s deletion was verified by
mutating `billing_model`'s *call site*, which said nothing about the identical
fallback one layer below it.

The last is the general lesson and it is not new here — LESSON-483 was written
during REQ-557 about exactly this, one layer up, and was not then applied
downward.

## Fix

- `ModelProvider::declared_model()` / `is_unusable_for_lacking_a_model()` — one
  definition of "declares a model", trimming blanks, read by every caller.
- `Router::is_routable()` screens both config-read paths (`resolve_freeform`'s
  default, `fallback_for`'s fallback). An unusable default falls into the
  existing no-default branch rather than a new one.
- `run_one_attempt` returns `NoTierAvailable` instead of falling back to the id —
  now a backstop behind those screens.
- `apply_config_update` rejects a modelless remote registration. The rule sits
  at **registration**, not in `validate()`: loading must stay permissive so a
  pre-REQ config can boot far enough to migrate (ADR-E), but registering is a
  fresh action with no legacy to honour.
- The zero-priced local rows are gone, with a test forbidding any zero row.
- `write_config_atomically()` — temp file, `sync_all`, rename.
- `provider add` refuses an existing id, before the credential prompt.
- The unusable arm fires only when the unusable set is implicated (phase policy,
  the default, or nothing usable at all).
- The one-shot migration writes down the positional default it was already
  using, gated so it can only fire on a demonstrably pre-REQ config.

## Verification

960 tests passing, 34 targets, clippy clean. Eleven mutations run by hand, each
confirmed red then reverted:

| Mutation | Result |
|---|---|
| freeform default un-screened | red |
| policy fallback un-screened | red *(after adding a router-level test — see below)* |
| RPC registration accepts no model | red |
| `migrate_models` local guard removed | red *(was green before this fix)* |
| `build_router` local arm removed | red *(was green before this fix)* |
| `build_router` re-derives "declared" | red |
| a zero-priced row reintroduced | red |
| config written straight at the target | red |
| unusable arm fires unconditionally | red |
| `default_provider` migration removed | red |
| id-as-model fallback restored | **green — see below** |

Two results are worth recording rather than smoothing over.

**The backstop and the screens masked each other.** Restoring the id-as-model
fallback leaves the suite green, because the three screens make that line
unreachable. Conversely, un-screening the policy fallback *also* left it green,
because the backstop caught it. Two guards, each covered only by the other, and
each individually mutable without detection. Fixed by pinning the fallback screen
directly at the router (`a_fallback_to_an_unregistered_provider_is_not_taken`),
which fails on the screen alone regardless of the backstop. The backstop itself
is deliberately left without independent coverage: it is unreachable by
construction, and that is the claim, not an oversight.

**Atomicity needed a failure to be observable.** A successful atomic write is
byte-identical to a successful truncating one, so the mutation was green until a
test exercised the *failure* path — a read-only config directory, asserting the
original file survives intact with its boundary.
