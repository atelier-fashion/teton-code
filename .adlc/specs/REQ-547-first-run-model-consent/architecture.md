# REQ-547 — Architecture: first-run model consent + real catalog

Parent decisions: ADR-001 (Rust), ADR-002 (JSON-RPC over UDS), ADR-004
(HuggingFace hosting) in `.adlc/context/architecture.md`. This document covers
the REQ-level design and the task decomposition.

## Approach

Two changes to the existing REQ-544 machinery, which already implements the
probe, the resumable/verified download library, the benchmark and the
step-down chain:

1. **Insert a consent gate** between probe and download. The daemon proposes;
   the client answers; only then does anything download. Modeled directly on
   the existing `permission_request` → `permission/respond` round-trip.
2. **Make the catalog real** — actual HuggingFace repos, pinned revisions, and
   true digests — plus the production HTTP fetcher that today does not exist.

## Key decisions (REQ-level)

### D-1: Digests come from HuggingFace LFS metadata, never from downloading

`GET https://huggingface.co/api/models/<repo>/tree/<revision>` returns, per
file, `lfs.oid` (the **SHA-256** of the artifact) and `lfs.size`. Verified
against `Qwen/Qwen2.5-Coder-1.5B-Instruct-GGUF`: 8 GGUF quants, each with a
usable oid+size.

**Consequences** (both significant):
- Authoring real catalog digests is an API query, not a 1–18 GB download.
- The BR-8/AC-8 catalog-integrity check is **one cheap API call per entry**
  comparing catalog `sha256`/`size_bytes` against `lfs.oid`/`lfs.size`. It
  therefore **can gate every CI run** — this **resolves OQ-3** (the spec assumed
  full-digest verification meant downloading artifacts and might have to be
  release-only). A true byte-level digest is still computed at download time by
  the existing verifier (BR-6); the CI check verifies the *catalog* is honest,
  which is the actual failure mode being guarded.

### D-2: Two HTTP clients, two trust contexts

| Client | Credentials | Redirects | Purpose |
|---|---|---|---|
| Egress (`egress::HttpTransport`) | endpoint-bound auth headers | **refused** | provider + MCP traffic |
| Model download (new) | **none, ever** | **followed** | GGUF artifacts from HF → CDN |

Both live in `tetond` (the `deny_http_client` invariant). The egress client's
`redirect::Policy::none()` is **not** relaxed — it exists because reqwest strips
`Authorization` but not custom headers like `x-api-key` across hosts. A model
fetch carries no user content and no credential, so it is a genuinely different
trust context and gets its own client. A test asserts both postures together so
relaxing one can never silently relax the other (BR-14, AC-11).

### D-3: Consent is a protocol round-trip that gates the tier, not the session

The daemon emits `model_selection_proposed` and awaits `model/confirm`. While
awaiting, **sessions still work** — they run remote-only (BR-1). The local tier
simply stays unavailable until answered. This avoids the failure mode where a
user who ignores the prompt has a dead tool.

### D-4: The decision is machine state, not project config

A `ModelSelection` record (accepted / declined / chosen, source, timestamp)
persists in the daemon state directory beside the weights. The user-authored
TOML holds only *inputs* — a pinned model, the base-URL override, auto-accept.
Rationale: "which model this machine installed" is not a property of a project,
and REQ-544 already sites machine state in the daemon dir. This also makes
BR-10's "don't re-litigate a decision" a simple state read.

## Task breakdown — dependency tiers

| Tier | Tasks |
|---|---|
| 0 | TASK-001 protocol + config surface |
| 1 | TASK-002 production download client · TASK-003 real catalog + revision pinning |
| 2 | TASK-004 consent gate + decision persistence · TASK-005 preflight + atomic install |
| 3 | TASK-006 catalog integrity check · TASK-007 CLI consent/override/model commands |
| 4 | TASK-008 acceptance suite + manual-gate runbook |

## AC coverage map

| AC | Covered by |
|---|---|
| AC-1 nothing downloads before an answer | 004 (+008) |
| AC-2 accept → download/verify/install/benchmark | 002, 005 (+008) |
| AC-3 override incl. above-RAM-floor double-confirm | 007 (+004) |
| AC-4 decline persists, no re-prompt | 004 |
| AC-5 auto-accept (CI path) | 004, 007 |
| AC-6 disk preflight refusal | 005 |
| AC-7 corrupt discarded, never installed | 005 |
| AC-8 catalog integrity in CI | 006 |
| AC-9 `teton model list/set/status` | 007 |
| AC-10 offline accept → clean error, not declined | 004 (+008) |
| AC-11 two-client posture (credential-free + redirects) | 002 |
| AC-12 moving-ref rejected, base-URL override, 429 backoff | 003, 006 |
| AC-13 real install **[MANUAL GATE]** | 008 (runbook only — human sign-off) |

## Proposed additions to `.adlc/context/architecture.md`

ADR-004 already records the hosting decision. D-1 (LFS-metadata digests) is
worth promoting to context level at wrapup if it proves out — it is the reason
the integrity check is cheap enough to be a CI gate.
