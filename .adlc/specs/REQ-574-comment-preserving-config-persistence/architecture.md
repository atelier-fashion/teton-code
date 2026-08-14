# REQ-574 — Architecture: Comment- and unknown-key-preserving config persistence

## Approach

The daemon's five config writers all funnel through one seam
(`write_config_atomically`, `crates/tetond/src/runtime.rs:4658`), which today
serializes the whole in-memory `Config`. The change splits that seam into a
**pure delta engine** (teton-core) and the existing **atomic I/O mechanism**
(tetond), and re-derives the `/web setup` preview artifacts from the edited
document so REQ-572 BR-7's "what you confirmed is what lands" survives as
byte-truth.

```
caller (current: &Config, candidate: &Config)
   │
   ▼
persist_config(path, current, candidate)          [tetond, NEW wrapper]
   ├─ read on-disk text (missing → "")
   ├─ apply_config_delta(text, current, candidate) [teton-core, NEW, pure]
   │     parse text with toml_edit  ── parse error → typed refusal (BR-6)
   │     diff canonical(current) vs canonical(candidate), recursive over tables
   │     set / remove exactly the differing keys in the document
   ├─ Config::load(edited_text)  ── validate the bytes that will land (BR-4)
   └─ write_config_atomically(path, edited_text)   [tetond, now takes text]
         temp file + mode preservation + fsync + rename  (unchanged mechanics)
```

`web_setup_preview` and `web_setup_commit` share one derivation helper that
returns `(edited_text, web_section, digest)`; the preview's `toml` field is the
`[web]` table **sliced from the edited document**, and the digest is
`sha256(edited_text)` — the exact bytes a commit would write.

## Key Decisions

### ADR-1: The delta is `diff(current, candidate)`, applied to the on-disk document read at write time

The semantic delta is computed by diffing the **canonical serializations** of
the caller's pre-mutation `current` and its `candidate` (both already in hand
at every call site — candidates are built by clone-and-mutate), then applying
exactly those key-level sets/removals to the on-disk document parsed with
`toml_edit`.

- **Why diff current-vs-candidate, not disk-vs-candidate**: the operation's
  footprint is precisely where `current` and `candidate` differ. Diffing
  against the on-disk parse would classify unrelated hand-edit drift as
  "changed" and write the in-memory value back over it — exactly the clobber
  BR-5 forbids. With current-vs-candidate, drift at untouched keys is never in
  the delta, so it survives by construction.
- **Missing file**: the edit base is the empty document and the delta base is
  `Config::default()` (the parse of an empty document), so every non-default
  candidate key is written and AC-6's "parse equals the candidate" holds.
