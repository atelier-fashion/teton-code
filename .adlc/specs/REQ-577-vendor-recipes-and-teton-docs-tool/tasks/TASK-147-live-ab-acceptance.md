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
  Both are recorded with the baseline comparison, and after the round-2 fixes
  below both spec ACs hold live.
- [ ] If deferred: verification.md carries the full manual protocol and the
  REQ's acceptance state says "CI-verified; live A/B deferred (weights
  absent)" — the honest claim, per ethos #7. **Not applicable: the weights
  were present and the live A/B ran.**

## Outcome (2026-08-14)

Two rounds. Full record: [`../verification.md`](../verification.md).

**Round 1 — the A/B as first run.** 27 sessions against two isolated
real-weights daemons, baseline `main@4569311` vs candidate `9ea2988`,
byte-identical replies across trials at temperature 0.2.

- **Spec AC-2: PASS.** `--kind anthropic --model claude-opus-5`, no
  `--endpoint`, `teton policy set-tier think claude`, 4/4; the control still
  calls `read` and answers from `Cargo.toml`, 3/3.
- **Spec AC-1: FAIL.** The provider command was exactly right — Moonshot's real
  endpoint and `kimi-k3` where the baseline fabricates `https://api.kimi.com/v1
  --model gpt-4` — with **zero** repository-search calls in every trial,
  including in a working tree full of Teton's own docs. But the routing command
  was `teton policy set-tier reflex kimi`, not `think`, in 4/4 trials.
- **Two defects only a live run could find:** `teton_docs` tripped a permission
  prompt at the default level (and would be denied at `plan`), against the
  requirement's own Permissions row; and two llama daemons cannot be resident
  at once on this machine, so an A/B of this shape must be serialized.

**Round 2 — both defects fixed, candidate matrix re-run.**

- `DOCS_TOOL_NAME` joins `READ_ONLY_TOOLS`, plus the missing *callability* test
  class (every prior `teton_docs` test asserted exposure only):
  `a_bundled_docs_read_is_allowed_at_every_level_and_asks_nothing` and a
  `teton_docs → Allow` row in the golden level table. Both mutation-checked.
- `self_config.md` step 2 names what each tier is **for** (+95 bytes, nothing
  else reworded); both prompt margins re-measured and re-recorded — 229 and 277
  bytes against a 48-byte floor, with `REDACT_BODY_OVERHEAD_BYTES` unchanged.
- Re-run, 8 candidate sessions: **Shape A 3/3 yields both exact commands
  including `teton policy set-tier think kimi`**, Shape B 2/2 unchanged,
  control still calls `read`, and two probes show `teton_docs` executing
  `[done]` with no permission prompt anywhere in the run.

**Final: spec AC-1 and AC-2 both live-verified** on macOS/Apple Silicon with
qwen3-coder-30b-a3b at temperature 0.2. AC-3..AC-8 are CI claims owned by
TASK-143..146 and are not what this task speaks to. Round 1 is kept in
verification.md unedited — it is the evidence the fixes were needed, and the
standing proof that a prompt edit moves behaviour it was not about.

## Technical Notes

- Isolation method (LESSON-482): release build `cargo build --release
  --workspace --features tetond/llama`; short `XDG_RUNTIME_DIR` (socket must
  fit SUN_LEN ~104 bytes); fresh base dir; symlink the weights dir so the
  second daemon shares the mmap'd inode.
- A/B discipline (BUG-168): baseline first, prompt as the only variable,
  byte-identical replies across trials are expected at temp 0.2.
- Repo memory: targeted e2e runs can test a stale daemon — build the
  workspace before any live run (BUG-164 rule).
