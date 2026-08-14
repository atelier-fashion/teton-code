<!--
Filename MUST be `LESSON-xxx-slug.md` (e.g., `LESSON-041-signed-url-ttl-mismatch.md`).
- `xxx` is the next available integer, zero-padded to 3 digits, unique across `.adlc/knowledge/lessons/`.
- `slug` is lowercase kebab-case, ≤6 words.
- Do NOT use date-prefixed names (`2026-MM-DD-…md`) or bare numeric prefixes (`034-…md`).
  Those are legacy schemes and are not valid for new lessons.
-->
---
id: LESSON-522
title: "An edit lands on an identity, not a position — and the fix pass is new code"
component: "daemon/config"
domain: "harness"
stack: ["rust"]
concerns: ["security", "reliability"]
tags: ["toml-edit", "array-diffing", "identity-keys", "verify-phase", "adversarial-review", "config"]
req: REQ-574
created: 2026-08-14
updated: 2026-08-14
---

## What Happened

REQ-574's verify phase found that wholesale array replacement in the new config
delta engine destroyed unknown keys inside `[[providers]]`, breaking BR-1. The
fix introduced element-wise diffing for arrays-of-tables — and the fix's
per-index branch matched document elements **by position only**, guarded by
nothing but a length check. Because BR-5 keeps the daemon blind to on-disk
drift, a user who hand-reordered `[[boundaries]]` or `[[providers]]`
mid-session would have a later daemon edit land on the same-*position*,
wrong-*entity* element: a privacy-boundary downgrade requested for `docs/**`
landed on `secrets/**`, and a rotated `auth_ref` bound the new keychain
reference to an untrusted mirror's endpoint. `Config::validate` accepted both.
The six-agent review of the original diff could not have seen it (the code did
not exist yet), and the fix's own tests passed — the defect was caught only by
the scoped re-verify's adversarial A/B probe, which diffed the *fix commits*
against their base and drove the reproduced scenarios end-to-end through the
real RPC writers.

## Lesson

Two halves, one incident:

1. **A positional index is not an identity.** An edit computed against one
   snapshot of an array and applied to another targets *the entity the caller
   named*, so the application site must re-establish identity before writing —
   here, by requiring the document element to agree with the expected element
   on the array's natural key (`id`, `path_glob`, `tier`, `name`), falling back
   to wholesale replacement on any mismatch. When identity cannot be
   established, destroying formatting (wholesale) is strictly safer than
   guessing an entity.
2. **A verify-phase fix is new code and gets the full adversarial treatment.**
   The confirmation loop must review the *fix diff* with the same hostility as
   the original diff — A/B against the pre-fix base, driven through production
   entry points — because a fix written under review pressure is exactly as
   likely to carry a fresh defect as the code it repairs, and its tests were
   written by the same hand at the same moment.

## Why It Matters

The two arrays this bug touched are the ones carrying the product's core
promises: `[[boundaries]]` is the privacy guarantee and `[[providers]]` holds
credential bindings. A validator-passing write that silently moves either onto
the wrong entity is a privilege inversion, not a formatting bug. And the
process half generalizes: every fix loop that ends at "the fixes are in and
tests are green" without re-probing the fixes themselves leaves the newest,
least-reviewed code in the tree as the most trusted.
