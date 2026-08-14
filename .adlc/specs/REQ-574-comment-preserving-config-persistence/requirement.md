---
id: REQ-574
title: "Comment- and unknown-key-preserving config persistence"
status: approved
deployable: true
created: 2026-08-14
updated: 2026-08-14
component: "daemon/config"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["developer-experience", "reliability", "security"]
tags: ["config", "toml", "toml-edit", "comment-preservation", "config-persistence", "web-setup", "atomic-write", "unknown-keys", "preview"]
---

## Description

Every daemon-side config write today rewrites the whole document: the shared
seam (`write_config_atomically`, `crates/tetond/src/runtime.rs`) serializes the
entire in-memory `Config` via `Config::to_toml()`. Comments are destroyed, key
order is normalized, and unknown keys are silently dropped (`Config` ignores
them at load, so a re-serialization cannot carry them). Five operations write
through this seam: REQ-563's "enable permanently" consent answer
(`persist_web_tier`), REQ-572's `/web setup` commit (`web_setup_commit`),
provider/routing updates over RPC (`apply_config_update`), and the two startup
migrations (REQ-557 model migration, routing-category migration).

The contradiction is now user-facing. The README teaches a hand-written,
heavily commented `[web]` block *and* `/web setup` in the same section — so a
user who follows the docs in order writes comments that the very next
daemon-side write destroys. REQ-572's review flagged this (reflector finding
M3); the interim floor was an unconditional preview warning ("saving rewrites
the whole config file — comments and unrecognized keys do not survive"), which
discloses the destruction without preventing it. This REQ removes the
destruction: daemon-side writes become in-place edits of the on-disk document
(`toml_edit`-class round-trip editing), so a write changes exactly the keys the
operation is about and everything else — comments, ordering, unknown keys —
survives byte-for-byte.

Why now, and why carefully: REQ-572 BR-7 promises "what the preview shows is
what the commit writes" and enforces it with a whole-document digest that the
commit re-checks (the TOCTOU guard). That digest and the preview's rendered
`[web]` section are today computed from a fresh `Config::to_toml()`
serialization. Under in-place editing those bytes are no longer the bytes that
land, so preview rendering and digesting must move to the edited document or
BR-7's byte-equality becomes a lie. This is why the change is a cross-REQ
behavior change and not a quick fix: it touches REQ-563's consent persistence,
REQ-572's preview/commit contract and its byte-equality tests, and the
migrations' write path, all through one seam.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| ConfigDocument | text | TOML document (on-disk bytes) | The user-authored artifact: comments, key order, whitespace, unknown keys/tables. Preserved by every daemon write except at the keys an operation changes. |
| SemanticDelta | changed keys | set of key-level assignments/removals | The only part of the document a write may touch. Derived from the operation (e.g. `persist_web_tier` touches `web.tier` and `web.permission_allow` only). |
| WrittenBytes | bytes | UTF-8 TOML | The exact bytes the seam puts on disk. The preview's rendered section, the commit digest, and pre-write validation are all computed from these bytes (or the document they serialize from), never from a parallel serialization. |

_Events: none new — `WebSetupCompleted` and the existing config-change
notices are unchanged. Permissions: unchanged — config writes remain behind
the existing RPC authorization and consent gates; this REQ changes how bytes
land, not who may land them._

## Business Rules

- [x] BR-1: **A write touches only its keys.** A daemon-side config write
      changes exactly the keys its operation semantically changes (including
      removals performed by migrations). Every other part of the on-disk
      document — comments (line and inline), key order, whitespace style,
      unknown keys and unknown tables — survives byte-for-byte. This holds for
      all five writers: `persist_web_tier`, `web_setup_commit`,
      `apply_config_update`, the REQ-557 model migration, and the
      routing-category migration. (informed by REQ-563, REQ-572)
      *Witnessed per writer in `crates/tetond/tests/config_preservation.rs`
      (one test each, exact-line-delta assertions), plus
      `an_unknown_key_inside_a_provider_entry_survives_a_registration` for the
      unknown-key-inside-an-array case and
      `an_inline_web_table_keeps_the_keys_the_consent_answer_is_not_about` for
      the inline spelling.*
      **Recorded exception:** an array of tables a write genuinely *reshapes* —
      shrunk or reordered, so no element correspondence survives — is re-rendered
      canonically where it stood, and comments inside that array go with it.
      Appends and per-element edits preserve (architecture ADR-1's amended array
      rule; pinned by
      `config_doc::a_reshaped_array_of_tables_is_replaced_wholesale_where_it_stood`).
- [x] BR-2: **One write body stays one, and keeps its guarantees.** The daemon
      retains exactly one config-write body (the current
      `write_config_atomically` invariant, the "single commit point" REQ-572
      BR-11 relies on). Preservation is implemented at that seam, not
      per-caller. Atomicity (temp file + fsync + rename), permission-mode
      preservation, and the `0600` first-write fallback are unchanged — the
      file can hold secret-adjacent material and must never widen.
- [x] BR-3: **The preview shows the bytes that land.** `/web setup`'s preview
      `toml` field is the `[web]` table exactly as it will appear in the
      written document — sliced from the edited document, never rendered by a
      fresh serialization — so a user's own comments inside `[web]` appear in
      the preview they confirm. The confirm digest is computed over the
      WrittenBytes. REQ-572 BR-7's byte-equality ("what the user confirmed is
      what is written") survives the seam change, and is strengthened: any
      on-disk change between preview and commit — including a comment-only
      hand edit — changes the digest and the commit refuses with the existing
      stale-digest error. (informed by REQ-572, LESSON-510)
- [x] BR-4: **Validate what will actually land.** The pre-write validation
      gate (`Config::validate`, the same validator startup runs) is applied to
      the parse of the WrittenBytes, so "the candidate validates" and "the
      file the daemon boots on validates" cannot come apart. Read-back
      equivalence is the contract: parsing the written document through the
      production loader yields the semantic `Config` the caller committed at
      every key the operation changed, and the prior on-disk semantics at
      every key it did not. Canonical byte form is explicitly *not* the
      contract. Stated consequence, and a deliberate behavior change: a hand
      edit that parses but fails validation makes subsequent daemon writes
      refuse with the validator's message — today's seam would silently
      overwrite (and thereby erase) the invalid edit. Refusal is the fail-safe
      choice: the daemon neither destroys the user's edit nor writes a
      document it would not boot on. (informed by LESSON-501)
      *The gate is `render_config_document`'s `Config::load` on the edited
      bytes; witnessed by
      `config_preservation::a_hand_edit_that_fails_validation_refuses_both_writers_and_survives_them`
      and `runtime::tests::config_document_seam::a_hand_edit_that_fails_validation_refuses_the_write_and_survives_it`.*
- [x] BR-5: **The edit base is the file, and drift survives.** The document
      edited is the on-disk document as read at write time, under the same
      config-mutex critical section that already serializes writes. A hand
      edit made while the daemon runs is therefore no longer clobbered by the
      next daemon write — it rides along untouched (the daemon's in-memory
      view stays blind to it until restart, exactly as today; hot-reload is
      out of scope). Where a hand edit collides with the operation's own keys,
      the operation's value wins at those keys only.
      *Witnessed at the engine by
      `config_doc::a_hand_edit_the_daemon_never_read_rides_along_untouched`, at
      a daemon writer by
      `config_preservation::a_hand_edit_mid_session_survives_a_provider_registration`,
      and for the collision half by `runtime::tests::web_setup_flow`'s
      pinned-field group (`an_answer_the_document_lost_is_written_back_even_when_memory_agrees`,
      `a_hand_deleted_web_table_is_restored_by_the_next_commit`,
      `a_document_that_already_holds_the_answer_still_makes_the_capability_live`,
      `a_removal_note_describes_the_document_and_not_a_stale_memory`).*
- [x] BR-6: **Degradation is a loud refusal, never a silent rewrite.** If the
      on-disk document cannot be parsed for editing at write time (e.g. a
      half-finished hand edit), the write refuses with an error naming the
      parse failure, writes nothing, and leaves in-memory state unchanged
      (the existing swap-after-write ordering). Falling back to full
      re-serialization — destroying the user's in-progress edit to make the
      write succeed — is forbidden. A missing file is not an error: the edit
      base is the empty document and the write produces a fresh document whose
      parse equals the candidate, at mode `0600`. (informed by LESSON-456,
      BUG-146)
- [x] BR-7: **The disclosure warning retires with the behavior it disclosed.**
      The unconditional `/web setup` preview warning ("saving rewrites the
      whole config file…") is removed — with preservation in place it would be
      false. The README's hand-written commented `[web]` example and
      `/web setup` stop being in tension, and the README's drift-check note is
      updated accordingly.

## Acceptance Criteria

- [x] AC-1: A config file containing the README's commented `[web]` example
      verbatim (comments, `search_auth = "X-Subscription-Token: {key}"`, key
      order), plus an unknown key inside a known table and an unknown
      top-level table, is put through `persist_web_tier`: every comment, the
      unknown key, the unknown table, and the key order survive; only the
      operation's keys differ. The README block is the test vector, not a
      paraphrase of it. (informed by LESSON-512, BUG-165)
      *`config_preservation::a_consent_answer_moves_its_own_keys_and_leaves_the_readme_config_alone`,
      with `the_fixture_is_the_readmes_own_block_byte_for_byte` reading README.md
      at test time so "verbatim" is machine-checked rather than asserted.*
- [x] AC-2: The same preservation property is exercised and asserted for
      `web_setup_commit`, `apply_config_update` (provider registration), and
      both startup migrations (whose writes today also canonicalize the file).
      *One test per writer in `config_preservation.rs`:
      `a_setup_commit_writes_the_bytes_its_preview_showed_and_moves_nothing_else`,
      `registering_a_provider_leaves_the_web_table_and_its_comments_alone`,
      `the_model_migration_carries_a_commented_config_across_the_upgrade`,
      `the_routing_migration_retires_its_table_without_taking_the_rest_of_the_file`.*
- [x] AC-3: With a commented on-disk config, `web_setup_preview`'s `toml`
      matches byte-for-byte the `[web]` section of the file the subsequent
      commit writes, and the preview digest equals the digest of the written
      file's full bytes. The existing BR-7 byte-equality tests (the
      preview/commit agreement suite in `runtime.rs` and the
      `web_consent_matrix.rs` TASK-129 pin) are updated to assert this
      stronger property — preview rendering read off the edited document —
      and pass.
      *`config_preservation::a_setup_commit_writes_the_bytes_its_preview_showed_and_moves_nothing_else`
      carries both halves (section byte-equality and whole-file digest) over the
      README fixture; the `runtime.rs` agreement suite passes under the new
      derivation.*
      **Deviation:** the `web_consent_matrix.rs` TASK-129 pin was deliberately
      *not* reseeded with a commented config. It drives `FileTierSink`, a test
      double that re-serializes with `Config::to_toml()`, and its preview leg
      runs on a `minimal()` runtime with no config path at all — so a
      preservation assertion there would be a claim about the double, not about
      the commit path (LESSON-451). The pin keeps its original subject (the
      candidate bytes answer no consent question); preservation is asserted
      where the production writer actually runs.
- [x] AC-4: A comment-only hand edit landing on disk between preview and
      commit causes the commit to refuse with the existing stale-digest
      message; nothing is written.
- [x] AC-5: An unparseable on-disk config at write time causes each writer to
      refuse with an error that carries the underlying parse failure; the
      file's bytes are untouched and the in-memory config is unchanged.
      Startup-migration writers warn and continue (their existing
      failure-tolerance), naming the parse failure.
      *RPC writers:
      `config_preservation::an_unparseable_document_is_refused_by_the_writers_that_would_have_rewritten_it`;
      `persist_web_tier` at the seam:
      `runtime::tests::config_document_seam::an_unparseable_document_refuses_the_write_and_names_the_parse_failure`;
      the startup-migration leg:
      `runtime::tests::config_document_seam::a_migration_that_cannot_edit_the_document_warns_and_keeps_the_session`,
      which asserts the in-memory migration still stands, the broken file is
      byte-identical afterwards, and the warning names the parse failure. The
      parse failure is located (line and column) and never quoted — a config
      line can hold a credential —
      (`a_parse_failure_of_the_edited_bytes_is_located_and_never_quoted`).*
- [x] AC-6: With `config_path` set but no file on disk, a write produces a
      fresh `0600` document whose parse equals the candidate (existing
      first-write behavior preserved).
- [x] AC-7: The unconditional rewrite warning no longer appears in any
      preview; the remaining conditional warnings are unaffected.
- [x] AC-8: Written bytes are parsed back through the production loader in
      tests and yield the expected semantic config (the existing
      `web_consent_matrix.rs` read-back posture, retained under the new seam).
      *Every test in `config_preservation.rs` ends in `Daemon::reload()`, which
      is `Config::load` over the file's actual bytes; the engine's unit tests do
      the same and additionally assert `reloaded == candidate`.*
- [x] AC-9: A config file at `0600` remains `0600` after a write; the
      temp-file + rename atomicity tests (including the fixed-temp-path
      concurrent-commit test) pass unchanged. Concurrent `web_setup_commit`
      calls still serialize under the config mutex. (informed by BUG-161)
- [x] AC-10: A parseable on-disk config carrying a hand edit that fails
      `Config::validate` (e.g. an invalid value in a key unrelated to the
      operation) causes `persist_web_tier` and `web_setup_commit` to refuse
      with the validator's own message; the file's bytes are untouched and
      the invalid edit is not overwritten (BR-4's stated consequence).
      *`config_preservation::a_hand_edit_that_fails_validation_refuses_both_writers_and_survives_them`
      and `runtime::tests::config_document_seam::a_hand_edit_that_fails_validation_refuses_the_write_and_survives_it`.
      The validator's sentence is passed through verbatim (it names the key to
      fix); only the parse arm is reduced to a location.*

## External Dependencies

- `toml_edit` (format-preserving TOML editing). Note: the workspace's existing
  `toml = "0.8"` is itself implemented on top of `toml_edit`, so this is
  expected to add little or no new transitive weight — confirm the exact
  version pairing at architecture time.

## Assumptions

- `toml_edit`'s round-trip guarantee (comments, ordering, whitespace preserved
  for untouched keys) holds for the constructs the config uses, including
  arrays of tables and inline tables. Validate against the README example
  early. **Held, with one thing it does not give you:** untouched keys round-trip
  exactly as assumed, but section *placement* is a numeric render order rather
  than a tree, and an array element's sub-tables are parented by the `[[…]]`
  header preceding them. Sections this engine adds near or inside an array must
  therefore be positioned deliberately, or an existing `[providers.capabilities]`
  re-parents onto the wrong entry and the document stops parsing. Recorded in
  architecture ADR-1's limitations, with two witnesses.
