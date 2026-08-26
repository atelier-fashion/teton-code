//! REQ-586 AC-3 / ADR-10: the two context-budget estimator allowances are
//! pinned against a committed token corpus.
//!
//! The daemon has no tokenizer at runtime. A remote route's budget is sized in
//! two currencies — whitespace words scaled by `REMOTE_TOKENS_PER_WORD` (3/2)
//! and bytes divided by a bytes-per-token floor (2) — and the claim behind
//! those two numbers is that, for the content classes a coding session sends,
//! `max(words × 3/2, bytes / 2) ≥ tokens`. This suite checks that claim against
//! `tests/fixtures/token_corpus/`: six samples (prose, a real slice of this
//! crate, a minified `cargo metadata` tool result, `find`-style absolute paths,
//! base64, and a small-integer matrix) tokenized ONCE with a reference
//! tokenizer (`tiktoken` `o200k_base`)
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
//! | sample           | words | bytes | tokens | B/token | tok/word |
//! |------------------|------:|------:|-------:|--------:|---------:|
//! | prose.txt        |   614 |  3414 |    743 |    4.59 |     1.21 |
//! | rust.rs          |   495 |  3364 |    839 |    4.01 |     1.69 |
//! | minified.json    |    33 |  5337 |   1489 |    3.58 |    45.12 |
//! | paths.txt        |   196 | 14952 |   4171 |    3.58 |    21.28 |
//! | base64.txt       |    54 |  4150 |   2868 |    1.45 |    53.11 |
//! | numeric_grid.txt | 10240 | 20480 |  20480 |    1.00 |     2.00 |
//!
//! Prose is the only class the word guard covers on its own; everything
//! denser needs the byte guard, which at 2 B/token covers Rust, minified JSON
//! and path-heavy output with room to spare — and does NOT cover base64,
//! which the reference tokenizer encodes at ≈1.45 B/token (cl100k_base:
//! ≈1.37; hex: ≈1.75), nor `numeric_grid.txt`, which it encodes at exactly
//! 1.00. Those gaps are recorded in [`KNOWN_UNCOVERED_AT_PINNED_FLOOR`] and
//! asserted in both directions, so the day the floor is lowered (or a sample
//! changes) this file says so.
//!
//! ## The sixth sample, and why REQ-590 needed one (AC-9)
//!
//! The first five samples were chosen to exercise the *estimator*. REQ-590
//! added `numeric_grid.txt` to exercise the **budget**: since D-3 took the
//! full engine window, the local word half carries exactly zero margin
//! (`words × 3/2 = usable`, ADR-6), so any class over 1.5 real tokens per
//! whitespace word overruns the engine at full budget. Rust already does, at
//! 1.69 — what saves those turns is that its 6.8 B/word spends the *byte*
//! budget long before the word one.
//!
//! `numeric_grid.txt` is the quadrant that leaves uncovered: token-dense and
//! byte-**light**, at 2.00 tokens/word and 2.00 B/word. Both guards admit a
//! full-budget turn of it and the engine cannot hold the result. See
//! [`a_full_word_budget_turn_of_token_dense_byte_light_content_overruns_the_engine`]
//! for the measured overrun and what is meant to catch it.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tetond::harness::budget::{derive, BudgetInputs, LOCAL_GENERATION_RESERVATION};
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
const EXPECTED_SAMPLES: [&str; 6] = [
    "base64.txt",
    "minified.json",
    "numeric_grid.txt",
    "paths.txt",
    "prose.txt",
    "rust.rs",
];

/// The prose sample: the class the word guard must cover on its own.
const PROSE_SAMPLE: &str = "prose.txt";

/// The token-dense, **byte-light** sample REQ-590 AC-9 measures the local word
/// budget against: a 160 × 64 matrix of space-separated single digits — the
/// shape `numpy.savetxt(fmt="%d")` writes, and the shape a quantized raster,
/// an occupancy mask or an adjacency matrix arrives in.
///
/// Sized deliberately at **exactly one turn's full local word budget** (10,240
/// words / 20,480 bytes), so the finding below is a measurement rather than an
/// extrapolation from a smaller file.
///
/// The density is a property of the *format*, not of this field: a random 0-9
/// grid, a sparse 0/1 mask and a run-heavy grid of the same shape all measure
/// exactly 2.000 tokens/word, because `o200k_base` gives every digit and every
/// separating space its own token. The sample was not searched for.
const DENSE_BYTE_LIGHT_SAMPLE: &str = "numeric_grid.txt";

