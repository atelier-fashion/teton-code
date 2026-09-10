//! Shared binary-resolution helpers for the `teton` crate's e2e suites.
//!
//! Both `cli_e2e` and `pty_e2e` spawn the real CLI against a real daemon, so
//! both need the same answer to "which binaries am I testing?". That answer is
//! not symmetric between the two, which is the whole point of this module.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

/// Path to the `teton` CLI under test.
///
/// Cargo guarantees this is rebuilt for every run, because `teton` is a binary
/// of *this* package and the variable is set by Cargo itself.
pub fn teton_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_teton"))
}

/// Path to the `teton-code` daemon, refusing to return a stale one.
///
/// The daemon cannot be reached the way `teton` is (BUG-164). Cargo sets
/// `CARGO_BIN_EXE_<name>` only for binaries belonging to the *same package* as
/// the integration test, and `teton-code` is a binary of `tetond`; referencing
/// `env!("CARGO_BIN_EXE_teton-code")` here is a compile error, not a fallback.
/// `teton` also declares no dependency on `tetond`, so nothing in the dependency
/// graph obliges Cargo to build the daemon for a `-p teton` run.
///
/// The path therefore has to be derived by joining onto `teton`'s directory, and
/// that yields a valid-looking path to whatever daemon was built last —
/// arbitrarily older than the change under test. The previous guard only checked
/// that the file *existed*, so a targeted run reported PASS against a daemon
/// that did not contain the change.
///
/// Existence and freshness are different properties. This checks the one that
/// matters: if the daemon is older than any source it is built from, the suite
/// refuses to run and says how to fix it, rather than passing against the wrong
/// binary. Under `cargo test --workspace` the daemon is always current and this
/// is a few `stat` calls.
///
/// Freshness is checked rather than *repaired* (by shelling out to `cargo build`)
/// deliberately: invoking Cargo from inside a Cargo-spawned test process
/// perturbed the pty suite in ways that were not worth carrying in a test
/// harness. Refusing is honest and has no such interaction.
pub fn daemon_bin() -> PathBuf {
    static DAEMON: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    match DAEMON.get_or_init(resolve_daemon) {
        Ok(path) => path.clone(),
        Err(why) => panic!("{why}"),
    }
}

fn resolve_daemon() -> Result<PathBuf, String> {
    let daemon = teton_bin()
        .parent()
        .ok_or("the `teton` binary has no parent directory")?
        .join("teton-code");

    let Ok(built) = mtime(&daemon) else {
        return Err(format!(
            "the `teton-code` daemon is not built at {}.\n\
             Run `cargo build --workspace` (or `cargo test --workspace`) first: a \
             `-p teton` run does not build the daemon, because `teton` declares no \
             dependency on `tetond` (BUG-164).",
            daemon.display()
        ));
    };

    if let Some((newest, source)) = newest_daemon_input() {
        if newest > built {
            return Err(format!(
                "the `teton-code` daemon at {} is older than {}.\n\
                 The suite refuses to run rather than report a pass against a stale \
                 daemon (BUG-164) — a `-p teton` run does not rebuild it.\n\
                 Run `cargo build --workspace` (or `cargo test --workspace`).",
                daemon.display(),
                source.display()
            ));
        }
    }

    Ok(daemon)
}

/// Newest mtime among the sources `teton-code` is built from, with the file it
/// came from (for the error message).
///
/// Scoped to the daemon's own inputs — the `tetond` crate and the library crates
/// it links — so editing only the CLI does not read as a stale daemon. A source
/// tree that cannot be walked yields `None`: this guard fails open on its own
/// bookkeeping, because refusing to run over an unreadable directory would be
/// worse than the staleness it protects against.
fn newest_daemon_input() -> Option<(SystemTime, PathBuf)> {
    const DAEMON_CRATES: [&str; 5] = [
        "tetond",
        "teton-core",
        "teton-protocol",
        "teton-providers",
        "teton-inference",
    ];

    let crates_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()?
        .to_path_buf();
    let workspace = crates_dir.parent()?.to_path_buf();

    let mut newest: Option<(SystemTime, PathBuf)> = None;
    let mut consider = |path: PathBuf| {
        if let Ok(t) = mtime(&path) {
            if newest.as_ref().is_none_or(|(best, _)| t > *best) {
                newest = Some((t, path));
            }
        }
    };

    consider(workspace.join("Cargo.lock"));
    consider(workspace.join("Cargo.toml"));
    for name in DAEMON_CRATES {
        let root = crates_dir.join(name);
        consider(root.join("Cargo.toml"));
        collect_rs(&root.join("src"), &mut consider);
    }

    newest
}