- `Config` continues to *ignore* unknown keys at load (no
  `deny_unknown_fields`); preservation at write is the fix, and schema
  widening is neither needed nor wanted.
- One daemon process writes a given config file at a time (the existing
  socket/singleton model); cross-process writer coordination is no worse than
  today, and the digest guard covers the setup flow's window.
- `Config::to_toml()` remains available for fresh-document cases (missing
  file, tests); it stops being the path by which an *existing* document is
  rewritten.

## Open Questions

- [x] OQ-1: Key *removal* semantics for migrations: when the routing migration
      retires a key/table, do comments attached to the removed key go with it,
      or are they preserved as orphans? Proposed: attached comments go with
      the key they document; free-standing comment blocks survive.
      Architecture decides the attachment rule `toml_edit` can actually honor.
      **Answered — architecture ADR-1, "Key removal" bullet:** the proposal is
      what `toml_edit` honors and what shipped. `Table::remove` takes the key
      together with the decor prefixed to it; a comment documenting a key
      outlives that key only as a lie. Witnessed by
      `config_doc::removing_a_key_takes_its_own_comment_and_leaves_its_neighbours_alone`
      and, at a writer,
      `config_preservation::the_routing_migration_retires_its_table_without_taking_the_rest_of_the_file`.
      The same rule decides where a comment goes when a key changes *spelling*
      (`an_inline_array_of_tables_is_rewritten_as_a_block_the_document_can_hold`):
      it travels with its key.