- **Array granularity (resolves spec OQ-2's granularity half)** — *amended
  during implementation; the rule below is what shipped*. Tables recurse
  key-by-key. **Value arrays are one key.** **Arrays of tables are diffed
  element-wise**, in four cases (`plan_array_edit`):
  - *untouched* — the canonical arrays agree, and the document's array is not
    read at all;
  - *append* — the common prefix agrees and the candidate is longer. The
    existing elements are not read, re-rendered or moved; only the new ones are
    written, at the render position past the whole of the array already there
    (its nested sub-tables included — see the limitations below);
  - *per-index* — same length, some elements differ. Each differing element is
    recursed into as a table, so only the keys that changed inside it move;
  - *wholesale* — shrunk, reordered, or the document spells the array some other
    way. No index correspondence to trust, so the array is replaced with the
    candidate's canonical rendering where it stood, comments inside it and all.

  The original decision wrote arrays off as one key on the ground that
  element-wise diffing needs an identity function per array type. That is right
  about identity and wrong about the cost: BR-1 says comments and unknown keys
  survive **for all five writers**, and the two that touch `[[providers]]` are
  precisely provider registration (an append) and the REQ-557 model migration
  (one key added per entry). Under the wholesale rule those two writers deleted
  every comment and unknown key inside `[[providers]]` — BR-1 broken by the only
  writers that could break it. So the two shapes that *do* have a trustworthy
  correspondence get one, and everything else keeps the recorded exception.

  **The index-matching residual.** Per-index matching trusts position, and BR-5
  leaves the daemon blind to drift until restart. A user who *reorders*
  `[[providers]]` by hand mid-session, without changing how many there are, and
  is then hit by a same-length element edit, gets that edit applied at the
  position rather than to the provider. The result still passes
  `Config::validate` before it lands (BR-4), and the alternative — wholesale —
  destroys strictly more in that same scenario. Recorded, not fixed: fixing it
  means the per-array identity function this ADR declined, which a `providers`
  array whose ids are themselves editable does not actually escape.

  **The inline spelling reshapes.** A document that spells the array as values
  (`providers = [ { … } ]`) cannot hold the delta's sections, so that one edit
  rewrites the key into `[[providers]]` blocks and moves the key's comment onto
  the first header (OQ-1: a comment travels with its key).
- **Key removal (resolves spec OQ-1)**: a key present in canonical(current)
  but absent in canonical(candidate) is removed from the document. toml_edit
  removes the key together with its attached decor (the comment block prefixed
  to that key), which is the proposed semantics the spec records: attached
  comments travel with their key; free-standing table decor survives. The
  existing `an_answer_that_omits_a_key_removes_it_and_says_so` behavior
  (runtime.rs:15967) is preserved through this path.

#### Recorded limitations of the shipped engine

Three things the implementation does that this ADR did not originally say, kept
here rather than left to be rediscovered:

1. **CRLF is normalized on the first write.** `toml_edit` re-renders line
   endings as `\n`, so the first daemon-side save to a CRLF document converts
   the whole file. Nothing else about it moves. Witness:
   `config_doc::the_first_write_to_a_crlf_document_normalizes_its_line_endings`.
2. **A whitespace-only file is the empty edit base.** The engine treats a
   document holding only whitespace as the empty document
   (`document_is_effectively_empty`): there is nothing in it to preserve, and
   editing around its blank lines would carry them forward forever. The engine
   only decides the *edit* base; the *delta* base is the caller's, and a caller
   that hands in a non-default `current` for such a file would get a document
   naming only the delta's keys.
3. **Empty-vs-missing is aligned at the call site.** `render_config_document`
   adopts the same predicate, so a whitespace-only file selects
   `Config::default()` as the delta base exactly as a missing one does, and the
   next write heals it into a whole document (verified in `runtime.rs`, the
   `present` binding). This is what commit 3c520b9 adopted; the two bases now
   agree, and AC-6's "parse equals the candidate" holds for both.

**Element positions are a render order, not a tree.** `toml_edit` renders
sections sorted by a numeric position, and an array element's sub-tables are
parented by the `[[…]]` header that precedes them rather than by their own
header path. So any section this engine *adds* near an array must be positioned
against the whole array — element headers and their sub-tables — and a section
added *inside* an element takes that element's own position. Getting either
wrong re-parents an existing `[providers.capabilities]` onto the wrong entry and
yields a document that does not parse, which the write then refuses. Witnesses:
`config_doc::a_second_append_renders_past_the_first_ones_sub_table` and
`config_doc::a_section_added_inside_an_array_element_renders_beside_that_element`.

### ADR-2: The delta engine is pure and lives in teton-core; I/O and atomicity stay in tetond

`apply_config_delta(doc_text, current, candidate) -> Result<String, DeltaError>`
is text-in/text-out with no filesystem access, so it belongs in
`crates/teton-core/src/config_doc.rs` next to the `Config` schema it must
understand — the "policy is pure, mechanism is gated" pattern
(architecture.md). teton-core gains `toml_edit = "0.22"` as a direct
dependency; that is a parsing library, not I/O, and it is **already in the
lockfile transitively** via `toml 0.8` (toml_edit 0.22.27), so the tree gains
no new code. tetond keeps the atomic-write mechanics (`write_config_atomically`
now takes the pre-rendered text) and the new `persist_config` wrapper — still
exactly one config-write body (spec BR-2, REQ-572 BR-11).

The section-extraction helper (`table_section(doc_text, "web")`) also lives in
config_doc.rs: it renders one table, decor included, from the edited document.
`web_table_toml` (config.rs:531) is retired from the preview path; it remains
only if tests still need a canonical section render, otherwise deleted.

### ADR-3: Preview and commit share one derivation — the test double shares the production commit path

Per LESSON-451 (a seam fakes the boundary, never the commit path) and
LESSON-502 (an invariant at N seams needs a witness at each), preview and
commit both call the same `render_persisted_document(path, current, candidate)`
helper; the preview digests and slices what that helper returns, and the
commit re-derives through the same helper and compares digests before handing
the same text to the writer. There is no second renderer to drift. The digest
remains whole-document (REQ-572's TOCTOU posture) and is now *strengthened*:
any on-disk change between preview and commit — including comment-only hand
edits — changes the edited text, hence the digest, hence refuses with the
existing `SETUP_DIGEST_STALE` message (spec BR-3, AC-4).

### ADR-4: Refusal semantics and the no-op check (resolves spec OQ-3)

- **Unparseable on-disk document** at write time → `DeltaError::Parse`
  carrying the toml_edit error; RPC writers surface it as their existing error
  codes with the inner reason attached (LESSON-456/BUG-146: never a generic
  "write failed"); startup migrations warn-and-continue as they already do.
  No fallback to full re-serialization, ever (spec BR-6).
- **Validation failure of the edited bytes** → the validator's own sentence,
  file untouched, in-memory untouched (spec BR-4 stated consequence, AC-10).
- **No-op checks stay in-memory** (`candidate.web == config.web`): OQ-3 is
  resolved as status quo. A drift-aware no-op would need a disk read inside
  the check, and the write path is now drift-preserving — the cost of a
  "redundant" write is a preserved file, not a clobber, so the extra
  complexity buys nothing user-visible.
- **Amended for `/web setup`'s commit only** (3c520b9): that flow reads the file
  anyway to derive its preview, so its no-op test is the **conjunction** — the
  derived text must be byte-identical to the file *and* the candidate must match
  the live config. The in-memory half alone reported `applied: false` for an
  answer that would have rewritten a drifted document; byte equality alone would
  report `applied: false` when the document already holds the answer but this
  process does not, leaving the capability dark until a restart. Both edges have
  witnesses in `runtime::tests::web_setup_flow`. The other four writers keep the
  in-memory check above — they never read the file before deciding.

## Data model changes

None. `Config`/`WebConfig` schemas, the wire protocol
(`WebSetupPreviewResult.toml/.digest/.warnings`), and the RPC surface are
unchanged in shape; only how bytes are produced changes. The unconditional
"saving rewrites the whole config file" warning (runtime.rs:6758) is removed —
with preservation in place it would be false — and its pinning test
(`every_preview_says_the_save_rewrites_the_whole_file`, runtime.rs:16228)
retires with it.

## Test strategy

- **Unit (teton-core/config_doc.rs)**: delta-engine properties — comment/
  unknown-key/order preservation, key set/remove, array-wholesale rule,
  attached-comment removal, empty-document base, parse-error refusal. The
  README's commented `[web]` block is embedded verbatim as a fixture
  (LESSON-512: a named example is a test case).
- **Per-writer witnesses (LESSON-502)**: each of the five writers gets its own
  preservation test (spec AC-1/AC-2) — `persist_web_tier`, `web_setup_commit`,
  `apply_config_update`, both migrations — asserting comments, an unknown key
  inside a known table, and an unknown top-level table survive, and read-back
  through `Config::load` matches expectations (web_consent_matrix.rs posture).
- **Preview/commit**: strengthen `a_preview_renders_the_bytes_the_commit_goes_
  on_to_write` (runtime.rs:15570) with a commented seed config; keep the
  digest-coverage and stale-digest tests, adding the comment-only-drift
  refusal (AC-4); keep concurrency (16111), permissions (7484), readonly-dir
  atomicity (7437) unchanged — they pin mechanics this change must not move.
- **CI placement**: all new tests are default-feature (`cargo test
  --workspace` leg); no feature-gated targets (BUG-166/LESSON-515).

## Proposed additions to `.adlc/context/architecture.md`

After merge, add a pattern entry: "**A durable document is edited, never
re-uttered** — a writer that persists user-authored configuration applies its
semantic delta to the document as it exists, and validates the bytes it will
write, so the user's comments, ordering, and unknown keys are not collateral
of an unrelated save" (with REQ-574 + lesson references). Deferred to wrapup.
