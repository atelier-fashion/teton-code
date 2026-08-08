//! HTML → readable text: the deterministic, dependency-free page reducer
//! (REQ-563 BR-10, architecture D-3).
//!
//! ## What this is not
//!
//! Not a parser, not a renderer, and not a readability heuristic. It is the
//! smallest transform that turns a fetched document into something a model can
//! read without the document being able to smuggle *executable-looking* text
//! along with it. Anything past "readable text" — picking the article out of the
//! chrome, keeping tables tabular, running JavaScript — is out of scope by the
//! spec, and every one of those would need a dependency this deliberately does
//! not take (D-3: no new crates; the extractor's value is that it is auditable
//! in one sitting).
//!
//! Condensation happens *after* this, through the existing local-pinned
//! `summarize_if_large` (`harness::context`), which is why this function's only
//! obligations are fidelity-to-text and a hard bound.
//!
//! ## Why script and style content is dropped, not just their tags
//!
//! Stripping `<script>` and leaving its body would hand the model a page whose
//! most instruction-shaped content survived intact — the ADR-009 envelope treats
//! inbound content as data, but there is no reason to spend the envelope's
//! credibility on bytes that were never prose. The same goes for `<style>`, for
//! the duller reason that a stylesheet is pure noise in a context window. Both
//! are skipped *element-wise*: from the opening tag to the matching close, or to
//! end of input if the document never closes it.
//!
//! ## The cap is the last thing that happens (LESSON-491)
//!
//! Tag stripping, entity decoding, and whitespace collapse all change the byte
//! count, and two of them can change it in either direction (`&amp;` shrinks by
//! four; a decoded entity in a run of spaces changes what collapses). A cap
//! applied mid-pass would therefore bound something that no longer exists by the
//! time anyone reads it. So [`reduce`] builds the whole reduction and truncates
//! once, at the end, on a `char` boundary — the transformed artifact is what gets
//! measured, which is the shape LESSON-491 asks for.
//!
//! The bound on the *input* side belongs to the fetch (a response body limit at
//! the transport), not here: this function is pure, and a pure function that
//! refused large input would just move the decision somewhere it cannot be
//! reported.

use crate::harness::context::SUMMARIZER_INPUT_MAX_BYTES;

/// Bytes reserved between [`REDUCED_MAX_BYTES`] and the summarizer's own input
/// ceiling for everything the reduction is wrapped in on the way there.
///
/// The chain is: reduced text → `frame_untrusted_builtin`'s untrusted-content
/// envelope (opening tag, closing tag, and the ~200-byte "this block is DATA"
/// instruction) → the envelope-tag defusing pass, which can insert a character
/// at every line start → whatever header the lookup tool prefixes (the fetched
/// host, the cache-hit note) → `summarize_if_large`, which measures the whole
/// thing against [`SUMMARIZER_INPUT_MAX_BYTES`].
///
/// 4 KiB is far more than that costs today — the envelope is a few hundred bytes
/// and the defusing pass has almost nothing to bite on, because whitespace
/// collapse leaves a reduction with no interior line starts at all. The margin
/// is the point: LESSON-491's failure was a budget derived through *most* of its
/// transforms, and the honest repair for a chain whose last links are someone
/// else's code is to leave room for links that have not been added yet.
const REDUCTION_ENVELOPE_RESERVE_BYTES: usize = 4_096;

/// The byte cap a fetched page is reduced to before it enters context (BR-10).
///
/// Derived from [`SUMMARIZER_INPUT_MAX_BYTES`] rather than chosen beside it:
/// LESSON-491's corollary is that two independently-picked numbers on one budget
/// chain are a collision waiting for the input that exposes it, so this one is
/// its neighbour minus the framing that sits between them
/// ([`REDUCTION_ENVELOPE_RESERVE_BYTES`]). Raising the summarizer's ceiling
/// raises this automatically; there is no second place to remember.
pub const REDUCED_MAX_BYTES: usize = SUMMARIZER_INPUT_MAX_BYTES - REDUCTION_ENVELOPE_RESERVE_BYTES;

