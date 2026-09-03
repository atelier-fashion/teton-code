# Reading `teton doctor`

`teton doctor` is the first thing to ask a user for when something is wrong. It
is read-only and prints, in order: the socket and lock paths, whether a daemon
is running and which build it is, the providers config holds, the transcript
posture, and two notices about the local model and provider reachability.

## Version skew is the commonest fault

The socket and lock filenames are stable across releases, so upgrading the
binaries without restarting the service leaves a new CLI talking to a daemon
from the previous release. Doctor reports it as `daemon: reachable, but it
rejected this CLI`, and the handshake error names which half is old:

- the daemon is older — the upgrade replaced the files on disk without
  restarting the process already running. Fix: `brew services restart teton`,
  or stop the running `teton-code` and let the next `teton` command start a
  fresh one.
- the CLI is older — fix: `brew upgrade teton`.

A reply the CLI cannot parse ("does not match this build's protocol types") is
the same cause wearing a different message: two builds that agree on the
protocol version while a message shape moved underneath it. Same remedy.

`daemon: not running` is not a fault. Running `teton` autostarts one.

## The local model

The local-tier lifecycle is event-driven, so doctor does not report the weights
as present or absent; it says so and points at a session. Start one and watch
the probe, download and benchmark events instead. The weights live in the state
directory with the rest of Teton's data, so a "model missing" answer usually
means a state directory that was moved or cleared, not a broken install.

## Providers and keys

Doctor lists the providers config holds. It does not probe them: reachability
is decided by the daemon at call time and the CLI has no network path of its
own, so a provider listed here is a registered one, not a reachable one. Keys
are never in config — they live in the OS keychain under the service `teton` —
so a provider registered without its key looks perfectly healthy here and fails
at its first call. See topic `providers` for what a 401 usually means.

Each row carries `window:` — the declared context window, or `unknown`. Doctor
advises on one that declares none, and on an inert `context_budget_cap` (at or
above its window). Topic `context`.

## The transcript line

`transcript:` reports the **durable** posture and nothing about the session
asking: whether `[transcript] enabled` is on in config, the directory
transcripts are written to, and how many days files are kept. Off is the stock
answer, and off means no directory and no file — nothing is recorded until
someone opts in.

The other switch never appears here. `/transcript on` and `/transcript off`
last one session and are never written to config, so doctor cannot see them;
bare `/transcript` answers for the session it is typed in, with that session's
file path and whether recording stopped. No tool may read the directory doctor
names — `read`, `edit`, `grep` and `glob` refuse it, `shell` excepted — so
asking to open a transcript gets a refusal, not a file.

## Where config lives

`config.toml` in Teton's state directory: `$XDG_RUNTIME_DIR/teton` when that is
set, otherwise `$HOME/Library/Application Support/teton` on macOS, and — when
neither variable is set, which is unusual and usually means a stripped
environment — the OS temp directory's `teton`. `TETON_CONFIG` overrides all
three. Cost history and the downloaded model sit beside it. Keys never do.

The third case is worth recognizing rather than debugging: a daemon started
without `HOME` binds under the temp directory, so it has its own empty config
and its own socket, and doctor reports "not running" from any shell that has
`HOME` set. The paths doctor prints are the first line of that answer.
