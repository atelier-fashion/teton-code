---
id: BUG-221
title: "The `no room` skill notice carries a run of spaces in the middle of its sentence"
status: resolved
severity: low
created: 2026-09-09
updated: 2026-09-09
resolved: 2026-09-09
component: "teton/session_ui"
domain: "cli"
stack: ["rust"]
concerns: ["developer-experience"]
tags: ["notice", "format-string", "req-618"]
introduced_by: ["REQ-618"]
attribution: manual
---

## Description

`no room: skill `analyze` fits this route's budget (25186 B against 63488 B) but would take more          than 25% of it, leaving the turn nothing to work with` — ten spaces between "more" and "than". Seen 2026-09-09.

## Root Cause

The format string in `format_refused_no_room` (`crates/teton/src/session_ui.rs`) was wrapped across a line during REQ-618 without a `\` continuation, so the indentation became part of the literal.

## Resolution

One line. No test: the sentence's words are pinned elsewhere and a whitespace assertion would pin the accident, not the intent.

## Deployment

- Pending merge.

## Files Changed

- `crates/teton/src/session_ui.rs`
