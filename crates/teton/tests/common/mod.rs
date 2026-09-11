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

/// Send `sig` to `pid`, reporting whether the kernel accepted it (REQ-622
/// AC-5).
///
/// BR-7 names three signals and two of them cannot be typed: `SIGTERM` is what
/// a wrapper or an init system sends, and a pty leg has no way to produce one
/// except by sending it.
///
/// `pid` is `kill(2)`'s parameter with `kill(2)`'s meaning, negatives included:
/// a pty leg's client runs inside a shell's process group (see `pty_e2e`'s
/// session helper), so `-pgid` is how a signal reaches *the client* rather than
/// the shell that is holding the terminal open around it — which is also what a
/// wrapper terminating a foreground job actually does.
///
/// The report is returned rather than asserted here: a leg that signalled a
/// process which had already exited is asking a different question from a leg
/// whose signal was refused, and only the leg knows which of those it meant.
pub fn send_signal(pid: libc::pid_t, sig: libc::c_int) -> bool {
    // SAFETY: `kill` takes two scalars, touches no memory of ours, and reports
    // failure through its return code, which is what this returns.
    unsafe { libc::kill(pid, sig) == 0 }
}

/// The two terminal flags REQ-622 changes, and the only two a leg may compare.
///
/// **Not the whole flag word, and not the whole `stty -a` line.** macOS sets the
/// driver-owned `PENDIN` bit in `c_lflag` when a terminal leaves non-canonical
/// mode — it means "input is queued for reprocessing", which is exactly what
/// leaving raw mode arranges — so a session that restored its saved settings
/// perfectly still reads back a `c_lflag` one bit different from the one it
/// saved, and an `stty -a` line one token different (`-pendin` becomes
/// `pendin`). A leg comparing either would be red on macOS for a fact about the
/// driver rather than about the client.
///
/// `ICANON` and `ECHO` are what [`crate::prompt::RawMode`] actually clears
/// (ADR-622-1 changes those two and `VMIN`/`VTIME`), so they are what BR-7's
/// "put the terminal back exactly as it found it" is observable as from
/// outside. The `VMIN`/`VTIME` half is pinned where it is authored, in
/// `prompt.rs`'s own unit tests, because `stty -a` reports it as `min`/`time`
/// inside the control-character block and a third field here would be a third
/// thing to get wrong for no third claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalFlags {
    /// Canonical mode: the kernel assembles lines and no read returns before a
    /// newline.
    pub icanon: bool,
    /// Echo: the kernel paints every keystroke at the cursor.
    pub echo: bool,
}

/// Open a pty's slave side by name, without making it this process's
/// controlling terminal (REQ-622 AC-4).
///
/// Two callers, one reason each, and both are about a descriptor's *lifetime*
/// rather than about reading anything through it.
///
/// [`terminal_flags_on`] needs a fresh one per readback: a `portable_pty` slave
/// handle stops being a terminal the moment anything has been spawned on it
/// (the spawn's `pre_exec` closes every descriptor above 2, and the handle the
/// parent still holds is then a number pointing at whatever was opened next),
/// so the readback has to reach the device rather than reuse the handle.
///
/// A pty leg also has to hold **one** of these open for the length of a
/// session, because a pty whose last slave descriptor closes is a pty whose
/// line discipline the kernel is free to reset — and AC-4's whole claim is
/// about the settings that are still there after the client has exited.
///
/// `O_NOCTTY` because a `cargo test` process that happened to be a session
/// leader without a controlling terminal would otherwise acquire this pty as
/// one, which is a side effect a test harness has no business having.
pub fn open_tty(tty: &Path) -> std::fs::File {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(tty)
        .unwrap_or_else(|e| panic!("open the session's own pty at {}: {e}", tty.display()))
}

/// Read the terminal settings of the pty at `tty` back by running `stty -a` on
/// it (REQ-622 AC-4).
///
/// **A child process on the same pty, deliberately.** The claim AC-4 makes is
/// about the state a user's terminal is left in, and the honest instrument for
/// it is the one a user would reach for: another program, attached to the same
/// terminal, asked what the settings are. Reading `tcgetattr` on the master
/// descriptor from inside the test process would be asking the same kernel
/// object through a different door — true, but it would keep passing if the
/// client had restored a *copy* rather than the terminal, and it is not what
/// the AC says.
///
/// The child is a plain [`std::process::Command`] with the device as its stdin
/// rather than a `portable_pty` spawn, and that is not a shortcut. A
/// `portable_pty` spawn sets the pty as the child's **controlling terminal**,
/// which — run during a session, which is exactly when the interesting reading
/// is taken — is a bid to take the terminal away from the client under test.
/// This one attaches a descriptor and nothing else: no `setsid`, no
/// `TIOCSCTTY`, no signal-disposition changes, and no bytes written into the
/// transcript every other assertion in the file reads, because the report comes
/// back through a pipe instead of through the pty.
pub fn terminal_flags_on(tty: &Path) -> TerminalFlags {
    let handle = open_tty(tty);
    let out = std::process::Command::new("stty")
        .arg("-a")
        .stdin(std::process::Stdio::from(handle))
        .output()
        .expect("run `stty -a` against the session's own pty");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    assert!(
        out.status.success(),
        "`stty -a` on {} failed ({:?}); it said:\n{text}",
        tty.display(),
        out.status
    );
    parse_terminal_flags(&text)
}

