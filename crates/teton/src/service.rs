//! First-run launchd registration — the install-side mirror of `teton uninstall`.
//!
//! The CLI has always autostarted `teton-code` directly when no daemon answers
//! ([`crate::client::ensure_connected`]), but a daemon spawned that way is not
//! launchd-managed: it dies with the machine and nothing restarts it. The
//! managed path is `brew services start teton` — a second command the README
//! used to hand every new user. This module absorbs it into the product: when
//! an interactive session finds no daemon and the running binary is
//! brew-managed, it offers — once, consent-first — to register the service
//! instead of silently spawning an unmanaged daemon.
//!
//! Consent shape: the offer defaults to **yes** (benign and reversible, the
//! same default as the first-run model proposal — unlike `teton uninstall`,
//! where an irreversible deletion defaults to no). An explicit "n" is recorded
//! in the daemon state directory and never asked again; EOF or a non-terminal
//! stdin skips the offer without recording anything, so scripted and piped
//! runs (the e2e suites, shell composition) never see a prompt and never
//! consume a line of stdin that was meant for the session.

use std::path::{Path, PathBuf};
use std::process::Command;

use teton_protocol::socket_path::DaemonPaths;

use crate::prompt::Prompter;
use crate::render::{LineKind, Surface};

/// What the user said to the registration offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// Register the service (the default: return alone accepts).
    Yes,
    /// Explicit decline — record it and never ask again.
    No,
    /// EOF / cancel — skip this time, record nothing.
    Cancel,
}

/// Offer to register the brew service, and run `brew services start teton` on
/// acceptance. Returns `true` when the service was started and the caller
/// should poll for the daemon's socket; `false` means "use the direct-spawn
/// path" (declined, not applicable, or the start failed — each already
/// reported to `surface`).
pub fn offer_registration(
    paths: &DaemonPaths,
    surface: &mut dyn Surface,
    prompter: &mut dyn Prompter,
) -> bool {
    // brew services on Linux is systemd and explicitly not a v1 claim
    // (README: "run `teton-code` yourself, or write your own systemd unit").
    if !cfg!(target_os = "macos") {
        return false;
    }
    // A prompt nobody is at a terminal to answer is noise at best and a
    // swallowed line of piped stdin at worst.
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return false;
    }
    let Some(state_dir) = paths.socket.parent().map(Path::to_path_buf) else {
        return false;
    };
    if decline_marker(&state_dir).exists() {
        return false;
    }
    let brew_managed = std::env::current_exe()
        .and_then(|exe| exe.canonicalize())
        .is_ok_and(|exe| is_brew_managed_path(&exe));
    if !brew_managed {
        return false;
    }
    // brew believing the service is already running while the socket is dead is
    // a diagnosis for `teton doctor`, not something a start can fix — and with
    // the service genuinely up, ensure_connected never reaches this offer.
    if brew_reports_service_running() {
        return false;
    }

    // REQ-565 BR-5: the default lifetime is on demand, so this offer is the
    // ALWAYS-ON opt-in and reads like one. It used to default to yes and lead
    // with "survives reboots", which made the always-on daemon the path of
    // least resistance — the standing-memory arrangement this REQ exists to
    // stop shipping. The cost is now stated, and pressing return declines.
    surface.line(
        LineKind::Notice,
        "no daemon is running — starting one for this session. Teton can instead keep a daemon \
         running permanently (brew services), which is faster to start but holds the local \
         model in memory continuously.",
    );
    match answer_from(prompter.ask("keep a daemon running permanently? [y/N] ")) {
        Answer::Yes => {}
        Answer::No => {
            record_decline(&state_dir, surface);
            surface.line(
                LineKind::Notice,
                "okay — Teton will not ask again, and will start a daemon whenever you need one. \
                 Change your mind any time with `brew services start teton`.",
            );
            return false;
        }
        Answer::Cancel => return false,
    }

    match Command::new("brew")
        .args(["services", "start", "teton"])
        .status()
    {
        Ok(status) if status.success() => true,
        // The commonest failure is Homebrew 6's tap-trust gate on the short
        // name (README §Install) — name the one-time fix instead of leaving a
        // bare exit code.
        Ok(_) => {
            surface.line(
                LineKind::Notice,
                "brew services start failed — if it printed a tap-trust error, run `brew trust \
                 atelier-fashion/tap` once and retry. Starting a daemon directly for this session.",
            );
            false
        }
        Err(e) => {
            surface.line(
                LineKind::Notice,
                &format!(
                    "could not run brew services start: {e} — starting a daemon directly for \
                     this session."
                ),
            );
            false
        }
    }
}

/// Maps the raw prompt answer onto the offer's semantics: only an explicit yes
/// accepts, return or an explicit no declines, EOF cancels.
///
/// The empty answer moved from `Yes` to `No` in REQ-565. Registering the
/// service is now the always-on opt-in rather than the recommended path, and an
/// opt-in that happens when you press return is not one — it would make the
/// standing-resident daemon the default again by way of the prompt.
fn answer_from(raw: Option<String>) -> Answer {
    match raw {
        None => Answer::Cancel,
        Some(text) => match text.trim().to_lowercase().as_str() {
            "y" | "yes" => Answer::Yes,
            "" | "n" | "no" => Answer::No,
            // Anything else is not consent to change service state; treat it
            // as a decline for this run but do not record it as permanent.
            _ => Answer::Cancel,
        },
    }
}

