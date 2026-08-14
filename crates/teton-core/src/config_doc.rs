//! Format-preserving edits to the on-disk config **document** (REQ-574).
//!
//! [`crate::config`] says what a config *means*; this module says how a change
//! to it *lands*. Every daemon-side write used to re-serialize the whole
//! in-memory [`Config`], which normalized key order and destroyed comments and
//! unknown keys as collateral — a contradiction the README makes user-facing,
//! because it teaches a hand-written, heavily commented `[web]` block and
//! `/web setup` in the same section. BR-1 makes the write surgical instead: it
//! changes exactly the keys its operation is about, and everything else in the
//! document survives byte-for-byte.
//!
//! # The delta is `diff(current, candidate)` — never `diff(document, …)`
//!
//! The whole preservation property rests on ADR-1's choice of *what* to diff.
//! The operation's footprint is precisely where the caller's pre-mutation
//! `current` and its `candidate` differ; a diff against the parse of the
//! document would classify unrelated hand-edit drift as "changed" and write the
//! stale in-memory value back over it — exactly the clobber BR-5 forbids. With
//! current-vs-candidate, drift at untouched keys is never *in* the delta, so it
//! survives by construction rather than by care.
//!
//! Both sides are diffed through [`Config::to_toml`], so the delta is computed
//! over the same canonical bytes the schema's serde attributes produce — a key
//! that serializes conditionally (`skip_serializing_if`) is absent from both
//! sides or present in both, and never becomes a spurious edit.
//!
//! # Granularity
//!
//! - **Tables recurse**, key by key, so a change inside `[web]` touches one
//!   line of it.
//! - **Value arrays are one key.** A changed `allowed_domains` is replaced
//!   wholesale with the candidate's canonical rendering; its elements have no
//!   identity to diff by.
//! - **Arrays of tables are diffed element-wise**, because BR-1 says *unknown
//!   keys survive for all writers* and the two writers that touch
//!   `[[providers]]` — provider registration and the REQ-557 model migration —
//!   are exactly the writers a wholesale replacement would make lie. See
//!   `plan_array_edit` for the four cases, and `identity_field` for the guard
//!   that keeps a per-index edit on the element it was computed for even when
//!   the document's order has drifted underneath it.
//! - **Removal takes the key's attached decor with it** (spec OQ-1): the
//!   comment block prefixed to a key documents that key, so it travels with it.
//!   Free-standing comments and every other key's decor survive.
//!
//! # Refusals are loggable
//!
//! A refusal names *where* the document is broken and *what* is wrong with it,
//! and never quotes a *value* from the document. (The parser's diagnosis is
//! kept verbatim, and for one class of failure — a duplicate key — that
//! diagnosis names the offending **key**. A key name is schema vocabulary; the
//! secret is always on the right-hand side, and that is the side this refuses
//! to reproduce. Pinned by
//! `a_duplicate_key_refusal_names_the_key_and_still_quotes_no_value`.)
//! `toml_edit`'s own `Display` prints the
//! offending source line under a caret gutter, and that line can be
//! `search_key_ref`, an endpoint with a credential in its query string, or an
//! `Authorization` template — the config file is secret-adjacent, which is why
//! `tetond` keeps it at `0600`. [`DeltaError`] therefore carries a *sanitized*
//! rendering built at construction (`parse_refusal`), on the same reasoning
//! [`crate::config`]'s validator states for its own messages: "the value is not
//! echoed … this message is loggable" (BR-7).
//!
//! # Recorded limitations
//!
//! - **CRLF line endings are normalized once.** `toml_edit` re-emits a parsed
//!   document with `\n` endings, so the first daemon write to a config authored
//!   with `\r\n` rewrites the file's line endings. Content, comments, key order
//!   and unknown keys all survive that write; only the invisible bytes change,
//!   and the second write is a no-op on them. Re-applying `\r\n` afterwards
//!   would mean editing the rendered text outside the parser — a second
//!   renderer to keep in agreement, for line endings — so the behavior is
//!   pinned by a test rather than papered over
//!   (`the_first_write_to_a_crlf_document_normalizes_its_line_endings`).
//!
//! # Purity
//!
//! Both functions are text-in/text-out with no filesystem access (ADR-2). The
//! atomic write — temp file, mode preservation, fsync, rename — stays in
//! `tetond` around them, so this crate's no-I/O rule is untouched and the
//! preservation logic is testable without a directory.

use crate::config::Config;
use toml_edit::{ArrayOfTables, Decor, DocumentMut, InlineTable, Item, RawString, Table, Value};

/// Why a config document could not be edited.
///
/// BR-6: degradation is a loud refusal, never a silent rewrite. Every variant
/// carries the underlying failure so the caller can name it (LESSON-456,
/// BUG-146: a write that fails must say what failed, not "write failed"), and
/// none of them carries the document — see the module's "Refusals are loggable".
#[derive(Debug, thiserror::Error)]
pub enum DeltaError {
    /// The document handed in is not parseable TOML — typically a half-finished
    /// hand edit.
    ///
    /// The caller refuses the write rather than falling back to a full
    /// re-serialization: that fallback would make the write succeed by
    /// destroying the edit in progress, which is the one outcome BR-6 forbids.
    ///
    /// The payload is `parse_refusal`'s sanitized rendering — location and
    /// diagnosis, no source line — and deliberately **not** the
    /// `toml_edit::TomlError`, whose `Display` reproduces the offending line and
    /// whose `Debug` retains the whole document. This message reaches the RPC
    /// surface and the daemon log; the line it points at may hold a credential.
    #[error("the config file could not be parsed for editing, so nothing was written: {0}")]
    Parse(String),

    /// A [`Config`] could not be serialized to its canonical TOML.
    ///
    /// Unreachable for a well-formed config — the same caveat
    /// [`Config::to_toml`] carries — and represented anyway because the
    /// alternative is a `.expect()` in the one code path whose whole purpose is
    /// to not lose the user's file.
    ///
    /// `toml::ser::Error` is carried whole, unlike [`Self::Parse`]: it is a
    /// message about a *shape* the serializer cannot express (an unsupported
    /// type, a non-string key), and neither its `Display` nor its `Debug`
    /// reaches for the value being serialized.
    #[error("the config could not be serialized to TOML, so nothing was written: {0}")]
    Serialize(toml::ser::Error),
}

/// A parse failure rendered so it can be logged: where the document is broken
/// and what is wrong with it, and nothing *from* the document.
///
/// `toml_edit::TomlError`'s own `Display` is
///
/// ```text
/// TOML parse error at line 9, column 52
///   |
/// 9 | search_key_ref = "sk-live-abc123…"
///   |                                    ^
/// expected `.`, `=`
/// ```
///
/// — the offending line, verbatim, in a message that ends up in the daemon log
/// and in an RPC error a client renders. This keeps the first line and the
/// diagnosis and drops the quotation, matching the posture
/// [`crate::config`]'s validator already states for endpoints, auth templates
/// and domains: the value is not echoed, because it can carry a credential and
/// the message is loggable (REQ-563 BR-7).
///
/// `input` is the text the error's span indexes into — the document for a
/// user-supplied parse, the canonical rendering for [`canonical_document`]'s.
fn parse_refusal(error: &toml_edit::TomlError, input: &str) -> DeltaError {
    let diagnosis: Vec<&str> = error
        .message()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let diagnosis = diagnosis.join("; ");
    DeltaError::Parse(match error.span() {
        Some(span) => {
            let (line, column) = position_of(input, span.start);
            format!("TOML parse error at line {line}, column {column}: {diagnosis}")
        }
        // Spanless in principle only; a location-free diagnosis still beats
        // silence, and it still quotes nothing.
        None => format!("TOML parse error: {diagnosis}"),
    })
}

/// The 1-based line and column a byte offset falls at, counting characters
/// rather than bytes so a comment in another script does not skew the column.
///
/// Deliberately its own arithmetic rather than a slice of `input`: the offset
/// arrives from a parser and lands on no char boundary the day it is wrong,
/// and a panic in the code path whose job is to refuse safely would be its own
/// bug. An out-of-range offset is clamped to the end of the input for the same
/// reason.
///
/// Exposed because `tetond` locates the *other* parser's spans — `toml::de`'s,
/// over the bytes a write derived — and needs the same answer with the same
/// safety. One implementation, so a span that lands mid-character cannot panic
/// in one of the two refusal paths and not the other (it would panic there
/// holding the config mutex, which poisons it for every later reader).
#[must_use]
pub fn position_of(input: &str, offset: usize) -> (usize, usize) {
    let bytes = input.as_bytes();
    let offset = offset.min(bytes.len());
    let mut line = 1;
    let mut column = 1;
    for byte in &bytes[..offset] {
        if *byte == b'\n' {
            line += 1;
            column = 1;
        } else if byte & 0xC0 != 0x80 {
            // Not a UTF-8 continuation byte, so it starts a character.
            column += 1;
        }
    }
    (line, column)
}

/// Whether a document holds nothing an edit could preserve.
///
/// A file of blank lines is a missing file that happens to exist — the shape a
/// truncated write leaves behind — and editing it as if it had content only
/// carries that emptiness forward. Treated here exactly like the empty string,
/// so the next write heals it into a complete document.
///
/// Exposed because the *caller* decides the delta base, and the two decisions
/// have to agree: [`apply_config_delta`] can only make the edit base empty; a
/// caller that also wants the delta base to be `Config::default()` for such a
/// file — so every candidate key is written, as it is for a missing one —
/// needs to ask this question itself before choosing what to pass as `current`.
#[must_use]
pub fn document_is_effectively_empty(doc_text: &str) -> bool {
    doc_text.trim().is_empty()
}

