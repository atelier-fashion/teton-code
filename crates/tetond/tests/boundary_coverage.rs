//! **REQ-571 BR-7 / BR-8 (ADR-E) — boundary coverage drift is a build failure.**
//!
//! Every other suite in this directory proves something *about* a tool. This one
//! proves something about the *set* of tools: that no tool able to put file or
//! external content in front of a model exists without a boundary test naming it.
//!
//! It exists because of LESSON-432, where the uncovered tools turned out to be
//! exactly the vulnerable ones — coverage was decided by whoever was writing
//! tests that week, and the gap was invisible until it was a bug. A reviewer
//! cannot see a missing test; a build can. So this file follows the precedent
//! ADR-009 rule 3 set for frame markers: the drift is caught by the compiler and
//! the suite, not by someone remembering.
//!
//! # What is derived and what is claimed
//!
//! The **universe** of tools is derived, never listed:
//!
//! - from this crate's own source, at compile time — every `impl Tool for …` in
//!   `crates/tetond/src/harness/tools/`, which is what a new tool *is*;
//! - from the live registry, at run time — [`ToolRegistry::with_builtins`]'s
//!   names, so a registration that the source scan somehow missed still trips.
//!
//! Only the **coverage claim** is written down ([`COVERAGE`]), and it is compared
//! as a *set* against both derivations. A tool added without an entry fails; an
//! entry naming a test that does not exist fails. Neither half can drift alone,
//! which is the whole point — a hand-kept list that nothing checks is the guard
//! LESSON-443 warns about.
//!
//! # Why the sources are embedded rather than read
//!
//! `include_str!`, not `std::fs::read_to_string`. BUG-159 is the trap: a scan
//! that lists then reads panics when anything rewrites `src/` in between, which
//! is precisely what this repo's mutation-check convention (LESSON-441) does
//! between `cargo test` runs. Embedding removes the race instead of tolerating
//! it — the bytes scanned are by construction the ones that built this binary.
//!
//! Every scan below therefore carries **floors**: an extractor that has gone
//! blind must fail rather than report an empty set and pass vacuously, which is
//! the other half of BUG-159's lesson.

use std::collections::BTreeSet;

use tetond::harness::tools::{ToolRegistry, SKILL_TOOL_NAME, WEB_TOOL_NAME};
use tetond::mcp::namespaced_tool_name;

// ---------------------------------------------------------------------------
// Embedded sources (BUG-159: compile-time, not a runtime read)
// ---------------------------------------------------------------------------

/// Every source file of the tools module, as `(file name, contents)`.
///
/// The list is fixed because `include_str!` takes a literal path — and
/// [`every_tool_source_file_is_scanned`] is what keeps that from becoming a
/// blind spot: a new tool file must be declared in `mod.rs` to compile at all,
/// and that test compares the declarations against this list.
const TOOL_SOURCES: &[(&str, &str)] = &[
    ("mod.rs", include_str!("../src/harness/tools/mod.rs")),
    ("docs.rs", include_str!("../src/harness/tools/docs.rs")),
    ("edit.rs", include_str!("../src/harness/tools/edit.rs")),
    ("glob.rs", include_str!("../src/harness/tools/glob.rs")),
    ("grep.rs", include_str!("../src/harness/tools/grep.rs")),
    ("mcp.rs", include_str!("../src/harness/tools/mcp.rs")),
    (
        "projects.rs",
        include_str!("../src/harness/tools/projects.rs"),
    ),
    ("read.rs", include_str!("../src/harness/tools/read.rs")),
    ("shell.rs", include_str!("../src/harness/tools/shell.rs")),
    ("skill.rs", include_str!("../src/harness/tools/skill.rs")),
    ("walk.rs", include_str!("../src/harness/tools/walk.rs")),
    ("web.rs", include_str!("../src/harness/tools/web.rs")),
];

/// The boundary suites this file cites, as `(file name, contents)`.
///
/// Cited by name from [`COVERAGE`] and [`SPELLING_PAIRS`], so a test that is
/// renamed or deleted takes this suite red with it.
const SUITES: &[(&str, &str)] = &[
    ("egress_capture.rs", include_str!("egress_capture.rs")),
    ("provenance_egress.rs", include_str!("provenance_egress.rs")),
    ("mcp_egress.rs", include_str!("mcp_egress.rs")),
    ("web_lookup_egress.rs", include_str!("web_lookup_egress.rs")),
    ("skill_boundary.rs", include_str!("skill_boundary.rs")),
];

