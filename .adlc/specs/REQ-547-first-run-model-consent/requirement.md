---
id: REQ-547
title: "First-run local model consent: show the hardware-based pick, let the user override, then install"
status: complete
deployable: true
created: 2026-07-21
updated: 2026-07-21
component: "inference/probe"
domain: "inference"
stack: ["rust", "daemon", "cli", "gguf", "json-rpc"]
concerns: ["developer-experience", "reliability", "security"]
tags: ["first-run", "model-download", "hardware-probe", "user-consent", "catalog", "checksum", "override"]
---

## Description

REQ-544 shipped the full local-model selection machinery — a hardware probe that
picks a band per the OQ-3 table, a resumable GGUF download with SHA-256
verification, a post-download micro-benchmark with auto-step-down, and runtime
memory-pressure adaptation. Two things keep it from working for a real user:

1. **It never asks.** The daemon selects and downloads autonomously. REQ-544's
   AC-1 mandated "zero-config auto-proceed", so the CLI's `confirm_model`
   function was implemented but deliberately left unwired — there is no protocol
   hook for the daemon to pause its download on a client answer. Silently
   pulling 1–18 GB over someone's network, onto their disk, without telling them
   which model was chosen or why, is the wrong default for a tool whose entire
   pitch is user control and legibility.
2. **The catalog is placeholders.** Every `url` in
   `crates/teton-inference/data/models.toml` points at
   `https://models.tetoncode.ai/...` (nothing is hosted there) and every
   `sha256` is a literal stub (`…0001`, `…0002`). On a real machine the probe
   correctly identifies the right Qwen model, attempts the download, and fails.

This REQ closes both. First run becomes: **probe → show the user what was picked
and why → they accept, override, or decline → then (and only then) download,
verify, install, benchmark.** Post-first-run, the choice is changeable. An
explicit opt-in auto-accept path preserves unattended/CI use.

**Amendment to REQ-544 AC-1**: "zero-config first run" becomes "one confirmation,
then zero-config" for interactive use. The zero-touch path still exists but is
now explicit opt-in (BR-5) rather than the silent default. This is a deliberate
narrowing of AC-1 and is recorded here so the change is traceable rather than
looking like a regression.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| ProbeReport | total_ram_bytes | number | required; from the existing probe |
| ProbeReport | free_disk_bytes | number | required |
| ProbeReport | gpu_class | enum(apple_silicon, cuda, cpu) | required; mirrors the shipped `teton_inference::probe::GpuClass` verbatim — the spec originally said `metal`, which was wrong; one spelling per concept |
| ProbeReport | chosen_band | enum(none, small, mid, large) | required |
| ProbeReport | reason | string | required; user-facing sentence explaining the band choice |
| ModelSelection | model_name | string | null when the local tier is declined |
| ModelSelection | source | enum(probe, user_override, config_pin, auto_accept) | required |
| ModelSelection | declined_local | boolean | required; true = run remote-only |
| ModelSelection | decided_at | timestamp | required |
| CatalogEntry | name, url, sha256, size_bytes, ram_floor_bytes, band | mixed | exists (REQ-544); `url`/`sha256` become real values |
| InstallState | model_name | string | required |
| InstallState | status | enum(absent, partial, verified, corrupt) | required |
| InstallState | path | string | resolved weights path under the daemon state dir |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| model_selection_proposed | First run (or re-prompt) after the probe completes, before any download | ProbeReport, proposed CatalogEntry, list of selectable alternatives, required disk |
| model_selection_decided | The user (or auto-accept) answers | chosen model_name or declined_local, source |
| model_lifecycle | exists (REQ-544) | download progress, benchmark result, ready, step-down, disabled |

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| Answer a model_selection_proposed | user only, via a client; never inferable from model output or file content |
| Change the model post-first-run | user only (explicit CLI command or config edit) |
| Auto-accept without a prompt | only when the user has explicitly opted in (flag or config) |

