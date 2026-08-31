//! REQ-602 AC-7 — a `runtime::…` path written in a comment still resolves.
//!
//! REQ-599 split a 14k-line module into a tree. Every comment that had pointed
//! a reader at `runtime::tests::dispatch::redact::…` kept saying so, and the
//! module it named stopped existing the moment `dispatch` moved under `duty`.
//! Nothing failed. A cross-reference is the one kind of documentation that can
//! go wrong silently, because the compiler never reads it and the reader who
//! does has no way to tell a moved path from a deleted test.
//!
//! # What this counts
//!
//! Two counting rules, stated here because the figures in REQ-602's own spec
//! were re-derived three times before they were right (LESSON-593):
//!
//! - **A path** is a `runtime::…::…` token appearing in a *comment* (`//`,
//!   `///`, `//!`) in the corpus below, or anywhere in `docs/*.md`. Code is not
//!   scanned: a path in code either compiles or does not.
//! - **The corpus** is `crates/*/src` and `crates/*/tests`, plus `docs/`. It
//!   deliberately excludes `.adlc/specs/` and `.adlc/knowledge/`. Those are a
//!   *historical record* — REQ-574's requirement describes the tree as it stood
//!   when REQ-574 shipped, and rewriting it to match a later refactor would
//!   make it lie about its own moment. Eight stale paths live there and are
//!   meant to.
//!
//! # How a path is resolved
//!
//! By building the module tree of `crates/tetond/src/runtime/` from disk:
//!
//! - a file's path gives its module prefix (`duty.rs` → `runtime::duty`,
//!   `nested/mod.rs` → `runtime::nested`), so `mod x;` declarations need no
//!   parsing;
//! - inside a file, nesting comes from **indentation**, not brace counting.
//!   The tree is rustfmt-formatted, so a `mod x {` at four spaces is one level
//!   deep, full stop. Brace depth would have to model braces inside string and
//!   char literals to get the same answer, and would fail quietly when it got
//!   one wrong.
//!
//! # Mutations run against this check
//!
//! Recorded because a guard nobody has seen fail is a guard nobody has tested.
//! Each was reverted; each failure below is the message that actually appeared,
//! not the one predicted. Every one of these compiled — a compile error also
//! reads as `FAILED`, and mistaking one for a caught mutation is how this REQ
//! previously recorded a pass it had not earned.
//!
//! | mutation | what went red |
//! |---|---|
//! | typo one cited path (`write_config_atomically` → `…atomicaly`) | that path, named with its file and line |
//! | `let structural = true` — stop tracking string literals | `runtime::tests::a_typed_project_skill_is_acknowledged_first::…`, 13k lines from any fixture |
//! | skip the `use x::*` re-exports | `runtime::LOCAL_ENGINE_N_CTX` |
//! | stop treating `impl` as a scope | `runtime::DaemonRuntime::accept_invocation` |
//! | silence the citation scanner | the citation vacuity floor, at 0 paths |
//! | silence the module-tree parser | the declaration vacuity floor, at 0 declarations |
//!
//! ## What adversarial review added, after the table above was written
//!
//! Every one is a parse that was wrong in the *permissive* direction — the
//! check stayed green while seeing less — except the first, which was wrong in
//! both directions at once:
//!
//! | defect | consequence |
//! |---|---|
//! | `impl<'a> Type<'a> {` skipped: splitting on the first `<` yields an empty subject | `runtime::views::WebSetupAnswers::from_preview` read as broken, while `runtime::views::from_preview` — naming a free function that does not exist — resolved |
//! | `runtime::a::{b, c}` checked only as far as `a` | renaming `b` or `c` left the guard green; three such citations exist and TASK-305 rewrote all three |
//! | `r"…"` (zero hashes) parsed as a plain string | `r"C:\"` leaves the scanner inside a string for the rest of the file, dropping every later declaration |
//! | `at += 1` in the no-literal arm | a non-ASCII char outside a string (`if c == '—'`) panics mid-codepoint |
//! | `trim_end_matches("mod")` on the path *string* | a future `runtime/submod.rs` becomes `runtime::sub` |
//! | `static mut G` | registers an item named `mut` |
//! | corpus hardcoded to three crates; citation floor a bare `>= 15` | losing `crates/tetond/tests` drops 18 citations and still clears the floor |
//!
//! The corpus is now enumerated from disk and the citation floor is keyed on
//! **shape** — each sub-corpus that carries citations today must still carry
//! them — because a count floor is exactly what a lost directory walks under.
//!
//! The middle two matter most: both are *modelling* choices, and getting either
//! wrong produces a list of false positives rather than a silent pass — which
//! is the failure that gets a check deleted rather than fixed.
//!
//! The walk is recursive (REQ-602 BR-4). A local copy of the walker, matching
//! the pattern five sibling integration tests already use: the canonical
//! `call_sites::scan::rust_files` is `#[cfg(test)]`-gated inside the lib and so
//! is unreachable from an integration test, which links the lib compiled
//! without that cfg.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/<crate> has a workspace root two levels up")
        .to_path_buf()
}

