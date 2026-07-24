# REQ-547 Phase 5 — Consolidated Review Findings

Six agents (reflector, correctness, quality, architecture, test-coverage,
security). Deduped, ranked. "Confirmed by N" = independent convergence.

## CRITICAL

### C-1 — Config pin bypasses consent AND inherits REQ-544's legacy key
Confirmed by reflector (Critical), architecture (Major), quality (Major, the
validation half). `ConsentGate::resolve`'s `pinned` branch calls
`commit(…, ConfigPin, None)` directly — no proposal published, no RAM-floor
check. `from_env` fills `config.pinned` from `effective_pinned_local_model()`,
which falls back to REQ-544's legacy top-level `pinned_local_model`. Under
REQ-544 that key meant "override the probe's pick", NOT "consent to a
download" (REQ-544 downloaded unconditionally anyway). **An existing user who
set it to the 30B entry gets an unprompted ~18 GB fetch on first REQ-547
start** — verbatim the failure this REQ exists to close. Also silently
falsifies AC-1's `[x]`: "zero bytes until the user answers" holds only absent a
pin, and no test counts artifact requests on the pinned path.
→ Fix: the pin already feeds `probe_decision()`; delete the `commit(ConfigPin)`
branch so the proposal NAMES the pinned entry and the user answers it.

## HIGH / MAJOR

- **H-1 `load_config` fails open (security High).** `Config::load(&text)
  .unwrap_or_default()` — any validation error discards the ENTIRE config for
  `Config::default()`, which has `boundaries: vec![]`. This REQ added three new
  validation errors on a new table and put them FIRST, so a typo like
  `base_url = "hf-mirror.corp.internal"` (no scheme) silently wipes every
  privacy boundary, provider, routing rule and MCP server. Nothing logged.
- **H-2 Consent screen omits provenance (security High).** The proposal carries
  name/band/size/RAM but not host, repo, or publisher — "huggingface" appears
  nowhere in `crates/teton/`. User approves an 18 GB transfer from a
  third-party quantizer blind, and `base_url`/`TETON_CATALOG` can redirect the
  fetch while the prompt renders identically. BR-11 (no URLs on wire) vs BR-2
  (legibility) collided and hygiene won. Publisher/host/short-revision are not
  credentials/paths/content — adding them does not violate BR-11.
- **H-3 `TETON_CATALOG` ungated + catalog `name` path traversal (security High,
  correctness Minor).** Ships in production, no gate, no host/scheme restriction
  (the https/HF assertion lives only in a test against the BUNDLED catalog).
  `name` is unvalidated and interpolated into `weights_dir.join(format!("{name}
  .gguf"))` → `../../../../…` escapes the weights dir.
- **M-1 `run_install` publishes `Ready` with no engine** (reflector Major,
  test-audit highest-risk). Contradicts TASK-009's `startup_lifecycle`, whose own
  comment says saying `ready` there "would be the exact untruth this function
  exists to stop". **The AC-2 test REQUIRES the overclaim** (asserts
  `stage == "ready"`), so the suite enshrines it. The CLI negative assertion
  passes only incidentally via process-exit timing.
- **M-2 Concurrent installs corrupt the shared `.part`** (correctness Major,
  security Medium). `create(true).append(true)`; two in-flight installs
  interleave bytes, each tracking its own counter → digest fails → user told
  "corrupt" when nothing upstream was wrong. `remove_file(dest)` pulls the file
  from under another open fd. `model/set` spawns unbounded install tasks.
- **M-3 `status()` re-hashes multi-GB synchronously on a tokio worker**
  (correctness Major). Reached inline from `do_handshake` → `startup_lifecycle`
  → `consent_required()`. Receipt is only written on the `Verified` branch, so a
  right-size-but-corrupt file is re-hashed on EVERY attach, forever, blocking a
  runtime thread.
