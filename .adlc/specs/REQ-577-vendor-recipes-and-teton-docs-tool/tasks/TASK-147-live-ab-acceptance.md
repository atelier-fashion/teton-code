---
id: TASK-147
title: "Live A/B acceptance against an isolated llama daemon"
status: draft
parent: REQ-577
created: 2026-08-14
updated: 2026-08-14
dependencies: ["TASK-146"]
repo: teton-code
---

## Description

Run spec AC-1/AC-2 live: against an isolated real-weights daemon, "I want to
hook up Kimi for deep reasoning" must yield the exact two commands (Moonshot
endpoint included) with zero repository-search tool calls (≤ 1 `teton_docs`
call permitted), and the control file-question must still call `read`.
Weights-gated: if the 17 GiB local weights are absent, record the run as
deferred manual verification — never claim it done.

## Files to Create/Modify

- `.adlc/specs/REQ-577-vendor-recipes-and-teton-docs-tool/verification.md` —
  new: the A/B transcript summary (pre-fix baseline vs post-fix, per trial),
  or, if weights are unavailable, the explicit deferred-manual record with
  the exact commands below and what to look for.

## Acceptance Criteria

- [ ] Weights availability checked first (weights dir present + symlinkable);
  the outcome path taken is stated plainly in verification.md.
- [ ] If run: AC-1 (Kimi request → exact `teton provider add kimi --kind
  openai-compatible --endpoint <catalog endpoint> --model <example>` +
  `teton policy set-tier think kimi`, zero repo-search calls, ≥ 3 trials)
  and AC-2 (Claude request → `--kind anthropic` recipe; control "What
  version is this crate? Check Cargo.toml." still calls `read`) both
  recorded with baseline comparison.
- [ ] If deferred: verification.md carries the full manual protocol and the
  REQ's acceptance state says "CI-verified; live A/B deferred (weights
  absent)" — the honest claim, per ethos #7.

## Technical Notes

- Isolation method (LESSON-482): release build `cargo build --release
  --workspace --features tetond/llama`; short `XDG_RUNTIME_DIR` (socket must
  fit SUN_LEN ~104 bytes); fresh base dir; symlink the weights dir so the
  second daemon shares the mmap'd inode.
- A/B discipline (BUG-168): baseline first, prompt as the only variable,
  byte-identical replies across trials are expected at temp 0.2.
- Repo memory: targeted e2e runs can test a stale daemon — build the
  workspace before any live run (BUG-164 rule).
