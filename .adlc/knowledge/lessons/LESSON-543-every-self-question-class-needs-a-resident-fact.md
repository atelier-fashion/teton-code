---
id: LESSON-543
title: "A model answers 'can you do X?' from whatever is in front of it — every class of question a user asks about the product needs its own resident fact, and a full prompt budget is where that fact gets refused"
component: "daemon/harness"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["developer-experience", "correctness"]
tags: ["system-prompt", "self-config-guide", "capability-claim", "honesty", "hallucination", "skills", "claude-code", "prompt-budget", "headroom", "bug-160-shape", "dogfood"]
req: BUG-181
created: 2026-08-19
updated: 2026-08-19
---

## What Happened

A user launched `teton` beside the ADLC toolkit — seventeen Claude Code
skills under `~/.claude/skills`, a `~/.claude/CLAUDE.md` describing them —
and asked *"are you able to leverage the skills and framework available?"*
The model had just `read` those files. It said **yes**. The user typed
`/analyze` and the closed slash-command table correctly answered `unknown
command`. Two lines, one contradiction, and the product looked like it did
not know itself.

BUG-160 was the same shape for a different question: asked how to hook up an
external model, the agent searched the *user's repository* for Teton's own
configuration and invented answers, because the only knowledge source it
could reach was the repository. REQ-577 answered that with the bundled
self-configuration guide — a few resident lines of facts about Teton's
*setup* so the model stops hunting for them. It worked: setup questions are
answered from the guide. **Capability** questions had no such line. Nothing
in the system prompt said what the session's `/` commands are, that only the
user runs them, or that `.claude/` and `~/.claude` files are not loaded. So
the model answered a capability question the only way it could — from the
evidence on disk, which all pointed one way.

The fix was one 186-byte sentence. Landing it was the second half of the
lesson: the resident prompt had **1 byte** of headroom above the pinned
floor, every other line of the guide is held by whole-line or per-segment
assertions tuned by live A/B, and the budget's own docs say "a sentence added
here is paid for by shortening another one." The documented alternative —
move the test-only overhead assumption with its arithmetic re-stated, the
REQ-577 path — was the right one, and the floor-plus-ceiling design made that
a reviewed decision rather than a silent squeeze. But the byte had to be
found first: a prompt that is full does not say so until the next sentence
fails to fit.

## Lesson

**Enumerate the classes of question a user asks the product about itself,
and give each one a resident fact.** Setup ("how do I add a provider?") got
its fact in REQ-577. Capability ("can you run X?") got its fact in BUG-181.
Cost ("what did that turn cost?"), privacy ("did that leave the machine?")
and identity ("what model are you?") are the remaining classes — check each
has a sentence the model can answer from, or a tool it is told to reach for.
The tell is cheap and happens at dogfood time: if the model's answer to a
question *about Teton* is derived from the contents of the user's repository
or home directory, the prompt is missing that class's fact.

**A capability fact has to name the negative space, not only the roster.**
"The session's commands are the ones `/help` lists" is half of it; "nothing
is loaded from `.claude/` or `~/.claude`" is the half that stops a model
beside a skills tree from affirming it. Name both places another agent loads
capabilities from — naming one leaves the other to be affirmed — and say who
runs the commands (the user, never the model), so the model hands off instead
of pretending.

**Pin the fact so the next feature amends it instead of deleting it.** The
sentence BUG-181 added is one REQ-585 will have to re-word (skills *will* be
loaded and listed by `/help`). The test asserts the parts that survive that
amendment — the `/help` pointer, the two paths, "only the user runs" — and
asserts the "loads nothing from" phrase **separately**, so REQ-585's failure
names the phrase and the fix is a re-word, not a delete.

**Measure the prompt's headroom before writing the sentence.** A
floor-guarded budget (`MIN_PROMPT_HEADROOM_BYTES`) turns the last byte into a
decision, as designed; it does not tell you the last byte is already spent.
The throwaway measurement (build the widest prompt, subtract from the
assumption) costs two minutes and decides whether the fix is a sentence or a
sentence plus a reviewed ceiling change.

## Why It Matters

A product that misstates its own capabilities is trusted less than one that
lacks the capability. The user in this incident did not mind that Teton has
no skills yet; they minded being told it did. Every class of self-question
without a resident fact is a place the model will confabulate in whichever
direction the evidence on disk points — and the evidence on disk, in a
developer's home directory, is always another agent's capabilities.

## Applies When

Adding anything to a resident system prompt (check headroom first; name the
negative space; pin what the next feature must amend). Any dogfood session
where the model answers a question *about the product* from the user's files.
Designing a feature (REQ-585) that will change a fact the prompt already
states — find the pinning test and plan the amendment with the feature.