fn rust_files_recursive(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(listing) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in listing.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files_recursive(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn files_recursive(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    let Ok(listing) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in listing.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files_recursive(&path, ext, out);
        } else if path.extension().is_some_and(|e| e == ext) {
            out.push(path);
        }
    }
}

/// Everything the `runtime` module tree defines: modules, types and items.
///
/// All in one set because a comment cites them the same way — a reader writing
/// `runtime::duty::dispatch` may mean the module, and one writing
/// `runtime::views::tests::a_test` means a function. Distinguishing them would
/// buy nothing and would need the citation to say which it meant.
fn declared_paths() -> BTreeSet<String> {
    let dir = workspace_root().join("crates/tetond/src/runtime");
    let mut files = Vec::new();
    rust_files_recursive(&dir, &mut files);
    files.sort();

    let mut out = BTreeSet::new();
    for file in &files {
        let relative = file
            .strip_prefix(&dir)
            .expect("walked from `dir`")
            .to_string_lossy()
            .into_owned();
        // `mod.rs` → `runtime`; `duty.rs` → `runtime::duty`;
        // `nested/mod.rs` → `runtime::nested`.
        // By path component, not by string suffix: `trim_end_matches("mod")`
        // turns a future `runtime/submod.rs` into `runtime::sub`, and every
        // citation into it then reads as broken.
        let mut parts: Vec<&str> = relative.trim_end_matches(".rs").split('/').collect();
        if parts.last() == Some(&"mod") {
            parts.pop();
        }
        let stem = parts.join("::");
        let prefix = if stem.is_empty() {
            "runtime".to_owned()
        } else {
            format!("runtime::{stem}")
        };
        out.insert(prefix.clone());

        let text = std::fs::read_to_string(file)
            .unwrap_or_else(|err| panic!("unreadable {}: {err}", file.display()));

        // (indent, segment) for each enclosing `mod` or `impl` block. `impl` is
        // a scope here because a comment cites a method as
        // `runtime::DaemonRuntime::persist_web_tier` — through the type, which
        // is how a reader finds it, even though Rust's module path has no such
        // segment.
        let mut stack: Vec<(usize, String)> = Vec::new();
        let mut literal = Literal::None;
        for line in text.lines() {
            let (structural, next) = scan_literals(line, literal);
            literal = next;
            if !structural {
                continue;
            }
            let indent = line.len() - line.trim_start().len();
            let code = line.split("//").next().unwrap_or("").trim();
            if code.is_empty() {
                continue;
            }
            while stack.last().is_some_and(|(at, _)| *at >= indent) {
                stack.pop();
            }
            let scoped = |name: &str, stack: &[(usize, String)]| {
                let mut path = prefix.clone();
                for (_, segment) in stack {
                    path.push_str("::");
                    path.push_str(segment);
                }
                path.push_str("::");
                path.push_str(name);
                path
            };
            if let Some(name) = opens_a_module(code) {
                out.insert(scoped(&name, &stack));
                stack.push((indent, name));
            } else if let Some(name) = opens_an_impl(code) {
                stack.push((indent, name));
            } else if let Some(name) = declared_item(code) {
                out.insert(scoped(&name, &stack));
            }
        }
    }

    for submodule in glob_reexports(&dir) {
        alias_glob(&mut out, &submodule);
    }
    alias_split_impls(&mut out);
    out
}

