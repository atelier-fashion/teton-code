//! **LESSON-546, swept across the whole daemon: a one-home rule is a test, and
//! the test has to look everywhere the copy could be.**
//!
//! REQ-589 BR-7c proposes a *vendor's* context window into a user's config. The
//! rule (LESSON-456) is that each such window is written down once — in its
//! `provider_recipes` catalog entry, beside the `verified_on` date that says
//! when a human last read it off the vendor's documentation — and that
//! everything needing one reads the catalog. A literal copy elsewhere keeps
//! agreeing with the vendor only until somebody re-verifies the entry and does
//! not know the copy exists; then the two drift on different schedules and the
//! failure surfaces far away, as a route budgeted against a window the vendor
//! stopped serving.
//!
//! # Why this file exists beside `provider_recipes`' own sweep
//!
//! TASK-240 wrote the in-module version, and narrowed it to two files —
//! `provider_recipes.rs` and `harness/budget.rs` — on the grounds that a
//! daemon-wide sweep is impossible for these numbers, because `1_000_000` is
//! also micro-USD-per-USD in `cost/prices.rs` and `4_096` is Teton's own local
//! budget. The *observation* is true. The *conclusion* is not: a sweep can be
//! daemon-wide as long as it names the unrelated homes instead of hiding from
//! them, which is what [`KNOWN_UNRELATED_HOMES`] does.
//!
//! The narrowing mattered. Between TASK-240 and this file, REQ-589 grew a
//! proposal path through `harness/permissions.rs` (the offer's option labels,
//! which name the concrete write) and `runtime.rs` (the wiring that puts the
//! offer on a screen). A window typed into either was outside the two files the
//! narrowed sweep looks at, and would have shipped green. Verified against this
//! file: planting `1_050_000` in `harness/turn_loop.rs` reddens the test below
//! and leaves the in-module one passing.
//!
//! # Why the counts are pinned exactly, and what to do when this fails
//!
//! Because "presence" is not the rule. `harness/budget.rs` is allowed to say
//! `4_096` **once** — it is `LOCAL_BUDGET_TOKENS`, the token half of Teton's own
//! local pair, which happens to be the same number Ollama's server grants by
//! default. A presence-only allowance there would re-open exactly the hole this
//! test exists for, since `budget.rs` is where a proposal is composed and
//! therefore the likeliest place for a hard-coded vendor window to appear.
//!
//! So if this test fails, there are only two honest answers:
//!
//! 1. The new occurrence *is* a recipe window. Read it from
//!    `provider_recipes::recipe_for_model` instead of typing it.
//! 2. The new occurrence is a different fact that happens to share the number.
//!    Add it to [`KNOWN_UNRELATED_HOMES`] with a one-line note saying what it
//!    is. That note is the deliverable — it is what stops the next reader
//!    treating the collision as evidence the rule does not apply.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tetond::provider_recipes::recipe_catalog;

/// Every production occurrence of a catalog window that is **not** a catalog
/// entry: `(window, path relative to src/, how many, what it actually is)`.
///
/// Each row is a number that collides with a vendor's window and means
/// something else entirely. Naming them is what lets the sweep below cover the
/// whole daemon instead of retreating to the two files nobody expected a copy
/// in anyway.
const KNOWN_UNRELATED_HOMES: &[(u32, &str, usize, &str)] = &[
    (
        1_000_000,
        "cost/prices.rs",
        2,
        "`MICROS_PER_USD` and `TOKENS_PER_MTOK` — the two unit conversions the \
         price table is arithmetic in",
    ),
    (
        1_000_000,
        "runtime.rs",
        2,
        "the bytes-per-megabyte half of `500_000 * 1_000_000`, the free-disk \
         figure the install preflight assumes",
    ),
    (
        500_000,
        "runtime.rs",
        2,
        "the megabytes half of that same free-disk figure",
    ),
    (
        4_096,
        "harness/budget.rs",
        1,
        "`LOCAL_BUDGET_TOKENS` — the token half of Teton's own local pair, and \
         the collision ADR-6's clears-the-refusal guard exists for: it is \
         numerically Ollama's served default and is a different fact from it",
    ),
    (
        4_096,
        "harness/duty.rs",
        1,
        "`DUTY_MAX_TOKENS_REQUEST` — how many tokens a duty call may generate",
    ),
    (
        4_096,
        "harness/tools/docs.rs",
        1,
        "`MAX_TOPIC_BYTES` — a byte cap on a docs topic name",
    ),
    (
        4_096,
        "harness/tools/grep.rs",
        1,
        "`MAX_MATCH_LINE_BYTES` — a byte cap on one reported match line",
    ),
    (
        4_096,
        "mcp/client.rs",
        1,
        "a byte ceiling on a string read off an MCP peer",
    ),
    (
        4_096,
        "session_root.rs",
        1,
        "`GIT_FILE_MAX_BYTES` — how much of a `.git` file the locator reads",
    ),
    (
        4_096,
        "web/reduce.rs",
        1,
        "`REDUCTION_ENVELOPE_RESERVE_BYTES` — headroom kept for a reduction's \
         envelope",
    ),
];

