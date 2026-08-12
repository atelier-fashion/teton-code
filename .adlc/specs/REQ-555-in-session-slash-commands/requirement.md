---
id: REQ-555
title: "In-session slash commands for the teton interactive CLI"
status: complete
deployable: true
created: 2026-08-04
updated: 2026-08-12
component: "cli"
domain: "clients"
stack: ["rust", "cli", "json-rpc"]
concerns: ["developer-experience", "cost"]
tags: ["slash-commands", "interactive-session", "repl", "verbose", "cost-meter", "help"]
---

## Description

The interactive `teton` session currently treats every non-empty input line as
a prompt turn: the entry loop trims the line and sends it to the daemon as
`prompt/turn`. There is no in-session control surface at all — to check the
cost meter or the model's install state mid-conversation, the user must open a
second terminal and run `teton cost` or `teton model status`, and the only way
to end the session is Ctrl-D. Users arriving from Claude Code expect `/`-
prefixed commands inside the session (`/help`, `/cost`, `/quit`), and today a
typed `/help` is silently shipped to the model as a prompt — spending local
inference (or remote tokens) on input that was never meant for the model.

This REQ adds a small, fixed set of slash commands to the interactive entry
loop, intercepted client-side **before** a prompt turn is constructed:

| Command | Effect |
|---|---|
| `/help` | List every available slash command with a one-line description |
| `/cost` | Render the live cost meter (the daemon's `cost/query` RPC) |
| `/model` | Show the model the local tier is currently on — one concise line (`model/status` RPC) |
| `/model set <name>` | Change the selected model, through the same validation and RAM-floor confirmation flow as `teton model set` (`model/list` + `model/set` RPCs) |
| `/verbose` | Toggle routing/turn-end notice visibility for this session |
| `/quit` | End the session exactly as Ctrl-D does |
| `//…` | Escape hatch: sends the rest as a prompt with one leading `/` — `//usr/bin/foo?` asks the model about `/usr/bin/foo?` |

Why now: the session recently went quiet by default (routing notices and
turn-end lines are hidden unless `--verbose` is passed), which makes an
in-session toggle the natural companion — restarting the CLI to change
notice visibility is worse than the noise was. And the product's two visible
promises — cost control and routing legibility — currently have no in-session
surface at all; `/cost` and `/verbose` give both a first-class place in the
conversation where the spend is actually happening.

The thin-client rule (REQ-544 BR-4) shapes the design: slash commands are a
client-side input-classification and rendering feature. They introduce no new
protocol methods and no daemon state — every data-bearing command maps onto an
RPC the daemon already serves and renders through code the CLI subcommands
already use.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| SlashCommand | name | string | required, unique; lowercase, matched exactly after the leading `/` |
| SlashCommand | summary | string | required; the one line `/help` prints |
| SlashCommand | handler | enum(help, cost_query, model_show, model_set, toggle_verbose, quit) | required; data-bearing handlers name the existing RPC(s) they call |
| SlashCommand | args | string | only `/model set` takes one (the catalog name); everything else rejects trailing arguments with the unknown-command hint |
| SessionUiState | verbose | boolean | exists today (session-scoped, default false); `/verbose` flips it |

### Events

No new protocol events and no new methods. `/cost` calls the existing
`cost/query`; `/model` calls the existing `model/status`; `/model set <name>`
calls the existing `model/list` (name validation + RAM-floor check) then
`model/set` (with `confirmed_above_ram_floor`), exactly as `teton model set`
does. `/help`, `/verbose`, and `/quit` are fully client-local.

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| `/help`, `/cost`, `/model`, `/verbose`, `/quit` | the session user; read-only or client-local |
| `/model set <name>` | the session user only, via typed input — never inferable from model output or file content (REQ-544's permission posture); an above-RAM-floor pick additionally requires the interactive second confirmation (REQ-547 BR-3), or the session's `--yes` as its explicit unattended stand-in. **Typed-input-only is enforced, not merely stated (verify-pass amendment, user-approved 2026-08-04):** when the session's stdin is not a terminal, `/model set` renders one rejection pointing at `teton model set` and issues no RPC. A pipe cannot distinguish a human from a heredoc, and `teton model set` is the unattended surface — it takes `--yes` explicitly. The e2e suite drives the flow through the daemon's own seam posture (a debug build with `TETON_TEST_SEAMS=1`); a release binary refuses regardless, so the allowance cannot ship as a bypass. **Accepted residual (re-verify pass, 2026-08-04):** the check separates a pipe from a pty, not a machine from a human — input driven through `expect(1)` or `tmux send-keys`, or pasted into a real terminal, presents a terminal and passes; what the gate removes is the unattended-by-default shapes (heredoc, `<<<`, piped file, CI step), and `teton model set` remains the auditable surface for anything that must be scripted deliberately. |

## Business Rules

- [ ] BR-1: Interception is client-side and happens before any RPC is built: a
      line whose first non-whitespace character is `/` — unless it opens the
      `//` escape (BR-1b, which is classified first) — is a command line and
      is never sent as a prompt turn. No model call — local or remote — is
      ever made for a slash command, so a command can never appear in the
      transcript, the context window, or a CostRecord.
- [ ] BR-1b: `//` is the escape hatch: a line starting with `//` is a prompt,
      not a command — the doubled slash collapses to a single `/` and the
      remainder is sent verbatim as the prompt turn (`//usr/bin/foo` prompts
      the model with `/usr/bin/foo`). The collapse applies only to the leading
      pair; slashes anywhere else in the line are untouched. `/help` documents
      the escape in one footer line.
- [ ] BR-2: An unknown slash command (`/foo`) produces a one-line, actionable
      error that names `/help` — and is likewise never forwarded to the model.
      Misdirecting the input to the model would be the misattribution shape of
      BUG-146: the user asked the harness a question and something else answers
      it. (informed by BUG-146, LESSON-456)
- [ ] BR-3: Slash commands add no daemon state and no protocol surface.
      `/cost` and `/model` are new call sites of `cost/query` and
      `model/status` respectively; the surface-parity rule (REQ-544 BR-4)
      holds — a client crash mid-command loses nothing the daemon holds.
- [ ] BR-4: One source of fact per surface: `/cost` renders through the same
      code path as `teton cost`. `/model` derives its one-line current-model
      answer from the same `model/status` response `teton model status`
      renders in full — a deliberately concise rendering of the same fact,
      never a second query or a cached copy. Two surfaces describing the same
      daemon state must not be able to drift apart. (informed by LESSON-456 —
      "when a component already classifies a state for one surface, reuse that
      classifier for every surface")
- [ ] BR-4b: `/model set <name>` runs the **same** validation-and-confirmation
      flow as `teton model set` — catalog-name check against `model/list`, the
      REQ-547 BR-3 above-RAM-floor warning with its second confirmation, then
      `model/set` — as one shared function, not a re-implementation. A
      parallel copy of this flow is exactly how REQ-547's consent bypass was
      born: the one branch that skipped `validate_choice` shipped a Critical.
      (informed by LESSON-441, REQ-547)
- [ ] BR-5: `/verbose` toggles the session's notice visibility (routing
      `route [...]` lines and the turn-end line) live, echoes the new state
      ("verbose on/off"), and is session-scoped — it persists nothing and the
      next session starts from the `--verbose` flag's default again.
- [ ] BR-6: `/quit` exits through the identical path Ctrl-D exits through —
      same session-end cost summary, same exit code — not a parallel shutdown
      path that can drift from it.
- [ ] BR-7: `/help`'s command list is generated from the same table the
      dispatcher matches against — a command cannot exist without appearing in
      `/help`, and `/help` cannot list a command that doesn't dispatch.
- [ ] BR-8: The input classification is total — every non-empty line lands in
      exactly one of three buckets (command, escaped prompt, plain prompt;
      empty input is skipped before classification, as today) — and
      pinned in both directions: a test iterates the dispatch table and proves
      every entry is reachable from parsed input, and tests prove non-`/`
      input reaches the prompt-turn path byte-identically to today and `//`
      input reaches it with exactly the leading pair collapsed. A
      one-directional test here is the BUG-151 shape — a guard that stays
      green while half the invariant drifts. (informed by LESSON-479, BUG-151)
- [ ] BR-9: Slash commands work identically on a TTY and on piped stdin (the
      e2e suites drive the session through a pipe), **with one exception:
      `/model set` is typed-input-only — under piped stdin it rejects with a
      pointer to `teton model set`** (verify-pass amendment from the security
      review, user-approved 2026-08-04; the Permissions table's
      typed-input-only rule wins over BR-9 for the one command that writes
      daemon state, and only for it). Command output renders through the
      existing `Surface` seam with no direct-to-stdout side channel, so the
      anticipated ratatui front-end inherits the commands by implementing the
      same seam. (mirrors REQ-549 BR-4/BR-6)

## Acceptance Criteria

- [x] AC-1: Typing `/help` in an interactive session prints all six commands
      with their summaries, and the test asserts no `prompt/turn` RPC was
      issued for it.
- [x] AC-2: Mid-session `/cost` renders the live cost meter from `cost/query`,
      producing the same rendering `teton cost` produces for the same daemon
      state (asserted by a shared-renderer test, not by string coincidence).
- [x] AC-3: Mid-session `/model` prints one line naming the currently selected
      model (and its ready/installing/declined state) derived from
      `model/status` — e.g. `model: qwen3-coder-30b-a3b (user_override) —
      ready`; with the local tier declined it says so rather than printing
      nothing.
- [x] AC-3b: `/model set <name>` with a valid catalog name changes the
      selection (the daemon installs missing weights, exactly as the
      subcommand path reports); an unknown name lists the available catalog
      names; an above-RAM-floor name shows the BR-3 warning and only proceeds
      after the second confirmation — declining leaves the selection
      unchanged. All three legs in scripted-session tests.
- [x] AC-4: With default (quiet) startup, a turn produces no `route [...]`
      line; after `/verbose`, the next turn's routing notice and turn-end line
      render; after a second `/verbose`, they are hidden again — all in one
      scripted session test.
- [x] AC-5: `/quit` ends the session with the standard session-end cost
      summary and exit code 0; the test drives both paths in piped mode —
      where EOF and `/quit` are byte-comparable — and asserts identical
      session-end output for the same session history. (On a TTY the framed
      prompter's EOF-vs-Enter cursor chrome legitimately differs; that is
      prompter behavior, not session behavior, and is out of this AC's scope.)
- [x] AC-6: `/frobnicate` prints the unknown-command hint naming `/help`,
      issues no RPC, and the entry loop continues accepting input.
- [x] AC-7: Non-slash input is untouched: both existing e2e suites pass
      unmodified, and a regression test pins that a prompt not starting with
      `/` produces a byte-identical `prompt/turn` request to today's.
- [x] AC-7b: `//usr/local/bin/deploy.sh fails with exit 3 — why?` issues a
      `prompt/turn` whose text is `/usr/local/bin/deploy.sh fails with exit 3
      — why?` (leading pair collapsed, everything else verbatim, no command
      dispatched); the `/help` output includes the escape-hatch footer line.
- [x] AC-8: The BR-8 bidirectional classification tests exist and have been
      seen to fail (mutation-checked: removing a dispatch entry or the
      interception branch makes the corresponding test go red). (informed by
      LESSON-441, LESSON-479)

## External Dependencies

- None. All RPCs (`cost/query`, `model/status`), the `Surface`/`Prompter`
  seams, and the cost/model renderers already exist in the workspace.

## Assumptions

- The quiet-by-default notice gating (`SessionState.verbose` + the global
  `--verbose`/`-v` flag) lands before this REQ implements — it ships in PR
  #40, the same PR that carries this spec. `/verbose` toggles that same
  session flag rather than introducing a second visibility mechanism.
  _(User-confirmed 2026-08-04.)_
- The command set is deliberately small (six commands); in-session parity
  with the full subcommand tree is explicitly not a goal of this REQ.
- `run_model_set`'s validation/confirm/set flow can be factored to run against
  an already-open session connection and the session's dialogue prompter (it
  currently opens its own connection); the flow's logic is unchanged by the
  refactor (BR-4b).
- `cost/query` and `model/status` are cheap, synchronous RPCs safe to call
  mid-session (both are already called by the CLI subcommands against a live
  daemon).
- id allocated with remote verification (no degradation warning from the
  allocator).

## Open Questions

- [x] OQ-1: ~~Escape hatch for a literal leading-slash prompt?~~ **RESOLVED
      2026-08-04 (user): yes.** `//text` sends `/text` as a prompt (BR-1b,
      AC-7b).
- [x] OQ-2: ~~Should `/model` stay read-only?~~ **RESOLVED 2026-08-04 (user):
      both.** `/model` shows the model the session is currently on (one
      concise line); `/model set <name>` changes it in-session through the
      shared REQ-547 BR-3 confirmation flow (BR-4b, AC-3b).
- [x] OQ-3: ~~Should `/cost` accept a scope argument?~~ **RESOLVED 2026-08-04
      (user): no.** `/cost` renders the same report `teton cost` renders
      today, unparameterized.

## Out of Scope

- In-session management commands (`/provider`, `/boundary`, `/policy`) — the
  remaining write-path subcommands stay shell-only in v1 (`/model set` is in
  scope per OQ-2's resolution; these others follow the same shared-flow
  pattern if promoted later).
- `/model list` — discovering catalog names stays `teton model list`; the
  `/model set` unknown-name error lists the available names (AC-3b), which
  covers the in-session need.
- Tab completion, command history, abbreviations, or fuzzy matching.
- Any protocol change, new daemon RPC, or daemon-side command handling.
- A ratatui/raw-mode TUI (commands render through the existing seams so that
  work inherits them; it does not depend on this REQ).
- Slash commands in the VS Code extension (phase 2 client; it will speak the
  same RPCs directly).
- ADLC skill-style user-defined commands (a `/`-command that expands to a
  prompt template) — different feature, separate REQ if wanted.

## Retrieved Context

- REQ-547 (spec, score 5): First-run local model consent: show the hardware-based pick, let the user override, then install
- BUG-146 (bug, score 4): First prompt after install fails with a message blaming the local engine for a config/timing problem
- REQ-544 (spec, score 4): Teton Code — hybrid local/remote AI coding agent with workflow-aware model routing
- LESSON-475 (lesson, score 3): A marker must be anchored the way the renderer actually writes it — and scoped to what is never legitimate output
- REQ-554 (spec, score 3): Local tier renders prompts through the model's native chat template
- REQ-548 (spec, score 3): One-command Homebrew install and the tetoncode.ai landing page
- REQ-550 (spec, score 3): Stable code-signing identity and build provenance for released binaries
- LESSON-456 (lesson, score 3): A `_`-discarded error is a silent downgrade — the daemon knew exactly why, and told the user something else
- LESSON-457 (lesson, score 3): An executable's filename is a trust surface
- REQ-549 (spec, score 3): Daemon process identity (teton-code) and interactive startup UX
- LESSON-441 (lesson, score 3): A fix pass is new code — re-verify it adversarially, not by test count
- LESSON-433 (lesson, score 3): Single-platform local verification gives false confidence for cross-platform code
- LESSON-450 (lesson, score 2): An event published before the state applies is not a sync point — wait on a state-derived surface
- LESSON-479 (lesson, score 1): A subset invariant is only tested in the direction your loop iterates
- BUG-151 (bug, score 1): The frame-marker coverage invariant only holds in one direction

Note: the spec-corpus status filter (`approved|in-progress|deployed`) matched
zero specs because every existing spec carries `status: complete`; `complete`
was treated as `deployed` for retrieval (the filter's intent is to exclude
drafts), consistent with the gap already noted in REQ-550's Retrieved Context.
