# Teton Code

**An AI coding agent that routes work across a range of models — so you spend
frontier-model money only where frontier-model intelligence matters.**

> ⛰️ Named in the Tetons, where the idea was born. A mountain range is a range —
> of peaks, of sizes, of routes. So is your model lineup.

## Install

```sh
brew install atelier-fashion/tap/teton
teton
```

That is the whole install: two binaries — `teton` (CLI) and `teton-code` (the
daemon) — with no Rust toolchain, no cmake, and no feature flags to get right.
On its first run `teton` notices there is no daemon and offers to register
`teton-code` with launchd (`brew services start teton`, run for you) so it
starts now and survives reboots; press return to accept, or decline and it
starts an unmanaged daemon for the session and never asks again. The manual
`brew services start teton` keeps working, before or instead of the offer.

If the service registration refuses with a tap-trust error, run
`brew trust atelier-fashion/tap` once — Homebrew 6 treats the fully-qualified
name in the install command as self-authorizing, but not the short name that
`brew services` resolves.

**The model arrives on first run, with your consent.** The install ships no
weights. The first `teton` run proposes a local model matched to your hardware
and names the download size and the RAM it needs *before* fetching anything;
nothing is downloaded until you accept. `teton model list` shows the catalog and
each entry's fit for your machine.

Afterwards, `teton doctor` diagnoses the daemon, its socket, the model state and
your providers; the daemon's own logs live under
`$(brew --prefix)/var/log/teton/`.

### In the session

Inside an interactive `teton` session, `/`-prefixed lines are commands, not
prompts:

| Command | Effect |
|---|---|
| `/help` | List the commands (and the escape hatch below) |
| `/cost` | The live cost meter — same report as `teton cost` |
| `/model` | One line naming the model the local tier is on |
| `/model set <name>` | Change the local model (typed input only; asks before an above-RAM-floor pick) |
| `/verbose` | Toggle routing and turn-end notices for this session |
| `/quit` (or `/exit`) | End the session (same as Ctrl-D) |

To send a prompt that genuinely starts with a slash — a pasted path, say —
double it: `//usr/local/bin/x — why?` asks the model about `/usr/local/bin/x`.

### Upgrading

```sh
brew upgrade teton
brew services restart teton
```

The restart is the load-bearing half. An upgrade replaces the binaries on disk;
the `teton-code` already running is still the old one until something restarts it,
and `teton doctor` names the running daemon's version so you can tell which one
answered.

Forget it and nothing is silently wrong: the two halves negotiate a protocol
version when the CLI attaches, so a daemon left behind by an upgrade is turned
away at that point — every command answers with which half is stale and the
restart command, rather than failing partway through with an internal error.

### Uninstalling

```sh
teton uninstall
```

One command, the whole chain: it stops the `brew services` daemon, deletes the
state directory (`~/Library/Application Support/teton` — the downloaded model,
cost history, and config), removes the daemon logs and any provider keys in the
macOS keychain, runs `brew uninstall teton`, and removes the tap registration
(skipped automatically if another formula from the tap is still installed). It
shows the full plan — with the size of what it's about to delete — and asks
once before touching anything; `--keep-data` preserves the state directory for
a later reinstall, and `--yes` answers the prompt for unattended runs.

