//! REQ-586 AC-3 / ADR-10: the two context-budget estimator allowances are
//! pinned against a committed token corpus.
//!
//! The daemon has no tokenizer at runtime. A remote route's budget is sized in
//! two currencies — whitespace words scaled by `REMOTE_TOKENS_PER_WORD` (3/2)
//! and bytes divided by a bytes-per-token floor (2) — and the claim behind
//! those two numbers is that, for the content classes a coding session sends,
//! `max(words × 3/2, bytes / 2) ≥ tokens`. This suite checks that claim against
//! `tests/fixtures/token_corpus/`: five samples (prose, a real slice of this
//! crate, a minified `cargo metadata` tool result, `find`-style absolute paths,
//! base64) tokenized ONCE with a reference tokenizer (`tiktoken` `o200k_base`)
//! by `tools/token_corpus/count.py`, whose output is the committed
//! `token_counts.json`. CI needs no tokenizer, no network and no new Cargo
//! dependency (ADR-10): the test only reads the json.
//!
//! Fixture fidelity (LESSON-460): `words` and `bytes` are recomputed from the
//! sample files here — with the daemon's own `approx_tokens` for the word rule
//! — and a row that no longer matches its file is a red test, not a silent
//! stale number. The set of samples is pinned too, so the inconvenient one
//! cannot be dropped to make the bound hold.
//!
//! What the corpus says (o200k_base, tiktoken 0.14.0; the table is also
//! printed by `combined_estimate_covers_every_sample_outside_the_documented_gap`):
//!
//! | sample        | words | bytes | tokens | B/token | tok/word |
//! |---------------|------:|------:|-------:|--------:|---------:|
//! | prose.txt     |   614 |  3414 |    743 |    4.59 |     1.21 |
//! | rust.rs       |   495 |  3364 |    839 |    4.01 |     1.69 |
//! | minified.json |    33 |  5337 |   1489 |    3.58 |    45.12 |
//! | paths.txt     |   196 | 14952 |   4171 |    3.58 |    21.28 |
//! | base64.txt    |    54 |  4150 |   2868 |    1.45 |    53.11 |
//!
//! Prose is the only class the word guard covers on its own; everything
//! denser needs the byte guard, which at 2 B/token covers Rust, minified JSON
//! and path-heavy output with room to spare — and does NOT cover base64,
//! which the reference tokenizer encodes at ≈1.45 B/token (cl100k_base:
//! ≈1.37; hex: ≈1.75). That gap is recorded in
//! [`KNOWN_UNCOVERED_AT_PINNED_FLOOR`] and asserted in both directions, so the
//! day the floor is lowered (or the sample changes) this file says so.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tetond::harness::context::approx_tokens;

// The three allowances are **read from the production constants**, not restated
// here (TASK-192's one-home pass; TASK-183 pinned them as literals with this
// swap deferred). That keeps the suite an empirical claim about the corpus
// rather than a restatement of budget.rs: the assertions below are measured
// token counts from a reference tokenizer, so lowering the ratio or the floor
// makes them fail *here* — mutation (e), 3/2 → 1/1, is red on prose — instead
// of moving the test's own expectations with it.
/// `REMOTE_TOKENS_PER_WORD` as an integer ratio: 3 tokens per 2 words, as
/// [`tetond::harness::budget`] derives a remote word budget.
const REMOTE_TOKENS_PER_WORD_NUM: u64 = tetond::harness::budget::REMOTE_TOKENS_PER_WORD_NUM as u64;
const REMOTE_TOKENS_PER_WORD_DEN: u64 = tetond::harness::budget::REMOTE_TOKENS_PER_WORD_DEN as u64;
/// The bytes-per-token floor the remote byte budget is derived at — the duty
/// constant `budget::derive` itself multiplies by, not a second 2.
const REMOTE_BYTES_PER_TOKEN_FLOOR: u64 =
    tetond::harness::duty::DUTY_REQUEST_BYTES_PER_TOKEN as u64;

/// The reference tokenizer every row must have been produced with.
const REFERENCE_TOKENIZER: &str = "o200k_base";

/// The corpus, by name. Pinned so a sample cannot be quietly removed (and so a
/// new one must be added here, to the generator's docstring, and to the json).
const EXPECTED_SAMPLES: [&str; 5] = [
    "base64.txt",
    "minified.json",
    "paths.txt",
    "prose.txt",
    "rust.rs",
];

/// The prose sample: the class the word guard must cover on its own.
const PROSE_SAMPLE: &str = "prose.txt";