- [x] OQ-2: How the SemanticDelta is derived at the seam: callers currently
      hand the seam a whole candidate `Config`. Options are a diff between
      current and candidate semantic configs, or callers passing an explicit
      delta. Architecture-phase decision; the behavioral contract (BR-1,
      BR-4, BR-5) is the same either way.
      **Answered — architecture ADR-1:** diff the *canonical serializations* of
      the caller's `current` and `candidate` (both already in hand, since every
      candidate is built by clone-and-mutate); no caller passes an explicit
      delta. Diffing the document against the candidate instead would classify
      drift as "changed" and write the stale in-memory value back — BR-5's
      clobber. ADR-1's granularity half is also answered there, and was
      *amended* during implementation: arrays of tables are element-wise, not
      one key.
- [x] OQ-3: Should `persist_web_tier`'s no-op check (`candidate.web ==
      config.web`) also become drift-aware — i.e. skip the write only when the
      *on-disk* document already holds the durable state — or keep comparing
      in-memory state (status quo)? Matters when a hand edit already enabled
      the tier the consent answer would persist.
      **Answered — architecture ADR-4:** status quo for `persist_web_tier` and
      the other three `persist_config` writers — a drift-aware check would need
      a disk read the writer does not otherwise make, and the cost of a
      "redundant" write is now a preserved file rather than a clobber. Amended
      for `/web setup`'s commit alone (3c520b9), which reads the file anyway to
      derive its preview: there the test is the conjunction of byte-identity and
      in-memory equality, with a witness at each edge in
      `runtime::tests::web_setup_flow`.

## Out of Scope

- Hot-reloading hand edits into the running daemon's in-memory config (BR-5
  deliberately keeps the daemon blind to drift until restart).
- Widening the `Config` schema or adding `deny_unknown_fields`.
- Any non-daemon config writer (the CLI does not write the config directly;
  all writes arrive over RPC or at daemon startup — verified against the
  current tree).
- Repairing or auto-formatting a broken config file (BR-6 refuses instead).
- Cross-process file locking beyond today's guarantees.
- Preserving formatting of the *keys an operation changes* (a changed value
  may be re-rendered in canonical value syntax; its comment handling follows
  OQ-1).

## Retrieved Context

- LESSON-512 (lesson, score 9): A spec's named example is a test case, not decoration
- BUG-165 (bug, score 9): The search credential only speaks Bearer, and the spec's own example backend doesn't
- REQ-572 (spec, score 9): Capability-aware refusals and guided in-session enablement
- REQ-560 (spec, score 8): Named permission levels and the interactive session status line
- REQ-563 (spec, score 8): Opt-in web lookup through the egress choke point
- BUG-161 (bug, score 8): Permission request_ids collide across concurrent sessions
- LESSON-501 (lesson, score 8): State carried past its creator's lifetime sheds invariants silently
- REQ-567 (spec, score 8): Cross-prompt conversation carry in interactive sessions
- LESSON-495 (lesson, score 8): A remembered grant answers every question its key matches
- LESSON-456 (lesson, score 8): A `_`-discarded error is a silent downgrade
- BUG-146 (bug, score 8): First prompt after install fails with a message blaming the local engine
- REQ-547 (spec, score 8): First-run local model consent
- LESSON-510 (lesson, score 7): Existence is not freshness
- REQ-548 (spec, score 7): One-command Homebrew install and the tetoncode.ai landing page
- REQ-554 (spec, score 7): Local tier renders prompts through the model's native chat template