/// The `icanon` and `echo` states in one `stty -a` report.
///
/// **Token equality, never a substring.** `echo` is a prefix of six other flag
/// names a terminal reports — `echoe`, `echok`, `echoke`, `echonl`, `echoctl`,
/// `echoprt` — so a `contains("echo")` is true of every terminal ever made, and
/// a `contains(" -echo")` is true of one that has `-echoprt` set and `echo`
/// **on**. That is the failure mode this parser exists to not have: it would
/// report "echo is off" for a perfectly restored terminal and pass AC-4 only
/// when AC-4 was violated.
///
/// The two layouts are different enough that the parser has to be layout-blind:
/// BSD `stty` groups the flags into `lflags:`/`iflags:` sections and separates
/// the control characters with `;`, GNU `stty` prints one long unlabelled run
/// with `;` after each control character. Splitting on whitespace **and** `;`
/// and comparing whole tokens reads both without knowing which it has.
///
/// Panics when either flag is missing rather than defaulting. A parser that
/// quietly returned `false` for an absent token would make two runs whose
/// `stty` never ran at all compare equal — the vacuous pass a normalizing
/// helper invites (`mask_session_id`'s reason, one file over).
pub fn parse_terminal_flags(stty: &str) -> TerminalFlags {
    let mut icanon = None;
    let mut echo = None;
    for token in stty.split(|c: char| c.is_whitespace() || c == ';') {
        match token {
            "icanon" => icanon = Some(true),
            "-icanon" => icanon = Some(false),
            "echo" => echo = Some(true),
            "-echo" => echo = Some(false),
            _ => {}
        }
    }
    let (Some(icanon), Some(echo)) = (icanon, echo) else {
        panic!(
            "an `stty -a` report names both `icanon` and `echo`, in one polarity \
             or the other; this one named icanon={icanon:?} echo={echo:?} in:\n{stty}"
        );
    };
    TerminalFlags { icanon, echo }
}

// ---------------------------------------------------------------------------
// The keys a pty leg types that are not characters (REQ-622 AC-9, AC-13)
// ---------------------------------------------------------------------------
//
// Written as `&str` rather than `&[u8]` because every one of them is valid
// UTF-8 and the pty writers in both suites take text — a control byte below
// 0x80 is its own encoding, and an escape sequence is `ESC` followed by ASCII.
// Named here rather than inline at each leg so that "an arrow key" is one
// sequence in one place: a leg that hand-wrote `\x1b[A` and meant `\x1bOA`
// would be testing a key the decoder handles through a different arm.

