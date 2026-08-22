//! `teton uninstall` — the whole removal chain behind one command.
//!
//! Homebrew formulae have no uninstall hook (`uninstall`/`zap` stanzas are
//! cask-only), and `brew uninstall` does not even stop a running `brew
//! services` service — so `brew uninstall teton` alone would leave the daemon
//! running, the multi-gigabyte model on disk, and the logs behind. This
//! subcommand owns the full sequence instead: stop the service, verify the
//! daemon is actually down, delete the state directory and logs, and finish
//! with `brew uninstall teton`.
//!
//! The state directory holds the downloaded model weights and the cost ledger —
//! irreversible to delete and expensive to re-download — so the whole plan is
//! shown and confirmed **before** anything is touched, and the confirmation
//! defaults to *no* (same rule as the over-RAM model confirmation: removal
//! cannot happen by pressing return). `--keep-data` preserves the directory;
//! the global `--yes` answers the confirmation for unattended runs.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail};

use teton_protocol::socket_path::DaemonPaths;

use crate::client::Connection;
use crate::firstrun::format_bytes;
use crate::prompt::Prompter;
use crate::render::{LineKind, Surface};

/// The Homebrew tap the install instructions register. Uninstall removes it
/// too — a tap kept for one formula is dead weight once that formula is gone.
const TAP: &str = "atelier-fashion/tap";

/// What the uninstall will touch, resolved up front so the confirmation can
/// name every step (and its cost) before anything runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Homebrew's prefix, when `brew` is on `PATH`. `None` means the binaries
    /// were not brew-managed (or brew is broken) — the service and binary
    /// steps are skipped and reported, not silently dropped.
    pub brew_prefix: Option<PathBuf>,
    /// The daemon state directory (socket parent — the same base `tetond`
    /// resolves): model weights, cost ledger, config, selection record.
    pub state_dir: PathBuf,
    /// Recursive size of `state_dir`, for the confirmation line.
    pub state_bytes: u64,
    /// Does `state_dir` exist at all? Distinguishes "--keep-data kept it" from
    /// "there was nothing to delete" in the rendered plan.
    pub state_exists: bool,
    /// Delete `state_dir`? False under `--keep-data` or when it is absent.
    pub delete_state: bool,
    /// The `brew services` log directory, when it exists.
    pub log_dir: Option<PathBuf>,
    /// Remove the [`TAP`] registration? True only when the tap is actually
    /// registered (and brew exists) — an absent tap is not a step to report.
    pub untap: bool,
}

impl Plan {
    /// Resolve the plan from the daemon paths and the flags.
    pub fn build(
        paths: &DaemonPaths,
        keep_data: bool,
        brew_prefix: Option<PathBuf>,
        tap_registered: bool,
    ) -> Self {
        let state_dir = paths
            .socket
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir);
        let state_exists = state_dir.is_dir();
        let log_dir = brew_prefix
            .as_deref()
            .map(|prefix| prefix.join("var/log/teton"))
            .filter(|dir| dir.is_dir());
        Self {
            untap: brew_prefix.is_some() && tap_registered,
            brew_prefix,
            state_bytes: if state_exists {
                dir_size(&state_dir)
            } else {
                0
            },
            state_exists,
            delete_state: state_exists && !keep_data,
            state_dir,
            log_dir,
        }
    }
}

/// `teton uninstall`: show the plan, confirm, execute.
pub fn run(
    paths: &DaemonPaths,
    plan: &Plan,
    auto_accept: bool,
    surface: &mut dyn Surface,
    prompter: &mut dyn Prompter,
) -> anyhow::Result<()> {
    render_plan(plan, surface);
    if !auto_accept && !confirmed(prompter) {
        surface.line(
            LineKind::Notice,
            "uninstall cancelled — nothing was changed.",
        );
        return Ok(());
    }
    execute(paths, plan, surface)
}