/// Samples the combined estimate is known NOT to cover at the pinned floor —
/// the measured gap, not a blessing of it. Whether the floor is lowered (at a
/// cost to every prose/code prompt, which the byte guard already binds) or
/// these classes are left to the typed `context_length_exceeded` backstop is a
/// REQ-586 decision, reaffirmed by REQ-590 D-3; either way the entries below
/// must track it: when the pinned floor covers a sample again this test fails
/// and says to remove it.
///
/// * `base64.txt` — `o200k_base` encodes random base64 at ≈1.45 B/token, under
///   the 2 B/token floor, so `max(words × 3/2, bytes / 2)` falls ≈28% short of
///   the true count.
/// * `numeric_grid.txt` — REQ-590 AC-9's sample, and the more uncomfortable
///   entry: it is under the floor in **both** currencies at once (1.00 B/token
///   *and* 2.00 tokens/word against 1.5), so the estimate falls 25% short —
///   15,360 against 20,480. Base64 at least spends its byte budget fast enough
///   that a *full-budget* turn of it is small; this one is byte-light, so a
///   full-budget turn of it is admitted whole. That is the AC-9 finding, and
///   [`a_full_word_budget_turn_of_token_dense_byte_light_content_overruns_the_engine`]
///   is where it is stated in engine terms rather than estimator terms.
const KNOWN_UNCOVERED_AT_PINNED_FLOOR: [&str; 2] = ["base64.txt", "numeric_grid.txt"];

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

