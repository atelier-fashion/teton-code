//! The `shell` tool: run a command under a timeout, a cwd jail, and a scrubbed
//! environment.
//!
//! Three hard constraints, each a security property (AC):
//!
//! - **cwd jail** — the command runs with the repo root as its working
//!   directory. (Absolute paths a command constructs itself are outside the
//!   tool's reach; the jail is the default surface an agent operates on.)
//! - **env scrub** — every variable whose name ends `_KEY` or `_TOKEN`
//!   (case-insensitive) is removed before the child starts, so an API key in the
//!   daemon's environment can never leak into a model-driven `env`/`printenv`
//!   (BR-7). `PATH`, `HOME`, and the rest pass through so ordinary commands still
//!   work.
//! - **timeout** — a runaway command is `SIGKILL`ed after the deadline and the
//!   timeout is reported to the model, so a bad command can never hang the loop.
//!
//! The command runs synchronously via `sh -c`; a watcher thread enforces the
//! deadline. Output (stdout + stderr) is captured and capped.

use std::io::Result as IoResult;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use serde_json::{json, Value};

use super::{opt_u64_arg, str_arg, Tool, ToolContext, ToolOutcome};

/// Cap on captured output characters, so a chatty command cannot blow the
/// small-model context budget.
const MAX_OUTPUT_CHARS: usize = 8_000;

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
        // which egress fail-closes whenever a boundary is configured. (The
        // pre-spawn argument/config errors above surface no command output and
        // carry no provenance.)
        match rx.recv_timeout(Duration::from_millis(timeout_ms)) {
            Ok(Ok(output)) => {
                let _ = handle.join();
                render_output(&command, &output).with_unknown_provenance()
            }
            Ok(Err(e)) => {
                let _ = handle.join();
                ToolOutcome::error(format!("command failed to run: {}", e.kind()))
                    .with_unknown_provenance()
            }
            Err(RecvTimeoutError::Timeout) => {
                // Kill by pid: wait_with_output moved the child into the watcher
                // thread, so we cannot call Child::kill here. libc is already a
                // daemon dependency (peer-cred / flock syscalls).
                // SAFETY: kill(2) with a pid we just spawned and a valid signal.
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGKILL);
                }
                let _ = handle.join();
                ToolOutcome::error(format!(
                    "command timed out after {timeout_ms}ms and was killed"
                ))
                .with_unknown_provenance()
            }
            Err(RecvTimeoutError::Disconnected) => {
                let _ = handle.join();
                ToolOutcome::error("command watcher disconnected").with_unknown_provenance()
            }
        }
    }
}

/// Remove secret-bearing variables (`*_KEY`, `*_TOKEN`) from an environment,
/// keeping everything else. Pure so it can be tested without mutating the
/// process environment.
pub(crate) fn scrub<I>(vars: I) -> Vec<(String, String)>
where
    I: IntoIterator<Item = (String, String)>,
{
    vars.into_iter()
        .filter(|(k, _)| !is_secret_key(k))
        .collect()
}

/// Whether an environment variable name looks like it holds a credential.
pub(crate) fn is_secret_key(key: &str) -> bool {
    let up = key.to_ascii_uppercase();
    up.ends_with("_KEY") || up.ends_with("_TOKEN")
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
    if body.chars().count() > MAX_OUTPUT_CHARS {
        let truncated: String = body.chars().take(MAX_OUTPUT_CHARS).collect();
        body = format!("{truncated}\n... (output truncated)");
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
    if code == Some(0) {
        ToolOutcome::ok(content)
    } else {
        ToolOutcome::error(content)
    }
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
    fn scrub_removes_only_secret_shaped_keys() {
        let input = vec![
            ("PATH".to_owned(), "/usr/bin".to_owned()),
            ("HOME".to_owned(), "/home/x".to_owned()),
            ("ANTHROPIC_API_KEY".to_owned(), "sk-secret".to_owned()),
            ("openai_api_key".to_owned(), "lower-secret".to_owned()),
            ("GITHUB_TOKEN".to_owned(), "ghp_secret".to_owned()),
            ("MY_KEYBOARD".to_owned(), "not-a-secret".to_owned()),
        ];
        let kept: Vec<String> = scrub(input).into_iter().map(|(k, _)| k).collect();
        assert!(kept.contains(&"PATH".to_owned()));
        assert!(kept.contains(&"HOME".to_owned()));
        assert!(kept.contains(&"MY_KEYBOARD".to_owned())); // ends in KEYBOARD, not _KEY
        assert!(!kept.contains(&"ANTHROPIC_API_KEY".to_owned()));
        assert!(!kept.contains(&"openai_api_key".to_owned()));
        assert!(!kept.contains(&"GITHUB_TOKEN".to_owned()));
    }

    #[test]
    fn is_secret_key_is_case_insensitive_and_suffix_anchored() {
        assert!(is_secret_key("FOO_KEY"));
        assert!(is_secret_key("foo_token"));
        assert!(!is_secret_key("KEYRING"));
        assert!(!is_secret_key("TOKENIZER"));
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
}
