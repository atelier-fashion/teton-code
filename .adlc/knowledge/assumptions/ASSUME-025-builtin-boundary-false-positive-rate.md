---
id: ASSUME-025
title: "The builtin boundary set's false positives are rarer than the leaks it prevents"
status: unresolved
req: REQ-597
created: 2026-08-29
resolved:
---

## Assumption

REQ-597 turns thirteen credential-shaped globs on for everyone. Two of them —
`**/*.pem` and `**/*.key` — are broad enough to match files that are not
credentials at all: test fixtures, sample certificates, generated keys checked
into a repo on purpose, and (in some ecosystems) localization or data files
using a `.key` extension.

The assumption the REQ rests on, stated in its own words: *users would rather
have a false positive — a file they wanted to send is blocked, with a clear
message and an opt-out — than a silent credential leak.* This is the inverse of
the previous default and is the central judgment of the REQ.

Two quantities are assumed and neither is measured:

1. **Frequency.** That a session hitting a benign `*.pem` / `*.key` is rare
   enough that the block is a surprise rather than a routine obstacle.
2. **Escape quality.** That when it does happen, the user finds the way out.
   The remedy is named at two surfaces — the `boundary list` empty-state line
   and the CHANGELOG — but the *block* itself surfaces as a `privacy_block` and
   a reroute, and a user who has not read either may experience it only as
   "my turn went to the local model and got worse".

## Context

There is no telemetry, and the dogfood surface for this is a real repo with real
fixtures over real sessions. The acceptance tests prove the mechanism, not the
rate.

## What Would Invalidate It

- A dogfood session where a benign `*.key` or `*.pem` block is hit more than
  once, or where the reroute is noticed before the reason is.
- Any report of a user disabling the whole set (`disable_default_boundaries`)
  rather than adding a narrower rule — that is the signal that the escape hatch
  is being used as a sledgehammer because the targeted remedy was not
  discoverable.

## If Invalidated

The REQ's own Assumptions section names the remedy and it is deliberately not
"loosen the rule": *if measurement contradicts this, the list — not the rule —
is what changes.* Narrow `**/*.key` and `**/*.pem` (for instance to the
conventional credential directories) rather than weakening the composition, the
opt-out, or the fail-closed posture.

Related: REQ-597 OQ-5 records a second, larger source of false positives — the
unpinnable-provenance path, which now covers content the daemon cannot attribute
to any path at all.
