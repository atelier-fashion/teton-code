---
id: LESSON-551
title: "When a test disagrees with the product, suspect the instrument first"
component: "daemon/harness"
domain: "harness"
stack: ["rust", "daemon", "ci"]
concerns: ["reliability", "developer-experience"]
tags: ["testing", "mocks", "mutation-testing", "ci", "platform", "http-framing", "vacuity", "skills"]
req: REQ-587
created: 2026-08-22
updated: 2026-08-22
---

## What Happened

Four times in one REQ, the **test instrument** was the defect rather than the
product:

1. **A mutation that was a no-op.** Publishing a file's dynamic slots onto a
   refusal record changed nothing, because no fixture that could be refused had
   any slots. The fixture gained a slot; the mutation then reddened.
2. **A mutation that could not fire.** Deleting `ResultDisposition::Expansion`'s
   digest bypass left `a_seven_thousand_word_expansion_enters_a_128k_route_
   whole_and_unelided` green — on a declared 128k window `digest_thresholds`
   scales past anything a fixture can write, so `summarize_if_large` is never
   reached there. The claim lives on the default-budget route instead.
3. **A mutation-table row naming a test that could not produce its red.**
   "Adding `skill` to `UNTRUSTED_OUTPUT_TOOLS`" was attributed to a byte-equality
   test, but the fold matches on disposition first and the `Expansion` arm
   returns without ever reading the name list.
4. **A mock that read by guesswork.** The `Vendor` mock's loop broke on
   `saw \r\n\r\n && read < buf.len()` — a guess about socket chunking, not HTTP
   framing. A short read is legal anywhere in a stream, so a body over 64 KiB
   truncated on Linux and not on macOS. CI failed on ubuntu only and reported it
   as *"the expansion was condensed or elided"* — a product claim about BR-7's
   central guarantee.

**The same loop existed in three files.** Two were found by the failure; the
third, in `provenance_egress.rs`, was found only by validating a citation — and
that is the file whose legs assert what a boundary keeps **off** the wire, where
a truncated capture makes an absence assertion pass for the wrong reason.

## Lesson

A difference between two platforms is a property of the instrument until shown
otherwise. Before believing a test's story about the product:

- **Reproduce the mechanism locally rather than reasoning about the platform.**
  Shrinking the mock's buffer to 512 bytes forces exactly the short-read
  condition Linux produced, and proves the fix without Linux.
- **Read by framing, never by a heuristic** — parse `Content-Length` and read to
  the end of the body.
- **A mutation that reddens nothing is not a passed mutation** — it is a fixture
  that cannot reach the state, and the fixture is what must change.
- **When you fix an instrument, grep for its copies.** All three read loops were
  the same twelve lines.

## Why It Matters

An instrument defect costs twice: once for the false signal, and again because
it is indistinguishable from the product defect it imitates. The Linux failure
named BR-7's guarantee, on the one platform not reproducible locally — every
signal pointed at the elision guard, and the guard was fine.

## Applies When

A test passes on one platform and fails on another; writing or reviewing a
mutation table; building a mock that speaks a framed protocol; a test asserts
the **absence** of something on a captured stream; or a mutation reddens nothing
and the temptation is to record it as caught.
