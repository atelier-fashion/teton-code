//! The `shell` tool: run a command under a timeout, a cwd jail, and a scrubbed
//! environment.
//!
//! Three hard constraints, each a security property (AC):
//!
//! - **cwd jail** — the command runs with the repo root as its working
//!   directory. (Absolute paths a command constructs itself are outside the
//!   tool's reach; the jail is the default surface an agent operates on.)
//! - **env scrub** — every variable whose *name* matches a credential substring
//!   (`SECRET`, `PASSWORD`, `PASSWD`, `TOKEN`, `KEY`, `CREDENTIAL`, or a `PAT`
//!   token) or whose *value* is a credential-bearing URL (`scheme://user:pass@…`)
//!   is removed before the child starts, so a secret in the daemon's environment
//!   can never leak into a model-driven `env`/`printenv` (BR-7). `PATH`, `HOME`,
//!   and the rest pass through so ordinary commands still work.
//! - **timeout** — a runaway command is `SIGKILL`ed after the deadline and the
//!   timeout is reported to the model, so a bad command can never hang the loop.
//!   The child is spawned as its own process-group leader and the whole group is
//!   killed, so a backgrounded grandchild cannot outlive the deadline (REQ-544
//!   L-2).
//!
//! The command runs synchronously via `sh -c`; a watcher thread enforces the
//! deadline. Output (stdout + stderr) is captured and capped.
//!
//! ## The `shell` duty attaches here (REQ-561 TASK-061)
//!
//! [`Tool::run`] answers with the command's status line and its output, capped
//! at [`MAX_OUTPUT_CHARS`]. [`Tool::refine`] then hands that result to the
//! `shell` duty — but **only** when reading it unaided is the hard part: the
//! command failed, or its raw output ran past the cap and what entered context
//! is a fragment of a thing (BR-4b). A short successful command is returned
//! verbatim with no model call at all, which matters because `shell` is the
//! highest-frequency tool call in a session.
//!
//! The category is not chosen here and is not inferred from anything: the
//! resolved route arrives on [`ToolDuties`](super::ToolDuties), and the whole of
//! this tool's per-category surface is calling
//! [`shell_duty::interpret_output`] with it. The split between `run` and
//! `refine` exists because interpretation is a **model call**: doing it inside
//! the synchronous `run` would park a runtime worker for the length of an
//! inference.
//!
//! Interpretation is an addition to the output, never a precondition for having
//! it — so every way the duty can fail returns `run`'s own result unchanged,
//! with the reason on [`RefinedOutcome::duty_error`](super::RefinedOutcome)
//! (BR-3). And because a `shell` result carries unknown provenance, a remotely
//! bound duty is *refused* on any machine with a privacy boundary configured;
//! see [`shell_duty`] for why that is the design working rather than a gap.

use std::io::Result as IoResult;
use std::os::unix::process::CommandExt;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{
    opt_str_arg, opt_u64_arg, str_arg, RefinedOutcome, Tool, ToolContext, ToolDuties, ToolOutcome,
};
use crate::harness::digest::tool_result_provenance;
use crate::harness::shell_duty;

/// Cap on captured output characters, so a chatty command cannot blow the
/// small-model context budget.
///
/// Also the `shell` duty's size trigger — a successful command is worth
/// interpreting exactly when this cap threw information away — which is why
/// [`shell_duty::SHELL_TRIGGER_OUTPUT_CHARS`] reads it rather than restating it
/// (REQ-561 ADR-5).
pub(crate) const MAX_OUTPUT_CHARS: usize = 8_000;

/// Runs shell commands under a timeout, cwd jail, and scrubbed environment.
#[derive(Debug, Clone, Copy)]
pub struct ShellTool {
    /// Timeout applied when the call does not specify one.
    default_timeout_ms: u64,
    /// Hard ceiling on any requested timeout.
    max_timeout_ms: u64,
}

impl Default for ShellTool {
    fn default() -> Self {
        Self {
            default_timeout_ms: 30_000,
            max_timeout_ms: 120_000,
        }
    }
}

