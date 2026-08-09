//! The optional domain allowlist (REQ-563 BR-11).
//!
//! A host matcher and nothing else: exact names and `*.suffix` wildcards, tested
//! against a bare host. No scheme, no port, no path — the destination's *name*
//! is what BR-11 constrains, and a matcher that also had opinions about paths
//! would be a second, unstated policy hiding inside a list of domains.
//!
//! ## What this does not represent
//!
//! BR-11's `allowed_domains` has three distinct states, and only two of them are
//! a [`DomainAllowlist`]:
//!
//! | config | meaning | here |
//! |---|---|---|
//! | absent (`None`) | unrestricted; tier grants alone govern | **not representable** |
//! | `["example.com"]` | listed destinations only | [`DomainAllowlist`] with patterns |
//! | `[]` (present, empty) | the most restrictive posture: nothing is allowed | [`DomainAllowlist`] with none |
//!
//! The gap in that table is the design. "Unrestricted" is the *absence* of an
//! allowlist, and an empty allowlist is its opposite — the two are one keystroke
//! apart in a config file and diametrically opposed in effect, so collapsing
//! them into one type with a "matches everything when empty" rule would put the
//! inversion one careless refactor away. The caller holds `Option<DomainAllowlist>`
//! and `None` is the unrestricted case; an empty list here allows nothing,
//! because that is what it says.
//!
//! For the same reason there is **no `*` pattern**. A user who wants everything
//! removes the key.
//!
//! ## Wildcards match subdomains, never the apex
//!
//! `*.example.com` covers `docs.example.com` and `a.b.example.com`, and does not
//! cover `example.com`. That is the convention everywhere this syntax appears
//! (TLS certificates, cookie domains, proxy configs), and it is the fail-closed
//! reading: a user who means both lists both, and nobody is surprised into
//! granting the apex. The label ahead of the suffix must be non-empty, so a
//! degenerate `.example.com` does not slip through the suffix test.
//!
//! ## The suffix trick is the reason this is a module and not a `contains`
//!
//! `example.com.evil.net` ends with nothing useful, but it *starts* with
//! `example.com`, and `evil-example.com` *ends* with `example.com`. Both are
//! hosts an attacker controls, both look like the allowed name to a substring
//! test, and both are registrable today. The only comparisons here are
//! whole-string equality and `ends_with` on a **dot-anchored** suffix, which is
//! what makes those two cases fall out as non-matches rather than as
//! near-misses someone has to remember to check for.

use super::normalize_host;

/// The wildcard prefix a pattern uses to name "any subdomain of".
const WILDCARD_PREFIX: &str = "*.";

/// A host matcher built from BR-11's `allowed_domains` patterns.
///
/// Patterns are folded to their canonical host form once, at construction, so
/// the config's spelling (`Example.COM.`) and the destination's spelling
/// (`example.com`) meet in the middle rather than at every comparison.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DomainAllowlist {
    /// Exact host names.
    exact: Vec<String>,
    /// Dot-anchored suffixes, stored **with** the leading dot (`.example.com`)
    /// so matching is one `ends_with` and the anchor cannot be forgotten at a
    /// call site.
    suffixes: Vec<String>,
}

impl DomainAllowlist {
    /// Build an allowlist from configured patterns.
    ///
    /// A pattern that names nothing — the empty string, a bare `*`, a `*.` with
    /// no suffix — is dropped rather than treated as a wildcard. Silently
    /// widening on malformed input is how an allowlist stops being one; dropping
    /// it means the pattern simply matches nothing, which the user sees as a
    /// refusal naming the allowlist (BR-11) rather than as an unexpected fetch.
    #[must_use]
    pub fn new<S: AsRef<str>>(patterns: &[S]) -> Self {
        let mut exact = Vec::new();
        let mut suffixes = Vec::new();
        for pattern in patterns {
            let pattern = pattern.as_ref().trim();
            match pattern.strip_prefix(WILDCARD_PREFIX) {
                Some(suffix) => {
                    let suffix = normalize_host(suffix);
                    if !suffix.is_empty() {
                        suffixes.push(format!(".{suffix}"));
                    }
                }
                None => {
                    let host = normalize_host(pattern);
                    if !host.is_empty() && host != "*" {
                        exact.push(host);
                    }
                }
            }
        }
        Self { exact, suffixes }
    }

    /// Whether `host` is allowed.
    ///
    /// Takes a **bare host**, and specifically the one
    /// [`canonical_lookup_url`](super::canonical_lookup_url) reads — the
    /// transport's own parse. It must never be handed
    /// `user_urls_host_of`'s answer: that splitter is
    /// narrowing by design and disagrees with the socket on exactly the
    /// spellings an allowlist exists to catch. A value carrying a port or a
    /// scheme simply fails to match every pattern, which is the direction a
    /// matcher should fail in when it is handed something other than what it
    /// asked for.
    #[must_use]
    pub fn matches(&self, host: &str) -> bool {
        let host = normalize_host(host);
        if host.is_empty() {
            return false;
        }
        if self.exact.contains(&host) {
            return true;
        }
        self.suffixes.iter().any(|suffix| {
            // `>`, not `>=`: the label ahead of the dot must be non-empty, so
            // `.example.com` is not a subdomain of `example.com`.
            host.len() > suffix.len() && host.ends_with(suffix.as_str())
        })
    }

