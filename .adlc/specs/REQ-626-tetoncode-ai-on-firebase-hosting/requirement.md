---
id: REQ-626
title: "tetoncode.ai on Firebase Hosting — retire the global load balancer that fronted one static page"
status: complete
deployable: true
created: 2026-10-06
updated: 2026-10-07
component: "site"
domain: "release-engineering"
stack: ["github-actions", "firebase-hosting", "gcs"]
concerns: ["cost", "availability", "deployment"]
tags: ["landing-page", "firebase", "load-balancer", "dns", "deploy-surface", "REQ-548", "REQ-550"]
---

## Description

tetoncode.ai is a single rendered `index.html`. Under REQ-548's `gcs` surface it sat in a
Cloud Storage bucket behind a global external HTTPS load balancer with Cloud CDN. The
2026-10-06 billing audit (the Atelier billing-export table, 30 days) showed what that
costs for what it does:

| Line | 30-day |
|---|---|
| Load balancer forwarding rule (flat, hourly) | $17.75 |
| CDN lookups (~48k requests), egress, storage, logs — everything else | < $0.10 |

~$213/yr of fixed cost for a page whose actual traffic costs pennies. Firebase Hosting
serves the same page with Google-managed TLS, a global CDN and the `www` → apex redirect
for free, in the same GCP project, with the same WIF identity.

## Business Rules

- [x] **BR-1** — The page served does not change. The Firebase release is the same
      `site/dist/index.html` the renderer produces (BR-8 of REQ-548 still stamps version +
      install command at deploy time); the initial cut-over release is byte-identical to
      the object the bucket served.
- [x] **BR-2** — `deploy-site.yml` gains a third surface, `firebase`, selected by
      `vars.GCP_DEPLOY_SURFACE` like the other two; the `gcs` and `cloud-run` surfaces are
      left intact (ADR-548-3's "the surface is configuration, not code").
- [x] **BR-3** — The guard semantics hold (LESSON-447): the `firebase` step writes its
      receipt only after the live default URL is observed to carry the released tag. A CLI
      "Deploy complete" without the page at the URL is a red job.
- [x] **BR-4** — No new secrets. The surface needs `secrets.GCP_PROJECT` and the existing
      WIF pair; the project's default Hosting site is used, so no site id (which equals
      the project id, a secret by ADR-548-3) is written into the public repo.
- [x] **BR-5** — The CI service account gets `roles/firebasehosting.admin` and
      `roles/serviceusage.serviceUsageConsumer`, project-level; `roles/compute.loadBalancerAdmin`
      is removed once the LB is gone.
- [x] **BR-6** — Cut-over order: Firebase release live on the default URL → domains
      registered → DNS flipped at the registrar → `certState: CERT_ACTIVE` → **then** the
      LB stack is deleted. The old surface keeps serving until the new one is proven.
- [x] **BR-7** — Runbook §2/§3/§4 are updated in the same PR (the runbook's own rule:
      every `vars.*` the workflow reads appears in the table, and vice versa).

## Acceptance Criteria

- [x] AC-1 — `https://<project>.web.app/` serves the current page (15,341 bytes, v0.1.36)
      — *done 2026-10-06, release 1791320051901000, before this PR*.
- [x] AC-2 — `actionlint` (with shellcheck) passes on `deploy-site.yml`; the repo's
      required checks are green.
- [x] AC-3 — With `GCP_DEPLOY_SURFACE=firebase`, a `workflow_dispatch` of `deploy-site.yml`
      goes green with the notice `Site deployed: Firebase Hosting … published with teton v0.1.36`.
- [x] AC-4 — After the DNS flip: `tetoncode.ai` and `www.tetoncode.ai` return 200 over
      HTTPS from Firebase (`server: Google Frontend`, cert issued to tetoncode.ai), and
      `www` redirects to the apex.
- [x] AC-5 — `gcloud compute forwarding-rules list --project <project>` is empty *(2026-10-07)*;
      the next billing month has no `Cloud Load Balancer Forwarding Rule` SKU for the project
      *(follow-up check, early November)*.

## External Dependencies

- GoDaddy DNS for tetoncode.ai (operator action: one `A`, one `TXT`, one `CNAME`).

## Assumptions

- A TLS gap of minutes between the `A` flip and certificate issuance is acceptable for a
  landing page. Firebase's zero-downtime "live migration" was attempted and refused —
  it needs the *old* host to answer an HTTP ACME challenge, which the bucket-backed LB
  does not.

## Open Questions

- [x] None.

## Out of Scope

- `tetoncode.com`, which resolves elsewhere and is not served from this project.
- Deleting the `tetoncode-ai` bucket (free while idle; keep as the `gcs` fallback).

## Verification record (2026-10-06 → 2026-10-07)

| Step | Evidence |
|---|---|
| Code | [#335](https://github.com/atelier-fashion/teton-code/pull/335) squash-merged; all 8 required checks green |
| AC-1 | Firebase release `1791320051901000` on the default site, 15,341 bytes, byte-identical to the bucket object |
| AC-3 | `GCP_DEPLOY_SURFACE=firebase`; [deploy-site run 37531304754](https://github.com/atelier-fashion/teton-code/actions/runs/37531304754) green, `web.app` serving v0.1.36 |
| DNS | GoDaddy (driven via Claude in Chrome, each save re-read from the table): `A @ → 199.36.158.100`, `TXT @ hosting-site=<project>`, `CNAME www → <project>.web.app`, plus the two DNS-01 challenge TXTs Firebase offered (`_acme-challenge`, `_acme-challenge.www`) because its HTTP-01 check kept hitting the old LB through Google's resolver cache (1 h TTL) |
| Certs | both customDomains `HOST_ACTIVE / OWNERSHIP_ACTIVE / cert CERT_ACTIVE` (2026-10-07 morning) |
| AC-4 | `https://tetoncode.ai/` → 200 from 199.36.158.100, cert verifies; `https://www.tetoncode.ai/` → 301 → apex, cert verifies |
| Teardown (BR-6 order) | 2026-10-07: deleted `teton-site-https-fr`, `teton-site-http-fr`, `teton-site-https`, `teton-site-http`, `teton-site-lb`, `teton-site-redirect`, `teton-site-cert`, `teton-site-backend`, `teton-site-ip`; all seven compute resource lists empty; bucket `tetoncode-ai` kept |
| BR-5 | `roles/compute.loadBalancerAdmin` removed from the CI SA (now only `firebasehosting.admin` + `serviceusage.serviceUsageConsumer`); `vars.GCP_CDN_URL_MAP` deleted (pointed at the deleted URL map) |

Lesson worth keeping: GoDaddy's DNS form ignores programmatic (DOM-level) field fills — the Save button stays disabled and a click is a silent no-op; values must be typed as keystrokes. The first TXT "save" produced no record until retried that way, which is why every save here was confirmed by re-reading the records table.
