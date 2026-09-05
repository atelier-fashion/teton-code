//! Content-provenance tagging — the honest implementation of BR-1.
//!
//! String-matching an outbound payload against every boundary file's contents is
//! both slow (re-scan the world on each request) and evadable (any whitespace or
//! encoding change defeats it). Instead we tag at the **context-assembly** layer:
//! every piece of context the daemon assembles carries the set of source files it
//! was derived from (its [`Provenance`]). When a request is built, the union of
//! its blocks' provenance travels with it, and the egress choke point rejects any
//! request whose provenance intersects a `local-only` boundary
//! ([`crate::egress::inspector`]).
//!
//! The load-bearing property is **survival across derivation**: a summary of a
//! boundary file, a snippet cut from it, or a tool result computed over it all
//! inherit its provenance, so BR-1's "derived verbatim" clause is enforced by
//! construction rather than by hoping a scanner catches the paraphrase. See the
//! residual limit documented in [`crate::egress`].

use std::collections::BTreeSet;

use teton_core::{ProvenanceError, ProvenanceId};
use teton_protocol::events::ProvenanceRejection;

/// The sentinel "path" reported when a request is blocked because some content
/// carried **unknown** provenance rather than a specific boundary source.
///
/// A tool whose touched files cannot be determined (notably `shell`, which runs
/// an arbitrary command) reports [`Provenance::unknown`]; the egress inspector
/// fail-closes on it (REQ-544 C-1). The block still needs a content-free `path`
/// for its `privacy_block` event and typed error — this is it. It is not a real
/// repo path, and by construction leaks no file content.
pub const UNKNOWN_PROVENANCE_PATH: &str = "<unknown-provenance>";

/// The sentinel "path" reported when a request is refused because a provenance
/// source was **malformed** rather than because it crossed a boundary.
///
/// The twin of [`UNKNOWN_PROVENANCE_PATH`], and a sentinel for the same reason:
/// `PrivacyBlock::path` is documented as a repo-relative path, and a source that
/// failed the canonical form is by definition not one. Naming it here keeps that
/// field honest, and keeps attacker-influenced text out of a field consumers
/// read as a path — the offending source travels on the paired
/// [`ProvenanceRejected`](teton_protocol::events::ProvenanceRejected) event
/// instead, sanitized, where it is labelled as the untrusted claim it is.
pub const MALFORMED_PROVENANCE_PATH: &str = "<malformed-provenance>";

/// The sentinel "path" reported when a `shell` command named a file that
/// matches a `local-only` boundary but lies **outside** the session root
/// (REQ-614 AC-5).
///
/// A third sentinel rather than reuse of [`UNKNOWN_PROVENANCE_PATH`], because
/// the two mean different things to the *pin*: an unknown-provenance block
/// taints the session liftably (`/shell allow`), and a boundary touch taints it
/// permanently. Folding them would make `~/.ssh/config` liftable, which BR-3
/// forbids.
///
/// It is a sentinel rather than the real path for the reason the other two are:
/// `PrivacyBlock::path` is consumed as a repo-relative path, and an absolute
/// path outside the root is not one — and printing the user's home directory
/// layout into an event is a disclosure the block does not need to make. The
/// *class* is what the user needs, and the remedy sentence says the rest.
pub const BOUNDARY_TOUCH_PATH: &str = "<boundary-touch>";

/// Byte cap on the source text carried by a `provenance_rejected` event.
///
/// A source is chosen by whoever asserted it — a remote MCP server can send
/// megabytes under a path-shaped key — so the report is bounded before it is
/// cloned to every subscriber.
const MAX_REPORTED_SOURCE_BYTES: usize = 256;

/// Prepare an attacker-influenced provenance source for reporting: strip
/// control characters, then truncate.
///
/// Both halves are load-bearing. **Control characters** are how a hostile source
/// forges structure in whatever renders it — a newline splits one notice into
/// two, an ANSI escape colours or moves a terminal cursor, a `\r` erases the
/// line that named the tool. They are replaced (not dropped) with `?` so the
/// report still shows that something was there. **Truncation** bounds a value
/// the daemon did not choose the length of, at a cost of a marker the reader can
/// see; the value is a diagnostic, never something to act on as a path, so a cut
/// tail loses nothing that could be trusted anyway.
#[must_use]
pub fn sanitize_reported_source(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(MAX_REPORTED_SOURCE_BYTES));
    for ch in raw.chars() {
        // `len_utf8` before pushing: truncating on a char boundary is what keeps
        // the result a valid `String` rather than a slice index panic waiting on
        // a multi-byte source.
        if out.len() + ch.len_utf8() > MAX_REPORTED_SOURCE_BYTES {
            out.push('…');
            return out;
        }
        out.push(if ch.is_control() { '?' } else { ch });
    }
    out
}

