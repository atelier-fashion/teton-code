---
id: BUG-205
title: "provider add refuses a cleartext LAN registration and names a remedy the command cannot reach"
status: in-review
severity: medium
created: 2026-08-28
updated: 2026-08-28
component: "cli"
domain: "providers"
stack: ["rust", "cli", "keychain"]
concerns: ["developer-experience", "security"]
tags: ["provider-add", "allow-cleartext", "bug-202", "dead-end", "keychain", "lan", "self-hosted"]
---

## Description

BUG-202 made `Config::validate()` refuse a provider that pairs an `auth_ref`
with a cleartext `http://` endpoint on a non-loopback host, and gave the rule an
escape hatch — `allow_cleartext = true` — which the refusal message names.

`teton provider add` cannot set that field. The protocol's `ProviderConfig`
carries no such member, and `apply_update` derives it from the *existing*
record, which for a first registration is `false`. So the guided flow refuses a
self-hosted LAN registration and points at a remedy that the command it just
refused has no way to apply.

This is the residual named in BUG-202's own Follow-up section, filed properly
rather than left as prose.

## Why it is worse than a papercut

`provider add` is the **only** command that puts a key in the OS keychain.
`ProviderAction` has exactly three variants — `Add`, `List`, `Test` — and the
single `Keychain::store` call site lives inside the add flow
(`crates/teton/src/main.rs:3695`). There is no `provider key` or equivalent.

So the full manual path for a user with a self-hosted model server on
`http://10.0.1.50:8000` is:

1. `teton provider add …` — refused at preview. No key stored (correct: BUG-171's
   `PriorKey::undo` behaviour is intact, and the refusal lands before the store).
2. Hand-edit `config.toml` to add the provider row **with**
   `allow_cleartext = true`.
3. Get the credential into the keychain **by hand** — `security
   add-generic-password -s teton -a <id> -w` on macOS — because no Teton command
   will do it now that step 1 is closed to them.

Step 3 is OS-specific, undocumented in the refusal, and the kind of instruction
that sends people to the other `auth_ref` form instead: `env:MY_KEY`, which
needs no keychain at all. That is the form REQ-596 exists to fix, because the
`shell` tool's environment scrub is a name denylist that does not consult
configured `env:` auth_refs. **The path of least resistance out of this bug
leads directly into REQ-596's hazard**, which is the strongest argument for
closing it rather than documenting it.

## Reproduction Steps

1. Have a self-hosted OpenAI-compatible server on a LAN address that is not
   loopback — e.g. `http://10.0.1.50:8000/v1/chat/completions` — with a token.
2. Run:
   ```
   teton provider add lan --kind openai-compatible \
     --endpoint http://10.0.1.50:8000/v1/chat/completions --model local-70b
   ```
3. The preview is refused with `PROVIDER_SETUP_INVALID`, naming
   `allow_cleartext = true` as the remedy.
4. Try to apply that remedy with any `teton` command. There is none.

## Expected Behavior

The remedy a refusal names is reachable from the command that produced the
refusal. Concretely, one of:

- **(a)** `teton provider add --allow-cleartext` — an explicit flag, carried on
  `ProviderConfig` through `config/set`, that sets the field for this
  registration. Keeps the decision explicit and in the user's hands, which is
  the property BUG-202 chose the flag for in the first place.
- **(b)** The refusal is re-offered as a consent prompt in the guided flow —
  "this credential would travel to 10.0.1.50 in the clear; register anyway?" —
  writing `allow_cleartext = true` on an affirmative answer. Richer, and closer
  to how the daemon handles other consequential choices, but a bigger change and
  it must not be answerable by a headless process (BR-10(b) class).

Either way the refusal message should name the *command-level* remedy, not only
the config-file field.

## Actual Behavior

The refusal names a config-file field. Applying it requires hand-editing
`config.toml` and hand-storing a keychain entry with an OS-specific command that
Teton never mentions.

## Environment