/// Render every step the confirmed run will take (and the ones it cannot).
pub fn render_plan(plan: &Plan, surface: &mut dyn Surface) {
    surface.line(LineKind::Info, "teton uninstall will:");
    if plan.brew_prefix.is_some() {
        surface.line(
            LineKind::Info,
            "  - stop the background daemon (brew services stop teton)",
        );
    } else {
        surface.line(
            LineKind::Notice,
            "  - `brew` was not found on PATH: the service and the binaries must be removed by \
             whatever installed them; only local data will be deleted.",
        );
    }
    if plan.delete_state {
        surface.line(
            LineKind::Info,
            &format!(
                "  - delete {} ({} — includes the downloaded model, cost history, and config)",
                plan.state_dir.display(),
                format_bytes(plan.state_bytes),
            ),
        );
    } else if plan.state_exists {
        surface.line(
            LineKind::Notice,
            &format!(
                "  - keep {} ({}) — delete it yourself later if you change your mind",
                plan.state_dir.display(),
                format_bytes(plan.state_bytes),
            ),
        );
    } else {
        surface.line(
            LineKind::Notice,
            &format!(
                "  - nothing to delete at {} (no state directory)",
                plan.state_dir.display()
            ),
        );
    }
    if let Some(log_dir) = &plan.log_dir {
        surface.line(
            LineKind::Info,
            &format!("  - delete the daemon logs at {}", log_dir.display()),
        );
    }
    if plan.brew_prefix.is_some() {
        surface.line(
            LineKind::Info,
            "  - remove the teton and teton-code binaries (brew uninstall teton)",
        );
    }
    if plan.untap {
        surface.line(
            LineKind::Info,
            &format!("  - remove the Homebrew tap (brew untap {TAP})"),
        );
    }
    #[cfg(target_os = "macos")]
    surface.line(
        LineKind::Info,
        "  - remove provider API keys from the macOS keychain (service \"teton\")",
    );
}

/// Ask once; only an explicit yes proceeds. Empty answer and EOF are both no —
/// an irreversible deletion must not happen by pressing return.
fn confirmed(prompter: &mut dyn Prompter) -> bool {
    match prompter.ask("proceed? [y/N] ") {
        Some(answer) => matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"),
        None => false,
    }
}

/// Run the confirmed plan, stopping before any deletion if the daemon cannot
/// be brought down.
fn execute(paths: &DaemonPaths, plan: &Plan, surface: &mut dyn Surface) -> anyhow::Result<()> {
    if plan.brew_prefix.is_some() {
        stop_service(surface);
    }
    // The gate before any deletion: a daemon still holding the state directory
    // (started by hand rather than brew services, or a stop that raced) would
    // recreate files under a directory we are about to remove.
    ensure_daemon_down(paths)?;

    if plan.delete_state {
        std::fs::remove_dir_all(&plan.state_dir)
            .map_err(|e| anyhow!("could not delete {}: {e}", plan.state_dir.display()))?;
        surface.line(
            LineKind::Info,
            &format!(
                "deleted {} ({} freed)",
                plan.state_dir.display(),
                format_bytes(plan.state_bytes)
            ),
        );
    }

    if let Some(log_dir) = &plan.log_dir {
        match std::fs::remove_dir_all(log_dir) {
            Ok(()) => surface.line(LineKind::Info, &format!("deleted {}", log_dir.display())),
            Err(e) => surface.line(
                LineKind::Notice,
                &format!(
                    "could not delete {}: {e} — remove it yourself.",
                    log_dir.display()
                ),
            ),
        }
    }

    #[cfg(target_os = "macos")]
    delete_keychain_entries(surface);

    if plan.brew_prefix.is_some() {
        // Last, so every earlier failure still leaves a working `teton doctor`
        // to diagnose with. Deleting the binary out from under this running
        // process is fine on Unix — the inode lives until we exit.
        let status = Command::new("brew")
            .args(["uninstall", "teton"])
            .status()
            .map_err(|e| anyhow!("could not run `brew uninstall teton`: {e}"))?;
        if !status.success() {
            bail!("`brew uninstall teton` failed; the binaries are still installed.");
        }
        if plan.untap {
            untap(surface);
        }
        surface.line(LineKind::Notice, "done.");
    } else {
        surface.line(
            LineKind::Notice,
            "done. The teton and teton-code binaries were not brew-managed — remove them yourself.",
        );
    }
    Ok(())
}

/// `brew services stop teton`, tolerantly: a service that was never started (or
/// a Linux brew without services) reports an error, and that is fine — the real
/// gate is [`ensure_daemon_down`], not this exit code.
fn stop_service(surface: &mut dyn Surface) {
    match Command::new("brew")
        .args(["services", "stop", "teton"])
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(_) => surface.line(
            LineKind::Notice,
            "brew services stop reported an error (service not registered?) — continuing.",
        ),
        Err(e) => surface.line(
            LineKind::Notice,
            &format!("could not run brew services stop: {e} — continuing."),
        ),
    }
}

