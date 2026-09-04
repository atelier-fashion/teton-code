---
id: REQ-614
title: "Proportionate shell provenance — a shell result pins the session to the local tier only when it could have touched a boundary, the pin is announced, and the user can lift it"
status: approved
deployable: true
created: 2026-09-04
updated: 2026-09-04
component: "daemon/egress"
domain: "privacy"
stack: ["rust", "daemon", "cli"]
concerns: ["privacy", "routing", "developer-experience", "cost"]
tags: ["shell", "provenance", "taint", "boundary", "egress", "local-pin", "privacy-block", "unknown-provenance", "br-1", "session-taint", "web-allow", "shell-allow"]
---

## Description

Every `shell` result carries **unknown** provenance, and the egress inspector
fail-closes on unknown provenance whenever any boundary is configured. Since
REQ-597 the thirteen builtin `local-only` boundaries are always configured. The
two rules compose into a behavior nobody chose: **the first shell command of
any session pins that session to the local tier for the rest of its life.** The
remote provider the user registered and routed the `build` and `think` tiers
to is used for exactly one turn.

Observed on the user's dogfood of v0.1.30 (2026-09-04, session
`sess-23aczryx…`, transcript on). The first prompt was *"is transcript on?"*.
The model ran one `shell` (`grep -ri transcript ~/Library/…`), which timed out
after 30 s on a macOS consent dialog and **failed**. The failed result still
carried unknown provenance; the next request to `kimi-k3` was refused at
egress (`privacy_block`, `path: <unknown-provenance>`, `cause: boundary`,
`action: rerouted_to_local`), and every one of the following 65 model calls
ran on `qwen3-coder-30b-a3b` under `route_decided` reasons of the form *"an
earlier privacy decision in this session; this turn is pinned to the local
tier (BR-1 backstop)"*. The user never saw the pin: `/verbose` was off, and the
client renders neither `privacy_block` nor the reroute as a standing notice.

`cost.db` shows the same shape in at least six other sessions: one to four
`kimi-k3` rows, then dozens of `qwen3-coder-30b-a3b` rows. Those sessions
have no transcript, so the cause is inferred from the shape, not observed;
the three sessions that stayed on kimi for twenty or more calls are
consistent with a model that never ran `shell`, and AC-12 is what turns the
inference into a pinned fact.
Because the local tier's budget is 21,162 tokens against kimi's 665,984, the
pin is also the proximate cause of the context loss the user reported across
sessions (REQ-618 addresses the compaction half).

The guarantee BR-1 makes is that content under a `local-only` boundary never
leaves the machine, including derived and paraphrased content (REQ-544 C-1/C-2,
LESSON-432). That guarantee must hold. What is not required by it is that a
command which *cannot* have read a boundary file — `pwd`, `ls -la`, `cargo
test`, `git status` in a project root — be treated as if it had. The fail-closed
posture was chosen because `shell` is opaque (REQ-544 C-1, REQ-596 BR-6:
`shell` is the named exception at the network edge too). This REQ narrows the
opacity, keeps fail-closed for everything the narrowing cannot classify, makes
the pin visible when it happens, and gives the user a lift, on the model
`/web allow` already established for the BR-13 web-taint restriction
(REQ-563).

