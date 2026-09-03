# REQ-611 — Architecture

## Approach

The daemon already has the one thing a transcript needs: a single point every session-scoped
event passes through, `EventBus::publish`. The design adds a **tap** on that point — not a
subscriber — and a small **sink** behind it that owns one append-only JSONL file per session.
Everything the bus does not carry (prompt text, tool input, tool result, the permission
decision) is handed to the same sink in-process from the two places that already hold it:
`SessionEvents` in the turn loop and `handle_permission_respond` in the server. Nothing new is
published to the bus except one small session-scoped `transcript_state` event, so the audience
of every existing event is unchanged (BR-4).

The two switches map onto two existing shapes. The durable one is a `[transcript]` table in
`config.toml` written through `config/set`, which inherits that method's connection binding
and commitment gate unchanged. The session one is a new `session/transcript` method gated by
`may_drive`, exactly like `session/permissions`, and reached from the CLI by a `/transcript`
row in the `COMMANDS` table beside `/clear` and `/verbose`. The model has no tool that reaches
either, so BR-3 is true by construction rather than by a refusal.

Three things the code survey settled, which the tasks depend on:

1. **`data_dir` is `base_dir`.** `DaemonRuntime::from_env` sets `data_dir = base_dir`, and on
   Linux `resolve_base_dir` prefers `$XDG_RUNTIME_DIR`. So today the cost ledger also lives in a
   logout-cleared directory on Linux. This REQ adds a `resolve_data_dir` beside
   `resolve_base_dir` and uses it **only for transcripts**; moving `cost.db` is a filed
   follow-up, not a side effect of this REQ (ADR-4).
2. **Provenance ids are repo-root-relative, and the tool jail already refuses everything
   else.** `ProvenanceId::from_resolved(root, path)` strips the root and errors otherwise, and
   `ToolContext::resolve` refuses any canonical path outside the root before minting one. So a
   boundary glob can never match a transcript outside the root — there is no id to match — and
   inside the root the jail is the seam that can see the path. The transcript directory is
   therefore a **denied prefix in the jail and the walkers**, not a boundary row (ADR-7). A
   first draft of this document composed a `<dir>/**` boundary row; validation refuted it.
3. **`config/set` is presence-gated as a whole.** `handle_config_set` runs `refuse_daemon_wide`
   and then `refuse_unattested_commitment` before it deserializes the update, for every variant.
   The new variant inherits both, and the spec's OQ-1 closes by inheritance (ADR-5).

## Key decisions

### ADR-1 — A bus tap, not a subscriber, feeds the sink

`EventBus` gains `install_tap(Arc<dyn EventTap>)`. `publish` mints the sequence number, builds
the envelope, calls `tap.observe(&envelope)` **before** the subscriber fan-out, and only then
retains over subscribers. The tap's `observe` is a `try_send` into the sink's own bounded
channel and returns immediately; a full channel increments a per-session dropped counter that the
sink turns into a `transcript_gap` record on its next successful write.

