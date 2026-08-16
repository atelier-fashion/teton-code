---
id: REQ-579
title: "Guided in-session provider setup: `/provider setup` collects, the daemon commits, the model hands off"
status: approved
deployable: true
created: 2026-08-15
updated: 2026-08-16
component: "cli"
domain: "providers"
stack: ["rust", "cli", "daemon", "json-rpc", "keychain", "llm-providers"]
concerns: ["developer-experience", "security", "routing"]
tags: ["provider-setup", "guided-enablement", "slash-command", "keychain", "policy-set-tier", "vendor-recipes", "presence-attestation", "web-setup"]
---

## Description

A user in a Teton session says *"set up Kimi for deep reasoning."* Today the
best the product can do is have the local model recite three shell commands —
`teton provider add …`, `teton policy set-tier think …` — for the user to leave
the session, run in another terminal, and come back. That is what REQ-577
bought: the model now *knows* the right commands. It cannot *run* them, and it
must not be the one collecting the API key.

This REQ closes the loop the way REQ-572 closed it for web lookup. It adds an
in-session guided walkthrough, `/provider setup [vendor]`, that asks the four
questions a provider registration needs — vendor, model, key, and which tier(s)
to route to it — reads the key echo-off into the OS keychain, previews the exact
change, and commits it through the daemon so the new provider is usable in the
same session with no restart. And it teaches the model to *hand off* to that
command instead of reciting configuration, exactly as it hands off to
`/web setup` today.

The user experience this is for:

```
› set up Kimi for deep reasoning
>> Registering a provider takes an API key, which never passes through me.
   Run `/provider setup kimi think` — it asks the four things it needs, and
   I'll pick up from there.
› /provider setup kimi think
   vendor:    Moonshot (Kimi) — https://api.moonshot.ai/v1/chat/completions
   model:     [kimi-k3]
   api key:   ********                       (stored in the OS keychain)
   route:     think → kimi?  [Y/n]
   — preview —
   [[providers]]
   id = "kimi"
   kind = "openai-compatible"
   endpoint = "https://api.moonshot.ai/v1/chat/completions"
   model = "kimi-k3"
   auth_ref = "keychain://teton/kimi"

   [[tiers]]
   tier = "think"
   provider_id = "kimi"
   apply?  [y/N] y
   provider `kimi` registered; `think` now routes to it. Say the word.
```

**Provenance.** REQ-555 deferred in-session `/provider` ("stays shell-only in v1 … if promoted later") and REQ-577 deferred "executing provider setup on the user's behalf" as REQ-575/576 territory. This REQ is that promotion, now that the presence gate exists.

**Why now.** The pieces all exist and were each built for exactly this
composition: the guided-flow shape and its BR-6/BR-11 credential hygiene
(REQ-572), daemon-owned catalogs (REQ-573), presence-attested commitment
(REQ-575/576), the vendor recipe catalog with Kimi's real endpoint and model
(REQ-577), base-URL composition and echo-before-key-prompt (REQ-578), and the
shared keychain undo machinery (BUG-171). What is missing is the wiring — and a
product that knows the answer but makes the user go type it elsewhere is the
exact gap the user hit.