/// The wire reason for a mint failure.
///
/// One map, at the single seam where a `teton-core` refusal becomes a protocol
/// event, so the two vocabularies cannot drift into disagreeing about the same
/// refusal. [`ProvenanceError::NotUnderRoot`] has no wire twin of its own: it is
/// reachable only from `from_resolved`, i.e. a file the daemon *opened* outside
/// the root, which the tool refuses outright rather than reporting — so it maps
/// to the closest true statement, that the source has no repo-relative form.
/// [`ProvenanceError::ReservedScope`] (REQ-619) has no wire twin either, and is
/// the same statement about a file *inside* the root: a `<root>/~/…` path has
/// no repo-relative form, because that spelling belongs to the home scope. Both
/// therefore share `Absolute`'s wire reason rather than adding a variant to an
/// event vocabulary no surface renders differently.
#[must_use]
pub fn rejection_reason(err: &ProvenanceError) -> ProvenanceRejection {
    match err {
        ProvenanceError::Absolute { .. }
        | ProvenanceError::NotUnderRoot { .. }
        | ProvenanceError::ReservedScope { .. } => ProvenanceRejection::Absolute,
        ProvenanceError::ParentTraversal { .. } => ProvenanceRejection::ParentTraversal,
        ProvenanceError::Empty => ProvenanceRejection::Empty,
    }
}

/// The set of repo-relative source identities a piece of content was derived
/// from.
///
/// A `BTreeSet` keeps the sources ordered so that inspection and any diagnostic
/// output are deterministic (a property the egress-capture tests rely on).
///
/// Beyond the known sources, provenance carries an [`unknown`](Provenance::is_unknown)
/// bit: content the daemon *could not attribute to a specific file set* (a
/// `shell` result, say). Unknown provenance is **fail-closed** at egress — when
/// any boundary is configured it is blocked exactly like a boundary hit, because
/// the daemon cannot prove the content is boundary-free (REQ-544 C-1).
///
/// # Minted in, `&str` out (REQ-571 ADR-A)
///
/// A source enters only as a [`ProvenanceId`] — there is no way to add a raw
/// string, which is what stops the next contributor to this channel from tagging
/// content with a value the daemon merely *received* rather than resolved. It
/// leaves as `&str` ([`Provenance::sources`]), because everything downstream —
/// glob matching, the `privacy_block` event's `path`, the typed error — crosses
/// a protocol boundary where the canonical form is the payload. Narrowing
/// construction is the invariant; String is still the currency at the seam.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Provenance {
    sources: BTreeSet<ProvenanceId>,
    /// Some contributing content had indeterminate origin: block fail-closed.
    unknown: bool,
    /// Some contributing content named a boundary file the source set cannot
    /// hold — a path outside the session root, which mints no `ProvenanceId`
    /// for a glob to match (REQ-614, LESSON-623).
    ///
    /// Blocks exactly as [`Self::unknown`] does; the bit exists so the block
    /// reports [`BOUNDARY_TOUCH_PATH`] instead, which is what tells the taint
    /// machinery the pin is permanent rather than liftable.
    boundary_touch: bool,
}

