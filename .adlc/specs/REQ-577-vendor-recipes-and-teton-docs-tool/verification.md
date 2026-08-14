# REQ-577 — live A/B acceptance record (TASK-147)

**Outcome path: the run happened.** The 17 GiB weights were present and
symlinkable, so this is the real A/B against two isolated real-weights daemons,
not the deferred-manual fallback.

> **Final state: AC-1 and AC-2 both PASS** — after two fixes the first run
> forced. Sections 1–7 are **round 1**, which failed AC-1; sections 8–10 are the
> fixes and the re-run. Round 1 is kept whole rather than rewritten: it is the
> evidence for why the fixes exist, and it is the standing proof that a prompt
> change moves behaviour it was not about.

## Round 1 (as run, before any fix)

**Headline: AC-2 passes; AC-1 fails on its second command.** The candidate
build produces the exact `teton provider add` line for Moonshot — real
endpoint, real kind, catalog example model — where the baseline fabricates a
plausible-looking wrong one, and it does so with **zero** repository-search
tool calls. But the routing step it hands the user is
`teton policy set-tier reflex kimi`, not `think`, in **4 of 4** candidate
trials of that shape. AC-1 asks for both commands. One of them is wrong, so
AC-1 is not met, and it is recorded here as failed rather than reworded into a
pass.

Two further defects surfaced that only a live run could find, both recorded in
[Findings](#findings): `teton_docs` trips a **permission prompt** (the
requirement's Permissions table says it must not), and two llama daemons cannot
be resident at once on this machine (the second dies of Metal OOM), so an A/B
of this shape must be serialized.

---

## 1. Builds

| Arm | Checkout | Commit | Build | Daemon binary |
|---|---|---|---|---|
| **baseline** (pre-REQ-577) | `/Users/brettluelling/Documents/GitHub/teton-code` (main) | `4569311` (`45693113592d47a24f65ac6e47ab2c56daea2707`) | `cargo build --release --workspace --features tetond/llama` → `Finished` in **31.36 s** (incremental; llama.cpp objects cached from earlier sessions) | `target/release/teton-code`, 11,917,584 bytes |
| **candidate** (REQ-577) | `.worktrees/REQ-577` on `feat/REQ-577-vendor-recipes-and-teton-docs-tool` | `9ea2988` (`9ea2988ece9eb813a93e0982d06050060ecf92e3`, TASK-146 complete) | same command → `Finished` in **48.74 s** | `target/release/teton-code`, 11,934,176 bytes |

Both binaries really carry the engine — 55 exported `llama_*` symbols in each,
and both daemons load and benchmark the GGUF at startup (a default build has no
inference engine and would silently go remote-only, LESSON-482). Both report
`teton-code 0.1.15 — core 0.1.15, protocol 0.1.15, providers 0.1.15,
inference 0.1.15`. No source or git change was made in the main checkout; it
was `git status`-clean before and after.

## 2. Isolation (LESSON-482 method)

```sh
# one base dir per arm; the socket is 28 bytes, far inside SUN_LEN (~104)
XDG_RUNTIME_DIR=/tmp/t577b   # baseline  → /tmp/t577b/teton/tetond.sock
XDG_RUNTIME_DIR=/tmp/t577c   # candidate → /tmp/t577c/teton/tetond.sock

cp  ~/"Library/Application Support/teton/model-selection.toml"  <base>/
ln -s ~/"Library/Application Support/teton/models"              <base>/models
<build>/target/release/teton-code --shutdown-policy never
```

- The weights directory is **symlinked**, so both daemons mmap the same
  18,556,689,568-byte inode. Nothing was downloaded and the real file was never
  written to.
- `model-selection.toml` is copied so the consent gate has a settled decision
  and does not re-litigate it (BR-10) — the run tests the prompt, not the
  first-run flow.
- `--shutdown-policy never` keeps one daemon alive across a shape's trials, so
  the model is loaded once per arm rather than once per trial (REQ-565's
  default would exit with each client).
- No `config.toml` exists in either base dir: default permission level
  (`guarded`), web lookup off, no remote providers. The local tier serves every
  turn (`route [design/think] → local`).
- The user's own Homebrew daemon (`/opt/homebrew/opt/teton/bin/teton-code`,
  8.4 GiB RSS, running 2 days) and the real state directory were left alone.
  Only processes matching the two `--shutdown-policy never` release paths were
  ever signalled.

