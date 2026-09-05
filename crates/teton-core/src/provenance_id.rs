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
//! Three constructors, all explicit at the call site:
//!
//! | Constructor | For | Guarantee |
//! |---|---|---|
//! | [`ProvenanceId::from_resolved`] | Files the daemon actually opened (`read`, `edit`, `grep`, `glob`) | Derived from the resolved path, so it names the file that was read |
//! | [`ProvenanceId::from_home_resolved`] | Files the daemon *discovered* under the user's home (REQ-619 BR-3/BR-4: user skills) | Derived from the resolved path, so it names the file that was read |
//! | [`ProvenanceId::claimed`] | A path *asserted* by a third party (MCP tool arguments) | Best-effort: normalized identically, but the daemon never observed it |
//!
//! # Two scopes, and why they cannot collide (REQ-619 ADR-619-3)
//!
//! An id names a file relative to a root, and there are two roots a first-party
//! id is minted against:
//!
//! | Scope | Root | Spelling | Minted by |
//! |---|---|---|---|
//! | **repo** | the session root | `secrets/prod.env` | [`ProvenanceId::from_resolved`] |
//! | **home** | the user's `$HOME` | `~/.claude/skills/x/SKILL.md` | [`ProvenanceId::from_home_resolved`] |
//!
//! The home scope's marker is a leading `~` segment, and **every other
//! constructor reserves it**: [`ProvenanceId::from_resolved`] refuses a
//! root-relative remainder whose first segment is `~`, and so does
//! [`ProvenanceId::claimed`] ([`ProvenanceError::ReservedScope`]). The
//! disjointness is a property of the *set of constructors*, not of one of them:
//! a rule enforced on the first-party minter alone would leave the assertion
//! path — an MCP server naming the files it touched — able to write into a
//! scope nothing on this machine resolved (REQ-619 verify, M4). That refusal is
//! what makes the two scopes disjoint as *strings*, which is the property BR-3
//! asks for — a user skill at `skills/x/SKILL.md` under the home and a project
//! file at `skills/x/SKILL.md` under the repo can never share an id, so a
//! boundary verdict about one is never a verdict about the other. Only
//! `from_home_resolved` produces the marker, and it produces it for a file
//! discovery listed under the home.
//!
//! `~` needs no new glob language: `globset` treats it as an ordinary
//! character, and every builtin boundary is `**/`-prefixed (which matches zero
//! or more leading directories), so `**/.ssh/**` matches `~/.ssh/config` and a
//! user's `**/.claude/skills/**` matches `~/.claude/skills/x/SKILL.md` —
//! pinned by `boundary::tests::a_tilde_scoped_id_is_matched_by_a_user_glob_and_by_no_builtin`.
//!
//! # Canonical form
//!
//! An id is a non-empty, root-relative, `/`-separated path with no `.`, `..`,
//! or empty segment; a home-scoped id is that, prefixed with the `~` segment.
//! Every constructor produces exactly that form or an error — never a third
//! outcome, and in particular never a fallback to the caller's input (ADR-B).
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
    /// The repo-relative remainder begins with the `~` segment, which the
    /// **home** scope owns (REQ-619 ADR-619-3).
    ///
    /// Raised by [`ProvenanceId::from_resolved`] for a real file at
    /// `<session root>/~/…` — a directory literally named `~`, which a shell
    /// creates by accident (`mkdir ~/x` inside quotes) more often than on
    /// purpose — and by [`ProvenanceId::claimed`] for a third party that
    /// *asserts* such a path. Minting it would produce a string
    /// [`ProvenanceId::from_home_resolved`] can also produce, and one string
    /// meaning two files is precisely the "one file, two identities" defect
    /// inverted: a boundary glob written for the user's skills directory would
    /// start matching a repository path, and vice versa.
    ///
    /// The refusal is checked **after** `strip_prefix`, so a path outside the
    /// root is still [`ProvenanceError::NotUnderRoot`] — the stronger statement
    /// keeps precedence.
    ///
    /// `claimed` refuses it for a sharper reason (REQ-619 verify, M4): an MCP
    /// server names the paths it touched, and without this an assertion of
    /// `~/.ssh/config` would mint the *home* scope's spelling from a value the
    /// daemon never resolved — a third party writing an id into the scope
    /// reserved for files this machine's own discovery listed.
    #[error(
        "provenance source '{path}' begins with the reserved '~' segment; \
         that scope belongs to the home-relative minter"
    )]
    ReservedScope {
        /// The offending remainder, in canonical form.
        path: String,
    },
}

