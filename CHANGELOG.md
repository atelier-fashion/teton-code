# Changelog

Notable changes to Teton Code, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

This file is the **hand-written** half of a release, and it is published as
one. [`.github/workflows/release.yml`](.github/workflows/release.yml) generates
the rest of the GitHub Release body — platforms, signing, checksums — and lifts
the **topmost section below** into it verbatim, under an "Upgrade notes"
heading, via
[`tools/release/changelog-section.sh`](tools/release/changelog-section.sh).
That is REQ-548 OQ-3 ("generated, or hand-written?") settled as *both*.

Nothing here is *required* for a release to go out: an absent file or an empty
section publishes no Upgrade notes section and the release is otherwise
unchanged. What belongs here is what an *upgrade* does to a machine that was
already running — above all, anything that changes where data goes without the
user having asked for it.

## [Unreleased]

### Added

- **The `/` commands you already wrote, discovered and run (REQ-585).** A
  session now reads `~/.claude/skills/*/SKILL.md`, `~/.claude/commands/*.md`
  and the same two under the session root's `.claude/` — four globs, one level
  deep, no recursion — and registers one `/name` per file. **On a machine with
  a `~/.claude/skills` tree this is new commands appearing in a session that
  did not have them**: against the seventeen-skill ADLC toolkit, all
  seventeen register and none is skipped. A built-in row always wins a name it
  shares; the skill is listed as shadowed rather than dispatched.

  - **An invocation is one prompt turn.** `/name <rest>` expands to the file's
    body with `$ARGUMENTS` replaced by the rest of the line **as typed** (not
    split, quotes not interpreted) and `$1`…`$N` by its whitespace-split
    tokens, preceded by one line naming the command and its file; a body with
    no placeholder gets a closing `ARGUMENTS: <rest>`. From there it is an
    ordinary prompt: same classifier, routing, permission level, egress choke
    point and cost row. `/help` lists every skill with its source and closes
    with what was found and skipped, and why.
  - **Dynamic context asks, under the skill's own key.** A `` !`cmd` `` in a
    body inlines that command's output at expansion time. It runs under
    `skill:<source>:<name>` — **never** the `shell` tool's key, so an existing
    "allow always" on `shell` does not silently authorize it and a grant on one
    skill frees nothing else. At the default `guarded` (and at `edits`) the
    session lists every command of the invocation and asks **once**; `plan`
    does not run them; `full` runs them; piped into a session at a level that
    would ask, they are refused without a line of stdin being read. Anything
    not run leaves `` [dynamic context not run: `cmd` — reason] `` in the
    prompt, so the model is told rather than misled. Project-skill grants are
    dropped when `/cd` moves the root.
  - **Nothing in a skill file changes the session.** Frontmatter other than
    `name`, `description` and `argument-hint` — `allowed-tools`, `model`,
    `effort`, `context`, `agent`, `hooks`, `disable-model-invocation` — is
    inert and listed by `/verbose`. `CLAUDE.md`, agents and hooks are still not
    loaded. The model cannot invoke a skill; only you can.
  - **Carried whole or refused, never shortened.** A skill turn that does not
    fit its route's budget (REQ-586) is refused before anything is sent, naming
    the skill, its size, the budget and the bound — and the body alone is
    checked *before* consent is asked, so nobody approves four commands and is
    then told the turn was refused.
  - **`teton_docs skills`** is a new bundled topic, carrying the above and the
    fidelity note below.

  **Fidelity, stated rather than faked.** Teton does not translate Claude Code
  tool names and does not rewrite a body's references to `Agent`, `Task`,
  `Skill`, `Workflow` or subagents — there is nothing behind them here.
  Prompt-template skills work. A skill that dispatches subagents or invokes
  other skills (`/proceed`, `/sprint`, `/analyze`) degrades to what one model
  with `read`/`edit`/`glob`/`grep`/`shell` can do, and **stalls** at its first
  "invoke the skill" step.