/// A method reached through its type, wherever the `impl` block lives.
///
/// REQ-599 ADR-3 establishes that an inherent `impl` may be split across
/// modules of the same crate, and REQ-600 does exactly that: `DaemonRuntime` is
/// declared in `mod.rs` while sixteen of its methods live in `turn.rs`. Rust
/// reaches those methods as `runtime::DaemonRuntime::run_prompt_turn` — through
/// the *type*, which has one canonical path — and comments cite them that way,
/// correctly.
///
/// Recording them only under the impl block's own module would report every
/// such citation as broken the moment a method moved, which is the opposite of
/// what this check is for: the method did not move relative to its type, and a
/// reader following the citation still arrives.
///
/// So for every `runtime::<module>::<Type>::<method>` whose `<Type>` is
/// declared elsewhere in the tree, the method is also recorded under the type's
/// declaring path.
fn alias_split_impls(paths: &mut BTreeSet<String>) {
    // Where each type is declared: the shortest path ending in its name.
    let mut declared_at: BTreeMap<String, String> = BTreeMap::new();
    for path in paths.iter() {
        let Some(name) = path.rsplit("::").next() else {
            continue;
        };
        if !name.starts_with(|c: char| c.is_uppercase()) {
            continue;
        }
        let entry = declared_at
            .entry(name.to_owned())
            .or_insert_with(|| path.clone());
        if path.matches("::").count() < entry.matches("::").count() {
            *entry = path.clone();
        }
    }

    let mut aliases = Vec::new();
    for path in paths.iter() {
        let segments: Vec<&str> = path.split("::").collect();
        // `runtime::<module>::<Type>::<method>` — at least four segments, with
        // a capitalised one directly before the leaf.
        let (Some(method), Some(ty)) = (
            segments.last(),
            segments.len().checked_sub(2).and_then(|i| segments.get(i)),
        ) else {
            continue;
        };
        if segments.len() < 4 || !ty.starts_with(|c: char| c.is_uppercase()) {
            continue;
        }
        if let Some(home) = declared_at.get(*ty) {
            if home != &segments[..segments.len() - 1].join("::") {
                aliases.push(format!("{home}::{method}"));
            }
        }
    }
    paths.extend(aliases);
}

/// The submodules `runtime/mod.rs` re-exports wholesale.
///
/// `pub(crate) use engine::*;` makes `runtime::startup_lifecycle` a real path
/// even though the function is declared in `engine.rs` — and comments cite it
/// that way, correctly. Without this, every such citation reads as stale and
/// the check's first run is a list of false positives, which is how a check
/// gets disabled.
fn glob_reexports(dir: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(dir.join("mod.rs")).expect("runtime/mod.rs is readable");
    let mut out = Vec::new();
    for line in text.lines() {
        let code = line.split("//").next().unwrap_or("").trim();
        let Some(rest) = strip_visibility(code) else {
            continue;
        };
        let Some(rest) = rest.strip_prefix("use ") else {
            continue;
        };
        if let Some(name) = rest.strip_suffix("::*;") {
            if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                out.push(name.to_owned());
            }
        }
    }
    out
}

/// Lift a glob-imported submodule's names into `runtime::`.
///
/// Direct children, plus one level under any child whose name is a type — the
/// impl-block scope above put methods there, and `runtime::WebTaintOverride`
/// being reachable while `runtime::RedactionGateImpl::redact_route` is not
/// would be an arbitrary distinction to a reader writing either.
fn alias_glob(paths: &mut BTreeSet<String>, submodule: &str) {
    let root = format!("runtime::{submodule}::");
    let children: Vec<String> = paths
        .iter()
        .filter_map(|p| p.strip_prefix(&root))
        .filter(|rest| !rest.contains("::"))
        .map(str::to_owned)
        .collect();
    for child in children {
        let is_type = child.starts_with(|c: char| c.is_uppercase());
        if is_type {
            let under = format!("{root}{child}::");
            let methods: Vec<String> = paths
                .iter()
                .filter_map(|p| p.strip_prefix(&under))
                .filter(|rest| !rest.contains("::"))
                .map(str::to_owned)
                .collect();
            for method in methods {
                paths.insert(format!("runtime::{child}::{method}"));
            }
        }
        paths.insert(format!("runtime::{child}"));
    }
}

/// Where a line sits relative to a string literal that spans lines.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Literal {
    None,
    /// Inside `"…"`, which is what this crate's TOML fixtures use.
    Plain,
    /// Inside `r#"…"#`, carrying the hash count that closes it.
    Raw(usize),
}

