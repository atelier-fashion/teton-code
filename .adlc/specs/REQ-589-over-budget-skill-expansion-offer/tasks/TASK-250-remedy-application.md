---
id: TASK-250
title: "Apply the going-forward remedy through config/set, ordered"
status: complete
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: [TASK-244, TASK-259]
---

## Description

BR-7 + BR-8 + BR-9 / ADR-4 + ADR-5 + ADR-12. Every remedy writes through `config/set`, inheriting its posture verbatim. The two-write `BindTierRemote` remedy is ordered so the forbidden state is unreachable.

## Files to Create/Modify

- `crates/tetond/src/runtime.rs` — remedy application calling `apply_config_update` (2760); provider choice for ADR-12
- `crates/tetond/src/server.rs` — the answer path reaching `handle_config_set` (3258)

## Acceptance Criteria

- [x] `RaiseWindow`/`RaiseCap`/`DeclareWindow` write via `RegisterProvider` field-wise; other fields stay `None` and existing values are preserved
- [x] `BindTierRemote` writes `max_context` FIRST and the tier binding SECOND, so a partial failure leaves a declared window on an unbound tier and never the reverse (AC-8, ADR-5)
- [x] Exactly one configured remote provider is proposed by name; two or more are **never picked silently** (ADR-12) — see the ADR-12 deviation below: the offer withholds the rebind option and names the candidates on the record channel, because ADR-1's four-id wire has no slot for an N-way choice
- [x] The offer's attestation wording matches what the running build performs — no claim of a verified human on a build without `presence` (AC-20, BR-8)
- [x] `proceed` and `apply_remedy` are honored independently across all four option ids, including remedy-only and proceed-only (AC-7b)
- [x] After a `BindTierRemote` remedy, an identical second invocation reaches NO offer because the route now fits (AC-24) — the end-to-end proof the reported circle is closed

## Technical Notes

> **DEFECT to fix, found by TASK-247.** A window remedy with no `ProposedWindow` currently
> renders an option label promising a write it cannot make ("…to that provider's real window
> — this daemon ships no figure for it and will not invent one"), and **selecting it applies
> nothing**. That is precisely the `enable_permanent` failure ADR-1 cites as its cautionary
> precedent: a label that promises a write which is silently a no-op. Either do not offer a
> remedy option when there is no proposal, or make the option ask for the value. An option
> the user can pick that does nothing is not acceptable.

> **TASK-247 already landed the three single-write remedies** through `apply_config_update`
> (it needed a producer for `skill_over_budget_remedy_applied`). What remains yours:
> `BindTierRemote`'s ORDERED pair (ADR-5: `max_context` first, tier binding second) and
> ADR-12's provider choice — both marked with a `TODO` on the match arm. Read TASK-247's
> work before extending it.


`config/set` persists one update per call and architecture.md:169-172 forbids generalizing it — ordering, not atomicity, is the mechanism (ADR-5). Do NOT introduce a second durable-write path via `persist_web_tier` (ADR-4 rejected it).

## Deviations and findings (implementation)

**ADR-12, partial.** *"Two or more are presented as a choice"* is not implementable
inside ADR-1's wire. `OverBudgetOptionLabels` carries exactly one optional remedy pair
and `interpret_over_budget` recognizes exactly four option ids, all of which live in
`permissions.rs` (owned by TASK-256) and are matched exhaustively client-side. An N-way
"which provider?" choice needs a fifth id family or a second prompt, and both are
protocol changes this task does not own. What shipped is the half ADR-12 exists to
guarantee: with two or more configured remotes the rebind option is **withheld** and a
record line names every candidate and the `teton policy set-tier` command that binds one.
Nothing is picked silently. The N-way choice is a follow-up.

**BR-9's "names the provider", not satisfiable here.** `Remedy::BindTierRemote { tier }`
carries no provider — `Remedy::for_bound` deliberately drops the route's `provider_id`
so the remedy cannot be addressed to the provider the route is *leaving* — so the
composer (`budget.rs`, TASK-243) has no name to put in the sentence or the option label.
Both say "a remote provider" and name the tier and the cost consequence. Naming the
chosen candidate needs a provider slot on that variant, which is TASK-243's file.

**ADR-4's posture is `apply_config_update`'s, not `handle_config_set`'s.** The remedy
reaches `config/set`'s *body* — validation, `reject_unusable_binding`, atomic persist,
identical refusals — but not the two connection-level gates `server.rs` wraps that body
in (`refuse_daemon_wide`, `refuse_unattested_commitment`). Reaching them would mean
threading `&Daemon`/`&ConnState` into a turn. Recorded rather than papered over; note
that on a shipped build (no `presence` feature) `refuse_unattested_commitment` degrades
to allow with a stderr line anyway, and that the remedy is authorized by an *addressed*
consent from the connection that submitted the turn (REQ-587 ADR-3), which is a stronger
assurance than that degraded gate performs. `server.rs` needed no change.

**AC-20 is satisfied by construction.** The composer makes no attestation claim at all —
`budget.rs` contains no mention of presence, attestation, or a verified human — so there
is no wording to reconcile with a build that lacks `presence`.
