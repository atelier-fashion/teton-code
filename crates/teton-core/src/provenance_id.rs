//! The minted identity of a provenance source (REQ-571 ADR-A).
//!
//! A privacy-boundary verdict keys on "which repo file did this content come
//! from?". Before this module that answer was a `String`, so the claim lived in
//! a doc comment: `grep`/`glob` happened to pass `strip_prefix(root)` output
//! while `read`/`edit` happened to pass the request argument, and both
//! type-checked identically. [`ProvenanceId`] makes the claim a fact the
//! compiler enforces — the channel accepts only a value that some code was in a
//! position to establish.
//!
//! Two constructors, both explicit at the call site:
//!
//! | Constructor | For | Guarantee |
//! |---|---|---|
//! | [`ProvenanceId::from_resolved`] | Files the daemon actually opened (`read`, `edit`, `grep`, `glob`) | Derived from the resolved path, so it names the file that was read |
//! | [`ProvenanceId::claimed`] | A path *asserted* by a third party (MCP tool arguments) | Best-effort: normalized identically, but the daemon never observed it |
//!
//! # Canonical form
//!
//! An id is a non-empty, repo-root-relative, `/`-separated path with no `.`,
//! `..`, or empty segment. Both constructors produce exactly that form or an
//! error — never a third outcome, and in particular never a fallback to the
//! caller's input (ADR-B).
//!
//! # No filesystem access
//!
//! `teton-core` is a no-I/O crate (conventions.md), so everything here is path
//! arithmetic: `strip_prefix`, separator normalization, segment validation.
//! **Canonicalization is the caller's job** and stays in `tetond`, where the
//! filesystem is. That division is why a `..` segment is *rejected* rather than
//! collapsed: resolving `..` lexically is unsound in the presence of symlinks
//! (`a/link/../b` need not be `a/b`), so a lexical collapse here would mint an
//! id naming a file that was never opened — the precise class of bug REQ-571
//! exists to close (BR-6, LESSON-494).
//!
//! A `.` or repeated-separator segment carries no such hazard — it is a pure
//! no-op that `std::path::Components` itself elides — so it is normalized away.
//! This keeps [`ProvenanceId::claimed`] agreeing with
//! [`ProvenanceId::from_resolved`] on the same file, which matters because
//! `strip_prefix` has already elided those segments by the time
//! `from_resolved` sees them.

use std::path::Path;

/// Why a provenance source could not be minted into a [`ProvenanceId`].
///
/// Every variant is a refusal, not a downgrade. A caller that cannot mint an id
/// must fail closed (refuse the tool call, or report the provenance as unknown);
/// it must never substitute the raw request string, which is exactly the value
/// an attacker controls (ADR-B).
///
/// The offending path is carried in the error because it is user- or
/// tool-supplied path text, not file content or a credential — the same posture
/// as [`crate::BoundaryError`], which surfaces the glob it failed to compile.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProvenanceError {
    /// `resolved` is not under `root`, so no repo-relative identity exists for
    /// it. The tool must refuse: a resolved path outside the root is precisely
    /// the case where substituting any other value would be worst.
    #[error("resolved path '{resolved}' is not under the repo root '{root}'")]
    NotUnderRoot {
        /// The repo root the id was derived against.
        root: String,
        /// The resolved path that failed to strip.
        resolved: String,
    },
    /// The derived id is absolute. Ids are repo-root-relative, and boundary
    /// globs are authored as repo-root-relative patterns, so an absolute id
    /// would silently match nothing.
    // NB: the field is `path`, not `source` — thiserror reads a field named
    // `source` as the error's cause, which a `String` cannot be.
    #[error("provenance source '{path}' is absolute; ids are repo-root-relative")]
    Absolute {
        /// The offending source, after separator normalization.
        path: String,
    },
    /// The derived id retains a `..` segment. This is never collapsed here:
    /// only the filesystem can say what `..` resolves through, so the caller
    /// must canonicalize first (BR-6).
    #[error(
        "provenance source '{path}' retains a '..' segment; \
         canonicalize before minting (a lexical collapse is unsound through symlinks)"
    )]
    ParentTraversal {
        /// The offending source, after separator normalization.
        path: String,
    },
    /// Nothing was left after normalization — an empty string, a lone `.`, or a
    /// resolved path equal to the root itself. There is no file to attribute.
    #[error("provenance source is empty after normalization; there is no file to attribute")]
    Empty,
}

