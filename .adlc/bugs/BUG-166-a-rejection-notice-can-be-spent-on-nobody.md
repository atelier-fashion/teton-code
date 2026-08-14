---
id: BUG-166
title: "A refused commit's one rejection notice can be spent on a session nobody holds"
status: open
severity: high
created: 2026-08-14
updated: 2026-08-14
component: "daemon/server"
domain: "web-setup"
stack: ["rust", "daemon"]
concerns: ["security", "observability"]
tags: ["req-572", "br-4", "web-setup", "announcement-budget", "transcript-injection", "silent-suppression", "audit"]
---

## Description

REQ-572 BR-4 has two legs: a refused `web/setup_commit` answers `NOT_ATTACHED`
(enforcement), and the session whose configuration was reached for gets a
`WebSetupRejected` notice in its transcript (announcement). The post-merge
security re-verification found the announcement leg defeasible — rated High.

`ConnState::may_announce_setup_rejection` (`crates/tetond/src/server.rs:738`)
budgets the notice at **one per connection lifetime**, via an unconditional
`AtomicBool::swap`. Its caller, `refuse_commit_without_session_access`
(`crates/tetond/src/server.rs:2282`), spends the budget **before and
independently of** whether the publish reached anyone: the event is
session-scoped, so a publish into a session nobody holds reaches nobody — and
still costs the connection its one notice. Three consequences:

1. **Burn attack.** One `web/setup_commit` against a nonexistent session id of
   plausible length (≤ 31 bytes — `sess-` + 26, `within_minted_length`,
   `crates/tetond/src/sessions.rs:302`) passes the length gate, fails
   `may_drive`, spends the bool, and publishes to nobody. Every later refused
   commit from that connection against **real** sessions is then silent — no
   event, and no stderr fallback either. The attacker needs no knowledge of
   any real session id to burn the budget; the real ids come afterwards, from
   the ungated `session/list`.
2. **Wrong key shape.** The budget's *audience* is per-session (the notice
   lands in the targeted session's transcript) but its *key* is
   per-connection. A connection that attacks session A and then session B
   announces only into A; B's user — a different person watching a different
   transcript — never learns their session was reached for, though no notice
   was ever written into B.
3. **The bound doesn't bind.** The field's own doc comment
   (`crates/tetond/src/server.rs:651`) concedes the budget is per connection
   "so it does not bound a peer that reconnects" — reconnecting buys a fresh
   bool. So the design gives up the flood-bounding motive while still paying
   the silent-suppression cost: an attacker who *wants* volume reconnects,
   and an attacker who wants *silence* burns one call.

Enforcement is intact: every offending commit still answers `NOT_ATTACHED`,
and the write never happens. This is the BR-4 announcement leg only — but
that leg is the half that exists to put the attempt **in front of a human**
(LESSON-505), and it is defeated by the first call an attacker makes.

## Reproduction Steps

1. As a same-UID peer, connect to the daemon socket without attaching to any
   session.
2. Send `web/setup_commit` with `session_id: "sess-aaaaaaaaaaaaaaaaaaaaaaaaaa"`
   (26-char body — passes `refuse_unmintable_session_id`, names nothing).
   Answer: `NOT_ATTACHED`. The `WebSetupRejected` publish targets a session
   nobody holds; the budget is now spent.
3. Read a real session id from `session/list`, then send `web/setup_commit`
   against it (any number of times, any number of distinct real sessions).

## Expected Behavior

Per REQ-572 BR-4/AC-4 ("each attempt emits"), every refused commit against a
session someone actually holds announces into that session — or, if a budget
suppresses repeats, the suppression itself is visible the way the grant
budget's arrears figure makes it visible.

## Actual Behavior

Step 3's refusals are enforcement-only: `NOT_ATTACHED` to the attacker, and
nothing — no event, no arrears on a later notice, no stderr line — for the
sessions' users. The one notice the design considered sufficient was spent in
step 2, on nobody.

## Environment