impl Provenance {
    /// Provenance with no sources — content that did not come from any file
    /// (a system prompt, a synthesized instruction). Never blocked, because it
    /// can carry no boundary content.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            sources: BTreeSet::new(),
            unknown: false,
            boundary_touch: false,
        }
    }

    /// Provenance for content read from the single file `path` names.
    #[must_use]
    pub fn tainted_by(path: ProvenanceId) -> Self {
        let mut sources = BTreeSet::new();
        sources.insert(path);
        Self {
            sources,
            unknown: false,
            boundary_touch: false,
        }
    }

    /// Provenance for content whose origin cannot be determined — fail-closed.
    ///
    /// Egress treats this exactly like a boundary hit whenever any boundary is
    /// configured (REQ-544 C-1): the daemon cannot prove the content is
    /// boundary-free, so it refuses to send it remotely.
    #[must_use]
    pub fn unknown() -> Self {
        Self {
            sources: BTreeSet::new(),
            unknown: true,
            boundary_touch: false,
        }
    }

    /// Provenance for content a `shell` command derived from a boundary file
    /// **outside** the session root (REQ-614 AC-5).
    ///
    /// Also unknown — the daemon cannot say what else the command read — so it
    /// fail-closes on the same arm. What the extra bit buys is the pin's cause.
    #[must_use]
    pub fn boundary_touch() -> Self {
        Self {
            sources: BTreeSet::new(),
            unknown: true,
            boundary_touch: true,
        }
    }

    /// Whether some contributing content touched a boundary the source set
    /// cannot name.
    #[must_use]
    pub fn is_boundary_touch(&self) -> bool {
        self.boundary_touch
    }

    /// Mark this provenance as carrying content of indeterminate origin.
    pub fn mark_unknown(&mut self) {
        self.unknown = true;
    }

    /// The same provenance with its **opacity** lifted — `unknown` cleared —
    /// and everything else kept (BUG-215, REQ-614 BR-4).
    ///
    /// This is what `/shell allow` asserts: *the command whose reach the daemon
    /// could not prove touched no protected file*. It says nothing about the
    /// sources the daemon **did** prove, so every minted id stays and is
    /// matched against the boundary globs exactly as before — a `cat .env`
    /// after a lift is still a `read` of `.env`. And it says nothing about a
    /// [`boundary_touch`](Self::boundary_touch): that verdict *named* a
    /// protected path (out of root, so there is no id to keep), and the
    /// sentinel's `unknown` bit is what carries it, so a lift leaves that bit
    /// alone. Only the opacity that has no path behind it is released.
    #[must_use]
    pub fn with_unknown_lifted(&self) -> Self {
        Self {
            sources: self.sources.clone(),
            unknown: self.unknown && self.boundary_touch,
            boundary_touch: self.boundary_touch,
        }
    }

    /// Whether any contributing content had indeterminate origin (fail-closed).
    #[must_use]
    pub fn is_unknown(&self) -> bool {
        self.unknown
    }

    /// Whether this provenance has no sources **and** is not unknown — i.e.
    /// content that can carry no boundary material and needs no inspection. An
    /// unknown provenance is deliberately *not* empty, so egress still inspects
    /// (and fail-closes on) it.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty() && !self.unknown
    }

    /// The source paths in canonical form, in deterministic (sorted) order —
    /// the seam where a minted identity becomes the `&str` boundary matching and
    /// the `privacy_block` event consume.
    pub fn sources(&self) -> impl Iterator<Item = &str> {
        self.sources.iter().map(ProvenanceId::as_str)
    }

    /// The source identities themselves, for a consumer that carries provenance
    /// onward rather than matching on it — the MCP bridge, which re-tags a tool
    /// result with the same ids the call was inspected under.
    pub fn ids(&self) -> impl Iterator<Item = &ProvenanceId> {
        self.sources.iter()
    }

    /// Whether `path` (in canonical form) is one of this provenance's sources.
    #[must_use]
    pub fn contains(&self, path: &str) -> bool {
        self.sources.iter().any(|s| s.as_str() == path)
    }

    /// Number of distinct sources.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sources.len()
    }

    /// Fold `other`'s sources (and its unknown bit) into this provenance in
    /// place. Unknown is monotonic: once any contributor is unknown, the union is
    /// unknown (fail-closed).
    pub fn merge(&mut self, other: &Provenance) {
        for s in &other.sources {
            self.sources.insert(s.clone());
        }
        self.unknown |= other.unknown;
        // Monotonic for the same reason `unknown` is: a union that folded a
        // boundary-touching contributor is boundary-touching.
        self.boundary_touch |= other.boundary_touch;
    }

    /// Consume two provenances into their union.
    #[must_use]
    pub fn union(mut self, other: &Provenance) -> Self {
        self.merge(other);
        self
    }
}

