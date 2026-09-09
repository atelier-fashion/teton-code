---
id: REQ-620
title: "Harmless redirects and pipeline stdin in the shell provenance grammar, and a model-facing contract for what pins"
status: approved
deployable: true
created: 2026-09-09
updated: 2026-09-09
component: "daemon/egress"
domain: "privacy"
stack: ["rust", "daemon"]
concerns: ["privacy", "routing", "developer-experience"]
tags: ["shell", "shell-provenance", "redirect", "dev-null", "unknown-shell", "local-pin", "session-pinned", "classifier", "grammar", "pipeline", "tool-description", "model-contract", "req-614", "req-619"]
---

## Description

REQ-614's shell provenance classifier decides, before a `shell` command runs,
whether everything it could read is inside the session root. Its grammar is
deliberately not a shell lexer: any command containing one of
`' " ` $ \ > < { } ! * ? [` is refused whole, before any verb is read
(ADR-614-1), because a matcher that tried to model quoting would start
guessing. The refusal is `unknown`, and an `unknown` shell result in a
session's context pins the session to the local tier for the rest of that
session, liftably, by a user typing `/shell allow` (REQ-614 BR-2, BR-5).

That rule was written for commands a user types. Most shell commands a
*model* writes contain a redirect: `2>/dev/null` to keep a probe quiet,
`2>&1` to see errors, `>/dev/null` to test an exit status. On 2026-09-09 the
first shell call the remote model made during a `/analyze` turn was

    ls .adlc/context/architecture.md .adlc/context/conventions.md 2>&1; echo ---; ls .adlc/ 2>/dev/null; echo ---; ls .adlc/partials/ 2>/dev/null | head; echo ---; ls tools/lint-skills/ 2>/dev/null; echo ---; which adlc-read; ls ~/bin/adlc-read 2>/dev/null

Six `ls` calls, an `echo`, a `which`, and nothing outside the root but a
`~/bin` probe. The session pinned on the `2>&1`, every later turn in the
session ran on the 7B local model with a 63 KB budget, the pin survived a
`/cd`, and the user's next `/analyze` was answered wrongly by the small model.
The pin was correct by the grammar and wrong by the charter: nothing in that
command could have read a protected byte. Every agentic remote turn will pin
on its first such call, and the model, which has never seen the grammar,
cannot avoid it. `/shell allow` is user-typed only (REQ-614 BR-5), so the
remedy is a treadmill the user runs on every session.

This requirement does two things and declines a third. First, it teaches the
grammar the redirect forms that provably read nothing — a redirect to
`/dev/null` and a file-descriptor duplication — and the fact that a
content-reading verb in a non-first pipeline position with no file argument
reads the previous segment's output, which was already classified, not the
root. Second, it tells the model the grammar: the `shell` tool's description
states what keeps a command in reach and what pins the session, and the pin
notice names the syntax class that tripped it, so both the model and the user
can act on the cause. It does not widen anything else: quoting, globs,
variables, substitution, redirects to or from files, and paths outside the
root stay `unknown` exactly as REQ-614 left them.

The toolkit side of the same lesson landed as adlc-toolkit BUG-220: skill
preambles are authored text and could be rewritten inside the grammar. Model
output cannot be rewritten, so the grammar has to meet it.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| NullRedirect | form | enum | one of: `[n]>/dev/null`, `[n]>>/dev/null`, `&>/dev/null`, `</dev/null`, `[n]>&m` (n, m single digits); attached as one word, or the operator word immediately followed by a separate `/dev/null` word |
| NullRedirect | reach | — | reads nothing and writes nothing the model sees; contributes no path token and no boundary evidence |
| UnmodelledSyntax | class | enum | `quote`, `redirect`, `substitution`, `variable`, `glob`, `brace`, `escape`, `history` — the class a refused command's reason names, never the text |
| SegmentPosition | position | enum | `first` (stdin is the terminal or nothing) or `piped` (stdin is the previous segment's stdout) |
| ShellToolContract | text | string | the `shell` tool description the model receives; states the grammar in one paragraph (BR-7); identical for typed and model-invoked skills (REQ-619 BR-6) |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| `session_pinned` | unchanged (REQ-614) | `cause: unknown_shell` gains `reason`: the content-free sentence already carried by `skill_invoked.reach_reason`, now naming the `UnmodelledSyntax.class` where that is the cause |
| `skill_invoked.outcomes[].reach_reason` | unchanged (REQ-619) | names the syntax class for the unmodelled arm ("the command uses a redirect this classifier does not model") instead of the single sentence for every class |

No event carries command text, a path, or output (REQ-619 BR-7, unchanged).

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| lift an `unknown_shell` pin | user, by typing `/shell allow` — unchanged (REQ-614 BR-5); the model gains no lift |

## Business Rules

- [x] **BR-1: A redirect to `/dev/null` or a descriptor duplication is stripped before the segment is classified, and the rest of the segment is classified exactly as today.** The forms are the `NullRedirect` entity's, and no others: `2>/dev/null`, `>/dev/null`, `1>/dev/null`, `2>>/dev/null`, `&>/dev/null`, `</dev/null`, `2>&1`, `1>&2`, each as one whitespace-delimited word, or as the operator word (`2>`, `>`, `>>`, `&>`, `<`) immediately followed by the separate word `/dev/null`. The stripped segment's verdict is `rooted`, `boundary_touch` or `unknown` by REQ-614 BR-1 unchanged — a redirect never adds reach and never removes it (informed by REQ-614, LESSON-653). **The forms are recognised as whole words before the command is split into segments**: `2>&1` and `&>/dev/null` contain `&`, which the grammar otherwise reads as a segment separator, and a splitter that saw them second would classify `2>` and `1` as verbs. `&&`, `||`, a lone `&`, `;` and `|` remain separators exactly as REQ-614 ADR-614-1 has them (informed by LESSON-494). **`&>/dev/null` lifts to a bare `&`, not to nothing** (added at the Phase-5 verify, C2): `&>` is bash, the executor is `sh -c`, and `dash` — `/bin/sh` on the Linux CI leg — reads that `&` as a command separator, so re-emitting it reproduces the parse of the shell that grants the *least* reach and is strictly more conservative than bash's.
- [x] **BR-2: Every other use of `>` or `<` stays unmodelled.** `> out.txt`, `>> log`, `< input`, `2>/dev/nul`, `2>/dev/null/x`, `2>/dev/nullx`, `2>$f`, `> "$f"`, a here-doc, and process substitution all refuse the whole command as `unknown`, as REQ-614 left them. Widening beyond `/dev/null` and descriptor duplication is a separate requirement (informed by REQ-614, LESSON-494).
- [x] **BR-3: `/dev/null` in a stripped redirect is not a path token.** It never reaches path resolution, never counts as an out-of-root touch, and never matches a boundary glob — the same seam LESSON-623 named: a path that is not a file access must not be scored as one (informed by LESSON-623, REQ-571).
- [x] **BR-4: A content-reading verb in a `piped` segment that names no existing file reads its stdin, not the root.** A segment is `piped` only when the separator before it is `|`; after `;`, `&&`, `||` or `&` its stdin is the terminal's and it is `first`. `ls src | head -5`, `git log | wc -l`, `cat README.md | grep foo` classify the piped segment as reading the previous segment's output, which was itself classified, so no root walk runs. **The exemption is a closed allowlist** (reworded at the Phase-5 verify, C1; it was first stated as a denylist of the recursive `grep` spellings, and `grep --directories recurse`, `--dir recurse`, `--dereference-recursive` and `--rec` all fell through it): it is granted only to the pure filters `head`, `tail`, `wc`, `sort`, `uniq`, `cut`, `nl`, `tr`, `md5`, `shasum`, `less`, `more`, and to `grep`/`egrep`/`fgrep` when no word starts with `--`, no word is `-d`, and no single-`-` cluster carries `r` or `R`. Every other content verb — `sed` and `awk` (whose `-f` takes a script that can open any path), `diff`, `cat` — and every unenumerated `grep` flag keeps today's root-walk rule. A `first` segment is unchanged: it reads the root (REQ-614 BR-1). **"No path argument" means "names no existing file"** (reworded at TASK-404 to match what shipped): the exemption is expressed over BR-1(d)'s own question — *was this verb handed explicit files?* — because a **pattern word is not a path**. `git log | grep fix` and `cat README.md | grep foo` name a pattern, and the second is listed above among the shapes that read their stdin; a file argument that does not **exist** is the same case *for a verb on the allowlist* (`ls | grep missing`), since the classifier cannot tell it from a pattern and BR-1(d) already scores the two alike. `ls | cat missing` is **not** that case and walks: `cat`'s operand is a path and never a pattern, so a `cat` whose file does not exist is a typo the walk should still account for. A piped verb that *did* name an existing file never reaches the walk at all.
- [x] **BR-5: A `first` or `piped` segment whose verb is opaque is `unknown` regardless of any redirect.** `python x.py 2>/dev/null` and `curl … >/dev/null 2>&1` pin exactly as before. **The order is the reverse of what this rule first stated, and every observable is unchanged** (reworded at TASK-403 to match ADR-620-2): the strip in BR-1 runs **first** — before the unmodelled scan and before the split, therefore before any verb is read — because the scan refuses on `>` and the splitter reads the `&` in `2>&1` as a separator, so a strip that ran second could not work at all. It cannot change the verb the opaque check reads: it lifts only whole whitespace-delimited words that are redirects in their entirety, and a redirect word is never a verb, so the first non-redirect word of each segment is the same word `sh` would take as the command (which is also why a *leading* redirect, `2>/dev/null cat secrets/prod.env`, still classifies on `cat` — AC-3). The opaque-verb verdict, its reason, and the boundary precedence of BUG-216 are what they were.
- [x] **BR-6: The pin carries the classifier's reason.** For a command the unmodelled scan refused, that reason is the `UnmodelledSyntax.class` sentence — one content-free sentence per class ("the command uses a quoted string this classifier does not model", "…a redirect other than to /dev/null…", "…a glob…"), and the class is the first present in a fixed order, never the first to appear in the command. For every **other** `unknown` verdict the reason is the verdict's own content-free sentence, which names no class because no syntax refused it: "the command's verb is not one this classifier recognises", "a path argument resolves outside the session root", "the command reads the root and it could hold a protected file", and the rest. *(Reworded at the Phase-5 verify: "the reason names the syntax class, and only the class" was true of the eight class sentences and false of the seven other refusals a pin can carry.)* It rides `skill_invoked.reach_reason`, the `session_pinned` event, and the CLI's pin notice. The sentence contains no byte of the command (REQ-619 BR-7; egress-capture posture of LESSON-624). **One seam records the class and publishes nothing** (recorded at the Phase-5 verify, M6): REQ-614's carry-seam backstop (`CarriedTurn::commit_now` → `runtime::context_taint_cause`) runs from `Drop`, holds no `SessionEvents`, and prints only `taint_pin_line`'s pre-REQ-620 sentence on stderr. The class it records is read back by the *next* turn's notice, off `SessionTaint::reason`. That shape is REQ-614's and is unchanged here; it is written down because "the reason rides the pin to the notice" is otherwise read as "every pin publishes an event".
- [x] **BR-7: The `shell` tool's description states the grammar to the model.** One paragraph the model receives with the tool: commands stay in reach when they use recognised verbs on paths inside the session root; a redirect to `/dev/null` and `2>&1` are fine; quotes, other redirects, globs, `$`, `~/` paths, interpreters and network clients, and an unrecognised verb pin the rest of the session to the local tier; a pin is announced and only the user can lift it. The paragraph is one constant the description and its test both read (informed by BUG-214's Fix B, REQ-619 BR-6).
- [x] **BR-8: A boundary read is never hidden by a redirect.** `cat secrets/prod.env 2>/dev/null` is `boundary_touch` and pins permanently as `boundary_hit`; the inspector's ordering from BUG-216 holds unchanged (informed by BUG-216).
- [x] **BR-9: Both readers of the verdict see the same verdict.** The router's pin decision and the egress choke point's inspection consume one classification of the command, so a command this grammar clears is neither pinned at routing nor blocked at egress (informed by BUG-215, LESSON-650).
- [x] **BR-10: The verdict is still decided before the command runs, from its text alone.** No exit status, output, or filesystem effect of the redirect participates (REQ-614 BR-8, BR-10 unchanged).

## Acceptance Criteria

- [x] AC-1: The 2026-09-09 command above, with its final `ls ~/bin/adlc-read 2>/dev/null` segment removed, classifies `rooted`. With that segment present it classifies `unknown` with the reason naming a path outside the session root — not a redirect.
- [x] AC-2: A differential table (LESSON-494) holds every BR-1 form alone, attached and space-separated, on `ls`, `cat README.md`, `git status`, `test -s x`, and `echo hi`, each `rooted`; and every BR-2 look-alike on the same verbs, each `unknown` with **the class the fixed order picks first**, which is not always the redirect: `2>$f` draws the variable class and `> "$f"` the quote class, because both spell a class that outranks `Redirect` in the order (reworded at the Phase-5 verify — "each `unknown` with the redirect-class reason" was false of exactly those two look-alikes, which the shipped table already asserted correctly); and the `&`-bearing forms beside the separators they must not be mistaken for — `ls 2>&1 && echo ok`, `ls &>/dev/null || echo no`, `ls 2>&1; ls` — each `rooted`, with `ls & ls` still two segments. The table is one fixture read by the classifier's unit test.
- [x] AC-3: `cat secrets/prod.env 2>/dev/null` and `2>/dev/null cat secrets/prod.env` are `boundary_touch` on a machine whose boundaries cover that path; the session pins `boundary_hit`; `/shell allow` does not lift it (BR-8).
- [x] AC-4: End to end, on a route bound to a mock remote provider: the model's first tool call is `ls src 2>/dev/null && echo ok 2>&1`. No `session_pinned` event is published, the next prompt's `route_decided` names the provider, and the mock provider receives that prompt's bytes (assert the absence of the pin and the presence of the bytes — LESSON-550, LESSON-650 — not the classifier's return value).
- [x] AC-5: On a fixture root holding a boundary-matching file, `ls src | head -5` is `rooted` (no root walk) where before this REQ it was `unknown` "reads the root"; `ls src | grep -r foo` remains `unknown` on that root (BR-4's `grep -r` exception); `head -5` alone remains `unknown` on that root (a `first` segment reading the root).
- [x] AC-6: One test per `UnmodelledSyntax.class` asserts the reason sentence, that the same sentence appears on `skill_invoked.reach_reason` for a preamble with that syntax, on `session_pinned` for a shell call with it, and in the CLI notice — and that a marker planted in the command text reaches **no event other than the two that quote the command by design**: the `tool_call` session update, whose title is how the user is shown what the model asked to run, and `permission_request`, which asks about it. Those two are the client-facing pair REQ-611 BR-4 draws the line around, and the test names them (`MAY_QUOTE_THE_COMMAND` in `crates/tetond/tests/e2e/shell_pin_shape.rs`) rather than asserting a blanket absence that the surfaces would falsify. The privacy chain — `session_pinned`, `privacy_block`, `provenance_rejected` — is asserted by name to carry none of it, and the carrier list is asserted non-empty so the check cannot pass vacuously on a marker that never arrived. *(Reworded at TASK-405; the original "found in no event and no provider request" was false of the pair above and of a provider request that carries the model's own tool call back.)*
- [x] AC-7: `python x.py 2>/dev/null`, `curl example.com >/dev/null 2>&1`, and `sh -c ls 2>/dev/null` are `unknown` with the opaque-verb reason (BR-5).
- [x] AC-8: The `shell` tool description, as serialised into the tool list a provider receives, contains the BR-7 paragraph; a test pins that it names `/dev/null`, `2>&1`, quotes, globs, `$`, `~/`, and "local tier", and that the same text is served for a typed and a model-invoked turn.
- [x] AC-9: The toolkit-preamble classifier test (`the_toolkit_preamble_shapes_are_rooted_and_the_old_ones_are_not`) and every REQ-614/REQ-619 classifier test stay green unchanged, except the rows that asserted `2>/dev/null` alone made a command `unknown`, which flip and say why.
- [x] AC-10: `teton doctor` and the routing notice after a cleared command show the remote provider; the transcript's `route_decided.reason` for that turn does not mention a pin.

## External Dependencies

- None.

## Assumptions

- A tool description changes model behaviour often, not always: BR-7 is the best-effort half and BR-1/BR-4 are the load-bearing half. If the description alone were enough, BR-1 would be unnecessary; it is not, because a model that has read the contract still writes `2>/dev/null` from habit.
- `/dev/null` exists and is the null device on every platform the daemon ships for (macOS, Linux). A Windows port would revisit BR-1's spelling.
- Models write the attached form (`2>/dev/null`) far more often than the spaced form (`2> /dev/null`); both are modelled because the cost is one extra table row and the executor accepts both (LESSON-494's shared-parser rule, met by enumerating exactly what `sh` accepts for these forms).
- The boundary walk's budget and its skip set are unchanged; BR-4 removes the walk from piped readers rather than making the walk survive a Rust `target/`. Whether the boundary scan should prune build output is a separate question (see Open Questions).

## Open Questions

- [ ] Should `< file` be modelled as a content read of `file` (a `READS_CONTENT` path token) rather than staying unmodelled? It is provable, but rare in model output; deferred unless a transcript shows it pinning.
- [ ] Should `> file` and `>> file` inside the root be modelled as writes that read nothing? A write cannot leak, but it can clobber, and the classifier has never reasoned about writes; deferred.
- [ ] The boundary scan that decides whether a directory read could reach a protected file is bounded by an entry count and does not prune build output, so on this repository's main checkout (about 741k entries under `target/`) any `first`-segment reader with no file, and any content verb naming a missing file, is `unknown` by exhaustion rather than by reach. Is that a REQ-614 amendment (prune build output from the boundary scan, or raise the budget) or accepted? Out of this REQ's scope; recorded here so it is not lost.

## Out of Scope

- Quoting, globs, `$`, `$(…)`, backticks, backslashes, braces, `!`, here-docs, process substitution — all stay unmodelled (ADR-614-1).
- Redirects to or from files, and `<` from anything but `/dev/null`.
- A model-initiated or automatic lift of an `unknown_shell` pin (REQ-614 BR-5 stands).
- Any change to which verbs are opaque, name-only, or content-reading beyond the pipeline-position rule in BR-4.
- The boundary walk's budget or skip set.
- Windows.

## Deferred

Four things this REQ knowingly did not do. None is a gap in what it claims;
each is recorded so the next reader does not rediscover it as a defect.

- **A *typed* `/skill` pin carries no syntax class — BUG-223 (open, low).**
  BR-6's sentence reaches `session_pinned.reason` for a `shell` call and for a
  model-invoked skill, whose results are `Provenance::Tool` blocks carrying the
  reason on the value. A typed `/skill` expansion enters as `Provenance::User`,
  which carries `sources`, `unknown` and `boundary_touch` and **no reason
  field**, so its pin renders the pre-REQ-620 notice. The class is still
  published that turn on `skill_invoked.outcomes[].reach_reason` (REQ-619
  BR-7) — the surface a `shell` call has no equivalent of, which is why the pin
  carries the reason for that path at all. Closing it means a fourth field on
  the `User` variant threaded through the three seams REQ-619 ADR-619-3 pins,
  each needing its own test; the `CtxProvenance::User` arm in
  `crates/tetond/src/harness/completion.rs` says so in a comment at the seam.
- **A redirect glued to its *verb* stays `unknown`, by design.**
  `ls>/dev/null` is a word the recogniser does not accept, so the unmodelled
  scan sees its `>` and refuses the command exactly as before REQ-620. A
  redirect glued to a following **separator** *is* peeled (`ls 2>&1; echo`,
  `ls 2>&1;ls`, `ls 2>&1|head`), because that peel is decidable without lexing:
  the head has to parse as a redirect in its entirety and the tail has to begin
  with a separator character, and the tail is re-emitted as its own word so the
  splitter still sees it. *(Widened at the Phase-5 verify, M1: the peel took a
  trailing **run** of separators only, so `2>&1|head` was not peeled and the
  write gate — which now wraps the same recogniser — refused `cmd 2>&1|head`
  at a home root, which the pre-REQ-620 gate allowed.)* The spaced form is
  stricter still: the operator word must be bare, so `>|`, `2>&` and `2>;`
  never lift a following `/dev/null`. Every miss lands on the old answer, which
  is the property that keeps BR-1's widening provable; relaxing the whole-word
  rule is what mutation 2 in `shell_syntax.rs`'s record shows going red, and
  relaxing the spaced arm is mutation 6.
- **`awk -f FILE` and `sed -f FILE` stay `rooted` where their script file
  exists — a follow-up, not a REQ-620 regression.** Both read a *program* from
  that file, and a program can open any path on the machine; the classifier
  scores the `-f` operand as an ordinary existing file and asks nothing about
  what it says. This is pre-existing REQ-614 behaviour, unchanged by anything
  here (REQ-620's C1 only stops `sed`/`awk` from taking the **piped** stdin
  exemption). Closing it means either treating an interpreter's script operand
  as opaque — which is BR-1(e)'s reading one argument further in — or moving
  `sed` and `awk` to `OPAQUE`, and both are widenings of the *denylist* that
  want their own requirement and their own benign table.
- **AC-1's literal command is discharged at the unit level.** TASK-403's
  `shell_provenance::tests::the_2026_09_09_command_is_rooted_without_its_home_probe`
  asserts both halves against fixture roots the module mints; TASK-407 records
  why it is not re-driven end to end (an e2e would need a fixture repo carrying
  `.adlc/`, `tools/lint-skills/` and a `$HOME` with `bin/adlc-read`, and would
  assert a grammar answer through six layers). The end-to-end suite drives the
  two-form reduction, `ls src 2>/dev/null && echo ok 2>&1`, and
  `docs/manual-verification.md` carries the literal command as a hand check.

## Retrieved Context

- REQ-619 (spec, score 20): Proportionate skill provenance — a skill pins the session only when its body or its preamble could have touched a protected file
- REQ-614 (spec, score 19): Proportionate shell provenance — a shell result pins the session to the local tier only when it could have read a protected file
- BUG-216 (bug, score 13): The egress inspector reports the unknown-provenance sentinel before a matched boundary source
- BUG-214 (bug, score 12): A typed `/skill` on a boundary-configured machine pins the session permanently and silently
- LESSON-653 (lesson, score 11): A derived property standing in for a bit the type does not carry will leak — add the bit
- BUG-215 (bug, score 11): `/shell allow` moves the route but not the egress verdict
- LESSON-624 (lesson, score 11): An egress-leak marker must live only in the file's bytes
- LESSON-623 (lesson, score 10): A boundary glob cannot protect a path the provenance seam never names
- LESSON-550 (lesson, score 10): A defect fixed once comes back unless a test asserts the absence, not the remedy
- REQ-571 (spec, score 10): Canonical provenance identity for privacy-boundary enforcement
- LESSON-432 (lesson, score 10): Provenance must derive from what a tool touches, not from an argument name
- LESSON-650 (lesson, score 9): A lift composed into one predicate still has to reach every reader of the fact it lifts
- REQ-596 (spec, score 9): A credential-safe environment for the shell tool, and an honest egress claim
- REQ-563 (spec, score 9): Opt-in web lookup through the egress choke point
- LESSON-494 (lesson, score 9): A security gate and the client that executes the request must share one parser
