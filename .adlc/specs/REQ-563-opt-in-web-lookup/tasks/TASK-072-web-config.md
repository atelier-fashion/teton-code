---
id: TASK-072
title: "WebConfig: [web] section with tier, endpoint, allowlist, cache TTL"
status: complete
parent: REQ-563
created: 2026-08-08
updated: 2026-08-08
dependencies: []
---

## Description

Add the `[web]` config section to `teton-core` per architecture D-9: tier is
the single source of truth (`off` = disabled — no separate `enabled` bool),
search configuration is keychain-referenced, and the domain allowlist is an
optional constraint on model-chosen destinations.

## Files to Create/Modify

- `crates/teton-core/src/config.rs` — add `WebConfig { tier: WebTier, search_endpoint: Option<String>, search_key_ref: Option<String>, allowed_domains: Option<Vec<String>>, cache_ttl_secs: u64 (default 900) }` with `WebTier` enum `Off | FetchUserUrl | FetchAnyUrl | Search` (ordered; serde `snake_case`, default `Off`); wire into `Config` as `#[serde(default, skip_serializing_if = ...)] web` following the `PrivacyConfig` pattern (its module comment explains the own-table placement — mirror that rationale); extend `Config::validate()`.

## Acceptance Criteria

- [x] `WebTier` is ordered (`Off < FetchUserUrl < FetchAnyUrl < Search`) and `tier.allows(WebTier::X)` expresses the each-tier-includes-lower rule (spec BR-3).
- [x] Default config deserializes with `tier = Off`; a config file with no `[web]` table round-trips without emitting one.
- [x] `validate()`: `tier = search` without `search_endpoint` is a config error naming the missing field (spec BR-8/AC-7); `search_key_ref` must look like a keychain reference and never a raw credential (reuse the existing `auth_ref` validation shape and its no-echo rule — config.rs:8-17); `allowed_domains` entries are charset-checked (`[A-Za-z0-9.*-]`, no scheme, no path, no `..`).
- [x] Unit tests cover: tier ordering, default-off, search-without-endpoint error, credential-shaped `search_key_ref` rejected without echoing the value, allowlist charset rejection.

## Technical Notes

- Follow `PrivacyConfig` (config.rs:80-126) for placement, serde attrs, and the
  "written-out config states posture" comment style.
- Error messages never echo credential-shaped values (charter BR-7).
- `cache_ttl_secs` default 900; zero is valid (= no caching).
