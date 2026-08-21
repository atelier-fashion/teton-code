---
id: TASK-222
title: "The suites that can see a model-issued call — and a Vendor that can script one"
status: draft
parent: REQ-587
created: 2026-08-20
updated: 2026-08-20
dependencies: [TASK-218, TASK-219, TASK-220]
---

## Description

The ACs whose only honest harness drives a real model-issued tool call. The
existing skill suite cannot do that yet, and saying so is half this task.

## Files to Create/Modify

- `crates/tetond/tests/skill_turn.rs` — `Vendor` gains a scripted body queue and per-call usage
- `crates/tetond/tests/skill_tool_loop.rs` (new) — AC-7, AC-8, AC-13
- `crates/tetond/tests/fixtures/skills/` (new) — the in-repo `/proceed`-shaped fixture and AC-8's synthetic bodies
- `crates/tetond/tests/egress_capture.rs`, `provenance_egress.rs` — AC-11's four legs
- `crates/teton/tests/cli_e2e.rs`, `pty_e2e.rs` — AC-10's surface half, AC-12, AC-5/AC-6's prompt bytes
- `docs/manual-verification.md` — AC-15's dogfood runbook
- `.adlc/specs/REQ-587-model-invoked-skills/requirement.md` — the Deferred section gains BUG-185's residual (ADR-11)

## Acceptance Criteria

