//! The `grep` tool: find lines containing a literal substring.
//!
//! Deliberately a **literal** (not regex) matcher for the MVP: it needs no
//! dependency (ADR-001) and literal + case-insensitive triage is what the
//! local-tier "grep triage" duty actually needs. An optional `glob` narrows the
//! file set (reusing the [`glob`](super::glob) matcher). Output is
//! `path:line: text`, capped for a small model's context.
//!
//! The walk itself — what is skipped, what is pruned from a home-kind root, how
//! far it may go, and what it says when it stops — is [`walk`]'s (REQ-583
//! ADR-3); this tool decides only what to do with a file it is handed. Every
//! line the harness appends after the matches (the walk's trailer, the cap
//! notice) starts with `... (`, and [`split_harness_trailer`] peels them all
//! before the triage duty sees a match list.
//!
//! ## The `triage` duty attaches here (REQ-561 TASK-060)
//!
//! [`Tool::run`] answers with the first [`MAX_MATCHES`] hits **in file order**,
//! which is what the filesystem walk happened to produce and has nothing to do
//! with the turn. [`Tool::refine`] then hands those same lines to the `triage`
//! duty, which returns the order they should be read in.
//!
//! The category is not chosen here and is not inferred from anything: the
//! resolved route arrives on
//! [`ToolDuties`](super::ToolDuties), and the whole of this tool's per-category
//! surface is calling [`triage::rank_matches`] with it. The split between `run`
//! and `refine` exists because ranking is a **model call**: doing it inside the
//! synchronous `run` would park a runtime worker for the length of an inference.
//!
//! Ranking is an improvement on the order, never a precondition for having the
//! matches — so every way the duty can fail returns `run`'s own result
//! unchanged, with the reason on
//! [`RefinedOutcome::duty_error`](super::RefinedOutcome) (BR-3).

use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::BTreeSet;

use super::glob::glob_match;
use super::walk;
use super::{opt_str_arg, str_arg, RefinedOutcome, Tool, ToolContext, ToolDuties, ToolOutcome};
use crate::harness::digest::tool_result_provenance;
use crate::harness::triage;

/// Cap on returned matching lines.
const MAX_MATCHES: usize = 200;

/// Searches files under the session root for a literal substring, jailed to
/// that root.
#[derive(Debug, Default, Clone, Copy)]
pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search files under the session root for a literal substring. Optional glob \
         narrows the files; set ignore_case for case-insensitive matching."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Literal substring to find" },
                "glob": { "type": "string", "description": "Optional file glob, e.g. **/*.rs" },
                "ignore_case": { "type": "boolean", "description": "Case-insensitive match" }
            },
            "required": ["pattern"]
        })
    }

    fn run(&self, ctx: &ToolContext, args: &Value) -> ToolOutcome {
        let pattern = match str_arg(args, "pattern") {
            Ok(p) => p,
            Err(e) => return e.into(),
        };
        if pattern.is_empty() {
            return ToolOutcome::error("pattern must not be empty");
        }
        let file_glob = opt_str_arg(args, "glob");
        let ignore_case = args
            .get("ignore_case")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let root = match ctx.repo_root().canonicalize() {
            Ok(r) => r,
            Err(_) => return ToolOutcome::error("session root does not exist"),
        };

        let needle = if ignore_case {
            pattern.to_lowercase()
        } else {
            pattern.clone()
        };

        let mut hits: Vec<String> = Vec::new();
        // REQ-544 C-1: the files whose *content* appears in the output. Only
        // matched files surface content into context, so those are the paths the
        // result's egress provenance must carry (a file that was scanned but had
        // no match contributes nothing and is not tagged).
        let mut matched_files = BTreeSet::new();
        // The pattern's leading literal segments name the tree the walk may enter
        // even where the policy would prune it (BR-12's "unless named").
        let named = file_glob
            .as_deref()
            .map(walk::leading_literal_segments)
            .unwrap_or_default();
        let report = walk::visit(
            &root,
            ctx.root_kind(),
            &named,
            ctx.walk_policy(),
            &mut |path, file_type, id| {
                // Past the cap the walk still runs to its end (so its trailer is
                // honest about what was and was not seen), but no more files are
                // opened: the result cannot grow, so the reads would buy nothing.
                if file_type.is_dir() || hits.len() > MAX_MATCHES {
                    return;
                }
                let rel = id.as_str();
                if let Some(g) = file_glob.as_deref() {
                    if !glob_match(g, rel) {
                        return;
                    }
                }
                // Skip binary-ish files: read_to_string fails on invalid UTF-8,
                // which is the cheap heuristic we want.
                let Ok(contents) = std::fs::read_to_string(path) else {
                    return;
                };
                for (i, line) in contents.lines().enumerate() {
                    let haystack = if ignore_case {
                        line.to_lowercase()
                    } else {
                        line.to_owned()
                    };
                    if haystack.contains(needle.as_str()) {
                        matched_files.insert(id.clone());
                        hits.push(format!("{rel}:{}: {}", i + 1, line.trim_end()));
                        if hits.len() > MAX_MATCHES {
                            return;
                        }
                    }
                }
            },
        );
        let trailer = walk::trailer_lines(&report);

        if hits.is_empty() {
            // REQ-561 verify M3: `measuring(0)` is what tells `refine` this is
            // prose rather than a match list. `pattern` is model-supplied and may
            // contain a newline — and a newline-bearing pattern can NEVER match,
            // since the search compares against `contents.lines()`, so this
            // branch is exactly where such a pattern always lands. Counting the
            // lines of the sentence below read that as two "matches" and bought a
            // `triage` call which then rebuilt the message out of its own chosen
            // subset.
            //
            // BR-10: a budget hit is never a silent partial — the walk's trailer
            // rides under the empty answer exactly as it rides under a full one.
            let mut out = format!("no matches for `{pattern}`");
            for line in &trailer {
                out.push('\n');
                out.push_str(line);
            }
            return ToolOutcome::ok(out).measuring(0);
        }
        let found = hits.len();
        let truncated = found > MAX_MATCHES;
        hits.truncate(MAX_MATCHES);
        let mut out = hits.join("\n");
        // Matches, then the walk's own lines, then the cap notice last — the
        // position it always had (ADR-3).
        for line in &trailer {
            out.push('\n');
            out.push_str(line);
        }
        if truncated {
            out.push('\n');
            out.push_str(&cap_notice());
        }
        ToolOutcome::ok(out)
            .with_paths(matched_files)
            .measuring(found)
    }

    /// Rank this search's matches against the turn's `request` through the
    /// `triage` duty (REQ-561 BR-1/BR-3/BR-7).
    ///
    /// Every early return and every failure arm hands back `outcome` — the very
    /// value [`Tool::run`] produced — so "the fallback is today's behaviour
    /// verbatim" is a property of the code's shape rather than a string someone
    /// has to keep in step (BR-3, LESSON-447).
    async fn refine(
        &self,
        args: &Value,
        request: &str,
        duties: &ToolDuties<'_>,
        outcome: ToolOutcome,
    ) -> RefinedOutcome {
        // A failed search has no matches to rank.
        if outcome.is_error {
            return RefinedOutcome::unrefined(outcome);
        }
        // **How many matches there are is a fact `run` measured**, handed over on
        // the outcome rather than recovered from the words `run` rendered
        // (REQ-561 verify M3). A search that found none answers in prose, and
        // that prose is not a one-line shape: `pattern` is model-supplied and may
        // contain a newline, so counting the rendered lines reported 2+ "matches"
        // for a result that had none — bought a `triage` call for a sentence and
        // then replaced the sentence with the duty's chosen subset of it.
        //
        // A `grep` outcome that measured nothing is not a search result at all,
        // so there is nothing here to rank either.
        let Some(found) = outcome.measured else {
            return RefinedOutcome::unrefined(outcome);
        };
        if !triage::worth_ranking(found) {
            // Zero matches, or a single match that is already first. No model
            // call, and nothing to report: a duty not worth making is not a duty
            // that failed.
            return RefinedOutcome::unrefined(outcome);
        }
        let (matches, trailer) = split_harness_trailer(&outcome.content);
        // BR-7 / LESSON-432: the egress provenance of the **matched files**, as
        // this tool itself reported them on the outcome — narrower than the
        // turn's context, and never read off an argument's name. A match inside a
        // `local-only` file is refused at the choke point while the rest of the
        // turn proceeds.
        let provenance = tool_result_provenance(&outcome.provenance);
        let ranked = triage::rank_matches(
            duties.triage,
            request,
            &describe_search(args),
            &matches,
            &provenance,
        )
        .await;
        match ranked {
            Ok(order) => {
                let content = render_ranked(&matches, &order, &trailer);
                // Rebuilt by *update*, not from scratch — the same shape
                // `ShellTool::refine` uses, and for a reason that outlives
                // today's caller. `ToolOutcome::ok(content)` would reset
                // `measured` to `None`, which means "this call never got far
                // enough to produce a result of this kind" — the exact opposite
                // of the truth for a ranked 200-match search — and would drop
                // `is_error` and the provenance with it. Only `content` changed
                // here; everything the outcome already knew about itself rides
                // through (REQ-561 verify).
                RefinedOutcome::unrefined(ToolOutcome { content, ..outcome })
            }
            Err(error) => RefinedOutcome::degraded(outcome, error),
        }
    }
}

