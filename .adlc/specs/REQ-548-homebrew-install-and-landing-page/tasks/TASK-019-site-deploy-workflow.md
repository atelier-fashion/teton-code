---
id: TASK-019
title: "Site deploy workflow (Atelier GCP) with loud degradation until secrets exist"
status: draft
parent: REQ-548
created: 2026-07-25
updated: 2026-07-25
dependencies: [TASK-018]
repo: teton-code
---

## Description

`deploy-site.yml`: on release-published and on `workflow_dispatch`, render
the site with the current release version and deploy to the Atelier GCP
infrastructure (user-confirmed hosting; exact surface + project id are OQ-5).
Until the GCP secrets are configured, the deploy step fails with a named,
::warning-annotated reason instead of green-while-deploying-nothing
(ADR-548-3, LESSON-447). A runbook documents the two likeliest GCP shapes
(Cloud Run vs Cloud Storage + LB) and the DNS/TLS steps for `tetoncode.ai`.

## Files to Create/Modify

- `.github/workflows/deploy-site.yml` — triggers: `release: [published]`, `workflow_dispatch`; steps: resolve version (release tag or latest release for dispatch), `site/render.sh`, auth via `google-github-actions/auth` with workload-identity inputs from secrets, deploy step selected by a `GCP_DEPLOY_SURFACE` repo variable (`cloud-run` | `gcs`); when required secrets/variables are unset, exit with a named failure ("site deploy blocked: configure GCP_* secrets — see docs/site-deploy-runbook.md"), never a silent success
- `docs/site-deploy-runbook.md` — OQ-5 intake (which surface serves the Atelier site, project id, region), secrets/variables to set (`GCP_WIF_PROVIDER`, `GCP_SERVICE_ACCOUNT`, `GCP_PROJECT`, `GCP_DEPLOY_SURFACE`, surface-specific target), DNS + managed-TLS steps for `tetoncode.ai` per surface, first-deploy human confirmation step, and the AC-7 verification checklist (HTTPS serves, version matches latest release)

## Acceptance Criteria

- [ ] Workflow YAML parses; the deploy job's secret-absence path is an explicit failing step with the runbook pointer in its message (grep-testable), not `continue-on-error` and not a skip
- [ ] Both trigger paths resolve a version: `release.published` uses the event tag; `workflow_dispatch` queries the latest release and fails loudly when none exists yet
- [ ] The render step reuses `site/render.sh` (no duplicated substitution logic)
- [ ] The runbook enumerates every secret/variable name the workflow reads — one source of truth, no drift (grep the workflow for `secrets.` / `vars.` and cross-check)
- [ ] No GCP credentials, project ids, or org-specific values are hardcoded anywhere in the workflow (placeholders + secrets only)

## Technical Notes

- Deploy surfaces: `cloud-run` → build a trivial nginx/static container? NO —
  keep dependency-free: for `cloud-run` use `gcloud run deploy` with the
  source-based static-site pattern only if the Atelier site already does so;
  otherwise `gcs` → `gcloud storage rsync site/dist gs://<bucket>` behind the
  existing LB. The runbook's OQ-5 intake decides which path gets exercised;
  both are written, one is selected by `GCP_DEPLOY_SURFACE`.
- Workload identity federation preferred over SA keys; the runbook documents
  both, recommends WIF (matches modern GCP posture; no long-lived key in
  GitHub).
- First actual deploy is human-confirmed (infra rule) — the workflow existing
  and being runnable is this task's deliverable; executing it against the
  real project is the runbook's step with the user in the loop.
