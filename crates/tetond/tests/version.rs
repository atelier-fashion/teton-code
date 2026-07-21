//! `tetond --version` / `-V` prints the daemon's version and exits cleanly —
//! without binding the socket or acquiring the single-instance lock.
//!
//! This is the CLI-surface coverage the acceptance matrix otherwise skipped: a
//! regression that broke `--version` handling (e.g. falling through to the daemon
//! startup path) would hang binding a socket instead of printing a line.

use std::process::Command;

#[test]
fn tetond_reports_its_version_on_both_flags() {
    for flag in ["--version", "-V"] {
        let output = Command::new(env!("CARGO_BIN_EXE_tetond"))
            .arg(flag)
            .output()
            .unwrap_or_else(|e| panic!("failed to run tetond {flag}: {e}"));

        assert!(
            output.status.success(),
            "tetond {flag} exited non-zero: {:?}",
            output.status
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("tetond"),
            "tetond {flag} should name the binary; stdout was: {stdout:?}"
        );
        // The version string is the crate version compiled into this test.
        assert!(
            stdout.contains(env!("CARGO_PKG_VERSION")),
            "tetond {flag} should print version {}; stdout was: {stdout:?}",
            env!("CARGO_PKG_VERSION")
        );
    }
}