/// Ctrl-C — `VINTR`, which the terminal turns into a `SIGINT` because
/// [`crate::prompt::RawMode`] leaves `ISIG` alone (ADR-622-1, BR-8).
pub const CTRL_C: &str = "\u{3}";
/// Ctrl-D — `VEOF`, inert during a turn and end-of-session at a prompt (BR-15).
pub const CTRL_D: &str = "\u{4}";
/// The Backspace key, as every terminal this ships to sends it (`DEL`).
pub const BACKSPACE: &str = "\u{7f}";
/// The four arrow keys in their `ESC [` (CSI) form — the cursor keys a terminal
/// sends in its normal mode, consumed as a unit and dropped (BR-9).
pub const ARROW_UP: &str = "\u{1b}[A";
pub const ARROW_DOWN: &str = "\u{1b}[B";
pub const ARROW_RIGHT: &str = "\u{1b}[C";
pub const ARROW_LEFT: &str = "\u{1b}[D";
/// `F1`, in its `ESC O` (SS3) form — the other escape shape the decoder knows,
/// and the reason a leg types a function key as well as an arrow.
pub const F1: &str = "\u{1b}OP";

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
    use super::{parse_terminal_flags, rendered_screen, TerminalFlags};

    /// `stty -a` on a fresh macOS pty in **canonical** mode, captured from a
    /// real one (Darwin 25.6) and pasted here verbatim.
    ///
    /// The whole report and not the `lflags:` line alone, because the parser's
    /// job is to be layout-blind: the control-character block below carries
    /// `min` and `time` separated by `;`, and a parser that split on whitespace
    /// only would read `^C;` as a token.
    const MACOS_CANONICAL: &str = "speed 9600 baud; 0 rows; 0 columns;\n\
        lflags: icanon isig iexten echo echoe -echok echoke -echonl echoctl\n\
        \t-echoprt -altwerase -noflsh -tostop -flusho -pendin -nokerninfo\n\
        \t-extproc\n\
        iflags: -istrip icrnl -inlcr -igncr ixon -ixoff ixany imaxbel -iutf8\n\
        \t-ignbrk brkint -inpck -ignpar -parmrk\n\
        oflags: opost onlcr -oxtabs -onocr -onlret\n\
        cflags: cread cs8 -parenb -parodd hupcl -clocal -cstopb -crtscts -dsrflow\n\
        \t-dtrflow -mdmbuf\n\
        cchars: discard = ^O; dsusp = ^Y; eof = ^D; eol = <undef>;\n\
        \teol2 = <undef>; erase = ^?; intr = ^C; kill = ^U; lnext = ^V;\n\
        \tmin = 1; quit = ^\\; reprint = ^R; start = ^Q; status = ^T;\n\
        \tstop = ^S; susp = ^Z; time = 0; werase = ^W;\n";

    /// The same terminal with [`crate::prompt::RawMode`] engaged: `-icanon`,
    /// `-echo`, `min = 0`, `time = 0` — **and `pendin` set**, which is the whole
    /// reason [`TerminalFlags`] is two fields rather than a flag word.
    ///
    /// `pendin` is the driver's, not ours: it says input is queued for
    /// reprocessing, which is what leaving canonical mode arranges. A leg that
    /// compared `c_lflag`, or the `lflags:` line, would read this bit as a
    /// client that failed to restore.
    const MACOS_RAW: &str = "speed 9600 baud; 0 rows; 0 columns;\n\
        lflags: -icanon isig iexten -echo echoe -echok echoke -echonl echoctl\n\
        \t-echoprt -altwerase -noflsh -tostop -flusho pendin -nokerninfo\n\
        \t-extproc\n\
        iflags: -istrip icrnl -inlcr -igncr ixon -ixoff ixany imaxbel -iutf8\n\
        \t-ignbrk brkint -inpck -ignpar -parmrk\n\
        oflags: opost onlcr -oxtabs -onocr -onlret\n\
        cflags: cread cs8 -parenb -parodd hupcl -clocal -cstopb -crtscts -dsrflow\n\
        \t-dtrflow -mdmbuf\n\
        cchars: discard = ^O; dsusp = ^Y; eof = ^D; eol = <undef>;\n\
        \teol2 = <undef>; erase = ^?; intr = ^C; kill = ^U; lnext = ^V;\n\
        \tmin = 0; quit = ^\\; reprint = ^R; start = ^Q; status = ^T;\n\
        \tstop = ^S; susp = ^Z; time = 0; werase = ^W;\n";

    /// GNU `stty -a`, whose layout shares nothing with the BSD one above: no
    /// section labels, the control characters first, and the flag names in one
    /// unlabelled run.
    ///
    /// Written out from coreutils' own output shape rather than captured on this
    /// machine, because this machine is a Mac — and the CI runner that is not is
    /// exactly the reader this case exists for.
    const LINUX_CANONICAL: &str = "speed 38400 baud; rows 40; columns 100; line = 0;\n\
        intr = ^C; quit = ^\\; erase = ^?; kill = ^U; eof = ^D; eol = <undef>;\n\
        eol2 = <undef>; swtch = <undef>; start = ^Q; stop = ^S; susp = ^Z; rprnt = ^R;\n\
        werase = ^W; lnext = ^V; discard = ^O; min = 1; time = 0;\n\
        -parenb -parodd -cmspar cs8 -hupcl -cstopb cread -clocal -crtscts\n\
        -ignbrk -brkint -ignpar -parmrk -inpck -istrip -inlcr -igncr icrnl ixon -ixoff\n\
        -iuclc -ixany imaxbel iutf8\n\
        opost -olcuc -ocrnl onlcr -onocr -onlret -ofill -ofdel nl0 cr0 tab0 bs0 vt0 ff0\n\
        isig icanon iexten echo echoe echok -echonl -noflsh -xcase -tostop -echoprt\n\
        echoctl echoke -flusho -extproc\n";

    /// The same GNU report with the mode change applied.
    const LINUX_RAW: &str = "speed 38400 baud; rows 40; columns 100; line = 0;\n\
        intr = ^C; quit = ^\\; erase = ^?; kill = ^U; eof = ^D; eol = <undef>;\n\
        eol2 = <undef>; swtch = <undef>; start = ^Q; stop = ^S; susp = ^Z; rprnt = ^R;\n\
        werase = ^W; lnext = ^V; discard = ^O; min = 0; time = 0;\n\
        -parenb -parodd -cmspar cs8 -hupcl -cstopb cread -clocal -crtscts\n\
        -ignbrk -brkint -ignpar -parmrk -inpck -istrip -inlcr -igncr icrnl ixon -ixoff\n\
        -iuclc -ixany imaxbel iutf8\n\
        opost -olcuc -ocrnl onlcr -onocr -onlret -ofill -ofdel nl0 cr0 tab0 bs0 vt0 ff0\n\
        isig -icanon iexten -echo echoe echok -echonl -noflsh -xcase -tostop -echoprt\n\
        echoctl echoke -flusho -extproc\n";

    const CANONICAL: TerminalFlags = TerminalFlags {
        icanon: true,
        echo: true,
    };
    const RAW: TerminalFlags = TerminalFlags {
        icanon: false,
        echo: false,
    };

    /// **REQ-622 AC-4 — the readback parser, against real `stty -a` layouts.**
    ///
    /// Both platforms, both modes, as literal oracles: the expectations are
    /// written out by hand from the reports above and none of them is computed
    /// by calling anything in this module (LESSON-569). This is the instrument
    /// AC-4 and AC-5 rest on — a parser that answered "canonical, echoing" for
    /// every input would green every restore leg in the suite while the client
    /// left terminals raw — so it is tested against the polarity it has to be
    /// able to *report*, in both directions, on both layouts.
    #[test]
    fn the_stty_parser_reads_both_layouts_in_both_modes() {
        assert_eq!(parse_terminal_flags(MACOS_CANONICAL), CANONICAL);
        assert_eq!(parse_terminal_flags(MACOS_RAW), RAW);
        assert_eq!(parse_terminal_flags(LINUX_CANONICAL), CANONICAL);
        assert_eq!(parse_terminal_flags(LINUX_RAW), RAW);
        assert_ne!(
            parse_terminal_flags(MACOS_CANONICAL),
            parse_terminal_flags(MACOS_RAW),
            "a parser that cannot tell the two modes apart cannot fail AC-4"
        );
    }

    /// `pendin` moves between the two reports and the comparison does not
    /// notice — which is the fact the whole helper is shaped around.
    ///
    /// macOS sets that bit when a terminal leaves non-canonical mode, so a
    /// client that restored its saved `termios` byte for byte still reads back
    /// a `c_lflag` one bit different from the one it saved. The pair below is
    /// the same terminal before the raw window and after it, differing in
    /// `pendin` (and in `min`, which the driver also carries) — and AC-4's
    /// "flags equal" has to be true across it.
    #[test]
    fn the_driver_owned_pendin_bit_is_not_part_of_the_comparison() {
        let restored = MACOS_CANONICAL.replace("-pendin", "pendin");
        assert!(
            restored != MACOS_CANONICAL,
            "the substitution must actually change the report, or this case \
             compares a string with itself"
        );
        assert_eq!(
            parse_terminal_flags(&restored),
            parse_terminal_flags(MACOS_CANONICAL),
            "REQ-622 AC-4 compares `icanon` and `echo`, never the flag word: a \
             terminal restored on macOS comes back with `pendin` set by the \
             driver, and a leg that compared everything would be red for a fact \
             about the kernel"
        );
    }

    /// `echo` is a prefix of six other flag names, and the parser must read the
    /// token rather than the substring.
    ///
    /// The trap in both directions, because both are reachable in one real
    /// report: a terminal with `echo` **on** carries `-echoprt` and `-echonl`
    /// (so a `contains(" -echo")` says echo is off), and a terminal with `echo`
    /// **off** carries `echoe`, `echoctl` and `echoke` (so a
    /// `contains(" echo")` says it is on). Every one of those tokens is in the
    /// two reports below, arranged so that a substring parser gets the answer
    /// exactly backwards.
    #[test]
    fn the_echo_prefixes_do_not_decide_the_echo_flag() {
        let on = "lflags: icanon isig echo -echoe -echok -echoke -echonl -echoctl -echoprt";
        assert_eq!(parse_terminal_flags(on), CANONICAL);
        let off = "lflags: -icanon isig -echo echoe echok echoke echonl echoctl echoprt";
        assert_eq!(parse_terminal_flags(off), RAW);
    }

    /// A report that names neither flag is a failed readback, and it panics.
    ///
    /// The alternative — defaulting to `false`, or to the last value seen —
    /// would make a leg whose `stty` never ran compare its "before" against its
    /// "after" and find them equal, which is the vacuous pass every normalizing
    /// helper in this suite is written to refuse.
    #[test]
    #[should_panic(expected = "names both `icanon` and `echo`")]
    fn a_report_missing_a_flag_is_a_failure_and_not_a_default() {
        parse_terminal_flags("stty: stdin isn't a terminal\n");
    }

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
