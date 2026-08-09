//! Opt-in web lookup — the daemon-side support pieces (REQ-563).
//!
//! This module holds the parts of web lookup that are **pure or purely local**:
//! nothing here opens a socket, and nothing here decides whether a lookup is
//! allowed. Egress belongs to the choke point (architecture D-2) and the
//! tier/consent/taint gates belong beside it (D-4, D-5); what lives here is the
//! machinery those gates and that seam are built out of.
//!
//! ## Module map
//! - [`cache`] — [`WebCache`]: the on-machine, content-addressed document cache
//!   (BR-12). A fresh hit is served with **zero egress**, which is why the cache
//!   is a support piece rather than part of the transport.
//! - [`reduce`] — [`reduce()`]: the deterministic, dependency-free HTML→text
//!   extractor (D-3, BR-10). Condensation beyond "readable text" rides the
//!   existing local-pinned `summarize_if_large`, so raw page bytes never reach a
//!   remote model by construction.
//! - [`user_urls`] — [`UserUrls`]: the per-session set of URLs that appeared
//!   **verbatim in a user message**, which is BR-3's definition of a user-pasted
//!   URL and therefore the difference between the `fetch_user_url` tier and the
//!   `fetch_any_url` one.
//! - [`allowlist`] — [`DomainAllowlist`]: BR-11's host matcher, constraining
//!   model-chosen destinations only.
//!
//! ## One normalization, two readers
//!
//! [`normalize_url`] is the single definition of "the same URL", and it is
//! deliberately shared by the two places that ask that question for different
//! reasons: the cache (is this document already on disk?) and [`UserUrls`] (did
//! the user actually paste this?). Two definitions would drift, and the drift
//! has a direction that matters — a normalizer that is *looser* for the
//! user-URL check than for the cache key would let a model-composed URL that
//! merely resembles a pasted one inherit the pasted one's authorization, which
//! is exactly the substitution BR-3's "not one the model composed" forbids.
//!
//! Its rules are therefore few and all *narrowing-safe*: lowercase the scheme
//! and the host (DNS is case-insensitive; the wire is not), drop the fragment
//! (never sent to a server, so it cannot name a different document), and leave
//! path and query **byte-exact**. Everything an equivalence table might add —
//! collapsing a default port, adding an empty path's `/`, sorting query
//! parameters, percent-encoding case — would *widen* the set of URLs that count
//! as "the one the user pasted". Each is individually defensible and all of them
//! together are a hole, so none are here. The cost is a cache miss and an
//! occasional refusal to treat a trivially-different spelling as pasted; both
//! fail in the direction of doing less.
//!
//! ## …and exactly one parser for every gate
//!
//! That narrowing bias is safe for "have I seen this before" and unsafe for
//! "where would this go". [`normalize_url`] and `user_urls_host_of` answer the
//! first question and are doc-fenced against the second; [`canonical_lookup_url`]
//! answers the second, using `reqwest::Url` — the parser the transport itself
//! uses — and returns the URL **re-serialized**, so the string that is checked,
//! the string shown at the consent prompt, and the string that goes on the wire
//! are one string. Anything else admits a spelling
//! (`https://evil.example\@allowed.example/x`) whose host reads one way to a
//! splitter and another way to a socket.

pub mod allowlist;
pub mod cache;
pub mod reduce;
pub mod user_urls;

pub use allowlist::DomainAllowlist;
pub use cache::{CacheEntry, CacheError, WebCache, CACHE_DIR_NAME};
pub use reduce::{reduce, REDUCED_MAX_BYTES};
pub use user_urls::{extract_urls, UserUrls};

/// A URL decomposed far enough to normalize it, and no further.
///
/// Not a URL type: it exists so [`normalize_url`] and `user_urls_host_of`
/// locate the authority the *same* way. A second parser here would be the same
/// drift hazard the module doc describes, one level down.
struct Split<'a> {
    /// The scheme, as written (lowercased by the caller).
    scheme: &'a str,
    /// `user:password` ahead of an `@`, when present. Carried through
    /// **byte-exact**: userinfo is credential-shaped and case-sensitive, and
    /// folding its case would make two different credentials look like one URL.
    userinfo: Option<&'a str>,
    /// `host` or `host:port`, as written.
    host_port: &'a str,
    /// Path and query, as written, with the fragment already gone.
    rest: &'a str,
}