**Rationale.** A subscriber whose channel fills is evicted and never re-admitted (LESSON-513).
That is the right contract for a client and the wrong one for a record: the transcript's promise
is "never a silent hole", which needs the drop to be *counted*, not the consumer to be removed.
A tap also sees every envelope with its `seq` exactly as the wire will carry it, so bus-sourced
records are the wire form verbatim (BR-7's "unchanged"). Placing the call before the fan-out under
the bus mutex is safe only because `observe` cannot block, which is why the trait returns nothing
and the sink's channel is `try_send` only (BR-5, LESSON-518).

**Alternatives rejected.** A large-capacity subscriber — still evictable, and eviction is exactly
the failure the spec forbids. A second publish path for the sink — two publishers means two
sequence spaces and the spec's `seq` promise breaks (LESSON-503).

### ADR-2 — Sink-local record kinds are daemon types, never protocol events

`prompt_submitted`, `tool_call_input`, `tool_result`, `permission_decided`, `transcript_opened`,
`transcript_resumed`, `transcript_gap`, and `transcript_closed` are defined in
`crates/tetond/src/transcript/record.rs` as `Record`, serialized straight to the file. They are
not `Event` variants and have no wire name.

**Rationale.** BR-4 forbids widening the bus. Defining these in `teton-protocol` would invite
the next author to publish them "since the type is already there" — the mapper's own first draft
put them in `events.rs`, which is the mistake this ADR exists to name. The one thing that *is* a
protocol event, `transcript_state`, carries `enabled` and `reason` and no path (BR-15).

The in-process hand-off is `TranscriptSink::record(&SessionId, Record)`, which goes down the
**same** channel as the tap. One channel, one writer task, so the file's `n` order is the arrival
order at that channel — a bus envelope and the `tool_result` that follows it land in the order the
turn produced them.

### ADR-3 — The sink owns per-session transcript state; the session registry does not

The sink holds `HashMap<SessionId, SessionTranscript { enabled, writer: Option<Writer>, n,
dropped, degraded, root }>`. The `session/create` handler calls `sink.session_created(id, root,
config.transcript.enabled, seq)` (in `runtime::transcript_session_created`, not in
`SessionRegistry::create` — the registry holds none of the three inputs). `session_closed(id,
SessionEnded)` has **no production call site**: this daemon has no session-removal path, so every
shipped transcript closes with `daemon_shutdown` via `shutdown()`; the `SessionEnded` reason is
exercised by the sink's unit tests only and is wired the day a session end exists (recorded in
`tests/transcript.rs`'s header). `session/transcript` reads and writes through the sink's API;
`SessionRecord` gains no field.

**Rationale.** The effective toggle is a fact about a file — whether one is open, how many
records it holds, why it stopped — and the sink is the only writer of that file. Splitting the
flag into the registry and the file into the sink puts one fact in two places, and the registry
lock is already the lock nobody wants to hold longer (LESSON-448 as cited in `sessions.rs`).
The sink's map is keyed by `SessionId` and never pruned by id reuse, the same posture as
`session_gates` in the runtime.

### ADR-4 — `resolve_data_dir`, used only for transcripts

`teton_protocol::socket_path` gains `resolve_data_dir(xdg_data_home, home)`: macOS
`~/Library/Application Support/teton`, Linux `$XDG_DATA_HOME/teton` else `~/.local/share/teton`,
else the OS temp dir. `TranscriptConfig::effective_dir(data_dir)` returns the user's `dir` when set,
otherwise `<data dir>/transcripts`.

**Rationale.** The spec's assumption is right and the runtime's `data_dir` is the wrong input to
it. Relocating `cost.db` and the web cache in the same change would be a silent migration of two
stores whose tests assume the current place; it is filed as a follow-up in TASK-367 and not done
here. On macOS the two resolvers agree, so the default install is unchanged.

### ADR-5 — The durable toggle is a `ConfigUpdate` variant and inherits `config/set`'s gates

`ConfigUpdate::SetTranscriptEnabled(bool)` is persisted by the existing `config_document` path.
No new gate and no exemption: the variant runs behind `refuse_daemon_wide` (REQ-570 BR-10 layer a)
and `refuse_unattested_commitment` (layer b, degrading to allow with a stderr line where no
mechanism exists, REQ-575 BR-3).

**Rationale, and the spec's OQ-1.** The spec recommended layer (a) only. The code makes that
choice unavailable without carving the first per-variant exemption out of `config/set`, and an
exemption is a new door a later variant will walk through (LESSON-578). On every shipped build
the gate degrades to allow with a notice, so the user cost of inheriting it is zero today, and on
a `--features presence` build one prompt for a durable rewrite is the posture REQ-575 already
chose. REQ-575 BR-5's classification obligation is discharged: **BR-10(b) by inheritance.**

### ADR-6 — `session/transcript` is a session method shaped like `session/permissions`

`SessionTranscriptParams { session_id, action: On | Off | Status }` →
`SessionTranscriptResult { enabled, path: Option<String>, records: u64, degraded: Option<String> }`.
Handler order: `refuse_unmintable_session_id`, `may_drive`, then the runtime. `ENDS_TURN` is
`false`, as for `session/permissions`. The result goes back only on the asking connection as the
RPC response; the `transcript_state` event is published session-scoped without the path.

**Rationale.** BR-15 splits news from location. The path is boundary content (REQ-569 BR-10
classifies `cwd` that way, and a transcript path names the user's home); a routed response is the
established shape for "this connection learns X" (architecture: *a message for one connection is
routed, never published*). `may_drive` rather than `may_receive` because on/off is a mutation.
`Status` takes the same gate deliberately: a monitor sees `transcript_state` but must not learn the
path.

### ADR-7 — The transcript directory is a denied prefix in the tool jail and the walkers

`ToolContext` gains `denied_prefixes: Vec<PathBuf>` (canonicalized at construction), set by the
runtime to the session's effective transcript directory. `ToolContext::resolve` — the one jail
every `read`/`edit` path and every walker seed passes through — refuses a checked path that
`starts_with` a denied prefix with `ToolError::jail("path `{raw}` is a session transcript; tools
do not read transcripts")`, after the outside-root check and before minting a `ProvenanceId`.
`WalkPolicy` carries the same list, and `walk::visit` prunes a directory whose canonical path is
a denied prefix exactly as it prunes `skip_dirs`. Two seams, so LESSON-502 applies: each has its
own adversarial test, and the e2e case drives `read`, `edit`, `grep` and `glob` at the file.

**Rationale.** A boundary row cannot do this job (Approach, item 2): outside the root there is no
provenance id for a glob to match, and inside the root the boundary would only *taint* a read that
should not happen at all. Refusal is stronger than local-only and it is the posture the jail already
gives every out-of-root path, so the user-visible rule is one sentence: *tools do not read
transcripts*. It also deletes machinery — no runtime fact on `Config`, no change to
`effective_boundaries`, no interaction with `disable_default_boundaries` (AC-21 now tests that the
refusal is independent of the boundary set). Composing the denial whenever a transcript directory is
known, not only while recording, is deliberate: last week's file is still there after `/transcript
off`.

**The named exception.** `shell` bypasses every path rule by design (REQ-596 BR-6) and its output is
unknown-provenance, fail-closed at egress while any boundary is in force. That is the existing
posture for every file on the machine, and the spec's Assumptions record the empty-boundary-set
caveat rather than pretending the transcript closes it.

### ADR-8 — Write failure closes the session's transcript; it never fails the turn

The writer task, on the first `io::Error` for a session, sets `degraded`, attempts a final
`transcript_closed { write_failure }`, publishes `transcript_state { enabled: false, reason:
write_failure }`, and drops the writer. The turn path's `record` calls are fire-and-forget by
type: they return `()`.

**Rationale.** BR-6 and LESSON-505 together: a record that cannot be written must be announced in
front of a human, and a user's turn must not die because their disk is full. Returning `()` from
`record` is what makes "never fails the turn" a property of the API rather than of each call site
remembering to ignore an error. The session line is one sentence, printed once, because a
notice that repeats is one users learn to read past (LESSON-513).

## Component map

| Layer | File | Change |
|---|---|---|
| Protocol | `crates/teton-protocol/src/socket_path.rs` | `resolve_data_dir` |
| Protocol | `crates/teton-protocol/src/events.rs` | `Event::TranscriptState`, `name()` arm, spec-table test row |
| Protocol | `crates/teton-protocol/src/methods.rs` | `SessionTranscriptParams/Result`, `ConfigUpdate::SetTranscriptEnabled`, ends-turn table row |
| Core config | `crates/teton-core/src/config.rs` | `TranscriptConfig`, `Config.transcript`, `effective_dir`, validation |
| Daemon tools | `crates/tetond/src/harness/tools/mod.rs`, `walk.rs` | `denied_prefixes` on `ToolContext::resolve` and `WalkPolicy` |
| Daemon bus | `crates/tetond/src/broadcast.rs` | `EventTap`, `install_tap`, tap call in `publish` |
| Daemon sink | `crates/tetond/src/transcript/{mod,record,writer,retention}.rs` | new module |
| Daemon runtime | `crates/tetond/src/runtime/mod.rs` | sink construction, prune at start, `session_transcript`, `SetTranscriptEnabled` persistence, `config/get` posture |
| Daemon runtime | `crates/tetond/src/runtime/config_document.rs` | render the `[transcript]` table |
| Daemon turn | `crates/tetond/src/harness/turn_loop.rs` | `SessionEvents` carries the sink; `prompt_submitted`, `tool_call_input`, `tool_result` hand-offs |
| Daemon turn | `crates/tetond/src/runtime/turn.rs` | construct `SessionEvents` with the sink |
| Daemon server | `crates/tetond/src/server.rs` | `handle_session_transcript`, dispatch arm, `permission_decided` hand-off |
| Daemon sessions | `crates/tetond/src/sessions.rs` | `session_created` / `session_closed` calls |
| Daemon lifetime | `crates/tetond/src/main.rs` / `lifetime.rs` | flush and `transcript_closed { daemon_shutdown }` on teardown |
| CLI | `crates/teton/src/slash.rs` | `/transcript` rows in `COMMANDS` |
| CLI | `crates/teton/src/main.rs` | `teton transcript enable\|disable\|status` |
| CLI | `crates/teton/src/session_ui.rs` | render `transcript_state` |
| CLI | `crates/teton/src/status.rs` | doctor posture line |
| Docs | `README.md`, `docs/transcript-format.md`, `crates/tetond/src/harness/docs/doctor.md` | command table, file format, doctor topic |
| Tests | `crates/tetond/tests/transcript.rs` (new), `multi_client.rs`, `egress_capture.rs`, `config_preservation.rs`, `crates/teton/tests/{cli_e2e,pty_e2e}.rs` | acceptance |

## Risks and accepted consequences

**The tap runs under the bus mutex.** Its cost is one `try_send` per publish. A regression that
makes `observe` block would stall every publisher; TASK-363 pins non-blocking with a test that
fills the sink channel and asserts `publish` returns.

**A tool result can be large and arrives as one record.** BR-12's truncation happens in the
writer, so the channel carries the full result once. Capacity is sized in records, not bytes; a
burst of 1 MiB results could hold tens of MiB in flight. Accepted for this REQ; a byte-budgeted
channel is a follow-up if measured.

**The denial has two seams.** `resolve` and `walk::visit` each enforce it, and a walker that
grows a third entry point (a future tool that lists files without `visit`) would silently miss it.
TASK-368 pins both with adversarial tests and TASK-366 drives all four file tools end to end; the
`boundary_coverage.rs` pattern — every tool has a test — is the model for keeping a fifth tool
honest.

**Linux gets a second state directory.** Transcripts under `~/.local/share/teton` while `cost.db`
stays under `$XDG_RUNTIME_DIR/teton`. Correct for transcripts, and it exposes the pre-existing
placement of the ledger, which TASK-367 files rather than fixes.

**Applied lessons.** LESSON-513 (eviction is not lost telemetry → tap), LESSON-503 (one
sequence space → one publisher), LESSON-518 (nothing blocking under a lock or reader loop),
LESSON-505 (announce in front of a human), LESSON-578 (no per-flow exemptions in a gate),
LESSON-519/520 (durable writes verified on disk with a refusal pair), LESSON-502 (two enforcement seams, two adversarial tests), LESSON-591 (do not pin
detached-task event positions in golden sequences — AC-2's order relations are deliberately few).
