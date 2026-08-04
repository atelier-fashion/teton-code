# Site deploy runbook — `tetoncode.ai`

How `https://tetoncode.ai` gets published, and what has to exist in this
repository before it can be.

The publisher is [`.github/workflows/deploy-site.yml`](../.github/workflows/deploy-site.yml).
It renders `site/index.html` with the release's version and the install command
(`site/render.sh`, REQ-548 BR-8) and pushes the result to the Atelier Google
Cloud infrastructure.

**How it gets triggered.** A release run does not *notify* this workflow — it
**dispatches** it. [`release.yml`](../.github/workflows/release.yml) calls
`deploy-site.yml` itself, once `bump-formula` has pushed the tap successfully.
There is no `release: published` subscription doing the work, because there
could not be: `release.yml` creates the release with the default `GITHUB_TOKEN`,
and GitHub does not raise workflow-triggering events for anything that token
does. A workflow subscribed to `release: published` would never have run after a
single release, and would have looked fine the whole time.

Three consequences worth holding on to:

- The site deploy is **strictly after** the tap bump, never beside it
  (ADR-548-3a). The page advertises a version *and* the `brew install` that
  fetches it; both are only true once the tap points at the new release.
- If `bump-formula` fails, the site is **not** deployed. That is correct, not a
  gap: the page still on tetoncode.ai matches the version the tap still serves.
- The dispatched run takes the `workflow_dispatch` path below, so it reads its
  version from `repos/{repo}/releases/latest` rather than from an event payload.

`deploy-site.yml` does still carry a `release: published` trigger, but only as a
fallback for a release cut **by a human through the UI or by a PAT** — those do
raise the event. Nothing in the release pipeline depends on it. And you can
always run the workflow by hand (Actions → Deploy site → Run workflow) for a
site copy fix or an infrastructure change.

**This is live.** The GCP coordinates were REQ-548's Open Question 5; it was
answered 2026-08-01 (§1), and the site has deployed for real on every release
since **v0.1.5**. `GCP_PROJECT`, `GCP_SERVICE_ACCOUNT` and `GCP_WIF_PROVIDER`
are secrets on the `site-deploy` environment, with no repository-level copy of
any of them.

The blocked path below is still reachable and still deliberate — it is what you
get if that configuration goes missing, and what every release through v0.1.4
got:

```
site deploy blocked: required GCP configuration is not set in this repository (not set: …)
nothing was published — tetoncode.ai still serves whatever it served before this run.
this step fails on purpose: a green deploy job has to mean the site was actually deployed.
fix: docs/site-deploy-runbook.md
```

That failure is deliberate (ADR-548-3, LESSON-447). The alternative — a skipped
step or a green job that deployed nothing — makes "the deploy is passing" stop
meaning "the site is current", and that is a lie you only discover from a user.
Seeing it **today** means the configuration was removed or the environment's
deployment rules refused the ref; it is a finding, not the starting state.

---

## 1. OQ-5 intake — **answered 2026-08-01**