/// Split `url` into scheme / userinfo / host:port / path+query, or `None` when
/// it is not a `scheme://…` URL at all.
///
/// The fragment is dropped here rather than by each caller: it is the one
/// component the wire never carries, so no reader downstream has any use for it.
fn split(url: &str) -> Option<Split<'_>> {
    let trimmed = url.trim();
    // A `#` cannot appear unescaped in a path or query, so the first one is
    // unambiguously the fragment delimiter.
    let no_fragment = match trimmed.find('#') {
        Some(at) => &trimmed[..at],
        None => trimmed,
    };
    let (scheme, after) = no_fragment.split_once("://")?;
    // RFC 3986: a scheme starts with a letter and continues with letters,
    // digits, `+`, `-`, `.`. Checked so that a bare `foo://` inside prose is not
    // mistaken for a URL — and so `normalize_url` can fall through to its
    // leave-it-alone arm rather than inventing structure that is not there.
    let mut scheme_chars = scheme.chars();
    if !scheme_chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        || !scheme_chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return None;
    }
    let authority_end = after.find(['/', '?']).unwrap_or(after.len());
    let (authority, rest) = after.split_at(authority_end);
    // `rsplit_once`, not `split_once`: an `@` may legally appear inside the
    // userinfo, and the *last* one is the authority delimiter.
    let (userinfo, host_port) = match authority.rsplit_once('@') {
        Some((userinfo, host)) => (Some(userinfo), host),
        None => (None, authority),
    };
    if host_port.is_empty() {
        return None;
    }
    Some(Split {
        scheme,
        userinfo,
        host_port,
        rest,
    })
}

/// The canonical spelling of `url` for equality purposes (REQ-563 BR-3, BR-12).
///
/// Lowercases the scheme and the host, drops the fragment, and leaves path and
/// query byte-exact — see the module doc for why the rule list stops there.
///
/// A string this cannot decompose (no `scheme://`, an empty host) is returned
/// trimmed and de-fragmented but otherwise **unchanged**. That is not a fallback
/// so much as an admission: with no authority to find there is nothing to
/// canonicalize, and guessing would manufacture equality between strings that
/// were never shown to name the same place. Such a value still keys a cache
/// entry and still compares in [`UserUrls`]; it simply compares as itself.
#[must_use]
pub fn normalize_url(url: &str) -> String {
    let Some(parts) = split(url) else {
        let trimmed = url.trim();
        return match trimmed.find('#') {
            Some(at) => trimmed[..at].to_owned(),
            None => trimmed.to_owned(),
        };
    };
    let mut out = String::with_capacity(url.len());
    out.push_str(&parts.scheme.to_ascii_lowercase());
    out.push_str("://");
    if let Some(userinfo) = parts.userinfo {
        out.push_str(userinfo);
        out.push('@');
    }
    out.push_str(&parts.host_port.to_ascii_lowercase());
    out.push_str(parts.rest);
    out
}

/// Fold a host to its canonical form: ASCII-lowercased, with the FQDN root dot
/// removed (`Example.COM.` and `example.com` are one name).
///
/// **ASCII** lowercasing, deliberately. Full Unicode case folding on a hostname
/// is IDNA's job, and doing half of it here would produce a name that neither
/// matches what the user configured nor what DNS resolves — a matcher that is
/// wrong in an unpredictable direction. A non-ASCII host is left as written and
/// therefore only ever matches an identically-written pattern, which is the
/// direction BR-11 can afford to be wrong in.
#[must_use]
pub fn normalize_host(host: &str) -> String {
    let host = host.trim();
    let host = host.strip_suffix('.').unwrap_or(host);
    host.to_ascii_lowercase()
}