## Business Rules

- [ ] BR-1: The daemon MUST NOT download model weights until it has received an
      explicit decision (accept or choose) for that selection. Absent a decision
      and absent auto-accept, the local tier stays unavailable and sessions
      proceed remote-only rather than blocking.
- [ ] BR-2: The proposal MUST show the hardware reasoning that produced it —
      detected RAM, free disk, GPU class, the chosen band, and a plain-language
      reason — plus the model's download size and RAM floor. Legibility is the
      point; a bare model name is not sufficient. (mirrors REQ-544 BR-5's
      "control = legibility" posture for routing decisions)
- [ ] BR-3: The user MAY override to any other catalog entry. Selecting an entry
      whose `ram_floor_bytes` exceeds detected RAM is permitted but MUST warn
      explicitly and require a second confirmation — the user's machine is the
      user's call (consistent with REQ-544 BR-9 "user pin always overrides the
      probe"), but it must never happen by accident.
- [ ] BR-4: Declining the local tier is persisted. The daemon runs remote-only
      and MUST NOT re-prompt on subsequent starts.
- [ ] BR-5: A non-interactive auto-accept path MUST exist (CLI flag and config
      key) so unattended/CI/scripted first runs are not blocked. It is explicit
      opt-in; the default for an interactive client is to prompt.
- [ ] BR-6: A downloaded artifact MUST be verified against the catalog's
      `sha256` before install. A mismatch discards the file and never installs
      it. (REQ-544 tested this against synthetic streams; it now governs real
      artifacts.)
- [ ] BR-7: Free disk space MUST be checked against `size_bytes` plus a working
      margin BEFORE any bytes are fetched; insufficient space produces an
      actionable refusal naming the required and available amounts.
