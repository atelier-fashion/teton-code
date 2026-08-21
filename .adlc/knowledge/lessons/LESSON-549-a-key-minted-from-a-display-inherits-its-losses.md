---
id: LESSON-549
title: "A key minted from a display inherits everything the display threw away"
component: "daemon/permissions"
domain: "security"
stack: ["rust"]
concerns: ["correctness", "security", "privacy"]
tags: ["permission-key", "identity", "display", "utf8", "path", "injectivity", "req-587", "bug-188"]
req: REQ-587
created: 2026-08-21
updated: 2026-08-21
---

## What Happened

REQ-587's project-skill acknowledgment key was minted as
`project_skill_trust:<root>`, where `<root>` came from `session_root::display_for`
— a helper written for the CLI banner's `cwd:` line. It ends in `Path::display`,
which renders every byte outside a valid UTF-8 sequence as `U+FFFD`. Two
repository roots differing only in such bytes rendered identically and minted one
key, so a session grant answered for one could be spent on the other (BUG-188).

The key's own doc already refused this collapse in the form its author expected
— "two long roots sharing a prefix must not collapse onto one key" — and then
reintroduced it through the input, by keying on a string built for reading.

## Lesson

**A value rendered for a person and a value compared by a machine are different
values, even when they are the same string today.** Reusing a display helper as
an identity source silently inherits every lossy transform it performs, and
those transforms are invisible at the call site: `display_for(path)` looks total.

When identity must also be *shown* — this key can reach a client's refusal line —
the answer is a **lossless** rendering, not a hash. Percent-escaping the bytes a
UTF-8 decode rejects (`%XX`, with a literal `%` as `%25`) keeps the string
injective, readable, and identical to the display for very nearly every path.
Hashing was the obvious fix and the wrong one: a digest of the raw bytes puts the
absolute path — and the username — back into the key, and cannot be displayed at
all.

Check both ends of a helper before reusing it. `display_for` had **two** lossy
exits, not one: the `~/{rest}` branch and the absolute fallback.

## Why It Matters

A permission key is a promise about scope. When two subjects mint one key, the
grant silently widens to cover something the user never saw — the exact harm a
per-root scope exists to prevent, arriving through a helper nobody thought of as
security-relevant.

The cheap mitigation is to fail closed on the ambiguous case, and REQ-587's
verify pass did that under concurrent file ownership. It is worth knowing that
this is a *holding action, not a fix*: it cost every repository with a non-UTF-8
path its model-invocable skills, and it has to be deleted rather than merged once
the real fix lands.

## Applies When

Minting any permission key, cache key, dedup key, or identity string from a path,
a name, or anything else with a `Display`/`to_string()` on it — especially when
the same value is also rendered to a user, which is what makes reusing the
display helper feel natural.
