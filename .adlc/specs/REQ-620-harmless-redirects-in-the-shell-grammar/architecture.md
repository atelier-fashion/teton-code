# REQ-620 — Architecture: harmless redirects, pipeline stdin, and a model-facing contract

Parent: `requirement.md`. Amends REQ-614 ADR-614-1 (the grammar) without changing its
polarity: the classifier can only become more permissive by an amount it can prove, and
every widening here is one the executor's own behaviour proves.

## Approach

Four seams, one classification.

1. **A null-redirect recogniser with one home.** `root_gate.rs` already reads `2>&1`,
   `>&2`, `>/dev/null` and `2> /dev/null` as non-writes (`has_top_level_redirection`) — a
   second, hand-rolled reading in the classifier would be the two-parser shape LESSON-494
   forbids. The recogniser moves to a shared module both gates read. *(Line numbers were
   cited here and went stale within the REQ; symbols are cited instead — the Phase-5
   re-verify's own note.)*
2. **Strip, then refuse, then split.** `classify_with_budget` today refuses on any
   unmodelled byte and then splits on `| ; & ( ) \n`. Null redirects contain `>`, `<` and
   `&`, so they must be lifted out of the command *before* both steps, as whole
   whitespace-delimited words. Everything after the strip is exactly today's grammar.
3. **Position-aware segments.** The splitter records the separator that preceded each
   segment. Only `|` makes a segment `piped`; a piped content verb with no path argument
   reads its stdin, which the previous segment already accounted for. Recursive `grep` is
   the one verb that reads the tree regardless of stdin and keeps the root-walk rule.
4. **The reason is a bit, not a derivation.** The verdict's `&'static str` reason names
   the syntax class that refused a command. It travels to the pin on the same provenance
   value the router and the choke point already share (`ToolProvenance::from_bits`), as an
   explicit `Option<&'static str>` beside the `unknown` bool — LESSON-653's rule — and
   surfaces on `session_pinned` as an additive optional field.

The model-facing contract is prose in the `shell` tool description and nothing else: no
new tool, no per-turn hint, no model-initiated lift.

## Data model changes

None on disk. Wire: `SessionPinned` gains `reason: Option<String>`
(`#[serde(default, skip_serializing_if = "Option::is_none")]`) — additive; an older client
ignores it and an older daemon omits it, so `PROTOCOL_VERSION_MIN`/`MAX` stay at 2
(release-runbook "did any wire shape change?"). `DynamicOutcomeView.reach_reason` already
exists (REQ-619) and carries the same sentences.

## Key decisions

### ADR-620-1: One recogniser for "this redirect reads nothing", shared by the write gate and the classifier

**Decision.** Extract `has_top_level_redirection`'s null-device and descriptor cases into
`harness/tools/shell_syntax.rs` as `NullRedirect::parse(word) -> Option<NullRedirect>` and
`strip_null_redirects(command) -> (String, usize)` (the residue and how many words were
lifted). The write gate and `shell_provenance::classify_with_budget` both call it; the
`/dev/null` target is consumed by the recogniser and never becomes a path token (BR-3).

**Rationale.** Two readings of `2>/dev/null` in one daemon is the LESSON-494 shape — the
day one of them accepts `2>/dev/nullx` and the other does not, one gate is wrong and no
test says which. The write gate's reading has been in production since REQ-596 and has a
benign-path table (`the_write_gate_refuses_both_triggers_and_nothing_benign`), so it is
the one to promote, not replace.

**Consequences.** The recogniser is total over the `NullRedirect` entity's forms and
returns `None` for everything else, so BR-2's look-alikes (`2>/dev/nul`, `2>/dev/null/x`,
`>$f`) fall through to the unmodelled check unchanged. The spaced form (`2> /dev/null`) is
two words; the recogniser consumes the operator word and the following `/dev/null` word
together, and an operator word with no such follower is *not* a null redirect (`2> out`).

**Amended at the Phase-5 verify (2026-09-09).** Sharing the recogniser was not enough, and
then sharing the *wrapper* was not enough either. Three corrections, in the order they were
found:

