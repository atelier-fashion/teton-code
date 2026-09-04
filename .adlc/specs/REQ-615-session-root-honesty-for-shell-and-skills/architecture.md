# REQ-615 — Architecture

## The shape of the problem

REQ-583 made the session root a *fact the prompt states*. This REQ makes that
fact **hold** at the three places that override a stated fact in practice:

1. a tool result the model trusts more than the prompt (`shell: cd … && pwd`),
2. a skill body that assumes a project and quietly falls back when there is none,
3. a client that lets a `cd` typed as a prompt become a model turn.

The existing code already has every seam this needs. `RootKind` is derived per
turn by one probe (`session_root::probe`) and reaches the tools as
`ToolContext::root_kind()` and the prompt as `route.harness.session_root` — both
built in `runtime/turn.rs` from **one** `ProbedRoot`. Nothing new needs to learn
where the session is; four surfaces need to start *acting* on what they already
know.

## ADR-1: The root gates are one pure policy module, not a check per tool

`harness/root_gate.rs` is a new **feature-free, I/O-free** module over plain data.
It holds two decisions and nothing else:

- `write_gate(command: &str, kind: RootKind) -> WriteVerdict` — BR-4.
- `cd_note(command: &str, root_display: &str) -> Option<String>` — BR-2.

Both are pure functions over a `&str` and a `RootKind`, so both are table-driven
unit-testable with no filesystem, no session, and no daemon (conventions.md:
"Router policy decisions are pure functions in `teton-core` — table-driven unit
tests"; ADR "Policy is pure, mechanism is gated"). The tools call them; they call
nothing.

This is deliberately **not** a check inlined into `shell::run` and a second one
inlined into `edit::run`. BR-4 is one rule with two enforcement points, and
architecture.md's standing rule is that an invariant with more than one
enforcement point needs a sweep, not a fix. One module is what makes the sweep
possible: a structural check can assert that every write-capable tool's `run`
names `root_gate::write_gate`, which a pair of hand-inlined conditions could not
support.

### The write-verb decision has two independent triggers

The spec's WriteGate row carries both, and they are independent because a
redirection is **never a first verb**:

| trigger | test |
|---|---|
| (a) write verb | the command's first command-position word, after `env`-assignment stripping, is in `WRITE_VERBS` |
| (b) redirection | a `>`, `>>` or `>\|` appears at top level, outside single quotes, double quotes and a backslash escape |

`echo hi > ~/x` has first verb `echo`; a first-verb-only rule cannot see it. The
existing `command_position_programs` in `shell.rs` already solves half of (a) —
it walks command positions across `;`, `&&`, `||` and pipes and strips
`VAR=value` prefixes for the REQ-607 withheld advisory. **Trigger (a) reuses that
function rather than writing a second tokenizer**, which is also what makes the
gate see `cd ~ && mkdir foo`: `mkdir` is a command-position program even though
it is not the first word.

**Fail closed.** The spec's assumption is explicit: a command the tokenizer
cannot parse is treated as a write when the root is non-project. `command_position_programs`
returning an empty list for a non-empty command is exactly that state, and it
refuses.

### `cd_note` fails toward emitting

BR-2 exempts a `cd` whose target *is* the session root. Resolving that target
statically is possible for a literal (`cd /a/b`, `cd ~`, `cd .`) and impossible
for `cd "$X"` or `cd $(cat p)`. The spec's assumption settles the direction: an
unresolvable target **emits** the note. A spurious advisory line costs one line;
a missing one restores the defect.

## ADR-2: The cwd note is harness text on the result, not an event

BR-2's note rides `render_output`'s content **outside the cap**, in the slot
REQ-607's withheld advisory already occupies (between the status line and the
body). Three properties come free from that placement, and each is why it is not
somewhere else:

