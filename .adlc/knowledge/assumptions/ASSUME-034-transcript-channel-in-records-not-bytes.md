---
id: ASSUME-034
title: "Sizing the transcript channel in records, not bytes, is safe until it is measured"
status: unresolved
req: REQ-611
created: 2026-09-03
---

## Assumption

The transcript sink's channel holds **4,096 records**, and a record is one
record whatever it weighs. Nothing bounds the bytes in flight: a `tool_result`
carrying a mebibyte of `read` output occupies one slot, exactly as a
twenty-byte `agent_message_chunk` does. The assumption is that a real session's
record mix never fills that channel with large records for long enough to
matter, so a byte budget would buy nothing a record budget does not.

## Context

BR-12's truncation happens in the **writer**, deliberately: the rule about
`max_record_bytes` has one home, so a future producer inherits it rather than
remembering it. The consequence is that the channel carries each record at its
full size — the cut happens after the queue, not before it — and the queue's
own bound is a count.

The arithmetic that follows is the reason this is written down. A burst of
1 MiB tool results, all queued and none yet written, would hold tens of
mebibytes of daemon memory in flight; at the channel's full depth the ceiling
is theoretically gigabytes. Two things make that unlikely rather than
impossible: the writer is a dedicated OS thread doing blocking appends, so it
drains continuously rather than in scheduler-shaped bursts, and one session
produces tool results serially — a burst needs many sessions recording at once,
each returning very large results.

The failure mode if the assumption is wrong is memory, not correctness. The
sink never blocks a publisher or a turn (BR-5), and a channel that does fill
drops records and says so in the file as a `transcript_gap`. So the bad case is
a daemon that grows unexpectedly under a recording load, which is visible, and
not a transcript that quietly lies, which would not be.

The alternative — a byte-budgeted channel — was rejected for this REQ rather
than overlooked. It requires either measuring each record's serialized size at
`try_send` (work on the bus's publish path, under the bus mutex, which
LESSON-518 says stays empty) or truncating before the queue, which moves BR-12
back to the call sites and gives the rule two homes.

## What would resolve it

A measurement, not an argument: peak sink-channel occupancy and the byte volume
behind it, taken from a real recording session doing large reads — the shape of
the REQ-583 incident, where a 1 MiB file read was the ordinary case rather than
the pathological one. If occupancy stays low, the record bound is right and
this closes as validated. If a realistic session parks tens of mebibytes in the
channel, the follow-up is a byte budget alongside the record count, with the
size taken where the record is already owned rather than on the publish path.

Until then, `CHANNEL_CAPACITY` is a number nobody has justified with a
measurement, and the architecture's risk register says so in the same words.
