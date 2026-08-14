# REQ-575 — Architecture: presence attestation for the web setup commit

## Approach

`web/setup_commit` becomes the **third** REQ-570 BR-10(b) daemon-wide
commitment, joining `model/confirm` and `model/set`. The mechanism already
exists and is reused verbatim: after the existing REQ-572 gates
(`refuse_unmintable_session_id`, `refuse_commit_without_session_access`), the
handler runs the **same** `refuse_unattested_commitment` the two model methods
run — same live check, same fail-closed/degrade split, same fixtures, same
error taxonomy. No new attestation code, no new protocol, no client change.

Two integration facts discovered during design shape the task graph and are the
reason this is not a one-line edit:

1. **The commit must leave the synchronous reader-loop dispatch.**
   `refuse_unattested_commitment` may run an OS presence prompt that **parks on
   a human**. `model/confirm`/`model/set`/`session/attach`/`attach/consent` are
   deliberately *absent* from the `dispatch` match (server.rs ~2135) and routed
   through `handle_client`'s `blocks_on_a_human` spawn path (server.rs
   ~1535-1567) precisely so a parked prompt never stalls the reader loop.
   `web/setup_commit` is currently handled *inline* in `dispatch` (server.rs
   ~2151). Left there with an added presence check it would park the whole
   connection on a human — including any concurrent RPC the same client has in
   flight. So the commit moves to the `blocks_on_a_human` set and becomes
   `async`. `web/setup_plan` and `web/setup_preview` stay in `dispatch` — they
   are reads, they never attest, they never park.

2. **The existing gating unit test must migrate off `dispatch`.** Once
   `web/setup_commit` leaves the `dispatch` match, the current unit test
   `a_commit_without_session_access_is_refused_and_the_session_is_told`
   (server.rs ~6911) — which calls `dispatch(&daemon, &intruder, …,
   WebSetupCommitParams::METHOD, …)` — would hit the `_ => method not found`
   arm and assert against the wrong refusal. It converts to calling
   `handle_web_setup_commit(…).await` directly, exactly as the model-method
   unit tests call `handle_model_set(…).await` directly (server.rs ~2569
   documents that the tests drive the handler with no runtime). The
   defense-in-depth mutation test at server.rs ~6890 migrates with it.

## Key decisions

### ADR-1: `web/setup_commit` is a BR-10(b) commitment, gated through the shared seam

