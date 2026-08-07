//! The `grep` tool: find lines containing a literal substring.
//!
//! Deliberately a **literal** (not regex) matcher for the MVP: it needs no
//! dependency (ADR-001) and literal + case-insensitive triage is what the
//! local-tier "grep triage" duty actually needs. An optional `glob` narrows the
//! file set (reusing the [`glob`](super::glob) matcher). Output is
//! `path:line: text`, capped for a small model's context.
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
use std::path::Path;

use super::glob::glob_match;
use super::{opt_str_arg, str_arg, RefinedOutcome, Tool, ToolContext, ToolDuties, ToolOutcome};
use crate::harness::digest::tool_result_provenance;
use crate::harness::triage;

/// Directory names never searched.
const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules"];

/// Cap on returned matching lines.
const MAX_MATCHES: usize = 200;

/// Searches repository files for a literal substring, jailed to the repo root.
#[derive(Debug, Default, Clone, Copy)]
pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search repository files for a literal substring. Optional glob narrows \
         the files; set ignore_case for case-insensitive matching."
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
            Err(_) => return ToolOutcome::error("repo root does not exist"),
        };

        let needle = if ignore_case {
            pattern.to_lowercase()
        } else {
            pattern.clone()
        };

        let mut hits = Vec::new();
        // REQ-544 C-1: the files whose *content* appears in the output. Only
        // matched files surface content into context, so those are the paths the
        // result's egress provenance must carry (a file that was scanned but had
        // no match contributes nothing and is not tagged).
        let mut matched_files = BTreeSet::new();
        search(
            &root,
            &root,
            &needle,
            ignore_case,
            file_glob.as_deref(),
            &mut hits,
            &mut matched_files,
        );

        if hits.is_empty() {
            return ToolOutcome::ok(format!("no matches for `{pattern}`"));
        }
        let truncated = hits.len() > MAX_MATCHES;
        hits.truncate(MAX_MATCHES);
        let mut out = hits.join("\n");
        if truncated {
            out.push('\n');
            out.push_str(&cap_notice());
        }
        ToolOutcome::ok(out).with_paths(matched_files)
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
        // A failed search has no matches to rank, and neither does a search that
        // found none — `run` answers that one in prose, not in `path:line:` form.
        if outcome.is_error {
            return RefinedOutcome::unrefined(outcome);
        }
        let (matches, capped) = split_cap_notice(&outcome.content);
        if !triage::worth_ranking(matches.len()) {
            // A single match is already first. No model call, and nothing to
            // report: a duty not worth making is not a duty that failed.
            return RefinedOutcome::unrefined(outcome);
        }
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
                let content = render_ranked(&matches, &order, capped);
                let tagged = outcome.provenance.clone();
                RefinedOutcome::unrefined(ToolOutcome::ok(content).with_provenance(tagged))
            }
            Err(error) => RefinedOutcome::degraded(outcome, error),
        }
    }
}

/// The line [`Tool::run`] appends when the search hit [`MAX_MATCHES`].
///
/// Written once here and recognized once in [`split_cap_notice`], for the reason
/// every other duty constant in this REQ is shared between its writer and its
/// reader: two spellings of one sentence drift, and this one is what tells
/// [`Tool::refine`] which trailing line is the harness talking rather than a
/// match.
fn cap_notice() -> String {
    format!("... (capped at {MAX_MATCHES} matches)")
}