- Platform: all
- Version: introduced on `main` at `0b8c1c7` (BUG-202, PR #226). **Not yet in a
  tagged release** — newest tag is v0.1.26 — so no released build is affected
  yet, and fixing this before the next release means users never meet the dead
  end.

## Root Cause

BUG-202 added the escape hatch at the layer the rule lives at — the config
document — and did not carry it up to the surface that writes config documents
on a user's behalf. The security fix and the flow that exercises it were
considered separately; the follow-up was named in the bug report but not filed
until now.

This is LESSON-578's own lesson pointing back at its author: a rule belongs to
the artifact it constrains, **and** every door that writes that artifact needs a
way to satisfy it. BUG-202 fixed the first half.

## Resolution

Option **(a)** — the explicit flag. `teton provider add --allow-cleartext`
registers a provider whose endpoint is cleartext on a non-loopback host, writing
`allow_cleartext = true` on its row. Option (b), a consent prompt in the guided
wizard, was **not** taken: a prompt there must not be answerable by a headless
process (the BR-10(b) class), which is a design question of its own rather than
part of closing this dead end.

The refusal message now names the command, not only the config field — that was
the actual defect. A remedy expressed as "hand-edit this TOML key" is not
reachable when `provider add` is the only command that stores a keychain entry.

**The merge is field-wise, and both halves are separately mutation-tested.**
`ProviderConfig::allow_cleartext` and `ProviderSetupCandidate::allow_cleartext`
are `Option<bool>` on the wire — `Some(v)` writes, `None` **preserves** — exactly
as the two window fields merge (REQ-586 ADR-7). The CLI's `bool` becomes
`then_some(true)`, so an untyped flag sends `None` and never `Some(false)`. That
distinction is the whole fix's risk: `Some(false)` compiles, registers
correctly, and then clears a hand-authored opt-out on the next
`provider add --model` — BUG-155's failure mode arriving through a new door.

`WindowFlags` became `RegistrationFlags` and gained the field, rather than
`provider_add_on` gaining an eighth parameter. That function already carries
`#[allow(clippy::too_many_arguments)]`, and widening it would deepen exactly the
debt REQ-598 exists to pay down. The type is confined to `main.rs`.

### Residual (stated, not hidden)

The guided `/provider setup` wizard still has no way to *set* the flag — it asks
no cleartext question, so its candidate sends `None`. It is no longer a dead end
(its refusal names a command that exists), but registering a cleartext LAN
provider through the wizard means dropping to `provider add --allow-cleartext`.
The wire field is in place for the wizard to learn the question later; that is
option (b), still open.

### Verification

- `cargo test --workspace --no-fail-fast`: **3,999 passed, 0 failed, 1 ignored**
  across 69 targets; output grepped for `FAILED` (0).
- `cargo clippy --workspace --all-targets` clean (0 warning/error lines);
  `cargo fmt --check` clean.
- `teton provider add --help` renders the flag.
- **Mutation A (run):** reverting the daemon merge to the stored value alone —
  what BUG-202 shipped — makes the flag inert; part 1 of the merge test fails.
- **Mutation B (run):** `unwrap_or(false)` clears a stored opt-out; part 2 fails
  with "an absent flag cleared a stored opt-out". Different assertion from A,
  which is why one test covers both halves without either implying the other.
- **Mutation C (run):** `then_some(true)` → `Some(...)` in the CLI makes an
  untyped flag send `Some(false)`; the payload test fails.
- Falsification in the merge test: with no opt-out ever supplied, the same
  endpoint is still refused — so the passing halves test the flag, not the
  endpoint.

## Files Changed

- `crates/teton-protocol/src/methods.rs` — `allow_cleartext: Option<bool>` on both `ProviderConfig` and `ProviderSetupCandidate`; wire-additive, so neither `PROTOCOL_VERSION` nor `PROTOCOL_VERSION_MIN` moves.
- `crates/teton/src/main.rs` — `--allow-cleartext`; `WindowFlags` → `RegistrationFlags`; the `then_some(true)` widening; two tests.
- `crates/teton/src/provider_setup_ui.rs` — the wizard candidate sends `None`, with the residual stated at the call site.
- `crates/tetond/src/runtime.rs` — field-wise merge in `apply_update`; preserve on a field-wise remedy; populate on snapshot; the merge test.
- `crates/teton-core/src/config.rs` — the refusal names the command; the message test pins it.

## Fix Notes

- The preservation rule is the trap. `apply_update` currently derives
  `allow_cleartext` from the existing record precisely so an unrelated `--model`
  re-registration cannot clear it (BUG-155's class, and it has a mutation test).
  A supplied flag must **override** while an absent flag still **preserves** —
  `Option<bool>`, not `bool`, exactly as `max_context` and `context_budget_cap`
  merge field-wise per REQ-586 ADR-7. A plain `bool` on the wire would reintroduce
  the clearing bug the existing test guards against, and that test would catch it.
- Whichever option is taken, add a test that the refusal message names a remedy
  the CLI can actually perform — a message-content assertion is what keeps this
  bug from recurring in a new spelling.
