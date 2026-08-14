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
- **Array granularity (resolves spec OQ-2's granularity half)**: tables recurse
  key-by-key; **arrays (value arrays and arrays-of-tables) are one key**. If
  the canonical arrays differ, the document's array is replaced wholesale with
  the candidate's canonical rendering. Element-wise array diffing needs an
  identity function per array type and mis-targets when on-disk order drifted;
  wholesale replacement is honest, simple, and matches the spec's out-of-scope
  note that a *changed* key may re-render canonically. Comments inside a
  changed array section do not survive; comments everywhere else do.
- **Key removal (resolves spec OQ-1)**: a key present in canonical(current)
  but absent in canonical(candidate) is removed from the document. toml_edit
  removes the key together with its attached decor (the comment block prefixed
  to that key), which is the proposed semantics the spec records: attached
  comments travel with their key; free-standing table decor survives. The
  existing `an_answer_that_omits_a_key_removes_it_and_says_so` behavior
  (runtime.rs:15967) is preserved through this path.

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