/// Whether `line` *begins* outside any literal, and the state it leaves behind.
///
/// Indentation is the nesting signal here (rustfmt makes it exact, and brace
/// counting would have to model these same literals to do as well). That trades
/// one problem for another: the config fixtures in `runtime/mod.rs` are
/// multi-line strings whose TOML sits at column 0, and a column-0 line reads as
/// "back to top level". The first draft of this test therefore reported
/// `runtime::tests::web_setup_flow` — a module 13,000 lines above the failure —
/// as a broken path. So literals are tracked rather than assumed away.
fn scan_literals(line: &str, mut state: Literal) -> (bool, Literal) {
    let structural = state == Literal::None;
    let bytes = line.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        match state {
            Literal::Raw(hashes) => {
                let close = format!("\"{}", "#".repeat(hashes));
                match line[at..].find(&close) {
                    Some(found) => {
                        state = Literal::None;
                        at += found + close.len();
                    }
                    None => return (structural, state),
                }
            }
            Literal::Plain => {
                if bytes[at] == b'\\' {
                    at += 2;
                } else {
                    if bytes[at] == b'"' {
                        state = Literal::None;
                    }
                    at += 1;
                }
            }
            Literal::None => {
                if line[at..].starts_with("//") {
                    return (structural, Literal::None);
                }
                // A quote inside a char literal opens nothing.
                if line[at..].starts_with("'\"'") {
                    at += 3;
                    continue;
                }
                if let Some(hashes) = raw_opener_at(line, at) {
                    state = Literal::Raw(hashes);
                    at += 1 + hashes + 1;
                    continue;
                }
                if bytes[at] == b'"' {
                    state = Literal::Plain;
                }
                // Advance by a whole character. Stepping one byte and then
                // slicing `line[at..]` panics mid-codepoint on any non-ASCII
                // outside a string — `if c == '—'` is enough — and the panic
                // names a byte offset, not the real problem.
                at += line[at..].chars().next().map_or(1, |c: char| c.len_utf8());
            }
        }
    }
    (structural, state)
}

/// The hash count of an `r#"`-style opener starting exactly at `at`.
fn raw_opener_at(line: &str, at: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    if bytes[at] != b'r' {
        return None;
    }
    // A word ending in `r` is not an opener.
    if at > 0 && (bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'_') {
        return None;
    }
    let mut end = at + 1;
    let mut hashes = 0;
    while bytes.get(end) == Some(&b'#') {
        hashes += 1;
        end += 1;
    }
    // `hashes == 0` is `r"…"`, a raw string too. Requiring a hash left `r"C:\\"`
    // parsed as a plain string whose trailing backslash escapes the closing
    // quote — after which the scanner stays "inside a string" for the rest of
    // the file and every later declaration silently leaves the corpus.
    // Raw strings do not nest, so the first `"` + N `#` closes.
    (bytes.get(end) == Some(&b'"')).then_some(hashes)
}

/// `mod x {` — an inline module. `mod x;` is not one: the file it names is
/// walked on its own and supplies the same path from its location on disk.
fn opens_a_module(code: &str) -> Option<String> {
    let rest = strip_visibility(code)?.strip_prefix("mod ")?;
    let (name, tail) = rest.split_once(|c: char| !(c.is_alphanumeric() || c == '_'))?;
    tail.trim_start().starts_with('{').then(|| name.to_owned())
}

/// `text` with a leading balanced `<…>` removed, if it opens with one.
///
/// Balanced rather than "up to the first `>`", so `<T: Into<String>>` is
/// consumed whole.
fn strip_balanced_generics(text: &str) -> &str {
    let text = text.trim_start();
    if !text.starts_with('<') {
        return text;
    }
    let mut depth = 0usize;
    for (at, ch) in text.char_indices() {
        match ch {
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    return text[at + 1..].trim_start();
                }
            }
            _ => {}
        }
    }
    text
}

