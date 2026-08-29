---
id: LESSON-583
title: "A refusal must name a remedy reachable from where the user is standing"
component: "cli"
domain: "providers"
stack: ["rust", "cli", "daemon"]
concerns: ["developer-experience", "security"]
tags: ["refusal", "remedy", "dead-end", "bug-202", "bug-205", "opt-out", "field-wise-merge", "option-bool"]
req: BUG-205
created: 2026-08-28
updated: 2026-08-28
---

## What Happened

BUG-202 added a fail-closed refusal — a provider credential beside a cleartext
`http://` endpoint — and gave it an escape hatch so no legitimate topology was
lost. The refusal message named the escape hatch: `allow_cleartext = true`. That
felt complete. A refusal that names its own way out is better than one that does
not, and the review said so.

It was still a dead end. The field could only be set by hand-editing
`config.toml`, and `teton provider add` is the **only** command that writes a
keychain entry — `ProviderAction` has three variants, `Add`, `List`, `Test`, and
one `Keychain::store` call site, inside the add flow. So refusing `provider add`
closed the only supported route to registering that provider at all. The full
manual path became: refused preview → hand-edit TOML → `security
add-generic-password` on macOS, an OS-specific command the product never
mentions.

The kicker is where that friction leads. The other `auth_ref` form, `env:MY_KEY`,
needs no keychain — so the path of least resistance out of the dead end runs
straight into REQ-596's hazard, where `shell`'s environment scrub is a name
denylist that never consults configured `env:` auth_refs.

## Lesson

**Naming a remedy is not the same as making one reachable.** Check that the
remedy can be performed *from the surface that produced the refusal*, with the
commands that surface has. A config-file field is a reachable remedy for someone
editing a config file; it is a dead end for someone running a CLI whose only
credential-writing command just refused them.

The test that keeps this from recurring in a new spelling is an assertion on the
**message text** — that it names a command the CLI can actually perform, not
merely a field that exists. Message-content assertions feel low-value until the
message is the entire interface between a refusal and a stuck user.

## Why It Matters

A fail-closed gate with an unreachable remedy is indistinguishable from a hard
block, and users route around hard blocks by whatever means remain — here, an
`env:` credential that a different subsystem leaks. **A security control that
pushes people toward a less safe configuration has negative value**, however
correct its own logic is.

## The other half: adding the field the remedy needs

Closing this meant making an existing field settable over the wire, and that
carried its own trap worth recording separately.

`ProviderConfig::allow_cleartext` is `Option<bool>`, not `bool`, and the CLI's
`bool` widens through `then_some(true)`. `Some(v)` writes; **`None` preserves
what is stored**. Sending `Some(false)` for an untyped flag would compile,
register correctly, pass every happy-path test — and then clear a hand-authored
opt-out on the next `provider add --model`, turning a working install into a
refusing one on a machine nobody meant to change.

That is BUG-155's failure mode arriving through a new door, one release after
the capability profile beside it was fixed for exactly the same reason. Three
mutations were needed to pin it, each failing a **different** assertion: the
flag inert, the absent flag clearing, and `Some(false)` reaching the wire. None
of the three implies the others, so a single happy-path test would have shipped
two of them.

## Applies When

- Writing any refusal, validation error, or consent denial that names a remedy —
  ask which surface the user is on and whether the remedy is performable there.
- Adding a fail-closed gate to a flow that is the sole path to some side effect
  (storing a credential, creating a record, provisioning a resource). Refusing
  the only door is a bigger change than refusing one of several.
- Making a previously internal field settable over an RPC that rebuilds records:
  use `Option<T>` with `None` = preserve, and never let a "not stated" client
  input become a stated default. See also [[LESSON-578]].
- Any time a security control has a legitimate exception — check where users go
  when the exception is hard to reach, and whether that destination is safer or
  worse than what you refused.