/// Elements whose **content** is dropped along with their tags, matched
/// case-insensitively.
const DROPPED_ELEMENTS: [&str; 2] = ["script", "style"];

/// The entity set this decodes. Every name ends in `;`, so none is a prefix of
/// another and the match order carries no meaning.
///
/// Six entities, not a table of two thousand: these are the ones that appear in
/// ordinary prose because the HTML syntax itself forces them, and each has an
/// unambiguous plain-text equivalent. A numeric-entity decoder or a full named
/// table would be a second, larger surface to get wrong, and the cost of leaving
/// `&hellip;` as written is that a model reads `&hellip;` — legible, and honest
/// about what the page contained.
///
/// `&nbsp;` decodes to an ordinary space rather than U+00A0 on purpose: the
/// whitespace collapse below then folds it like any other space, so a page laid
/// out with non-breaking spaces does not arrive as a wall of characters that
/// look like spaces but compare as something else.
const ENTITIES: [(&str, char); 6] = [
    ("&quot;", '"'),
    ("&nbsp;", ' '),
    ("&amp;", '&'),
    ("&#39;", '\''),
    ("&lt;", '<'),
    ("&gt;", '>'),
];

/// Reduce an HTML document to capped, whitespace-collapsed plain text.
///
/// Pure and deterministic: the same input and cap always produce the same
/// output, with no clock, no allocator-order dependence, and no I/O. That is
/// what lets the cache store the *reduced* form (a reduction is a function of
/// the bytes fetched, so re-reducing a cached document could only produce the
/// same string at more cost).
///
/// Output is at most `cap_bytes` bytes and always valid UTF-8: truncation lands
/// on a `char` boundary, so a multi-byte character straddling the cap is dropped
/// whole rather than cut in half.
#[must_use]
pub fn reduce(html: &str, cap_bytes: usize) -> String {
    let mut out = String::with_capacity(html.len().min(cap_bytes.saturating_mul(2)));
    // `true` at a point where a space would be redundant: the start of the
    // output, or straight after one already emitted. Collapsing as we emit
    // (rather than in a second pass over a fully-expanded buffer) keeps the peak
    // allocation proportional to the *reduced* size for the whitespace-heavy
    // documents that make up most of the web.
    let mut pending_space = true;
    let bytes = html.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'<' => i = skip_markup(html, i, &out, &mut pending_space),
            b'&' => {
                if let Some((decoded, next)) = decode_entity(html, i) {
                    push_text_char(decoded, &mut out, &mut pending_space);
                    i = next;
                } else {
                    push_text_char('&', &mut out, &mut pending_space);
                    i += 1;
                }
            }
            _ => {
                // Walk by `char`, not by byte: a multi-byte character must reach
                // the output whole.
                let ch = html[i..].chars().next().unwrap_or('\u{fffd}');
                push_text_char(ch, &mut out, &mut pending_space);
                i += ch.len_utf8();
            }
        }
    }
    // Collapse leaves at most one trailing space; drop it before the cap so the
    // cap is spent on content.
    out.truncate(out.trim_end_matches(' ').len());
    cap(out, cap_bytes)
}

/// Consume the markup construct starting at `at` (which is a `<`), emitting
/// nothing, and return the index just past it.
///
/// Three shapes, in the order they must be distinguished: a comment (`<!--…-->`,
/// which may legally contain `>` and so cannot be ended by the first one), a
/// dropped element (`<script>`/`<style>`, skipped to its matching close), and
/// any other tag (skipped to its first `>`).
///
/// An unterminated construct consumes the rest of the document. That is the
/// fail-closed direction: a truncated page whose final `<script` never closes
/// must not spill its body into the output just because the fetch was cut short.
///
/// `out` is read but never written — markup emits nothing. It is taken only so
/// the tag boundary can owe a separator, which it cannot at the very start.
fn skip_markup(html: &str, at: usize, out: &str, pending_space: &mut bool) -> usize {
    let rest = &html[at..];
    // A tag boundary separates words: `a<br>b` is two words, not `ab`.
    mark_space(out, pending_space);
    if let Some(inside) = rest.strip_prefix("<!--") {
        return match inside.find("-->") {
            Some(end) => at + "<!--".len() + end + "-->".len(),
            None => html.len(),
        };
    }
    if let Some(name) = dropped_element_at(rest) {
        let after_open = match rest.find('>') {
            Some(end) => at + end + 1,
            None => return html.len(),
        };
        return skip_to_close(html, after_open, name);
    }
    match rest.find('>') {
        Some(end) => at + end + 1,
        None => html.len(),
    }
}