**Startup cost, recorded because it surprised the run:** each daemon spends
roughly two minutes between `listening` and `local model … ready` — the
startup deep re-digest of 17 GiB, then the load, then the benchmark. Polling
`teton model status` for the string `disabled` is a trap: the *loading* notice
contains it.

### Session working directories

Both arms ran in the **same** cwd for a given trial, because the prompt is
supposed to be the only variable (BUG-168 discipline):

- `/tmp/t577proj` — the primary environment. A three-file crate
  (`Cargo.toml` at `version = "0.4.2"`, `src/main.rs`, `README.md`) that
  documents nothing about Teton. A repository hunt there **cannot** succeed,
  which is the honest test of whether the knowledge comes from the prompt; and
  its `Cargo.toml` is what the control question reads.
- `/tmp/t577repo` — the secondary environment: `git archive` of main @`4569311`
  unpacked to a temp dir (12 MB, no `.git`), i.e. a working tree full of
  Teton's own documentation. This is the BUG-160 setting, where hunting is
  tempting and *would* half-succeed. Used to check the zero-repo-search claim
  where it is hardest.

### Driving

One CLI process per trial (`teton -y -v`, stdin and stdout piped), one prompt
per session — a second prompt in the same session would be answered with the
first one carried in context (REQ-567), which is a different experiment. The
driver paces on the `› ` ready marker rather than on output growth, since the
CLI buffers piped stdout mid-turn. If a turn goes quiet for 25 s with an open
`? permission requested` line, the driver answers `n`: a driver that granted a
shell command the agent chose for itself would be running commands nobody
approved. Both denials that happened are reported below, not hidden.

Tool calls are counted from the CLI's own ` - <tool> [running]` status lines —
what the session did, not what the prose claimed it did.

## 3. Trials

Three trials per shape per arm on the primary environment, plus one per shape
per arm on the repo-tempting environment, plus two diagnostics per arm: **27
sessions** (14 baseline — the extra one is a driver rehearsal on shape A whose
reply is identical to the three counted trials — and 13 candidate).

**Determinism.** Every reply within a shape/arm cell is byte-identical across
trials once the session id, the wall-clock line and the `>> context:`
accounting line are excluded — including across the two working directories.
So `n = 3` here is a check that the temperature-0.2 profile is behaving as
BUG-168 recorded, not a sample from a distribution.

### Shape A (AC-1) — "I want to hook up Kimi for deep reasoning"

| | baseline (3 + 1 trials) | candidate (3 + 1 trials) |
|---|---|---|
| provider command | `teton provider add kimi --kind openai-compatible --endpoint https://api.kimi.com/v1 --model gpt-4` | `teton provider add kimi --kind openai-compatible --endpoint https://api.moonshot.ai/v1 --model kimi-k3` |
| endpoint | **fabricated** (`api.kimi.com` is not Moonshot's API host) | **exact catalog value** |
| model | **`gpt-4`** — a different vendor's model on a Kimi provider | `kimi-k3`, and the reply labels it an example |
| routing command | `teton policy set-tier think kimi` | **`teton policy set-tier reflex kimi`** |
| repo-search calls | 0 | 0 |
| `teton_docs` calls | n/a (no such tool) | **0** |
| turn time | 2.8 s | 2.6 s |

Candidate reply, the two load-bearing lines quoted verbatim:

> 1. First, add Kimi as a provider:
> ```bash
> teton provider add kimi --kind openai-compatible --endpoint https://api.moonshot.ai/v1 --model kimi-k3
> ```
> 2. Then, assign it to the reflex tier (for deep reasoning):
> ```bash
> teton policy set-tier reflex kimi
> ```

Baseline reply, same two positions:

> ```bash
> teton provider add kimi --kind openai-compatible --endpoint https://api.kimi.com/v1 --model gpt-4
> ```
> 2. Then, set the reasoning tier to use this provider:
> ```bash
> teton policy set-tier think kimi
> ```

So the two builds fail in opposite halves. The baseline gets the *tier* right
and invents the endpoint and the model — the "command template with a hole in
it", filled in with a guess, that REQ-577 was written to end, and precisely the
BUG-165 texture OQ-4 predicted (a plausible endpoint that fails later as a
connection error). The candidate gets the endpoint and the model exactly right
and misroutes the tier, calling `reflex` "the reflex tier (for deep
reasoning)", which is false: `reflex` is the sub-second, never-leaves-the-
machine tier, and `think` is the reasoning-depth one. A user who pasted the
candidate's two commands would bind their paid deep-reasoning provider to the
tier that carries `route`, `redact` and `title` — every turn, at reflex
latency expectations — and would still have nothing on `think`.