/// Recursively feed every `.rs` file under `dir` to `consider`.
fn collect_rs(dir: &Path, consider: &mut impl FnMut(PathBuf)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => collect_rs(&path, consider),
            Ok(t) if t.is_file() && path.extension().is_some_and(|e| e == "rs") => {
                consider(path);
            }
            _ => {}
        }
    }
}

fn mtime(path: &Path) -> std::io::Result<SystemTime> {
    std::fs::metadata(path)?.modified()
}

/// Every glyph REQ-621's activity row can open with: the ten spinner frames and
/// the stalled glyph (`activity.rs`'s `SPINNER` and `STALLED_GLYPH`).
///
/// Written out here rather than imported from `activity.rs`, because that is
/// where they are authored and an oracle reading them from their own definition
/// would agree with any value it was given (LESSON-569). Shared between the two
/// e2e suites because they need it for opposite claims — the pty legs find rows
/// by it, the piped legs assert its total absence (BR-6) — and one list is one
/// place for a frame the row can draw to be missing from.
///
/// Nothing else this binary prints draws braille, so a search for one of these
/// characters is a search for an activity row.
pub const ACTIVITY_GLYPHS: [char; 11] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏', '⠿'];

/// A pty transcript replayed through a terminal, so a claim about what is
/// **left on screen** is a claim about the screen and not about the stream
/// (REQ-621 AC-7).
///
/// A withdrawn row is still in the byte stream forever — `\x1b[1A\r\x1b[K` does
/// not delete the characters it scrolled past, it tells a terminal to draw over
/// them. So "the turn left no row behind" cannot be asserted by searching the
/// transcript: every frame the row ever painted is in there, and a search finds
/// the whole animation whether or not the last one was taken back. It has to be
/// asserted on the *result* of replaying those bytes, which is what this does.
///
/// Deliberately small. It answers the sequences the activity row and the entry
/// frame actually emit — save and restore (`\x1b[s` / `\x1b[u`), cursor up and
/// down, carriage return, newline, erase-in-line and erase-in-display — and
/// **skips** every other CSI, which for this suite means the SGR colour runs
/// that occupy no column. It is not a terminal emulator: there is no scroll
/// region, no wrap at the right margin, no tab stops, and no character sets. A
/// row wider than the pty would be hard-wrapped by a real terminal and is not
/// here, so a leg reading this screen keeps its content inside its window — the
/// same discipline the `display_rows` helpers in `pty_e2e` keep for the same
/// reason.
///
/// Rows are returned top to bottom with trailing blanks trimmed, so an
/// assertion can name what a reader would see.
pub fn rendered_screen(transcript: &str) -> Vec<String> {
    let mut screen = Screen::default();
    let mut chars = transcript.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => screen.col = 0,
            '\n' => {
                screen.row += 1;
                screen.col = 0;
            }
            '\x1b' if chars.peek() == Some(&'[') => {
                chars.next();
                let mut params = String::new();
                let mut final_byte = None;
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        final_byte = Some(c);
                        break;
                    }
                    params.push(c);
                }
                if let Some(final_byte) = final_byte {
                    screen.csi(final_byte, &params);
                }
            }
            // A two-character escape: its second character is consumed by the
            // `next` above, and neither moves the cursor.
            '\x1b' => {
                chars.next();
            }
            // Everything a reader would not see as a column of text. `\x07` is
            // the bell the prompter rings on a refused key.
            c if (c as u32) < 0x20 || c == '\x7f' => {}
            c => screen.put(c),
        }
    }
    screen.rows()
}

/// The screen [`rendered_screen`] paints onto: rows of characters and a cursor.
#[derive(Default)]
struct Screen {
    rows: Vec<Vec<char>>,
    row: usize,
    col: usize,
    saved: Option<(usize, usize)>,
}