/// Apply the `current` → `candidate` delta to `doc_text`, returning the edited
/// document.
///
/// `doc_text` is the document as it exists on disk (the empty string for a file
/// that is not there yet — a missing file is not an error, BR-6). A document
/// holding only whitespace is treated as that same empty base
/// ([`document_is_effectively_empty`]): there is nothing in it to preserve, and
/// editing around its blank lines would carry them forward forever. `current` is
/// the caller's pre-mutation config and `candidate` the config it wants
/// durable; every call site already holds both, because candidates are built by
/// clone-and-mutate.
///
/// Everything the delta does not name is preserved byte-for-byte: comments
/// (line and inline), key order, whitespace style, unknown keys, and unknown
/// tables (BR-1).
///
/// # Errors
/// Returns [`DeltaError::Parse`] when `doc_text` is not parseable TOML, or
/// [`DeltaError::Serialize`] when a config will not serialize.
pub fn apply_config_delta(
    doc_text: &str,
    current: &Config,
    candidate: &Config,
) -> Result<String, DeltaError> {
    // A whitespace-only file heals to a complete candidate on the next write,
    // exactly as a missing one does — as far as this engine can, anyway: the
    // *delta base* is the caller's to choose, and a caller that hands in a
    // non-default `current` for such a file gets only the delta's keys written.
    let base = if document_is_effectively_empty(doc_text) {
        ""
    } else {
        doc_text
    };
    let mut document: DocumentMut = base.parse().map_err(|error| parse_refusal(&error, base))?;

    // ADR-1, and the single most load-bearing line in this module: the two
    // sides of the diff are the caller's configs, NEVER the parse of
    // `document`. Diffing the document against the candidate would mark every
    // hand edit made since the daemon last read the file as "changed" and write
    // the stale in-memory value over it (BR-5's clobber). Here, drift at a key
    // this operation does not touch simply never enters the delta.
    let current_canonical = canonical_document(current)?;
    let candidate_canonical = canonical_document(candidate)?;

    // Where a section this write *adds* should render (see `appended_at`),
    // measured before the edit so it is past everything the user already wrote.
    let appended_at = last_render_position(document.as_table()) + 1;
    apply_table_delta(
        &mut TargetTable::Standard(document.as_table_mut()),
        current_canonical.as_table(),
        candidate_canonical.as_table(),
        appended_at,
    );

    Ok(document.to_string())
}

/// The named top-level table as it appears in `doc_text` — header, contents and
/// decor — or `None` when the document does not name it.
///
/// This is how `/web setup`'s preview shows the `[web]` section (ADR-2 / BR-3):
/// sliced from the document the commit will write, so a user's own comments
/// inside `[web]` appear in the preview they confirm. A second renderer would
/// agree with the writer only until one of them changed.
///
/// The slice is the table *with the decor toml_edit attaches to it*, which
/// includes any comment block written immediately above the header — that block
/// is what a reader would call part of the section. A leading blank line is
/// dropped: it separates the section from what precedes it, and the preview has
/// nothing preceding it. An unparseable document has no sections, so `None`
/// covers that too; callers that need to *report* the parse failure go through
/// [`apply_config_delta`], which does.
#[must_use]
pub fn table_section(doc_text: &str, key: &str) -> Option<String> {
    let document: DocumentMut = doc_text.parse().ok()?;
    let item = present(document.as_table(), key)?;
    let mut section = DocumentMut::new();
    let _ = section.as_table_mut().insert(key, item.clone());
    Some(strip_leading_blank_lines(&section.to_string()))
}

/// A config's canonical TOML, parsed as an editable document.
///
/// The parse of freshly serialized TOML cannot realistically fail; it is still
/// reported rather than unwrapped, for the reason [`DeltaError::Serialize`]
/// gives.
fn canonical_document(config: &Config) -> Result<DocumentMut, DeltaError> {
    let canonical = config.to_toml().map_err(DeltaError::Serialize)?;
    canonical
        .parse()
        .map_err(|error| parse_refusal(&error, &canonical))
}

/// Apply one table's worth of delta to `target`.
///
/// Two passes so the outcome does not depend on map iteration order: keys the
/// canonical `current` names (changes and removals) first, then the keys only
/// `candidate` names (insertions, which append).
fn apply_table_delta(
    target: &mut TargetTable<'_>,
    current: &Table,
    candidate: &Table,
    appended_at: usize,
) {
    for (key, current_item) in current.iter() {
        apply_key_delta(
            target,
            key,
            Some(current_item),
            present(candidate, key),
            appended_at,
        );
    }
    for (key, candidate_item) in candidate.iter() {
        if present(current, key).is_none() {
            apply_key_delta(target, key, None, Some(candidate_item), appended_at);
        }
    }
}

/// The delta for a single key, given its canonical item on each side.
///
/// A table on both sides recurses. A table on exactly one side recurses against
/// an empty table, which is deliberately gentler than setting or removing the
/// whole table: a `[web]` block whose every key still holds its default is
/// *absent* from the canonical serialization (`skip_serializing_if`), and it is
/// precisely that hand-written, all-default, heavily commented block the README
/// teaches. Replacing it wholesale on the first `/web setup` write — or
/// deleting it when a setting reverts to its default — would destroy the
/// comments this REQ exists to keep. Per-key edits inside it change exactly the
/// keys the delta names and parse back identically.
fn apply_key_delta(
    target: &mut TargetTable<'_>,
    key: &str,
    current: Option<&Item>,
    candidate: Option<&Item>,
    appended_at: usize,
) {
    let empty = Table::new();
    match (current, candidate) {
        (Some(current_item), Some(candidate_item)) => {
            match (current_item.as_table(), candidate_item.as_table()) {
                (Some(current_table), Some(candidate_table)) => {
                    apply_sub_table_delta(target, key, current_table, candidate_table, appended_at);
                }
                // An array of tables — `[[providers]]` — is diffed element-wise
                // (see `plan_array_edit`); everything else here is a scalar or a
                // value array, which is one key (ADR-1).
                _ => match (
                    current_item.as_array_of_tables(),
                    candidate_item.as_array_of_tables(),
                ) {
                    (Some(current_array), Some(candidate_array)) => apply_array_of_tables_delta(
                        target,
                        key,
                        current_array,
                        candidate_array,
                        candidate_item,
                        appended_at,
                    ),
                    _ => {
                        if !items_agree(current_item, candidate_item) {
                            target.set(key, candidate_item, appended_at);
                        }
                    }
                },
            }
        }
        (None, Some(candidate_item)) => match candidate_item.as_table() {
            Some(candidate_table) => {
                apply_sub_table_delta(target, key, &empty, candidate_table, appended_at);
            }
            None => target.set(key, candidate_item, appended_at),
        },
        (Some(current_item), None) => match current_item.as_table() {
            Some(current_table) => {
                apply_sub_table_delta(target, key, current_table, &empty, appended_at);
            }
            // OQ-1: `remove` takes the key together with the decor attached to
            // it — the comment block prefixed to that key — which is the
            // proposed semantics the spec records. A comment documenting a key
            // outlives that key only as a lie.
            None => target.remove(key),
        },
        (None, None) => {}
    }
}

/// Recurse into the document's own table at `key`, creating one only if the
/// delta actually has something to put there.
///
/// The document is the truth about where a table is and what else is in it, so
/// a table the document already holds is edited in place — whatever spelling
/// the user chose for it, `[web]`, `web.tier = …`, or `web = { … }`. A table
/// the document does not hold at all is built from the delta alone, not from
/// the candidate's whole table: keys the operation did not change must keep
/// taking their value from the document (BR-5), and in an absent table that
/// value is the schema default.
fn apply_sub_table_delta(
    target: &mut TargetTable<'_>,
    key: &str,
    current: &Table,
    candidate: &Table,
    appended_at: usize,
) {
    if let Some(mut existing) = target.descend(key) {
        apply_table_delta(&mut existing, current, candidate, appended_at);
        return;
    }
    let mut fresh = Table::new();
    apply_table_delta(
        &mut TargetTable::Standard(&mut fresh),
        current,
        candidate,
        appended_at,
    );
    if !fresh.is_empty() {
        target.set(key, &Item::Table(fresh), appended_at);
    }
}

/// What a canonical array-of-tables delta asks of the document's own array.
enum ArrayEdit {
    /// The two canonical arrays say the same thing; the document keeps its own.
    Untouched,
    /// The candidate is the current array plus elements from this index on.
    Append { from: usize },
    /// Same length, differing at these indexes.
    PerIndex(Vec<usize>),
    /// Shrunk, reordered, or reshaped — no index correspondence to trust.
    Wholesale,
}

/// Classify an array-of-tables delta.
///
/// ADR-1 wrote arrays off as one key, on the ground that element-wise diffing
/// needs a per-array identity function and mis-targets when on-disk order
/// drifted. That is right about identity and wrong about the cost: BR-1 says
/// unknown keys and comments survive **for all five writers**, and the two that
/// touch `[[providers]]` are precisely provider registration (an append) and the
/// REQ-557 model migration (one key added to each entry). Under the wholesale
/// rule those two writers re-render the array canonically, which deletes every
/// comment and every unknown key inside `[[providers]]` — BR-1 broken by the
/// only writers that could break it. So the two shapes that *do* have a
/// trustworthy correspondence get one, and everything else keeps the recorded
/// exception:
///
/// - **Append** — the common prefix is untouched and the candidate is longer.
///   Position is not an identity claim here: the elements that already exist are
///   not touched at all, only pushed past.
/// - **Per-index** — same length, some elements differ. This is the migration
///   shape. The correspondence it assumes is *checked against the document*
///   before anything is written — see [`identity_field`] — and falls back to
///   wholesale when the check fails.
/// - **Wholesale** — anything else. The document's array is replaced with the
///   candidate's canonical rendering, comments inside it and all.
///
/// This function sees only the two canonical arrays, so what it returns is a
/// *proposal*. [`apply_array_of_tables_delta`] is where the document gets a
/// vote, and where a per-index proposal is downgraded.
fn plan_array_edit(current: &ArrayOfTables, candidate: &ArrayOfTables) -> ArrayEdit {
    if candidate.len() < current.len() {
        return ArrayEdit::Wholesale;
    }
    let differing: Vec<usize> = (0..current.len())
        .filter(|index| !tables_agree(current.get(*index), candidate.get(*index)))
        .collect();
    if candidate.len() > current.len() {
        return if differing.is_empty() {
            ArrayEdit::Append {
                from: current.len(),
            }
        } else {
            ArrayEdit::Wholesale
        };
    }
    if differing.is_empty() {
        ArrayEdit::Untouched
    } else {
        ArrayEdit::PerIndex(differing)
    }
}

