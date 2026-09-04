//! The two decisions a session root makes about a tool call (REQ-615
//! architecture ADR-1): may this write, and did this command say `cd`.
//!
//! Pure and I/O-free — every input arrives as a `&str`, a [`RootKind`] and, for
//! the paths, a borrowed [`Path`]. Nothing here opens a file, spawns a child or
//! reads an environment variable, so every rule below is reachable from a
//! table-driven unit test with no session, no daemon and no filesystem
//! (conventions.md; architecture.md "Policy is pure, mechanism is gated").
//!
//! # One module, two enforcement points
//!
//! BR-4 is one rule that `shell` and `edit` both enforce, and architecture.md's
//! standing rule is that an invariant with more than one enforcement point needs
//! a sweep rather than a fix. A pair of conditions hand-inlined into two `run`
//! bodies could not support one; a named function that both call can.
//!
//! # What this gate is, and what it is not
//!
//! It is a **guard rail against scaffolding a project into `$HOME`** — the
//! observed harm is a model that believed a `cd` had persisted and ran
//! `mkdir -p .adlc/context` in the user's home folder (REQ-615 Description,
//! consequence 2).
//!
//! It is **not a sandbox**, and the difference is worth stating plainly because
//! a documented guarantee that is false is worse than a narrower one that is
//! true (architecture.md, REQ-596 BR-6). [`command_position_programs`] is a
//! whitespace tokenizer, not a shell lexer, so a write reached through
//! indirection — `sh -c 'mkdir x'`, `xargs mkdir`, a script — is **not** seen.
//! Those spellings are REQ-614's opaque-verb territory, and closing them here
//! would mean refusing every `sh -c` at a home root, which is far wider than
//! this rule. The residual is recorded rather than papered over.
//!
//! What the gate *does* fail closed on is a command it cannot parse at all: a
//! non-empty command yielding no command-position program refuses at a
//! non-project root, per the REQ's own assumption.

use std::path::Path;

use teton_protocol::methods::RootKind;

use crate::harness::tools::shell::command_position_programs;

/// Whether a write is permitted from this session root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteVerdict {
    /// The write proceeds.
    Allowed,
    /// The root is a home folder or the filesystem root (BR-4).
    RefusedNonProject,
}

/// The verbs whose *purpose* is to change the filesystem (BR-4, trigger (a)).
///
/// A pinned table, as the REQ's assumption asks — the same shape REQ-614's
/// opaque-verb set takes. Membership is by the program's basename, so
/// `/bin/mkdir` and `mkdir` are one entry.
///
/// Two-word verbs are handled separately by [`WRITE_SUBCOMMANDS`]: `git` is not
/// a write verb (`git status` and `git log` are the reason a session at a home
/// root still works at all), but `git init` is.
const WRITE_VERBS: &[&str] = &["mkdir", "touch", "rm", "mv", "cp", "tee", "install", "ln"];

/// Two-word write verbs: the program, and the first argument that makes it one.
const WRITE_SUBCOMMANDS: &[(&str, &str)] = &[("git", "init")];

/// Whether this kind of root gates writes at all.
///
/// `home` and `filesystem_root` only. A `plain` directory root — a folder that
/// is not a project and is not home — is deliberately **not** gated: it is
/// where a user scaffolds a new project, and REQ-613's `TETON.md` write must
/// keep working there (BR-4's carve-out; OQ-2, resolved). A `project` root
/// gates nothing at all (BR-9).
#[must_use]
pub(crate) fn gates_writes(kind: RootKind) -> bool {
    matches!(kind, RootKind::Home | RootKind::FilesystemRoot)
}

/// BR-4 for `edit`, which is unconditionally a write.
///
/// A separate entry point rather than a fabricated command string, so the two
/// callers share one table of kinds without one of them lying about what it is
/// doing.
#[must_use]
pub(crate) fn edit_gate(kind: RootKind) -> WriteVerdict {
    if gates_writes(kind) {
        WriteVerdict::RefusedNonProject
    } else {
        WriteVerdict::Allowed
    }
}

