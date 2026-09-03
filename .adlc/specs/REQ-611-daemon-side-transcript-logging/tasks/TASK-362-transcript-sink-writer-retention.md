---
id: TASK-362
title: "The transcript sink: records, the per-session writer, truncation, gaps, retention"
status: draft
parent: REQ-611
repo: teton-code
created: 2026-09-03
updated: 2026-09-03
dependencies: [TASK-360]
---

## Description

The new `tetond::transcript` module, unit-tested in isolation against a temp directory: the
`Record` kinds (ADR-2), a per-session append-only JSONL writer with owner-only modes, the
contiguous `n` counter, truncation with marker, gap records, open/resume/close, degradation on
write failure (ADR-8), and the age-based prune. It has no bus, no session registry, and no
runtime dependency — TASK-363 wires it. Covers BR-6, BR-7 (per-file), BR-9, BR-12, BR-13,
BR-14, AC-13, AC-15, AC-16, AC-17.

## Files to Create/Modify

- `crates/tetond/src/transcript/mod.rs` — `TranscriptSink` (the channel receiver task, the
  per-session map from ADR-3, `record()`, `session_created()`, `session_closed()`,
  `set_enabled()`, `status()`), `SinkConfig` built from `TranscriptConfig` + effective dir.
- `crates/tetond/src/transcript/record.rs` — `Record` enum with the sink-local kinds and a
  `BusEnvelope(EventEnvelope)` arm; the on-disk `Line { n, ts, session_id, seq?, kind, …,
  truncated?, original_bytes? }` serializer.
- `crates/tetond/src/transcript/writer.rs` — `Writer::open(dir, session_id, started_at)`:
  create dir `0o700` if absent, refuse an existing dir or file wider than owner-only, create the
  file `0o600` with `OpenOptions` append, write `transcript_opened`; `append(Line)`;
  `resume()`; `close(reason)`; `flush()`. Uses the `auth.rs:223–257` permission helpers.
- `crates/tetond/src/transcript/retention.rs` — `prune(dir, retain_days, now) -> PruneReport`:
  matches only `^\d{8}T\d{6}Z-sess-[0-9a-hjkmnp-tv-z]{26}\.jsonl$` (Crockford base32, lowercased — the alphabet `sessions.rs:282` mints; not RFC 4648), uses `symlink_metadata`, never follows a
  symlink, never leaves `dir`.
- `crates/tetond/src/lib.rs` — `pub mod transcript;`.

## Acceptance Criteria

- [ ] AC-13 / BR-9: after `open`, dir mode is `0o700` and file mode `0o600`; a pre-existing
      `0o644` file at the path makes `open` return `Err(Refused::Mode)` and the file is not
      appended to.
- [ ] AC-17 / BR-14: every written line parses with `serde_json` alone and carries `n`, `ts`,
      `session_id`, `kind`; `n` runs from 1 with no holes across open, resume, and close.
- [ ] AC-15 / BR-12: a content field of `max_record_bytes + 1` is cut to `max_record_bytes` with
      `truncated: true, original_bytes: <len>`; a field of exactly `max_record_bytes` carries no
      marker. The same two fields appear whether one byte or a mebibyte was cut.
- [ ] BR-5 (sink half): `dropped(session, k)` followed by any record writes a `transcript_gap {
      dropped: k }` line **before** that record, and `n` stays contiguous.
- [ ] BR-6 / ADR-8: an injected `io::Error` on append sets `status().degraded = Some(reason)`,
      attempts one `transcript_closed { write_failure }`, and every later `record()` for that
      session is a no-op returning `()`; the sink's `on_degraded` callback fires exactly once.
- [ ] BR-7: two sessions recording concurrently produce two files with no shared lines; a record
      for an unknown session is dropped, not written to a fresh file.
- [ ] AC-16 / BR-13: with `retain_days = 1` and three two-day-old entries — a matching file, a
      non-matching file, a symlink to a file outside `dir` — `prune` removes only the first and
      reports `removed = 1`; the symlink target is untouched; `retain_days = 0` removes nothing.
- [ ] `transcript_opened` records daemon version, session id, root display form, redact posture,
      `max_record_bytes`, and `seq_at_open`.
- [ ] `cargo test -p tetond transcript:: --no-fail-fast` is green on macOS and the ubuntu CI leg
      (mode bits differ on no platform CI runs, but umask does — set modes explicitly).

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-5 | test-case | `crates/tetond/src/transcript/mod.rs::a_dropped_run_becomes_one_gap_record_and_n_stays_contiguous` | yes |
| BR-6 | test-case | `crates/tetond/src/transcript/mod.rs::first_write_failure_degrades_once_and_never_again` | yes |
| BR-7 | test-case | `crates/tetond/src/transcript/mod.rs::two_sessions_two_files_no_crosstalk` | no |
| BR-9 | test-case | `crates/tetond/src/transcript/writer.rs::open_refuses_a_wider_than_owner_only_file` | yes |
| BR-12 | test-case | `crates/tetond/src/transcript/record.rs::truncation_is_marked_and_exact_size_is_not` | yes |
| BR-13 | test-case | `crates/tetond/src/transcript/retention.rs::prune_removes_only_matching_old_files_and_never_follows_symlinks` | yes |
| BR-14 | test-case | `crates/tetond/src/transcript/writer.rs::every_line_parses_standalone_and_n_is_contiguous` | no |
| AC-13 | test-case | `crates/tetond/src/transcript/writer.rs::open_creates_owner_only_dir_and_file` | no |
| AC-15 | test-case | `crates/tetond/src/transcript/record.rs::truncation_is_marked_and_exact_size_is_not` | yes |
| AC-16 | test-case | `crates/tetond/src/transcript/retention.rs::prune_removes_only_matching_old_files_and_never_follows_symlinks` | yes |
| AC-17 | test-case | `crates/tetond/src/transcript/writer.rs::every_line_parses_standalone_and_n_is_contiguous` | no |

## Technical Notes

One writer task per daemon, one file handle per session. The channel is `tokio::sync::mpsc`
bounded at 4096 records; `record()` and the tap both `try_send`. Do the truncation in the writer
task, not at the call site, so the rule has one home (BR-12) — but be aware the channel then
carries full results (architecture: risks).

The file name embeds the session id. Session ids are names, not credentials (REQ-569 BR-8), so
this discloses nothing a `session/list` would not; it is what lets `prune` match its own files
without a manifest.

`transcript_closed` on `daemon_shutdown` is TASK-363's call; this task only exposes `close`.
Write with `write_all` then `flush` per line; a crash can leave at most one partial trailing
line, which BR-14 permits and the format doc (TASK-367) states.