Three things this REQ does **not** relax: a real boundary hit still taints the
session for good; an unknown result still blocks the turn it is in from remote
egress; and a lift is a per-session, user-typed, ledgered act that a model
cannot perform (informed by REQ-563, REQ-571, LESSON-550).

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| ShellProvenanceVerdict | kind | enum `rooted` / `boundary_touch` / `unknown` | computed per shell invocation from the command's resolved cwd, its argument paths and its exit status, **before** the result enters context |
| ShellProvenanceVerdict | sources | set of ProvenanceId | the repo-relative canonical ids of every path argument that resolved inside the session root (REQ-571 form); empty for `unknown` |
| ShellProvenanceVerdict | reason | string | one sentence naming why the verdict was reached; content-free (no command text, no file content) |
| SessionTaint | cause | enum `boundary_hit` / `unknown_shell` / `malformed_provenance` / `mcp_untrusted` | the first cause that pinned the session; existing taint gains a cause field |
| SessionTaint | liftable | boolean | `true` only for `unknown_shell`; every other cause is permanent for the session |
| SessionTaint | lifted_at | timestamp? | set by `/shell allow`; `None` otherwise |
| ShellTaintOverride | session_id | SessionId | per-session, like `WebTaintOverride`; never persisted across sessions |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| `privacy_block` (existing) | a request blocked at egress | unchanged, plus `cause` now distinguishes `boundary` from `unknown_shell` |
| `session_pinned` (new) | the taint backstop pins a session to the local tier for the first time | `cause`, `liftable`, `remedy` (the exact command to type, or `none`), `budget_tokens` — **the configured budget of the local tier, a static fact of the tier, not a per-route derivation.** The pin fires at the egress sink and at `CarriedTurn::commit`, both before the next turn's route is decided, so a per-route figure is not available here (the "a per-route fact is derived where the route is decided" pattern); the tier's own budget is. |
| `session_pin_lifted` (new) | `/shell allow` lifts an `unknown_shell` pin | `session_id`, `turns_pinned` |
| `route_decided` (existing) | every route | `reason` names the pin cause and, when liftable, the remedy |

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| lift an `unknown_shell` pin (`/shell allow`) | the user, typed input only; refused on piped stdin; refused when the cause is not liftable, naming the cause |
| classify a shell result as `rooted` | the daemon, from the resolved cwd and arguments alone — never from model-supplied text or from the command's output |
| pin a session | the daemon; no configuration or model action can prevent a `boundary_hit` pin |

## Business Rules