/// BR-4 for `shell`: refuse `command` when either trigger fires and the root is
/// one that gates writes.
///
/// The two triggers are **independent**, and that is the whole reason there are
/// two. A redirection is never a first verb — `echo hi > ~/x` has first verb
/// `echo` — so a single verb rule cannot see it, and a single redirection rule
/// cannot see `mkdir`.
#[must_use]
pub(crate) fn write_gate(command: &str, kind: RootKind) -> WriteVerdict {
    if !gates_writes(kind) {
        return WriteVerdict::Allowed;
    }
    if names_a_write_verb(command) || has_top_level_redirection(command) {
        return WriteVerdict::RefusedNonProject;
    }
    // Fail closed (the REQ's assumption): a non-empty command the tokenizer
    // read nothing out of is treated as a write. An *empty* command is not —
    // it is the argument-validation error the tool reports for itself.
    if !command.trim().is_empty() && command_position_programs(command).is_empty() {
        return WriteVerdict::RefusedNonProject;
    }
    WriteVerdict::Allowed
}

/// Trigger (a): a command-position word in [`WRITE_VERBS`], or a
/// [`WRITE_SUBCOMMANDS`] pair.
///
/// Reads **command positions**, not just the first word, so `cd ~ && mkdir foo`
/// refuses — which is the exact spelling the 2026-09-04 session used.
fn names_a_write_verb(command: &str) -> bool {
    if command_position_programs(command)
        .iter()
        .any(|program| WRITE_VERBS.contains(program))
    {
        return true;
    }
    // The two-word forms. Segment-wise for the same reason: `cd x && git init`
    // is a `git init`.
    command
        .split(['|', ';', '&', '(', '\n'])
        .any(|segment| {
            let mut words = segment.split_whitespace();
            let Some(program) = words.next() else {
                return false;
            };
            let program = program.rsplit('/').next().unwrap_or(program);
            let Some(argument) = words.next() else {
                return false;
            };
            WRITE_SUBCOMMANDS
                .iter()
                .any(|(verb, sub)| *verb == program && *sub == argument)
        })
}

/// Trigger (b): a `>`, `>>` or `>|` at top level — outside single quotes,
/// double quotes and a backslash escape.
///
/// `2>&1` is a redirection of a file **descriptor**, not a write to a path, and
/// it is by far the most common `>` in a read-only command (`cmd 2>/dev/null`
/// is the counter-example and *is* a write, to `/dev/null`, which is harmless
/// but not worth a special case). The quote awareness is what keeps
/// `echo "2 > 1"` allowed.
#[must_use]
pub(crate) fn has_top_level_redirection(command: &str) -> bool {
    top_level_positions(command, '>').next().is_some()
}

/// Byte offsets of every top-level occurrence of `needle` in `command`.
///
/// Top level means: not inside `'…'`, not inside `"…"`, and not immediately
/// preceded by a backslash escape. Single quotes suppress the backslash, as a
/// shell does.
///
/// One scanner, two consumers — [`has_top_level_redirection`] here and
/// [`split_top_level`] for the `||` split BR-6 needs. Two quote scanners
/// agreeing on ordinary input is not a property; the adversarial spellings are
/// where they diverge (REQ-563's rule, architecture.md).
fn top_level_positions(command: &str, needle: char) -> impl Iterator<Item = usize> + '_ {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    command.char_indices().filter_map(move |(at, c)| {
        if escaped {
            escaped = false;
            return None;
        }
        match c {
            '\\' if !in_single => {
                escaped = true;
                None
            }
            '\'' if !in_double => {
                in_single = !in_single;
                None
            }
            '"' if !in_single => {
                in_double = !in_double;
                None
            }
            _ if in_single || in_double => None,
            _ if c == needle => Some(at),
            _ => None,
        }
    })
}

