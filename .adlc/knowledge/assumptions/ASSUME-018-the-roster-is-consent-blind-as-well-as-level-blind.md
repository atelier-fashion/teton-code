---
id: ASSUME-018
title: "The roster's level-blindness was reasoned about; its consent-blindness was not"
status: unresolved
req: REQ-587
created: 2026-08-22
resolved:
---

## Assumption

BR-2 makes the `skill` tool's roster **level-blind** on purpose: a project skill
that `plan` would refuse stays listed, because the roster changes only with the
registry and a level-varying roster would churn the prompt prefix on every
`/permissions`. The spec argues that trade-off explicitly and at length.

What it never states is that the same design makes the roster **consent-blind**.
`model_invocable(registry)` filters on `invocable_by_model()` with no source
filter, and `SkillTool::new` renders the roster into `description` at
construction — so up to `ROSTER_MAX_BYTES` (512) of repository-authored,
attacker-chosen `[a-z0-9_-]` tokens enter the resident tool docs on **every
turn**, un-enveloped, *before* any project-skill acknowledgment is raised and
regardless of permission level.

## Context

REQ-587's whole consent story is that repository content reaches the model
labelled *instructions* only after a human acknowledges the root. The roster is
the one channel where repository-authored bytes reach the model with no
acknowledgment at all — as harness prose, in the system prompt, on every turn.

The bytes are only *names*, constrained to `^[a-z0-9][a-z0-9_-]{0,63}$` and
bounded at 512 total. But names are chosen by whoever committed them, and
hyphen-joined lowercase phrases read as vocabulary: a repo naming its skills
`ignore-previous-instructions` or `always-approve-shell` puts those words in
front of the model every turn, and weak local-tier models are exactly the
population LESSON-532 says act on framing rather than content.

Raised by REQ-587's Phase 5 security review, which classified it Medium and
noted it is a **design consequence rather than a deviation** — BR-2 specifies
this behaviour. It is logged here because the requirement reasons about the
level dimension and is silent on the consent dimension, so a future change to
BR-2 would be made without knowing this was ever considered.

## Resolution

Unresolved, and deliberately so for v1 — closing it means either excluding
un-acknowledged project names from the roster (which makes the roster change
with the acknowledgment as well as the registry; both are already prefix-cache
events, so the churn argument does not obviously forbid it) or rendering project
entries under a distinguishing prefix so the model cannot read them as harness
vocabulary.

Revisit alongside durable project-skill trust, which is Deferred in the same
spec: a trust decision that persists gives the roster a natural place to become
acknowledgment-aware without per-turn churn.
