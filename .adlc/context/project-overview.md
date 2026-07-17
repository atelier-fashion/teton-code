# Teton Code — Project Overview

## What It Does

Teton Code is a standalone AI coding agent (Claude Code–style agentic harness)
that routes work across a range of models so users spend frontier-model money
only where frontier-model intelligence matters. It ships with a slim local
model (hardware-adaptive, downloaded on first run, running in a persistent
daemon) that handles the always-on cheap tier — routing, summarization, commit
messages, secret redaction, offline fallback — and lets users register any
remote provider (Anthropic, any OpenAI-compatible endpoint) for the heavy
tiers.

Two visible product promises:

1. **Cost control** — workflow-aware routing (development phases determine the
   model tier: spec/architecture/review → frontier, implementation → cheap
   model executing from task artifacts, mechanical I/O → local) plus a live
   cost meter with per-phase attribution.
2. **Privacy boundaries** — paths marked `local-only` never leave the machine;
   enforced at the daemon's single egress point and verified by egress-capture
   tests.

Target user: cost-conscious developers who want explicit control over which
models do what (cut API spend 60–80%).

Product charter/spec: `.adlc/specs/` (see the founding REQ). Brand: domains
tetoncode.ai / .com / .dev; CLI binary `teton`; mountain-range metaphor
(base camp = local daemon, summit = frontier model, routes = routing policy).

## Tech Stack

| Layer | Technology |
|---|---|
| Daemon (engine) | Rust (ADR-001) |
| Local inference | Embedded llama.cpp (GGUF); Metal on Apple Silicon |
| CLI (`teton`) | Rust, same workspace as daemon |
| VS Code extension (phase 2) | TypeScript, thin client over daemon protocol |
| Remote providers | Anthropic Messages API; OpenAI-compatible chat completions |
| Credential storage | OS keychain (macOS Keychain first) |
| Platforms | macOS/Apple Silicon first-class; Linux next; Windows later |

## Project Scope

**In scope (MVP):** daemon + CLI, one local model with hardware-adaptive
selection, ≥2 remote provider kinds, phase-based routing policy, cost meter,
`local-only` privacy boundaries, freeform + structured (ADLC-derived) modes.

**Out of scope (MVP):** VS Code extension (phase 2), JetBrains/Neovim/web
clients, VS Code fork, per-user fine-tuning, Windows, hosted/teams/billing
anything, ML-based difficulty routing, `redact-then-remote` privacy mode.
