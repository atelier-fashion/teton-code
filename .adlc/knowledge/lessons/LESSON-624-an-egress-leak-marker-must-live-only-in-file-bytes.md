---
id: LESSON-624
title: "An egress-leak marker must live only in the file's bytes — tool arguments and harness lines echo into provider requests"
component: "daemon/egress"
domain: "privacy"
stack: ["rust", "daemon"]
concerns: ["privacy", "developer-experience"]
tags: ["egress-capture", "test-fixture", "false-positive", "grep", "tool-call-arguments"]
req: REQ-611
created: 2026-09-03
updated: 2026-09-03
---

## What Happened

REQ-611's AC-12 test planted a decoy transcript containing a marker, drove the
four file tools and a `shell` `cat` at it, and asserted that no captured
provider request carried the marker. The assertion failed twice while the
daemon was behaving correctly. First, the scripted `edit` call used the marker
as its `old_string`, and a tool call's arguments travel back to the provider as
the assistant's own message. Then the scripted `grep` used the marker as its
pattern, and grep's harness line `no matches for `…`` echoes the pattern into
the conversation, from which every later request carries it. The daemon log
showed the shell output *had* pinned the session local; the leak was the
fixture's.

## Lesson

In an egress-capture test, the marker you assert absent may exist in exactly
one place: the bytes of the file the protection guards. Never put it in a
tool-call argument, a grep pattern, a prompt, or a file name — each of those
is legitimately echoed into the conversation and reaches the provider. When
the assertion fires, read the leaking request body before touching the code:
a request that carries the marker inside the assistant's tool-call JSON or a
`no matches for` line is an echo, not a leak.

## Why It Matters

A false egress-leak finding sends a reviewer at the choke point, the most
sensitive code in the tree, to hunt a bug that is not there — and a fixture
"fixed" by loosening the assertion would stop detecting the real leak. The
diagnostic that settled it in one run was printing which request indices carry
the marker alongside the ordered event names; add that to the assertion
message from the start.

## Applies When

Writing or debugging any test that asserts boundary content never reaches a
remote body (`assert_no_boundary_bytes`, `MockProvider::requests()`); scripting
a `MockProvider` turn whose tool call mentions the protected content; choosing
a grep pattern or `old_string` in a fixture that also plants that content.
