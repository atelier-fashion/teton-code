# Taxonomy — Retrieval Tag Vocabulary

This project's legal values for retrieval tag dimensions. Currently used by `/spec` when retrieving relevant prior context via the unified tag-based retriever. Integration into `/architect`, `/bugfix`, and `/review` is planned in follow-up REQs.

**This file is project-local.** Different projects have different taxonomies. Extend it as new areas emerge. Values are advisory — the retrieval system does not currently enforce them, but consistent vocabulary improves retrieval quality.

**Note on `tags`:** the `tags` dimension is intentionally free-form and is NOT enumerated here. Authors add whatever keywords feel descriptive at the time of writing. See the `tags (free-form)` section at the bottom for guidance.

## component (narrow area)

Single string. Hierarchical if helpful (e.g., `API/auth` or `iOS/SwiftUI/WardrobeView`).

Values are project-local — extend this list as new components emerge.

Values for this project:
- `daemon/router`
- `daemon/session`
- `daemon/egress`
- `daemon/cost-ledger`
- `inference/local`
- `inference/probe`
- `providers/anthropic`
- `providers/openai-compat`
- `protocol`
- `cli`
- `extension/vscode`
- `agent/engine` (charter-level, whole-engine work)

## domain (broad area)

Single string. Higher-level than `component`.

Values are project-local — extend this list as new domains emerge.

Values for this project:
- `devtools` (charter-level)
- `routing`
- `inference`
- `providers`
- `privacy`
- `cost`
- `harness` (tool-use loop, permissions, verification)
- `clients`

## stack (tech layers)

Array. One entry per technology touched.

Values are project-local — extend this list as new technologies are adopted.

Values for this project:
- `rust`
- `llama.cpp`
- `daemon`
- `cli`
- `vscode-extension`
- `typescript`
- `llm-providers`
- `json-rpc` (or the protocol chosen in ADR-002)
- `keychain`
- `gguf`

## concerns (cross-cutting dimensions)

Array. Identifies quality attributes or aspects the work touches.

Values are project-local — extend this list as new concerns emerge.

Values for this project:
- `privacy` (BR-1 boundary guarantees)
- `cost` (BR-2 attribution, savings)
- `routing` (policy, tiering)
- `latency` (BR-8 local-tier duty)
- `security` (keychain, egress, permission model)
- `reliability` (degradation, fallback)
- `extensibility` (BYOM, adapters, protocol)
- `developer-experience`

## tags (free-form)

Array of any keywords. Intentionally NOT enumerated — authors add whatever feels descriptive. Examples: `password-reset`, `rate-limiting`, `snapshot-testing`, `canary-deploy`.

The `tags` dimension is the lowest-weight signal in retrieval (+1 per match vs +2 for concerns/domain, +3 for component) but provides useful lexical signal.