/// A single assembled context block: some content plus the provenance that
/// governs whether it may leave the machine.
///
/// The `content` is the text (or serialized bytes) that will end up in a prompt.
/// Its `provenance` is what the egress inspector consults — never the content
/// itself, so the check is O(sources), not O(bytes), and cannot be defeated by
/// paraphrasing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextBlock {
    content: String,
    provenance: Provenance,
}

impl ContextBlock {
    /// A block read verbatim from the file `path` names. Its provenance is
    /// `{path}`.
    #[must_use]
    pub fn from_file(path: ProvenanceId, content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            provenance: Provenance::tainted_by(path),
        }
    }

    /// A block that came from no file (a system prompt, a synthesized message).
    /// Its provenance is empty, so it never triggers a boundary block.
    #[must_use]
    pub fn synthetic(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            provenance: Provenance::empty(),
        }
    }

    /// A block built directly from explicit `provenance` (e.g. a tool result the
    /// daemon computed over several files).
    #[must_use]
    pub fn with_provenance(content: impl Into<String>, provenance: Provenance) -> Self {
        Self {
            content: content.into(),
            provenance,
        }
    }

    /// Derive a new block *from* this one — a summary, a snippet, an extraction.
    ///
    /// This is the heart of BR-1's "derived verbatim" clause: the derived block
    /// inherits this block's full provenance, so a summary of a `local-only` file
    /// is itself `local-only` and will be blocked at egress even though its text
    /// shares no bytes with the original.
    #[must_use]
    pub fn derive(&self, content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            provenance: self.provenance.clone(),
        }
    }

    /// The block's content.
    #[must_use]
    pub fn content(&self) -> &str {
        &self.content
    }

    /// The block's provenance.
    #[must_use]
    pub fn provenance(&self) -> &Provenance {
        &self.provenance
    }
}