impl ShellTool {
    /// A shell tool with explicit timeout bounds (used by tests to keep the
    /// timeout path fast).
    #[must_use]
    pub fn with_timeouts(default_timeout_ms: u64, max_timeout_ms: u64) -> Self {
        Self {
            default_timeout_ms,
            max_timeout_ms,
        }
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Run a shell command in the repository root under a timeout. Use it to \
         verify changes (build, test, grep). Secrets in the environment are \
         removed."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command to run" },
                "timeout_ms": { "type": "integer", "description": "Optional timeout in ms" }
            },
            "required": ["command"]
        })
    }

    fn run(&self, ctx: &ToolContext, args: &Value) -> ToolOutcome {
        // The first two of the three arms that leave `measured` at `None`. All
        // three carry no provenance — no command output exists, so no bytes came
        // off this machine — and, as `ToolOutcome::error` leaves `measured`
        // unset, all three are invisible to the `shell` duty. That `None` is the
        // load-bearing half: a duty asked to interpret "invalid arguments:
        // missing `command`" with an empty command line is a model call bought
        // for a harness sentence (REQ-561 verify).
        //
        // The third is the `cmd.spawn()` failure below — the only one of the
        // three where a launch was actually attempted, and still one where no
        // command output exists to interpret.
        let command = match str_arg(args, "command") {
            Ok(c) => c,
            Err(e) => return e.into(),
        };
        let root = match ctx.repo_root().canonicalize() {
            Ok(r) => r,
            Err(_) => return ToolOutcome::error("repo root does not exist"),
        };

        let timeout_ms = opt_u64_arg(args, "timeout_ms")
            .unwrap_or(self.default_timeout_ms)
            .min(self.max_timeout_ms);

        let scrubbed = scrub(std::env::vars());

        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(&command)
            .current_dir(&root)
            .env_clear()
            .envs(scrubbed)
            // REQ-544 L-2: make the child its own process-group leader (pgid ==
            // child pid) so that on timeout we can SIGKILL the whole group and no
            // backgrounded grandchild survives the deadline.
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return ToolOutcome::error(format!("failed to start command: {}", e.kind())),
        };
        let pid = child.id();

        let (tx, rx) = mpsc::channel::<IoResult<Output>>();
        let handle = std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });

        // BR-1 (REQ-544 C-1): a shell command runs arbitrary code, so the daemon
        // cannot know which files its output was derived from. Every result of a
        // command that actually started is therefore tagged UNKNOWN provenance,
        // which egress fail-closes whenever a boundary is configured. (The three
        // arms above — two pre-spawn, one failed spawn — surface no command
        // output and carry no provenance.)
        //
        // Every arm below also *measures* — `Some(n)`, even when `n` is zero —
        // because a spawned command's arms are exactly the arms where a command
        // ran. That is what makes `refine`'s "did a command run at all" question
        // answerable without re-reading the rendered text (REQ-561 verify).
        //
        // `Some(0)` answers that question with a yes and the next one with a no:
        // a command ran, and it captured nothing. `shell_duty::worth_interpreting`
        // reads the second half and declines, so the three harness-authored
        // sentences below buy no model call either — the same argument the
        // `None` above makes, one step further along.
        const NO_OUTPUT_CAPTURED: usize = 0;
        match rx.recv_timeout(Duration::from_millis(timeout_ms)) {
            Ok(Ok(output)) => {
                let _ = handle.join();
                render_output(&command, &output).with_unknown_provenance()
            }
            Ok(Err(e)) => {
                let _ = handle.join();
                ToolOutcome::error(format!("command failed to run: {}", e.kind()))
                    .with_unknown_provenance()
                    .measuring(NO_OUTPUT_CAPTURED)
            }
            Err(RecvTimeoutError::Timeout) => {
                // Kill the whole process group, not just the direct child:
                // `wait_with_output` moved the child into the watcher thread, so
                // we cannot call `Child::kill` here, and a bare `kill(pid)` would
                // leave backgrounded grandchildren running (REQ-544 L-2). The
                // child is its own group leader (`process_group(0)`), so its pgid
                // equals its pid; a negative target signals the entire group.
                // libc is already a daemon dependency (peer-cred / flock).
                // SAFETY: kill(2) with the negated pgid of a group we just created
                // and a valid signal.
                unsafe {
                    libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                }
                let _ = handle.join();
                ToolOutcome::error(format!(
                    "command timed out after {timeout_ms}ms and was killed"
                ))
                .with_unknown_provenance()
                .measuring(NO_OUTPUT_CAPTURED)
            }
            Err(RecvTimeoutError::Disconnected) => {
                let _ = handle.join();
                ToolOutcome::error("command watcher disconnected")
                    .with_unknown_provenance()
                    .measuring(NO_OUTPUT_CAPTURED)
            }
        }
    }

    /// Interpret this command's output through the `shell` duty, when reading it
    /// unaided is the hard part (REQ-561 BR-1/BR-3/BR-4b/BR-7).
    ///
    /// Every early return and every failure arm hands back `outcome` — the very
    /// value [`Tool::run`] produced — so "the fallback is today's 8,000-character
    /// result" is a property of the code's shape rather than a string someone has
    /// to keep in step (BR-3, LESSON-447).
    async fn refine(
        &self,
        args: &Value,
        _request: &str,
        duties: &ToolDuties<'_>,
        outcome: ToolOutcome,
    ) -> RefinedOutcome {
        // **The duty is for command output, so it fires only when there is
        // command output** (REQ-561 verify). `measured` is `Some` on exactly the
        // arms of `run` that spawned something; a missing `command` argument or a
        // repo root that does not exist never reached a shell, and handing the
        // duty a fixed harness sentence and an empty command line buys an
        // interpretation of nothing.
        //
        // It also restores a claim `shell_duty`'s module doc makes without
        // qualification: "on a machine with any privacy boundary configured, a
        // remotely bound shell duty is refused". Those two pre-spawn errors carry
        // no provenance — correctly, since no command ran and nothing they say
        // came off the machine — which means they take the egress fast path and
        // are never tested against a boundary at all. Gating here is what makes
        // the sentence true of every `shell` duty that is actually performed,
        // rather than tagging harness-authored strings `Unknown` and pinning the
        // whole session local (REQ-544 C-2) over an argument typo.
        let Some(raw_output_chars) = outcome.measured else {
            return RefinedOutcome::unrefined(outcome);
        };
        // **ADR-5, the whole of it.** The size arm is answered by the length of
        // the stdout+stderr the command really produced, which `render_output`
        // captured at the one moment it existed — before the cap was applied.
        // Measuring the *rendered* result here instead would compare a
        // post-truncation length against the cap that produced it, so the arm
        // could never fire on the results it exists for (LESSON-443).
        if !shell_duty::worth_interpreting(outcome.is_error, raw_output_chars) {
            // A command that succeeded and fit is already legible. No model call,
            // and nothing to report: a duty not worth making is not a duty that
            // failed. This is the case `shell` is in for most of a session.
            return RefinedOutcome::unrefined(outcome);
        }
        // BR-7 / LESSON-432: the egress provenance of the command output, as this
        // tool itself reported it on the outcome — `Unknown` for anything a
        // command produced, because the daemon cannot know which files it read.
        // The choke point fail-closes on that before it looks at a boundary glob,
        // so a remotely bound duty is refused on any machine with a boundary
        // configured, and the arm below degrades. That is the guarantee holding,
        // not a gap.
        let provenance = tool_result_provenance(&outcome.provenance);
        let command = opt_str_arg(args, "command").unwrap_or_default();
        let interpreted =
            shell_duty::interpret_output(duties.shell, &command, &outcome.content, &provenance)
                .await;
        match interpreted {
            Ok(interpretation) => {
                let content = render_interpreted(&interpretation, &outcome.content);
                // `is_error` and the provenance ride through untouched: a failing
                // command that has been *explained* has still failed, and the
                // BR-6 verification gate must keep seeing that (REQ-544 MED-4).
                RefinedOutcome::unrefined(ToolOutcome { content, ..outcome })
            }
            Err(error) => RefinedOutcome::degraded(outcome, error),
        }
    }
}

