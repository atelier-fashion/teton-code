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
  network in the clear. An address carrying a tab or a line break is refused
  outright rather than guessed at, because such a URL renders differently from
  how it dials.

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
