---
id: TASK-003
title: "Real catalog: HF repos, pinned revisions, true digests, base-URL override"
status: complete
parent: REQ-547
created: 2026-07-21
updated: 2026-07-23
dependencies: [TASK-001]
---

## Description

Replace the placeholder catalog with real HuggingFace entries. Digests come from
the LFS metadata API (D-1) — no multi-GB download required.

## Files to Create/Modify

- `crates/teton-inference/data/models.toml` — real entries: for each band, an actual HF repo + a **pinned commit SHA** revision + `sha256` (= `lfs.oid`) + `size_bytes` (= `lfs.size`) + `ram_floor_bytes`. Remove the placeholder banner.
- `crates/teton-inference/src/catalog.rs` — parse/expose `revision`; add `validate()` rejecting a moving ref (`/resolve/main/`, any non-40-hex revision) with an actionable message (BR-15); resolve a configured base-URL override (BR-16)
- `tools/refresh-catalog.rs` (or a documented `xtask`/script) — queries `GET /api/models/<repo>/tree/<revision>` and emits the TOML rows, so refreshing is reproducible rather than hand-typed

## Acceptance Criteria

- [x] Every entry names a real, public, **ungated** HF repo and pins a 40-hex commit SHA (never `main`)
- [x] Each entry's `sha256`/`size_bytes` equal the repo's `lfs.oid`/`lfs.size` for that exact file+revision
- [x] `validate()` rejects a moving-ref URL and a non-hex/short revision with a clear message (AC-12 partial)
- [x] A configured base-URL override rewrites the host while preserving repo/revision/file path (BR-16)
- [x] The refresh tool regenerates the committed TOML byte-identically from the live API (proves the data is derived, not hand-edited)
- [x] Tests: revision validation table, base-URL rewrite, catalog parse round-trip

## Technical Notes

Verified working: `GET https://huggingface.co/api/models/Qwen/Qwen2.5-Coder-1.5B-Instruct-GGUF/tree/main`
returns per-file `lfs.oid` (SHA-256) and `lfs.size`. Prefer official `Qwen/*-GGUF`
repos; if a band has no official GGUF, pick a well-known quantizer repo and note
the choice in a comment. Model picks remain provisional pending REQ-544 OQ-3 —
this task makes the *data pipeline* real, not the final picks.
