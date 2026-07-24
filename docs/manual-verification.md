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

1. **The daemon never loads the weights it installs.** `tetond` constructs a
   local engine only from `TETON_LOCAL_SCRIPT`; nothing anywhere builds a
   `LlamaEngine` from an installed GGUF, and `tetond` exposes no `llama` feature
   at all. Consent, download, verification and install are real and complete —
   *serving a turn from the result is not wired.* Step 5 below therefore
   benchmarks the installed file directly rather than through a session.
2. **No post-install benchmark.** The consent flow publishes `ready` when the
   install returns; it never runs `teton_inference::benchmark::run_benchmark`.
   The `benchmark …` line you see at startup is REQ-544's *synthetic* probe
   sequence and describes no measurement.
3. **The startup lifecycle overstates reality.** That same synthetic sequence
   prints `download …`, `benchmark …` and `local model … ready` on every client
   attach — including before you have answered the proposal, and on a machine
   with no weights at all.
4. **The proposal event never reaches a client.** `model_selection_proposed` is
   published before the daemon accepts connections, so every client reconstructs
   the prompt from `model/status` + `model/list` and cannot name *which* entry
   was proposed — only its band.

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

```sh
# Inspect first, then remove. These are the daemon's machine-state files.
ls "${XDG_RUNTIME_DIR:-/tmp}/teton/"
rm -f  "${XDG_RUNTIME_DIR:-/tmp}/teton/model-selection.toml"
rm -rf "${XDG_RUNTIME_DIR:-/tmp}/teton/models"
```

Record what you removed. If you had a working local model before this run, you
are about to re-download it.

### 1. Build with the real engine

```sh
cargo build --workspace --release --features teton-inference/llama
```

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

Then answer `a` (accept as offered).

> Expect known gaps 3 and 4 above to be visible here: the synthetic `… ready`
> lines before you answer, and a prompt that names the *band* rather than the
> proposed model. Note whether you see them; they are not AC-13 failures.

### 3. Watch the real transfer

Record from the progress output and from the filesystem:

- [ ] download progress advances (`model_lifecycle` `download` events)
- [ ] a `.part` file exists **during** the transfer and is gone after it
      ```sh
      ls -la "${XDG_RUNTIME_DIR:-/tmp}/teton/models"
      ```
- [ ] the transfer completes and the daemon reports the model ready
- [ ] wall-clock duration of the download: __________
- [ ] the installed file's size matches the catalog's `size_bytes`

**Verify the digest yourself — do not take the daemon's word for it:**

```sh
shasum -a 256 "${XDG_RUNTIME_DIR:-/tmp}/teton/models/<model>.gguf"     # macOS
sha256sum     "${XDG_RUNTIME_DIR:-/tmp}/teton/models/<model>.gguf"     # Linux
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

### 5. Load the installed weights and measure them

Per known gap 1, the daemon does not yet serve turns from installed weights, so
"run a session on the local tier" is **not currently possible** and must not be
reported as done. What *is* possible — and is the measurement AC-13 asks for — is
loading the file you just installed into the real llama.cpp binding and timing
it.

```sh
TETON_TEST_GGUF="${XDG_RUNTIME_DIR:-/tmp}/teton/models/<model>.gguf" \
  cargo test -p teton-inference --features llama --test llama_smoke -- --ignored --nocapture
```

- [ ] the GGUF loads without error — the bytes are a working model, not just
      bytes with a matching digest
- [ ] the completion streams and is coherent

Then time it. `teton_inference::benchmark::run_benchmark` is the same function
the production step-down chain uses; run it against the loaded engine (a short
`examples/` binary or an added `#[ignore]`d test is fine — record which you used)
and note:

- [ ] observed time to first token: __________ ms
- [ ] observed decode throughput: __________ tok/s

> REQ-544's BR-8 latency duty is **≤ 1000 ms to first token**. If the observed
> figure is worse, that is a finding to record here, not a reason to re-run until
> it passes.
>
> Also record, explicitly: **a session was not served from these weights**, and
> why (known gap 1). AC-13 is signed off on what was actually observed, never on
> what was expected to happen.

### 6. Restart: the decision is not re-litigated (BR-10)

```sh
kill %1 && ./target/release/tetond &
./target/release/teton model status
```

- [ ] no proposal is raised on the second start
- [ ] the model is still reported verified and no bytes were re-fetched

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
Session served from these weights : no  (known gap 1 — leave as `no` unless the
                                         daemon has since been wired to load them)
Restart re-prompt:  none / observed
Known gaps 1–4 observed as described :  yes / no  (list any differences)
Notes / findings :
```

<!-- Add further sign-off blocks below, one per platform and per release. -->

_No sign-off has been recorded yet. AC-13 remains unticked._