/// The union of every block's provenance — the provenance to attach to the
/// request those blocks were assembled into.
#[must_use]
pub fn assembled_provenance(blocks: &[ContextBlock]) -> Provenance {
    let mut prov = Provenance::empty();
    for b in blocks {
        prov.merge(&b.provenance);
    }
    prov
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture_id;

    #[test]
    fn empty_provenance_has_no_sources() {
        let p = Provenance::empty();
        assert!(p.is_empty());
        assert_eq!(p.len(), 0);
        assert_eq!(p.sources().count(), 0);
    }

    #[test]
    fn tainted_by_records_the_source() {
        let p = Provenance::tainted_by(fixture_id("secrets/prod.env"));
        assert!(!p.is_empty());
        assert!(p.contains("secrets/prod.env"));
        assert_eq!(p.sources().collect::<Vec<_>>(), vec!["secrets/prod.env"]);
    }

    #[test]
    fn merge_is_a_set_union_without_duplicates() {
        let mut a = Provenance::tainted_by(fixture_id("a.txt"));
        a.merge(&Provenance::tainted_by(fixture_id("b.txt")));
        a.merge(&Provenance::tainted_by(fixture_id("a.txt"))); // duplicate ignored
        assert_eq!(a.len(), 2);
        // Deterministic, sorted order.
        assert_eq!(a.sources().collect::<Vec<_>>(), vec!["a.txt", "b.txt"]);
    }

    #[test]
    fn union_folds_both_sides() {
        let u =
            Provenance::tainted_by(fixture_id("x")).union(&Provenance::tainted_by(fixture_id("y")));
        assert!(u.contains("x"));
        assert!(u.contains("y"));
    }

    #[test]
    fn a_file_block_carries_its_path_as_provenance() {
        let b = ContextBlock::from_file(fixture_id("secrets/key.pem"), "-----BEGIN KEY-----");
        assert!(b.provenance().contains("secrets/key.pem"));
    }

    #[test]
    fn synthetic_block_has_empty_provenance() {
        let b = ContextBlock::synthetic("You are a helpful assistant.");
        assert!(b.provenance().is_empty());
    }

    #[test]
    fn a_derived_summary_inherits_the_source_provenance() {
        // The BR-1 "derived verbatim" clause: a summary OF a boundary file is
        // itself boundary content, even though it shares no bytes with the file.
        let original =
            ContextBlock::from_file(fixture_id("secrets/prod.env"), "API_KEY=super-secret-xyzzy");
        let summary = original.derive("This file configures the production API credentials.");
        assert_eq!(summary.provenance(), original.provenance());
        assert!(summary.provenance().contains("secrets/prod.env"));
        // And the derived content genuinely shares no bytes with the secret.
        assert!(!summary.content().contains("xyzzy"));
    }

    #[test]
    fn a_chain_of_derivations_still_carries_the_original_source() {
        let original = ContextBlock::from_file(fixture_id("secrets/a"), "raw");
        let once = original.derive("summary");
        let twice = once.derive("summary of the summary");
        assert!(twice.provenance().contains("secrets/a"));
    }

    #[test]
    fn assembled_provenance_unions_every_block() {
        let blocks = vec![
            ContextBlock::synthetic("system"),
            ContextBlock::from_file(fixture_id("src/main.rs"), "fn main() {}"),
            ContextBlock::from_file(fixture_id("secrets/prod.env"), "API_KEY=1"),
        ];
        let prov = assembled_provenance(&blocks);
        assert_eq!(prov.len(), 2);
        assert!(prov.contains("src/main.rs"));
        assert!(prov.contains("secrets/prod.env"));
    }

    #[test]
    fn assembled_provenance_of_only_synthetic_blocks_is_empty() {
        let blocks = vec![
            ContextBlock::synthetic("system"),
            ContextBlock::synthetic("developer"),
        ];
        assert!(assembled_provenance(&blocks).is_empty());
    }

    /// **BUG-215.** A lift releases opacity and nothing else: the sources stay,
    /// and a boundary-touch sentinel — a *named* protected path with no id —
    /// keeps the `unknown` bit that carries it.
    ///
    /// Mutation: make `with_unknown_lifted` clear `unknown` unconditionally
    /// and the boundary-touch assertion goes red; make it drop `sources` and
    /// the first goes red.
    #[test]
    fn a_lift_releases_opacity_but_keeps_sources_and_a_boundary_touch() {
        let mut opaque_read = Provenance::tainted_by(fixture_id("src/main.rs"));
        opaque_read.mark_unknown();
        let lifted = opaque_read.with_unknown_lifted();
        assert!(!lifted.is_unknown(), "the opacity is released");
        assert!(lifted.contains("src/main.rs"), "the source is kept");
        assert!(!lifted.is_empty(), "…so egress still inspects it");

        let plain = Provenance::unknown().with_unknown_lifted();
        assert!(!plain.is_unknown());
        assert!(
            plain.is_empty(),
            "opacity with nothing behind it lifts to nothing"
        );

        let touch = Provenance::boundary_touch().with_unknown_lifted();
        assert!(touch.is_boundary_touch(), "a boundary touch is not opacity");
        assert!(
            touch.is_unknown(),
            "and keeps the bit the inspector reads first"
        );
        assert_eq!(
            touch,
            Provenance::boundary_touch(),
            "byte-identical: nothing lifted"
        );

        let clean = Provenance::tainted_by(fixture_id("README.md"));
        assert_eq!(
            clean.with_unknown_lifted(),
            clean,
            "nothing to lift, nothing changed"
        );
    }
    #[test]
    fn unknown_provenance_is_not_empty_so_egress_still_inspects_it() {
        // REQ-544 C-1: content of indeterminate origin (a shell result) must be
        // inspected, not skipped — so `is_empty()` is false and `is_unknown()`
        // is true.
        let p = Provenance::unknown();
        assert!(!p.is_empty(), "unknown provenance must not read as empty");
        assert!(p.is_unknown());
        assert_eq!(p.len(), 0, "unknown carries no specific sources");
    }

    #[test]
    fn unknown_is_monotonic_under_merge() {
        // Once any contributor is unknown, the union stays unknown (fail-closed).
        let mut p = Provenance::tainted_by(fixture_id("src/main.rs"));
        assert!(!p.is_unknown());
        p.merge(&Provenance::unknown());
        assert!(p.is_unknown());
        assert!(p.contains("src/main.rs"), "known sources are retained");
        // Merging a clean provenance never clears the unknown bit.
        p.merge(&Provenance::tainted_by(fixture_id("README.md")));
        assert!(p.is_unknown());
    }

    #[test]
    fn mark_unknown_flips_the_bit_in_place() {
        let mut p = Provenance::empty();
        assert!(p.is_empty());
        p.mark_unknown();
        assert!(p.is_unknown());
        assert!(!p.is_empty());
    }
}