    /// Whether this allowlist permits nothing.
    ///
    /// True for `allowed_domains = []` — BR-11's most restrictive posture, and
    /// deliberately *not* the same as having no allowlist at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.exact.is_empty() && self.suffixes.is_empty()
    }

    /// The configured patterns in their canonical form, for the refusal message
    /// BR-11 requires ("refused with the allowlist named").
    ///
    /// Rebuilt rather than retained: the canonical spelling is what actually
    /// governs, so showing the user the raw config text would explain a refusal
    /// with a string the matcher never used.
    #[must_use]
    pub fn patterns(&self) -> Vec<String> {
        let mut out: Vec<String> = self.exact.clone();
        out.extend(
            self.suffixes
                .iter()
                .map(|suffix| format!("{WILDCARD_PREFIX}{}", &suffix[1..])),
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowlist(patterns: &[&str]) -> DomainAllowlist {
        DomainAllowlist::new(patterns)
    }

    #[test]
    fn an_exact_host_matches() {
        let list = allowlist(&["example.com", "docs.rs"]);
        assert!(list.matches("example.com"));
        assert!(list.matches("docs.rs"));
    }

    #[test]
    fn matching_folds_case_and_the_fqdn_root_dot_on_both_sides() {
        let list = allowlist(&["Example.COM."]);
        assert!(list.matches("example.com"));
        assert!(list.matches("EXAMPLE.com"));
        assert!(list.matches("example.com."));
    }

    #[test]
    fn a_wildcard_matches_subdomains_at_any_depth() {
        let list = allowlist(&["*.example.com"]);
        assert!(list.matches("docs.example.com"));
        assert!(list.matches("a.b.c.example.com"));
    }

    /// The convention this syntax carries everywhere else, pinned: a wildcard
    /// covers subdomains, not the name itself.
    #[test]
    fn a_wildcard_does_not_match_the_apex() {
        assert!(!allowlist(&["*.example.com"]).matches("example.com"));
        // Listing both is how a user asks for both.
        let both = allowlist(&["example.com", "*.example.com"]);
        assert!(both.matches("example.com"));
        assert!(both.matches("docs.example.com"));
    }

    /// The two registrable hosts that a substring test would hand to an
    /// attacker. Neither may match, under either pattern form.
    #[test]
    fn suffix_trick_hosts_are_refused() {
        let list = allowlist(&["example.com", "*.example.com"]);
        for host in [
            // ends with the allowed name, but on a different registrable domain
            "evil-example.com",
            "notexample.com",
            "xexample.com",
            // starts with the allowed name, but is a subdomain of the attacker's
            "example.com.evil.net",
            "example.com.evil.net.",
            // the allowed name as a label inside a hostile name
            "example-com.evil.net",
            "wwwexample.com",
        ] {
            assert!(!list.matches(host), "{host} must be refused");
        }
    }

    /// A degenerate empty label must not satisfy the dot-anchored suffix test.
    #[test]
    fn an_empty_label_is_not_a_subdomain() {
        let list = allowlist(&["*.example.com"]);
        assert!(!list.matches(".example.com"));
        assert!(!list.matches(""));
    }

    /// A sibling domain is not a match, and neither is a parent.
    #[test]
    fn sibling_and_parent_domains_are_refused() {
        let list = allowlist(&["docs.example.com"]);
        assert!(!list.matches("api.example.com"));
        assert!(!list.matches("example.com"));
        assert!(!list.matches("com"));
    }

    /// BR-11's "present but empty" state: the most restrictive posture, not the
    /// least. The absence of an allowlist is spelled by not having one at all.
    #[test]
    fn an_empty_allowlist_allows_nothing() {
        let list = allowlist(&[]);
        assert!(list.is_empty());
        assert!(!list.matches("example.com"));
        assert!(!list.matches("anything.at.all"));
    }

    /// There is no "allow everything" pattern — a bare `*` is dropped, not
    /// promoted to a universal wildcard.
    #[test]
    fn a_bare_star_is_not_a_universal_wildcard() {
        let list = allowlist(&["*"]);
        assert!(list.is_empty());
        assert!(!list.matches("example.com"));
    }

    #[test]
    fn patterns_that_name_nothing_are_dropped() {
        let list = allowlist(&["", "   ", "*.", "*. ", "."]);
        assert!(list.is_empty());
    }

    /// The matcher constrains the name only. A value that is not a bare host
    /// fails closed rather than being parsed into one here.
    #[test]
    fn a_value_carrying_a_port_or_scheme_is_not_a_host_and_does_not_match() {
        let list = allowlist(&["example.com"]);
        assert!(!list.matches("example.com:8443"));
        assert!(!list.matches("https://example.com"));
        assert!(!list.matches("example.com/path"));
    }

    /// The refusal message has to name what actually governs, so the canonical
    /// spelling is what comes back.
    #[test]
    fn patterns_read_back_in_canonical_form() {
        let list = allowlist(&["Example.COM.", "*.Docs.RS"]);
        assert_eq!(list.patterns(), vec!["example.com", "*.docs.rs"]);
    }

    /// Built from what the config actually holds (`Vec<String>`), not only from
    /// string literals.
    #[test]
    fn construction_accepts_the_configs_owned_strings() {
        let configured = vec!["example.com".to_owned(), "*.example.org".to_owned()];
        let list = DomainAllowlist::new(&configured);
        assert!(list.matches("example.com"));
        assert!(list.matches("www.example.org"));
        assert!(!list.matches("example.org"));
    }
}