/// Samples the combined estimate is known NOT to cover at the pinned floor —
/// the measured gap, not a blessing of it. `o200k_base` encodes random base64
/// at ≈1.45 B/token, under the 2 B/token floor, so `max(words × 3/2, bytes / 2)`
/// falls ≈28% short of the true count for `base64.txt`. Whether the floor is
/// lowered (at a cost to every prose/code prompt, which the byte guard already
/// binds) or base64 is left to the typed `context_length_exceeded` backstop is
/// a REQ-586 decision; either way the entry below must track it: when the
/// pinned floor covers the sample again this test fails and says to remove it.
const KNOWN_UNCOVERED_AT_PINNED_FLOOR: [&str; 1] = ["base64.txt"];

/// One row of `token_counts.json`, as `tools/token_corpus/count.py` writes it.
#[derive(Debug, Clone, Deserialize)]
struct Row {
    file: String,
    words: u64,
    bytes: u64,
    tokens: u64,
    tokenizer: String,
    tokenizer_version: String,
}

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/token_corpus")
}

fn load_rows() -> Vec<Row> {
    let path = corpus_dir().join("token_counts.json");
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let rows: Vec<Row> =
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
    assert!(!rows.is_empty(), "{} has no rows", path.display());
    rows
}

fn read_sample(name: &str) -> String {
    let path = corpus_dir().join(name);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read sample {}: {e}", path.display()))
}

/// The word guard's token estimate: `words × 3/2`, integer arithmetic as the
/// budget derivation does it.
fn words_estimate(words: u64) -> u64 {
    words * REMOTE_TOKENS_PER_WORD_NUM / REMOTE_TOKENS_PER_WORD_DEN
}

/// The byte guard's token estimate: `bytes / 2`.
fn bytes_estimate(bytes: u64) -> u64 {
    bytes / REMOTE_BYTES_PER_TOKEN_FLOOR
}

/// The combined estimate AC-3 asserts covers a sample.
fn combined_estimate(row: &Row) -> u64 {
    words_estimate(row.words).max(bytes_estimate(row.bytes))
}