/// How long to wait for launchd to actually bring the daemon down after
/// `brew services stop` returns.
const STOP_POLL_ATTEMPTS: u32 = 10;
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Refuse to delete anything while the daemon still answers its socket.
fn ensure_daemon_down(paths: &DaemonPaths) -> anyhow::Result<()> {
    for _ in 0..STOP_POLL_ATTEMPTS {
        if Connection::connect(&paths.socket).is_err() {
            return Ok(());
        }
        thread::sleep(STOP_POLL_INTERVAL);
    }
    bail!(
        "the daemon is still running at {} — if it was started outside `brew services` \
         (a manual `teton-code`, a custom unit), stop that process and re-run \
         `teton uninstall`.",
        paths.socket.display()
    );
}

/// Delete every generic-password entry filed under the Teton keychain service
/// (BR-7 stored provider keys there; nothing else of ours lives in the
/// keychain). Deleting by service alone needs no account list, so this works
/// without reading the (possibly already deleted) config.
#[cfg(target_os = "macos")]
fn delete_keychain_entries(surface: &mut dyn Surface) {
    // `security delete-generic-password` removes one matching item per call and
    // fails when none match — loop until it does. Bounded far above any real
    // provider count so a surprising exit-0-forever can't spin.
    let mut deleted = 0u32;
    while deleted < 64 {
        let done = Command::new("security")
            .args(["delete-generic-password", "-s", crate::keychain::SERVICE])
            .output()
            .map(|out| !out.status.success())
            .unwrap_or(true);
        if done {
            break;
        }
        deleted += 1;
    }
    if deleted > 0 {
        surface.line(
            LineKind::Info,
            &format!("removed {deleted} provider key(s) from the macOS keychain"),
        );
    }
}

/// `brew untap`, tolerantly: brew itself refuses to untap while another
/// formula from the tap is still installed, and that refusal is correct — the
/// binaries are already gone by this point, so a kept tap is a notice with the
/// manual command, never a failure of the uninstall.
fn untap(surface: &mut dyn Surface) {
    match Command::new("brew").args(["untap", TAP]).status() {
        Ok(status) if status.success() => {
            surface.line(LineKind::Info, &format!("removed the {TAP} tap"));
        }
        Ok(_) => surface.line(
            LineKind::Notice,
            &format!(
                "brew untap refused (another formula from the tap still installed?) — \
                 run `brew untap {TAP}` yourself when you are done with it."
            ),
        ),
        Err(e) => surface.line(
            LineKind::Notice,
            &format!("could not run brew untap: {e} — run `brew untap {TAP}` yourself."),
        ),
    }
}

/// Is [`TAP`] registered with this brew? Asked per-tap (`brew tap-info`) rather
/// than by listing every tap: the exit code alone answers, no parsing.
pub fn tap_registered() -> bool {
    Command::new("brew")
        .args(["tap-info", TAP])
        .output()
        .map(|out| {
            out.status.success() && !String::from_utf8_lossy(&out.stdout).contains("Not installed")
        })
        .unwrap_or(false)
}

/// Homebrew's prefix, when `brew` is on `PATH` and answers.
pub fn brew_prefix() -> Option<PathBuf> {
    let out = Command::new("brew").arg("--prefix").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let prefix = text.trim();
    (!prefix.is_empty()).then(|| PathBuf::from(prefix))
}

