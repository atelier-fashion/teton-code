//! Pure privacy-boundary matching.
//!
//! Maps a repo-relative path to the [`PrivacyBoundary`] that governs it, if
//! any. This is the classification step that the daemon's single egress choke
//! point uses to enforce BR-1 — but this module makes **no** egress decision
//! itself; it only answers "which boundary, if any, covers this path?".
//!
//! Semantics:
//! - Globs use gitignore-like separator rules: `*` does not cross `/`, `**`
//!   does. So `secrets/**` covers `secrets/a` and `secrets/a/b` but not a file
//!   literally named `secrets`.
//! - Matching is **case-sensitive** — `Secrets/x` does not match `secrets/**`.
//! - **Declaration order is precedence**: when several boundaries match a path,
//!   the earliest one in the slice wins. Order your most-specific/strictest
//!   rules first.
//! - Paths outside the repo (absolute, or containing `..`) simply match no
//!   repo-relative glob and return `None`. Matching never panics on any input.

use crate::entities::PrivacyBoundary;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

/// A compile error for one boundary's glob pattern.
#[derive(Debug, thiserror::Error)]
#[error("privacy boundary glob '{glob}' is not a valid pattern: {source}")]
pub struct BoundaryError {
    /// The offending glob (safe to surface — it is user-authored config, not a
    /// secret).
    pub glob: String,
    /// The underlying globset parse error.
    #[source]
    pub source: globset::Error,
}

/// A pre-compiled set of privacy boundaries for repeated, allocation-light
/// matching. Build once (e.g. when the daemon loads config), then call
/// [`BoundaryMatcher::match_path`] on the hot egress path.
#[derive(Debug)]
pub struct BoundaryMatcher<'a> {
    boundaries: &'a [PrivacyBoundary],
    set: GlobSet,
}

impl<'a> BoundaryMatcher<'a> {
    /// Compile the globs of `boundaries`. Returns the first invalid glob as an
    /// error so misconfigured boundaries surface at load time, not silently at
    /// egress.
    pub fn new(boundaries: &'a [PrivacyBoundary]) -> Result<Self, BoundaryError> {
        let mut builder = GlobSetBuilder::new();
        for b in boundaries {
            let glob = GlobBuilder::new(&b.path_glob)
                .literal_separator(true)
                .build()
                .map_err(|source| BoundaryError {
                    glob: b.path_glob.clone(),
                    source,
                })?;
            builder.add(glob);
        }
        // Building the set from already-parsed globs is infallible in practice,
        // but propagate any error rather than unwrap.
        let set = builder.build().map_err(|source| BoundaryError {
            glob: String::new(),
            source,
        })?;
        Ok(Self { boundaries, set })
    }

    /// Return the governing boundary for `path`, or `None` if no boundary
    /// covers it. When multiple boundaries match, the earliest in declaration
    /// order wins.
    #[must_use]
    pub fn match_path(&self, path: &str) -> Option<&'a PrivacyBoundary> {
        let normalized = normalize(path);
        self.set
            .matches(normalized)
            .into_iter()
            .min()
            .map(|i| &self.boundaries[i])
    }
}

/// Convenience one-shot: compile `boundaries` and match `path` in a single
/// call. Returns `None` if the boundaries fail to compile — prefer
/// [`BoundaryMatcher::new`] when you want compile errors surfaced, or when
/// matching many paths against the same set.
#[must_use]
pub fn match_boundary<'a>(
    path: &str,
    boundaries: &'a [PrivacyBoundary],
) -> Option<&'a PrivacyBoundary> {
    BoundaryMatcher::new(boundaries).ok()?.match_path(path)
}

