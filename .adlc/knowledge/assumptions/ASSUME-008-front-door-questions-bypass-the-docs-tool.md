---
id: ASSUME-008
title: "Front-door setup questions reach the docs tool when the guide thins"
status: invalidated
component: "tetond/harness"
domain: "harness"
made_during: REQ-577
observed_again_during: REQ-579
created: 2026-08-15
updated: 2026-08-16
---

## Assumption

REQ-577's spec assumed the local model will call `teton_docs` when the tool's
topic index names the subject, making the tool a safe "growth path" (BR-10):
knowledge moved out of the resident guide into a bundled topic would still be
reached on the question shapes that need it.

## Evidence So Far

Live A/B (verification.md, rounds 1–3, 2026-08-14): on the provider-setup
front-door shapes, the model answered from the resident guide's inline
recipes and called `teton_docs` in **0 of 11** candidate sessions — the
cheaper path won every time. Explicit probes ("what topics can teton_docs
show?", a web-setup question) did reach the tool and completed without a
prompt, so the tool *works when asked for*; what is unproven is that the
model reaches for it when the resident guide no longer holds the answer.

Live A/B (REQ-579 `verification.md`, TASK-158, 2026-08-15): the pattern
**held again, unchanged**. Across 5 candidate sessions on the REQ-579 build
(3 AC-1 rounds plus 2 diagnostics), every one a front-door provider-setup
question, `teton_docs` was called **0 times**. The model answered entirely
from the resident guide — the endpoints and model ids in its replies are the
inline recipes verbatim, not a topic read. The only tool call of any kind in
the whole candidate arm was a `shell` attempt to run `teton provider add`
itself. Second REQ, same direction: on these shapes the guide wins and the
tool is never reached.

REQ-579 also supplies the sharpest instance yet of *why* this matters. Its
AC-1 hand-off clause was added to the resident guide and still lost, 3/3, to
the numbered `teton provider add` procedure two lines below it — including on
a diagnostic that asked point-blank whether a slash command existed. A fact
that loses to a neighbouring procedure while resident in the same file would
not have fared better as a topic nobody opens.

**Round 2 of the same REQ (2026-08-15, restructured guide) flipped the
tool-reaching half and falsified a deeper one.** With the hand-off moved
inside step 1 and the CLI recipe demoted to a "Shell only:" clause, the model
called `teton_docs providers` on the *same front-door prompt*, 1×/round, 3/3,
completing with no permission prompt. So front-door shapes **can** reach the
tool; round 1's 0-call result was a property of that guide wording, not of the
shape — and this assumption's "will it reach the tool" question is answered
yes.

**Round 3 (2026-08-16) is this assumption's own experiment, run.** The
Implication below demanded that before any load-bearing fact is moved out of
`self_config.md` into a bundled topic, the front-door A/B be run against the
thinned guide. REQ-579 round 3 did exactly that: all six vendor recipes were
deleted from the guide (2,402 → 1,914 bytes), leaving a pointer to read
`teton_docs providers` instead, with a test gating the removal. The verdict is
**split, and the split is the whole finding**:

- **Data transfers.** The model called `teton_docs providers`, and the endpoint
  and example model in its answer were the catalog's exact values. No
  fabrication, no BUG-165 texture. Moving *facts* into a topic works.
- **Instructions do not.** The same fetched topic opens by naming
  `/provider setup <vendor> [tier]` as the answer, and the model ignored it
  3/3 and recited the CLI — as it had in round 2.
- **And the move has a cost the byte budget hides.** Turns went from 2 model
  calls to 4, tool round-trips from 1 to 2, and a denied `shell` permission
  prompt reappeared 3/3 (the model, missing the facts, first tried to *hunt*
  for them). 488 bytes of margin bought two extra prefills per turn.

So the assumption should be split in two going forward. "Reference data may
live in a topic" is **supported**. "Instructions may live in a topic" is
**falsified on this model**. And "topic content is free because it is not
resident" is **false** — it is paid per turn instead of per prompt.

The half that failed is the one nobody had thought to doubt. The
`providers` topic the model fetched opens with:

    # Connecting an external provider
    In a session it is one line the user types:
        /provider setup <vendor> [tier]

The model read that and then answered "Here are the exact commands you need to
run: `teton provider add …`", 3/3. **The topic was retrieved and disregarded.**
That is a strictly worse failure for BR-10's growth-path premise than the topic
going unread: an unread topic is a routing problem with obvious fixes, whereas
a read-and-ignored topic means the premise "knowledge in a topic is knowledge
the model will act on" does not hold on this model for instruction-shaped
content that competes with a familiar recipe.

## Status: resolved-with-a-split (was: open, partially invalidated)

After REQ-579's three rounds this is no longer one assumption. **Reference data
in a bundled topic: supported** — reached and used faithfully. **Instructions in
a bundled topic: falsified** — reached, read, and disregarded, twice
independently. **Topic content as "free" context: falsified** — it costs extra
model calls and tool round-trips per turn, which the prompt-byte floor does not
measure. The growth path in BR-10 survives for facts and does not survive for
behaviour.

The growth-path premise is unexercised in exactly the case it exists for.
The prompt margins are now thin — **72 bytes** on the opted-out shape and
**119** on the opted-in one, against a floor of 48 (the shipped guide is round
2's, 2,390 bytes; `crates/tetond/src/egress/redact.rs`) — so the pressure to
move guide content into topics is real and will arrive.

## Implication

Before moving any load-bearing fact out of `self_config.md` into a bundled
topic (the ADR-2 fallback posture), run the front-door A/B against the
thinned guide and prove the shapes still succeed via the tool. If they do
not, the fix is a prompt affordance (a dictated "for X, read `teton_docs`
topic Y" clause), not a bigger guide — and that clause needs its own live
verification (BUG-168's rule: rewordings are unverified until A/B'd).

REQ-579 sharpens the second half of that: a dictated clause is not
automatically enough either. Its hand-off clause was dictated, resident, and
unconditional, and the model still never emitted it. Placement relative to the
competing procedure looked like the lever — REQ-577's F-1 was only fixed by
putting the dictated sentence *inside* the numbered step it governed — so
REQ-579 round 2 did exactly that, and **it still failed 3/3**.

So the implication is now stronger than "run the A/B before thinning the
guide". It is: **for instruction-shaped content that competes with a recipe the
model already knows, presence in the context — resident guide, dictated
sentence, correct placement, competing recipe deleted, or fetched topic — has
been falsified five ways on this model, across three live A/B rounds, 0/9.**
Before relying on any of those, produce the behaviour deterministically from
the surface instead: REQ-579's non-TTY path already prints its hand-off from
code, with no model involvement, and that is the only place in three rounds
where a user was reliably told the command exists.

Concretely, for the next person here:

- **Do** move reference data (endpoints, model ids, tables) into a topic when
  the guide is tight. It transfers cleanly.
- **Do not** move an instruction you need obeyed into a topic, and do not
  expect a resident guide sentence to win either. Budget a live A/B if you try
  anyway, and budget for it to fail.
- **Do** price the move in *round-trips*, not bytes — round 3 doubled the model
  calls per turn.
- **Prefer** a deterministic surface affordance for anything that must happen
  every time.

**The last bullet was taken, not just written.** REQ-579 shipped it: ADR-9 /
TASK-159 append the `/provider setup` hand-off from the CLI surface when the
model's reply reaches for the shell recipe — deterministic, TTY-only, once per
turn, dormant if the model ever volunteers the command — and BR-1/AC-1 were
amended in the open to say that either half may satisfy the criterion
(`verification.md` §25). So this assumption now has a worked example of its own
recommendation, including the shape of the amendment that goes with it: the
0/9 stays on the record rather than being rescored.