This is a `teton` subcommand rather than a `brew uninstall` hook on purpose:
Homebrew formulae have no uninstall hook (that's cask-only), and
`brew uninstall` doesn't even stop a running service — run bare, it removes
the binaries and strands the daemon, the model, and the logs. If you've
already done that, the leftovers are `brew services stop teton`,
`~/Library/Application Support/teton`, `$(brew --prefix)/var/log/teton`, and
`brew untap atelier-fashion/tap`.

### What runs where

| Platform | Release target | Local inference |
|---|---|---|
| macOS, Apple Silicon | `aarch64-apple-darwin` | Metal GPU acceleration |
| macOS, Intel | `x86_64-apple-darwin` | CPU only |
| Linux x86_64 (glibc) | `x86_64-unknown-linux-gnu` | CPU only |
| Windows | — | Not supported |

Remote models run the same everywhere; only the local base-camp model depends on
your hardware. On Linux, Homebrew installs the binaries, but `brew services` is
not a v1 claim — run `teton-code` yourself, or write your own systemd user unit.

### Verify a release

Every published release asset carries GitHub build provenance — the three
tarballs *and* the `checksums.txt` published beside them — and the macOS
binaries are signed. Both are checkable before you trust the bytes — `<target>`
is one of the release targets in the table above:

```sh
# Provenance: GitHub Actions built these exact bytes, from this repository, at a
# tagged release. The release pipeline runs this command as a gate over every
# published asset, checksums.txt included.
gh attestation verify teton-v<X.Y.Z>-<target>.tar.gz --repo atelier-fashion/teton-code

# macOS: the signature holds on BOTH binaries, and names the authority and the
# team it should. A green check on one binary says nothing about the other.
tar -xzf teton-v<X.Y.Z>-<target>.tar.gz
for b in teton teton-code; do
  codesign --verify --strict "$b" &&
    codesign -dvv "$b" 2>&1 | grep -E 'Developer ID Application|TeamIdentifier'
done
```

Each binary prints these two lines:

```
Authority=Developer ID Application: Atelier Fashion LLC (545BU9G9D6)
TeamIdentifier=545BU9G9D6
```

Both `v`s in `-dvv` are load-bearing: at verbosity 1 codesign prints
`TeamIdentifier=` and no `Authority=` line at all, so a perfectly signed binary
reads as anonymous.

Before this repository's first attested release, `gh attestation verify` reports
no attestations and exits non-zero with an HTTP 404. That is expected until the
first signed release ships — it means nothing has been attested yet, not that
your download was tampered with.

Linux artifacts are unsigned in v1; the attestation is the whole check there.

## What it is

Teton Code is a Claude Code–style agentic coding harness with two
differentiators:

- **Base camp: a slim local model, always with you.** Downloaded on first run
  (hardware-adaptive — the app probes your machine and benchmarks the best fit),
  running as a persistent daemon. It handles the always-on cheap tier: routing,
  summarization, commit messages, secret redaction, offline fallback.
- **Summits: bring your own models.** Register Anthropic, any OpenAI-compatible
  endpoint (DeepSeek, Kimi, Ollama, vLLM…), and Teton Code routes each phase of
  work to the tier you choose — architecture to a frontier model, implementation
  to a cheap one executing from well-specified task artifacts, mechanical I/O to
  the local daemon.

Two promises, both made visible:

- **Cost control** — a live cost meter with per-phase attribution and measured
  savings vs. an all-frontier baseline (`teton cost`).
- **Privacy boundaries** — mark paths as *local-only* and their content never
  leaves your machine. Enforced at the daemon's single egress point, verified by
  egress-capture tests, not vibes.

## Architecture

Engine/surface separation: all differentiating logic (router, workflow state,
privacy enforcement, cost accounting, provider adapters) lives in a local
daemon. Clients are thin:

1. **CLI (`teton`)** — first surface, MVP target.
2. **VS Code extension** — second surface, same daemon protocol.

Architecture decisions are recorded in
[.adlc/context/architecture.md](.adlc/context/architecture.md); the product
charter (business rules, acceptance criteria, open questions) is
[REQ-544](.adlc/specs/REQ-544-teton-code-charter/requirement.md).

## For contributors

Homebrew is how you *use* Teton Code. Building from source is how you work on
it — and it needs the toolchain the formula exists to spare everyone else.

Prerequisites: a Rust toolchain (the channel is pinned in
`rust-toolchain.toml`) and **cmake** — the `tetond/llama` feature compiles
llama.cpp from source.

```sh
cargo build --workspace --release --features tetond/llama
```

The feature flag is not decoration. Without `tetond/llama` the daemon builds
and runs, offers you a model, downloads it — and then cannot load it. Release
tarballs are always built with it (`tools/release/package.sh`), so an installed
daemon can serve the model the CLI offered.

Maintainers cutting a release: [docs/release-runbook.md](docs/release-runbook.md).

## Status

Early, and real. The daemon and CLI are implemented in Rust: first-run model
consent, local inference through llama.cpp, provider registration, routing
policy, privacy boundaries, and the cost meter all run on your machine today.
The first-run flow has been human-verified end to end on Apple Silicon and only
there — see [docs/manual-verification.md](docs/manual-verification.md), which
records the unrun platforms as unrun rather than assuming them.

Homebrew distribution is published by the release workflow on every `vX.Y.Z`
tag. If `brew install` cannot find the formula, no release has been cut yet and
the source build above is the way in.

## License

[MIT](LICENSE)
