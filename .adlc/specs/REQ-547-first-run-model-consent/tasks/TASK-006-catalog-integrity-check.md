---
id: TASK-006
title: "Catalog integrity check (CI-gating, via HF LFS metadata)"
status: draft
parent: REQ-547
created: 2026-07-21
updated: 2026-07-21
dependencies: [TASK-003]
---

## Description

BR-8/AC-8. Guarantee the shipped catalog is honest — the failure this REQ exists
to prevent from recurring. Cheap enough to gate every CI run because it uses the
LFS metadata API rather than downloading artifacts (D-1). **Resolves OQ-3.**

## Files to Create/Modify

- `crates/teton-inference/tests/catalog_integrity.rs` — for each entry: resolve the URL, fetch `GET /api/models/<repo>/tree/<revision>`, assert `lfs.oid == sha256` and `lfs.size == size_bytes`, assert the revision is a pinned 40-hex SHA, assert the repo is public/ungated (anonymous request succeeds)
- `.github/workflows/ci.yml` — run it; mark network-dependent so a transient HF outage reports distinctly from a genuine mismatch

## Acceptance Criteria

- [ ] A deliberately corrupted digest in the catalog fails the check (mutation-verified: change one hex char, observe red, revert — report that you did this)
- [ ] A moving-ref revision (`main`) fails with an actionable message (AC-12 partial)
- [ ] A network/API failure is reported as *unverified*, NOT as a mismatch — an outage must never look like corruption, and must not silently pass either
- [ ] Runs in CI in well under a minute for the full catalog (no artifact downloads)
- [ ] Documented: this verifies the **catalog is honest**; byte-level artifact verification still happens at download time (BR-6)

## Technical Notes

This is the mechanized answer to "placeholder data shipped once already." Keep
the failure messages specific enough that a future drift (repo renamed, file
removed, revision GC'd) is immediately diagnosable rather than a generic 404.
