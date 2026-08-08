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