/// The bare host of `url` by the **narrowing** splitter above — `#[cfg(test)]`,
/// and that is the fence (REQ-563 verify, C-1).
///
/// It shares [`split`] with [`normalize_url`], which is what made the cache key
/// and the pasted-URL check agree about where the host ends. That splitter is
/// narrowing by design (see the module doc) and therefore exactly the thing a
/// gate must not be built on — it read `https://evil.example\@allowed.example/x`
/// as `allowed.example` while the socket went to `evil.example`. Rather than
/// document that and hope, the function is compiled only into the test binary,
/// so "nobody uses this for a security decision" is a fact about which symbols
/// exist. Its one surviving caller is
/// `the_two_parsers_disagree_exactly_where_a_gate_must_use_the_transports`,
/// which pins the disagreement so a future reader can see why the fence is
/// there.
///
/// The parser every gate uses is [`canonical_lookup_url`] — the transport's own.
///
/// An IPv6 literal is returned here **without** its brackets, so `[::1]` reads
/// as `::1`; [`canonical_lookup_url`] keeps them, because that is what the wire
/// serialization carries.
#[cfg(test)]
#[must_use]
fn user_urls_host_of(url: &str) -> Option<String> {
    let parts = split(url)?;
    let raw = parts.host_port;
    let host = if let Some(inner) = raw.strip_prefix('[') {
        // `[::1]:8080` — the port, if any, is past the closing bracket.
        inner.split_once(']').map_or(inner, |(addr, _)| addr)
    } else {
        raw.split(':').next().unwrap_or(raw)
    };
    let host = normalize_host(host);
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

/// Parse `url` with the **transport's own** parser, answering with the URL as it
/// would be serialized on the wire and the host it would be sent to.
///
/// The single definition of "what does this URL mean", and the whole of REQ-563
/// verify's C-1: every gate — the allowlist, the consent prompt's `(host …)`,
/// the ledger row, the address-class floor at the seam — must read the *same*
/// authority the socket does, or a spelling exists that is checked as one
/// destination and sent to another. `https://evil.example\@allowed.example/x` is
/// the canonical instance: a hand-written splitter that stops the authority at
/// the first `/` or `?` reads the host as `allowed.example`, and `reqwest`
/// (WHATWG URL, which treats `\` as a segment separator) sends it to
/// `evil.example`.
///
/// The answer is therefore a **pair**, and the caller is expected to use both:
/// the re-serialized string is what is handed to the seam and shown at the
/// prompt, so "checked" and "sent" and "displayed" are one string rather than
/// three readings of one input. A re-serialization that differs from what the
/// model wrote is not a hazard for the same reason — the re-serialized form *is*
/// what leaves.
///
/// `None` when the string is not an absolute URL with a host. The scheme is not
/// filtered here: [`FETCHABLE_SCHEMES`](crate::harness::tools::web) is the
/// caller's closed list, and this function's job is parsing, not policy.
#[must_use]
pub fn canonical_lookup_url(url: &str) -> Option<(String, String)> {
    let parsed = reqwest::Url::parse(url.trim()).ok()?;
    let host = parsed.host_str()?.to_owned();
    if host.is_empty() {
        return None;
    }
    Some((String::from(parsed), host))
}

/// The host [`canonical_lookup_url`] reads, for callers that need only that.
#[must_use]
pub fn canonical_host_of(url: &str) -> Option<String> {
    canonical_lookup_url(url).map(|(_, host)| host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_lowercases_the_scheme_and_host() {
        assert_eq!(
            normalize_url("HTTPS://Example.COM/Path"),
            "https://example.com/Path"
        );
    }

    /// The whole reason path and query are exempt from case folding: a path
    /// segment and a query value are opaque bytes the server assigns meaning to,
    /// and two spellings are two documents until that server says otherwise.
    #[test]
    fn normalization_keeps_path_and_query_byte_exact() {
        assert_eq!(
            normalize_url("https://example.com/A/b?Q=One%2Ftwo"),
            "https://example.com/A/b?Q=One%2Ftwo"
        );
    }

    #[test]
    fn normalization_strips_the_fragment() {
        assert_eq!(
            normalize_url("https://example.com/a?b=1#section-2"),
            "https://example.com/a?b=1"
        );
        // A fragment on a bare host leaves the host, not an empty string.
        assert_eq!(
            normalize_url("https://example.com#top"),
            "https://example.com"
        );
    }

    #[test]
    fn normalization_trims_surrounding_whitespace() {
        assert_eq!(
            normalize_url("  https://Example.com/a\n"),
            "https://example.com/a"
        );
    }

    /// Userinfo is credential-shaped, so it is carried through byte-exact: two
    /// different credentials must not normalize to one URL.
    #[test]
    fn normalization_preserves_userinfo_case_while_folding_the_host() {
        assert_eq!(
            normalize_url("https://User:Pw@Example.com/a"),
            "https://User:Pw@example.com/a"
        );
        assert_ne!(
            normalize_url("https://User@example.com/a"),
            normalize_url("https://user@example.com/a")
        );
    }

    /// Every equivalence deliberately *not* added, asserted as a set so that
    /// adding one later has to delete a test that says why it is absent.
    #[test]
    fn normalization_adds_no_equivalences_beyond_scheme_host_and_fragment() {
        let base = normalize_url("https://example.com/a?x=1&y=2");
        for widened in [
            // default port
            "https://example.com:443/a?x=1&y=2",
            // trailing-slash / empty-path equivalence
            "https://example.com/a/?x=1&y=2",
            // query-parameter ordering
            "https://example.com/a?y=2&x=1",
            // percent-decoding
            "https://example.com/%61?x=1&y=2",
        ] {
            assert_ne!(
                normalize_url(widened),
                base,
                "{widened} must stay distinct — widening the equivalence widens \
                 what counts as user-pasted (BR-3)"
            );
        }
    }

    #[test]
    fn normalization_is_idempotent() {
        let once = normalize_url("HTTP://EXAMPLE.com/A#frag");
        assert_eq!(normalize_url(&once), once);
    }

    #[test]
    fn a_string_that_is_not_a_url_is_returned_unchanged() {
        assert_eq!(normalize_url("not a url"), "not a url");
        assert_eq!(
            normalize_url("mailto:someone@example.com"),
            "mailto:someone@example.com"
        );
        // No authority to canonicalize — but the fragment still goes.
        assert_eq!(normalize_url("  ./relative#x  "), "./relative");
    }

    #[test]
    fn host_extraction_drops_scheme_userinfo_port_and_path() {
        assert_eq!(
            user_urls_host_of("https://User:Pw@API.Example.com:8443/v1/x?q=1#f").as_deref(),
            Some("api.example.com")
        );
        assert_eq!(
            user_urls_host_of("http://example.com").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            user_urls_host_of("https://example.com.").as_deref(),
            Some("example.com")
        );
        assert_eq!(user_urls_host_of("[::1]/x"), None);
        assert_eq!(
            user_urls_host_of("https://[::1]:8080/x").as_deref(),
            Some("::1")
        );
        assert_eq!(user_urls_host_of("not a url"), None);
        assert_eq!(user_urls_host_of("https:///just-a-path"), None);
    }

    /// The reason `user_urls_host_of` is doc-fenced: on the one spelling that
    /// matters it and the transport's parser **disagree**, and the transport
    /// wins because the transport is what opens the socket.
    ///
    /// A backslash is a path separator to the WHATWG URL parser, so everything
    /// after it is path — which makes the `@` a path byte rather than a userinfo
    /// delimiter, and the authority `evil.example`. The narrowing splitter reads
    /// the authority to the first `/` or `?`, sees no `/`, and takes the last
    /// `@` as the delimiter: `allowed.example`. Anything gated on the second
    /// answer is gated on a host the request never reaches.
    #[test]
    fn the_two_parsers_disagree_exactly_where_a_gate_must_use_the_transports() {
        const SPOOF: &str = "https://evil.example\\@allowed.example/x";
        assert_eq!(
            user_urls_host_of(SPOOF).as_deref(),
            Some("allowed.example"),
            "the narrowing splitter's answer — non-vacuity for the fence"
        );
        assert_eq!(
            canonical_host_of(SPOOF).as_deref(),
            Some("evil.example"),
            "the transport's answer is where the bytes go"
        );
    }

    /// The canonical parse hands back the URL **as it will be serialized**, so
    /// the checked string and the sent string are the same bytes.
    #[test]
    fn the_canonical_parse_returns_the_reserialized_url_and_its_host() {
        for (raw, url, host) in [
            (
                "https://evil.example\\@allowed.example/x",
                "https://evil.example/@allowed.example/x",
                "evil.example",
            ),
            // Userinfo is not a host, however much it looks like one.
            (
                "https://allowed.example@evil.example/x",
                "https://allowed.example@evil.example/x",
                "evil.example",
            ),
            // A decimal IP is an IP; the wire form is dotted-quad.
            ("http://2130706433/", "http://127.0.0.1/", "127.0.0.1"),
            // An IPv6 literal keeps its brackets — that IS the serialization.
            ("http://[::1]:8080/x", "http://[::1]:8080/x", "[::1]"),
            // An empty path gains its `/`; the default port goes.
            ("https://docs.rs:443", "https://docs.rs/", "docs.rs"),
            ("HTTPS://Docs.RS/Tokio", "https://docs.rs/Tokio", "docs.rs"),
            // A third slash is skipped, not treated as an empty authority — so
            // this names a host, and the narrowing splitter's `None` for the
            // same string is precisely the disagreement the fence is about.
            (
                "https:///just-a-path",
                "https://just-a-path/",
                "just-a-path",
            ),
        ] {
            assert_eq!(
                canonical_lookup_url(raw),
                Some((url.to_owned(), host.to_owned())),
                "{raw}"
            );
        }
        assert_eq!(canonical_lookup_url("not a url"), None);
        // A scheme with no host to send to is not a lookup destination; the
        // *scheme* filter is the caller's, but there is nothing here to answer.
        assert_eq!(canonical_lookup_url("file:///etc/passwd"), None);
    }

    #[test]
    fn host_folding_is_case_and_root_dot_insensitive() {
        assert_eq!(normalize_host("Example.COM."), "example.com");
        assert_eq!(normalize_host(" example.com "), "example.com");
    }
}
