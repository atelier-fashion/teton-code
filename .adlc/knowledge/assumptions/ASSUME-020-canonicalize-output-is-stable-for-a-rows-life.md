---
id: ASSUME-020
title: "A canonicalized path stays byte-identical for the life of a trust row"
status: unresolved
req: REQ-591
created: 2026-08-25
resolved:
---

## Assumption

A `[skills] trusted_project_roots` row is matched by **exact string equality** against
`std::fs::canonicalize(session_root)` taken at discovery. This assumes the canonical form of a
given repository is stable from the moment a human writes the row until every later consult —
potentially months.

## Context

REQ-591 D-4 made the durable row the canonical absolute path specifically so its meaning would
not depend on `$HOME` at consult time. Exact equality (never a prefix test) is what stops a row
for `~/dev/repo` authorizing `~/dev/repo/vendor/other`, so relaxing the comparison is not
available as a fix.

The failure is silent and fails **closed**: the row simply stops matching, and an unattended
session refuses with no indication that a row it can see was intended to cover this tree. Ways it
can happen without anyone moving the repository:

- an external volume remounting at a different path
- a container or CI image changing its checkout root between runs
- a restore from backup landing the tree elsewhere
- a platform changing its firmlink or symlink layout under `/private`, `/System/Volumes`, etc.

The neighbouring residual — a tree *replaced* at a listed path still matching — **is** documented
in `durable_trust_root_name`'s own doc as accepted and unfixable by any name-for-a-location. This
one is not documented anywhere, and it is the more likely of the two.

## Resolution

Unresolved. No shipped release carries a row yet (see ASSUME-021), so nothing has been observed
in the wild. Resolving it means either accepting it explicitly in the same doc that accepts the
tree-replacement residual, or giving the refusal path a way to say *"a row exists that no longer
resolves to any tree"* — which is diagnosable, unlike silence.