**Why the model does not ask for the key.** Not because the local model is
untrusted, but because the transcript is *context*, and the whole point of this
setup is that context will shortly be routed to the very vendor whose key it
is. A key typed into chat lands in the outbound payload, session logs, and
compaction summaries. BR-7 forbids it, REQ-572 BR-6 already solved it for web:
collection is client-side and echo-off; the model learns only that a provider
named `kimi` now exists.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| ProviderSetupPlan (daemon → client) | `catalog` | `[ProviderRecipeEntry]` | the REQ-577 recipe catalog, typed, seam-pinned; never empty |
| | `existing_ids` | `[string]` | provider ids already registered, so the flow can offer replace/rotate vs new |
| | `tiers` | `[TierSummary]` | each routable tier with its current binding, so the routing question is asked against truth |
| ProviderRecipeEntry | `id_suggestion` | string | e.g. `kimi`; the default id offered |
| | `label` | string | e.g. `Moonshot (Kimi)` |
| | `kind` | enum `anthropic \| openai-compatible` | drives which fields are asked |
| | `endpoint` | url? | absent for `anthropic` (defaults), required otherwise; already the composed request URL |
| | `example_model` | string | offered as the default answer, labeled as an example |
| ProviderSetupCandidate (client → daemon, preview + commit) | `id` | string | non-empty; must be a valid provider id |
| | `kind` | enum | as above |
| | `endpoint` | url? | base URL accepted and composed per REQ-578; absolute URL is what is persisted |
| | `model` | string | required for remote kinds; never inferred from id (REQ-557 BR-1) |
| | `key_ref` | string | a keychain reference only (`keychain://teton/<id>` — the same account `teton provider add` uses) — never a key value |
| | `bindings` | `[{tier, provider_id}]` | zero or more tier→id bindings; `think` is the default offer |
| ProviderSetupPreview (daemon → client) | `toml` | string | the exact bytes that would land in config (providers table + any policy rows) |
| | `dial_host` | string | rendered by the *same* parser that dials (LESSON-529) |
| | `warnings` | `[string]` | e.g. "replaces existing provider `kimi`", "unpriced model — cost meter shows unpriced" |
| | `digest` | string | binds commit to this exact preview (REQ-572 BR-7) |
| ProviderSetupCommitResult | `applied` | bool | false when config was already exactly this |
| | `provider_id`, `bindings` | as above | what actually landed |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| `provider_setup_completed` | commit applied | `{provider_id, kind, model, bindings}` — never the key, never the endpoint's userinfo |
| `provider_setup_rejected_nonuser` | the setup **commit** arrives from a model tool call or a non-session connection (plan/preview refuse in-response only, BR-12) | `{method}` |
| existing `config_changed` / provider-registered events | as today via the config path | unchanged |

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| setup plan, setup preview (method names per OQ-1) | the session's own user connection; read-only; a model tool call or foreign connection gets a wire-code refusal in the response and no event |
| setup commit (method name per OQ-1) | the session's own user connection **and** presence-attested where the platform can attest (REQ-575/576 pattern); a model tool call or foreign connection gets `provider_setup_rejected_nonuser` in the response and as an event |
| Model | may *say* `/provider setup <vendor>`; may never invoke it, and never sees the key |

## Business Rules

