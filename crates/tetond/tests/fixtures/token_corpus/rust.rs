/// Approximate token count by whitespace splitting (matches the mock engine's
/// prompt-token heuristic, so budgets are consistent end to end).
#[must_use]
pub fn approx_tokens(text: &str) -> usize {
    text.split_whitespace().count()
}

/// Bytes-per-whitespace-token bridge between the two budget currencies.
///
/// A whitespace "token" of source code averages ~7–8 bytes (word plus
/// separator), so a token budget of N is consistent with a byte budget of
/// N × 8. At the local engine's window this is also the safe direction: 8 bytes
/// per whitespace word ≈ 2 bytes per real BPE token for code, comfortably above
/// the ~2-bytes-per-token floor valid UTF-8 tokenizes at in practice.
pub const APPROX_BYTES_PER_TOKEN: usize = 8;

/// Byte ceiling on the tool-result text handed to the summarizer engine.
///
/// The summarizer's own prompt must fit the engine window too — sending an
/// unbounded result to the engine that exists to shrink it just moves the
/// over-window failure one call earlier (the pre-fix behavior). 16 KiB is at
/// most ~8k BPE tokens of pathological input, about half the 16,384-token
/// window (`LOCAL_ENGINE_N_CTX`), leaving ample room for the instruction and
/// generation.
pub const SUMMARIZER_INPUT_MAX_BYTES: usize = 16_384;

/// Truncate `text` to at most `max_bytes`, keeping the head and tail with an
/// elision marker between them (errors cluster at the end of build logs, paths
/// and signatures at the top of files). Splits on `char` boundaries; returns
/// the text unchanged when it already fits.
#[must_use]
pub fn truncate_middle(text: &str, max_bytes: usize) -> String {
    const MARKER: &str =
        "\n[... middle elided: content truncated to fit the local context window ...]\n";
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let keep = max_bytes.saturating_sub(MARKER.len());
    if keep < 64 {
        // Degenerate cap: no room for a useful head/tail split.
        return text[..floor_char_boundary(text, max_bytes)].to_owned();
    }
    let head_len = keep * 2 / 3;
    let head_end = floor_char_boundary(text, head_len);
    let tail_start = ceil_char_boundary(text, text.len() - (keep - head_len));
    format!("{}{MARKER}{}", &text[..head_end], &text[tail_start..])
}

/// Largest index ≤ `i` that is a `char` boundary of `s`.
pub(super) fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Smallest index ≥ `i` that is a `char` boundary of `s`.
fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// What [`summarize_if_large`] did with a tool result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummarizeOutcome {
    /// The text to fold into context: the engine's summary, a mechanical
    /// truncation (engine failure), or the original (under threshold).
    pub text: String,
    /// The engine error hit while summarizing, when the summary fell back to
    /// mechanical truncation. The caller MUST surface this (log or event) — the
    /// summarization duty guards the context window, so its failure is never
    /// allowed to be silent.
    pub engine_error: Option<String>,
}