- **The gate scans the residue, not positions in the command** (M1). Both gates wrapped
  `NullRedirect`, and the wrappers disagreed: the write gate widened each top-level `>` to
  its whitespace word, while the classifier called `strip_null_redirects`, which peels a
  redirect glued to a following separator. `ls 2>&1; ls` therefore read nothing in the
  classifier and was a **write** at the gate — a regression against the pre-REQ-620 gate,
  which allowed it. The question is now asked once, of the residue: *after the strip, does
  any word still carry a `>` or a `<` outside quotes?*
- **`<` counts.** That unification makes any top-level `<` a write trigger — `cat < input`
  is refused at a home root where it used to run. It creates nothing, so the refusal
  sentence names redirection as well as creation. The gate's two directions are not
  symmetric (a false "write" costs one refused read; a false "not a write" scaffolds a
  project into `$HOME`), so the tightening is the safe way to be wrong and is recorded
  rather than papered over. `cmd>&2` and `cmd >|/dev/null` tighten for the same reason.
- **Trigger (a) reads the residue too** (the Phase-5 *re*-verify). The verb trigger was
  still reading the raw command, and `command_position_programs` is a whitespace tokenizer
  — so in `2>/dev/null rm -rf ~/x` the "program" is the redirect word and the `rm` was
  never read. Six spellings were allowed at a Home / FilesystemRoot root. The residue is
  now computed **once** in `write_gate` and handed to both triggers; the residue's first
  word is the word `sh` itself would run, which is BR-5's argument one gate over. The
  fail-closed arm keeps reading the raw command, so a command that is *only* a null
  redirect is not refused for stripping to nothing.

### ADR-620-2: Strip before the unmodelled check and before the split; the splitter records position

**Decision.** `classify_with_budget` becomes: (1) `strip_null_redirects`; (2) the
`UNMODELLED` scan over the residue, which now reports the *first* offending class in a
fixed order (`quote`, `substitution`, `variable`, `redirect`, `glob`, `brace`, `escape`,
`history`) rather than one sentence for all; (3) a splitter that yields
`(SegmentPosition, &str)` where `Piped` follows a single `|` and `First` follows anything
else (`||` is two separators and yields `First` — it is an *or*, not a pipe); (4)
`classify_segment(scope, position, segment, …)` unchanged except for BR-4's arm.

**Rationale.** Order is the whole correctness argument. Stripping after the unmodelled
check cannot work (the check refuses on `>`); stripping after the split cannot work (`&`
in `2>&1` is a separator). The position bit rides with the segment because the root-walk
rule is inside `classify_segment`, and passing a flag is the LESSON-653-shaped alternative
to re-deriving position from the text.

**Consequences.** `command_position_programs` (shell.rs) keeps its own split — it answers
a different, advisory question and REQ-614 already declined to share it. The differential
table (AC-2) pins `ls 2>&1 && echo ok` (stripped, then split on `&&`) and `ls & ls` (a
lone `&`, still two segments).

**Amended at the Phase-5 verify (2026-09-09).** Two corrections to what the strip emits,
both about a character the splitter reads:

- **The peel takes a redirect glued to a following separator** (M1). It first took a
  trailing *run* of separators only, so `2>&1|head` was not peeled — and once the write
  gate wrapped the same recogniser, `cmd 2>&1|head` was refused at a home root, which the
  pre-REQ-620 gate allowed. The head must parse as a redirect **in its entirety** and the
  tail must begin with a separator character; the tail is re-emitted as its own word so the
  splitter still sees it. The peel is **one level deep** by design: the tail is not handed
  back to the loop, so `ls 2>&1;&>/dev/null` peels once and stays `Unknown` on the
  surviving `>` — every miss lands on the pre-REQ-620 answer.
- **`&>` re-emits its `&`** (C2). `&>/dev/null` is bash and the executor is `sh -c`, which
  on the Linux CI leg is `dash`; `dash` reads that `&` as a command separator and the
  `>/dev/null` as a redirect on the command *before* it. Lifting the word whole made
  `ls &>/dev/null grep -r SECRET . 1>&2` one segment whose verb is `ls`, and the recursive
  `grep` was never verb-checked at all. Re-emitting the `&` reproduces `dash`'s parse and is
  strictly more conservative than bash's, where the same text is one command: a spurious
  separator can only split a segment in two, and two segments are each classified in full.
  `NullRedirect::BothStreams` is the only form whose residue is non-empty.

