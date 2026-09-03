---
id: TASK-367
title: "The transcript format document, README and doctor topic, context additions, and the follow-up filings"
status: draft
parent: REQ-611
repo: teton-code
created: 2026-09-03
updated: 2026-09-03
dependencies: [TASK-362]
---

## Description

What a reader of the file and a reader of the repository need. The format document is the
BR-14 contract ("readers are told to expect a partial trailing line"), the doctor topic and README
carry the commands, `architecture.md` gains the two patterns this REQ introduced, and two
follow-ups are filed rather than absorbed.

## Files to Create/Modify

- `docs/transcript-format.md` — new: the JSONL line shape (`n`, `ts`, `session_id`, `seq`,
  `kind`, `truncated`, `original_bytes`), every sink-local `kind` with its fields, the bus
  envelope form, the `seq`-is-not-contiguous note (LESSON-503), the partial-trailing-line rule,
  file naming, modes, retention, and a five-line `jq` example.
- `README.md` — `/transcript` row (if TASK-365 has not already added it, coordinate), a
  "Transcripts" paragraph under the session section naming the two switches, the default
  location per platform, and that the directory is local-only.
- `crates/tetond/src/harness/docs/doctor.md` — the `transcript:` line explained.
- `.adlc/context/architecture.md` — two Key Patterns entries: *a record sink is a tap, not a
  subscriber* (ADR-1) and *a runtime fact on Config is `serde(skip)`, set once, never read from
  disk* (ADR-7). Component table row for `tetond/src/transcript`.
- `.adlc/knowledge/assumptions/ASSUME-034-transcript-channel-in-records-not-bytes.md` — the
  channel is sized in records; a byte budget is deferred until measured.
- `.adlc/bugs/` (or the follow-up REQ stub the team prefers) — file: *on Linux `cost.db` and the
  web cache live under `$XDG_RUNTIME_DIR`, which is cleared at logout* (ADR-4).

## Acceptance Criteria

- [ ] `docs/transcript-format.md` lists every `kind` that `transcript/record.rs` defines — a
      unit test in that module enumerates `Record` variants and asserts each name appears in the
      doc (the doc is the contract, the test keeps it honest).
- [ ] The partial-trailing-line rule and the `seq` note are present verbatim as the spec words
      them.
- [ ] README and the doctor topic name both switches with their lifetimes.
- [ ] The two follow-ups exist with the spec's wording of the problem.
- [ ] `.adlc/context/architecture.md` diff is additive; no existing pattern text changed.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-14 | test-case | `crates/tetond/src/transcript/record.rs::every_record_kind_is_documented_in_the_format_doc` | no |

## Technical Notes

The doc-enumeration test is deliberately in `record.rs` and not in `docs/`: the source of truth
is the enum, the doc is what must keep up. It reads the doc relative to `CARGO_MANIFEST_DIR`.

Do not describe the transcript as an audit log anywhere in prose (spec Description, LESSON-505).
