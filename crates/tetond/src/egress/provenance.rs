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

/// The sentinel "path" reported when a request is blocked because some content
/// carried **unknown** provenance rather than a specific boundary source.
///
/// A tool whose touched files cannot be determined (notably `shell`, which runs
/// an arbitrary command) reports [`Provenance::unknown`]; the egress inspector
/// fail-closes on it (REQ-544 C-1). The block still needs a content-free `path`
/// for its `privacy_block` event and typed error — this is it. It is not a real
/// repo path, and by construction leaks no file content.
pub const UNKNOWN_PROVENANCE_PATH: &str = "<unknown-provenance>";

/// The set of repo-relative source paths a piece of content was derived from.
///
/// A `BTreeSet` keeps the sources ordered so that inspection and any diagnostic
/// output are deterministic (a property the egress-capture tests rely on). Paths
/// are stored exactly as the reader supplied them; boundary matching normalizes
/// them (see [`crate::egress::inspector`]).
///
/// Beyond the known sources, provenance carries an [`unknown`](Provenance::is_unknown)
/// bit: content the daemon *could not attribute to a specific file set* (a
/// `shell` result, say). Unknown provenance is **fail-closed** at egress — when
/// any boundary is configured it is blocked exactly like a boundary hit, because
/// the daemon cannot prove the content is boundary-free (REQ-544 C-1).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Provenance {
    sources: BTreeSet<String>,
    /// Some contributing content had indeterminate origin: block fail-closed.
    unknown: bool,
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
        }
    }

    /// Provenance for content read from a single file at `path`.
    #[must_use]
    pub fn tainted_by(path: impl Into<String>) -> Self {
        let mut sources = BTreeSet::new();
        sources.insert(path.into());
        Self {
            sources,
            unknown: false,
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
        }
    }

    /// Mark this provenance as carrying content of indeterminate origin.
    pub fn mark_unknown(&mut self) {
        self.unknown = true;
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

    /// The source paths, in deterministic (sorted) order.
    pub fn sources(&self) -> impl Iterator<Item = &str> {
        self.sources.iter().map(String::as_str)
    }

    /// Whether `path` is one of this provenance's sources.
    #[must_use]
    pub fn contains(&self, path: &str) -> bool {
        self.sources.contains(path)
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
    /// A block read verbatim from `path`. Its provenance is `{path}`.
    #[must_use]
    pub fn from_file(path: impl Into<String>, content: impl Into<String>) -> Self {
        let path = path.into();
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

    #[test]
    fn empty_provenance_has_no_sources() {
        let p = Provenance::empty();
        assert!(p.is_empty());
        assert_eq!(p.len(), 0);
        assert_eq!(p.sources().count(), 0);
    }

    #[test]
    fn tainted_by_records_the_source() {
        let p = Provenance::tainted_by("secrets/prod.env");
        assert!(!p.is_empty());
        assert!(p.contains("secrets/prod.env"));
        assert_eq!(p.sources().collect::<Vec<_>>(), vec!["secrets/prod.env"]);
    }

    #[test]
    fn merge_is_a_set_union_without_duplicates() {
        let mut a = Provenance::tainted_by("a.txt");
        a.merge(&Provenance::tainted_by("b.txt"));
        a.merge(&Provenance::tainted_by("a.txt")); // duplicate ignored
        assert_eq!(a.len(), 2);
        // Deterministic, sorted order.
        assert_eq!(a.sources().collect::<Vec<_>>(), vec!["a.txt", "b.txt"]);
    }

    #[test]
    fn union_folds_both_sides() {
        let u = Provenance::tainted_by("x").union(&Provenance::tainted_by("y"));
        assert!(u.contains("x"));
        assert!(u.contains("y"));
    }

    #[test]
    fn a_file_block_carries_its_path_as_provenance() {
        let b = ContextBlock::from_file("secrets/key.pem", "-----BEGIN KEY-----");
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
        let original = ContextBlock::from_file("secrets/prod.env", "API_KEY=super-secret-xyzzy");
        let summary = original.derive("This file configures the production API credentials.");
        assert_eq!(summary.provenance(), original.provenance());
        assert!(summary.provenance().contains("secrets/prod.env"));
        // And the derived content genuinely shares no bytes with the secret.
        assert!(!summary.content().contains("xyzzy"));
    }

    #[test]
    fn a_chain_of_derivations_still_carries_the_original_source() {
        let original = ContextBlock::from_file("secrets/a", "raw");
        let once = original.derive("summary");
        let twice = once.derive("summary of the summary");
        assert!(twice.provenance().contains("secrets/a"));
    }

    #[test]
    fn assembled_provenance_unions_every_block() {
        let blocks = vec![
            ContextBlock::synthetic("system"),
            ContextBlock::from_file("src/main.rs", "fn main() {}"),
            ContextBlock::from_file("secrets/prod.env", "API_KEY=1"),
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
        let mut p = Provenance::tainted_by("src/main.rs");
        assert!(!p.is_unknown());
        p.merge(&Provenance::unknown());
        assert!(p.is_unknown());
        assert!(p.contains("src/main.rs"), "known sources are retained");
        // Merging a clean provenance never clears the unknown bit.
        p.merge(&Provenance::tainted_by("README.md"));
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
