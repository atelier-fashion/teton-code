---
id: LESSON-496
title: "\"Cut first under pressure\" means \"never available\" when the limit equals the count"
component: "daemon/harness"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["developer-experience", "extensibility"]
tags: ["tool-registry", "max-tools", "degraded-profile", "ordering-policy", "silent-absence"]
req: REQ-563
created: 2026-08-09
updated: 2026-08-09
---

## What Happened

The charter's BR-6 gives weak tool-calling providers a reduced harness: a
`max_tools` cap trims the exposed tool list. REQ-563 therefore registered its
new `web` tool **last**, so that under pressure it would be "cut first" — a
sound-sounding ordering policy, recorded in the architecture and implemented
faithfully.

Two constants, defined independently, then met: `DEGRADED_MAX_TOOLS` is 5, and
`ToolRegistry::with_builtins()` registers exactly 5 tools. The gap between the
limit and the count was **zero**. So "cut first under pressure" was in practice
"never exposed, ever" — for every provider classified degraded, which is every
OpenAI-compatible provider *and the local engine the product is built around*.

A user on Teton's own flagship local setup could set `[web] tier = "search"`,
answer the consent prompt, and then watch the model search the repo anyway.
Nothing was misconfigured; the capability simply never reached the model.
Worse, the "web lookup is off, name the opt-in" prompt clause keyed on
*registration*, not *exposure*, so the model got neither the tool nor the
sentence explaining its absence — the feature was invisible from both sides.

## Lesson

**An ordering policy is only as meaningful as the gap between the limit and
the count.** "Registered last so it is dropped first" silently becomes "never
present" the moment `limit == count`, and nothing in the code says so: the two
numbers live in different crates, were chosen for different reasons, and their
coincidence is invisible at both definition sites.

Two habits fall out:

1. **When a policy is expressed as an ordering against a cap, assert the
   headroom.** A test (or a `const` assertion) that the cap exceeds the
   mandatory set by at least one turns an invisible coincidence into a build
   failure. Absent that, the policy's meaning depends on an arithmetic
   accident.
2. **A capability the user explicitly enabled should not be silently
   withheld.** Whatever the resolution — exempt it, raise the cap, or refuse
   to enable it — the one unacceptable outcome is the state REQ-563 shipped
   into review: opted in, consented, and inert with no signal. If a policy can
   suppress an explicit user choice, the suppression needs a voice.

REQ-563 resolved it by making the opted-in tool **cap-exempt**: the cap bounds
only non-exempt tools, so the mandatory five are never displaced and the
budget grows by exactly one. That collapsed the signalling machinery built to
describe the old behavior — the honest sign that the underlying condition was
the bug, not the reporting of it.

## How to Apply

- Any time a limit trims a list, write down `limit - mandatory_count` and pin
  it. If it can be zero, decide *now* whether that means "degrade" or "never".
- Prefer an explicit exemption to a lucky ordering: "this tool is exempt
  because the user turned it on" is a rule a reader can check, while "this
  tool is registered last" is a rule whose effect depends on a number
  elsewhere.
- When you find yourself building UI to explain why a feature is unavailable,
  ask whether the unavailability itself is the defect. Signalling an accident
  is not the same as fixing it.
