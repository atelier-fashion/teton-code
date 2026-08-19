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

**The daemon runs on demand.** `teton` starts `teton-code` when it needs one and
the daemon exits when your last session ends, so nothing holds the local model
in memory while you are not using it. The tradeoff is deliberate: the next
session pays the model load again.

If you would rather keep one running permanently — worth it if you start
sessions constantly, and it costs the model's memory continuously — that is one
command:

```sh
brew services start teton
```

That registers `teton-code` with launchd, starts it now, and keeps it across
reboots. If the registration refuses with a tap-trust error, run
`brew trust atelier-fashion/tap` once — Homebrew 6 treats the fully-qualified
name in the install command as self-authorizing, but not the short name that
`brew services` resolves.

> **Upgrading from v0.1.13 or earlier?** Those versions ran the daemon under
> `brew services` with keep-alive, so it started at login and could not be
> stopped short of `brew services stop teton`. Run that once after upgrading to
> pick up the on-demand lifetime. Nothing else changes.

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
| `/clear` | Drop this session's retained conversation; the next prompt starts fresh |
| `/cd [path]` | Move this session's root — the directory tools are scoped to — and clear the conversation; bare, print the current root |
| `/verbose` | Toggle routing and turn-end notices for this session |
| `/effort [level]` | Show, or change, the global reasoning effort |
| `/permissions [level]` | Show, or change, what this session may run without asking |
| `/web setup` | Set up web lookup: pick a tier, name a backend, confirm before anything is written |
| `/web allow` | Lift this session's web taint restriction (grants no new tier) |
| `/web refresh <url>` | Drop a URL's cached copy so the next lookup re-fetches |
| `/quit` (or `/exit`) | End the session (same as Ctrl-D) |

A session's tools are scoped to one directory — its **session root**: the
directory you ran `teton` from, or the one you name with `teton --cwd <path>`
(a relative path resolves against your shell, `~` expands, and a path that does
not exist or is not a directory is refused before the session starts). The
banner's `cwd:` line shows it. Outside a project — your home folder, `/`, or a
plain directory — the session says so under the banner, because every search
then walks all of it and no project's privacy boundaries apply; `/cd <path>`
moves the root of a live session and starts the conversation fresh.

Everything you would otherwise open a second terminal for is here too. The ten
commands below that have a `teton …` twin *are* that twin — the same arguments,
the same error messages, the same renderer, the same daemon call — so the two
surfaces cannot describe your machine differently:

| Command | Effect |
|---|---|
| `/provider list` | The providers registered on this machine, with what each one calls |
| `/provider add <id> --kind … --endpoint … --model …` | Register one by hand; the session confirms first, then asks for the key echo-off — never typed on the line |
| `/boundary list` | The path globs whose content never leaves this machine |
| `/boundary add <glob> [--mode local-only\|redact-then-remote]` | Add a privacy boundary |
| `/policy show` | The effective routing table: every tier, every category, where each resolves now |
| `/policy set-tier <tier> <provider> [--fallback <id>]` | Route a tier |
| `/policy set-category <category> <provider> [--fallback <id>]` | Route one category ahead of its tier |
| `/model list` | The catalog, each entry's fit for this machine, and the selection |
| `/model status` | The recorded model decision and the weights' install state |
| `/doctor` | Diagnose the daemon, socket, model state and providers, over this session's own connection |

Two more `/provider` commands are session-first rather than twins:
`/provider setup [vendor] [tier]` registers a provider and routes a tier to it,
guided, confirming before anything is written (there is no `teton provider
setup`); and `/provider test <id>` makes one consented call to a provider and
reports the provider's own answer, exactly as `teton provider test <id>` does
from a shell.

A line you type that begins with `teton` and names a real subcommand runs the
session command for it, after one line saying so — `teton provider list` prints
`>> teton provider list → /provider list` and then the listing. It is the same
command, not a subprocess: no second connection and no model call. The binary's
own `--yes`/`--verbose` flags on such a line are ignored, and the session says
so; `teton provider --help` prints the family's help. A line that is not a
command (`teton is slow today`) still reaches the model unchanged, and `teton
uninstall` is refused with the reason — it would stop the daemon under the
session running it.

