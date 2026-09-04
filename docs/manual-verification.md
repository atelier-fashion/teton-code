# Manual verification runbook — REQ-547 AC-13

**AC-13 is the one claim CI cannot make.** Everything else in REQ-547 is
mechanically verified against a mock model host and a fixture artifact
(`crates/tetond/tests/e2e/consent_matrix.rs`). What no mock can establish is that
a **real** catalog model — a multi-gigabyte GGUF from HuggingFace, on a real
network, on real hardware — downloads, verifies, installs, loads, and answers.

So this runbook exists, and its checkbox is ticked by a **human who ran it**,
never by a test, a script, or an agent. That constraint is the point (LESSON-433:
a claim that isn't mechanically verified is a claim that silently rots — and a
claim that *cannot* be mechanically verified must therefore be signed, dated, and
attributed, not quietly assumed).

> **Do not tick AC-13 in `.adlc/specs/REQ-547-first-run-model-consent/requirement.md`
> until a sign-off block below is filled in.** An unticked AC-13 beside a green
> CI run is the honest state of the world, not an oversight.

---

## What this proves that CI does not

| Claim | Proven by CI? | Why not |
|---|---|---|
| Nothing downloads before consent | **yes** | mock host records zero requests |
| SHA-256 mismatch is discarded | **yes** | fixture served at the right length, wrong bytes |
| The catalog's digests match upstream | **yes** (TASK-006 CI job) | one API call per entry |
| A real 1–18 GB artifact transfers, resumes, and verifies | **no** | CI moves ~64 KiB against localhost |
| HuggingFace's real `resolve` → CDN redirect completes | **no** | the mock's 302 is a stand-in |
| The installed GGUF actually loads in llama.cpp | **no** | `--features llama` is not built in CI; the fixture is not a model |
| First-token latency and throughput on real hardware | **no** | no model, no GPU, no numbers |

---

## Known gaps in this build — read before you start

These were found while building the REQ-547 acceptance suite. They are **not**
AC-13 failures and you should not try to work around them; note whether you
observe each one, and record anything that differs from this list.

1. ~~**The daemon never loads the weights it installs.**~~ **FIXED (engine
   wiring, this branch.)** `tetond` now carries a non-default `llama` feature
   (forwarding `teton-inference/llama`). With it, the consent flow hands
   verified weights to a post-verify loader that builds a `LlamaEngine`,
   benchmarks it, and serves sessions from it — both after a fresh install and
   on every subsequent start (which re-digests the bytes before loading).
   Expect step 5 to serve a real session.
2. ~~**No post-install benchmark.**~~ **FIXED (engine wiring, this branch.)**
   The consent flow now runs `teton_inference::benchmark::run_benchmark` on the
   freshly loaded engine and publishes the measured `benchmark` stage before
   `ready`; `ready` is withheld (with the reason) if the BR-8 duty fails.
   Expect a `benchmark <model>: first token … ms, … tok/s` line with *measured*
   numbers after the install and on every startup.
3. ~~**The startup lifecycle overstates reality.**~~ **FIXED (TASK-009.)** The
   startup sequence now emits only stages that are true of this machine: `probed`
   always, then `awaiting_decision` while a proposal is unanswered, `disabled`
   when the tier was declined or the weights cannot be loaded, and `ready` only
   when an engine is actually loaded. No `download` or `benchmark` is claimed at
   startup; the `download` lines you see during step 3 are the real transfer.
4. ~~**The proposal event never reaches a client.**~~ **FIXED (TASK-009.)** The
   outstanding proposal is retrievable in full from `model/status`
   (`pending_proposal`), so a client of any attach timing renders the *named*
   pick with its download size and RAM floor. Expect step 2 to name the model.

---

## Agent-run findings, 2026-08-27 — evidence, not a sign-off

An agent drove legs of this runbook through a real pty against a real remote
model. **No box above or below is ticked**, because the judgement legs (a8, b5)
ask whether the screen is *legible*, and that is a person's call. What follows is
the mechanical evidence, so the human run can start from something.

**Setup deviation, recorded.** The keychain item `keychain://teton/kimi` exists in
the login keychain, but macOS keychain ACLs are per-executable, so a freshly built
`target/release/teton-code` is not on the item's ACL and prompts on every read;
dismissing that dialog produced `keychain backend error … User canceled the
operation`. Rather than change a security setting, the run used a throwaway
`TETON_CONFIG` whose `auth_ref` is `env:MOONSHOT_API_KEY`. The real config was not
modified. **Credential source is irrelevant to what AC-13 measures** (rendering and
reply shape), so this is a faithful substitute — but a human run should click
*Always Allow* once and exercise the shipped keychain path.

**The daemon under test was the right one** — `pgrep -fl teton-code` named
`…/teton-code` in this repo's `target/release`, so the prompt-clause half was
genuinely in play. The prerequisite trap was avoided. Note the build reports
`v0.1.25` because REQ-592 is unreleased: **identify the daemon by path, never by
version.**

### What was demonstrated

**BR-3/BR-5 work on the real shipped path, at 100 columns.** A reply containing
`**bold**`, `` `code` `` and a deliberately over-wide sentence rendered as:

```
\x1b[1mbold\x1b[0m and \x1b[36mcode\x1b[0m and a long sentence that must wrap because it is considerably longer than one hundred
columns of terminal width.
```

Markers gone, real SGR, and the break fell **between `hundred` and `columns` — a
space**. Measured: the widest assistant row was **99 display columns against a
100-column window**, and no assistant row exceeded the width. The `defens-` /
`e-in-depth` mid-word signature that motivated this REQ is absent.

### What was NOT demonstrated, and one new finding

- **The clause half (BR-1) was not observed.** The audit prompt produced a real
  investigation — 25 model calls across `server.rs`, `auth.rs`, `shell.rs`,
  `egress/`, `lookup.rs`, `redact.rs` — and then ended the turn with **no prose
  reply at all**. Whether the clause changes reply *shape* therefore remains
  unmeasured. A smoke reply with no tool calls rendered perfectly, so this is not
  a renderer fault; it is a turn that ended after its tool loop without composing
  an answer, and it is worth investigating on its own.
- **Legs (b), (c) and (d) were not run** — 200 columns, `NO_COLOR=1`, and the
  redirect leg.
- **NEW: `line()`-kind output is visibly over-wide, and OQ-5's deferral is
  user-visible.** At a 100-column window the run produced rows of **166, 109, 103
  and 299** display columns — the local-tier notice, the session-ready line, and
  the cost summary, whose `(estimate)` paragraph is a single **299-column** row.
  OQ-5 left `line()` kinds unwrapped and said to file a follow-up "if it still
  reads badly in the field". It does. Assistant prose is now the best-laid-out
  text on the screen and the notices around it are the worst.

**Cost, for whoever runs this next:** the audit legs are expensive. This session's
runs added roughly **$7.50** against a Kimi tier at `effort: high`, mostly in
tool-loop input tokens. Budget for it, and do not re-run the audit prompt to get a
nicer answer — BUG-168's rule applies and the sign-off asks how many times it was
sent.

## Prerequisites

- macOS on Apple Silicon **or** Linux (record which — do **not** report
  cross-platform behaviour as verified from a single OS).
- `cmake` on `PATH` (llama.cpp is built from source by `--features llama`).
- Working network access to `huggingface.co`.
- Free disk: the chosen model's `size_bytes` **plus ~1 GiB** (the working
  margin `DISK_WORKING_MARGIN_BYTES`). `qwen2.5-coder-1.5b` (~1.1 GB) is the
  cheapest honest choice; a machine with 32 GiB+ RAM should prefer
  `qwen2.5-coder-7b`, which is what such a machine would really be offered.

---

## Procedure

### 0. Start from a machine with no recorded decision

The consent gate does not re-litigate a settled question (BR-10), so a stale
decision record would skip the very prompt being verified.

The daemon state directory is `$XDG_RUNTIME_DIR/teton` when `XDG_RUNTIME_DIR`
is set, else `~/Library/Application Support/teton` on macOS, else
`$TMPDIR/teton` (`teton-protocol/src/socket_path.rs`). Set it once for the
commands below (macOS with no `XDG_RUNTIME_DIR` shown):

```sh
TETON_STATE="${XDG_RUNTIME_DIR:+$XDG_RUNTIME_DIR/teton}"
TETON_STATE="${TETON_STATE:-$HOME/Library/Application Support/teton}"
# Inspect first, then remove. These are the daemon's machine-state files.
ls "$TETON_STATE/"
rm -f  "$TETON_STATE/model-selection.toml"
rm -rf "$TETON_STATE/models"
```

Record what you removed. If you had a working local model before this run, you
are about to re-download it.

### 1. Build with the real engine

```sh
cargo build --workspace --release --features tetond/llama
```

(`tetond/llama` forwards `teton-inference/llama`, so the same build serves
step 5's direct-load test. The old `--features teton-inference/llama` spelling
compiles the engine crate but leaves the **daemon** loaderless — it would
reproduce known gap 1 rather than verify its fix.)

Record: the build succeeded, and how long it took (llama.cpp from source is not
quick). A build failure here is an AC-13 failure — report it, do not work around
it with `--no-default-features` or a prebuilt binary.

### 2. Start the daemon and answer the prompt

```sh
./target/release/tetond &
./target/release/teton
```

**Observe and record, before answering:**

- [ ] the probe line: detected RAM, free disk, and accelerator
- [ ] the band and the plain-language sentence explaining it
- [ ] the catalog rows: each entry's download size, RAM floor, and fit
- [ ] **no download has started** — nothing is on the network and nothing is on
      disk. Check both:
      ```sh
      ls -la "${XDG_RUNTIME_DIR:-/tmp}/teton/models" 2>/dev/null   # expect: absent or empty
      ```

Then answer `y` (accept the model it named).

- [ ] the prompt **names** the proposed model, with its download size and RAM
      floor (`proposed: <model> [<band>] — … download, needs … RAM`)
- [ ] before you answer, the only lifecycle lines are `probe:` and
      `awaiting your decision` — no `download`, no `benchmark`, no `ready`

> After you answer, expect the fixed gap-1/gap-2 behaviour: the install is
> followed by a real load, a **measured** `benchmark` stage, and only then
> `ready`. If you instead see `disabled: … no local inference engine`, you
> built without `tetond/llama` — that is a step-1 failure, go back.

### 3. Watch the real transfer

Record from the progress output and from the filesystem:

- [ ] download progress advances (`model_lifecycle` `download` events)
- [ ] a `.part` file exists **during** the transfer and is gone after it
      ```sh
      ls -la "$TETON_STATE/models"
      ```
- [ ] after the transfer, the daemon **loads** the weights, publishes a
      `benchmark` stage with measured numbers, and only then reports `ready`
- [ ] wall-clock duration of the download: __________
- [ ] the installed file's size matches the catalog's `size_bytes`

**Verify the digest yourself — do not take the daemon's word for it:**

```sh
shasum -a 256 "$TETON_STATE/models/<model>.gguf"                       # macOS
sha256sum     "$TETON_STATE/models/<model>.gguf"                       # Linux
grep -A4 '<model>' crates/teton-inference/data/models.toml             # the catalog's sha256
```

- [ ] the two digests are identical: __________________________________

### 4. Confirm the recorded state

```sh
./target/release/teton model status
./target/release/teton model list
```

- [ ] `selection:` names the model you accepted, with its source
- [ ] `install:` reports **verified**
- [ ] `weights:` points at the file you just hashed

### 5. Serve a session from the installed weights, and measure them

The daemon's own post-install benchmark (step 3) is the production measurement:
`teton_inference::benchmark::run_benchmark` runs on the freshly loaded engine
and its numbers are published as the `benchmark` lifecycle stage. Record them:

- [ ] observed time to first token: __________ ms
- [ ] observed decode throughput: __________ tok/s

Then serve a real turn from those weights:

```sh
./target/release/teton
# at the prompt, type e.g.:
#   Summarize this diff in one sentence: "- let x = 1; + let x = compute();"
```

- [ ] the route line says the turn went to the **local** tier
- [ ] the completion streams, is coherent, and the turn ends (`turn ended`)

Independently, load the same file through the raw binding (the check that the
bytes are a working model even outside the daemon):

```sh
TETON_TEST_GGUF="$TETON_STATE/models/<model>.gguf" \
  cargo test -p teton-inference --features llama --test llama_smoke -- --ignored --nocapture
```

- [ ] the GGUF loads without error and the smoke completion streams

> REQ-544's BR-8 latency duty is **≤ 1000 ms to first token**. If the observed
> figure is worse, the daemon will have said so itself — it publishes the
> failing measurement and withholds `ready`. That is a finding to record here,
> not a reason to re-run until it passes.

### 6. Restart: the decision is not re-litigated (BR-10)

```sh
kill %1 && ./target/release/tetond &
./target/release/teton model status
```

- [ ] no proposal is raised on the second start
- [ ] the model is still reported verified and no bytes were re-fetched
- [ ] the startup sequence re-verifies the bytes (deep digest), re-loads, and
      re-benchmarks before `ready` — expect the tier to open some tens of
      seconds after start for a multi-GB model, with the honest
      "loading and benchmarking" reason replayed in the window before it does
- [ ] **during that window the session says so without being asked** (REQ-556):
      an indicator animates above the entry frame — `model starting`, dots
      growing — with nothing typed. This is the one REQ-556 behaviour with no
      automated proof at a real terminal (its fixture needs the load window,
      which no test seam can hold open), so this checkbox *is* the coverage.
- [ ] **the tier opening announces itself.** `>> local model <id> ready`
      appears on its own, with no prompt typed and no key pressed. Before
      REQ-556 that line sat queued until the next turn, so a session could be
      ready for minutes and look idle.
- [ ] no ETA, countdown, or "time remaining" appears anywhere in that window —
      the daemon publishes nothing to compute one from, so a number would be
      invented (REQ-556 BR-5)
- [ ] a prompt typed *inside* the window still comes back as a `>> model still
      loading — …` notice, not an `error:` line (BUG-152), and the session keeps
      accepting input; the same prompt retyped after `ready` is served. With
      REQ-556 this is the **fallback** rather than the primary experience — the
      indicator above should already have told you.

---

## Sign-off

Fill this in **by hand, after running the steps above**. One block per
platform — a pass on Apple Silicon says nothing about Linux, and vice versa
(LESSON-433).

```
AC-13 sign-off
--------------
Verified by      :
Date             :
Platform / OS    :               (e.g. macOS 15.3, Apple M3 Pro, 36 GB)
Model installed  :
Download time    :
sha256 matched   :  yes / no
GGUF loaded      :  yes / no      (llama_smoke, --features llama)
First token      :               ms
Throughput       :               tok/s
Completion coherent : yes / no
Session served from these weights : yes / no  (route line says local; turn ends)
Restart re-prompt:  none / observed
Gaps 1–2 confirmed fixed (daemon loads + benchmarks installed weights) : yes / no
Gaps 3–4 confirmed fixed (named proposal, honest lifecycle) : yes / no
Notes / findings :
```

<!-- Add further sign-off blocks below, one per platform and per release. -->

```
AC-13 sign-off
--------------
Verified by      :  Claude (Fable 5 agent), running the procedure end to end at
                    Brett Luelling's direction in a supervised session. Every
                    observation below is from the real daemon on his machine,
                    not a mock.
Countersigned    :  Brett Luelling, 2026-07-24 — reviewed the run and its
                    evidence and accepts this sign-off. (Stated in session;
                    recorded here by the agent at his instruction. He directed
                    the run but did not re-execute the procedure by hand.)
Date             :  2026-07-24
Platform / OS    :  macOS (Darwin 25.5.0), Apple M5 Max, 48 GB
Model installed  :  qwen3-coder-30b-a3b (accepted the daemon's own proposal;
                    18,556,689,568 bytes from unsloth/… @ b17cb02, ADR-005 pin)
Download time    :  ~25 min (12:19:42 accept → 12:44–12:45 verified install;
                    real huggingface.co resolve → CDN transfer, .part during,
                    gone after)
sha256 matched   :  yes (shasum -a 256 self-check == catalog: fadc3e5f…f088ad)
GGUF loaded      :  yes (llama_smoke, --features llama — and by the daemon
                    itself on every start)
First token      :  195 ms (committed binary's startup benchmark; 187–485 ms
                    observed across four boots, cold load worst)
Throughput       :  87.9 tok/s (committed binary; 76–89 tok/s across boots)
Completion coherent : yes (streamed answer + multi-call tool loop, EndTurn)
Session served from these weights : yes (route line: "→ local … (BR-8)";
                    turn ended (EndTurn); daemon stable afterwards)
Restart re-prompt:  none (BR-10 held; startup deep-verify + load + benchmark
                    re-opened the tier ≈44 s after start, honest "loading and
                    benchmarking" reason replayed in the window)
Gaps 1–2 confirmed fixed (daemon loads + benchmarks installed weights) : yes
Gaps 3–4 confirmed fixed (named proposal, honest lifecycle) : yes
Notes / findings :
  - Two engine defects were found BY this run and fixed before sign-off:
    (1) the harness's word-approximated context budget overflowed the engine's
    4,096-BPE-token window on the first folded `read` ("local engine could not
    serve the turn") — window raised to 16,384 with the mismatch documented;
    (2) a >2,048-token prompt hit llama.cpp's GGML_ASSERT(n_tokens_all <=
    n_batch) and ABORTED the daemon — prompt decoding is now chunked at 2,048
    and over-window prompts are refused with a typed error before llama.cpp
    sees them. Both fixes are part of this branch; the final binary served the
    turn cleanly.
  - Verified on macOS/Apple Silicon ONLY. Nothing here claims Linux (LESSON-433);
    the Linux leg needs its own run and sign-off block.
```

_The macOS sign-off above closes gap 1/2 verification on that platform; the
Linux leg has not been run._

---

## REQ-557 — provider model identity and an explicit default

Everything REQ-557 claims is covered by automated tests except the one leg
below. It is recorded here rather than closed by assertion, at the strength it
was actually verified — which is **not at all**.

### Not automated: `teton provider add --model` for a *remote* kind

**What is uncovered.** AC-1's success path end-to-end through the CLI:
`teton provider add opus --kind anthropic --model claude-opus-5` followed by
`teton provider add sonnet --kind anthropic --model claude-sonnet-5`, then
`teton provider list` showing two providers, same kind, distinct models.

**Why there is no harness — corrected (BUG-155).** The original wording here
claimed there was "no test seam on that path — deliberately, since a seam on a
credential store is a liability." That overstated the case, and the review panel
was right to call it out: a `Keychain` trait and a `MockKeychain` already exist
(`crates/teton/src/keychain.rs`) and are already used by `build_provider_registration`'s
own unit tests one frame down.

The real gap is narrower and closeable: `run_provider_add` hardcodes
`keychain::default_keychain()` rather than accepting an injectable backend, so
the *subprocess* CLI e2e harness cannot substitute the mock. On macOS that means
an automated success leg would write real entries into the developer's login
keychain (and can prompt for keychain access mid-suite). `TETON_PROVIDER_KEY`
supplies the *secret* without a prompt but does not redirect where it is stored.

This is a legitimate near-term exemption — an env-gated backend override is a
security-sensitive seam and deserves its own design — but it is plumbing, not a
structural impossibility, and it should be described that way.

**What IS covered automatically, and how far it goes:**

| Leg | Where | Strength |
|---|---|---|
| Two providers, one kind, distinct models, registered and read back with their models | `tetond/tests/e2e/ac_matrix.rs::ac2_two_remote_providers_complete_sessions` | Full — over the same `config/set` RPC the CLI drives, including the `config/get` round-trip |
| Missing `--model` exits non-zero, names the flag, registers nothing, and never prompts for a credential | `teton/tests/cli_e2e.rs::provider_add_without_a_model_refuses_before_asking_for_a_credential` | Full — through the real CLI binary against a live daemon |
| The same requirement over the `config/set` RPC (the surface every non-`teton` ACP client uses) | `tetond/tests/e2e/model_identity.rs::registering_a_remote_provider_over_rpc_requires_a_model` | Full — BUG-155; the CLI-only guard was bypassable |
| Re-adding an already-registered id fails, changes nothing, and prompts for no credential | `teton/tests/cli_e2e.rs::provider_add_refuses_an_id_that_is_already_registered` | Full — BUG-155; AC-1's third clause had no implementation at all |
| A local-kind add still succeeds with no `--model` and no credential | `teton/tests/cli_e2e.rs::a_local_provider_still_registers_without_a_model` | Full — the local path reaches no keychain |
| `provider list` renders the model, and flags a remote provider that has none | `teton/src/main.rs::provider_list_names_the_model_or_says_the_provider_is_unusable` (unit) + `cli_e2e.rs::provider_list_renders_the_declared_model` (e2e) | Full for the rendering; the e2e leg reaches it via the load-time migration rather than via `provider add` |
| Argument parsing for two same-kind providers with distinct models | `teton/src/main.rs::two_providers_of_one_kind_parse_to_distinct_models` | Full at the parser |

So the gap is narrow and specific: **the keychain write and the CLI's own
success rendering for a remote provider**. The registration payload it produces,
the daemon's handling of it, and every refusal path are all covered.

**To close it by hand** (macOS, ~2 min):

```
TETON_PROVIDER_KEY=dummy-not-a-real-key teton provider add opus   --kind anthropic --model claude-opus-5
TETON_PROVIDER_KEY=dummy-not-a-real-key teton provider add sonnet --kind anthropic --model claude-sonnet-5
teton provider list
TETON_PROVIDER_KEY=dummy-not-a-real-key teton provider add opus   --kind anthropic --model claude-opus-5   # must fail: duplicate id
```

Expected: the first two succeed and report the keychain ref; `provider list`
shows `opus` and `sonnet`, both `[anthropic]`, with `claude-opus-5` and
`claude-sonnet-5` respectively; the fourth fails on the duplicate id. Afterwards
delete the two `teton` generic-password entries from Keychain Access.

**Status: NOT RUN.** No sign-off block below, because nobody has executed it.

---

## REQ-558 — purpose-oriented routing categories

Everything REQ-558 claims is covered by automated tests except the legs below.
They are recorded here at the strength they were **actually** verified, which
for the first one is *not at all*.

### Not automated: the `route` classifier's latency on a real local model

**What is uncovered.** REQ-558 adds a model call to a path that had none: every
freeform judgment turn now waits on a local classification before its real call
starts. The architecture names this as a risk in as many words — *"the latency
cost on the available case is new and unmeasured"* — and the spec's own
assumption is *"the risk is latency, not accuracy."*

**Why there is no harness.** Measuring it needs weights and an engine. CI ships
neither: `tetond` is built without `--features tetond/llama`, and the local tier
in every automated test is `ScriptedFileEngine`, which answers a classification
in microseconds from a string table. A stand-in engine can prove the *call
count* (which it does — see below) but not the wall clock, and a wall-clock
number measured against a string table would be a fabricated one.

**What the estimate is, and where it came from.** TASK-053 sized the prompt and
the output budget for 0.2–0.5 s on a 3B model over Metal
(`CLASSIFIER_INPUT_MAX_BYTES = 2_048`, `CLASSIFIER_MAX_TOKENS = 8`, greedy, a
one-word answer contract). **That figure is a design target, not a measurement.
Nobody has run it.** It is stated here so a later reader does not mistake it for
an observation.

**What IS covered automatically, and how far it goes:**

| Leg | Where | Strength |
|---|---|---|
| The bypass issues **no call at all** when the local tier cannot serve | `tetond/src/classify.rs::a_bypassed_turn_issues_no_call_at_all`, `runtime.rs::an_unavailable_local_tier_bypasses_classification_with_no_call` | Full — asserted by call count on a counting engine, not by output text |
| A structured turn issues **zero** classifier calls | `runtime.rs::dispatch` tests | Full — call count |
| Classification never reaches a remote provider | `teton-core/src/category.rs::route_never_resolves_to_a_bound_provider` **plus** `runtime.rs::the_local_tier_id_is_never_a_registered_remote_providers_id` | Full, but **only as a pair**. The resolver is pure: it can assert `route` resolves to `local_provider_id` and consults no binding, and nothing more — it has no notion of a transport. That the id in that field is engine-backed is `local_tier_id`'s guarantee, pinned separately. Read either test alone and the claim is "it resolved to the expected *name*", which is what let a config registering a remote provider under the id `local` keep both sides green while dispatching over HTTP. |
| The prompt and output budgets are what the design says | `classify.rs` constants + their tests | Full at the constants; says nothing about elapsed time |
| A classifier failure degrades to the declared default and says so | `classify.rs` + `routing.rs` | Full |

