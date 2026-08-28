---
id: BUG-205
title: "provider add refuses a cleartext LAN registration and names a remedy the command cannot reach"
status: open
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

(filled after fix)

## Files Changed

- `crates/teton-protocol/src/methods.rs` — `ProviderConfig` gains the field (option (a)).
- `crates/teton/src/main.rs` — the `provider add` flag and its plumbing.
- `crates/tetond/src/runtime.rs` — `apply_update` takes the supplied value where present, still preserving the stored one when absent (the BUG-155 rule must survive: an omitted flag on re-registration must not clear a set one).
- `crates/teton-core/src/config.rs` — the refusal message gains the command-level remedy.

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