### ADR-620-3: BR-4 removes the root walk from piped readers; it does not touch the walk

**Decision.** In `classify_segment`, the root-walk condition
`reads_content && (paths.is_empty() || !named_an_existing_file) && !saw_directory` gains
`&& !(position == Piped && reads_its_default_source && reads_only_its_stdin(verb, flags))`.

**Amended at the Phase-5 verify (2026-09-09), C1 — the polarity is an allowlist.** This
ADR first specified `reads_tree`, a **denylist** of the recursive `grep` spellings, and
TASK-404 shipped it. A denylist inside an allowlist grammar is a machine for false
negatives, and this one had four: `grep --directories recurse`, its `--dir` abbreviation,
`--dereference-recursive` and `--rec` are all recursion GNU `grep` accepts, none was on
the list, and each skipped the root walk and returned `rooted` for a command that reads
every file under the root. The ADR's own claim that the enumerated forms "are the ones GNU
and BSD `grep` accept" was false as written, and the list could not have been completed by
adding rows. So the question is inverted: `reads_only_its_stdin` is true only for a closed
set of **pure filters** (`head`, `tail`, `wc`, `sort`, `uniq`, `cut`, `nl`, `tr`, `md5`,
`shasum`) and for `grep`/`egrep`/`fgrep`, and in both cases only when **no** word starts
with `--` and **no** single-`-` cluster carries `d`, `r` or `R`. Everything else — `sed`,
`awk`, `diff`, `cat`, and every `grep` flag not enumerated — keeps the walk.

**Amended again at the Phase-5 re-verify (2026-09-09) — the flag rule covers every verb,
and two names leave the list.** The inversion above left the *filter* half flag-blind:
membership was the whole test for everything but `grep`, so `wc --files0-from -`,
`sort --files0-from -` and `shasum -c -` — each of which reads a list of **paths** off its
stdin and then opens every one — took the exemption and returned `rooted`. Four
corrections. **(i)** The `--` guard is hoisted over the whole allowlist, `grep` included:
a long option is a reach nobody has enumerated, whatever verb it belongs to. **(ii)**
`shasum` and `md5` additionally refuse `-c`, the one short flag on the filter half that
opens files. **(iii)** The `-d` clause reads the character *inside* a cluster rather than
matching the whole word `-d`: `grep -nd recurse` is `-n -d recurse`, and `-nd`, `-id` and
`-drecurse` were all recursion the exemption granted. **(iv)** `less` and `more` leave the
list. They are pagers, not filters — `:e path` and `!cmd` come from the terminal and
`less` honours `LESSOPEN`, none of which this grammar can see — and a pager on the end of
a pipe is not a shape a model writes for output it means to consume.

**Rationale.** A piped `head -5` reads bytes the previous segment produced and the previous
segment was classified on its own paths; walking the root for it is a walk for a read that
cannot happen. Every miss of the allowlist lands on the pre-REQ-620 answer, which is the
property that makes the widening provable. The walk's budget and skip set stay as they are
— the requirement's third open question records the `target/` exhaustion as a separate
decision.

**Consequences.** `head -5` as a first segment still walks the root and still goes
`unknown` on a root the walk cannot finish. `cat README.md | grep -r foo` walks, and so now
do `ls | grep --color foo`, `ls | sed -r foo` and `ls | cat missing`, each of which
TASK-404 asserted the other way — and, after the re-verify, `ls | grep -nd recurse foo`,
`ls | wc --files0-from -`, `ls | shasum -c -`, `ls | less` and `ls | more`.
`ls | grep foo`, `ls | grep -H foo`, `ls | head -5` and `ls | cut -f 2` do not.

### ADR-620-4: The reason travels on the provenance value, as an explicit bit

