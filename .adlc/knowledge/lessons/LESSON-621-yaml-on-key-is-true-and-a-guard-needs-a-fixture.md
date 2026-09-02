---
id: LESSON-621
title: "A bare `on:` parses to the boolean True, and a guard against a parser gotcha needs the fixture that fails without it"
component: "ci/tooling"
domain: "ci"
stack: ["python", "yaml", "github-actions"]
concerns: ["reliability", "testing"]
tags: ["yaml-1.1", "boolean-coercion", "pyyaml", "workflow-parsing", "unguarded-guard", "mutation", "req-608"]
req: REQ-608
created: 2026-09-02
updated: 2026-09-02
---

## What Happened

REQ-608's parity check parses `.github/workflows/ci.yml` with `yaml.safe_load`
to derive job names and to warn when `on.pull_request` carries a `paths` filter
(a required context that can skip deadlocks every merge — BR-7). PyYAML follows
YAML 1.1, where the unquoted scalar `on` resolves to the boolean `True` — as do
`yes`, `no`, `off`. So `workflow["on"]` is a `KeyError` on every GitHub
workflow ever written, and `workflow.get("on")` is quietly `None`.

The first implementation knew this: `_triggers()` looked up both `"on"` and
`True`, and its docstring called the branch "the single most likely place for a
silent wrong answer in this file". What it did not have was a test. The
reflector ran the mutation — reduced the lookup to `"on"` alone — and the
suite stayed 15/15 green, because no fixture carried a path filter. The
docstring asserted the guard was load-bearing; nothing could prove it. The
same pass found three more refusal branches (`${{` in a name, the job-key
fallback, the boolean matrix value) that were written, documented, and never
exercised.

## Lesson

Two halves. **(1)** When parsing GitHub workflow YAML with a YAML 1.1 parser,
look up triggers under both `"on"` and `True`, and never let a matrix or name
value that PyYAML coerced (`true`, `3.10` → `3.1`) reach a string comparison —
refuse it by name. **(2)** A guard written against a known gotcha is not
verified by knowing the gotcha. It needs a fixture that *exhibits* the gotcha
and an assertion that fails when the guard is removed. Write the mutation into
the test's docstring — "reduced to one key → this case fails on
`AssertionError: '::warning title=Path-filtered workflow::' not found`" — so
the next reader can tell a guarded branch from a documented one.

## Why It Matters

The failure is silent by construction: the lookup returns `None`, the warning
never fires, and every other assertion in the suite is untouched. Someone
"tidying" a `True` that looks like a typo would have removed BR-7's only
detector with a green suite as cover. LESSON-464's shape — a control that
exists and that nothing fails when it is removed — applies to a single `if` as
much as to a CI job. The cost of the fixture was eleven lines.

## Applies When

Parsing any GitHub Actions workflow (or any YAML 1.1 document with keys like
`on`/`off`/`yes`/`no`) in Python; writing a guard whose reason for existing is
a comment; reviewing a script that documents its own most dangerous branch —
ask for the test, then remove the branch and watch it go red.