/// The key inside an array element that says *which* entity the element is —
/// the natural identity of each array the schema declares.
///
/// Read off `Config`'s own fields (serialized names, `serde(rename)` included)
/// and the element structs they hold:
///
/// | array | element type | identity key |
/// |-------|--------------|--------------|
/// | `providers` | `ModelProvider` | `id` |
/// | `mcp_server` | `McpServerConfig` | `id` |
/// | `boundaries` | `PrivacyBoundary` | `path_glob` |
/// | `tiers` | `TierBinding` | `tier` |
/// | `categories` | `CategoryOverride` | `name` |
/// | `routing` (`Config::legacy_routing`, `serde(rename = "routing")`) | `LegacyRoutingRule` | `phase` |
///
/// This is not a second opinion about identity. For the four arrays a daemon
/// writer actually mutates, it is the key that writer *already* matched on:
/// `apply_update` does replace-or-insert by `id` for `providers`, by `tier` for
/// `tiers`, by `name` for `categories`, and by `path_glob` for `boundaries`. So
/// the question asked of the document is the same question the mutation asked of
/// memory.
///
/// The other two rows are forward-looking, and named so rather than implied:
/// `mcp_server` is hand-declared and has no per-element daemon writer today, and
/// `routing` is read once and cleared wholesale by the REQ-558 migration. Their
/// keys are the ones the schema treats as the row's identity anyway —
/// `Config::validate` rejects two `mcp_server` entries sharing an `id`, and a
/// phase → provider table is keyed by its phase — so a writer that arrives later
/// finds the guard already right rather than already wrong.
///
/// `None` means "no known identity", which sends a per-index edit to the
/// wholesale fallback. It is the conservative default and it is also nearly
/// unreachable: both sides of the delta are *canonical serializations of
/// `Config`*, so a key this table has never heard of cannot appear in the delta
/// at all. It exists so that adding an array to the schema and forgetting this
/// table degrades to a costly-but-safe rewrite rather than to a
/// position-trusting edit.
fn identity_field(array_key: &str) -> Option<&'static str> {
    match array_key {
        "providers" | "mcp_server" => Some("id"),
        "boundaries" => Some("path_glob"),
        "tiers" => Some("tier"),
        "categories" => Some("name"),
        "routing" => Some("phase"),
        _ => None,
    }
}

/// Whether the document's element and the canonical `current` element the
/// delta's index was computed against are the **same entity**.
///
/// A missing identity key on either side is a mismatch, not a pass: an element
/// the document spells without an `id` is one this engine cannot recognize, and
/// "cannot recognize" must not read as "matches".
fn elements_share_identity(identity: &str, current: &Table, document: &Table) -> bool {
    match (present(current, identity), present(document, identity)) {
        (Some(current_id), Some(document_id)) => items_agree(current_id, document_id),
        _ => false,
    }
}

/// Apply an array-of-tables delta to the document's own array.
///
/// # The per-index branch checks the document's element is the right one
///
/// A per-index edit is a claim about *position*, and BR-5 leaves the daemon
/// blind to the file until restart: a user who reorders `[[boundaries]]` or
/// `[[providers]]` by hand mid-session, without changing how many there are,
/// leaves the daemon's index *i* pointing at a different entity than the
/// document's index *i*. Applied blind, a `SetPrivacyBoundary` for `docs/**`
/// lands on `secrets/**` — a privacy guarantee silently inverted, and one
/// `Config::validate` has no way to catch, because the result is a perfectly
/// valid config that says something the user never asked for. The same shape
/// binds a rotated `auth_ref` to another provider's endpoint.
///
/// So before an element is edited, the document's element at that index must
/// agree with the canonical `current` element on the array's identity key
/// ([`identity_field`]). Any mismatch — including an array whose identity this
/// engine does not know — falls through to the wholesale replacement, which is
/// semantically correct in every case and costs the comments and unknown keys
/// *inside* the array (nothing outside it). That is the honest trade: a lossy
/// write the user can see, instead of a lossless write of the wrong thing.
///
/// Only the indexes this delta actually edits are checked. An element the write
/// does not touch may sit wherever the user moved it — refusing over drift
/// somewhere else in the array would spend the array's comments to protect an
/// element nothing was going to write to.
///
/// # The other branches
///
/// *Untouched* reads the document not at all. *Append* reads no existing element
/// either — it pushes past them — so it needs no identity check and gets none:
/// a hand-added element it cannot see is caught downstream instead, by the
/// caller's validation of the edited bytes (a hand-added `[[providers]]` whose
/// id is the one being registered makes the write refuse with the validator's
/// duplicate-id sentence, and the file is left alone). What every document-
/// reading branch does check is that the document spells the key as `[[key]]`
/// sections at all; the per-index branch additionally checks it holds the number
/// of elements the delta's indexes were computed against.
fn apply_array_of_tables_delta(
    target: &mut TargetTable<'_>,
    key: &str,
    current: &ArrayOfTables,
    candidate: &ArrayOfTables,
    candidate_item: &Item,
    appended_at: usize,
) {
    match plan_array_edit(current, candidate) {
        ArrayEdit::Untouched => return,
        ArrayEdit::Append { from } => {
            if let Some(document_array) = target.array_of_tables(key) {
                // The existing elements are not read, not re-rendered and not
                // moved: their comments, unknown keys and formatting are
                // untouched by construction. The new ones render as a block
                // continuing the one already there — same position, and
                // toml_edit's sort is stable, so they follow it.
                //
                // "The one already there" means the whole of it, sub-tables
                // included: an element carries `[providers.capabilities]`,
                // which parses at a *later* position than its own header. Take
                // only the headers' positions and the appended element lands
                // between an existing element and that element's own sub-table
                // — which, since a section's parent is whatever `[[…]]` header
                // precedes it, silently re-parents the sub-table onto the new
                // entry and leaves the document holding two
                // `[providers.capabilities]` under one element. It stops
                // parsing at that point, so the write is refused and the
                // second registration onto a config fails outright.
                let carried = document_array
                    .iter()
                    .filter_map(|element| {
                        element
                            .position()
                            .map(|position| position.max(last_render_position(element)))
                    })
                    .max()
                    .unwrap_or(appended_at);
                for index in from..candidate.len() {
                    let Some(element) = candidate.get(index) else {
                        continue;
                    };
                    let mut fresh = element.clone();
                    place_at(&mut fresh, carried);
                    document_array.push(fresh);
                }
                return;
            }
        }
        ArrayEdit::PerIndex(indexes) => {
            // No identity to check by is not permission to trust position; it is
            // the reason not to (`identity_field`).
            if let (Some(identity), Some(document_array)) =
                (identity_field(key), target.array_of_tables(key))
            {
                let addresses_the_same_entities = document_array.len() == current.len()
                    && indexes.iter().all(|index| {
                        matches!(
                            (current.get(*index), document_array.get(*index)),
                            (Some(before), Some(element))
                                if elements_share_identity(identity, before, element)
                        )
                    });
                if addresses_the_same_entities {
                    for index in indexes {
                        let (Some(before), Some(after), Some(element)) = (
                            current.get(index),
                            candidate.get(index),
                            document_array.get_mut(index),
                        ) else {
                            continue;
                        };
                        // A section this edit *adds* inside an element belongs
                        // to that element, and only to it because of where it
                        // renders: an array element's sub-tables are parented
                        // by the `[[…]]` header above them, not by their own
                        // header path the way `[web.nested]` is. Placed at the
                        // document's append position, a new
                        // `[providers.capabilities]` would render past
                        // everything else in the file, land under whichever
                        // element is *last*, and collide with that element's
                        // own sub-table. So inside an element, "append" means
                        // the element's position.
                        let element_at = element.position().unwrap_or(appended_at);
                        apply_table_delta(
                            &mut TargetTable::Standard(element),
                            before,
                            after,
                            element_at,
                        );
                    }
                    return;
                }
            }
        }
        ArrayEdit::Wholesale => {}
    }
    target.set(key, candidate_item, appended_at);
}

/// The table an edit lands in, as the *document* spells it.
///
/// Two shapes, because the document is the user's: a table written as `[web]`
/// is a standard table, one written as `web = { … }` is an inline table.
/// Recursing into both is what extends BR-1 to the second spelling — replacing
/// an inline table wholesale would drop the unknown keys inside it, which is
/// the destruction this module exists to end.
enum TargetTable<'doc> {
    /// A `[header]` table, or the document root.
    Standard(&'doc mut Table),
    /// A `{ key = value }` table.
    Inline(&'doc mut InlineTable),
}

impl TargetTable<'_> {
    /// Set `key` to `item`'s value.
    ///
    /// An existing key is assigned *through* its entry rather than re-inserted,
    /// so the key keeps its own decor — the comment block above it and the
    /// spacing around it — and only the value moves. The document's value decor
    /// is carried across too, which keeps a trailing inline comment attached to
    /// the key it annotates (BR-1 names inline comments explicitly), and its
    /// table position, which keeps a replaced section where the user put it.
    ///
    /// A key the document does not have is appended, and any section it brings
    /// with it renders past everything already in the file (`appended_at`)
    /// rather than at whatever position it held in the canonical document — an
    /// addition belongs at the end of the user's file, not wedged between two
    /// sections they wrote.
    fn set(&mut self, key: &str, item: &Item, appended_at: usize) {
        match self {
            Self::Standard(table) => {
                let mut replacement = item.clone();
                let Some(existing) = table.get(key) else {
                    place_addition(&mut replacement, appended_at);
                    let _ = table.insert(key, replacement);
                    return;
                };
                // The document spells the key as a *value* — `providers = [ { …
                // } ]` — and the delta needs sections. Everything the key's own
                // decor was written for now renders *inside the brackets*: the
                // space before the `=` comes out as `[[providers ]]`, and a
                // comment block above the key comes out as an unparseable
                // `[[# …⏎providers]]`. And a value carries no render position,
                // so the canonical positions inside the replacement would
                // strand `[providers.capabilities]` on the far side of the
                // user's `[web]` block. So the key reverts to the header
                // default, the comments it carried move onto the block they
                // document (OQ-1: a comment travels with its key), and the block
                // is placed as an addition so it renders contiguously.
                let reshapes_a_value_into_sections = matches!(existing, Item::Value(_))
                    && matches!(replacement, Item::Table(_) | Item::ArrayOfTables(_));
                carry_document_placement(existing, &mut replacement);
                if reshapes_a_value_into_sections {
                    let carried_comment = carried_key_comment(table, key, existing);
                    place_addition(&mut replacement, appended_at);
                    if let Some(comment) = carried_comment {
                        prefix_first_section(&mut replacement, &format!("\n{comment}"));
                    }
                }
                if let Some(existing) = table.get_mut(key) {
                    *existing = replacement;
                }
                if reshapes_a_value_into_sections {
                    if let Some(mut header) = table.key_mut(key) {
                        *header.leaf_decor_mut() = Decor::default();
                    }
                }
            }
            Self::Inline(table) => {
                // A table nested in an inline table can only be spelled inline;
                // `into_value` performs that conversion and fails only for
                // `Item::None`, which never reaches here.
                let mut value = match item.clone().into_value() {
                    Ok(value) => value,
                    Err(unconvertible) => {
                        // Unreachable, and a silent drop if it ever stops being:
                        // the key would keep its old value while the caller
                        // believes the write landed. Loud in debug, and still
                        // non-panicking in release — this code path exists to
                        // not lose the user's file. The item is named by *type*,
                        // never by content: a panic message is as loggable as an
                        // error message.
                        debug_assert!(
                            false,
                            "a `{}` will not convert to an inline value, so `{key}` was not set",
                            unconvertible.type_name(),
                        );
                        return;
                    }
                };
                if let Some(existing) = table.get_mut(key) {
                    *value.decor_mut() = existing.decor().clone();
                    *existing = value;
                } else {
                    let _ = table.insert(key, value);
                }
            }
        }
    }

    /// Remove `key`, together with the decor toml_edit attaches to it (OQ-1).
    fn remove(&mut self, key: &str) {
        match self {
            Self::Standard(table) => {
                let _ = table.remove(key);
            }
            Self::Inline(table) => {
                let _ = table.remove(key);
            }
        }
    }

    /// The nested table at `key`, in whichever spelling the document uses, or
    /// `None` when the document holds no table there.
    fn descend(&mut self, key: &str) -> Option<TargetTable<'_>> {
        match self {
            Self::Standard(table) => match table.get_mut(key)? {
                Item::Table(nested) => Some(TargetTable::Standard(nested)),
                Item::Value(Value::InlineTable(nested)) => Some(TargetTable::Inline(nested)),
                _ => None,
            },
            Self::Inline(table) => match table.get_mut(key)? {
                Value::InlineTable(nested) => Some(TargetTable::Inline(nested)),
                _ => None,
            },
        }
    }

    /// The document's own array-of-tables at `key`, or `None` when the document
    /// spells that key some other way — an inline `key = [ { … } ]`, a scalar,
    /// or nothing at all.
    ///
    /// `None` is what sends [`apply_array_of_tables_delta`] to its wholesale
    /// branch: element-wise editing needs elements the document agrees are
    /// elements.
    fn array_of_tables(&mut self, key: &str) -> Option<&mut ArrayOfTables> {
        match self {
            Self::Standard(table) => match table.get_mut(key)? {
                Item::ArrayOfTables(array) => Some(array),
                _ => None,
            },
            // An inline table cannot hold `[[…]]` sections at all.
            Self::Inline(_) => None,
        }
    }
}

