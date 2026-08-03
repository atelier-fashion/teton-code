---
id: ASSUME-001
title: "Hosted-runner cert import suffices for signing; brew-only distribution keeps notarization out of scope"
status: validated
req: REQ-550
created: 2026-07-31
resolved: 2026-08-03
---

## Assumption

Two load-bearing assumptions from REQ-550's spec: (1) the Developer ID
signing step can run on GitHub-hosted macOS runners via a base64 P12 in an
environment secret imported into a throwaway keychain — no self-hosted
runner, no Apple notarization service dependency; (2) distribution stays
brew-only, so Gatekeeper quarantine — and therefore notarization — is not
load-bearing, and signing alone delivers the Keychain-identity stability
users need.

## Context

Made at spec time (2026-07-31) with no prior signing infrastructure in the
project. REQ-550's architecture (ephemeral keychain, ADR-550-2), REQ-551's
restructuring, and the deferral of notarization (OQ-3, user-confirmed) all
depend on these holding.

## Resolution

VALIDATED by production evidence, 2026-08-03: releases v0.1.2 and v0.1.3
both signed successfully on hosted macos-15 runners (import → sign →
early-destroy), with per-release smoke gates asserting the Developer ID
signature and team id on every macOS artifact. The user-visible contract
followed: AC-6 human-verified — a Keychain grant given under v0.1.2
survived the upgrade to v0.1.3 with no re-prompt, with no notarization
anywhere in the chain. Brew-only distribution remains true (the landing
page carries the brew command only). Re-open only if direct downloads
enter scope (REQ-548 OQ-4) — that invalidates assumption (2) and brings
notarization in.
