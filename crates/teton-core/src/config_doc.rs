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
//! - **Arrays are one key** — value arrays and arrays-of-tables alike. A
//!   changed array is replaced wholesale with the candidate's canonical
//!   rendering (ADR-1: element-wise diffing needs an identity function per
//!   array type and mis-targets when on-disk order drifted). Comments *inside*
//!   a changed array do not survive; comments everywhere else do.
//! - **Removal takes the key's attached decor with it** (spec OQ-1): the
//!   comment block prefixed to a key documents that key, so it travels with it.
//!   Free-standing comments and every other key's decor survive.
//!
//! # Purity
//!
//! Both functions are text-in/text-out with no filesystem access (ADR-2). The
//! atomic write — temp file, mode preservation, fsync, rename — stays in
//! `tetond` around them, so this crate's no-I/O rule is untouched and the
//! preservation logic is testable without a directory.

use crate::config::Config;
use toml_edit::{DocumentMut, InlineTable, Item, Table, Value};

/// Why a config document could not be edited.
///
/// BR-6: degradation is a loud refusal, never a silent rewrite. Every variant
/// carries the underlying failure so the caller can name it (LESSON-456,
/// BUG-146: a write that fails must say what failed, not "write failed").
#[derive(Debug, thiserror::Error)]
pub enum DeltaError {
    /// The document handed in is not parseable TOML — typically a half-finished
    /// hand edit.
    ///
    /// The caller refuses the write rather than falling back to a full
    /// re-serialization: that fallback would make the write succeed by
    /// destroying the edit in progress, which is the one outcome BR-6 forbids.
    #[error("the config file could not be parsed for editing, so nothing was written: {0}")]
    Parse(toml_edit::TomlError),

    /// A [`Config`] could not be serialized to its canonical TOML.
    ///
    /// Unreachable for a well-formed config — the same caveat
    /// [`Config::to_toml`] carries — and represented anyway because the
    /// alternative is a `.expect()` in the one code path whose whole purpose is
    /// to not lose the user's file.
    #[error("the config could not be serialized to TOML, so nothing was written: {0}")]
    Serialize(toml::ser::Error),
}

/// Apply the `current` → `candidate` delta to `doc_text`, returning the edited
/// document.
///
/// `doc_text` is the document as it exists on disk (the empty string for a file
/// that is not there yet — a missing file is not an error, BR-6). `current` is
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
    let mut document: DocumentMut = doc_text.parse().map_err(DeltaError::Parse)?;

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
    config
        .to_toml()
        .map_err(DeltaError::Serialize)?
        .parse()
        .map_err(DeltaError::Parse)
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
                // Not a table on both sides: a scalar, a value array or an
                // array-of-tables. All three are one key (ADR-1).
                _ => {
                    if !items_agree(current_item, candidate_item) {
                        target.set(key, candidate_item, appended_at);
                    }
                }
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
                if let Some(existing) = table.get_mut(key) {
                    carry_document_placement(existing, &mut replacement);
                    *existing = replacement;
                } else {
                    place_addition(&mut replacement, appended_at);
                    let _ = table.insert(key, replacement);
                }
            }
            Self::Inline(table) => {
                // A table nested in an inline table can only be spelled inline;
                // `into_value` performs that conversion and fails only for
                // `Item::None`, which never reaches here.
                let Ok(mut value) = item.clone().into_value() else {
                    return;
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
    fn a_changed_array_of_tables_is_replaced_wholesale_where_it_stood() {
        let current = hand_written();
        let mut candidate = current.clone();
        candidate.providers[0].model = Some("claude-sonnet-4".to_owned());

        let edited =
            apply_config_delta(HAND_WRITTEN_CONFIG, &current, &candidate).expect("edit applies");

        assert!(edited.contains(r#"model = "claude-sonnet-4""#), "{edited}");
        // ADR-1's stated cost, pinned rather than hoped for: a comment *inside*
        // a replaced array does not survive.
        assert!(
            !edited.contains("# The one I actually pay for."),
            "the array is re-rendered canonically:\n{edited}",
        );
        // The section keeps its place in the file, ahead of [web] and the
        // unknown table, and it renders as one block — the nested
        // `[providers.capabilities]` table comes with it rather than stranding
        // itself on the far side of the user's [web] block.
        let providers_at = edited.find("[[providers]]").expect("providers section");
        let capabilities_at = edited
            .find("[providers.capabilities]")
            .expect("capabilities section");
        let web_at = edited.find("[web]").expect("web section");
        assert!(
            providers_at < capabilities_at && capabilities_at < web_at,
            "a replaced section stays put, whole:\n{edited}",
        );
        assert!(edited.contains("# My machine. Hand-written"), "{edited}");
        assert!(edited.contains("# \"off\" (default)"), "{edited}");
        assert!(
            edited.contains(r#"experimental_reranker = "colbert""#),
            "{edited}"
        );
        assert!(edited.contains("knob = 3"), "{edited}");

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
    fn an_unparseable_document_refuses_and_names_the_parse_failure() {
        let error = apply_config_delta("[web\ntier = ", &hand_written(), &hand_written())
            .expect_err("a half-finished hand edit must refuse, not panic");

        match &error {
            DeltaError::Parse(inner) => assert!(
                error.to_string().contains(&inner.to_string()),
                "the refusal must carry the underlying failure, got: {error}",
            ),
            DeltaError::Serialize(_) => panic!("expected a parse refusal, got: {error}"),
        }
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
