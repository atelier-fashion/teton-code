---
id: TASK-134
title: "Docs: README web-setup section, manual-verification steps"
status: complete
parent: REQ-572
created: 2026-08-13
updated: 2026-08-13
dependencies: ["TASK-132"]
---

## Description

User-facing documentation for the new surface, and the manual gates for the
model-behavior legs automation cannot pin.

## Files to Create/Modify

- `README.md` — a "Turning on web lookup" section: `/web setup` walkthrough, the `[web]` table for hand-editing, keychain-reference rule, `search_auth` template with the Brave/Kagi/SearxNG examples (mirrors the bundled guide — the AC-8 contract tests are the consistency anchor), and the off-by-default posture sentence.
- `docs/manual-verification.md` — REQ-572 section: (1) live AC-1 probe — fresh config, ask a web-needing question, expect the refusal to name web lookup and `/web setup` with zero tool calls (the BUG-160 verification pattern); (2) AC-9 dedup — ask two web-needing questions, expect the second refusal to reference, not repeat; (3) real-keychain flow once on macOS — store, commit, `security find-generic-password -s teton -a web-search` exists, abort path removes it.

## Acceptance Criteria

- [x] README section exists and the commands/keys in it appear verbatim in the bundled guide (drift check note pointing at the AC-8 enumeration helper) — `README.md` "Turning on web lookup"; every command, key and template in it (`/web setup`, the six `[web]` keys, `keychain://teton/web-search`, `Authorization: Bearer {key}`, `X-Subscription-Token: {key}`, `Authorization: Bot {key}`, `?format=json`) is the string `crates/tetond/src/harness/self_config.md:6` carries. Read with note 2 below: the three concrete **endpoint URLs** live in `web_setup_ui.rs`'s `ENDPOINT_HELP`, not the guide, and the drift comment names both sources rather than implying one
- [x] manual-verification steps are copy-pasteable and name their expected outputs — `docs/manual-verification.md`, the REQ-572 section: three legs, each with a shell-runnable procedure and quoted expected text (the completion notice verbatim from `session_ui.rs:626`, the cleanup line from `web_setup_ui.rs:745`, the daemon's save-failure sentence from `runtime.rs:4020`, and `security find-generic-password -s teton -a web-search`)
- [x] Both docs state the user-only rule: the model can name the opt-in but only the user can run it — README's opening paragraph and the REQ-572 section's preamble, both grounded in the same fact (`slash.rs:841-845`: a tool call named `web setup` reaches the registry and finds no such tool)

## Technical Notes

Keep README additions inside the existing "bring your own models" narrative
flow (BUG-160 added the provider commands there — this extends the same
section family, not a new top-level).

## Implementation notes (as built)

**Sections added.** `README.md`: `### Turning on web lookup`, a sibling of
`### Hooking up an external model` under `## What it is` (placed after the two
promises, so the privacy promise leads into the opt-in that is shaped like one).
`docs/manual-verification.md`: `## REQ-572 — capability-aware refusals and
guided web enablement`, in the ascending `## REQ-nnn` family of the first
runbook document — **before** the `# Manual verification runbook — REQ-570
AC-3b` top-level, whose trailing sign-off template must stay last in the file.

Every claim was read out of the shipped code before it was written; the
file:line list is in the commit's report. Notes on the judgement calls:

1. **The README documents `/web setup` as the live path and the hand edit as
   the restart-shaped one**, because that is what the code does: the commit
   swaps the in-memory config under the mutex `build_tools` clones per turn
   (`runtime.rs:3973-3975`, `4024`), and nothing anywhere reloads the file —
   `load_config` has exactly one production caller, `Runtime::from_env`
   (`runtime.rs:1410`). "A hand-edited config is read when the daemon next
   starts" is therefore a fact about the loader, not a caution.
2. **Two drift sources, not one.** The task file says the README's strings
   appear "verbatim in the bundled guide". They do — except the three concrete
   endpoint URLs, which the guide deliberately does not carry (TASK-131 note 6:
   prompt headroom compressed that section to one line, and it clears the
   ceiling by 87 bytes). Those come from `ENDPOINT_HELP`
   (`web_setup_ui.rs:670-676`). The drift comment names both files and what
   each owns, so the next person editing a backend row knows there are three
   places, not two.
3. **The drift comment names `crates/tetond/tests/web_setup_contracts.rs`,
   which does not exist yet.** It is TASK-133's file (in flight at this
   commit); the AC-8 enumeration helper this task's AC points at is defined
   there. Naming the intended path makes a dangling reference discoverable if
   TASK-133 lands a different shape; leaving it vague would not. Flagged in the
   report as the one forward reference in these docs.
4. **The example `[web]` table is the Brave shape, not a mixture.** A block
   pairing a SearxNG endpoint with a Brave header and a key ref would be a
   config the validator accepts and the backend rejects; worse, `search_auth`
   beside an absent `search_key_ref` is *refused* at load
   (`config.rs:393-396`), so a "here are all the keys" block with a keyless
   endpoint would not load at all. One coherent table, plus a sentence saying
   what to delete for a keyless backend.
5. **The manual gates record what is NOT expected as carefully as what is.**
   AC-1's leg says the status row should *not* appear (TASK-132 note 6 and
   `the_capability_alone_never_makes_the_row_appear`), because a verifier who
   expects a `web:` row and does not see one would file the deliberate decision
   as a bug. The same paragraph records the Ctrl-C-during-key-read echo
   residual (TASK-132 note 2) as a known residual rather than a finding, with
   `stty sane` as its fix.
6. **The keychain leg has a live cleanup trigger.** `Keychain::delete` runs
   only when a commit fails *after* a store, which no ordinary abort produces —
   preview failures happen before the store, and a declined confirm never
   stores. The reachable trigger is a read-only config **directory**:
   `write_config_atomically` creates its temp file beside the target
   (`runtime.rs:4591-4593`), so a `chmod 555` parent makes the write fail at
   exactly that point, with the in-memory config untouched. Recorded as an
   optional step, since it is the only way a human sees the delete path run
   against a real keychain.
7. **No AC in the requirement was ticked by this task.** These docs are the
   manual gates *for* AC-1 and AC-9's model-behavior legs, not evidence for
   them: all three legs are recorded `Status: NOT RUN`, and the requirement's
   boxes stay unticked until someone runs them (LESSON-433 / the 9c2d2ed
   deferred-AC convention).

**Verification.** Docs only — no `crates/` file was touched (TASK-133 was
running the suites in this worktree concurrently). Both files re-read in full
after editing; every command, path, key, prompt string and rendered sentence
traced to a file and line in the shipped tree.