- Platform: all
- Version: unreleased — introduced by REQ-572 (PR #124) on `main` after
  v0.1.14; no tagged release carries it yet

## Root Cause

The budget conflates two different questions — "has this *connection* been
noisy?" and "has this *session's user* been told?" — and answers both with
one connection-keyed bool that is debited at decision time rather than at
delivery time. The neighbouring precedent got both points right:
`GrantAnnouncementBudget::take` (`crates/tetond/src/server.rs:682`) carries a
saturating `suppressed` count out with the next granted announcement, so
suppression is itself visible; and it is windowed rather than lifetime, so a
legitimate later notice is not forfeit to an earlier one. The
setup-rejection budget took the same idea "to its limit" (the doc comment's
words) and in doing so dropped both properties: no arrears, no window, and a
spend that doesn't check whether the notice it was spending existed for
anyone.

The REQ-572 verify FIX 1c reasoning ("the second notice says nothing the
first did not") holds only when the first notice reached the same audience
the second would have — exactly the property the per-connection key fails to
guarantee.

## Remediation Direction (from the audit — to validate during fix)

Either of:

- **Re-key and carry arrears**: budget per `(connection, session_id)` with a
  `GrantAnnouncementBudget`-style suppressed count, so a real session's first
  notice always lands and suppression of repeats is visible in the next one.
  A bounded map (the ids are ≤ 31 bytes and the set of *real* sessions is
  daemon-bounded) — mind that an attacker-supplied nonexistent id must not
  become an unbounded allocation keyed to a connection, the same trap
  `session/attach`'s length gate exists for.
- **Spend only on delivery**: debit the budget only when the publish had at
  least one recipient, so a notice into nobody costs nothing. Cheaper, fixes
  consequence (1); consequences (2) and (3) would need the re-key anyway or a
  recorded spec deviation.

Whichever lands, reconcile with BR-4/AC-4's "each attempt emits" wording — if
any budget survives, the deviation belongs in the REQ-572 architecture
spec-mapping table next to the FIX 1b/1c entries that already amended this
clause once.

## Bundled Residuals (same audit, lower severity — fix in the same pass)

- **(a) Silent TOCTOU-check downgrade on version skew.** In the CLI commit
  flow (`crates/teton/src/web_setup_ui.rs:433`), a daemon that offers no
  preview digest (a pre-BR-7 daemon across version skew) degrades to the
  protocol's "do not check" — correctly, but silently. Render a one-line
  notice that the confirmed-bytes check is off, so the downgrade is a fact
  the user saw rather than one they'd need the source for.
- **(b) `WebSetupCompleted.config_path` still rides the wire.** The event
  (`crates/teton-protocol/src/events.rs:1572`, published at
  `crates/tetond/src/runtime.rs:4085`) carries the absolute config path;
  the CLI no longer renders it. Monitor-scope connections still receive it.
  Decide: blank/relativize it at publish, or record why an absolute local
  path in a broadcast event is acceptable.
- **(c) Length-gate consistency across `may_drive` seams.**
  `refuse_unmintable_session_id` guards the three setup handlers, and
  `session/attach` has its own inline check (`crates/tetond/src/server.rs:3088`),
  but the remaining handlers that hash an attacker-supplied `session_id`
  through `may_drive` have no length gate: `handle_web_override`
  (`crates/tetond/src/server.rs:2184`), `handle_session_permissions`
  (`:2424`), `handle_session_clear` (`:3510`), and the prompt path
  (`spawn_prompt_turn`, `:1703`). (`handle_permission_respond` is fine — its
  `may_drive` takes a registry-resolved owner, not the caller's string.)
  Apply the F9 rule uniformly; per LESSON-502, each seam needs its own line
  and its own mutation-killable test.
- **(d) The credential-prohibition content pin is weaker than it reads.**
  `the_system_prompt_forbids_asking_for_a_credential_in_the_conversation`
  (`crates/tetond/src/harness/turn_loop.rs:2104`) pins three needles, but
  the `"keychain"` needle is vacuous — `self_config.md` names `keychain://`
  in its `[web]` reference line independently of the prohibition sentence,
  so that needle stays green with the prohibition gone. And all three
  needles are substring checks, so an `"…unless they offer"` weakening of
  the prohibition sentence passes untouched. Pin the sentence tightly enough
  that softening it fails the test.

## Resolution

Both remediation directions landed, because each fixes what the other cannot:

- **Re-keyed per (connection, session).** `ConnState::setup_rejection_spent`
  (a lifetime `AtomicBool`) became `setup_rejections_announced`
  (`Mutex<HashSet<SessionId>>`); `may_announce_setup_rejection` claims a
  (connection, session) pair atomically via the set insert. A connection
  refused on session A and then on session B now announces into both — each
  targeted session's user hears about each offending connection exactly once
  (consequence 2).
- **Spent only for sessions that exist.** `refuse_commit_without_session_access`
  asks the registry (`SessionRegistry::contains`, added — `get` without the
  summary clone) *before* consulting the budget. A refused commit against an
  id that names nothing publishes nothing and burns nothing (consequence 1),
  and — a gain the report had not asked for — monitor-scope subscribers, who
  receive every session-scoped envelope, no longer receive `WebSetupRejected`
  envelopes wearing attacker-chosen ids. The ordering also keeps the budget
  set bounded by daemon-minted ids: junk ids never enter it, which is the
  `session/attach` allocation trap the report flagged.
- **Delivery-side sliver accepted and recorded**: an existing session with no
  attached client at publish time still spends its (connection, session)
  notice into an empty room. The bus is broadcast-with-receive-side-filtering
  (`may_receive` runs in each connection's forwarder), so publish cannot count
  entitled recipients without new machinery; the spend is now scoped to that
  one session rather than the connection's whole budget, which reduces the
  cost of the sliver from "all future notices" to "that session's one notice".
- **No arrears, and why that differs from the grant budget** (the Expected
  Behavior's "or" branch): `GrantAnnouncementBudget`'s arrears exists because
  each suppressed grant announcement carries information (a different grant
  was minted). A suppressed rejection here is a byte-identical duplicate to
  the identical audience; a count of unsent duplicates tells the reader
  nothing. Recorded in the field docs and the architecture deviation row.
- **BR-4/AC-4 reconciled**: the REQ-572 architecture spec-mapping row for the
  rejection announcement now records the re-key, the existence gate, the
  no-arrears rationale, and the four pinning tests.
- Consequence 3 (reconnect refreshes the budget) is unchanged and remains the
  stated limit, for the reason the field docs already carried: the daemon has
  no subject an attacker cannot re-mint.

Verified by mutation: deleting the existence gate fails
`a_nonexistent_session_buys_no_notice_and_burns_no_budget`; reverting the key
to the connection alone fails
`a_refusal_against_a_second_session_announces_into_that_session_too`.

Bundled residuals, all landed in the same pass:

- **(a)** `/web setup` renders `DIGEST_CHECK_UNAVAILABLE` when the daemon
  offers no preview digest — placed **before** the confirm question, because
  declining is the one act the flow offers a user who minds that the commit
  is no longer pinned to the previewed bytes. Pinned both ways (notice when
  absent, no notice when present) in
  `a_previewed_digest_rides_the_commit_and_an_absent_one_does_not`.
- **(b)** `WebSetupCompleted.config_path` **stays on the wire, recorded as a
  decision** on the field's doc comment: every receiver is same-UID (monitors
  additionally consent-granted), the path is derivable by any such peer
  (`teton status` serves it), so blanking protects nothing from the only
  parties who can receive it — while the real exposure (a username on
  somebody else's screen) is a rendering concern already handled by the CLI
  deliberately not printing it (`format_web_setup_completed`).
- **(c)** `refuse_unmintable_session_id` now guards `web/override`,
  `session/permissions`, `session/clear`, and the `session/prompt` spawn;
  its doc names the full seam list and why `permission/respond` needs no gate
  (its `may_drive` takes a registry-resolved owner, never the caller's
  string). Each seam pinned:
  `an_unmintable_session_id_is_refused_by_every_driving_method_before_the_gate`
  and `an_unmintable_session_id_is_refused_by_the_prompt_spawn`.
- **(d)** The credential-prohibition pin is whole-line **equality** on the
  exact sentence, on both model profiles. Verified by mutation: appending
  "unless they offer it" to the guide's sentence fails the test that the old
  substring needles passed.

## Files Changed

- `crates/tetond/src/server.rs` — the re-keyed budget
  (`setup_rejections_announced`), existence-gated spend in
  `refuse_commit_without_session_access`, length gates at the four driving
  seams, doc rewrites, four new/updated tests.
- `crates/tetond/src/sessions.rs` — `SessionRegistry::contains`.
- `crates/tetond/src/harness/turn_loop.rs` — whole-line-equality content pin.
- `crates/teton/src/web_setup_ui.rs` — `DIGEST_CHECK_UNAVAILABLE` notice
  before the confirm question; test pins both directions.
- `crates/teton-protocol/src/events.rs` — the `config_path` on-the-wire
  decision recorded on the field.
- `.adlc/specs/REQ-572-capability-aware-refusals-and-guided-enablement/architecture.md`
  — spec-mapping deviation row amended (BR-4/AC-4).
- `CHANGELOG.md` — `[Unreleased]` entry.