**To close it by hand** (macOS/Apple Silicon, ~5 min, after a
`docs/manual-verification.md` REQ-547 AC-13 install has already put weights on
disk):

```sh
cargo build --workspace --release --features tetond/llama
./target/release/tetond &
./target/release/teton
# in the session, with /verbose on, submit a FREEFORM judgment prompt:
#   Explain the tradeoffs between these two architectures, then apply one.
```

Record: the wall-clock gap between submitting the prompt and the first token of
the *answer*, and the same gap for a **structured** turn (which runs no
classifier) as the control. The classifier's cost is the difference. REQ-544
BR-8's duty for a local-tier call is ≤ 1000 ms to first token; a classifier that
eats a large share of that on top of the real call is a finding to record, not a
number to re-run until it looks acceptable.

**Status: NOT RUN.** No sign-off block, because nobody has executed it.

### Recorded exception: a taint-pinned `route_decided` carries no category

AC-8 says every `route_decided` carries a category, a tier, a provider, and a
non-empty reason. **One route is a deliberate exception**: the session-taint
backstop (`Router::resolve_local_pin`, BR-7).

That path consults no binding on purpose — it is a privacy guarantee overriding
every category binding, not a category decision — so it carries
`resolution: None`, and `route_decided` therefore omits `category` and `tier`
entirely (both are `skip_serializing_if = "Option::is_none"`). The provider and
the reason are still present and still non-empty.

This is ADR-D's rule applied to itself: `route_decided` projects the category
and the tier **off the resolution** and recomputes neither, so a route that
resolved no category has none to report. Minting one — "well, it *would* have
been `design`" — would be exactly the second computation ADR-D exists to
prevent, and it would be a lie about which decision was made.

It is asserted rather than dodged, in
`tetond/tests/e2e/routing_categories.rs::a_tainted_session_stays_local_and_the_pre_taint_turn_proves_it_would_not_have`
and in `router.rs::every_route_but_the_taint_pin_carries_its_resolution`. Any
client rendering `route_decided` must handle the absence; `teton`'s own
`/verbose` route line already does.

### Not automated: the `digest` duty running on a real remote provider, end to end

**What is uncovered.** `digest_route`'s remote construction driven by a live
daemon: a `scan` tier (or a `digest` override) bound to a remote provider, an
oversized tool result, and the summarizer's prompt reaching that provider's
endpoint through the real egress choke point.

**Why the existing tests do not close it.** The capture tests in
`harness/digest.rs` build a `DigestRoute::remote` **directly** and drive
`summarize_if_large` over it. That covers everything downstream of the route —
the duty prompt, the provenance scoping, the boundary refusal, the bounded
failure paths — and nothing upstream: the daemon's own `digest_route` decides
locality from `ProviderKind`, resolves the model, and builds the transport, and
no test exercises that construction against a real remote endpoint. The
daemon-level tests that *do* call `digest_route`
(`runtime.rs::dispatch::digest`) assert which provider it chose, not that a
remote choice actually sends.

The **local** direction is closed end to end
(`routing_categories.rs::an_upgraded_config_digests_on_the_local_tier_and_the_file_never_egresses`,
which asserts on captured bytes that a file's contents do not reach the
provider), so the gap is one-directional: nothing shows the remote path
*working*, and the fixture above is what shows it not firing when it should not.

**To close it by hand** (~2 min, against a mock or a real provider):

```sh
teton policy set-tier scan <remote-provider>
teton policy show                       # `digest → <remote-provider> [tier]`
# then, in a session, read a large file and watch the provider's request log:
#   Read <a file over ~2 kB> and summarize it.
```

Record: that the summarizer's prompt (it ends with the
`SUMMARIZER_OUTPUT_CONTRACT` line) appears in a request to
`<remote-provider>`, and that the summary comes back in the follow-up turn's
context. Then repeat in a session that has touched `local-only` content and
record that it does **not** — the BR-7 pin, which is covered at the unit layer
but not through a live remote binding either.

**Status: NOT RUN.**

### Not automated: `teton policy set-tier` / `set-category` through the CLI binary

**What is uncovered.** The two *write* commands end to end through the shipped
binary: `teton policy set-tier think anthropic`, then `teton policy show`
reporting the new binding.

