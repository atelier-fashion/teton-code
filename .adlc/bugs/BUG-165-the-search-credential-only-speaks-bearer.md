---
id: BUG-165
title: "The search credential only speaks Bearer, and the spec's own example backends do not"
status: open
severity: medium
created: 2026-08-13
updated: 2026-08-13
component: "daemon/egress"
domain: "harness"
stack: ["rust", "daemon", "keychain"]
concerns: ["security", "developer-experience"]
tags: ["web-search", "auth", "config", "req-563", "keychain", "brave", "kagi"]
---

## Description

REQ-563's search tier resolves `[web] search_key_ref` from the OS keychain and
attaches it as `Authorization: Bearer <secret>` — hardcoded in
`DaemonRuntime::search_auth` (`crates/tetond/src/runtime.rs`), whose doc
comment reasons that "`Authorization: Bearer` is what an unblessed one is most
likely to accept."

But the REQ's own External Dependencies section names Brave Search API and
Kagi as the example backends, and neither authenticates with Bearer:

- Brave expects `X-Subscription-Token: <key>` — a different header entirely.
- Kagi expects `Authorization: Bot <key>` — the right header, a different
  scheme word.

A user who configures either of the spec's named examples, exactly as
documented, gets a 401 on every search. The failure is silent at config time
(the config validates cleanly) and misleading at use time — the HTTP-status
ending looks like a bad key, not a wrong header shape, so the natural next
move is to re-cut the key, which changes nothing.

## Reproduction Steps

1. Store a valid Brave Search API key in the keychain and configure:
   `[web] tier = "search"`,
   `search_endpoint = "https://api.search.brave.com/res/v1/web/search"`,
   `search_key_ref = "keychain:brave-search"`.
2. Restart the daemon, grant the search tier at the consent prompt.
3. Ask anything that triggers a search.

## Expected Behavior

The search succeeds: the resolved key rides the header shape the configured
backend actually accepts.

## Actual Behavior

Every search ends as an HTTP-status ending: the request carries
`Authorization: Bearer <key>`, Brave ignores it (it wants
`X-Subscription-Token`) and answers 401/422. The same happens for Kagi, which
wants `Authorization: Bot <key>`.

## Environment

- Platform: all
- Version: workspace 0.1.14 (present since REQ-563 landed in 0.1.13)

## Root Cause

There is no blessed search backend (BR-8), so `search_auth` had to assume a
header shape — and it assumed exactly one, with no way to configure another.
The assumption ("Bearer is what an unblessed backend is most likely to
accept") is drawn from the OpenAI-compatible provider world
(`provider_auth_headers` picks headers by `ProviderKind`), but search
backends have no `kind`, and the commercial search APIs the spec itself names
happen to be the counterexamples.

The gap is a *missing degree of freedom*, not a wrong constant: any fixed
header shape loses to some legitimate backend, so the shape has to be
user-configurable — while keeping BR-7/BR-8 intact (the secret itself lives
in the keychain; config stores only references).

Verified secondary points:

- `search_request` (`crates/tetond/src/egress/lookup.rs`) builds the GET with
  `q=<query>` and attaches **no** credential; the header rides the
  endpoint-bound transport built by `search_auth` → `for_lookup_with_endpoint_auth`,
  origin-bound per REQ-544 M-3. So the fix is confined to the shape
  `search_auth` builds; the transport binding machinery is already general.
- A self-hosted SearxNG instance (the spec's third example, with
  `format=json`) needs no key at all and already works: an absent
  `search_key_ref` builds a credential-free transport. The `search()` seam
  deliberately applies no address-class floor to the configured endpoint, so
  `search_endpoint = "http://localhost:8888/search"` is a supported
  configuration (its doc comment says so in as many words), and IP-literal
  endpoints never reach the DNS screen. Only a non-localhost *name* that
  resolves to a non-global address is refused by `GlobalOnlyResolver` — the
  narrowing already recorded in the REQ-563 architecture Deviations. The
  originally-suspected "localhost search endpoints are SSRF-refused" claim is
  **false**; no doc correction on that point is needed beyond what the
  architecture already records.

## Resolution

New optional key: `[web] search_auth` — a template for the one header the
credential rides, with `{key}` marking where the resolved secret goes:

```toml
search_auth = "X-Subscription-Token: {key}"   # Brave's shape
search_auth = "Authorization: Bot {key}"      # Kagi's shape
```

Absent, it means what it always meant: `Authorization: Bearer {key}` — no
existing configuration changes behavior.

Shape over enum, template over key-pair, deliberately:

- An enum of backend names would bless backends (BR-8 forbids exactly that).
- A `search_auth_header` + `search_auth_scheme` pair needs a conditional
  default (Bearer for `authorization`, nothing otherwise) — cross-key magic.
- The template is one key whose config spelling mirrors the wire header, and
  `{key}` keeps the secret out of config *by shape*: validation refuses a
  template without the placeholder, the same stance the `keychain:`/`env:`
  reference DSL already takes for `auth_ref`.

Parsing lives in `teton-core` as one function used by both
`Config::validate` (fail-closed, didactic errors) and the accessor the daemon
reads, so the two cannot disagree. Validation requires the value to be
`Header-Name: {key}` or `Header-Name: Scheme {key}` (header name an RFC 7230
token, scheme a single token), and rejects `search_auth` beside an absent
`search_key_ref` — a shape describing how a credential rides, with no
credential named, is a config the daemon would silently ignore.
`DaemonRuntime::search_auth` renders the shape with the resolved secret; a
shape that fails to parse at use time (unvalidated config) attaches no
credential and says so on stderr, the same fail-closed posture as an
unresolvable `search_key_ref`.

## Files Changed

- `crates/teton-core/src/config.rs` — `WebConfig::search_auth` field,
  `SearchAuthShape` + `parse_search_auth`, two `ConfigError` variants,
  `validate_web` rules, tests.
- `crates/tetond/src/runtime.rs` — `search_auth` renders the configured
  shape; doc comment rewritten; tests for both named example shapes.
- `crates/tetond/tests/web_lookup_egress.rs` — wire-level assertion that a
  configured shape reaches the socket.
- `CHANGELOG.md` — `[Unreleased]` entry.
- `.adlc/specs/REQ-563-opt-in-web-lookup/requirement.md` — `search_auth`
  entity row + External Dependencies amendment (marked).
- `.adlc/specs/REQ-563-opt-in-web-lookup/architecture.md` — Deviations note.