/// The privacy-boundary matcher's own source — the BR-8 half of this file.
///
/// Reached across the crate boundary deliberately: the matcher assertions live
/// in `teton-core`, their tool-layer twins live in `tetond`'s suites, and the
/// pairing is only checkable from a place that can see both.
const BOUNDARY_RS: &str = include_str!("../../teton-core/src/boundary.rs");

// ---------------------------------------------------------------------------
// The coverage claim (BR-7 / AC-12)
// ---------------------------------------------------------------------------

/// One tool that can surface external or file content, and the boundary tests
/// that cover it.
struct Covered {
    /// The `impl Tool for …` type in `crates/tetond/src/harness/tools/`. This is
    /// the key the source-derived universe is compared against.
    tool_type: &'static str,
    /// The name the model calls it by.
    ///
    /// `None` for MCP alone: an MCP tool's name is minted per server at
    /// discovery (`mcp__<server>__<tool>`), so there is no static name to
    /// assert — see [`the_mcp_naming_class_is_real`].
    registered: Option<&'static str>,
    /// Whether [`ToolRegistry::with_builtins`] registers it. `web` is `false`:
    /// it is opt-in and registered by its caller (REQ-563 D-1), and MCP tools
    /// are `false` because they are discovered, not built in.
    builtin: bool,
    /// A token the covering test's body must contain, so a citation cannot be
    /// satisfied by pointing at an unrelated passing test. For MCP this is the
    /// namespace prefix rather than a whole name.
    mention: &'static str,
    /// What this tool puts in front of a model — the reason it needs coverage
    /// at all. Prose, for the human deciding whether a *new* tool belongs here.
    surfaces: &'static str,
    /// The boundary tests covering it, as `(suite file, test fn)`. At least one
    /// (AC-12); more when one test cannot carry the whole claim.
    tests: &'static [(&'static str, &'static str)],
}