/// The daemon's own `src/` directory.
fn daemon_src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Every `.rs` file under `dir`, recursively.
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap_or_else(|err| {
        panic!("unreadable source dir {}: {err}", dir.display());
    }) {
        let path = entry
            .unwrap_or_else(|err| panic!("unreadable entry under {}: {err}", dir.display()))
            .path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// `text` with its test modules and whole-line comments removed.
///
/// Both trims are the ones `call_sites::scan` makes, restated here because that
/// module is `#[cfg(test)]` inside the library and so is invisible from an
/// integration test. Both can only *shrink* what the sweep sees where they are
/// wrong — a `#[cfg(test)]` module that is not last truncates early, a trailing
/// comment is left in and over-counts — and over-counting fails loudly while
/// under-counting is guarded by the floor assertions below.
fn production_code(text: &str) -> String {
    let code = match text.find("\n#[cfg(test)]") {
        Some(at) => &text[..at],
        None => text,
    };
    code.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every production `.rs` under `src/`, as `(path relative to src/, code)`.
fn production_sources() -> Vec<(String, String)> {
    let root = daemon_src();
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    files.sort();
    let sources: Vec<(String, String)> = files
        .iter()
        .map(|path| {
            let text = std::fs::read_to_string(path)
                .unwrap_or_else(|err| panic!("unreadable source {}: {err}", path.display()));
            let rel = path
                .strip_prefix(&root)
                .expect("a file under src/")
                .to_string_lossy()
                .into_owned();
            (rel, production_code(&text))
        })
        .collect();
    assert!(
        sources.len() > 10,
        "the daemon source scan found only {} file(s) under {}. Every assertion below would \
         pass vacuously, so this is a failure of the scan rather than of the code it scans.",
        sources.len(),
        root.display()
    );
    sources
}

/// The Rust integer suffixes a window literal could wear.
///
/// Without these, `4_096usize` is invisible to a boundary-anchored search and a
/// second home spelled with a suffix lives forever — the inert-sweep failure
/// this project has now shipped twice (TASK-259's `budget::derive(` scan, which
/// could not see the unqualified call in the file it excluded).
const INT_SUFFIXES: &[&str] = &[
    "usize", "isize", "u128", "i128", "u64", "i64", "u32", "i32", "u16", "i16", "u8", "i8",
];

/// Occurrences of `needle` in `haystack` that are a whole number literal.
///
/// `4_096` is a substring of `14_096` and `4096` of `40960`; a plain substring
/// count would charge those to a recipe window. A trailing type suffix is part
/// of the literal, so it is stepped over rather than treated as a boundary.
fn standalone(haystack: &str, needle: &str) -> usize {
    let free = |c: Option<char>| !c.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
    haystack
        .match_indices(needle)
        .filter(|(at, _)| {
            if !free(haystack[..*at].chars().next_back()) {
                return false;
            }
            let after = &haystack[at + needle.len()..];
            let after = INT_SUFFIXES
                .iter()
                .find_map(|suffix| after.strip_prefix(suffix))
                .unwrap_or(after);
            free(after.chars().next())
        })
        .count()
}

/// `1000000` as Rust conventionally writes it: `1_000_000`.
fn grouped(n: u32) -> String {
    let plain = n.to_string();
    let digits = plain.len();
    plain
        .char_indices()
        .flat_map(|(at, digit)| {
            let separator = (at > 0 && (digits - at).is_multiple_of(3)).then_some('_');
            separator.into_iter().chain(std::iter::once(digit))
        })
        .collect()
}

/// Both spellings of `window`, counted together.
fn homes_in(code: &str, window: u32) -> usize {
    standalone(code, &window.to_string()) + standalone(code, &grouped(window))
}

/// **The one-home rule for every window the recipe catalog ships, swept across
/// the whole daemon.**
///
/// # Why this test exists
///
/// LESSON-546: the rule was previously a `grep` in a task file, and a grep has
/// no schedule, no owner and no failure mode — "the next copy of `89,127` ships
/// green" was written about exactly this shape. TASK-240 turned it into a test
/// and narrowed it to two files; this is the same rule with nowhere left to
/// hide. See this file's header for what the narrowing missed and why the
/// counts are pinned rather than merely the file set.
#[test]
fn every_recipe_window_has_one_home_across_the_whole_daemon() {
    const CATALOG: &str = "provider_recipes.rs";

    let sources = production_sources();
    assert!(
        sources.iter().any(|(rel, _)| rel == CATALOG),
        "the sweep did not find `{CATALOG}`, so it cannot be measuring the rule it is about"
    );

    // Counted per *distinct* window, because several vendors happen to declare
    // 1M: those are separate verified facts that coincide numerically, and each
    // is written where it belongs — in its own entry. What the count enforces is
    // that there is no *other* home.
    let mut declared: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    for recipe in recipe_catalog() {
        declared
            .entry(recipe.max_context)
            .or_default()
            .push(recipe.id_suggestion);
    }
    assert!(!declared.is_empty(), "the catalog declares no windows");

    for (window, vendors) in declared {
        let mut expected: Vec<(&str, usize)> = KNOWN_UNRELATED_HOMES
            .iter()
            .filter(|(w, ..)| *w == window)
            .map(|(_, rel, n, _)| (*rel, *n))
            .collect();
        expected.push((CATALOG, vendors.len()));
        // `production_sources` sorts by path and so does the assertion's left
        // side; sorting here keeps a row's position in the table above free.
        expected.sort();

        let found: Vec<(&str, usize)> = sources
            .iter()
            .filter_map(|(rel, code)| {
                let n = homes_in(code, window);
                (n > 0).then_some((rel.as_str(), n))
            })
            .collect();

        assert_eq!(
            found,
            expected,
            "{} is the context window {vendors:?} declare. It belongs in its catalog entry \
             and nowhere else: everything that needs a vendor's window reads \
             `provider_recipes::recipe_for_model`, so that the value and the `verified_on` \
             date that justifies it move together. If the new occurrence really is a \
             different fact that happens to share this number, record it in \
             `KNOWN_UNRELATED_HOMES` with a note saying what it is.\n\
             known unrelated homes for this number: {:?}",
            grouped(window),
            KNOWN_UNRELATED_HOMES
                .iter()
                .filter(|(w, ..)| *w == window)
                .map(|(_, rel, _, why)| (rel, why))
                .collect::<Vec<_>>()
        );
    }
}

/// **The sweep can actually see a second home — the anti-inert check.**
///
/// # Why this test exists
///
/// A one-home sweep fails silently in one direction only: if the matcher never
/// matches, every assertion above passes forever and the guard is decoration.
/// This project has shipped that twice — TASK-259's first draft searched for
/// `budget::derive(` while the file it excluded called `derive` unqualified, and
/// the same pass found a four-line-wrapped import that defeated a line-at-a-time
/// scan. So the matcher is exercised against a source it *must* flag, in every
/// spelling a window could plausibly be typed in, before it is trusted about
/// the real tree.
#[test]
fn the_sweep_flags_a_second_home_in_every_spelling_a_window_could_wear() {
    for planted in [
        "const COPY: u32 = 1_050_000;",
        "const COPY: u32 = 1050000;",
        "const COPY: u32 = 1_050_000u32;",
        "let copy = 1050000usize;",
        "if window == 1_050_000 { }",
    ] {
        assert_eq!(
            homes_in(&production_code(planted), 1_050_000),
            1,
            "the sweep cannot see a window written as `{planted}`, so a copy spelled that \
             way would live forever"
        );
    }

    // And it does not charge a longer number, or an identifier, to the window.
    for innocent in [
        "const OTHER: u32 = 11_050_000;",
        "const OTHER: u32 = 1_050_0001;",
        "const OTHER: &str = \"x1050000y\";",
    ] {
        assert_eq!(
            homes_in(&production_code(innocent), 1_050_000),
            0,
            "`{innocent}` was charged to a recipe window it is not"
        );
    }

    // The two trims really do trim: a window inside a test module or a comment
    // is not a production home, and counting one would make the table above
    // unmaintainable for the wrong reason.
    let with_test_module = "const REAL: u32 = 1_050_000;\n\
         // a comment naming 1_050_000\n\
         \n#[cfg(test)]\nmod tests {\n    const ALSO: u32 = 1_050_000;\n}\n";
    assert_eq!(
        homes_in(&production_code(with_test_module), 1_050_000),
        1,
        "the test-module and comment trims are not doing what the table above assumes"
    );
}