- **A turn is assembled to fit the model it is routed to (REQ-586).** Every
  turn — local or remote — used to run under one budget sized for the local
  engine: 4,096 words and 32,768 bytes, whatever window the provider actually
  had. A prompt that did not fit had its oldest blocks dropped and its newest
  message middle-elided in place, and nothing told you. The budget is now a
  property of the **route**, derived once where the route is decided:

  - **Declare a window.** `teton provider add … --max-context <tokens>`, with
    an optional `--context-budget-cap <tokens>` to hold a large window to a
    smaller budget. `/provider setup` records the window from the shipped
    recipe when you take that recipe's example model, and `config/set` carries
    both keys. The recipes now ship verified windows: Anthropic
    `claude-opus-5` 1,000,000, OpenAI `gpt-5.6` 1,050,000, Moonshot `kimi-k3`
    1,000,000, DeepSeek `deepseek-v4-pro` 1,000,000, xAI `grok-4.6` 500,000,
    and Ollama `llama3.2` 4,096 — Ollama's *served* default, not the model
    card's 128k, and a declared window below the local default legitimately
    yields a smaller budget.
  - **Two currencies, and the bound is named.** A remote budget is
    `(window − 1,024) × 2/3` words and `(window − 1,024) × 2` bytes; on a
    remote route it is the byte guard that binds for prose and for code.
    `/verbose` ends the route line with
    `· budget 665,984 words / 2 MB (bound: window)` — one of `window`,
    `unknown window`, `redact scan`, `user cap`, `local engine`.
  - **Nothing is clamped in silence.** A new `context_pressure` event, and one
    CLI line that is never gated by `/verbose`, whenever blocks are dropped, a
    block is elided in place, or the context is re-fitted after a mid-turn
    reroute. An elided *newest* message is additionally a notice in the turn's
    own output. The in-prompt elision marker now names the route's window
    instead of always saying "local context window".
  - **`teton doctor` and `teton provider list` show the window.** A `window:`
    column on every provider row — `1m`, `unknown — context budget defaulted
    (set capabilities.max_context)`, or `(local engine)` — plus a doctor
    advisory for a provider that declares none, and for a
    `context_budget_cap` at or above its window (inert rather than invalid).
  - **A provider's "context length exceeded" is a typed outcome** (RPC
    `-32022`) naming the window and the assembled size. It does not retry, it
    does not fail over, and it does not count against the provider's health.
  - **`teton_docs context`** is a new bundled topic carrying all of the above,
    including the number worth knowing: the budget bounds one model **call**,
    and a single prompt may make up to 25 of them.

  With `[privacy] redact = true` the byte budget is additionally held to what
  the redact scan can cover (≈89 KB) and the bound reads `redact scan`; the
  word figure stays window-derived.

### Changed

- **The `digest` threshold scales with the route (REQ-586).** A tool result is
  condensed above the same ≈36.6% of the route's budget it has always been,
  rather than a fixed 1,500 words / 12,000 bytes — capped on every route at
  20,000 words / 160 KiB, so one enormous result is still digested. The local
  tier's numbers are byte-identical to before. Compaction still fires at 70%
  of either budget, and the `compact` duty's own prompt stays bounded to the
  local engine's window as the conversation grows.

### Upgrade notes

- **REQ-585 adds commands, not settings.** No config is rewritten and nothing
  is watched: discovery is four globs at launch and on `/cd`, a missing
  directory costs nothing, and a machine with no `~/.claude` sees a `/help`
  byte-identical to the one it has now. Nothing in a discovered file can change
  a permission level, a route or a boundary.
- **One privacy consequence to know before you invoke one.** A skill file rides
  its turn as a source, and dynamic-context output is unattributed exactly as
  `shell` output is — so on a machine with a privacy boundary configured, an
  invocation that ran a dynamic command **pins that turn to this machine**.
  Every one of the ADLC toolkit's seventeen skills runs one (the ethos
  include), so on such a machine they are all pinned to the local tier; the
  seven that exceed the local budget are then refused there rather than served
  remotely. There is no "run without dynamic context" option in this version.
- **REQ-586 moves no data and rewrites no config**: a provider with no declared
  window behaves exactly as it did, under the default budget — the difference
  is that Teton now says so instead of leaving you to guess.
- The protocol fields are **additive** and `PROTOCOL_VERSION` has not moved,
  so mixed builds degrade rather than break. An older **CLI** against this
  daemon ignores `max_context`, `context_budget_cap`, `budget_tokens`,
  `budget_bytes` and `bound`, and drops the `context_pressure` event; its
  route line is byte-for-byte the pre-REQ-586 one. An older **daemon** behind
  this CLI reports no window at all, and the row reads `window: not reported`
  rather than claiming one is unset. And an older client re-registering a
  provider cannot zero a window you declared: the registration merges these
  fields, so an absent value preserves what is stored.

## [0.1.23] - 2026-08-19

### Added