- [ ] BR-1: **The session hands off; the model does not collect.** When a turn asks to add, connect, register, or route to a remote provider, and a guided flow exists, the *session's* answer names `/provider setup <vendor> [tier]` — by the model volunteering it (the resident guide says so first, ADR-3) **or, when the model recites `teton provider add …` / `teton policy set-tier …` instead, by the interactive surface appending one harness-voiced line naming the command** (ADR-9). The CLI recipes remain the *non-interactive* answer (BR-11). *(informed by REQ-572, REQ-577; amended after verification.md rounds 1–3 — the local model recited the CLI 9/9 across three guide revisions)*
- [ ] BR-2: **The key never enters the transcript, model context, config, logs, events, or cost ledger.** It is read echo-off by the client, stored in the OS keychain, and only the reference travels to the daemon. The model learns that a provider with a given id exists — nothing else. *(informed by REQ-572, REQ-563)*
- [ ] BR-3: **Collection at the edge, commitment at the core.** The client owns step state and asks the questions; the daemon is stateless across the flow and exposes `plan` / `preview` / `commit`. Preview returns the exact bytes; commit is keyed to that preview's digest and refuses if the candidate no longer matches. *(informed by REQ-572)*
- [ ] BR-4: **The recipe catalog is served to the client from the daemon, from the same typed source the model reads.** No second copy in the CLI; the entry the client renders is the entry the model would have named. If the catalog and the model's inline guide can drift, a contract test pins them. *(informed by REQ-573, REQ-577, LESSON-517)*
- [ ] BR-5: **Endpoint composition and validation are REQ-578's, reused — not mirrored.** A pasted base URL composes to the request URL; the composed absolute URL is echoed to the user *before* the key is asked; a URL shape the dial-time parser rejects is refused at the same seam. No CLI-side re-implementation of the predicate. *(informed by REQ-578, LESSON-528, LESSON-529)*
- [ ] BR-6: **The model is required, never inferred.** A remote provider candidate without a model is refused before the key prompt, with the recipe's `example_model` offered as the default answer and labeled as an example. *(informed by REQ-557)*
- [ ] BR-7: **Routing is part of the same flow, and its default is the tier the user asked for.** The command accepts an optional tier (`/provider setup kimi think`) so the model's hand-off can carry the user's stated intent; when present it is the default offer, otherwise `think` is (the "deep reasoning" front door). The flow then asks which tier(s) to bind and commits provider + bindings atomically. A user may decline all bindings and end with a registered-but-unrouted provider, and the flow says so plainly.
- [ ] BR-8: **Abort is safe and clean.** Cancel at any prompt writes nothing and stores no key. A refused preview stores no key. A refused commit runs the shared keychain undo (delete a fresh entry / restore a displaced one) and reports the outcome truthfully. A transport error on commit leaves the keychain untouched and tells the user how to verify. *(informed by REQ-572, BUG-171, LESSON-525)*
- [ ] BR-9: **What the user confirmed is what lands.** The preview shows the exact TOML for the provider table and any policy rows plus the keychain reference and the dial host; the confirm question is asked against that preview; the write is comment-preserving (REQ-574). *(informed by REQ-572, LESSON-529)*
- [ ] BR-10: **Live pickup, same session.** On commit the daemon re-derives routing from the new config; the committing session can route to the new provider on its next turn with no restart and no re-attach. *(informed by REQ-572)*
- [ ] BR-11: **Non-interactive degradation.** On a non-TTY surface, or when the slash command arrives from anything but typed input, the flow prints the exact CLI recipe (`teton provider add … --model …` and `teton policy set-tier …`) and exits without consuming stdin. *(informed by REQ-572, REQ-555, REQ-560)*
- [ ] BR-12: **User-only, presence-attested where possible.** All three setup methods refuse model tool calls and non-session connections with a distinct wire code *in the response*; only the commit refusal is additionally announced as an event, because a pre-authorization publish on a read-only method is attacker-paced. The commit sits behind the same presence gate as `web/setup_commit` and `config/set`, and degrades exactly as they do on a build without the presence feature — never more permissively. *(informed by REQ-570, REQ-575, REQ-576)*
- [ ] BR-13: **No egress during setup.** The flow collects, validates, and writes locally. It performs no connection test against the vendor. Verification is a subsequent, ordinary, consented turn — and the completion message says how to do one. *(informed by REQ-572, REQ-563)*
- [ ] BR-14: **Replacing an existing provider is explicit.** If the chosen id already exists, the flow says so, shows what would change, and asks; a key rotation restores the prior key on refusal. Silent replace-or-insert is the BUG-155 class and is not permitted here. *(informed by REQ-557, BUG-171)*
- [ ] BR-15: **Completion is announced, not just logged.** A successful commit emits `provider_setup_completed` to connected clients so the interactive surface can print the one-line "registered; `think` now routes to it" without polling.

## Acceptance Criteria

