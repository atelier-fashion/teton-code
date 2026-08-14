# Reading `teton doctor`

`teton doctor` is the first thing to ask a user for when something is wrong. It
is read-only and prints, in order: the socket and lock paths, whether a daemon
is running and which build it is, the providers config holds, and two notices
about the local model and provider reachability.

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