/// Carry the document's own formatting decisions onto a replacement item.
///
/// Two of them matter. A value's decor holds the spacing after `=` and any
/// trailing inline comment, and that comment documents the key rather than the
/// old value, so it stays. An array-of-tables' `position` is the sort key
/// toml_edit renders sections by; taking it from the canonical document instead
/// would move a section this write changed away from where the user put it,
/// which BR-1 does not permit.
///
/// A standard table is not among the shapes that arrive here: a table the
/// document already holds is descended into and edited key by key, never
/// replaced ([`apply_sub_table_delta`]). Any other pairing is a document whose
/// shape contradicts the schema (`[effort]` where a scalar belongs), and the
/// candidate's own rendering — validated before it lands, BR-4 — is what
/// resolves it.
fn carry_document_placement(existing: &Item, replacement: &mut Item) {
    match (existing, replacement) {
        (Item::Value(old), Item::Value(new)) => {
            *new.decor_mut() = old.decor().clone();
        }
        (Item::ArrayOfTables(old), Item::ArrayOfTables(new)) => {
            // Element-wise by index, and any element beyond the old array's
            // length inherits the last position seen — toml_edit's sort is
            // stable, so a grown array renders as a block where the old one
            // stood rather than jumping to the top of the file.
            let mut carried = None;
            for (index, table) in new.iter_mut().enumerate() {
                if let Some(position) = old.get(index).and_then(Table::position) {
                    carried = Some(position);
                }
                if let Some(position) = carried {
                    place_at(table, position);
                }
            }
        }
        _ => {}
    }
}

/// Render an added item after everything the document already holds.
///
/// Only tables carry a position; a scalar or a value array renders at its key,
/// so there is nothing to place.
fn place_addition(addition: &mut Item, appended_at: usize) {
    match addition {
        Item::Table(table) => place_at(table, appended_at),
        Item::ArrayOfTables(array) => {
            for table in array.iter_mut() {
                place_at(table, appended_at);
            }
        }
        _ => {}
    }
}

/// Every comment a key carries, gathered because the key's *spelling* is about
/// to change from a value to `[[sections]]`.
///
/// Two places hold one, and both document the key rather than the old value:
/// the block written above it (the key's own leaf decor) and the note written
/// beside it (`providers = [ … ] # the one I pay for`, which lives in the
/// value's suffix decor). The key survives the reshape — only its spelling
/// changes — so both travel with it (OQ-1). A header's own decor is the only
/// place a comment can live above a `[[…]]` block, so the note that sat beside
/// the old line becomes a line of its own beneath the block that sat above it,
/// in the order the two were written.
///
/// `None` when the key carried no comment at all, so nothing is prefixed.
fn carried_key_comment(table: &Table, key: &str, existing: &Item) -> Option<String> {
    let mut above = table
        .key(key)
        .and_then(|header| header.leaf_decor().prefix())
        .and_then(RawString::as_str)
        .filter(|prefix| prefix.contains('#'))
        .map(strip_leading_blank_lines)
        .unwrap_or_default();
    if !above.is_empty() && !above.ends_with('\n') {
        above.push('\n');
    }
    // Only the first line of the suffix: a trailing comment ends at the end of
    // its line, and anything past that belongs to whatever follows the key.
    let beside = existing
        .as_value()
        .and_then(|value| value.decor().suffix())
        .and_then(RawString::as_str)
        .map(|suffix| suffix.lines().next().unwrap_or_default().trim())
        .filter(|note| note.starts_with('#'))
        .map(|note| format!("{note}\n"))
        .unwrap_or_default();
    let carried = format!("{above}{beside}");
    (!carried.is_empty()).then_some(carried)
}

/// Render `comment` in front of the first section an item produces.
///
/// Only used when a key changes spelling from a value to a section: the comment
/// block that sat above `providers = [ … ]` belongs above the `[[providers]]`
/// header it becomes, and a header's own decor is the only place a comment can
/// live there.
fn prefix_first_section(item: &mut Item, comment: &str) {
    let first = match item {
        Item::Table(table) => Some(table),
        Item::ArrayOfTables(array) => array.iter_mut().next(),
        _ => None,
    };
    if let Some(table) = first {
        table.decor_mut().set_prefix(comment);
    }
}

/// The last render position the document uses, so an addition can be placed
/// after it.
///
/// toml_edit numbers tables as it parses them and renders them in that order
/// (ties broken stably, by traversal order), so "one past the largest" is the
/// end of the file.
fn last_render_position(table: &Table) -> usize {
    let mut last = 0;
    for (_, item) in table.iter() {
        match item {
            Item::Table(nested) => {
                last = last.max(nested.position().unwrap_or(0));
                last = last.max(last_render_position(nested));
            }
            Item::ArrayOfTables(array) => {
                for nested in array.iter() {
                    last = last.max(nested.position().unwrap_or(0));
                    last = last.max(last_render_position(nested));
                }
            }
            _ => {}
        }
    }
    last
}

/// Give a table and everything nested under it one shared render position.
///
/// A sub-table carries its own position, and a canonical one is a position in
/// the *canonical* document — leave it and `[providers.capabilities]` renders
/// wherever that document happened to put it, which is how a replaced array
/// ends up with a stray section on the far side of the user's `[web]` block.
/// One shared position plus toml_edit's stable sort renders the whole section
/// as a contiguous block, in traversal order, where the document already had it.
fn place_at(table: &mut Table, position: usize) {
    table.set_position(position);
    for (_, item) in table.iter_mut() {
        match item {
            Item::Table(nested) => place_at(nested, position),
            Item::ArrayOfTables(nested) => {
                for nested_table in nested.iter_mut() {
                    place_at(nested_table, position);
                }
            }
            _ => {}
        }
    }
}

/// Whether two canonical items say the same thing.
///
/// Compared as parsed *values*, not as text: the two canonical documents render
/// the same content identically, but blank lines and table positions are
/// formatting, and formatting must not make an untouched key look changed —
/// a false "changed" here is a needless rewrite of a key the user may have
/// hand-formatted. An item that will not round-trip through the value parser is
/// reported as changed, which is the safe direction: the write is the
/// candidate's own value, and the bytes are validated before they land (BR-4).
fn items_agree(left: &Item, right: &Item) -> bool {
    match (semantic_value(left), semantic_value(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

/// Whether two canonical array elements say the same thing.
///
/// A missing element on either side is "different", which is the safe direction
/// for the same reason [`items_agree`] gives: the caller's fallback is to write
/// the candidate, and the candidate is validated before it lands (BR-4).
fn tables_agree(left: Option<&Table>, right: Option<&Table>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => {
            items_agree(&Item::Table(left.clone()), &Item::Table(right.clone()))
        }
        _ => false,
    }
}

/// One item, rendered under a fixed key and re-parsed as a plain TOML value.
fn semantic_value(item: &Item) -> Option<toml::Value> {
    const COMPARISON_KEY: &str = "value";
    let mut document = DocumentMut::new();
    let _ = document.as_table_mut().insert(COMPARISON_KEY, item.clone());
    toml::from_str::<toml::Value>(&document.to_string()).ok()
}

/// The item at `key`, treating toml_edit's `Item::None` placeholder as absence.
fn present<'table>(table: &'table Table, key: &str) -> Option<&'table Item> {
    table.get(key).filter(|item| !item.is_none())
}