/// Is this executable installed by Homebrew? A brew-managed `teton` lives (or
/// resolves, via the `opt` symlink the caller canonicalizes away) under
/// `<prefix>/Cellar/teton/<version>/…` — that `Cellar/teton` spine is stable
/// across prefixes (`/opt/homebrew`, `/usr/local`, Linuxbrew) while a dev
/// build under `target/` never has it.
fn is_brew_managed_path(exe: &Path) -> bool {
    let mut components = exe.components().map(|c| c.as_os_str());
    while let Some(part) = components.next() {
        if part == "Cellar" {
            return components.next() == Some("teton".as_ref());
        }
    }
    false
}

/// The recorded "asked and declined" marker. It lives in the daemon state
/// directory beside the other recorded decisions (`model-selection.toml`):
/// a preference about this machine's daemon, kept where the daemon's state
/// lives — and swept away with it by `teton uninstall`.
fn decline_marker(state_dir: &Path) -> PathBuf {
    state_dir.join("service-declined")
}

/// Record the explicit decline. Failure to record is reported, not fatal —
/// the cost is being asked again next time, not a broken session.
fn record_decline(state_dir: &Path, surface: &mut dyn Surface) {
    let path = decline_marker(state_dir);
    let write = std::fs::create_dir_all(state_dir).and_then(|()| {
        std::fs::write(
            &path,
            "The user declined launchd registration for teton-code; `teton` will not offer \
             again while this file exists.\n",
        )
    });
    if let Err(e) = write {
        surface.line(
            LineKind::Notice,
            &format!("could not record the decline at {}: {e}", path.display()),
        );
    }
}

/// Does brew itself report the service as running? Parsed defensively from
/// `brew services info teton --json`; any failure to run or parse is `false`
/// (the offer's other gates carry the decision).
pub(crate) fn brew_reports_service_running() -> bool {
    let Ok(out) = Command::new("brew")
        .args(["services", "info", "teton", "--json"])
        .output()
    else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    service_json_reports_running(&String::from_utf8_lossy(&out.stdout))
}

/// The pure half of [`brew_reports_service_running`]: does this
/// `brew services info --json` payload say the service is loaded and running?
fn service_json_reports_running(json: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return false;
    };
    // `brew services info NAME --json` prints a one-element array.
    let entry = match &value {
        serde_json::Value::Array(items) => items.first(),
        _ => Some(&value),
    };
    entry.is_some_and(|e| {
        e.get("running").and_then(serde_json::Value::as_bool) == Some(true)
            || e.get("status").and_then(serde_json::Value::as_str) == Some("started")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brew_cellar_paths_are_managed_and_dev_builds_are_not() {
        for managed in [
            "/opt/homebrew/Cellar/teton/0.1.3/bin/teton",
            "/usr/local/Cellar/teton/0.1.3/bin/teton",
            "/home/linuxbrew/.linuxbrew/Cellar/teton/0.1.3/bin/teton",
        ] {
            assert!(is_brew_managed_path(Path::new(managed)), "{managed}");
        }
        for unmanaged in [
            "/Users/x/repo/target/debug/teton",
            "/usr/local/bin/teton",
            // Another formula's Cellar path must not count as ours.
            "/opt/homebrew/Cellar/other/1.0/bin/teton",
        ] {
            assert!(!is_brew_managed_path(Path::new(unmanaged)), "{unmanaged}");
        }
    }

    /// REQ-565 BR-5: only an explicit yes registers the always-on service.
    ///
    /// The empty answer moved from `Yes` to `No` with the on-demand default.
    /// An always-on daemon that you get by pressing return is not an opt-in —
    /// it would restore the standing-resident arrangement through the prompt,
    /// while the formula was busy removing it.
    #[test]
    fn only_an_explicit_yes_opts_into_the_always_on_daemon() {
        assert_eq!(
            answer_from(Some(String::new())),
            Answer::No,
            "pressing return must decline the opt-in, not accept it"
        );
        assert_eq!(answer_from(Some("y".into())), Answer::Yes);
        assert_eq!(answer_from(Some("YES".into())), Answer::Yes);
        assert_eq!(answer_from(Some("n".into())), Answer::No);
        assert_eq!(answer_from(Some("No".into())), Answer::No);
        // Garbage is not consent — and not a permanent decline either.
        assert_eq!(answer_from(Some("wat".into())), Answer::Cancel);
        assert_eq!(answer_from(None), Answer::Cancel);
    }

    #[test]
    fn service_json_running_states_parse_defensively() {
        assert!(service_json_reports_running(
            r#"[{"name":"teton","running":true,"status":"started"}]"#
        ));
        assert!(service_json_reports_running(
            r#"[{"name":"teton","running":false,"status":"started"}]"#
        ));
        for not_running in [
            r#"[{"name":"teton","running":false,"status":"none"}]"#,
            r#"[{"name":"teton","status":"stopped"}]"#,
            r"[]",
            "not json at all",
            "",
        ] {
            assert!(!service_json_reports_running(not_running), "{not_running}");
        }
    }

    #[test]
    fn a_recorded_decline_is_found_where_uninstall_sweeps() {
        let dir = std::env::temp_dir().join(format!("teton-service-test-{}", std::process::id()));
        let mut surface = crate::render::RecordingSurface::new();
        record_decline(&dir, &mut surface);
        assert!(decline_marker(&dir).exists());
        // Recording is quiet on success.
        assert!(surface.calls.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