/// Split `command` at its **first top-level `||`**, into the primary and the
/// remainder (BR-6, architecture ADR-6).
///
/// `None` when the command has no top-level `||` — that command runs exactly as
/// it does today. `a || b || c` yields `("a", "b || c")`: only the *first*
/// branch's exit is observed, and the remainder is handed to the shell whole so
/// a chain's semantics stay the shell's.
///
/// A single `|` is a pipe, not a separator, so the scan requires the pair.
#[must_use]
pub(crate) fn split_top_level_or(command: &str) -> Option<(&str, &str)> {
    let bars: Vec<usize> = top_level_positions(command, '|').collect();
    let at = bars
        .windows(2)
        .find(|pair| pair[1] == pair[0] + 1)
        .map(|pair| pair[0])?;
    let (primary, rest) = command.split_at(at);
    Some((primary.trim(), rest[2..].trim()))
}

/// The remedy every BR-4 refusal names, and the payload field the event
/// carries — one spelling, so the sentence the model reads and the record a
/// client renders cannot come to disagree.
pub(crate) const WRITE_REMEDY: &str = "/cd <name>";

/// The sentence a refused write gets (BR-4).
///
/// Composed **here**, at one site, rather than at each tool: the tools differ
/// in what they were about to do and not in why they may not, and a message
/// composed at each point of detection is how two surfaces come to word one
/// rule two ways (architecture.md, LESSON-557).
#[must_use]
pub(crate) fn write_refusal(root_display: &str, kind: RootKind) -> String {
    let place = match kind {
        RootKind::FilesystemRoot => "the filesystem root",
        _ => "your home folder",
    };
    format!(
        "refused: this session's root is {root_display} ({place}), not a project, \
         so nothing may be created here. Ask the user to run `{WRITE_REMEDY}` — \
         only they can move the root."
    )
}

/// BR-2's note for `command`, or `None` when it named no `cd` — or named one
/// whose target *is* the session root.
///
/// # The unresolvable direction is deliberate
///
/// A literal target (`cd /a/b`, `cd ~`, `cd .`) is compared against the root. A
/// target that cannot be resolved without running the command (`cd "$X"`,
/// `cd $(cat p)`) **earns the note**: it is advisory text, so a spurious one
/// costs a line while a missing one restores the defect this REQ exists to
/// close (the REQ's own assumption fixes this direction).
#[must_use]
pub(crate) fn cd_note(
    command: &str,
    root: &Path,
    root_display: &str,
    home: Option<&Path>,
) -> Option<String> {
    if !cd_leaves_the_root(command, root, home) {
        return None;
    }
    Some(format!(
        "[ran in {root_display}; the next command starts there again]\n"
    ))
}

/// Whether `command` contains a `cd` in command position whose target is not
/// provably the session root.
fn cd_leaves_the_root(command: &str, root: &Path, home: Option<&Path>) -> bool {
    command.split(['|', ';', '&', '(', '\n']).any(|segment| {
        let mut words = segment.split_whitespace().skip_while(|word| {
            // The same env-assignment skip `command_position_programs` applies,
            // so `FOO=1 cd x` is still a `cd`.
            word.contains('=') && !word.contains(['\'', '"']) && !word.starts_with('=')
        });
        let Some(program) = words.next() else {
            return false;
        };
        if program.rsplit('/').next().unwrap_or(program) != "cd" {
            return false;
        }
        // `cd` with no argument goes to `$HOME`, which is the root only when
        // the session is rooted at home.
        let Some(target) = words.next() else {
            return home.is_none_or(|home| home != root);
        };
        // A second argument means this is not a plain `cd` we can reason about
        // (`cd a b` is a bash substitution form); say so by noting.
        if words.next().is_some() {
            return true;
        }
        !target_is_the_root(target, root, home)
    })
}