**Decision**: add `refuse_unattested_commitment(daemon, conn, &id).await` to
`handle_web_setup_commit`, **after** `refuse_unmintable_session_id` and
`refuse_commit_without_session_access` and **before** the runtime is touched.
Make `handle_web_setup_commit` `async`. Add `WebSetupCommitParams::METHOD` to
the `blocks_on_a_human` set in `handle_client` with a branch calling
`handle_web_setup_commit(&daemon, &conn, id, params).await`, and **remove** its
arm from the `dispatch` match. Update the two dispatch-site comments (the
`blocks_on_a_human` rationale block ~1532 and the "three setup methods are
session-scoped" comment ~2145) so they name the commit's new home honestly —
the plan and preview stay session-scoped reads; the commit is now additionally
a daemon-wide commitment.

**Rationale**: this is the validated spec, BR-1/BR-2. Ordering after the
session gate is BR-2 and the `model/confirm` precedent — a caller that may not
drive the session at all is refused with no prompt appearing on anyone's
screen. Reusing `refuse_unattested_commitment` rather than a parallel check is
BR-1 and LESSON-499 (one policy, one implementation) — the degrade-not-refuse
posture (BR-3), the synthetic connection-scoped binding id, and the "never
recorded into the attestation registry" property all come for free because it
is the *same function*.

**Degrade, don't refuse (BR-3)**: `refuse_unattested_commitment` already
returns `None` (allow) with a stderr notice when
`daemon.verifier.availability()` is `Unavailable` — every default/CI build.
So `/web setup` on a shipped build gains **zero** new prompts or steps; only a
macOS `--features presence` build prompts at commit. This is REQ-570 BR-8's
asymmetry, inherited unchanged, and is what keeps AC-3's regression bar green.

### ADR-2: OQ-3 resolved — `config/set` deferred to a tracked follow-up; the consent path scoped out

`/validate` surfaced two live sibling methods that meet the same BR-10(b)
trigger. The product owner deferred the gate-vs-scope-out decision here.
Resolution:

**`config/set` (`handle_config_set` → `apply_config_update`) — deferred to
REQ-576, NOT folded into this REQ.** It is a strictly *larger* blast radius
than `web/setup_commit` (`RegisterProvider` = an arbitrary remote egress
endpoint; `SetPrivacyBoundary` = the privacy boundary itself;
`SetTierBinding`/`SetCategoryBinding`), reachable by the same NotDescendant
same-UID attacker, and today gated with `refuse_daemon_wide` (layer a) only.
It should be gated — but **as its own REQ**, for three reasons that make it
unfit as a rider on a spec written for `web/setup_commit`:

  1. Gating it **reverses a documented, deliberate decision**: the
     `handle_config_set` comment (server.rs ~2617) explains it was *knowingly*
     kept at layer (a) after BUG-162, on the same "removes immediacy, not
     capability" reasoning this REQ's spec rejects. Reversing a recorded
     decision that touches `SetPrivacyBoundary` deserves its own spec, its own
     review, and its own validation — not to be bundled invisibly.
  2. It carries a **distinct regression surface** — `teton provider add`
     (CLI) and tier/category-binding commands call `config/set`. On a
     `--features presence` build each would raise a presence prompt; that is
     an AC-8-style analysis (is prompting on `provider add` correct? almost
     certainly yes, but it must be *decided*, not stumbled into) that belongs
     to config/set's own cycle. (On shipped builds it degrades — no prompt —
     so the practical regression is near-empty, which is exactly why REQ-576
     is low-risk and should follow promptly.)
  3. ADLC discipline: a finding discovered in validation becomes its own
     tracked REQ. The finding-7 slip happened because a method was added with
     **no** classifying artifact. The mitigation is a real REQ-576 artifact
     (created as a sub-step of this phase), not a bundled edit and not a prose
     TODO that evaporates.

  **This REQ therefore does not claim to close config/set.** REQ-575 closes
  finding 7 as filed (the `web/setup_commit` method); config/set is the
  larger sibling, tracked in REQ-576, and the PR/architecture say so plainly.
  The `refuse_daemon_wide`-only gate on config/set is a **stated residual**
  for the life of REQ-576, not a silent one.

**Consent-path `persist_web_tier` (via `permission/respond` +
`enable_permanent`) — scoped out, documented residual.** It durably writes
config and swaps in-memory config, so it meets BR-5's literal trigger, but it
is materially lower severity: it is **raise-only within an already-configured
`[web]` table** and cannot author the endpoint, credential, or a brand-new
capability — the dangerous powers unique to `web/setup_commit` and
`config/set`. Gating it would also put a presence prompt on the ordinary
"enable permanently" answer a user gives to a legitimately-raised consent
prompt — a real intrusion on the common path and a REQ-570 AC-8 regression
concern. Recorded here as an accepted residual; its classification is folded
into REQ-576's scope for a considered disposition rather than a reflex gate.

### ADR-3: Test strategy — migrate the dispatch test, then pin the new seam per LESSON-502/508

The attestation check is enforced at **one** new seam (the commit handler), so
it needs its own tests at that seam, independent of the model-method seams
(LESSON-502). Because the method also moves off `dispatch`, the existing gating
test migrates rather than being added-to:

- **Migrate** `a_commit_without_session_access_is_refused_and_the_session_is_told`
  and its mutation twin (server.rs ~6890-6947) from `dispatch(…)` to
  `handle_web_setup_commit(…).await`. Behavior asserted is unchanged; only the
  call vehicle changes. This keeps the REQ-572 BR-4 session-gate coverage green
  and non-vacuous after the move.
