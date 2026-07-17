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
        PrivacyBoundary {
            path_glob: glob.to_owned(),
            mode,
        }
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
        assert!(m.match_path("/etc/secrets/x").is_none());
        assert!(m.match_path("../secrets/x").is_none());
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