- **The session knows where it stands, says so when it is nowhere, and a
  search can no longer crawl your disk (REQ-583).** Launched from a home
  folder and asked to "look in my development folder for the Teton repo",
  Teton walked the whole of `~` — macOS asked for Music, then Photos, then
  "data from other apps", then Desktop — and still found nothing. Three
  changes, one upgrade:

  - **Every prompt carries the session root.** One line —
    `Session root: ~/Documents/GitHub/teton-code (project teton-code, branch
    main). Platform: macOS.` — so the model reasons from where it actually is.
    A refusal to read outside it names the root (``path `x` is outside the
    session root ~/…``), and the tools say "session root", never "repository".
    It is paid for inside the same resident-prompt ceiling as before: the
    guide's `[web]` key reference moved into the `teton_docs web` topic; no
    fact was lost.
  - **Starting outside a project is announced, and movable.** `teton` run from
    your home folder, `/`, or any directory without a `.git` or build manifest
    prints one notice naming the root and its consequence ("every search walks
    all of it, and privacy boundaries declared for a project do not apply
    here") with the two remedies: `teton --cwd <path>`, or `/cd <path>` inside
    the session, which moves the session's root, clears the conversation
    (provenance identities are root-relative), and says so; `/cd` alone prints
    the current root. `--cwd` refuses a missing directory before anything
    connects. Session "allow always" answers survive a `/cd`, exactly as they
    survive `/clear`.
  - **Searches are bounded and honest.** `glob` and `grep` run under one walk
    policy — 100,000 entries or 10 seconds — and say when they stopped
    (`... (stopped after …; narrow the pattern, or move the session root with
    /cd)`); `glob` returns directories when the pattern names one
    (`**/teton-code` finds the folder); from a home-rooted session the
    top-level `Library`, `Music`, `Pictures`, `Movies`, `.Trash` and dev caches
    are not entered unless you name them — those are the trees macOS gates
    behind a consent dialog — and folders that could not be read are reported
    instead of silently skipped (on macOS the line says a consent dialog may be
    why; a timed-out shell command from such a root says the same). `grep`
    skips devices, FIFOs and multi-GB files and caps a match line; `read` and
    `edit` refuse anything that is not a regular file.

  No wire-format break: `SessionCreateResult.root`, `session/set_cwd` and the
  `session_root_changed` event are additive, so an older CLI keeps working
  against this daemon and this CLI against an older daemon (it says "this
  daemon build cannot move a session root" if you `/cd` there). The one check
  that cannot be automated — that no Media / Photos / "other apps" dialog
  appears during a `~`-rooted search — is a runbook step in
  `docs/manual-verification.md`.

## [0.1.22] - 2026-08-18

### Fixed

- **A remote provider that calls tools natively can finish a task again
  (BUG-178).** With a provider at `tool_call_tier = "native"` — Kimi through
  Moonshot reproduces it — the model's first tool call ran, and the very next
  request died with `degraded: kimi (invalid response) — no fallback
  configured`, on every tool-using turn. The model had answered with a
  structured call and no prose, Teton recorded that turn as an empty assistant
  message, and the provider refused the follow-up request that carried it
  (Moonshot: "the message … with role 'assistant' must not be empty";
  Anthropic has the same rule). The turn is now recorded as the call the model
  made, in the same `{"tool": …, "arguments": …}` form the system prompt
  teaches — so the request is never empty and the model can see what it asked
  for. A turn cancelled while its call waited at the permission prompt drops
  the call and keeps the prose, as it does on the local tier. And an assistant
  turn that genuinely has no text — a thinking model that spent its whole
  output budget on reasoning — is no longer sent to the provider as an empty
  message either; the request skips it. When a provider
  refuses or breaks off a turn, the daemon's log now says which provider and
  what happened (`teton: provider `kimi` failed the turn before it answered:
  provider returned client error status 400`) instead of leaving only
  "invalid response" on screen. No wire-format change.

## [0.1.21] - 2026-08-18

### Added

- **The session runs Teton's own commands — no second terminal (REQ-582).**
  Asked "I want to test the kimi connection", the model named `/provider test`
  and then sent you to a shell for `teton provider list`; typed at the session
  prompt, `teton provider list` went to the model as chat and came back as
  "That one's for you to run — I can't execute `teton` commands myself. Type it
  in your shell." That round trip is gone. Ten commands that were shell-only now
  have a session spelling — `/provider list`, `/provider add`, `/boundary list`,
  `/boundary add`, `/policy show`, `/policy set-tier`, `/policy set-category`,
  `/model list`, `/model status`, `/doctor` — and each one *is* its shell twin:
  the same argument grammar (the CLI's own parser, so `/policy set-tier build
  kimi --fallback local` and its error messages are the ones you already know),
  the same renderer, the same daemon call. A test drives both surfaces against
  one daemon and diffs the lines, so the two cannot describe your machine
  differently.

  A line you type that *begins* with `teton` and names a real subcommand now
  runs that command here, after one line saying so:

  ```
  › teton provider list
  >> teton provider list → /provider list
  providers:
    kimi [openai-compatible]  kimi-k3  https://api.moonshot.ai/v1/chat/completions  auth: keychain
  ```

  No subprocess, no second connection, no model call — it dispatches to the same
  row `/provider list` does, over the session's own socket. A line that is *not*
  a command ("teton is slow today") still reaches the model with its bytes
  unchanged; `teton uninstall` is refused with the reason (it would stop the
  daemon under the session running it); a stray word gets the parser's own
  `unexpected argument`, which is what a shell would have told you.

  `/doctor` reports the connection the session already has rather than dialling
  the socket again — one line differs from `teton doctor`'s (`(this session's
  connection)` in place of the protocol version) and nothing else does, because
  a fresh attach would announce a client into the very session being diagnosed.
  And when a reply recites a shell command that has a session spelling, the
  session says so once: `>> in this session: /provider list, /policy show`. The
  bundled setup guide now names the `/` spellings first, so the model reaches
  for them too.

  Two limits worth knowing before you upgrade. Command arguments are split on
  whitespace and **quotes are not interpreted**, so a value with a space in it
  (a glob like `src/my notes/**`) still has to be given to `teton` in a shell —
  `/help` says so in its footer. And the four rows that *write* (`/provider
  add`, `/boundary add`, `/policy set-tier`, `/policy set-category`) are typed
  input only: on a piped session they change nothing and print one line naming
  the shell command to use instead. Every daemon-side gate is untouched — a
  write from the session meets exactly what the shell twin meets, presence
  attestation included. `/provider add`'s key is still read echo-off into the
  keychain and is never an argument of the line.

  Nothing new on the wire: no method, no event, no config key, no protocol bump.

## [0.1.20] - 2026-08-18

### Added

- **`/provider test <id>` — ask a provider whether it works, and get the
  provider's own answer (REQ-581).** Registering a remote model and then asking
  "is it connected?" used to be a question the product had no answer of its
  own for: the local model would run `teton provider list`, misread it, guess
  at a config directory, and tell you to re-export a key that was already in
  the keychain. Now there is a command that actually asks:

  ```
  /provider test kimi
    provider:  kimi (openai-compatible, kimi-k3) — https://api.moonshot.ai/v1/chat/completions
    this sends one minimal request (a few tokens in, at most 8 out) to that endpoint. proceed?  [y/N] y
    kimi kimi-k3: reachable — answered in 1.4 s (2040 in / 21 out, $0.006400 recorded); provider health: healthy.
    `build` routes here (edit, shell).
  ```

  It sends **one** completion request down the exact path a turn takes — same
  adapter, same transport, same credential resolution, same egress choke point
  — with a fixed prompt, no tools, no conversation and the smallest token
  budget the adapter allows. Never a `GET /v1/models` shortcut: proving an
  endpoint reachable that a turn never POSTs to answers a question nobody
  asked. Nothing leaves the machine until the preview names the provider, the
  model and the endpoint it will dial and you answer `y` — an empty answer is
  a no, and a piped invocation refuses unless you passed `--yes`.

  What comes back is the daemon's own classification, typed, not a vendor's
  prose read back to you: *reachable* (with latency, the token counts the
  vendor billed, and the cost the ledger recorded), *refused* (401/403 —
  naming the credential **reference**, `keychain://teton/kimi`, never the key
  itself), *model unknown* (404, naming the model string your config declares),
  *rate limited* (429), *server error* (5xx — the vendor answered and is
  failing, so your configuration is not the suspect), *unreachable* (nothing
  answered at all: DNS, TCP, TLS, a closed port), *answered, but not with a
  completion* (something is listening and it is not a chat endpoint — a
  redirect, a non-streaming endpoint, an address pasted without its
  `/v1/chat/completions` path), or *no answer within 30 s* (the connection was
  taken and nothing came back before the test stopped waiting). Those last
  three are three different next moves — check the address, check the path,
  check whether the vendor is up — so they are three different answers rather
  than one word with three sentences. No response body and no header is ever
  echoed into your transcript.

  A test moves the same health the router reads, so a provider an earlier
  failure had pushed aside is routable again on your very next turn once it
  answers — and the report says what now routes there. It is a real model call
  and is billed as one, tagged as a probe so `teton cost` counts it apart
  (`probes: 1 connection test(s) — billed like any call, counted apart`)
  instead of folding it into the turns you asked questions with.

  `teton provider test <id> [--yes]` is the same thing from a shell — one
  daemon method, two call sites, and the CLI still has no network path of its
  own. And the session hands off to it rather than improvising: ask a turn
  whether a provider is working and it names `/provider test <id>`.

  Additive on the wire: one new `provider/test` method, one new
  `provider_tested` event, a `probe` flag on cost records and a `probe_calls`
  count on the cost report — both defaulted, so a `teton` reading an older
  daemon's report simply sees no probes, which is the honest reading of a
  daemon that could not make one.

### Fixed

- **Another client attaching no longer re-announces the local model into
  your session (BUG-177).** The startup lines a client is caught up with on
  attach — `>> probe: … clears the local-tier floor`, `>> local model … ready`
  — were broadcast to every open session, so a `teton doctor` in another
  terminal, or any `teton …` the model ran through its own shell tool,
  reprinted them mid-turn as if the tier had restarted (and reset a loading
  indicator on the way). The catch-up now goes to the client that attached
  and nobody else; the deliberate `a CLI client attached` announcement is
  unchanged. No wire-format change.

## [0.1.19] - 2026-08-17

### Changed

- **A prompt typed while the local model is still loading now waits for it
  instead of being refused (REQ-580).** Start `teton`, type before the tier
  opens, and the session says `message queued until <model> finishes loading —
  it will run as soon as the local tier opens.`; the `benchmark` and `ready`
  lines land under it as they happen, and then the reply — no retyping. The
  same holds while an accepted download is still installing. Only the two
  states that end by themselves are waited for: a declined tier, a machine
  below the floor, a failed load, or an unanswered proposal still refuse at
  once with the sentence that names the fix, and a turn routed to a remote
  provider is never held for the local one. A Ctrl-C while the message is
  queued abandons it cleanly — nothing runs on the model later on its behalf.

  Additive on the wire: one new `turn_queued` event. An older `teton` against
  a newer daemon simply sees the reply arrive; a newer `teton` against an older
  daemon still gets the `model still loading` notice it always did.

## [0.1.18] - 2026-08-17

### Added

- **`/provider setup` — connect a remote provider without leaving the session
  (REQ-579).** Say "set up Kimi for deep reasoning" and the session hands you
  one command:

  ```
  /provider setup kimi think
  ```

  It asks the six things a registration needs — vendor (from the built-in
  recipe catalog: Anthropic, OpenAI, Moonshot/Kimi, DeepSeek, Grok, Ollama),
  provider id, model, API key, which tiers to route, and a yes to the exact
  TOML it is about to write — then commits it in one write and routes to it on
  your very next turn. No restart, no second terminal.

  The API key is read echo-off and goes straight into the OS keychain; the
  daemon only ever sees `keychain://teton/<id>`. It is never in the transcript,
  never in the model's context, never in config, events, the log, or the cost
  ledger. Cancel at any prompt and nothing was written; a refused commit puts
  the keychain back the way it was.

  On a pipe, or on a platform with no OS keychain, the command prints the
  equivalent `teton provider add …` recipe instead of asking anything.

- **The session tells you about it even when the model doesn't.** The local
  model is good at repeating a vendor's endpoint and poor at volunteering a
  new command — three live rounds against qwen3-coder-30b never got it to say
  `/provider setup` unprompted. So when a reply in a terminal session recites
  `teton provider add`, the session appends one line of its own:

  ```
  >> in this session, /provider setup <vendor> [tier] does this without leaving it; no key in chat.
  ```

  It is the harness speaking, not the model, and it cannot be talked off by
  a reply that names the command and then asks you to paste the key anyway.

### Fixed

- **The bundled guide no longer suggests putting a live API key on the
  command line (BUG-176).** On 0.1.17, asked to connect a provider, the local
  model's reply could end with "replace `kimi` with the actual API key" — a
  key in your shell history. The guide now leads with the in-session command
  and marks the shell path as `key via TETON_PROVIDER_KEY or a prompt`. If
  you followed that older advice, treat the key as exposed and rotate it.

### Upgrade notes

Nothing to do. There is no wire-shape change, so a 0.1.17 daemon still
running under `brew services` will keep serving until you stop it — the
0.1.18 CLI will tell you the exact command if that is your setup (0.1.17's
own fix). `/provider setup` needs the 0.1.18 daemon; against an older one it
says so and asks nothing.

## [0.1.17] - 2026-08-15

### Fixed

- **If you run the always-on daemon, the stale-build notice finally tells you
  something that works (BUG-174).** When your CLI outran the daemon serving it,
  Teton has always said: *"Exit every teton session to stop it; the next one
  starts the new daemon."* For a daemon you started with `brew services` that
  advice could never work — the formula runs it under `--shutdown-policy never`,
  so it does not exit with your last session, by design. Closing every session
  changed nothing, the same notice printed again, and there was no way out of
  the loop from inside the product. It now detects an always-on daemon and names
  the command that actually ends it:

  ```
  this CLI is 0.1.17 but the running daemon is 0.1.13 — commands are being
  served by the older binary. This is the always-on `brew services` daemon,
  which does not exit with your last session — run `brew services stop teton`
  once, and the next command starts the new daemon on demand.
  ```

  **This is the one upgrade note worth reading.** If you are on an always-on
  daemon older than 0.1.14, upgrading to 0.1.17 does *not* by itself put the new
  daemon in charge — Homebrew cannot unload a launchd agent registered by an
  earlier version of the formula. Run `brew services stop teton` once (close any
  open session first); from then on the CLI starts a daemon on demand and stops
  it with your last session. Confirm with `teton doctor`, which prints the
  version of the daemon that actually answered.

- **Commands installed by Homebrew are visible to the agent again
  (BUG-174).** A daemon started by launchd inherits launchd's `PATH` —
  `/usr/bin:/bin:/usr/sbin:/sbin` — which names no package-manager prefix at
  all, and the daemon passed that straight to every subprocess it spawned. So
  inside a session, `gh`, `rg`, `jq`, Homebrew's `python3` and `teton` itself
  were simply not found: the agent saw "command not found" and would report that
  Teton was not installed while running inside Teton. The daemon now floors the
  `PATH` it hands its children with the usual package-manager prefixes.

  The same starved `PATH` reached **stdio MCP servers**, where it was worse than
  degraded: a server declared as `npx @scope/server` could not be launched at
  all under an always-on daemon. It is floored on that path too.

  Floor entries are appended, never prepended, so if your daemon already had a
  working `PATH` — anything started from a normal shell — nothing about which
  binary a command resolves to changes. A server that declares its own `PATH`
  still overrides the floor untouched.

## [0.1.16] - 2026-08-15

### Added

- **`teton provider add` now takes the base URL your vendor documents
  (REQ-578).** Paste `https://api.moonshot.ai/v1` — the address Moonshot's
  quickstart prints and every OpenAI-compatible SDK takes — and Teton registers
  `https://api.moonshot.ai/v1/chat/completions`, the URL it will actually POST,
  and says so on the spot: `endpoint stored as … — that exact URL is what Teton
  will POST.` The full request URL remains the canonical documented form and
  still registers byte-identically, in silence: composition is forgiveness, not
  a new convention.

  It completes only what is unambiguously missing — a URL with no path, a bare
  `/`, or a bare `/v1` — and **never touches an explicit path**. A gateway or
  proxy serving chat completions at `/llm/proxy` is a first-class deployment,
  not a typo to correct. The completion happens once, at registration, and
  nothing joins a path at call time before or after this change: what is in
  your config is exactly what leaves the machine.

  **This upgrade rewrites nothing.** No existing config is migrated or
  normalized, and a provider you registered earlier keeps the endpoint it has.

- **`--kind anthropic` no longer needs an `--endpoint` (REQ-578).** It defaults
  to `https://api.anthropic.com/v1/messages`, written explicitly into your
  config file so the document still states exactly what will be called — there
  is no invisible runtime default. The missing endpoint used to be refused by
  the daemon *after* `provider add` had already read your API key into the
  keychain (BUG-170). That particular sequence is now impossible: the endpoint —
  along with the model and the provider id — is settled and shown before you are
  asked for a credential. A registration can still be refused after the key is
  read for reasons only the daemon can know at that moment, and when that
  happens Teton takes the key back out and tells you so.

  If the endpoint is `http://` to anything but your own machine, `provider add`
  now says so before the prompt: the key you are about to type would cross the
  network in the clear.

  `provider add` also refuses an `--endpoint` that is not an absolute `http://`
  or `https://` URL with a host — including near-misses like `http:/host` or an
  address with a tab or line break in it, which a URL parser and a plain string
  read differently. Teton will not register an address it cannot show you the
  same way it dials it. **This is a check on the command, not on your config:**
  a config file that already holds such an endpoint still loads and the daemon
  still starts, exactly as before, and `teton doctor` is where you will see it.

- **`teton doctor` now names the request URL for an endpoint that looks like a
  base URL (REQ-578).** If a provider's stored endpoint has no request path —
  because you wrote the config by hand, or registered before the completion
  above existed — doctor prints the form Teton would store. Where that form is
  genuinely ambiguous — a bare host for an OpenAI-compatible provider, since
  some vendors serve `/v1` and some do not — it says so and points you at your
  vendor's documentation rather than asserting an address it cannot know. It is
  advice and nothing more: the config stays valid, doctor's exit status is
  unchanged, and the file is not edited. A custom gateway path is never flagged.

- **Teton now hands you the exact command for the provider you name
  (REQ-577).** "How do I connect Claude / Kimi / DeepSeek?" is answered from
  recipes that ship inside the binary — the vendor's real endpoint, which
  provider kind it speaks, and an example model — for Anthropic, OpenAI,
  Moonshot (Kimi), DeepSeek, Ollama and Grok (xAI). Previously the best
  available answer was a `provider add` template with a hole where the endpoint
  goes, and the local model routinely went hunting through your repository for
  a fact only Teton knows.

  The prompt now also states outright what it could only imply before: Teton
  **cannot run its own setup commands**. Registration stays yours to perform —
  the key is still read echo-off into the OS keychain and never typed into a
  conversation — and the agent's job is to give you the exact lines to run.

  The recipes have exactly one source. The bundled guide, the README's own
  quick-start commands and the new `providers` doc topic are checked against it
  in CI in both directions, so an endpoint that moves fails the build instead of
  quietly shipping a command that connects to nothing.

- **A `teton_docs` tool, so Teton's own documentation grows without growing
  every prompt (REQ-577).** The model can read four bundled topics on demand —
  `providers`, `policy`, `web`, `doctor` — rather than carrying that depth in
  the resident prompt of every turn. Nothing is fetched: the topics are
  compiled into this binary and served out of process memory, so a docs read
  opens no file, no socket and no network destination, produces no egress
  event, and works in a fully offline session. It also never stops a turn to
  ask your permission — there is nothing to consent to — at any permission
  level, including `plan`, where reading is the only thing allowed. It is
  exempt from the tool-count cap that trims tool lists on weak or degraded
  providers, so it is present in exactly the sessions whose model is least
  likely to know Teton's setup surface, and it never displaces a file tool to
  get there. An upgrade therefore sends nothing anywhere new — the tool's whole
  content is knowledge that shipped with the binary you installed.

### Fixed

- **The provider commands in the README could not have worked, and now do
  (BUG-170).** Two of them have shipped since 0.1.13. The `anthropic` example
  passed no `--endpoint`, which the daemon refuses — *after* `provider add` has
  already read your API key into the keychain — and the Kimi example passed
  Moonshot's `base_url`, which registers a provider whose every call 404s.
  Teton's `--endpoint` is the whole request URL and is posted exactly as given;
  nothing appends a path to it, so a vendor's `base_url` is the wrong half of
  the URL. Every recipe now carries the URL the vendor's own `curl` example
  posts to, Anthropic included (`https://api.anthropic.com/v1/messages`).
  **If you registered a provider from an older README, `teton provider list`
  will show its endpoint — add the path (`/chat/completions`, or `/v1/messages`
  for an `anthropic` kind) and re-run `provider add` with the same id to update
  it.** A new test drives each recipe through config validation and the request
  builder, so a recipe that cannot serve a turn is a build failure rather than a
  documentation bug.

### Changed

- Internal sizing only, recorded because it is an assumption and not a
  measurement: the assumed system-prompt overhead that sizes the redaction
  chunk cap moved 8 → 9 KiB to fit the new tool's description (REQ-577). The
  redaction input ceiling and its chunk count are unchanged, and no behavior
  or limit a user can observe moves with it.

## [0.1.15] - 2026-08-14

### Added

- **Teton now helps you turn capabilities on instead of dead-ending
  (REQ-572).** Ask a question that needs the web with web lookup off, and the
  answer names the capability, says it is available but switched off, and
  gives the enablement path — it no longer hunts your repository or leaves
  you with a bare "I cannot search the web." Upgrading changes nothing on its
  own: web lookup stays off by default, the model can only *tell* you about
  the opt-in, and enabling remains your act alone (REQ-575 hardens that: the
  commit that writes config requires a human-present, session-holding caller
  — a headless same-UID process cannot make it).

  The act itself is now guided: **`/web setup`** walks tier → backend →
  key → preview → confirm, and the capability is live in the same session
  with no daemon restart. The key is collected echo-off and written straight
  to the OS keychain by the CLI — it never crosses the daemon socket and
  never appears in config — the preview shows the exact `[web]` TOML and the
  destination host derived from the same parse the lookup will use, the
  confirm defaults to no, and the commit refuses if the config moved since
  the preview you read. Backend suggestions (Brave, Kagi, keyless SearxNG,
  with their real auth-header shapes) are served by the daemon
  (REQ-573), so every client offers the same list and a suggested backend is
  one whose request shape ships tested.

- **Daemon config writes now preserve your hand-written comments and unknown
  keys (REQ-574).** "Enable permanently" consent answers and `/web setup`
  commits previously rewrote `config.toml` from the parsed document,
  dropping comments; writes now edit in place.

- **`config/set` requires presence attestation (REQ-576).** The same
  human-present rule the model-change surface already enforces now covers
  daemon-wide config mutation — a tightening; interactive use is unchanged.

### Fixed

- **The web-off refusal now actually says the sentence (BUG-168).** The prompt
  clause that names the opt-in was descriptive, and the local tier routinely
  paraphrased it away — users got "I cannot search the web" with no mention
  that the capability exists. The clause now dictates the ending, so the
  refusal names web lookup, the off state, and `/web setup` in so many words.

- **A stranger's refused attempt to change your session's web setup is now
  announced reliably (BUG-166).** The `web_setup_rejected` notice was budgeted
  at one per client connection, spent whether or not it reached anyone — so
  one refused call aimed at a session id that named nothing silently used up
  the only notice that connection would ever produce, and later refused
  attempts against your real sessions were never announced. The budget is now
  per (connection, targeted session) and is spent only when the targeted
  session actually exists, so each session's user hears about each offending
  connection exactly once. Refusal enforcement itself was never affected.
  Alongside it, the same audit's smaller findings: session-id length checks
  now guard every session-driving RPC (not only the setup family); `/web
  setup` against an older daemon now *says* when the commit cannot be pinned
  to the previewed bytes instead of degrading silently; and the
  credential-prohibition line in the model's self-help guide is pinned by
  exact wording so a softened edit fails the suite.

- **The search credential can now ride the header your backend actually wants
  (BUG-165).** `[web] search_key_ref` was always sent as
  `Authorization: Bearer <key>` — and neither of the search backends REQ-563
  itself names as examples accepts that shape (Brave wants
  `X-Subscription-Token: <key>`, Kagi wants `Authorization: Bot <key>`), so
  configuring either got a 401 on every search that looked exactly like a bad
  key. A new optional `[web] search_auth` names the shape as a template, with
  `{key}` marking where the resolved secret goes:

  ```toml
  [web]
  search_auth = "X-Subscription-Token: {key}"   # Brave's shape
  # search_auth = "Authorization: Bot {key}"    # Kagi's shape
  ```

  Unset means `Authorization: Bearer {key}`, so an existing config behaves
  exactly as before. The key itself stays in the OS keychain under
  `search_key_ref` — a template without `{key}`, or one set with no
  `search_key_ref` beside it, is refused at load with the fix named — and the
  credential is still bound to the endpoint's origin, so the new shapes can
  travel nowhere the old one couldn't.

## [0.1.13] - 2026-08-09

### Added

- **Web lookup, off by default (REQ-563).** Teton can now fetch a page or run a
  search when a question cannot be answered from its weights or from your
  files. **Upgrading changes nothing on its own**: with no `[web]` section in
  your config the tool is not registered at all, so a session makes zero lookup
  requests — that absence is structural, not a policy the agent is asked to
  respect. Turning it on is a deliberate edit plus a consent prompt.

  What the capability is, when you do enable it:

  - **Three tiers you opt into separately** — `fetch_user_url` (fetch a link
    *you* pasted), `fetch_any_url` (let the model choose the destination), and
    `search` (free-text queries to a backend you configure). A grant at one
    tier never answers for a higher one, so allowing a fetch of your own link
    does not authorize the model's choices.
  - **Every lookup is egress and goes through the same choke point as a
    provider call.** The tool holds no HTTP client of its own; it hands the
    request to the egress module, which applies your privacy boundaries, the
    redaction scan when `[privacy] redact` is on, an address screen that
    refuses loopback/link-local/private destinations, bounded redirects, and
    connect/total timeouts — then records the lookup in the cost ledger with
    the destination host, never the URL or query.
  - **The consent prompt shows the exact query or URL that will leave**, and
    the destination host is derived from the same parse the request is sent
    with.
  - **`search` requires the local model**, because enabling search enables the
    redaction scan as one decision; there is no configuration that sends
    unscanned queries. On a machine with no local tier every search is
    blocked, and the notice says so.
  - **Fetched pages are treated as untrusted data** — reduced to text locally,
    never shipped raw to a remote model, and framed by the same containment
    that already covers tool results.
  - **A local cache** (15-minute default) serves repeat lookups with no network
    request; `/web refresh <url>` forces a re-fetch.
  - **After Teton reads privacy-boundary content**, model-composed lookups stop
    for the rest of the session with a notice naming the cause; links you paste
    still work, and `/web allow` lifts the restriction for that session only.

  Config lives in a new `[web]` table (`tier`, `search_endpoint`,
  `search_key_ref`, `allowed_domains`, `cache_ttl_secs`, `permission`). Search
  keys are keychain references, never values.

### Changed

- **Permission keys for the web tool are per-tier** — `web_fetch_user_url`,
  `web_fetch_any_url`, `web_search`. Only relevant if you consume
  `permission_request` frames programmatically (an ACP-style client): there is
  no single `web` key, and a tool-name match on `web_fetch` will not fire. No
  effect on the CLI or on existing tools.

## [0.1.12] - 2026-08-08

### Fixed

- **Asking Teton how to hook up external models now answers directly**
  (BUG-160). The agent's system prompt bundles Teton's own provider-setup
  instructions — `teton provider add`, `teton policy set-tier`, where
  `config.toml` lives, and the keychain rule — so a setup question is answered
  from them instead of triggering a search of your repository for
  documentation that was never there. Nothing about this changes where data
  goes: the bundled text is part of the local prompt frame. The README gained
  a matching "Hooking up an external model" section.

## [0.1.11] - 2026-08-08

### Changed

- **`scan`-tier duties now send on the ordinary remote configuration.** Two
  harness duties are newly wired to the `scan` tier: `triage`, which ranks a
  `grep` result against your request before it enters the model's context, and
  `compact`, which decides what a conversation may forget when it no longer
  fits. Both are improvements to what the agent does with what it already
  gathered.

  **Read this if your config has a `default_provider` and no explicit
  `[[tiers]]` rows** — which is the shape a REQ-557 migration leaves behind, and
  therefore the shape most upgraded machines are in. `scan` inherits
  `default_provider`, so after this upgrade:

  - **`grep` match text** — lines of your repository's files — is sent to that
    provider on turns where a search returned more than one hit.
  - **Conversation history** — the blocks of the session so far, which include
    your own prompts and previously read file content — is sent to that
    provider on turns where the context comes under budget pressure.

  Neither was sent before. **No configuration change of yours causes this**; it
  is a consequence of categories that previously had no call site acquiring
  one, and it is disclosed here because a routing table that quietly widens
  what leaves the machine is a privacy change whether or not it is a bug.

  What is unchanged: privacy boundaries still hold. A `local-only` file whose
  content is in a match, or in the conversation, is refused at the egress choke
  point before a byte leaves, the duty degrades, and the turn carries on.
  Session titles (`title`) remain local on every ordinary configuration —
  `reflex` never inherits `default_provider`.

  **To keep them on your machine**, bind the tier to your local provider:

  ```sh
  teton policy set-tier scan local
  ```

  Or bind just one of the two, leaving the other where it is:

  ```sh
  teton policy set-category triage local     # grep match text
  teton policy set-category compact local    # conversation history
  ```

  `local` is the id the on-device tier uses for itself unless your config
  declares a `kind = "local"` provider under some other name; `teton policy
  show` prints the id it actually resolved, alongside the provider chosen for
  every category and the class of content each one sends. Check there rather
  than inferring it from the tier table.

### Added

- **Sessions are named automatically** from the first substantive prompt, once
  per session, on the local model. The naming runs alongside the turn rather
  than ahead of it, so it never delays an answer.
- **`shell` output is interpreted** when — and only when — reading it unaided
  is the hard part: the command failed, or its output ran past the capture cap.
  A short successful command costs nothing.