- **M-4 `set_model` does not cancel the outstanding proposal** (correctness
  Major). Stale prompt persists; answering it `Accept` overwrites the user's
  explicit choice and installs a different model. Violates BR-10.
- **M-5 Unbounded retry** (correctness Major). `while written < size_bytes`
  exits only on stall or non-resumable error; a host returning 1 byte per
  connection resets the stall counter each iteration → infinite loop spawning an
  OS thread + tokio runtime per iteration. No total-attempt or wall-clock cap.
- **M-6 `let _ = store.record(...)` discards persistence errors** at all three
  sites (reflector Major). BR-4's "declining is persisted" silently fails; user
  re-prompted forever with no signal.
- **M-7 Legacy `pinned_local_model` alone is never shape-validated** (quality
  Major, reflector Major). Shape check runs only inside the new-key branch;
  `effective_pinned_local_model()` then promotes the unvalidated value.
- **M-8 `TETON_DISK_FREE_BYTES` disables BR-7 preflight in production**
  (security Medium, reflector Minor). A seam that can RAISE the measurement can
  disable the safety check outright.
- **M-9 `refresh-catalog.py` injects unvalidated `lfs["oid"]` into TOML**
  (security Medium). A hostile/MITM'd API response containing `"` + newline
  injects arbitrary catalog entries during `--update`.
- **M-10 Install receipt is forgeable but gates the tier** (security Medium).
  Keyed on (digest, size, mtime); mtime is not a tamper signal. It is a cache,
  not an attestation. `deep_status()` is never called in production.
- **M-11 Symlink-following on a predictable `.part` path** (security Medium).
  No `O_EXCL`/`O_NOFOLLOW`; under the temp-dir fallback base this is a
  cross-user arbitrary-file create/append primitive.
- **M-12 TOCTOU between verify and rename** (security Medium). Bounded by
  directory ownership; interacts with M-11.
- **M-13 Third-party quantizer trust undocumented** (security Medium). The
  `unsloth/` entry's pin prevents post-pin substitution but says nothing about
  fidelity at pin time; `--update` silently re-grants trust to new commits.

## MINOR (selected)
WEIGHTS_DIR + `.gguf` filename duplicated across crates (quality/reflector — BR-11
does NOT actually block sharing a bare constant; `socket_path` is the precedent);
`format_bytes` prints GB while dividing by 1024 and the daemon says GiB, on
ADJACENT lines of the same proposal; `Accept` isn't pre-validated so it consumes
the waiter permanently (local tier dead for daemon lifetime, `ChoiceRefusal` doc
is false); `Undecided` branch unreachable + consent task leaks; live-event answer
is fire-and-forget so daemon refusals are invisible; `handle_model_set` silently
no-ops outside a runtime while returning Ok; stale `.part` left after a Verified
early return; HTTP 416 (catalog size drift) reported as a network error;
`pub mod hash` over-exposes the hand-rolled SHA-256 internals; code cites `(BR-9)`
meaning REQ-544's BR-9 while THIS REQ's BR-9 is atomic install; CI job still
labeled "acceptance suite (AC-1..AC-9)"; `.expect()` on the production event
pump; fixed-duration `drain_events` waits risk CI flake; `shortfall_bytes`
ignores a corrupt file at the final path; **AC-8's "full-digest verification
mode" does not exist as such** (D-1 reinterprets it via BR-6; OQ-3's checkbox is
still `[ ]` though architecture.md claims resolution); no `cargo audit` in CI;
the spec's "user only" permission is a convention, not a control — any same-uid
process (incl. an agent `shell` call) can trigger an install.