/// The repo-relative identity of a file whose content entered model context.
///
/// This is the only type the provenance channel accepts. It is comparable and
/// orderable so a set of them dedupes by identity: two spellings of one file
/// mint one id and therefore occupy one slot, never two.
///
/// # Deliberately not convertible from a string
///
/// There is **no** `From<String>`, `From<&str>`, `Into<String>`, `FromStr`, or
/// `Display` impl, and that absence is the entire point (ADR-A). A permissive
/// conversion would mean every present and future call site is a place the
/// invariant can be quietly dropped — and the call site that drops it would
/// type-check exactly like the one that honours it. Passing a raw string is a
/// compile error rather than a review catch:
///
/// ```compile_fail
/// use teton_core::ProvenanceId;
/// // A raw string is not an identity, and cannot become one implicitly.
/// let id: ProvenanceId = "secrets/prod.env".into();
/// ```
///
/// The reverse decay is equally absent, so an id cannot silently rejoin the
/// population of interchangeable strings; read it with
/// [`as_str`](ProvenanceId::as_str) instead, which is explicit at the call site:
///
/// ```compile_fail
/// use std::path::Path;
/// use teton_core::ProvenanceId;
/// let id = ProvenanceId::from_resolved(Path::new("/repo"), Path::new("/repo/a.rs")).unwrap();
/// let s: String = id.into();
/// ```
///
/// Note also the absence of `Deserialize`: deriving it on a newtype is a
/// `From<String>` in disguise, since any wire value would become an identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProvenanceId(String);

impl ProvenanceId {
    /// Mint the identity of a file the daemon actually resolved and opened.
    ///
    /// `resolved` must already be canonical — see the module docs; `root` is the
    /// canonical repo root. The id is `resolved` relative to `root`, with
    /// separators normalized to `/`.
    ///
    /// # Errors
    ///
    /// - [`ProvenanceError::NotUnderRoot`] when `resolved` is not under `root`.
    ///   **It never falls back to any other value** (ADR-B): a path outside the
    ///   root has no repo-relative identity, and the tool must refuse.
    /// - [`ProvenanceError::ParentTraversal`] when the remainder retains `..`,
    ///   i.e. the caller skipped canonicalization.
    /// - [`ProvenanceError::Absolute`] when the remainder is still absolute
    ///   (reachable only for a degenerate empty `root`).
    /// - [`ProvenanceError::Empty`] when `resolved` *is* `root`.
    ///
    /// ```
    /// use std::path::Path;
    /// use teton_core::ProvenanceId;
    ///
    /// let id = ProvenanceId::from_resolved(
    ///     Path::new("/repo"),
    ///     Path::new("/repo/secrets/prod.env"),
    /// )
    /// .unwrap();
    /// assert_eq!(id.as_str(), "secrets/prod.env");
    ///
    /// // Outside the root there is no identity — and no fallback.
    /// assert!(ProvenanceId::from_resolved(Path::new("/repo"), Path::new("/etc/passwd")).is_err());
    /// ```
    pub fn from_resolved(root: &Path, resolved: &Path) -> Result<Self, ProvenanceError> {
        let rel = resolved
            .strip_prefix(root)
            .map_err(|_| ProvenanceError::NotUnderRoot {
                root: root.to_string_lossy().into_owned(),
                resolved: resolved.to_string_lossy().into_owned(),
            })?;
        mint(&rel.to_string_lossy())
    }