**Decision.** `ToolProvenance::from_bits(sources, unknown: bool, out_of_root)` becomes
`from_bits(sources, unknown: Option<&'static str>, out_of_root)` — `Some(reason)` is the
unknown bit *and* its cause. The egress `Provenance` carries the same `Option`;
`TaintingPrivacySink` passes it to `TaintRegistry::mark(session, cause, reason)`; the pin
publishes `SessionPinned { reason, .. }`; `session_ui::format_session_pinned` renders it
after the cause. The skill fold (`skills::provenance::ExpansionProvenance::into_tool_provenance`,
`ContextManager::compaction_summary`) hands the same reason it already writes to
`reach_reason`. *(Line numbers were cited here and went stale within the REQ; symbols are
cited instead.)*

**Rationale.** Three readers (router, choke point, notice) and one writer. Deriving the
class again at the notice from the cause string would be a second classifier; carrying it
on the value the readers already share is one. `&'static str` is load-bearing: the reason
cannot contain command bytes because no `String` is ever built from the command (REQ-619
BR-7; the classifier's module docs).

**Consequences.** `BoundaryTouch` carries `None` — its cause is the path, already named by
`privacy_block`. A pin whose cause is `boundary_hit` renders no reason line.

### ADR-620-5: The contract is one paragraph in the shell tool description, and the prompt margin is raised to pay for it

**Decision.** `ShellTool::description()` gains one paragraph — a `const SHELL_REACH_CONTRACT`
the description and its test both read — of at most 420 bytes, naming: recognised verbs on
in-root paths stay in reach; `2>/dev/null` and `2>&1` are fine; quotes, other redirects,
globs, `$`, `~/` paths, interpreters, network clients and unrecognised verbs pin the rest
of the session to the local tier; the pin is announced and only the user lifts it.
`REDACT_BODY_OVERHEAD_BYTES` rises from 23 KiB to 24 KiB and the four derived figures
(`REDACT_TOTAL_CAP_CHUNKS`, `REDACT_INPUT_MAX_BYTES`, `REDACT_SCANNABLE_CONTEXT_BYTES`,
`REDACT_MAX_CHUNKS`) are re-derived and re-asserted; `RECORDED_PROMPT_MARGIN_BYTES` is
re-recorded.

**Rationale.** The margin is 105 bytes (`RECORDED_PROMPT_MARGIN_BYTES`, after REQ-617), and
ASSUME-043 — that the next REQ could always shorten its way in — is *invalidated*, with
REQ-617's own advice that the next claimant should raise the ceiling and re-derive rather
than borrow. This is that claimant. A contract shorter than 105 bytes could not name the
classes the model has to avoid, and a contract that names them is the point of BR-7.

**Consequences.** Every redact-scanning route's context shrinks by 1 KiB; the re-derivation
test (`the_overhead_raise_restates_the_chunk_count_and_the_scannable_bound`) is the gate.
The paragraph is description, not instruction to act on repository text (REQ-612's
framing), and is identical for typed and model-invoked turns because there is one tool.

### ADR-620-6: BR-9 is satisfied by construction and asserted by bytes

**Decision.** No new predicate. The single `Verdict` → `ToolProvenance::from_bits` value is
what both the router (`RoutePin`) and the choke point (`egress::inspect`) read today
(BUG-215, LESSON-650). AC-4 asserts the property the way LESSON-550 requires: no
`session_pinned` event *and* the next prompt's bytes captured at the mock provider.

## Test posture

Unit: one differential table in `shell_provenance.rs` (AC-2, AC-5, AC-7, AC-9) reading the
fixture roots the module already mints (`project_root`, `fixture_home`), plus one row per
`UnmodelledSyntax` class (AC-6). Integration: `provenance_egress.rs`'s `CaptureSse` for
AC-4 and AC-10; `e2e/shell_pin_shape.rs` for AC-3 and the `session_pinned.reason` shape;
`e2e/skill_provenance.rs` for `reach_reason` classes; `completion.rs`'s prompt-margin test
for AC-8; `session_ui.rs` unit for the notice. Every detection rule carries a must-not-fire
row (LESSON-440).

## Proposed additions to `.adlc/context/architecture.md`

Append to ADR-614-1 (as a bold lead-in, the house style): "**REQ-620 amends the
consequence list (2026-09-09).** A redirect to `/dev/null` or a descriptor duplication is
lifted out before the unmodelled scan and the split; a piped content verb with no path
reads stdin; the unmodelled scan names the class it refused on. Everything else in the
list stands."
