//! Which categories the harness actually dispatches on today (REQ-558 ADR-A).
//!
//! REQ-558 declares all eleven categories and resolves all eleven, so the config
//! schema stabilizes once and the remaining call sites can be tagged later
//! without a second migration. But some of them had **no model call site at
//! all**, and a knob that silently does nothing invites a user to tune it —
//! LESSON-481's shape ("a gate that hides a feature from users also hides it
//! from the test suite"). So `teton policy show` marks such a category
//! `declared, no call site yet`, and this module is where that marker comes from.
//!
//! As of REQ-562 the marked set is **empty**: every declared category is
//! dispatched on. The module stays because the marker is derived rather than
//! trusted — an exhaustive match plus the source scan below — and a twelfth
//! category, or a call site someone deletes, still has to answer for itself.
//!
//! # The marker is only honest because a test derives it
//!
//! [`has_call_site`] is an exhaustive match, which makes a *new category*
//! impossible to add without answering the question. It cannot, on its own, make
//! a *new call site* impossible to add without updating the answer — and that is
//! the direction the list actually rots in. The test at the bottom of this file
//! closes that gap: it reads the daemon's own source, finds every routing call
//! site, works out which categories reach a router through them, and asserts the
//! result equals this match. Wire up a category and the test fails until the
//! marker follows — which is exactly how `triage` (REQ-561 TASK-060), `shell`
//! (TASK-061), `title` (TASK-062), `compact` (TASK-063) and finally `redact`
//! (REQ-562 TASK-070) arrived. It works in the other direction too: delete a
//! call site and the marker is caught still claiming the category is reached.
//!
//! That test is the load-bearing half of ADR-A. This match is just where its
//! answer is written down.

use teton_core::category::Category;

/// Whether any model call in the harness dispatches on `category` today.
///
/// Exhaustive on purpose: a twelfth category cannot be added without a decision
/// here, and the decision is a fact about the daemon's code rather than about
/// configuration — which is why it lives in `tetond` and not in `teton-core`.
#[must_use]
pub const fn has_call_site(category: Category) -> bool {
    match category {
        // `DaemonRuntime::classify_freeform` asks the resolver whether `route`
        // can be served, then `classify::run` makes the call (REQ-558 TASK-053).
        // The architecture's table listed `route` under "must be built"; it has
        // been.
        Category::Route => true,
        // `summarize_if_large`'s digest duty, routed rather than hardcoded to
        // the local engine (TASK-054).
        Category::Digest => true,
        // Turn completion. All four arrive at the same call, differing only in
        // what the classifier said (freeform) or what the phase maps to
        // (structured) — so they are reached together or not at all.
        Category::Edit | Category::Design | Category::Debug | Category::Review => true,
        // The `grep` tool's own duty: `GrepTool::refine` ranks the matches it
        // just found against the turn's request before they enter context
        // (REQ-561 TASK-060). Unreached until then — the hits were returned in
        // whatever order the filesystem walk produced.
        Category::Triage => true,
        // The `shell` tool's own duty: `ShellTool::refine` says what a command's
        // output means, on the two results a weak model cannot read for itself —
        // a failure, or output the 8,000-character cap truncated (REQ-561
        // TASK-061). REQ-558's ADR-I deferred this on the reading that `shell`
        // meant *deciding to run a command*, which indeed cannot be routed ahead
        // of the model's answer; BR-4b resolved it the other way round — the
        // category dispatches on **interpreting** the output, which happens after
        // the command has already run and is routable like any other duty.
        Category::Shell => true,
        // The session's own duty, and the only one of the five that belongs to
        // no tool: `DaemonRuntime::title_session` names a session from its first
        // substantive prompt, once for the session's whole life, and publishes
        // `session_titled` (REQ-561 TASK-062). `SessionSummary.title` was on the
        // wire long before anything populated it.
        Category::Title => true,
        // The context's own duty, and the second of the five that belongs to no
        // tool: `ContextManager::compact_if_pressured` asks which blocks a
        // pressured conversation may forget, at a soft fraction of the budget
        // and ahead of the unconditional `truncate_to_budget` (REQ-561 TASK-063,
        // ADR-4). The deterministic oldest-first drop is still what *enforces*
        // the budget — the duty only ever improves the choice, which is why
        // wiring it cannot weaken the gate.
        Category::Compact => true,
        // The last of the eleven, and the only one whose call site is not in
        // the harness at all: `RedactionGateImpl::redact_route` resolves it
        // inside the egress choke point, where the scan runs on the exact bytes
        // that would leave the machine (REQ-562 ADR-1, TASK-070). Unreached
        // until then — egress refused by provenance and nothing else, because a
        // regex pass with no model behind it is not a call site.
        Category::Redact => true,
    }
}