/// The type an `impl` block is about: `impl<T> Trait for Thing<T> {` → `Thing`.
fn opens_an_impl(code: &str) -> Option<String> {
    let rest = code.strip_prefix("impl")?;
    if !rest.starts_with(['<', ' ']) {
        return None;
    }
    // Drop `impl`'s own generic parameters before looking for the subject.
    // Splitting on the first `<` instead — which is what this did — yields an
    // empty first field for `impl<'a> WebSetupAnswers<'a> {` and the whole impl
    // is skipped. That is live at `runtime/views.rs`: it made
    // `runtime::views::WebSetupAnswers::from_preview` read as a broken path
    // while `runtime::views::from_preview`, naming a free function that does not
    // exist, resolved. Both directions wrong from one parse.
    let rest = strip_balanced_generics(rest);
    let head = rest.split_once('{')?.0;
    let subject = head.rsplit(" for ").next().unwrap_or(head).trim();
    let subject = strip_balanced_generics(subject);
    let subject = subject.split(['<', ' ']).next().unwrap_or("").trim();
    let name: String = subject
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty() && name.starts_with(|c: char| c.is_uppercase())).then_some(name)
}

/// A named item: `fn`, `struct`, `enum`, `trait`, `type`, `const`, `static`.
fn declared_item(code: &str) -> Option<String> {
    let mut rest = strip_visibility(code)?;
    for modifier in ["async ", "unsafe ", "extern \"C\" ", "default "] {
        rest = rest.strip_prefix(modifier).unwrap_or(rest);
    }
    for kind in [
        "fn ", "struct ", "enum ", "trait ", "type ", "const ", "static ", "union ",
    ] {
        if let Some(after) = rest.strip_prefix(kind) {
            // `const fn` is a modifier, not the item kind — the name follows the
            // `fn`. Recurse rather than special-case the ordering.
            if kind == "const " && after.starts_with("fn ") {
                return declared_item(after);
            }
            // `static mut G` — `mut` is a modifier, not the name.
            let after = after.strip_prefix("mut ").unwrap_or(after);
            let name: String = after
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            return (!name.is_empty()).then_some(name);
        }
    }
    None
}

fn strip_visibility(code: &str) -> Option<&str> {
    let rest = code.strip_prefix("pub").map_or(code, |after| {
        after
            .strip_prefix('(')
            .and_then(|a| a.split_once(')'))
            .map_or_else(|| after.trim_start(), |(_, tail)| tail.trim_start())
    });
    Some(rest)
}

/// Every `runtime::…` path cited in a comment, with where it was cited.
fn cited_paths() -> BTreeMap<String, Vec<String>> {
    let root = workspace_root();
    let mut sources: Vec<(PathBuf, bool)> = Vec::new();
    // Every crate in the workspace, enumerated from disk. A hardcoded list of
    // crate names is a corpus that silently shrinks: renaming or adding a crate
    // drops or omits its citations, `rust_files_recursive` returns empty for the
    // missing path without complaint, and the count floor below still clears.
    let crates = std::fs::read_dir(root.join("crates"))
        .unwrap_or_else(|err| panic!("unreadable crates/: {err}"));
    for entry in crates.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        for sub in ["src", "tests", "benches", "examples"] {
            let mut files = Vec::new();
            rust_files_recursive(&entry.path().join(sub), &mut files);
            sources.extend(files.into_iter().map(|f| (f, true)));
        }
    }
    let mut docs = Vec::new();
    files_recursive(&root.join("docs"), "md", &mut docs);
    sources.extend(docs.into_iter().map(|f| (f, false)));

    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (file, comments_only) in sources {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let shown = file
            .strip_prefix(&root)
            .unwrap_or(&file)
            .to_string_lossy()
            .into_owned();
        // This file is the one place that must name broken paths in prose — its
        // header explains the failure using the very citations that failed, and
        // its doc comments give worked examples. Scanning itself would make
        // every explanation a violation. The exemption is one file wide and
        // stated rather than silent.
        if shown.ends_with("tests/runtime_doc_paths.rs") {
            continue;
        }
        for (number, line) in text.lines().enumerate() {
            let scanned = if comments_only {
                match line.find("//") {
                    Some(at) => &line[at..],
                    None => continue,
                }
            } else {
                line
            };
            for path in runtime_paths_in(scanned) {
                out.entry(path)
                    .or_default()
                    .push(format!("{shown}:{}", number + 1));
            }
        }
    }
    out
}

