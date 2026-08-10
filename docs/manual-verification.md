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
| Off costs nothing at all — no gate, no call, no latency | `runtime::tests::dispatch::redact::off_means_no_gate_and_on_means_a_gate_that_reaches_the_engine`, `egress::tests::off_means_zero_scanner_calls_and_on_means_exactly_one` | Full — by call count. This is what bounds the blast radius of the unmeasured budget: nobody who has not opted in pays it (OQ-3) |
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
peak KV is two contexts rather than one (~1.5 GiB each at `n_ctx` 16384). The
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

**Status: NOT RUN.**
