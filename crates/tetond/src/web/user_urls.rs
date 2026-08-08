//! The per-session set of URLs the **user** pasted (REQ-563 BR-3, D-4).
//!
//! BR-3 defines a user-pasted URL precisely: one that "appeared verbatim in a
//! user message of the current session — not one the model composed". That
//! sentence is the whole of this module. [`UserUrls`] is fed from each user
//! prompt as it arrives and answers exactly one question afterwards: was this
//! destination the user's idea?
//!
//! Two gates depend on the answer, and they lean opposite ways:
//!
//! - The `fetch_user_url` tier grants *only* destinations this set contains, so
//!   a false positive here hands the floor tier the reach of the tier above it.
//! - The taint split (BR-13) lets a tainted session fetch a user-pasted URL,
//!   because the user authored the bytes that would egress. A false positive
//!   there lets a model-composed destination inherit that exemption.
//!
//! Both failures are the same failure — a URL the model chose being treated as
//! one the user chose — so every judgement call in this module resolves toward
//! *not* finding a match. The consequence of being too strict is a permission
//! prompt the user answers; the consequence of being too loose is a gate that
//! silently does not apply.
//!
//! Membership is by [`normalize_url`], the same canonicalization the cache keys
//! on, which is where the "verbatim" in BR-3 is given its exact meaning: the
//! same scheme, the same host, and a byte-identical path and query.

use std::collections::HashSet;

use super::normalize_url;

/// Characters that end a URL run in the extractor.
///
/// Wider than RFC 3986 strictly requires. `"`, `'`, `<`, `>` and a backtick can
/// all appear in a percent-encoded URL, but every one of them is far more likely
/// to be the *container* — an HTML attribute, a markdown code span, a quoted
/// paste — than a character the user meant to include. Stopping early yields a
/// URL that will not match the one the model later asks for, and a non-match is
/// this module's safe answer; running long past a closing quote yields the same
/// non-match with a longer string. Neither direction can manufacture a match, so
/// the set is chosen for the common paste rather than for the standard.
const TERMINATORS: [char; 5] = ['"', '\'', '<', '>', '`'];

/// Extract the URL-shaped runs from a block of user-authored text, in the order
/// they appear.
///
/// Deliberately crude: an `http://` or `https://` (case-insensitively — a paste
/// from a mail client can arrive shouting) followed by everything up to the next
/// whitespace or [`TERMINATORS`] character. No validation, no repair, no
/// trailing-punctuation trimming.
///
/// The trailing punctuation choice is worth naming, because it is the one that
/// looks like an oversight: a URL ending a sentence records **with** its full
/// stop, and therefore does not match the same URL without it. That is a lookup
/// the user is prompted for rather than one that is auto-granted — the safe
/// direction — whereas a trimmer would have to decide that a `.` or `)` was not
/// part of the path, and a path really can end in either. The extractor does not
/// get to make that call on the user's behalf.
///
/// Returned strings are **verbatim**, not normalized: normalization happens at
/// insertion and at lookup, so the raw run stays available for anything that
/// needs to echo back what the user actually typed.
#[must_use]
pub fn extract_urls(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let lowered = text.to_ascii_lowercase();
    let mut cursor = 0usize;
    while cursor < text.len() {
        // Searched in the lowercased copy so the scheme match is
        // case-insensitive; the two strings share byte offsets because
        // `to_ascii_lowercase` only ever swaps ASCII bytes in place.
        let Some(offset) = lowered[cursor..].find("http") else {
            break;
        };
        let start = cursor + offset;
        let rest = &text[start..];
        let scheme_len = if lowered[start..].starts_with("https://") {
            8
        } else if lowered[start..].starts_with("http://") {
            7
        } else {
            cursor = start + 4;
            continue;
        };
        let end = rest
            .find(|c: char| c.is_whitespace() || TERMINATORS.contains(&c))
            .unwrap_or(rest.len());
        // A bare `https://` with no authority is not a URL; skip it rather than
        // record a string nothing can ever match.
        if end > scheme_len {
            found.push(rest[..end].to_owned());
        }
        cursor = start + end.max(scheme_len);
    }
    found
}

/// The URLs a session's user has pasted, held normalized.
///
/// Session-scoped and in memory only: this is evidence about *this*
/// conversation, and persisting it would turn a per-session authorization into a
/// standing one — the same reason permission grants are never written to disk
/// (`harness::permissions`).
#[derive(Debug, Clone, Default)]
pub struct UserUrls {
    normalized: HashSet<String>,
}

impl UserUrls {
    /// An empty set — a fresh session has pasted nothing, so `fetch_user_url`
    /// grants nothing until the user speaks.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record every URL in one user message, returning how many *new* ones it
    /// contributed.
    ///
    /// Call this with user-authored text only. Feeding it a model turn, a tool
    /// result, or a fetched page would let the model author its own
    /// authorization by writing a URL into content this set then trusts — which
    /// is precisely the "not one the model composed" clause, defeated in one
    /// call site.
    pub fn record_user_message(&mut self, text: &str) -> usize {
        extract_urls(text)
            .into_iter()
            .filter(|url| self.insert(url))
            .count()
    }

    /// Record a single URL as user-pasted. Returns whether it was new.
    ///
    /// Same caution as [`UserUrls::record_user_message`]: the caller is
    /// asserting the user authored this.
    pub fn insert(&mut self, url: &str) -> bool {
        self.normalized.insert(normalize_url(url))
    }