- It is **harness-authored and outside the untrusted frame** — command output
  cannot forge it, because the frame is written around the body and the note sits
  ahead of it (ADR-009's containment).
- It does **not count toward `raw_output_chars`**, so it cannot change whether
  the `shell` duty fires (REQ-561 ADR-5's trigger stays the length the command
  actually produced).
- A chatty command **cannot push it out**, for the same reason the withheld
  advisory cannot.

It is *not* an event. An event carrying "this command said `cd`" has no
actionable payload for any client and would be noise on the bus for a fact whose
only reader is the model reading the result it is attached to (REQ-607's rule for
the withheld advisory, applied unchanged).

## ADR-3: The environment block gains a second line, and the ceiling is re-derived from the new worst case

This is the one place the REQ collides with an existing invariant, and the
collision is load-bearing.

`environment_block_with_projects` today renders **one line** and is pinned to it
(`the environment block is ONE line; a name must not be able to add another`).
Its budget, `environment_block_ceiling()`, is **measured** by rendering the
worst-case *project* row — and the known-projects shrink loop in the non-project
branch spends against that same number.

BR-3 adds ~180 bytes of dictated ending to the **non-project** branch only. Three
consequences, and all three are the change:

1. **The block becomes at most two lines**, and the ONE-line test becomes a
   `matches('\n').count() <= 2` assertion whose *purpose* is preserved verbatim:
   a project **name** still may not add a line. The invariant was never "one
   line"; it was "user-controlled data cannot add structure". The new assertion
   states that directly, and the dictation is harness-authored constant text
   that no name can reach.
2. **`environment_block_ceiling()` is re-derived from the new worst case** — the
   larger of the worst-case project row and the worst-case *home* row (display at
   the byte bound, dictation, plus the pointer clause). Still **measured by
   calling the function that builds it**, never arithmetic: an arithmetic budget
   here would be a second derivation free to drift, which is the exact defect the
   current doc comment exists to prevent.
3. **The known-projects shrink loop is unaffected in shape.** It still adds names
   while the rendered whole stays within the ceiling; the ceiling simply now
   accommodates the dictation the home row always carries, so step 1 (names that
   fit) does not starve to step 3 (no clause).

The dictation is **data-shaped**, in the register the block already uses. It is
placed after the facts, and it is the one directive on the line — justified
because LESSON-532's rule ("a small model transfers data reliably and directives
unreliably") is an argument for *not relying* on a directive, not for omitting a
sentence that the WriteGate and the SkillGate independently enforce. The prompt
says it; the gates make it true. That ordering is the REQ.

## ADR-4: The skill gate detects statically, before any preamble runs

BR-5's compatibility path is a `!cmd` preamble that references `.adlc/`. Detection
is `skills::dynamic::scan` over `skill.body` — the **pure** scanner that already
exists — checking the resulting `Command` texts for the `.adlc/` token. Two
properties matter:

- **It scans the command texts, not the prose.** A skill whose body merely
  *mentions* `.adlc/` in a sentence is not gated. `scan` is what separates them,
  and reusing it means the gate and the runner agree on what a command is by
  construction rather than by two parsers agreeing (REQ-563's rule).
- **Nothing is executed.** The spec's own Description is the argument: running the
  preamble to find out whether the preamble should run is the harm. `scan` is
  pure and touches no filesystem.

The refusal lands in `SkillTool::invoke` as a new `Refusal::NeedsProject`,
positioned **after `resolve_for_model`** (a file must be known before its body can
be scanned) and **before `acknowledge_project` and `expand_and_fold`**. That
position is what makes "no model turn is spent on the body" structural: the
expander is never reached, so there is no expansion to budget, no dynamic command
to consent to, and no body to fold.

Frontmatter gains an optional `requires: project`. It is the forward path; the
`.adlc/` token is the compatibility path for the shipped ADLC skills, exactly as
the spec's assumption records.

**Only `home` and `filesystem_root` refuse** (OQ-2, resolved). A `plain` root may
be a project-to-be and `/init` must run there.

## ADR-5: `known_projects` reaches the tools the same way it reaches the prompt

Both refusals name known projects. `runtime/turn.rs` already builds that ranked,
bounded list once and hands it to `route.harness.known_projects`; `ToolContext`
gains `with_known_projects(...)` fed from **the same expression**, beside the
existing `.with_denied_prefix(...)`. One reading, two consumers — the pattern the
probe itself established, and the reason the jail's display and the block's
display cannot disagree today.

It is a builder on `ToolContext` rather than a new parameter to `for_root`
because every existing `ToolContext::for_root` call site (tests included) must
keep compiling with an empty list, which is also the honest value for a context
nobody gave projects to.

## ADR-6: The preamble fallback is observable only because the daemon splits the `||`

The spec's own System Model records why: handing `cat X || echo none` to one
shell returns exit 0 and the fallback's stdout, byte-identical to a primary that
succeeded. The daemon cannot observe the branch it did not run.

So `dynamic::run_one` splits `command.as_str()` at its **top-level `||`** (outside
quotes, and not `|`), runs the primary through `run_bounded`, and on a non-zero
exit runs the remainder. `DynamicOutcome::Ran` gains `fell_back: bool`.

Three constraints on the split, each a correctness property:

- **Top-level only.** A `||` inside quotes is not a separator. The same
  quote-aware scanner trigger (b) uses does this job; it is written once in
  `root_gate.rs` and used by both.
- **First separator only.** `a || b || c` splits into primary `a` and remainder
  `b || c`, which the shell then evaluates itself — so the semantics of a chain
  are the shell's, unchanged, and only the *first* branch's exit is observed.
- **No `||` means no change.** The command runs exactly as today and can still
  report a fallback by exiting non-zero (`Failed`), which is the pre-existing
  path.

The harness prefix line is written where the fold renders the outcome, not where
the command ran — the sanitize-and-frame-at-the-authoring-layer rule (LESSON-477).

## ADR-7: The `cd` prompt hint is a pure predicate, gated on `typed_input` at the loop

BR-7 belongs in `slash.rs` as `cd_as_prompt_hint(line) -> Option<&'static str>`,
a pure predicate over the trimmed line matching `^cd(\s+\S+)?\s*$`, and is called
from `main.rs`'s entry loop **only when `ctx.typed_input`** — the flag already
read once at the edge as `IsTerminal::is_terminal(&stdin())`, and the same gate
REQ-584's writing commands use.

It is *not* a new `Input` variant on `classify`. `classify` is pure and knows
nothing about how a line arrived; making the piped exemption a classifier concern
would put a terminal fact inside a function whose whole value is that it has
none. The predicate is pure and testable with no terminal; the gate is one `if`
at the one call site that holds `typed_input`.

**`//cd …` needs no new code.** `classify` already strips one `/` and returns
`Input::EscapedPrompt(rest)` for a leading pair, so `//cd /teton-code` sends
`/cd /teton-code` as prompt text — which is exactly what AC-6 pins.

## ADR-8: The composed prompt is measured last, and the margin pins move in that diff

`REDACT_BODY_OVERHEAD_BYTES`'s ledger, `RECORDED_PROMPT_MARGIN_BYTES` (733) and
`RECORDED_WEB_PROMPT_MARGIN_BYTES` (780) are `assert_eq!` pins that fail on any
resident-prompt edit. This REQ moves the prompt twice — BR-1's tool-description
sentence and BR-3's block dictation — and per architecture.md a tool description
is a **production input** to that overhead.

So the measurement task runs **after every task that writes to the prompt**
(REQ-612's rule, LESSON-541), re-measures rather than reasons, and moves both
pins in one diff with a ledger line naming this REQ. If the 733 bytes of margin
do not cover both sentences, the *decision* is to shorten one — not to raise the
overhead, which is a whole-KiB move with a scannable-bound consequence that
belongs to its own REQ.

**REQ-617 moves the same two pins concurrently.** Whichever of the two merges
second re-measures after the rebase; a pre-rebase figure is stale by
construction.

## What does not change (BR-9)

With `kind == Project`: no gate fires, no dictation is emitted, the environment
block renders the same single line it renders today, and the shipped ADLC skills
expand exactly as they do now. BR-1's sentence and BR-2's note are the only two
things a project session sees, and both are stated in the spec as universal. The
existing skill-expansion tests are the pin; they are not edited.

## Task graph

```
TASK-001 (protocol events) ─┬─> TASK-003 (shell: BR-1, BR-2, BR-4a)
TASK-002 (root_gate policy) ┘   
                            ├─> TASK-004 (edit: BR-4b)
                            ├─> TASK-006 (skill gate: BR-5)
                            └─> TASK-007 (preamble fallback: BR-6)
TASK-005 (env block: BR-3) ─────────────────────────────────────┐
TASK-008 (projects line + cd hint: BR-7, BR-8) ─────────────────┤
                                                                 └─> TASK-009
                                                       (measure composed prompt,
                                                        BR-9 regression, AC-8)
```

TASK-009 depends on **all** of 003, 004, 005, 006, 007, 008 — it is the task that
measures a composed artifact, and it must run after every task that writes to it.