/// The line [`Tool::run`] appends when the command's raw output ran past
/// [`MAX_OUTPUT_CHARS`], telling the model how much the cap threw away.
///
/// **This sentence is for the model and for nothing else.** It used to be the
/// channel the length travelled on as well — [`Tool::refine`] parsed the number
/// back off the last line — and that made the duty's size trigger something a
/// command could write for itself: output ending in this exact line, whether by
/// accident or from a repository-controlled `Makefile` or build script, is
/// indistinguishable from the harness's own notice. (`grep`'s cap notice is not
/// forgeable the same way, because every real hit there carries a `{path}:{line}: `
/// prefix; a line of command output carries nothing.)
///
/// The consequence was bounded but larger than "one wasted model call": on a
/// machine with **no privacy boundary configured**, unknown provenance takes the
/// egress fast path, so a forged notice would ship the command's output to
/// whatever the `build` tier is bound to — a frontier provider, on the ordinary
/// remote configuration — when the real trigger would never have sent it. The
/// length now rides on [`ToolOutcome::measured`], where a command cannot reach
/// it (REQ-561 verify).
fn truncation_notice(raw_output_chars: usize) -> String {
    format!("{TRUNCATION_NOTICE_PREFIX}{raw_output_chars}{TRUNCATION_NOTICE_SUFFIX}")
}

/// The opening of [`truncation_notice`], up to the reported length.
const TRUNCATION_NOTICE_PREFIX: &str = "... (output truncated; ";

/// The close of [`truncation_notice`], after the reported length.
const TRUNCATION_NOTICE_SUFFIX: &str = " chars total)";

/// The command's own result with the duty's one-line interpretation above it.
///
/// The interpretation goes **first** so a weak model reads what happened before
/// it reads the wall of output, and the output is kept in full: the duty says
/// what a failure means, it does not get to decide what the model is allowed to
/// see.
fn render_interpreted(interpretation: &str, output: &str) -> String {
    format!("[shell: {interpretation}]\n{output}")
}

/// Remove credential-bearing variables from an environment, keeping everything
/// else. Pure so it can be tested without mutating the process environment.
pub(crate) fn scrub<I>(vars: I) -> Vec<(String, String)>
where
    I: IntoIterator<Item = (String, String)>,
{
    vars.into_iter()
        .filter(|(k, v)| !is_secret_var(k, v))
        .collect()
}

/// Whether an environment entry carries a credential and must be scrubbed before
/// the model-driven `shell` child sees it (BR-7). Two signals: a secret-shaped
/// *name*, or a credential-bearing *value* (a `scheme://user:pass@host` URL).
pub(crate) fn is_secret_var(key: &str, value: &str) -> bool {
    is_secret_key(key) || looks_like_credential_url(value)
}

/// Whether an environment variable name looks like it holds a credential.
///
/// A case-insensitive substring denylist (REQ-544 MED-1). The old suffix rule
/// (`*_KEY` / `*_TOKEN`) missed `*_SECRET`, `*PASSWORD*`, `PGPASSWORD`, and the
/// like. `PAT` (a GitHub personal-access token) is matched only as a whole,
/// delimiter-bounded token, so essential names like `PATH` — and words like
/// `COMPATIBLE` — are not swept up.
pub(crate) fn is_secret_key(key: &str) -> bool {
    const SECRET_SUBSTRINGS: &[&str] =
        &["SECRET", "PASSWORD", "PASSWD", "TOKEN", "KEY", "CREDENTIAL"];
    let up = key.to_ascii_uppercase();
    if SECRET_SUBSTRINGS.iter().any(|s| up.contains(s)) {
        return true;
    }
    up.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|token| token == "PAT")
}

