---
id: ASSUME-048
title: "A tool description steers the model often enough to be worth 408 prompt bytes, but it does not bind"
status: unresolved
req: REQ-620
created: 2026-09-09
resolved:
---

## Assumption

That `SHELL_REACH_CONTRACT` — the paragraph BR-7 puts in the `shell` tool's
description, telling the model which command shapes keep a session on its
current tier and which pin it to the local one — changes what the model writes
often enough to justify 408 bytes of resident system prompt on every remote
turn, and a whole-KiB raise of `REDACT_BODY_OVERHEAD_BYTES` to pay for them.

Explicitly **not** assumed: that it changes what the model writes *always*. A
model that has read the contract still writes `2>/dev/null` from habit, the way
a person who knows a shortcut still types the long form.

## Context

REQ-620 has two halves and they are not equal partners.

BR-1 and BR-4 are the **load-bearing** half: the classifier learns that a
redirect to `/dev/null` and a descriptor duplication read nothing, and that a
content verb in a piped segment reads its stdin rather than the root. Those are
proofs about command text, checked before the command runs, and they hold
whatever the model writes.

BR-7 — this paragraph — is the **best-effort** half. It cannot be enforced, it
is not consulted by any gate, and nothing downstream depends on the model having
read it. If the description alone were enough, BR-1 would be unnecessary; the
requirement's own assumptions section says so in as many words, and this record
is the pin for it.

The reason it is worth paying for anyway is that the two halves fail
differently. BR-1 makes the pin *proportionate* — the shapes that cannot leak
stop pinning. BR-7 attacks the shapes that legitimately still pin: a model that
knows `> out.txt` costs the session its remote tier can reach for `2>/dev/null`
instead, and a model that knows only the user can lift a pin can say so rather
than trying `/shell allow` itself. Neither of those is reachable from the
grammar.

What the bytes cost, precisely: 407 for the paragraph plus the space joining it
to REQ-615's cwd contract. Against the 105 bytes of margin REQ-620 inherited,
that is 303 over, so `REDACT_BODY_OVERHEAD_BYTES` went 23 → 24 KiB — the raise
ASSUME-043's resolution predicted the next claimant would have to make — and
every `[privacy] redact = true` route's byte budget fell 931 bytes with it
(`REDACT_SCANNABLE_CONTEXT_BYTES` 184,265 → 183,334). That is a real,
measurable cost paid for an unmeasured behavioural benefit, which is why this is
an assumption record and not a design note.

## Resolution

Unresolved at ship. **How to validate it, after the release is in transcripts:**

Count `session_pinned` events whose `cause` is `unknown_shell` and whose
`reason` names a syntax class the contract explicitly warns about — a redirect
other than to `/dev/null`, a quote, a glob, a variable, a `~/` path — as a
share of remote agentic turns, and compare the rate before and after this
release. `reason` is content-free by construction (REQ-620 BR-6), so the count
is available without reading a single command.

Three readings, decided in advance so the number is not interpreted after the
fact:

- **A clear fall** in the warned-about classes, with the unwarned ones flat:
  the paragraph steers. Validated; leave it, and the next REQ that wants prompt
  bytes knows this one earns its keep.
- **No change**, with the classes that BR-1 now models having dropped out
  anyway: the grammar did all the work and the prose did none. Invalidated —
  and the remedy is to delete the paragraph and give the 408 bytes back to the
  margin, not to reword it, because a reword is another unmeasured claim on the
  same budget.
- **A rise in a class the contract does not name**: the paragraph is teaching
  the model to route around the classes it lists and into one it does not,
  which is worse than silence. That is a bug in the contract's coverage, and
  the fix is a class added to the paragraph or to the grammar — decided by
  which one can prove the shape reads nothing.

Do not read a fall in `unknown_shell` pins *overall* as evidence for this
assumption. BR-1 and BR-4 move that number on their own, and attributing their
effect to the prose is exactly the mistake this record exists to prevent.
