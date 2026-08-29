---
id: LESSON-588
title: "A guard on the write path is only tested by a test that performs that write"
component: "daemon/config"
domain: "testing"
stack: ["rust", "daemon"]
concerns: ["testing", "backward-compatibility"]
tags: ["mutation-testing", "serde", "config-preservation", "toml-edit", "req-597"]
req: REQ-597
created: 2026-08-29
updated: 2026-08-29
---

## What Happened

REQ-597 added an `origin` field to `PrivacyBoundary`, with
`skip_serializing_if = "BoundaryOrigin::is_user"`. That attribute is
load-bearing: without it, `config_doc::canonical_document` renders
`origin = "user"` into every `[[boundaries]]` table, and the next `config/set`
writes lines into the user's file that they never authored — the AC-10 failure.

AC-10's test drove an **unrelated** write (a provider registration) against a
config that already declared boundaries, and asserted the file's bytes. Its doc
comment claimed two mutations would fail it: populating `Config.boundaries` with
the builtin set at load, and removing `skip_serializing_if`.

Running the second mutation, the test stayed **green**.

`apply_config_delta` diffs an array of tables **element-wise**, and an element
nothing changed is never re-rendered. A provider registration reads no boundary
element, so no boundary element is re-emitted, so the missing
`skip_serializing_if` has nothing to leak through. The test was not near the
mechanism it named — it was one layer of the writer away from it.

The fix was a second leg that performs a write which really does emit a boundary
element (`ConfigUpdate::SetPrivacyBoundary`). That leg fails under the mutation.
The doc comment now says which leg guards which mechanism instead of claiming
both.

## Lesson

When a guard lives on a serialization path, the mutation test has to trigger a
**write that reaches that serializer for that field**. A surgical, diff-based
writer will skip whole regions of the document, so "I wrote something and the
region was untouched" is evidence about the differ, not about the field's
attributes.

The trap generalizes past serde: any guard behind a lazily-evaluated,
short-circuiting, or diff-driven mechanism needs its mutation test sited where
that mechanism actually runs. Otherwise you get a green test whose doc comment
names a mutation it cannot detect — which is worse than no test, because the
comment tells the next reader the mechanism is covered.

This is LESSON-569's rule applied to a writer: **run the mutation you wrote
down.** The claim in a doc comment is a hypothesis until the test has been
observed failing for that specific reason.

## Applies To

- serde `skip_serializing_if` / `default` attributes whose absence is a defect.
- Any config-preservation test standing on `toml_edit` or another
  document-preserving writer.
- Reviewing a doc comment that lists mutations: ask whether each one was run, and
  what number came back.