/// Strip a single leading `./` so `./secrets/x` and `secrets/x` are equivalent.
/// Everything else is left untouched; absolute or `..`-bearing paths are simply
/// not repo-relative and will match no repo-relative glob.
fn normalize(path: &str) -> &str {
    path.strip_prefix("./").unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::BoundaryMode;

    fn boundary(glob: &str, mode: BoundaryMode) -> PrivacyBoundary {
        PrivacyBoundary::user(glob, mode)
    }

    #[test]
    fn matches_nested_files_under_a_recursive_glob() {
        let bs = vec![boundary("secrets/**", BoundaryMode::LocalOnly)];
        let m = BoundaryMatcher::new(&bs).unwrap();
        assert!(m.match_path("secrets/prod.env").is_some());
        assert!(m.match_path("secrets/aws/keys.json").is_some());
        assert!(m.match_path("secrets/a/b/c/deep.txt").is_some());
    }

    #[test]
    fn single_star_does_not_cross_a_slash() {
        let bs = vec![boundary("secrets/*", BoundaryMode::LocalOnly)];
        let m = BoundaryMatcher::new(&bs).unwrap();
        assert!(m.match_path("secrets/prod.env").is_some());
        // A nested file must NOT match a single-star glob.
        assert!(m.match_path("secrets/aws/keys.json").is_none());
    }

    #[test]
    fn non_matching_paths_return_none() {
        let bs = vec![boundary("secrets/**", BoundaryMode::LocalOnly)];
        let m = BoundaryMatcher::new(&bs).unwrap();
        assert!(m.match_path("src/main.rs").is_none());
        // A file literally named `secrets` is not under `secrets/**`.
        assert!(m.match_path("secrets").is_none());
    }

    #[test]
    fn matching_is_case_sensitive() {
        let bs = vec![boundary("secrets/**", BoundaryMode::LocalOnly)];
        let m = BoundaryMatcher::new(&bs).unwrap();
        assert!(m.match_path("secrets/x").is_some());
        assert!(m.match_path("Secrets/x").is_none());
        assert!(m.match_path("SECRETS/x").is_none());
    }

    #[test]
    fn nested_globs_resolve_by_declaration_order() {
        // `secrets/vendor/x` matches both globs; declaration order decides.
        let broad_first = vec![
            boundary("secrets/**", BoundaryMode::LocalOnly),
            boundary("secrets/vendor/**", BoundaryMode::RedactThenRemote),
        ];
        let m = BoundaryMatcher::new(&broad_first).unwrap();
        assert_eq!(
            m.match_path("secrets/vendor/x").unwrap().mode,
            BoundaryMode::LocalOnly,
            "earliest declared boundary should win"
        );

        // Reversing precedence flips the winner — proves order is load-bearing.
        let specific_first = vec![
            boundary("secrets/vendor/**", BoundaryMode::RedactThenRemote),
            boundary("secrets/**", BoundaryMode::LocalOnly),
        ];
        let m2 = BoundaryMatcher::new(&specific_first).unwrap();
        assert_eq!(
            m2.match_path("secrets/vendor/x").unwrap().mode,
            BoundaryMode::RedactThenRemote
        );
    }

    /// **REQ-571 BR-8 — retained, and paired.**
    ///
    /// What this test says is true of the matcher and only of the matcher: an
    /// out-of-repo *string* matches no repo-relative glob. It is deliberately
    /// **not** a claim that a boundary file is safe when named that way — the
    /// opposite, in fact. `secrets/**` returning `None` for
    /// `/abs/root/secrets/prod.env` is what made a non-canonical spelling
    /// invisible to enforcement before REQ-571 (BR-9).
    ///
    /// The reason that is no longer reachable lives one layer up, in the tools:
    /// `ToolContext::resolve` canonicalizes a path to its repo-relative form
    /// before any identity is minted, so a spelling like the ones below never
    /// becomes a `ProvenanceId` and never arrives here. Each assertion is
    /// therefore paired with the tool-layer test that proves it, named inline
    /// below.
    ///
    /// **Do not delete either half alone.** Without the assertion, nothing
    /// states what the matcher does with such a string; without the twin,
    /// nothing states that the tool layer keeps it hypothetical.
    /// `crates/tetond/tests/boundary_coverage.rs`
    /// (`each_out_of_repo_matcher_assertion_is_paired_with_a_tool_layer_test`)
    /// fails if this test, either assertion, or the names below go missing.
    #[test]
    fn out_of_repo_paths_never_match_and_never_panic() {
        let bs = vec![boundary("secrets/**", BoundaryMode::LocalOnly)];
        let m = BoundaryMatcher::new(&bs).unwrap();
        // Absolute paths, parent-relative escapes, home-relative, empty, and
        // odd separators must all return None without panicking.
        for p in [
            "/etc/passwd",
            "/Users/someone/secrets/x", // absolute — not repo-relative
            "../secrets/x",
            "../../secrets/leak",
            "~/secrets/x",
            "",
            "   ",
            "secrets\\windows\\style",
            "./secrets", // normalizes to `secrets`, still not under secrets/**
        ] {
            let _ = m.match_path(p); // must not panic
        }
        // Absolute. Tool-layer twins — an absolute in-root path is canonicalized
        // before it is minted, so this spelling never reaches the matcher:
        //   tetond/tests/egress_capture.rs
        //     read_blocks_every_boundary_spelling_under_one_identity
        //     edit_blocks_every_boundary_spelling_under_one_identity
        //   tetond/tests/provenance_egress.rs
        //     a_session_tainted_by_an_absolute_spelling_reaches_the_pin_and_closes_the_web
        assert!(m.match_path("/etc/secrets/x").is_none());
        // `..`-bearing. Tool-layer twins — a `..` component is either resolved
        // away inside the root or refused by the jail, so no `..`-bearing string
        // survives to become an identity:
        //   tetond/tests/egress_capture.rs
        //     read_blocks_every_boundary_spelling_under_one_identity
        //     edit_blocks_every_boundary_spelling_under_one_identity
        //   tetond/tests/provenance_egress.rs
        //     a_session_tainted_by_a_traversing_spelling_reaches_the_pin_and_closes_the_web
        assert!(m.match_path("../secrets/x").is_none());
    }

    /// **REQ-619 BR-3 / ADR-619-3 — the claim the `~` scope rests on, pinned
    /// against the real `globset` rather than reasoned about.**
    ///
    /// ADR-619-3 chose the ordinary path spelling (`~/.claude/skills/x/SKILL.md`)
    /// for a user skill's identity instead of inventing a glob language for the
    /// user scope, and that choice is only sound if two things are true of this
    /// matcher, neither of which is obvious by inspection:
    ///
    /// 1. `~` is an ordinary character to `globset`, and every builtin is
    ///    `**/`-prefixed (matching zero or more leading directories), so a user
    ///    who writes `**/.claude/skills/**` covers their skills directory and
    ///    `**/.ssh/**` — a *shipped* builtin — covers `~/.ssh/config`. The
    ///    second is REQ-619's out-of-root boundary touch: LESSON-623 says a
    ///    glob cannot protect a path the provenance seam never names, and this
    ///    is the other half — the seam now names it, and the glob reaches it.
    /// 2. **None** of the thirteen `DEFAULT_BOUNDARIES` matches a user skill
    ///    file, which is what makes AC-1 ("a user skill routes remote by
    ///    default") true of the shipped set rather than an assumption about it.
    ///    Asserted against `config::DEFAULT_BOUNDARIES` itself, so adding a
    ///    fourteenth glob that happens to cover `.claude/**` fails here.
    ///
    /// Note the deliberate asymmetry with
    /// `out_of_repo_paths_never_match_and_never_panic`, which lists `~/secrets/x`
    /// among the strings that reach no *repo-relative* glob: `secrets/**` is
    /// unanchored at the head, so it genuinely does not match `~/secrets/x`.
    /// That test says what a repo-scope glob does with a home-scope id; this one
    /// says a `**/`-prefixed glob is how you reach one. Both are true, and a
    /// user who wants their skills covered writes the `**/` form.
    ///
    /// **Mutations run, both confirmed red and restored:**
    /// 1. Make [`normalize`] strip a leading `~/` as it strips `./` — the home
    ///    scope collapses into the repo scope, and the first leg goes red (the
    ///    repo-anchored `.claude/skills/**` starts matching a user skill).
    ///    This is the mutation that matters: it is the plausible "helpful"
    ///    edit, and it is exactly what BR-3's disjointness forbids.
    /// 2. Replace one shipped glob with `**/.claude/**` — the third leg goes
    ///    red, so the "no builtin covers a user skill" claim is a live
    ///    assertion about the constant and not a restatement of it.
    #[test]
    fn a_tilde_scoped_id_is_matched_by_a_user_glob_and_by_no_builtin() {
        const USER_SKILL: &str = "~/.claude/skills/x/SKILL.md";
        const USER_COMMAND: &str = "~/.claude/commands/x.md";

        // 1. A user who wants their skills directory covered writes the
        //    ordinary `**/`-prefixed glob, and it reaches the `~` spelling.
        let covering = vec![boundary(".claude/skills/**", BoundaryMode::LocalOnly)];
        assert!(
            BoundaryMatcher::new(&covering)
                .unwrap()
                .match_path(USER_SKILL)
                .is_none(),
            "a repo-anchored glob does not reach a home-scoped id — the `**/` form is the one to write"
        );
        let covering = vec![boundary("**/.claude/skills/**", BoundaryMode::LocalOnly)];
        assert!(
            BoundaryMatcher::new(&covering)
                .unwrap()
                .match_path(USER_SKILL)
                .is_some(),
            "`**/` matches the leading `~` directory, so the ordinary spelling needs no new glob language"
        );

        // 2. And a *shipped* builtin reaches an out-of-root credential path in
        //    the same spelling — REQ-619's boundary-touch case.
        let ssh = vec![boundary("**/.ssh/**", BoundaryMode::LocalOnly)];
        assert!(BoundaryMatcher::new(&ssh)
            .unwrap()
            .match_path("~/.ssh/config")
            .is_some());

        // 3. The default set covers no user skill, which is what AC-1 rests on.
        let defaults: Vec<PrivacyBoundary> = crate::config::DEFAULT_BOUNDARIES
            .iter()
            .map(|glob| PrivacyBoundary::builtin(*glob))
            .collect();
        assert_eq!(defaults.len(), 13, "the shipped set is thirteen globs");
        let m = BoundaryMatcher::new(&defaults).unwrap();
        for path in [USER_SKILL, USER_COMMAND] {
            assert!(
                m.match_path(path).is_none(),
                "no builtin may cover {path:?}, or every user skill pins every session \
                 on every machine — the defect REQ-619 exists to end"
            );
        }
        // The control that gives that teeth: the same set does cover the
        // credential shapes, in the same `~` spelling.
        assert!(m.match_path("~/.ssh/id_rsa").is_some());
        assert!(m.match_path("~/.aws/credentials").is_some());
    }

    #[test]
    fn leading_dot_slash_is_normalized() {
        let bs = vec![boundary("secrets/**", BoundaryMode::LocalOnly)];
        let m = BoundaryMatcher::new(&bs).unwrap();
        assert!(m.match_path("./secrets/prod.env").is_some());
    }

    #[test]
    fn empty_boundaries_match_nothing() {
        let bs: Vec<PrivacyBoundary> = Vec::new();
        let m = BoundaryMatcher::new(&bs).unwrap();
        assert!(m.match_path("anything/at/all").is_none());
    }

    #[test]
    fn convenience_fn_matches_like_the_matcher() {
        let bs = vec![boundary("private/**", BoundaryMode::LocalOnly)];
        assert!(match_boundary("private/notes.md", &bs).is_some());
        assert!(match_boundary("public/notes.md", &bs).is_none());
    }

    #[test]
    fn invalid_glob_is_reported_with_its_pattern() {
        let bs = vec![boundary("secrets/[unterminated", BoundaryMode::LocalOnly)];
        let err = BoundaryMatcher::new(&bs).unwrap_err();
        assert!(err.glob.contains("unterminated"));
        assert!(err.to_string().contains("not a valid pattern"));
    }
}