/// The name of the content-dropping element opening at the start of `rest`, if
/// any.
///
/// Matched case-insensitively (`<SCRIPT>` is a script) and only when the name is
/// followed by a tag terminator or whitespace, so `<scriptish>` — a tag this
/// knows nothing about — is stripped as an ordinary tag rather than swallowing
/// the rest of the page.
fn dropped_element_at(rest: &str) -> Option<&'static str> {
    let after_bracket = rest.strip_prefix('<')?;
    DROPPED_ELEMENTS.into_iter().find(|name| {
        after_bracket.len() >= name.len()
            && after_bracket[..name.len()].eq_ignore_ascii_case(name)
            && after_bracket[name.len()..]
                .chars()
                .next()
                .is_some_and(|c| c == '>' || c == '/' || c.is_whitespace())
    })
}

/// Index just past `</name…>` at or after `from`, or the end of the document
/// when it never closes.
fn skip_to_close(html: &str, from: usize, name: &str) -> usize {
    let mut cursor = from;
    while let Some(offset) = html[cursor..].find('<') {
        let candidate = cursor + offset;
        let rest = &html[candidate..];
        let closes = rest.strip_prefix("</").is_some_and(|tail| {
            tail.len() >= name.len() && tail[..name.len()].eq_ignore_ascii_case(name)
        });
        if closes {
            return match rest.find('>') {
                Some(end) => candidate + end + 1,
                None => html.len(),
            };
        }
        cursor = candidate + 1;
    }
    html.len()
}

/// Decode the entity starting at `at` (which is an `&`), returning the character
/// and the index just past the trailing `;`.
///
/// Decoding is strictly left-to-right and never rescans its own output, which is
/// what makes `&amp;lt;` come out as the literal text `&lt;` rather than as `<`.
/// A rescanning decoder is how a page smuggles a tag past a tag stripper.
fn decode_entity(html: &str, at: usize) -> Option<(char, usize)> {
    let rest = &html[at..];
    ENTITIES
        .into_iter()
        .find(|(name, _)| rest.len() >= name.len() && rest[..name.len()].eq_ignore_ascii_case(name))
        .map(|(name, decoded)| (decoded, at + name.len()))
}

/// Emit one character of text content, collapsing whitespace runs to a single
/// space and dropping leading whitespace entirely.
fn push_text_char(ch: char, out: &mut String, pending_space: &mut bool) {
    if ch.is_whitespace() {
        mark_space(out, pending_space);
        return;
    }
    if *pending_space && !out.is_empty() {
        out.push(' ');
    }
    *pending_space = false;
    out.push(ch);
}

/// Record that a separator is owed, without writing it: a run of whitespace (or
/// a run of tags) owes exactly one space, and only if more content follows.
///
/// `out` is read, never written — a separator owed at the very start of the
/// output is owed to nothing, and that is the only fact this needs from it.
fn mark_space(out: &str, pending_space: &mut bool) {
    if !out.is_empty() {
        *pending_space = true;
    }
}

