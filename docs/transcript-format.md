# The transcript file format

A **transcript** is one file per session, written by the daemon itself, holding
what that session saw and did: the prompts you sent, the model's streamed text,
the tool calls the harness dispatched and the results it handed back, route
decisions, cost rows, permission answers. Because the daemon writes it, it is
complete whichever client is attached — or none, in a piped and scripted
session.

It is off unless you ask for it, and it is a record for you rather than a
control for the machine. Its guarantees are fidelity and honesty about gaps —
a dropped record is always counted in the file, never silently missing — and
not tamper evidence: the file is owner-only, and anything running as you can
write it. Nothing in the daemon's authorization story leans on a transcript
existing, being intact, or being read; the events that must reach a person
still reach them on the event bus.

## Switching it on

Two switches, with two lifetimes:

| Switch | Lifetime | On disk |
|---|---|---|
| `[transcript] enabled = true` in `config.toml` | the default for every session created **afterwards** | yes — it is the durable setting |
| `/transcript on`, `/transcript off` | this session only, from the next record | no — never written |

A durable change made while a session is running does not alter that session.
A session started with the default off and switched on records from the switch
forward; nothing already said is backfilled.

Bare `/transcript` prints this session's state, the path of its file, how many
records it holds, and — if recording stopped — why. That answer goes back only
on the connection that asked: other clients attached to the session are told
that recording started or stopped, and are not told where to read it.

The rest of the `[transcript]` table:

| Key | Default | Meaning |
|---|---|---|
| `dir` | the platform default below | where files are written; absolute |
| `retain_days` | `30` | days a file is kept; `0` keeps everything |
| `max_record_bytes` | `65536` | per-field content budget before truncation; minimum `1024` |

## Where the files are

One session is one file, named for the session's start and its id:

```
<dir>/<session-start-utc>-<session_id>.jsonl
20260903T101112Z-sess-0123456789abcdefghjkmnpqrs.jsonl
```

`<dir>` is `[transcript] dir` when you set one, and otherwise a **data**
location — not a runtime one, which on Linux is a tmpfs cleared at logout:

| Platform | Default directory |
|---|---|
| macOS | `~/Library/Application Support/teton/transcripts` |
| Linux | `$XDG_DATA_HOME/teton/transcripts`, else `~/.local/share/teton/transcripts` |

`$XDG_DATA_HOME` wins wherever it is set, macOS included; with neither it nor a
home directory available the daemon falls back to the OS temp directory, which
is unusual and normally means a stripped environment.

The directory is created `0700` and every file `0600`, on every platform and
regardless of your umask. A directory or file that already exists **wider** than
owner-only is refused rather than tightened or reused, and so is a symlink at
either path: the session then runs normally with no transcript and says so once.

Files are pruned by age at daemon start and at every transcript open. Only names
matching `\d{8}T\d{6}Z-sess-<26 lowercase Crockford base32 characters, [0-9a-hjkmnp-tv-z]>.jsonl` are candidates, symlinks are
skipped whole, and nothing outside the directory is ever touched — so a
directory of your own that you point `dir` at keeps everything else in it. A
pass that removed anything says so in one line on the daemon's stderr.

**The session's own file tools do not read transcripts.** A `read`, `edit`,
`grep` or `glob` that resolves under the transcript directory is refused with
that reason, whether the directory sits inside or outside the session root, and
the walkers never list one. The refusal is not a privacy boundary and does not
change with `[privacy] disable_default_boundaries`. `shell` is the one
exception, as it is for every path rule — its output carries unknown provenance
and is fail-closed at egress while any privacy boundary is in force.

## One line

The file is JSONL: one JSON object per line, UTF-8, appended and flushed a line
at a time. Four fields are on every line, three more appear when they apply, and
the record's own fields sit flat beside them in the same object.

| Field | Present | Meaning |
|---|---|---|
| `n` | always | the per-file counter, contiguous from 1 |
| `ts` | always | when the sink wrote the record — RFC 3339 UTC with milliseconds |
| `session_id` | always | the session the file belongs to; the same value on every line |
| `kind` | always | what the line is — the tables below |
| `seq` | bus records only | the daemon-wide bus sequence number |
| `truncated` | when a field was cut | always `true` when present |
| `original_bytes` | with `truncated` | how many bytes the cut field(s) held before the cut |

Those seven names, plus `event`, belong to the line. A record body that carried
one of them has it dropped rather than emitting the key twice — no record kind
has such a field today, and the rule is there for the one somebody adds.

One line, as it appears in a file (wrapped here, not in the file):

```json
{"n":1,"ts":"2026-09-03T10:11:12.345Z","session_id":"sess-0123456789abcdefghjkmnpqrs",
 "kind":"transcript_opened","daemon_version":"0.1.28","max_record_bytes":65536,
 "redact":false,"root":"/Users/you/code/teton-code","seq_at_open":41}
```

## Reading it without teton

> **The file is readable without teton.** JSONL, one object per line, UTF-8,
> records self-describing by `kind`, `n` contiguous from 1, `ts` on every
> record. A partial trailing line (crash mid-write) is the only permitted
> malformation and readers are told to expect it.
>
> — REQ-611 BR-14

So read the file a line at a time and be ready for the **last** line to be half
a line: a record is written and flushed per line with no `fsync`, so a crash or
a kill can leave one incomplete line at the end. Everything before it is intact.
Tools that slurp the whole file (`jq -s`, `json.load` over a joined string) will
fail on such a file after having been perfectly able to read all of it but the
tail; line-at-a-time readers just skip the last line.

The model's replies are recorded as the **chunks** the daemon emitted, not as
one message per turn. Concatenate the `agent_message_chunk` updates between two
`prompt_submitted` records to get the reply as it was rendered.

