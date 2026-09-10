---
id: ASSUME-049
title: "The held-turn sentence is composed by the client, and one composition can serve both surfaces that render it"
status: unresolved
req: REQ-621
created: 2026-09-10
resolved:
---

## Assumption

That the sentence naming a held turn — the model it is waiting on and which of
its two transient states that model is in — is the **client's** to compose, and
that composing it **once** (`session_ui::tier_warming_clause`) is enough for
both surfaces that render it: REQ-580's durable `turn_queued` notice, and
REQ-621's activity row.

Explicitly **not** assumed: that the two sentences must be identical. They are
two presentations of one clause and their lead-ins differ on purpose — the
notice announces a queued message once ("message queued until X finishes
loading — it will run as soon as the local tier opens."), the row says what the
turn is doing right now ("held until X finishes loading"). What is assumed is
that the *classification* and the *name* have exactly one author.

## Context

REQ-621's BR-2 as first written said the row fills the held detail with "the
sentence the daemon wrote from `turn_queued`", and its entity table said "the
daemon's own sentence". No daemon writes one. `TurnQueued` carries
`turn_id`, `model_id`, and `waiting_on: TierWarming` — a typed enum with two
variants — and every word around those values is composed on the client side.
The spec's own rule for the row is that detail comes only from event payloads
and is "never composed by the client", so the held case was the one place where
the rule as written could not be followed, because there was no sentence to
carry.

At verify this was resolved in the direction BR-10 points ("no second
renderer"): the branch on `waiting_on` moved into one function in
`session_ui`, and both the notice and `activity::held_clause` compose from it.
The alternative — having the row print the notice's whole sentence — was
rejected on two counts: it is ~100 columns, so an 80-column terminal would
truncate away the elapsed counters that are the row's reason for existing; and
it would restate a durable line the reader has already seen directly above,
which is the second-renderer failure BR-10 names.

**Why this is an assumption and not a design note.** It assumes the daemon will
not later grow a reason to send prose for this event. If it does — a
localization pass, a warming state whose explanation depends on facts the
client cannot see (a queue position, a download that is being resumed) — the
honest shape is the daemon's sentence rendered verbatim by both surfaces, and
this function becomes the fallback for a daemon that sent none. The
alternative failure is the one this record is a pin against: a third client-side
sentence for the same event, composed by whoever needs one next.

## Resolution

Unresolved at ship. **How to settle it:**

- **A second wire field arrives** (a `message`, a `detail`, a queue position on
  `turn_queued`) — the assumption is invalidated in the good direction. Render
  the daemon's text in both places and keep `tier_warming_clause` only as the
  fallback for an older daemon, which is the shape REQ-580's own notices take
  for events that carry prose.
- **A third surface needs the sentence** — the VS Code extension's status bar
  is the obvious candidate. If it can call this function, the assumption is
  holding. If it cannot (a different process, a different language), that is the
  evidence that the composition belongs on the wire after all, and it should be
  raised as a protocol REQ rather than solved by a fourth copy.
- **The two sentences are found disagreeing** in a transcript — a model name
  formatted one way in the notice and another in the row, or a warming state
  named differently — the assumption failed and the function was bypassed.
  Grep for `TierWarming::` in the client's production code; at the time of
  writing the only match is inside `tier_warming_clause` itself, and every
  other match in the crate is a test fixture.

Do not read "no bug reports about the held wording" as validation. Held turns
need a warming local tier to occur at all, so the population that sees either
sentence is small, and silence from it says nothing about whether the two agree.