/// Every tool that can surface external or file content, with its coverage.
///
/// **There is no exemption arm, and that is deliberate.** Every tool in this
/// harness exists to bring something into a model's context — that is what a
/// tool *is* here — so "surfaces nothing" is not a case to leave a hole for in
/// advance. A future tool that genuinely surfaces nothing has to be argued for
/// in this file, in front of a reviewer, which is the conversation BR-7 wants
/// forced rather than skipped.
const COVERAGE: &[Covered] = &[
    Covered {
        tool_type: "ReadTool",
        registered: Some("read"),
        builtin: true,
        mention: "read",
        surfaces: "the bytes of one repo file, whole or windowed",
        tests: &[(
            "egress_capture.rs",
            "read_blocks_every_boundary_spelling_under_one_identity",
        )],
    },
    Covered {
        tool_type: "EditTool",
        registered: Some("edit"),
        builtin: true,
        mention: "edit",
        surfaces: "the text it replaced, which the model that issued it holds",
        tests: &[(
            "egress_capture.rs",
            "edit_blocks_every_boundary_spelling_under_one_identity",
        )],
    },
    Covered {
        tool_type: "GrepTool",
        registered: Some("grep"),
        builtin: true,
        mention: "grep",
        surfaces: "matching lines out of every file it searched",
        tests: &[(
            "provenance_egress.rs",
            "grep_matching_a_boundary_file_blocks_the_next_remote_turn",
        )],
    },
    Covered {
        tool_type: "GlobTool",
        registered: Some("glob"),
        builtin: true,
        mention: "glob",
        surfaces: "the paths of the files it enumerated — a file name is content",
        tests: &[(
            "provenance_egress.rs",
            "glob_enumerating_a_boundary_file_blocks_the_next_remote_turn",
        )],
    },
    Covered {
        tool_type: "ShellTool",
        registered: Some("shell"),
        builtin: true,
        mention: "shell",
        surfaces: "whatever a command wrote to stdout, from anywhere in the jail",
        tests: &[(
            "provenance_egress.rs",
            "shell_cat_of_a_boundary_file_blocks_the_next_remote_turn",
        )],
    },
    Covered {
        tool_type: "ProjectsTool",
        registered: Some("projects"),
        // Not a `with_builtins` tool: the daemon registers it per session,
        // because it needs the machine's registry — the same reason `skill`
        // and `web` are registered there.
        builtin: false,
        // The second entry whose `surfaces` is an argument rather than a
        // warning (REQ-584 BR-5). This tool *does* touch the filesystem — it
        // reads directory names — and the distinction the boundary cares about
        // is that it never opens a file, so `Sources(∅)` is a fact rather than
        // an omission. Checked, not asserted in prose: the cited test drives a
        // real turn against a repo that HAS a boundary file, and the next
        // remote turn goes out, which is only true if no content was read.
        mention: "projects",
        surfaces: "directory names from the machine's own project registry and dev \
                   folders — no file is opened, and therefore no provenance to carry",
        tests: &[(
            "provenance_egress.rs",
            "projects_touches_no_repo_file_and_leaves_the_next_remote_turn_free",
        )],
    },
    Covered {
        tool_type: "DocsTool",
        registered: Some("teton_docs"),
        builtin: true,
        // The one entry whose `surfaces` is an argument rather than a warning,
        // which is exactly why it is written down (REQ-577). This tool reads no
        // path and opens no socket: its bodies are `include_str!`d at compile
        // time, so `Sources(∅)` is a fact rather than an omission. The claim is
        // still checked rather than asserted in prose — the cited test drives a
        // real turn against a repo that *has* a boundary file, and the next
        // remote turn goes out, which is only true if nothing was touched.
        mention: "teton_docs",
        surfaces: "bundled documentation compiled into this binary — no repo file, no \
                   network, and therefore no provenance to carry",
        tests: &[(
            "provenance_egress.rs",
            "teton_docs_touches_no_repo_file_and_leaves_the_next_remote_turn_free",
        )],
    },
    Covered {
        tool_type: "WebTool",
        registered: Some(WEB_TOOL_NAME),
        builtin: false,
        mention: "web",
        surfaces: "the text of a page fetched from the network — external, untrusted",
        tests: &[(
            "web_lookup_egress.rs",
            "no_raw_page_bytes_reach_any_remote_provider_payload",
        )],
    },
    Covered {
        tool_type: "SkillTool",
        registered: Some(SKILL_TOOL_NAME),
        // Registered by `build_tools`, and only when the session's registry
        // holds a model-invocable skill (REQ-587 BR-2) — the `web` posture, for
        // the same kind of reason: presence is a fact `with_builtins` does not
        // have and should not acquire.
        builtin: false,
        mention: "skill",
        surfaces: "the body of a Markdown file the model is told to **follow** — from \
                   under the root for a project skill, and from `~/.claude` for a user \
                   skill, which is the one content class that reaches the model as \
                   instructions rather than as data",
        // Two rules, two tests, plus the control that keeps them meaning
        // something (BR-10, ADR-8). The second is the one `ToolOutcome::ok`'s
        // `Sources(∅)` default would have broken in silence.
        tests: &[
            (
                "skill_boundary.rs",
                "a_project_skill_mints_its_identity_and_pins_the_next_remote_turn",
            ),
            (
                "skill_boundary.rs",
                "a_user_skill_is_unknown_and_pins_under_a_boundary_it_never_touched",
            ),
        ],
    },
    Covered {
        tool_type: "McpToolHandle",
        registered: None,
        builtin: false,
        mention: "mcp",
        surfaces: "whatever a user-registered MCP server returns, local or remote",
        tests: &[
            (
                "mcp_egress.rs",
                "a_remote_mcp_call_touching_a_boundary_path_is_blocked_at_egress",
            ),
            (
                "mcp_egress.rs",
                "a_local_mcp_result_reading_a_boundary_path_is_blocked_from_later_remote_egress",
            ),
        ],
    },
];

// ---------------------------------------------------------------------------
// The BR-8 pairing (AC-13)
// ---------------------------------------------------------------------------

/// The matcher test in `teton-core/src/boundary.rs` that BR-8 retains.
const MATCHER_TEST: &str = "out_of_repo_paths_never_match_and_never_panic";

/// One retained matcher assertion and the tool-layer tests it is paired with.
struct SpellingPair {
    /// The assertion as `boundary.rs` writes it, matched verbatim. BR-8 retains
    /// these: they are correct *at the matcher layer*, and deleting one on the
    /// grounds that the tool layer now handles the spelling would remove the
    /// statement that the matcher is not the thing handling it.
    matcher_assertion: &'static str,
    /// Why the spelling never reaches the matcher, for a reader of either side.
    why_unreachable: &'static str,
    /// The tool-layer tests proving that, as `(suite file, test fn)`. Each is
    /// named in a comment in `boundary.rs` beside its assertion, and that
    /// comment is itself checked here — so neither half can be deleted alone.
    twins: &'static [(&'static str, &'static str)],
}