- **Add** a unit test: with `AlwaysFailsVerifier` installed, a commit from a
  *properly attached* connection is refused with the attestation error code,
  and the runtime is never reached (config on disk and in memory unchanged) —
  AC-1. Mirror `model_consent.rs`'s attested-refusal pattern.
- **Add** the mutation test (AC-5): deleting the `refuse_unattested_commitment`
  line from `handle_web_setup_commit` turns at least one test red,
  independently of the `model/confirm`/`model/set` seams (LESSON-508 rule 2 —
  a redundant/parallel guard needs its own deletion test).
- **Add** an integration test: the granted path over the socket via the
  `TETON_PRESENCE_ACCEPT` seam (AC-2/AC-6,
  `an_attested_commit_lands_over_the_socket_through_the_presence_seam` in
  web_setup_flow.rs — a spawned-binary harness, so the accepting verifier is
  reached through the seam rather than `with_presence_verifier`).
- **Add** the reader-loop liveness test the `blocks_on_a_human` move exists for:
  `a_parked_web_setup_commit_does_not_stall_the_connection` (multi_client.rs),
  which installs a `ParkingVerifier` that blocks inside `verify` on a
  multi-thread runtime (exercising the production `block_in_place` branch) and
  asserts a concurrent `session/list` on the same connection is still served
  while the commit is parked. *(As-built: this replaced the earlier plan to fold
  the check into web_setup_flow.rs — that harness is spawned-binary and cannot
  inject a blocking verifier; the in-process socket harness in multi_client.rs
  can.)*
- **Add** a degradation test (AC-3): with the shipped no-mechanism verifier the
  commit lands and the stated notice is emitted, with zero new prompts.
- **Add/extend** the spawned-binary e2e (AC-6): an attested `/web setup` commit
  driven through `TETON_TEST_SEAMS` + `TETON_PRESENCE_ACCEPT`, and the
  release-build refusal of those seams left intact.
- **AC-8** is a recorded human pass on a `--features presence` macOS build,
  written to `docs/manual-verification.md` at the strength verified — not
  satisfied by any of the above.

## Data model changes

None. No new protocol types, no config schema change, no new events. The
`WebSetupCommitParams`/`WebSetupCommitResult` shapes are unchanged.

## Interaction with in-flight work

- REQ-570's `blocks_on_a_human` set and `refuse_unattested_commitment` are the
  load-bearing dependencies; both are merged. This REQ adds one member to the
  set and one call site to the function.
- No overlap with the open `fix/BUG-166-*` branches (rejection-notice budget,
  web-off opt-in) — those touch the REQ-572 announcement path and the web-off
  refusal text, not the commit's gating.
- REQ-576 (config/set) will add a *fourth* member to the BR-10(b) set; the
  BR-5 doc update in this REQ names the set as three, and REQ-576 will make it
  four. No conflict — additive.

## Spec-mapping table

| Spec item | As designed | Why the intent holds |
|---|---|---|
| BR-1 (commit gated via shared seam) | `refuse_unattested_commitment` added to `handle_web_setup_commit`; method made async; moved to `blocks_on_a_human` | same function as model/set — one policy, one impl (LESSON-499) |
| BR-2 (order after session gate) | check sits after `refuse_commit_without_session_access`, before runtime | model/confirm ordering precedent — no prompt for a caller that may not act |
| BR-3 (degrade, don't refuse) | inherited from `refuse_unattested_commitment`'s `Unavailable` arm | REQ-570 BR-8 asymmetry, unchanged; AC-3 regression bar |
| BR-4 (one line, per method) | single call line at top of the commit handler | mutation-deletable in isolation (LESSON-502/508) |
| BR-5 (fix "only those two" docs → three; standing rule) | update `refuse_unattested_commitment` doc + `blocks_on_a_human` comment to name the three-method set; state the classification obligation | REQ-576 will make it four — additive |
| BR-6 (honest strength) | architecture + PR state parity-with-model/set, degraded elsewhere, no ancestry-escape claim, config/set residual named | honesty over completeness |
| OQ-3 | ADR-2: config/set → REQ-576 (tracked), consent-path scoped out | product-owner-deferred decision, resolved with rationale |
