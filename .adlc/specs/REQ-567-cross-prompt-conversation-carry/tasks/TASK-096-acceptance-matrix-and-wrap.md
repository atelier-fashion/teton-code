---
id: TASK-096
title: "Acceptance matrix: privacy, cache warmth, A/B, classifier pin, multi-client; spec flips"
status: complete
parent: REQ-567
created: 2026-08-10
updated: 2026-08-10
dependencies: [TASK-093, TASK-094, TASK-095]
repo: teton-code
---

## Description

The cross-cutting acceptance tests that span the seams the earlier tasks
built, plus the spec bookkeeping: privacy egress across prompts, prefix-
cache boundary warmth and A/B correctness, the classifier input bound,
multi-client continuity, the mutation check, budget/compaction across
prompts, and the requirement checkbox flips + dogfood follow-up note.

## Files to Create/Modify

- `crates/tetond/tests/conversation_carry.rs` (new; scripted-engine e2e in
  the `prefix_cache_session.rs` style) — AC-3 (budget compaction across
  prompts; post-compaction boundary is a `divergent: true` hit), AC-7
  (fixed-seed A/B: assembled contexts + outputs byte-identical, cache on
  vs off), AC-8 (well-behaved boundary emits `divergent: false` with
  `cached_tokens` equal to the full retained prior context; ledger rows
  match), AC-11 (classifier cap-site input is the prompt text and stays
  fixed while the conversation grows), AC-12 (cross-session isolation).
- `crates/tetond/tests/e2e/` (existing harness) — AC-2 egress-capture:
  prompt 1 reads `local-only` boundary content locally; prompt 2 routes
  remote; same `privacy_block`/reroute as same-turn inclusion; zero
  boundary bytes in any remote payload (`assert_no_boundary_bytes`). AC-9
  multi-client: client A prompts and disconnects (client B holds the
  daemon); client B's prompt on the same session carries A's conversation.
- `.adlc/specs/REQ-567-cross-prompt-conversation-carry/requirement.md` —
  flip BR/AC checkboxes as each lands; AC-10 note (the mutation check ran:
  TASK-093's carry test is red against pre-change dispatch).
- `docs/manual-verification.md` — REQ-567 section: the real-model recap
  dogfood (AC-1/AC-8 real-model leg), superseding the REQ-564 sign-off's
  boundary observations; procedure references the piped-driver pitfalls
  recorded there.

## Acceptance Criteria

- [x] Every REQ-567 AC (1-12) is covered by a named test or an explicit
  manual-verification entry; the mapping is listed in the test file's
  module doc.
- [x] AC-8's boundary-warmth assertion uses the real `PrefixCacheState`
  via `CachingScriptedEngine` (policy-pure seam, LESSON-499) — no
  reimplemented reuse rule.
- [x] AC-10: reverting the dispatch seeding (temporary local patch or
  `#[cfg]`-free assertion against the old construction) turns AC-1 and
  AC-8 tests red — documented in the test module doc as executed, with
  the observed failures.
- [x] `cargo test --workspace --no-fail-fast` green; workspace built
  before any targeted e2e run (stale-daemon pitfall).
- [x] Requirement checkboxes flipped; `updated` date bumped.

## Technical Notes

AC-8's "full retained prior context" is the resident length (prompt +
generated) per REQ-564's record semantics — assert via `cached_tokens`
equality, not approximate bounds. AC-9 rides the e2e harness's real socket
client (two connections, REQ-565 activity guards keep the daemon alive).
The egress-capture suite's secret-sentinel pattern is the acceptance bar
for AC-2 (conventions.md: code inspection is not acceptance for BR-1
claims).
