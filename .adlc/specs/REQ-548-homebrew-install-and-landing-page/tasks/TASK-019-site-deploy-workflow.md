---
id: TASK-019
title: "Site deploy workflow (Atelier GCP) with loud degradation until secrets exist"
status: complete
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

- [x] Workflow YAML parses; the deploy job's secret-absence path is an explicit failing step with the runbook pointer in its message (grep-testable), not `continue-on-error` and not a skip
- [x] Both trigger paths resolve a version: `release.published` uses the event tag; `workflow_dispatch` queries the latest release and fails loudly when none exists yet
- [x] The render step reuses `site/render.sh` (no duplicated substitution logic)
- [x] The runbook enumerates every secret/variable name the workflow reads — one source of truth, no drift (grep the workflow for `secrets.` / `vars.` and cross-check)
- [x] No GCP credentials, project ids, or org-specific values are hardcoded anywhere in the workflow (placeholders + secrets only)

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

## Verification (local, 2026-07-25)

- `actionlint` (with shellcheck) clean on `.github/workflows/deploy-site.yml`;
  the file parses as YAML; `bash -n` clean on all 7 `run:` bodies.
- The guard step's `run` body was extracted and executed standalone in four
  states. Blocked in all three failure states — exit **78** (`EX_CONFIG`),
  `::warning title=Site deploy blocked`, and `docs/site-deploy-runbook.md` in
  the message: (a) env fully unset, (b) env as the workflow sets it with no GCP
  config, (c) a resolved surface whose deploy step left no receipt. Positive
  control: a receipt present ⇒ exit 0 with the `Site deployed` notice.
- Secret/variable cross-check: the workflow references 9 `secrets.*`/`vars.*`
  names; the runbook's table lists the same 9. Symmetric difference empty in
  both directions.
- The render step body was run against the real `site/render.sh` — `v0.1.0`
  renders with the tap install command and zero surviving `{{` placeholders;
  a malformed tag maps render.sh's 64 to the named `Render REFUSED` `::error`.

## Deviations

- The guard's message is **"site deploy blocked"** (task file's wording), not
  ADR-548-3's illustrative "site deploy skipped". "Skipped" would misdescribe a
  step that deliberately fails, which is the whole point of the guard.
- The guard asserts a **deploy receipt**, not merely secret presence: it fails
  whenever no deploy step ran to completion, so a bogus `GCP_DEPLOY_SURFACE`
  value or a half-finished deploy also fails loudly rather than passing.
- Two names beyond the task file's sketch: `secrets.GCP_CREDENTIALS_JSON` (so
  the documented SA-key fallback is real rather than prose-only) and the
  optional `vars.GCP_CDN_URL_MAP` (without CDN invalidation, AC-7 is false at
  the URL while the job is green — the workflow `::warning`s when it is unset).
- The Cloud Run leg uses `gcloud run deploy --source site/dist` (buildpacks).
  Whether the Atelier site builds that way is OQ-5 intake question 1; the
  runbook names the exact two-line change if it uses a Dockerfile instead.