/// **REQ-590 AC-9 (BR-10): the quadrant the byte guard cannot see, measured
/// with a real tokenizer — and it does not fit.**
///
/// D-3 took the full engine window, so the local **word** half is exactly
/// saturating: `10,240 × 3/2 = 15,360 = usable` (ADR-6; the zero gap is pinned
/// at its definition site by
/// `budget::tests::the_local_word_budgets_slack_is_exactly_zero_by_design`).
/// Zero margin means every class denser than 1.5 real tokens per whitespace
/// word overruns the engine at full budget.
///
/// Ordinary Rust already is denser — 1.69 tokens/word. What saves those turns
/// is not the ratio but the **byte** guard: at 6.8 B/word, Rust spends the byte
/// budget after ≈4,800 words and never reaches the word budget. The uncovered
/// quadrant is therefore content that is dense in tokens and *light* in bytes,
/// which is what [`DENSE_BYTE_LIGHT_SAMPLE`] is.
///
/// # The measurement (o200k_base, tiktoken 0.14.0)
///
/// | | value |
/// |---|---|
/// | sample | 10,240 words / 20,480 bytes — a full-word-budget turn |
/// | admitted by the word guard | 10,240 ≤ 10,240 — exactly at the budget |
/// | admitted by the byte guard | 20,480 ≤ 32,768 — 12,288 bytes to spare |
/// | the budget claims | 15,360 provider tokens |
/// | it really costs | **20,480** provider tokens |
/// | overrun | **+5,120 tokens, 1.33× the engine's usable window** |
///
/// So a turn of this class passes both guards and is 33% over the window
/// before the 1,024-token generation is even counted.
///
/// **This finding did not move when D-4 did.** The byte half was 30,720 while
/// D-4 stood and is 32,768 since ADR-9 reversed it; the sample is 20,480 bytes
/// and is admitted at both, at the same 20,480 real tokens against the same
/// 15,360 usable. That is not a coincidence to note in passing — it is the
/// evidence ADR-9 turned on. No byte value in this range catches this class,
/// so lowering the byte half bought nothing here while costing a 6.25% refusal
/// regression on ordinary code. Only a real tokenizer catches it, which is what
/// this test is.
///
/// # The two halves no longer claim the same number, and the difference is
///   exactly the reservation (ADR-9)
///
/// While both halves were window-derived the pair was saturating in *both*
/// currencies — `bytes / 2 = usable` as well (ADR-6a). Since ADR-9 the byte
/// half is [`LOCAL_BUDGET_BYTES`](tetond::harness::budget::LOCAL_BUDGET_BYTES),
/// 32,768, which at the 2 B/token floor claims **16,384** provider tokens: the
/// engine's *whole* window, with nothing left for the reply. The word half
/// claims 15,360, the usable window. The gap between the two claims is
/// therefore `LOCAL_GENERATION_RESERVATION` exactly, 1,024 tokens.
///
/// **That 1,024-token overclaim is the residual ADR-9 knowingly accepted, not
/// a new defect.** It is the state the local route ran under before REQ-590
/// existed — 32,768 bytes has always claimed the whole window — and ADR-9's
/// reasoning is that closing it would have cost the very case the REQ was
/// opened for while catching nothing this test does not already catch. It is
/// asserted below as a stated equality rather than left as an inequality,
/// because a residual nobody can name the size of is one that grows.
///
/// # Why this is recorded rather than fixed
///
/// D-3 accepted it. The intended catches, in order: REQ-589's over-budget
/// offer where the guards do bite, and — for this quadrant, where they do not
/// — the engine's own typed **`context_length_exceeded`** outcome (BR-2,
/// ADR-8), which is why that path is not dead code on the local tier. This
/// test's job is to keep the size of the accepted risk a known number instead
/// of an adjective, and to redden if it changes in either direction.
///
/// # Why it cannot be written in whitespace words
///
/// An assertion phrased in the daemon's own `approx_tokens` would read
/// `10,240 ≤ 10,240` and pass identically whether the content ran at 1.2 or at
/// 2.0 real tokens per word — the two cases this test exists to tell apart.
/// The `tokens` column is the only figure here the estimator did not produce.
#[test]
fn a_full_word_budget_turn_of_token_dense_byte_light_content_overruns_the_engine() {
    let rows = load_rows();
    let row = rows
        .iter()
        .find(|r| r.file == DENSE_BYTE_LIGHT_SAMPLE)
        .unwrap_or_else(|| panic!("{DENSE_BYTE_LIGHT_SAMPLE} missing from token_counts.json"));
    let local = derive(BudgetInputs::local());

    // The ceiling the turn is measured against: what the *word* half claims,
    // which ADR-6's zero slack makes the engine's usable window exactly. That
    // equality is pinned at its definition site
    // (`budget::tests::the_local_word_budgets_slack_is_exactly_zero_by_design`);
    // here it is the denominator every figure below is quoted against.
    let usable = words_estimate(local.budget_tokens as u64);

    // **The residual ADR-9 accepted, pinned as a stated fact rather than
    // hidden.** The byte half is the constant, so at the 2 B/token floor it
    // claims the engine's *whole* window while the word half claims the usable
    // one. The difference is the generation reservation, to the token — and
    // saying so is the point: a byte-saturated local prompt is over the engine
    // by 1,024 tokens before it is assembled, it was over by the same 1,024
    // before REQ-590 existed, and nothing in this REQ closed it. ADR-9 records
    // why closing it was declined: the byte value that would have closed it
    // does not catch this sample either, and refuses ordinary code that serves
    // today.
    assert_eq!(
        local.budget_bytes,
        tetond::harness::budget::LOCAL_BUDGET_BYTES,
        "ADR-9: the local byte half is the constant. If it has become window-derived again, the \
         residual below is gone and this whole section needs rewriting rather than re-tuning"
    );
    assert_eq!(
        bytes_estimate(local.budget_bytes as u64) - usable,
        u64::from(LOCAL_GENERATION_RESERVATION),
        "ADR-9's residual has changed size. The byte half ({} B) claims {} provider tokens and \
         the word half claims {usable}; the two are supposed to differ by exactly the \
         {LOCAL_GENERATION_RESERVATION}-token generation reservation, because the byte half \
         bridges the *whole* window at the 2 B/token floor while the word half bridges the \
         usable one. A different number here means one of the halves moved — record the new \
         figure in ADR-9 rather than relaxing this assertion",
        local.budget_bytes,
        bytes_estimate(local.budget_bytes as u64)
    );

    // 1. The sample IS a full-budget turn, not a small file scaled up. The
    //    equality is the coupling: if the budget moves, AC-9's finding moves
    //    with it and has to be re-measured rather than re-derived.
    assert_eq!(
        row.words, local.budget_tokens as u64,
        "{DENSE_BYTE_LIGHT_SAMPLE} is sized to be exactly one turn at the full local word budget, \
         which is now {} words, not {}. Regenerate the sample at the new size — \
         tools/token_corpus/count.py documents the construction — rerun \
         `python3 tools/token_corpus/count.py`, and restate the finding in this doc comment. \
         Scaling the old numbers instead would report an extrapolation as a measurement",
        local.budget_tokens, row.words
    );

    // 2. The byte guard does not bind. This is precisely what makes the sample
    //    the uncovered quadrant rather than one more dense blob: base64 and
    //    minified JSON are caught here, and this is not.
    assert!(
        row.bytes < local.budget_bytes as u64,
        "{DENSE_BYTE_LIGHT_SAMPLE}: {} bytes against a {}-byte budget — the byte guard binds \
         first, so this sample no longer exercises the token-dense/byte-light quadrant AC-9 \
         exists for. Make it lighter in bytes per word, or AC-9 has no witness",
        row.bytes,
        local.budget_bytes
    );

    // 3. It is genuinely token-dense, measured. Pins the sample from below so
    //    it cannot be quietly diluted into agreeing with the 3/2 ratio.
    assert!(
        row.tokens >= row.words * 2,
        "{DENSE_BYTE_LIGHT_SAMPLE}: {:.3} tokens/word — the sample has been diluted below the \
         2.00 tokens/word it was authored at, and AC-9's headline number is no longer this \
         class's",
        row.tokens as f64 / row.words as f64
    );

    // 4. The finding itself.
    println!(
        "AC-9: {DENSE_BYTE_LIGHT_SAMPLE} — {} words / {} bytes admitted by a ({}, {}) budget; \
         claimed {usable} provider tokens, really {} ({:.2} tok/word, {:.2} B/token) — \
         overrun {} tokens, {:.2}x usable",
        row.words,
        row.bytes,
        local.budget_tokens,
        local.budget_bytes,
        row.tokens,
        row.tokens as f64 / row.words as f64,
        row.bytes as f64 / row.tokens as f64,
        row.tokens.saturating_sub(usable),
        row.tokens as f64 / usable as f64
    );
    assert!(
        row.tokens > usable,
        "AC-9's finding has changed and this test is now the stale half of it: a full-budget turn \
         of {DENSE_BYTE_LIGHT_SAMPLE} costs {} provider tokens against {usable} usable, i.e. it \
         FITS. That is good news, and it means the word half has regained margin — record the new \
         measurement in the REQ-590 architecture doc and invert this assertion rather than \
         deleting it",
        row.tokens
    );
}