- [ ] AC-1: In an interactive (TTY, typed-input) session with no remote providers, "set up Kimi for deep reasoning" leaves the user looking at `/provider setup <vendor> [tier]` on screen before the next prompt — either in the model's reply, or as the harness line the surface appends when the reply recited `teton provider add` / `teton policy set-tier`. The line is (a) printed at most once per turn, (b) never printed on a non-TTY surface (BR-11 already prints the recipe there), (c) never printed for the user's own typed text or for `/help` output — only for the model's reply. Deterministic; covered by a unit test over the render seam and a pty walk if the harness supports a scripted reply. The model-volunteers half is recorded live (verification.md, three rounds: 0/9) and is not what this AC's pass depends on.
- [ ] AC-2: `/provider setup kimi` in a TTY session walks vendor → model (default `kimi-k3`, labeled example) → key (echo-off) → routing (default `think`) → preview → confirm, and on `y` the same session's next `think`-classified turn routes to `kimi`. No restart, no re-attach.
- [ ] AC-3: `/provider setup` with no vendor lists every vendor in the catalog and accepts a selection by number or id.
- [ ] AC-4: The key is absent from: the session transcript, every event payload, the daemon log, the cost ledger, and the written config. Asserted by inspection of the real artifacts, not by absence of error (LESSON-519). The config carries only `api_key = "keychain://…"`.
- [ ] AC-5: The entry the client renders for `kimi` and the one-line recipe the model's inline guide carries are both derived from the single typed catalog — a contract test enumerates the typed source and gates the prose copy against it in both directions (REQ-573 pattern), failing if either consumer drifts.
- [ ] AC-6: Pasting `https://api.moonshot.ai/v1` at the endpoint prompt echoes `https://api.moonshot.ai/v1/chat/completions` *before* the key prompt; the persisted endpoint is the composed absolute URL; a backslash-in-authority URL is refused at the same seam with the same message `teton provider add` gives.
- [ ] AC-7: Cancel at each of the five prompts leaves config byte-identical and the keychain without a `provider:kimi` entry — asserted by reading both artifacts.
- [ ] AC-8: A refused commit on a *fresh* key deletes the keychain entry; a refused commit on a *rotation* restores the prior value; both outcomes are reported in the surface text.
- [ ] AC-9: Piped stdin (`echo '/provider setup kimi' | teton`) prints the CLI recipe and exits 0 having consumed no further stdin.
- [ ] AC-10: A model tool call naming the setup-commit method, and a setup-commit from a second connection that did not open the session, are both refused with the `provider_setup_rejected_nonuser` wire code, and no config or keychain change occurs. (Method naming per OQ-1.)
- [ ] AC-11: On a presence-capable build, the setup-commit prompts for presence exactly as `web/setup_commit` does; with the existing presence-refusal test seam engaged (`TETON_PRESENCE_ACCEPT=fail`, REQ-575) the commit is refused and BR-8 cleanup runs and is asserted on the real keychain.
- [ ] AC-12: Choosing an id that already exists prints "replaces existing provider `kimi` (model `kimi-k2` → `kimi-k3`)" in the preview and requires the confirm; declining leaves the original intact.
- [ ] AC-13: Declining every tier binding registers the provider, prints that it is registered but unrouted, and names `teton policy show` and `/provider setup` as the ways to route it later.
- [ ] AC-14: `/help` lists `/provider setup` from the same command table that dispatches it (REQ-555 BR-7); no hand-maintained help text.

## External Dependencies

- None new. Reuses: OS keychain (existing), the REQ-577 recipe catalog, the REQ-578 endpoint composer, the REQ-574 config writer, the REQ-575/576 presence gate.

## Assumptions

- The REQ-577 recipe catalog is a suitable client-facing catalog as-is (fields: `id_suggestion`, `label`, `kind`, `endpoint`, `example_model`, `notes`). If a client needs a field the model does not (e.g. a "needs key?" flag for `ollama`), it is added to the typed source once and reaches both consumers.
- Presence attestation remains inert on the shipped (non-`--features presence`) build, as it is for `web/setup_commit` and `config/set` today; this REQ inherits that posture rather than fixing it, and says so. Closing that gap is REQ-576's follow-up thread, not this one.
- The model can be steered to hand off with a resident-prompt sentence of the same shape and size as the `/web setup` steer clause; prompt margins are thin (ASSUME-008), so the clause budget is a design constraint for `/architect`.
- `think` is the right default binding offer for the "deep reasoning" front door; other phrasings ("for builds", "for scanning") should map to their tiers, but that mapping is the model's job in the hand-off, not the flow's.
- Keychain availability posture is inherited from `teton provider add`: on a platform where no OS keychain is reachable, the flow does not invent a fallback store — it degrades to the BR-11 instructions, which name the existing `env:VAR` reference form for supplying the key out of band.
- ID allocated with remote verification (not degraded).