/// The line [`Tool::run`] appends when the search hit [`MAX_MATCHES`].
///
/// It wears the `... (` prefix every harness line wears
/// ([`walk::is_harness_line`]), which is what tells [`Tool::refine`] that a
/// trailing line is the harness talking rather than a match — one recogniser
/// for this notice and for every line the walk appends (ADR-3), so a new
/// harness line is never ranked as a match by accident.
fn cap_notice() -> String {
    format!("... (capped at {MAX_MATCHES} matches)")
}

/// A `grep` result split into its match lines and the harness's own trailing
/// lines — the walk's stopped/unreadable lines and the cap notice, in the order
/// they were written — so [`render_ranked`] can put the same lines back
/// verbatim under the ranked matches.
fn split_harness_trailer(content: &str) -> (Vec<&str>, Vec<&str>) {
    let mut lines: Vec<&str> = content.lines().collect();
    let mut trailer = Vec::new();
    while let Some(last) = lines.last() {
        if !walk::is_harness_line(last) {
            break;
        }
        trailer.push(*last);
        lines.pop();
    }
    trailer.reverse();
    (lines, trailer)
}

/// The search, in one line, for the duty prompt: what "relevant" is being judged
/// against besides the request itself.
fn describe_search(args: &Value) -> String {
    let pattern = opt_str_arg(args, "pattern").unwrap_or_default();
    match opt_str_arg(args, "glob") {
        Some(glob) => format!("grep for the literal `{pattern}` in files matching `{glob}`"),
        None => format!("grep for the literal `{pattern}`"),
    }
}