/// The two spellings `out_of_repo_paths_never_match_and_never_panic` pins, each
/// paired with the tool-layer tests that keep it hypothetical.
const SPELLING_PAIRS: &[SpellingPair] = &[
    SpellingPair {
        matcher_assertion: r#"m.match_path("/etc/secrets/x").is_none()"#,
        why_unreachable: "`ToolContext::resolve` canonicalizes an absolute \
                          in-root path to its repo-relative form before any \
                          identity is minted, so the matcher is never handed \
                          the absolute spelling",
        twins: &[
            (
                "egress_capture.rs",
                "read_blocks_every_boundary_spelling_under_one_identity",
            ),
            (
                "egress_capture.rs",
                "edit_blocks_every_boundary_spelling_under_one_identity",
            ),
            (
                "provenance_egress.rs",
                "a_session_tainted_by_an_absolute_spelling_reaches_the_pin_and_closes_the_web",
            ),
        ],
    },
    SpellingPair {
        matcher_assertion: r#"m.match_path("../secrets/x").is_none()"#,
        why_unreachable: "a `..` component is either resolved away inside the \
                          root or refused by the jail, so no `..`-bearing \
                          string survives to become a provenance id",
        twins: &[
            (
                "egress_capture.rs",
                "read_blocks_every_boundary_spelling_under_one_identity",
            ),
            (
                "egress_capture.rs",
                "edit_blocks_every_boundary_spelling_under_one_identity",
            ),
            (
                "provenance_egress.rs",
                "a_session_tainted_by_a_traversing_spelling_reaches_the_pin_and_closes_the_web",
            ),
        ],
    },
];

// ---------------------------------------------------------------------------
// Source extraction
// ---------------------------------------------------------------------------

