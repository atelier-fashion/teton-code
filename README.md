# Teton Code

**An AI coding agent that routes work across a range of models — so you spend
frontier-model money only where frontier-model intelligence matters.**

> ⛰️ Named in the Tetons, where the idea was born. A mountain range is a range —
> of peaks, of sizes, of routes. So is your model lineup.

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
  savings vs. an all-frontier baseline.
- **Privacy boundaries** — mark paths as *local-only* and their content never
  leaves your machine. Enforced at the daemon's single egress point, verified by
  egress-capture tests, not vibes.

## Architecture

Engine/surface separation: all differentiating logic (router, workflow state,
privacy enforcement, cost accounting, provider adapters) lives in a local
daemon. Clients are thin:

1. **CLI (`teton`)** — first surface, MVP target.
2. **VS Code extension** — second surface, same daemon protocol.

## Status

🚧 **Pre-alpha — product spec stage.** See
[docs/requirement-draft.md](docs/requirement-draft.md) for the full requirement
spec (business rules, acceptance criteria, open questions).

## License

[MIT](LICENSE)
