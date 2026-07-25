# Site deploy runbook — `tetoncode.ai`

How `https://tetoncode.ai` gets published, and what has to exist in this
repository before it can be.

The publisher is [`.github/workflows/deploy-site.yml`](../.github/workflows/deploy-site.yml).
It runs on `release: published` and on `workflow_dispatch`, renders
`site/index.html` with the release's version and the install command
(`site/render.sh`, REQ-548 BR-8), and pushes the result to the Atelier Google
Cloud infrastructure.

**Right now it cannot finish.** The GCP coordinates are REQ-548's Open Question
5 and nobody has answered it, so the workflow's last step fails with:

```
site deploy blocked: required GCP configuration is not set in this repository (not set: …)
nothing was published — tetoncode.ai still serves whatever it served before this run.
this step fails on purpose: a green deploy job has to mean the site was actually deployed.
fix: docs/site-deploy-runbook.md
```

That failure is deliberate (ADR-548-3, LESSON-447). The alternative — a skipped
step or a green job that deployed nothing — makes "the deploy is passing" stop
meaning "the site is current", and that is a lie you only discover from a user.
Working through this runbook is what turns it green.

---

## 1. OQ-5 intake

Three answers are needed, and only someone with access to the Atelier GCP org
can give them. Everything else in this runbook follows from them.

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
against evidence rather than memory.

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
| `gcs` | `roles/storage.objectAdmin` **on the bucket only** (not project-wide); plus `roles/compute.loadBalancerAdmin` if `vars.GCP_CDN_URL_MAP` is set, for cache invalidation |
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
| Resolve the release version | `release` event → the event's tag. `workflow_dispatch` → `repos/{repo}/releases/latest`. Exits `65` (release event with no tag), `69` (dispatched with no published release yet), `75` (the lookup failed — not the same as "no release"). |
| Render the landing page | `site/render.sh <tag> "brew install atelier-fashion/tap/teton"`. Exits `64` if a version is malformed or a template placeholder survives; the partial output is deleted, so a page reading `{{VERSION}}` can never reach a deploy step. |
| Upload the rendered site | Always. Artifact `site-dist` is the page that would have gone out — this is how the site is reviewed before any of the above exists. |
| Resolve the GCP deploy configuration | Reads the table in section 2 and records what is missing. Never fails on its own; it only reports. |
| Authenticate / Set up gcloud / Deploy | Run only when the configuration is complete. The deploy step writes a **receipt** as its final action. |
| **Deploy result** | The guard. Exits `0` only on a receipt from a deploy step that ran to completion; otherwise `78` (`EX_CONFIG`) with the reason and this file's path. |

`workflow_dispatch` picks up the *latest published* release, which excludes
drafts and pre-releases. Dispatching after cutting a pre-release republishes the
last stable version, not the pre-release — by design.

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
6. Only then let a release-triggered run happen unattended.

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

**No published release yet (exit `69`)** — the landing page displays a release
version, so there must be a release. Cut one with the release workflow first.

**Nothing is being cleaned up** — the deploy is an rsync **without** delete, on
purpose: `vars.GCP_SITE_BUCKET` may be a prefix in a bucket shared with the rest
of the Atelier site, and no landing-page deploy should be able to remove objects
it did not create. Remove stale objects by hand if the page's asset set ever
shrinks.
