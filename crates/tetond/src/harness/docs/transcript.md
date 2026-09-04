# Transcript — the record of a session, and why you cannot read it

A transcript is a per-session JSONL file the daemon writes: every event on the
bus, plus the things the bus does not carry (prompt text, tool input, tool
result, permission answers), in the order they happened.

**You cannot check whether it is on, and you cannot read it.** Both facts have
one cause and it is deliberate — see "Why your tools refuse" below. When a user
asks whether the transcript is on, the answer is one sentence: *type
`/transcript` — it prints the state and the file's path; I cannot run it.* Do
not search the repository. Teton's own configuration is never inside the
repository you are working in, and a file you find there named for another tool
is another tool's file.

## Two switches, two lifetimes

| Switch | Where | Lifetime |
|---|---|---|
| `[transcript] enabled` | `config.toml` in Teton's state directory | durable — the default every new session starts at |
| `/transcript on` / `/transcript off` | typed in a session | that session only, never written to disk |

`/transcript` with no argument reports the effective state: the config default,
then any session override, then `false` if writing stopped on its own. It also
prints the file's path, the number of records written, and the number dropped.

The state is **effective**, not stored — a session that started enabled and was
turned off reports `off`, and a session whose disk filled reports `off` with the
reason. So there is no single setting anywhere that answers "is it on"; only
`/transcript` composes the answer, which is the other reason to name the command
rather than go looking.

## Where the files are

Teton's data directory, under `transcripts/`, one file per session. The
directory can be moved with `[transcript] dir`. Files are pruned by age at
daemon start and at every transcript open; `[transcript] retain_days = 0` never
prunes. `[transcript] max_record_bytes` bounds a single field before truncation,
so one enormous tool result cannot make a file unbounded.

## Why your tools refuse

The transcript directory is a **denied prefix** on the tool jail. `read`, `glob`,
`grep` and every walker refuse it — not because its contents are private in the
privacy-boundary sense, but because a transcript is the record *of* the session
and letting the session read its own record is a loop with no floor. The refusal
is unconditional: it does not depend on any privacy setting, and it covers last
week's files as well as the one being written now.

`shell` is the one tool that can reach any file on the machine, and it is not an
exception to the rule so much as outside the daemon's reach. Do not use it to
read a transcript. If a user wants to see one, `/transcript` prints the path and
they can open it themselves.

## What is in a file

One JSON object per line. The first is `transcript_opened`, recording the daemon
version, the session root, the bus sequence number the file starts at, and
whether `[privacy] redact` was on while it was written. After that: every bus
event in wire form, and the in-process records for what the bus does not carry.

A `transcript_gap` record is written in front of the next record whenever the
sink had to drop something, carrying the count. So the file's own record count
never has a silent hole — a reader can always tell a quiet session from a lossy
one.