The repo-tempting environment changed nothing: byte-identical replies in both
arms, still zero repository tool calls in both.

### Shape B (AC-2) — "How do I connect Claude?"

| | baseline (3 + 1) | candidate (3 + 1) |
|---|---|---|
| provider command | `teton provider add claude --kind anthropic --model claude-3-opus-latest` | `teton provider add claude --kind anthropic --model claude-opus-5` |
| `--endpoint` present? | no | no |
| routing command | `teton policy set-tier think claude` | `teton policy set-tier think claude` |
| repo-search calls | 0 | 0 |
| `teton_docs` calls | n/a | 0 |
| turn time | 2.6 s | 2.8 s |

AC-2's letter — the `--kind anthropic` recipe with no endpoint flag, plus the
routing step — holds on the candidate, 4/4. It also, notably, already held on
the **baseline**: the generic guide line names `--kind anthropic` for
Anthropic, so the shape was answerable before this REQ. What REQ-577 changes
here is the fact inside the command: `claude-opus-5` (the catalog's example)
instead of `claude-3-opus-latest`, a model id from the training data that
Anthropic's API would reject. That is a real improvement and it is not what
AC-2 asks about.

### Control (AC-2's second clause) — "What version is this crate? Check Cargo.toml."

| | baseline (3) | candidate (3) |
|---|---|---|
| tool calls | `read Cargo.toml` | `read Cargo.toml` |
| answer | "The crate version is 0.4.2…" | "The crate version is 0.4.2, as specified in the Cargo.toml file." |
| recipe/clause bleed into the answer | none | none |
| turn time | 1.5 s | 1.3 s |

The tool path is unchanged: a file question still reads the file, and the
recipes and the docs tool do not leak into an answer that did not ask for
them.

### Diagnostics (not AC trials — recorded to characterize the AC-1 failure)

**D1 — "What do Teton's routing tiers reflex, scan, build and think mean?"**

The candidate **did** reach for the tool:

```
 - teton_docs policy [running]
? permission requested: teton_docs — teton_docs policy
  allow teton_docs? [y]es / [n]o / [a]llow-always / [d]eny-always:
 - teton_docs policy [failed]
```

The driver denied it after the 25 s stall, and the model then answered from its
own knowledge — correctly, as it happens. Two things follow. The docstring
affordance works: shown a question whose subject matches a topic name, this
model picks the right topic on the first try. And the call does not complete,
because of the permission finding below. The baseline, asked the same thing,
answered from knowledge with no tool call at all.

**D2 — "Which Teton tier should deep reasoning work be routed to?"**

The candidate answers, with no tool call: *"deep reasoning work should be
routed to the `think` tier"*, and lists all four tiers correctly. So the
shape-A failure is **not** ignorance of tier semantics — asked the question
directly, the model has the answer. It loses it while composing a recipe.

The baseline, on the same question, tried to **run** the inspection itself:

```
 - shell: teton policy show [running]
? permission requested: shell — shell: teton policy show
 - shell: teton policy show [failed]
```

— the "the agent tried to perform the setup itself" behaviour that BR-5's
referral sentence exists to stop. The candidate makes no such attempt in any of
its 13 sessions; every reply hands the commands to the user. That is live
evidence for BR-5, on a shape no AC covers.

## 4. Verdicts

| Criterion | Verdict | Evidence |
|---|---|---|
| **AC-1** — exact two commands, zero repo-search, ≤ 1 `teton_docs`, ≥ 3 trials, baseline recorded | **FAIL** | Command 1 exact 4/4 ✅; command 2 is `set-tier reflex` not `set-tier think` 4/4 ❌; repo-search calls 0/4 ✅; `teton_docs` calls 0 ✅; baseline recorded ✅ |
| **AC-2** — `--kind anthropic` recipe, no endpoint, routing step; control still calls `read` | **PASS** | 4/4 candidate trials; control reads `Cargo.toml` and answers 0.4.2 3/3 |
| TASK-147 AC "weights checked first, outcome path stated" | **PASS** | §Outcome path, §2 |
| TASK-147 AC "if run: AC-1 and AC-2 recorded with baseline comparison" | **PASS as a record** | §3 — and the record says AC-1 failed |

**REQ-577's acceptance state at the end of round 1:** AC-2 live-verified;
**AC-1 not met**. The half of AC-1 the REQ exists for — the vendor facts, and
the absence of a repository hunt — is verified and is a clear improvement over
the baseline. The routing half regressed relative to the baseline. (Both are
fixed and re-verified in §8–§10; this paragraph describes the build that was
run here, not the one on the branch now.)

## 5. Findings

**F-1 — the recipe answer misroutes the tier (AC-1's failure).** 4/4,
deterministic. Two observations bound the cause. The candidate knows the tier
semantics when asked directly (D2), and the `providers` topic ships the correct
pairing (`teton policy set-tier think kimi`) — but the *resident* guide never
says what a tier means. Its recipe list carries endpoints and models only, and
its routing line enumerates `<reflex|scan|build|think>` with `reflex` first.
The model fills the tier slot from that enumeration and rationalizes the
choice. The baseline, whose recipe list is absent, composed the sentence
differently and landed on `think`. This is exactly the chaos BUG-168 recorded
— a byte-level prompt change flipping behaviour that the change was not about
— and the requirement's own Assumptions section predicted it ("wording … may
need the same dictation-style treatment as the web-off clause"). The plausible
fixes are the tier *in* each recipe (which is what the `providers` topic
already does) or one clause naming what `think` is for; both are prompt
changes, so both are unverified until the next A/B.

**F-2 — `teton_docs` asks for permission, and the requirement says it must
not.** The Permissions table in `requirement.md` reads: "call `teton_docs` |
the model, in any session, without a permission prompt (read-only, no egress,
touches no user data)". Live, at the default `guarded` level, it prompts. The
cause is in `crates/tetond/src/harness/permissions.rs`: `READ_ONLY_TOOLS` is
`["read", "glob", "grep"]` and nothing anywhere references `DOCS_TOOL_NAME`, so
the tool falls to the level's `default` policy — `Ask` at `guarded` and
`edits`, and **`Deny` at `plan`**, where a docs read would be refused outright.
The module's own comment names this shape as a known degradation ("a new
*read-only* first-party tool that nobody adds to `READ_ONLY_TOOLS` merely
asks"), which is why nothing failed in CI: the exposure tests (BR-7, AC-5)
assert the tool is in the tool list, and it is. Being *callable* is a different
claim, and it is the one that was wrong.

**F-3 — `teton_docs` is never reached on the shapes it was built for.** Zero
calls in all 11 candidate sessions of shapes A, B and the control (the only
call in the whole run is D1's). This is
inside AC-1's budget ("at most one") and it is also the requirement's
Assumption — "the local model will call `teton_docs` when the topic index names
the subject" — coming back **falsified for provider-setup prompts** and
confirmed only for an explicitly-topic-shaped question (D1). It is not a defect
on its own (the inline recipes answered the question without a tool round-trip,
which is the cheaper path and the design's intent), but it means the tool's
live value is currently unproven for the front-door shapes, and any future
knowledge moved *out* of the guide and *into* a topic would go unread. Worth
stating plainly next to BR-10's growth-path claim.

**F-4 — two llama daemons do not fit on this machine.** Starting the candidate
daemon while the baseline daemon was still resident produced a run of
`ggml_metal_synchronize: error: command buffer 0 failed with status 5 /
Insufficient Memory (kIOGPUCommandBufferCallbackErrorOutOfMemory)`, and the
daemon reported `local tier disabled: qwen3-coder-30b-a3b loaded but failed its
benchmark: inference backend error: Decode Error -3: unknown`. The weights file
is shared by inode, but the Metal buffers are not, and the user's own 8.4 GiB
daemon was also resident. The A/B must be **serialized** — one arm's daemon
stopped before the other starts — at a cost of one ~2-minute load per switch.
An A/B that ignored this would silently compare a working arm against a
`disabled`-tier arm. Recorded here because LESSON-482's isolation recipe does
not mention it, and the failure is not self-evidently about memory from the
daemon's own message.

**F-5 (minor) — the web-off clause bleeds unevenly.** The baseline's Kimi
answer ends with the REQ-572/BUG-168 sentence ("Web lookup is available but
switched off; turn it on with `/web setup`…"); the candidate's does not, on the
same question. Neither behaviour is wrong — no AC covers it — but it is another
instance of prompt-adjacent output moving under a change that was not about it.

## 6. Follow-ups this run earns

1. Fix F-1 before REQ-577 can claim AC-1. Then re-run **this** matrix; a
   prompt fix is unverified until it is A/B'd (BUG-168's rule, and the reason
   this record exists).
2. Fix F-2: add `teton_docs` to the read-only set (or whatever mechanism the
   REQ-560 authors prefer for a first-party no-egress tool) and pin it with a
   test that asserts the *decision*, not the exposure — the exposure tests were
   green throughout.
3. Consider whether F-3 changes BR-10's growth-path claim in practice.

## 7. Reproduction

> **Superseded by §12–13.** The commands here still reproduce the *setup*, but
> the expectations below are the **pre-fix defects**: they call for
> `https://api.moonshot.ai/v1` and for an `anthropic` registration with no
> `--endpoint`, both of which BUG-170 showed cannot serve a turn. Follow §12–13
> (round 3) for the runnable matrix and the current expectations.

```sh
# baseline
cd <main checkout @ 4569311>
cargo build --release --workspace --features tetond/llama
mkdir -p /tmp/t577b/teton
cp  ~/"Library/Application Support/teton/model-selection.toml" /tmp/t577b/teton/
ln -s ~/"Library/Application Support/teton/models"             /tmp/t577b/teton/models
cd /tmp/t577proj   # a 3-file crate at version 0.4.2, documenting nothing about Teton
XDG_RUNTIME_DIR=/tmp/t577b <main>/target/release/teton-code --shutdown-policy never &
# wait for `local model … ready` (~2 min: deep re-digest, load, benchmark)
XDG_RUNTIME_DIR=/tmp/t577b <main>/target/release/teton -y -v   # one prompt per session

# then STOP that daemon (F-4) and repeat with the REQ-577 worktree and /tmp/t577c
```

Prompts, three sessions each per arm:

1. `I want to hook up Kimi for deep reasoning`
2. `How do I connect Claude?`
3. `What version is this crate? Check Cargo.toml.`

What to look for: in (1) the endpoint must be `https://api.moonshot.ai/v1` and
the second command must name the **`think`** tier, with no ` - read`/` - grep`/
` - glob`/` - shell` status line anywhere in the turn; in (2) `--kind
anthropic` with no `--endpoint`; in (3) a ` - read Cargo.toml` line and the
version out of the file.

---

**Run by:** Claude (Fable 5 agent), 2026-08-14, at Brett Luelling's direction.
**Platform:** macOS (Darwin 25.6.0), Apple Silicon, 48 GiB, local tier
qwen3-coder-30b-a3b (18,556,689,568-byte GGUF), temperature-0.2 profile.
**Not signed off by a human.** Every line above is from the two isolated
daemons on this machine; the transcripts they were written from are the
27 session captures described in §3.

---

# Round 2 — after the two fixes (2026-08-14, same day)

Everything above is the **first** run and stands as written: it is the record
of a build that failed AC-1, and deleting it would delete the evidence that the
fixes below were needed. This section is what changed and what the same matrix
then did.

## 8. What was fixed

**Fix 1 — `teton_docs` is callable (F-2).**
`crates/tetond/src/harness/permissions.rs`: `DOCS_TOOL_NAME` joins
`READ_ONLY_TOOLS`, so every level's table allows it — `Allow` at `guarded`,
`edits` and `plan` instead of `Ask`/`Ask`/`Deny`. The constant is used rather
than a fifth spelling of the string.

Two tests, because the missing coverage was a whole *class* — every existing
`teton_docs` test asserts **exposure** (it is in the tool list, it survives the
`max_tools` cap), and none asserted **callability**:

- `permissions::tests::a_bundled_docs_read_is_allowed_at_every_level_and_asks_nothing`
  — drives the real `PermissionGate` at every `PermissionLevel::ALL` and
  asserts three things per level: the decision is `Allowed`, no prompt is
  registered (`pending_count() == 0`), and nothing is published on the bus.
- `permissions::tests::each_level_expands_to_its_documented_table` — the
  hand-written golden table gains a `teton_docs → Allow` row for `guarded`,
  `edits` and `plan`.

Both were **mutation-checked**: with `DOCS_TOOL_NAME` removed from
`READ_ONLY_TOOLS` the golden table fails with ``guarded: `teton_docs` should be
Allow`` and the gate test fails in 5 s with ``guarded: `teton_docs` blocked
waiting for an answer``.

That timeout is itself a finding worth recording. The **first** draft of the
gate test had no timeout, and under the mutation it did not fail — it **hung**,
because `authorize` on an `ask` policy waits for a client answer that no test
will ever give. It had to be killed by hand. This is precisely the trap the
neighbouring `answer_next` helper documents ("a hang reads as infrastructure
trouble and gets retried; a failure gets read"), and the draft walked into it
from the other direction — that helper bounds a test that *expects* a prompt,
and this one bounds a test that expects none. Both need the bound, for opposite
reasons.

**Fix 2 — the resident guide names what a tier is for (F-1).**
`crates/tetond/src/harness/self_config.md`, step 2 only. The tier enumeration
`<reflex|scan|build|think>` becomes `<tier>` and the purposes move into the
sentence, with the failing mapping dictated outright as its own sentence rather
than left to be inferred (BUG-168's rule):

> 2. `teton policy set-tier <tier> <provider-id>` routes a tier: `reflex`
> always-on duties, `scan` bulk reads, `build` edits, `think` deep reasoning.
> Deep reasoning means `think`. …

**+95 bytes.** Nothing else in the guide moved: the recipe line and the
referral sentence are byte-identical. Both margin tests re-measured and
re-recorded in their doc comments:

| Prompt shape | Worst prompt | Spent | Margin (floor 48) |
|---|---|---|---|
| opted-out (`egress::redact`) | 5,711 B (was 5,616) | 8,987 | **229** (was 324) |
| web opted-in (`tools::web`) | 5,663 B (was 5,568) | 8,939 | **277** (was 372) |

`REDACT_BODY_OVERHEAD_BYTES` was **not** raised; the 95 bytes came out of the
margin, which is the decision the floor exists to force somebody to make.

## 9. The re-run (candidate only)

The baseline is unchanged and unrebuilt, so its results above still stand as
the comparison. Serialized per F-4: the round-1 candidate daemon was stopped
before this one started, same isolation, same `/tmp/t577proj` cwd, one session
per trial. Rebuild: `Finished` in 19.5 s. 8 sessions; replies byte-identical
within each shape.

| Trial | Result |
|---|---|
| **fix-A-1/2/3** — "I want to hook up Kimi for deep reasoning" | `teton provider add kimi --kind openai-compatible --endpoint https://api.moonshot.ai/v1 --model kimi-k3` **and** `teton policy set-tier think kimi`. 0 repo-search calls, 0 `teton_docs` calls. 2.6–2.8 s. **3/3 PASS** |
| **fix-B-1/2** — "How do I connect Claude?" | `teton provider add claude --kind anthropic --model claude-opus-5` (no `--endpoint`), `teton policy set-tier think claude`. 0 repo calls. **2/2 PASS, no regression** |
| **fix-C-1** — control | ` - read Cargo.toml` → "The crate version is 0.4.2". **PASS** |
| **fix-P1** — "What topics can teton_docs show? Read the providers one…" | ` - teton_docs providers [running]` → **`[done]`**, no permission prompt; answer names all four topics and the six vendor recipes. **PASS** |
| **fix-P2** — "What do Teton's routing tiers … mean?" | ` - teton_docs policy [running]` → **`[done]`**, no permission prompt; all four tiers correct with their categories. **PASS** (round 1: same call, `[failed]` on the denied prompt) |

The shape-A answer in full, in the two positions AC-1 names:

> 1. **Add Kimi as a provider**:
> ```bash
> teton provider add kimi --kind openai-compatible --endpoint https://api.moonshot.ai/v1 --model kimi-k3
> ```
> 2. **Assign Kimi to the `think` tier**:
> ```bash
> teton policy set-tier think kimi
> ```

`? permission requested` appears in **zero** of the 8 sessions.

## 10. Final verdicts

| Criterion | Round 1 | Round 2 |
|---|---|---|
| **AC-1** — exact two commands, zero repo-search, ≤ 1 `teton_docs`, ≥ 3 trials, baseline recorded | **FAIL** (`set-tier reflex`) | **PASS** — 3/3, both commands exact, 0 repo calls, 0 docs calls |
| **AC-2** — `--kind anthropic` recipe + routing step; control still calls `read` | PASS | **PASS** — 2/2 plus the control |
| Requirement Permissions row — `teton_docs` callable without a prompt | **violated** | **holds**, live and in CI (two mutation-checked tests) |

**REQ-577's acceptance state after round 2: AC-1 and AC-2 are both
live-verified**, on this platform, on this build, against this model. AC-3
through AC-8 are CI claims covered by TASK-143..146 and are not what this
document speaks to.

Findings F-3 (the tool is not reached on provider-setup shapes — the inline
recipes answer without it, and P1/P2 now prove the tool works when it *is*
reached), F-4 (serialize the daemons) and F-5 (clause bleed) are unchanged by
this round and stand as recorded.

**A caveat this document should carry rather than bury:** both fixes are prompt
and policy changes verified at temperature 0.2 against one model on one
machine. Round 1 is the standing evidence that a byte-level prompt change moves
behaviour that the change was not about — the recipes fixed the endpoint and
broke the tier. Any future edit to `self_config.md` re-opens this question and
should re-run §7's matrix rather than reason about it.

---

# Round 3 — after the endpoint correction (2026-08-14, same day)

Rounds 1 and 2 stand as written. This round exists because the *facts* rounds 1
and 2 verified so carefully were the wrong kind of URL: every recipe endpoint
was a vendor `base_url` and Anthropic had none at all, so the exact commands
those rounds recorded as passes could not have served a turn (BUG-170). The
behaviour was right and the payload was wrong, which is why a live run that
checked "does it emit the catalog value" could not catch it — it emitted the
catalog value faithfully, 4/4.

**Candidate-only, per the instruction and per sense**: the baseline is
unchanged and unrebuilt, and its round-1 results still stand as the comparison.
Nothing in this round is a claim about the baseline.

## 11. What changed since round 2

- Every catalog endpoint is now the vendor's documented **request** URL
  (`…/chat/completions`, and `https://api.anthropic.com/v1/messages` for
  Anthropic, which previously had `endpoint: None`). Re-verified against each
  vendor's own `curl` example — see TASK-143's round-2 Verification Record.
- The guide's recipe step lost its two-form
  `anthropic`-without-`--endpoint` spelling and gained
  `All three are required. `--endpoint` is the full request URL, not a base
  URL.` **+136 bytes**, paid out of the margin; `REDACT_BODY_OVERHEAD_BYTES`
  was **not** raised again.

| Prompt shape | Worst prompt | Spent | Margin (floor 48) |
|---|---|---|---|
| opted-out (`egress::redact`) | 5,847 B (was 5,711) | 9,123 | **93** (was 229) |
| web opted-in (`tools::web`) | 5,799 B (was 5,663) | 9,075 | **141** (was 277) |

## 12. Isolation

Same LESSON-482 method as §2, fresh base dir `/tmp/t577d` (round 1 and 2's
`/tmp/t577b` / `/tmp/t577c` are gone), `model-selection.toml` copied, weights
**symlinked** to the same 18,556,689,568-byte inode — nothing downloaded, the
real file never written. Serialized per F-4: no other `--shutdown-policy never`
daemon was resident at any point, and the user's own Homebrew daemon was left
alone. Same `/tmp/t577proj` cwd (a three-file crate at `version = "0.4.2"` that
documents nothing about Teton). Release build: `cargo build --release
--workspace --features tetond/llama`. One CLI process per trial, one prompt per
session; a permission prompt would have stalled the session to its 90 s bound,
which is itself the observation.

## 13. The re-run

8 sessions. Replies byte-identical within each shape.

| Trial | Result |
|---|---|
| **A1/A2/A3** — "I want to hook up Kimi for deep reasoning" | `teton provider add kimi --kind openai-compatible --endpoint https://api.moonshot.ai/v1/chat/completions --model kimi-k3` **and** `teton policy set-tier think kimi`. 0 tool calls of any kind. **3/3 PASS** |
| **B1/B2** — "How do I connect Claude?" | `teton provider add claude --kind anthropic --endpoint https://api.anthropic.com/v1/messages --model claude-opus-5` **and** `teton policy set-tier think claude`. 0 tool calls. **2/2 PASS** |
| **C1** — control | ` - read Cargo.toml [done]` → "The crate version is 0.4.2". **PASS**, tool path unchanged |
| **P1** — docs probe | ` - teton_docs providers [running]` → **`[done]`**, no permission prompt; the answer names all four topics and all six vendors *with the corrected endpoints*, including DeepSeek's missing `/v1`. **PASS** |

`? permission requested` appears in **zero** of the 8 sessions. No ` - read`,
` - grep`, ` - glob` or ` - shell` line appears in any shape-A or shape-B
session.

### One resident-prompt change landed after this run, deliberately un-A/B'd

The re-review pass that followed round 3 reworded the web tool's own
description — `every lookup asks the user` → `asks unless already allowed` —
because the old spelling was the same false absolute the `web` topic and the
README carried. It is resident prompt, so §14's caveat applies to it and is not
being waved away: it is recorded here as an **accepted sub-threshold change**
rather than re-verified.

The reasons it is accepted, stated so a later reader can disagree with them:
two words inside an existing clause of an existing sentence, **+1 byte**; it
appears only in the *opted-in* prompt shape, which is not the shape any AC-1 or
AC-2 trial ran (every round-3 session had web lookup off, so this string was in
none of the 8 prompts measured); and none of `self_config.md`'s model-facing
lines moved. Round 1 is the standing warning that byte-level prompt changes move
unrelated behaviour — but round 1's change was 379 bytes of new recipe list in
the resident guide, and the honest comparison is with that, not with this. If a
later run shows web-tool selection behaving differently, this paragraph is where
to start.

**D1 (diagnostic, no AC covers it) — "How do I hook up Together AI as a
provider?"** A vendor the catalog does *not* ship, which is the only shape that
tests the new clause rather than the recipes. The model answered
`--endpoint https://api.together.ai/v1/chat/completions` — a full request URL,
composed for a vendor it had no recipe for — then hedged that the exact
endpoint should be checked against the vendor's docs, and the web-off clause
fired. That is the clause doing the job it was added for: the generalization
from six recipes to an unlisted seventh now carries the right URL *shape*. It
is one trial on one vendor and is recorded as a diagnostic, not as evidence for
an AC.

## 14. Verdicts

| Criterion | Round 1 | Round 2 | Round 3 |
|---|---|---|---|
| **AC-1** — exact two commands, zero repo-search, ≤ 1 `teton_docs`, ≥ 3 trials | **FAIL** (`set-tier reflex`) | PASS (3/3) — but with a `base_url` endpoint that could not have served a turn | **PASS** — 3/3, both commands exact **and runnable**, 0 tool calls |
| **AC-2** — `--kind anthropic` recipe + routing step; control still calls `read` | PASS | PASS | **PASS as amended** — see below; 2/2 plus the control |
| Requirement Permissions row — `teton_docs` callable without a prompt | violated | holds | **holds** — P1 completes, 0 prompts in 8 sessions |

**AC-2 is amended, not merely re-passed.** Its drafted letter reads "answers
with the `--kind anthropic` recipe (**no endpoint flag**)", and that clause is
now known to be false as a requirement: `Config::validate` refuses any
`is_remote()` provider without an endpoint, and `Anthropic` is one, so a reply
that omitted `--endpoint` would be handing the user a command that stores their
key and is then rejected. Round 3 records the AC's *intent* — the anthropic kind
answered with its own recipe plus the routing step, and the control's tool path
unchanged — and the parenthetical is superseded by BUG-170. Rounds 1 and 2
recorded "no `--endpoint`: correct" as a pass against the drafted letter; that
is left as written, because it is the evidence that a criterion can be met
exactly and still be wrong.

**REQ-577's acceptance state after round 3: AC-1 and AC-2 are both
live-verified against a build whose commands are runnable.** AC-3 through AC-8
remain CI claims covered by TASK-143..146.

Findings F-3 (the tool is not reached on provider-setup shapes — the inline
recipes answer without it; P1 again shows it works when it *is* reached), F-4
(serialize the daemons — observed, obeyed, no OOM this round) and F-5 (clause
bleed — D1 shows the web-off clause firing on a provider question, which is the
other direction from round 1's observation) are unchanged and stand as
recorded.

**The caveat from round 2 now has three instances behind it.** Round 1: the
recipes fixed the endpoint and broke the tier. Round 2: the tier fix held.
Round 3: the same prompt, 136 bytes longer, kept both. Every one of those was
unknowable without running it. Any future edit to `self_config.md` re-opens the
question and should re-run §7's matrix rather than reason about it.

**Run by:** Claude (Fable 5 agent), 2026-08-14, at Brett Luelling's direction.
**Platform:** macOS (Darwin 25.6.0), Apple Silicon, 48 GiB, local tier
qwen3-coder-30b-a3b, temperature-0.2 profile. **Not signed off by a human.**