Two limits. Arguments are split on whitespace and quotes are **not**
interpreted, so a value containing a space has to be given to `teton` in a
shell. And the four commands that write (`/provider add`, `/boundary add`,
`/policy set-tier`, `/policy set-category`) are typed-input only: piped into a
session they change nothing and name the shell command instead. Every
daemon-side gate is the same one the shell twin meets.

To send a prompt that genuinely starts with a slash — a pasted path, say —
double it: `//usr/local/bin/x — why?` asks the model about `/usr/local/bin/x`.

### Permission levels

How much the agent asks before it acts is one named setting. At a terminal it
shows in a status row under the entry frame, beside the reasoning effort — the
two session-wide settings that silently change what every later turn does and
costs. Both also print on demand (`/permissions`, `/effort`), which is what
makes them readable over a pipe, where the row is not drawn.

| Level | What runs without asking |
|---|---|
| `guarded` | Reads. Edits and shell commands ask first. **The default.** |
| `edits` | Reads and edits. Shell commands still ask. |
| `plan` | Nothing that changes anything — every mutating tool is refused, so you get a plan rather than a prompt for each step. |
| `full` | Everything. |

```sh
/permissions          # what am I in?
/permissions edits    # stop asking about edits; keep asking about shell
```

A level lasts for **one session**. It is never written to disk, so a `full` you
set today cannot outlive the window you set it in — every new session starts at
`default_permission_level`, which is `guarded` unless your config says
otherwise:

```toml
[permissions]
default_level = "edits"
```

Two things worth being precise about, because both are easy to assume wrongly:

- **No level affects privacy.** `full` grants tool *execution*. It does not
  touch the `local-only` boundary, and it does not unpin a session that has read
  unknown-provenance content — such a session stays on the local tier at every
  level. Which tools may run and what may leave the machine are separate
  questions with separate answers.
- **A level outranks a remembered answer.** "Allow for this session" is an
  answer to a question the level decides whether to ask, so switching to `plan`
  refuses a tool you allowed earlier — and switching back restores it, because
  the answer was never discarded.

Tools a level has never heard of — anything an MCP server supplies — follow the
level's default rather than a name list: they ask at `guarded` and `edits`, are
refused at `plan`, and run at `full`.

### Upgrading

```sh
brew upgrade teton
```

On the default on-demand lifetime that is the whole thing: an upgrade replaces
the binaries on disk, and the next session starts a daemon from the new ones.
Exit any session you already have open — that daemon is still the old build
until it does.

If you opted into the always-on daemon, restart it too, because nothing else
will:

```sh
brew services restart teton
```

You will be told either way. When the CLI attaches to a daemon built from a
different version, it prints one line naming both versions and what to do about
it. That check is separate from — and stricter than — the protocol negotiation:
two adjacent releases usually speak the *same* protocol version, so a stale
daemon handshakes perfectly happily and would otherwise serve every command
silently. When the protocol versions genuinely disagree, the handshake is
refused outright and the error says which half is behind.

`teton doctor` names the running daemon's version at any time.

[`CHANGELOG.md`](CHANGELOG.md) records what an upgrade changes on a machine that
was already running — in particular any release that changes where your data
goes without you having changed a setting. Read it before a restart, not after.

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

### Hooking up an external model

```bash
# Register the provider. Every remote kind needs --kind, --endpoint and
# --model; the API key is read from TETON_PROVIDER_KEY or prompted for, and
# stored in the OS keychain — never written to a file. --endpoint is the full
# request URL, posted exactly as given, not a vendor's base_url:
teton provider add opus --kind anthropic \
  --endpoint https://api.anthropic.com/v1/messages --model claude-opus-5
teton provider add kimi --kind openai-compatible \
  --endpoint https://api.moonshot.ai/v1/chat/completions --model kimi-k3

# Route work to it — a whole tier (reflex | scan | build | think), with an
# optional fallback, or a single category ahead of its tier:
teton policy set-tier think opus --fallback kimi
teton policy set-category review opus

# Inspect:
teton policy show
teton provider list
teton doctor
```

Config lives in `config.toml` in Teton's state directory (override with
`TETON_CONFIG`); API keys are never stored in it.

