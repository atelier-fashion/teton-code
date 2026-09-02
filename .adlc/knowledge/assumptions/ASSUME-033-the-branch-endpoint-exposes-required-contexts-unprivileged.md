---
id: ASSUME-033
title: "The public branch endpoint keeps exposing required contexts to an unprivileged token"
status: validated
req: REQ-608
created: 2026-09-02
resolved: 2026-09-02
---

## Assumption

`GET /repos/{owner}/{repo}/branches/{branch}` returns
`protection.required_status_checks.contexts` for a public repository to a
caller holding no admin scope — anonymously, and to the workflow's own
`GITHUB_TOKEN` at `permissions: contents: read`. REQ-608's parity check rests
entirely on this read path; no `administration: read` and no PAT is
provisioned.

## Context

The spec's first draft assumed the opposite — that reading protection needed a
widened permission or a standing secret (OQ-3, BR-10). Measuring it at Step 0
of `/proceed` dissolved the question: the branch endpoint answered
unauthenticated with the six contexts, while `/branches/main/protection`
answered 401. Every architectural choice downstream (own job with no
`permissions:` override, `GITHUB_TOKEN` used for rate limit only, no secret to
rotate) depends on GitHub continuing to expose that field on that endpoint for
public repositories. GitHub's REST reference marks the `protection` object on
this endpoint as present for public repositories; a private repository would
need push access, which the workflow token has.

## Resolution

Validated 2026-09-02 three ways: an unauthenticated `curl` returned the
contexts; the parity job passed on PR #271's runs and on `main` after the
merge, reading with `GITHUB_TOKEN`; and the local check flipped from exit 1 to
exit 0 across the protection edit. **If this ever stops holding**, the failure
is loud and repo-wide, not silent: the check's BR-5 contract turns a 401/403 or
a missing `protection` object into exit 75 ("UNCHECKED — nothing was
learned") on every PR, naming the URL. The remedy at that point is the
job-scoped token ADR-608-2 declined to add, and the reason it was declined
(nothing needed widening) should be re-read before adding it.
