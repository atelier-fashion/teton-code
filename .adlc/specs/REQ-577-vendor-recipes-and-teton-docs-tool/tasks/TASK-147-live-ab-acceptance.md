---
id: TASK-147
title: "Live A/B acceptance against an isolated llama daemon"
status: complete
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

- [x] Weights availability checked first (weights dir present + symlinkable);
  the outcome path taken is stated plainly in verification.md.
- [x] If run: AC-1 (Kimi request → exact `teton provider add kimi --kind
  openai-compatible --endpoint <catalog endpoint> --model <example>` +
  `teton policy set-tier think kimi`, zero repo-search calls, ≥ 3 trials)
  and AC-2 (Claude request → `--kind anthropic` recipe; control "What
  version is this crate? Check Cargo.toml." still calls `read`) both
  recorded with baseline comparison.
  **Ticked for the recording, which is this task's deliverable — and the
  recording says spec AC-1 FAILED.** See Outcome below; do not read this box
  as spec AC-1 being met.
- [ ] If deferred: verification.md carries the full manual protocol and the
  REQ's acceptance state says "CI-verified; live A/B deferred (weights
  absent)" — the honest claim, per ethos #7. **Not applicable: the weights
  were present and the live A/B ran.**

## Outcome (2026-08-14)

The run happened — 27 sessions against two isolated real-weights daemons,
baseline `main@4569311` vs candidate `9ea2988`, byte-identical replies across
trials at temperature 0.2. Full record:
[`../verification.md`](../verification.md).

- **Spec AC-2: PASS.** `--kind anthropic --model claude-opus-5`, no
  `--endpoint`, `teton policy set-tier think claude`, 4/4; the control still
  calls `read` and answers from `Cargo.toml`, 3/3.
- **Spec AC-1: FAIL.** The provider command is exactly right — Moonshot's real
  endpoint and `kimi-k3` where the baseline fabricates `https://api.kimi.com/v1
  --model gpt-4` — and there are **zero** repository-search calls in every
  trial, including in a working tree full of Teton's own docs. But the routing
  command is `teton policy set-tier reflex kimi`, not `think`, in 4/4 trials.
  AC-1 asks for both commands, so it is not met. Recorded as failed rather
  than reworded (ethos #7).
- **Two defects found that only a live run could find** (verification.md §5):
  `teton_docs` trips a permission prompt at the default level and is denied
  outright at `plan` — the requirement's Permissions table says it must not
  prompt — and two llama daemons cannot be resident at once on this machine,
  so an A/B of this shape must be serialized.

REQ-577's honest acceptance state is therefore: **AC-2 live-verified; AC-1 not
met**; a prompt fix plus a re-run of this matrix is the remaining work.

## Technical Notes

- Isolation method (LESSON-482): release build `cargo build --release
  --workspace --features tetond/llama`; short `XDG_RUNTIME_DIR` (socket must
  fit SUN_LEN ~104 bytes); fresh base dir; symlink the weights dir so the
  second daemon shares the mmap'd inode.
- A/B discipline (BUG-168): baseline first, prompt as the only variable,
  byte-identical replies across trials are expected at temp 0.2.
- Repo memory: targeted e2e runs can test a stale daemon — build the
  workspace before any live run (BUG-164 rule).
