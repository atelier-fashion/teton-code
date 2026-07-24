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
                    Brett Luelling's direction in a supervised session. The
                    runbook asks for a human runner; Brett should countersign
                    here (or re-run) to satisfy that letter — every observation
                    below is from the real daemon on his machine, not a mock.
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
