---
id: TASK-139
title: "README drift comment collapse + workspace verification"
status: complete
parent: REQ-573
created: 2026-08-14
updated: 2026-08-14
dependencies: ["TASK-136", "TASK-137", "TASK-138"]
---

## Description

Collapse the three-way drift comment to name the daemon catalog as the single
in-tree source (AC-5), and run the full-workspace verification including e2e
against a freshly built daemon (ADR-E / LESSON-510).

## Files to Create/Modify

- `README.md` — rewrite the drift-check comment (~334–345): the backend rows
  are prose mirrors of `crates/tetond/src/web_setup_catalog.rs`; the contract
  suite enumerates that catalog typed; the bundled guide is CI-checked
  against it; the CLI renders it over RPC (no in-tree copy to name)
- `docs/manual-verification.md` — touch the `/web setup` section only if its
  described behavior changed (expected: no change; verify and say so)

## Acceptance Criteria

- [x] The README comment names exactly one in-tree source of backend strings
      and states where each other surface's sync is enforced (AC-5) — the
      comment (README.md:333) names `crates/tetond/src/web_setup_catalog.rs`
      alone, then the contract suite (typed enumeration), the bundled guide
      (bidirectional check), and the CLI (no copy; renders `web/setup_plan`)
- [x] README backend rows still match the catalog strings byte-exact (manual
      diff recorded in the task completion note — they are prose, the comment
      is the enforcement pointer) — **no drift**: all three endpoints and both
      auth templates are byte-identical to `web_setup_catalog.rs` (lines
      57/75/86, 79/89), the SearxNG row's "none — keyless" matches
      `auth_template: None` + `needs_key: false`, and README:305's
      `Authorization: Bearer {key}` is `GENERIC_SEARCH_AUTH_TEMPLATE`
      verbatim. Only the first column differs, as prose: the table reads
      "SearxNG (self-hosted)" where the catalog label is "self-hosted
      SearxNG" — same words in table-column order, unchanged since v0.1.14,
      and not a value any surface parses. Rows left as they are
- [x] `cargo build --workspace` then `cargo test --workspace` green — build
      first so `cli_e2e` exercises a fresh `tetond` (LESSON-510; the repo's
      known stale-daemon trap) — build exit 0, then
      `--no-fail-fast`: 2425 passed, 0 failed, 1 ignored (the `--features
      live` smoke) across 50 targets
- [x] `a_piped_web_setup_prints_the_instructions_and_asks_nothing` and the
      full-walk e2e pass against the fresh daemon: piped output still carries
      the SearxNG line, sourced from the RPC catalog (AC-6 end-to-end) — both
      ok in `cli_e2e` (30 passed); the walkthrough asserts the rendered
      `self-hosted SearxNG` label, which now exists only in the daemon catalog
- [x] `cargo clippy --workspace` introduces no new warnings —
      `--all-targets -- -D warnings` exit 0, zero warning lines

## Technical Notes

If any doc besides README names the deleted constants (`ENDPOINT_HELP` etc.),
sweep with a tree-wide grep and update (docs/manual-verification.md:1082
references web_setup_ui.rs — confirm context still reads true). Use
`--no-fail-fast` when a workspace test run reports failures, per the repo's
counting convention.

## Completion note

**Grep sweep** (`ENDPOINT_HELP|KNOWN_BACKEND_AUTH|DEFAULT_SEARCH_AUTH`, whole
tree minus `.git`). The one live doc hit was the README comment this task
rewrote; it is gone. What remains is accurate: `web_setup_ui.rs`'s
`ENDPOINT_HELP_HEADER` / `ENDPOINT_HELP_BARE` (new names introduced by
TASK-138, holding no backend strings), `web_setup_contracts.rs:23` describing
the `include_str!` parse REQ-573 deleted, and `.adlc` spec/task artifacts —
REQ-573's own, plus REQ-572's, which describe the pre-REQ-573 state correctly.

**`docs/manual-verification.md`: unchanged, deliberately.** Line 1082 cites
`teton/src/web_setup_ui.rs` as where the whole client flow is covered against a
fake keychain and a scripted daemon — still true after TASK-138. The macOS
keychain walkthrough (lines ~1175+) drives `3` → Brave endpoint → `y` → the
`X-Subscription-Token: {key}` default → key → confirm; that prompt sequence and
that offered default are exactly what the RPC-fed flow produces (AC-6 parity),
so nothing in the described behavior changed.
