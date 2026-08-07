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