impl Screen {
    /// Write one character at the cursor and step right, growing the screen to
    /// reach it. A write past the end of a row pads with spaces rather than
    /// wrapping — see [`rendered_screen`] on what this is not.
    fn put(&mut self, c: char) {
        while self.rows.len() <= self.row {
            self.rows.push(Vec::new());
        }
        let row = &mut self.rows[self.row];
        while row.len() <= self.col {
            row.push(' ');
        }
        row[self.col] = c;
        self.col += 1;
    }

    /// Apply one CSI sequence, ignoring the ones that paint no column.
    fn csi(&mut self, final_byte: char, params: &str) {
        // An omitted parameter is 1 for the cursor moves and 0 for the erases,
        // which is the ANSI default in both cases.
        let n = params.parse::<usize>().unwrap_or(0);
        match final_byte {
            'A' => self.row = self.row.saturating_sub(n.max(1)),
            'B' => self.row += n.max(1),
            'C' => self.col += n.max(1),
            'D' => self.col = self.col.saturating_sub(n.max(1)),
            's' => self.saved = Some((self.row, self.col)),
            'u' => {
                if let Some((row, col)) = self.saved {
                    self.row = row;
                    self.col = col;
                }
            }
            'K' => self.erase_line(n),
            'J' => self.erase_display(n),
            _ => {}
        }
    }

    /// `\x1b[K` (0, to the end of the row), `\x1b[1K` (to the start) or
    /// `\x1b[2K` (the whole row).
    fn erase_line(&mut self, mode: usize) {
        let Some(row) = self.rows.get_mut(self.row) else {
            return;
        };
        match mode {
            1 => {
                for c in row.iter_mut().take(self.col + 1) {
                    *c = ' ';
                }
            }
            2 => row.clear(),
            _ => row.truncate(self.col),
        }
    }

    /// `\x1b[J` (0, to the end of the screen) or `\x1b[2J` (all of it).
    fn erase_display(&mut self, mode: usize) {
        match mode {
            2 => self.rows.clear(),
            _ => {
                self.erase_line(0);
                self.rows.truncate(self.row + 1);
            }
        }
    }

    fn rows(&self) -> Vec<String> {
        let mut rows: Vec<String> = self
            .rows
            .iter()
            .map(|row| row.iter().collect::<String>().trim_end().to_owned())
            .collect();
        while rows.last().is_some_and(String::is_empty) {
            rows.pop();
        }
        rows
    }
}

/// **REQ-621 AC-7 — the cursor interpreter's own oracle.**
///
/// [`rendered_screen`] is the only thing standing between "the row was
/// withdrawn" and "the row is still on screen": every activity frame the pump
/// ever painted stays in the transcript, so the residue claims in `pty_e2e`
/// are claims about the *replay*, and an interpreter that quietly mishandled an
/// erase would green them all. It shipped with no tests of its own — the legs
/// that use it were its only exercise, and they exercise it in exactly the one
/// direction that cannot notice a bug (an interpreter that erases too much
/// reports "no residue" forever).
///
/// So these are literal oracles: every expectation is written out by hand from
/// the ANSI definition of the sequence, and none of them is computed by calling
/// anything in this module (LESSON-569). The cases are the sequences the row
/// and the entry frame actually emit, plus the two edges that would silently
/// change what a leg means.
///
/// # What breaks these tests
///
/// The mutation below was **applied, run, and observed failing**, not reasoned
/// about (AC-11, LESSON-441):
///
/// | Mutation | Fails |
/// |---|---|
/// | `erase_line`'s default arm becomes `row.clear()` — mode 0 handled as mode 2 | `an_erase_clears_the_span_its_mode_names`, `left: []` against `right: ["abc"]` (the other five stay green) |
///
/// That mutation is the shape the interpreter is most exposed to and the one
/// its users cannot see: `\x1b[K` is the erase in every sequence the row emits,
/// and an interpreter that cleared the whole row instead of the tail would
/// still report "nothing left on screen" for every leg in `pty_e2e` — while
/// hiding any row the client failed to take back that had been drawn to the
/// *left* of the cursor. The withdraw case below stays green under it (a
/// withdraw erases from column 0, where the two modes agree), which is exactly
/// why the erase case has to be here as well.
#[cfg(test)]
mod tests {
    use super::rendered_screen;

    /// Nothing at all, so the "gone" expectations below are comparable.
    fn blank() -> Vec<String> {
        Vec::new()
    }