    /// Mint an identity from a path a third party *asserted*, for MCP tool
    /// arguments only.
    ///
    /// Normalization is identical to [`from_resolved`](ProvenanceId::from_resolved),
    /// but the guarantee is strictly weaker: **the daemon never observed this
    /// file**. A remote MCP server names the paths it touched under arbitrary
    /// argument keys, and nothing on this machine can confirm the claim, so the
    /// id is a best-effort input to boundary matching rather than a record of
    /// what was read.
    ///
    /// The separate name is the point. It keeps the weaker guarantee visible at
    /// the call site instead of letting one permissive conversion reopen the
    /// hole for every caller — and it is *not* an escape hatch for the
    /// first-party file tools: `claimed` appearing in `read`, `edit`, `grep`, or
    /// `glob` is a bug, since those tools resolve the file themselves and must
    /// use [`from_resolved`](ProvenanceId::from_resolved).
    ///
    /// # Errors
    ///
    /// [`ProvenanceError::Absolute`], [`ProvenanceError::ParentTraversal`], or
    /// [`ProvenanceError::Empty`], per the canonical form in the module docs.
    /// A malformed assertion is refused here rather than silently matching no
    /// glob later.
    ///
    /// ```
    /// use teton_core::ProvenanceId;
    ///
    /// assert_eq!(ProvenanceId::claimed("./secrets/prod.env").unwrap().as_str(), "secrets/prod.env");
    /// assert!(ProvenanceId::claimed("/etc/passwd").is_err());
    /// assert!(ProvenanceId::claimed("../outside").is_err());
    /// ```
    pub fn claimed(source: &str) -> Result<Self, ProvenanceError> {
        mint(source)
    }

    /// The canonical form: repo-root-relative, `/`-separated, no `.`/`..`
    /// segment. This is the value boundary matching consumes.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// **Test seam** (REQ-571 TASK-122). Wrap `raw` verbatim, minting nothing
    /// and validating nothing.
    ///
    /// ## Why a deliberately unsound constructor exists
    ///
    /// ADR-D puts a well-formedness guard at the egress inspection point, ahead
    /// of boundary matching: a source that is absolute, or retains a `.`/`..`
    /// segment, fails closed whether or not a boundary is configured. ADR-A is
    /// what makes that guard **unreachable** — [`mint`] is the only way to get a
    /// [`ProvenanceId`], and it refuses every one of those spellings — and that
    /// is exactly the problem LESSON-508 names: a redundant guard with no test
    /// is one refactor away from being deleted as dead weight. The guard cannot
    /// be tested without a value the type system says cannot exist, so the seam
    /// that produces one is here, named for what it is.
    ///
    /// ## Why the feature gate rather than `#[doc(hidden)]`
    ///
    /// `test-seam` is off in every shipped build and is switched on only by
    /// `tetond`'s **dev**-dependency, so production code that reached for this
    /// does not compile — the same posture as `tetond`'s `fixture_id` and
    /// `RetainedContext::from_blocks`, and the reason ADR-A's "only a minted
    /// identity enters the channel" is a property of the binary rather than a
    /// convention. A `#[doc(hidden)] pub` escape hatch would compile fine in
    /// production and rely on nobody noticing it.
    #[cfg(any(test, feature = "test-seam"))]
    #[must_use]
    pub fn unvalidated_for_test(raw: &str) -> Self {
        ProvenanceId(raw.to_owned())
    }
}

