---
id: LESSON-612
title: "An advisory about a withheld thing must key on the absence, never on the rule that would have caused it"
component: "daemon/tools"
domain: "developer-experience"
stack: ["rust", "daemon"]
concerns: ["developer-experience", "security", "observability"]
tags: ["diagnostics", "misattribution", "allowlist", "advisory", "bug-205", "req-596", "false-explanation"]
req: REQ-607
created: 2026-09-01
updated: 2026-09-01
---

## What Happened

REQ-596 gave the `shell` tool a twelve-name environment allowlist. `SSH_AUTH_SOCK`
is not on it, so `git push` over ssh fails inside a shell command — and it fails
saying *"Permission denied (publickey)"*, which names ssh and never names Teton.
REQ-607 added one sentence to the failing call to fix that.

The obvious implementation reads only static facts: *is this name on the
diagnosis table, and is it absent from the allowlist?* Both are compile-time
constants, so the sentence is cheap and always available.

It is also wrong on any machine with no `ssh-agent` running. There, `git push`
fails for a completely different reason, the daemon never had `SSH_AUTH_SOCK` to
withhold, and the advisory would confidently tell the user that Teton took
something away and offer a config key that would have changed nothing. A feature
written to remove a misattributed error would have shipped a new one, wearing the
daemon's own name for extra credibility.

The fix was to define "withheld" against the world rather than against the rules:
the variable is **present in the daemon's environment and absent from the composed
child**. Same table, one more question, and the sentence became true instead of
plausible.

Asking the composed environment also answered three cases nobody had enumerated.
With the opt-in on, the variable is present and nothing is said — so a user who
takes the remedy stops being told to take it. With `auth_ref = "env:SSH_AUTH_SOCK"`
configured, the credential removal wins and the advisory correctly *does* fire.
And a future table row needs no new logic at all.

The same defect then reappeared one layer up, which is the part worth
remembering. The sentence named `programs.first()` rather than the program that
matched the table, so `cd /repo && ssh host` would have read *"a problem with
`cd`"*. The test asserted `content.contains("Teton")` and was green throughout.

## Lesson

**Derive a diagnostic from the observed outcome, not from the policy that
usually produces it.** A message of the form "X is missing because we removed it"
is a claim about this machine, and only the machine can settle it. Reading the
rule instead is faster, always available, and silently false wherever the rule
did not actually bite.

Two corollaries, both paid for here:

- **The check that makes the message honest usually also makes it complete.**
  Every edge case above fell out of asking the composed environment; none of them
  had to be enumerated. A predicate that reads the world tends to cover the
  situations you did not think of, because the world already contains them.
- **`contains("<keyword>")` does not test a sentence.** It tests that a word is
  present. Assert what the sentence *claims* — that it names the program that
  matched, that it names the key that helps — or a message can go wrong in every
  way except the one word you grepped for.

## Why It Matters

A wrong explanation costs more than no explanation. A user with no message
investigates; a user handed a confident, branded, specific message stops
investigating and goes to change a config key that cannot help. That is BUG-205's
failure mode — a refusal naming a remedy no command can reach — and the cost is
not one confused session but the trust in every later advisory the daemon emits.
It is also self-concealing: on the machines where the static rule happens to be
right, the feature looks like it works.

## Applies When

Writing any user-facing message that explains an absence, a refusal, a removal, or
a degradation — "we withheld X", "this was filtered", "your value was dropped",
"the feature is off". Also when reviewing a diagnostic whose test asserts only
that some keyword appears in the output.