    /// `\x1b[1A\r\x1b[K` — the sequence `withdraw_row_above(1)` writes, and the
    /// one every AC-7 leg's claim rests on.
    ///
    /// Both directions, because only the pair says the interpreter is reading
    /// the escape rather than dropping the row for some other reason.
    #[test]
    fn the_withdraw_sequence_takes_the_row_it_drew_off_the_screen() {
        assert_eq!(rendered_screen("row\n"), vec!["row".to_owned()]);
        assert_eq!(rendered_screen("row\n\x1b[1A\r\x1b[K"), blank());
    }

    /// `\x1b[s` … `\x1b[u` — a repaint: save, step up, erase, redraw, restore.
    ///
    /// The claim is that the new frame replaces the old one **in place** and
    /// the cursor comes back to where it was, so whatever prints next lands
    /// below rather than on top of the row (BR-5). The trailing `next` is what
    /// makes the second half observable.
    #[test]
    fn a_repaint_replaces_the_row_above_and_restores_the_cursor() {
        assert_eq!(
            rendered_screen("row1\n\x1b[s\x1b[1A\r\x1b[Krow2\x1b[u"),
            vec!["row2".to_owned()]
        );
        assert_eq!(
            rendered_screen("row1\n\x1b[s\x1b[1A\r\x1b[Krow2\x1b[unext"),
            vec!["row2".to_owned(), "next".to_owned()]
        );
    }

    /// The three erase-in-line modes, each on the same row with the cursor at
    /// the same column, so the only variable is the mode.
    ///
    /// `\x1b[K` (0) clears from the cursor to the end, `\x1b[1K` clears from
    /// the start **through** the cursor cell, `\x1b[2K` clears the row. The
    /// mode-2 case carries a second row so its expectation is a cleared row
    /// rather than an empty screen.
    #[test]
    fn an_erase_clears_the_span_its_mode_names() {
        assert_eq!(
            rendered_screen("abcdef\r\x1b[3C\x1b[K"),
            vec!["abc".to_owned()]
        );
        assert_eq!(
            rendered_screen("abcdef\r\x1b[3C\x1b[1K"),
            vec!["    ef".to_owned()]
        );
        assert_eq!(rendered_screen("abcdef\r\x1b[3C\x1b[2K"), blank());
        assert_eq!(
            rendered_screen("abcdef\nkept\x1b[1A\r\x1b[2K"),
            vec![String::new(), "kept".to_owned()]
        );
    }

    /// A cursor-up past the top of the screen stops at row 0.
    ///
    /// The pump measures its row with `\x1b[1A` from wherever the cursor is,
    /// and a session's first turn can leave it on the first row — so this is a
    /// real state and not a hypothetical. An interpreter that underflowed here
    /// would panic in debug and wrap in release, which would make an AC-7 leg
    /// fail on the harness instead of on the client.
    #[test]
    fn a_cursor_up_saturates_at_the_top_row() {
        assert_eq!(rendered_screen("\x1b[5Atop"), vec!["top".to_owned()]);
        assert_eq!(
            rendered_screen("only\n\x1b[9A\rover"),
            vec!["over".to_owned()]
        );
    }

    /// A transcript with no cursor motion in it comes back as itself.
    ///
    /// The base case, and the one that says the interpreter is not the reason a
    /// leg sees fewer rows than the session printed: durable lines print in
    /// their usual place and the row moves beneath them (BR-5), so every AC-7
    /// leg reads a screen that is mostly plain text.
    #[test]
    fn a_plain_transcript_replays_unchanged() {
        assert_eq!(
            rendered_screen("alpha\nbeta\ngamma\n"),
            vec!["alpha".to_owned(), "beta".to_owned(), "gamma".to_owned()]
        );
    }

    /// The colour runs and the bell occupy no column.
    ///
    /// Both are on the row's own bytes — a `line()` draw closes with
    /// `\x1b[0m` and the prompter rings the bell on a refused key — so an
    /// interpreter that gave either a column would shift every assertion about
    /// what a reader sees.
    #[test]
    fn styling_and_control_bytes_take_up_no_columns() {
        assert_eq!(
            rendered_screen("\x1b[1mbold\x1b[0m"),
            vec!["bold".to_owned()]
        );
        assert_eq!(rendered_screen("a\x07b"), vec!["ab".to_owned()]);
    }
}