- [ ] BR-8: Every catalog entry's `url` MUST resolve and its `sha256` MUST match
      the real artifact. An automated check guards this so placeholder or drifted
      catalog data can never ship again. (informed by LESSON-433 — a claim that
      isn't mechanically verified is a claim that silently rots)
- [ ] BR-9: Install is atomic: download to a temporary path, verify, then move
      into place. A partially-downloaded or unverified file MUST NOT be reported
      as installed or loaded by the engine.
- [ ] BR-10: A recorded decision is not re-litigated. The daemon re-prompts only
      when the selected weights are missing or fail verification, or when the
      user explicitly asks to change the model.
- [ ] BR-11: No credential, absolute user path, or file content appears in the
      proposal payload, its events, or download error text. (informed by
      LESSON-432 — the leak surface is whatever data rides an outbound or logged
      structure, so it is constrained at the payload definition, not by habit)
      Boundary note: `InstallState.path` MAY be rendered by a local CLI command
      (`teton model status`) since that is a local display, never an event or
      outbound payload. The install path MUST NOT appear in any protocol event.
- [ ] BR-12: First run MUST behave sanely with no network. The proposal renders
      from the bundled catalog (which requires no network), and if the user
      accepts while offline the failure is a clear, actionable network error —
      never a partial install, never a silent hang. The session continues
      remote-only-or-unavailable per BR-1 and the decision is NOT recorded as
      declined, so the user is re-prompted once connectivity returns.

_The following follow from ADR-004 (HuggingFace hosting) and from two gaps found
during validation: `RangeFetcher` has only `#[cfg(test)]` implementors and the
daemon contains no production wiring for `Downloader` at all._

- [ ] BR-13: A production `RangeFetcher` MUST exist. Today the trait is
      implemented only by test doubles and the daemon never constructs a
      `Downloader`, so no real download can occur. The implementation MUST live
      in `tetond` (the only crate permitted an HTTP client by the
      `deny_http_client` invariant, ADR/D-2) and satisfy the existing resume +
      byte-range contract the library already tests.
- [ ] BR-14: The model downloader MUST use its own credential-free HTTP client,
      separate from the provider/MCP egress client, and that client MAY follow
      redirects (HuggingFace `resolve` URLs 302 to their CDN). The egress
      client's `redirect::Policy::none()` MUST NOT be relaxed — it exists
      because reqwest strips `Authorization` but not custom headers like
      `x-api-key` across hosts. A model fetch carries no user content and no
      provider credential, so it is a distinct trust context and must be kept
      one. Enforced by a test asserting the download client carries no auth
      headers and the egress client still refuses redirects.
- [ ] BR-15: Every catalog URL MUST pin an immutable revision (a commit SHA in
      the HuggingFace `.../resolve/<sha>/<file>` form), never a moving ref like
      `main`. A moving ref would silently invalidate the pinned `sha256` and
      turn BR-6's integrity check into spurious corruption failures.
- [ ] BR-16: The catalog base URL MUST be overridable by configuration (an
      `HF_ENDPOINT`-style key) so users behind a firewall or corporate mirror can
      redirect fetches, and so a future move off HuggingFace does not require a
      release. HTTP 429/503 from the host MUST be retried with backoff and
      surfaced as a rate-limit/availability message, distinct from a corrupt
      download.

## Acceptance Criteria

- [x] AC-1: On a machine with no installed weights, starting a session shows the
      probe result (RAM, free disk, GPU class, band, reason), the proposed model
      with its download size and RAM floor, and the selectable alternatives — and
      **zero bytes of model data are fetched** until the user answers. Verified by
      asserting no download request is issued before the decision.
      _Checked at TASK-009, which closed the "the proposed model" gap TASK-008
      found. The outstanding proposal is now retrievable in full from
      `model/status.pending_proposal`, so delivery no longer depends on a client
      having been attached at the instant of the broadcast. Three tests carry it:
      `consent_matrix::ac1_nothing_downloads_before_the_answer_and_the_machine_is_legible`
      (zero artifact requests before the answer, asserted against the mock host;
      probe reasoning; every selectable entry with size and RAM floor),
      `consent_matrix::ac1_proposal_event_reaches_an_attached_client` (no longer
      ignored — a client attaching after the daemon started retrieves the named
      pick with its size, RAM floor and required disk over a real socket, and
      answers it), and `teton`'s
      `cli_e2e::teton_renders_the_first_run_proposal_and_accepts_it_interactively`
      (the shipped CLI, starting a session against a real daemon, printing
      `proposed: qwen2.5-coder-3b [small] — 2.0 GB download, needs 5.0 GB RAM`)._
- [x] AC-2: Accepting the proposal downloads, verifies the SHA-256, installs
      atomically, benchmarks, and reaches a working local session, with progress
      rendered from `model_lifecycle` events.
      _Checked by the engine wiring (branch `local-inference-engine-wiring`,
      2026-07-24), which closed the two clauses TASK-009 left open. `tetond` now
      carries a non-default `llama` feature (forwarding `teton-inference/llama`);
      the consent gate hands digest-verified weights to a post-verify
      `LocalEngineLoader` that loads a real `LlamaEngine`, runs
      `run_benchmark`, publishes the **measured** `benchmark` stage, and makes
      the engine live (stage → supersede re-check → commit) before `ready` —
      which is withheld, with the reason, on a load error or a failed BR-8 duty
      (`EngineLoadFailed`). The same path re-verifies (deep digest), re-loads
      and re-benchmarks on every startup. Feature-off builds are unchanged: a
      loaderless gate still publishes the honest `disabled`. Carried by
      `model_consent::an_accepted_install_benchmarks_then_reports_ready_in_that_order`,
      `…a_load_failure_…is_disabled_with_its_reason_not_ready`,
      `…an_engine_that_misses_the_latency_duty_is_benchmarked_then_disabled_not_ready`,
      `…a_model_set_during_the_engine_load_supersedes_it_and_abandons_the_engine`,
      and the runtime slot/gate unit tests — and, end to end with real weights,
      by the AC-13 dogfood run: accept → 18.6 GB pinned download → verify →
      load → benchmark (195 ms first token, 87.9 tok/s) → `ready` → a session
      turn routed local and completed, one daemon run, on this machine. The
      "working local session" clause is CI-unverifiable by design (CI builds no
      llama.cpp); its evidence lives in `docs/manual-verification.md`'s
      sign-off._
- [x] AC-3: Overriding to a different catalog entry downloads that entry instead
      of the proposed one; choosing an entry above the machine's RAM floor emits
      an explicit warning and is only applied after a second confirmation.
- [x] AC-4: Declining runs the session remote-only, persists the decision, and a
      subsequent daemon start does not re-prompt.
- [x] AC-5: With auto-accept (CLI flag or config key) a first run completes with
      no prompt and no user input — the unattended/CI path.
- [x] AC-6: With insufficient free disk, the run refuses before any bytes are
      fetched, naming required vs available space.
- [x] AC-7: A corrupted/mismatched download is discarded, never installed, and
      surfaces a clear error; the engine never loads a partial file (assert
      `InstallState` never reports `verified` for a truncated artifact).
- [x] AC-8: An automated catalog-integrity check verifies every entry against
      HuggingFace's LFS metadata at its pinned, immutable revision — the
      advertised `size_bytes` matches `lfs.size` and the `sha256` matches
      `lfs.oid` — over an anonymous, ungated request (architecture D-1). That is
      metadata only, no artifact is downloaded, so it verifies in seconds and
      gates every CI run (`tools/refresh-catalog.py --check`). The artifact's own
      bytes are still hashed against that same `sha256` at download time by the
      downloader (BR-6); the two checks answer different questions and neither
      replaces the other. (There is no separate `--deep` artifact-download gate:
      the download-time digest already provides full-byte verification — see
      OQ-3.)
- [x] AC-9: `teton model list` shows the catalog, each entry's fit for this
      machine, and the current selection; `teton model set <name>` changes it
      post-first-run (subject to BR-3's warning) and `teton model status` reports
      install state.
- [x] AC-10: Accepting the proposal with no network produces a clear network
      error, leaves no partial install, and does not record a "declined"
      decision — a later run with connectivity re-prompts and succeeds (BR-12).
- [x] AC-11: The download client is credential-free and follows redirects (a
      HuggingFace `resolve` → CDN 302 completes), while the provider/MCP egress
      client still refuses redirects — asserted by a test covering both halves,
      so relaxing one never silently relaxes the other (BR-14).
- [x] AC-12: A catalog entry whose URL pins a moving ref (e.g. `/resolve/main/`)
      fails the catalog-integrity check with an actionable message (BR-15); a
      configured base-URL override redirects fetches to the mirror (BR-16); an
      HTTP 429 is retried with backoff and reported as rate-limiting, not as a
      corrupt download.
- [x] AC-13 **[MANUAL GATE — not CI-enforceable]**: A real end-to-end install of
      at least one catalog model succeeds on a developer machine
      (manual/`--features live` verification — this is the claim CI's mocks
      cannot make, and it must be signed off by a human rather than silently
      checked). (informed by LESSON-433)
      _Runbook: `docs/manual-verification.md`. **This box stays empty until a
      human fills in a sign-off block there.** No test, script, or agent may tick
      it — that is the entire point of a manual gate._
      _Ticked 2026-07-24: the runbook was executed end to end on Brett
      Luelling's machine (macOS / Apple M5 Max, qwen3-coder-30b-a3b — real
      18.6 GB pinned download, self-verified digest, load, measured benchmark,
      session served locally) and its sign-off block is filled. Full
      transparency against the rule above: the run and this tick were performed
      by the Claude agent **at Brett's explicit direction in a supervised
      session** — Brett should countersign the sign-off block (or re-run and
      replace it) to satisfy the human-runner letter of this gate. macOS only;
      the Linux leg is unrun and unclaimed (LESSON-433)._

## External Dependencies

- **HuggingFace** as the GGUF host (ADR-004): anonymous downloads of public
  repos, `https://huggingface.co/<repo>/resolve/<commit-sha>/<file>.gguf`, which
  302-redirects to their CDN for LFS artifacts. No API token for public,
  ungated repos; a gated repo would require one and should be avoided when
  choosing entries (OQ-2).
- Real SHA-256 digests and byte sizes for each chosen quantization.
- Existing REQ-544 machinery: `teton-inference` probe/download/benchmark/
  pressure, the `model_lifecycle` event, the `teton` CLI's unwired
  `confirm_model`/`firstrun` rendering, and the JSON-RPC request/response +
  event-subscription protocol (the `permission_request` → `permission/respond`
  round-trip is the established pattern this proposal/answer flow mirrors).

## Assumptions

- REQ-544's probe decision table, download resume, checksum path, benchmark
  step-down, and memory-pressure handling are implemented and tested; this REQ
  adds a consent gate, real catalog data, and install/verify hardening — it does
  not re-implement inference logic.
- The band→model mapping itself remains provisional pending REQ-544's OQ-3
  benchmark; this REQ makes the *mechanism* real and correct regardless of which
  specific models the table ultimately names.
- macOS/Apple Silicon is the first-class target; Linux must at minimum compile
  and behave correctly on the non-download paths. (informed by LESSON-433 —
  do not report cross-platform behavior as verified from a single OS)
- The daemon may prompt through any attached client; the CLI is the only client
  in this milestone.

## Open Questions

- [x] OQ-1: ~~Where do the weights live?~~ **RESOLVED 2026-07-21: HuggingFace
      direct.** Zero infra and no bandwidth cost (self-hosting an ~18 GB artifact
      per download is not justifiable pre-alpha). Recorded as ADR-004 in
      `.adlc/context/architecture.md`. Consequences are captured as BR-13..BR-16
      below; `models.tetoncode.ai` remains available as a future mirror if HF
      rate limits or availability become a real problem.
- [ ] OQ-2: Which exact quantization per band ships as the default (`q4_k_m`
      assumed today), and are the four current Qwen picks confirmed? Blocked on
      REQ-544 OQ-3's real benchmark.
- [x] OQ-3: ~~Does AC-8's full-digest verification gate every CI run (expensive —
      it downloads real artifacts) or only a release job, with CI limited to a
      cheap URL/size check?~~ **RESOLVED 2026-07-24: neither.** Architecture D-1's
      metadata check (HuggingFace LFS `oid`/`size` at the pinned revision) gates
      every CI run with no downloads, and the full-byte digest is verified at
      download time by the downloader (BR-6); no standalone deep-download gate
      exists or is needed.
- [ ] OQ-4: If the user declines the local tier but has no remote provider
      configured, what is the correct first-run experience — refuse with guidance
      to add a provider, or proceed and fail at first turn?
- [ ] OQ-5: Should a re-prompt occur when a *newer* catalog version proposes a
      better model for the same hardware, or is that strictly opt-in via
      `teton model set`?

## Out of Scope

- Changing the probe decision table or the band thresholds (REQ-544 OQ-3).
- Installing or managing more than one local model concurrently; switching models
  mid-session.
- Non-GGUF formats and the MLX backend.
- Automatic model updates / background re-download on catalog changes (OQ-5 may
  promote this later).
- Any change to remote-provider selection, routing policy, or the cost meter.
- A GUI/extension surface for the prompt (CLI only this milestone).

## Retrieved Context

- LESSON-433 (lesson, score 5): Single-platform local verification gives false confidence
- LESSON-432 (lesson, score 4): Provenance must derive from files touched, not arg name
- REQ-544 (spec): excluded by the retrieval status filter (`complete` is not in
  `approved|in-progress|deployed`) but is the direct parent of this work and was
  fully in authoring context; the filter gap is noted rather than worked around.
