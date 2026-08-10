---
id: LESSON-501
title: "State carried past its creator's lifetime sheds invariants silently"
component: "daemon/session"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["privacy", "security", "reliability"]
tags: ["context-carry", "state-management", "provenance", "taint", "lifecycle", "commit-seam"]
req: REQ-567
created: 2026-08-10
updated: 2026-08-10
---

## What Happened

REQ-567 promoted the per-turn `ContextManager` into a per-session
conversation: blocks that used to die with the turn now commit to the session
registry and replay into every later prompt. The implementation was clean and
the whole acceptance matrix was green — and the verify panel still found four
defects that were all one defect:

1. **Provenance amnesia.** `truncate_to_budget` dropped tool blocks (with
   their `local-only` provenance) while the model's paraphrase of that
   content survived. Egress evaluated the surviving blocks, saw clean
   provenance, and would have shipped boundary-derived content remote on a
   later turn. Harmless before carry — the context died with the turn.
2. **Asymmetric taint evaluation.** The REQ-544 C-2 pin ran only in the
   success arm; the cancellation path (armed `Drop`) also committed, without
   it.
3. **Honesty-flag death.** The `truncated` bit lived on the manager, so the
   `[earlier conversation truncated]` note appeared on the turn that cut and
   silently vanished on every later replay.
4. **Re-derivation at a distance.** The cancellation trim re-parsed committed
   text to ask "is this a tool call?" — a fact only the *source* had known
   (remote calls are structural, never in text), so innocent JSON in remote
   prose got mutilated.

## Lesson

Extending a value's lifetime extends none of its invariants. Everything that
was true of the per-turn context was true because the turn's scope enforced
it — its egress checks ran on its blocks, its flags described its manager,
its parses happened where the format was known. The moment the value outlives
that scope, every invariant must either **travel with the value**
(`RetainedContext` carries blocks + truncated + dropped provenance) or be
**re-asserted at the commit seam** (`CarriedTurn::commit_now` runs the taint
pin and the budget clamp on every path that writes — success, abort, all of
them), and facts must be **recorded when known, never re-derived later**
(the pending-call flag is set by the source that parsed the text).

## Why It Matters

"Make X persistent" reads as a state-management refactor and reviews as one —
every reviewer of the original diff signed off on layering and atomicity. The
leaks lived in the *difference between lifetimes*: checks that ran per-turn
were suddenly guarding per-session data on only one of three exit paths.
When a change extends a lifetime, audit every invariant of the old scope by
asking three questions: does this flag describe the value or its old
container? does this check run on every path that now writes? is this fact
re-derived somewhere the knowledge no longer exists? A commit to longer-lived
storage is a security boundary and deserves a single seam that every path
shares.