## `n` counts; `seq` is not a count

> The per-file `n` counter, not the bus `seq`, is the contiguity guarantee.
> `seq` is minted daemon-wide and is expected to skip in every file; a reader
> who treats a `seq` skip as a gap is wrong, and the record documentation says
> so (informed by LESSON-503).
>
> — REQ-611, Assumptions

`n` starts at 1, advances by one per line, and never has a hole: records the
sink could not keep up with are counted and written as a `transcript_gap` line
*before* the record that follows them. `seq` is the daemon's own numbering
across every session, so a file that holds one session's records sees only its
own subset of it, and sink-local records carry no `seq` at all — they never
travelled on the bus, and a fabricated one would be a claim about ordering the
sink cannot make.

Within a turn the order is the order the daemon produced things in: a
`prompt_submitted` precedes that turn's `route_decided`, a `tool_call_input`
precedes its `tool_result`, and a `cost_recorded` follows the model call it
prices. A tool-using turn makes two model calls and prices each, so the exact
interleaving of a turn is not fixed.

## The record kinds the sink writes

These eight exist only in the file. They are not protocol events, have no wire
name, and are never published to any client.

| `kind` | Written when | Fields |
|---|---|---|
| `transcript_opened` | the file is created for a session — always the first record | `daemon_version`, `root` (the session root, display form), `redact` (the `[privacy] redact` posture at open), `max_record_bytes` (the budget every truncation in this file was measured against), `seq_at_open` |
| `prompt_submitted` | a prompt is accepted for the session | `turn_id`, `prompt` (the blocks as received — `{"type":"text","text":…}`), `skill` (`{"name":…,"raw_arguments":…}`, only when the line was a `/name`) |
| `tool_call_input` | the harness dispatches a tool call | `tool_call_id`, `tool`, `input` (as the harness parsed it) |
| `tool_result` | a tool returns to the harness | `tool_call_id`, `status` (`pending`, `in_progress`, `completed`, `failed`), `output` |
| `permission_decided` | a permission request is answered | `request_id`, `option_id`, `remembered` |
| `transcript_gap` | the sink fell behind and dropped records | `dropped`, and `seq_before` / `seq_after` where a bus record on either side of the hole names one |
| `transcript_resumed` | `/transcript on` after an `off`, in the same session and the same file | `seq_at_resume` |
| `transcript_closed` | recording stops | `reason` (`session_ended`, `session_command`, `daemon_shutdown`, `write_failure`), `records` (the final `n` — this record's own, so you can check the file is whole against the last line you parsed) |

A file whose last line is not `transcript_closed` was cut short: the daemon was
killed, or the machine went down. That is a different story from
`transcript_closed { "reason": "write_failure" }`, which says the sink stopped
because a write failed and nothing after it was recorded.

## The record kinds the bus writes

Every other line is a session-scoped event envelope, recorded in its **wire form
unchanged** — the same object a fully attached client would have seen. The
envelope's `event` is re-spelled as the line's `kind`, its `session_id` and
`seq` are lifted onto the line, and everything else survives byte for byte. So
`session_update`, `route_decided`, `cost_recorded`, `privacy_block`,
`permission_request`, `skill_invoked` and the rest appear under their own names,
with their own fields, documented by the protocol rather than by this page.

Two things never appear:

- **another session's records** — one file holds exactly one session;
- **daemon-scoped events** — model lifecycle, client attach, grant mints. They
  belong to no session, and a file its owner may share should not carry other
  sessions' activity.

## Truncation is marked, never silent

Any string value in a record — at any depth, so a prompt block's `text` and a
streamed chunk's `update.text` too — longer than `max_record_bytes` is cut, and
the line then carries `truncated: true` and `original_bytes`. `original_bytes`
is the total size the cut fields held before the cut, and the two fields read
the same whether a byte or a mebibyte was removed.

Two edges worth knowing when you compare lengths:

- a field of **exactly** `max_record_bytes` is not cut and carries no marker;
- a cut lands on a UTF-8 character boundary at or **below** the budget, so a cut
  field can be a byte or three short of `max_record_bytes` rather than over it.

## What is never in the file

- **Credentials.** Keys entered through setup flows are keychain references in
  every event already, and stay references here: no record carries a key, a
  token, or a grant value.
- **The remembered-grant key** on a `permission_decided`: the option id and the
  remembered flag only, because the key would turn a decision record into a map
  of what the session is permitted to do.
- **The wire to the model.** No composed provider request, no raw provider
  response. The transcript records the session's surface, not its egress.
- **The transcript's own path**, in anything published. Clients are told that
  recording started or stopped; only the connection that asks is told where.

The `[privacy] redact` scan does **not** run over transcript records: it gates
egress, and a transcript does not egress. Prompt text and tool results are
recorded as the harness received them, which is why `transcript_opened` records
the redact posture that was in force — so a reader knows what the egress side
was doing while these bytes were being written.

## Five lines of `jq`

```sh
# everything, pretty-printed (jq stops at a partial trailing line, having
# printed every whole line before it)
jq . 20260903T101112Z-sess-0123456789abcdefghjkmnpqrs.jsonl

# what you typed
jq -r 'select(.kind == "prompt_submitted") | .prompt[].text' session.jsonl

# what the model said, chunk by chunk
jq -r 'select(.kind == "session_update" and .update.kind == "agent_message_chunk")
       | .update.text' session.jsonl

# every tool call beside the result that came back
jq -r 'select(.kind == "tool_call_input" or .kind == "tool_result")
       | "\(.n) \(.kind) \(.tool_call_id) \(.tool // .status)"' session.jsonl

# anything the sink had to drop, and anything it had to cut
jq -c 'select(.kind == "transcript_gap" or .truncated == true)' session.jsonl
```