/// Recursive size of a directory, tolerating unreadable entries (a partial
/// answer in the confirmation beats refusing to uninstall). Symlinks are
/// counted as themselves, never followed — the model dir contains only regular
/// files, and following a stray link out of the state dir would misreport.
fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    let mut total = 0;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            total += dir_size(&entry.path());
        } else {
            total += meta.len();
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::ScriptedPrompter;
    use crate::render::RecordingSurface;

    fn temp_state(tag: &str) -> DaemonPaths {
        let base =
            std::env::temp_dir().join(format!("teton-uninstall-test-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(base.join("models")).unwrap();
        std::fs::write(base.join("cost.db"), vec![0u8; 100]).unwrap();
        std::fs::write(base.join("models/weights.gguf"), vec![0u8; 2048]).unwrap();
        DaemonPaths {
            socket: base.join("tetond.sock"),
            lock: base.join("tetond.lock"),
            log: base.join("tetond.log"),
            projects: base.join("projects.json"),
        }
    }

    #[test]
    fn dir_size_sums_files_recursively() {
        let paths = temp_state("size");
        let base = paths.socket.parent().unwrap();
        assert_eq!(dir_size(base), 2148);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn plan_measures_state_and_honours_keep_data() {
        let paths = temp_state("plan");
        let plan = Plan::build(&paths, false, None, false);
        assert!(plan.delete_state);
        assert_eq!(plan.state_bytes, 2148);
        assert_eq!(plan.log_dir, None);

        let kept = Plan::build(&paths, true, None, false);
        assert!(!kept.delete_state, "--keep-data must not schedule a delete");
        std::fs::remove_dir_all(paths.socket.parent().unwrap()).unwrap();

        // An absent state dir schedules nothing regardless of the flag, and
        // the plan says "nothing to delete" rather than pretending to keep it.
        let absent = Plan::build(&paths, false, None, false);
        assert!(!absent.delete_state);
        assert!(!absent.state_exists);
        assert_eq!(absent.state_bytes, 0);
        let mut surface = RecordingSurface::new();
        render_plan(&absent, &mut surface);
        assert!(surface.any_line_contains(crate::render::LineKind::Notice, "nothing to delete"));
    }

    #[test]
    fn untap_needs_both_brew_and_a_registered_tap() {
        let paths = temp_state("untap");
        let brew = Some(PathBuf::from("/opt/homebrew"));
        assert!(Plan::build(&paths, false, brew.clone(), true).untap);
        assert!(!Plan::build(&paths, false, brew, false).untap);
        // A registered tap without brew cannot happen in practice, but the plan
        // must not schedule a brew command it cannot run.
        assert!(!Plan::build(&paths, false, None, true).untap);
        std::fs::remove_dir_all(paths.socket.parent().unwrap()).unwrap();
    }

    #[test]
    fn plan_names_the_size_and_every_step() {
        let paths = temp_state("render");
        let plan = Plan::build(&paths, false, Some(PathBuf::from("/opt/homebrew")), true);
        let mut surface = RecordingSurface::new();
        render_plan(&plan, &mut surface);
        // The irreversible step names its cost before anything is confirmed.
        assert!(surface.any_line_contains(crate::render::LineKind::Info, "2.1 KiB"));
        assert!(surface.any_line_contains(crate::render::LineKind::Info, "brew uninstall teton"));
        assert!(surface.any_line_contains(crate::render::LineKind::Info, "brew untap"));

        // No registered tap → no untap step in the plan.
        let no_tap = Plan::build(&paths, false, Some(PathBuf::from("/opt/homebrew")), false);
        let mut surface = RecordingSurface::new();
        render_plan(&no_tap, &mut surface);
        assert!(!surface.any_line_contains(crate::render::LineKind::Info, "brew untap"));
        std::fs::remove_dir_all(paths.socket.parent().unwrap()).unwrap();
    }

    #[test]
    fn without_brew_the_plan_says_so_instead_of_dropping_steps() {
        let paths = temp_state("nobrew");
        let plan = Plan::build(&paths, false, None, false);
        let mut surface = RecordingSurface::new();
        render_plan(&plan, &mut surface);
        assert!(surface.any_line_contains(crate::render::LineKind::Notice, "`brew` was not found"));
        std::fs::remove_dir_all(paths.socket.parent().unwrap()).unwrap();
    }

    #[test]
    fn only_an_explicit_yes_confirms() {
        for (answer, expected) in [("y", true), ("YES", true), ("", false), ("n", false)] {
            let mut p = ScriptedPrompter::new(&[answer]);
            assert_eq!(confirmed(&mut p), expected, "answer {answer:?}");
        }
        // EOF (Ctrl-D) is a cancel, not a confirmation.
        let mut eof = ScriptedPrompter::new(&[]);
        assert!(!confirmed(&mut eof));
    }

    #[test]
    fn declining_changes_nothing_and_says_so() {
        let paths = temp_state("decline");
        let plan = Plan::build(&paths, false, None, false);
        let mut surface = RecordingSurface::new();
        let mut prompter = ScriptedPrompter::new(&["n"]);
        run(&paths, &plan, false, &mut surface, &mut prompter).unwrap();
        assert!(surface.any_line_contains(crate::render::LineKind::Notice, "cancelled"));
        let base = paths.socket.parent().unwrap();
        assert!(base.join("models/weights.gguf").exists());
        std::fs::remove_dir_all(base).unwrap();
    }
}