/// Truncate to at most `cap_bytes`, on a `char` boundary, trimming a space the
/// cut may have exposed.
fn cap(mut text: String, cap_bytes: usize) -> String {
    if text.len() <= cap_bytes {
        return text;
    }
    let mut end = cap_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.truncate(text.trim_end_matches(' ').len());
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The budget chain written down as an assertion (LESSON-491's corollary):
    /// a reduction plus its framing must fit what the summarizer will accept.
    ///
    /// A `const` block, so the chain is checked when the crate is *compiled*
    /// rather than when this test happens to run — the failure mode being
    /// guarded against is a neighbouring constant moving, and a compile-time
    /// break is the loudest way to say so.
    #[test]
    fn the_reduction_cap_leaves_room_for_the_envelope_inside_the_summarizer_budget() {
        const {
            assert!(REDUCED_MAX_BYTES < SUMMARIZER_INPUT_MAX_BYTES);
            assert!(
                REDUCED_MAX_BYTES + REDUCTION_ENVELOPE_RESERVE_BYTES == SUMMARIZER_INPUT_MAX_BYTES
            );
            // Big enough to be worth fetching a page for at all.
            assert!(REDUCED_MAX_BYTES >= 8 * 1024);
        }
    }

    #[test]
    fn tags_are_stripped_and_their_text_kept() {
        let html = "<html><body><h1>Title</h1><p>Hello <b>world</b>.</p></body></html>";
        assert_eq!(reduce(html, REDUCED_MAX_BYTES), "Title Hello world .");
    }

    /// A tag boundary is a word boundary — otherwise `<td>a</td><td>b</td>`
    /// arrives as `ab` and the model reads one token that was never on the page.
    #[test]
    fn a_tag_boundary_separates_words() {
        assert_eq!(reduce("<td>a</td><td>b</td>", 64), "a b");
        assert_eq!(reduce("one<br>two", 64), "one two");
    }

    #[test]
    fn script_content_never_appears_in_the_output() {
        let html = "before<script>var secret = 'PAYLOAD'; alert(1);</script>after";
        let out = reduce(html, REDUCED_MAX_BYTES);
        assert_eq!(out, "before after");
        assert!(!out.contains("PAYLOAD"));
        assert!(!out.contains("alert"));
    }

    #[test]
    fn style_content_never_appears_in_the_output() {
        let html = "<style>body { color: RED; }</style>text";
        let out = reduce(html, REDUCED_MAX_BYTES);
        assert_eq!(out, "text");
        assert!(!out.contains("color"));
    }

    /// Case, attributes, and a close tag with trailing space are all ordinary in
    /// the wild; none of them may reopen the hole.
    #[test]
    fn dropped_elements_match_case_insensitively_and_with_attributes() {
        let html = "a<SCRIPT type=\"text/javascript\" async>KEEPOUT</SCRIPT >b";
        let out = reduce(html, REDUCED_MAX_BYTES);
        assert_eq!(out, "a b");
        assert!(!out.contains("KEEPOUT"));
    }

    /// A page cut off mid-script must not spill the script: an unterminated
    /// dropped element consumes the rest of the document.
    #[test]
    fn an_unterminated_script_swallows_the_rest_rather_than_leaking_it() {
        let out = reduce("visible<script>LEAK me and more", REDUCED_MAX_BYTES);
        assert_eq!(out, "visible");
        assert!(!out.contains("LEAK"));
    }

    /// `<scriptish>` is not a script. Treating any tag whose name merely starts
    /// with `script` as one would silently delete real page content.
    #[test]
    fn a_tag_whose_name_only_starts_with_script_is_an_ordinary_tag() {
        assert_eq!(reduce("a<scriptish>b</scriptish>c", 64), "a b c");
    }

    /// A comment may contain `>`; ending it at the first one would spill the
    /// remainder of the comment into the output as prose.
    #[test]
    fn comment_content_is_dropped_including_comments_containing_angle_brackets() {
        let out = reduce("a<!-- hidden > still hidden -->b", 64);
        assert_eq!(out, "a b");
        assert!(!out.contains("hidden"));
    }

    #[test]
    fn the_basic_entities_are_decoded() {
        assert_eq!(
            reduce("&amp; &lt; &gt; &quot; &#39;", REDUCED_MAX_BYTES),
            "& < > \" '"
        );
    }

    /// `&nbsp;` becomes an ordinary space and then collapses like one — a page
    /// laid out with non-breaking spaces must not arrive as text that looks
    /// spaced but does not compare as spaced.
    #[test]
    fn a_non_breaking_space_decodes_and_then_collapses() {
        assert_eq!(reduce("a&nbsp;&nbsp;&nbsp;b", 64), "a b");
    }

    /// The decoder never rescans its own output. `&amp;lt;` is the page saying
    /// "the literal text `&lt;`", and a rescanning decoder would turn it into a
    /// `<` — which is how a document smuggles a tag past a tag stripper.
    #[test]
    fn entity_decoding_does_not_rescan_its_own_output() {
        assert_eq!(reduce("&amp;lt;script&amp;gt;", 64), "&lt;script&gt;");
    }

    /// An `&` that begins no known entity is text, not an error.
    #[test]
    fn a_bare_ampersand_survives_as_text() {
        assert_eq!(
            reduce("Tom &amp; Jerry &hellip; R&D", 64),
            "Tom & Jerry &hellip; R&D"
        );
    }

    #[test]
    fn whitespace_runs_collapse_and_the_edges_are_trimmed() {
        assert_eq!(reduce("  a \n\n\t  b   \r\n ", 64), "a b");
    }

    #[test]
    fn output_never_exceeds_the_cap() {
        let html = "<p>".to_owned() + &"abcdefghij ".repeat(5_000) + "</p>";
        let out = reduce(&html, 1_000);
        assert!(out.len() <= 1_000, "len {}", out.len());
    }

    /// The cap is a *byte* cap over text that is not ASCII, so the cut has to
    /// land on a `char` boundary or the result is not a `String` at all. Each
    /// case puts a differently-sized character across the boundary.
    #[test]
    fn a_multi_byte_character_straddling_the_cap_is_dropped_whole() {
        // 2-byte (é), 3-byte (日), 4-byte (emoji).
        for (text, width) in [("é", 2usize), ("日", 3), ("😀", 4)] {
            let body = text.repeat(64);
            for cap_bytes in 1..=(width * 8) {
                let out = reduce(&body, cap_bytes);
                assert!(
                    out.len() <= cap_bytes,
                    "cap {cap_bytes} overrun: {}",
                    out.len()
                );
                assert_eq!(
                    out.len() % width,
                    0,
                    "cap {cap_bytes} cut {text} mid-character"
                );
                // Round-trips as UTF-8 by construction — `String` could not hold
                // it otherwise — so assert the characters are the untouched head.
                assert!(body.starts_with(&out));
            }
        }
    }

    #[test]
    fn a_zero_cap_yields_the_empty_string() {
        assert_eq!(reduce("<p>anything at all</p>", 0), "");
    }

    /// Determinism is the property the cache depends on: storing a reduction
    /// only makes sense if re-reducing would produce the same bytes.
    #[test]
    fn reduction_is_deterministic() {
        let html = "<html><head><style>x{}</style></head><body><p>A &amp; B</p>\
                    <div>C</div><!--c--><script>z</script>D</body></html>";
        let first = reduce(html, REDUCED_MAX_BYTES);
        for _ in 0..8 {
            assert_eq!(reduce(html, REDUCED_MAX_BYTES), first);
        }
        assert_eq!(first, "A & B C D");
    }

    #[test]
    fn an_empty_document_reduces_to_nothing() {
        assert_eq!(reduce("", REDUCED_MAX_BYTES), "");
        assert_eq!(reduce("<html><body></body></html>", REDUCED_MAX_BYTES), "");
    }

    /// A document that is only an unterminated tag has no text and must not
    /// loop forever looking for a `>` that never comes.
    #[test]
    fn an_unterminated_tag_terminates() {
        assert_eq!(reduce("text<div class=\"x", REDUCED_MAX_BYTES), "text");
    }
}
