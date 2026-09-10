//! The two decisions a session root makes about a tool call (REQ-615
//! architecture ADR-1): may this write, and did this command say `cd`.
//!
//! Pure and I/O-free — every input arrives as a `&str`, a [`RootKind`] and, for
//! the paths, a borrowed [`Path`]. Nothing here opens a file, spawns a child or
//! reads an environment variable, so every rule below is reachable from a
//! table-driven unit test with no session, no daemon and no filesystem
//! (conventions.md; architecture.md "Policy is pure, mechanism is gated").
//!
//! One test is the exception and says so: the cross-gate differential
//! ([`tests::the_write_gate_and_the_classifier_agree_on_what_reads_nothing`])
//! drives the *classifier* as well as this gate, and the classifier walks a
//! root. It mints a fixture directory for that reason. The production code in
//! this module is unchanged in its purity.
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
use crate::harness::tools::shell_syntax::strip_null_redirects;
/// Doc-link only: this gate no longer calls the recogniser directly — it calls
/// [`strip_null_redirects`], which is the one wrapper both gates share.
#[cfg(doc)]
use crate::harness::tools::shell_syntax::NullRedirect;

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
///
/// # One strip, and **both** triggers read its residue (Phase-5 re-verify)
///
/// Trigger (a) used to read the raw command, and a *leading* null redirect
/// therefore shadowed the verb: [`command_position_programs`] takes the first
/// word of each segment, and in `2>/dev/null rm -rf ~/x` that word is
/// `2>/dev/null`. Six spellings were `Allowed` at a home root —
/// `2>/dev/null rm -rf ~/x`, `2>&1 mkdir foo`, `</dev/null git init`,
/// `ls &>/dev/null rm -f ~/.zshrc`, `ls; >/dev/null mkdir ~/evil` and
/// `ls 2>&1; 2>&1 rm -rf ~/x` — each of which is exactly the harm BR-4 exists
/// to stop, reached by prefixing it with a redirect that reads nothing.
///
/// So the residue is computed **once**, here, and handed to both triggers. The
/// redirect words are the words `sh` itself does not take as a command, so the
/// residue's first word is the word `sh` would run — which is the same argument
/// REQ-620 BR-5 makes for the classifier, one gate over.
///
/// The fail-closed arm below deliberately keeps reading the **raw** command: it
/// asks "did the tokenizer read anything out of what the user typed", and a
/// command that is *nothing but* a null redirect (`2>/dev/null`) has a word for
/// it to find and creates nothing. Reading the residue there would refuse that
/// command for having stripped to the empty string, which is a tightening
/// nothing asked for and no transcript carries.
#[must_use]
pub(crate) fn write_gate(command: &str, kind: RootKind) -> WriteVerdict {
    if !gates_writes(kind) {
        return WriteVerdict::Allowed;
    }
    let residue = strip_null_redirects(command).residue;
    if names_a_write_verb(&residue) || residue_carries_redirection(&residue) {
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
///
/// **`command` here is [`write_gate`]'s residue, not the raw command**, and that
/// is load-bearing rather than incidental: [`command_position_programs`] is a
/// whitespace tokenizer, so a leading `2>/dev/null` is a "program" and the real
/// verb behind it is never read. See [`write_gate`]'s own docs for the six
/// spellings that shadowed a write that way.
fn names_a_write_verb(command: &str) -> bool {
    if command_position_programs(command)
        .iter()
        .any(|program| WRITE_VERBS.contains(program))
    {
        return true;
    }
    // The two-word forms. Segment-wise for the same reason: `cd x && git init`
    // is a `git init`.
    command.split(['|', ';', '&', '(', '\n']).any(|segment| {
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

/// Trigger (b): a `>` or `<` surviving [`strip_null_redirects`] at top level —
/// outside single quotes, double quotes and a backslash escape.
///
/// # Two spellings are redirections and are not writes
///
/// Both were false positives in the first implementation of this gate, and both
/// are common enough that refusing them would have made a home-rooted session
/// unusable for reading — which is the state a user is in *before* they run
/// `/cd`, so it is the state that matters most:
///
/// * **`2>&1`, `>&2`, `1>&2`** — a redirection of a file *descriptor*. Nothing
///   is created; the target is a number, not a path.
/// * **`2>/dev/null`, `>/dev/null`** — a write to the null device. It is a
///   write in the strictest reading and creates nothing anywhere, and
///   `cmd 2>/dev/null` is the single most common redirection in a read-only
///   command. `.adlc`-reading skill preambles are written this way.
///
/// Everything else with a top-level `>` is treated as a write, including a
/// target this scanner cannot resolve — fail closed, as the REQ's assumption
/// requires. The quote awareness is what keeps `echo "2 > 1"` allowed.
///
/// # One recogniser, two gates, and **one wrapper** (REQ-620 ADR-620-1)
///
/// Both exemptions moved to [`NullRedirect`], which
/// [`super::tools::shell_provenance::classify`] also reads. Two hand-rolled
/// readings of `2>/dev/null` in one daemon is LESSON-494's shape: the day one
/// of them accepts `2>/dev/nullx` and the other does not, one gate is wrong and
/// no test says which.
///
/// **Sharing the recogniser was not enough** (Phase-5 verify, M1). The first
/// version of this function wrapped [`NullRedirect`] in a *positional* scan of
/// its own — find each top-level `>`, widen to its whitespace-delimited word,
/// ask the recogniser about that word — while the classifier wrapped the same
/// recogniser in [`strip_null_redirects`], which peels a redirect glued to a
/// following separator. Two wrappers around one recogniser is the same defect
/// one step out: `ls 2>&1; ls` read nothing in the classifier and was a
/// **write** here, which the pre-REQ-620 gate had allowed. So the question is
/// now asked once, of the residue: *after the strip, does any word still carry
/// a `>` or a `<` outside quotes?*
///
/// # What the unification tightens, and why that is the safe direction
///
/// Three spellings neither the table below nor any transcript carries become
/// writes: `cmd>&2` and `cmd >|/dev/null` (whole-word, where the old inline
/// scan was positional), and any top-level `<` — `cat < input` — which the old
/// scan never looked for at all. The two directions of this gate are not
/// symmetric: a false "write" refuses a command at a home root, a false "not a
/// write" scaffolds a project into `$HOME`. So the tightening is the safe way
/// to be wrong, and it is recorded here rather than papered over.
///
/// # Quoting still belongs to this gate
///
/// [`strip_null_redirects`] is whitespace-word-based and quote-blind, and it
/// cannot become quote-aware without becoming the shell lexer ADR-614-1
/// refuses. It does not have to be: no word carrying a quote parses as a
/// [`NullRedirect`], so a lift can never change the residue's quote parity, and
/// [`top_level_positions`] does the quote reasoning over the residue exactly as
/// it did over the command. `echo "2 > 1"` is still allowed.
///
/// **Mutation (run, red, reverted):** drop the strip, so the residue is the
/// command — [`tests::the_write_gate_refuses_both_triggers_and_nothing_benign`]
/// reds on `cat missing 2>/dev/null` and
/// [`tests::the_write_gate_and_the_classifier_agree_on_what_reads_nothing`]
/// reds on its first row. Drop the `<` disjunct and the differential test reds
/// on `cat < input`, alone.
///
/// # Where the production code asks this, and why the raw-command form is a
/// test helper
///
/// [`write_gate`] does **not** call this: it strips once and asks
/// [`residue_carries_redirection`], because trigger (a) needs the same residue
/// and a second strip would be a second wrapper around one recogniser — the
/// defect M1 removed one level up.
///
/// So this function is the *composition* write_gate performs, spelled once for
/// a reader who has a raw command in hand. It is `#[cfg(test)]` for the reason
/// [`super::tools::shell_provenance::is_recognised_verb`] is: a second reader of
/// a rule on the production path is how a gate comes to have two answers
/// (LESSON-494), and the one place that genuinely wants the raw-command form is
/// [`tests::the_write_gate_and_the_classifier_agree_on_what_reads_nothing`],
/// which has to ask this gate's question of exactly the string it asks the
/// classifier.
#[cfg(test)]
#[must_use]
pub(crate) fn has_top_level_redirection(command: &str) -> bool {
    residue_carries_redirection(&strip_null_redirects(command).residue)
}

/// Trigger (b) over an already-stripped command — the production half of
/// [`has_top_level_redirection`], whose docs carry the rule and its mutations.
fn residue_carries_redirection(residue: &str) -> bool {
    ['>', '<']
        .into_iter()
        .any(|needle| top_level_positions(residue, needle).next().is_some())
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
///
/// # Why it names redirection and not only creation (Phase-5 re-verify)
///
/// "Nothing may be created here" was the whole sentence, and it stopped being
/// true of every refusal the moment trigger (b) started reading `<` as well as
/// `>`: `cat < input` creates nothing at all and is refused, and a user told
/// only that nothing may be created would go looking for the write. The gate is
/// deliberately coarser than "creates a file" — it refuses any redirection it
/// cannot prove reads nothing — so the sentence says both halves. It stays one
/// sentence for both tools: `edit` never carries a redirection and simply does
/// not reach that clause's example.
#[must_use]
pub(crate) fn write_refusal(root_display: &str, kind: RootKind) -> String {
    let place = match kind {
        RootKind::FilesystemRoot => "the filesystem root",
        _ => "your home folder",
    };
    format!(
        "refused: this session's root is {root_display} ({place}), not a project, \
         so nothing may be created here — and a command carrying a redirection \
         other than to /dev/null is refused whether or not it would create \
         anything. Ask the user to run `{WRITE_REMEDY}` — only they can move \
         the root."
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
            rest.strip_prefix('/')
                .is_some_and(|rest| home.join(rest) == root)
        };
    }
    Path::new(target) == root
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use teton_core::config::DEFAULT_BOUNDARIES;
    use teton_core::entities::PrivacyBoundary;

    use crate::harness::tools::shell_provenance;
    use crate::harness::tools::shell_syntax::STRIP_ROWS;

    fn home_root() -> (PathBuf, PathBuf) {
        let home = PathBuf::from("/Users/dev");
        (home.clone(), home)
    }

    /// A **project** root for the cross-gate differential, and the one place
    /// this module's tests touch a filesystem.
    ///
    /// `RootKind::Project` and a non-empty boundary set are both preconditions
    /// for `shell_provenance::classify` to read the command at all — it answers
    /// `Unknown` before the grammar on either — so a bare temp dir or an empty
    /// boundary list would make every row of the differential agree for the
    /// wrong reason.
    fn classifier_root(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "teton-rootgate-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        dir
    }

    fn builtin_boundaries() -> Vec<PrivacyBoundary> {
        DEFAULT_BOUNDARIES
            .iter()
            .map(|g| PrivacyBoundary::builtin(*g))
            .collect()
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
    ///
    /// **Mutation (run, red, reverted, Phase-5 re-verify):** hand
    /// [`names_a_write_verb`] the raw command instead of the residue — **1 test
    /// red of 2,252**, this one, naming `2>/dev/null rm -rf ~/x`. All **six**
    /// leading-redirect rows come back `Allowed` under it (measured by probing
    /// the six directly, since the `assert_eq!` aborts on the first), and the
    /// must-not-fire row `2>/dev/null ls` is `Allowed` either way. That mutation
    /// *is* the shipped bug: `command_position_programs` takes `2>/dev/null` as
    /// the program and the `rm -rf ~/x` behind it was never read.
    ///
    /// One test, and that is the finding rather than a shortfall: this is the
    /// only table that asserts trigger (a)'s **input**. The differential below
    /// drives rows that name no write verb, so it cannot see this mutation at
    /// all, and no integration suite runs a `shell` call at a home root.
    #[test]
    fn the_write_gate_refuses_both_triggers_and_nothing_benign() {
        let refused = [
            ("mkdir -p .adlc/context", "the 2026-09-04 command itself"),
            // Phase-5 re-verify: a null redirect in **command position**
            // shadowed the verb behind it, because trigger (a) read the raw
            // command and the tokenizer's "program" was the redirect word.
            // Every one of these was `Allowed` at a home root.
            (
                "2>/dev/null rm -rf ~/x",
                "a leading redirect shadowing an `rm`",
            ),
            (
                "2>&1 mkdir foo",
                "a leading duplication shadowing a `mkdir`",
            ),
            (
                "</dev/null git init",
                "a leading stdin redirect shadowing the two-word form",
            ),
            (
                "ls &>/dev/null rm -f ~/.zshrc",
                "`&>` mid-command, whose residue separator re-splits the segment",
            ),
            (
                "ls; >/dev/null mkdir ~/evil",
                "a redirect in a later segment's command position",
            ),
            (
                "ls 2>&1; 2>&1 rm -rf ~/x",
                "a redirect glued to a separator, then another shadowing the verb",
            ),
            (
                "cd ~ && mkdir foo",
                "a write past a cd, in command position",
            ),
            ("rm -rf build", "a removal"),
            ("git init", "the two-word form"),
            (
                "echo hi > notes.md",
                "a redirection, whose first verb is echo",
            ),
            ("cat a >> b", "an appending redirection"),
            (
                "cat a > /dev/nullx",
                "a path that merely starts like the device",
            ),
            (
                "cat a > out 2>/dev/null",
                "one real redirection among exempt ones",
            ),
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
            (
                "grep -rn mkdir src",
                "a write verb as an argument, not in command position",
            ),
            ("echo 'mkdir x'", "a write verb inside a string"),
            // The two redirections that are not writes. Both were false
            // positives in this gate's first implementation, and both are
            // ordinary in a session that is only reading — which is the state a
            // user is in *before* they run `/cd`, so it is the state that
            // matters most.
            ("cat missing 2>/dev/null", "stderr to the null device"),
            (
                "cat missing 2> /dev/null",
                "with a space, as a shell allows",
            ),
            ("ls -la > /dev/null", "stdout to the null device"),
            ("make 2>&1", "a file-descriptor duplication, not a path"),
            ("cmd >&2", "the same, the other way round"),
            (
                "cat .adlc/context/architecture.md 2>/dev/null || echo none",
                "the shipped ADLC preamble shape",
            ),
            // Phase-5 verify, M1: the three shapes the positional scan called
            // writes and the pre-REQ-620 gate allowed.
            ("ls 2>&1; ls", "a duplication glued to a `;`"),
            (
                "ls 2>&1; echo ok",
                "the same, with the separator spaced off",
            ),
            ("ls 2>&1|head", "a duplication glued to a pipe"),
            // The must-not-fire row for the leading-redirect refusals above: a
            // redirect in command position is still not a write when the verb
            // behind it is not one. Without this row the fix "refuse anything
            // whose first word is a redirect" would pass the table.
            (
                "2>/dev/null ls",
                "a leading redirect in front of an ordinary read",
            ),
        ];
        for (command, why) in allowed {
            assert_eq!(
                write_gate(command, RootKind::Home),
                WriteVerdict::Allowed,
                "`{command}` must still run at a home root ({why})"
            );
        }
    }

    /// **Phase-5 verify, M1: the write gate and the classifier ask one
    /// question of one recogniser.**
    ///
    /// Sharing [`NullRedirect`] was not enough. Both gates wrapped it, and the
    /// wrappers disagreed: this one widened each top-level `>` to its
    /// whitespace word, `strip_null_redirects` peeled a redirect glued to a
    /// following separator, and `ls 2>&1; ls` came out "reads nothing" in the
    /// classifier and "write" here — a regression against the pre-REQ-620 gate,
    /// which allowed it. The wrapper is now the strip, for both.
    ///
    /// So this is a **differential** table (LESSON-494's shape): each row goes
    /// through [`has_top_level_redirection`] *and* through the residue the
    /// classifier reads, and the two must answer the same question. A row that
    /// only asserted the gate's verdict would go green again the day the two
    /// wrappers drift apart, which is exactly the defect being fixed.
    ///
    /// **Mutation (run, red, reverted):** restore the positional scan (drop the
    /// strip from [`has_top_level_redirection`]) — the three separator-glued
    /// benign rows red here and in
    /// [`tests::the_write_gate_refuses_both_triggers_and_nothing_benign`].
    /// Drop the `<` disjunct — the `cat < input` row reds here, alone.
    ///
    /// # It asks the classifier, because it used to ask itself (Phase-5 re-verify)
    ///
    /// The second half of this test used to define a local `reads_nothing`
    /// closure that **re-implemented** [`has_top_level_redirection`] line for
    /// line — strip, then scan the residue for `>` and `<`. Two spellings of one
    /// function agree by construction, so the half that was supposed to be the
    /// differential asserted nothing at all: the wrappers could have drifted
    /// exactly as M1 found them drifted, and the closure would have drifted with
    /// them.
    ///
    /// So the other reader is now the **actual** other reader. For every row,
    /// the classifier is run and the question is asked of its verdict: did it
    /// refuse on [`UnmodelledSyntax::Redirect`]? That must hold **iff** this
    /// gate sees a redirection, and the rows are driven from
    /// [`STRIP_ROWS`] — the recogniser's own residue table — so a row added for
    /// the strip is a row both gates answer for.
    ///
    /// **Mutation (run, red, reverted, Phase-5 re-verify):** make
    /// `first_unmodelled_class` stop mapping `<` to `Redirect` — **5 tests red
    /// of 2,252**, this one among them, naming `ls</dev/null`: the classifier
    /// stops refusing it as a redirect while this gate still refuses it as a
    /// write. (The other four are `shell_provenance`'s own —
    /// `adversarial_spellings_are_all_unknown`,
    /// `each_unmodelled_class_names_itself_and_nothing_else`,
    /// `every_other_redirect_stays_unmodelled` and
    /// `the_redirect_differential_table`.) Under the old self-referential
    /// closure this test was **not** among them, which is the whole reason the
    /// closure had to go: it would have agreed with the gate no matter what the
    /// classifier had come to think.
    #[test]
    fn the_write_gate_and_the_classifier_agree_on_what_reads_nothing() {
        let root = classifier_root("gate-differential");
        let redirect_class = shell_provenance::UnmodelledSyntax::Redirect.reason();
        // Both readers, on one row. `classify` is the production entry point,
        // not a re-statement of the gate's own scan.
        let both = |command: &str| {
            let verdict = shell_provenance::classify(
                &root,
                RootKind::Project,
                &builtin_boundaries(),
                Vec::new(),
                command,
            );
            (
                has_top_level_redirection(command),
                verdict.reason == redirect_class,
            )
        };

        // The tightenings, recorded rather than papered over: each is a write
        // now and was not before REQ-620, and each is the *safe* way for this
        // asymmetric gate to be wrong.
        let tightened = [
            ("cmd>&2", "a duplication glued to its verb"),
            ("cmd >|/dev/null", "a `>|` glued to the device"),
            (
                "cmd >| /dev/null",
                "and spaced — `>|` is not a bare operator",
            ),
            ("cat < input", "a `<` the old scan never looked for"),
        ];

        let rows = STRIP_ROWS
            .iter()
            .map(|(command, _, _)| *command)
            .chain(tightened.iter().map(|(command, _)| *command));
        for command in rows {
            let (gate_sees_a_redirect, classifier_refused_a_redirect) = both(command);
            assert_eq!(
                gate_sees_a_redirect, classifier_refused_a_redirect,
                "`{command}`: the write gate says redirection={gate_sees_a_redirect} and the \
                 classifier says redirect-class={classifier_refused_a_redirect}. One recogniser, \
                 one answer — this is the M1 drift, one level out"
            );
        }

        // The gate's verdicts on the same rows, which the iff above does not
        // pin: an agreed "there is a redirection here" still has to *refuse*.
        let benign = [
            "ls 2>&1; echo ok",
            "ls 2>&1|head",
            "cat x > /dev/null; ls",
            "ls 2>&1; ls",
            "ls -la",
            "cat missing 2>/dev/null",
            "cat missing 2> /dev/null",
            "make 2>&1",
            "cmd >&2",
            "cat .adlc/context/architecture.md 2>/dev/null || echo none",
        ];
        for command in benign {
            assert!(
                !has_top_level_redirection(command),
                "`{command}` leaves no redirect in the residue"
            );
            assert_eq!(
                write_gate(command, RootKind::Home),
                WriteVerdict::Allowed,
                "`{command}` reads nothing, so it is not a write"
            );
        }
        for (command, why) in tightened {
            assert_eq!(
                write_gate(command, RootKind::Home),
                WriteVerdict::RefusedNonProject,
                "`{command}` is a write under the unified wrapper ({why})"
            );
        }

        // **The gate-only half.** These two rows are not in the differential
        // above and must not be: the classifier refuses them on
        // `UnmodelledSyntax::Quote`, which outranks `Redirect` in
        // `UNMODELLED_ORDER`, so it never reaches the redirect question and the
        // iff would hold for a reason that has nothing to do with redirection.
        // What they assert is this gate's own quote awareness, which
        // `strip_null_redirects` deliberately does not have (it is
        // whitespace-word-based, and no word carrying a quote parses as a
        // `NullRedirect`, so a lift cannot change the residue's quote parity).
        for command in ["echo \"2 > 1\"", "echo 'a > b'"] {
            assert!(
                !has_top_level_redirection(command),
                "`{command}` has no redirect outside its quotes"
            );
            assert_eq!(
                write_gate(command, RootKind::Home),
                WriteVerdict::Allowed,
                "`{command}` is a quoted string, not a redirection"
            );
        }

        // Ordinary writes, so the table cannot pass by calling everything a
        // read: the benign half above is what stops the converse.
        for command in ["echo hi > notes.md", "cat a >> b", "cat a > /dev/nullx"] {
            assert!(
                has_top_level_redirection(command),
                "`{command}` is a real redirection"
            );
        }
        std::fs::remove_dir_all(&root).ok();
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
        assert!(
            note("echo 'cd /tmp'").is_none(),
            "a cd inside a string is not a cd"
        );
        assert!(
            note("cdto /tmp").is_none(),
            "a program merely starting with cd"
        );
        assert!(note("cd .").is_none(), "the root itself");
        assert!(note("cd ~").is_none(), "the root itself, spelled home");
        assert!(
            note("cd /Users/dev").is_none(),
            "the root itself, spelled absolutely"
        );
        assert!(
            note("cd").is_none(),
            "a bare cd at a home root goes nowhere"
        );

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
        assert_eq!(
            split_top_level_or("echo 'a || b'"),
            None,
            "and single quotes"
        );
        assert_eq!(
            split_top_level_or("a | b"),
            None,
            "a pipe is not a separator"
        );
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