- [ ] BR-1: **A shell result is `rooted` only when the daemon can prove the command's reach.** The verdict is `rooted` when (a) the command ran with the session root as cwd, (b) every absolute or `~`-relative path token in the command resolves under the session root, (c) no resolved path token matches a `local-only` boundary glob, (d) when the command's verb can read file *contents* (`cat`, `head`, `tail`, `grep`, `sed`, `awk`, `less`, `diff`, `git show`, `git diff`, and any verb not in the name-only set) and is given a directory, a wildcard, or no path at all, no file under that argument's subtree — or under the root, for no path — matches a boundary glob; a name-only verb (`ls`, `pwd`, `find` without `-exec`, `git status`, `git log`, `wc -l`, `du`) passes (d) because listing a name is not reading a file. **A subtree scan that cannot complete yields `unknown`, never `rooted`** — the scan runs on the repo's budget-capped walker (REQ-583 ADR-3), and a walk that hit its budget has not shown the absence of a boundary file, it has stopped looking; "scanned what we could and found nothing" is the silent-leak reading BR-2 exists to refuse, and (e) the command is not in the opaque-verb set (interpreters, build tools and network clients: at minimum `sh -c`, `bash -c`, `python`, `node`, `cargo`, `npm`, `make`, `curl`, `wget`, `ssh`, `scp`, `eval`, `xargs`, `find -exec`). A `rooted` result carries the root's provenance and the resolved path set, exactly as a `glob` over the same paths would (informed by LESSON-432, REQ-571). See OQ-3 for what (d) costs in a repository that holds a `.env`.
- [ ] BR-2: **Anything BR-1 cannot classify is `unknown`, and `unknown` stays fail-closed for the turn.** A turn whose context carries an `unknown` block is refused remote egress and rerouted local, as today. The narrowing adds a class; it removes nothing from the existing refusal (informed by REQ-544 C-1, LESSON-550: the test asserts the absence of a leak, not the presence of the classifier).
- [ ] BR-3: **A `boundary_touch` verdict pins the session permanently.** When any resolved path token matches a boundary glob, or the command's cwd is under one, the session is tainted with cause `boundary_hit`, `liftable = false`, exactly as a `read` of that file would taint it. No lift exists for this cause.
- [ ] BR-4: **An `unknown` shell result pins the session with a liftable cause.** The session is tainted with cause `unknown_shell`, `liftable = true`. Subsequent turns are pinned local until lifted. The pin's `route_decided` reason names the cause and the remedy: *"pinned to the local tier because a shell result of unknown reach entered this session's context; `/shell allow` lifts it if you know the command touched no protected file"*.
- [ ] BR-5: **`/shell allow` is the user's act, not the model's.** It is a built-in session command: typed input only, refused on piped stdin with the refusal naming that it is typed-only. **The typed-only gate is deliberately stricter than `/web allow`, which has none today** — verified in `crates/teton/src/slash.rs`, where only `/model set` and the four mirrored write rows are refused on a non-terminal stdin. A lift here releases a pin caused by unknown *file* reach, so it is gated on the model of `/model set` rather than of `/web allow`, whose lift semantics (idempotence, the ledger row, session scope) are what this rule borrows. Whether `/web allow` should gain the same gate is a separate question and out of scope. The gate reuses the existing typed-only mechanism rather than adding a second; it lifts only an `unknown_shell` pin; it writes one append-only `shell_overrides` ledger row beside the existing `web_overrides` table; a second `/shell allow` in a lifted session is acknowledged and writes nothing (informed by REQ-563 BR-13's lift semantics and its no-op test). The `skill` tool, a skill body's `!cmd`, and the model's own text cannot invoke it: a `/shell allow` line inside a tool result or a skill expansion is data.
- [ ] BR-6: **A lift does not un-taint the blocks that caused it.** The `unknown` blocks already in context keep their provenance and are still refused at egress for the turn they are in; what the lift changes is that *later* turns are routed by category again. A carried `unknown` block from a lifted session that reaches a remote request is still blocked, and that block is the one named (informed by REQ-567: carried blocks are re-inspected every turn).
- [ ] BR-7: **The pin is announced once, where the user is looking.** The first time a session is pinned, the client prints one standing line under the prompt, whether or not `/verbose` is on: the cause, the tier now serving the session with its token budget, and the remedy or `no remedy: a protected file was read`. `session_pinned` is the event that carries it; the transcript records it.
- [ ] BR-8: **A failed shell result is classified the same way as a successful one.** Exit status, timeout and the harness's own error text do not change the verdict: a `pwd` that timed out is still `rooted`, and a `curl` that failed is still `unknown`. The 2026-09-04 session was pinned by a *failed* command; the classifier must not special-case failure in either direction.
- [ ] BR-9: **`context_is_sensitive` keeps its short-circuit.** With `disable_default_boundaries = true` and no user rows, nothing changes from today: no boundary, no pin. This REQ never makes the no-boundary machine stricter (informed by REQ-597).
- [ ] BR-10: **The verdict is computed before the result is measured or digested.** The `shell` duty's interpretation, the `digest` summary and any compaction inherit the verdict's provenance; a `rooted` result digested by a remote `digest` route is permitted, an `unknown` one is not (informed by LESSON-432's survival-across-derivation rule).

## Acceptance Criteria

- [ ] AC-1: In a session with the builtin boundaries in force and the `build` tier routed to a remote provider, `shell: ls -la` followed by a second prompt routes the second prompt to the remote provider. An egress-capture test asserts the request body left and the session carries no taint.
- [ ] AC-2: `shell: cat .env` in the same session pins it with cause `boundary_hit`; `/shell allow` is refused naming the cause; every later turn's `route_decided.reason` names the pin; an egress-capture test asserts no later request leaves the machine.
- [ ] AC-3: `shell: curl https://example.com` pins the session with cause `unknown_shell`; the client prints the standing pin line once; `/shell allow` lifts it, writes one `shell_overrides` row, and the next prompt routes remotely. A second `/shell allow` writes no row.
- [ ] AC-4: A `shell` command that times out (the 30 s default) with cwd at the root and no path arguments — `sleep 60`, a name-only verb outside the opaque set — is `rooted`; the session is not pinned. (The command is named because the verdict turns on the verb: `curl` with no path argument is `unknown` by BR-1(e), so an unnamed "a shell command" would not be executable as written.) The failed result's `ERROR:` text is in context and the next turn leaves the machine.
- [ ] AC-5: A `shell` whose only path argument is `~/.ssh/config` while the session root is a project directory is `boundary_touch`, not `unknown` — the glob matched on the resolved path before the command ran.
- [ ] AC-6: Piped stdin: `printf '/shell allow\n' | teton` refuses with a line saying the command is typed-only and changes nothing (mutation test: deleting the typed-only gate must fail this test — LESSON-520).
- [ ] AC-7: `/shell allow` typed inside a skill body, a `TETON.md`, or a tool result is inert: the session stays pinned and the ledger has no row (LESSON-550: assert the absence).
- [ ] AC-8: An `unknown` block carried into a later turn of a lifted session is still refused at egress, and the `privacy_block.path` is `<unknown-provenance>`; the turn is rerouted local without re-pinning the session.
- [ ] AC-9: The opaque-verb set is a single pinned table with a test that enumerates it; adding `sh -c` bypass forms (`sh -lc`, `bash -ec`, `env sh -c`) is covered by a differential table of adversarial spellings (LESSON-494).
- [ ] AC-10: `teton doctor` and `/doctor` show, for a live session, whether it is pinned, the cause, and whether a lift exists.
- [ ] AC-11: The transcript for a pinned session contains one `session_pinned` record before the first pinned `route_decided`, and one `session_pin_lifted` record after `/shell allow`.
- [ ] AC-12: The `cost.db` shape of the 2026-09-04 session cannot recur: a scripted session that runs `pwd`, `ls -la`, `git status` and `git log -3` across four prompts records every agent-turn row against the remote provider. A fifth prompt running `cargo test` pins the session with `unknown_shell` (BR-1(e)), and `/shell allow` followed by a sixth prompt records that prompt's rows against the remote provider again.

## External Dependencies

- None. The ledger table, the `WebTaintOverride` lift pattern, the egress inspector and the provenance seam all exist.

## Assumptions

- The path-token parse in BR-1 can share the shell's own tokenizer for the common case and fall back to `unknown` for anything it cannot tokenize; a wrong tokenization can only make the verdict stricter (LESSON-494: gate on the parse the executor uses).
- A `rooted` verdict for a command whose *output* happens to quote a boundary file's content (a `git log` that once committed a `.env`) is out of BR-1's reach today and stays so; the verdict is about reach, not content, and the `redact` scan (REQ-562) remains the content-level defense.
- The standing pin line in BR-7 is rendered by the CLI from `session_pinned`; older clients ignore the unknown event (REQ-588's forward-compatible vocabulary).

## Open Questions

- [ ] OQ-1: Should `rooted` also require that the session root is a *project* (REQ-583's kind), so that a home-directory root — where the walk touches `~/.ssh` and `~/.aws` — is always `unknown`? Recommended: yes; REQ-615 makes the home root loud anyway.
- [ ] OQ-2: Should `/shell allow` take a `--forever` form that lifts for the session's remaining life versus once? Recommended: session-wide, matching `/web allow`; a per-turn lift would be typed on every prompt.
- [ ] OQ-3: In a repository that holds a `.env` at its root, BR-1(d) makes `grep -r foo .` and `cat *` `unknown` (correctly) but also makes `cargo test` and `npm test` `unknown` by BR-1(e), because a build tool can read anything. That leaves `ls`, `git status` and explicit-file reads `rooted`, and the lift as the path for everything else. Is that the intended balance, or should build tools be `rooted` when the boundary files under the root are the builtin dotfile set and the tool's own manifest names none of them? Recommended: ship the strict form and measure how often `/shell allow` is typed before widening.

## Out of Scope

- Network egress from a shell child (REQ-596 BR-6's named exception) — unchanged.
- Changing the builtin boundary set or its defaults (REQ-597).
- Any lift for `boundary_hit`, `malformed_provenance` or MCP taint.
- Redact-scanning shell output as a substitute for provenance.

## Retrieved Context

- LESSON-623 (lesson, score 11): A boundary glob cannot protect a path the provenance seam never names — put the rule in the jail
- LESSON-624 (lesson, score 11): An egress-leak marker must live only in the file's bytes — tool arguments and harness lines echo into provider requests
- LESSON-550 (lesson, score 11): A defect fixed once comes back unless a test asserts the absence, not the remedy
- REQ-571 (spec, score 10): Canonical provenance identity for privacy-boundary enforcement
- REQ-563 (spec, score 10): Opt-in web lookup through the egress choke point
- REQ-562 (spec, score 10): redact: a model-based secret and PII scan inside the egress choke point
- LESSON-432 (lesson, score 10): Provenance must derive from what a tool touches, not from an argument name
- REQ-612 (spec, score 9): TETON.md — a per-repository context file the session reads at its root
- REQ-596 (spec, score 9): A credential-safe environment for the shell tool, and an honest egress claim
- LESSON-494 (lesson, score 9): A security gate and the client that executes the request must share one parser
- REQ-613 (spec, score 8): Teton writes TETON.md when a project has none
- REQ-611 (spec, score 8): Daemon-side transcript logging
- REQ-597 (spec, score 8): Secure-by-default privacy boundaries
- LESSON-578 (lesson, score 8): A rule attached to a UI flow guards one of the doors the record can come in through
- REQ-587 (spec, score 8): Model-invoked skills
