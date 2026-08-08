---
id: TASK-074
title: "tetond web module: cache, deterministic reduction, user-URL set, allowlist matcher"
status: complete
parent: REQ-563
created: 2026-08-08
updated: 2026-08-08
dependencies: ["TASK-072"]
---

## Description

The daemon-side support pieces (architecture D-3/D-4/D-9): a local-only
content-addressed cache, a dependency-free HTML→text extractor, the
per-session verbatim-URL tracker for the user-pasted tier, and the domain
allowlist matcher.

## Files to Create/Modify

- `crates/tetond/src/web/mod.rs` — module root wiring the pieces below.
- `crates/tetond/src/web/cache.rs` — `WebCache`: content-addressed by SHA-256 of the normalized URL under the daemon data dir (`web-cache/` beside the ledger DB); entry = reduced text + `fetched_at` + ttl; `get()` returns only fresh entries; `put()`, `evict(url)` (the explicit-refresh path); files created 0600; never synced or egressed.
- `crates/tetond/src/web/reduce.rs` — `reduce(html: &str, cap_bytes: usize) -> String`: strip `<script>`/`<style>` blocks and all tags, decode the basic entities (`&amp; &lt; &gt; &quot; &#39; &nbsp;`), collapse whitespace runs, hard byte cap on a char boundary. Pure function, no new crate dependencies.
- `crates/tetond/src/web/user_urls.rs` — `UserUrls`: per-session set fed by a conservative URL extractor (`https?://` runs up to whitespace/`>`/`"`); `contains(url)` after the same normalization the cache uses.
- `crates/tetond/src/web/allowlist.rs` — `DomainAllowlist::matches(host)`: exact host or `*.suffix` wildcard per config patterns; no scheme/path logic.

## Acceptance Criteria

- [x] Cache: fresh hit returns content without touching the network layer (pure function of the store); stale/absent returns None; `evict` removes; `cache_ttl_secs = 0` disables persistence entirely; entry files are 0600.
- [x] `reduce`: script/style content NEVER appears in output; tags stripped; entities decoded; output ≤ cap and cuts on a char boundary (multi-byte UTF-8 test); deterministic (same input → same output).
- [x] `UserUrls`: URL pasted in a user prompt is found verbatim after normalization; a URL differing in host, path, or query is NOT found (spec BR-3's "not one the model composed").
- [x] `DomainAllowlist`: exact and wildcard matches pass; sibling-domain and suffix-trick hosts (`evil-example.com` vs `example.com`, `example.com.evil.net`) fail.
- [x] Unit tests for every bullet above, including the suffix-trick negative cases.

## Technical Notes

- URL normalization (shared by cache + user-URL set): lowercase scheme+host,
  strip fragment, keep path/query byte-exact. One helper, one definition.
- The reduce cap is enforced AFTER all transforms (LESSON-491 — enforce at the
  last transform); pick the constant so a capped reduction plus envelope
  overhead stays inside the tool-result budget used by `summarize_if_large`.
- No new dependencies — the extractor is deliberately conservative; fidelity
  beyond "readable text" is out of scope (spec Out of Scope: no JS rendering).
