---
id: TASK-024
title: "deploy-site.yml: declare the site-deploy environment"
status: draft
parent: REQ-550
created: 2026-07-31
updated: 2026-07-31
dependencies: []
---

## Description

Declare `environment: site-deploy` on deploy-site.yml's credential-bearing
job so the WIF token exchange and any GCP configuration secrets are gated by
the environment's deployment rules (tags `v*.*.*` + branch `main`), and
document the GCP-side `assertion.ref` hardening as an explicit human step.

## Files to Create/Modify

- `.github/workflows/deploy-site.yml` — add `environment: site-deploy` to the `deploy` job (the one carrying `id-token: write` and the google-github-actions/auth step); comment why (REQ-550 BR-4)
- `docs/site-deploy-runbook.md` — record the environment gating, and add the GCP-side step: add `assertion.ref` (allow `refs/heads/main` and `refs/tags/v*`) to the workload-identity-pool provider's attribute condition — a gcloud console/CLI action the pipeline cannot perform, with the exact gcloud command spelled out

## Acceptance Criteria

- [ ] actionlint clean; the deploy job still runs on push-to-main (environment rule allows `main` — verified in the spec's inventory) and on release-dispatched runs (tag rule)
- [ ] The runbook's `assertion.ref` section is copy-pasteable and marked as REQUIRED-HUMAN with a checkbox
- [ ] No behavior change for the guard/receipt logic — environment declaration only

## Technical Notes

There are no repo-level `GCP_*` secrets to move (spec Verified Inventory);
the WIF provider/service-account identifiers read from `secrets.GCP_*` /
`vars.*` continue to resolve, now environment-scoped where defined. This
task is independent of the signing chain — Tier 1, parallel with TASK-021.
