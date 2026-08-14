---
id: ASSUME-007
title: "toml_edit's round-trip preserves the config schema's constructs"
status: validated
req: REQ-574
created: 2026-08-14
resolved: 2026-08-14
---

## Assumption

`toml_edit`'s format-preserving round-trip (comments, key order, whitespace
for untouched keys) holds for every construct the Teton config actually uses —
tables, arrays of tables, inline tables, dotted keys — so a delta engine built
on it can honor BR-1's "a write touches only its keys" promise.

## Context

REQ-574's whole design rests on this: the delta engine (teton-core
`config_doc`) applies semantic edits to the parsed document and writes the
re-rendered text. If the parser normalized untouched content, preservation
would be unachievable at any diff granularity.

## Resolution

Validated, with three recorded caveats that shipped as documented limitations
rather than blockers (ADR-1 "Recorded limitations", module docs, and unit
tests pinning each):

1. **CRLF normalizes on the first write** — toml_edit re-renders line endings
   as `\n`; content, comments, and order survive, but a CRLF-authored file is
   rewritten once.
2. **Sections render by numeric position, not by tree** — an array element's
   sub-tables are parented by whichever `[[…]]` header precedes them, so any
   inserted section must be positioned against the *whole* array including
   nested sub-tables (`last_render_position`), or documents stop parsing. Two
   defects of this class were caught and fixed in verify (REQ-574, commit
   b11de9e).
3. **An inline-spelled array of tables cannot hold section edits** — the key
   is rewritten into `[[…]]` blocks, with its comment carried onto the first
   header.

A 200k-iteration structured fuzz over malformed/CRLF/multi-byte/inline
documents produced zero panics (verify-phase security probe).
