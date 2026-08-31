# Teton Code — Conventions

## File Organization

Planned Cargo workspace layout (created when daemon work begins):

```
teton-code/
  Cargo.toml            # workspace root
  crates/
    tetond/             # daemon binary
    teton/              # CLI binary
    teton-core/         # router, session state, cost ledger (no I/O)
    teton-providers/    # provider adapters (anthropic, openai-compat)
    teton-inference/    # llama.cpp embedding, hardware probe, benchmark
    teton-protocol/     # client↔daemon protocol types (shared with clients)
  extension/            # VS Code extension (TypeScript, phase 2)
  docs/
  .adlc/                # ADLC artifacts (specs, bugs, knowledge, context)
```

## Naming

- Rust: standard rustfmt + clippy defaults; crates prefixed `teton-`.
- Binaries: `teton-code` (daemon — renamed from `tetond` in REQ-549/ADR-007;
  the crate is still `tetond`), `teton` (CLI).
- Branches: `feat/REQ-xxx-slug`, `fix/BUG-xxx-slug` (ADLC convention).

## Testing

- `cargo test` per crate; workspace-wide in CI. Locally, always
  `cargo test --workspace --no-fail-fast` and grep the output for `FAILED` —
  cargo's default fail-fast stops at the first failing *target*, so a summed
  "N passed, 0 failed" from an interrupted run is a floor, not a total
  (LESSON-533; a summed count also under-counts because `; ` splits into an
  empty field).
- Privacy boundary (BR-1) claims require egress-capture integration tests
  (mock transport asserting no boundary content in any remote payload) — code
  inspection is not acceptance.
- Router policy decisions are pure functions in `teton-core` — table-driven
  unit tests.
- A fixture must not depend on directory listing order (APFS hashes, ext4 and
  tmpfs do not) nor on a spawned CLI reading its stdin before it exits — when
  "the first entry" matters, plant one; when a child may legitimately exit
  early, treat `BrokenPipe` on the stdin write as that outcome and keep every
  other write error fatal (LESSON-540). CI's ubuntu leg is where the macOS-only
  assumption fails.
- **A source-scanning check must bound its span and key on the hazard**
  (REQ-600). This codebase derives several guarantees by reading its own source;
  three rules keep those honest. **Bound the slice to the item you mean** — an
  unbounded `&source[start..]` is a claim about the rest of the file, and after a
  decomposition the rest of the file is other functions. **Compare positions only
  where order means order** — inside one function body, textual order is
  execution order; across sibling definitions it is file layout, so a call-order
  claim belongs on the orchestrator's body. **Assert on the hazard, not the
  remedy** — forbidding `block_in_place` forbids the mitigation and permits the
  bare blocking syscall (LESSON-585). Cut every corpus at the first column-0
  `#[cfg(test)]`: a check whose own patterns appear in its own file will
  otherwise match itself and its vacuity floors can never fire.
- **Re-run a derived check's mutation after any change to program structure**
  (LESSON-598). Do not re-read the check — a guard that has stopped covering its
  subject looks exactly like a guard that passes. REQ-600 moved one line into a
  helper and an inversion that had gone red went green, with nothing else in
  4,000 tests noticing.
- **Bound a mechanical rename to code tokens** (LESSON-599). A word-boundary
  regex over a region reaches string literals and comments, and those are the
  one place the compiler, clippy and the whole suite are structurally incapable
  of noticing. On any refactor claiming "bodies are byte-identical", diff the
  prose too: `git diff origin/main..HEAD | grep '^[-+].*//'`.
- **Show the test can fail before trusting that it passed.** Break the thing the
  test guards and confirm it goes red; record the mutation in the test's doc
  comment. REQ-592 shipped seven green assertions that could not have failed —
  an oracle that called the code under test, a fixture built on the wrong
  failure mechanism, and two ACs written against a test surface that lacks the
  buffer they asserted about (LESSON-569). Three traps in order of how easy
  they are to fall into: never let the expected value be computed by the
  subject; verify the failure *mechanism* before building a fixture around it;
  pick the test double by what property you are asserting, not by what is easy
  to assert. When a test guards a gate or a structural rule, invert the gate,
  count what fails, and write the number down — "the old tests still pass" is
  not evidence the gate works.
- **An invariant with more than one enforcement point needs a sweep, not a
  fix.** Enumerate every site that can violate it before fixing the one in
  front of you. A sweep that *counts* call sites is weaker than one that
  *region-checks* them: relocating a required call keeps the count identical
  (REQ-592, LESSON-568). Where the rule is a property of a method rather than of
  its callers, put it on the method — `RpcMethod::ENDS_TURN` is the worked
  example.

## Error Handling

- `thiserror` for library crates, `anyhow` at binary edges.
- Provider failures degrade (fallback provider, `provider_degraded` event) —
  never abort a session on a single provider error.
- No credential, file content, or prompt text in error messages or logs that
  leave the machine.
- **Config validity vs usability.** `Config::validate()` is fail-closed and gates
  daemon startup, so it carries *structural* errors only — duplicate ids, a raw
  key where a reference belongs, a reference naming something that does not
  exist. A record that is merely **incomplete** in one field is reported by a
  separate non-fatal pass, marked unusable, and refused at the point of use; the
  daemon still starts and every other record still works. Enforcing
  incompleteness at load makes one bad entry fatal for all of them, and vetoes
  any migration meant to fix it (REQ-557 ADR-E, LESSON-506).
- **A typed outcome needs both halves.** An error the retry / fallback / degrade
  machinery must not act on gets `failure_class() -> None` **and** its own arm on
  the turn path. The first half only keeps the machinery away from it; without
  the second, it falls through to the generic remote arm and the user is told
  "provider failed unrecoverably" — wrong about the cause and naming no remedy.
  Three outcomes are shaped this way: `PrivacyBlocked` (rerouted to local),
  `ContextLengthExceeded` (REQ-586 BR-2), and `SpendCeilingReached` (REQ-588
  BR-3). Adding a fourth means writing both halves (LESSON-557).
- **Compose the sentence where the facts are.** A refusal message cannot ride a
  `Copy` enum, and `TransportError` is `Copy`. Carry the *fact* across such a
  boundary — an enum, a couple of integers — and word it at the surface that
  renders it, which is also what makes a single composer enforceable. Composing
  at the point of detection loses the message silently: it compiles, the tests
  pass, and only the user sees the difference (REQ-588, LESSON-557).

## Git Conventions

- Public OSS repo (MIT) under the `atelier-fashion` org; HTTPS remotes.
- PR-gated CI on `main` (plain OSS flow — this repo does NOT use the
  staging-first pipeline of other atelier-fashion repos).
- Conventional-style commit subjects; PRs reference their REQ.