**Why.** The same shape as REQ-557's `provider add` gap above — nothing about
the keychain here, just that no CLI e2e test drives the write path yet.
`teton policy show` **is** now covered end to end
(`teton/tests/cli_e2e.rs::policy_show_renders_the_daemons_resolved_table`), and
the write path's daemon half is covered at the RPC and unit layers
(`runtime.rs::reject_unusable_binding` and its tests, `main.rs`'s parser tests).
What is untested is the CLI's argument plumbing into `config/set` for these two
subcommands.

**Precisely what `reject_unusable_binding` covers**, because it is easy to
overstate: the **usability** leg only — a binding naming a *registered* remote
provider that declares no `model`. It deliberately does **not** answer for an
**unregistered** id; that is `Config::validate`'s, which already names the
provider and lists what is registered, and duplicating the sentence would give
one condition two authors. Both legs are covered, by different code.

**To close it by hand** (~1 min, against any running daemon):

```sh
teton policy show                                   # note `think`'s current binding
teton policy set-tier think deepseek
teton policy show                                   # `think → deepseek [configured]`
teton policy set-category design deepseek
teton policy show                                   # `design … [override]`
teton policy set-tier think no-such-provider        # must fail, naming the registered ids
teton policy set-category redact deepseek           # must fail, naming the pin (BR-4)
```

**Status: NOT RUN.**

---

## REQ-562 — `redact`, a model call inside the egress choke point

Everything REQ-562 claims is covered by automated tests except the leg below,
which is the one the REQ's own BR-9 calls an acceptance criterion rather than a
nice-to-have: **latency**.

### Not automated: the redaction scan's latency on real weights (AC-7, BR-9, ADR-8)

**What is uncovered.** The wall clock. `redact` is unlike every other duty in
this daemon: REQ-558's classifier runs on freeform *judgment* turns only, and
REQ-561's five duties are threshold-triggered, but this one is on the
**synchronous send path of every remote call** once `[privacy] redact = true`.
Every outbound request now waits for a complete local inference over the whole
outbound body before a byte leaves. The spec says so in as many words — *"a
stated budget and a measurement are acceptance criteria, not nice-to-haves"* —
and its own Assumptions section names user tolerance for that latency as one of
the three things most likely to be wrong.

**The stated budget (ADR-8).** On real mid-tier weights, a model call over a
chunk at `REDACT_CHUNK_MAX_BYTES` completes in **p50 ≤ 2 s, p95 ≤ 5 s**. The
budget is **per chunk**: the model pass scans a payload larger than one engine
window in several overlapping calls, so a context-budget-full turn (~41 KiB
body) is two chunks and ~4 s at p50, and a payload at the total cap is five
chunks and ~10 s. The duty seam's
`DUTY_DEADLINE` (120 s) is the hard stop, and an overrun is `Unavailable` →
**Block** (ADR-6) — a timed-out guard does not become a guard that passes
everything, so a machine that misses the budget badly enough degrades into
blocked turns rather than into unscanned ones. That is the failure mode a
measurement is looking for.

**The budget is per *scan*, and a turn is not one scan.** This is the thing the
procedure has to measure and the earlier version of it did not. The gate sits in
`Egress::send`, so it runs **once per remote call**, and one user turn is many
remote calls:

| Multiplier | Where it comes from | Count |
|---|---|---|
| Tool-call iterations | `HarnessConfig::max_turns` — the agent loop calls the provider once per iteration | up to **12** (weak-model default) or **40** (`for_strong_model`) |
| Remotely-bound duties | every `RemoteDuty` send crosses the same choke point | 0–2 per turn typically (`title` once per session, `compact` under pressure) |
| Remote MCP calls | ADR-003 makes a `tools/call` remote egress | 1 per remote MCP tool use |

So a 2-second p50 per scan is **up to 80 seconds added to one long tool-looping
turn** on a strong-model route. A procedure that times the first scan of a turn
measures a number nobody experiences. Measure the **turn**, and measure it
against a control.

**Why there is no harness.** The same reason REQ-558's classifier gap has none,
and more so. CI ships no weights: `tetond` is built without
`--features tetond/llama`, and every automated fixture's local tier is a
`ScriptedFileEngine` or a canned mock, which answers a scan of a chunk at the
27,070-byte window from a string table in microseconds. A stand-in can prove the
*call count* (including the **chunk** count, which is the same number), the
*caps*, and the *decision* — it does, exhaustively — but a wall-clock number
measured against a string table would be a fabricated one.

**The budget's provenance, stated so nobody mistakes it for an observation.**
2 s / 5 s is a **design target**. It was sized against a 3B model on Metal from
an input cap of 64 KiB — which is **not what a model call sees any more**: a
call sees one chunk, `REDACT_CHUNK_MAX_BYTES`, derived from the engine window at
**27,070 bytes** (≈13.5k tokens at the duty seam's 2-bytes-per-token
convention), less than half what the target was sized for. The output side is
tiny either way (`REDACT_OUTPUT_MAX_BYTES` = 2 KiB, a sixteen-line contract). So
the per-chunk target is if anything *pessimistic* now — but a chunked scan makes
**several** such calls, which is the direction that cuts the other way, and
**nobody has run either number**. The measurement is what settles it; do not
re-derive it from the window and call that a result.

**What IS covered automatically, and how far it goes:**

| Leg | Where | Strength |
|---|---|---|
| A payload past the **total** cap costs **zero** model calls and blocks | `harness::redact::tests::an_over_cap_payload_is_unavailable_before_any_model_call`, `tests/redact_egress.rs::a_payload_past_the_input_cap_blocks_unscanned_and_costs_no_model_call` | Full — by model-call count, not by elapsed time |
| A payload past one engine **window** is scanned in several calls, not refused | `harness::redact::tests::a_payload_larger_than_one_window_is_scanned_in_several_calls_and_forwards`, `tests/redact_egress.rs::a_context_budget_full_payload_is_scanned_across_windows_and_forwards` | Full at the count — the number of model calls is what tells chunking from a raised cap. Says nothing about what those calls cost, which is step 4 |
| The chunk count is bounded, and every chunk fits the window | `harness::redact::tests::{the_chunker_never_cuts_more_chunks_than_the_derived_ceiling, the_chunker_covers_the_payload_with_overlapping_windows_on_char_boundaries}` | Full. This is what makes "p50 × chunks" a bounded number rather than an open one |
| A deadline overrun is `Unavailable` → Block | `harness::redact::tests::a_scan_that_overruns_the_deadline_is_unavailable` | Full, on a **paused clock**. It pins the wiring, and says nothing about how long a real scan takes |
| A whole scan waits at most **one** `DUTY_DEADLINE`, not one per chunk | `harness::redact::tests::a_scan_whose_chunks_each_answer_in_time_still_stops_at_one_scan_deadline` | Full at the bound, on a **paused clock** — a first chunk answering at ⅔ of the budget and a second that stalls. It says nothing about real latency; what it removes is the ×`REDACT_MAX_CHUNKS` worst case the chunked design introduced |
| Off costs nothing at all — no gate, no call, no latency | `runtime::duty::dispatch::redact::off_means_no_gate_and_on_means_a_gate_that_reaches_the_engine`, `egress::tests::off_means_zero_scanner_calls_and_on_means_exactly_one` | Full — by call count. This is what bounds the blast radius of the unmeasured budget: nobody who has not opted in pays it (OQ-3) |
| The scan runs once per outbound payload, not more | `tests/redact_egress.rs::a_clean_payload_forwards_and_the_scan_provably_ran` | Full at the count; says nothing about elapsed time — and "once per payload" is many times per *turn*, which is what step 2 measures |
| Engine-mutex contention between sessions | — | **Nothing.** Every automated fixture answers from a string table in microseconds, so no fixture can hold the mutex long enough for a second session to notice. Step 3 is the only instrument |

**To close it by hand** (macOS/Apple Silicon, ~10 min, after a REQ-547 AC-13
install has already put weights on disk):

```sh
cargo build --workspace --release --features tetond/llama
# opt in — the switch is its own table, NOT a [[categories]] row (BR-10).
# The daemon reads $TETON_CONFIG, else <base>/config.toml, where <base> is
# $XDG_RUNTIME_DIR/teton or, on macOS, ~/Library/Application Support/teton.
cat >> ~/"Library/Application Support/teton/config.toml" <<'EOF'
[privacy]
redact = true
EOF
./target/release/teton-code &
./target/release/teton
```

Every step below is run **twice**: once as written, and once with
`redact = false` as the control. **The scan's cost is the difference**, not the
absolute number — the remote call is in both. Record p50 and p95 over the stated
run count for each half, and record the difference.

**Step 1 — the single scan (ADR-8's stated budget).** With `/verbose` on, run
TWENTY remote turns of two shapes, each answered without a tool call:

- a short prompt — the everyday case;
- a prompt whose assembled context is **at the input cap** — read several large
  files first, then ask a question; `/verbose` shows the context size.

Measure the gap between submitting the prompt and the first token of the answer.
Check against ADR-8: p50 ≤ 2 s, p95 ≤ 5 s **at the cap**.

**Step 2 — the whole turn (the number a user actually feels).** Run TEN turns
that force the agent loop to iterate: *"read every file under `crates/teton-core/src/`
and list the public types in each"* is one shape that reliably produces a
double-digit tool loop. Measure **total wall-clock from submit to the turn
ending**, and record alongside it the **number of remote calls the turn made**
(`/verbose` shows each `route_decided`; count them, and add any `tools/call` to a
remote MCP server).

Then check the arithmetic: `turn_delta ≈ remote_calls × step_1_p50`. If it does
not, say so — either the scan is faster on the short intermediate payloads (good,
and worth recording as the real shape of the cost) or something else is serial
that this document has not accounted for. **A turn that takes more than 30 s
longer with the switch on is a finding**, whatever the per-scan number said.

**Step 3 — two sessions at once (contention on the one engine).** The scan holds
the **single local engine mutex** for its whole completion, and it is the first
duty that runs unconditionally, so two sessions contend on every remote call. In
two terminals against the same daemon:

1. In session A, start a long **local** turn — a prompt routed to the local tier
   (a tainted session, or `teton policy set-tier build local`) over a large
   context, so the engine mutex is held for many seconds.
2. Immediately, in session B, submit an ordinary **remote** turn.

Record session B's time to first token. With `redact = false` it does not touch
the engine at all and should be unaffected; with the switch on it cannot start
its scan until A's completion releases the mutex. **Record the difference and
whether B ever hit the 120-second `DUTY_DEADLINE`** — that is the fail-closed
timeout, and it means B's turn was *blocked* by A's unrelated work.

**Also record, in every step, how often the scan came back `Unavailable`** (a
`privacy_block` with cause `scan_unavailable`, or a turn failing with *"the
redaction scan could not run"*): at the cap on slow weights that is the deadline
firing, and a fail-closed timeout is a worse user outcome than a slow one.

**Step 4 — the chunk-count distribution and the per-chunk latency (what
replaced the over-cap block rate).**

Round 2 of this REQ measured a *block rate*: `REDACT_INPUT_MAX_BYTES` was
27,070 bytes against a `context_budget_bytes` of 32,768, so a
context-budget-full remote turn assembled a body the scan refused and blocked
with `ScanUnavailable`. That collision is **closed** — the model pass chunks
now (ADR-6), the total cap is 108,280 bytes, and a context-heavy turn is
scanned in two model calls instead of refused. There is nothing left to measure
a rate of, and a step that kept counting a number that should now be zero would
be measuring nothing.

What replaced it is the number the cost actually moved to: **how many chunks a
real turn's payload is cut into, and what each chunk costs.** Chunking did not
delete the latency; it converted a fail-closed block into a multiple of the
per-chunk budget, and whether that trade is a good one is exactly what a
measurement can tell you and an argument cannot.

Over the twenty turns of step 1 and the ten of step 2:

| Record | How |
|---|---|
| **Chunk-count distribution** | count `route_decided` events with `category: redact` per outbound payload — a chunked scan announces **once per chunk** (ADR-8), so the count per send *is* the chunk count. Report the histogram: how many sends were 1 chunk, 2, 3+ |
| **Per-chunk latency** | for multi-chunk sends, the send's total scan time divided by its chunk count. Check against ADR-8: p50 ≤ 2 s, p95 ≤ 5 s **per chunk** — the budget is per chunk, not per send |
| **The context-budget-full send specifically** | force one deliberately (below) and record its chunk count and total scan time. ADR-8 expects **2 chunks, ≤ 4 s p50**. Anything much over two chunks at that size means the body carries more overhead than the cap's arithmetic assumes |
| **Any `ScanUnavailable` at all** | it should no longer come from size. If one appears, record the payload size and whether the daemon log shows an engine error — a block from a payload under 108,280 bytes is now a **finding**, not the expected fail-closed path |

Force the multi-chunk state deliberately at least once: read four or five large
files in one turn until `/verbose` shows the context near budget, then ask a
question. That turn should **succeed**, in two chunks. Before this change it
blocked; if it still blocks, the change did not land and that is the single most
important line to write in this document.

**A turn whose scan takes more than 10 s is a finding**, whatever the per-chunk
number said — that is the `REDACT_MAX_CHUNKS` ceiling (5) at ADR-8's p50, and a
payload that reaches it is at the total cap.

**Also record the worst-case wait.** The seam's `DUTY_DEADLINE` is per model
call, so a five-chunk scan can in principle wait `5 × 120 s` before failing
closed (ADR-8's stated residual). Note any send whose scan took longer than 120
seconds without failing — that is the residual becoming real rather than
theoretical, and it is the evidence a scan-wide deadline needs.

A miss anywhere is a finding to record here, not a number to re-run until it
looks acceptable — and the honest response to a miss is a smaller total cap, a
faster tier, or a scan that does not run on every call, never a partial scan
that reports itself complete (BR-7).

**Also worth recording while the weights are loaded** (it is the assumption the
REQ says is most likely to be wrong, and this is the only place it can be
observed): of the payloads the scan flagged, **how many did the model catch that
the pattern pass did not** — the `Confidence::Low` findings. That question, not
raw recall, is what tells you whether the model call earns its latency (OQ-2's
recorded counter-argument).

**Where to read it.** A low-confidence finding forwards (BR-4), so it produces
no `privacy_block` and nothing in the CLI. It writes one line per finding to the
daemon's log — `tetond`'s stderr, i.e. `<base>/tetond.log`:

```sh
grep 'redact — low-confidence' ~/"Library/Application Support/teton/tetond.log"
# tetond: redact — low-confidence credential at bytes 1402-1440 of the outbound
# payload; forwarded (a low-confidence finding is reported, not blocked — BR-4).
```

Kind and byte span, never the matched text (BR-6) — the span is what lets you
find the string yourself in the payload you sent.

Procedure: plant a handful of paraphrased credentials that no pattern shape
matches (`the deploy password is <something>`, a connection string described in
prose, an address), run one remote turn each, and count the lines. **Zero lines
across the whole set is the finding**: it means the model half caught nothing
the pattern pass could not, and OQ-2's recorded counter-argument — that the
pattern pass makes the feature *look* like it works — has come true. Record the
count either way; a number here is the only evidence the model call earns its
latency.

**Status: NOT RUN.** No sign-off block below, because nobody has executed it.

---

## REQ-564 — persistent llama context (prefix-cached KV)

REQ-564's policy, plumbing, window guard and accounting are all covered by
`crates/tetond/tests/prefix_cache_session.rs`, which runs the **same**
`PrefixCacheState` and the **same** `over_window` guard the real engine runs.
One claim is left over, and it is the claim the whole REQ exists for.

### Not automated: that llama.cpp actually reuses the KV

**What is uncovered.** Everything below the trait. The `llama` cargo feature is
non-default and CI never compiles it, so no automated run in this repository
links llama.cpp at all. The acceptance suite proves that *when an engine reports
a hit, the harness prefills only the suffix, reports the split honestly, and
records it* — it cannot prove that truncating the KV at `clear_kv_cache_seq` and
decoding from position `reuse` produces the same logits as a full prefill,
because nothing in a default build ever calls those functions.

Reading a green suite as "prefix reuse works" is exactly the overclaim
LESSON-448 names: a fast test double proving a latency property it could not
observe.

**The baseline.** The requirement's own measurement, 2026-08-09 dogfooding on an
M5 Max with the 17 GB model: **211 context create/destroy cycles** in the daemon
log, and a single user question driving an 11-generation agent loop that took
over five minutes of wall time, most of it redundant prefill.

**Procedure.**

1. Build the workspace first — *not* a targeted test target. A
   `-p teton --test …` run does not rebuild `tetond`, so the CLI would drive a
   stale daemon and the measurement would describe the old binary:
   ```
   cargo build --workspace --features tetond/llama
   ```
2. Start a daemon with a real model on the large band and note its log path.
3. Run one multi-turn agent session — at least five turns that build on each
   other, so each turn's prompt genuinely extends the previous one. A session of
   unrelated one-shot questions is not a test of this feature.
4. Record, from the daemon log and the `prefix_cache` events:
   - context create/destroy cycles for the session (the 211-cycle number's
     successor);
   - per-turn `cached_tokens`, `new_tokens` and `divergent` on each
     `prefix_cache_hit`. A `divergent: true` hit is a turn whose history was
     rewritten mid-stream — on a weak model that is usually a BUG-147
     fabrication cut — reusing the kept head and re-prefilling the tail (BR-2
     as amended 2026-08-10). A fabricating session should show divergent
     *hits* with non-zero `cached_tokens`, not `divergent` *misses*; a run
     full of `divergent` misses means the pre-amendment all-or-nothing rule is
     somehow back;
   - wall time for the whole session, against the >5 minute baseline.
5. Confirm correctness alongside the latency: the answers must be coherent
   across turns. A cache bug that reuses a wrong offset shows up as an
   answer that drifts or repeats, not as an error.
6. Note the **duty interleave** in the same run: a `summarize` or `triage` duty
   between two agent turns must not turn the following agent turn into a miss
   (BR-5). Grep the events for a `miss` whose reason is `evicted` or
   `session_switch` immediately after a duty — that is the failure this looks
   for.

**What a failure looks like.** Cycles roughly unchanged from 211 means the
policy is deciding "hit" and the mechanism is not delivering — check that the
recorded prefix includes the generated tokens, not just the prompt. Coherent
counts with incoherent answers means the reuse offset and the KV disagree, which
is the correctness bug the offset arithmetic exists to avoid.

**Also worth measuring while you are there.** Peak RSS during a duty call. With
the cache resident, a duty's own throwaway context coexists with the agent's, so
peak KV is two contexts rather than one (~1.5 GiB each at `n_ctx` 16384 when
this was written; ~3 GiB each since `n_ctx` went to 32768). The
architecture accepts this as bounded and transient with eviction as the
compensating control; a number here would tell us whether sizing duty contexts
to their own budgets should be promoted from follow-up to urgent.

**Status: RUN — sign-off below (2026-08-10).**

### Sign-off: 2026-08-10, M5 Max 48 GB, qwen3-coder-30b-a3b (17 GB), main @ d6df9b7

Run after the BR-2 amendment (PR #82) landed, as the amendment's own note
required. Isolated daemon (`XDG_RUNTIME_DIR=/tmp/…`, weights symlinked), the
workspace built `--release --features tetond/llama` first. The session was a
scripted 6-prompt run driven over the CLI's stdin by a pacing harness (the CLI
buffers piped stdout mid-turn, so a driver must pace on the `› ` ready marker,
not on output growth). Model benchmark at load: first token 151 ms, 97.2 tok/s.

**Numbers** (from the per-generation `prefix_cache` events):

- 14 generations across 6 prompts; **1 cold miss** (the first generation),
  **13 hits**, of which **5 divergent**. No `session_switch`, no `evicted`,
  no divergent *misses*.
- 14,799 prompt tokens total; **12,413 reused from KV (83.9%)**, 2,386
  prefilled.
- Context creations for the whole run: **2** (the load-time benchmark's, and
  the session's one persistent context created at the cold miss) against the
  211-cycle baseline. Nothing was destroyed mid-session.
- Wall time: 61.1 s for the whole session including model load; per-prompt
  turn latency 1–3 s against the >5-minute baseline session.
- Peak daemon RSS 20.4 GiB; no second-context spike was observed (no separate
  duty context was allocated in this run — see the interleave note).

**Where the divergent hits came from.** Every prompt boundary (5 of 5)
produced a hit with `divergent: true` reusing exactly the ~814-token system
prompt head with ~20–34 new tokens — not a BUG-147 fabrication cut. The cause
is structural: the runtime builds a **fresh `ContextManager` per prompt**
(`runtime.rs` — `ContextManager::new` + `push_user` per dispatch), so an
interactive session's consecutive prompts share only the system prompt, and
the boundary is a mid-stream divergence every time. Under pre-amendment BR-2
all five boundaries would have been divergent cold misses (~4,100 tokens
re-prefilled); measured reuse would have dropped from 83.9% to ~56%. The
amendment is what keeps prompt boundaries warm in the session shape that
actually ships. Within a prompt's agent loop, every generation was a pure
extension hit (reuse growing 850 → 1,531), as designed.

**Correctness.** Answers were checked against the fixture files: the summary,
the 5-entry checklist listing, the exact "Aug 4" quote, the 4-row/3-column CSV
description, and the cross-file reconciliation (invoice 102, $8,400) were all
correct and coherent; no drift, repetition-past-the-answer, or garbling that a
wrong reuse offset would produce. (The final "recap the conversation" prompt
was answered with a request to re-share the files — that is the per-prompt
context reset above, a session-layer property reproduced identically with the
cache untouched, not a KV-coherence failure. Logged for follow-up.)

**Duty interleave.** The route classifier ran between agent generations
throughout (5 classifier calls interleaved with agent turns on the cached
path); the agent generation following each still hit with reuse ≥ 882 and no
eviction was emitted — the interleave never cost the agent its cache (BR-5).

**Failure-shape watch items from the procedure:** cycles are 2, not ~211, so
the mechanism delivers; counts were coherent with correct answers, so the
reuse offset and the KV agree.

## REQ-567 — cross-prompt conversation carry

REQ-567's mechanics are covered end to end without a model: what carries
(`runtime::tests::conversation_carry`), what it costs at the cache and the
ledger (`crates/tetond/tests/conversation_carry.rs`, running the real
`PrefixCacheState`), and what it must never leak
(`crates/tetond/tests/e2e/conversation_carry.rs`, over the real socket with
egress capture). Two claims are left over, and they are the two the REQ was
written from — both need a real model, and both are answered by one session.

### Not automated: that a carried conversation is a conversation the model uses

**What is uncovered.** AC-1's tool-free recap *answering*, and AC-8's real-model
boundary measurement.

The scripted legs prove that the recap prompt's material is in the context the
engine was handed, and that the reuse policy reports the whole retained
conversation as cached. Neither can prove that a real model, shown that
context, answers from it rather than asking for the files again — a scripted
engine answers from a script — nor that llama.cpp truly reuses that many KV
tokens, since no default build links llama.cpp at all (the REQ-564 section above
says the same at greater length, and it is the same limitation).

**The baseline this supersedes.** The 2026-08-10 REQ-564 sign-off, above. Two of
its observations are the ones to replace, and they were the same defect seen
from two directions:

- *the product symptom*: its final "recap the conversation" prompt was answered
  with a request to re-share the files (logged there as a session-layer
  follow-up — this REQ is that follow-up);
- *the measurement*: **all 5 of 5 prompt boundaries** were `divergent: true`
  hits reusing exactly the ~814-token system head with ~20–34 new tokens,
  because a fresh `ContextManager` per prompt left consecutive prompts sharing
  only the system prompt. Session totals were 14 generations, 14,799 prompt
  tokens, 12,413 reused (83.9%).

A REQ-567 run should move both. Note that the *within-prompt* generations were
already pure-extension hits before this REQ; the number that must change is the
boundary one.

**Procedure.**

1. Build the workspace first — *not* a targeted test target. A `-p teton --test
   …` run does not rebuild `tetond`, so the CLI would drive a stale daemon and
   the measurement would describe the old binary:
   ```
   cargo build --workspace --features tetond/llama
   ```
2. Start an isolated daemon with a real model (short `XDG_RUNTIME_DIR`,
   symlinked weights) and note its log path — the REQ-564 run's setup.
3. Drive **one six-prompt session** whose prompts build on each other and whose
   **last prompt is the recap**, e.g.: read a file; ask a follow-up about what
   was read; read a second file; ask something that needs both; ask a question
   answerable from the conversation alone; then finally *"recap what we
   established"*. The recap must name nothing — no paths, no restatement — or
   it is not testing the conversation.
4. Read the recap answer. It passes when it answers **from the conversation**:
   it re-reads no file (no `read`/`grep`/`glob` tool call in that turn) and it
   names facts established in prompts 1–5. It fails in the shape the REQ exists
   for if it asks for the files again.
5. Record, from the `prefix_cache` events, the **boundary** rows specifically —
   the first generation of each prompt after the first:
   - `cached_tokens` must be far above the ~814-token system head, and should be
     close to the previous generation's prompt + generated total (that is AC-8's
     scripted assertion, and the real-model number is what this leg adds);
   - `divergent` should be `false` at a well-behaved boundary. A `divergent:
     true` boundary is not automatically a failure: a compaction, a truncation,
     or a BUG-147 fabrication cut in the previous turn each legitimately produce
     one (spec BR-4, LESSON-500). What it must never be is *silent* — pair every
     divergent boundary with the compaction/cut that explains it, and if none
     exists, that is the finding.
6. Record the session totals against the baseline: generations, prompt tokens,
   reused tokens and percentage, context create/destroy cycles, wall time.
7. Note the context budget: a six-prompt session carrying two files may reach
   compaction, which is the interesting case (AC-3's real-model counterpart).
   If compaction fires, the turn must still complete and the answer must stay
   coherent.
8. Then type `/clear` and prompt once more. The next turn's context must start
   from the system head alone: the prefix-cache boundary drops back to ~head
   reuse (a `divergent` hit, per architecture D-5 — no eviction is expected),
   and the model no longer knows what was established.

**Driving it with a piped stdin.** The REQ-564 sign-off used a scripted driver;
its pitfalls are the same here and each one silently corrupts a run rather than
failing it:

- the CLI buffers piped stdout mid-turn, so a driver must pace on the `› `
  ready marker, never on output growth — otherwise prompts arrive while a turn
  is still running and are answered out of order, or refused as
  `SESSION_BUSY` (which, since REQ-567, is what a second concurrent prompt now
  gets);
- permission prompts are effectively invisible to a piped driver: a turn parked
  on one looks exactly like a slow turn. Pre-grant what the session will need,
  or drive it by hand;
- name the files explicitly in prompts 1–4. A prompt that says "the file we
  looked at" is testing the recap in the middle of the session rather than at
  the end of it.

**What a failure looks like.** A recap answered with "please share the files"
means the conversation is not reaching the model — the same symptom this REQ was
opened for. Boundary `cached_tokens` still pinned near ~814 means the
conversation reaches the model but not the KV: check that what was committed is
byte-identical to what was rendered (REQ-554 determinism), because a boundary
that re-renders differently diverges at the first changed token.

**Status: RUN — sign-off below (2026-08-10).**

### Sign-off: 2026-08-10, M5 Max 48 GB, qwen3-coder-30b-a3b (17 GB), main @ a8c779c

Same isolated-daemon setup and piped driver as the REQ-564 sign-off (paced on
the `› ` marker; files named in prompts 1–4 only). One eight-line session:
six building prompts ending in a nothing-named recap, then `/clear`, then a
no-tools probe. Benchmark at load: first token 193 ms, 92.5 tok/s.

**The recap (step 4): PASS.** One generation, zero tool calls, and the answer
names facts from all five prior prompts — the 5-entry checklist, the Aug 4
cron line, invoice 102 (Borealis Cloud, 8,400) as the largest, the
data.csv-reconciliation entry, and the Thursday vendor call. This is the exact
prompt shape the REQ-564 sign-off recorded failing with "could you please
share the relevant files?". Prompt 5 also answered from the carried
conversation with no tool call.

**Boundaries (step 5): PASS.** Every prompt boundary after the first was a
`divergent: false` hit far above the ~814-token head, tracking the previous
turn's prompt + generated total:

| boundary | cached_tokens | divergent |
|---|---|---|
| prompt 2 | 1,101 | false |
| prompt 3 | 1,383 | false |
| prompt 4 | 1,658 | false |
| prompt 5 | 1,889 | false |
| prompt 6 (recap) | 1,985 | false |
| post-`/clear` probe | 814 | true |

The baseline's "5/5 boundaries divergent at ~814" is fully displaced; the one
`divergent: true` boundary is the post-clear probe, exactly architecture
D-5's no-eviction prediction, paired with its explanation (the clear).

**Totals (step 6)** vs the REQ-564 baseline: 11 generations (was 14), 15,680
prompt tokens, 13,951 reused — **89.0%** (was 83.9%) — 1 session context
creation plus the load-time benchmark, wall 59.1 s including model load,
peak daemon RSS 20.2 GiB. Turns 2–3 chose to re-read notes.txt anyway (weak
model re-verifying); turns 5–6 did not need to, which is the claim under
test.

**Budget (step 7):** compaction never fired — a six-prompt two-file session
stays well under the budget. The compaction-under-carry case remains covered
by the scripted AC-3 leg only; a longer dogfood would be needed to observe it
live.

**`/clear` (step 8): PASS.** The notice rendered exactly once ("context
cleared; 20 retained blocks dropped."), the next boundary dropped to exactly
the 814-token head as a `divergent` hit, and the probe's answer named nothing
from the cleared conversation. One behavior worth recording: asked "without
using any tools, list what we have discussed", the post-clear model
paraphrased its **system prompt** (tools, providers, configuration) rather
than saying "nothing yet" — correct on the load-bearing claim (no cleared
content resurfaced), just a weak model filling silence with what it can see.

## REQ-572 — capability-aware refusals and guided web enablement

REQ-572's machinery is covered without a model or a keychain: the per-state
prompt clauses (`tetond/src/harness/turn_loop.rs`), the classifier every
consumer reads (`teton-core/src/capability.rs`), the plan/preview/commit
endpoints and their atomic write (`tetond/src/runtime.rs`), and the whole
client flow against a fake keychain and a scripted daemon
(`teton/src/web_setup_ui.rs`). Three legs are left over, and each needs
something CI does not have: a real model's *prose* (twice), and a real OS
keychain (once).

A rule that holds in all three, and is worth watching for rather than assuming:
**the model may name the opt-in; only the user can run it.** `/web setup` is a
client command, and tool dispatch has no path to that table
(`teton/src/slash.rs`, `handle_web_setup`) — so the only failure available to a
model here is *prose claiming* it enabled something. If you see that, it is a
finding.

### Not automated: that a real model refuses a web question by naming the opt-in (AC-1)

**What is uncovered.** The prompt clause is pinned by content
(`the_off_clause_names_the_capability_its_off_state_and_both_enablement_paths`,
`turn_loop.rs`), and the turn has no web tool to call in this state at all
(`registration_is_the_capability_classifiers_exposure_predicate`,
`harness/tools/web.rs`). What no test can prove is that a real model, handed
that clause, *uses* it: says the capability is off, names `/web setup`, and
stops — instead of answering from stale weights or hunting through the open
repository for Teton's config, which is BUG-160's failure one capability over.

**Procedure.**

1. Build the workspace — *not* a targeted test target, or the CLI drives a
   stale daemon (BUG-164):
   ```
   cargo build --workspace --features tetond/llama
   ```
2. Start an isolated daemon with a real model (short `XDG_RUNTIME_DIR`,
   symlinked weights — the REQ-564 setup) over a config with **no** `[web]`
   table, so the capability derives as `OffAvailable`:
   ```
   grep -n '^\[web\]' "$TETON_CONFIG"      # must print nothing
   ```
3. Open a session in a directory that is a real repository (the hunt this leg
   watches for needs somewhere to hunt), and ask a question the weights cannot
   answer, e.g.:
   ```
   What is the current released version of tokio on crates.io?
   ```
4. **Expect**: an answer that says web lookup is available on this machine but
   off, names `/web setup` (naming the `[web]` config table as well is correct,
   not a second failure), and makes **zero tool calls** in that turn — tool
   lines always render, so an empty turn is visibly empty (`/verbose` adds the
   routing notices, which are worth having for context).
5. **Failure shapes**, in the order they are worth reporting: a `grep`/`glob`/
   `read` sweep for the opt-in (the clause's third sentence did not land); a
   confident version number with no hedge (the refusal ending was not taken at
   all); a refusal that names no enablement path (the clause reached the model
   but not the answer); prose claiming the model turned the capability on.
6. Note what the status row does, and do not expect it to appear: a session
   that has not touched the web draws no `web:` row, capability or not — that
   is deliberate and pinned by
   `the_capability_alone_never_makes_the_row_appear` (`teton/src/session_ui.rs`).

**Status: NOT RUN.**

### Not automated: that the second offer in a conversation is one line (AC-9)

**What is uncovered.** The *instruction* is pinned
(`every_capability_clause_carries_the_repeat_instruction_and_only_a_clause_does`,
`turn_loop.rs`): every capability clause ends with "If you already said this
earlier in this conversation, refer back to it in one line." Whether a model
obeys it is model behavior, and this is the leg that reads it.

**Procedure.** In the same session as the leg above, immediately after the first
refusal, ask a second web-needing question on a different subject (e.g. "and
what did the last Rust release change about `impl Trait`?").

**Expect**: the second answer refers back to the offer in about a line — "as
above, this needs `/web setup`" — rather than repeating the paragraph. A verbatim
repeat is the failure this instruction exists to prevent; it is a wording
problem, not a wiring one, so record the exact text of both answers.

**Status: NOT RUN.**

### Not automated: the flow against a real OS keychain (AC-5/AC-6, macOS)

**What is uncovered.** Every keychain assertion in the suite runs against
`MockKeychain`. The real `security_framework` calls — the store, the delete, and
the fact that the entry lands under the service and account the config
reference names — are exercised nowhere, exactly as `teton provider add`'s
write is not (see the REQ-557 section above; this is the same gap for the same
reason).

**Run it once on macOS, with a config you are willing to have written** (point
`TETON_CONFIG` at a scratch file). A real search key is not needed — any dummy
string proves storage. Seed that scratch file by hand first, with a comment and
a key this build has never heard of — REQ-574 made the commit an in-place edit
of the document on disk, and a hand-commented config is the thing worth
watching a real write survive:

```
cat > "$TETON_CONFIG" <<'EOF'
# My machine. Hand-written, and staying that way.
effort = "high"

[web]
# This line sits on the key the flow changes.
tier = "off"
# Nothing in this build reads this key.
experimental_reranker = "colbert"
EOF
```

1. Start the daemon, then run `teton` **in a real terminal**. A piped session is
   a different, already-automated path (it prints instructions and reads
   nothing).
2. `/web setup`, and answer: `3` (search) →
   `https://api.search.brave.com/res/v1/web/search` → `y` (needs a key) →
   `X-Subscription-Token: {key}` → a dummy key → `y` at
   `write this to your config? [y/N]`.
3. **Expect** the key prompt to echo nothing as you type, the preview to show
   the exact `[web]` table — **including the comments already in your file**,
   since the preview is sliced from the document the commit will write rather
   than re-rendered (REQ-574 BR-3) — and `searches would go to:
   api.search.brave.com`, and the completion notice:
   ```
   web lookup enabled (`search`) — written to your Teton config. Nothing has
   been looked up yet: the next web-needing question will ask before anything
   leaves the machine.
   ```
   The notice names no path on purpose: it reaches every open session, and an
   absolute config path is a home directory on somebody else's screen.
4. Check the store and the config, in that order:
   ```
   security find-generic-password -s teton -a web-search      # must succeed
   grep -n 'search_key_ref' "$TETON_CONFIG"                   # keychain://teton/web-search
   grep -c '<the dummy key>' "$TETON_CONFIG"                  # must be 0
   grep -n 'staying that way' "$TETON_CONFIG"                 # your comment survived
   grep -n 'sits on the key' "$TETON_CONFIG"                  # and so did the one on `tier`
   grep -n 'experimental_reranker' "$TETON_CONFIG"            # as did the unknown key
   ```
   Nothing in the session transcript should contain the dummy key either. The
   three `grep`s for your own text are the manual half of REQ-574's preservation
   claim — the middle one is the interesting one, since that comment sits
   directly on a key the commit rewrites: only `tier` and the keys `/web setup`
   sets may have moved.
5. **Abort path.** Remove the entry, then run the flow again and answer `n` at
   the confirm:
   ```
   security delete-generic-password -s teton -a web-search
   ```
   Expect nothing stored (`security find-generic-password -s teton -a web-search`
   exits non-zero with "The specified item could not be found in the keychain.")
   and the config byte-identical — the store deliberately happens *after* the
   confirm, so a declined preview never reaches the keychain at all.
6. **Cleanup-after-a-failed-commit path** (optional, and the only way to see the
   delete run for real): put the config in a directory you then make read-only
   (`chmod 555`), so the daemon's atomic write fails *after* the key is stored.
   Run the flow and confirm. Expect the daemon's own sentence ("the
   configuration could not be saved …") followed by "the key that was stored for
   this attempt has been removed from your keychain.", and
   `security find-generic-password -s teton -a web-search` failing afterwards.
   Restore the directory's mode when done.
7. Delete any leftover `teton` / `web-search` entry from Keychain Access when
   you are finished.

**Known residual, not a finding.** Ctrl-C *during* the key prompt kills the
process before the echo-restoring guard runs, leaving the terminal with echo
off; `stty sane` fixes it. Every `read -s`-shaped prompt without a signal
handler has this window. The ordinary aborts (Enter, EOF) restore normally — if
*those* leave the terminal silent, that is a real defect.

**Status: NOT RUN.**

---

# Manual verification runbook — REQ-570 AC-3b

**Status: OUTSTANDING. Nobody has run this yet.**

AC-3b is the second claim in this repo that CI structurally cannot make, and the
reason is sharper than REQ-547 AC-13's. That one needed real hardware and a real
network. This one needs **a human being**, by construction: REQ-570 BR-2 requires
a presence mechanism "a headless same-UID process cannot satisfy without a human
acting at the machine", and CI *is* a headless same-UID process. A CI job that
could tick this box would be a CI job that disproves the feature.

This is why AC-3 was amended on 2026-08-11. Its original clause — "no
test-harness auto-consent anywhere in the path" — asked a machine to demonstrate
that a machine cannot do something, by doing it. The amended AC-3 says "no
auto-consent in any **shipped build**", which is mechanically checkable and is
what the `TETON_TEST_SEAMS` contract already guarantees (a release build refuses
to start when it is set, so `TETON_PRESENCE_ACCEPT` cannot exist in a shipped
artifact). AC-3b carries the part that remains, and a human carries it.

> **Do not tick AC-3b in
> `.adlc/specs/REQ-570-human-attested-attach-consent/requirement.md` until a
> sign-off block below is filled in by a person who ran it.** An unticked AC-3b
> beside a green CI run is the honest state of the world (LESSON-433).

## What this proves that CI does not

| Claim | Proven by CI? | Why not |
|---|---|---|
| An unattested approval mints nothing | **yes** | fail-closed verifier, grant registry asserted empty |
| Each refusal ending is distinct and mints nothing | **yes** | table-driven over the refusal taxonomy |
| The no-mechanism posture refuses rather than self-approves | **yes** | injected `UnavailableVerifier` |
| Every daemon-wide method refuses a failing connection | **yes** | per-method, mutation-checked |
| **A real Touch ID prompt appears during a real resume** | **no** | needs a human finger |
| **Exactly ONE consent step is visible to the user** | **no** | "visible" is a property of a person's screen |
| **The prompt's wording is comprehensible at the moment of trust** | **no** | no machine can judge this |
| **A non-interactive CLI invocation does not auto-approve** | partly | asserted in-process; the shipped-binary path wants eyes |

## Procedure

1. Build with the mechanism compiled in — the default build has none:
   `cargo build --features presence` (macOS only; `presence` is macOS-gated).
2. Ensure `TETON_TEST_SEAMS` and `TETON_PRESENCE_ACCEPT` are **unset**. If
   either is set you are testing the seam, not the feature. Confirm with `env`.
3. Start the daemon from that build. Create a session with `teton` and send at
   least one prompt so the session has state worth resuming.
4. Exit the CLI **without** stopping the daemon (the session outlives its
   client — REQ-565/567).
5. Start a **fresh** `teton` and resume that session.
6. Record: how many consent steps were visible (AC-3 says exactly one), whether
   a real OS presence prompt appeared, what it said, and whether the resume
   completed after authenticating.
7. Cancel the prompt on a second attempt and confirm the attach is **refused**
   and no grant is minted (`grant_minted` should not fire).
8. Run a non-interactive invocation (piped stdin / no TTY) and confirm it does
   **not** auto-approve (AC-4).

## Sign-off

```
AC-3b sign-off
--------------
Verified by      :
Date             :
Platform / OS    :               (e.g. macOS 26.6, Apple M-series)
Build            :               (cargo build --features presence)
TETON_TEST_SEAMS / TETON_PRESENCE_ACCEPT confirmed unset : yes / no
Resume succeeded after authenticating : yes / no
Visible consent steps : ___      (AC-3 requires exactly 1)
OS prompt appeared    : yes / no
Prompt wording as shown :
Biometry or password used : Touch ID / login credential
Cancelled attempt refused, no grant minted : yes / no
Non-interactive invocation did NOT auto-approve (AC-4) : yes / no
Notes / findings :
```

<!-- Add further sign-off blocks below, one per platform and per release. -->

# Manual verification runbook — REQ-575 AC-8

**Status: OUTSTANDING.** This criterion is **not** satisfied by reasoning, by a
test, or by the seam. REQ-575 gates `web/setup_commit` behind the same
BR-10(b) presence check as `model/confirm`/`model/set`; the CI suite proves the
gate refuses (`AlwaysFailsVerifier`), degrades (`UnavailableVerifier`), and — via
the `TETON_PRESENCE_ACCEPT` seam — accepts. What no CI run can prove is that a
**real OS presence prompt** appears in front of a **real human** at the commit,
because the accept seam simulates the human by construction. Leave this section
marked outstanding in
`.adlc/specs/REQ-575-presence-attested-web-setup-commit/requirement.md` (AC-8)
until a person runs the procedure below and records the result.

## What this proves that CI does not

That on a build with a live presence mechanism, `/web setup`'s commit raises the
OS presence prompt, an approval lands the `[web]` table (and the capability is
live in-session with no restart), and a **cancel refuses the commit with nothing
written** — the failure/cancel leg the automated `AlwaysFailsVerifier` test
asserts only in the abstract.

## Procedure

1. `cargo build --features presence` (macOS, Apple M-series). Confirm
   `TETON_TEST_SEAMS` / `TETON_PRESENCE_ACCEPT` are **unset** — the shipped
   mechanism, not the accept seam.
2. Start the daemon and a CLI client; create/attach a session.
3. Run the guided `/web setup` to a valid `fetch_user_url` (or `search` with an
   endpoint) and reach the commit step.
4. **Approve** at the OS prompt (Touch ID / login credential). Expect: the commit
   applies, the `[web]` table is written, and a lookup serves in the **same
   session with no restart**.
5. Repeat to the commit step and **cancel** the OS prompt. Expect: the commit is
   refused with a distinct attestation code, and the config file is
   **byte-identical** to before (nothing written).

## Sign-off

```
REQ-575 AC-8 sign-off
---------------------
Verified by      :
Date             :
Platform / OS    :               (e.g. macOS 26.6, Apple M-series)
Build            :               (cargo build --features presence)
TETON_TEST_SEAMS / TETON_PRESENCE_ACCEPT confirmed unset : yes / no
OS prompt appeared at the commit : yes / no
Prompt wording as shown :
Biometry or password used : Touch ID / login credential
Approved commit applied and served in-session (no restart) : yes / no
Cancelled commit refused, config byte-identical (nothing written) : yes / no
Notes / findings :
```

# Manual verification runbook — REQ-576 AC-6

**Status: OUTSTANDING.** REQ-576 gates `config/set` behind the same BR-10(b)
presence check as `model/confirm`/`model/set`/`web/setup_commit`. CI proves the
gate refuses (`AlwaysFailsVerifier` via the shared commitment harness and the
`TETON_PRESENCE_ACCEPT=fail` e2e), degrades (no-mechanism), and leaves
`config.toml` byte-identical on a refusal. What no CI run can prove is that a
**real OS presence prompt** appears in front of a **real human** when a user
changes machine-wide config — because the accept/fail seams simulate the human.
Leave this outstanding in
`.adlc/specs/REQ-576-presence-attested-config-set/requirement.md` (AC-6) until a
person runs the procedure below.

## What this proves that CI does not

That on a `--features presence` build, `teton provider add` (or a tier/privacy
`config/set`) raises the OS presence prompt; an approval lands the config change
(and the capability is live with no restart); and a **cancel refuses it with
nothing written** — the same-UID quiet-path hole REQ-572 finding 7 named, closed
on a presence build for the config writer with the largest blast radius.

## Procedure

1. `cargo build --features presence` (macOS, Apple M-series). Confirm
   `TETON_TEST_SEAMS` / `TETON_PRESENCE_ACCEPT` are **unset**.
2. Start the daemon; run `teton provider add <id> --model <m> …` (a
   `RegisterProvider`), or a `teton boundary add <glob> --mode <mode>`
   (`SetPrivacyBoundary`).
3. **Approve** at the OS prompt. Expect: the command succeeds, `config.toml`
   gains the entry, and the change is live with no restart.
4. Repeat and **cancel** the OS prompt. Expect: refused with a distinct
   attestation code, and `config.toml` **byte-identical** to before.

## Sign-off

```
REQ-576 AC-6 sign-off
---------------------
Verified by      :
Date             :
Platform / OS    :               (e.g. macOS 26.6, Apple M-series)
Build            :               (cargo build --features presence)
TETON_TEST_SEAMS / TETON_PRESENCE_ACCEPT confirmed unset : yes / no
OS prompt appeared on the config change : yes / no
Variant exercised : provider add / tier binding / privacy boundary
Biometry or password used : Touch ID / login credential
Approved change applied and live (no restart) : yes / no
Cancelled change refused, config byte-identical (nothing written) : yes / no
Notes / findings :
```

# Manual verification runbook — REQ-580 (a prompt during the real load)

**Status: OUTSTANDING.** REQ-580 holds a prompt that arrives while the local
tier is still coming up and runs it the moment the tier opens, instead of
refusing it with "retry in a moment". CI proves the whole arc on the real
daemon binary — but only against the seam loader
(`TETON_FAKE_ENGINE_LOADER` + `TETON_FAKE_ENGINE_LOADER_DELAY_MS`), whose
"load" is a sleep. What no CI run exercises is the shape the report came from:
a **real GGUF** deep-verifying, mapping and benchmarking on a **`--features
llama` build**, with the user typing into that window. Leave this outstanding in
`.adlc/specs/REQ-580-hold-turns-for-a-warming-local-tier/requirement.md` until a
person runs the procedure below.

## What this proves that CI does not

That across a real multi-second load, the held turn's notice appears at once,
the lifecycle's own `benchmark` and `ready` lines land under it as they happen,
and the reply follows with **no retyping** — and that a Ctrl-C during the hold
leaves no ghost turn behind (the next `teton` session's first prompt is not
queued behind an abandoned one on the engine).

## Procedure

1. Build with the real engine: `cargo build --release --features tetond/llama`.
   Confirm `TETON_TEST_SEAMS` is **unset**. Use a machine with a recorded
   decision and verified weights (`teton model status` → `verified`), so the
   daemon's start goes straight to the load.
2. Stop any running daemon (`brew services stop teton` if it is the launchd
   one), then start a fresh session: `teton`. Watch for
   `local tier disabled: <model>'s weights are installed and verified; the
   daemon is loading and benchmarking them now …`.
3. **Immediately** type a prompt (`hi`) and press Enter — inside the load,
   before `benchmark` prints. Expect, in order:
   - `>> message queued until <model> finishes loading — it will run as soon
     as the local tier opens.` (a notice, at once);
   - `>> benchmark <model>: …` and `>> local model <model> ready` as the load
     completes;
   - the model's reply, with no second prompt from you.
   The wait should be exactly the load's remaining time — no longer.
4. Ctrl-D. Restart the daemon (so it loads again), start `teton`, type a
   prompt inside the load window as in step 3, and while the `message queued`
   notice is showing press **Ctrl-C**. Then start `teton` again and, once
   `ready` has printed, type a prompt. Expect: it is served promptly, with no
   sign of the abandoned prompt (no reply to `hi` streams into the new session,
   and the new prompt is not delayed by a turn you did not send).
5. Optional, remote-configured machine: with a remote provider bound as
   `default_provider` and a category whose fallback is that provider, repeat
   step 3. Expect **no** `message queued` — the turn routes to the remote
   provider while the tier loads (REQ-547 D-3), exactly as before.

## Sign-off

```
REQ-580 sign-off
----------------
Verified by      :
Date             :
Platform / OS    :               (e.g. macOS 26.6, Apple M-series)
Build            :               (cargo build --release --features tetond/llama)
TETON_TEST_SEAMS confirmed unset : yes / no
Model / load duration observed :
Step 3 — notice appeared at once, before `benchmark` : yes / no
Step 3 — reply followed `ready` with no retyping : yes / no
Step 4 — no ghost turn after Ctrl-C mid-hold : yes / no
Step 5 (if run) — remote-bound turn was NOT held : yes / no / not run
Notes / findings :
```

# Manual verification runbook — REQ-581 (the connection-test hand-off, and one real `reached`)

**Status: OUTSTANDING.** Two claims in REQ-581 are not CI's to make.

AC-8b says the session points a user at `/provider test <id>` when a turn asks
whether a provider works and answers by *inspecting configuration* instead. CI
pins the deterministic half — `session_ui`'s predicate table drives the exact
turn shape from the report (prompt, `shell: teton provider list`, a reply that
names no command) and asserts one line on a TTY and none on a pipe. What CI
cannot say is whether a **real** local model, asked in a person's own words,
produces a turn of that shape at all: the predicate could be right and never
fire, or fire on turns nobody meant. That is a live measurement, and LESSON-532
is explicit that the guarantee is claimed only after one.

AC-1's `reached` is the other: every automated outcome comes from the e2e
harness's `MockProvider`. Nothing in CI has put a request on the public
internet, so "the probe reaches a real vendor, is billed as one call, and moves
the health map" is unproven until somebody watches it happen once.

Leave both outstanding in
`.adlc/specs/REQ-581-provider-connection-test/requirement.md` until the
procedures below are run. **Both need `/provider test` to exist** — it is
TASK-165's command; run this once TASK-165 has landed, not before.

## What this proves that CI does not

That the phrasings a user actually types reach the predicate; that the harness
line lands where a person can read it, once, in the turn they asked about; and
that a `reached` report against a real vendor carries a latency, token counts
and a cost that are the vendor's rather than a fixture's.

## Procedure — the AC-8b A/B (three phrasings, real local model)

1. Build with the real engine: `cargo build --release --features tetond/llama`.
   Confirm `TETON_TEST_SEAMS` is **unset**. Register a remote provider
   (`kimi` is the one the report used) so the config snapshot has an id for the
   predicate to read, and let the local tier finish loading (`ready`).
2. **Baseline first**, on the shipped binary (`brew install teton`, or the
   previous release tag) — the same three prompts, so the model's own behaviour
   is measured against a build without the line rather than against memory.
3. For each phrasing, a **fresh session** (`teton`, one prompt, Ctrl-D):
   - `alright, I followed your instructions. Can you test the Kimi connection?`
   - `is kimi actually working?`
   - `can you verify my provider connection is reachable?`
   Record, per prompt and per build: what the turn *did* (tool calls shown), and
   whether the reply named `/provider test` **by itself**.
4. On the REQ-581 build, record whether the harness line
   `in this session, /provider test <id> makes one consented call and reports
   what came back; that is the connection test.` printed — and that it printed
   **once**, after the reply.
5. The two failure shapes worth writing down, because they are what would send
   this back to the architecture rather than to the wording:
   - the line fires on a turn that was **not** a connection question (a false
     positive is worse than silence: it talks over a correct answer);
   - the model answers a connection question with no diagnostic at all — no
     recital, no `shell` call — in which case the predicate's second half is
     the wrong reading of the failure, not merely a narrow one.
6. Repeat the first phrasing with stdout piped (`echo '…' | teton`). Expect
   **nothing** from the hand-off, and output byte-identical to the baseline
   build's.

## Procedure — one real `reached` against Kimi

7. With `kimi` registered and a live key in the keychain, in a TTY session:
   `/provider test kimi`. Confirm the preview names the provider, the model and
   the host it will dial, and that answering `n` sends nothing.
8. Answer `y`. Record the reported outcome, latency, input/output tokens and
   cost. Then `teton cost` in the same session: expect the probe to be counted
   (one probe row/`probe_calls` of 1), and `teton provider list` to show `kimi`
   healthy.
9. Confirm what the report does **not** contain: no key value anywhere in the
   line or in the `provider_tested` event — the credential *reference*
   (`keychain://teton/kimi`) is the most that may appear (AC-2).
10. Optional and cheap, if a spare id can be pointed at a bad key: repeat with
    a deliberately wrong credential and confirm the `refused` line names the
    status and the reference, and that no ledger row is written.

## Sign-off

```
REQ-581 sign-off
----------------
Verified by      :
Date             :
Platform / OS    :               (e.g. macOS 26.6, Apple M-series)
Build            :               (cargo build --release --features tetond/llama)
Baseline build   :               (shipped version the A/B compares against)
Local model      :
TETON_TEST_SEAMS confirmed unset : yes / no
A/B — phrasing 1 : harness line printed? yes / no | model named /provider test itself? yes / no
A/B — phrasing 2 : harness line printed? yes / no | model named /provider test itself? yes / no
A/B — phrasing 3 : harness line printed? yes / no | model named /provider test itself? yes / no
A/B — baseline build named /provider test itself (0-3 of 3) :
A/B — line printed at most once per turn, after the reply : yes / no
A/B — false positive on a non-connection turn observed : yes / no  (describe below)
Piped run — nothing printed, output matches baseline : yes / no
Real test — outcome / latency / tokens / cost :
Real test — `teton cost` counted the probe : yes / no
Real test — health after : healthy / other
Real test — no key value in the line or the event : yes / no
Refused variant (if run) — names status and credential reference : yes / no / not run
Notes / findings :
```

# Manual verification runbook — BUG-177 (a second attach is silent in the first session)

**Status: OUTSTANDING.** The fix is pinned in CI at the wire
(`ac_matrix::bug177_a_replayed_lifecycle_reaches_only_the_client_that_attached`
— an ordering-decided absence over three real connections), so this is a
five-minute confirmation on the shipped binary rather than a claim CI cannot
make. It is here because the symptom was found by dogfooding on 0.1.19 and the
person who saw it should see it gone.

## Procedure

1. Start a session with `teton` and wait for `local model … ready`.
2. In a second terminal run `teton doctor` (or, in the session, ask the model
   to run any `teton …` command through its shell tool — e.g. "run
   `teton provider list`").
3. Watch the first session. Expect exactly one new line —
   `a CLI client attached (protocol 2)` — and **no** `>> probe: …` and **no**
   `>> local model … ready` re-announcement. On 0.1.19 both reprinted on every
   attach.
4. If a load was in progress (a fresh daemon on a cold model), also confirm the
   loading indicator was not reset/hidden by the second terminal's attach.

## Sign-off

```
BUG-177 sign-off
----------------
Verified by      :
Date             :
Build            :               (shipped version, `teton --version`)
Second attach printed only `a CLI client attached` : yes / no
Replay lines absent from the first session       : yes / no
Notes / findings :
```

# Manual verification runbook — BUG-178 (a native tool call no longer kills the next request)

**Status: OUTSTANDING.** CI pins the mechanism
(`turn_loop::tests::a_remote_tool_call_with_no_prose_is_recorded_as_the_call_not_a_blank_turn`
— the loop records a remote call as the call, and the prompt the next request
is built from has no empty message; `carry::tests::a_cancelled_remote_*` — a
cancellation drops the call, keeps the prose, leaves no blank turn). What CI
cannot make is the claim that opened this bug: a **real** native-tool
provider accepts the follow-up request Teton now builds and the turn goes on to
finish. The defect was found by dogfooding 0.1.21 against Kimi, and the same
prompt should now complete.

## Prerequisites

- A remote provider at `tool_call_tier = "native"` bound to a tier with **no
  fallback** — the configuration in which the defect was fatal. Kimi
  (`kimi-k3` at `https://api.moonshot.ai/v1/chat/completions`) is the one that
  reproduced it; Anthropic's rule is the same.
- `/provider test <id>` passes (that probe never exercised this path — a
  single-message request has no assistant turn to be empty).

## Procedure

1. Start a session with `teton`, on the shipped build.
2. Ask something that needs a tool and that the model will call **without a
   preamble** — the shape that produced the empty turn: `list the files in
   this directory` (→ `shell` or `glob`), or the original prompt, `In the
   development folder on this machine, I have repos. Find it and look for the
   Teton repo`.
3. Allow the tool. On 0.1.21 the tool line went `[running]` → `[failed]`/`[done]`
   and the next line was `degraded: <id> (invalid response) — no fallback
   configured`, then `error: prompt failed: provider failed and no fallback is
   configured`. Expect instead: the tool result is folded and the model
   **continues** — a second tool call or a final answer — with no `degraded`
   line.
4. Let it run to a final answer. A failing first command (the original
   `ls -d ~/development …` exited 1) is fine and is the more interesting case:
   the model should react to the failure, not the session to the provider.
5. Two exits from the permission prompt, both of which left the empty turn on
   0.1.21:
   - **Deny**: ask for another tool-using step and answer `n`. The model is
     told the user declined and takes another turn — expect that next model
     call to be served (on 0.1.21 it was the one that died).
   - **Cancel**: ask for another tool-using step and press Esc at the prompt.
     Then ask a plain follow-up question. Expect it to be answered — on 0.1.21
     the cancelled turn committed an empty assistant message and every later
     prompt in the session would have died on the same 400.
6. Check the daemon log (`~/Library/Application Support/teton/tetond.log`) for
   `teton: provider `<id>` failed the turn`. Expect **none** for the runs above.
   Optionally provoke one — misconfigure the model name and prompt once — and
   expect a line naming the provider and the status
   (`… failed the turn before it answered: provider returned client error
   status 404`), and nothing from the request or the response body in it.

## Sign-off

```
BUG-178 sign-off
----------------
Verified by      :
Date             :
Build            :               (shipped version, `teton --version`)
Provider / model :               (native tier, no fallback)
Tool-using turn completed after the first call    : yes / no
No `degraded: … (invalid response)` on the turn   : yes / no
Cancelled-at-gate turn, later prompt still served : yes / no
Provoked failure named provider + status, no content : yes / no / not run
Notes / findings :
```

# Manual verification runbook — REQ-582 (the session runs `teton`'s own commands)

**Status: OUTSTANDING.** CI pins the mechanism at every seam it can reach: the
read rows are byte-diffed against their shell twins over one daemon
(`cli_e2e::every_read_row_prints_exactly_what_its_shell_twin_prints`), the write
rows are read back through those twins, a typed `teton provider list` is diffed
against `/provider list` and pinned to cost no turn, and the hand-off nudge has
both a terminal test and a piped negative.

What no CI run can settle is the thing that opened this REQ: a **live model**,
asked a configuration question in a real session, sending the user to a shell.
The harness line is the guarantee (LESSON-532 — presence in context is not
instruction-following), and the guide edit is the improvement; only a real turn
says whether the improvement landed. This runbook is that turn, and it is
deliberately the exact flow from the 2026-08-18 dogfood of 0.1.20.

## Procedure

Run on the **shipped** binary, with `TETON_TEST_SEAMS` unset (a release build
refuses to start when it is set, so a session that opened is already evidence).

1. Start a session with `teton` and wait for `local model … ready`.
2. Type, verbatim, the question that opened the REQ:

   ```
   I want to test the kimi connection
   ```

   Expect **either** the reply itself to name `/provider test <id>` or
   `/provider list`, **or** the session to print one harness line after it:
   `>> in this session: /provider list` (or, for a setup-shaped reply, REQ-579's
   `/provider setup` sentence). Record which of the two happened — a reply that
   names the `/` spelling by itself is the guide edit working; the harness line
   is the guarantee behind it. A reply that sends you to a shell **and** no
   harness line is the finding this REQ exists to prevent.
3. At the same prompt type the shell spelling:

   ```
   teton provider list
   ```

   Expect one notice — `>> teton provider list → /provider list` — followed by
   the provider listing, and **no** model reply of any kind ("that one's for you
   to run…" is the 0.1.20 behaviour and must be gone). Confirm from a second
   terminal that `teton provider list` prints the same lines.
4. Type `/doctor`. Expect the daemon line to end `(this session's connection)`,
   and expect **no** `a CLI client attached` line in this session — a fresh
   attach would be announcing a client into the session being diagnosed.
5. Type `/policy show`, then in a second terminal run `teton policy show`, and
   compare. They must agree line for line.
6. Type a line that is *not* a command — `teton is slow today` — and confirm it
   reaches the model as an ordinary question, unchanged.
7. Type `teton uninstall`. Expect one refusing line naming the shell, and
   confirm the session is still alive and the daemon still running.
8. A write, at the terminal: `/policy set-tier build <a registered provider>`.
   Expect the binding line, then confirm `teton policy show` from a shell agrees.
   Put it back afterwards.
9. The piped half: `echo '/policy set-tier build local' | teton`. Expect one line
   naming `teton policy set-tier` and **no** change to `teton policy show`.

## Sign-off

```
REQ-582 sign-off
----------------
Verified by      :
Date             :
Platform / OS    :
Build            :               (shipped version, `teton --version`)
Local model      :
TETON_TEST_SEAMS confirmed unset : yes / no
Step 2 — reply named the / spelling itself : yes / no
Step 2 — harness hand-off line printed     : yes / no  (which sentence?)
Step 3 — `teton provider list` ran in-session, no model reply : yes / no
Step 3 — same lines as the shell twin      : yes / no
Step 4 — `/doctor` named this session's connection, no attach line : yes / no
Step 5 — `/policy show` == `teton policy show` : yes / no
Step 6 — a non-command `teton…` line still reached the model : yes / no
Step 7 — `teton uninstall` refused, session and daemon alive : yes / no
Step 8 — a typed write applied and the shell twin agreed : yes / no
Step 9 — a piped write refused and changed nothing : yes / no
Notes / findings :
```

# Manual verification runbook — REQ-583 (session root awareness)

**Status: OUTSTANDING.** CI pins the mechanism at every seam it can reach: the
root's kind, display and branch are derived by one pure module and probed once
per turn; the environment block is in every prompt under the resident ceiling
(both sweeps cross a 200-character root); the walkers share one skip set, one
media set and one budget, and end a stopped or partly unreadable walk with the
line that says so; `--cwd`, `/cd`, the refusals and the two events are asserted
through the shipped binaries on a pipe (`cli_e2e`) and at a pty (`pty_e2e`),
and `session/set_cwd` at every permission level over the socket
(`tetond/tests/e2e/session_root.rs`).

What no CI run can settle is the thing that opened this REQ: a **real model**,
started from the home folder, asked for a repository it has to go looking for —
and the **macOS consent dialogs** that a walk into `~/Library`, `~/Music` or
`~/Pictures` raises. Whether a dialog appeared is a fact about the screen, not
about any process's output; it cannot be observed from a script, so it is a
by-hand step by design (LESSON-481's "pay for the harness or record the gap";
AC-20/AC-21). And whether the model's *reply* is a good one is an observation,
never an assertion (LESSON-532): the guarantees are at the surface — the notice,
the trailer lines, the jail — and the prose is recorded beside them.

## Procedure

Run on a build with the local engine (`cargo build --release --features
tetond/llama`) or the shipped binary, with `TETON_TEST_SEAMS` unset. Use a
machine with verified weights (`teton model status` → `verified`). To run it
without disturbing a launchd daemon, use the isolation recipe above (a short
`XDG_RUNTIME_DIR` base, the weights symlinked at `$BASE/teton/models`,
`model-selection.toml` copied beside them) — but the consent-dialog step (2)
needs a **terminal**, so at least that step is typed, not piped.

1. `cd ~ && teton`. Wait for `local model … ready`. Expect, **under the banner
   and before the ready line**, one notice:
   `Not inside a project — the session root is ~ (your home folder); tools are
   scoped to it: every search walks all of it, and privacy boundaries declared
   for a project do not apply here. Run teton from the project, `teton --cwd
   <path>`, or `/cd <path>` here.` The banner's `cwd:` line reads `~`. (BR-5,
   AC-8.)
2. Type, verbatim:

   ```
   look in my development folder for the Teton repo
   ```

   Watch the **screen**, not the transcript, for the whole turn. Expect **no**
   macOS consent dialog for *Media & Apple Music*, *Photos*, or *"data from
   other apps"* — those are what a walk into `~/Music`, `~/Pictures` or
   `~/Library` raises, and from a home-kind root the walkers do not enter them
   unless the pattern names them (BR-12). A dialog for **Desktop** or
   **Documents** may still appear if this terminal was never granted them —
   that is the OS asking about a folder the user *did* name, and is not a
   failure; record it if it happens. Record every tool line the session drew
   (`- glob …`, `- grep …`, `- shell …`, each with its `[done]`/`[failed]`),
   and the reply. (AC-20 b.)
3. Every `glob`/`grep` the model ran either completed or ended with a
   `... (stopped after N entries; narrow the pattern, or move the session root
   with /cd)` (or `stopped after N s`) line, or a `... (N folder(s) could not be
   read (permission denied): …)` line — and the turn **finished**. Nothing hung.
   If a `shell` command timed out, its message ended with the consent-dialog
   sentence (BR-14). Record which trailer lines appeared, verbatim. (BR-10,
   BR-13, AC-20 c.)
4. Type `/cd ~/Documents/GitHub/teton-code`. Expect two lines, in this order:
   `context cleared; N retained blocks dropped.` (or `there was nothing retained
   to drop.` if step 2 retained nothing) and
   `session root is now ~/Documents/GitHub/teton-code (project teton-code,
   branch <the checked-out branch>)` — and **no** notice, because the new root
   is a project. Then ask a trivial question about the repo (`what is in
   Cargo.toml?`) and confirm the reply is about *this* repo — the next turn's
   environment block names the new root. (BR-7, AC-10.)
5. Type `/cd` alone. Expect
   `session root: ~/Documents/GitHub/teton-code (project teton-code, branch …)`
   — the same spelling as step 4's line. Then `/cd ~` and expect the clear
   line, `session root is now ~ (your home folder)`, and the step-1 notice
   again (BR-8, AC-11). `/cd /nope` must print
   `the session root could not be moved: path `/nope` does not exist or is not
   a directory` and a following `/cd` still names `~`. From a shell, `teton
   --cwd /nope` must print `teton: could not start a session: path `/nope` does
   not exist or is not a directory` and exit 1 **without** autostarting a
   daemon or printing a banner (the CLI fails fast; the daemon's validator
   answers the same sentence for a path that passes the CLI but not the
   daemon).

Record the model's replies in steps 2 and 4 as **observations** — what it
searched, what it found, what it said — not as pass/fail (LESSON-532).

## Sign-off

```
REQ-583 sign-off
----------------
Verified by      :
Date             :
Platform / OS    :
Build            :               (`teton --version`, or the release build's commit)
Local model      :
TETON_TEST_SEAMS confirmed unset : yes / no
Step 1 — notice under the banner, before the ready line, cwd `~` : yes / no
Step 2 — NO Media & Apple Music / Photos / other-apps dialog     : yes / no
Step 2 — a Desktop/Documents dialog appeared (not a failure)     : yes / no / n/a
Step 2 — tool lines drawn (verbatim)                             :
Step 3 — every walk completed or ended with a `... (stopped …)` /
         `... (… could not be read …)` line; nothing hung          : yes / no
Step 3 — trailer lines seen (verbatim)                           :
Step 4 — `context cleared;` then `session root is now … (project teton-code, branch …)` : yes / no
Step 4 — no notice after the move to a project                   : yes / no
Step 5 — `/cd` alone printed the same root line                  : yes / no
Step 5 — `/cd ~` re-fired the notice; `/cd /nope` refused naming the path : yes / no
Model prose (steps 2 and 4), as observations :
Notes / findings :
```

### Run record — 2026-08-18, from a script (TASK-180)

The procedure was driven once against the merged tip (commit of TASK-180's
parent, `6bf9cf9` + this task's tree), **from a script, not by a person**:
`cargo build --workspace --release --features tetond/llama` (51 s, engine
cached), an isolated daemon (`XDG_RUNTIME_DIR=/tmp/t583`, weights symlinked at
`/tmp/t583/teton/models`, `model-selection.toml` copied beside them,
`--shutdown-policy never`), the real `qwen3-coder-30b-a3b` weights on Apple
Silicon (48 GiB), `TETON_TEST_SEAMS` unset. Steps 1–3 were run **piped** (so
step 1's notice could not appear there — a pipe never carries it, by design)
and step 1/4/5 again under `script -q /dev/null` for a pty. Everything below is
an **observation** (LESSON-532); the sign-off block stays blank because the one
thing this runbook exists for — whether a **consent dialog** appeared on the
screen — cannot be observed from a script and was not observed. Left
OUTSTANDING for a person at the keyboard.

Run 1 — `cd ~ && printf 'look in my development folder for the Teton repo\n/quit\n' | teton -v`
(48 s wall clock including the model load; the model's own words are its own):

```
>> local tier disabled: qwen3-coder-30b-a3b's weights are installed and verified; the daemon is loading and benchmarking them now — the local tier opens when that completes.
session sess-… ready (freeform). Type a prompt or /help for commands; Ctrl-D to end.
› >> message queued until qwen3-coder-30b-a3b finishes loading — it will run as soon as the local tier opens.
>> benchmark qwen3-coder-30b-a3b: first token 640 ms, 61.9 tok/s
>> local model qwen3-coder-30b-a3b ready
>> route [edit/build] → local (model tbd) — …
I'll look for the Teton repo in your development folder.
 - glob dev/**/teton* [running]
 - glob dev/**/teton* [done]
Let me try a broader search for any Teton-related repositories.
 - glob **/teton* [running]
 - glob **/teton* [done]
Let me check what's in your home directory to see if we can find the Teton repo there.
 - glob ~/*teton* [running]
 - glob ~/*teton* [done]
Let me check what directories exist in your home folder to get a better sense of your file structure.
 - shell: ls -la ~ | grep -E '^[d].*teton' [running]
? permission requested: shell — shell: ls -la ~ | grep -E '^[d].*teton'
  allow shell? [y]es / [n]o / [a]llow-always / [d]eny-always: …
 - shell: ls -la ~ | grep -E '^[d].*teton' [failed]
I apologize, but I'm unable to locate the Teton repository in your development folder. The search attempts didn't return any matches for "teton" in your home directory or development folders. …
turn ended (EndTurn).
```

Observations: three `glob` walks from a `home`-kind root, each `[done]` within
seconds — the whole turn, three walks and a refused shell, ended in well under a
minute, so nothing hung (AC-20 c). The shell call was refused only because the
piped stdin had no answer left for the permission question. This home holds
more than 100,000 entries under the walk's skip rules (`find` with the same
prunes reaches the cap in 0.2 s), so `glob **/teton*` from `~` can only have
ended by a budget — the entry budget, or the 10 s wall clock — and therefore
with a `... (stopped after …; narrow the pattern, or move the session root with
/cd)` line in the model's tool result (BR-10). That is an **inference** from
the counts and from CI's BR-10 tests, not a reading of this transcript: the
transcript shows tool status only, not the tool's text, and the model's prose
did not relay the line. Whether the walk **entered** `~/Library`,
`~/Music` or `~/Pictures` cannot be read from this transcript either; the
walkers' pruning of them from a home root is CI's (`tools::walk` /
`glob`/`grep` AC-16 tests), and the dialog non-appearance is the by-hand step.
The model did not find the repository (it lives at `~/Documents/GitHub/teton-code`;
its first pattern named a `dev/` folder this home does not have, its third used
a literal `~` segment) — recorded, not judged.

Run 2 — `teton -v --cwd ~/Documents/GitHub/teton-code` piped `/cd`, a
question, `/cd ~`, `/cd`, `/cd /nope`, `/cd`, `/quit` (1 s; the model was
already loaded):

```
› session root: ~/Documents/GitHub/teton-code (project teton-code, branch main)
 - read Cargo.toml [running]
 - read Cargo.toml [done]
The package name is not explicitly stated in the provided Cargo.toml file. It appears to be a workspace with multiple crates, but the root package name is not defined. …
turn ended (EndTurn).
› >> context cleared; 4 retained blocks dropped.
>> session root is now ~ (your home folder)
› session root: ~ (your home folder)
› error: the session root could not be moved: path `/nope` does not exist or is not a directory
› session root: ~ (your home folder)
```

(Transcripts re-spelled to the wording shipped after the verify pass — `path`
for `cwd` in the refusal, and the notice's `the session root is … ; tools are
scoped to it` form; the runs' behaviour is unchanged.)

Observations: `/cd` alone spelled the root with its kind and branch (step 5);
the model's `read Cargo.toml` was root-relative and succeeded — the
environment block named the root it read from (step 4's "the reply is about
this repo"; the workspace `Cargo.toml` really has no `[package]`); `/cd ~` drew
the clear line then the root line, in that order, and no notice on a pipe;
`/cd /nope` was refused naming the path and a following `/cd` still said `~`.

Run 3 — the same binary under `script -q /dev/null` (a pty), `teton --cwd ~`
then `/cd`, `/cd ~/Documents/GitHub/teton-code`, `/cd ~`, `/quit`:

```
  Teton Code v0.1.22 — local-first AI coding agent
  cwd: ~
>> probe: 48.0 GiB RAM — clears the local-tier floor
>> local model qwen3-coder-30b-a3b ready
>> Not inside a project — the session root is ~ (your home folder); tools are scoped to it: every search walks all of it, and privacy boundaries declared for a project do not apply here. Run teton from the project, `teton --cwd <path>`, or `/cd <path>` here.
session sess-… ready (freeform). Type a prompt or /help for commands; Ctrl-D to end.
…
session root: ~ (your home folder)
… /cd /Users/brettluelling/Documents/GitHub/teton-code
>> context cleared; there was nothing retained to drop.
>> session root is now ~/Documents/GitHub/teton-code (project teton-code, branch main)
… /cd ~
>> context cleared; there was nothing retained to drop.
>> session root is now ~ (your home folder)
>> Not inside a project — the session root is ~ (your home folder); tools are scoped to it: every search walks all of it, and privacy boundaries declared for a project do not apply here. Run teton from the project, `teton --cwd <path>`, or `/cd <path>` here.
```

Observations: at a terminal the notice sits under the banner (`cwd: ~`) and
before the ready line (step 1); a move to the project drew no notice; `/cd ~`
re-fired it after the root line (step 5, BR-8).

Cleanup: my daemon was stopped, `/tmp/t583/teton/models` was unlinked before
`/tmp/t583` was removed, and the real weights at `~/Library/Application
Support/teton/models` were listed intact afterwards.

**Still outstanding for a person:** step 2 at a real terminal, watching the
screen for the *Media & Apple Music* / *Photos* / *"data from other apps"*
dialogs across the turn (and noting a Desktop/Documents one if it appears).

# Manual verification runbook — REQ-586 (a declared window, and what a turn is assembled to fit)

**Status: OUTSTANDING.** CI pins the mechanism everywhere it can reach it: the
derivation is a pure table-tested function (`harness::budget`), the router is
its one caller, the corpus fixture pins both guards against a real tokenizer,
the wire fields and the event are contract-tested, the `window:` column and the
two doctor advisories run through the shipped binaries (`cli_e2e`), and the
reroute refit, the carry drop, the redact bound and the pressure emissions are
driven end to end in the daemon's integration suite.

What no CI run settles is the claim a user actually cares about: that a real
frontier provider, given a real window, really does take a prompt that used to
be silently cut — and that the cost row agrees. The mock providers accept
whatever they are sent, so "the whole prompt arrived" is only observable
against a vendor that would have refused it (LESSON-481: pay for the harness or
record the gap). The model's *reply* is an observation, never an assertion
(LESSON-532); what is asserted is the surfaces.

## Prerequisites

A build with the local engine (`cargo build --release --features
tetond/llama`) or the shipped binary, `TETON_TEST_SEAMS` unset, verified
weights (`teton model status` → `verified`), and a real Moonshot key. Use the
isolation recipe above (a short `XDG_RUNTIME_DIR` base, weights symlinked at
`$BASE/teton/models`) to keep a launchd daemon out of it. A 6,000-word prompt
is ~34 KB of prose — well past the old 4,096-word budget and past the 32,768-B
one, so before this REQ it was guaranteed to be cut.

## Procedure

1. [ ] **Declare the window through the new surface, not by hand.** Either
   `/provider setup kimi build` in a session (it records the recipe's window —
   1,000,000 — when you take `kimi-k3`, the recipe's example model), or
   `teton provider add kimi --kind openai-compatible --endpoint
   https://api.moonshot.ai/v1/chat/completions --model kimi-k3 --max-context
   128000`. AC-14 names 128,000; the recipe now ships 1,000,000, and either is
   a valid record of this step — write down which you used. Confirm
   `config.toml` gained `max_context` under `[providers.kimi.capabilities]`
   without you opening the file.
2. [ ] **`teton doctor` shows the window.** The `kimi` row ends `window: 128k`
   (or `1m`), and there is **no** advisory line for it. A provider you have
   *not* given a window still reads `window: unknown — context budget
   defaulted (set capabilities.max_context)` and draws the advisory — check
   one, so the column is not vacuously green. `teton provider list` shows the
   same column.
3. [ ] **A 6,000-word prompt reaches Kimi whole.** Route `build` to `kimi`
   (`teton policy set-tier build kimi`), start a session with `/verbose`, and
   paste ~6,000 words (any long document; prose is the class the byte guard
   binds on). Expect on the route line
   `· budget 84,650 words / 254 KB (bound: window)` for a 128k window, or
   `· budget 665,984 words / 2 MB (bound: window)` for 1m — and **no**
   `context:` pressure line at all. Record both the route line and `/cost`'s
   input-token count for the turn; the input tokens should be in the same
   ballpark as the prompt's own token count (≈7,500 for 6,000 words of prose),
   not clipped near 4,096 words.
4. [ ] **The same prompt under `redact = true` names the redact bound.** Set
   `[privacy] redact = true`, restart the daemon, and paste the same prompt.
   Expect `(bound: redact scan)` on the route line — the byte figure drops to
   `89 KB` while the word figure stays window-derived — and expect the turn to
   **complete**, never a block for an unscannable body.
5. [ ] **Chunk-count note.** Record how long step 4's turn took before the
   model's first token. A body near the bound is scanned in up to five chunks
   (`REDACT_MAX_CHUNKS` — four chunk-widths plus the overlap), each one a local
   model call, so this is where a redacted big-window turn spends its latency.
   There is no per-chunk surface by design; the number to write down is the
   wall time and whether anything was flagged.
6. [ ] **Record the worst case per prompt at this budget.** The budget bounds
   one model **call**. A prompt runs up to 25 tool iterations
   (`NATIVE_MAX_ITERATIONS`) and each re-sends the context, so a 1,000,000-token
   window — 665,984 words / 2 MB per call — is up to **≈25 million input
   tokens for a single prompt**; a 128,000-token window is 84,650 words / 254 KB
   per call and ≈3.2 million per prompt. Price both against your provider's
   rate and write the two numbers down. `context_budget_cap` is the knob that
   lowers the ceiling; there is no spend cap, and this runbook is where that is
   said out loud (BR-9).
7. [ ] **Once REQ-585 lands: `/proceed REQ-xxx` expands rather than being
   refused for size.** The seven ADLC skills that fit on no tier before this
   REQ — `/spec`, `/manifest`, `/analyze`, `/template-drift`, `/wrapup`,
   `/sprint`, `/proceed` — should expand on a Kimi-routed `build` tier with no
   pressure line. Until REQ-585 ships there is nothing to type here; leave the
   box open rather than closing it on the smaller claim.

## Recorded resident-prompt headroom after REQ-585

**Superseded — see "Re-measured at REQ-587" below for the current figures.**
This section is the record of what REQ-587 inherited and of how the
measurement is taken; the table in it is REQ-585's tip, not today's.

The number REQ-587 should read before it writes a resident sentence. Measured
on this branch's tip with the two margin tests
(`egress::redact::tests::the_total_cap_clears_the_harness_context_budget_with_margin`
and `harness::tools::web::tests::the_web_tool_docs_clear_the_outbound_body_overhead`),
against BUG-181's 10,240-byte body overhead and the unmoved 48-byte floor:

| shape | worst prompt | spent | margin |
|---|---|---|---|
| opted-out (no web tool) — the tighter | 6,138 | 9,414 | **826** |
| opted-in (web tool docs + schema) | 6,091 | 9,367 | **873** |

REQ-585 spent **68** of what it inherited: 52 on BR-9's amended capability
sentence, and 16 on the `skills` topic's name, which `teton_docs` renders
twice — once in its description and once in the schema's `One of: …`. Verified
two-sided on the tip rather than by arithmetic: 778 bytes of filler leaves the
tighter shape at exactly the 48-byte floor and passes, 779 fails.

**How to read these off the tests, because neither prints when green.** Both
assertions report `worst`, the escaping term and the overhead only in their
failure message, so the figures above were taken by appending a known number
of filler bytes to `crates/tetond/src/harness/self_config.md` (the guide is
embedded verbatim, so a byte added there is a byte added to every shape),
running the two tests, reading the `N-byte system prompt` each one names, and
subtracting the filler. A 1,000-byte pad trips both; the pad is then removed.
This is the two-minute measurement LESSON-543 says to run *before* writing a
resident sentence, not after.

Before this task the same two were 6,070 / 9,346 / **894** and 6,023 / 9,299 /
**941**. REQ-585 spent **52 bytes**, and that is the whole of its resident
cost: BR-9's amendment to the guide's capability sentence, which grew from 186
to 238 bytes when "loads nothing from `.claude/` or `~/.claude`" became "loads
skills and commands from `.claude/` and `~/.claude` but nothing else there".
The skill roster is deliberately **not** in the guide (OQ-2) — a resident line
that grew with the user's `~/.claude` tree is the one shape these two tests
cannot bound — and the registry, the `/help` section, the invocation preamble
and the refusal messages are surfaces and turn-scoped text, which the resident
prompt does not pay for. Neither the overhead nor the floor moved.

**That quotation is REQ-585's wording, not the shipped one.** REQ-612 amended
the same sentence again — it now reads "loads skills and commands from
`.claude/` and `~/.claude`, and the repository notes from `TETON.md` (or
`AGENTS.md`) at the session root, but nothing else there (no CLAUDE.md, agents
or hooks)" — and paid for it by moving `REDACT_BODY_OVERHEAD_BYTES` 14 → 23 KiB
rather than out of this margin; the measured figures for that move live in the
ledger on that constant in `crates/tetond/src/egress/redact.rs`, which is the
one home of them.

**A 26-byte correction to REQ-586's recorded figures.** That REQ recorded
6,096 / 9,372 / **868** and 6,049 / 9,325 / **915**; re-measured here, its own
tip is 6,070 and 6,023. `self_config.md`, `turn_loop.rs` and `harness/tools/`
are byte-identical between REQ-586's merge commit (`c9e9265`) and this
branch's tip before this task (`57e733f`), and
the gap between the two shapes (47 bytes) is unchanged, so both recorded
numbers were 26 high rather than anything having shrunk since. The prose
figures in `egress/redact.rs` and `harness/tools/web.rs` still carry the old
pair; treat this table as the measured one. REQ-586's account of *what it
spent* stands: 18 bytes, nine for `context` in the `teton_docs` topic index
and nine for the same word in the tool's schema, with the 3.4 KB `context`
topic, the `window:` column, the doctor advisories, the budget clause and the
pressure lines all tool results and CLI surfaces the prompt does not pay for
(ADR-11).

**And the tool description is not at its ceiling** — a claim this section used
to make. REQ-586's own verify pass bought the room where the review said to buy
it: the sentence in front of the index lost `setup and troubleshooting`, leaving
`DESCRIPTION` at **94** of `MAX_DESCRIPTION_CHARS` = 120. TASK-209 then spent
**8** of those 26 on `skills`, exactly as that note said the next topic should,
and moved no ceiling: the description is **102, with 18 left**. Its resident
cost is 8 bytes twice — the description and the tool schema's `One of: …`, both
rendered into the prompt verbatim (`ToolRegistry::docs`) — so **16 bytes** come
off the table above, by arithmetic rather than a second measurement: 6,138 /
9,414 / **826** and 6,091 / 9,367 / **873** as of TASK-209. The 4.0 KB `skills`
topic itself, the `/help` section, the consent block, the echo line and the
refusal message are tool results and CLI surfaces the prompt does not pay for.

## Re-measured at REQ-587 — and the overhead moved

REQ-587 spent **1,092** bytes against the 826 above, so the assumption moved
rather than the margin absorbing it: `REDACT_BODY_OVERHEAD_BYTES` is **11 KiB**
(10 → 11, the fourth such raise), the 48-byte floor is untouched, and both
sweeps now register a `skill` tool at BR-2's worst-case roster.

| shape | worst prompt | spent | margin |
|---|---|---|---|
| opted-out (no web tool) — the tighter | 7,230 | 10,506 | **758** |
| opted-in (web tool docs + schema) | 7,183 | 10,459 | **805** |

Where the 1,092 went, measured rather than reasoned:

| line item | bytes |
|---|---|
| the `skill` tool's docs entry (description + schema) at a roster of `ROSTER_MAX_BYTES` = 512 | **1,010** |
| BR-8's third amendment to the guide's capability sentence (238 → 320 on the file) | **82** |

Verified two-sided on both shapes with the pad method below: **710** bytes of
filler leaves the tighter shape at exactly the 48-byte floor and passes, 711
fails; **757** and 758 are the same pair for the looser one. So **710 usable
bytes** is the figure the next resident sentence reads.

**Two things about this raise that the previous three did not carry.** The
first is that a resident line now **grows with the user's tree** — the roster
is the names of every model-invocable skill — which is why it is bounded by a
byte cap and why both sweeps measure it *at* that cap rather than at whatever
the developer's `~/.claude` holds. The second is that since REQ-586 the
overhead has a production reader, so raising it narrowed every
`[privacy] redact = true` route's byte budget:
`REDACT_SCANNABLE_CONTEXT_BYTES` dropped **89,127 → 88,196**, a 931-byte cut to
the budget BR-7's `bound: redact scan` refusal measures against. The chunk
count is unchanged (2 × (32,768 + 11,264) = 88,064 ≤ 108,280; 88,064 / 27,070 =
3.25 → 4). Both claims are asserted by
`egress::redact::tests::the_overhead_raise_restates_the_chunk_count_and_the_scannable_bound`,
because every other assertion about that bound is an inequality that holds
either way — the cut is silent unless it is stated.

REQ-584 contends for the same constant. It moves once; whichever REQ lands
second re-measures against the moved value rather than moving it again.

## Sign-off

```
Date / build     :
Provider + model :
Window declared  :            (how: /provider setup | provider add --max-context)
Step 1 — config gained max_context without a hand edit : yes / no
Step 2 — doctor row window, and an unknown row advised : yes / no
Step 3 — route line budget + bound                     :
Step 3 — /cost input tokens for the turn               :
Step 3 — any `context:` pressure line                  : yes / no
Step 4 — bound: redact scan, turn completed            : yes / no
Step 5 — wall time to first token under redact         :
Step 6 — worst case per prompt, priced                 :
Step 7 — REQ-585 skills expand                         : yes / no / not yet shipped
Notes / findings :
```

# Manual verification runbook — REQ-585 (the user's own `/` commands)

**Status: OUTSTANDING — nothing below has been executed.** This is a runbook
written for a person with the ADLC toolkit on their machine; it is not a record
of a run, and no box in it may be ticked from CI.

## What this proves that CI does not

CI reaches the mechanism at every seam it can: discovery is a pure function over
a recording lister (so what was opened is asserted, not assumed), the expander
is pure, and `/help`'s section, the consent matrix, the pipe rule, the two-stage
budget refusal and the boundary pin are each driven through the shipped
surfaces by this REQ's verification suites (TASK-208) — all of it against
**fixture** roots, which is the point of the recording lister. One figure was taken by hand against the **real** tree on the
dogfood machine while this REQ was implemented, and it is the precondition for
everything below rather than a CI assertion: discovery registers **17 skills
(user 17, project 0); 0 skipped**, with the largest — `proceed` at 49.8 KiB —
inside the 64 KiB body cap. Step 2 is where a person re-confirms it.

What no CI run settles is what a person actually gets. The mock providers accept
whatever they are sent, so "the whole of `/proceed` arrived" is only observable
against a vendor that would have refused it; and a model's *reply* is an
observation, never an assertion (LESSON-532). Legs (a)–(c) observe a real model
against a real corpus — including **where it stops**, which is this REQ's most
important negative result. Legs (d)–(f) assert surfaces.

## Prerequisites

- The shipped binary or a `--release` build, with `TETON_TEST_SEAMS` unset.
- The ADLC toolkit installed, `~/.claude/skills` being the symlink into
  `~/Documents/GitHub/adlc-toolkit`. A symlinked **root** is followed on
  purpose; a symlinked *entry* under one is refused and named.
- A Kimi provider with a declared window — **write down which**: the shipped
  recipe's `max_context = 1000000` (what `/provider setup kimi` records when you
  take `kimi-k3`), or a hand-lowered `teton provider add … --max-context
  128000`. Both clear the ≈31.3k tokens a `/proceed` expansion needs; AC-20
  names either.
- The tier you will type on routed there: `teton policy set-tier build kimi`.
- macOS: the first read of a root that resolves under `~/Documents` can raise a
  consent dialog. Answer it. If you decline, the skills section will say
  `unreadable (permission denied)` — which is the correct behaviour, and worth
  noting rather than retrying blindly.

## Procedure

### (a) `/status` at `guarded`: one consent, every command shown, one report

1. [ ] Start `teton` in the teton-code repo and turn on `/verbose`.
2. [ ] `/help`. Expect a skills section — header
   `skills — arguments are passed through as typed:`, one row per skill with
   its source, and the diagnostic `17 skills (user 17, project 0); 0 skipped` —
   sitting **above** the argument footer, with the escape footer still the last
   line of all.
3. [ ] Type `/status`. Expect, in this order: one echo line
   `/status → skill status (user, <n.n> KiB, <N> dynamic commands)`; one consent
   block, ``skill `status` (user) wants to run <N> dynamic-context commands:``
   followed by one indented line per command, **verbatim**; your single answer;
   then the model's status report, produced with `read`/`glob`/`shell`.
4. [ ] Confirm the consent was asked **once** for the whole invocation, not once
   per command, and that the commands listed are the ones that ran.
5. [ ] Record the echo line verbatim. Where the two numbers differ it says so
   (`<N> dynamic commands, none run`, `…, 1 run`) — a bare count beside
   placeholders in the prompt would be the bug BR-12 exists to prevent.
6. [ ] `/verbose` for the turn shows the path **relative, never absolute** —
   for this *user* skill, `~/.claude/skills/status/SKILL.md` (never an absolute
   path carrying your username) — plus the ignored frontmatter keys and each
   command's typed outcome. A **project** skill is spelled from the session
   root instead (`.claude/skills/<name>/SKILL.md`), which is what BUG-187
   fixed; its sharp case — a checkout outside `$HOME` — is covered by
   `skills_discovery.rs` and `skills_list_contracts.rs` and needs no step
   here.

### (b) `/validate REQ-585`: the argument reaches the body

7. [ ] Type `/validate REQ-585`. Expect the expansion to carry `REQ-585` where
   the body writes `$ARGUMENTS`, and the model to read and validate this REQ's
   own `requirement.md`. Record what it produced — the point is that the
   argument arrived as typed, not that the verdict is right.
8. [ ] `/cost` shows one ordinary prompt turn for it, with input tokens in the
   ballpark of the expansion's size. Not zero, and not two turns.

### (c) The fidelity leg — and the evidence the Deferred follow-ups are written on

9. [ ] `/analyze teton code repo` on the Kimi route. Expect it to expand with no
   pressure line, and the model to perform a **read-based** audit: one model, no
   subagents. The body asks for four parallel auditors; record what happened
   instead. That degradation is BR-13's documented behaviour, not a failure.
10. [ ] `/proceed REQ-585`. The `proceed` skill has no `$ARGUMENTS`, so the
    argument arrives as the closing `ARGUMENTS: REQ-585` line — that fallback is
    what makes this invocation work at all; confirm it in `/verbose`.
11. [ ] **Record the exact step at which it stalls.** Quote (i) the line of the
    skill body the model reached and (ii) the model's own words on reaching it.
    The expected shape is the first *"invoke the actual `/validate` skill"* gate:
    Teton has no model-invocable skill surface, and the model cannot even `read`
    `~/.claude/skills/validate/SKILL.md` from a repo-rooted session, because it
    is outside the tool jail. It will narrate the step, ask you to run it, or do
    that phase's work itself without invoking anything. Any of the three is the
    finding; paste the quote into the sign-off. This is the evidence the two
    Deferred follow-ups — **model-invoked skills** and **subagent dispatch** —
    are written against, and the reason they are separate REQs.

### (d) On the local tier, a big skill is refused, and the bound is spoken

12. [ ] `teton policy set-tier build local` (or `/policy set-tier build local`
    at a terminal), then type `/analyze teton code repo` again.
13. [ ] Expect a refusal naming the skill, the measured size, the budget and
    `(bound: local engine)` — the **spoken** form. `local_engine` is the wire
    spelling and must not appear on a surface.
14. [ ] In the same breath, three negatives: **no consent was asked** (the body
    alone is measured before consent — nobody approves four commands and is then
    told the turn was refused); **no `context:` pressure line and no elision
    notice** (a refused turn never joined the conversation); and `/cost` shows
    no provider call for it.
15. [ ] Put the tier back.

### (e) Unattended: `full` runs it, `guarded` refuses without reading a line

16. [ ] Two real recipes; use one and say which:
    `printf '/permissions full\n/status\n' | teton`, or `[permissions]
    default_level = "full"` in a throwaway config (`TETON_CONFIG=…`) with
    `printf '/status\n' | teton`. (`--permissions` is **not** a flag — the
    globals are `--yes` and `--verbose`; AC-20 said otherwise and was corrected
    along with the refusal line it described.)
    Expect the dynamic context to run with no prompt, and the report produced.
17. [ ] The same at the default `guarded`: `printf '/status\n' | teton`. Expect
    one refusal line — ``skill `status`'s dynamic context was refused without
    asking: this session's input is not a terminal, so nobody could be asked``
    — a placeholder in the prompt for every command, an echo line reading
    `<N> dynamic commands, none run`, and **the turn still completing**. The
    line's remedy names `/permissions full` and `[permissions] default_level`,
    both of which exist — check that it does not say `--permissions`, which
    does not, and which would send an unattended runner to a parse error at the
    one moment nobody can be asked anything.
18. [ ] Confirm no stdin line was eaten: `printf '/status\ny\n' | teton` must
    treat the `y` as an ordinary prompt line, never as an answer to a question
    that was never asked.

### (f) With a privacy boundary configured: pinned, which for seven means refused

19. [ ] On a machine that has a `local-only` boundary (`teton boundary list`
    shows one), repeat (a) and (b) with the tier still routed to Kimi.
20. [ ] Expect both to run on the **local** tier. The reason is BR-7 and it is
    worth writing down as it actually is: dynamic-context output carries
    `Unknown` provenance, exactly as `shell` output does, and the egress
    inspector fails closed on unknown provenance whenever any boundary is
    configured. A **user** skill file outside the session root pins under the
    same unknown rule (it has no root-relative identity to match a glob
    against); a **project** skill pins exactly as reading that file would.
21. [ ] **Pinned is not run.** All seventeen ADLC skills run the ethos include,
    so on such a machine all seventeen are pinned to the local tier — and seven
    of them (`/spec`, `/manifest`, `/analyze`, `/template-drift`, `/wrapup`,
    `/sprint`, `/proceed`) exceed the local budget and are therefore **refused
    there**, not served. The pin is what forces the refusal. Verify with one of
    the seven and record the refusal beside the pin. (BR-7's parenthetical
    originally said those seventeen "run" on the local tier; TASK-196 amended
    it. The amended sentence is the true one.)
22. [ ] **OQ-8's residual, in v1:** the consent offers no "run without dynamic
    context" option, so there is no way to keep an invocation remote once a
    command has run. The two remedies are to let the model run the command
    itself with `shell` (which pins identically) or to `/cd` out of the boundary
    — record which, if either, you reached for.
23. [ ] **OQ-7's residual, in v1:** a project skill gets no separate trust
    acknowledgment. The permission gate is the whole trust boundary: at the
    default `guarded` every dynamic command of every invocation is shown and
    asked about **every time**, and the body is prompt text the model reads
    under the same level a typed prompt would. If you answer "for this session"
    on a project skill, note that the grant dies when `/cd` moves the root.

## Sign-off

```
REQ-585 sign-off
----------------
Verified by      :
Date             :
Platform / OS    :
Build            :               (`teton --version`)
TETON_TEST_SEAMS confirmed unset : yes / no
Provider + model :
Window used      : 1,000,000 (shipped recipe) | 128,000 (hand-lowered)   <-- circle one
/help — skills section, diagnostic line read : ______________________________
(a) echo line, verbatim                      :
(a) one consent for the whole invocation     : yes / no
(a) report produced                          : yes / no
(a) /verbose path was relative (user: ~/…)   : yes / no
(b) $ARGUMENTS carried REQ-585               : yes / no
(b) /cost showed one ordinary prompt turn    : yes / no
(c) /analyze audited read-only, no subagents : yes / no  (what it did instead:)
(c) /proceed expanded (ARGUMENTS: fallback)  : yes / no
(c) STALL STEP — body line quoted            :
(c) STALL STEP — model's words               :
(d) refusal named skill/size/budget          : yes / no
(d) bound printed                            : `bound: local engine` / other:
(d) no consent asked, no pressure line, no provider call : yes / no
(e) recipe used for `full`                   : /permissions line | config default_level
(e) full: dynamic context ran unattended     : yes / no
(e) guarded on a pipe: refused, placeholders, turn completed : yes / no
(e) remedy names `/permissions full` + `[permissions] default_level`, not `--permissions` : yes / no
(e) no stdin line was consumed               : yes / no
(f) boundary configured on this machine      : yes / no
(f) (a) and (b) ran on the local tier        : yes / no
(f) one of the seven was refused there       : which, and the message:
(f) OQ-8 remedy reached for, if any          : shell | /cd | none
(f) OQ-7 noted (no project-skill trust step) : yes / no
Notes / findings :
```

---

# Manual verification runbook — REQ-587 (the model runs a skill)

**Status: OUTSTANDING — nothing below has been executed.** This is a runbook
written for a person with the ADLC toolkit on their machine; it is not a record
of a run, and no box in it may be ticked from CI or by an agent.

## What this proves that CI does not

CI reaches every seam it can, and TASK-222 closed the ones that needed a real
model-issued call: a scripted vendor now drives a thirteen-call chain, a
committed expansion through a live reroute, the per-call cost rows, and the four
egress legs. What no mock settles is the thing this REQ exists for — whether a
**real** model, handed a real `/proceed`, actually calls `skill`, follows the
body it gets back, and reaches the next gate. A mock provider replies with
whatever it was scripted to reply with; a model's behaviour is an *observation*,
never an assertion (LESSON-532).

Two negative results are the point rather than a side effect. Leg (a) records
**where the run stops** — the first "dispatch an agent" step — which is the
evidence the subagent spec needs. Leg (e) records what the local tier does with
a large orchestrator, which is what a machine with no remote provider gets.

## Prerequisites — and one of them is not optional

- The shipped binary or a `--release` build, with `TETON_TEST_SEAMS` unset.
- The ADLC toolkit installed, `~/.claude/skills` being the symlink into
  `~/Documents/GitHub/adlc-toolkit`.
- A Kimi provider with a declared window — **write down which**: the shipped
  recipe's `max_context = 1000000` (what `/provider setup kimi` records for
  `kimi-k3`), or a hand-lowered `teton provider add … --max-context 128000`.
  Both clear what a `/proceed` expansion needs; AC-15 names either, and the
  sign-off block asks which was used because the two are not interchangeable
  evidence for leg (a)'s cumulative case.
- The tier you will type on routed there: `teton policy set-tier build kimi`.
- **No privacy boundary configured.** This is a precondition, not a
  convenience, and it is stated because the failure it prevents looks like a
  feature that does not work. Every ADLC skill lives under `~/.claude`; BR-10
  makes a user skill's block **unpinnable** (`ProvenanceId::from_resolved`
  refuses a file outside the session root, by design); and an unpinnable block
  pins the turn under *any* boundary, related to the file or not. So on a
  machine with `[[privacy.boundaries]]` set, every leg below routes to the local
  tier and the large ones are refused there — and you would be recording "the
  model cannot run `/proceed`" when what you measured was the boundary rule
  working exactly as `provenance_egress.rs`'s
  `a_model_invoked_user_skill_pins_the_turn_wherever_any_boundary_exists`
  says it does. **Check with `/boundary list` before starting** — not
  `/doctor`, which issues one `config/get` and renders **providers only**; it
  never prints a boundary, so a boundary-configured machine reads a clean
  doctor report as "none configured" and then measures the boundary rule for
  six legs. `/boundary list` on a clean machine prints exactly
  ``no privacy boundaries configured. Add one with `teton boundary add`.``;
  anything else is a listing headed `privacy boundaries:` with one
  `<glob> [<mode>]` line per boundary. The shell twin is `teton boundary list`
  and prints the same lines. **A machine that has one configured runs
  leg (f) instead**, and records that it did.
- macOS: the first read of a root under `~/Documents` can raise a consent
  dialog. Answer it; a decline renders `unreadable (permission denied)`, which
  is correct behaviour and worth noting rather than retrying blindly.

> **A note on AC-15's letters.** The AC used to refer to a "leg (g)" for the
> boundary-configured machine while defining only (a)–(e). It has been
> renumbered: AC-15 now defines **(f)** and names it as the leg run *instead
> of* (a)–(e), which is leg (f) below. The two documents agree; do not add a
> second lettering.

## Procedure

### (a) `/proceed REQ-587` — how far one prompt gets, and where it stops

1. [ ] Start `teton` in the teton-code repo, `/verbose` on, and confirm the
       route: `/policy show` names `kimi` for `build`.
2. [ ] `/help`. Record the skills diagnostic line verbatim (it was
       `17 skills (user 17, project 0); 0 skipped` when REQ-585 shipped).
3. [ ] Type `/proceed REQ-587`. Expect the echo line
       `/proceed → skill proceed (user, <n.n> KiB, <N> dynamic commands)` —
       **record `<N>`, do not predict it.** Every ADLC body carries 1–6
       `` !`…` `` commands (Assumptions, the 2026-08-19 corpus), so `<N>` is
       non-zero and `/proceed`'s own value is one of the things this leg
       produces. At the default `guarded` that non-zero count also means the
       **first thing you see is a dynamic-context consent** under the skill's
       own key `skill:user:proceed`, listing each command as it will run
       (substitution happens first). That prompt is the feature, not a defect:
       answer it, and record which way. Declining is a valid run — it leaves
       one ``[dynamic context not run: `cmd` — reason]`` placeholder per
       command and the echo line then reads `<N> dynamic commands, none run`.
       Then the body lands as one prompt turn.
4. [ ] Watch for the **first model-issued call**: a status line
       `- skill validate [running]` and then an echo line
       `skill validate (user, <n.n> KiB, <N> dynamic commands) — invoked by the
       model`. Record it verbatim. *This line is the whole REQ.*
5. [ ] Record how many `continue` prompts the run needed. One prompt's 25
       iterations do not span the pipeline (OQ-8); the count is the number this
       runbook exists to produce.
6. [ ] Record **the exact step at which it next stalls**, quoting the line of
       the skill body it stopped on and the model's own words. It should be the
       first "dispatch an agent" step (`/proceed` Phase 4). This is the
       subagent spec's evidence — write the quote down, not a paraphrase.
7. [ ] Record `/verbose`'s per-invocation lines for one of the calls: the
       home-relative path, any declared flags, each dynamic outcome, and the
       `invocation N of 12 this turn` count.

### (b) A scratch skill hidden from the model

1. [ ] Copy any small skill to `~/.claude/skills/scratch-hidden/SKILL.md` and
       add `disable-model-invocation: true` to its frontmatter.
2. [ ] In a fresh session, ask the model to list what it can run. Expect
       `scratch-hidden` **absent** from the roster and from `skill {}`'s listing.
3. [ ] Ask it to run `scratch-hidden` by name. Expect a typed refusal naming the
       flag, no expansion, and no dynamic command run. Record the words.
4. [ ] Type `/scratch-hidden` yourself. It must still dispatch: the flag hides
       the row from the model and costs the user nothing.
5. [ ] Delete the scratch copy.

### (c) The two capability questions (BR-8)

1. [ ] Ask *"can you run `/validate`?"*. Expect: yes, through the `skill` tool.
2. [ ] Ask *"can you run `/help`?"*. Expect: no — the user runs the built-in
       commands, and it names one for you to type.
3. [ ] Record both answers verbatim. A model that denies the skills it *does*
       have, or claims the built-ins it does not, is BUG-181's defect with its
       sign flipped and is a finding either way.

### (d) Unattended, on a pipe

1. [ ] `printf '/permissions full\n/proceed REQ-587\n' | teton`. Expect the
       ethos include to run and the first gate to be reached with **no prompt**.
       There is no `--permissions` flag; the level is set with REQ-560's
       `/permissions` session command. (REQ-585 AC-20(e) carries the same
       nonexistent flag and needs the same correction when it is run.)
2. [ ] The same piped run at the default `guarded`. Expect placeholders where
       the dynamic context would have been, the refusal naming
       `/permissions full` and `[permissions] default_level` as the remedies,
       **no stdin line consumed**, and the run still completing.
3. [ ] The teton-code repo has no project skills, so the acknowledgment path
       needs a throwaway root: `mkdir -p /tmp/scratch-root/.claude/skills/scratch`
       with a one-line `SKILL.md`, then run a piped session there at `guarded`
       and ask the model to use it. Expect the client to refuse **without
       reading stdin** and the model to be told the user must acknowledge or run
       at `full`.
4. [ ] Note the BUG-185 residual if you see it: at `full` a cloned repository's
       dynamic context runs unattended, up to 12 invocations per turn, each
       command with a 30-second deadline inside a non-cancellable
       `spawn_blocking`. Nothing here reproduces it; record only what you
       observe.

### (e) On the local tier

1. [ ] Route `build` to the local tier (`teton policy set-tier build local`).
2. [ ] Ask the model for `skill { name: "proceed" }`. Expect a typed refusal
       naming the skill, its size, the budget, and **`bound: local engine`** —
       the spoken form, never `local_engine`.
3. [ ] Ask it for `skill { name: "status" }`. Expect that one to expand.
4. [ ] Record both messages verbatim; the pair is what shows the refusal is the
       budget rather than the tier.

### (f) The boundary-configured machine (AC-15's leg (f))

Run **this leg instead of (a)–(e)** if `/boundary list` reports a configured
privacy boundary and you are not willing to remove it. (`/doctor` cannot answer
this question — see Prerequisites.)

1. [ ] Confirm the boundary with `/boundary list` and record its glob and mode,
       verbatim from the `<glob> [<mode>]` line.
2. [ ] Ask the model to run any `~/.claude` skill. Expect the turn to route to
       the **local tier** and nothing to reach the provider — a user skill's
       block is unpinnable and pins under any boundary.
3. [ ] Ask it for a large one (`proceed`). Expect the local-tier refusal from
       leg (e), for the same reason.
4. [ ] Record that this machine ran (f) rather than (a)–(e), so a reader does
       not read a boundary rule as a broken feature.

## Sign-off

```
REQ-587 AC-15 verified by : ______________________  date: ____________
Machine          :               (chip / RAM / OS)
Build            :               (`teton --version`)
TETON_TEST_SEAMS confirmed unset : yes / no
Window used      : 1,000,000 (shipped recipe) | 128,000 (hand-lowered)   <-- circle one
Privacy boundary configured?     : no (ran a–e) | yes (ran f)   <-- circle one
  ^ answered with `/boundary list` (NOT `/doctor`)  : yes / no
  ^ line it printed, verbatim :
/help — skills diagnostic line read          : ______________________________
(a) `/proceed` echo line, verbatim           :
(a) dynamic commands `/proceed` carried      : ____  (recorded, not predicted)
(a) dynamic-context consent raised at guarded : yes / no   answered: accept | decline
(a) first model-issued echo line, verbatim   :
(a) `continue` prompts needed                : ____
(a) STALL STEP — body line quoted            :
(a) STALL STEP — model's words               :
(a) /verbose showed path, flags, outcomes, count : yes / no
(b) hidden skill absent from the roster      : yes / no
(b) model's call refused, flag named         : yes / no  (words:)
(b) no dynamic command ran                   : yes / no
(b) `/scratch-hidden` still dispatched for the user : yes / no
(c) "can you run /validate?" — answer        :
(c) "can you run /help?" — answer            :
(d) full on a pipe: reached the gate with no prompt : yes / no
(d) guarded on a pipe: placeholders, run completed  : yes / no
(d) no stdin line was consumed               : yes / no
(d) remedy named `/permissions full` + `[permissions] default_level` : yes / no
(d) scratch root at guarded: refused without reading stdin : yes / no
(e) `proceed` refused, `bound: local engine` : yes / no  (message:)
(e) `status` expanded on the local tier      : yes / no
(f) boundary glob, if this leg was run       :
(f) turn routed local, nothing reached the provider : yes / no
Notes / findings :
```

---

# Manual verification runbook — REQ-584 (the project locator)

## What this proves that CI does not

CI proves the parts: the registry records and prunes, the scan stays inside its
budget and its skip sets, the tool is exposed at every level, the environment
line stays inside REQ-583's ceiling, `/cd <name>` reads a directory first and a
project second. What no mock settles is the question the 2026-08-18 incident
actually asked — whether a **real** model, sitting in `~` and asked "look in my
development folder for the Teton repo", now *answers* instead of walking the
disk and apologising.

That is an **observation, not an assertion** (LESSON-532). The model's prose is
recorded, never required. What *is* required is at the surface: the environment
line carried the names, and the hand-off line appeared if the tool was called.
Both are the harness's, not the model's — which is the whole of BR-7 and BR-11.

## Prerequisites

- The shipped binary or a `--release` build, with `TETON_TEST_SEAMS` unset.
- A local tier that actually loads (this is the tier the REQ is *for* — a weak
  model is the one that reaches for a disk walk).
- `~/Documents/GitHub/teton-code` present. On this machine it is, and it is why
  `Documents/GitHub` leads the dev-folder table.
- **macOS note, expected and not a defect:** the first scan may raise
  *"Terminal would like to access files in your Documents folder"*. That dialog
  is raised by a question the user asked (`/projects`, or a `projects` call),
  never by a launch — BR-3 forbids a scan at launch, and AC-4 pins it. Answer it
  once. If you decline, the scan reports what it could reach and the registry
  still fills from use (BR-1).

## Procedure

### Leg (a) — the warm registry, which is the ordinary case

1. `cd ~/Documents/GitHub/teton-code && teton`, then `/quit`. That one launch is
   BR-1: the root is recorded, `source: launched`.
2. `cd ~ && teton`.
3. **Before typing anything**, read the launch notice. It should now name up to
   five known projects with `/cd <name>` (BR-10).
4. Ask, verbatim: `look in my development folder for the Teton repo`
5. Record what happens, then `/quit`.

### Leg (b) — the cold registry, which is the first-run case

1. Stop the daemon, delete `projects.json` from the state dir
   (`$XDG_RUNTIME_DIR/teton` or `~/Library/Application Support/teton`), restart.
2. `cd ~ && teton`, ask the same question.
3. This is the leg the dev-folder scan exists for: with nothing remembered, the
   scan should still find `~/Documents/GitHub/teton-code`.

### Leg (c) — `/cd` by name

1. From `~`, type `/cd teton-code`. The session should move, with REQ-583's
   `context cleared` and `session root is now …` lines.
2. From a directory that has a `src/` subdirectory, type `/cd src`. It must move
   to `./src` **even if** a known project is also named `src` — REQ-583's
   reading wins, and this is the leg that proves the ordering by hand.

## Sign-off

```
Date / build                                  :
(a) launch notice named known projects        : yes / no   (names:)
(a) environment line carried `Known projects:` : yes / no
     — check with /verbose, or read the turn's prompt
(a) `→ /cd teton-code` hand-off line appeared : yes / no
(a) the turn ran a glob/grep over ~           : yes / no
     — if yes, did it END BY BUDGET as REQ-583 guarantees? yes / no
(a) model's prose (recorded, not required)    :
     — did it name ~/Documents/GitHub/teton-code?  yes / no
     — did it name `/cd teton-code`?               yes / no
(b) cold registry: scan found the repo        : yes / no
(b) macOS Documents dialog raised             : yes / no   answered: allow | deny
(b) if denied — the answer said where it looked : yes / no
(c) `/cd teton-code` moved the session        : yes / no
(c) `/cd src` chose ./src over the known project : yes / no
(c) an ambiguous name listed candidates and moved nowhere : yes / no  (n/a if none)
Notes / findings :
```

---

# Manual verification runbook — REQ-591 AC-11 (project-skill trust, unattended)

**Status: OUTSTANDING.** REQ-591 puts an acknowledgment in front of every
project-authored skill body — the typed `/name` path as well as the model's
`skill` tool — and then gives an unattended session one way past it: a
`[skills] trusted_project_roots` row a human wrote earlier. CI proves both
halves against fixtures: a scripted route double for the gate, and a whole
`run_prompt_turn` against a temporary directory tree. What no CI run exercises
is the shape the feature is *for*: a real `teton` on a real pipe, on a machine
whose `config.toml` a person edited by hand, against a repository whose
canonical path the daemon has to agree with. Leave this outstanding in
`.adlc/specs/REQ-591-project-skill-trust-and-unattended-allowlist/requirement.md`
until a person runs the procedure below.

## What this proves that CI does not

Three things, and the third is the one nobody can fixture:

- **The row a human copies is the row that matches.** Every automated leg mints
  both sides of the comparison from the same function. By hand, the user reads
  a row out of a refusal message, pastes it into a file, restarts, and the
  daemon has to agree — through `$TETON_CONFIG` resolution, TOML quoting, and
  whatever `/tmp` → `/private/tmp` firmlinking macOS applies on the way.
- **The unattended refusal is legible on a pipe**, arriving as a message a
  script's author can act on rather than as a silent empty turn.
- **The gap between the prompt and the file.** The acknowledgment shows
  `~/dev/repo`; the row that matches is absolute. D-5 turns the natural mistake
  into a refusal to start, and only a person retyping the row by hand finds out
  whether that refusal is legible or merely correct.
- **The two behaviour reversals, seen rather than asserted.** A listed row does
  not open the model's door (D-2), and `plan` now expands a typed project skill
  with its commands unrun (D-3). Both are places the product does something
  *different* from what it did last release, and a green suite says only that
  the new answer is the one the code was written to give.

(OQ-4 — whether a daemon's `HOME` changes what a row means — is closed
structurally rather than by this runbook: D-4 mints the row absolutely, so there
is no `~` left for a `HOME` to resolve.)

## Prerequisites

- A `--release` build, `TETON_TEST_SEAMS` **unset**.
- Two repositories with a project skill each, so "listed" and "unlisted" are two
  trees rather than one tree in two states:
  ```sh
  for r in ~/dev/teton-trusted ~/dev/teton-unlisted; do
    mkdir -p "$r/.claude/skills/dogfood"
    printf -- '---\ndescription: dogfood\n---\n\nSay OK and stop.\n' \
      > "$r/.claude/skills/dogfood/SKILL.md"
    git -C "$r" init -q 2>/dev/null || true
  done
  ```
- A config file you can edit, and knowledge of which one the daemon reads:
  `$TETON_CONFIG`, else `<base>/config.toml`, where `<base>` is
  `$XDG_RUNTIME_DIR/teton` or, on macOS,
  `~/Library/Application Support/teton`.
- Start from **no** recorded decision: confirm `[skills] trusted_project_roots`
  is absent from that file before leg (a).

## Procedure

### Leg (a) — an unlisted root refuses, and the refusal names the row

1. With `trusted_project_roots` absent, run the invocation with **no terminal**:
   ```sh
   printf '/dogfood\n' | teton --cwd ~/dev/teton-unlisted
   ```
   Stdin is a pipe, so there is no terminal to ask. Note the `y` test: put a
   `y` on the line *after* `/dogfood` and confirm it is never consumed as
   consent — it should arrive as your next prompt line, or not at all.
2. Expect a refusal naming the repository, saying `there was no client to ask`,
   and ending with the remedy — *acknowledge it once at a terminal and answer
   `p`, or add `<row>` to `[skills] trusted_project_roots` in your config.*
3. **Record the `<row>` verbatim.** This is the assertion no fixture can make:
   the string is the canonical name of the tree, and on macOS it is frequently
   *not* the path you typed.
4. Confirm nothing ran: no model output, and `[skills] trusted_project_roots`
   is still absent from the config file. An unattended run consults the list; it
   never writes one.

### Leg (b) — the same root, listed, proceeds with no prompt

1. Add the row **exactly as leg (a) printed it**:
   ```toml
   [skills]
   trusted_project_roots = ["<row from leg (a)>"]
   ```
2. Restart the daemon (the list is read at session create).
3. Re-run the piped invocation from leg (a) step 1. Expect the skill to expand
   and the model to answer, with **no prompt drawn** and nothing written back to
   the config.
4. Now the negative pair, on the same config: run the piped invocation in
   `~/dev/teton-unlisted`'s *sibling*:
   ```sh
   printf '/dogfood\n' | teton --cwd ~/dev/teton-trusted
   ```
   Whichever of the two is not listed must still refuse. A list that admits both
   is a list matched by prefix or by `$HOME` alone, and either is a defect.
5. And the nesting case, which is BR-6 by hand:
   ```sh
   mkdir -p ~/dev/teton-trusted/vendor/other/.claude/skills/dogfood
   cp ~/dev/teton-trusted/.claude/skills/dogfood/SKILL.md \
      ~/dev/teton-trusted/vendor/other/.claude/skills/dogfood/SKILL.md
   printf '/dogfood\n' | teton --cwd ~/dev/teton-trusted/vendor/other
   ```
   With the *parent* listed, this must **refuse**. A tree a dependency update
   dropped inside a trusted root does not inherit its trust.

### Leg (c) — the attended path, and the durable answer

1. At a real terminal, in a repository that is **not** listed:
   `cd ~/dev/teton-unlisted && teton`, then type `/dogfood`.
2. Expect the acknowledgment prompt to name **you** as the asker ("you asked to
   run this repository's skills as instructions"), never the model, and to offer
   a fifth option whose label spells the concrete write:
   `Trust this repository permanently (writes `[skills] trusted_project_roots +=
   "<row>"` …)`.
3. Answer `p`. Then read the config file with your own eyes and confirm the row
   that landed is byte-for-byte the row the label named. This is BR-7 by hand.
4. Decline the same prompt in a fresh session (answer reject) and confirm the
   turn is refused with the trust sentence — and that nothing was appended.

### Leg (d) — the row a user would *naturally* write is refused at load (D-5)

This leg replaces the one that gathered data for OQ-4. D-4 closed OQ-4
structurally: a durable row is minted absolutely and carries no `~`, so a
daemon's `HOME` can no longer change what a row means. What is left is the
failure D-4 created and D-5 caught — and it is the one a real user hits first,
because the acknowledgment prompt shows them `~/dev/teton-trusted` while the row
that matches is `/Users/<you>/dev/teton-trusted`.

1. With leg (b) working, stop the daemon and edit the row to the home-relative
   spelling by hand:
   ```toml
   [skills]
   trusted_project_roots = ["~/dev/teton-trusted"]
   ```
2. Start the daemon. It must **refuse to start**, and the message must name the
   correct form and say Teton writes the row for you when you answer `p`.
   A daemon that starts and silently never matches is the exact failure D-5
   exists to prevent — the allowlist appears to contain your repository and does
   not.
3. Repeat with `/Users/<you>/dev/teton-trusted/` (trailing slash) and with a
   relative `dev/teton-trusted`. Both must be refused for the same reason.
4. Restore leg (b)'s row and confirm the daemon starts again.

### Leg (e) — a listed row does **not** open the model's door (D-2)

The behaviour reversal a person most needs to see, because the automated legs
all drive the typed door and this is the door that is now *narrower* than the
row implies.

1. With leg (b)'s row in place and working — so the typed door at that root is
   demonstrably proceeding unattended — run a piped session at the same root and
   ask the **model** to use the skill rather than typing it:
   ```sh
   printf 'Use the dogfood skill.\n' | teton --cwd ~/dev/teton-trusted
   ```
2. Expect a refusal. The row answers for the typed path and for nothing else, so
   there must be **no** expansion of the repository's body here even though the
   very same root ran it in leg (b).
3. Read the refusal. It must not tell the user to add a row — there is no row
   that would help, and a sentence proposing one is BUG-181's shape.

### Leg (f) — `plan` expands a typed project skill, unrun (D-3)

Before D-3 the acknowledgment fell to `plan`'s deny default, so `plan` refused a
typed project skill outright: no body, no prompt, nothing. That was the most
restrictive outcome at the level a user picks *because* they want to read a
repository.

1. At a real terminal in `~/dev/teton-unlisted`, run `teton`, then
   `/permissions plan`.
2. Type `/dogfood`. Expect the acknowledgment prompt to be **put** — `plan` asks,
   it does not refuse. Answer `y` (allow once).
3. Expect the body to expand and the turn to run.
4. Now the half `plan` still denies. Add a dynamic-context slot to the skill:
   ```sh
   printf -- '---\ndescription: dogfood\n---\n\nHost is !`hostname`. Say OK.\n' \
     > ~/dev/teton-unlisted/.claude/skills/dogfood/SKILL.md
   ```
   Restart, repeat steps 1–3, and confirm the body still arrives while the
   command is **not run** — a placeholder, never a hostname. `plan`'s promise is
   that nothing changes and nothing leaves; D-3 relaxed the refusal, not that.

### Leg (g) — a typed answer does not open the model's door (D-7)

Leg (e) is the *durable* half of D-2. This is the session half, and it is the
one a user can trip over in a single sitting.

1. At a real terminal in `~/dev/teton-unlisted`, run `teton` and type
   `/dogfood`. Answer `a` (allow for this session).
2. In the **same session**, ask the model to use the skill:
   `use the dogfood skill`.
3. Expect a **second** acknowledgment prompt, naming the model as the asker.
   Before D-7 there was none: one answer about a skill you typed also let a
   file on disk reach the model for the rest of the session.
4. Answer it, then ask again. The second ask must draw no prompt — the model's
   own answer is remembered under its own key.

### Leg (h) — the prompt names the repository the way the rest of the prompt does (D-9)

1. At a terminal in `~/dev/teton-unlisted`, type `/dogfood` and read the
   acknowledgment.
2. No line of it may contain `/Users/` (or your home path). The repository is
   named `~/dev/teton-unlisted`.
3. Answer `p`, then read the config file. The row that landed is the **absolute**
   path — the label said "by its full path", and this is that.
4. Now the pair: in a fresh session at a root that is *not* listed, run the
   piped invocation from leg (a) and read the refusal. That sentence **does**
   carry the absolute row, deliberately — it is what you are being told to
   paste, and after D-5 a `~/…` row stops the daemon starting.

## Sign-off

```
REQ-591 AC-11 sign-off
----------------------
Verified by      :
Date             :
Platform / OS    :
Build            :               (cargo build --release)
TETON_TEST_SEAMS confirmed unset : yes / no
Config file the daemon read      :
(a) piped run at an UNLISTED root refused          : yes / no
(a) refusal named `there was no client to ask`     : yes / no
(a) refusal printed a copy-pasteable row           : yes / no
     — row, verbatim :
     — did it differ from the path you typed?  yes / no  (how:)
(a) nothing was written to the config              : yes / no
(b) piped run at the LISTED root proceeded         : yes / no
(b) no prompt was drawn                            : yes / no
(b) the sibling, unlisted, still refused           : yes / no
(b) the NESTED tree under the listed root refused  : yes / no
(c) the prompt said YOU asked, not the model       : yes / no
(c) the `p` label named the exact row              : yes / no
(c) the row in the file matched the label byte for byte : yes / no
(c) a declined prompt wrote nothing                : yes / no
(d) a `~/…` row made the daemon refuse to start    : yes / no   <-- must be "yes"
(d) the message named the canonical form + the `p` remedy : yes / no
(d) trailing-slash and relative rows also refused  : yes / no
(d) leg (b)'s row restored and the daemon started  : yes / no
(e) the MODEL's door at the LISTED root refused    : yes / no   <-- must be "yes"
(e) the refusal proposed no config row             : yes / no   <-- must be "yes"
(f) `plan` PUT the acknowledgment (did not refuse) : yes / no
(f) the body expanded at `plan` after `y`          : yes / no
(f) the `!`cmd`` slot was NOT run at `plan`        : yes / no   <-- must be "yes"
(g) the model's door asked AGAIN after a typed `a`  : yes / no   <-- must be "yes"
(g) answering it, then re-asking, drew no prompt    : yes / no
(h) no line of the acknowledgment contained `/Users/` : yes / no <-- must be "yes"
(h) the row written to config was the ABSOLUTE path : yes / no
(h) the unattended refusal DID quote the absolute row : yes / no
Notes / findings :
```

---

# Manual verification runbook — REQ-590 AC-14 (the engine-derived local budget)

**Status: OUTSTANDING.** REQ-590 stops the local tier short-circuiting to a fixed
`(4,096 words, 32,768 B)`. Its pair now derives from the window the daemon
actually loads with: `LOCAL_ENGINE_N_CTX 32,768` less a 1,024-token generation
reservation, giving **21,162 words / 63,488 bytes**. (When this runbook was
written the window was 16,384 and only the **word** half derived — 10,240 words
— with the byte half held at the `LOCAL_BUDGET_BYTES` constant, 32,768 B: D-4
had derived it too (30,720) and ADR-9 reversed that because the derived value
was the *smaller* one there. The window was raised after a `/analyze` turn's
55 KB of dynamic context was refused against that fixed half, and at 32,768 the
derived byte half is 63,488, so both halves derive again.) Leave AC-14 unticked in
`.adlc/specs/REQ-590-engine-derived-local-context-budget/requirement.md` until a
person has run the procedure below and filled in the sign-off.

> **REQ-589's AC-15 runbook was never written**, which is why REQ-590 arrived
> with no field data of its own and had to take its measurements from scratch.
> This leg exists so that does not happen twice. It is not satisfied by
> intending to run it.

## What this proves that CI does not

CI proves the arithmetic and every surface that quotes it: the derivation from
both sides (AC-1), the bound and its remedy (AC-2/AC-6/AC-16), the compaction
ceiling relation (AC-8), the token-dense corpus (AC-9), the compaction cadence
(AC-11, `crates/tetond/tests/compaction_cadence.rs`), the 4,097-word turn that
used to be refused (AC-12). All of it runs against `ScriptedEngine` and
`MockEngine`.

What none of it establishes is the one claim the whole REQ rests on: that a
prompt assembled **to** the new budget is a prompt the **real** engine accepts
and answers. The default build has no engine — `--features llama` is not built in
CI — so nothing in the suite has ever handed a full-budget prompt to llama.cpp.
The zero word-slack ADR-6 accepts (`21,162 × 3/2 = 31,743 = 32,768 − 1,024 − 1`;
the 1 is integer truncation) is precisely the kind of thing that is fine in
arithmetic and discovers a one-token-off error in the field.

The byte half gives a second reason to run this by hand: at the 2 B/token floor
it claims exactly the usable window, 31,744 tokens, with **no** margin for
content that tokenizes under two bytes a token. (On the 16,384-token window it
was the 32,768 B constant and claimed 1,024 *more* than the engine had — an
overclaim ADR-9 accepted and the raise to 32,768 closed.) A **byte-saturated**
local prompt is therefore still the shape most likely to meet the engine's own
refusal. Leg (c) is where that would show up.

| Claim | Proven by CI? | Why not |
|---|---|---|
| The pair is `(21,162, 63,488)` | **yes** | `derive` is feature-free |
| The rendered bound names the window and reservation | **yes** | asserted on the string |
| A full-budget prompt fits the loaded `n_ctx` | **no** | no engine in CI |
| A full-budget local turn actually answers | **no** | no engine, no weights |
| What a full-budget turn *costs* in wall clock | **no** | see AC-10, taken by hand |

## Prerequisites

- A build **with the engine**: `cargo build --release --workspace --features
  tetond/llama`. A default build has no local tier at all, and every session
  goes remote-only — `teton doctor` says so in as many words. The flag is
  non-default because it compiles llama.cpp, which needs `cmake`.
- Weights present and verified, so nothing re-downloads 18 GiB. On this machine
  they are at `~/Library/Application Support/teton/models/`.
- The local tier actually loading — this REQ is about the local tier's budget,
  so a session that silently went remote proves nothing. Confirm with
  `teton doctor` **before** you start, not after.
- `/verbose` on, which is what surfaces the `route …` line the budget rides.

## Procedure

### Leg (a) — the reported budget is the engine's window, arithmetic included

1. `teton`, then `/verbose`.
2. Ask anything short that stays local (`say hello`).
3. Read the `route [ask/local] → …` line. It must carry a budget clause reading
   **`budget 21,162 words / 63 KB`** with **`(bound: local engine)`**. (The
   client's route line renders the plain three-word bound; the *daemon's* own
   sentences — a refusal or an over-budget offer — carry AC-16's fuller
   account, which leg (b') below reads.)
4. **Check the arithmetic against the source, not against this page**:
   `32,768 − 1,024 = 31,744` usable tokens; `31,744 × 2 ÷ 3 = 21,162` words;
   `31,744 × 2 = 63,488` bytes, rendered `63 KB`. Both halves reconcile
   against the window (AC-16, BR-12).
5. The bound must **not** offer `set capabilities.max_context` — there is no
   provider declaration behind a local route, and sending a user to edit one is
   BR-6's whole prohibition.

### Leg (b) — the turn that used to be refused now serves (AC-12, the field report)

This is the exact `/analyze` case that motivated REQ-589: **4,097 whitespace
words against a 4,096-word budget**, on a machine whose engine had 16,384 tokens
loaded.

1. Make a file of a little over 4,096 whitespace words —
   `python3 -c "print(('word '*4200).strip())" > /tmp/req590-4200.txt` is enough.
2. In a local session, paste it or ask the model to read it and summarise.
3. It must **serve**. Not a refusal, and **no over-budget offer** — REQ-589's
   offer is for content that genuinely does not fit, and this now does.

### Leg (c) — a turn at the *full* budget, which is the claim CI cannot make

1. Build a prompt close to the new ceiling — around 10,000 whitespace words, or
   around 32,000 bytes, whichever your content hits first. Real content, not
   `word word word`: a couple of large source files is the honest sample.
2. Send it as one turn.
3. It must be answered. What must **not** happen is
   `context_length_exceeded` / a `ContextWindowExceeded` backend error — that is
   the engine refusing a prompt the harness said would fit, and it is the exact
   failure the zero word-slack (ADR-6, BR-10) makes possible.
4. **Time it.** A full-budget local turn takes materially longer to reach its
   first token than a small one; the recorded figures are in `architecture.md`
   § Measurements. If the turn appears to hang, wait at least 30 seconds before
   deciding it has — on the reference machine prefill alone is ~14 seconds, and
   a slower machine is the case A-4 flags.

### Leg (d) — the deliberately dense turn, where the slack is zero

The word half has **no** margin: 1.5 real BPE tokens per whitespace word is
assumed exactly, and `budget.rs` measures Rust at 1.69. So content denser than
the ratio overruns at full budget, and the byte guard does not cover
*token-dense but byte-light* content.

1. Build ~10,000 whitespace words of something token-dense and byte-light —
   single-character tokens separated by spaces, or a numeric column
   (`python3 -c "print(' '.join(str(i%10) for i in range(10000)))"`).
2. Send it as one turn.
3. **Either outcome is a pass for this leg, and which one it is, is the finding.**
   If it answers, the ratio held. If it comes back as a
   `context_length_exceeded` — the engine's own typed refusal — that is D-3's
   intended catch working, and it should read as an ordinary local outcome in
   words you can act on, **not** as a crash, a hang, or an opaque backend
   string. Record which, verbatim.

   **Do not expect an over-budget offer behind it.** REQ-589's offer fires only
   when a *skill expansion* does not fit; this turn is inside both guards, so no
   offer is reachable and the typed refusal is the only catch there is. That is
   the point of the leg: it measures the one place the harness hands the
   decision to the engine.

### Leg (e) — the reported measurement's own density, and the bound's account of itself

*This leg replaces "the byte band D-4 gave up" (BR-7, AC-7). **There is no
band**: D-4 was reversed, the byte half is unchanged at 32,768, and no turn that
served before this REQ is newly refused. What is worth checking by hand instead
is the case the reversal was made for, at its real density, plus the sentence
AC-16 added.*

1. Make a payload at the reported density — **~4,100 whitespace words and
   ~31,000 bytes**, i.e. about 7.5 bytes per word, which is what code is. A
   couple of real source files is the honest sample; `head -c 31000` of a
   concatenation is fine.
2. Send it in a local session. It must **serve**, with no over-budget offer.
   This is the size that was refused before REQ-590 (on words, by one). The
   report's word count is exact at 4,097; its byte count is only known to
   [30,500, 31,499] — the daemon rendered `31 KB` and `bytes_figure` rounds to
   the nearest KB — so **any** payload you build in that range is faithful to
   it, and under D-4 most of that range would have been refused again, on bytes.
3. Now make one that genuinely does not fit — the same content grown past 63 KB
   (and under discovery's 128 KiB per-file ceiling, or it is skipped rather
   than measured) — and send it **as a skill** (`/something`), which is the
   only path REQ-589's offer covers. Expect the over-budget question, **not** a
   silent elision.
4. Read the offer's sentence. Its bound clause must account for the budget:
   `bound: local engine — both halves come from the engine's 32,768-token
   window, less the 1,024 reserved for the reply` (AC-16). Record it verbatim
   — if the numbers in it do not produce the 21,162 words / 63 KB beside it,
   that is the defect this leg exists to catch.

## Sign-off

```
Date / build / commit                              :
Machine (chip, RAM, OS)                            :
Model loaded (name, quant, n_ctx)                  :
`teton doctor` confirmed a loaded local engine     : yes / no  <-- must be "yes"

(a) route line read `budget 21,162 words / 63 KB`   : yes / no  (verbatim:)
(a) bound read `(bound: local engine)`              : yes / no
(a) the word half reconciled to the window          : yes / no
(a) bound did NOT offer capabilities.max_context    : yes / no  <-- must be "yes"
(b) the 4,097-word turn SERVED                      : yes / no  <-- must be "yes"
(b) no over-budget offer was raised                 : yes / no  <-- must be "yes"
(c) a ~full-budget turn was answered                 : yes / no
(c) no ContextWindowExceeded / backend error         : yes / no
(c) wall clock to first token                        :        s
(c) wall clock to the finished answer                :        s
(d) token-dense, byte-light turn — outcome           : answered | context_length_exceeded | other
(d) if refused, the refusal was typed and readable   : yes / no
(d) if refused, NO over-budget offer appeared        : yes / no  <-- must be "yes"
(d) verbatim message                                 :
(e) ~4,100 w / ~31 KB turn SERVED                    : yes / no  <-- must be "yes"
(e) no over-budget offer was raised for it           : yes / no  <-- must be "yes"
(e) an over-63 KB skill DID raise the offer          : yes / no
(e) it was NOT silently elided                       : yes / no  <-- must be "yes"
(e) the bound clause, verbatim                       :
(e) its numbers reconciled to the 21,162 beside them : yes / no
Anything that hung, aborted, or crashed              :
Notes / findings                                     :
```

# Manual verification runbook — REQ-592 AC-13 (a real audit at 100 and at 200 columns)

**Status: PARTIALLY EXERCISED, still OUTSTANDING.** This is a runbook written
for a person at a terminal; it is not a record of a run, and no box in it may be
ticked from CI or by an agent. Leave AC-13 unticked in
`.adlc/specs/REQ-592-markdown-aware-terminal-rendering/requirement.md` until
somebody has run the procedure below and filled in the sign-off.

REQ-592 has **two halves in two crates**, and this is the only place they are
judged together. TASK-282 changed **what the model writes** — an output-format
clause in the system prompt telling it its reply lands in a narrow terminal.
TASK-277..281 changed **how what it writes is drawn** — word wrap, table layout,
inline styling, and the terminal gate. Neither is a bug in the other's layer, and
fixing either alone leaves a visibly bad screen, which is why the REQ did both.

## What this proves that CI does not

Every automated acceptance criterion in this REQ **scripts its replies
precisely** so that it does *not* depend on model behaviour. That is the correct
way to test a renderer — and it is exactly why the pair of halves has never once
been observed working together. A scripted fixture cannot be persuaded by a
system-prompt clause.

| Claim | Proven by CI? | Why not |
|---|---|---|
| Prose breaks at spaces, never mid-word | **yes** | AC-3, pure unit tests, no pty |
| The audit fixture transposes with no over-wide row at 100 and 200 | **yes** | AC-4, fixture read from disk |
| The renderer is inert with stdout piped | **yes** | AC-7 — and it is the *entire* guard for BR-7 |
| `NO_COLOR` wraps and emits zero escapes | **yes** | AC-8, unit leg and pty leg |
| Rendered bytes at a fixed pty width | **yes** | AC-12 |
| A **real** reply is legible at 100 columns | **no** | legibility is a judgement, not an assertion |
| The clause changes what a real model writes | **no** | every fixture reply is scripted |
| The two halves compose on one screen | **no** | nothing in the suite runs both |

The first judgement is the plain one: **is it better?** The fixture at
`.adlc/specs/REQ-592-markdown-aware-terminal-rendering/fixtures/audit-2026-08-26.md`
is the **before** — the session that motivated this REQ, whose tables reached the
terminal as raw `|`-delimited prose hard-broken mid-word (`defens-` /
`e-in-depth`). **That file holds the model's *reply*, not the prompt** — its own
provenance comment says so, and says it is one section of one reply transcribed
by hand rather than a captured stdout dump. The original session's exact wording
was never recorded, so **this runbook carries the prompt** and every leg below
uses it verbatim.

The second is an *observation*, and it must be recorded as one. Leg (a) asks
whether the clause moved the model — whether the reply arrives as short
paragraphs and bullets rather than as sentence-in-a-cell tables. **If it did not,
that is a finding about BR-1 worth writing down, not a reason to re-run until it
looks good.** Prompt-adjacent behaviour is chaotic under byte-level changes
(BUG-168 flipped a reply's shape with a 23-byte trim two sentences away), and
REQ-577's `teton_docs` docstring needed live re-tuning for exactly this reason.
A partial outcome is a real outcome: the renderer half can pass while the clause
half does nothing, and the sign-off has separate lines for the two so that it
can say so. Re-running the same prompt until a pleasing shape appears measures
sampling noise and records it as evidence.

## Prerequisites

- **Both binaries built from this branch**, in the worktree:
  `cargo build --release --workspace`. Run the CLI by path —
  `./target/release/teton` — because `client.rs`'s daemon resolver prefers a
  `teton-code` sitting **beside the CLI executable** before it falls back to
  `PATH`.
- **Stop any daemon that is already running, and confirm which one serves.**
  This is the prerequisite that silently invalidates half the run: the daemon
  takes a single-instance lock, so an already-running *older* `teton-code` is
  **reused**, the system prompt is the shipped one, and TASK-282's half of this
  REQ is not under test at all — while the renderer half works perfectly,
  because that half lives in the CLI. `brew services stop teton` if it is the
  launchd one; otherwise `pkill -f teton-code`. Then, once the session is up,
  check from another shell that the path is this worktree's:

  ```
  pgrep -fl teton-code
  ```

  It must name `…/.claude/worktrees/<name>/target/release/teton-code`. Anything
  else and legs (a) and (b) measure the old prompt.
- `TETON_TEST_SEAMS` **unset**.
- **Start the session inside the teton-code repository**, so the session root is
  the tree being audited (REQ-583/584) and the model's file tools can actually
  reach it.
- **A tier that can audit a repository — write down which model.** A finding
  about reply shape is a finding about *that* model: a local-tier reply is not
  evidence about a remote one, or the reverse. Confirm the route with `/verbose`
  and record the `route …` line.
- **A terminal you can genuinely resize**, with `tput cols` to confirm the width
  before each leg. `stty cols 100` sets the same `TIOCGWINSZ` the CLI reads and
  is a fine way to drive the *layout*, but it does not narrow the window, so it
  cannot support the legibility judgement legs (a) and (b) exist for. Where a
  real 200-column window is not available (a laptop panel, a large font), say so
  in the sign-off and run leg (b) with `stty cols 200` — the layout claim stays
  checkable from a captured typescript, and the legibility half is recorded as
  weakened rather than as passed.

## The prompt

Use this **verbatim in all four legs**. A leg that reworded it is measuring
something else, and the clause's effect is only visible when the ask is held
constant. It deliberately does not ask for a table and does not forbid one —
what shape the model reaches for is the measurement.

```
Audit this repository for security defects. Cover the socket and IPC surface,
the path jail, the shell tool, egress and provenance, SSRF, and redaction. Say
what is strong and what is weak, and name the files.
```

## Procedure

### Leg (a) — 100 columns, which is where the reported defect lived

1. [ ] Resize the window to **100 columns**; confirm with `tput cols` → `100`.
2. [ ] `./target/release/teton` in the repo, then `/verbose`. Record the route
       line and the model.
3. [ ] Paste the prompt and send it. Let the whole reply finish.
4. [ ] **Prose.** Every wrapped row must break at a **space**. A break inside a
       word — the `defens-` / `e-in-depth` signature — means the terminal did the
       wrapping and the renderer did not, and is an outright failure of the leg.
       No row may run past the window's right edge and continue at column 0.
5. [ ] **Tables, if any arrived.** Each must be one of two shapes, never raw
       `|` rows: **aligned** (columns lined up, a drawn rule under the header,
       no printed `|---|` separator row), or **transposed** (one labelled
       `Column: value` block per data row, continuation rows indented two
       columns). At 100 columns a wide audit table should transpose.
6. [ ] **Inline.** `**bold**` and `` `code` `` must appear as styling, not as
       literal asterisks and backticks. Note that **inline styling inside a
       table cell is deliberately absent** — a bold cell renders as unstyled
       text at the right column. That is the documented trade, not a defect.
7. [ ] **Fenced code, if any arrived.** Original line breaks kept, no styling
       applied inside the fence, and a line longer than the window is left long
       rather than re-flowed.
8. [ ] **The judgement, in prose.** Compare against the fixture. Is this
       legible — can you skim it and find a finding? Write a sentence or two in
       the sign-off. "Better" is not enough on its own; say what is better and
       what still is not.
9. [ ] **The BR-1 observation, recorded as an observation.** Did the model
       produce tables at all? If it did: how many columns, and did any cell hold
       a whole sentence? The clause asks for at most three short columns and no
       sentence in a cell. Record what actually arrived — including "it produced
       the same wide tables as before", which is a legitimate and useful result.
       **Do not re-run to get a nicer answer.**
10. [ ] Keep the reply. Copying the scrollback into a scratch file is enough;
        the sign-off quotes from it.

### Leg (b) — 200 columns, where the layout has room

1. [ ] **Without leaving the session**, resize the window to **200 columns**;
       confirm with `tput cols`. Resizing mid-session is legitimate and is
       itself worth one observation: the new width takes effect on the **next
       turn**, and rows already on screen keep the breaks they were drawn with.
       Record whether the next turn did lay out at the new width — a width
       frozen at construction is a defect this REQ specifically closed.
2. [ ] Send the same prompt again.
3. [ ] Same four checks as leg (a): spaces not mid-word, tables in one of the
       two shapes, inline styling, fences verbatim.
4. [ ] **Expect some values not to wrap, and do not read that as a failure.** A
       transposed block wraps a value only when the value is wider than the room
       left beside its label; at 200 columns several of the fixture's values fit
       on one row, and inserting a break there would be wrong. The rule is the
       biconditional — wrapped **if and only if** too wide — not "everything
       wraps".
5. [ ] The judgement again, in prose: is 200 columns better than 100, worse, or
       merely different? A table that transposes at 100 may align at 200, and
       which reads better is worth recording.

### Leg (c) — `NO_COLOR=1` at 100 columns: wrapping without escapes

Colour and layout are separate branches of the same surface. This leg is the
shipped-path witness that turning colour off does **not** turn wrapping off.

1. [ ] Back to **100 columns** (`tput cols`).
2. [ ] `NO_COLOR=1 ./target/release/teton`, same prompt.
3. [ ] **Wrapping is still present** — prose breaks at spaces, tables still
       aligned or transposed. A plain unwrapped ribbon here means the colour
       switch reached the layout, which it must not.
4. [ ] **No escapes reach the screen**: no colour, and no visible `[1m` debris.
5. [ ] For a mechanical check, capture a typescript and count the lines
       carrying an SGR sequence (BSD `script`, as on macOS; GNU `script` wants
       `-c 'cmd' file`):

   ```
   NO_COLOR=1 script -q /tmp/req592-nocolor.typescript ./target/release/teton
   LC_ALL=C grep -c $'\x1b\[[0-9;]*m' /tmp/req592-nocolor.typescript
   ```

   Expect **0**. If it is not zero, record the sequences verbatim — that is
   the finding. (Cursor-motion escapes from the entry frame and the loading
   indicator are *not* SGR and are not what this counts.)
6. [ ] The same capture gives an honest widest-row figure, because with no SGR
       in the stream the bytes and the columns agree for ASCII output:

   ```
   LC_ALL=C awk '{ if (length($0) > m) m = length($0) } END { print m }' \
     /tmp/req592-nocolor.typescript
   ```

   Rows wider than 100 are expected only for the two documented cases: a
   single word longer than the window (kept whole rather than split), and a
   line inside a fence. Anything else is a finding. Non-ASCII content makes
   byte length disagree with display width — note it rather than trusting
   the number.

### Leg (d) — stdout to a file: the renderer must be inert (BR-7, by eye)

AC-7 asserts that piped bytes equal the raw chunks the daemon sent, for a
**scripted** turn. This leg makes the same claim against a **real** reply, which
is the only version that can catch a gate that happens to be inert for the
fixture's shapes and not for something the fixture never contained. It matters
more than it looks: inverting the gate leaves all of the pre-existing end-to-end
tests green, so AC-7 plus this leg are the whole of BR-7's defence.

1. [ ] `./target/release/teton > /tmp/req592-piped.md`, in the repo.
2. [ ] **You will be typing blind.** The entry frame is written straight to
       stdout by the prompter, and stdout is now the file — expected, not a bug.
       Type the prompt, press Return, wait for the reply to finish, then Ctrl-D.
3. [ ] Read the file. The assistant text must be **unrendered markdown**:
       literal `|` table rows including the `|---|` separator, literal `**bold**`
       and backticks, and long lines **unbroken** — no inserted newlines at any
       column.
4. [ ] No escape sequences at all:

   ```
   LC_ALL=C grep -c $'\x1b' /tmp/req592-piped.md
   ```

   Expect **0**.
5. [ ] If typing blind is unworkable, the convenience form is

   ```
   printf '%s\n' 'Audit this repository for security defects. …' \
     | ./target/release/teton > /tmp/req592-piped.md
   ```

   Note in the sign-off which form was used. The redirect-only form is the
   faithful one — BR-7's gate reads *stdout* alone — while the piped form
   changes stdin as well and matches AC-7's shape exactly. Either is
   acceptable evidence; they are not the same evidence.
6. [ ] **One observation worth taking here.** The output-format clause is
       unconditional: the daemon is not told whether the client's stdout is a
       terminal, so the model is told about a narrow terminal even on this run.
       The reply's *shape* should therefore resemble legs (a) and (b) even
       though its *rendering* does not. If the shape differs markedly, that is
       sampling variance and belongs in the notes — it is also the cheapest
       available evidence about how stable the clause's effect is.

## Sign-off

```
REQ-592 sign-off
----------------
Verified by                                        :
Date / build / commit                              :
Platform / OS / terminal emulator                  :
Model and tier (from the `route …` line)           :
`pgrep -fl teton-code` named THIS worktree's binary : yes / no  <-- must be "yes"
TETON_TEST_SEAMS confirmed unset                    : yes / no
Session started inside the teton-code repo          : yes / no

(a) tput cols read 100                              : yes / no
(a) every prose break landed on a space             : yes / no  <-- must be "yes"
(a) no row ran past the edge and continued at col 0 : yes / no  <-- must be "yes"
(a) tables arrived?                                 : yes / no   (how many:)
(a) each table was aligned or transposed, never raw : yes / no / n.a.
(a) no literal `|---|` separator row was printed    : yes / no / n.a.
(a) bold/code rendered as styling, not punctuation  : yes / no
(a) fenced blocks kept their breaks, unstyled       : yes / no / n.a.
(a) LEGIBILITY vs the fixture, in prose             :
(a) BR-1 OBSERVATION — shape the model chose        :
       tables produced?                             : yes / no
       widest table, in columns                      :
       any cell holding a whole sentence?            : yes / no
       verdict: clause moved it / no visible effect / unclear
(a) number of times the prompt was sent             :        <-- 1 is the honest answer

(b) tput cols read 200 (or stty cols 200 — which:)  :
(b) the resize took effect on the NEXT turn         : yes / no
(b) already-printed rows kept their old breaks      : yes / no
(b) every prose break landed on a space             : yes / no  <-- must be "yes"
(b) tables aligned or transposed, never raw         : yes / no / n.a.
(b) values that fit on one row were NOT broken      : yes / no
(b) LEGIBILITY at 200 vs at 100, in prose           :

(c) wrapping still present with NO_COLOR=1           : yes / no  <-- must be "yes"
(c) SGR-carrying line count in the typescript        :          <-- must be 0
(c) if non-zero, the sequences, verbatim             :
(c) widest captured row                              :        cols
(c) any over-wide row NOT a long word or a fence line:

(d) form used                                        : redirect-only / piped-stdin
(d) output was unrendered markdown (literal | ** `)  : yes / no  <-- must be "yes"
(d) long lines were unbroken — no inserted newlines  : yes / no  <-- must be "yes"
(d) escape-byte count in the file                    :          <-- must be 0
(d) reply SHAPE resembled legs (a)/(b)               : yes / no  (notes:)

Anything that hung, aborted, panicked, or crashed    :
Findings worth filing (renderer half)                :
Findings worth filing (clause half, incl. "no effect"):
Notes                                                :
```

---

# Manual verification runbook — REQ-612 AC-13 (the repository's own notes, dogfooded)

**Status: OUTSTANDING — nothing below has been executed.** REQ-612 spends up to
8,192 resident bytes of every local turn on the repository's own `TETON.md`, and
it does so on the strength of ASSUME-1: that a small local model *uses* resident
repository notes the way it uses the environment line, rather than searching the
tree anyway. AC-13 is the only check that assumption gets, and no test can make
it — a scripted engine answers what the fixture told it to answer, which is
precisely the thing in question. Leave AC-13 unticked in
`.adlc/specs/REQ-612-teton-md-repo-context-file/requirement.md` until the
sign-off block below is filled in.

> If the answer is "the local model searched anyway", that is a **finding, not a
> failure of the run**: the cap is then spent for nothing on that tier, and
> BR-2's switch (`/context off`, `[context] repo_file = false`) is the remedy the
> architecture already names. Record it and file it; do not re-run until it
> comes out the way the spec hoped.

## What this proves that CI does not

| Claim | Proven by CI? | Why not |
|---|---|---|
| The file is found, capped, framed, sanitized | **yes** | `crates/tetond/tests/repo_context.rs` and the renderer's own tests |
| A covered file is withheld and the manager knows | **yes** | the provenance/egress acceptance legs |
| The resident ceiling still clears its floor | **yes** | the two sweeps, at the block's worst case |
| A **real** local model answers from the notes | **no** | no engine in CI, and a scripted one answers as scripted |
| Zero searches is worth up to 8 KiB of every call | **no** | that is a judgement about answers, not about bytes |

## Prerequisites

- A build **with the engine**: `cargo build --release --workspace --features
  tetond/llama`. A default build has no local tier, and a remote answer says
  nothing about ASSUME-1, which is a claim about the weak tier.
- `teton doctor` confirming a loaded local engine **before** you start.
- This repository as the session root, `[context] repo_file` at its default
  (on), and `/verbose` on.
- A `TETON.md` at the repository root describing the **crate layout** — `teton`
  (CLI), `tetond` (daemon: `harness`, `runtime`, `egress`, `repo_context`),
  `teton-core`, `teton-protocol`, `teton-providers`, `teton-inference` — and
  naming in one line that the system prompt is assembled by
  `build_system_prompt` in `crates/tetond/src/harness/turn_loop.rs`. Keep it
  under 8,192 bytes: a truncated file measures a different file than the one you
  wrote, and `/context` will say so.

## The prompt

Exactly this, as the **first** prompt of a fresh session, on every leg:

    where does the system prompt get built?

Nothing before it — no `/help`, no `/cd`, no warm-up. A first prompt is the one
whose answer can only have come from the notes or from a search of the tree.

## Counting the searches

`/transcript on` before the prompt, then count the tool calls the daemon
recorded rather than the lines that scrolled past:

    jq -r 'select(.kind=="tool_call_input") | .tool' <session>.jsonl | sort | uniq -c

`glob` and `grep` are the two AC-13 counts. A `read` of a path the notes
themselves named is **not** a search — record it separately, because "read the
one file the notes pointed at" is the behaviour the feature is buying.

## Procedure

### Leg (a) — with the file (AC-13's positive half)

1. `TETON.md` in place. Fresh session in this repository. `/transcript on`.
2. `/context` — expect state `loaded`, the file `TETON.md`, its resident bytes,
   and **not** truncated. If it says `truncated`, trim the file and start over.
3. Send the prompt, once. Do not re-ask, and do not rephrase: a second attempt
   measures a different thing and the count is no longer AC-13's.
4. Count `glob` + `grep` calls. AC-13 wants **zero**.
5. Record the answer verbatim, and whether it names
   `crates/tetond/src/harness/turn_loop.rs` (or `build_system_prompt`). A
   zero-search answer that is *wrong* is a worse outcome than one search, and
   the sign-off has a row for exactly that.

### Leg (b) — without the file (the control)

1. `mv TETON.md TETON.md.off`, and check no `AGENTS.md` is left at the root —
   the fallback would quietly make this the same leg as (a).
2. Fresh session, `/transcript on`, `/context` — expect state `absent`.
3. The same prompt, once. Count again: AC-13 wants **at least one** `glob` or
   `grep`. If this leg also searches zero times, the local model knew the answer
   without either source and **neither leg is evidence** — say so in the notes
   and pick a fact the weights cannot hold (a path this branch introduced).

### Leg (c) — the switch, on the way past

1. Put `TETON.md` back. Fresh session, `/context off`.
2. `/context` — expect `withheld_off`; `config.toml` must be untouched
   (`grep -c repo_file` on it is the cheap check).
3. The same prompt. It should behave like leg (b), which is what says the switch
   reaches the prompt and not just the report.

## Sign-off

```
REQ-612 AC-13 sign-off
----------------------
RESULT                                              : OUTSTANDING
Verified by                                         :
Date / build / commit                               :
Machine (chip, RAM, OS)                             :
Model loaded (name, quant, n_ctx)                   :
`teton doctor` confirmed a loaded local engine      : yes / no  <-- must be "yes"
TETON.md size, in bytes                             :
`/context` reported state / resident bytes          :

(a) glob calls                                      :
(a) grep calls                                      :          <-- AC-13 wants 0 for both
(a) any read of a path the notes named              : yes / no  (which:)
(a) the answer named turn_loop.rs/build_system_prompt: yes / no
(a) the answer, verbatim                            :
(a) prompt sent exactly once                        : yes / no  <-- must be "yes"

(b) TETON.md moved away, no AGENTS.md present       : yes / no  <-- must be "yes"
(b) `/context` reported `absent`                    : yes / no
(b) glob calls                                      :
(b) grep calls                                      :          <-- AC-13 wants >= 1 total
(b) the answer, verbatim                            :

(c) `/context off` reported `withheld_off`          : yes / no
(c) config.toml untouched                           : yes / no  <-- must be "yes"
(c) searched like leg (b)                           : yes / no

ASSUME-1 verdict: notes used / notes ignored / inconclusive
Anything that hung, aborted, or crashed             :
Notes / findings                                    :
```

---

# Manual verification runbook — REQ-613 AC-13 (the file Teton writes, dogfooded)

**Status: OUTSTANDING — nothing below has been executed.** REQ-613 spends one
frontier model call per repository and then writes its answer into the user's
working tree, where REQ-612 makes it resident on every turn of every session
afterwards. It does that on the strength of ASSUME-1: that a frontier-tier model
given the whole tree, the manifests, the README and the entry-point headers
produces a description **a new contributor would not object to**. AC-13 is the
only check that assumption gets. No test can make it — CI can prove the file is
bounded, headed, no-clobber and loaded, and none of that is a judgement about
whether the prose is any good. Leave AC-13 unticked in
`.adlc/specs/REQ-613-generate-teton-md-when-absent/requirement.md` until the
sign-off block below is filled in.

> A file that comes out thin is a **finding, not a failed run**. The levers the
> architecture already names are OQ-5's cap and ADR-4's tier: `/policy
> set-category draft <tier>` moves the model, and the evidence tables in
> `crates/tetond/src/repo_context/evidence.rs` are where a missing input would
> be added. Record what was thin and file it; do not re-roll the same repository
> until it reads better — a second draft of the same tree measures the sampler,
> not the feature.

## What this proves that CI does not

| Claim | Proven by CI? | Why not |
|---|---|---|
| One prompt, one write, no clobber, header first | **yes** | `crates/tetond/tests/repo_context_generation.rs` and the writer's unit tests |
| The draft is bounded to the cap less the header, and loads as `loaded` | **yes** | the write/load acceptance legs |
| Covered evidence never reaches the call | **yes** | the egress-capture leg |
| The evidence tables and the walk bounds hold | **yes** | the gatherer's unit tests |
| The **prose** is one a new contributor would accept | **no** | that is a judgement about answers, not about bytes |
| A frontier model needs nothing this evidence set leaves out | **no** | only a real repository's real draft can say |

## Prerequisites

- A **fresh clone** of this repository, at a path this machine has never run a
  session in: `git clone <this repo> ~/tmp/teton-ac13 && cd ~/tmp/teton-ac13`.
  A fresh clone is the subject — a working tree with your own `TETON.md`,
  `AGENTS.md`, or an editor's leftovers in it measures a different repository.
- **No `TETON.md` and no `AGENTS.md` at the clone's root.** Check both by name;
  either one suppresses the offer, and a run that never saw a prompt has
  measured nothing.
- `teton doctor` reporting `repo notes: on (default)` and `[context] generate`
  at its default `ask`. If this machine has `generate = never` set, the offer
  will not be raised; if it has `always`, the file is written with no prompt and
  the consent half of the leg is not exercised.
- A policy whose `think` tier resolves to a **remote frontier provider**
  (`/policy show`). A local-tier draft is a legitimate configuration and a
  different measurement; if that is what you ran, say so in the sign-off — the
  header line records the tier either way.
- The session at `guarded` (`/permissions` to confirm) and `/verbose` on.

## The prompt

Exactly this, as the **first** prompt of the session in the fresh clone:

    what is this repository?

Nothing before it — no `/help`, no `/cd`, no warm-up. The offer rides the first
prompt turn, so anything typed ahead of it spends the turn the offer was going
to ride. The *answer* to this prompt is not what AC-13 measures; the **file** is.
Reading the answer is still worth a line in the sign-off, because a good answer
on that same turn is the evidence that the file was written and made resident
before the turn's own model call.

## Procedure

1. Fresh session in the clone. The banner should carry the announcement clause —
   `no TETON.md here — Teton will offer to write one on your first prompt`.
   Record whether it did; a prompt that arrives unannounced is a finding on its
   own (BR-1).
2. Send the prompt, once. Exactly one permission prompt should be drawn, naming
   the root, the path `TETON.md`, and that it is a **write** (not a replace).
3. Accept it **once** (not "for this session" — the offer runs once per session
   per root anyway, and `once` is the answer whose scope this leg is about).
4. Watch the event lines: `walking`, `drafted`, `written`. With `/verbose` on,
   record the drafting line's tier, entries walked and input tokens.
5. `/context` — expect state `loaded`, file `TETON.md`, `origin: generated`, and
   **not** `truncated`. A `truncated` here is a defect, not a finding: the draft
   is bounded to the cap less the header before it is written.
6. Confirm the turn was answered. The prompt raised the offer and should still
   have produced a reply.
7. **Now read the file** — `cat TETON.md`, whole, once, as a new contributor
   would. Score it against the rubric below before reading any source.
8. `git status` — the only change in the tree should be the one new untracked
   `TETON.md`. Record `wc -c TETON.md` and check the first line is the header.
9. `/cost` — one row under the `draft` category, and the tier it names should be
   the tier the header names.

## The reading rubric

**The bar is "a new contributor would not object".** Not "is it accurate in
every clause", not a score out of ten — would a person joining this repository
tomorrow read this file and find nothing in it to argue with, and something in
it to use. Three facts are named outright because they are what this repository
is, and a draft that misses them has missed the tree it was given:

| # | The file names… | Found? |
|---|---|---|
| 1 | **the crates** — `teton` (CLI), `tetond` (daemon), `teton-core`, `teton-protocol`, `teton-providers`, `teton-inference` | |
| 2 | **the daemon/CLI split** — that `tetond` holds the harness, runtime and providers and `teton` is a client speaking JSON-RPC over a socket | |
| 3 | **the test command** — `cargo test --workspace`, or the per-crate form a contributor would actually run | |

Record each as found / partly / absent, **and record the verdict separately**:
the three facts are the floor, not the bar. A file that names all three and
reads like a manifest still fails "a new contributor would not object", and a
file that misses one but is otherwise right is a finding about the evidence set
rather than about the model. The result is **recorded, not scored** — there is
no pass mark here, only a written answer and the objections a reader had.

## Sign-off

```
REQ-613 AC-13 sign-off
----------------------
RESULT                                              : OUTSTANDING
Verified by                                         :
Date / build / commit                               :
Clone path (fresh, never sessioned)                 :
No TETON.md and no AGENTS.md at the root            : yes / no  <-- must be "yes"
Permission level / `[context] generate`             :          / ask
`think` resolved to (provider, model)               :
Local-tier draft instead?                           : yes / no  (if yes, say why)

Banner carried the announcement clause              : yes / no
Exactly one permission prompt was drawn             : yes / no  <-- must be "yes"
The prompt named the root, `TETON.md`, and a write  : yes / no
Answer given                                        : once / session / declined
Event lines seen (walking/drafted/written)          :
`/verbose` drafting line, verbatim                  :
`/context` state / origin / resident bytes          :          / generated /
The turn's own prompt was still answered            : yes / no
`git status` showed only the new TETON.md           : yes / no
TETON.md size, in bytes                             :
First line was the generated header, verbatim       :
`/cost` row: category / tier                        : draft    /

(rubric) 1. names the crates                        : found / partly / absent
(rubric) 2. names the daemon/CLI split              : found / partly / absent
(rubric) 3. names the test command                  : found / partly / absent

VERDICT — "a new contributor would not object"      : agree / object
What a new contributor would object to              :
What was missing that the evidence set could supply :
Anything that hung, aborted, panicked, or crashed   :
Notes / findings                                    :
```