- [ ] **AC-2 is owned here** — it had no owner, because TASK-214 said "the byte-equality test AC-2 will write", which is passive voice, not an assignment. One fixture, both callers: `run_prompt_turn` with a `SkillInvocation` and a model-issued `skill` call, asserting the **body bytes are equal** and only the frame differs. Plus the four planted-marker legs (`</tool-result>`, `User:`, `Assistant:`, `<|im_start|>`) and the negative pin that the fold never wrapped the expansion. Without this, "one expander, two callers" ships unasserted and the two paths can drift into disagreeing about what a skill says, with a green suite (LESSON-456).
- [ ] **Deferred names BUG-185's residual** (ADR-11, BR-5). The spec says either the slot cap and invocation deadline land first **or** Deferred names the residual; no task does the first, and Deferred lists six other items and not this one. State the multiplier: at `full` — AC-15(d)'s prescribed unattended posture — a cloned repo's 400-slot body invoked up to 12 times per turn is 4,800 sequential 30-second commands inside a non-cancellable `spawn_blocking` holding the session claim.
- [ ] **`Vendor` cannot script a tool call today** — `Vendor::start` answers every connection with one hard-coded SSE body (`"done"`, `finish_reason: stop`, fixed `usage`). Give it a body queue and per-call usage, lifting `remote_loop.rs`'s `sse_turn(content_deltas, tool, prompt_tokens, completion_tokens)`, which already emits the exact `delta.tool_calls[0].function{name,arguments}` + `finish_reason: "tool_calls"` shape. Without this, AC-7's thirteen-call chain and AC-10's token claim are both unwritable.
- [ ] AC-10's **cost half lives here**, against a remote `Vendor` — not `cli_e2e`, whose scripted tier is local and never metered. BUG-183 records that exact vacuity for REQ-585's AC-19.
- [ ] AC-11 is **four** legs: (a) a project skill mints and pins as a `read` would; (a2) a user skill has no root-relative identity, is `unknown`, and pins under **any** boundary — stricter, asserted separately; (b) a command that **spawned** pins, not one that `Ran`; (c) no boundary ⇒ the expansion reaches the provider.
- [ ] **Do not copy `provenance_egress.rs`'s `ran_expansion` verbatim** — it computes `ran` with `did_run` (`Ran` only) while AC-11(b)'s predicate is `spawned` (`Ran | Failed | TimedOut`). Copying it reproduces the narrower predicate the AC explicitly warns about. Fix the helper or state why not.
- [ ] AC-1's "no consent prompt raised, no dynamic command run" cannot live in `skills_discovery.rs` — that suite has no gate and no consent recorder. It belongs where a `Consent` double exists.
- [ ] Every fixture is in-repo and deterministic: no test-time read of `~/.claude`, entries sorted, and the EPERM-style legs skip under root with the skip stated.
- [ ] **AC-15's runbook**, written and marked **OUTSTANDING** — six legs for a human, recording: the Kimi window actually used (the shipped recipe's `max_context = 1000000`, or a hand-lowered `128000` — say which); **no privacy boundary configured**, which is not optional, because every ADLC skill lives under `~/.claude`, BR-10 makes a user skill's block unpinnable, and an unpinnable block pins under *any* boundary — so on a boundary-configured machine every leg routes local and the large ones are refused there. A machine that has one runs the boundary leg instead. Leg (a) records how far one prompt of `/proceed REQ-587` gets and the exact step at which it next stalls (the first "dispatch an agent"), which is the subagent spec's evidence.
- [ ] **AC-14's workspace half**: `cargo test --workspace --no-fail-fast` green, `cargo clippy --workspace --all-targets` clean, `cargo fmt --all --check` clean.
- [ ] **The measured-equals-seeded assertion at the runtime seam.** TASK-214 pinned "the frame is inside what the expander returns" as a *unit* test in `expand.rs` (`what_stage_a_measures_is_byte_identical_to_the_block_the_seed_carries`); in `runtime.rs` the identity is only *structural* — Stage A, Stage B and `CarriedTurn::begin` all read the one `SkillTurn::text`. Assert it literally in `skill_turn.rs`: the bytes handed to `skill_fit` equal the bytes `CarriedTurn::begin` seeds, for **both** callers. Structure is not a test.
- [ ] **An RPC-level pin that `skill/invoke` refuses a model-only skill.** TASK-212 gave BR-3 teeth in the daemon (`runtime.rs`'s `skill/invoke` now resolves through `dispatchable_by_user`, with a refusal naming the flag), but under the mutation that renames the resolver **without narrowing it**, `skill_turn.rs` stayed fully green — only the registry unit tests reddened. Nothing drives a `user-invocable: false` skill over the wire. Drive it, and assert the refusal names the flag.
- [ ] **AC-6's `cli_e2e` pipe leg.** TASK-219 could not reach it: the acknowledgment is raised only by a model-issued `skill` call, so no typed line drives it, and the negative pin lives at unit level (`prompter.asked == 0` with the pasted `y` still queued, so it arrives as the next prompt line). Once the tool is wired, drive it end-to-end and assert the refusal without a read.
- [ ] **The model path's `outcome_view` projection is unasserted on a non-empty outcome list.** TASK-217 had to move its AC-13 test onto a consent-free user skill (`homeonly`, no dynamic context) so that dropping `invoker` and dropping the publish reddened *different* tests — a project skill can never expand without an addressable connection, because `acknowledge_project` refuses on `invoker == None` before consulting the gate, so both mutations collapsed onto one test. The cost: nothing pins that a model invocation's dynamic outcomes project the same way the user path's do. `outcome_view` now lives in `skills/dynamic.rs` with two callers, and one projection is the whole reason their two events are the same event — so assert it on both paths with a non-empty outcome list.
- [ ] **The reroute leg has no behavioural test, and TASK-218 says exactly why.** Reaching either `skill_would_not_survive_refit` arm needs a live reroute *after* an expansion is committed: the privacy pin needs a local engine (`DaemonRuntime::minimal()` has none, so the turn breaks with `PRIVACY_BLOCKED` first), and the provider-fallback arm needs the mock `Vendor` to fail a request mid-script, which it cannot. Today the guard is pinned only *structurally* — that it reads a `Vec` refreshed before both sites. Since you are already giving `Vendor` a scripted body queue, give it `will_fail()` too and register a second provider as `fallback_id`, then drive the arm. This is the seam REQ-585 built a guard for and REQ-587 found blind; a structural pin is not evidence that it fires.
- [ ] **The reroute residual is a stated exception to BR-6/BR-9 and belongs in Deferred, in these words** (TASK-218's, verbatim-ish): a model-invoked expansion caught at a mid-turn reroute **ends the prompt turn** with `error_code::SKILL_EXPANSION_TOO_LARGE` rather than reaching the model as a relayable tool result, because both call sites sit in `run_prompt_turn`'s `'turn` retry loop *after* `run_session_turn_with_source` returned — no `ToolCall` id in scope, and the expansion already a committed block. It is neither silent nor a crash; it is a turn that ends where the spec would prefer one that continues. Closing it means giving the retry a way to fold a result back into the loop it just left.
- [ ] Mutation table per AC group, each deletion red on a named test.

## Technical Notes

- Three scripting mechanisms exist and are **not** interchangeable: `TETON_LOCAL_SCRIPT` (text-form calls, whole-CLI, never metered), `ScriptedEngine` (text-form, records every prompt handed to the engine — the only instrument for "the body reached the model verbatim"), and `ScriptedSseTransport` + `sse_turn` (the only **native** tool-call emitter, with a real ledger). Pick per AC and say which.