/// The `runtime::…::…` tokens in one line.
///
/// Anchored so `harness::runtime::x` and `my_runtime::x` do not match: the
/// segment before `runtime` must not be an identifier character, and a
/// `crate::`/`super::`/`self::` qualifier is consumed rather than rejected.
fn runtime_paths_in(line: &str) -> Vec<String> {
    let mut found = Vec::new();
    for (at, _) in line.match_indices("runtime::") {
        let before = line[..at].trim_end_matches(|c: char| c.is_whitespace());
        let qualified = ["crate::", "super::", "self::"]
            .iter()
            .any(|q| before.ends_with(q));
        if !qualified
            && before
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == ':')
        {
            continue;
        }
        let path: String = line[at..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == ':')
            .collect();
        let path = path.trim_end_matches(':').to_owned();
        // A bare `runtime` with nothing after it names the module and is not a
        // cross-reference into it.
        if !path.contains("::") {
            continue;
        }
        // `runtime::duty::dispatch::{a_test, another_test}` — expand the group.
        // Without this the citation is checked only as far as `dispatch`, so
        // renaming either test leaves the guard green. Three such citations
        // exist and TASK-305 rewrote all three, which is the worst place to
        // stop checking.
        let after = &line[at + path.len()..];
        let group = after
            .trim_start_matches(':')
            .strip_prefix('{')
            .and_then(|rest| rest.split_once('}'));
        match group {
            Some((members, _)) => {
                let mut any = false;
                for member in members.split(',') {
                    let member = member.trim();
                    if !member.is_empty() && member.chars().all(|c| c.is_alphanumeric() || c == '_')
                    {
                        found.push(format!("{path}::{member}"));
                        any = true;
                    }
                }
                // A group wrapped across a `//!` continuation has no `}` on
                // this line; fall back to the prefix rather than dropping it.
                if !any {
                    found.push(path);
                }
            }
            None => found.push(path),
        }
    }
    found
}

#[test]
fn every_runtime_path_named_in_a_comment_still_resolves() {
    let declared = declared_paths();
    let cited = cited_paths();

    // Vacuity floors. A parser that matches nothing agrees with any tree
    // (LESSON-585), and this test has two parsers that could each go quiet
    // independently — so each is floored, and floored on shape as well as
    // count, because a count alone is satisfied by a parser that finds only
    // top-level modules.
    assert!(
        declared.len() >= 500,
        "vacuity floor: the module-tree parser found only {} declarations under \
         `runtime/`. It should see thousands; something stopped matching.",
        declared.len()
    );
    for deep in [
        "runtime::duty::dispatch::redact",
        "runtime::views::tests",
        "runtime::config_document::tests",
        "runtime::provider::provider_test",
    ] {
        assert!(
            declared.contains(deep),
            "vacuity floor: the parser did not find `{deep}`, a module known to be \
             nested three levels deep. A parser that sees only the top level would \
             still clear the count floor above while resolving nothing.",
        );
    }
    // The citation floor, keyed on shape as well as count. A bare count is
    // cleared by a corpus that lost a whole directory: `crates/tetond/tests`
    // alone contributes ~18 distinct paths, and losing it leaves ~30 — still
    // over any count floor worth setting, with 18 citations silently unchecked.
    // So each sub-corpus that carries citations today must still carry them.
    let contributors: BTreeSet<&str> = cited
        .values()
        .flatten()
        .filter_map(|site| {
            [
                "crates/tetond/src",
                "crates/tetond/tests",
                "crates/teton/tests",
                "docs",
            ]
            .into_iter()
            .find(|prefix| site.starts_with(prefix))
        })
        .collect();
    for required in ["crates/tetond/src", "crates/tetond/tests", "docs"] {
        assert!(
            contributors.contains(required),
            "vacuity floor: no `runtime::…` citation was found anywhere under `{required}`, \
             which carries them today. A directory that leaves the corpus takes its \
             citations with it and the scanner reports success over what remains — \
             which is the count floor's blind spot, not a hypothetical."
        );
    }
    assert!(
        cited.len() >= 40,
        "vacuity floor: the citation scanner found only {} distinct `runtime::…` \
         paths in comments. The corpus carries more than 45; the scanner has gone quiet.",
        cited.len()
    );

    let mut broken: Vec<String> = Vec::new();
    for (path, sites) in &cited {
        if !declared.contains(path) {
            broken.push(format!("  {path}\n      cited at {}", sites.join(", ")));
        }
    }

    assert!(
        broken.is_empty(),
        "these `runtime::…` paths are cited in comments but do not exist. A module \
         moved and the comments pointing at it did not — which nothing else in the \
         suite can see, because no compiler reads a comment:\n{}",
        broken.join("\n")
    );
}
