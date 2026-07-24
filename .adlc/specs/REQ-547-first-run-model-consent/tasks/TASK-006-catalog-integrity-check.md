---
id: TASK-006
title: "Catalog integrity check (CI-gating, via HF LFS metadata)"
status: complete
parent: REQ-547
created: 2026-07-21
updated: 2026-07-23
dependencies: [TASK-003]
---

## Description

BR-8/AC-8. Guarantee the shipped catalog is honest — the failure this REQ exists
to prevent from recurring. Cheap enough to gate every CI run because it uses the
LFS metadata API rather than downloading artifacts (D-1). **Resolves OQ-3.**

## Files to Create/Modify

- `crates/teton-inference/tests/catalog_integrity.rs` — the **structural half**: hermetic, no network, runs in every `cargo test`. Pinned 40-hex revisions agreeing with the URL, HuggingFace/TLS host, no userinfo credentials, no placeholder markers or copy-pasted digests/URLs/sizes, `ram_floor > size`, moving-ref rejection against the real committed file, and a guard that the upstream half stays wired into CI
- `tools/refresh-catalog.py` — the **upstream half**: `--check` now asserts the pin is 40-hex *before* any network call, asserts the repo is public/ungated anonymously (a gated repo serves metadata but refuses weights, so the flags are read explicitly), and compares `sha256`/`size_bytes` field-by-field against `lfs.oid`/`lfs.size` with a per-field diagnostic, on top of the existing byte-identity check. Exit codes are a taxonomy: 0 verified / 1 mismatch / 75 unverified
- `.github/workflows/ci.yml` — a dedicated `catalog` job that maps those exit codes to `::notice::` / `::error::` / `::warning::` so an outage can never be read as corruption

Split rationale: `teton-inference` is deliberately transport-free, and TASK-003 already ships a reproducible generator. Re-implementing the fetch in Rust would duplicate the generator and add an HTTP client to a crate that has none; wiring the tool in is cleaner. What stays in Rust is exactly what must be deterministic and offline.

## Acceptance Criteria

- [x] A deliberately corrupted digest in the catalog fails the check (mutation-verified: change one hex char, observe red, revert — report that you did this)
- [x] A moving-ref revision (`main`) fails with an actionable message (AC-12 partial)
- [x] A network/API failure is reported as *unverified*, NOT as a mismatch — an outage must never look like corruption, and must not silently pass either
- [x] Runs in CI in well under a minute for the full catalog (no artifact downloads) — 1.7 s for 4 entries / 8 API calls
- [x] Documented: this verifies the **catalog is honest**; byte-level artifact verification still happens at download time (BR-6)

## Mutation verification (performed 2026-07-23)

| Mutation | Result |
|---|---|
| `qwen2.5-coder-3b` sha256 `…730b7` → `…730b6` | `--check` exit **1**, `MISMATCH qwen2.5-coder-3b sha256` naming both values |
| Same, structural test only | **passes** — proving why the network half must exist (asserted as a test) |
| `qwen2.5-coder-1.5b` revision + URL → `main` | exit **1**, moving-ref message; still exit 1 *during a simulated outage* |
| API host → unroutable, catalog untouched | exit **75** `UNVERIFIED`, explicitly disclaims corruption |
| URL host → `models.example.com`; size → `2_100_000_000` | structural test red on both |

Every mutation was reverted and the file confirmed byte-identical (`shasum -c` + clean `git diff`).

## Technical Notes

This is the mechanized answer to "placeholder data shipped once already." Keep
the failure messages specific enough that a future drift (repo renamed, file
removed, revision GC'd) is immediately diagnosable rather than a generic 404.
