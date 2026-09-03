---
id: REQ-611
title: "Daemon-side transcript logging: an opt-in, per-session JSONL record you can switch on and off"
status: complete
deployable: true
created: 2026-09-03
updated: 2026-09-03
component: "daemon/session"
domain: "privacy"
stack: ["rust", "daemon", "cli", "json-rpc"]
concerns: ["privacy", "security", "developer-experience", "reliability"]
tags: ["transcript", "logging", "jsonl", "observability", "events", "redaction", "retention", "config-toggle", "session-command"]
---

## Description

Today nothing a session does survives it. The conversation is daemon-lifetime
memory, replaced wholesale at the end of every turn and dropped on `/clear`,
`/cd`, or daemon exit (REQ-567 BR-9 said so deliberately). The only durable
trace of a session is the daemon's own stderr under
`$(brew --prefix)/var/log/teton/`, which carries startup lines, refused
connections, taint pins, and over-budget acceptances — never a prompt, a model
reply, a tool call, or a tool result. When something goes wrong, the user has
no record to read back: the 2026-08-18 incident in REQ-583 had to be
reconstructed from `cost.db` and `lsof` because, as that spec records, "no
transcript is persisted" (informed by REQ-583).

This REQ adds a **transcript**: an opt-in, per-session, append-only JSONL file
the daemon writes itself, holding what the session saw and did — the user's
prompts, the model's streamed text, tool calls and the results the harness
handed back, proposed diffs, plans, route decisions, cost records, privacy
blocks, permission requests and their answers, and skill invocations. It is
written by an in-daemon sink, not by a client, so it is complete regardless of
which client is attached, whether one is attached at all, or whether the
session is piped and scripted.

The user asked for one thing above all: **it must be possible to turn on and
off.** The design gives two switches with different lifetimes, following the
project's existing pattern for `/permissions` (a session-lifetime level, never
written to disk) beside durable config keys:

- `[transcript] enabled` in `config.toml` — the durable default for every new
  session. Off on a stock install.
- `/transcript on` and `/transcript off` — this session only, effective at the
  next record, never persisted. Bare `/transcript` prints the session's
  transcript status, including where the file is.

Three findings from the code survey shape the rest of the spec and are worth
stating up front because they correct the original framing of "a sink on the
broadcast channel":

1. **The bus does not carry everything a transcript needs.** The user's prompt
   text arrives as a `session/prompt` request, not an event, and
   `session_update` carries a tool call's id, title and status but never its
   input or result. Those must reach the sink in-process from the turn path.
   They must **not** be added to the bus to get there: the bus fans out to every
   attached client and every monitor, and widening it to carry prompt text and
   tool results would be a new disclosure surface (informed by REQ-568,
   LESSON-513).
2. **A plain bus subscription is not a reliable sink.** `EventBus::publish`
   evicts a subscriber whose channel is full and never re-admits it
   (LESSON-513). A transcript that silently stops mid-session is worse than
   none. The sink therefore has its own delivery contract: it may fall behind,
   it may not lose a record without saying so in the file.
3. **A transcript is boundary content, and the session's file tools do not
   read it.** Session titles and roots are already classified as boundary
   content, not metadata (REQ-569 BR-10); a file holding every prompt and tool
   result is more so. The file is owner-only on disk, and its directory is a
   **denied prefix** for every file tool the session runs: a `read`, `edit`,
   `grep`, `glob` or walk that resolves under it is refused the way a path
   outside the session root is refused, whether the directory sits inside or
   outside the root. The `shell` tool is the one named exception, as it is for
   every path rule (REQ-596 BR-6); its output already carries unknown
   provenance and is fail-closed at egress, so a transcript read through a
   shell is no worse than any other file a shell can print (informed by
   REQ-583, REQ-596).