## Open Questions

- [ ] OQ-1: **Dedicated `provider/setup_*` trio, or ride `config/set`?** `RegisterProvider` and `SetTierBinding` already exist as `ConfigUpdate` variants behind `config/set` (REQ-576, presence-attested). REQ-572 chose a dedicated trio for web because plan/preview needed to return typed state and exact bytes; provider setup needs the same preview. Recommend the trio for the same reasons, with commit *composing* the existing `ConfigUpdate`s so there is one write path — but this is `/architect`'s call.
- [ ] OQ-2: **Should the flow offer a `--fallback` binding?** `set-tier` supports one. Adding a sixth question makes the walkthrough longer; leaving it out means the CLI can do something the flow cannot. Lean: leave it out of v1 and name it in the completion message.
- [ ] OQ-3: **Anthropic vendor path.** `kind = anthropic` needs no endpoint. Does the flow skip the endpoint prompt entirely, or show the default and accept enter? Lean: skip, and echo the default in the preview.
- [ ] OQ-4: **Does the steer clause name the vendor id?** "Run `/provider setup kimi`" is better UX than "run `/provider setup`", but requires the model to map "Kimi" → `kimi` from the inline guide. REQ-577's A/B showed the model answering front-door provider questions from the inline guide; whether it reliably emits the *id* is a fresh A/B question.

## Out of Scope

- A connection test against the vendor during setup (BR-13; verification is a consented turn).
- Editing or removing providers (`/provider remove`, `/provider edit`) — separate REQ; this one registers and routes.
- Category-level bindings (`set-category`) — tier bindings only in v1.
- Changing what the CLI `teton provider add` / `teton policy set-tier` do; they remain the scripted/non-TTY path.
- Fixing presence attestation's inert-on-release posture (REQ-576 thread).
- Web-lookup setup — already `/web setup`.
- Any change to what the model may do with the key: it may do nothing with it, before and after this REQ.

## Retrieved Context

Retrieval query: component `cli`, domain `providers`, stack `[rust, cli, daemon, json-rpc, keychain, llm-providers]`, concerns `[developer-experience, security, routing]`, tags `[provider-setup, guided-enablement, slash-command, keychain, policy-set-tier, vendor-recipes, presence-attestation, web-setup]`. Spec filter admitted `status: complete` (this repo's terminal status; the skill's `approved|in-progress|deployed` filter matches zero specs here — noted for the skill).

- LESSON-529 (lesson, score 11): A display helper is a second parser — render the host the request will reach
- REQ-560 (spec, score 11): Named permission levels and the interactive session status line
- REQ-578 (spec, score 8): Kind-aware endpoint composition at the provider registration seam
- LESSON-525 (lesson, score 8): Sweep every concern across every surface the REQ already enumerated
- BUG-171 (bug, score 8): A refused provider registration keeps the key it collected — and doesn't say so
- BUG-165 (bug, score 8): The search credential only speaks Bearer, and the spec's own example backends do not
- REQ-555 (spec, score 8): In-session slash commands for the teton interactive CLI
- REQ-556 (spec, score 8): Live model-loading progress in the interactive session
- REQ-557 (spec, score 8): Provider model identity and an explicit default provider
- REQ-563 (spec, score 8): Opt-in web lookup through the egress choke point
- REQ-570 (spec, score 8): Human-attested attach consent
- REQ-547 (spec, score 8): First-run local model consent
- LESSON-528 (lesson, score 7): A mirrored private predicate inherits the code but not the precondition
- LESSON-517 (lesson, score 7): A sanitizing seam owns the styling too
- LESSON-519 (lesson, score 7): An 'assert by inspection, not from the error' AC needs the real artifact

Grounding read outside the ranked list (direct precedents, ranked below the cut by tags): REQ-572 (the `/web setup` design this mirrors), REQ-577 (the recipe catalog), `crates/teton/src/web_setup_ui.rs` (the concrete flow).