/// Drop whitespace-only leading lines from a rendered section.
fn strip_leading_blank_lines(rendered: &str) -> String {
    let mut rest = rendered;
    while let Some(line_end) = rest.find('\n') {
        if rest[..line_end].trim().is_empty() {
            rest = &rest[line_end + 1..];
        } else {
            break;
        }
    }
    rest.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{WebConfig, WebTier};

    /// A hand-authored config whose `[web]` table is the README's example
    /// **verbatim** (README.md, the "Or write the table by hand" block) —
    /// comments, `search_auth = "X-Subscription-Token: {key}"`, key order and
    /// all. LESSON-512: a spec's named example is a test case, not decoration,
    /// and this one is the exact document a user who followed the docs in order
    /// ends up with. It carries two more things the daemon has never heard of:
    /// an unknown key inside a known table, and an unknown top-level table.
    const HAND_WRITTEN_CONFIG: &str = r#"# My machine. Hand-written, and staying that way.
effort = "high"

[[providers]]
# The one I actually pay for.
id = "anthropic"
kind = "anthropic"
endpoint = "https://api.anthropic.com"
auth_ref = "keychain:anthropic"

[web]
# "off" (default) | "fetch_user_url" | "fetch_any_url" | "search"
tier = "search"
search_endpoint = "https://api.search.brave.com/res/v1/web/search"
# A reference, never a raw key — the value lives in the OS keychain.
search_key_ref = "keychain://teton/web-search"
# The header the key rides, `{key}` marking the secret. Absent means
# `Authorization: Bearer {key}`, and it is refused with no key reference
# beside it — a header shape with no secret to place would do nothing.
search_auth = "X-Subscription-Token: {key}"
# Optional; constrains model-chosen destinations only. Absent = unrestricted,
# present but empty = nothing allowed. A URL you pasted yourself is exempt.
allowed_domains = ["docs.rs", "crates.io"]
# Cache freshness window in seconds; 0 means no caching. Defaults to 900.
cache_ttl_secs = 900
# Nothing in this build reads this key.
experimental_reranker = "colbert"

# Nothing in this build reads this table either.
[experimental]
knob = 3
"#;

    /// The document as the daemon sees it: parsed, validated, ready to mutate.
    fn hand_written() -> Config {
        Config::load(HAND_WRITTEN_CONFIG).expect("the README's own example must load and validate")
    }

    /// The 0-based line numbers at which two documents differ, plus a same-line
    /// -count check — the assertion shape BR-1 actually asks for ("only these
    /// lines changed"), rather than a spot check of a few survivors.
    fn changed_lines(before: &str, after: &str) -> Vec<usize> {
        let before: Vec<&str> = before.lines().collect();
        let after: Vec<&str> = after.lines().collect();
        assert_eq!(
            before.len(),
            after.len(),
            "the write added or removed lines:\nbefore:\n{}\nafter:\n{}",
            before.join("\n"),
            after.join("\n"),
        );
        (0..before.len())
            .filter(|index| before[*index] != after[*index])
            .collect()
    }

    #[test]
    fn a_tier_change_moves_one_line_of_the_readme_config_and_nothing_else() {
        let current = hand_written();
        let mut candidate = current.clone();
        candidate.web.tier = WebTier::FetchAnyUrl;

        let edited = apply_config_delta(HAND_WRITTEN_CONFIG, &current, &candidate)
            .expect("the README's example must be editable");

        let changed = changed_lines(HAND_WRITTEN_CONFIG, &edited);
        let tier_line = HAND_WRITTEN_CONFIG
            .lines()
            .position(|line| line == r#"tier = "search""#)
            .expect("fixture holds the tier line");
        assert_eq!(
            changed,
            vec![tier_line],
            "only the operation's key may change:\n{edited}",
        );
        assert!(edited.contains(r#"tier = "fetch_any_url""#));

        // The keys `Config` serializes unconditionally (`effort`,
        // `judgment_default`) are equal on both sides of the delta, so an
        // absent one stays absent rather than being "helpfully" written in.
        assert!(
            !edited.contains("judgment_default"),
            "a key the delta never names must not appear:\n{edited}",
        );

        let reloaded = Config::load(&edited).expect("the edited document must load");
        assert_eq!(reloaded, candidate);
    }

    #[test]
    fn removing_a_key_takes_its_own_comment_and_leaves_its_neighbours_alone() {
        let current = hand_written();
        let mut candidate = current.clone();
        candidate.web.search_auth = None;

        let edited =
            apply_config_delta(HAND_WRITTEN_CONFIG, &current, &candidate).expect("edit applies");

        // OQ-1: the key and the comment block documenting it go together.
        assert!(!edited.contains("search_auth"), "{edited}");
        assert!(!edited.contains("X-Subscription-Token"), "{edited}");
        assert!(!edited.contains("# The header the key rides"), "{edited}");
        // Every other comment — including the free-standing block above the
        // unknown table — survives, as does every other key.
        assert!(
            edited.contains("# A reference, never a raw key"),
            "{edited}"
        );
        assert!(
            edited.contains("# Optional; constrains model-chosen"),
            "{edited}"
        );
        assert!(edited.contains("# Nothing in this build reads this table either."));
        assert!(edited.contains(r#"search_key_ref = "keychain://teton/web-search""#));
        assert!(edited.contains(r#"experimental_reranker = "colbert""#));

        let reloaded = Config::load(&edited).expect("the edited document must load");
        assert_eq!(reloaded, candidate);
    }

    #[test]
    fn a_key_the_document_never_wrote_is_inserted_into_the_table_it_belongs_to() {
        let current = hand_written();
        assert!(
            !HAND_WRITTEN_CONFIG.contains("permission_allow"),
            "the README block omits this key — that is what makes this an insertion",
        );
        let mut candidate = current.clone();
        candidate.web.permission_allow = vec![WebTier::Search];

        let edited =
            apply_config_delta(HAND_WRITTEN_CONFIG, &current, &candidate).expect("edit applies");

        assert!(
            edited.contains(r#"permission_allow = ["search"]"#),
            "the new key must land in [web]:\n{edited}",
        );
        assert!(
            edited.contains("# Cache freshness window in seconds"),
            "{edited}"
        );
        let reloaded = Config::load(&edited).expect("the edited document must load");
        assert_eq!(reloaded, candidate);
    }

    #[test]
    fn a_table_the_document_never_named_is_added_whole() {
        let current = hand_written();
        let mut candidate = current.clone();
        candidate.permissions.default_level = teton_protocol::permissions::PermissionLevel::Plan;

        let edited =
            apply_config_delta(HAND_WRITTEN_CONFIG, &current, &candidate).expect("edit applies");

        assert!(edited.contains("[permissions]"), "{edited}");
        assert!(
            edited.starts_with(HAND_WRITTEN_CONFIG.trim_end()),
            "an added table appends; the document up to it is untouched:\n{edited}",
        );
        let reloaded = Config::load(&edited).expect("the edited document must load");
        assert_eq!(reloaded, candidate);
    }

    #[test]
    fn a_changed_array_is_replaced_wholesale_and_the_comments_around_it_survive() {
        let current = hand_written();
        let mut candidate = current.clone();
        candidate.web.allowed_domains = Some(vec!["example.com".to_owned()]);

        let edited =
            apply_config_delta(HAND_WRITTEN_CONFIG, &current, &candidate).expect("edit applies");

        let changed = changed_lines(HAND_WRITTEN_CONFIG, &edited);
        assert_eq!(changed.len(), 1, "an array is one key:\n{edited}");
        assert!(
            edited.contains(r#"allowed_domains = ["example.com"]"#),
            "{edited}"
        );
        assert!(
            edited.contains("# Optional; constrains model-chosen"),
            "the comment above a replaced array is decor of its key, not of the array:\n{edited}",
        );
        let reloaded = Config::load(&edited).expect("the edited document must load");
        assert_eq!(reloaded, candidate);
    }

    #[test]
    fn an_element_wise_change_to_an_array_of_tables_edits_the_element_in_place() {
        // The REQ-557 model migration in miniature: one key added to each
        // `[[providers]]` entry. Under the old wholesale rule this re-rendered
        // the array canonically and deleted the comment inside it — BR-1 broken
        // by one of the five writers BR-1 names.
        let current = hand_written();
        let mut candidate = current.clone();
        candidate.providers[0].model = Some("claude-sonnet-4".to_owned());

        let edited =
            apply_config_delta(HAND_WRITTEN_CONFIG, &current, &candidate).expect("edit applies");

        let added = r#"model = "claude-sonnet-4""#;
        assert!(edited.contains(added), "{edited}");
        assert!(
            edited.contains("# The one I actually pay for."),
            "a comment inside a *touched* element survives, because only the \
             delta's own key is written:\n{edited}",
        );
        // The whole assertion BR-1 actually asks for: one line added, and every
        // other line of the document byte-identical and in the same order.
        let without_the_addition: Vec<&str> =
            edited.lines().filter(|line| *line != added).collect();
        assert_eq!(
            without_the_addition,
            HAND_WRITTEN_CONFIG.lines().collect::<Vec<&str>>(),
            "only the operation's key may change:\n{edited}",
        );

        let reloaded = Config::load(&edited).expect("the edited document must load");
        assert_eq!(reloaded, candidate);
    }

    /// A config whose `[[providers]]` entry carries exactly what a wholesale
    /// replacement destroys: a comment inside the entry, and a key this build
    /// has never heard of. The engine-level witness for BR-1's "unknown keys
    /// survive" *inside an array*; the per-writer witness is `tetond`'s.
    const REGISTERED_PROVIDER_CONFIG: &str = r#"effort = "high"

[[providers]]
# The one I actually pay for.
id = "anthropic"
kind = "anthropic"
endpoint = "https://api.anthropic.com"
auth_ref = "keychain:anthropic"
model = "claude-opus-5"
# Nothing in this build reads this key.
nickname = "the good one"

[web]
# The comment that must not move.
tier = "off"
"#;

    fn registered_provider() -> Config {
        Config::load(REGISTERED_PROVIDER_CONFIG).expect("a registered provider loads")
    }

    fn second_provider() -> crate::entities::ModelProvider {
        crate::entities::ModelProvider {
            id: "cheap".to_owned(),
            kind: crate::entities::ProviderKind::OpenaiCompatible,
            endpoint: Some("https://api.deepseek.com/v1".to_owned()),
            model: Some("deepseek-chat".to_owned()),
            auth_ref: Some("keychain:deepseek".to_owned()),
            capabilities: crate::entities::ProviderCapabilities::default(),
        }
    }

    #[test]
    fn registering_a_provider_appends_and_reads_nothing_of_the_entries_already_there() {
        let current = registered_provider();
        let mut candidate = current.clone();
        candidate.providers.push(second_provider());

        let edited = apply_config_delta(REGISTERED_PROVIDER_CONFIG, &current, &candidate)
            .expect("edit applies");

        // The registration is an append: every line the user wrote is still
        // there, in order, including the comment and the unknown key *inside*
        // the array — the two things the wholesale rule deleted.
        let mut before = REGISTERED_PROVIDER_CONFIG.lines();
        let mut expected = before.next();
        for line in edited.lines() {
            if Some(line) == expected {
                expected = before.next();
            }
        }
        assert_eq!(
            expected, None,
            "the document the user wrote must survive an append intact:\n{edited}",
        );
        assert!(edited.contains("# The one I actually pay for."), "{edited}");
        assert!(edited.contains(r#"nickname = "the good one""#), "{edited}");

        // And the new entry renders as a block continuing the array, rather
        // than being scattered past the user's [web] section.
        let first_at = edited.find(r#"id = "anthropic""#).expect("first provider");
        let second_at = edited.find(r#"id = "cheap""#).expect("second provider");
        let web_at = edited.find("[web]").expect("web section");
        assert!(
            first_at < second_at && second_at < web_at,
            "an appended element continues the array where it stands:\n{edited}",
        );

        let reloaded = Config::load(&edited).expect("the edited document must load");
        assert_eq!(reloaded, candidate);
    }

    #[test]
    fn a_second_append_renders_past_the_first_ones_sub_table() {
        // Every appended element brings a nested `[providers.capabilities]`
        // section with it. The second append therefore has to render past the
        // *whole* of the array already in the document, sub-tables included —
        // not merely past the last `[[providers]]` header. An element wedged
        // between an existing entry and that entry's own sub-table re-parents
        // the sub-table onto the new entry, and the document stops parsing at
        // all: `duplicate key capabilities in table providers`.
        let mut third_provider = second_provider();
        third_provider.id = "local".to_owned();

        let current = registered_provider();
        let mut candidate = current.clone();
        candidate.providers.push(second_provider());
        let once = apply_config_delta(REGISTERED_PROVIDER_CONFIG, &current, &candidate)
            .expect("the first append applies");
        assert_eq!(
            once.matches("[providers.capabilities]").count(),
            1,
            "the appended element brings its own sub-table:\n{once}",
        );

        let current = candidate;
        let mut candidate = current.clone();
        candidate.providers.push(third_provider);
        let twice = apply_config_delta(&once, &current, &candidate).expect("edit applies");

        let reloaded = Config::load(&twice).expect("the twice-appended document must load");
        assert_eq!(reloaded, candidate);
        assert_eq!(
            twice.matches("[providers.capabilities]").count(),
            2,
            "each appended element keeps its own sub-table:\n{twice}",
        );
        // And the user's own entry is still untouched under both appends.
        assert!(twice.contains("# The one I actually pay for."), "{twice}");
        assert!(twice.contains(r#"nickname = "the good one""#), "{twice}");
    }

    #[test]
    fn a_section_added_inside_an_array_element_renders_beside_that_element() {
        // The per-index counterpart of the append defect above, and the same
        // cause: a sub-table added to element 0 of a two-element array is
        // parented by the `[[providers]]` header that precedes it. Placed at
        // the *document's* append position it renders past `[web]`, lands
        // under the last element, and collides with that element's own
        // `[providers.capabilities]` — an unparseable document.
        let mut current = registered_provider();
        current.providers.push(second_provider());
        let seed = apply_config_delta(REGISTERED_PROVIDER_CONFIG, &registered_provider(), &current)
            .expect("the two-provider document is built by an append");
        let mut candidate = current.clone();
        candidate.providers[0].capabilities.max_context = 4242;

        let edited = apply_config_delta(&seed, &current, &candidate).expect("edit applies");

        let reloaded = Config::load(&edited).expect("the edited document must load");
        assert_eq!(reloaded, candidate);
        assert_eq!(
            reloaded.providers[0].capabilities.max_context, 4242,
            "the added section belongs to the element the delta named:\n{edited}",
        );
        let first_at = edited.find(r#"id = "anthropic""#).expect("first provider");
        let second_at = edited.find(r#"id = "cheap""#).expect("second provider");
        let added_at = edited
            .find("[providers.capabilities]")
            .expect("added section");
        assert!(
            first_at < added_at && added_at < second_at,
            "an addition inside an element renders inside that element:\n{edited}",
        );
        assert!(edited.contains("# The one I actually pay for."), "{edited}");
    }

    #[test]
    fn a_reshaped_array_of_tables_is_replaced_wholesale_where_it_stood() {
        // ADR-1's exception, kept and pinned: an array that shrank has no index
        // correspondence to preserve, so it is re-rendered canonically and the
        // comments inside it do not survive. Everything outside it does.
        let mut current = registered_provider();
        current.providers.push(second_provider());
        let seed = apply_config_delta(REGISTERED_PROVIDER_CONFIG, &registered_provider(), &current)
            .expect("the two-provider document is built by an append");
        let mut candidate = current.clone();
        candidate.providers.remove(0);

        let edited = apply_config_delta(&seed, &current, &candidate).expect("edit applies");

        assert!(!edited.contains(r#"id = "anthropic""#), "{edited}");
        assert!(edited.contains(r#"id = "cheap""#), "{edited}");
        assert!(
            !edited.contains("# The one I actually pay for."),
            "a shrunk array is re-rendered canonically — the recorded cost:\n{edited}",
        );
        assert!(
            !edited.contains(r#"nickname = "the good one""#),
            "and the unknown key inside it goes with the element:\n{edited}",
        );
        // The section keeps its place in the file, ahead of [web], and it
        // renders as one block — the nested `[providers.capabilities]` table
        // comes with it rather than stranding itself on the far side of the
        // user's [web] block.
        let providers_at = edited.find("[[providers]]").expect("providers section");
        let capabilities_at = edited
            .find("[providers.capabilities]")
            .expect("capabilities section");
        let web_at = edited.find("[web]").expect("web section");
        assert!(
            providers_at < capabilities_at && capabilities_at < web_at,
            "a replaced section stays put, whole:\n{edited}",
        );
        assert!(
            edited.contains("# The comment that must not move."),
            "everything outside the array survives:\n{edited}",
        );

        let reloaded = Config::load(&edited).expect("the edited document must load");
        assert_eq!(reloaded, candidate);
    }

    /// Two privacy boundaries, each with a comment of its own, in the order the
    /// daemon read them. `teton privacy set <glob> <mode>` replaces the matching
    /// row in place, so this is the array whose per-index edits are a *privacy*
    /// claim rather than a formatting one.
    const BOUNDARIES_CONFIG: &str = r#"effort = "high"

[[boundaries]]
# The one that must never leave this machine.
path_glob = "secrets/**"
mode = "local-only"

[[boundaries]]
# Notes. Fine to send once they have been through the redactor.
path_glob = "docs/**"
mode = "local-only"
"#;

    /// The same two boundaries, swapped by hand while the daemon runs — the
    /// drift BR-5 leaves the daemon blind to.
    const BOUNDARIES_REORDERED: &str = r#"effort = "high"

[[boundaries]]
# Notes. Fine to send once they have been through the redactor.
path_glob = "docs/**"
mode = "local-only"

[[boundaries]]
# The one that must never leave this machine.
path_glob = "secrets/**"
mode = "local-only"
"#;

    /// The mode the written document gives a glob, read back through the
    /// production loader — the only reading that matters, because it is the one
    /// the daemon boots on.
    fn boundary_mode(document: &str, glob: &str) -> Option<crate::entities::BoundaryMode> {
        Config::load(document)
            .expect("the edited document must load")
            .boundaries
            .iter()
            .find(|boundary| boundary.path_glob == glob)
            .map(|boundary| boundary.mode)
    }

    #[test]
    fn a_per_index_edit_lands_on_the_element_it_names_even_after_a_hand_reorder() {
        use crate::entities::BoundaryMode;

        // The security case, at the engine. The daemon holds [secrets, docs];
        // `SetPrivacyBoundary { path_glob: "docs/**", … }` replaces the row it
        // matches, so the delta is a same-length one-element edit at **index
        // 1**. Let the user swap the two blocks in the file mid-session and
        // index 1 of the document is `secrets/**`: applied by position, this
        // write relaxes the secrets boundary to `redact-then-remote` and
        // validates cleanly — a privacy guarantee inverted with no error
        // anywhere to notice it by.
        let current = Config::load(BOUNDARIES_CONFIG).expect("two boundaries load");
        assert_eq!(
            current.boundaries[1].path_glob, "docs/**",
            "non-vacuity: the edit below is at index 1",
        );
        assert_eq!(
            Config::load(BOUNDARIES_REORDERED)
                .expect("the reordered document loads")
                .boundaries[1]
                .path_glob,
            "secrets/**",
            "non-vacuity: index 1 of the document is now the other boundary",
        );
        let mut candidate = current.clone();
        candidate.boundaries[1].mode = BoundaryMode::RedactThenRemote;

        // In order: the identity key agrees, so the edit is applied in place and
        // the comments inside the array survive — the whole reason per-index
        // matching exists.
        let in_order =
            apply_config_delta(BOUNDARIES_CONFIG, &current, &candidate).expect("edit applies");
        assert_eq!(
            boundary_mode(&in_order, "docs/**"),
            Some(BoundaryMode::RedactThenRemote)
        );
        assert_eq!(
            boundary_mode(&in_order, "secrets/**"),
            Some(BoundaryMode::LocalOnly)
        );
        assert!(
            in_order.contains("# The one that must never leave this machine."),
            "an in-order per-index edit keeps the array's comments:\n{in_order}",
        );

        // Reordered: `path_glob` disagrees at the index the delta names, so the
        // edit falls through to the wholesale replacement.
        let edited =
            apply_config_delta(BOUNDARIES_REORDERED, &current, &candidate).expect("edit applies");

        assert_eq!(
            boundary_mode(&edited, "docs/**"),
            Some(BoundaryMode::RedactThenRemote),
            "the write must land on the glob it named:\n{edited}",
        );
        assert_eq!(
            boundary_mode(&edited, "secrets/**"),
            Some(BoundaryMode::LocalOnly),
            "and must not relax the one it did not name:\n{edited}",
        );
        // Which path was taken, said out loud rather than inferred: the
        // fallback re-renders the array, so the comments *inside* it are the
        // price of not writing to the wrong element. Everything outside it
        // survives, as it does under every other branch.
        assert!(
            !edited.contains("# The one that must never leave this machine."),
            "the wholesale fallback is what ran, and its cost is the array's \
             comments — if this survived, the per-index path took the edit and \
             the assertions above are passing for the wrong reason:\n{edited}",
        );
        assert!(
            edited.contains(r#"effort = "high""#),
            "the fallback replaces one key, not the document:\n{edited}",
        );
    }

    #[test]
    fn a_reordered_providers_array_does_not_bind_a_rotated_credential_to_another_endpoint() {
        // The same defect in the array the writers touch most. Memory holds
        // [anthropic, mirror]; the user swaps them by hand; `teton provider add
        // anthropic --auth-ref …` rotates the credential, which
        // replace-or-insert applies at **index 0**. By position that writes the
        // new keychain reference — and anthropic's model and kind — onto the
        // mirror's entry, leaving a document that sends the rotated credential
        // to `mirror.example.com` and still validates.
        const IN_ORDER: &str = r#"[[providers]]
# The one I actually pay for.
id = "anthropic"
kind = "anthropic"
endpoint = "https://api.anthropic.com"
model = "claude-opus-5"
auth_ref = "keychain:anthropic-old"

[[providers]]
id = "mirror"
kind = "openai-compatible"
endpoint = "https://mirror.example.com/v1"
model = "mirror-model"
auth_ref = "keychain:mirror"
"#;
        let reordered = {
            let mut blocks: Vec<&str> = IN_ORDER.split("[[providers]]").collect();
            blocks.swap(1, 2);
            blocks.join("[[providers]]")
        };
        let current = Config::load(IN_ORDER).expect("two providers load");
        assert_eq!(current.providers[0].id, "anthropic");
        assert_eq!(
            Config::load(&reordered)
                .expect("the reordered document loads")
                .providers[0]
                .id,
            "mirror",
            "non-vacuity: the document's index 0 is the other provider",
        );
        let mut candidate = current.clone();
        candidate.providers[0].auth_ref = Some("keychain:anthropic-rotated".to_owned());

        let edited = apply_config_delta(&reordered, &current, &candidate).expect("edit applies");

        let reloaded = Config::load(&edited).expect("the edited document must load");
        let provider = |id: &str| {
            reloaded
                .providers
                .iter()
                .find(|provider| provider.id == id)
                .unwrap_or_else(|| panic!("`{id}` survived the write:\n{edited}"))
                .clone()
        };
        assert_eq!(
            provider("anthropic").auth_ref.as_deref(),
            Some("keychain:anthropic-rotated"),
            "the rotation must land on the provider it named:\n{edited}",
        );
        assert_eq!(
            provider("mirror").auth_ref.as_deref(),
            Some("keychain:mirror"),
            "and must not attach the new credential to another endpoint:\n{edited}",
        );
        assert_eq!(
            provider("mirror").endpoint.as_deref(),
            Some("https://mirror.example.com/v1"),
        );
        assert!(
            !edited.contains("# The one I actually pay for."),
            "the wholesale fallback is what ran; if this comment survived, the \
             per-index path took the edit:\n{edited}",
        );
    }

    #[test]
    fn a_longer_candidate_whose_prefix_also_changed_is_replaced_wholesale() {
        // The classification edge with no witness until now, and the costliest
        // of the four branches to reach silently: the array grew *and* an
        // element already there changed, so neither "append past what is there"
        // nor "edit at these indexes" describes it. There is no correspondence
        // worth trusting, so the array is re-rendered — and the comment and
        // unknown key inside the existing entry are the recorded cost.
        let current = registered_provider();
        let mut candidate = current.clone();
        candidate.providers[0].model = Some("claude-sonnet-4".to_owned());
        candidate.providers.push(second_provider());

        let edited = apply_config_delta(REGISTERED_PROVIDER_CONFIG, &current, &candidate)
            .expect("edit applies");

        let reloaded = Config::load(&edited).expect("the edited document must load");
        assert_eq!(
            reloaded, candidate,
            "the array says what the candidate says"
        );
        assert!(
            !edited.contains("# The one I actually pay for."),
            "a wholesale replacement re-renders the array canonically:\n{edited}",
        );
        assert!(
            !edited.contains(r#"nickname = "the good one""#),
            "and takes the unknown key inside it with it — the cost, pinned so a \
             change to the classification cannot make it silent:\n{edited}",
        );
        assert!(
            edited.contains("# The comment that must not move."),
            "everything outside the array survives:\n{edited}",
        );
    }

    #[test]
    fn a_duplicate_key_refusal_names_the_key_and_still_quotes_no_value() {
        // The module's claim, narrowed to what it can actually promise. A
        // duplicate key is a *parse* failure whose diagnosis names the key —
        // schema vocabulary, and the one thing from the document that does
        // reach the log. The value on the line never does, which is the half
        // that matters: this file is secret-adjacent.
        const DOUBLED: &str = "[web]\n\
                               tier = \"off\"\n\
                               search_key_ref = \"keychain://teton/web-search\"\n\
                               tier = \"search\"\n";
        let current = Config::default();
        let mut candidate = current.clone();
        candidate.web.tier = WebTier::FetchAnyUrl;

        let refusal = apply_config_delta(DOUBLED, &current, &candidate)
            .expect_err("a duplicate key is not parseable TOML");

        let message = refusal.to_string();
        assert!(
            message.contains("line 4"),
            "a refusal says where:\n{message}",
        );
        assert!(
            message.contains("tier"),
            "the parser's diagnosis names the duplicated key, and this keeps it \
             verbatim:\n{message}",
        );
        assert!(
            !message.contains("keychain://teton/web-search"),
            "no value from the document may reach a loggable message \
             (BR-7):\n{message}",
        );
    }

    #[test]
    fn an_inline_array_of_tables_is_rewritten_as_a_block_the_document_can_hold() {
        // The third spelling of an array-of-tables: values, not sections. The
        // delta's replacement *is* sections, so this is the one edit that
        // reshapes the document — and it must reshape it into something legible
        // rather than into `[[providers ]]` with a stranded sub-table. (The
        // blank line the vacated `providers = …` line leaves behind is the
        // separator the *next* section owns, and is left alone: trimming it
        // would mean editing rendered text outside the parser.)
        let document = r#"# The provider, on one line because I like it that way.
providers = [ { id = "anthropic", kind = "anthropic", endpoint = "https://api.anthropic.com", auth_ref = "keychain:anthropic" } ] # and the one I pay for

[web]
# The comment that must not move.
tier = "off"
"#;
        let current = Config::load(document).expect("an inline providers array loads");
        let mut candidate = current.clone();
        candidate.providers[0].model = Some("claude-sonnet-4".to_owned());

        let edited = apply_config_delta(document, &current, &candidate).expect("edit applies");

        assert!(
            !edited.contains("[[providers ]]"),
            "the inline key's decor must not leak into the header:\n{edited}",
        );
        assert!(edited.contains("[[providers]]"), "{edited}");
        assert!(!edited.contains("providers = ["), "{edited}");
        let providers_at = edited.find("[[providers]]").expect("providers section");
        let capabilities_at = edited
            .find("[providers.capabilities]")
            .expect("capabilities section");
        let web_at = edited.find("[web]").expect("web section");
        assert!(
            web_at < providers_at && providers_at < capabilities_at,
            "the reshaped block renders contiguously, past what the user wrote:\n{edited}",
        );
        assert!(
            edited.contains("# The comment that must not move."),
            "{edited}"
        );
        // The comments around the key documented the key, and the key still
        // exists — only its spelling changed. Rendered through the header's own
        // decor they would have produced `[[# …⏎providers]]`, which is not TOML
        // at all, so they move onto the block instead of into the brackets
        // (OQ-1). Both of them: the block above the key *and* the note that sat
        // beside it, which has nowhere else to go once the line it annotated is
        // gone — a comment silently deleted is the collateral this whole module
        // exists to end.
        assert!(
            edited.contains(
                "# The provider, on one line because I like it that way.\n\
                 # and the one I pay for\n\
                 [[providers]]"
            ),
            "both of the key's comments travel with it, in the order they were \
             written:\n{edited}",
        );

        let reloaded = Config::load(&edited).expect("the edited document must load");
        assert_eq!(reloaded, candidate);
    }

    #[test]
    fn a_dotted_key_is_edited_where_the_document_spells_it() {
        // The third spelling of a *table*: `web.tier = …` at the top level,
        // which the module claims to edit in place alongside `[web]` and
        // `web = { … }`. Claimed, therefore pinned.
        let document = r#"# The knob I flip most.
web.tier = "fetch_user_url"
# Nothing in this build reads this one.
web.mood = "curious"
"#;
        let current = Config::load(document).expect("a dotted [web] loads");
        let mut candidate = current.clone();
        candidate.web.tier = WebTier::FetchAnyUrl;

        let edited = apply_config_delta(document, &current, &candidate).expect("edit applies");

        let changed = changed_lines(document, &edited);
        assert_eq!(
            changed,
            vec![1],
            "a dotted key is edited in place:\n{edited}"
        );
        assert!(edited.contains(r#"web.tier = "fetch_any_url""#), "{edited}");
        assert!(edited.contains(r#"web.mood = "curious""#), "{edited}");
        assert!(edited.contains("# The knob I flip most."), "{edited}");

        let reloaded = Config::load(&edited).expect("the edited document must load");
        assert_eq!(reloaded, candidate);
    }

    #[test]
    fn a_section_the_write_adds_lands_after_everything_already_in_the_file() {
        // Registering the first provider into a config that names none. The
        // canonical document puts `[[providers]]` before the unknown table this
        // document ends with; the write must not take that ordering as
        // permission to wedge a new section between two the user wrote.
        let document = r#"effort = "high"

[web]
# The comment that must not move.
tier = "off"

# A table this build has never heard of, and the last thing in the file.
[experimental]
knob = 3
"#;
        let current = Config::load(document).expect("a provider-less config loads");
        assert!(current.providers.is_empty());
        let mut candidate = current.clone();
        candidate.providers = hand_written().providers;

        let edited = apply_config_delta(document, &current, &candidate).expect("edit applies");

        assert!(
            edited.starts_with(document.trim_end()),
            "everything already in the file stays where it is:\n{edited}",
        );
        let experimental_at = edited.find("[experimental]").expect("unknown table");
        let providers_at = edited.find("[[providers]]").expect("added section");
        assert!(
            experimental_at < providers_at,
            "an addition appends:\n{edited}",
        );
        let reloaded = Config::load(&edited).expect("the edited document must load");
        assert_eq!(reloaded, candidate);
    }

    #[test]
    fn an_untouched_array_of_tables_keeps_its_own_comments() {
        let current = hand_written();
        let mut candidate = current.clone();
        candidate.web.cache_ttl_secs = 60;

        let edited =
            apply_config_delta(HAND_WRITTEN_CONFIG, &current, &candidate).expect("edit applies");

        assert!(
            edited.contains("# The one I actually pay for."),
            "an array nobody changed is not rewritten:\n{edited}",
        );
        assert_eq!(changed_lines(HAND_WRITTEN_CONFIG, &edited).len(), 1);
    }

    #[test]
    fn a_hand_edit_the_daemon_never_read_rides_along_untouched() {
        // BR-5 / ADR-1's reason for diffing current-vs-candidate: the file has
        // drifted since the daemon read it (someone shortened the cache
        // window), and the in-memory `current` still says 900. A write about
        // the tier must not carry that stale 900 back onto the file.
        let drifted = HAND_WRITTEN_CONFIG.replace("cache_ttl_secs = 900", "cache_ttl_secs = 42");
        let current = hand_written();
        let mut candidate = current.clone();
        candidate.web.tier = WebTier::FetchUserUrl;

        let edited = apply_config_delta(&drifted, &current, &candidate).expect("edit applies");

        assert_eq!(changed_lines(&drifted, &edited).len(), 1, "{edited}");
        let reloaded = Config::load(&edited).expect("the edited document must load");
        assert_eq!(reloaded.web.cache_ttl_secs, 42, "the document is the truth");
        assert_eq!(
            reloaded.web.tier,
            WebTier::FetchUserUrl,
            "except at the delta"
        );
    }

    #[test]
    fn a_commented_all_default_web_table_keeps_its_comments_on_the_first_write() {
        // The `[web]` table is `skip_serializing_if = "WebConfig::is_unset"`,
        // so a hand-written block that still holds every default is absent from
        // canonical(current) entirely. Treating that as "insert the whole
        // table" would overwrite the very comments the README teaches.
        let document = r#"[web]
# Turning this on once I pick a search backend.
tier = "off"
# Fifteen minutes, the default, spelled out so I can see it.
cache_ttl_secs = 900
# Nothing reads this.
mood = "curious"
"#;
        let current = Config::load(document).expect("an all-default [web] loads");
        assert!(current.web.is_unset());
        let mut candidate = current.clone();
        candidate.web.tier = WebTier::FetchUserUrl;

        let edited = apply_config_delta(document, &current, &candidate).expect("edit applies");

        assert!(edited.contains("# Turning this on once I pick a search backend."));
        assert!(edited.contains("# Fifteen minutes, the default, spelled out so I can see it."));
        assert!(edited.contains(r#"mood = "curious""#));
        assert!(edited.contains(r#"tier = "fetch_user_url""#), "{edited}");
        let reloaded = Config::load(&edited).expect("the edited document must load");
        assert_eq!(reloaded, candidate);
    }

    #[test]
    fn a_table_falling_back_to_its_defaults_loses_its_keys_not_its_unknown_neighbours() {
        let current = hand_written();
        let mut candidate = current.clone();
        candidate.web = WebConfig::default();

        let edited =
            apply_config_delta(HAND_WRITTEN_CONFIG, &current, &candidate).expect("edit applies");

        assert!(!edited.contains("tier ="), "{edited}");
        assert!(!edited.contains("search_endpoint"), "{edited}");
        assert!(
            edited.contains(r#"experimental_reranker = "colbert""#),
            "an unknown key is not collateral of a table reverting to defaults:\n{edited}",
        );
        let reloaded = Config::load(&edited).expect("the edited document must load");
        assert_eq!(reloaded, candidate);
    }

    #[test]
    fn an_inline_table_keeps_the_keys_the_delta_never_names() {
        let document = "web = { tier = \"off\", mood = \"curious\" }\n";
        let current = Config::load(document).expect("an inline [web] loads");
        let mut candidate = current.clone();
        candidate.web.tier = WebTier::FetchUserUrl;

        let edited = apply_config_delta(document, &current, &candidate).expect("edit applies");

        assert!(
            edited.contains(r#"mood = "curious""#),
            "an inline table is recursed into, not replaced:\n{edited}",
        );
        assert!(edited.contains(r#"tier = "fetch_user_url""#), "{edited}");
        let reloaded = Config::load(&edited).expect("the edited document must load");
        assert_eq!(reloaded, candidate);
    }

    #[test]
    fn an_empty_document_becomes_exactly_the_candidate() {
        // BR-6 / AC-6: a missing file is not an error. The edit base is the
        // empty document and the delta base is `Config::default()`, which *is*
        // the parse of an empty document.
        let mut candidate = Config::default();
        candidate.web.tier = WebTier::Search;
        candidate.web.search_endpoint = Some("https://search.example/api".to_owned());

        let edited = apply_config_delta("", &Config::default(), &candidate)
            .expect("the empty document is a valid edit base");

        let reloaded = Config::load(&edited).expect("a fresh document must load");
        assert_eq!(reloaded, candidate);
    }

    #[test]
    fn an_unparseable_document_refuses_and_names_where_the_parse_failed() {
        let error = apply_config_delta("[web\ntier = ", &hand_written(), &hand_written())
            .expect_err("a half-finished hand edit must refuse, not panic");

        match &error {
            DeltaError::Parse(sanitized) => assert!(
                error.to_string().contains(sanitized.as_str())
                    && sanitized.starts_with("TOML parse error at line 1, column "),
                "the refusal must carry the underlying failure, got: {error}",
            ),
            DeltaError::Serialize(_) => panic!("expected a parse refusal, got: {error}"),
        }
    }

    #[test]
    fn a_parse_refusal_says_where_and_what_without_quoting_the_line() {
        // The config file is secret-adjacent — `tetond` keeps it at 0600 — and
        // this message reaches the daemon log and an RPC error a client renders.
        // toml_edit's own Display would print the offending source line under a
        // caret gutter; the line here is the one that would hurt.
        const SECRET: &str = "sk-live-4b7-not-a-real-key";
        let document = format!(
            "[web]\ntier = \"search\"\nsearch_key_ref = \"keychain://teton/{SECRET}\nsearch_endpoint = \"https://x.example/api\"\n"
        );

        let error = apply_config_delta(&document, &hand_written(), &hand_written())
            .expect_err("an unterminated string must refuse");

        let message = error.to_string();
        assert!(
            message.contains("could not be parsed for editing"),
            "{message}"
        );
        // Where, and what — the two things a user needs to fix it.
        assert!(
            message.contains("TOML parse error at line 3, column "),
            "the refusal must locate the failure: {message}",
        );
        assert!(
            message.contains("invalid basic string"),
            "the refusal must diagnose the failure: {message}",
        );
        // And nothing of the line itself, through either rendering: `Display`
        // is what gets logged, `Debug` is what a `{:?}` in a caller would log.
        assert!(
            !message.contains(SECRET) && !message.contains("search_key_ref"),
            "the refusal quoted the document back: {message}",
        );
        let debugged = format!("{error:?}");
        assert!(
            !debugged.contains(SECRET) && !debugged.contains("search_key_ref"),
            "the error retained the document: {debugged}",
        );
    }

    #[test]
    fn a_whitespace_only_document_is_the_same_edit_base_as_a_missing_one() {
        // What a truncated write leaves behind. There is nothing in it to
        // preserve, so it heals into a complete document rather than carrying
        // its blank lines forward.
        assert!(document_is_effectively_empty("  \n\n\t\n"));
        assert!(document_is_effectively_empty(""));
        assert!(!document_is_effectively_empty("# a comment\n"));

        let mut candidate = Config::default();
        candidate.web.tier = WebTier::FetchUserUrl;

        let healed = apply_config_delta("  \n\n\t\n", &Config::default(), &candidate)
            .expect("a whitespace-only document is a valid edit base");
        let fresh = apply_config_delta("", &Config::default(), &candidate)
            .expect("the empty document is a valid edit base");

        assert_eq!(healed, fresh, "the two bases must agree");
        let reloaded = Config::load(&healed).expect("the healed document must load");
        assert_eq!(reloaded, candidate);
    }

    #[test]
    fn the_first_write_to_a_crlf_document_normalizes_its_line_endings() {
        // A recorded limitation, pinned so it is visible rather than silent:
        // toml_edit re-emits a parsed document with `\n`, so the first daemon
        // write to a config authored on Windows rewrites its line endings once.
        // Content, comments, key order and unknown keys all survive it.
        let document = HAND_WRITTEN_CONFIG.replace('\n', "\r\n");
        let current = Config::load(&document).expect("a CRLF document loads");
        let mut candidate = current.clone();
        candidate.web.tier = WebTier::FetchAnyUrl;

        let edited = apply_config_delta(&document, &current, &candidate).expect("edit applies");

        assert!(
            !edited.contains('\r'),
            "the write normalizes CRLF to LF — recorded, not fixed:\n{edited}",
        );
        assert_eq!(
            changed_lines(HAND_WRITTEN_CONFIG, &edited),
            vec![HAND_WRITTEN_CONFIG
                .lines()
                .position(|line| line == r#"tier = "search""#)
                .expect("fixture holds the tier line")],
            "and it changes nothing else — the CRLF document ends up exactly \
             where the LF one would:\n{edited}",
        );
        let reloaded = Config::load(&edited).expect("the edited document must load");
        assert_eq!(reloaded, candidate);
    }

    #[test]
    fn table_section_slices_the_web_table_out_of_the_document_verbatim() {
        let section = table_section(HAND_WRITTEN_CONFIG, "web").expect("the document names [web]");

        assert!(section.starts_with("[web]"), "{section}");
        assert!(
            HAND_WRITTEN_CONFIG.contains(section.trim_end()),
            "the slice must be a substring of the document it came from:\n{section}",
        );
        assert!(
            section.contains("# A reference, never a raw key"),
            "{section}"
        );
        assert!(section.contains(r#"search_auth = "X-Subscription-Token: {key}""#));
        assert!(
            section.contains(r#"experimental_reranker = "colbert""#),
            "{section}"
        );
        assert!(!section.contains("[experimental]"), "{section}");
    }

    #[test]
    fn table_section_is_none_when_the_document_names_no_such_table() {
        assert!(table_section(HAND_WRITTEN_CONFIG, "permissions").is_none());
        assert!(table_section("", "web").is_none());
        assert!(table_section("[web\n", "web").is_none());
    }

    #[test]
    fn the_edited_web_section_is_the_one_the_write_lands() {
        // ADR-2 / BR-3 in miniature: what a preview would show is sliced from
        // the edited document, so it cannot disagree with what is written.
        let current = hand_written();
        let mut candidate = current.clone();
        candidate.web.tier = WebTier::FetchUserUrl;

        let edited =
            apply_config_delta(HAND_WRITTEN_CONFIG, &current, &candidate).expect("edit applies");
        let section = table_section(&edited, "web").expect("the edited document names [web]");

        assert!(edited.contains(section.trim_end()), "{section}");
        assert!(section.contains(r#"tier = "fetch_user_url""#), "{section}");
        assert!(
            section.contains("# A reference, never a raw key"),
            "{section}"
        );
    }
}