The three questions below are **closed**. Their answers are recorded, redacted,
against OQ-5 in
[REQ-548's requirement file](../.adlc/specs/REQ-548-homebrew-install-and-landing-page/requirement.md):
surface `gcs` (Cloud Storage behind an external HTTPS load balancer with Cloud
CDN, §4's resource names), a dedicated GCP project whose id lives only in the
`GCP_PROJECT` secret, and workload identity federation as the CI deploy
identity — with the provider's attribute condition already restricted to this
repository and to `refs/heads/main` / `refs/tags/v*`, which closes §9's
REQUIRED-HUMAN item.

They are kept here rather than deleted because they are the questions to
re-answer if the site ever moves surfaces or projects — and because the
redaction rule below (LESSON-462) governs how the answers get recorded next
time too.

**Q1. Which GCP surface serves the Atelier website today?**
`cloud-run`, or Cloud Storage behind an external load balancer (`gcs`)? The
landing page follows whatever the existing site already does — a static page
fits either, and matching the existing pattern means one TLS/DNS story to
operate instead of two.

> If the answer is `cloud-run`, also answer: **how is that container built?**
> The workflow's Cloud Run step uses `gcloud run deploy --source site/dist`,
> which builds with Google Cloud buildpacks. If the Atelier site ships a
> Dockerfile instead, add the equivalent Dockerfile to `site/` and change that
> step's `--source` to `site`. It is a two-line change, called out here so it
> is a decision made before the first run rather than a surprise during it.

**Q2. Which GCP project id hosts it?** This becomes `secrets.GCP_PROJECT`.

**Q3. Which region (Cloud Run) or which bucket / path prefix (GCS)?** And, for
GCS: is there a Cloud CDN in front, and if so what is the URL map called?

Record the answers in REQ-548's requirement file when they land, so OQ-5 closes
against evidence rather than memory. **Record them redacted** (LESSON-462):
this is a public repository, and ADR-548-3 keeps the project id out of it on
purpose. Write *where each value lives* ("project id: repo secret
`GCP_PROJECT`"), not the value itself — inline only names this runbook already
uses in its own commands, and nothing else identifying (project id/number, IPs,
service-account emails).

---

## 2. Secrets and variables

Exactly what the workflow reads — no more, no less. Every name below appears in
`deploy-site.yml`, and every `secrets.*` / `vars.*` reference in
`deploy-site.yml` appears below. If you add one to either, add it to both.

| Name | Kind | Required when | Example shape |
|------|------|---------------|---------------|
| `secrets.GCP_PROJECT` | secret | always | `atelier-prod-1234` |
| `secrets.GCP_WIF_PROVIDER` | secret | always, unless using a key | `projects/123456789/locations/global/workloadIdentityPools/github/providers/github` |
| `secrets.GCP_SERVICE_ACCOUNT` | secret | always, unless using a key | `teton-site-deploy@PROJECT_ID.iam.gserviceaccount.com` |
| `secrets.GCP_CREDENTIALS_JSON` | secret | only as the not-recommended fallback to the two above | the full contents of a service-account key JSON |
| `vars.GCP_DEPLOY_SURFACE` | variable | always | `cloud-run` or `gcs` |
| `vars.GCP_RUN_SERVICE` | variable | surface is `cloud-run` | `teton-site` |
| `vars.GCP_RUN_REGION` | variable | surface is `cloud-run` | `us-central1` |
| `vars.GCP_SITE_BUCKET` | variable | surface is `gcs` | `tetoncode-ai` or `atelier-sites/teton` |
| `vars.GCP_CDN_URL_MAP` | variable | optional; surface is `gcs` behind Cloud CDN | `atelier-sites-lb` |

Notes on the two that surprise people:

- **`GCP_PROJECT` is a secret, not a variable.** A project id is not a
  credential, but it names Atelier's infrastructure in a public repository's
  logs. ADR-548-3 calls it a secret; keep it one. The cost is that it appears
  masked (`***`) in deploy logs — if you are debugging a `gcloud` error and the
  project is starred out, that is why.
- **`GCP_CDN_URL_MAP` is optional but load-bearing for AC-7.** With a CDN in
  front and no invalidation, the deploy job goes green while the edge keeps
  serving the previous version — exactly the "green means nothing" failure the
  rest of this design refuses. The workflow emits a `::warning` on every run
  where the variable is unset, so the gap stays visible instead of silent. Set
  it if a CDN exists.

  **The invalidation is scoped to the objects this deploy writes** — `/` plus
  every file the renderer produced under `site/dist` (today, just
  `/index.html`), enumerated at deploy time and issued one `--path` at a time.
  It is deliberately **not** `--path '/*'`. This URL map may be the one fronting
  the whole Atelier site; a landing-page release has no business evicting every
  other property's cache behind it, and the cost of a full flush is paid by
  services that had nothing to do with the release. If you point
  `GCP_CDN_URL_MAP` at a shared load balancer, that is fine and expected — the
  scoping is what makes it fine. If the page ever gains assets, they are picked
  up automatically; nobody has to remember to widen a path list.

Setting them (`gh` CLI, from a checkout of this repository):

```sh
gh secret set GCP_PROJECT           # paste the project id
gh secret set GCP_WIF_PROVIDER      # paste the full provider resource name
gh secret set GCP_SERVICE_ACCOUNT   # paste the service-account email

gh variable set GCP_DEPLOY_SURFACE --body 'gcs'          # or 'cloud-run'
gh variable set GCP_SITE_BUCKET    --body 'tetoncode-ai' # gcs
# gh variable set GCP_RUN_SERVICE  --body 'teton-site'   # cloud-run
# gh variable set GCP_RUN_REGION   --body 'us-central1'  # cloud-run
# gh variable set GCP_CDN_URL_MAP  --body 'atelier-sites-lb'  # gcs + Cloud CDN
```

Do not set these until someone is ready to watch the first run — see
section 6.

---

## 3. Authentication

### Workload identity federation (recommended)

GitHub's OIDC token is exchanged for a short-lived GCP credential at run time.
Nothing long-lived is stored in this repository, so there is no key to leak,
rotate, or forget to revoke when a maintainer leaves. `deploy-site.yml` already
requests `id-token: write` for this.

```sh
PROJECT_ID=…            # secrets.GCP_PROJECT
PROJECT_NUMBER=$(gcloud projects describe "$PROJECT_ID" --format='value(projectNumber)')
SA="teton-site-deploy@${PROJECT_ID}.iam.gserviceaccount.com"

gcloud iam service-accounts create teton-site-deploy \
  --project="$PROJECT_ID" \
  --display-name='tetoncode.ai site deploy (GitHub Actions)'

gcloud iam workload-identity-pools create github \
  --project="$PROJECT_ID" --location=global \
  --display-name='GitHub Actions'

# The attribute condition is the security boundary. WITHOUT IT, ANY repository
# on GitHub can mint tokens for this service account. Do not omit it, and do not
# widen it to the whole org unless every repository in the org should be able to
# deploy the site.
gcloud iam workload-identity-pools providers create-oidc github \
  --project="$PROJECT_ID" --location=global \
  --workload-identity-pool=github \
  --issuer-uri='https://token.actions.githubusercontent.com' \
  --attribute-mapping='google.subject=assertion.sub,attribute.repository=assertion.repository' \
  --attribute-condition="assertion.repository == 'atelier-fashion/teton-code'"

gcloud iam service-accounts add-iam-policy-binding "$SA" \
  --project="$PROJECT_ID" \
  --role=roles/iam.workloadIdentityUser \
  --member="principalSet://iam.googleapis.com/projects/${PROJECT_NUMBER}/locations/global/workloadIdentityPools/github/attribute.repository/atelier-fashion/teton-code"

# The value for secrets.GCP_WIF_PROVIDER:
gcloud iam workload-identity-pools providers describe github \
  --project="$PROJECT_ID" --location=global \
  --workload-identity-pool=github --format='value(name)'
```

Roles the service account needs, by surface — grant the narrowest that works:

| Surface | Roles |
|---------|-------|
| `gcs` | `roles/storage.objectAdmin` **and** `roles/storage.legacyBucketReader`, both **on the bucket only** (not project-wide); plus `roles/compute.loadBalancerAdmin` if `vars.GCP_CDN_URL_MAP` is set, for cache invalidation. `legacyBucketReader` is not optional: `gcloud storage rsync` calls `storage.buckets.get`, which `objectAdmin` does not carry — the first configured deploy run failed on exactly this |
| `cloud-run` | `roles/run.admin`; `roles/iam.serviceAccountUser` on the Cloud Run runtime service account; `roles/cloudbuild.builds.editor` and `roles/artifactregistry.writer` for `--source` builds |

### Service-account key (fallback, not recommended)

Use only if the org cannot stand up WIF.

```sh
gcloud iam service-accounts keys create /tmp/teton-site-key.json \
  --iam-account="$SA" --project="$PROJECT_ID"
gh secret set GCP_CREDENTIALS_JSON < /tmp/teton-site-key.json
shred -u /tmp/teton-site-key.json   # or `rm -P` on macOS
```

What you are accepting by doing this: the key never expires; anyone with admin
on this repository can read it out by adding a workflow; and revoking it means
remembering it exists. Many GCP orgs block it outright with
`constraints/iam.disableServiceAccountKeyCreation`. The workflow accepts the key
and prints a `::warning` naming this cost on **every** run that uses it, so the
fallback cannot quietly become the permanent arrangement. Delete the secret once
WIF is in place.

---

## 4. DNS and managed TLS for `tetoncode.ai`

DNS for `tetoncode.ai` is REQ-548's OQ-1 — find the registrar and the
authoritative nameservers before starting. Both surfaces below need a DNS record
pointing at Google **before** a Google-managed certificate will provision;
issuance typically takes 15–60 minutes after the record resolves, and the
certificate sits in `PROVISIONING` until then. That wait is normal, not a fault.

### Surface `gcs` — bucket behind an external HTTPS load balancer

```sh
# 1. Bucket, public-read, index.html as both entry point and 404 page.
gcloud storage buckets create "gs://${BUCKET}" \
  --project="$PROJECT_ID" --location=US --uniform-bucket-level-access
gcloud storage buckets update "gs://${BUCKET}" \
  --web-main-page-suffix=index.html --web-error-page=index.html
gcloud storage buckets add-iam-policy-binding "gs://${BUCKET}" \
  --member=allUsers --role=roles/storage.objectViewer

# 2. Static IP + backend bucket (CDN on).
gcloud compute addresses create teton-site-ip --global
gcloud compute backend-buckets create teton-site-backend \
  --gcs-bucket-name="$BUCKET" --enable-cdn

# 3. URL map (its name is vars.GCP_CDN_URL_MAP), cert, proxy, forwarding rule.
gcloud compute url-maps create teton-site-lb --default-backend-bucket=teton-site-backend
gcloud compute ssl-certificates create teton-site-cert \
  --domains=tetoncode.ai --global
gcloud compute target-https-proxies create teton-site-https \
  --url-map=teton-site-lb --ssl-certificates=teton-site-cert
gcloud compute forwarding-rules create teton-site-https-fr \
  --global --target-https-proxy=teton-site-https \
  --address=teton-site-ip --ports=443

# 4. HTTP → HTTPS redirect (separate url map + http proxy on port 80).
gcloud compute url-maps import teton-site-redirect --global --source=- <<'EOF'
name: teton-site-redirect
defaultUrlRedirect:
  httpsRedirect: true
  redirectResponseCode: MOVED_PERMANENTLY_DEFAULT
EOF
gcloud compute target-http-proxies create teton-site-http --url-map=teton-site-redirect
gcloud compute forwarding-rules create teton-site-http-fr \
  --global --target-http-proxy=teton-site-http \
  --address=teton-site-ip --ports=80
```

DNS: an `A` record for `tetoncode.ai` pointing at the reserved global IP
(`gcloud compute addresses describe teton-site-ip --global --format='value(address)'`).
Add `AAAA` too if you reserved an IPv6 address. Then:

```sh
gcloud compute ssl-certificates describe teton-site-cert --global \
  --format='value(managed.status,managed.domainStatus)'
# ACTIVE / tetoncode.ai=ACTIVE  → done.
```

Set `vars.GCP_SITE_BUCKET` to `$BUCKET` and `vars.GCP_CDN_URL_MAP` to
`teton-site-lb`.

### Surface `cloud-run` — Cloud Run service

Two ways to attach the domain; pick the one the Atelier site already uses.

**a. Cloud Run domain mapping** — simplest, but not available in every region
and it requires the domain to be verified in Google Search Console first.

```sh
gcloud beta run domain-mappings create \
  --project="$PROJECT_ID" --region="$REGION" \
  --service="$SERVICE" --domain=tetoncode.ai
gcloud beta run domain-mappings describe \
  --project="$PROJECT_ID" --region="$REGION" --domain=tetoncode.ai \
  --format='value(status.resourceRecords)'
```

Create exactly the `A`/`AAAA` records it prints. TLS is provisioned and renewed
by Google once they resolve.

**b. External HTTPS load balancer with a serverless NEG** — more moving parts,
same TLS story as the `gcs` surface, and the right choice if Atelier already
runs one LB for everything.

```sh
gcloud compute network-endpoint-groups create teton-site-neg \
  --region="$REGION" --network-endpoint-type=serverless --cloud-run-service="$SERVICE"
gcloud compute backend-services create teton-site-backend --global
gcloud compute backend-services add-backend teton-site-backend \
  --global --network-endpoint-group=teton-site-neg --network-endpoint-group-region="$REGION"
```

Then the URL map / certificate / proxy / forwarding-rule steps from the `gcs`
section, with `--default-service=teton-site-backend` instead of
`--default-backend-bucket`.

Set `vars.GCP_RUN_SERVICE` and `vars.GCP_RUN_REGION`.

---

## 5. What the workflow does, step by step

| Step | Behaviour |
|------|-----------|
| Resolve the release version | `workflow_dispatch` (including the dispatch from `release.yml`) → `repos/{repo}/releases/latest`. `release` event, i.e. the human/PAT fallback → the event's tag. Exits `65` (release event with no tag), `69` (dispatched with no published release yet), `75` (the lookup failed — not the same as "no release"). |
| Render the landing page | `site/render.sh <tag> "brew install atelier-fashion/tap/teton"`. Exits `64` if a version is malformed or a template placeholder survives; the partial output is deleted, so a page reading `{{VERSION}}` can never reach a deploy step. |
| Upload the rendered site | Always. Artifact `site-dist` is the page that would have gone out — this is how the site is reviewed before any of the above exists. **Ordered before the auth step on purpose**: `google-github-actions/auth` writes a `gha-creds-*.json` credential into the workspace, and this step runs while that file does not yet exist. Do not move it down. |
| Resolve the GCP deploy configuration | Reads the table in section 2 and records what is missing. Never fails on its own; it only reports. |
| Authenticate / Set up gcloud / Deploy | Run only when the configuration is complete. The deploy step writes a **receipt** as its final action. On `gcs`, that includes the scoped CDN invalidation (section 2). |
| **Deploy result** | The guard. Exits `0` only on a receipt from a deploy step that ran to completion; otherwise `78` (`EX_CONFIG`) with the reason and this file's path. |

`workflow_dispatch` picks up the *latest published* release, which excludes
drafts and pre-releases. Dispatching after cutting a pre-release republishes the
last stable version, not the pre-release — by design.

**The guard names the right cause.** Because `Deploy result` runs on every
non-cancelled outcome, it can be reached on a run where the configuration step
never executed — a version resolution that exited `69`/`75`, or a render that
exited `64`, stops the job before it. On those runs every `steps.config.*`
output is empty, and a guard reading only those outputs would announce
*"required GCP configuration is not set … (no deploy surface was resolved)"* for
a run that never looked at the configuration at all. It does not: it checks
whether the configuration step ran before it interprets that step's silence, and
says **"an earlier step in this job failed before the deploy configuration was
ever read"** instead. If you see that message, the fix is the red step above it —
this run tells you nothing about your secrets either way.

---

## 6. The first deploy is human-confirmed

Do not set the secrets and variables and then walk away. The first real deploy
writes to Atelier infrastructure, and nobody has watched this workflow touch it
yet.

1. Complete sections 1–4. Do **not** set the repository secrets/variables yet.
2. Run the workflow once *unconfigured* (Actions → Deploy site → Run workflow).
   Expect: green through the render, `site-dist` artifact attached, and a red
   `Deploy result` step saying `site deploy blocked`. Download the artifact and
   read the page — version, install command, no `{{` anywhere.
3. Now set the secrets and variables from section 2.
4. Dispatch again, **watching the run**. Read the `gcloud` output. Confirm the
   `Deploy result` step prints a `Site deployed` notice naming what went where.
5. Run the AC-7 checklist in section 7.
6. Only then let a release run dispatch this workflow unattended.

If step 4 fails partway, the site is unchanged or partially updated — check
section 8 before re-running.

---

## 7. AC-7 verification checklist

> AC-7: `https://tetoncode.ai` serves the landing page over HTTPS with the
> overview and the install command; the displayed version matches the latest
> release at deploy time (BR-8).

Run these from a machine outside GCP, after a deploy:

```sh
# 1. HTTPS, 200, HTML.
curl -sSI https://tetoncode.ai | head -5

# 2. Displayed version == latest published release.
site_version="$(curl -fsS https://tetoncode.ai | grep -Eom1 'v[0-9]+\.[0-9]+\.[0-9]+')"
release_tag="$(gh release view --json tagName --jq .tagName)"
printf 'site=%s release=%s\n' "$site_version" "$release_tag"
[ "$site_version" = "$release_tag" ] && echo 'AC-7 version: OK'

# 3. The install command is the tap invocation, verbatim.
curl -fsS https://tetoncode.ai | grep -F 'brew install atelier-fashion/tap/teton'

# 4. No unrendered placeholder reached production.
curl -fsS https://tetoncode.ai | grep -F '{{' && echo 'FAIL: placeholder leaked'

# 5. Plain HTTP redirects to HTTPS.
curl -sSI http://tetoncode.ai | head -3

# 6. The certificate is Google-managed, valid, and covers the apex.
echo | openssl s_client -connect tetoncode.ai:443 -servername tetoncode.ai 2>/dev/null \
  | openssl x509 -noout -issuer -subject -dates
```

Also confirm, in the GitHub run: the `Deploy result` step is **green** and its
notice names the surface and the version. A green job with no such notice is not
possible by construction — if you see one, that is a bug in the guard and it
matters more than whatever you were doing.

Tick AC-7 only when 1–6 pass *and* someone loaded the page in a browser: the
checks above prove bytes and TLS, not that the page reads correctly.

---

## 8. Troubleshooting

**`site deploy blocked: required GCP configuration is not set …`** — expected
until section 2 is done. The message lists the exact names that are unset. This
is the workflow working, not failing.

**`site deploy blocked: an earlier step in this job failed before the deploy
configuration was ever read`** — do **not** start checking secrets. Something
above the guard went red: the version resolution (`65`/`69`/`75`) or the render
(`64`). Scroll up, fix that, re-run. This run learned nothing about your GCP
configuration, and the guard says so rather than blaming the settings it never
looked at.

**`site deploy blocked: the deploy-configuration step itself did not succeed`**
— the `Resolve the GCP deploy configuration` step failed while running, which is
different from it reporting things missing. Read that step's log; the settings
may well be fine.

**`vars.GCP_DEPLOY_SURFACE is 'cloudrun', which is neither cloud-run nor gcs`**
— the value is normalised (lowercased, whitespace stripped) but not guessed at.
Set it to exactly `cloud-run` or `gcs`.

**`the 'gcs' deploy step left no receipt`** — the deploy started and did not
finish. The bucket may be partially updated. Read the failed step's `gcloud`
output, fix the cause, and re-run; the deploy is an rsync + overwrite, so
re-running is safe and idempotent.

**Permission denied from `gcloud`** — the service account is missing a role from
section 3, or the WIF attribute condition does not match this repository. The
`auth` step succeeding proves identity, not authorization; those fail
differently and at different steps.

**The site serves the old version after a green deploy** — a CDN is in front and
`vars.GCP_CDN_URL_MAP` is unset. The run's `::warning` says so. Set the variable
and re-run; `index.html` is uploaded with `max-age=300`, so an unset CDN
invalidation self-corrects within minutes on the bucket's own caching but not
necessarily at the edge.

If the variable *is* set and the edge is still stale, check which paths the run
invalidated — the receipt names the count, and the step logs one
`invalidate-cdn-cache --path …` line per path. The invalidation covers `/` and
each file under `site/dist` (section 2); a URL serving the page from some other
path (a `/teton/` prefix on a shared load balancer, say) is not covered, and
that is a URL-map routing question rather than a workflow bug.

**No published release yet (exit `69`)** — the landing page displays a release
version, so there must be a release. Cut one with the release workflow first.
This is also what a dispatch from `release.yml` would hit if it somehow ran
before the release existed; it cannot, because the dispatch happens after
`bump-formula`, which runs after the `release` job publishes.

**Nothing is being cleaned up** — the deploy is an rsync **without** delete, on
purpose: `vars.GCP_SITE_BUCKET` may be a prefix in a bucket shared with the rest
of the Atelier site, and no landing-page deploy should be able to remove objects
it did not create. Remove stale objects by hand if the page's asset set ever
shrinks.

---

## 9. Which refs may deploy (REQ-550 BR-4)

Two gates, on two sides of the token exchange. Both are needed, and only one of
them lives in this repository.

### The GitHub side — done

The `deploy` job declares `environment: site-deploy`. GitHub will not hand that
job an OIDC token, or resolve any secret or variable scoped to the environment,
unless the run's ref satisfies the environment's deployment rules:

| Rule | Value |
|------|-------|
| Protected tags | `v*.*.*` |
| Deployment branch | `main` |

The environment and those rules were created on **2026-07-31**. Nothing in this
repository creates them — the workflow only *declares* the name, and a run on a
ref the rules reject fails to start the job rather than deploying from it.

Both real entry points already satisfy the rules, which is why this is a gate
and not a change in behaviour:

- A release-driven run. `release.yml`'s `dispatch-site-deploy` job runs
  `gh workflow run deploy-site.yml --ref "$TAG"`, so the run's ref is
  `refs/tags/vX.Y.Z` — matched by the tag rule.
- A hand-run from Actions → Deploy site → Run workflow, which defaults to
  `main` — matched by the branch rule.

A run dispatched from a topic branch is the case this closes. It used to be able
to mint a GCP credential and publish to tetoncode.ai; now it cannot reach the
auth step at all.

### The GCP side — REQUIRED-HUMAN

The environment rules stop GitHub from *issuing* the token. They do not stop
Google from *accepting* one. The workload identity pool provider decides that,
and its attribute condition today (section 3) constrains only the repository:

```
assertion.repository == 'atelier-fashion/teton-code'
```

Any ref of this repository — any branch, any fork-free push that can start a
workflow — still satisfies it. Closing that means adding `assertion.ref` to the
condition, and it is a `gcloud`/console action against the Atelier GCP org that
no workflow in this repository can perform.

- [x] **Restrict the WIF provider to release refs.** Done 2026-08-01: the
      provider was created with its attribute condition already restricted to
      `atelier-fashion/teton-code` and refs `refs/heads/main` /
      `refs/tags/v*` (recorded against OQ-5 in REQ-548's requirement file).
      Copy-paste template kept for re-creation — substitute `PROJECT_ID`, and
      the pool/provider names if section 3's defaults (`github`/`github`)
      were not used:

```sh
PROJECT_ID=…            # secrets.GCP_PROJECT
POOL=github             # --workload-identity-pool from section 3
PROVIDER=github         # the provider created by `providers create-oidc`

gcloud iam workload-identity-pools providers update-oidc "$PROVIDER" \
  --project="$PROJECT_ID" --location=global \
  --workload-identity-pool="$POOL" \
  --attribute-condition="assertion.repository == 'atelier-fashion/teton-code' && (assertion.ref == 'refs/heads/main' || assertion.ref.startsWith('refs/tags/v'))"
```

`--attribute-condition` **replaces** the whole condition rather than appending
to it, so the repository clause is restated above on purpose. Dropping it while
adding the ref clause would let every repository on GitHub mint tokens from
`main` — a strictly worse position than before the change.

Verify the condition that is actually live, not the one you meant to set:

```sh
gcloud iam workload-identity-pools providers describe "$PROVIDER" \
  --project="$PROJECT_ID" --location=global \
  --workload-identity-pool="$POOL" \
  --format='value(attributeCondition)'
```

Then dispatch Deploy site once at a release tag and confirm it still reaches the
`Deploy result` step green. If the auth step fails after this change, the
condition is wrong — read it back with the command above before touching
anything in this repository. A rejected token fails at
`google-github-actions/auth`, which is identity, not the missing-role
authorization failure described in section 8.


### Secret scoping (completed 2026-08-03)

`GCP_PROJECT`, `GCP_SERVICE_ACCOUNT`, `GCP_WIF_PROVIDER` live as
**environment secrets on `site-deploy`** (names only here — the values are
GCP-side identifiers, recoverable from the project itself, never committed
to this public repo). Repository-level copies were deleted after the
environment copies were verified, and environment-only resolution was
proven by a green `main`-dispatched deploy (run 30835455409). The
repository holds no repo-scoped Actions secrets at all; every credential
resolves through an environment whose deployment rules gate the ref
(REQ-550 BR-4).