What this is **not**: a security or audit control. LESSON-505 is explicit that
"we log it" is only as strong as the log, and a same-UID adversary can write
the transcript directory. Nothing in the daemon's authorization story may lean
on the transcript existing, being intact, or being read; the events that must
reach a human still reach a human on the bus. The transcript is a user's
record of their own session, and its guarantees are fidelity and honesty about
gaps, not tamper evidence.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| TranscriptConfig (`[transcript]` table in `config.toml`) | enabled | boolean | required; default `false`; serialized unconditionally within the table when present (same posture as `[privacy] redact`) |
| TranscriptConfig | dir | path (string) | optional; default is a **data** location, never a runtime one: `~/Library/Application Support/teton/transcripts` on macOS, `$XDG_DATA_HOME/teton/transcripts` (default `~/.local/share/teton/transcripts`) on Linux; never under `$XDG_RUNTIME_DIR`, which is a tmpfs cleared at logout; may sit inside or outside the session root; the session's file tools refuse it either way (BR-8) |
| TranscriptConfig | retain_days | integer | optional; default `30`; `0` means never prune; files older than this are removed at daemon start and at every transcript open |
| TranscriptConfig | max_record_bytes | integer | optional; default `65536`; any single content field longer than this is truncated with an explicit marker, never silently |
| TranscriptFile | path | path | one file per session: `<dir>/<session-start-utc>-<session_id>.jsonl`; created `0600` in a `0700` directory; append-only; opened at most once per session and reopened for append on `/transcript on` after an `off` |
| TranscriptRecord | n | integer | required; per-file contiguous counter starting at 1; a hole in `n` never appears — a dropped run is a `transcript_gap` record |
| TranscriptRecord | ts | RFC 3339 UTC timestamp | required; time the sink wrote the record |
| TranscriptRecord | seq | integer | present on bus-sourced records; the daemon-wide bus sequence number, **non-contiguous per file by construction** because the bus counter is daemon-scoped and other sessions' events interleave (informed by LESSON-503) |
| TranscriptRecord | session_id | string | required; the session the file belongs to; every record in a file carries the same value |
| TranscriptRecord | kind | string | required; either a wire `event` name from `teton-protocol` (recorded as its flattened envelope) or one of the sink-local kinds in the Events table below |
| TranscriptRecord | truncated | boolean | present and `true` only when a content field was cut at `max_record_bytes`; accompanied by `original_bytes` |
| TranscriptStatus (in-memory, per session) | enabled | boolean | the session's effective state: config default, then any `/transcript` override |
| TranscriptStatus | path | path or none | the file, once opened |
| TranscriptStatus | records | integer | records written this session |
| TranscriptStatus | degraded | string or none | why writing stopped, if it did (write failure, directory refused) |

### Events

Two classes. **Bus events** are published on the daemon's event bus and follow
REQ-568's session-scoped delivery. **Sink-local record kinds** exist only in
the transcript file and are never published.