## VERIFIED SOLID (multiple agents)
Digest pin IS enforced at fetch time (`install` rewrites only `url`, never
`sha256`). Two-client separation holds; userinfo URLs refused fail-closed. BR-11
holds against all new payloads (`model_results_never_carry_an_install_path`).
BR-1 enforced by construction EXCEPT the pin hole. BR-2 legibility is real and
`pending_proposal` made it attach-timing-independent. Atomic install correct
(`.part` → verify → `rename(2)`). AC-1's zero-download test is unfakeable (real
TCP listener, production fetcher, real-URL builder never invoked in tests).
AC-13 genuinely manual-only, sign-off block unfilled. Suite is hermetic. No
vacuous tests found. No new dependencies; transport crates at non-vulnerable
versions.

## USER DECISIONS PENDING (halt #2)
1. Pin-as-consent: fix so the pin feeds the proposal, or amend the spec?
2. Legacy `pinned_local_model`: hard-deprecate or keep?
3. Test seams (`TETON_CATALOG`, `TETON_DISK_FREE_BYTES`): gate behind
   debug/flag, or ship as operator overrides?
4. AC-8 traceability: is D-1's reinterpretation the intended reading?

## USER DECISIONS — RESOLVED 2026-07-24
1. Pin behavior: **PIN PROPOSES, USER ANSWERS** (delete silent commit(ConfigPin); pin feeds the proposal)
2. Legacy pinned_local_model: **HARD-DEPRECATE** (validation error → move to [local_model] pinned)
3. Test seams: **GATE behind TETON_TEST_SEAMS=1** (release refuses); TETON_DISK_FREE_BYTES may only LOWER the measurement
4. AC-8: **REWORD to match D-1** (metadata + download-time digest); close OQ-3

## FIX PASS PLAN (sequential — shared tetond files)
- Group A: C-1, H-1, M-4, M-6, M-7, decisions 1+2, Accept/Undecided minors → model_consent.rs, runtime.rs, config.rs, server.rs
- Group B: M-1 (+AC-2 test), M-2, M-3, M-5, install minors → install.rs, download.rs, consent_matrix.rs
- Group C (security): H-2 provenance, H-3 name+TETON_CATALOG gate, M-8, M-10, M-11, M-12, M-13, M-9, decision 3 → catalog.rs, install.rs, events, model_ui, refresh-catalog.py
- Group D (minors + docs): WEIGHTS_DIR→teton-protocol, format_bytes units, 416, hash vis, BR-9 cites, CI label, .expect, cargo-audit, AC-8 reword + OQ-3 close

## PHASE 5 RESOLUTION (2026-07-24)
Five fix groups (A consent/config, B lifecycle/install, C security, D polish,
E re-verify regressions) + a dedicated flake fix. Re-verify pass (security +
correctness) caught THREE regressions the earlier fix pass introduced:
 - CRITICAL: the C-1 fix opened a BR-3 bypass (decide() honours a pin with no
   RAM-floor test; Accept never called validate_choice; prompt defaults to yes)
 - deep_status() reintroduced the M-3 blocking-hash class on a new path
 - H-1's "fail loudly" was swallowed by a nulled daemon stderr
All fixed in Group E, mutation-checked (E-1, E-2, E-9). The ac8 flake was a
REAL classifier bug: an uncaught ConnectionResetError exits Python 1 ==
EXIT_MISMATCH, so a network reset was reported as catalog corruption; also
fixed a macOS accept() O_NONBLOCK inheritance bug that was silently recording
EMPTY request bodies (weakening REQ-544's BR-1 egress capture).
Final: 728 tests, 5/5 clean runs, clippy+fmt clean.

DEFERRED (documented, non-blocking):
 - auto_accept path has no RAM-floor check (judged: two explicit config keys
   read as deliberate consent, unlike a single Enter) — revisit if disputed
 - weights-dir PARENT chain uid/mode check (temp-dir fallback is /tmp 1777;
   sticky-bit rule needs design)
 - AC-2 (no LlamaEngine wired — REQ-544 debt) and AC-13 (manual gate) remain
   correctly UNCHECKED
 - pre-existing: commit trailers on this branch name "Claude Fable 5"; correct
   attribution is Claude Opus 4.8 (not rewritten — metadata only)