/// A `grep` result split into its match lines and whether [`Tool::run`] capped
/// it.
fn split_cap_notice(content: &str) -> (Vec<&str>, bool) {
    let suffix = format!("\n{}", cap_notice());
    match content.strip_suffix(&suffix) {
        Some(head) => (head.lines().collect(), true),
        None => (content.lines().collect(), false),
    }
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

/// The ranked matches, in the duty's order, under this tool's own cap.
///
/// [`MAX_MATCHES`] is applied **after** ranking as well as before it: the duty
/// may reorder and it may drop, but a ranking is resolved against the list it
/// was given, so it can never hand back more lines than `run` found — and the
/// cap here says so at the one place a future change could break it.
fn render_ranked(matches: &[&str], order: &[usize], capped: bool) -> String {
    let body: Vec<&str> = order
        .iter()
        .take(MAX_MATCHES)
        .map(|&i| matches[i])
        .collect();
    let mut out = format!(
        "[triage: {} of {} matches, most useful first]\n{}",
        body.len(),
        matches.len(),
        body.join("\n")
    );
    if capped {
        out.push('\n');
        out.push_str(&cap_notice());
    }
    out
}

/// Recursively search text files under `dir`. `matched` accumulates the
/// repo-relative paths of every file that produced at least one hit — the egress
/// provenance of the result (REQ-544 C-1).
#[allow(clippy::too_many_arguments)]
fn search(
    root: &Path,
    dir: &Path,
    needle: &str,
    ignore_case: bool,
    file_glob: Option<&str>,
    out: &mut Vec<String>,
    matched: &mut BTreeSet<String>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        if out.len() > MAX_MATCHES {
            return;
        }
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if file_type.is_dir() {
            if SKIP_DIRS.contains(&name.as_ref()) {
                continue;
            }
            search(root, &path, needle, ignore_case, file_glob, out, matched);
            continue;
        }
        let rel = match path.strip_prefix(root) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        if let Some(g) = file_glob {
            if !glob_match(g, &rel) {
                continue;
            }
        }
        // Skip binary-ish files: read_to_string fails on invalid UTF-8, which is
        // the cheap heuristic we want.
        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for (i, line) in contents.lines().enumerate() {
            let haystack = if ignore_case {
                line.to_lowercase()
            } else {
                line.to_owned()
            };
            if haystack.contains(needle) {
                matched.insert(rel.clone());
                out.push(format!("{rel}:{}: {}", i + 1, line.trim_end()));
                if out.len() > MAX_MATCHES {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "teton-grep-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
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
        assert_eq!(out.provenance, ToolProvenance::path("secrets/prod.env"));
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
            Ok(Completion {
                text: self.reply.clone(),
                prompt_tokens: 1,
                completion_tokens: 1,
            })
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
                &ToolDuties { triage: route },
                raw.clone(),
            )
            .await;
        (raw, refined)
    }

    /// The match lines of a grep result, with the harness's own header and cap
    /// notice removed.
    fn match_lines(content: &str) -> Vec<&str> {
        content
            .lines()
            .filter(|l| !l.starts_with("[triage:") && !l.starts_with("... (capped at"))
            .collect()
    }

    /// The happy path: the duty's order is the order the model sees, and the
    /// matches it left out are gone.
    #[tokio::test]
    async fn a_ranked_grep_result_is_reordered_and_filtered_by_the_duty() {
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
        assert_eq!(after, vec![before[2], before[0]]);
        assert!(
            refined
                .outcome
                .content
                .starts_with("[triage: 2 of 3 matches"),
            "{}",
            refined.outcome.content
        );
        assert_eq!(refined.duty_error, None);
        // The matched-files provenance survives ranking: it is what a later
        // remote turn is judged against (REQ-544 C-1).
        assert_eq!(refined.outcome.provenance, raw.provenance);
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
            ToolProvenance::paths(["hit.rs", "secrets/prod.env"]),
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

    /// The cap notice is written once and recognized once, so `refine` can tell
    /// the harness's own trailing line from a match — and never ranks it as one.
    #[test]
    fn the_cap_notice_is_split_off_rather_than_ranked() {
        let capped = format!("a.rs:1: x\nb.rs:2: y\n{}", cap_notice());
        assert_eq!(
            split_cap_notice(&capped),
            (vec!["a.rs:1: x", "b.rs:2: y"], true)
        );
        assert_eq!(
            split_cap_notice("a.rs:1: x\nb.rs:2: y"),
            (vec!["a.rs:1: x", "b.rs:2: y"], false)
        );
    }
}