| Event | Trigger | Payload |
|-------|---------|---------|
| `transcript_state` (bus, session-scoped) | the session's effective transcript state changes: opened from the config default, `/transcript on`, `/transcript off`, or the sink degrading | `enabled: bool`, `reason: config_default \| session_command \| write_failure \| dir_refused`; **no path** — the path is boundary content and is shown only by `/transcript` on the owning connection (a routed reply, never a publish) |
| `transcript_opened` (record) | the file is created or first written for this session | daemon version, `session_id`, session root display form, `[privacy] redact` posture, `max_record_bytes`, the bus `seq` at open so a reader knows where the record begins |
| `prompt_submitted` (record) | a `session/prompt` is accepted for this session | `turn_id`, the prompt blocks as received, the skill invocation if any |
| `tool_call_input` (record) | the harness dispatches a tool call | `tool_call_id`, tool name, the input as the harness parsed it |
| `tool_result` (record) | a tool returns to the harness | `tool_call_id`, status, the result text as the harness received it (subject to `max_record_bytes`) |
| `permission_decided` (record) | a `permission/respond` resolves a request in this session | `request_id`, the chosen option id, whether it was remembered for the session |
| `transcript_gap` (record) | the sink could not keep up and dropped one or more records | `dropped: integer`, the bus `seq` before and after the hole where known |
| `transcript_resumed` (record) | `/transcript on` after an `off` in the same session | bus `seq` at resume |
| `transcript_closed` (record) | the session ends, `/transcript off`, or the daemon shuts down | `reason: session_ended \| session_command \| daemon_shutdown \| write_failure`; the final `n` |
| every session-scoped bus event | the bus publishes an envelope whose `session_id` is this session | the envelope's flattened wire form, unchanged, plus `n` and `ts` |

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| `/transcript on`, `/transcript off`, `/transcript` (status) | a user-typed or piped session command on a connection attached to the session; **never the model** — no tool exposes it, the same shape as `/clear` (REQ-567 BR-8) |
| durable `[transcript] enabled` change via `config/set` | the connection-bound `config/set` path with whatever commitment gate REQ-570 BR-10 and REQ-575 BR-5 classify it under (see Open Questions) |
| receive `transcript_state` | connections attached to the session, and declared monitors (REQ-568 BR-1) |
| read a transcript file | the filesystem owner, outside the session; the session's file tools refuse it (BR-8), and only `shell` can reach it, fail-closed at egress |
| prune transcript files | the daemon only, only in its own transcript directory, only files matching its own naming pattern, never through a symlink |

## Business Rules