/// The contents of `file` among `sources`, or a panic naming what is available.
fn source(sources: &[(&str, &'static str)], file: &str) -> &'static str {
    sources
        .iter()
        .find(|(name, _)| *name == file)
        .map(|(_, text)| *text)
        .unwrap_or_else(|| {
            panic!(
                "no embedded source for {file:?}; add it to the `include_str!` list \
                 (available: {:?})",
                sources.iter().map(|(n, _)| *n).collect::<Vec<_>>()
            )
        })
}

/// Everything in `text` before its `#[cfg(test)] mod …` test module.
///
/// A test-only fake (`mod.rs`'s `StubTool`) is not a tool the product ships, so
/// the universe is built from the production half only. The anchor is the
/// attribute *and* the `mod` line together, never the bare attribute: a lone
/// `#[cfg(test)]` item (a const, a helper fn) above a file's `impl Tool` block
/// would otherwise truncate the scan before the impl and silently drop the tool
/// from the universe — the one direction this suite must never fail in, and one
/// REQ-577 actually hit (`docs.rs`'s `MAX_DESCRIPTION_CHARS`).
/// [`a_cfg_test_item_above_the_impl_does_not_hide_the_tool_from_the_scan`] pins
/// the distinction. A file that loses the marker pair is scanned whole, which
/// can only make the universe *wider* — the fail-safe direction: the suite goes
/// red naming the extra type rather than quietly shrinking what it looks at.
fn production_half(text: &str) -> &str {
    match text.find("\n#[cfg(test)]\nmod ") {
        Some(at) => &text[..at],
        None => text,
    }
}

/// Every type with an `impl Tool for …` in `text`, ignoring comment lines.
fn tool_impl_types(text: &str) -> BTreeSet<String> {
    const NEEDLE: &str = "impl Tool for ";
    text.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .filter_map(|line| line.split_once(NEEDLE))
        .map(|(_, rest)| -> String {
            rest.chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect()
        })
        .filter(|ty| !ty.is_empty())
        .collect()
}

/// Every `mod <name>;` declared in `text`, ignoring comment lines.
fn declared_modules(text: &str) -> BTreeSet<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.starts_with("//"))
        .filter_map(|line| {
            let decl = line.strip_prefix("pub ").unwrap_or(line);
            let name = decl.strip_prefix("mod ")?.strip_suffix(';')?;
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
                .then(|| name.to_owned())
        })
        .collect()
}

/// A function definition found in a source file.
struct Definition {
    /// The line immediately above the signature — the attribute, for a test.
    preceding: String,
    /// The signature line through the closing brace at its own indent.
    body: String,
}

/// The definition of `fn <name>` in `text`, delimited by rustfmt's indentation:
/// the closing brace is the first later line that is exactly the signature's
/// indent followed by `}`. `None` when there is no such function.
fn definition(text: &str, name: &str) -> Option<Definition> {
    let lines: Vec<&str> = text.lines().collect();
    let signature = format!("fn {name}(");
    let (start, indent) = lines.iter().enumerate().find_map(|(index, line)| {
        let trimmed = line.trim_start();
        let at = trimmed.find(&signature)?;
        // Only qualifiers may precede it, so a line that merely calls the
        // function is not mistaken for its definition.
        let is_definition = trimmed[..at]
            .split_whitespace()
            .all(|word| matches!(word, "pub" | "const" | "async" | "unsafe"));
        is_definition.then(|| (index, line.len() - trimmed.len()))
    })?;
    let closer = format!("{}}}", " ".repeat(indent));
    let end = lines[start..].iter().position(|line| *line == closer)? + start;
    Some(Definition {
        preceding: start
            .checked_sub(1)
            .map(|prior| lines[prior].to_owned())
            .unwrap_or_default(),
        body: lines[start..=end].join("\n"),
    })
}

/// The definition of test `name` in suite `file`, or a panic explaining that the
/// citation has gone stale.
fn cited_test(file: &str, name: &str, cited_by: &str) -> Definition {
    let found = definition(source(SUITES, file), name).unwrap_or_else(|| {
        panic!(
            "{cited_by} cites `{file}::{name}` as its boundary coverage, and no such \
             test exists there. Either the test was renamed or deleted — in which \
             case restore the coverage — or the citation was wrong when it was \
             written."
        )
    });
    assert!(
        found.preceding.contains("test]") || found.preceding.contains("test("),
        "{cited_by} cites `{file}::{name}`, which is not a test — it is a helper. \
         A helper is only ever run by a test that decides what it means, so it is \
         not coverage: {:?}",
        found.preceding
    );
    found
}

// ---------------------------------------------------------------------------
// AC-12 — every content-surfacing tool has a boundary test
// ---------------------------------------------------------------------------

/// **REQ-571 AC-12 (BR-7).** Every tool that can surface external or file
/// content has at least one boundary test, and adding one without coverage
/// fails here.
///
/// The universe is the set of `impl Tool for …` types in the tools module,
/// because that is what adding a tool looks like — before it is registered,
/// before it is named, before anyone decides which suite should cover it. The
/// claim in [`COVERAGE`] is compared against it as a set, in both directions:
/// a tool with no entry fails, and an entry for a type that no longer exists
/// fails too, so the list cannot rot into fiction.
#[test]
fn every_content_surfacing_tool_has_a_boundary_test() {
    let implemented: BTreeSet<String> = TOOL_SOURCES
        .iter()
        .flat_map(|(_, text)| tool_impl_types(production_half(text)))
        .collect();

    // Floors first: a scan that has gone blind must fail rather than agree with
    // an empty claim (BUG-159).
    assert_eq!(
        TOOL_SOURCES.len(),
        12,
        "the embedded source list shrank; the scan below is narrower than the module"
    );
    for (file, text) in TOOL_SOURCES {
        assert!(
            text.len() > 500,
            "embedded source for {file} is {} bytes — that is not a tool module",
            text.len()
        );
    }
    for anchor in ["ReadTool", "WebTool", "McpToolHandle"] {
        assert!(
            implemented.contains(anchor),
            "the scan did not find `{anchor}`, which is a tool it has always found; \
             the extractor is broken rather than the module: {implemented:?}"
        );
    }

    let claimed: BTreeSet<String> = COVERAGE
        .iter()
        .map(|covered| covered.tool_type.to_owned())
        .collect();
    assert_eq!(
        implemented,
        claimed,
        "the set of tools and the set of tools with boundary coverage disagree. \
         A tool that can surface file or external content and has no boundary test \
         is the LESSON-432 shape this test exists to stop: give it a test in one of \
         {:?} and add its entry to `COVERAGE`. If it genuinely surfaces nothing, say \
         so in `COVERAGE`'s doc comment and argue it in review — do not delete this \
         assertion.",
        SUITES.iter().map(|(name, _)| *name).collect::<Vec<_>>()
    );

    // Each claim is real: the cited test exists, is a test, and is about the
    // tool that cites it.
    let mut cited: BTreeSet<(&str, &str)> = BTreeSet::new();
    for covered in COVERAGE {
        assert!(
            !covered.tests.is_empty(),
            "`{}` claims no covering test, and it surfaces {}",
            covered.tool_type,
            covered.surfaces
        );
        for (file, test) in covered.tests {
            let found = cited_test(file, test, covered.tool_type);
            assert!(
                found.body.to_lowercase().contains(covered.mention),
                "`{}` cites `{file}::{test}`, whose body never mentions {:?} — a test \
                 that does not touch the tool is not coverage of it",
                covered.tool_type,
                covered.mention
            );
            assert!(
                cited.insert((file, test)),
                "`{file}::{test}` is cited by two tools; one test standing in for two \
                 tools' coverage is how one tool's regression hides behind the other's \
                 assertions (LESSON-502)"
            );
        }
    }
    assert!(
        cited.len() >= COVERAGE.len(),
        "fewer distinct tests than tools, so some tool is uncovered"
    );
}

/// **REQ-571 AC-12, the second derivation.** The live registry registers nothing
/// the enumeration has not claimed.
///
/// The source scan above and this are deliberately redundant: the scan sees a
/// tool the moment it is written, and this sees it the moment it is *reachable*
/// by a model. A tool defined outside the tools module — which the scan's file
/// list would miss — is still caught here as soon as it is registered.
#[test]
fn the_builtin_registry_registers_nothing_the_enumeration_has_not_claimed() {
    let registry = ToolRegistry::with_builtins();
    let registered: BTreeSet<&str> = registry.names().into_iter().collect();

    // Non-vacuity: an empty registry would satisfy any subset claim.
    assert!(
        registered.len() >= 6,
        "`with_builtins` registered {} tools; the registry under test is not the \
         populated one",
        registered.len()
    );

    let claimed: BTreeSet<&str> = COVERAGE
        .iter()
        .filter(|covered| covered.builtin)
        .filter_map(|covered| covered.registered)
        .collect();
    assert_eq!(
        registered, claimed,
        "`ToolRegistry::with_builtins` and `COVERAGE` disagree about the built-in \
         tool set. A newly registered tool needs a boundary test and an entry \
         (BR-7); a removed one needs its entry removed."
    );

    // `web` is registered by its caller rather than by `with_builtins` (REQ-563
    // D-1), so its name is pinned against the constant the caller uses.
    let web = COVERAGE
        .iter()
        .find(|covered| covered.tool_type == "WebTool")
        .expect("the web tool is enumerated");
    assert_eq!(
        web.registered,
        Some(WEB_TOOL_NAME),
        "the opt-in web tool's registered name moved"
    );
    assert!(
        !web.builtin,
        "the web tool became a built-in; `with_builtins` is now the whole registry \
         claim and this exemption is stale"
    );
}

/// **REQ-571 AC-12, the blind-spot floor.** Every tool source file is scanned.
///
/// [`TOOL_SOURCES`] is a fixed list because `include_str!` takes a literal path,
/// and a fixed list is exactly the guard LESSON-443 warns about — unless
/// something checks it. A new tool in a new file must be declared in `mod.rs` to
/// compile at all, so the declarations are the authority and this compares them
/// against what is actually embedded.
#[test]
fn every_tool_source_file_is_scanned() {
    let declared = declared_modules(production_half(source(TOOL_SOURCES, "mod.rs")));
    assert!(
        declared.len() >= 8,
        "found {} module declarations in the tools module; the extractor is broken: \
         {declared:?}",
        declared.len()
    );

    let scanned: BTreeSet<String> = TOOL_SOURCES
        .iter()
        .filter(|(file, _)| *file != "mod.rs")
        .map(|(file, _)| file.trim_end_matches(".rs").to_owned())
        .collect();
    assert_eq!(
        declared, scanned,
        "a tool source file is declared in `mod.rs` but not embedded here (or the \
         reverse). An unscanned file is a tool the AC-12 enumeration cannot see: add \
         its `include_str!` to `TOOL_SOURCES`."
    );
}

/// **REQ-571 AC-12, the truncation anchor.** A `#[cfg(test)]` *item* above a
/// file's `impl Tool` block does not hide the tool from the scan.
///
/// [`production_half`] must cut at the test *module*, not at the first
/// `#[cfg(test)]` line. When it anchored on the bare attribute, a single
/// `#[cfg(test)] const` near the top of a tool file truncated the scan before
/// the impl, the tool vanished from the derived universe, and
/// [`every_content_surfacing_tool_has_a_boundary_test`] would agree with a
/// claim that never mentioned it — the silent-shrink direction, the exact
/// LESSON-432 shape this suite exists to stop. REQ-577 hit this for real
/// (`docs.rs`'s `MAX_DESCRIPTION_CHARS`) and worked around it by moving the
/// constant into the test module; this pins the fix so the workaround is no
/// longer load-bearing.
#[test]
fn a_cfg_test_item_above_the_impl_does_not_hide_the_tool_from_the_scan() {
    // The doc line above the item matters: it puts a newline before the
    // attribute, the position every real file's items occupy. Without it the
    // item sits at byte zero, where a newline-anchored marker could never
    // match, and this test would pass against the exact bug it exists to stop.
    let file = "\
//! A hypothetical tool module.

#[cfg(test)]
const CEILING: usize = 120;

pub struct HypotheticalTool;

impl Tool for HypotheticalTool {
}

#[cfg(test)]
mod tests {
    struct TestOnlyFake;

    impl Tool for TestOnlyFake {
    }
}
";
    let found = tool_impl_types(production_half(file));
    assert!(
        found.contains("HypotheticalTool"),
        "a `#[cfg(test)]` item above the impl truncated the scan and hid the \
         tool — the universe shrank silently, which is the failure direction \
         `production_half` exists to rule out: {found:?}"
    );
    assert!(
        !found.contains("TestOnlyFake"),
        "the scan crossed into the `#[cfg(test)] mod` test module; a test-only \
         fake is not a tool the product ships: {found:?}"
    );
}

/// **REQ-571 AC-12, grounding the MCP entry.** MCP tool names really are a
/// namespaced class rather than a fixed name.
///
/// [`COVERAGE`]'s MCP entry claims no registered name and matches on the `mcp`
/// prefix instead. That is only honest if the daemon actually mints MCP tool
/// names that way, which is what this pins — otherwise the entry could quietly
/// become a claim about nothing.
#[test]
fn the_mcp_naming_class_is_real() {
    let mcp = COVERAGE
        .iter()
        .find(|covered| covered.tool_type == "McpToolHandle")
        .expect("MCP results are enumerated");
    assert_eq!(
        mcp.registered, None,
        "an MCP tool's name is per-server; a static name here would be a fiction"
    );
    let minted = namespaced_tool_name("some-server", "read_file");
    assert!(
        minted.starts_with(mcp.mention),
        "an MCP tool is named {minted:?}, which does not start with {:?} — the \
         enumeration's MCP entry no longer identifies the class it stands for",
        mcp.mention
    );
}

// ---------------------------------------------------------------------------
// AC-13 — the matcher assertions and their tool-layer twins
// ---------------------------------------------------------------------------

/// **REQ-571 AC-13 (BR-8).** Each retained matcher assertion about an absolute
/// or `..`-bearing path is paired with a tool-layer test proving that spelling
/// cannot reach the matcher — and the pairing is checked from both ends.
///
/// The two halves say different things and are both needed:
///
/// - the matcher assertion says an out-of-repo *string* matches no repo-relative
///   glob, which is true of the matcher and stays true whatever the tools do;
/// - the tool-layer test says no such string is ever minted as an identity in
///   the first place, so the matcher's answer is never the thing standing
///   between a boundary file and the wire.
///
/// Deleting either alone would leave the other looking like the whole story —
/// a matcher assertion that reads as a guarantee it does not make, or a tool
/// test with no statement of what the layer beneath it does. So this fails if
/// the assertion goes, if the twin goes, **or** if the comment in `boundary.rs`
/// naming the twin goes.
#[test]
fn each_out_of_repo_matcher_assertion_is_paired_with_a_tool_layer_test() {
    // Floor: the matcher test still exists and still has a body to assert about.
    let matcher = definition(BOUNDARY_RS, MATCHER_TEST).unwrap_or_else(|| {
        panic!(
            "`teton-core::boundary::tests::{MATCHER_TEST}` is gone. BR-8 retains it: \
             it is correct at the matcher layer, and the tool-layer tests that make \
             its spellings unreachable are a different claim, not a replacement."
        )
    });
    assert!(
        matcher.body.len() > 200,
        "the matcher test is {} bytes; it has been hollowed out rather than kept",
        matcher.body.len()
    );
    assert!(
        !SPELLING_PAIRS.is_empty(),
        "no pairs declared, so this test asserts nothing"
    );

    for pair in SPELLING_PAIRS {
        assert!(
            matcher.body.contains(pair.matcher_assertion),
            "`{MATCHER_TEST}` no longer asserts `{}`. BR-8 retains these assertions; \
             the tool layer making the spelling unreachable ({}) is why they are \
             hypothetical, not why they are unnecessary.",
            pair.matcher_assertion,
            pair.why_unreachable
        );
        assert!(
            !pair.twins.is_empty(),
            "`{}` is paired with nothing",
            pair.matcher_assertion
        );

        for (file, test) in pair.twins {
            // The link, as written in `boundary.rs` beside the assertion. This is
            // the half that makes the pairing explicit rather than remembered:
            // a reader of the matcher test is told which tool test it depends on,
            // and deleting that comment fails here.
            assert!(
                BOUNDARY_RS.contains(test),
                "`boundary.rs` does not name `{file}::{test}` beside its \
                 `{}` assertion. AC-13 wants the pairing visible from both sides, so \
                 neither half can be deleted alone — restore the comment.",
                pair.matcher_assertion
            );

            let twin = cited_test(file, test, MATCHER_TEST);
            assert!(
                twin.body.contains("spelling"),
                "`{file}::{test}` is paired with `{}` but says nothing about \
                 spellings; it is no longer the test that proves the spelling cannot \
                 reach the matcher",
                pair.matcher_assertion
            );
        }
    }
}

// ---------------------------------------------------------------------------
// REQ-583 — one walk policy (AC-18) and no "repository" in the rendered docs (AC-6)
// ---------------------------------------------------------------------------

/// **REQ-583 AC-18 (BR-11), the source half.** No walker declares a private
/// skip list: outside `walk.rs`, no tool source names a `SKIP_DIRS`-shaped
/// constant, and both walkers drive their recursion through `walk::visit`.
///
/// The behavioural half — that the walkers honour every name in the shared
/// definition — lives beside each tool
/// (`glob::tests::glob_is_built_from_the_shared_walk_definition`,
/// `grep::tests::grep_is_built_from_the_shared_walk_definition`). This
/// half is what stops the next walker from quietly bringing its own list back:
/// BR-11 says one skip set, one media set, one budget, and a second copy is a
/// build failure rather than a review comment.
#[test]
fn no_walker_declares_a_private_skip_list_and_both_walk_through_the_shared_driver() {
    // Floor: the shared definition really is in `walk.rs` and really is what
    // the scan below would catch elsewhere.
    let walk = production_half(source(TOOL_SOURCES, "walk.rs"));
    for anchor in [
        "pub const WALK_SKIP_DIRS",
        "pub const HOME_TOP_LEVEL_SKIPS",
        "pub const MEDIA_BUNDLE_SUFFIXES",
        "pub struct WalkBudget",
        "pub fn visit(",
    ] {
        assert!(
            walk.contains(anchor),
            "`walk.rs` no longer declares `{anchor}`; the shared definition moved and \
             this scan no longer knows what a private copy would look like"
        );
    }

    // A `const`/`static` whose name says SKIP, outside `walk.rs`, is a private
    // skip list — the exact shape `glob.rs` and `grep.rs` each carried before
    // ADR-3. Comment lines are ignored; the test modules are cut off (a test
    // may import the shared constants by name).
    let is_private_skip_decl = |line: &str| {
        let line = line.trim_start();
        if line.starts_with("//") {
            return false;
        }
        let is_decl = [
            "const ",
            "pub const ",
            "pub(crate) const ",
            "static ",
            "pub static ",
        ]
        .iter()
        .any(|prefix| line.starts_with(prefix));
        is_decl && line.contains("SKIP")
    };
    for (file, text) in TOOL_SOURCES {
        if *file == "walk.rs" {
            continue;
        }
        let production = production_half(text);
        let offenders: Vec<&str> = production
            .lines()
            .filter(|line| is_private_skip_decl(line))
            .collect();
        assert!(
            offenders.is_empty(),
            "{file} declares a skip list of its own: {offenders:?}. BR-11: the skip \
             set, the media set and the budget are defined once in `walk.rs` and read \
             through `ToolContext::walk_policy()`."
        );
        assert!(
            !production.contains("SKIP_DIRS"),
            "{file} names a `SKIP_DIRS` in its production half; the walkers read the \
             policy off the context rather than naming the constant (BR-11)"
        );
    }

    for walker in ["glob.rs", "grep.rs"] {
        let text = production_half(source(TOOL_SOURCES, walker));
        assert!(
            text.contains("walk::visit("),
            "{walker} no longer walks through `walk::visit`; a walker with its own \
             recursion is a walker with its own budget, skip set and trailer (BR-10/11)"
        );
        assert!(
            text.contains("ctx.walk_policy()"),
            "{walker} does not read the context's walk policy; the AC-14 budget seam \
             and the shared definition both ride on it"
        );
        assert!(
            !text.contains("fn walk(") && !text.contains("fn search("),
            "{walker} still carries a private recursive walker"
        );
    }
}

/// **REQ-583 AC-6, the rendered half.** No tool description or argument
/// schema the model is shown says "repository": the tools are jailed to a
/// *session root*, which may be a home folder or `/`, and a description that
/// promises a repository misdescribes every non-project session.
///
/// Asserted over the registry's rendered docs — the bytes the prompt carries —
/// rather than over the source strings, so a description assembled at runtime
/// (`teton_docs`'s is) is covered too.
#[test]
fn no_rendered_tool_doc_says_repository() {
    let docs = ToolRegistry::with_builtins().docs(None);
    assert!(
        docs.len() > 500 && docs.contains("- glob:") && docs.contains("- shell:"),
        "the rendered docs are not what this test expects to scan: {docs:?}"
    );
    for word in ["repository", "Repository", "Repo-relative", "repo-relative"] {
        assert!(
            !docs.contains(word),
            "the rendered tool docs say {word:?}; the tools are jailed to a session \
             root, not a repository (BR-2/AC-6):\n{docs}"
        );
    }
}
