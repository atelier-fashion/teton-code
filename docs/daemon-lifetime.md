# The daemon's lifetime

`teton` (the CLI) is a thin client. All the work — sessions, routing, cost
accounting, the local model — lives in `teton-code`, the daemon. This document
is about when that daemon runs, and why the answer changed in v0.1.14.

## What changed

| | v0.1.13 and earlier | v0.1.14 onward |
|---|---|---|
| Started by | launchd, at login | the CLI, on demand |
| Registered by | `brew install` (service block with keep-alive) | nothing — opt-in only |
| Stops when | never; launchd restarts it | its last client disconnects |
| Memory when idle | the full model, continuously | none — the process is gone |
| After `brew upgrade` | keeps running the **old** binary | next session runs the new one |

The old arrangement had two concrete costs, both observed on 2026-08-09:

- **A standing memory tax.** On the large model band the daemon holds roughly
  17 GB resident. On a 48 GB machine that is a permanent third of the machine,
  with swap pressure, whether or not you have used `teton` that day.
- **A stale-binary window.** `brew upgrade` replaces the binaries on disk but
  cannot restart another formula version's launchd agent. A daemon was found
  still serving v0.1.12 four hours after v0.1.13 was installed, because nothing
  ever restarts the service.

Inverting the lifetime removes both. The accepted tradeoff is that the first
session after an idle period pays the model load again.

## The default: exit with the last client

1. You run `teton`. It finds no daemon and starts one.
2. The daemon serves your session.
3. You exit. The daemon has no clients left, so it shuts down and the process
   is gone.

Two sessions at once are fine — the daemon counts its clients and only the
**last** disconnect stops it.

### What delays the exit

Shutdown waits for work that is genuinely in flight:

- a prompt turn that is still running — it finishes, and its cost-ledger row is
  written, before the daemon exits;
- a model download, verification, or load — a multi-gigabyte download is never
  killed because you closed a terminal.

Waiting for *you* is not work: a first-run model proposal you never answer does
not keep the daemon alive.

### If a new session arrives mid-shutdown

Either the shutdown is cancelled and your session is served by the same daemon,
or — if it had already committed to exiting — the handshake is refused, your CLI
starts a fresh daemon, and your command runs there. You will not notice either
way. What cannot happen is two daemons: the exiting one holds the single-instance
lock until after it has removed its socket, and a successor waits for that lock
rather than giving up.

## Keeping one running all the time

```sh
brew services start teton
```

This is the explicit opt-in. It registers `teton-code` with launchd, starts it
now, and brings it back after a reboot. The daemon runs under
`--shutdown-policy never`, which the formula passes for you.

That flag is not optional decoration. launchd supervision and an exit-on-idle
daemon are a flap: the daemon would exit when your last session ended, launchd
would restart it, it would reload the model, and round again. Passing the policy
explicitly is what makes the two agree.

Stopping it:

```sh
brew services stop teton
```

## Migrating an existing install

A formula upgrade cannot unload the launchd agent registered by an older
version, so this one step is manual. If you ever ran `brew services start teton`
(or installed a version that did it for you), run:

```sh
brew services stop teton
```

once, after upgrading. If you skip it you keep the old always-on behaviour —
nothing breaks, but you also keep the standing memory cost and the stale-binary
window that the new lifetime removes.

## Configuring it

The policy is one setting. In `config.toml`:

```toml
[lifetime]
shutdown = "on-last-disconnect"   # the default
```

```toml
[lifetime]
shutdown = "linger"               # exit N seconds after the last client
linger_seconds = 300
```

```toml
[lifetime]
shutdown = "never"                # never self-terminate
```

`linger` is the middle ground, and the one to reach for if you run `teton` from
scripts in quick succession and would rather not pay the model load each time.

The same setting can be given as a flag or an environment variable, which is how
the Homebrew service block passes `never`:

```sh
teton-code --shutdown-policy linger --linger-seconds 300
TETON_SHUTDOWN_POLICY=never teton-code
```

Precedence is **flag > environment > config file > default**, and the daemon
reports which one won on its first line of output:

```
teton-code: shutdown policy linger(300s) (from config)
```

An unrecognized value refuses to start rather than falling back to the default —
a typo that silently produced exit-on-last-client would be exactly wrong for an
always-on service.

## Version skew

When the CLI attaches to a daemon built from a different version, it prints one
line naming both and the remedy. This matters more under the on-demand lifetime
than it looks: it is a *warning*, not a refusal, because the two builds usually
speak the same protocol version and work together fine. The protocol check only
refuses when the versions are genuinely incompatible.

Under the default lifetime the remedy is just to end your sessions — the daemon
goes with them, and the next one is built from whatever is on disk now.

## Troubleshooting

**"could not reach the daemon after autostart"** — the CLI started `teton-code`
but it never answered. The message quotes the daemon's own log; the commonest
cause is a config file it refused to load. Full log:
`$(brew --prefix)/var/log/teton/teton-code.err.log`, or
`~/Library/Application Support/teton/tetond.log` for an unmanaged daemon.

**A daemon is running when I expect none** — check whether a model download or
load is in flight (`teton model status`); those defer the exit by design. Also
check `brew services list`: a service registered before the upgrade is still
always-on until you stop it.

**Is a daemon running right now?** `teton doctor` reports the socket, the daemon
and its version, the model state, and your providers.