/// Normalize and validate a candidate id into canonical form.
///
/// Shared by both constructors so `claimed` and `from_resolved` cannot drift
/// into disagreeing about the same file. Separator normalization happens
/// *first*, so a `\`-spelled traversal (`sub\..\x`) is validated as a traversal
/// rather than passing as one opaque segment.
fn mint(raw: &str) -> Result<ProvenanceId, ProvenanceError> {
    let normalized = raw.replace('\\', "/");

    // Absolute check: the platform parser catches native forms (including a
    // Windows drive prefix on a Windows target), and the leading-`/` test
    // catches a POSIX-style or `\`-spelled absolute path on any target.
    if normalized.starts_with('/') || Path::new(raw).is_absolute() {
        return Err(ProvenanceError::Absolute { path: normalized });
    }

    let mut segments: Vec<&str> = Vec::new();
    for segment in normalized.split('/') {
        match segment {
            // `..` is never collapsed here — see the module docs.
            ".." => return Err(ProvenanceError::ParentTraversal { path: normalized }),
            // A `.` or an empty segment (`a//b`, a trailing `/`) is a pure
            // no-op that `Path::components` elides, so elide it identically.
            "." | "" => {}
            s => segments.push(s),
        }
    }

    if segments.is_empty() {
        return Err(ProvenanceError::Empty);
    }
    Ok(ProvenanceId(segments.join("/")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    const ROOT: &str = "/repo";
    /// The one identity every spelling of the boundary file must mint.
    const CANONICAL: &str = "secrets/prod.env";

    /// The BR-3 spelling set, as a tool would receive it in a request argument:
    /// bare relative, `./`-prefixed, repeated `./` (both forms),
    /// absolute-inside-root, and `..`-traversing-but-inside-root.
    const BR3_SPELLINGS: [&str; 6] = [
        "secrets/prod.env",
        "./secrets/prod.env",
        ".//secrets/prod.env",
        "././secrets/prod.env",
        "/repo/secrets/prod.env",
        "sub/../secrets/prod.env",
    ];

    fn root() -> PathBuf {
        PathBuf::from(ROOT)
    }

    /// What a tool does before minting: join the request onto the root. The
    /// daemon additionally canonicalizes (filesystem work that cannot happen in
    /// this crate); this models the *un*-canonicalized caller, which is the
    /// case that must not produce a second identity.
    fn joined(spelling: &str) -> PathBuf {
        root().join(spelling)
    }

    #[test]
    fn lexically_equivalent_spellings_mint_one_identical_id() {
        // Five of the six BR-3 spellings need no filesystem to resolve:
        // `strip_prefix` is component-based, so `.` and repeated separators are
        // already gone by the time the id is derived.
        let ids: Vec<String> = BR3_SPELLINGS[..5]
            .iter()
            .map(|s| {
                ProvenanceId::from_resolved(&root(), &joined(s))
                    .unwrap_or_else(|e| panic!("spelling {s:?} should mint: {e}"))
                    .as_str()
                    .to_owned()
            })
            .collect();

        for (spelling, id) in BR3_SPELLINGS[..5].iter().zip(&ids) {
            assert_eq!(id, CANONICAL, "spelling {spelling:?} minted a divergent id");
        }
    }

    #[test]
    fn no_spelling_ever_mints_a_second_identity() {
        // The BR-3 invariant at this layer: every spelling yields *the* id or an
        // error. A second `Ok` value would be a second identity for one file,
        // which is what boundary verdicts would then disagree about.
        for spelling in BR3_SPELLINGS {
            match ProvenanceId::from_resolved(&root(), &joined(spelling)) {
                Ok(id) => assert_eq!(
                    id.as_str(),
                    CANONICAL,
                    "spelling {spelling:?} minted a second identity"
                ),
                Err(e) => assert!(
                    matches!(e, ProvenanceError::ParentTraversal { .. }),
                    "spelling {spelling:?} failed for an unexpected reason: {e}"
                ),
            }
        }
    }

    #[test]
    fn parent_traversing_spelling_agrees_once_the_caller_canonicalizes() {
        // `sub/../secrets/prod.env` is refused un-canonicalized (only the
        // filesystem can resolve `..` soundly). Once `tetond` canonicalizes it,
        // it is byte-identical to every other spelling — which is what closes
        // BR-3 end to end, with the canonicalization step covered at the tool
        // layer where the filesystem exists.
        let traversing = BR3_SPELLINGS[5];
        assert!(ProvenanceId::from_resolved(&root(), &joined(traversing)).is_err());

        let canonicalized = PathBuf::from("/repo/secrets/prod.env");
        let id = ProvenanceId::from_resolved(&root(), &canonicalized).unwrap();
        assert_eq!(id.as_str(), CANONICAL);
    }

    #[test]
    fn spellings_dedupe_to_one_entry_in_a_set() {
        // ADR-A's operational consequence: a provenance set keyed by identity
        // cannot end up holding one file twice under two names.
        let set: BTreeSet<ProvenanceId> = BR3_SPELLINGS[..5]
            .iter()
            .map(|s| ProvenanceId::from_resolved(&root(), &joined(s)).unwrap())
            .collect();
        assert_eq!(set.len(), 1);
        assert_eq!(set.iter().next().unwrap().as_str(), CANONICAL);
    }

    #[test]
    fn strip_prefix_failure_is_an_error_and_never_a_fallback() {
        // ADR-B: an exploration pass proposed `.unwrap_or_else(|_| raw.clone())`
        // here. It is rejected — a resolved path outside the root is exactly
        // where substituting the caller's string is worst.
        let outside = PathBuf::from("/etc/passwd");
        let err = ProvenanceId::from_resolved(&root(), &outside).unwrap_err();
        assert!(matches!(err, ProvenanceError::NotUnderRoot { .. }));
        assert!(err.to_string().contains("not under the repo root"));
    }

    #[test]
    fn a_sibling_directory_sharing_the_root_prefix_is_not_under_the_root() {
        // `/repo-evil/x` starts with the bytes of `/repo` but is a different
        // directory; matching must be component-wise, not textual.
        let err =
            ProvenanceId::from_resolved(&root(), Path::new("/repo-evil/secrets/x")).unwrap_err();
        assert!(matches!(err, ProvenanceError::NotUnderRoot { .. }));
    }

    #[test]
    fn absolute_input_is_rejected_by_both_constructors() {
        for source in ["/etc/passwd", "/repo/secrets/prod.env", "\\etc\\passwd"] {
            assert!(
                matches!(
                    ProvenanceId::claimed(source),
                    Err(ProvenanceError::Absolute { .. })
                ),
                "claimed({source:?}) should be rejected as absolute"
            );
        }

        // `from_resolved` reaches the same guard for a degenerate empty root,
        // where `strip_prefix` succeeds without consuming anything.
        assert!(matches!(
            ProvenanceId::from_resolved(Path::new(""), Path::new("/etc/passwd")),
            Err(ProvenanceError::Absolute { .. })
        ));
    }

    #[test]
    fn parent_traversal_is_rejected_by_both_constructors() {
        for source in ["../outside", "secrets/../../outside", "..", "sub/../x"] {
            assert!(
                matches!(
                    ProvenanceId::claimed(source),
                    Err(ProvenanceError::ParentTraversal { .. })
                ),
                "claimed({source:?}) should be rejected as a traversal"
            );
        }

        assert!(matches!(
            ProvenanceId::from_resolved(&root(), &joined("sub/../secrets/prod.env")),
            Err(ProvenanceError::ParentTraversal { .. })
        ));
    }

    #[test]
    fn windows_style_separators_are_normalized() {
        assert_eq!(
            ProvenanceId::claimed("secrets\\prod.env").unwrap().as_str(),
            CANONICAL
        );
        assert_eq!(
            ProvenanceId::claimed(".\\secrets\\prod.env")
                .unwrap()
                .as_str(),
            CANONICAL
        );

        // On a POSIX target the backslash form is one opaque filename segment
        // under the root; on Windows it is a real separator. Either way the id
        // normalizes to the same canonical form.
        assert_eq!(
            ProvenanceId::from_resolved(&root(), &joined("secrets\\prod.env"))
                .unwrap()
                .as_str(),
            CANONICAL
        );

        // Normalization precedes validation, so a `\`-spelled traversal is
        // still caught as a traversal rather than passing as one segment.
        assert!(matches!(
            ProvenanceId::claimed("sub\\..\\secrets\\prod.env"),
            Err(ProvenanceError::ParentTraversal { .. })
        ));
    }

    #[test]
    fn empty_and_no_op_sources_are_rejected() {
        for source in ["", ".", "./", "././", "./."] {
            assert!(
                matches!(ProvenanceId::claimed(source), Err(ProvenanceError::Empty)),
                "claimed({source:?}) should not mint an identity"
            );
        }
        // A resolved path equal to the root itself names no file.
        assert!(matches!(
            ProvenanceId::from_resolved(&root(), &root()),
            Err(ProvenanceError::Empty)
        ));
    }

    #[test]
    fn claimed_and_from_resolved_agree_on_the_same_file() {
        // The weaker guarantee of `claimed` is about provenance, not about
        // spelling: both constructors must name one file identically, or an MCP
        // assertion would miss a boundary a first-party read catches.
        let observed = ProvenanceId::from_resolved(&root(), &joined(CANONICAL)).unwrap();
        let asserted = ProvenanceId::claimed("./secrets//prod.env").unwrap();
        assert_eq!(observed, asserted);
        assert_eq!(asserted.as_str(), CANONICAL);
    }

    #[test]
    fn as_str_exposes_the_canonical_form_for_boundary_matching() {
        let id = ProvenanceId::from_resolved(&root(), &joined("src/main.rs")).unwrap();
        assert_eq!(id.as_str(), "src/main.rs");
        // Nested paths keep their `/` separators — boundary globs are
        // separator-sensitive (`secrets/*` does not cross a `/`).
        let deep = ProvenanceId::from_resolved(&root(), &joined("a/b/c/d.txt")).unwrap();
        assert_eq!(deep.as_str(), "a/b/c/d.txt");
    }
}