/// The recorded counts still describe the files: every expected sample has a
/// row and a file, no stray sample or row exists, every row names the reference
/// tokenizer, and `words` (by the daemon's own `approx_tokens`) and `bytes`
/// recompute to what the generator wrote. A stale fixture is red here first.
#[test]
fn fixture_rows_match_the_sample_files() {
    let rows = load_rows();
    let expected: BTreeSet<&str> = EXPECTED_SAMPLES.into_iter().collect();

    let on_disk: BTreeSet<String> = fs::read_dir(corpus_dir())
        .expect("list the corpus dir")
        .map(|entry| {
            entry
                .expect("dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        // Ignore dotfiles (a Finder `.DS_Store` is not a sample) and the json.
        .filter(|name| !name.starts_with('.') && name != "token_counts.json")
        .collect();
    let on_disk: BTreeSet<&str> = on_disk.iter().map(String::as_str).collect();
    assert_eq!(
        on_disk, expected,
        "the corpus directory must hold exactly the pinned samples (EXPECTED_SAMPLES)"
    );

    let recorded: BTreeSet<&str> = rows.iter().map(|r| r.file.as_str()).collect();
    assert_eq!(
        recorded, expected,
        "token_counts.json must have exactly one row per pinned sample — regenerate it \
         with `python3 tools/token_corpus/count.py`"
    );

    for row in &rows {
        assert_eq!(
            row.tokenizer, REFERENCE_TOKENIZER,
            "{}: tokenized with {:?}, expected {REFERENCE_TOKENIZER}",
            row.file, row.tokenizer
        );
        assert!(
            row.tokenizer_version.starts_with("tiktoken "),
            "{}: tokenizer_version {:?} should name the tiktoken release",
            row.file,
            row.tokenizer_version
        );
        assert!(row.tokens > 0, "{}: zero tokens recorded", row.file);

        let text = read_sample(&row.file);
        let words = approx_tokens(&text) as u64;
        let bytes = text.len() as u64;
        assert_eq!(
            words, row.words,
            "{}: recorded words {} but the file has {words} — the fixture is stale; \
             regenerate with `python3 tools/token_corpus/count.py`",
            row.file, row.words
        );
        assert_eq!(
            bytes, row.bytes,
            "{}: recorded bytes {} but the file has {bytes} — the fixture is stale; \
             regenerate with `python3 tools/token_corpus/count.py`",
            row.file, row.bytes
        );
        assert!(
            bytes >= 2048,
            "{}: {bytes} bytes — every sample is at least 2 KiB",
            row.file
        );
    }
}

/// AC-3's bound, per sample: `max(words × 3/2, bytes / 2) ≥ tokens`, naming
/// the sample and both estimates on failure. Samples listed in
/// [`KNOWN_UNCOVERED_AT_PINNED_FLOOR`] are asserted the other way round, so the
/// documented gap cannot go stale in either direction. Prints each sample's
/// measured bytes/token and tokens/word (run with `--nocapture`), the evidence
/// the 2 B/token floor is revisited with.
#[test]
fn combined_estimate_covers_every_sample_outside_the_documented_gap() {
    let rows = load_rows();
    println!(
        "{:<15}{:>8}{:>8}{:>8}{:>10}{:>10}{:>10}  covered",
        "sample", "words", "bytes", "tokens", "B/token", "tok/word", "estimate"
    );
    for row in &rows {
        let estimate = combined_estimate(row);
        let covered = estimate >= row.tokens;
        println!(
            "{:<15}{:>8}{:>8}{:>8}{:>10.2}{:>10.2}{:>10}  {}",
            row.file,
            row.words,
            row.bytes,
            row.tokens,
            row.bytes as f64 / row.tokens as f64,
            row.tokens as f64 / row.words as f64,
            estimate,
            if covered { "yes" } else { "NO" }
        );
        if KNOWN_UNCOVERED_AT_PINNED_FLOOR.contains(&row.file.as_str()) {
            assert!(
                !covered,
                "{}: max(words {} × {REMOTE_TOKENS_PER_WORD_NUM}/{REMOTE_TOKENS_PER_WORD_DEN} = {}, \
                 bytes {} / {REMOTE_BYTES_PER_TOKEN_FLOOR} = {}) = {estimate} now covers {} tokens — \
                 the pinned allowances cover this sample; remove it from \
                 KNOWN_UNCOVERED_AT_PINNED_FLOOR",
                row.file,
                row.words,
                words_estimate(row.words),
                row.bytes,
                bytes_estimate(row.bytes),
                row.tokens
            );
        } else {
            assert!(
                covered,
                "{}: max(words {} × {REMOTE_TOKENS_PER_WORD_NUM}/{REMOTE_TOKENS_PER_WORD_DEN} = {}, \
                 bytes {} / {REMOTE_BYTES_PER_TOKEN_FLOOR} = {}) = {estimate} < {} tokens \
                 ({:.2} B/token, {:.2} tokens/word) — the estimator under-counts this sample",
                row.file,
                row.words,
                words_estimate(row.words),
                row.bytes,
                bytes_estimate(row.bytes),
                row.tokens,
                row.bytes as f64 / row.tokens as f64,
                row.tokens as f64 / row.words as f64
            );
        }
    }
}

/// Non-vacuity: the word guard alone covers prose (`words × 3/2 ≥ tokens`, so
/// the 3/2 ratio is pinned from below — 1/1 fails here), and at least one dense
/// sample is NOT covered by words alone (`words × 3/2 < tokens`), so the byte
/// guard is demonstrably needed rather than decorative. Neither half requires
/// any sample to fail the combined bound.
#[test]
fn words_guard_alone_covers_prose_but_not_dense_content() {
    let rows = load_rows();
    let prose = rows
        .iter()
        .find(|r| r.file == PROSE_SAMPLE)
        .unwrap_or_else(|| panic!("{PROSE_SAMPLE} missing from token_counts.json"));
    assert!(
        words_estimate(prose.words) >= prose.tokens,
        "{PROSE_SAMPLE}: words {} × {REMOTE_TOKENS_PER_WORD_NUM}/{REMOTE_TOKENS_PER_WORD_DEN} = {} \
         < {} tokens — the word guard no longer covers prose on its own ({:.2} tokens/word)",
        prose.words,
        words_estimate(prose.words),
        prose.tokens,
        prose.tokens as f64 / prose.words as f64
    );

    let dense_uncovered: Vec<&str> = rows
        .iter()
        .filter(|r| r.file != PROSE_SAMPLE && words_estimate(r.words) < r.tokens)
        .map(|r| r.file.as_str())
        .collect();
    assert!(
        !dense_uncovered.is_empty(),
        "every dense sample is covered by words × {REMOTE_TOKENS_PER_WORD_NUM}/{REMOTE_TOKENS_PER_WORD_DEN} \
         alone — the byte guard would be decorative; the corpus no longer exercises it"
    );
    // Every non-prose class in this corpus needs the byte guard — including
    // Rust source (≈1.69 tokens/word), which is why the architecture says the
    // byte guard is what binds for code, not only for blobs.
    println!("samples the word guard alone does not cover: {dense_uncovered:?}");
}