    /// Whether `url` appeared verbatim in a user message of this session
    /// (BR-3).
    #[must_use]
    pub fn contains(&self, url: &str) -> bool {
        self.normalized.contains(&normalize_url(url))
    }

    /// How many distinct URLs the user has pasted.
    #[must_use]
    pub fn len(&self) -> usize {
        self.normalized.len()
    }

    /// Whether the user has pasted no URLs at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.normalized.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_pasted_in_a_user_prompt_is_found_verbatim() {
        let mut urls = UserUrls::new();
        urls.record_user_message("please read https://example.com/docs/page?x=1 and summarize");
        assert!(urls.contains("https://example.com/docs/page?x=1"));
    }

    /// Normalization's job, from this side: the user's spelling and the model's
    /// spelling of *the same* destination must agree.
    #[test]
    fn the_match_survives_scheme_case_host_case_and_a_fragment() {
        let mut urls = UserUrls::new();
        urls.insert("HTTPS://Example.COM/docs?x=1");
        assert!(urls.contains("https://example.com/docs?x=1"));
        assert!(urls.contains("https://example.com/docs?x=1#section"));
    }

    /// BR-3's load-bearing negative: a URL the model composed by altering the
    /// pasted one is not the pasted one.
    #[test]
    fn a_url_differing_in_host_path_or_query_is_not_found() {
        let mut urls = UserUrls::new();
        urls.insert("https://example.com/docs/page?x=1");
        for composed in [
            // host
            "https://evil.com/docs/page?x=1",
            "https://sub.example.com/docs/page?x=1",
            "https://example.com.evil.net/docs/page?x=1",
            // path
            "https://example.com/docs/other?x=1",
            "https://example.com/docs/page/?x=1",
            "https://example.com/DOCS/page?x=1",
            // query
            "https://example.com/docs/page?x=2",
            "https://example.com/docs/page",
            "https://example.com/docs/page?x=1&y=2",
            // scheme
            "http://example.com/docs/page?x=1",
        ] {
            assert!(
                !urls.contains(composed),
                "{composed} must not inherit the pasted URL's authorization"
            );
        }
    }

    #[test]
    fn a_fresh_session_has_pasted_nothing() {
        let urls = UserUrls::new();
        assert!(urls.is_empty());
        assert_eq!(urls.len(), 0);
        assert!(!urls.contains("https://example.com/"));
    }

    #[test]
    fn recording_reports_only_newly_seen_urls() {
        let mut urls = UserUrls::new();
        assert_eq!(
            urls.record_user_message("https://a.test/1 https://b.test/2"),
            2
        );
        // The same two, one respelled — normalization means neither is new.
        assert_eq!(
            urls.record_user_message("HTTPS://A.test/1 again, and https://b.test/2#x"),
            0
        );
        assert_eq!(urls.len(), 2);
    }

    #[test]
    fn the_extractor_finds_urls_in_order_and_stops_at_whitespace() {
        assert_eq!(
            extract_urls("see https://a.test/one then http://b.test/two now"),
            vec!["https://a.test/one", "http://b.test/two"]
        );
    }

    #[test]
    fn the_extractor_stops_at_quotes_angle_brackets_and_backticks() {
        assert_eq!(
            extract_urls("<a href=\"https://a.test/x\">link</a>"),
            vec!["https://a.test/x"]
        );
        assert_eq!(extract_urls("`https://b.test/y`"), vec!["https://b.test/y"]);
        assert_eq!(extract_urls("'https://c.test/z'"), vec!["https://c.test/z"]);
        assert_eq!(extract_urls("<https://d.test/w>"), vec!["https://d.test/w"]);
    }

    #[test]
    fn the_extractor_accepts_a_shouted_scheme() {
        assert_eq!(
            extract_urls("HTTPS://Example.com/A"),
            vec!["HTTPS://Example.com/A"],
            "the run is returned verbatim; normalization happens on insert"
        );
    }

    /// Only `http`/`https`. A `file://`, `ftp://`, or bare `http` word is not a
    /// destination this feature fetches.
    #[test]
    fn the_extractor_ignores_other_schemes_and_bare_words() {
        assert!(extract_urls("file:///etc/passwd").is_empty());
        assert!(extract_urls("ftp://a.test/x").is_empty());
        assert!(extract_urls("we use http and https daily").is_empty());
        assert!(extract_urls("https:// nothing here").is_empty());
    }

    /// The documented consequence of not trimming trailing punctuation, pinned
    /// so a later "helpful" trimmer has to argue with a test.
    #[test]
    fn a_url_ending_a_sentence_records_with_its_punctuation() {
        assert_eq!(
            extract_urls("read https://a.test/page."),
            vec!["https://a.test/page."]
        );
        let mut urls = UserUrls::new();
        urls.record_user_message("read https://a.test/page.");
        assert!(
            !urls.contains("https://a.test/page"),
            "the trimmed URL is not the pasted one — a prompt, not a silent grant"
        );
    }

    /// Multi-byte text around a URL must not desynchronize the scan: the
    /// lowercased copy is searched but the original is sliced.
    #[test]
    fn extraction_survives_multi_byte_text_around_the_url() {
        assert_eq!(
            extract_urls("día — https://a.test/ñ — fin"),
            vec!["https://a.test/ñ"]
        );
    }

    /// A run of `http` that never becomes a URL must advance the cursor, or the
    /// scan spins.
    #[test]
    fn repeated_non_url_http_runs_terminate() {
        assert!(extract_urls("http http http httphttp").is_empty());
    }
}
