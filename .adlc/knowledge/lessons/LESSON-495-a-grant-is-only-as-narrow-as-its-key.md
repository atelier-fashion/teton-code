---
id: LESSON-495
title: "A remembered grant answers every question its key matches — so the key must encode the whole question"
component: "daemon/harness"
domain: "harness"
stack: ["rust", "daemon", "cli"]
concerns: ["security", "developer-experience"]
tags: ["permissions", "consent", "session-grants", "tiers", "persistence"]
req: REQ-563
created: 2026-08-09
updated: 2026-08-09
---

## What Happened

REQ-563 graded web lookup into three separately-consented tiers — fetch a URL
the *user* pasted, fetch a URL the *model* composed, free-text search — with
BR-3 stating plainly that "a grant at a lower tier never implies a higher
tier." The implementation then keyed permission grants on the tool's *kind*:
`web_fetch` for both fetch tiers, `web_search` for search. The module header
even argued for per-tier keying while the code shared one key across the two
fetch tiers.

Consequence: the user pastes a link, is asked "web fetch https://docs.rs/tokio
(host docs.rs)", answers *allow for this session* — and every model-composed
fetch to any host runs unprompted for the rest of the session. The user
consented to fetching **their own link** and silently authorized **the
model's** choices.

The same defect then reappeared one layer down, in the fix. Making consent
durable added `[web] permission = "allow"`, which the gate fanned out to all
three keys — so one *enable permanently* answered at a pasted-URL prompt
permanently un-asked search too. The narrow in-session behavior was correct
while the durable projection of it was wide, and the widening was invisible
until the next daemon session.

## Lesson

**A remembered answer is not attached to the question that produced it; it is
attached to its key.** Every later request whose key matches inherits that
answer, whether or not a human would call it the same question. So the key
must carry every dimension the user was actually deciding about — here the
*tier*, which is the whole point of grading the capability, not just the tool.

Two corollaries that the second half of this bug makes concrete:

1. **Check the durable form separately from the session form.** They are
   different code paths and can disagree about breadth. A test that only
   exercises the in-session grant proves nothing about what a restart honors.
2. **The consent label must name what is actually written.** REQ-563's
   `enable_permanent` promised `[web] tier = "…"` while writing a different
   key entirely — a prompt describing a write that provably could not happen.
   If the label and the effect are authored in different places, they drift;
   derive the label from the effect, and pin both with one test.

## How to Apply

- When adding a permission prompt, write down the sentence the user is
  answering, then check the grant key contains every noun in it. "Allow web
  fetch?" and "Allow the model to choose any URL?" are different questions and
  need different keys.
- For any escalating capability (tiers, scopes, levels), make the key a
  function of the level — `permission_key_for(tier)` — so adding a level is a
  compile error rather than a silent grant.
- Store durable consent in the same shape as the question: a per-tier list,
  not a boolean that fans out.
- Test the mixed case explicitly: grant at the low tier, then request the high
  tier, and assert a prompt still appears. A test that grants and re-requests
  at the *same* tier cannot see this class of bug.
