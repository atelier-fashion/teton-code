//! The `shell` tool: run a command under a timeout, a cwd jail, and a composed
//! environment.
//!
//! Three hard constraints, each a security property (AC) — plus one usability
//! guarantee about `PATH` that the security constraints do not imply:
//!
//! - **cwd jail** — the command runs with the session root as its working
//!   directory. (Absolute paths a command constructs itself are outside the
//!   tool's reach; the jail is the default surface an agent operates on.)
//! - **env allowlist** — the child's environment is *composed*, not filtered
//!   (REQ-596). Only the names on [`crate::child_env::SHELL_ENV_ALLOW`] are
//!   admitted from the daemon's environment, so a variable nobody thought about
//!   is absent by default and adding one to the daemon can never silently widen
//!   what the child sees (BR-2). What the allowlist admits is then checked by
//!   *value* — a `scheme://user:pass@…` URL is withheld whatever it is called
//!   (BR-8) — and finally every variable a configured `auth_ref = "env:<VAR>"`
//!   names is removed unconditionally, so the allowlist cannot re-admit a
//!   credential the user told the daemon about (BR-1, BR-3).
//!
//!   This replaced a name-shaped denylist, which missed any credential whose
//!   variable name contained none of its substrings — `env:DEEPSEEK_AUTH` and
//!   `env:MY_LLM_CRED` among them — and one `echo $VAR` put such a value in tool
//!   output bound for the next remote turn. The composer is shared with the MCP
//!   spawn path, which had the right model first.
//! - **PATH floor** — `PATH` passes through *and is then floored* with the
//!   package-manager prefixes in [`PATH_FLOOR`](crate::env_path::PATH_FLOOR). Inheriting it unmodified was
//!   the BUG-174 defect: the daemon's `PATH` is only as good as whatever started
//!   it, and under launchd that is `/usr/bin:/bin:/usr/sbin:/sbin`, in which no
//!   Homebrew binary — `teton` included — can be found.
//! - **timeout** — a runaway command is `SIGKILL`ed after the deadline and the
//!   timeout is reported to the model, so a bad command can never hang the loop.
//!   The child is spawned as its own process-group leader and the whole group is
//!   killed, so a backgrounded grandchild cannot outlive the deadline (REQ-544
//!   L-2).
//!
//! The command runs synchronously via `sh -c`; a watcher thread enforces the
//! deadline. Output (stdout + stderr) is captured and capped.
//!
//! ## Three functions, because the spawn body has two callers and one is not a tool
//!
//! [`run_bounded`] is the spawn body — jail, env composition, `PATH` floor, process
//! group, deadline, group kill — and it hands back the **raw** streams as a
//! typed [`BoundedRun`]. [`cap_output`] is the ceiling, applied over the
//! *merged* stdout/stderr body. `render_output` is this tool's presentation: it
//! merges, caps, prepends the status line, and hands [`cap_output`]'s
//! **pre-cap** length to `measuring(…)`.
//!
//! The cap is its own function rather than the runner's last step because those
//! are two different numbers and the duty's size trigger is decided on the
//! first (REQ-561 ADR-5). A runner that capped would make `measured` the
//! *capped* length, at which point the trigger compares a truncated body
//! against the cap that truncated it and can never fire (LESSON-443). Splitting
//! it out rather than leaving it inside `render_output` is the other half: a
//! second caller — a skill's dynamic context (REQ-585 ADR-14), which wants the
//! bytes without this tool's status line or its duty — must reach the ceiling
//! without reaching the presentation, or it would inline uncapped command
//! output into a prompt.
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
use std::path::Path;
use std::process::{Command, ExitStatus, Output, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use teton_protocol::methods::RootKind;

use super::{
    opt_str_arg, opt_u64_arg, str_arg, RefinedOutcome, Tool, ToolContext, ToolDuties, ToolOutcome,
};
use std::collections::BTreeMap;
use crate::harness::digest::tool_result_provenance;
use crate::harness::shell_duty;

/// The sentence appended to a timeout from a `home`/`filesystem_root` context
/// on macOS (REQ-583 BR-14): the one place a killed command is plausibly a
/// consent dialog nobody can see, because the session root's own trees are the
/// ones the OS gates. From a `project` root the message is unchanged — no
/// noise where the cause is implausible.
///
/// `cfg!`-selected at the call site rather than a `#[cfg]` item, so both
/// spellings compile on every platform (the `service.rs` idiom).
const TIMEOUT_CONSENT_HINT: &str = " On macOS a consent dialog for a protected folder holds a \
                                    command until it is answered — narrow the command to a \
                                    project path or move the session root with /cd.";

/// Cap on captured output characters, so a chatty command cannot blow the
/// small-model context budget.
///
/// Also the `shell` duty's size trigger — a successful command is worth
/// interpreting exactly when this cap threw information away — which is why
/// [`shell_duty::SHELL_TRIGGER_OUTPUT_CHARS`] reads it rather than restating it
/// (REQ-561 ADR-5).
pub(crate) const MAX_OUTPUT_CHARS: usize = 8_000;

/// The timeout a `shell` call gets when it names none — and, since REQ-585, the
/// deadline a skill's dynamic-context command runs under (BR-6: "the `shell`
/// tool's jail, timeout and output cap").
///
/// A named constant rather than a literal inside [`ShellTool::default`] for
/// [`MAX_OUTPUT_CHARS`]'s reason: the skill path is a second consumer of the
/// same figure, and a second consumer that restated it would be two spellings of
/// one deadline (LESSON-528).
pub(crate) const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Runs shell commands under a timeout, cwd jail, and composed environment.
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
            default_timeout_ms: DEFAULT_TIMEOUT_MS,
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
        "Run a shell command in the session root under a timeout. Use it to \
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
        // The third is [`BoundedRun::SpawnFailed`] below — the only one of the
        // three where a launch was actually attempted, and still one where no
        // command output exists to interpret.
        let command = match str_arg(args, "command") {
            Ok(c) => c,
            Err(e) => return e.into(),
        };
        // The tool's own precondition, and it stays the tool's: the refusal is
        // the context's own sentence — the one `resolve`, `glob` and `grep`
        // print too — not something a runner shared with the skill path should
        // be phrasing. [`run_bounded`] canonicalizes again for its jail, which
        // on the path through here is a no-op on an already-canonical root; the
        // duplicate is what lets neither caller depend on the other's check.
        let root = match ctx.repo_root().canonicalize() {
            Ok(r) => r,
            Err(_) => return ctx.root_missing_error().into(),
        };

        let timeout_ms = opt_u64_arg(args, "timeout_ms")
            .unwrap_or(self.default_timeout_ms)
            .min(self.max_timeout_ms);

        // BR-1 (REQ-544 C-1): a shell command runs arbitrary code, so the daemon
        // cannot know which files its output was derived from. Every result of a
        // command that actually started is therefore tagged UNKNOWN provenance,
        // which egress fail-closes whenever a boundary is configured. (The three
        // arms that carry none — the two pre-spawn ones above and the failed
        // spawn below — surface no command output.)
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
        match run_bounded(&root, &command, timeout_ms) {
            BoundedRun::Completed {
                status,
                stdout,
                stderr,
            } => render_output(&command, status, &stdout, &stderr).with_unknown_provenance(),
            // The third arm with no measurement and no provenance: nothing ran,
            // so nothing this machine holds is in the answer. The runner has
            // already phrased the reason.
            BoundedRun::SpawnFailed(reason) => ToolOutcome::error(reason),
            BoundedRun::Lost(reason) => ToolOutcome::error(reason)
                .with_unknown_provenance()
                .measuring(NO_OUTPUT_CAPTURED),
            BoundedRun::TimedOut => {
                let mut message = format!("command timed out after {timeout_ms}ms and was killed");
                // BR-14: from a home-kind root on macOS, the likeliest reason a
                // command hangs is a consent dialog for a protected folder that
                // nobody at the terminal can see. Say so, once, only there.
                //
                // This sentence, and the decoration around it, are the *tool's*
                // presentation of a timeout. `run_bounded` reports only that the
                // deadline passed: the skill path answers the same fact with a
                // placeholder, and neither spelling belongs to the runner.
                if cfg!(target_os = "macos")
                    && matches!(ctx.root_kind(), RootKind::Home | RootKind::FilesystemRoot)
                {
                    message.push_str(TIMEOUT_CONSENT_HINT);
                }
                ToolOutcome::error(message)
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
        // session root that does not exist never reached a shell, and handing the
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

/// What one bounded run came to — the whole of [`run_bounded`]'s answer.
///
/// Typed rather than a rendered sentence, because the callers decorate the arms
/// differently and must not have to recover the arm by reading text. The
/// distinction that carries the most weight is **ran** versus **never ran**:
/// [`Tool::run`] tags anything that reached a shell with unknown provenance and
/// a measurement and tags the rest with neither, and a skill's dynamic context
/// leaves a different placeholder for each (REQ-585 BR-6).
#[derive(Debug)]
pub(crate) enum BoundedRun {
    /// The command ran to completion inside the deadline.
    ///
    /// The streams are **raw**: unmerged, unrendered, and *uncapped*. The
    /// ceiling is [`cap_output`]'s, applied by whichever caller is presenting
    /// them, because the pre-cap length is a number one of them needs
    /// (REQ-561 ADR-5).
    Completed {
        status: ExitStatus,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    /// The deadline passed and the whole process group was `SIGKILL`ed. What to
    /// *say* about that is the caller's — the tool has a sentence and a
    /// consent hint, the skill path has a placeholder.
    TimedOut,
    /// The command **never started**: the jail root could not be resolved, or
    /// `sh` could not be launched. `reason` is a whole sentence.
    SpawnFailed(String),
    /// The command **started** and its output never arrived — the collector's
    /// `wait_with_output` failed, or its channel hung up. Distinct from
    /// [`BoundedRun::SpawnFailed`] on the axis that matters: something ran on
    /// this machine, so a caller that tags provenance still has to tag it.
    /// `reason` is a whole sentence.
    Lost(String),
}

/// Run `command` under the shell tool's jail, composed environment, `PATH`
/// floor, process group and deadline — and hand back the **raw** result.
///
/// This is the whole of the spawn body and the single home of every guarantee
/// this module's header claims. It is deliberately *not* the home of the output
/// cap ([`cap_output`]) or of any sentence a user or a model reads: those
/// differ between the two callers, and a runner that owned them would have to
/// grow a mode flag.
///
/// `root` is canonicalized here rather than trusted, so the jail is this
/// function's property and not an invariant a caller has to maintain.
pub(crate) fn run_bounded(root: &Path, command: &str, timeout_ms: u64) -> BoundedRun {
    // Phrased exactly as the spawn failure below, on purpose: a root that
    // disappears between a caller's own check and this call is the same
    // `NotFound` that `spawn` would have reported a microsecond later, and no
    // caller should be able to tell which syscall happened to notice first.
    let root = match root.canonicalize() {
        Ok(root) => root,
        Err(e) => return BoundedRun::SpawnFailed(spawn_failure(&e)),
    };

    // REQ-596: a positive allowlist, not a denylist. The composer is shared with
    // the MCP spawn path and takes this path's own allowlist as a parameter, so
    // the two can never widen each other (BR-7.1). It also applies the BUG-174
    // `PATH` floor — the daemon's own `PATH` is only as good as whatever started
    // it, and launchd starts it with a bare one — and removes every variable a
    // configured `auth_ref = "env:<VAR>"` names, unconditionally and last (BR-1,
    // BR-3). This is the single construction site for a shell child's
    // environment; the region check in `child_env`'s tests fails the build if a
    // second one appears (AC-8).
    let child_env = crate::child_env::compose_child_env(
        std::env::vars(),
        crate::child_env::SHELL_ENV_ALLOW,
        &crate::child_env::credential_env_names(),
        &BTreeMap::new(),
    );

    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(&root)
        .env_clear()
        .envs(child_env)
        // REQ-544 L-2: make the child its own process-group leader (pgid ==
        // child pid) so that on timeout we can SIGKILL the whole group and no
        // backgrounded grandchild survives the deadline.
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return BoundedRun::SpawnFailed(spawn_failure(&e)),
    };
    let pid = child.id();

    let (tx, rx) = mpsc::channel::<IoResult<Output>>();
    let handle = std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });

    // **The group is killed on the timeout arm only** — and the alternative
    // was tried and reverted (REQ-569 re-verify, R4).
    //
    // The verify pass moved the `kill(-pgid)` up here, ahead of the match,
    // so it ran on *every* ending. The intent was to reach the escapee in
    // `sh -c 'helper >/dev/null 2>&1 &'`: the command backgrounds a
    // grandchild, closes the pipes, `wait_with_output` returns promptly and
    // successfully, and on the old shape nothing killed the group. The
    // escapee reparents to `launchd`/`init`, which breaks the ancestry
    // chain REQ-569 BR-4 keys on — it reconnects classified
    // `NotDescendant`, with full client rights.
    //
    // It is reverted because it cost more than it bought:
    //
    // - **It killed work a command legitimately backgrounded.** `npm run dev
    //   &`, a fixture server, a language server — anything an agent starts
    //   on purpose and expects to outlive one tool call died on the
    //   *success* path. That is a functional regression in the common case,
    //   paid for a security case the same change did not close.
    // - **On a success arm the group leader has already been reaped** by
    //   `wait_with_output`, so the pgid may already have been released.
    //   Signalling it then is not the `ESRCH` no-op the timeout arm's
    //   reasoning describes: it can reach a *recycled* group.
    // - **It did not close the escape it was aimed at.** `setsid helper`
    //   leaves the process group outright, so no group-directed signal on
    //   any arm reaches it.
    //
    // So the escape stands, recorded rather than papered over, in
    // `crate::peer`'s module docs and in the REQ-569 architecture ADR-A
    // residuals. Closing it needs a mechanism that does not key on the
    // process group at all.
    match rx.recv_timeout(Duration::from_millis(timeout_ms)) {
        Ok(Ok(Output {
            status,
            stdout,
            stderr,
        })) => {
            let _ = handle.join();
            BoundedRun::Completed {
                status,
                stdout,
                stderr,
            }
        }
        Ok(Err(e)) => {
            let _ = handle.join();
            BoundedRun::Lost(format!("command failed to run: {}", e.kind()))
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
            BoundedRun::TimedOut
        }
        Err(RecvTimeoutError::Disconnected) => {
            let _ = handle.join();
            BoundedRun::Lost("command watcher disconnected".to_owned())
        }
    }
}

/// The one spelling of "it never started", shared by the two ways that can
/// happen so a caller cannot distinguish them (see [`run_bounded`]).
fn spawn_failure(error: &std::io::Error) -> String {
    format!("failed to start command: {}", error.kind())
}

/// The line [`cap_output`] appends when the command's raw output ran past
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

/// Apply the [`MAX_OUTPUT_CHARS`] ceiling to a merged stdout+stderr body, and
/// report the length it had **before** the cap.
///
/// The one home of the ceiling, and the reason it is a function of its own
/// rather than the last step of [`run_bounded`] or a few lines inside
/// `render_output`:
///
/// - **The pre-cap length is a second answer, not an implementation detail.**
///   It is what the `shell` duty's size trigger is decided on (REQ-561 ADR-5),
///   and this is the last moment it exists — the caller is handed only the
///   capped text. Capping inside the runner would make the measured length the
///   *capped* one, so the trigger would compare a clamped body against the cap
///   that clamped it and could never fire (LESSON-443).
/// - **Both callers need the ceiling and only one needs the presentation.** A
///   skill's dynamic context inlines these bytes into a prompt (REQ-585 BR-6)
///   without the status line, the error flag or the duty; leaving the cap
///   inside `render_output` would mean it inlined *uncapped* output.
///
/// The input is the merged body rather than the two streams because the cap is
/// over what the model will read, `[stderr] ` label included — capping the
/// streams separately would admit twice the ceiling.
///
/// # The third element
///
/// **Whether the ceiling fired** — a third answer, for the reason the second is
/// one. REQ-585's `skill_invoked` reports it (`truncated`, so a surface can say
/// the model is reading a prefix), and it is the branch this function has
/// already taken: a caller that re-derived it would need the comparison, which
/// means a second copy of the ceiling in a second file — the very thing
/// [`the_output_cap_has_exactly_one_home`](tests::the_output_cap_has_exactly_one_home)
/// forbids. It is not recoverable from the pair alone: a capped body's length
/// and the length it reports are two independent numbers that can coincide.
pub(crate) fn cap_output(merged: String) -> (String, usize, bool) {
    let raw_output_chars = merged.chars().count();
    if raw_output_chars <= MAX_OUTPUT_CHARS {
        return (merged, raw_output_chars, false);
    }
    let truncated: String = merged.chars().take(MAX_OUTPUT_CHARS).collect();
    (
        format!("{truncated}\n{}", truncation_notice(raw_output_chars)),
        raw_output_chars,
        true,
    )
}

/// Render a finished command's output for the model, capped.
fn render_output(command: &str, status: ExitStatus, stdout: &[u8], stderr: &[u8]) -> ToolOutcome {
    let mut merged = String::new();
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    if !stdout.trim().is_empty() {
        merged.push_str(stdout.trim_end());
        merged.push('\n');
    }
    if !stderr.trim().is_empty() {
        merged.push_str("[stderr] ");
        merged.push_str(stderr.trim_end());
        merged.push('\n');
    }
    // The cap, and the length the command **actually** produced — which
    // [`cap_output`] takes before it truncates, because that number is the one
    // the `shell` duty's size trigger is decided on and the capped text can no
    // longer show it (REQ-561 ADR-5, LESSON-443).
    let (body, raw_output_chars, _truncated) = cap_output(merged);

    let code = status.code();
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
    use crate::env_path::apply_path_floor;
    use std::path::{Path, PathBuf};

    /// The counter, not the timestamp, guarantees uniqueness: `SystemTime::now()`
    /// can return the same value for two calls within one clock tick.
    fn temp_root(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "teton-shell-{tag}-{}-{}-{}",
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

    /// **AC-19 (BR-14).** A timeout under a `home`-kind context carries the
    /// consent-dialog sentence on macOS (and nowhere else); the same command
    /// under a `project`-kind context receives today's message byte-for-byte.
    #[test]
    fn a_timeout_from_a_home_kind_root_hints_at_the_consent_dialog() {
        let root = temp_root("ac19");
        let args = json!({ "command": "sleep 10" });
        let plain = "command timed out after 200ms and was killed";

        for kind in [RootKind::Home, RootKind::FilesystemRoot] {
            let ctx = ToolContext::new(&root).with_root_kind(kind);
            let out = ShellTool::with_timeouts(200, 500).run(&ctx, &args);
            assert!(out.is_error);
            if cfg!(target_os = "macos") {
                assert_eq!(
                    out.content,
                    format!("{plain}{TIMEOUT_CONSENT_HINT}"),
                    "{kind:?}"
                );
                assert!(out.content.contains("consent dialog"), "{kind:?}");
            } else {
                assert_eq!(out.content, plain, "{kind:?}: the hint is macOS-only");
            }
            // The arm's other facts are unchanged: no output, unknown provenance.
            assert_eq!(out.measured, Some(0), "{kind:?}");
        }

        for kind in [RootKind::Project, RootKind::Plain] {
            let ctx = ToolContext::new(&root).with_root_kind(kind);
            let out = ShellTool::with_timeouts(200, 500).run(&ctx, &args);
            assert!(out.is_error);
            assert_eq!(
                out.content, plain,
                "{kind:?}: no noise where the cause is implausible"
            );
            assert_eq!(out.measured, Some(0), "{kind:?}");
        }
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

    /// **The runner still floors the child's `PATH`** (BUG-174) — asserted
    /// against the function that does the flooring rather than a hand-listed set
    /// of directories, so the two cannot drift.
    ///
    /// This is the extraction's faithfulness check for the one guarantee in the
    /// spawn body whose removal nothing else notices. The floor is a
    /// *usability* control, so deleting it changes nothing at all on a machine
    /// whose ambient `PATH` already names every floor directory that exists —
    /// which is most developer machines and most of CI. The equality below
    /// fails wherever the floor adds anything, which is exactly the machine
    /// class BUG-174 is about (a launchd-started daemon inheriting
    /// `/usr/bin:/bin:/usr/sbin:/sbin`), and passes rather than flaking where it
    /// adds nothing.
    ///
    /// It also pins the "exactly one `PATH`" half: `apply_path_floor` rewrites
    /// the variable rather than appending a second one, and `env_clear` plus
    /// `envs` would happily carry two.
    /// AC-1 and AC-1.1, end to end: a credential the daemon was *told* about
    /// through `auth_ref = "env:<VAR>"` never reaches a `shell` child — and that
    /// holds for **both** fields `is_recognized_auth_ref` gates, not only
    /// `providers[].auth_ref` (BR-1.1).
    ///
    /// # What the AC-5 mutation actually shows, and where it does not
    ///
    /// AC-5 asks that deleting the BR-1 removal step (composer step 5) makes
    /// AC-1 fail. Run against the two *named* credentials below it does **not**,
    /// and the reason is structural rather than a gap in the test: under BR-2's
    /// allowlist, `DEEPSEEK_AUTH_SENTINEL_*` was never admitted in the first
    /// place, so two guards stand between it and the child and removing one
    /// changes nothing observable. The AC was written for a world where BR-1 is
    /// the only guard.
    ///
    /// So this test pins the case where step 5 **is** load-bearing: `LANGUAGE`
    /// is on the allowlist, so nothing but the unconditional credential removal
    /// keeps it out once a config names it. That is BR-3's scenario exactly —
    /// the allowlist cannot re-admit a configured credential.
    ///
    /// **Mutation run (AC-5, BR-1 half).** Deleting the
    /// `for name in credential_env_names { env.remove(name); }` loop from
    /// `compose_child_env` fails the `LANGUAGE` assertion here — "an allowlisted
    /// name the config declared a credential reached the child" — and fails
    /// `child_env::tests::a_credential_name_on_the_allowlist_is_still_removed`
    /// and `a_declared_var_cannot_re_admit_a_credential_name`, 3 assertions in
    /// all. The two named-credential assertions stay green, which is the honest
    /// report: the allowlist alone already withheld them.
    #[test]
    fn a_configured_credential_never_reaches_the_child_from_either_gated_field() {
        let root = temp_root("credential-env");
        let unique = std::process::id();
        let provider_var = format!("DEEPSEEK_AUTH_SENTINEL_{unique}");
        let web_var = format!("WEB_SEARCH_SENTINEL_{unique}");

        let config = teton_core::config::Config::from_toml(&format!(
            r#"
[[providers]]
id = "deepseek"
kind = "openai-compatible"
endpoint = "https://deepseek.invalid/v1"
model = "m"
auth_ref = "env:{provider_var}"

[web]
search_key_ref = "env:{web_var}"
"#
        ))
        .expect("the fixture config parses");

        // `LANGUAGE` is allowlisted, so it is the one name here that composer
        // step 5 alone keeps out. Declared as a credential by a second config so
        // the assertion is about the rule, not about this provider.
        let mut names = crate::child_env::credential_env_names_of(&config);
        names.insert("LANGUAGE".to_owned());
        crate::child_env::set_credential_env_names_provider(move || names.clone());

        let secrets = [
            (provider_var.clone(), format!("SENTINEL-provider-{unique}")),
            (web_var.clone(), format!("SENTINEL-web-{unique}")),
            ("LANGUAGE".to_owned(), format!("SENTINEL-language-{unique}")),
        ];
        // SAFETY: process-unique names apart from `LANGUAGE`, which nothing in
        // this suite reads; set and removed around one spawn.
        for (k, v) in &secrets {
            unsafe { std::env::set_var(k, v) };
        }

        let stdout = match run_bounded(&root, "env", 5_000) {
            BoundedRun::Completed { stdout, .. } => stdout,
            other => panic!("the fixture must run to completion: {other:?}"),
        };
        for (k, _) in &secrets {
            unsafe { std::env::remove_var(k) };
        }
        let printed = String::from_utf8_lossy(&stdout);

        // AC-1: the provider field.
        assert!(
            !printed.contains(provider_var.as_str()) && !printed.contains(&secrets[0].1),
            "a providers[].auth_ref credential reached the child"
        );
        // AC-1.1: the web field, which BR-1.1 exists for.
        assert!(
            !printed.contains(web_var.as_str()) && !printed.contains(&secrets[1].1),
            "a [web] search_key_ref credential reached the child — covering only the \
             provider half is the leak this REQ closes, in the field written second"
        );
        // BR-3, and the assertion the AC-5 mutation moves.
        assert!(
            !printed.contains("LANGUAGE=") && !printed.contains(&secrets[2].1),
            "an allowlisted name the config declared a credential reached the child"
        );
        assert!(
            printed.contains("PATH="),
            "the child received no environment at all, so this fixture proved nothing"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// AC-2 and AC-6. The names that motivated REQ-596: each matches **none** of
    /// the retired denylist's substrings (`SECRET`, `PASSWORD`, `PASSWD`,
    /// `TOKEN`, `KEY`, `CREDENTIAL`, `PAT`), so under the old rule every one of
    /// them reached `sh -c` intact and a single `echo $VAR` put the value into
    /// tool output bound for the next remote turn. They are gone now because the
    /// allowlist never admitted them — not because a longer denylist caught
    /// them, which is the distinction the REQ exists to make.
    ///
    /// The assertion is over **captured child output** (AC-6): the child really
    /// runs `env`, and what it printed is what is searched. Asserting that a
    /// composer was called would prove only that a call happened.
    ///
    /// `RANDOM_UNRELATED_VAR` rides along to pin AC-3 end-to-end: the allowlist
    /// withholds by default, not only for things that look like secrets.
    #[test]
    fn a_credential_named_nothing_like_a_credential_never_reaches_the_child() {
        let root = temp_root("env-allowlist");
        let unique = std::process::id();
        // SAFETY: process-unique names, set and removed around one spawn.
        let planted: Vec<(String, String)> = [
            "MY_LLM_CRED",
            "GEMINI_PW",
            "LLM_AUTH",
            "RANDOM_UNRELATED_VAR",
        ]
        .iter()
        .map(|n| {
            (
                format!("{n}_{unique}"),
                format!("SENTINEL-value-{n}-{unique}"),
            )
        })
        .collect();
        for (k, v) in &planted {
            unsafe { std::env::set_var(k, v) };
        }

        let stdout = match run_bounded(&root, "env", 5_000) {
            BoundedRun::Completed { stdout, .. } => stdout,
            other => panic!("the fixture must run to completion: {other:?}"),
        };
        for (k, _) in &planted {
            unsafe { std::env::remove_var(k) };
        }

        let printed = String::from_utf8_lossy(&stdout);
        for (name, value) in &planted {
            assert!(
                !printed.contains(name.as_str()),
                "the child's environment still names {name}"
            );
            assert!(
                !printed.contains(value.as_str()),
                "a planted value reached the child under {name}"
            );
        }
        // The floor still ran, so this is not vacuously green on an empty
        // environment.
        assert!(
            printed.contains("PATH="),
            "the child received no PATH at all, so this fixture proved nothing"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_bounded_runner_floors_the_childs_path() {
        let root = temp_root("path-floor");

        // The oracle is the floor rule applied to the daemon's own `PATH`, NOT
        // `compose_child_env` — the subject must not compute its own expected
        // value (conventions.md, LESSON-569). Taking `PATH` straight from the
        // daemon environment mirrors what the allowlist admits without asking
        // the composer anything.
        let mut expected_env: Vec<(String, String)> =
            std::env::vars().filter(|(k, _)| k == "PATH").collect();
        apply_path_floor(&mut expected_env);
        let expected: Vec<&String> = expected_env
            .iter()
            .filter(|(k, _)| k == "PATH")
            .map(|(_, v)| v)
            .collect();
        assert_eq!(
            expected.len(),
            1,
            "the floor must leave exactly one PATH behind"
        );

        let stdout = match run_bounded(&root, "printf %s \"$PATH\"", 5_000) {
            BoundedRun::Completed { stdout, .. } => stdout,
            other => panic!("the fixture must run to completion: {other:?}"),
        };
        assert_eq!(
            &String::from_utf8_lossy(&stdout),
            expected[0],
            "the child's PATH is not the one `apply_path_floor` produces from the daemon's \
             own environment"
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

    /// **The cap reports the length it saw, not the one it left behind.**
    ///
    /// The pair [`cap_output`] returns is two different facts, and
    /// `render_output` hands the second to `measuring(…)`. Counting *after* the
    /// truncation instead would report `MAX_OUTPUT_CHARS` for every capped
    /// command, at which point the `shell` duty's size trigger compares the cap
    /// against itself and can never fire (LESSON-443).
    ///
    /// That mutation is caught twice on purpose — here on the function, and
    /// through the whole tool by
    /// [`the_raw_output_length_is_carried_beside_the_text_not_inside_it`] — because
    /// the ceiling now has a second caller that never touches `render_output`.
    #[test]
    fn cap_output_reports_the_length_before_the_cap_not_after() {
        // Under the cap: the body is handed back untouched, and the length is
        // its own.
        let short = "hello\n[stderr] boom\n".to_owned();
        let (text, raw, truncated) = cap_output(short.clone());
        assert_eq!(text, short, "an uncapped body must not be rewritten");
        assert_eq!(raw, short.chars().count());
        assert!(!truncated, "a body under the cap threw nothing away");

        // Exactly at the cap is not past it — the boundary the duty's trigger
        // sits on (`worth_interpreting` is `>`, not `>=`).
        let at_the_cap = "x".repeat(MAX_OUTPUT_CHARS);
        let (text, raw, truncated) = cap_output(at_the_cap.clone());
        assert_eq!(text, at_the_cap, "at the cap is not past it");
        assert_eq!(raw, MAX_OUTPUT_CHARS);
        assert!(!truncated, "at the cap is not past it, so nothing was cut");

        // Over it: the text is clamped and the *reported* length is the pre-cap
        // one, which the clamped text can no longer show.
        let over = MAX_OUTPUT_CHARS + 1_000;
        let (text, raw, truncated) = cap_output("x".repeat(over));
        assert!(truncated, "a body past the cap reports that it was cut");
        assert_eq!(
            raw, over,
            "the reported length must be what the command produced, not what survived the cap"
        );
        let notice = truncation_notice(over);
        assert!(text.ends_with(&notice), "the notice must close the body");
        assert_eq!(
            text.chars().count(),
            MAX_OUTPUT_CHARS + 1 + notice.chars().count(),
            "the kept body is exactly the cap, plus the separating newline and the notice"
        );
    }

    /// **LESSON-546: a one-home rule is a test, not a grep in a task file.**
    ///
    /// [`MAX_OUTPUT_CHARS`] is the shell tool's output ceiling *and* the `shell`
    /// duty's size trigger, which is why `shell_duty` derives its constant from
    /// it rather than restating the number (REQ-561 ADR-5). Extracting
    /// [`cap_output`] moved the place the cap is *applied*; this asserts it left
    /// no second one behind, in both directions:
    ///
    /// - the number 8,000 is written down in exactly one production line, and
    /// - `MAX_OUTPUT_CHARS` is named by exactly two production files — this one,
    ///   where it is defined and applied, and `shell_duty`, which derives from
    ///   it — with exactly one application site here.
    ///
    /// The second caller this REQ adds (a skill's dynamic context, REQ-585
    /// ADR-14) reaches the ceiling through [`cap_output`] and so appears in
    /// neither list. A file that *does* appear has re-implemented the cap, which
    /// is the thing being refused.
    #[test]
    fn the_output_cap_has_exactly_one_home() {
        use crate::call_sites::scan::{code_only, count, production_sources};

        /// Occurrences of `needle` that are not part of a longer word.
        ///
        /// `8_000` is a substring of `128_000`, and `8000` of `128000`, both of
        /// which the daemon writes for provider context windows. A plain
        /// substring count would charge those to this constant.
        fn standalone(haystack: &str, needle: &str) -> usize {
            let free = |c: Option<char>| !c.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
            haystack
                .match_indices(needle)
                .filter(|(at, _)| {
                    free(haystack[..*at].chars().next_back())
                        && free(haystack[at + needle.len()..].chars().next())
                })
                .count()
        }

        let sources: Vec<(String, String)> = production_sources()
            .into_iter()
            .map(|(rel, src)| (rel, code_only(&src)))
            .collect();
        // Non-vacuity: the sweep really did see the module it is about.
        assert!(
            sources
                .iter()
                .any(|(rel, _)| rel == "harness/tools/shell.rs"),
            "the scan missed the module it is about: {:?}",
            sources.iter().map(|(rel, _)| rel).collect::<Vec<_>>()
        );

        let number_homes: Vec<(&str, usize)> = sources
            .iter()
            .filter_map(|(rel, code)| {
                let n = standalone(code, "8_000") + standalone(code, "8000");
                (n > 0).then_some((rel.as_str(), n))
            })
            .collect();
        assert_eq!(
            number_homes,
            vec![("harness/tools/shell.rs", 1usize)],
            "8,000 is the shell tool's output cap and must be written down exactly once, at \
             `MAX_OUTPUT_CHARS`. Everything else that needs it reads the constant."
        );

        let identifier_homes: Vec<&str> = sources
            .iter()
            .filter(|(_, code)| code.contains("MAX_OUTPUT_CHARS"))
            .map(|(rel, _)| rel.as_str())
            .collect();
        assert_eq!(
            identifier_homes,
            vec!["harness/shell_duty.rs", "harness/tools/shell.rs"],
            "only the module that owns the cap and the duty that derives its trigger from it \
             may name `MAX_OUTPUT_CHARS`. A third file is a second cap; call `cap_output`."
        );

        let here = sources
            .iter()
            .find(|(rel, _)| rel == "harness/tools/shell.rs")
            .map(|(_, code)| code.as_str())
            .expect("this module is a production source");
        assert_eq!(
            count(here, "MAX_OUTPUT_CHARS"),
            3,
            "the cap is applied in exactly one place: the constant's definition, plus the \
             comparison and the `take` inside `cap_output`. A fourth mention is a second \
             application site."
        );
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
    /// Both pre-spawn failures: a missing `command` argument, and a session root
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
                "session root does not exist",
                PathBuf::from("/nonexistent-teton-root-for-this-test"),
                json!({ "command": "echo hi" }),
            ),
        ] {
            let (route, calls) = counting_route("Something went wrong.");
            let (raw, refined) = run_and_refine(&root, &args, &route).await;

            assert!(raw.is_error, "{label}: the fixture must be a failure");
            if label == "session root does not exist" {
                // The missing-root arm is the context's one refusal, verbatim
                // — the sentence `resolve`, `glob` and `grep` print too.
                assert_eq!(
                    raw.content,
                    ToolContext::new(&root).root_missing_error().to_string(),
                    "{label}"
                );
                assert!(
                    raw.content.contains("does not exist"),
                    "{label}: {}",
                    raw.content
                );
            }
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
            if label == "session root does not exist" {
                // The missing-root arm is the context's one refusal, verbatim
                // — the sentence `resolve`, `glob` and `grep` print too.
                assert_eq!(
                    raw.content,
                    ToolContext::new(&root).root_missing_error().to_string(),
                    "{label}"
                );
                assert!(
                    raw.content.contains("does not exist"),
                    "{label}: {}",
                    raw.content
                );
            }
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