/// Whether `value` is a URL that embeds a credential in its userinfo, e.g.
/// `postgres://user:pass@host/db` — the shape `DATABASE_URL` often takes, which
/// a name-only check cannot catch (REQ-544 MED-1).
fn looks_like_credential_url(value: &str) -> bool {
    let Some((_scheme, after)) = value.split_once("://") else {
        return false;
    };
    // The authority ends at the first '/', '?', or '#'.
    let authority = after.split(['/', '?', '#']).next().unwrap_or("");
    match authority.split_once('@') {
        // A ':' in the userinfo before the '@' is an embedded password
        // (`user:pass@` or `:pass@`).
        Some((userinfo, _host)) => userinfo.contains(':'),
        None => false,
    }
}

/// Render a finished command's output for the model, capped.
fn render_output(command: &str, output: &Output) -> ToolOutcome {
    let mut body = String::new();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stdout.trim().is_empty() {
        body.push_str(stdout.trim_end());
        body.push('\n');
    }
    if !stderr.trim().is_empty() {
        body.push_str("[stderr] ");
        body.push_str(stderr.trim_end());
        body.push('\n');
    }
    // REQ-561 ADR-5: the length of what the command **actually** produced, taken
    // before the cap is applied. This is the last moment it exists — `run`
    // returns only the capped text — and it is the number the `shell` duty's size
    // trigger is decided on, so it is recorded in the notice rather than left to
    // be re-derived from a string the cap has already clamped (LESSON-443).
    let raw_output_chars = body.chars().count();
    if raw_output_chars > MAX_OUTPUT_CHARS {
        let truncated: String = body.chars().take(MAX_OUTPUT_CHARS).collect();
        body = format!("{truncated}\n{}", truncation_notice(raw_output_chars));
    }

    let code = output.status.code();
    let status_line = match code {
        Some(0) => format!("$ {command}\n(exit 0)\n"),
        Some(c) => format!("$ {command}\n(exit {c})\n"),
        None => format!("$ {command}\n(terminated by signal)\n"),
    };
    let content = format!("{status_line}{body}");

    // A non-zero exit is a failure the model must see (so verification can tell
    // a passing test from a failing one).
    let outcome = if code == Some(0) {
        ToolOutcome::ok(content)
    } else {
        ToolOutcome::error(content)
    };
    // The raw length rides out **beside** the text, not inside it. The notice
    // above still says the number, because the model needs to know how much it
    // is missing — but that is a sentence for the model, and a sentence in
    // model-visible text is a sentence command output can write for itself
    // (REQ-561 verify).
    outcome.measuring(raw_output_chars)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "teton-shell-{tag}-{}-{}",
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
    fn scrub_removes_credential_bearing_vars_and_keeps_essentials() {
        let input = vec![
            ("PATH".to_owned(), "/usr/bin".to_owned()),
            ("HOME".to_owned(), "/home/x".to_owned()),
            ("ANTHROPIC_API_KEY".to_owned(), "sk-secret".to_owned()),
            ("openai_api_key".to_owned(), "lower-secret".to_owned()),
            ("GITHUB_TOKEN".to_owned(), "ghp_secret".to_owned()),
            // REQ-544 MED-1: the shapes the old suffix rule let through.
            ("PGPASSWORD".to_owned(), "hunter2".to_owned()),
            ("STRIPE_SECRET".to_owned(), "sk_live_x".to_owned()),
            ("MY_CREDENTIAL".to_owned(), "c".to_owned()),
            ("GITHUB_PAT".to_owned(), "ghp_x".to_owned()),
            (
                "DATABASE_URL".to_owned(),
                "postgres://user:pass@db.example.com/app".to_owned(),
            ),
        ];
        let kept: Vec<String> = scrub(input).into_iter().map(|(k, _)| k).collect();
        // Essentials survive.
        assert!(kept.contains(&"PATH".to_owned()));
        assert!(kept.contains(&"HOME".to_owned()));
        // Every credential-bearing var is gone.
        for scrubbed in [
            "ANTHROPIC_API_KEY",
            "openai_api_key",
            "GITHUB_TOKEN",
            "PGPASSWORD",
            "STRIPE_SECRET",
            "MY_CREDENTIAL",
            "GITHUB_PAT",
            "DATABASE_URL",
        ] {
            assert!(!kept.contains(&scrubbed.to_owned()), "leaked: {scrubbed}");
        }
    }

    #[test]
    fn is_secret_key_matches_substrings_but_not_essential_names() {
        // Case-insensitive substrings.
        assert!(is_secret_key("FOO_KEY"));
        assert!(is_secret_key("foo_token"));
        assert!(is_secret_key("PGPASSWORD"));
        assert!(is_secret_key("db_passwd"));
        assert!(is_secret_key("MY_SECRET_THING"));
        assert!(is_secret_key("aws_credential_file"));
        // `PAT` as a whole token, but not inside another word.
        assert!(is_secret_key("GITHUB_PAT"));
        assert!(!is_secret_key("PATH"));
        assert!(!is_secret_key("COMPATIBLE"));
        // A benign name with no secret substring survives.
        assert!(!is_secret_key("EDITOR"));
    }

    #[test]
    fn credential_urls_are_scrubbed_by_value() {
        assert!(looks_like_credential_url("postgres://user:pass@host/db"));
        assert!(looks_like_credential_url("redis://:password@host:6379"));
        // No embedded credential -> kept.
        assert!(!looks_like_credential_url("https://example.com/path"));
        assert!(!looks_like_credential_url("postgres://host/db"));
        assert!(!looks_like_credential_url("/usr/local/bin"));
    }

    #[test]
    fn runs_a_command_in_the_repo_root() {
        let root = temp_root("cwd");
        std::fs::write(root.join("marker.txt"), "x").unwrap();
        let ctx = ToolContext::new(&root);
        let out = ShellTool::default().run(&ctx, &json!({ "command": "ls" }));
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("marker.txt"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn any_shell_result_carries_unknown_provenance() {
        use crate::harness::context::ToolProvenance;
        let root = temp_root("prov");
        std::fs::create_dir_all(root.join("secrets")).unwrap();
        std::fs::write(root.join("secrets/prod.env"), "API_KEY=sk-live\n").unwrap();
        let ctx = ToolContext::new(&root);
        // REQ-544 C-1: `cat`-ing a boundary file cannot be parsed by the daemon,
        // so the result is UNKNOWN provenance — fail-closed at egress.
        let out = ShellTool::default().run(&ctx, &json!({ "command": "cat secrets/prod.env" }));
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(out.provenance, ToolProvenance::Unknown);
        // Even a boundary-free command is UNKNOWN — the daemon never parses it.
        let out2 = ShellTool::default().run(&ctx, &json!({ "command": "echo hi" }));
        assert_eq!(out2.provenance, ToolProvenance::Unknown);
    }

    #[test]
    fn nonzero_exit_is_a_model_visible_error() {
        let root = temp_root("fail");
        let ctx = ToolContext::new(&root);
        let out = ShellTool::default().run(&ctx, &json!({ "command": "exit 3" }));
        assert!(out.is_error);
        assert!(out.content.contains("exit 3"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn timeout_kills_a_runaway_command() {
        let root = temp_root("timeout");
        let ctx = ToolContext::new(&root);
        let started = std::time::Instant::now();
        let out = ShellTool::with_timeouts(200, 500).run(&ctx, &json!({ "command": "sleep 10" }));
        assert!(out.is_error);
        assert!(out.content.contains("timed out"));
        // Killed promptly, nowhere near the 10s sleep.
        assert!(started.elapsed() < Duration::from_secs(3));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn timeout_kills_a_backgrounded_grandchild_too() {
        // REQ-544 L-2: a backgrounded grandchild must not outlive the deadline.
        // The command backgrounds a subshell that would `touch survivor.txt`
        // after 2s, then blocks. On timeout the whole process group is SIGKILLed,
        // so the marker is never created.
        let root = temp_root("pgroup");
        let ctx = ToolContext::new(&root);
        let out = ShellTool::with_timeouts(200, 500).run(
            &ctx,
            &json!({
                "command": "(sleep 2; touch survivor.txt) & echo started; sleep 10"
            }),
        );
        assert!(out.is_error);
        assert!(out.content.contains("timed out"));
        // Wait past the grandchild's 2s delay; if it survived the group kill it
        // would have created the marker by now.
        std::thread::sleep(Duration::from_millis(2_800));
        assert!(
            !root.join("survivor.txt").exists(),
            "backgrounded grandchild outlived the deadline"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    // -----------------------------------------------------------------------
    // The `shell` duty at its call site (REQ-561 TASK-061).
    //
    // Driven through the real `run` against real commands rather than
    // hand-written `ToolOutcome`s, because the claims under test are about
    // *today's* result — the 8,000-character cap, the status line, the error
    // flag — and today's result is whatever `run` produces, not what a fixture
    // author believes it produces. It is also the only way the raw-length
    // capture (ADR-5) is exercised at all: a hand-built outcome would have to
    // hand-write the notice, which is exactly the code under test.
    // -----------------------------------------------------------------------

    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use teton_inference::{Completion, Engine, EngineError, GenParams, MockEngine};

    use crate::harness::context::ToolProvenance;
    use crate::harness::duty::DutyRoute;
    use crate::harness::shell_duty::{SHELL_DUTY, SHELL_TRIGGER_OUTPUT_CHARS};

    /// An engine that answers `reply` and counts how often it was asked.
    ///
    /// The count is the load-bearing half of AC-13: an outcome that merely
    /// *looks* unchanged is equally consistent with a duty that ran and returned
    /// something the caller then discarded. Only the counter distinguishes
    /// "cost nothing" from "cost a model call and threw it away".
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

    /// A local `shell` route answering `reply`, and its call counter.
    fn counting_route(reply: &str) -> (DutyRoute, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let engine: Arc<Mutex<dyn Engine>> = Arc::new(Mutex::new(CountingEngine {
            reply: reply.to_owned(),
            calls: Arc::clone(&calls),
        }));
        (DutyRoute::local(SHELL_DUTY, "local", engine), calls)
    }

    /// A local `shell` route answering `reply`.
    fn local_route(reply: &str) -> DutyRoute {
        let engine: Arc<Mutex<dyn Engine>> =
            Arc::new(Mutex::new(MockEngine::with_response("mock", reply)));
        DutyRoute::local(SHELL_DUTY, "local", engine)
    }

    /// `run` then `refine`, over `route`.
    async fn run_and_refine(
        root: &Path,
        args: &Value,
        route: &DutyRoute,
    ) -> (ToolOutcome, RefinedOutcome) {
        let ctx = ToolContext::new(root);
        let raw = ShellTool::default().run(&ctx, args);
        let refined = ShellTool::default()
            .refine(
                args,
                "make the tests pass",
                &ToolDuties {
                    // `shell` never reaches it.
                    triage: &DutyRoute::unresolved("no triage route in this test"),
                    shell: route,
                },
                raw.clone(),
            )
            .await;
        (raw, refined)
    }

    /// A repo holding `out.txt`, sized so that `cat out.txt` produces a
    /// [`render_output`] body of **exactly** `body_chars` characters.
    ///
    /// `render_output` trims the trailing newline off stdout and appends its own,
    /// so a file of N `x`s plus a newline yields a body of N + 1 characters —
    /// hence the one fewer written here. The exactness is what lets the
    /// at-the-cap row below actually sit on the boundary rather than near it.
    fn repo_printing(tag: &str, body_chars: usize) -> PathBuf {
        let root = temp_root(tag);
        let mut body = "x".repeat(body_chars - 1);
        body.push('\n');
        std::fs::write(root.join("out.txt"), body).unwrap();
        root
    }

    /// **AC-13, the load-bearing test.** Four commands, one duty, counted calls.
    ///
    /// The row that matters is the first: a short successful command — most of
    /// what a session runs — costs **zero** model calls. Everything else this
    /// duty does is only affordable because of that row, so it is asserted by
    /// call count rather than by output shape.
    ///
    /// The second row is the mutation detector for ADR-5. Its raw output fills
    /// the cap exactly and is therefore *not* truncated, so it must not fire —
    /// and a trigger evaluated on the rendered result instead of on the raw
    /// length would fire, because the status line pushes the rendered result
    /// past the cap.
    #[tokio::test]
    async fn the_duty_fires_on_failure_or_a_capped_output_and_on_nothing_else() {
        let over = SHELL_TRIGGER_OUTPUT_CHARS + 1_000;
        for (label, tag, chars, command, expected_calls) in [
            ("exit 0, small", "ok-small", 16usize, "cat out.txt", 0usize),
            (
                "exit 0, exactly at the cap",
                "ok-exact",
                SHELL_TRIGGER_OUTPUT_CHARS,
                "cat out.txt",
                0,
            ),
            ("exit 0, over the cap", "ok-over", over, "cat out.txt", 1),
            (
                "exit non-zero, small",
                "fail-small",
                16,
                "cat out.txt; exit 3",
                1,
            ),
            (
                "exit non-zero, over the cap",
                "fail-over",
                over,
                "cat out.txt; exit 3",
                1,
            ),
        ] {
            let root = repo_printing(tag, chars);
            let args = json!({ "command": command });
            let (route, calls) = counting_route("The command produced a lot of output.");

            let (raw, refined) = run_and_refine(&root, &args, &route).await;

            // Non-vacuity: the fixture really is in the state its label claims.
            assert_eq!(
                raw.is_error,
                command.contains("exit 3"),
                "{label}: the fixture's exit status is not what the row says"
            );
            assert_eq!(
                raw.content.contains(TRUNCATION_NOTICE_PREFIX),
                chars > SHELL_TRIGGER_OUTPUT_CHARS,
                "{label}: the fixture's output is not on the side of the cap the row says"
            );

            assert_eq!(
                calls.load(Ordering::SeqCst),
                expected_calls,
                "{label}: expected {expected_calls} model call(s)"
            );
            if expected_calls == 0 {
                assert_eq!(
                    refined.outcome, raw,
                    "{label}: a result no duty was made for must come back untouched"
                );
                assert_eq!(refined.duty_error, None, "{label}");
            } else {
                assert!(
                    refined.outcome.content.starts_with("[shell: "),
                    "{label}: {}",
                    &refined.outcome.content[..refined.outcome.content.len().min(120)]
                );
            }
            std::fs::remove_dir_all(&root).ok();
        }
    }

    /// **BR-3, the whole of it.** Every way the duty can fail returns the value
    /// `run` produced — asserted by struct equality against that value, so
    /// "today's 8,000-character truncated output, verbatim" means verbatim — and
    /// says why on the outcome rather than only in a log.
    #[tokio::test]
    async fn every_shell_duty_failure_returns_the_tools_own_capped_result_verbatim() {
        let root = repo_printing("degrade", SHELL_TRIGGER_OUTPUT_CHARS + 1_000);
        let args = json!({ "command": "cat out.txt" });

        let unresolvable = DutyRoute::unresolved(
            "The 'shell' category inherits the 'build' tier, which is not bound to any provider.",
        );
        let engine_failure: DutyRoute = {
            let engine: Arc<Mutex<dyn Engine>> = Arc::new(Mutex::new(MockEngine::unavailable(
                "mock",
                "the local tier is not loaded",
            )));
            DutyRoute::local(SHELL_DUTY, "local", engine)
        };
        let empty_answer = local_route("   ");

        for (label, route, expected) in [
            (
                "unresolvable binding",
                &unresolvable,
                "not bound to any provider",
            ),
            ("provider failure", &engine_failure, "not loaded"),
            (
                "an answer with nothing in it",
                &empty_answer,
                "nothing to interpret",
            ),
        ] {
            let (raw, refined) = run_and_refine(&root, &args, route).await;
            // Non-vacuity: the fallback really is the truncated result, not a
            // short one that never exercised the cap.
            assert!(
                raw.content.contains(TRUNCATION_NOTICE_PREFIX),
                "{label}: the fixture must exercise the cap"
            );
            assert_eq!(
                refined.outcome, raw,
                "{label}: the tool's own capped result must come back untouched"
            );
            let error = refined
                .duty_error
                .expect("a degradation must be visible on the outcome, not only in a log");
            assert!(error.contains(expected), "{label}: {error}");
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// The interpretation is added *above* the command's own result, and the
    /// facts the rest of the loop reads off that result are untouched.
    ///
    /// `is_error` in particular: REQ-544 MED-4 makes the BR-6 verification gate
    /// turn on it, so a failing `cargo test` that has been *explained* must still
    /// count as unverified. An interpretation that cleared the flag would let a
    /// weak model declare victory off a failed check.
    #[tokio::test]
    async fn an_interpreted_failure_keeps_its_output_its_error_flag_and_its_provenance() {
        let root = repo_printing("interpret", 16);
        let args = json!({ "command": "cat out.txt; exit 3" });
        let route = local_route("The command exited 3 because the file check failed.");

        let (raw, refined) = run_and_refine(&root, &args, &route).await;

        assert_eq!(refined.duty_error, None);
        assert_eq!(
            refined.outcome.content,
            format!(
                "[shell: The command exited 3 because the file check failed.]\n{}",
                raw.content
            ),
            "the command's own result must survive under the interpretation"
        );
        assert!(
            refined.outcome.is_error,
            "an explained failure still failed"
        );
        assert_eq!(
            refined.outcome.provenance,
            ToolProvenance::Unknown,
            "interpreting a result cannot make its origin knowable"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// **ADR-5, pinned.** The raw stdout+stderr length is captured before the cap
    /// is applied and carried on the outcome, so `refine` reads the length the
    /// command really produced rather than the one the cap left behind.
    ///
    /// Without the carried number there is nothing to read: the truncated body
    /// is clamped to exactly [`MAX_OUTPUT_CHARS`], so comparing *it* against the
    /// cap that produced it is a guard that can never fire (LESSON-443).
    ///
    /// Both halves are asserted, because they are two different claims: the
    /// number the *duty* reads travels out-of-band on
    /// [`ToolOutcome::measured`], and the sentence the *model* reads travels in
    /// the text. They agree here; the point of separating them is that the model
    /// -visible one is the only one a command can write for itself.
    #[test]
    fn the_raw_output_length_is_carried_beside_the_text_not_inside_it() {
        let root = repo_printing("raw-len", MAX_OUTPUT_CHARS + 1_000);
        let ctx = ToolContext::new(&root);
        let out = ShellTool::default().run(&ctx, &json!({ "command": "cat out.txt" }));

        assert_eq!(
            out.measured,
            Some(MAX_OUTPUT_CHARS + 1_000),
            "the outcome must carry the length the command produced, not the capped one"
        );
        // And the body really was capped, so the number is telling the model
        // something the text no longer shows.
        assert!(out
            .content
            .contains(&truncation_notice(MAX_OUTPUT_CHARS + 1_000)));
        std::fs::remove_dir_all(&root).ok();
    }

    /// **A command cannot buy itself a model call by printing the harness's own
    /// notice** (REQ-561 verify).
    ///
    /// The size trigger used to be read back off the result's last line, and
    /// command output can end with any line at all — including this exact one,
    /// from a repository-controlled `Makefile` or build script, which makes it
    /// not self-inflicted. `grep`'s cap notice is unforgeable because every real
    /// hit there carries a `{path}:{line}: ` prefix; a line of command output
    /// carries nothing.
    ///
    /// The consequence was never only a wasted call: on a machine with no
    /// privacy boundary configured, unknown provenance takes the egress fast
    /// path, so the forged trigger would ship this command's output to whatever
    /// `build` is bound to — a frontier provider on the ordinary remote config —
    /// when the real trigger would not have sent it at all.
    ///
    /// Non-vacuity lives in the table above: its `ok-over` row proves a
    /// genuinely capped output still buys exactly one call, so this is the
    /// forgery being refused rather than the trigger being dead.
    #[tokio::test]
    async fn a_forged_truncation_notice_in_command_output_buys_no_model_call() {
        let root = temp_root("forged");
        let forged = truncation_notice(SHELL_TRIGGER_OUTPUT_CHARS + 1_000_000);
        std::fs::write(root.join("out.txt"), format!("all fine\n{forged}\n")).unwrap();
        let args = json!({ "command": "cat out.txt" });
        let (route, calls) = counting_route("The command produced a lot of output.");

        let (raw, refined) = run_and_refine(&root, &args, &route).await;

        // The fixture really is in the state the name claims: a short, successful
        // command whose own last line is the harness's notice.
        assert!(!raw.is_error, "{}", raw.content);
        assert!(
            raw.content.lines().next_back() == Some(forged.as_str()),
            "the fixture must end with the forged notice: {}",
            raw.content
        );
        assert!(
            raw.measured
                .is_some_and(|n| n <= SHELL_TRIGGER_OUTPUT_CHARS),
            "the fixture must be genuinely under the cap: {:?}",
            raw.measured
        );

        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "command output claiming to have been truncated bought an interpretation \
             — and, on a machine with no boundary configured, an egress"
        );
        assert_eq!(refined.outcome, raw);
        assert_eq!(refined.duty_error, None);
        std::fs::remove_dir_all(&root).ok();
    }

    /// **The duty is for command output, so a call that never ran a command does
    /// not buy one** (REQ-561 verify).
    ///
    /// Both pre-spawn failures: a missing `command` argument, and a repo root
    /// that does not exist. `worth_interpreting` sees `is_error == true` for both
    /// and would fire, handing the model a fixed harness sentence to interpret
    /// with an empty command line beside it.
    ///
    /// The provenance assertion is the other half. These two carry *empty*
    /// provenance rather than `Unknown` — correctly, since nothing ran and
    /// nothing came off the machine — and empty provenance takes the egress fast
    /// path without consulting a boundary. That is why `shell_duty`'s claim that
    /// "on a machine with any privacy boundary configured, a remotely bound shell
    /// duty is refused" needed this gate to be universal rather than usual.
    #[tokio::test]
    async fn a_command_that_never_ran_is_not_worth_interpreting() {
        for (label, root, args) in [
            (
                "no command argument",
                temp_root("pre-spawn-args"),
                json!({ "timeout_ms": 1_000 }),
            ),
            (
                "repo root does not exist",
                PathBuf::from("/nonexistent-teton-root-for-this-test"),
                json!({ "command": "echo hi" }),
            ),
        ] {
            let (route, calls) = counting_route("Something went wrong.");
            let (raw, refined) = run_and_refine(&root, &args, &route).await;

            assert!(raw.is_error, "{label}: the fixture must be a failure");
            assert_eq!(
                raw.measured, None,
                "{label}: nothing spawned, so nothing was measured"
            );
            assert_eq!(
                raw.provenance,
                ToolProvenance::none(),
                "{label}: a call that ran no command surfaced no command output"
            );
            assert!(
                shell_duty::worth_interpreting(raw.is_error, 1),
                "{label}: non-vacuity — this failure would be worth interpreting if it \
                 had said anything, so what refuses here is the missing measurement"
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                0,
                "{label}: a harness-authored argument error was sent to a model"
            );
            assert_eq!(refined.outcome, raw, "{label}");
            assert_eq!(refined.duty_error, None, "{label}");
            std::fs::remove_dir_all(&root).ok();
        }
    }

    /// **A failure with nothing to say buys no model call either** (REQ-561
    /// verify).
    ///
    /// The `measured` gate above stops the arms where no command ran. It does
    /// not stop the arms where a command ran and captured **nothing** — those
    /// measure `Some(0)`, so the old `failed || oversize` predicate said yes and
    /// the duty was handed one of three sentences the *harness* wrote:
    /// `command timed out after 50ms and was killed`, `command failed to run: …`,
    /// `command watcher disconnected`. That is the same purchase the `measured`
    /// gate exists to decline, made one step further along, and it is what the
    /// empty-output arm of [`shell_duty::worth_interpreting`] now refuses.
    ///
    /// The silent non-zero exit is in the same set and reaches it by a different
    /// route: `test -f missing` fails with an empty body, so what a duty would
    /// be given is a command line and `(exit 1)`.
    ///
    /// The last row is the non-vacuity, and it is the one that keeps this
    /// honest: the *same* failing command with one line of output still fires.
    /// What was removed is the empty case, not the failure trigger.
    #[tokio::test]
    async fn a_failure_that_captured_no_output_is_not_worth_interpreting() {
        for (label, args, expected_calls) in [
            (
                "timed out and was killed",
                json!({ "command": "sleep 5", "timeout_ms": 50 }),
                0usize,
            ),
            ("non-zero exit, silent", json!({ "command": "exit 3" }), 0),
            (
                "non-zero exit, with something to read",
                json!({ "command": "echo boom >&2; exit 3" }),
                1,
            ),
        ] {
            let root = temp_root(&format!("silent-{expected_calls}-{}", label.len()));
            let (route, calls) = counting_route("Something went wrong.");
            let (raw, refined) = run_and_refine(&root, &args, &route).await;

            assert!(raw.is_error, "{label}: the fixture must be a failure");
            assert!(
                raw.measured.is_some(),
                "{label}: a command that reached a shell always measures"
            );
            assert_eq!(
                raw.measured == Some(0),
                expected_calls == 0,
                "{label}: the fixture is not on the side of 'captured nothing' the row says"
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                expected_calls,
                "{label}: expected {expected_calls} model call(s)"
            );
            if expected_calls == 0 {
                assert_eq!(
                    refined.outcome, raw,
                    "{label}: a result no duty was made for must come back untouched"
                );
                assert_eq!(refined.duty_error, None, "{label}");
            } else {
                assert!(
                    refined.outcome.content.starts_with("[shell: "),
                    "{label}: the failure trigger itself must still work"
                );
            }
            std::fs::remove_dir_all(&root).ok();
        }
    }
}