Recipes for Anthropic, OpenAI, Moonshot (Kimi), DeepSeek, Ollama and
Grok (xAI) ship inside the binary. Ask in a session and the agent hands back
the exact `provider add` and `policy set-tier` commands for any of them, filled
in — it never runs them for you, and it never asks you to type a key into the
conversation.

Two promises, both made visible:

- **Cost control** — a live cost meter with per-phase attribution and measured
  savings vs. an all-frontier baseline (`teton cost`).
- **Privacy boundaries** — mark paths as *local-only* and their content never
  leaves your machine. Enforced at the daemon's single egress point, verified by
  egress-capture tests, not vibes.

### Turning on web lookup

Web lookup ships **off**. Nothing is fetched, searched, or sent anywhere until
you turn it on, and turning it on is a *user* act. The model is told the
capability exists, that it is off, and how to enable it — so a question that
needs the live web gets a refusal that names the way forward instead of a hunt
through your repository for Teton's config. It cannot take that way forward
itself: `/web setup` is a client command, and a model that emits a tool call by
that name reaches no tool.

In a session:

```
/web setup
```

It asks, in this order, and writes nothing until the last answer:

1. **tier** — `1) fetch_user_url` (fetch a URL you pasted into the session),
   `2) fetch_any_url` (also fetch a URL the model composed), `3) search` (also
   search through a backend you name). Each tier includes the ones before it.
   On a machine that cannot serve `search`, that row is marked
   `(unavailable: …)` with the reason, and choosing it is refused rather than
   written.
2. **search endpoint** — `search` only; the three shapes below are printed
   above the prompt.
3. **does this backend need an API key? [Y/n]** — answer `n` for a keyless
   self-hosted backend.
4. **auth header template** — Enter takes the offered default: the matched
   backend's own header (Brave and Kagi each want their own), or
   `Authorization: Bearer {key}` for a backend the daemon does not name.
5. **API key** — not echoed. It goes straight into the OS keychain as
   `keychain://teton/web-search`; only that *reference* is written to your
   config, and only that reference crosses the socket to the daemon.

Then it prints the exact `[web]` TOML it would write and the host searches would
go to, and asks `write this to your config? [y/N]` — default **no**. Answer `y`
and the table is written atomically and is live immediately: that session and
every other open session pick the capability up on their next turn, with no
restart. Enter, an empty answer, EOF or Ctrl-C at any prompt leaves the config
untouched and stores no key. On a piped (non-terminal) session the command asks
nothing at all and prints the hand-edit instructions below instead.

Enabling is not consenting. A lookup asks before anything leaves the machine
unless that tier has already been granted — for the session, or permanently via
`[web] permission_allow`. The durable-consent key is written only when you
answer "enable permanently" at a lookup prompt, never by `/web setup`; once it
is written, lookups at that tier stop asking. The three tiers are consented
separately, so an answer about one is never an answer about the others.
Removing a tier from `permission_allow` restores asking for it, and takes
effect when the daemon next starts.

Backends whose shapes are known to work:

| Backend | Endpoint | `search_auth` |
|---|---|---|
| SearxNG (self-hosted) | `http://localhost:8888/search?format=json` | none — keyless |
| Brave Search API | `https://api.search.brave.com/res/v1/web/search` | `X-Subscription-Token: {key}` |
| Kagi Search API | `https://kagi.com/api/v0/search` | `Authorization: Bot {key}` |

The `?format=json` on a SearxNG endpoint is load-bearing: without it the
instance answers with a web page rather than JSON.

<!--
Drift check. One in-tree source for the three backend rows above:
`crates/tetond/src/web_setup_catalog.rs`. The rows are a prose mirror of it —
change the catalog first, then this table. The `[web]` key names and the
`keychain://teton/web-search` reference below are the bundled guide's strings,
and the guide is itself checked against that catalog.

