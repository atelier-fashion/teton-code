---
id: LESSON-469
title: "A green dispatch job is not a live deploy — verify the dispatched run and the endpoint itself"
component: "distribution/release"
domain: "release-engineering"
stack: ["github-actions", "gcs", "cloud-cdn"]
concerns: ["deployment-verification", "async-pipelines", "caching"]
tags: ["workflow-dispatch", "deploy-site", "cdn-ttl", "fresh-fetch", "release-pipeline", "false-green"]
req: none
created: 2026-08-03
updated: 2026-08-03
---

## What Happened

The v0.1.4 release run finished all-green, including its
"publish tetoncode.ai" job — but the live site still served the v0.1.3 page.
The publish job only **dispatches** `deploy-site.yml` as a separate workflow
run (deliberately, so the page never claims a version brew can't serve yet);
its success means "dispatch accepted", not "site deployed". The dispatched run
was still executing, and the page also sits behind a 5-minute CDN TTL
(`cache-control: public,max-age=300`). The user saw the stale page and
reasonably asked whether the deploy was broken.

## Lesson

Treat `workflow_dispatch` as fire-and-forget: completion of the *caller* says
nothing about the *callee*. Verification of a dispatched deploy requires
(1) watching the dispatched run itself to conclusion, and (2) fetching the
live endpoint fresh and checking a content marker that only the new version
carries (here: the version string and a phrase unique to the new page) —
CDN headers (`age:`, `last-modified:`) tell you whether you're seeing cache
or origin.

## Why It Matters

"Successful release with stale public state" is a confusing half-truth: the
tap serves new binaries while the site contradicts them. Reporting "site
updated" off the caller's green job is a false claim; the gap here was long
enough for the user to notice before the deploy landed.

## Applies When

Any pipeline where one workflow dispatches another (site publishes, downstream
repo bumps, fan-out deploys); any verification step for CDN-fronted content;
writing status summaries that claim something is "live".