/// Whether a literal `cd` target names the session root itself.
///
/// Anything not resolvable from the token alone answers `false` — the caller
/// reads that as "emit the note".
fn target_is_the_root(target: &str, root: &Path, home: Option<&Path>) -> bool {
    // Unquoted, no expansion, no substitution: anything else is not a literal.
    if target.contains(['$', '`', '*', '?', '\'', '"', '\\']) {
        return false;
    }
    let target = target.trim_end_matches('/');
    if target.is_empty() {
        // `cd /` — the filesystem root.
        return root == Path::new("/");
    }
    if target == "." {
        return true;
    }
    if let Some(rest) = target.strip_prefix('~') {
        let Some(home) = home else {
            return false;
        };
        return if rest.is_empty() {
            home == root
        } else {
            rest.strip_prefix('/').is_some_and(|rest| home.join(rest) == root)
        };
    }
    Path::new(target) == root
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn home_root() -> (PathBuf, PathBuf) {
        let home = PathBuf::from("/Users/dev");
        (home.clone(), home)
    }

    /// **BR-4 / AC-3: both triggers refuse, and nothing benign does.**
    ///
    /// The benign half is the load-bearing half. A detector validated only
    /// against the spellings it was written for ships broken and passes its own
    /// suite — a home-rooted session that could not run `ls` or `git status`
    /// would be unusable, and the gate would be removed rather than fixed.
    ///
    /// Mutation: drop `mkdir` from `WRITE_VERBS`, or make
    /// `has_top_level_redirection` quote-blind — the corresponding row goes red.
    #[test]
    fn the_write_gate_refuses_both_triggers_and_nothing_benign() {
        let refused = [
            ("mkdir -p .adlc/context", "the 2026-09-04 command itself"),
            ("cd ~ && mkdir foo", "a write past a cd, in command position"),
            ("rm -rf build", "a removal"),
            ("git init", "the two-word form"),
            ("echo hi > notes.md", "a redirection, whose first verb is echo"),
            ("cat a >> b", "an appending redirection"),
            ("/bin/touch x", "a path-qualified verb"),
        ];
        for (command, why) in refused {
            assert_eq!(
                write_gate(command, RootKind::Home),
                WriteVerdict::RefusedNonProject,
                "`{command}` must be refused at a home root ({why})"
            );
        }

        let allowed = [
            ("ls -la", "a listing"),
            ("cat README.md", "a read"),
            ("git status", "git without its write subcommand"),
            ("git log --oneline -5", "likewise"),
            ("echo \"2 > 1\"", "a redirection character inside quotes"),
            ("echo 'a > b'", "and inside single quotes"),
            ("grep -rn mkdir src", "a write verb as an argument, not in command position"),
            ("echo 'mkdir x'", "a write verb inside a string"),
        ];
        for (command, why) in allowed {
            assert_eq!(
                write_gate(command, RootKind::Home),
                WriteVerdict::Allowed,
                "`{command}` must still run at a home root ({why})"
            );
        }
    }

    /// **BR-4: a command the tokenizer reads nothing out of fails closed.**
    ///
    /// The REQ's assumption fixes this direction: at a non-project root, an
    /// unparseable command is treated as a write. An *empty* command is not —
    /// that is the tool's own argument error, reported before this gate.
    ///
    /// Mutation: invert the fail-closed arm to `Allowed` — this goes red.
    #[test]
    fn an_unparseable_command_fails_closed_at_a_non_project_root() {
        assert_eq!(
            write_gate("|||", RootKind::Home),
            WriteVerdict::RefusedNonProject,
            "a non-empty command with no command-position program fails closed"
        );
        assert_eq!(
            write_gate("   ", RootKind::Home),
            WriteVerdict::Allowed,
            "an empty command is the tool's own argument error, not a write"
        );
    }

    /// **BR-9: a project or plain root gates nothing, whatever the command.**
    ///
    /// The benign path for the whole REQ. A `plain` root is where a user
    /// scaffolds a new project and where REQ-613's `TETON.md` write lands, so
    /// gating it would break a shipped feature (OQ-2, resolved).
    ///
    /// Mutation: add `RootKind::Plain` to `gates_writes` — the plain rows go red.
    #[test]
    fn a_project_or_plain_root_gates_nothing() {
        for kind in [RootKind::Project, RootKind::Plain] {
            for command in ["mkdir -p a/b", "rm -rf x", "echo hi > f", "|||"] {
                assert_eq!(
                    write_gate(command, kind),
                    WriteVerdict::Allowed,
                    "`{command}` at a {kind:?} root is not this REQ's business"
                );
            }
            assert_eq!(edit_gate(kind), WriteVerdict::Allowed);
        }
        for kind in [RootKind::Home, RootKind::FilesystemRoot] {
            assert_eq!(edit_gate(kind), WriteVerdict::RefusedNonProject);
        }
    }

    /// **BR-2 / AC-2: the note fires on a `cd` that leaves the root, and on
    /// nothing else.**
    ///
    /// Mutation: drop the `program != "cd"` early return — every command earns
    /// a note and the benign rows go red.
    #[test]
    fn the_cd_note_fires_on_a_cd_and_on_nothing_else() {
        let (home, root) = home_root();
        let note = |command: &str| cd_note(command, &root, "~", Some(&home));

        assert!(
            note("cd ~/GitHub/teton-code && pwd").is_some(),
            "the exact command the 2026-09-04 session ran five times"
        );
        assert!(note("cd /tmp").is_some());
        assert!(note("pwd && cd /tmp").is_some(), "a cd in a later segment");
        assert!(note("FOO=1 cd /tmp").is_some(), "past an env assignment");

        assert!(note("ls -la").is_none(), "no cd, no note");
        assert!(note("echo 'cd /tmp'").is_none(), "a cd inside a string is not a cd");
        assert!(note("cdto /tmp").is_none(), "a program merely starting with cd");
        assert!(note("cd .").is_none(), "the root itself");
        assert!(note("cd ~").is_none(), "the root itself, spelled home");
        assert!(
            note("cd /Users/dev").is_none(),
            "the root itself, spelled absolutely"
        );
        assert!(note("cd").is_none(), "a bare cd at a home root goes nowhere");

        let elsewhere = PathBuf::from("/Users/dev/GitHub/teton-code");
        assert!(
            cd_note("cd", &elsewhere, "~/GitHub/teton-code", Some(&home)).is_some(),
            "a bare cd at a project root DOES leave it — it goes to $HOME"
        );
    }

    /// **BR-2: a target that cannot be resolved without running the command
    /// still earns a note.**
    ///
    /// The direction the REQ's assumption fixes: a spurious advisory line costs
    /// a line, a missing one restores the defect.
    ///
    /// Mutation: make `target_is_the_root` return `true` for an unresolvable
    /// token — every row here goes red.
    #[test]
    fn an_unresolvable_cd_target_still_earns_a_note() {
        let (home, root) = home_root();
        for command in [
            "cd \"$PROJECT\"",
            "cd $(cat .last-project)",
            "cd `pwd`",
            "cd ~/proj*",
            "cd a b",
        ] {
            assert!(
                cd_note(command, &root, "~", Some(&home)).is_some(),
                "`{command}` cannot be resolved statically, so it notes"
            );
        }
    }

    /// **BR-6 / TASK-007's dependency: the `||` split is top level and first
    /// only.**
    ///
    /// Mutation: drop the quote tracking in `top_level_positions` — the quoted
    /// row goes red. Drop the `windows(2)` adjacency test — the pipe row goes
    /// red.
    #[test]
    fn the_top_level_or_split_takes_the_first_separator_outside_quotes() {
        assert_eq!(
            split_top_level_or("cat .adlc/context/architecture.md || echo none"),
            Some(("cat .adlc/context/architecture.md", "echo none"))
        );
        assert_eq!(
            split_top_level_or("a || b || c"),
            Some(("a", "b || c")),
            "the remainder is handed to the shell whole"
        );
        assert_eq!(split_top_level_or("echo \"a || b\""), None, "inside quotes");
        assert_eq!(split_top_level_or("echo 'a || b'"), None, "and single quotes");
        assert_eq!(split_top_level_or("a | b"), None, "a pipe is not a separator");
        // Two *non-adjacent* bars, which is what actually exercises the
        // adjacency test: with one bar `windows(2)` is empty and the check
        // cannot be observed at all. Dropping `pair[1] == pair[0] + 1` splits
        // this pipeline at its first pipe and silently runs half of it.
        assert_eq!(
            split_top_level_or("grep -rn adlc . | head -20 | wc -l"),
            None,
            "a pipeline is not a fallback, however many pipes it has"
        );
        assert_eq!(split_top_level_or("cat x"), None, "no separator at all");
    }
}
