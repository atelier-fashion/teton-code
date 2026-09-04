---
id: ASSUME-038
title: "The egress choke point covers the credential-bearing files in the drafting evidence table"
status: unresolved
req: REQ-613
created: 2026-09-04
resolved:
---

## Assumption

`Dockerfile`, `docker-compose.yml` and the first 4 KiB of every entry-point file (`main.py`, `index.js`, `main.go` and their kin) are in REQ-613's closed evidence table (ADR-3) although they routinely carry `environment:` values and hard-coded keys. The assumption is that no built-in secret-shape exclusion is needed in the gatherer because two controls already stand between those bytes and a remote provider: the REQ-597 default boundaries (thirteen globs covering the credential-shaped *names*) and REQ-562's redaction inside the egress choke point, which every draft passes through with the evidence's own provenance.

## Context

The Phase 5 security audit raised this as a major finding (finding 3). The table was the product owner's "solid context, not cheap context" choice, and narrowing it would trade the very files that make a draft accurate. A user who keeps secrets in a Dockerfile can add a boundary, and `draft` is a bindable category that can be pinned local. What depends on this: every `generate = always` session, which writes without a human reading the draft first.

## Resolution

To validate: an egress-capture test that plants an AWS-shaped access key and a `-----BEGIN PRIVATE KEY-----` block in a fixture `Dockerfile`, runs the generation pipeline against a recording remote duty, and asserts the redactor rewrote both before the request left the daemon. If REQ-562's pattern set does not cover one of them, this assumption is invalidated and the gatherer needs a built-in secret-shape cut of its own.