- [ ] BR-1: **Off by default, and "off" means the sink does not exist.** With `[transcript] enabled = false` and no `/transcript on`, the daemon opens no file, creates no directory, and hands nothing to a sink — zero writes, zero overhead. This mirrors `[privacy] redact`, where off means the gate is not installed rather than installed and permitting (informed by REQ-562).
- [ ] BR-2: **Two switches, two lifetimes.** `[transcript] enabled` is the durable default read when a session is created. `/transcript on|off` overrides it for that session only, takes effect at the next record, and is never written to disk. A durable change made while a session is running does not alter that session; it applies to sessions created afterwards. A session started with the default off and switched on records from the switch forward; nothing is backfilled from the retained conversation.
- [ ] BR-3: **The session command is a user act, unreachable by the model.** `/transcript` is dispatched like `/clear` and `/verbose`: from a typed or piped session line, never from a tool call or from observed content. There is no protocol method that lets a model or a bystander connection toggle another session's transcript (informed by REQ-567, REQ-572).
- [ ] BR-4: **Content the bus does not carry reaches the sink in-process and is never published to get there.** Prompt text, tool inputs, tool results and permission decisions are handed to the sink from the turn path. Adding any of them to `EventBus::publish` is forbidden by this REQ; the bus's audience is every attached client and monitor, and the transcript must not widen it (informed by REQ-568, LESSON-513).
- [ ] BR-5: **The sink is not an evictable subscriber and never blocks a publish or a turn.** Writing happens off the bus's publish path and off any connection's reader loop; a slow disk delays the file, not the session. If the sink falls behind past its buffer, records are dropped and the drop is written as a `transcript_gap` record naming the count — a hole in the per-file `n` counter never occurs (informed by LESSON-513, LESSON-518).
- [ ] BR-6: **A write failure is announced once and then honest forever.** On the first failed write the sink closes the file with `transcript_closed { reason: write_failure }` where it can, publishes `transcript_state { enabled: false, reason: write_failure }`, prints one line to the session, and stops for that session. `/transcript` status reports the degraded reason. The turn that was in flight is not failed by the transcript failing (informed by LESSON-505).
- [ ] BR-7: **One session, one file, session-scoped records only.** A file holds exactly one session's records. Daemon-scoped envelopes (`session_id` absent — model lifecycle, client attach, grant mints) are not recorded, because they belong to no session and would put other sessions' activity into a file its owner may share (informed by REQ-568).
- [ ] BR-8: **The transcript directory is a denied prefix for the session's file tools.** The default directory is under the daemon's data directory, and a configured `dir` may sit inside or outside the session root. Every file tool resolves paths through the session's one jail, and a canonical path under the effective transcript directory is refused there with a reason that names it as a transcript, exactly as a path outside the root is refused. The walkers (`grep`, `glob`, the discovery walk) consult the same denied prefix through their shared skip set, so a transcript is never listed either. This refusal is not a privacy boundary and is unaffected by `[privacy] disable_default_boundaries`. The `shell` tool is the one named exception, and its output's unknown provenance is fail-closed at egress by the existing rule (informed by REQ-583, REQ-596, REQ-597, LESSON-502).
- [ ] BR-9: **Owner-only on disk.** The directory is created `0700` and every file `0600`; the daemon never relaxes either. An existing directory or file with wider permissions is not silently reused: the sink refuses to open it and degrades per BR-6 with a stated reason.
- [ ] BR-10: **No credential ever lands in the file.** Keys entered through setup flows are keychain references in every event already (REQ-572 BR-6) and stay so; `session_grant_minted` carries scope and requester, never a grant value, and the transcript records exactly the wire form. Any future event field that carries a secret must be elided at the sink, and the test that sweeps a planted fixture key across files, logs, and event payloads is extended to sweep the transcript directory (informed by REQ-572).
- [ ] BR-11: **The `[privacy] redact` scan does not gate transcript writes.** That scan exists for egress and the transcript never egresses; running a model-backed scan on every record would add latency to a local write for no boundary gain. The transcript records prompt text and tool results as the harness received them. The compensating controls are BR-1, BR-8, BR-9 and BR-10, and the `transcript_opened` record states the redact posture so a reader knows what the egress side was doing (informed by REQ-562, REQ-567).
- [ ] BR-12: **Truncation is marked, never silent.** A content field over `max_record_bytes` is cut and the record carries `truncated: true` and `original_bytes`. The same words appear whether one byte or a megabyte was cut (informed by LESSON-447).
- [ ] BR-13: **Retention is a stated policy, applied only to the daemon's own files.** At daemon start and at every transcript open, files in the transcript directory whose name matches the transcript pattern and whose modification time is older than `retain_days` are deleted; `0` disables pruning. Files that do not match the pattern, symlinks, and anything outside the directory are never touched. Pruning writes one stderr line naming the count when it removed anything.
- [ ] BR-14: **The file is readable without teton.** JSONL, one object per line, UTF-8, records self-describing by `kind`, `n` contiguous from 1, `ts` on every record. A partial trailing line (crash mid-write) is the only permitted malformation and readers are told to expect it.
- [ ] BR-15: **State is announced as news, location is answered on request.** Every effective-state change publishes `transcript_state` to the session's attached connections and monitors; the path is never in that event. Bare `/transcript` answers on the asking connection only, as a routed reply, with enabled state, path, record count, and degraded reason (informed by REQ-568).
- [ ] BR-16: **The durable half is verified on disk, paired with a refusal.** A test of `[transcript] enabled` written through `config/set` reads `config.toml` back and re-parses it; its refusal counterpart on the same fixture proves nothing was written. A test of the session override proves `config.toml` is byte-identical before and after `/transcript on` (informed by REQ-591, LESSON-519, LESSON-520).

## Acceptance Criteria