/// Reading the daemon's own source as text, for the tests that assert facts
/// about the code rather than about its behaviour.
///
/// Shared rather than copied: the derived-marker test here and the seam
/// assertions in [`crate::harness::duty`] (REQ-561 AC-8/AC-10) both walk the
/// daemon's sources and both need the same "production source only" rule. Two
/// spellings of that rule are two rules that drift, and a drifted one is a scan
/// that quietly stops seeing a file.
#[cfg(test)]
pub(crate) mod scan {
    use std::path::{Path, PathBuf};

    /// The daemon's own `src/` directory.
    pub(crate) fn daemon_src() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
    }

    /// Every `.rs` file under `dir`, recursively.
    ///
    /// A directory that vanishes between being listed and being descended into
    /// is skipped rather than fatal — BUG-159's race one level up from the read
    /// side. `path.is_dir()` is a separate syscall from the `read_dir` that
    /// follows it, so a `git checkout`/`git stash` removing a module directory
    /// in between panics a walk that has nothing to do with the change under
    /// test. Every other error stays loud.
    ///
    /// This tolerance can only ever *shrink* what a scan sees, so it is paired
    /// with [`production_sources`]'s floor assertion: a walk that silently found
    /// nothing would otherwise let every sweep assertion pass vacuously.
    pub(crate) fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let listing = match std::fs::read_dir(dir) {
            Ok(listing) => listing,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                eprintln!(
                    "call_sites::scan: {} vanished before it could be walked; skipped",
                    dir.display()
                );
                return;
            }
            Err(err) => panic!("unreadable source dir {}: {err}", dir.display()),
        };
        for entry in listing {
            let path = match entry {
                Ok(entry) => entry.path(),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => panic!("unreadable dir entry under {}: {err}", dir.display()),
            };
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// A file's source with its test modules removed.
    ///
    /// Every module in this crate puts `#[cfg(test)]` items last, so truncating
    /// at the first one is exact today and *conservative* if that ever changes:
    /// it can only shrink what a scan sees, which makes an assertion fail loudly
    /// rather than pass wrongly.
    /// # A named file is read loudly, but survives a rename window (BUG-159)
    ///
    /// This is the **loud half** of the tolerance split. [`production_sources`]
    /// sweeps every file and *skips* one that is genuinely gone, because nobody
    /// asked for it by name. A caller naming a specific module is the opposite
    /// case: if that module was renamed or deleted, the test must fail naming
    /// it, never pass against an empty string.
    ///
    /// So the only thing tolerated here is the microsecond window an atomic
    /// rename opens — an editor saving, a `git checkout` rewriting the file —
    /// which is a fact about *when* we looked, not about the source tree. One
    /// retry closes that window; a file that is still missing is fatal, which
    /// keeps a deleted module loud.
    pub(crate) fn production_source(path: &Path) -> String {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => std::fs::read_to_string(path)
                .unwrap_or_else(|err| {
                    panic!(
                        "source file {} is missing on a second read, so this is a deleted or \
                         renamed module rather than a save in flight: {err}",
                        path.display()
                    )
                }),
            Err(err) => panic!("unreadable source file {}: {err}", path.display()),
        };
        strip_test_modules(&text)
    }

    /// `text` truncated at its first `#[cfg(test)]` item. See
    /// [`production_source`] for why truncating is exact today and conservative
    /// if that changes.
    fn strip_test_modules(text: &str) -> String {
        match text.find("\n#[cfg(test)]") {
            Some(at) => text[..at].to_owned(),
            None => text.to_owned(),
        }
    }

    /// Every production `.rs` under `src/`, as `(path relative to src/, source)`,
    /// in sorted order.
    ///
    /// ## A file that vanishes mid-scan is skipped, not fatal (LESSON-489/BUG-159)
    ///
    /// [`rust_files`] lists the directory and this reads the results a moment
    /// later, and a file can disappear in between: an editor saving by
    /// atomic rename, a `git checkout` or `git stash` in another worktree
    /// sharing this checkout, a generated file being rewritten. That is a race
    /// in the **scan**, not a fact about the source tree, and crashing every
    /// caller over it makes a whole test binary fail for a reason no diff
    /// explains.
    ///
    /// The loud failure is kept exactly where it means something: a caller
    /// looking for a *specific* file still asserts its own presence
    /// (`.expect("this module is a production source")`), so a module that was
    /// renamed or deleted fails by name. What is skipped is only a file nobody
    /// asked for by name — and the skip is announced rather than silent.
    ///
    /// ## "Vanished" is re-checked, because several callers are set-based
    ///
    /// A `NotFound` on its own does not establish that a file is gone: the
    /// commonest cause is an editor's atomic-rename save, where the path is
    /// missing for microseconds and then back. Skipping on the first `NotFound`
    /// would therefore drop a file that *exists* — and the callers that sweep
    /// **every** production source for a property (the seam assertions, the
    /// no-printers checks) pass a little more vacuously for each file they did
    /// not see, silently. So the directory is re-listed: if the path is still
    /// there the read is retried and a second failure is fatal, and only a path
    /// that is genuinely absent from a fresh listing is skipped.
    ///
    /// ## The tolerance is floored, so it cannot become a vacuous pass
    ///
    /// Every skip above shrinks what the sweep sees, and the callers are
    /// set-based: each file they miss makes an "every source has property P"
    /// assertion pass a little more easily. A scan that saw *nothing* would pass
    /// all of them. The floor below turns that failure mode back into a loud
    /// one, so the race tolerance can never be mistaken for a green suite.
    pub(crate) fn production_sources() -> Vec<(String, String)> {
        let root = daemon_src();
        let mut files = Vec::new();
        rust_files(&root, &mut files);
        files.sort();
        let sources: Vec<(String, String)> = files
            .iter()
            .filter_map(|path| {
                let text = match std::fs::read_to_string(path) {
                    Ok(text) => text,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        let mut current = Vec::new();
                        rust_files(&root, &mut current);
                        if !current.contains(path) {
                            eprintln!(
                                "call_sites::scan: {} vanished between the directory listing \
                                 and the read, and is gone from a fresh listing; skipped",
                                path.display()
                            );
                            return None;
                        }
                        // Still there — the window was a rename, not a deletion.
                        // The tolerance is for a file that is gone, not for one
                        // this process could not read.
                        std::fs::read_to_string(path).unwrap_or_else(|err| {
                            panic!(
                                "source file {} is listed but unreadable: {err}",
                                path.display()
                            )
                        })
                    }
                    Err(err) => panic!("unreadable source file {}: {err}", path.display()),
                };
                let rel = path
                    .strip_prefix(&root)
                    .expect("a file under src/")
                    .to_string_lossy()
                    .into_owned();
                (rel, strip_test_modules(&text)).into()
            })
            .collect();
        assert!(
            sources.len() > 10,
            "the daemon source scan found only {} file(s) under {}. Every sweep assertion built \
             on this would pass vacuously, so this is a failure of the scan, not of the code it \
             scans.",
            sources.len(),
            root.display()
        );
        sources
    }

    /// `source` with whole-line comments removed.
    ///
    /// The derived-marker scan in this module deliberately reads comments —
    /// ADR-9 exists because it does. The seam assertions deliberately do not: an
    /// ADR quoted in a doc comment is not a second implementation. Trailing
    /// comments are left in place, which can only make a seam assertion
    /// over-count and fail loudly.
    pub(crate) fn code_only(source: &str) -> String {
        source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// How many times `needle` occurs in `haystack`.
    pub(crate) fn count(haystack: &str, needle: &str) -> usize {
        haystack.match_indices(needle).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeSet;

    use teton_core::category::{category_for_phase, JudgmentCategory};
    use teton_core::Phase;

    use super::scan::{daemon_src, production_source, production_sources};

    /// The `Router` methods that answer "where does this category go".
    ///
    /// The source scan below understands calls to exactly these. Asserting the
    /// set rather than assuming it is what keeps a *fifth* entry point from
    /// being added and silently going unscanned — at which point the scan would
    /// keep passing while missing the call site it exists to find, which is the
    /// same silent rot the hand-maintained list has.
    const ROUTER_ENTRY_POINTS: [&str; 4] = [
        "resolve",
        "resolution_for",
        "resolve_judgment",
        // Takes no category: the taint backstop pins the local tier whatever the
        // table says (BR-7), so it reaches no category through the table and
        // contributes nothing to the reached set.
        "resolve_local_pin",
    ];

    /// The reporting surface, which resolves **every** category and dispatches
    /// none of them.
    ///
    /// Excluded by name, and it must stay excluded: `teton policy show` asks
    /// about `triage` on every invocation, so counting its calls as call sites
    /// would mark all eleven reached and make the marker vacuous. It is
    /// recognized here rather than skipped silently so that the exclusion is a
    /// stated decision — `Router::table_report` is the only routing call in the
    /// daemon that is allowed to name no particular category.
    const REPORTING_ONLY: &str = "table_report";

    /// The three the scan reads a category out of.
    const CATEGORY_BEARING: [&str; 3] = ["resolve", "resolution_for", "resolve_judgment"];

    /// The argument text of the call whose `(` is at `open`, paren-balanced.
    fn argument(source: &str, open: usize) -> &str {
        let bytes = source.as_bytes();
        let mut depth = 0usize;
        for (i, b) in bytes.iter().enumerate().skip(open) {
            match b {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return &source[open + 1..i];
                    }
                }
                _ => {}
            }
        }
        panic!("unbalanced parentheses after byte {open}");
    }

    /// Every `Category::Variant` / `CoreCategory::Variant` named in `expr`.
    fn categories_named(expr: &str) -> Vec<Category> {
        let mut found = Vec::new();
        for (i, _) in expr.match_indices("Category::") {
            let rest = &expr[i + "Category::".len()..];
            let name: String = rest
                .chars()
                .take_while(char::is_ascii_alphanumeric)
                .collect();
            if let Some(c) = Category::ALL
                .into_iter()
                .find(|c| c.as_str().eq_ignore_ascii_case(&name))
            {
                found.push(c);
            }
        }
        found
    }

    /// The categories reached through one routing call, or `None` when the
    /// argument is opaque to a textual scan.
    fn reached_by(method: &str, arg: &str) -> Option<Vec<Category>> {
        // A judgment turn dispatches on whatever the classifier returned, and
        // the classifier's return type is exhaustive — so this call site reaches
        // all four by construction, no matter what the argument says.
        if method == "resolve_judgment" {
            return Some(
                JudgmentCategory::ALL
                    .into_iter()
                    .map(Category::from)
                    .collect(),
            );
        }
        let mut reached = categories_named(arg);
        // ADR-C's structured dispatch: `category_for_phase`, the total
        // phase→**one** category map, over every phase a session can be in.
        //
        // Deliberately not its one-to-many sibling `categories_for_phase`, which
        // is BR-10's *migration* expansion — that one turns `io` into four
        // categories because one retired knob became several, and using it here
        // would mark `title`, `compact` and `triage` reached by a call site that
        // dispatches on `digest` alone.
        if arg.contains("category_for_phase(") {
            reached.extend(Phase::ALL.into_iter().map(category_for_phase));
        }
        if reached.is_empty() {
            return None;
        }
        Some(reached)
    }

    /// ADR-A, enforced: the `declared, no call site yet` marker is compared
    /// against the daemon's actual call sites, not trusted.
    ///
    /// This is the test the architecture calls the load-bearing half of ADR-A.
    /// Wiring up a call site for `triage` — or removing the one for `digest` —
    /// fails it until [`has_call_site`] is updated to match, which is the
    /// intended prompt.
    #[test]
    fn the_unreached_marker_matches_the_daemons_actual_call_sites() {
        // `production_sources` rather than a second `rust_files` + per-file read:
        // this is a *sweep*, so it wants the sweep API's BUG-159 tolerance — a
        // file deleted between the listing and the read is not this test's
        // subject. Its own floor assertion replaces the `files.len() > 10` this
        // test used to make.
        let sources = production_sources();

        let mut derived: BTreeSet<&'static str> = BTreeSet::new();
        let mut opaque: Vec<String> = Vec::new();

        for (rel, source) in &sources {
            for method in CATEGORY_BEARING {
                let needle = format!("router.{method}(");
                for (at, _) in source.match_indices(&needle) {
                    // `router.resolve(` must not also match `router.resolve_judgment(`
                    // through its shorter prefix: the `(` is part of the needle,
                    // so it cannot.
                    let open = at + needle.len() - 1;
                    let arg = argument(source, open);
                    match reached_by(method, arg) {
                        Some(categories) => {
                            derived.extend(categories.into_iter().map(Category::as_str));
                        }
                        None => opaque.push(format!(
                            "{rel}: router.{method}({arg}) — the scan cannot tell which category \
                             this dispatches on"
                        )),
                    }
                }
            }
        }

        assert!(
            opaque.is_empty(),
            "a routing call site names its category in a way this scan cannot read, so the \
             `declared, no call site yet` marker can no longer be derived. Either name the \
             category literally at the call site or teach `reached_by` the new shape:\n  {}",
            opaque.join("\n  ")
        );

        let marked: BTreeSet<&'static str> = Category::ALL
            .into_iter()
            .filter(|c| has_call_site(*c))
            .map(Category::as_str)
            .collect();

        assert_eq!(
            derived,
            marked,
            "`has_call_site` disagrees with the daemon's own call sites. Categories the code \
             reaches but the marker calls unreached: {:?}. Categories the marker claims are \
             reached but nothing calls: {:?}.",
            derived.difference(&marked).collect::<Vec<_>>(),
            marked.difference(&derived).collect::<Vec<_>>(),
        );
    }

    /// The scan reads three of the router's resolving methods and knows the
    /// fourth carries no category. A fifth would be unscanned — and unscanned is
    /// how a derived fact quietly becomes a hand-maintained one again.
    #[test]
    fn the_scan_covers_every_router_entry_point() {
        let source = production_source(&daemon_src().join("router.rs"));
        let mut declared: BTreeSet<String> = BTreeSet::new();
        for (at, _) in source.match_indices("pub fn ") {
            let rest = &source[at + "pub fn ".len()..];
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if name.starts_with("resolve")
                || name.starts_with("resolution")
                || name == REPORTING_ONLY
            {
                declared.insert(name);
            }
        }
        let known: BTreeSet<String> = ROUTER_ENTRY_POINTS
            .iter()
            .chain(std::iter::once(&REPORTING_ONLY))
            .map(|s| (*s).to_owned())
            .collect();
        assert_eq!(
            declared, known,
            "the router grew or lost a resolving entry point; \
             `the_unreached_marker_matches_the_daemons_actual_call_sites` scans for \
             {CATEGORY_BEARING:?} and will miss anything else"
        );
    }

    /// The unreached set is stated once, here, so a reviewer can check it
    /// against the architecture's table without reading the match.
    ///
    /// Named for the list rather than its length (REQ-561 shrank it one category
    /// per task and REQ-562 emptied it, and a test whose *name* has to change
    /// with each one is a rename nobody wants five times).
    ///
    /// **The list is now empty**, which is a stronger assertion than it looks:
    /// every declared category has a call site, so the `declared, no call site
    /// yet` marker has nothing to mark. It is not deleted — the exhaustive match
    /// is what makes a twelfth category answer the question, and this row is
    /// what makes an answer of `false` visible to a reviewer.
    #[test]
    fn the_declared_unreached_categories_are_stated_once() {
        let unreached: Vec<&str> = Category::ALL
            .into_iter()
            .filter(|c| !has_call_site(*c))
            .map(Category::as_str)
            .collect();
        assert_eq!(
            unreached,
            Vec::<&str>::new(),
            "the unreached set changed; every category REQ-558 declared is now dispatched on"
        );
    }
}