Where every surface's sync is enforced:
  - this table: `the_readme_backend_rows_and_the_catalog_agree` in
    `crates/tetond/tests/web_setup_contracts.rs` (REQ-573 BR-5) reads these rows
    and the catalog and fails the build in either direction — a catalog endpoint
    or header shape missing from a row, or a row naming a URL no suggestion
    offers;
  - the same file (REQ-572 AC-8) enumerates the catalog typed, parsing no one's
    source text, so a backend added to the catalog without a contract fixture
    fails CI;
  - `crates/tetond/src/harness/self_config.md`, the guide bundled into the
    system prompt, is checked against the catalog in both directions
    (`the_bundled_guide_and_the_catalog_agree`) — a template only one side
    names fails, whichever side moved;
  - `crates/teton/src/web_setup_ui.rs` keeps no copy: `/web setup` renders the
    catalog the daemon hands it on `web/setup_plan`, the piped instructions
    included;
  - the fenced `[web]` block below: `README_WEB_BLOCK` in
    `crates/tetond/tests/config_preservation.rs` is that block copied
    byte-for-byte, and `the_fixture_is_the_readmes_own_block_byte_for_byte`
    reads *this file* at test time and fails on any edit inside the fence the
    fixture did not follow (REQ-574 AC-1 wants the README's own block as the
    preservation test vector, not a paraphrase of it);
  - `crates/teton-core/src/config_doc.rs` — `HAND_WRITTEN_CONFIG` embeds the
    same block again, as the delta engine's own unit-test vector. Nothing there
    reads this file, so that copy is not self-enforcing: move it by hand when
    the fence moves.
-->

**Or write the table by hand.** `/web setup` exists because it is live in the
session; a hand-edited config is read when the daemon next starts. Neither
choice costs you the other: every daemon-side save — `/web setup`'s commit, an
"enable permanently" answer at a lookup prompt, a provider registration, a
startup migration — edits the file in place rather than re-serializing it, so a
save moves exactly the keys its own operation is about. Comments, key order,
and keys this build has never heard of survive it untouched.

That holds inside lists like `[[providers]]` too: registering a provider appends
an entry and leaves the ones already there exactly as you wrote them, and a save
that changes one entry touches only that entry. The one exception is a list a
save genuinely *reshapes* — an entry removed, or the order changed — where there
is no longer any way to match the old entries to the new ones; that list alone is
rewritten whole, and comments inside it go with it. Everything outside it is
still untouched.

```toml
[web]
# "off" (default) | "fetch_user_url" | "fetch_any_url" | "search"
tier = "search"
search_endpoint = "https://api.search.brave.com/res/v1/web/search"
# A reference, never a raw key — the value lives in the OS keychain.
search_key_ref = "keychain://teton/web-search"
# The header the key rides, `{key}` marking the secret. Absent means
# `Authorization: Bearer {key}`, and it is refused with no key reference
# beside it — a header shape with no secret to place would do nothing.
search_auth = "X-Subscription-Token: {key}"
# Optional; constrains model-chosen destinations only. Absent = unrestricted,
# present but empty = nothing allowed. A URL you pasted yourself is exempt.
allowed_domains = ["docs.rs", "crates.io"]
# Cache freshness window in seconds; 0 means no caching. Defaults to 900.
cache_ttl_secs = 900
```

The keychain entry `search_key_ref` names is created by `/web setup`; to write
it by hand, run `security add-generic-password -s teton -a web-search -w` — put
`-w` last and it prompts for the key instead of leaving it in your shell
history.

A keyless backend is the same table with `search_key_ref` and `search_auth`
left out. `tier = "search"` with no `search_endpoint` is the one combination
the daemon refuses to start on, and it names the missing key when it does.

What a save edits is the document on disk, not the copy the daemon booted on —
so an edit you make while it runs rides along instead of being clobbered, and
is still only *read* at the next start. The other side of that is a refusal: a
file that no longer parses, or that parses but fails the validation startup
runs, stops the next daemon-side save with the reason rather than overwriting
your work to make the save succeed.

Config lives in `config.toml` in Teton's state directory
(`$XDG_RUNTIME_DIR/teton` when that is set, else
`~/Library/Application Support/teton` on macOS; `TETON_CONFIG` overrides both).
Keys are never stored in it — `search_key_ref` names a keychain entry under the
same `teton` service every Teton credential is filed under, which is why writing
one is the CLI's job and never the daemon's.

The `search` tier also needs the local model: every query is scanned before it
leaves the machine, so a machine with no local model can fetch but cannot
search. That is why the tier menu marks `search` unavailable there rather than
letting you configure a tier that would refuse every query.

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
