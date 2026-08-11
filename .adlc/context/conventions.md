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

- `cargo test` per crate; workspace-wide in CI.
- Privacy boundary (BR-1) claims require egress-capture integration tests
  (mock transport asserting no boundary content in any remote payload) — code
  inspection is not acceptance.
- Router policy decisions are pure functions in `teton-core` — table-driven
  unit tests.

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

## Git Conventions

- Public OSS repo (MIT) under the `atelier-fashion` org; HTTPS remotes.
- PR-gated CI on `main` (plain OSS flow — this repo does NOT use the
  staging-first pipeline of other atelier-fashion repos).
- Conventional-style commit subjects; PRs reference their REQ.
