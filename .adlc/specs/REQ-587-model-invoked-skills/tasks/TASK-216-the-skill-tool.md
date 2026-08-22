---
id: TASK-216
title: "The `skill` tool: a roster rendered once, a typed refusal, and a per-turn cap"
status: complete
parent: REQ-587
created: 2026-08-20
updated: 2026-08-20
dependencies: [TASK-211, TASK-212, TASK-214]
---

## Description

BR-1, BR-2, BR-6, BR-11, BR-12. The tool itself: pure where it can be, and
producing a value the loop decides about (ADR-1).

## Files to Create/Modify

- `crates/tetond/src/harness/tools/skill.rs` (new) — `SkillTool`, `SKILL_TOOL_NAME`, `register_skill_tool(...) -> bool`, the roster renderer, the typed refusal, the per-turn cap state
- `crates/tetond/src/harness/tools/mod.rs` — `pub mod skill;` and the cap-exempt reason table (ADR-10)

## Acceptance Criteria

- [x] **The roster is rendered at construction and stored** — `description: String`, returning `&self.description`. `Tool::description` returns `&str` borrowed from `&self`, so this needs no trait change. Do **not** reach for a `OnceLock` or a leaked `&'static str`: both make the roster per-**process** rather than per-**registry**, so `/cd` would leave the model reading the previous root's skills. Names only (OQ-5), bounded, with an `… and N more` collapse and its own pinned char cap.
- [x] `register_skill_tool(...) -> bool` expresses the condition **once, inside itself**, on the `register_web_tool` precedent, and is never called from `with_builtins()`. **TASK-217 wires the call site**, because `build_tools` has neither the registry nor the invoker until that task adds them — this task ships the function and its condition, unit-tested against a registry built in the test. That is forced twice over: BR-2 requires absence when no skill is model-invocable, and `docs_are_capped_by_max_tools_for_degraded_providers` asserts `exposed_names(None)` by **equality**.
- [x] Cap-exempt, with its distinct stated reason: *the only path to text outside the jail, whose opt-in is the install*. `a_cap_exempt_tool_is_never_displaced_by_the_max_tools_cut` asserts a count of 7 with a message re-stating the arithmetic — it becomes 8 and the message moves with it.
- [x] **ADR-10's mechanism, or AC-17 asserts a property nothing enforces.** A declared `&[(&str, &str)]` table of `(tool, reason)` cross-checked against the registry, in the `RESERVED_SKILL_NAMES` shape. The self-gating pin becomes a table too — amending `gates_itself() == (name == WEB_TOOL_NAME)` to name two tools *relaxes* it, which is the complaint.
- [x] **AC-4's exposure arithmetic**, asserted at three caps: under `Some(5)` the exposed set is the five capped built-ins plus `teton_docs` plus `skill` (plus `web` when opted in); under a strong profile (`None`) everything; under `Some(0)` the exempt tools are **still** exposed, because the cap bounds only non-exempt registrations.
- [x] **AC-14's purity half**: the roster renderer, the flag view, the frame renderer and the cap/repeat decision each have a unit test with no daemon, no gate and no pty.
- [x] Arguments are `skill { name, args }` (OQ-2) — `args`, not `arguments`, because the local tier's text form otherwise nests as `arguments.arguments`, which a weak model fumbles.
- [x] Per-turn cap **12** (OQ-7) and the repeat rule: every call counts including refusals, only expansions seed the repeat rule. Pinned against an **in-repo fixture**, never a test-time read of `~/.claude` — a cap measured against the developer's machine is LESSON-540's class.
- [x] **AC-12's missing assertion (ADR-12):** a `user-invocable: false` skill **resolves for the model** through `invocable_by_model`, not merely that it is listed. No AC asserted a successful model invocation of a model-only skill, which is how B-4's arm would have shipped green.
- [x] Provenance is set **explicitly** (ADR-8): `Sources` for a project skill, `Unknown` for a user skill. `ToolOutcome::ok` defaults to `Sources(∅)`, which for a skill body is **fail-open** — a user skill has no root-relative identity and would egress under any boundary.
- [x] The roster renderer, the flag parser's view, the frame renderer and the cap/repeat decision are **pure functions**, unit-testable with no daemon and no gate (BR-12).
- [x] **BR-4's frame delimiter joins the neutralizer alphabets in `harness/render.rs`** (moved here from TASK-211, 2026-08-21 — that task could not meet it, the frame did not exist yet). Whatever opening/closing marker the frame renderer writes must join `neutralize_envelope_tags`'s `UNTRUSTED_ENVELOPE_TAGS` **or** the flat/ChatML anchored markers, or `the_input_alphabet_covers_every_output_marker` reddens. ADR-009 is two-sided: a marker the harness writes is a marker the harness must be able to defuse. AC-2's planted-marker legs (the frame's own closing tag, `<tool-result>`, `User:`, `Assistant:`, `<|im_start|>`) are TASK-222's behavioural half of the same obligation.
- [x] **The cheap-reject trap in `starts_with_frame_label` (`render.rs:383`).** It opens with `matches!(line.as_bytes().first(), Some(b'U' | b'A' | b'T'))` — every *existing* transcript label happens to start with one of those bytes. A **prose** frame label beginning with any other letter is silently skipped even after being added to the marker sets, so `the_input_alphabet_covers_every_output_marker` passes while the defuser never fires. Either choose a `<`-prefixed tag (which routes through `starts_with_envelope_tag` and is unaffected) or widen the cheap reject — and pin the choice with a test that plants the closing marker flush-left in a skill body and asserts it arrives defused.
- [x] **Re-verify `harness/docs/skills.md` against what this task actually ships, and state BR-2's conditional registration there.** TASK-220 amended that topic on 2026-08-21 to six decisions this task had not yet made — it asserts the `args` key (OQ-2, *not* `arguments`) and BR-4's "instructions, not data" frame. If either resolves differently here, the topic and two of its needles move with it. Separately, TASK-220 left one claim **unstated** for want of room: the topic reads as though `skill` is always present, when BR-2 registers it only if at least one skill is model-invocable — so a model on a machine with no model-invocable skills is told about a tool it does not have. Say it, and pay for it by cutting elsewhere: the topic is **4,080 of 4,096**, sixteen bytes, and the clause costs about fifty-five. Do not raise `MAX_TOPIC_BYTES` and do not delete the ceiling sweep.
- [x] **`READ_ONLY_TOOLS` carries `"skill"` as a bare literal** (TASK-215 had to — `SKILL_TOOL_NAME` ships with *this* task, and the comment there says so). Swap in the constant so the registry's name and the permission row cannot drift, and pin that they are the same value — nothing does today.
- [x] Mutation: rendering the roster per call, registering inside `with_builtins`, defaulting the provenance, and dropping the cap each fail a named test.

## Technical Notes

- The tool returns `ResultDisposition::Expansion` for an expansion and **`UntrustedData`** for the roster, `unknown_skill`, and every typed refusal — **not `Data`**. `Data` means "classify by the tool's name", and `skill` is deliberately pinned *out* of `UNTRUSTED_OUTPUT_TOOLS`, so `Data` would leave file-authored `description`/`argument-hint` text from a cloned repo reaching the model as unframed harness prose. That is the failure ADR-1's own argument names; the third enum value exists precisely to avoid reproducing it. It performs **no** budget check — TASK-218 owns that, for ADR-2's reason.
- Interior mutability for the per-turn cap is naturally per-prompt: the registry is rebuilt every turn by `build_tools`.