/// The home scope's marker: the first segment of every id
/// [`ProvenanceId::from_home_resolved`] mints, and the segment
/// [`ProvenanceId::from_resolved`] refuses (REQ-619 ADR-619-3).
const HOME_SCOPE: &str = "~";

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
    /// - [`ProvenanceError::ReservedScope`] when the remainder's first segment
    ///   is `~`, which the home scope owns (REQ-619 ADR-619-3).
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
    ///
    /// // The `~` scope is the home minter's; the repo scope will not spell it.
    /// assert!(ProvenanceId::from_resolved(Path::new("/repo"), Path::new("/repo/~/x")).is_err());
    /// ```
    pub fn from_resolved(root: &Path, resolved: &Path) -> Result<Self, ProvenanceError> {
        let rel = resolved
            .strip_prefix(root)
            .map_err(|_| ProvenanceError::NotUnderRoot {
                root: root.to_string_lossy().into_owned(),
                resolved: resolved.to_string_lossy().into_owned(),
            })?;
        let id = mint(&rel.to_string_lossy())?;
        // After the strip, so `NotUnderRoot` keeps precedence, and after the
        // mint, so `./~/x` is judged in the same canonical form `~/x` is.
        if id.first_segment() == HOME_SCOPE {
            return Err(ProvenanceError::ReservedScope { path: id.0 });
        }
        Ok(id)
    }

    /// Mint the identity of a file the daemon resolved under the user's **home**
    /// (REQ-619 BR-3/BR-4).
    ///
    /// `resolved` must already be canonical — see the module docs; `home` is the
    /// canonical home directory. The id is `~/` followed by `resolved` relative
    /// to `home`, with separators normalized to `/`.
    ///
    /// # What this is for, and what it is not
    ///
    /// It exists because a **user skill** is a file the user installed
    /// themselves and the registry then listed, so the daemon is in a position
    /// to say which file a block came from — the same fact that makes a project
    /// skill's file nameable. It is **not** a widening of what may be read:
    /// REQ-587 ADR-9 refused to invent an identity the minter has no root for,
    /// and that refusal stands. The home scope has its own root and its own
    /// constructor, and the tool jail is untouched — a `read` of
    /// `~/.claude/skills/x/SKILL.md` from a repo-rooted session is refused
    /// exactly as it was (REQ-619 AC-9). Call this only where discovery
    /// happened.
    ///
    /// # Errors
    ///
    /// - [`ProvenanceError::NotUnderRoot`] when `resolved` is not under `home`
    ///   — a skills directory symlinked out of the home, say. **No fallback**:
    ///   the caller fails closed, exactly as it does for the repo scope.
    /// - [`ProvenanceError::ParentTraversal`] when the remainder retains `..`,
    ///   i.e. the caller skipped canonicalization.
    /// - [`ProvenanceError::Absolute`] when the remainder is still absolute
    ///   (reachable only for a degenerate empty `home`).
    /// - [`ProvenanceError::Empty`] when `resolved` *is* `home`; the home
    ///   directory itself is not a file to attribute, and `~` alone is not an
    ///   id.
    ///
    /// ```
    /// use std::path::Path;
    /// use teton_core::ProvenanceId;
    ///
    /// let id = ProvenanceId::from_home_resolved(
    ///     Path::new("/home/u"),
    ///     Path::new("/home/u/.claude/skills/x/SKILL.md"),
    /// )
    /// .unwrap();
    /// assert_eq!(id.as_str(), "~/.claude/skills/x/SKILL.md");
    ///
    /// // Outside the home there is no home-relative identity — and no fallback.
    /// assert!(
    ///     ProvenanceId::from_home_resolved(Path::new("/home/u"), Path::new("/etc/passwd")).is_err()
    /// );
    /// ```
    pub fn from_home_resolved(home: &Path, resolved: &Path) -> Result<Self, ProvenanceError> {
        let rel = resolved
            .strip_prefix(home)
            .map_err(|_| ProvenanceError::NotUnderRoot {
                root: home.to_string_lossy().into_owned(),
                resolved: resolved.to_string_lossy().into_owned(),
            })?;
        // Mint the remainder through the one normalizer first, so the home
        // scope refuses every spelling the repo scope refuses and in the same
        // words — in particular `Empty`, which is what keeps a bare `~` (the
        // home directory itself) from becoming an id. The marker is prepended
        // to an already-canonical value, so the result is canonical by
        // construction rather than by a second pass.
        let relative = mint(&rel.to_string_lossy())?;
        Ok(ProvenanceId(format!("{HOME_SCOPE}/{}", relative.0)))
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
    /// [`ProvenanceError::Absolute`], [`ProvenanceError::ParentTraversal`],
    /// [`ProvenanceError::Empty`], or [`ProvenanceError::ReservedScope`], per
    /// the canonical form in the module docs. A malformed assertion is refused
    /// here rather than silently matching no glob later.
    ///
    /// `ReservedScope` is the reason this is not a bare `mint` (REQ-619 verify,
    /// M4). The `~` scope names files the daemon's own discovery listed under
    /// the user's home; a third party that could spell it would be asserting an
    /// identity in a scope it has no standing in — and the caller fails closed
    /// on the refusal exactly as it does for an absolute path.
    ///
    /// ```
    /// use teton_core::ProvenanceId;
    ///
    /// assert_eq!(ProvenanceId::claimed("./secrets/prod.env").unwrap().as_str(), "secrets/prod.env");
    /// assert!(ProvenanceId::claimed("/etc/passwd").is_err());
    /// assert!(ProvenanceId::claimed("../outside").is_err());
    /// // The home scope is not assertable.
    /// assert!(ProvenanceId::claimed("~/.ssh/config").is_err());
    /// // `~` anywhere else is an ordinary segment.
    /// assert!(ProvenanceId::claimed("x/~").is_ok());
    /// ```
    pub fn claimed(source: &str) -> Result<Self, ProvenanceError> {
        let id = mint(source)?;
        // The same test `from_resolved` makes, on the same canonical form, for
        // the same reason: one string may mean one file.
        if id.first_segment() == HOME_SCOPE {
            return Err(ProvenanceError::ReservedScope { path: id.0 });
        }
        Ok(id)
    }

    /// The canonical form: repo-root-relative, `/`-separated, no `.`/`..`
    /// segment. This is the value boundary matching consumes.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The id's first `/`-separated segment — the scope test, and nothing else.
    ///
    /// Private, and reading a *minted* value rather than raw input: by the time
    /// it is called the string is canonical, so "first segment" is unambiguous
    /// (there is no leading `./` or `//` left to make it two answers).
    fn first_segment(&self) -> &str {
        self.0.split('/').next().unwrap_or_default()
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

    // -----------------------------------------------------------------------
    // REQ-619 BR-3 — the home scope
    // -----------------------------------------------------------------------

    const HOME: &str = "/home/u";

    /// **REQ-619 BR-3 / ADR-619-3.** A file the registry discovered under the
    /// user's home mints `~/<home-relative>` — the spelling a user recognizes,
    /// and one that carries no absolute home layout into an event.
    ///
    /// The refusals are asserted beside it because they are what makes the
    /// constructor a *scope* rather than a string builder: a path outside the
    /// home has no home-relative identity and gets no fallback (the ADR-B
    /// posture `from_resolved` has), the home directory itself names no file,
    /// and an un-canonicalized remainder is refused rather than collapsed.
    ///
    /// **Mutations run, all confirmed red and restored:**
    /// 1. Prepend nothing (`Ok(relative)`) — this test goes red on the first
    ///    assertion, and so does
    ///    `the_repo_scope_refuses_a_leading_tilde_segment`'s disjointness half.
    /// 2. Format the raw remainder instead of minting it
    ///    (`format!("~/{}", rel.to_string_lossy())`) — the `Empty` leg goes
    ///    red, because a bare `~/` becomes an id for the home directory itself.
    /// 3. Fall back instead of refusing (`strip_prefix(home).unwrap_or(resolved)`)
    ///    — the `NotUnderRoot` leg goes red.
    #[test]
    fn a_home_resolved_file_mints_a_tilde_scoped_id() {
        let home = Path::new(HOME);

        let id = ProvenanceId::from_home_resolved(
            home,
            &PathBuf::from(HOME).join(".claude/skills/x/SKILL.md"),
        )
        .unwrap();
        assert_eq!(id.as_str(), "~/.claude/skills/x/SKILL.md");

        // A `commands/` row, the other user shape, and a nested one: the
        // separators survive, because boundary globs are separator-sensitive.
        assert_eq!(
            ProvenanceId::from_home_resolved(
                home,
                &PathBuf::from(HOME).join(".claude/commands/x.md")
            )
            .unwrap()
            .as_str(),
            "~/.claude/commands/x.md"
        );

        // Outside the home — a skills directory symlinked out of it, resolved
        // — there is no home-relative identity, and no fallback.
        let err =
            ProvenanceId::from_home_resolved(home, Path::new("/elsewhere/x/SKILL.md")).unwrap_err();
        assert!(matches!(err, ProvenanceError::NotUnderRoot { .. }), "{err}");

        // A sibling sharing the prefix bytes is a different directory.
        assert!(matches!(
            ProvenanceId::from_home_resolved(home, Path::new("/home/u-other/x")),
            Err(ProvenanceError::NotUnderRoot { .. })
        ));

        // The home directory itself names no file: `~` alone is not an id.
        assert!(matches!(
            ProvenanceId::from_home_resolved(home, home),
            Err(ProvenanceError::Empty)
        ));

        // An un-canonicalized remainder is refused, not collapsed — the same
        // rule the repo scope has, for the same symlink reason.
        assert!(matches!(
            ProvenanceId::from_home_resolved(home, &PathBuf::from(HOME).join("sub/../x")),
            Err(ProvenanceError::ParentTraversal { .. })
        ));

        // `.` and repeated separators are elided identically to the repo scope,
        // so one file has one home-scoped id whichever spelling reached it.
        assert_eq!(
            ProvenanceId::from_home_resolved(home, &PathBuf::from(HOME).join(".//a/./b.md"))
                .unwrap()
                .as_str(),
            "~/a/b.md"
        );
    }

    /// **REQ-619 BR-3 — the reservation, which is what makes the two scopes
    /// disjoint.**
    ///
    /// `mint` accepted a `~` segment before this REQ; nothing minted one. Now
    /// the home scope does, so the repo scope must stop — otherwise a
    /// repository containing a directory literally named `~` mints a string
    /// `from_home_resolved` also mints, and a boundary glob written for the
    /// user's skills directory silently becomes a verdict about a repository
    /// path (and the reverse).
    ///
    /// The precedence half matters as much as the refusal: the check sits
    /// **after** `strip_prefix`, so a path outside the root is still
    /// `NotUnderRoot` — the stronger, older statement — and a `..` remainder is
    /// still `ParentTraversal`.
    ///
    /// **Mutation:** delete the `first_segment() == HOME_SCOPE` check in
    /// `from_resolved` and this goes red on the first assertion (`/repo/~/x`
    /// mints `~/x`) and on the disjointness assertion. Confirmed red, then
    /// restored. Moving the check *above* the `strip_prefix` is not expressible
    /// (there is no remainder yet), which is why the ordering is stated in the
    /// doc rather than only asserted.
    #[test]
    fn the_repo_scope_refuses_a_leading_tilde_segment() {
        let err = ProvenanceId::from_resolved(&root(), &joined("~/x")).unwrap_err();
        assert!(
            matches!(&err, ProvenanceError::ReservedScope { path } if path == "~/x"),
            "{err}"
        );
        assert!(err.to_string().contains("reserved '~' segment"));

        // Normalization runs first, so every spelling of the reserved segment
        // is refused, not just the bare one.
        for spelling in ["./~/x", "~//x", "~/a/b.md", "~"] {
            assert!(
                matches!(
                    ProvenanceId::from_resolved(&root(), &joined(spelling)),
                    Err(ProvenanceError::ReservedScope { .. })
                ),
                "spelling {spelling:?} should be refused as the reserved scope"
            );
        }

        // Only the **first** segment is reserved: a `~` deeper in the path is an
        // ordinary directory name and still mints.
        assert_eq!(
            ProvenanceId::from_resolved(&root(), &joined("a/~/b.md"))
                .unwrap()
                .as_str(),
            "a/~/b.md"
        );
        // And a name that merely starts with `~` is not the segment.
        assert_eq!(
            ProvenanceId::from_resolved(&root(), &joined("~backup/x"))
                .unwrap()
                .as_str(),
            "~backup/x"
        );

        // Precedence: the older refusals still win where they apply.
        assert!(matches!(
            ProvenanceId::from_resolved(&root(), Path::new("/elsewhere/~/x")),
            Err(ProvenanceError::NotUnderRoot { .. })
        ));
        assert!(matches!(
            ProvenanceId::from_resolved(&root(), &joined("~/../x")),
            Err(ProvenanceError::ParentTraversal { .. })
        ));

        // The property the reservation buys: no repo path and no home path can
        // ever spell the same id, so one boundary verdict is about one file.
        let home_id =
            ProvenanceId::from_home_resolved(Path::new(HOME), &PathBuf::from(HOME).join("x"))
                .unwrap();
        assert_eq!(home_id.as_str(), "~/x");
        assert!(
            ProvenanceId::from_resolved(&root(), &joined("~/x")).is_err(),
            "the repo scope must not be able to produce {:?}",
            home_id.as_str()
        );
    }

    /// REQ-619 verify, M4: the `~` reservation belongs to the **set** of
    /// constructors, not to `from_resolved` alone.
    ///
    /// `claimed` is the assertion path — an MCP server naming the paths its
    /// tool touched, under keys the daemon does not control. Left as a bare
    /// `mint` it was the one constructor that could spell the home scope from
    /// a value nothing on this machine resolved, so a server could assert
    /// `~/.claude/skills/x/SKILL.md` and have its result tagged with the
    /// identity of a file the *user* installed. The refusal is what the caller
    /// already fails closed on (`mcp::client::collect_paths` marks the call
    /// unknown and records a `provenance_rejected` entry), so no new handling
    /// is needed — only the refusal itself.
    ///
    /// **Mutation (run, red, reverted):** restore `claimed`'s body to a bare
    /// `mint(source)` and `cargo test -p teton-core --lib` reports **exactly
    /// one** failure — this test, on its first assertion, where
    /// `~/.ssh/config` mints itself — with the other 362 green. That count is
    /// the finding: before this test the reservation had no coverage on the
    /// assertion path at all.
    #[test]
    fn the_assertion_path_refuses_the_home_scope_too() {
        let err = ProvenanceId::claimed("~/.ssh/config").unwrap_err();
        assert!(
            matches!(&err, ProvenanceError::ReservedScope { path } if path == "~/.ssh/config"),
            "{err}"
        );

        // Every spelling the normalizer folds into the reserved segment, since
        // the check reads the minted form and not the raw input.
        for spelling in ["~/x", "./~/x", "~//x", "~"] {
            assert!(
                matches!(
                    ProvenanceId::claimed(spelling),
                    Err(ProvenanceError::ReservedScope { .. }) | Err(ProvenanceError::Empty)
                ),
                "claimed({spelling:?}) should be refused"
            );
        }
        // `~` alone normalizes to the segment, not to nothing.
        assert!(matches!(
            ProvenanceId::claimed("~"),
            Err(ProvenanceError::ReservedScope { .. })
        ));

        // Benign twin, and the half that must NOT change: only the first
        // segment is reserved.
        assert_eq!(ProvenanceId::claimed("x/~").unwrap().as_str(), "x/~");
        assert_eq!(
            ProvenanceId::claimed("a/~/b.md").unwrap().as_str(),
            "a/~/b.md"
        );
        assert_eq!(
            ProvenanceId::claimed("~backup/x").unwrap().as_str(),
            "~backup/x"
        );
        assert_eq!(
            ProvenanceId::claimed("./secrets/prod.env")
                .unwrap()
                .as_str(),
            "secrets/prod.env"
        );

        // Precedence: the older refusals still win where they apply, so the new
        // check cannot mask a traversal or an absolute path.
        assert!(matches!(
            ProvenanceId::claimed("~/../x"),
            Err(ProvenanceError::ParentTraversal { .. })
        ));
        assert!(matches!(
            ProvenanceId::claimed("/~/x"),
            Err(ProvenanceError::Absolute { .. })
        ));
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