/// The matches in the duty's order, **with the ones it left out below them**.
///
/// ## The duty reorders; it never removes (REQ-561 verify)
///
/// [`triage`]'s "answer with numbers" shape already makes *fabrication*
/// unreachable — an invented line has nowhere to land, so the result is always
/// drawn from what `grep` really found. What that shape does not bound is
/// **omission**, and omission is the sharper edge here: the ranked content is
/// repository text, an attacker may control a file in the repository, and a
/// ranking of `"1"` reduces a 200-hit search to one line. A `grep` for a
/// backdoor pattern could then be steered to return the single benign hit, and
/// the model would have no way to know the other 199 existed. BR-3 speaks only
/// to a duty that *fails*; a duty that succeeds and answers narrowly is not a
/// failure, so nothing there applied.
///
/// So the remainder is appended in file order under a marker of its own, and the
/// duty's authority is reduced to what it is actually good at: saying what to
/// read first.
///
/// **Why this rather than a minimum-coverage floor.** A floor ("refuse a ranking
/// covering less than a named fraction") bounds the suppression without removing
/// it: at any honest fraction, a 200-hit search still lets 150 hits be dropped,
/// and the one hit that mattered can be among them. It also has to pick a
/// number that trades the attack against the duty's usefulness, and there is no
/// principled place to put it. Appending costs nothing that was not already
/// being spent: `run` had already decided to put up to [`MAX_MATCHES`] lines
/// into context, that is exactly what the BR-3 fallback folds, and the ceiling
/// on this result is therefore unchanged. What is given up is context *savings*
/// the caller never budgeted for — and what is bought is that this function's
/// output is a permutation of its input, which is a property a reader can check.
///
/// [`MAX_MATCHES`] is applied **after** ranking as well as before it: the duty
/// may reorder, but a ranking is resolved against the list it was given, so it
/// can never hand back more lines than `run` found — and the cap here says so at
/// the one place a future change could break it.
///
/// `trailer` is what [`split_harness_trailer`] peeled off — the walk's lines
/// and the cap notice — re-appended verbatim and in order, so a stopped or
/// partly unreadable walk says so after ranking exactly as it did before.
fn render_ranked(matches: &[&str], order: &[usize], trailer: &[&str]) -> String {
    let ranked: Vec<&str> = order
        .iter()
        .take(MAX_MATCHES)
        .map(|&i| matches[i])
        .collect();
    let mut chosen = vec![false; matches.len()];
    for &i in order.iter().take(MAX_MATCHES) {
        chosen[i] = true;
    }
    let rest: Vec<&str> = matches
        .iter()
        .enumerate()
        .filter(|(i, _)| !chosen[*i])
        .map(|(_, line)| *line)
        .collect();

    let mut out = format!(
        "[triage: {} of {} matches, most useful first]\n{}",
        ranked.len(),
        matches.len(),
        ranked.join("\n")
    );
    if !rest.is_empty() {
        out.push('\n');
        out.push_str(&format!(
            "[triage: the other {} matches, unranked, in file order]\n{}",
            rest.len(),
            rest.join("\n")
        ));
    }
    for line in trailer {
        out.push('\n');
        out.push_str(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture_id;
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use teton_protocol::methods::RootKind;

    use crate::harness::tools::walk::{WalkBudget, HOME_TOP_LEVEL_SKIPS, WALK_SKIP_DIRS};

    /// The counter, not the timestamp, guarantees uniqueness: `SystemTime::now()`
    /// can return the same value for two calls within one clock tick.
    fn temp_root(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "teton-grep-{tag}-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn finds_literal_matches_with_location() {
        let root = temp_root("hit");
        std::fs::write(root.join("a.rs"), "fn main() {}\nlet needle = 1;\n").unwrap();
        let ctx = ToolContext::new(&root);
        let out = GrepTool.run(&ctx, &json!({ "pattern": "needle" }));
        assert!(!out.is_error);
        assert!(out.content.contains("a.rs:2:"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ignore_case_matches() {
        let root = temp_root("case");
        std::fs::write(root.join("a.rs"), "HELLO world\n").unwrap();
        let ctx = ToolContext::new(&root);
        assert!(GrepTool
            .run(&ctx, &json!({ "pattern": "hello" }))
            .content
            .contains("no matches"));
        let out = GrepTool.run(&ctx, &json!({ "pattern": "hello", "ignore_case": true }));
        assert!(out.content.contains("a.rs:1:"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn glob_narrows_the_file_set() {
        let root = temp_root("narrow");
        std::fs::write(root.join("a.rs"), "TODO fix\n").unwrap();
        std::fs::write(root.join("b.txt"), "TODO fix\n").unwrap();
        let ctx = ToolContext::new(&root);
        let out = GrepTool.run(&ctx, &json!({ "pattern": "TODO", "glob": "**/*.rs" }));
        assert!(out.content.contains("a.rs"));
        assert!(!out.content.contains("b.txt"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn provenance_is_the_set_of_matched_files_only() {
        use crate::harness::context::ToolProvenance;
        let root = temp_root("prov");
        std::fs::create_dir_all(root.join("secrets")).unwrap();
        std::fs::write(root.join("secrets/prod.env"), "API_KEY=sk-live\n").unwrap();
        std::fs::write(root.join("public.rs"), "// nothing to see\n").unwrap();
        let ctx = ToolContext::new(&root);
        // REQ-544 C-1: a grep whose only match is in a boundary file tags the
        // result with that file — so the next remote turn is blocked at egress.
        let out = GrepTool.run(&ctx, &json!({ "pattern": "sk-live" }));
        assert!(!out.is_error);
        assert_eq!(
            out.provenance,
            ToolProvenance::path(fixture_id("secrets/prod.env"))
        );
        std::fs::remove_dir_all(&root).ok();
    }

    // -----------------------------------------------------------------------
    // The `triage` duty at its call site (REQ-561 TASK-060).
    //
    // Driven through the real `run` against a real temp repo rather than a
    // hand-written `ToolOutcome`, because the claim under test is "on failure
    // the tool returns *today's* result" — and today's result is whatever `run`
    // produces, not what a fixture author believes it produces.
    // -----------------------------------------------------------------------

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use teton_inference::{Completion, Engine, EngineError, GenParams, MockEngine};
    use teton_protocol::Category;

    use crate::egress::Provenance;
    use crate::harness::context::ToolProvenance;
    use crate::harness::duty::{Duty, DutyRoute};
    use crate::harness::triage::{TRIAGE_DUTY, TRIAGE_OUTPUT_MAX_BYTES};

    /// An engine that answers `reply` and counts how often it was asked.
    ///
    /// The count is the load-bearing half of the "no duty was worth making"
    /// tests: an outcome that merely *looks* unchanged is equally consistent
    /// with a duty that ran and chose the identity ranking.
    struct CountingEngine {
        reply: String,
        calls: Arc<AtomicUsize>,
    }

    impl Engine for CountingEngine {
        fn model_id(&self) -> &str {
            "counting"
        }
        fn complete(
            &self,
            _prompt: &str,
            _params: &GenParams,
            _on_token: &mut dyn FnMut(&str) -> bool,
        ) -> Result<Completion, EngineError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Completion::cold(self.reply.clone(), 1, 1))
        }
    }

    /// A local `triage` route answering `reply`, and its call counter.
    fn counting_route(reply: &str) -> (DutyRoute, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let engine: Arc<Mutex<dyn Engine>> = Arc::new(Mutex::new(CountingEngine {
            reply: reply.to_owned(),
            calls: Arc::clone(&calls),
        }));
        (DutyRoute::local(TRIAGE_DUTY, "local", engine), calls)
    }

    /// A local `triage` route answering `reply`.
    fn local_route(reply: &str) -> DutyRoute {
        let engine: Arc<Mutex<dyn Engine>> =
            Arc::new(Mutex::new(MockEngine::with_response("mock", reply)));
        DutyRoute::local(TRIAGE_DUTY, "local", engine)
    }

    /// A repo with `files` files, each holding `per_file` matching lines.
    fn repo_with_matches(tag: &str, files: usize, per_file: usize) -> PathBuf {
        let root = temp_root(tag);
        for f in 0..files {
            let body: String = (0..per_file)
                .map(|n| format!("let needle_{f}_{n} = 1;\n"))
                .collect();
            std::fs::write(root.join(format!("f{f}.rs")), body).unwrap();
        }
        root
    }

    /// `run` then `refine`, over `route`.
    async fn run_and_refine(
        root: &Path,
        args: &Value,
        route: &DutyRoute,
    ) -> (ToolOutcome, RefinedOutcome) {
        let ctx = ToolContext::new(root);
        let raw = GrepTool.run(&ctx, args);
        let refined = GrepTool
            .refine(
                args,
                "find where the needle is defined",
                &ToolDuties {
                    triage: route,
                    // `grep` never reaches it; an unresolved route would be a
                    // reported failure if it somehow did.
                    shell: &DutyRoute::unresolved("no shell route in this test"),
                },
                raw.clone(),
            )
            .await;
        (raw, refined)
    }

    /// The match lines of a grep result, with the harness's own header and
    /// trailer lines (the walk's, and the cap notice) removed.
    fn match_lines(content: &str) -> Vec<&str> {
        content
            .lines()
            .filter(|l| !l.starts_with("[triage:") && !walk::is_harness_line(l))
            .collect()
    }

    /// The happy path: the duty's order is the order the model sees, and the
    /// matches it left out follow underneath rather than disappearing.
    #[tokio::test]
    async fn a_ranked_grep_result_is_reordered_by_the_duty() {
        let root = repo_with_matches("rank", 3, 1);
        let args = json!({ "pattern": "needle" });
        let route = local_route("3 1");

        let (raw, refined) = run_and_refine(&root, &args, &route).await;

        let before = match_lines(&raw.content);
        assert_eq!(before.len(), 3, "the fixture must offer a real ranking");
        let after = match_lines(&refined.outcome.content);
        // Asserted as a permutation of the *observed* first-200 result rather
        // than against hard-coded filenames: `read_dir` order is not a promise,
        // and a test that depended on it would be flaky rather than wrong.
        //
        // The duty chose 3 then 1; 2 was left out, so it comes last.
        assert_eq!(after, vec![before[2], before[0], before[1]]);
        assert!(
            refined
                .outcome
                .content
                .starts_with("[triage: 2 of 3 matches"),
            "{}",
            refined.outcome.content
        );
        assert!(
            refined
                .outcome
                .content
                .contains("[triage: the other 1 matches, unranked, in file order]"),
            "the unranked remainder must be labelled as such: {}",
            refined.outcome.content
        );
        assert_eq!(refined.duty_error, None);
        // The matched-files provenance survives ranking: it is what a later
        // remote turn is judged against (REQ-544 C-1).
        assert_eq!(refined.outcome.provenance, raw.provenance);
        std::fs::remove_dir_all(&root).ok();
    }

    /// **A duty cannot make a match disappear** (REQ-561 verify).
    ///
    /// The numbers-only answer already makes fabrication unreachable — an
    /// invented line has nowhere to land. Omission is the direction it does not
    /// bound, and the ranked content is repository text somebody may have
    /// written on purpose: a ranking of `"1"` would otherwise reduce a search
    /// for a backdoor pattern to whichever single hit the duty was steered to
    /// name, with the model given no sign that the others existed.
    ///
    /// Asserted as **set equality against the tool's own unranked result**, in
    /// both directions, so this stays true of any ranking rather than of the one
    /// this fixture happens to produce.
    #[tokio::test]
    async fn a_ranking_that_names_one_match_still_delivers_all_of_them() {
        let root = repo_with_matches("suppress", 6, 1);
        let args = json!({ "pattern": "needle" });
        // The narrowest ranking a duty can give: one match, out of six.
        let route = local_route("2");

        let (raw, refined) = run_and_refine(&root, &args, &route).await;

        assert_eq!(refined.duty_error, None, "the fixture must reach the duty");
        let before = match_lines(&raw.content);
        let after = match_lines(&refined.outcome.content);
        assert_eq!(before.len(), 6, "the fixture must offer a real ranking");
        assert_eq!(
            after.len(),
            before.len(),
            "a one-match ranking suppressed {} of {} matches",
            before.len() - after.len(),
            before.len()
        );
        assert_eq!(
            after.iter().collect::<BTreeSet<_>>(),
            before.iter().collect::<BTreeSet<_>>(),
            "the ranked result must be a permutation of what `grep` found"
        );
        // Non-vacuity: the duty really did rank, and its choice really is first
        // — so this is the omission being repaired, not the duty being ignored.
        assert_eq!(after[0], before[1], "the duty's choice must lead");
        assert!(refined
            .outcome
            .content
            .starts_with("[triage: 1 of 6 matches"));
        std::fs::remove_dir_all(&root).ok();
    }

    /// **Ranking edits the content and nothing else** (REQ-561 verify).
    ///
    /// The refined outcome used to be rebuilt with `ToolOutcome::ok(content)`
    /// and the provenance re-attached by hand, which silently reset `measured`
    /// to `None`. `None` is not "unknown" — it is defined as *the call did not
    /// get far enough to produce a result of this kind*, which for a ranked
    /// 200-match search is the exact opposite of the truth. Nothing downstream
    /// reads it today ([`super::super::turn_loop`] discards the field), so this
    /// is a claim the type makes about itself going wrong quietly, which is the
    /// kind that outlives the person who introduced it.
    ///
    /// [`super::shell::ShellTool::refine`] rebuilds by update (`..outcome`) and
    /// always did. This pins `grep` to the same rule: after a ranking, the only
    /// field that may differ from what `run` produced is `content`.
    #[tokio::test]
    async fn a_ranked_result_still_reports_what_the_search_measured() {
        let root = repo_with_matches("measured-survives", 6, 1);
        let args = json!({ "pattern": "needle" });
        let route = local_route("2");

        let (raw, refined) = run_and_refine(&root, &args, &route).await;

        assert_eq!(refined.duty_error, None, "the fixture must reach the duty");
        assert_ne!(
            refined.outcome.content, raw.content,
            "non-vacuity: the ranking really did rewrite the content"
        );
        assert_eq!(
            raw.measured,
            Some(6),
            "the fixture must offer a measurement to preserve"
        );
        assert_eq!(
            refined.outcome.measured, raw.measured,
            "ranking a search does not un-measure it"
        );
        assert_eq!(
            refined.outcome.provenance, raw.provenance,
            "nor does it change which files the result came from"
        );
        assert_eq!(refined.outcome.is_error, raw.is_error);
        std::fs::remove_dir_all(&root).ok();
    }

    /// **A pattern containing a newline can never match, and must not buy a
    /// model call** (REQ-561 verify M3).
    ///
    /// `search` compares against `contents.lines()`, so a newline-bearing
    /// pattern always lands on the zero-hit branch — and that branch answers in
    /// prose that *embeds the pattern*, so counting the rendered lines reported
    /// two "matches" for a search that found none. `worth_ranking` then passed,
    /// a `triage` call was bought for a sentence, and `render_ranked` rebuilt
    /// that sentence out of the duty's chosen subset of its own words:
    /// ``[triage: 1 of 2 matches, most useful first]\nb` `` where a plain "no
    /// matches" line belongs.
    ///
    /// Both halves are asserted, because a fix that merely stopped the model
    /// call would still leave the count wrong for anything else that reads it.
    #[tokio::test]
    async fn a_newline_bearing_pattern_is_a_no_match_result_not_a_two_match_one() {
        let root = repo_with_matches("newline-pattern", 2, 1);
        let args = json!({ "pattern": "needle\nb" });
        let (route, calls) = counting_route("1 2");

        let (raw, refined) = run_and_refine(&root, &args, &route).await;

        // The fixture really is in the state the name claims: a result whose
        // rendered text spans two lines and whose match count is zero.
        assert!(raw.content.contains("no matches for"), "{}", raw.content);
        assert_eq!(
            raw.content.lines().count(),
            2,
            "the fixture must render as more than one line, or it tests nothing: {:?}",
            raw.content
        );
        assert_eq!(raw.measured, Some(0));

        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a search that found nothing bought a ranking"
        );
        assert_eq!(
            refined.outcome, raw,
            "the tool's own `no matches` answer must come back untouched"
        );
        assert_eq!(refined.duty_error, None);
        std::fs::remove_dir_all(&root).ok();
    }

    /// **BR-3, the whole of it.** Every way the duty can fail returns the value
    /// `run` produced — asserted by struct equality against that value, so
    /// "verbatim" means verbatim — and says why on the outcome rather than only
    /// in a log.
    #[tokio::test]
    async fn every_triage_failure_returns_the_tools_own_unranked_result_verbatim() {
        let root = repo_with_matches("degrade", 3, 1);
        let args = json!({ "pattern": "needle" });

        let unresolvable = DutyRoute::unresolved(
            "The 'triage' category inherits the 'scan' tier, which is not bound to any provider.",
        );
        let engine_failure: DutyRoute = {
            let engine: Arc<Mutex<dyn Engine>> = Arc::new(Mutex::new(MockEngine::unavailable(
                "mock",
                "the local tier is not loaded",
            )));
            DutyRoute::local(TRIAGE_DUTY, "local", engine)
        };
        let unusable_answer = local_route("they all look relevant to me");

        for (label, route, expected) in [
            (
                "unresolvable binding",
                &unresolvable,
                "not bound to any provider",
            ),
            ("provider failure", &engine_failure, "not loaded"),
            (
                "an answer that is not a ranking",
                &unusable_answer,
                "no usable match numbers",
            ),
        ] {
            let (raw, refined) = run_and_refine(&root, &args, route).await;
            assert_eq!(
                refined.outcome, raw,
                "{label}: the tool's own first-{MAX_MATCHES} result must come back untouched"
            );
            let error = refined
                .duty_error
                .expect("a degradation must be visible on the outcome, not only in a log");
            assert!(error.contains(expected), "{label}: {error}");
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// A result with nothing to rank costs nothing: no model call, and no
    /// failure reported for a duty that was never worth making.
    #[tokio::test]
    async fn a_single_match_is_returned_unchanged_without_a_model_call() {
        let root = repo_with_matches("single", 1, 1);
        let args = json!({ "pattern": "needle" });
        let (route, calls) = counting_route("1");

        let (raw, refined) = run_and_refine(&root, &args, &route).await;

        assert_eq!(match_lines(&raw.content).len(), 1);
        assert_eq!(refined.outcome, raw);
        assert_eq!(refined.duty_error, None);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "one match is already first; ranking it is a model call bought for nothing"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// A search that found nothing is prose, not a match list, and is left alone.
    #[tokio::test]
    async fn a_search_with_no_matches_is_left_alone() {
        let root = repo_with_matches("none", 1, 1);
        let args = json!({ "pattern": "absent-from-this-repo" });
        let (route, calls) = counting_route("1 2 3");

        let (raw, refined) = run_and_refine(&root, &args, &route).await;

        assert!(raw.content.contains("no matches for"));
        assert_eq!(refined.outcome, raw);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        std::fs::remove_dir_all(&root).ok();
    }

    /// **The duty may reorder and drop; it may never inflate.** A duty naming
    /// four hundred matches against a result the tool already capped at
    /// [`MAX_MATCHES`] still yields at most `MAX_MATCHES` lines, and the cap
    /// notice `run` appended survives the ranking.
    #[tokio::test]
    async fn a_duty_cannot_inflate_the_result_past_the_tools_own_cap() {
        let root = repo_with_matches("cap", 1, MAX_MATCHES + 100);
        let args = json!({ "pattern": "needle" });
        let over: String = (1..=400)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let route = local_route(&over);

        let (raw, refined) = run_and_refine(&root, &args, &route).await;

        assert!(
            raw.content.contains("(capped at"),
            "the fixture must exercise the cap"
        );
        let after = match_lines(&refined.outcome.content);
        assert!(
            after.len() <= MAX_MATCHES,
            "a ranking naming 400 matches produced {} lines",
            after.len()
        );
        assert!(
            refined.outcome.content.contains("(capped at"),
            "the cap notice must survive ranking: {}",
            &refined.outcome.content[..refined.outcome.content.len().min(120)]
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// **BR-2, both halves against one route** (LESSON-485).
    ///
    /// The number of matches is the only difference between the two calls. The
    /// three-match search reaches the duty, so the route announces; the
    /// one-match search returns before the duty is touched, so it announces
    /// nothing. Split apart, the negative half would be satisfied by an emitter
    /// that never emits and the positive by one that emits at resolution — only
    /// the pair pins "announced iff performed".
    #[tokio::test]
    async fn a_triage_announces_its_route_only_when_there_is_a_ranking_to_make() {
        use teton_protocol::events::{Event, RouteDecided};
        use teton_protocol::{Category, ProviderId, Tier};

        use crate::broadcast::EventBus;

        let bus = Arc::new(EventBus::new());
        let mut sub = bus.subscribe(16);
        let route = local_route("2 1 3").announcing(
            &bus,
            Some(teton_protocol::SessionId::from("sess")),
            Some(RouteDecided {
                effort: None,
                category: Some(Category::Triage),
                tier: Some(Tier::Scan),
                phase: None,
                provider_id: ProviderId::from("local"),
                model: None,
                reason: "Routing the 'triage' category to 'local' through its 'scan' tier \
                         binding."
                    .to_owned(),
            }),
        );
        let announced = |sub: &mut crate::broadcast::Subscription| -> Vec<RouteDecided> {
            std::iter::from_fn(|| sub.try_recv())
                .filter_map(|env| match env.event {
                    Event::RouteDecided(rd) => Some(rd),
                    _ => None,
                })
                .collect()
        };

        let one = repo_with_matches("announce-one", 1, 1);
        let args = json!({ "pattern": "needle" });
        let (_, refined) = run_and_refine(&one, &args, &route).await;
        assert_eq!(refined.duty_error, None);
        assert!(
            announced(&mut sub).is_empty(),
            "a result with no ranking to make announces a routed model call that never happened"
        );

        let many = repo_with_matches("announce-many", 3, 1);
        let (_, refined) = run_and_refine(&many, &args, &route).await;
        assert_eq!(refined.duty_error, None);
        let decided = announced(&mut sub);
        assert_eq!(decided.len(), 1, "{decided:?}");
        assert_eq!(decided[0].category, Some(Category::Triage));
        assert_eq!(decided[0].tier, Some(Tier::Scan));
        assert_eq!(decided[0].provider_id.0, "local");
        assert!(!decided[0].reason.is_empty());

        std::fs::remove_dir_all(&one).ok();
        std::fs::remove_dir_all(&many).ok();
    }

    /// A duty that records the provenance it was scoped by, and ranks nothing.
    ///
    /// Attached straight onto [`DutyRoute::Serves`] so the assertion is made on
    /// the value the seam actually hands the egress choke point, rather than
    /// inferred from whether a mock transport happened to refuse.
    struct RecordingDuty {
        seen: Arc<Mutex<Vec<Provenance>>>,
    }

    #[async_trait]
    impl Duty for RecordingDuty {
        fn category(&self) -> Category {
            Category::Triage
        }
        fn ceiling_bytes(&self) -> usize {
            TRIAGE_OUTPUT_MAX_BYTES
        }
        async fn perform(&self, _prompt: &str, provenance: &Provenance) -> Result<String, String> {
            self.seen
                .lock()
                .expect("seen poisoned")
                .push(provenance.clone());
            Ok("1 2".to_owned())
        }
    }

    /// **BR-7 / LESSON-432, at the call site.** The duty is scoped by the
    /// **matched files**, which is narrower than the turn and narrower than the
    /// files the search walked: a file that was scanned and did not match
    /// contributes no content and must not taint the duty's egress.
    ///
    /// Asserted in both directions, because "the provenance contains the
    /// boundary file" is equally true of turn-level scoping — it is the
    /// *absence* of the scanned-but-unmatched file that distinguishes the two.
    #[tokio::test]
    async fn a_triage_is_scoped_by_the_matched_files_not_by_what_the_search_walked() {
        let root = temp_root("scope");
        std::fs::create_dir_all(root.join("secrets")).unwrap();
        std::fs::write(root.join("secrets/prod.env"), "needle=sk-live\n").unwrap();
        std::fs::write(root.join("hit.rs"), "let needle = 1;\n").unwrap();
        std::fs::write(root.join("walked_but_clean.rs"), "// nothing here\n").unwrap();

        let seen = Arc::new(Mutex::new(Vec::new()));
        let route = DutyRoute::Serves {
            provider_id: "frontier".to_owned(),
            duty: Arc::new(RecordingDuty {
                seen: Arc::clone(&seen),
            }),
            announce: None,
        };

        let args = json!({ "pattern": "needle" });
        let (raw, refined) = run_and_refine(&root, &args, &route).await;
        assert_eq!(refined.duty_error, None, "the fixture must reach the duty");
        assert_eq!(
            raw.provenance,
            ToolProvenance::paths([fixture_id("hit.rs"), fixture_id("secrets/prod.env")]),
            "the fixture must match exactly the two files"
        );

        let seen = seen.lock().expect("seen poisoned");
        assert_eq!(seen.len(), 1, "the duty ran once");
        let scoped = &seen[0];
        assert!(scoped.contains("secrets/prod.env"), "{scoped:?}");
        assert!(scoped.contains("hit.rs"), "{scoped:?}");
        assert!(
            !scoped.contains("walked_but_clean.rs"),
            "a file the search read but did not match surfaced no content, so it must \
             not scope the duty's egress: {scoped:?}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// Every harness line — the walk's trailer and the cap notice — is split off
    /// rather than ranked, in the order it was written, so `refine` can tell the
    /// harness talking from a match and put the same lines back.
    #[test]
    fn the_harness_trailer_is_split_off_rather_than_ranked() {
        let stopped =
            "... (stopped after 3 entries; narrow the pattern, or move the session root with /cd)";
        let unreadable = "... (1 folder could not be read (permission denied): secrets/)";
        let capped = format!(
            "a.rs:1: x\nb.rs:2: y\n{stopped}\n{unreadable}\n{}",
            cap_notice()
        );
        assert_eq!(
            split_harness_trailer(&capped),
            (
                vec!["a.rs:1: x", "b.rs:2: y"],
                vec![stopped, unreadable, "... (capped at 200 matches)"]
            )
        );
        assert_eq!(
            split_harness_trailer("a.rs:1: x\nb.rs:2: y"),
            (vec!["a.rs:1: x", "b.rs:2: y"], vec![])
        );
        // A harness-looking line *inside* the matches is a match: only the
        // trailing run is peeled.
        assert_eq!(
            split_harness_trailer("a.rs:1: x\n... (mid)\nb.rs:2: y"),
            (vec!["a.rs:1: x", "... (mid)", "b.rs:2: y"], vec![])
        );
    }

    /// **The trailer survives ranking.** Driven through `refine` on an outcome
    /// carrying the stopped line, the unreadable line and the cap notice: all
    /// three come back verbatim, in order, after the ranked matches — and none
    /// of them is offered to the duty as a match.
    #[tokio::test]
    async fn the_walk_trailer_survives_refine_verbatim_and_unranked() {
        let stopped =
            "... (stopped after 3 entries; narrow the pattern, or move the session root with /cd)";
        let unreadable = "... (1 folder could not be read (permission denied): secrets/)";
        let matches = ["a.rs:1: needle", "b.rs:2: needle", "c.rs:3: needle"];
        let content = format!(
            "{}\n{stopped}\n{unreadable}\n{}",
            matches.join("\n"),
            cap_notice()
        );
        let outcome = ToolOutcome::ok(&content)
            .with_paths([fixture_id("a.rs"), fixture_id("b.rs"), fixture_id("c.rs")])
            .measuring(matches.len());
        let route = local_route("2 3");
        let refined = GrepTool
            .refine(
                &json!({ "pattern": "needle" }),
                "find the needle",
                &ToolDuties {
                    triage: &route,
                    shell: &DutyRoute::unresolved("no shell route in this test"),
                },
                outcome,
            )
            .await;
        assert_eq!(refined.duty_error, None, "the fixture must reach the duty");
        let out = &refined.outcome.content;
        assert!(out.starts_with("[triage: 2 of 3 matches"), "{out}");
        assert_eq!(
            match_lines(out),
            vec![matches[1], matches[2], matches[0]],
            "{out}"
        );
        assert!(
            out.ends_with(&format!("\n{stopped}\n{unreadable}\n{}", cap_notice())),
            "the trailer must come back verbatim, in order, after the matches: {out}"
        );
    }

    // -----------------------------------------------------------------------
    // The walk under `grep` (REQ-583 ADR-3) — twins of the `glob` tests.
    // -----------------------------------------------------------------------

    /// Create `rel` (a file holding `needle`) under `root`, with its parents.
    fn plant(root: &Path, rel: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "let needle = 1;\n").unwrap();
    }

    /// The files a `grep` for `needle` matched, sorted.
    fn matched_in(out: &ToolOutcome) -> Vec<String> {
        let mut files: Vec<String> = match_lines(&out.content)
            .iter()
            .filter(|l| !l.starts_with("no matches for"))
            .map(|l| l.split(':').next().unwrap().to_owned())
            .collect();
        files.sort();
        files
    }

    /// **AC-14 / AC-15 (BR-10), entries.** An injected budget smaller than the
    /// fixture ends the result with the stopped-by-entries line — with matches
    /// found and with none — and a fixture under the budget produces no such
    /// line.
    #[test]
    fn ac14_ac15_grep_reports_the_entry_budget_with_and_without_matches() {
        let root = temp_root("ac14-entries");
        for n in 0..8 {
            plant(&root, &format!("f{n}.rs"));
        }
        let small = WalkBudget {
            max_entries: 3,
            max_wall: Duration::from_secs(60),
        };
        let stopped =
            "... (stopped after 3 entries; narrow the pattern, or move the session root with /cd)";

        let ctx = ToolContext::new(&root).with_walk_budget(small);
        let out = GrepTool.run(&ctx, &json!({ "pattern": "needle" }));
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(matched_in(&out).len(), 3, "{}", out.content);
        assert_eq!(out.content.lines().last(), Some(stopped), "{}", out.content);
        assert_eq!(out.measured, Some(3), "the trailer is not a match");

        let out = GrepTool.run(&ctx, &json!({ "pattern": "absent-from-this-repo" }));
        assert_eq!(
            out.content,
            format!("no matches for `absent-from-this-repo`\n{stopped}")
        );
        assert_eq!(out.measured, Some(0));

        let out = GrepTool.run(&ToolContext::new(&root), &json!({ "pattern": "needle" }));
        assert_eq!(matched_in(&out).len(), 8);
        assert!(!out.content.contains("stopped after"), "{}", out.content);
        std::fs::remove_dir_all(&root).ok();
    }

    /// **AC-14 (BR-10), wall clock.** A zero wall budget lets the root's own
    /// listing through and stops at the first descent.
    #[test]
    fn ac14_grep_reports_the_wall_clock_budget() {
        let root = temp_root("ac14-wall");
        plant(&root, "top.rs");
        plant(&root, "sub/deep.rs");
        let ctx = ToolContext::new(&root).with_walk_budget(WalkBudget {
            max_entries: 1_000,
            max_wall: Duration::ZERO,
        });
        let stopped =
            "... (stopped after 0 s; narrow the pattern, or move the session root with /cd)";

        let out = GrepTool.run(&ctx, &json!({ "pattern": "needle" }));
        assert_eq!(matched_in(&out), vec!["top.rs"], "{}", out.content);
        assert_eq!(out.content.lines().last(), Some(stopped), "{}", out.content);

        let out = GrepTool.run(&ctx, &json!({ "pattern": "needle", "glob": "sub/*.rs" }));
        assert_eq!(out.content, format!("no matches for `needle`\n{stopped}"));

        let out = GrepTool.run(&ToolContext::new(&root), &json!({ "pattern": "needle" }));
        assert_eq!(matched_in(&out), vec!["sub/deep.rs", "top.rs"]);
        assert!(!out.content.contains("stopped after"), "{}", out.content);
        std::fs::remove_dir_all(&root).ok();
    }

    /// **AC-16 (BR-12).** From a `home`-kind root the top-level media trees and
    /// a bundle at any depth are not searched; a project's own `Library/` is;
    /// naming the tree through the glob enters it; from a `project` root
    /// everything is searched.
    #[test]
    fn ac16_grep_prunes_home_media_trees_unless_named() {
        let root = temp_root("ac16");
        plant(&root, "Library/Preferences/lib.rs");
        plant(&root, "Music/tune.rs");
        plant(&root, "Pictures/pic.rs");
        plant(&root, "Documents/x.photoslibrary/inside.rs");
        plant(&root, "Documents/app/Library/proj_lib.rs");
        plant(&root, "Documents/app/src/main.rs");
        let home = ToolContext::new(&root).with_root_kind(RootKind::Home);

        let out = GrepTool.run(&home, &json!({ "pattern": "needle" }));
        assert_eq!(
            matched_in(&out),
            vec![
                "Documents/app/Library/proj_lib.rs",
                "Documents/app/src/main.rs"
            ],
            "{}",
            out.content
        );

        let out = GrepTool.run(
            &home,
            &json!({ "pattern": "needle", "glob": "Library/**/*.rs" }),
        );
        assert_eq!(
            matched_in(&out),
            vec!["Library/Preferences/lib.rs"],
            "{}",
            out.content
        );

        let project = ToolContext::new(&root).with_root_kind(RootKind::Project);
        let out = GrepTool.run(&project, &json!({ "pattern": "needle" }));
        assert_eq!(
            matched_in(&out),
            vec![
                "Documents/app/Library/proj_lib.rs",
                "Documents/app/src/main.rs",
                "Documents/x.photoslibrary/inside.rs",
                "Library/Preferences/lib.rs",
                "Music/tune.rs",
                "Pictures/pic.rs",
            ],
            "{}",
            out.content
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// **AC-17 (BR-13).** An unreadable folder ends the result with the
    /// permission-denied line; matches elsewhere are still returned; the macOS
    /// consent sentence is present on macOS and absent elsewhere. Skipped as
    /// root, who can read anything.
    #[test]
    fn ac17_grep_reports_an_unreadable_folder_and_still_returns_the_rest() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: `geteuid` reads the process's effective uid; no arguments.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipped: running as root, mode 000 does not refuse");
            return;
        }
        let root = temp_root("ac17");
        plant(&root, "secrets/hidden.rs");
        plant(&root, "src/main.rs");
        let locked = root.join("secrets");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

        let out = GrepTool.run(&ToolContext::new(&root), &json!({ "pattern": "needle" }));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(!out.is_error, "{}", out.content);
        assert_eq!(matched_in(&out), vec!["src/main.rs"], "{}", out.content);
        let last = out.content.lines().last().unwrap();
        assert!(
            last.starts_with("... (1 folder could not be read (permission denied): secrets/"),
            "{last}"
        );
        let sentence = "macOS may have blocked access to that folder, or be waiting on a consent dialog for it";
        assert_eq!(
            last.contains(sentence),
            cfg!(target_os = "macos"),
            "the consent sentence is macOS-only: {last}"
        );
        assert_eq!(out.measured, Some(1), "the trailer is not a match");
        std::fs::remove_dir_all(&root).ok();
    }

    /// **AC-18 (BR-11), the behavioural half.** The walker is built from the
    /// shared definition: every name in [`WALK_SKIP_DIRS`] is skipped and every
    /// name in [`HOME_TOP_LEVEL_SKIPS`] is pruned from a home root — read off
    /// the definition, not off a second list. (`glob`'s twin lives beside it;
    /// the source scan in `tests/boundary_coverage.rs` proves neither tool
    /// declares a list of its own.)
    #[test]
    fn ac18_grep_is_built_from_the_shared_walk_definition() {
        let root = temp_root("ac18");
        for name in WALK_SKIP_DIRS.iter().chain(HOME_TOP_LEVEL_SKIPS) {
            plant(&root, &format!("{name}/inside.rs"));
        }
        plant(&root, "src/main.rs");
        let home = ToolContext::new(&root).with_root_kind(RootKind::Home);
        let out = GrepTool.run(&home, &json!({ "pattern": "needle" }));
        assert_eq!(matched_in(&out), vec!["src/main.rs"], "{}", out.content);
        assert_eq!(home.walk_policy(), &walk::WalkPolicy::default());
        std::fs::remove_dir_all(&root).ok();
    }
}