- [ ] AC-1: On a stock config, a full session (prompt, tool call, reply, exit) leaves no `transcripts` directory and no file anywhere under the data directory; the sink is not constructed (assert by inspecting the filesystem, not from the absence of a log line).
- [ ] AC-2: With `[transcript] enabled = true`, a session that sends one prompt which triggers one tool call produces a file at `<dir>/<start>-<session_id>.jsonl` whose first record is `transcript_opened` and last is `transcript_closed { reason: session_ended }`, whose `n` runs 1..k with no holes, and which contains at least one each of `prompt_submitted`, the `route_decided` envelope, `tool_call_input`, the `session_update` tool-call envelopes, `tool_result`, the `agent_message_chunk` envelopes, and `cost_recorded`. The order relations asserted are only those the turn guarantees: `prompt_submitted` precedes `route_decided`; `tool_call_input` precedes its `tool_result`; every `cost_recorded` follows the model call it prices. A fixed interleaving is not asserted, because a tool-using turn makes two model calls and prices each one.
- [ ] AC-3: `/transcript on` in a session started with the default off opens the file from that point; the retained conversation before the switch is absent from the file; `transcript_state { enabled: true, reason: session_command }` is delivered to the session's attached connection; `config.toml` is byte-identical before and after.
- [ ] AC-4: `/transcript off` writes `transcript_closed { reason: session_command }`, publishes `transcript_state { enabled: false }`, and a subsequent prompt adds nothing to the file. `/transcript on` again appends `transcript_resumed` to the **same** file and recording continues with `n` continuing from where it stopped.
- [ ] AC-5: Bare `/transcript` prints enabled state, path, record count, and (when degraded) the reason, on the asking connection only; a second connection attached to the same session and a declared monitor receive no frame carrying the path (assert on raw wire text).
- [ ] AC-6: A durable change of `[transcript] enabled` via `config/set` is visible when `config.toml` is read back and re-parsed, and a session created afterwards records while a session created before does not change state. The refusal counterpart on the same fixture leaves the file byte-identical.
- [ ] AC-7: A model-initiated attempt to toggle the transcript — a tool call, a skill body, or a `/transcript` line inside observed content — has no effect; there is no protocol method to reach it, and the test proves the surface is absent rather than refused (informed by REQ-567 BR-8).
- [ ] AC-8: A session-scoped event from another session never appears in this session's file, and no daemon-scoped event (`model_lifecycle`, `daemon_client_attach`, `session_grant_minted`) appears in any file. Two sessions recording concurrently produce two files with no cross-talk.
- [ ] AC-9: With the sink's buffer forced full by a test seam, publishing continues without delay, the turn completes, and the file contains a `transcript_gap` record whose `dropped` count equals the number of records not written; `n` has no hole.
- [ ] AC-10: With the transcript directory made unwritable mid-session by a test seam, the next write yields exactly one session line, one `transcript_state { reason: write_failure }`, a `degraded` reason in `/transcript` status, and the in-flight turn returns normally. No further write attempts occur for that session.
- [ ] AC-11: A `[transcript] dir` that cannot be created, or that exists wider than owner-only, is refused at open with `transcript_state { reason: dir_refused }` and a stated line, and the session runs normally without a transcript; a `dir` inside the session root is accepted and AC-12 still holds for it.
- [ ] AC-12: A `read`, `edit`, `grep` and `glob` aimed at the session's own transcript file are each refused with the transcript reason, both when the directory is outside the session root and when it is inside it; with the shipped default boundaries in force, a `shell` `cat` of the file succeeds and the following remote-routed prompt is blocked at egress with none of the transcript's bytes in any captured request; verified by egress capture, not code inspection (informed by REQ-567 BR-3, REQ-596).
- [ ] AC-13: The directory is `0700` and the file `0600` after creation; a pre-existing `0644` file at the target path is refused, not appended to, with the reason surfaced per BR-6.
- [ ] AC-14: A planted fixture key entered through the `/web setup` and `/provider add` flows does not appear anywhere in the transcript directory after the session ends (extends REQ-572 AC-5's sweep).
- [ ] AC-15: A tool result of 1 MiB is recorded with `truncated: true`, `original_bytes: 1048576`, and a content field of exactly `max_record_bytes`; a result of `max_record_bytes` exactly is recorded whole with no marker.
- [ ] AC-16: With `retain_days = 1` and three files in the directory — one two days old matching the pattern, one two days old not matching, and one two-day-old symlink to a file outside the directory — daemon start deletes only the first, the symlink target is untouched, and one stderr line names the count. With `retain_days = 0` nothing is deleted.
- [ ] AC-17: A file produced by AC-2 parses line by line with a stock JSON parser and no teton code; every line has `n`, `ts`, `session_id`, and `kind`.
- [ ] AC-18: Daemon shutdown with a recording session flushes the file and writes `transcript_closed { reason: daemon_shutdown }` before the process exits; a SIGKILL leaves at most one partial trailing line.
- [ ] AC-19: An unrelated `config/set` on a `config.toml` that never named `[transcript]` does not add the table, and one on a file that did name it preserves it byte-for-byte; the effective transcript directory never appears in the file unless the user wrote `dir`.
- [ ] AC-20: `teton doctor` and the `/doctor` twin report the transcript posture (durable default, effective directory, retention) in one line.
- [ ] AC-21: With `[privacy] disable_default_boundaries = true` and a transcript enabled, a tool read of the transcript file is still refused per AC-12, while a read of an in-root `.ssh/id_rsa` fixture is no longer blocked at egress; both legs on one fixture.

## External Dependencies

- None. Serialization uses the `serde` types already in `teton-protocol`; file I/O uses the standard library and the tokio runtime the daemon already runs.

## Assumptions

- On macOS the daemon's base directory (`~/Library/Application Support/teton`) is the right home for user-owned records: it is outside every project and already holds `config.toml`, so a user who backs up or wipes their teton state gets the transcripts with it. On Linux the socket resolver puts the base directory under `$XDG_RUNTIME_DIR`, a tmpfs cleared at logout, so a transcript directory derived from it would contradict a 30-day retention policy; the default there is `$XDG_DATA_HOME/teton/transcripts` instead. That `config.toml` itself lives under the runtime directory on Linux is a pre-existing oddity this REQ does not fix, and is noted for a follow-up.
- Bus-sourced records are recorded as their wire envelope unchanged. The transcript makes no claim to be the harness's retained conversation (post containment cut, post compaction — REQ-567 BR-1); it is the stream a fully attached client would have seen plus what the harness handed in and out. Replaying a transcript into a new session is therefore explicitly not a goal of this REQ.
- Recording agent text as the chunks the bus emitted, rather than reassembling one message per turn, is acceptable for a first release. A reader can concatenate chunks between `prompt_submitted` records; the daemon does not buffer to do it for them.
- `max_record_bytes` at 64 KiB and `retain_days` at 30 are reasonable defaults; both are keys, so a user who wants whole files or forever can say so.
- The per-file `n` counter, not the bus `seq`, is the contiguity guarantee. `seq` is minted daemon-wide and is expected to skip in every file; a reader who treats a `seq` skip as a gap is wrong, and the record documentation says so (informed by LESSON-503).
- The `shell` leg of AC-12 relies on the existing rule that unknown-provenance content is fail-closed at egress **when at least one privacy boundary is in force**. With every boundary removed (`disable_default_boundaries = true` and no user rows) unknown provenance takes the egress fast path today, and a shell-printed transcript would travel like any other shell output. That is REQ-597 BR-5's documented consequence of an empty boundary set, not a transcript-specific hole, and the `unbounded_root_warning` posture already names it.
- id allocated with remote verification (7 participating repos, not degraded).

## Open Questions

- [ ] OQ-1: Is a durable `[transcript] enabled` write a REQ-570 BR-10(b) commitment requiring presence attestation, or a layer-(a) connection-bound write like `SetEffort`? It rewrites `config.toml` and changes what every future session does, which is the shape REQ-575 BR-5 says must be classified in the architecture phase. The recommendation is layer (a) only: turning on a local, owner-only record is a preference, and REQ-575 warns that prompting for low-stakes writes trains users to click through the prompt that matters. Architecture must record the decision either way (informed by REQ-570, REQ-575).
- [ ] OQ-2: Should `/transcript on` be refused when piped into a session with nobody at the terminal, the way `/permissions` refuses a piped escalation? It is not an escalation — it writes an owner-only file — so the draft allows it. Confirm.
- [ ] OQ-3: Should a session's `AllowAlways` answers and its web taint state be recorded as sink-local records at open, so a reader can tell what the session was already permitted to do? The draft records decisions as they happen (`permission_decided`) and nothing retroactive.
- [ ] OQ-4: Whether the `permission_decided` record should include the remembered-grant key (`permission_key_for(tier)`) or only the option id. The key is more informative and is not a credential; the draft includes the option id and the remembered flag only.

## Out of Scope

- Replaying or resuming a session from its transcript. REQ-567 BR-9 kept the conversation daemon-lifetime state on purpose; this REQ records, it does not restore.
- A `teton transcript show|tail|grep` viewer. The file is plain JSONL by BR-14; a viewer is a separate REQ if the raw form proves insufficient.
- Recording egress payloads — the composed prompt as sent to a provider, or the provider's raw response. The transcript records the session's surface, not the wire to the model.
- Running the `[privacy] redact` scan, or any scan, over transcript records (BR-11). A future `[transcript] scan` key is possible; it is not this REQ.
- Sandboxing the `shell` tool so it cannot read the transcript directory. REQ-596 named shell as the one exception to every path rule and the fix as a sandbox; that remains its own REQ. The transcript relies on shell output's unknown provenance being fail-closed at egress, which is the posture every other file on the machine already has.
- Shipping transcripts anywhere: no upload, no sync, no remote sink. Any such feature is egress and would go through the choke point in its own REQ.
- Tamper evidence, signing, or any claim that the transcript is an audit control (LESSON-505). Events that must reach a human still travel the bus.
- Size-based rotation within a session. One session is one file; retention is by age only.
- Recording daemon-scoped events or a daemon-wide transcript (BR-7).
- A monitor-client transcript (a `teton` subcommand that attaches as a monitor and writes what it sees). The in-daemon sink covers the same ground without depending on a client staying attached.
- The VS Code extension's rendering of transcript state; it receives `transcript_state` like any client and may ignore it.

## Retrieved Context

- REQ-570 (spec, score 15): Human-attested attach consent: a surface a headless process cannot satisfy, and a client that can answer
- REQ-572 (spec, score 14): Capability-aware refusals and guided in-session enablement
- REQ-568 (spec, score 14): Session-scoped event delivery and bounded request frames
- REQ-569 (spec, score 13): Session attach requires a grant: closing the same-UID ambient-attach path
- LESSON-505 (lesson, score 13): An audit control is judged in the adversarial case, not the honest one — and 'we log it' is only as strong as the log
- REQ-591 (spec, score 12): The project-skill trust gate and its unattended allowlist
- REQ-583 (spec, score 12): Session-root awareness and bounded discovery
- LESSON-519 (lesson, score 12): An 'assert by inspection, not from the error' AC needs the real artifact — add a refusing test seam to reach it
- REQ-575 (spec, score 12): Presence attestation for the web setup commit
- REQ-567 (spec, score 12): Cross-prompt conversation carry in interactive sessions
- REQ-589 (spec, score 11): Offer to proceed when a skill expansion exceeds the route's context budget
- LESSON-513 (lesson, score 11): A pre-authorization publish is attacker-paced — bound the id and budget the notice
- LESSON-518 (lesson, score 11): A blocking gate's reader-loop freedom is not inherited from the await-based reader-loop tests
- LESSON-520 (lesson, score 11): A gate that fires before deserialization makes an invalid-payload test vacuous — use a persistable payload + a refuse/accept pair
- LESSON-503 (lesson, score 11): An id must be minted at the scope that resolves it — and tightening isolation surfaces the latent collisions ambient broadcast was hiding
