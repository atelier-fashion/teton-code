//! REQ-614: what a `shell` command could have read, decided before it runs.
//!
//! Every `shell` result used to carry [`ToolProvenance::Unknown`], and egress
//! fail-closes on unknown provenance whenever any boundary is configured. Since
//! REQ-597 made the thirteen builtin globs always-on, those two rules composed
//! into a behaviour nobody chose: the first shell command of any session pinned
//! that session to the local tier for the rest of its life.
//!
//! This module narrows the opacity. It answers one question — *could this
//! command have read a file the session must not send remotely?* — and it
//! answers it from the command's resolved cwd and its path arguments alone,
//! never from the command's output and never from its exit status.
//!
//! # Two consumers, one grammar
//!
//! [`classify`] has a second caller since REQ-619: `skills::dynamic::run_all`,
//! which runs a skill's `` !`cmd` `` preambles. It asks the same question of
//! the same command text, with the session root as cwd, once per command and
//! before that command spawns (ADR-619-1) — so a `cat` typed by the model
//! through `shell` and the same `cat` written into a skill body cannot reach
//! different answers. Nothing in the grammar below is skill-aware; the second
//! consumer supplies the same four inputs the first one does.
//!
//! # The default is `Unknown`, and that is the whole design (ADR-614-1)
//!
//! REQ-614 BR-1(e) reads like a denylist: a set of opaque verbs (`sh -c`,
//! `python`, `cargo`, `curl`, …) that force `Unknown`. Implemented as one, it
//! would be a machine for generating security false negatives. The executor is
//! `sh -c <command>` ([`super::shell::run_bounded`]), so the parse that decides
//! a command's reach is POSIX `sh`'s, and a hand-rolled tokenizer diverges from
//! it on exactly the adversarial spellings that matter (LESSON-494: one
//! backslash defeated REQ-563's allowlist because the gate and the socket used
//! two parsers).
//!
//! [`super::shell::command_position_programs`] — the tokenizer already in this
//! directory — documents its own misses (indirection through `xargs`, a quoted
//! env-assignment value) and calls them acceptable, because for REQ-607's
//! withheld advisory "a false negative costs one user the sentence they would
//! have got". Here a false negative costs a **leak**. The polarity is inverted,
//! so that tokenizer cannot be reused as the basis of a `Rooted` verdict.
//!
//! So this is an **allowlist grammar**: [`classify`] returns [`Verdict::Rooted`]
//! only when every token of the command was recognised, and returns
//! [`Verdict::Unknown`] for everything else — which is precisely today's
//! behaviour. **The classifier can only ever be more permissive than the
//! pre-REQ-614 daemon by an amount it can prove.** Every miss, every unhandled
//! spelling, every verb nobody thought of lands on the old answer.
//!
//! # Content-free by construction
//!
//! [`Verdict::reason`] is a `&'static str` drawn from a closed set. The spec
//! requires the reason to name why the verdict was reached while carrying no
//! command text and no file content; a `String` would make that a rule someone
//! has to keep, and a `&'static str` makes it a thing that cannot be violated
//! without changing the type.
//!
//! # What the verdict says, and what it must not be inferred from
//!
//! REQ-619's verify found four defects that share one shape: a consumer, or an
//! arm of this grammar, **inferring** a fact instead of reading one.
//!
//! - [`Verdict::sources`] was read as "the touch was in-root" by both consumers.
//!   It is an accumulator over every token, so `cat ~/.ssh/id_rsa README.md`
//!   filled it while touching a key outside the root. The verdict now carries
//!   [`Verdict::out_of_root_touch`] and says so (C2).
//! - The per-segment boundary flag was a local, so an `Unknown` returned later
//!   in the *same* segment discarded it. It is now [`BoundaryEvidence`], shared
//!   with the caller, and the "a boundary touch outranks an unknown" rule holds
//!   within a segment as well as between segments (H1).
//! - A verb's **basename** opened the permissive tables, so `bin/ls` inherited
//!   `ls`'s reach. A verb naming a path is now `Unknown`; the basename strip
//!   survives for the [`OPAQUE`] denylist, where it can only tighten (H2).
//! - A mint failure was read as "this is the root" ([`resolve_token`]) and as
//!   "nothing to see" (`walk::visit`'s skip, which
//!   [`subtree_is_boundary_free`] concluded a clean subtree from). Both now
//!   fail closed on anything but [`ProvenanceError::Empty`] (C3).
//!
//! The out-of-root arm also matches a path's `~/…` spelling when it lies under
//! the user's home, so a boundary glob written the way REQ-619 taught the
//! daemon to *mint* reaches a file a shell command named (m2).
//!
//! # One widening, and the order it depends on (REQ-620)
//!
//! [`UNMODELLED`] refuses every command containing `>` or `<`, and that rule
//! was written for commands a user types. On 2026-09-09 the first shell call a
//! remote model made in an `/analyze` turn was six `ls` calls, an `echo` and a
//! `which` — nothing outside the root but a `~/bin` probe — and it pinned the
//! session for the rest of its life on a `2>&1`. Nothing in that command could
//! have read a protected byte, and a model that has never seen this grammar
//! cannot avoid writing the form (REQ-620 Description).
//!
//! So [`classify_with_budget`] now runs
//! [`super::shell_syntax::strip_null_redirects`] **before** the [`UNMODELLED`]
//! scan and **before** the segment split (ADR-620-2 steps 1–2). Only the
//! `NullRedirect` entity's forms are lifted, as whole words; every other use of
//! `>` and `<` refuses the whole command exactly as REQ-614 left it (BR-2).
//! The order is not a preference:
//!
//! - Stripping *after* the unmodelled scan cannot work — the scan refuses on
//!   the `>` the strip exists to remove.
//! - Stripping *after* the split cannot work — `2>&1` and `&>/dev/null` contain
//!   `&`, which the splitter reads as a segment separator, so it would hand
//!   `2>` to [`classify_segment`] as a verb.
//!
//! The widening adds no reach in either direction. A lifted redirect
//! contributes no path token, so `/dev/null` never reaches [`resolve_token`]
//! and never matches a boundary glob (BR-3); and it removes none, so
//! `cat .env 2>/dev/null` is the `BoundaryTouch` it always was (BR-8) and
//! `python x.py 2>/dev/null` is the opaque-verb `Unknown` it always was (BR-5).
//! The verdict is still a function of the command's text alone (BR-10) — the
//! strip is a pure string transform and no redirect's *effect* is consulted.
//!
//! # Mutation record (conventions.md — show the test can fail)
//!
//! Inverting the fallthrough in [`classify_segment`] so an unrecognised verb
//! yields `Rooted` turns **exactly one** test red:
//! [`tests::an_unrecognised_verb_is_unknown_not_rooted`].
//!
//! That number was first written as "9, including every adversarial spelling",
//! and it was wrong — the mutation was run and **nothing failed**. Every
//! spelling in [`tests::adversarial_spellings_are_all_unknown`] is caught by
//! the opaque table or by [`UNMODELLED`] before the fallthrough is reached, so
//! the line that makes this an allowlist had no test at all. The verbs that
//! reach it are ordinary file-reading programs in no table — `base64`,
//! `strings`, `dd`, `tar` — and the denylist reading of BR-1(e) would pass
//! every one of them. This paragraph is left in as the worked example of why
//! conventions.md requires the mutation to be *run* rather than reasoned about
//! (LESSON-569, LESSON-598).
//!
//! Deleting the `truncated_by` check in [`subtree_is_boundary_free`] turns
//! [`tests::a_truncated_scan_is_unknown_never_rooted`] red and nothing else.
//! Building the scan from `WalkPolicy::default()` turns
//! [`tests::the_scan_does_not_inherit_the_discovery_walks_skip_set`] and
//! [`tests::a_truncated_scan_is_unknown_never_rooted`] red — two, because the
//! default budget also stops the starved-scan fixture from truncating.
//!
//! REQ-620's strip was mutated four ways and the counts live with the
//! recogniser ([`super::shell_syntax`]'s module docs), because that is the
//! module that owns the rule. Two results belong here. Making
//! [`super::shell_syntax::strip_null_redirects`] a no-op reds **six** tests in
//! this module and leaves [`tests::every_other_redirect_stays_unmodelled`]
//! green — correctly, since a no-op preserves exactly the refusal that test
//! asserts, and a widening that broke BR-2 would have to be a different
//! mutation. That different mutation is the third: lifting any word carrying
//! `>` or `<` reds `every_other_redirect_stays_unmodelled` *and* REQ-614's own
//! [`tests::adversarial_spellings_are_all_unknown`], on `cat <src/main.rs`.
//! One widening, both suites — which is the evidence that the REQ-614 grammar
//! is still the thing being widened rather than replaced.

use std::collections::BTreeSet;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::Duration;

use teton_core::boundary::BoundaryMatcher;
use teton_core::entities::PrivacyBoundary;
use teton_core::provenance_id::{ProvenanceError, ProvenanceId};
use teton_protocol::methods::RootKind;

use super::shell_syntax::strip_null_redirects;
use super::walk::{self, WalkBudget, WalkPolicy};
use super::{canonical_through_existing_ancestor, lexical_normalize, under_denied_prefix};

/// The scan budget, and deliberately far below [`WalkBudget::default`].
///
/// This walk runs synchronously in `ShellTool::run`, before the command is
/// spawned, and it runs with **no** name-based pruning
/// ([`WalkPolicy::for_boundary_scan`]) — so in a repository with a large build
/// tree it will exhaust the budget and the verdict will be `Unknown`. That is
/// the intended balance (REQ-614 OQ-3: ship the strict form and measure how
/// often the lift is typed before widening), not an oversight: `ls`,
/// `git status` and explicit-file reads — the overwhelming majority of shell
/// calls — never reach the scan at all.
const SCAN_BUDGET: WalkBudget = WalkBudget {
    max_entries: 20_000,
    max_wall: Duration::from_millis(1_500),
};

/// What the daemon could prove about a command's reach.
///
/// `pub` rather than `pub(crate)` since REQ-619: `skills::dynamic::run_all` is
/// `pub` (the `skills` module is reached from outside the harness) and returns
/// a `PreambleRun` carrying one of these, so the type has to be as reachable as
/// the function that hands it out. The grammar itself is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerdictKind {
    /// Every token was recognised and every path it names resolved under the
    /// session root without touching a boundary.
    Rooted,
    /// A path the command names matches a `local-only` boundary glob. Pins the
    /// session permanently (REQ-614 BR-3) — there is no lift for this.
    BoundaryTouch,
    /// The classifier could not prove the command's reach. Fail-closed, exactly
    /// as every `shell` result was before REQ-614.
    Unknown,
}

/// A classification of one `shell` invocation — or, since REQ-619, of one
/// skill preamble command.
///
/// `pub` for the reason [`VerdictKind`] is: it rides out of
/// `skills::dynamic::run_all` on a `PreambleRun`.
#[derive(Debug, Clone)]
pub struct Verdict {
    /// What was proved.
    pub kind: VerdictKind,
    /// The repo-relative canonical ids of every path argument that resolved
    /// inside the session root — including an **in-root** boundary path, which
    /// mints and is matched by the glob that protects it.
    ///
    /// It is the accumulator over the whole command, so it is **not** a proxy
    /// for "the touch was in-root": `cat ~/.ssh/id_rsa README.md` fills it with
    /// `README.md` while touching a boundary outside the root. Read
    /// [`Self::out_of_root_touch`] for that question (REQ-619 verify, C2).
    pub sources: BTreeSet<ProvenanceId>,
    /// Some path argument matched a boundary glob and resolved **outside** the
    /// session root, so no id in [`Self::sources`] names it.
    ///
    /// This is the fact `ToolProvenance::BoundaryTouch` exists to carry
    /// (ADR-614-3, LESSON-623), stated explicitly rather than inferred from an
    /// empty source set. Both consumers used to read `sources.is_empty()` as
    /// the proxy, and the proxy is wrong for exactly one shape — a command that
    /// names an out-of-root boundary file *and* an ordinary in-root one. That
    /// command reported `Sources({README.md})`, a clean liftable provenance,
    /// for a command that read a private key (REQ-619 verify, C2).
    ///
    /// `false` for a purely in-root touch, which needs no bit: its id is in
    /// `sources`, the glob matches it, and egress blocks naming the file.
    pub out_of_root_touch: bool,
    /// Why this verdict was reached. `&'static str`, so it cannot carry command
    /// text or file content (see the module docs).
    ///
    /// Rendered on the daemon's stderr by `ShellTool::run` for any non-`Rooted`
    /// verdict — the answer to "why did *that* command pin my session". It is a
    /// sentence rather than a bare discriminant for that reason, and a
    /// `&'static str` so logging it cannot leak what the command said.
    pub reason: &'static str,
}

impl Verdict {
    fn unknown(reason: &'static str) -> Self {
        Self {
            kind: VerdictKind::Unknown,
            sources: BTreeSet::new(),
            out_of_root_touch: false,
            reason,
        }
    }

    /// The verdict for a command the daemon **did not classify at all**
    /// (REQ-619 verify, m3).
    ///
    /// `Unknown`, and content-free like every other verdict here: "not asked"
    /// and "asked, and could not prove it" must be indistinguishable
    /// downstream, because the only honest thing to say about an unclassified
    /// command is that its reach is unknown. It is a named constructor rather
    /// than a raw [`Self::unknown`] call so the one legitimate synthesis site
    /// is visible — `skills::dynamic::run_all_with`, for a command the
    /// invocation's whole-run budget stopped before it started — and a second
    /// one has to be added here to exist.
    ///
    /// The fold ignores a `NotRun` command's verdict entirely (BR-2), so this
    /// value reaches no provenance decision. It exists because `PreambleRun`
    /// carries a verdict for every command and a half-record would be worse
    /// than a conservative one.
    pub(crate) fn not_classified() -> Self {
        Self::unknown("the invocation's budget was spent before this command was classified")
    }

    /// A boundary touch, carrying whatever the tokens proved: the in-root ids
    /// in `sources`, and whether any matched path lay outside the root.
    ///
    /// One constructor for both places a `BoundaryTouch` is returned (the
    /// unknown-after-a-boundary arm and the end of the loop), so the two cannot
    /// come to disagree about which evidence rides along.
    fn boundary_touch(sources: BTreeSet<ProvenanceId>, evidence: &BoundaryEvidence) -> Self {
        Self {
            kind: VerdictKind::BoundaryTouch,
            sources,
            out_of_root_touch: evidence.out_of_root,
            reason: "a path argument matches a privacy boundary",
        }
    }
}

/// What the tokens seen so far proved about a boundary — the state that used to
/// be a bare `saw_boundary` local in each of [`classify`] and
/// [`classify_segment`] (REQ-619 verify, C2 and H1).
///
/// Two changes ride on making it a value the segment classifier **shares with
/// its caller** rather than recomputes:
///
/// - `out_of_root` is the evidence [`Verdict::out_of_root_touch`] carries, and
///   it can only be observed in the token loop that matched the glob.
/// - `any` is now set the moment a token matches, so an `Unknown` returned
///   *later in the same segment* — a denied prefix, a dirty subtree, an
///   unresolvable token — no longer discards it. Before, the segment-level
///   precedence rule ("a boundary touch outranks an unknown") held between
///   segments and silently failed within one: `cat ~/.ssh/id_rsa /tmp/x` was
///   `Unknown`, which `/shell allow` lifts.
///
/// Carrying `any` across segments is deliberate and cannot loosen a verdict:
/// the caller already answers `BoundaryTouch` for the whole command once any
/// segment touched, so a later segment that short-circuits on it only skips
/// work whose answer could not have changed the result.
/// The environment one segment is classified in: the session root, the compiled
/// glob set, the directories no tool may read, the scan budget, and the user's
/// home.
///
/// One named value rather than five positional parameters, which is what
/// `suppression_ratchet`'s rule asks for — a `too_many_arguments` suppression
/// is an unnamed parameter cluster, and this cluster has a name: it is the four
/// inputs `ShellTool::run` hands [`classify`], plus the home
/// [`classify`] resolves once (REQ-619 verify, m2). Every field is read-only
/// for the whole classification; the two values that *accumulate* travel
/// separately, as `&mut`, so the difference is visible in the signature.
struct Scope<'a> {
    root: &'a Path,
    matcher: &'a BoundaryMatcher<'a>,
    denied_prefixes: &'a [PathBuf],
    budget: WalkBudget,
    /// `None` when `$HOME` is unset or unresolvable: a `~/` token is then
    /// unresolvable and no home-relative spelling is tried.
    home: Option<&'a Path>,
}

#[derive(Debug, Default)]
struct BoundaryEvidence {
    /// Some path argument matched a boundary glob.
    any: bool,
    /// Some path argument matched a boundary glob **outside** the session root.
    out_of_root: bool,
}

/// Verbs that read no file at all. A timeout on one of these is still `Rooted`
/// (REQ-614 AC-4).
const READS_NOTHING: &[&str] = &[
    "pwd", "sleep", "true", "false", "date", "whoami", "uname", "hostname", "id", "echo", "printf",
];

/// Verbs that surface **names**, not file contents. Listing a name is not
/// reading a file, so these pass BR-1(d) without a subtree scan.
///
/// `test` is here because it only ever `stat`s its operands — `-s`, `-f`,
/// `-d` and the rest answer questions about a name, never about content — and
/// because the ADLC toolkit's ethos preamble is written as
/// `test -s .adlc/ETHOS.md && cat .adlc/ETHOS.md || cat ~/.claude/skills/ETHOS.md`
/// (BUG-218): the `test` is what keeps an empty project copy from swallowing
/// the ethos, and without it here the guard alone made every toolkit skill
/// `Unknown`. Its bracket spelling `[` stays unmodelled; see [`UNMODELLED`].
const NAME_ONLY: &[&str] = &[
    "ls", "find", "du", "basename", "dirname", "stat", "file", "which", "test",
];

/// Verbs that can read file *contents*. Given a directory, a wildcard, or no
/// path at all, these require the BR-1(d) subtree scan.
const READS_CONTENT: &[&str] = &[
    "cat", "head", "tail", "grep", "egrep", "fgrep", "sed", "awk", "less", "more", "diff", "wc",
    "sort", "uniq", "cut", "md5", "shasum", "nl", "tr",
];

/// Interpreters, build tools and network clients: a command whose reach is the
/// whole machine (REQ-614 BR-1(e)).
///
/// Pinned as one table with a test that enumerates it (AC-9). This is **not**
/// the mechanism that makes the classifier safe — an unrecognised verb is
/// already `Unknown` by [`classify_segment`]'s fallthrough — it is a statement
/// of intent, so a later author who adds `cargo` to [`NAME_ONLY`] has to delete
/// a line that says why it is here.
const OPAQUE: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "ksh",
    "dash",
    "fish",
    "env",
    "eval",
    "exec",
    "source",
    "python",
    "python3",
    "node",
    "deno",
    "bun",
    "ruby",
    "perl",
    "php",
    "cargo",
    "rustc",
    "npm",
    "npx",
    "yarn",
    "pnpm",
    "make",
    "cmake",
    "gradle",
    "mvn",
    "go",
    "curl",
    "wget",
    "ssh",
    "scp",
    "rsync",
    "nc",
    "netcat",
    "telnet",
    "xargs",
    "sudo",
    "doas",
    "docker",
    "kubectl",
    "git-crypt",
];

/// `git` subcommands that surface names or metadata only. Anything else under
/// `git` — `show`, `diff`, `cat-file` — reads content and is treated as such.
const GIT_NAME_ONLY: &[&str] = &[
    "status",
    "log",
    "branch",
    "remote",
    "tag",
    "rev-parse",
    "diff-tree",
    // `worktree list` and `for-each-ref` print ref names, paths and commit
    // metadata — the same reach `log` and `branch` already have; neither can
    // print a tracked file's content.
    "worktree",
    "for-each-ref",
];

/// Characters whose presence means `sh` will do something this grammar does not
/// model: quoting, redirection, substitution, expansion, escaping.
///
/// Rejecting the whole command on any of them is what keeps the tokenizer
/// honest. A grammar that tried to *handle* quoting would be a half-written
/// shell lexer, which is how a matcher starts guessing (the argument
/// [`super::shell::is_env_assignment`] already makes, one step further).
const UNMODELLED: &[char] = &[
    '\'', '"', '`', '$', '\\', '>', '<', '{', '}', '!', '*', '?', '[',
];

/// Classify one `shell` invocation.
///
/// Takes **no exit status and no output** (REQ-614 BR-8): a `pwd` that timed
/// out is `Rooted` and a `curl` that failed is `Unknown`, and that is enforced
/// by the signature rather than by a branch nobody may add.
pub(crate) fn classify(
    root: &Path,
    root_kind: RootKind,
    boundaries: &[PrivacyBoundary],
    denied_prefixes: Vec<PathBuf>,
    command: &str,
) -> Verdict {
    classify_with_budget(
        root,
        root_kind,
        boundaries,
        denied_prefixes,
        command,
        SCAN_BUDGET,
        // Canonicalized once, here, because every comparison below is against a
        // path `canonical_through_existing_ancestor` produced: on macOS a home
        // reached through `/var` would never `strip_prefix` a resolved
        // `/private/var/…`, and the `~/…` spelling m2 adds would silently never
        // match. Falls back to the raw value when the home does not resolve —
        // the `~/` expansion is no worse off than it was.
        crate::session_root::home().map(|home| home.canonicalize().unwrap_or(home)),
    )
}

/// [`classify`] with the scan budget exposed — the seam the truncation test
/// needs.
///
/// The budget is a parameter rather than a constant read inside the walk
/// because ADR-614-5's fail-closed rule is otherwise **untestable**: a fixture
/// large enough to exhaust the production budget is slow and fragile, and a
/// test that starves a walk it built itself asserts a property of `walk::visit`
/// rather than of this module. That distinction is not academic — the first
/// draft of [`tests::a_truncated_scan_is_unknown_never_rooted`] did exactly
/// that, and deleting the `truncated_by` check left it green (LESSON-569:
/// verify the failure *mechanism* before building a fixture around it).
///
/// `home` is a parameter for the same reason (REQ-619 verify, m2). The grammar
/// needs the user's home twice — to expand a `~/` token, and to try a resolved
/// path's home-relative spelling against the globs — and reading `$HOME` inside
/// the grammar would make both untestable without mutating the test process's
/// environment, which is shared by every other test in the binary.
/// [`classify`] reads it once and hands it down, so everything below this line
/// is a function of its arguments.
fn classify_with_budget(
    root: &Path,
    root_kind: RootKind,
    boundaries: &[PrivacyBoundary],
    denied_prefixes: Vec<PathBuf>,
    command: &str,
    budget: WalkBudget,
    home: Option<PathBuf>,
) -> Verdict {
    // BR-9, first and before anything that touches the filesystem: with no
    // boundary configured there is nothing to protect, and the verdict is the
    // pre-REQ-614 one so that "nothing changes from today" is true by
    // construction rather than by argument.
    if boundaries.is_empty() {
        return Verdict::unknown("no privacy boundary is configured");
    }

    // ADR-614-2 (OQ-1 resolved: yes). A home or filesystem root's subtree holds
    // `**/.ssh/**` and `**/.aws/**`, so every content-reading verb there is
    // already caught by the subtree rule; what this buys is the *name-only*
    // verbs, and a home-directory listing is not worth defending as provably
    // in-reach.
    if root_kind != RootKind::Project {
        return Verdict::unknown("the session root is not a project");
    }

    // REQ-620 ADR-620-2, steps 1 and 2. The strip is **first**, and the order
    // is the whole correctness argument: stripping after the unmodelled scan
    // cannot work (the scan refuses on `>`), and stripping after the split
    // cannot work (the `&` in `2>&1` is a separator, so a splitter that saw it
    // first would hand `2>` to `classify_segment` as a verb and `1` to the next
    // segment as one). Everything past this line reads the residue and is
    // exactly the REQ-614 grammar.
    let stripped = strip_null_redirects(command);
    let command = stripped.residue.as_str();

    if command.chars().any(|c| UNMODELLED.contains(&c)) {
        return Verdict::unknown("the command uses shell syntax this classifier does not model");
    }

    let matcher = match BoundaryMatcher::new(boundaries) {
        Ok(m) => m,
        // A boundary set that does not compile is the fail-closed case
        // `context_taint_cause` already treats as sensitive.
        Err(_) => return Verdict::unknown("the configured boundary set does not compile"),
    };

    let scope = Scope {
        root,
        matcher: &matcher,
        denied_prefixes: &denied_prefixes,
        budget,
        home: home.as_deref(),
    };
    let mut sources = BTreeSet::new();
    let mut evidence = BoundaryEvidence::default();

    for segment in command.split(['|', ';', '&', '(', ')', '\n']) {
        if segment.trim().is_empty() {
            continue;
        }
        match classify_segment(&scope, segment, &mut sources, &mut evidence) {
            SegmentVerdict::Rooted => {}
            // `evidence.any` is already set by the token that matched; the arm
            // is kept so the three outcomes stay enumerated at the caller.
            SegmentVerdict::BoundaryTouch => evidence.any = true,
            SegmentVerdict::Unknown(reason) => {
                // A boundary touch outranks an unknown: it is the more severe
                // consequence (permanent, unliftable), so a command that both
                // names a protected file and does something opaque must pin
                // permanently rather than liftably.
                //
                // Since REQ-619's verify this also catches the case where the
                // unknown and the boundary are in the *same* segment (H1) —
                // `evidence` is shared with `classify_segment`, so a token that
                // matched is remembered even when a later token in that segment
                // returns first.
                if evidence.any {
                    return Verdict::boundary_touch(sources, &evidence);
                }
                return Verdict::unknown(reason);
            }
        }
    }

    if evidence.any {
        // `sources` holds the ids of every **in-root** path the command named,
        // boundary or not; `evidence.out_of_root` says whether a matched path
        // lay outside the root, where no id exists. The consumers read the flag
        // rather than `sources.is_empty()`, which is only the same question
        // when the command named nothing else (REQ-619 verify, C2).
        return Verdict::boundary_touch(sources, &evidence);
    }
    Verdict {
        kind: VerdictKind::Rooted,
        sources,
        out_of_root_touch: false,
        reason: "every path the command names resolved inside the session root",
    }
}

enum SegmentVerdict {
    Rooted,
    BoundaryTouch,
    Unknown(&'static str),
}

/// One `|`/`;`/`&`-separated segment.
///
/// **The fallthrough is `Unknown`** — see the module docs. Every verb this does
/// not recognise, every path form it cannot resolve, every scan that ran out of
/// budget lands here.
fn classify_segment(
    scope: &Scope<'_>,
    segment: &str,
    sources: &mut BTreeSet<ProvenanceId>,
    evidence: &mut BoundaryEvidence,
) -> SegmentVerdict {
    let mut words = segment.split_whitespace();
    let Some(raw_verb) = words.next() else {
        return SegmentVerdict::Rooted;
    };
    // An `=` anywhere in a word is an environment assignment (or something
    // stranger). Either way it can change what the command does — `IFS=`,
    // `LD_PRELOAD=` — and modelling that is out of this grammar's reach.
    if segment.split_whitespace().any(|w| w.contains('=')) {
        return SegmentVerdict::Unknown("the command sets an environment variable");
    }
    // The basename, and **only** for the denylist below. A verb naming a path
    // is a different program from the one that name resolves to on `PATH`.
    let verb = raw_verb.rsplit('/').next().unwrap_or(raw_verb);

    if OPAQUE.contains(&verb) {
        return SegmentVerdict::Unknown(
            "the command runs an interpreter, build tool or network client",
        );
    }

    // REQ-619 verify, H2. Past this line the tables are **permissive** — they
    // are what makes a command `Rooted` — and a basename may not open them.
    // `bin/ls`, `./cat` and `tools/git` are repository-local executables whose
    // contents this daemon has not read; matching them against `ls`, `cat` and
    // `git` let a planted file inherit an allowlisted verb's reach, which is
    // the "false negative costs a leak" polarity the module docs open with.
    //
    // The denylist keeps the basename strip on purpose: there, reading
    // `/usr/bin/python3` as `python3` makes the answer *stricter*, and a
    // spelling it misses lands on the fallthrough's `Unknown` anyway. Widening
    // and narrowing are not symmetric here, so the two lookups do not share a
    // rule.
    if raw_verb.contains('/') {
        return SegmentVerdict::Unknown("the command names its program by path");
    }

    let rest: Vec<&str> = words.collect();

    // `find ... -exec` runs an arbitrary program (BR-1(e)); plain `find` lists
    // names.
    if verb == "find"
        && rest
            .iter()
            .any(|w| *w == "-exec" || *w == "-execdir" || *w == "-ok")
    {
        return SegmentVerdict::Unknown("the command runs `find -exec`");
    }

    let reads_content = if verb == "git" {
        match rest.first() {
            Some(sub) if GIT_NAME_ONLY.contains(sub) => false,
            // `git show`, `git diff`, `git cat-file` read content; any other
            // subcommand is one this table does not know.
            _ => return SegmentVerdict::Unknown("the `git` subcommand is not a name-only one"),
        }
    } else if READS_NOTHING.contains(&verb) {
        // Reads nothing at all: no path arguments to resolve, no scan.
        return SegmentVerdict::Rooted;
    } else if NAME_ONLY.contains(&verb) {
        false
    } else if READS_CONTENT.contains(&verb) {
        true
    } else {
        return SegmentVerdict::Unknown("the command's verb is not one this classifier recognises");
    };

    // Path tokens: every word that is not a flag. A flag's *argument* is
    // indistinguishable from a path without a per-verb option table, so a word
    // following a flag is resolved as a path too — resolving a non-path as a
    // path can only add a boundary match or an unresolvable token, both of
    // which fail closed.
    let paths: Vec<&str> = rest
        .iter()
        .copied()
        .filter(|w| !w.starts_with('-'))
        .collect();

    let mut saw_directory = false;
    // Whether any token named an **existing regular file**.
    //
    // This is what closes the `grep -r foo` leak. That command names one token,
    // `foo`, which is a *pattern* and not a path; the first draft resolved it
    // to a nonexistent file under the root, found `paths` non-empty and
    // therefore skipped BR-1(d)'s root scan, and returned `Rooted`. GNU grep's
    // `-r` with no path searches `.` recursively, so a repository holding a
    // `.env` would have had its contents read and sent under a clean
    // provenance.
    //
    // Telling a pattern from a path needs a per-verb option table, which is the
    // second parser ADR-614-1 refuses. This needs no table: **a content verb
    // given at least one existing file was given explicit files**, so the reach
    // is those files; given none, the reach is whatever the verb defaults to —
    // the root — and the subtree decides.
    //
    // `head -n 5 src/main.rs` is why the rule is "at least one" rather than
    // "all": `5` is a flag's argument and resolves to nothing, and a rule
    // demanding every token resolve would make every flag-carrying read
    // unknown. Measured on a 192,000-file repository, that cost `grep` 123ms
    // and an unknown verdict where OQ-3 expected an explicit-file read to stay
    // rooted.
    let mut named_an_existing_file = false;

    for token in &paths {
        let resolved = resolve_token(scope.root, scope.home, token);
        // REQ-611 BR-8 / ADR-7, and the regression the transcript suite caught:
        // a denied prefix is **not** a privacy boundary — it is a directory no
        // tool may read at all — so it has no glob for the matcher above to
        // find. Before REQ-614 a `shell` reading one was held at egress by the
        // constant `Unknown`, which was the whole of `shell`'s standing as "the
        // named exception, fail-closed at egress like every other file on the
        // machine". Narrowing the verdict without this check handed that file a
        // clean `Rooted` provenance and let it egress.
        //
        // `Unknown` rather than `BoundaryTouch`: no boundary was crossed, and a
        // pin that claimed one would be a false sentence to the user. This is
        // exactly the pre-REQ-614 answer for exactly the pre-REQ-614 reason.
        if let Resolved::InsideRoot(abs, _)
        | Resolved::RootItself(abs)
        | Resolved::OutsideRoot(abs) = &resolved
        {
            if under_denied_prefix(scope.denied_prefixes, abs) {
                return SegmentVerdict::Unknown(
                    "a path argument is inside a directory tools may not read",
                );
            }
        }
        match resolved {
            Resolved::InsideRoot(abs, id) => {
                if scope.matcher.match_path(id.as_str()).is_some() {
                    // In-root: `evidence.out_of_root` stays as it is, because
                    // this touch *is* nameable and the id below carries it.
                    evidence.any = true;
                    // Keep the id. An **in-root** boundary path mints a real
                    // `ProvenanceId`, so the tool reports it as `Sources` and
                    // egress blocks naming the actual file — exactly what a
                    // `read` of it does, with no new machinery and a better
                    // event than a sentinel. `ToolProvenance::BoundaryTouch`
                    // exists only for the out-of-root case, where there is no
                    // id for a glob to match (ADR-614-3, LESSON-623).
                    sources.insert(id);
                    continue;
                }
                if abs.is_dir() {
                    saw_directory = true;
                    if reads_content
                        && !subtree_is_boundary_free(
                            &abs,
                            scope.denied_prefixes,
                            scope.matcher,
                            scope.budget,
                        )
                    {
                        return SegmentVerdict::Unknown(
                            "a directory the command reads could hold a protected file",
                        );
                    }
                } else if abs.is_file() {
                    named_an_existing_file = true;
                    sources.insert(id);
                }
                // Neither a file nor a directory: a pattern, a flag's argument,
                // or a path that does not exist. It contributes no provenance
                // and no evidence about the verb's reach — deliberately not an
                // `else` branch, because there is nothing to record.
            }
            Resolved::RootItself(abs) => {
                saw_directory = true;
                if reads_content
                    && !subtree_is_boundary_free(
                        &abs,
                        scope.denied_prefixes,
                        scope.matcher,
                        scope.budget,
                    )
                {
                    return SegmentVerdict::Unknown(
                        "a directory the command reads could hold a protected file",
                    );
                }
            }
            Resolved::OutsideRoot(abs) => {
                // LESSON-623: a path outside the root receives no
                // `ProvenanceId`, so no glob can match it through the ordinary
                // identity path. The boundary globs are matched against the
                // resolved absolute path with its leading `/` stripped, which is
                // what lets `**/.ssh/**` reach `Users/x/.ssh/config` (AC-5) —
                // and, when the path is under the user's home, against its
                // `~/…` spelling as well.
                //
                // Two spellings because two vocabularies name the same file
                // (REQ-619 verify, m2). The builtins are `**/`-prefixed and
                // reach the stripped absolute form; a user glob written in the
                // spelling REQ-619 taught the daemon to mint —
                // `~/.claude/skills/**` — reaches only the home-relative one.
                // Trying both can only *add* matches, and every added match is
                // a refusal, so the direction is the safe one.
                let spelling = abs.to_string_lossy();
                let stripped = spelling.strip_prefix('/').unwrap_or(&spelling);
                let home_spelling = scope
                    .home
                    .and_then(|home| abs.strip_prefix(home).ok())
                    .map(|rel| rel.to_string_lossy().replace('\\', "/"))
                    .filter(|rel| !rel.is_empty())
                    .map(|rel| format!("~/{rel}"));
                let matched = scope.matcher.match_path(stripped).is_some()
                    || home_spelling
                        .as_deref()
                        .is_some_and(|spelling| scope.matcher.match_path(spelling).is_some());
                if matched {
                    evidence.any = true;
                    // The bit C2 turns on: this touch has no id, so nothing in
                    // `sources` names it and no consumer may infer it from that
                    // set being empty.
                    evidence.out_of_root = true;
                } else {
                    return SegmentVerdict::Unknown(
                        "a path argument resolves outside the session root",
                    );
                }
            }
            Resolved::Unresolvable => {
                return SegmentVerdict::Unknown("a path argument could not be resolved");
            }
        }
    }

    if evidence.any {
        return SegmentVerdict::BoundaryTouch;
    }

    // BR-1(d): a content-reading verb given no path at all reads whatever is
    // under the root.
    // BR-1(d): a content-reading verb reads the root when it names no path at
    // all, and may read it when none of its tokens named an existing file (see
    // `named_an_existing_file` above). Either way the root's subtree decides.
    if reads_content
        && (paths.is_empty() || !named_an_existing_file)
        && !saw_directory
        && !subtree_is_boundary_free(
            scope.root,
            scope.denied_prefixes,
            scope.matcher,
            scope.budget,
        )
    {
        return SegmentVerdict::Unknown(
            "the command reads the root and it could hold a protected file",
        );
    }

    SegmentVerdict::Rooted
}

enum Resolved {
    InsideRoot(PathBuf, ProvenanceId),
    /// The root itself, or `.` — inside the root but naming no file under it,
    /// so `ProvenanceId::from_resolved` mints nothing. Always a directory read.
    RootItself(PathBuf),
    OutsideRoot(PathBuf),
    Unresolvable,
}

fn resolve_token(root: &Path, home: Option<&Path>, token: &str) -> Resolved {
    let joined = if let Some(tail) = token.strip_prefix("~/") {
        match home {
            Some(home) => home.join(tail),
            None => return Resolved::Unresolvable,
        }
    } else if token == "~" {
        match home {
            Some(home) => home.to_path_buf(),
            None => return Resolved::Unresolvable,
        }
    } else if token.starts_with('~') {
        // `~user` needs the password database; not modelled.
        return Resolved::Unresolvable;
    } else if Path::new(token).is_absolute() {
        PathBuf::from(token)
    } else {
        root.join(token)
    };

    let normalized = lexical_normalize(&joined);
    let Some(checked) = canonical_through_existing_ancestor(&normalized) else {
        return Resolved::Unresolvable;
    };
    let Ok(canonical_root) = root.canonicalize() else {
        return Resolved::Unresolvable;
    };
    if !checked.starts_with(&canonical_root) {
        return Resolved::OutsideRoot(checked);
    }
    match ProvenanceId::from_resolved(&canonical_root, &checked) {
        Ok(id) => Resolved::InsideRoot(checked, id),
        // `Empty` is the one refusal that means "this *is* the root" — the
        // remainder after the strip was nothing — and that is a directory read,
        // not a failure.
        Err(ProvenanceError::Empty) => Resolved::RootItself(checked),
        // Every other refusal is a path under the root that the daemon cannot
        // name, and `Resolved::RootItself` is the wrong answer for it twice
        // over: it claims the token is the root (so a *file* is scanned as a
        // directory, which finds nothing) and it lands on `Rooted`.
        // `<root>/~/.env` is the reachable case — `ProvenanceError::ReservedScope`
        // since TASK-398 — and `cat ./~/.env` came back `Rooted` with a clean
        // provenance (REQ-619 verify, C3). A token with no identity is exactly
        // what `Unresolvable` is for.
        Err(_) => Resolved::Unresolvable,
    }
}

/// Whether **no** file under `dir` matches a boundary glob.
///
/// ADR-614-5: a scan that exhausts its budget has not shown the absence of a
/// boundary file — it stopped looking — so it answers `false`, the same as a
/// scan that found one. The two are not distinguished because the caller does
/// the same thing with both, and giving them separate values would invite a
/// later author to treat "stopped looking" as "found nothing".
///
/// An entry the walk could not **name** is the third member of that family
/// (REQ-619 verify, C3). The matcher runs on a minted id, so a file with no id
/// is a file no glob was ever run against: `<root>/~/.env` was skipped by
/// `walk::visit`'s mint-failure arm and this function answered *boundary-free*
/// for a tree holding a `.env`, which made `grep -r foo .` `Rooted`. It reads
/// [`walk::WalkReport::unmintable`] and fails closed on it, exactly as it does
/// on `truncated_by` and for the same reason: the walk did not look at that
/// file, so nothing here may claim it is clean.
fn subtree_is_boundary_free(
    dir: &Path,
    denied_prefixes: &[PathBuf],
    matcher: &BoundaryMatcher<'_>,
    budget: WalkBudget,
) -> bool {
    let policy = WalkPolicy::for_boundary_scan(budget, denied_prefixes.to_vec());
    let mut hit = false;
    let report = walk::visit(
        dir,
        RootKind::Project,
        &[],
        &policy,
        &mut |_path, file_type, id| {
            if file_type.is_dir() {
                return ControlFlow::Continue(());
            }
            if matcher.match_path(id.as_str()).is_some() {
                hit = true;
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        },
    );
    !hit && report.truncated_by.is_none() && report.unmintable == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use teton_core::config::DEFAULT_BOUNDARIES;
    use teton_core::entities::{BoundaryMode, PrivacyBoundary};

    /// A **project** root — ADR-614-2 makes `RootKind::Project` a precondition
    /// for `Rooted`, so a bare temp dir would classify everything `Unknown` and
    /// every benign-path assertion below would pass for the wrong reason.
    fn project_root(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "teton-shellprov-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        dir
    }

    fn builtins() -> Vec<PrivacyBoundary> {
        DEFAULT_BOUNDARIES
            .iter()
            .map(|g| PrivacyBoundary::builtin(*g))
            .collect()
    }

    fn verdict(root: &Path, command: &str) -> Verdict {
        classify(root, RootKind::Project, &builtins(), Vec::new(), command)
    }

    /// A fixture HOME holding the two files the out-of-root cases name.
    ///
    /// The machine's real home is not usable for these: `~/.ssh/id_rsa` may or
    /// may not exist on the runner, and a test that skipped itself when it did
    /// not would be a test that never ran anywhere the CI matrix cares about.
    /// The home is a *parameter* of the classifier since this verify pass
    /// (`classify_with_budget`'s last argument), so planting one costs nothing
    /// and mutates no shared environment.
    fn fixture_home(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let home = std::env::temp_dir().join(format!(
            "teton-shellprov-home-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(home.join(".ssh/id_rsa"), "PRIVATE KEY\n").unwrap();
        std::fs::create_dir_all(home.join(".aws")).unwrap();
        std::fs::write(home.join(".aws/credentials"), "aws_secret=x\n").unwrap();
        // Canonical, because the classifier compares a *canonicalized* resolved
        // path against it and macOS's temp dir is a symlink (`/var` →
        // `/private/var`). `classify` canonicalizes the real `$HOME` for the
        // same reason.
        home.canonicalize().unwrap()
    }

    /// [`classify`] against a fixture home, an explicit boundary set and an
    /// explicit denial set — every input the grammar reads.
    fn verdict_full(
        root: &Path,
        home: Option<&Path>,
        boundaries: &[PrivacyBoundary],
        denied: Vec<PathBuf>,
        command: &str,
    ) -> Verdict {
        classify_with_budget(
            root,
            RootKind::Project,
            boundaries,
            denied,
            command,
            SCAN_BUDGET,
            home.map(Path::to_path_buf),
        )
    }

    /// [`verdict_full`] with the builtin boundaries and no denied prefix.
    fn verdict_with_home(root: &Path, home: &Path, command: &str) -> Verdict {
        verdict_full(root, Some(home), &builtins(), Vec::new(), command)
    }

    /// BUG-218 (adlc-toolkit): the ethos preamble every toolkit skill opens with
    /// is `test -s .adlc/ETHOS.md && cat .adlc/ETHOS.md || echo "No ethos …"`,
    /// and it must be `Rooted` — the `sh`-wrapped spelling it replaced was
    /// `Unknown` by verb and pinned a session on every typed skill. `test`
    /// only `stat`s its operand, so it is a name-only verb; `cat` names an
    /// existing in-root file; `echo` reads nothing.
    ///
    /// The toolkit's first draft kept `cat ~/.claude/skills/ETHOS.md` as the
    /// fallback, and the middle assertion is why it did not ship: a path
    /// outside the session root is one this grammar cannot prove, whatever it
    /// names, and the verdict says so. That is the REQ-614 design, not a gap —
    /// widening it to "files under the home that match no glob" would be a
    /// policy change (the `read` tool is root-jailed for the same reason), so
    /// the toolkit moved instead.
    ///
    /// The last half is the benign path's mirror: `test` on a protected name
    /// is still a boundary touch, exactly as `ls` on it is — being name-only
    /// does not exempt the operand from the globs.
    ///
    /// **Mutation (run, red, reverted):** drop `"test"` from [`NAME_ONLY`] —
    /// the first assertion reds with "the command's verb is not one this
    /// classifier recognises"; the other two stay green.
    #[test]
    fn the_toolkit_ethos_preamble_is_rooted_and_test_is_name_only() {
        let root = project_root("ethos");
        let home = fixture_home("ethos");
        std::fs::create_dir_all(root.join(".adlc")).unwrap();
        std::fs::write(root.join(".adlc/ETHOS.md"), "# project ethos\n").unwrap();
        std::fs::create_dir_all(home.join(".claude/skills")).unwrap();
        std::fs::write(home.join(".claude/skills/ETHOS.md"), "# toolkit ethos\n").unwrap();

        let v = verdict_with_home(
            &root,
            &home,
            "test -s .adlc/ETHOS.md && cat .adlc/ETHOS.md || echo No ethos found",
        );
        assert_eq!(v.kind, VerdictKind::Rooted, "{}", v.reason);
        assert!(!v.out_of_root_touch);

        let v = verdict_with_home(
            &root,
            &home,
            "test -s .adlc/ETHOS.md && cat .adlc/ETHOS.md || cat ~/.claude/skills/ETHOS.md",
        );
        assert_eq!(
            v.kind,
            VerdictKind::Unknown,
            "an out-of-root fallback is unprovable even when it names a real, unprotected file ({})",
            v.reason
        );
        assert_eq!(
            v.reason,
            "a path argument resolves outside the session root"
        );

        std::fs::write(root.join(".env"), "SECRET=1\n").unwrap();
        let v = verdict_with_home(&root, &home, "test -s .env && cat README.md");
        assert_eq!(
            v.kind,
            VerdictKind::BoundaryTouch,
            "a name-only verb on a protected name is still a touch ({})",
            v.reason
        );
    }

    /// Toolkit BUG-220 / teton-code BUG-219: the shapes every ADLC skill
    /// preamble was rewritten into must be `Rooted` against a project that has
    /// been `/init`ed, and the shapes they replaced must still be `Unknown` —
    /// the grammar rejects quoting and redirection before it reads the verb, so
    /// this is the contract the toolkit's `conventions.md` is written against.
    #[test]
    fn the_toolkit_preamble_shapes_are_rooted_and_the_old_ones_are_not() {
        let root = project_root("toolkit-preambles");
        std::fs::create_dir_all(root.join(".adlc/context")).unwrap();
        std::fs::create_dir_all(root.join(".adlc/specs/REQ-1-x")).unwrap();
        std::fs::write(root.join(".adlc/ETHOS.md"), "ethos\n").unwrap();
        std::fs::write(root.join(".adlc/context/architecture.md"), "arch\n").unwrap();
        std::fs::write(
            root.join(".adlc/specs/REQ-1-x/requirement.md"),
            "status: draft\n",
        )
        .unwrap();
        for rewritten in [
            "test -s .adlc/ETHOS.md && cat .adlc/ETHOS.md || echo No ethos found — run /init to vendor .adlc/ETHOS.md",
            "cat .adlc/context/architecture.md || echo No architecture context found",
            "grep -rl -e status:.draft -e status:.approved -e status:.in-progress --include requirement.md .adlc/specs || echo No active specs",
            "ls .adlc/specs/ || echo No specs found",
            "find .adlc/specs -name pipeline-state.json",
            "git branch --show-current || echo Not a git repo",
            "git diff-tree --stat -r main HEAD || echo No diff available",
            "git status --short",
            "git worktree list || echo Not a git repo",
            "git for-each-ref refs/remotes/origin/feat",
            "which gh && echo installed - auth and network checked at run || echo not installed — branch-only",
            "test -f .adlc/ETHOS.md && echo present || echo absent — run /init",
        ] {
            let v = verdict(&root, rewritten);
            assert_eq!(
                v.kind,
                VerdictKind::Rooted,
                "{rewritten:?} should be Rooted, got {:?} ({})",
                v.kind,
                v.reason
            );
        }
        for old in [
            r#"test -s .adlc/ETHOS.md && cat .adlc/ETHOS.md || echo "No ethos found — run /init to vendor .adlc/ETHOS.md""#,
            r#"cat .adlc/context/architecture.md 2>/dev/null || echo "No architecture context found""#,
            r#"grep -rl 'status: draft\|status: approved' .adlc/specs/*/requirement.md 2>/dev/null | head -20 || echo "No active specs""#,
            "git diff main --stat || echo No diff available",
            "cat .adlc/templates/task-template.md || cat ~/.claude/skills/templates/task-template.md || echo none",
        ] {
            let v = verdict(&root, old);
            assert_eq!(v.kind, VerdictKind::Unknown, "{old:?} should be Unknown");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// BR-1, benign path. The legitimate actor — the four commands AC-12 scripts
    /// plus an explicit file read — must NOT trip the classifier. A detector
    /// validated only against adversarial input ships broken and passes its own
    /// suite (LESSON-440).
    #[test]
    fn rooted_only_when_every_token_is_understood() {
        let root = project_root("rooted");
        for benign in [
            "pwd",
            "ls -la",
            "ls src",
            "git status",
            "git log -3",
            "git worktree list",
            "git for-each-ref refs/remotes/origin/feat",
            "cat src/main.rs",
            "wc -l src/main.rs",
            "sleep 60",
        ] {
            let v = verdict(&root, benign);
            assert_eq!(
                v.kind,
                VerdictKind::Rooted,
                "{benign:?} should be Rooted, got {:?} ({})",
                v.kind,
                v.reason
            );
        }
        // And the sources really are the resolved file, exactly as a `glob` over
        // the same path would report (BR-1's last sentence).
        let v = verdict(&root, "cat src/main.rs");
        assert_eq!(
            v.sources
                .iter()
                .map(ProvenanceId::as_str)
                .collect::<Vec<_>>(),
            vec!["src/main.rs"]
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// BR-3 with its benign twin: a boundary file named is a `BoundaryTouch`,
    /// an ordinary file named is not.
    #[test]
    fn a_boundary_path_token_is_a_boundary_touch() {
        let root = project_root("boundary");
        std::fs::write(root.join(".env"), "API_KEY=x\n").unwrap();
        assert_eq!(verdict(&root, "cat .env").kind, VerdictKind::BoundaryTouch);
        // Benign: the same verb on a file no glob covers.
        assert_eq!(
            verdict(&root, "cat src/main.rs").kind,
            VerdictKind::Rooted,
            "an ordinary file must not read as a boundary touch"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// **REQ-619 verify, C2.** A boundary touch outside the root stays a
    /// boundary touch when the same command also names an ordinary in-root
    /// file.
    ///
    /// `sources` is the accumulator over every token, so it is not evidence
    /// about *where the touch was*. Both consumers read `sources.is_empty()` as
    /// that evidence, which made `cat ~/.ssh/id_rsa README.md` report
    /// `Sources({README.md})` — a clean, liftable provenance for a command that
    /// read a private key. `out_of_root_touch` states the fact instead.
    ///
    /// Both shapes, because the two reach the flag by different routes: one
    /// segment holding both tokens, and two segments holding one each.
    ///
    /// # Benign twins
    ///
    /// An **in-root** boundary path must NOT set the flag — its id is in
    /// `sources`, the glob matches it, and egress names the file — and an
    /// ordinary read must stay `Rooted` with the flag clear. A fix that set the
    /// bit whenever a boundary was seen would pass the first half of this test
    /// and turn every `cat .env` into an unliftable pin naming nothing.
    ///
    /// **Mutation (run, red, reverted):** drop `evidence.out_of_root = true`
    /// from the `OutsideRoot` arm — the flag is then never set and the code is
    /// back to inferring the answer — and **four** tests go red:
    /// this one, [`tests::an_unknown_reached_after_a_boundary_token_is_a_boundary_touch`],
    /// [`tests::an_out_of_root_path_is_matched_in_both_spellings`], and
    /// `shell::tests::an_out_of_root_touch_beside_an_in_root_read_maps_to_the_sentinel`
    /// at the consumer. Before this pass the same mutation reddened nothing at
    /// all, which is the finding: the flag did not exist and the proxy it
    /// replaces had no case that could tell them apart.
    #[test]
    fn a_boundary_touch_outside_the_root_beside_an_in_root_file_is_still_a_boundary_touch() {
        let root = project_root("mixedtouch");
        let home = fixture_home("mixedtouch");
        std::fs::write(root.join("README.md"), "# readme\n").unwrap();

        for command in [
            "cat ~/.ssh/id_rsa README.md",
            "cat README.md; cat ~/.aws/credentials",
        ] {
            let v = verdict_with_home(&root, &home, command);
            assert_eq!(
                v.kind,
                VerdictKind::BoundaryTouch,
                "{command:?} names a protected file ({})",
                v.reason
            );
            assert!(
                v.out_of_root_touch,
                "{command:?} touched a boundary no id in `sources` can name"
            );
            assert_eq!(
                v.sources
                    .iter()
                    .map(ProvenanceId::as_str)
                    .collect::<Vec<_>>(),
                vec!["README.md"],
                "the in-root file it also read is still named"
            );
        }

        // Benign twin 1: an in-root boundary path needs no bit.
        std::fs::write(root.join(".env"), "API_KEY=x\n").unwrap();
        let in_root = verdict_with_home(&root, &home, "cat .env README.md");
        assert_eq!(in_root.kind, VerdictKind::BoundaryTouch);
        assert!(
            !in_root.out_of_root_touch,
            "an in-root touch mints an id, so it is not the out-of-root case"
        );
        assert_eq!(
            in_root
                .sources
                .iter()
                .map(ProvenanceId::as_str)
                .collect::<Vec<_>>(),
            vec![".env", "README.md"]
        );

        // Benign twin 2: an ordinary read is untouched.
        let clean = verdict_with_home(&root, &home, "cat README.md");
        assert_eq!(clean.kind, VerdictKind::Rooted);
        assert!(!clean.out_of_root_touch);

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    /// **REQ-619 verify, H1.** A token that cannot be classified does not erase
    /// a boundary token that came before it *in the same segment*.
    ///
    /// The precedence rule — a boundary touch outranks an unknown, because its
    /// pin is permanent and an unknown's is liftable — was written at the
    /// segment loop and held only *between* segments. Inside one segment the
    /// token loop returned `Unknown` directly, discarding the local
    /// `saw_boundary`, so `cat ~/.ssh/id_rsa /tmp/x` was a liftable `Unknown`:
    /// `/shell allow` then released the block over a command that had read a
    /// private key.
    ///
    /// Three spellings, because there are three ways a later token returns
    /// `Unknown`: an out-of-root path that matches nothing, a directory whose
    /// subtree is dirty, and a path under a denied prefix.
    ///
    /// # Benign path — and the half that makes this non-vacuous
    ///
    /// Each spelling is run again *without* the boundary token. All three must
    /// still be `Unknown`: that is what shows the second token really is the
    /// unknown-producing one, so the first assertion is about precedence rather
    /// than about a command that was never unknown to begin with.
    ///
    /// **Mutation (run, red, reverted):** restore the per-segment local — a
    /// `saw_boundary` inside `classify_segment` in place of the caller's
    /// `evidence.any` — and **exactly one** test goes red: this one, on the
    /// first spelling, `left: Unknown` where a boundary touch was due. The
    /// cross-segment rule keeps working under that mutation, which is why the
    /// gap survived: every existing test put the two facts in two segments.
    #[test]
    fn an_unknown_reached_after_a_boundary_token_is_a_boundary_touch() {
        let root = project_root("h1");
        let home = fixture_home("h1");
        // An out-of-root path no glob covers.
        let elsewhere =
            std::env::temp_dir().join(format!("teton-h1-outside-{}", std::process::id()));
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("notes.txt"), "x").unwrap();
        // A directory under the root that holds a protected file.
        std::fs::create_dir_all(root.join("dirty")).unwrap();
        std::fs::write(root.join("dirty/.env"), "API_KEY=x\n").unwrap();
        // A directory no tool may read at all (REQ-611 BR-8).
        let transcripts = root.join("transcripts");
        std::fs::create_dir_all(&transcripts).unwrap();
        std::fs::write(transcripts.join("t.jsonl"), "{}\n").unwrap();

        let tails = [
            elsewhere.join("notes.txt").to_string_lossy().into_owned(),
            "dirty".to_owned(),
            "transcripts/t.jsonl".to_owned(),
        ];
        // Canonical, because `under_denied_prefix` compares against the
        // canonicalized resolved path and the temp root is reached through a
        // symlink on macOS.
        let denied_prefix = transcripts.canonicalize().unwrap();
        for tail in &tails {
            let denied = vec![denied_prefix.clone()];
            let with_boundary = verdict_full(
                &root,
                Some(&home),
                &builtins(),
                denied.clone(),
                &format!("cat ~/.ssh/id_rsa {tail}"),
            );
            assert_eq!(
                with_boundary.kind,
                VerdictKind::BoundaryTouch,
                "a boundary token must outrank an unknown one in its own segment \
                 (tail {tail:?}, reason {})",
                with_boundary.reason
            );
            assert!(
                with_boundary.out_of_root_touch,
                "and the touch was out of root (tail {tail:?})"
            );

            // Non-vacuity: the tail alone really is what produces `Unknown`.
            let alone = verdict_full(
                &root,
                Some(&home),
                &builtins(),
                denied,
                &format!("cat {tail}"),
            );
            assert_eq!(
                alone.kind,
                VerdictKind::Unknown,
                "tail {tail:?} must be the unknown-producing token ({})",
                alone.reason
            );
        }

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&elsewhere).ok();
    }

    /// **REQ-619 verify, H2.** A verb that names a path is never looked up in a
    /// permissive table.
    ///
    /// The basename strip made `bin/ls`, `./cat` and `tools/git` inherit the
    /// reach of `ls`, `cat` and `git` — so a repository-local executable, whose
    /// contents this daemon has never read, ran under an allowlisted verb and
    /// its output carried a `Rooted` provenance. The strip stays for the
    /// **denylist** only, where reading `/usr/bin/python3` as `python3` makes
    /// the answer stricter and a miss lands on the fallthrough anyway.
    ///
    /// # Benign path
    ///
    /// The plain spellings must keep working — this rule is about the `/`, not
    /// about the verbs — and `/bin/sh` must keep its *interpreter* reason, so
    /// the denylist is still consulted first.
    ///
    /// **Mutation (run, red, reverted):** delete the `raw_verb.contains('/')`
    /// refusal and the three path-spelled commands come back `Rooted`;
    /// **exactly one** test red — this one, on `bin/ls README.md`.
    #[test]
    fn a_verb_named_by_path_is_never_a_permissive_table_hit() {
        let root = project_root("pathverb");
        std::fs::write(root.join("README.md"), "# readme\n").unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("bin/ls"), "#!/bin/sh\ncat \"$@\"\n").unwrap();

        for command in ["bin/ls README.md", "./cat README.md", "tools/git status"] {
            let v = verdict(&root, command);
            assert_eq!(
                v.kind,
                VerdictKind::Unknown,
                "{command:?} runs a program this daemon cannot identify ({})",
                v.reason
            );
            assert_eq!(v.reason, "the command names its program by path");
        }

        // The denylist still wins, and still on the basename.
        let sh = verdict(&root, "/bin/sh -c ls");
        assert_eq!(sh.kind, VerdictKind::Unknown);
        assert_eq!(
            sh.reason, "the command runs an interpreter, build tool or network client",
            "the opaque table is consulted before the path rule"
        );

        // Benign: the plain spellings are untouched.
        assert_eq!(verdict(&root, "ls -la").kind, VerdictKind::Rooted);
        assert_eq!(verdict(&root, "cat README.md").kind, VerdictKind::Rooted);
        assert_eq!(verdict(&root, "git status").kind, VerdictKind::Rooted);

        std::fs::remove_dir_all(&root).ok();
    }

    /// **REQ-619 verify, C3 (the resolver half).** A token under the root whose
    /// identity will not mint is unresolvable, not the root itself.
    ///
    /// `resolve_token` treated *every* mint failure as "this is the root", a
    /// reading that was true when `Empty` was the only reachable error. TASK-398
    /// added `ReservedScope`, so `<root>/~/.env` — a directory literally named
    /// `~`, which a shell creates inside quotes — became `RootItself`: a
    /// **file** classified as a directory read, scanned as a tree (finding
    /// nothing, because it is not one) and answered `Rooted`.
    ///
    /// # Benign path
    ///
    /// `Empty` must keep its arm, so `cat .` — the root itself — still resolves
    /// as a directory rather than falling to `Unknown`.
    ///
    /// **Mutation (run, red, reverted):** restore the blanket
    /// `Err(_) => Resolved::RootItself(checked)` and `cat ./~/.env` comes back
    /// `Rooted`; **exactly one** test red — this one, on its first
    /// assertion.
    #[test]
    fn a_token_whose_identity_will_not_mint_is_unknown_not_rooted() {
        let root = project_root("reservedtoken");
        std::fs::create_dir_all(root.join("~")).unwrap();
        std::fs::write(root.join("~/.env"), "API_KEY=x\n").unwrap();

        let v = verdict(&root, "cat ./~/.env");
        assert_eq!(
            v.kind,
            VerdictKind::Unknown,
            "a path with no identity cannot be proved in-root ({})",
            v.reason
        );
        assert_eq!(v.reason, "a path argument could not be resolved");

        // Benign: the `Empty` arm is what the root itself needs, and it stays.
        assert_eq!(
            verdict(&root, "ls .").kind,
            VerdictKind::Rooted,
            "`.` is the root, which is a directory read and not a failure"
        );
        assert_eq!(verdict(&root, "cat src/main.rs").kind, VerdictKind::Rooted);

        std::fs::remove_dir_all(&root).ok();
    }

    /// **REQ-619 verify, C3 (the walker half).** A subtree holding a file the
    /// walk could not name is never answered boundary-free.
    ///
    /// The matcher runs on a minted id, so an entry with no id is an entry no
    /// glob was ever run against. `walk::visit` skipped it silently, and this
    /// module read "no hit" as "clean": a root holding `~/.env` answered
    /// `Rooted` for `grep -r foo .`, and the repository's secrets left under a
    /// clean provenance. The scan now fails closed on `WalkReport::unmintable`,
    /// exactly as it does on a truncated walk.
    ///
    /// # Benign path
    ///
    /// The identical command in a tree with nothing unnameable must stay
    /// `Rooted` — otherwise the fix is just "every scan fails", which passes
    /// the first assertion and destroys the narrowing REQ-614 exists for.
    ///
    /// **Mutation (run, red, reverted):** drop `&& report.unmintable == 0` from
    /// `subtree_is_boundary_free` and the dirty root answers `Rooted`;
    /// **exactly one** test red — this one, on `grep -r foo .`. Deleting the
    /// walker's own `unmintable` counter instead reddens **two**: this test and
    /// `walk::tests::an_entry_whose_identity_will_not_mint_is_counted_not_silently_skipped`.
    #[test]
    fn a_file_the_walk_cannot_name_is_never_boundary_free() {
        let root = project_root("unmintablescan");
        std::fs::create_dir_all(root.join("~")).unwrap();
        std::fs::write(root.join("~/.env"), "API_KEY=x\n").unwrap();

        for command in ["grep -r foo .", "cat"] {
            let v = verdict(&root, command);
            assert_eq!(
                v.kind,
                VerdictKind::Unknown,
                "{command:?} reads a tree holding a file the walk cannot name ({})",
                v.reason
            );
        }

        // Benign twin: a tree the walk can name entirely stays `Rooted`.
        let clean = project_root("mintablescan");
        for command in ["grep -r foo .", "cat"] {
            let v = verdict(&clean, command);
            assert_eq!(
                v.kind,
                VerdictKind::Rooted,
                "{command:?} over a clean tree must keep its narrow verdict ({})",
                v.reason
            );
        }

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&clean).ok();
    }

    /// **REQ-619 verify, m2.** An out-of-root path is matched in **both**
    /// spellings a boundary glob can be written in.
    ///
    /// A path outside the root mints no id, so the globs are matched against
    /// the path text. There are two texts for one file: the absolute form with
    /// its leading `/` stripped, which is what `**/`-prefixed builtins reach,
    /// and the `~/…` form REQ-619 taught the daemon to mint for files under the
    /// home — the spelling a user writes in `~/.claude/skills/**`, and the one
    /// they see in a `privacy_block` line. Matching only the first left a user's
    /// own glob unable to reach the file a shell command named.
    ///
    /// # Benign path, and the non-vacuity half
    ///
    /// The builtin spelling must keep working (first assertion), and the same
    /// command with **no** home must stay `Unknown` — which is what shows the
    /// home spelling is doing the matching rather than the glob happening to
    /// reach the absolute form too.
    ///
    /// **Mutation (run, red, reverted):** delete the `home_spelling` disjunct
    /// from the `OutsideRoot` arm and **exactly one** test goes red — this one,
    /// on the first home-glob spelling — while its builtin assertion above
    /// stays green, which is what localises the failure to the new branch.
    #[test]
    fn an_out_of_root_path_is_matched_in_both_spellings() {
        use teton_core::entities::BoundaryMode;

        let root = project_root("spellings");
        let home = fixture_home("spellings");
        std::fs::create_dir_all(home.join(".claude/skills/x")).unwrap();
        std::fs::write(home.join(".claude/skills/x/SKILL.md"), "# skill\n").unwrap();
        let user_glob = vec![PrivacyBoundary::user(
            "~/.claude/skills/**",
            BoundaryMode::LocalOnly,
        )];

        // Spelling one, the builtin's: `**/.ssh/**` over the `/`-stripped
        // absolute path (AC-5's rule, unchanged).
        let builtin = verdict_with_home(&root, &home, "cat ~/.ssh/id_rsa");
        assert_eq!(
            builtin.kind,
            VerdictKind::BoundaryTouch,
            "{}",
            builtin.reason
        );
        assert!(builtin.out_of_root_touch);

        // Spelling two, the user's: `~/.claude/skills/**` reaches the same file
        // named either way round.
        let skill_abs = home.join(".claude/skills/x/SKILL.md");
        for command in [
            "cat ~/.claude/skills/x/SKILL.md".to_owned(),
            format!("cat {}", skill_abs.to_string_lossy()),
        ] {
            let v = verdict_full(&root, Some(&home), &user_glob, Vec::new(), &command);
            assert_eq!(
                v.kind,
                VerdictKind::BoundaryTouch,
                "a user glob in the home spelling must reach {command:?} ({})",
                v.reason
            );
            assert!(v.out_of_root_touch);
        }

        // Non-vacuity: with no home there is no second spelling to try, and the
        // absolute form matches nothing.
        let no_home = verdict_full(
            &root,
            None,
            &user_glob,
            Vec::new(),
            &format!("cat {}", skill_abs.to_string_lossy()),
        );
        assert_eq!(
            no_home.kind,
            VerdictKind::Unknown,
            "the `/`-stripped absolute path is not what `~/…` matches ({})",
            no_home.reason
        );

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    /// BR-8. Enforced by the signature: `classify` takes no exit status and no
    /// output, so a failed command *cannot* be classified differently from a
    /// successful one. Asserted structurally because there is no runtime input
    /// to vary — which is the point.
    ///
    /// **Mutation**: add a status parameter and branch on it, and this check is
    /// what a reviewer is pointed at.
    #[test]
    fn the_verdict_takes_no_exit_status() {
        let source = include_str!("shell_provenance.rs");
        let start = source
            .find("pub(crate) fn classify(")
            .expect("classify is declared");
        let sig_end = source[start..]
            .find(") -> Verdict")
            .expect("signature ends");
        let signature = &source[start..start + sig_end];
        for forbidden in ["status", "ExitStatus", "exit", "output", "stdout", "stderr"] {
            assert!(
                !signature.contains(forbidden),
                "BR-8: `classify` must not take {forbidden:?} — its signature is: {signature}"
            );
        }
    }

    /// BR-9. With no boundary configured the verdict is the pre-REQ-614 one and
    /// no walk happens — asserted by pointing the classifier at a root whose
    /// subtree would be expensive and a command that would otherwise scan it.
    #[test]
    fn an_empty_boundary_set_short_circuits_before_any_walk() {
        let root = project_root("nobounds");
        let v = classify(&root, RootKind::Project, &[], Vec::new(), "grep -r x .");
        assert_eq!(v.kind, VerdictKind::Unknown);
        assert_eq!(v.reason, "no privacy boundary is configured");
        // Benign twin: with boundaries in force the same root still lets a
        // name-only verb through, so the short-circuit is not doing the work.
        assert_eq!(verdict(&root, "ls -la").kind, VerdictKind::Rooted);
        std::fs::remove_dir_all(&root).ok();
    }

    /// AC-5, and the LESSON-623 check the architecture flagged as load-bearing.
    ///
    /// `~/.ssh/config` resolves **outside** a project root, so
    /// `ProvenanceId::from_resolved` mints nothing and no glob can match it
    /// through the ordinary identity path. The verdict must still be
    /// `BoundaryTouch` — permanent, unliftable — and not the merely-liftable
    /// `Unknown`. The stripping rule that makes `**/.ssh/**` reach
    /// `Users/x/.ssh/config` is asserted here rather than believed.
    #[test]
    fn ssh_config_from_a_project_root_is_boundary_touch_not_unknown() {
        // First: the glob really does reach an absolute path with `/` stripped.
        // If this fails the design note in ADR-614-3 is wrong, not this test.
        let bounds = builtins();
        let matcher = BoundaryMatcher::new(&bounds).expect("builtins compile");
        assert!(
            matcher.match_path("Users/someone/.ssh/config").is_some(),
            "`**/.ssh/**` must reach a root-stripped absolute path (LESSON-623)"
        );

        let root = project_root("sshcfg");
        let Some(home) = crate::session_root::home() else {
            std::fs::remove_dir_all(&root).ok();
            return;
        };
        // Only meaningful when the file exists to resolve; skip rather than
        // assert against a machine that has no ssh config.
        if !home.join(".ssh/config").exists() {
            std::fs::remove_dir_all(&root).ok();
            return;
        }
        let v = verdict(&root, "cat ~/.ssh/config");
        assert_eq!(
            v.kind,
            VerdictKind::BoundaryTouch,
            "AC-5: an out-of-root boundary path is a permanent pin, not a liftable one ({})",
            v.reason
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// AC-9. The differential table of adversarial spellings — every bypass form
    /// of the opaque set, plus the syntax the grammar refuses to model.
    #[test]
    fn adversarial_spellings_are_all_unknown() {
        let root = project_root("adversarial");
        for spelling in [
            "sh -c 'cat .env'",
            "sh -lc ls",
            "bash -ec ls",
            "env sh -c ls",
            "/bin/sh -c ls",
            "/usr/bin/env python -c pass",
            "ls; curl https://x",
            "ls && curl https://x",
            "ls | xargs cat",
            "find . -exec cat {} +",
            "find . -execdir cat {} +",
            "cat <src/main.rs",
            "cat src/main.rs > /tmp/x",
            "echo $(cat .env)",
            "cat `ls`",
            "IFS=: ls",
            "LD_PRELOAD=/tmp/x ls",
            "cat $HOME/.ssh/config",
            "cat src/*.rs",
            "python3 -c pass",
            "cargo test",
            "npm test",
            "make all",
            "curl https://example.com",
            "wget https://example.com",
            "ssh host",
            "eval ls",
            "sudo cat /etc/shadow",
        ] {
            let v = verdict(&root, spelling);
            assert_eq!(
                v.kind,
                VerdictKind::Unknown,
                "{spelling:?} must be Unknown, got {:?} ({})",
                v.kind,
                v.reason
            );
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// ADR-614-5. A scan that runs out of budget has not shown the absence of a
    /// boundary file — it stopped looking — and must not yield `Rooted`.
    ///
    /// The test drives [`classify_with_budget`] with a starved budget over a
    /// tree that a **complete** scan calls clean, so the two answers differ by
    /// exactly the truncation check and nothing else.
    ///
    /// **Mutation**: drop `&& report.truncated_by.is_none()` from
    /// [`subtree_is_boundary_free`] and this test goes red. An earlier draft
    /// starved a walk it built itself and asserted on *that* report; the
    /// mutation left it green, because it never called the function it claimed
    /// to guard (LESSON-569).
    #[test]
    fn a_truncated_scan_is_unknown_never_rooted() {
        let root = project_root("truncated");
        // No boundary file anywhere: a complete scan answers `Rooted`.
        std::fs::create_dir_all(root.join("clean")).unwrap();
        for i in 0..5 {
            std::fs::write(root.join(format!("clean/f{i}.txt")), "x").unwrap();
        }
        let bounds = builtins();

        let complete = classify_with_budget(
            &root,
            RootKind::Project,
            &bounds,
            Vec::new(),
            "grep foo clean",
            SCAN_BUDGET,
            None,
        );
        assert_eq!(
            complete.kind,
            VerdictKind::Rooted,
            "the fixture is boundary-free, so an unstarved scan is Rooted ({})",
            complete.reason
        );

        let starved = classify_with_budget(
            &root,
            RootKind::Project,
            &bounds,
            Vec::new(),
            "grep foo clean",
            WalkBudget {
                max_entries: 1,
                max_wall: Duration::from_millis(1),
            },
            None,
        );
        assert_eq!(
            starved.kind,
            VerdictKind::Unknown,
            "a scan that hit its budget has not shown the absence of a boundary file ({})",
            starved.reason
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// The unrecognised-verb fallthrough — the single line that makes
    /// [`classify_segment`] an allowlist rather than a denylist (ADR-614-1).
    ///
    /// This test exists because inverting that line broke **nothing** in the
    /// first draft: every spelling in [`adversarial_spellings_are_all_unknown`]
    /// is caught by [`OPAQUE`] or by [`UNMODELLED`] before the fallthrough is
    /// reached, so the line that makes this an allowlist had no test at all.
    /// The verbs below are in **no** table — ordinary programs that read files,
    /// every one of which the denylist reading of BR-1(e) would let through.
    ///
    /// **Mutation**: change the fallthrough to `SegmentVerdict::Rooted` and this
    /// test goes red; nothing else in the module does.
    #[test]
    fn an_unrecognised_verb_is_unknown_not_rooted() {
        let root = project_root("unknownverb");
        for verb_form in [
            "base64 src/main.rs",
            "strings src/main.rs",
            "hexdump src/main.rs",
            "tar -cf - src",
            "openssl dgst src/main.rs",
            "cp src/main.rs /tmp/x",
            "install src/main.rs /tmp/x",
            "vim src/main.rs",
            "jq . src/main.rs",
            "rg foo src",
        ] {
            let v = verdict(&root, verb_form);
            assert_eq!(
                v.kind,
                VerdictKind::Unknown,
                "{verb_form:?} names a verb in no table and must be Unknown, got {:?} ({})",
                v.kind,
                v.reason
            );
        }
        // The reason must be the fallthrough's, not an earlier gate's — that is
        // what proves this test reaches the line it claims to guard.
        assert_eq!(
            verdict(&root, "base64 src/main.rs").reason,
            "the command's verb is not one this classifier recognises"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// REQ-611 BR-8: a path under a denied prefix — a session transcript — is
    /// never `Rooted`, even though it sits inside the session root and carries a
    /// perfectly good `ProvenanceId` that matches no boundary glob.
    ///
    /// The transcript directory is deliberately **not** a privacy boundary
    /// (REQ-611 ADR-7): there is nothing to taint, the read simply must not
    /// happen. `shell` is the named exception the jail cannot refuse, and before
    /// REQ-614 it was held at egress by the constant `Unknown`. The first draft
    /// of this module narrowed the verdict without checking denied prefixes and
    /// handed a `cat` of a transcript a clean `Rooted` provenance — caught by
    /// `transcript::every_file_tool_refuses_the_transcript_and_shell_output_is_held_at_egress`,
    /// not by anything in this file.
    ///
    /// **Mutation**: delete the `under_denied_prefix` check in
    /// [`classify_segment`] and this test goes red.
    #[test]
    fn a_path_under_a_denied_prefix_is_never_rooted() {
        let root = project_root("denied");
        let transcripts = root.join("transcripts");
        std::fs::create_dir_all(&transcripts).unwrap();
        std::fs::write(transcripts.join("s.jsonl"), "{}\n").unwrap();

        // Benign twin first: with no denied prefix the same read is `Rooted`,
        // so the difference below is the check and nothing else.
        let free = classify(
            &root,
            RootKind::Project,
            &builtins(),
            Vec::new(),
            "cat transcripts/s.jsonl",
        );
        assert_eq!(free.kind, VerdictKind::Rooted, "{}", free.reason);

        let denied = classify(
            &root,
            RootKind::Project,
            &builtins(),
            vec![transcripts.canonicalize().unwrap()],
            "cat transcripts/s.jsonl",
        );
        assert_eq!(
            denied.kind,
            VerdictKind::Unknown,
            "a transcript read must stay fail-closed ({})",
            denied.reason
        );
        assert_eq!(
            denied.reason, "a path argument is inside a directory tools may not read",
            "and it must not claim a privacy boundary was crossed"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// **The Phase-5 review finding.** A content-reading verb whose tokens do
    /// not name existing files is scanned against the root.
    ///
    /// `grep -r foo` names exactly one token, and it is a **pattern**. The first
    /// implementation resolved it as a path — to a nonexistent file under the
    /// root — found `paths` non-empty, and therefore skipped BR-1(d)'s root
    /// scan, returning `Rooted`. GNU grep's `-r` with no path argument searches
    /// `.` recursively, so in a repository holding a `.env` that command reads
    /// it and the output would have gone to a remote provider under a clean
    /// provenance. Nothing in the suite caught it: every earlier fixture either
    /// named a real file or named nothing at all.
    ///
    /// Telling a pattern from a path needs a per-verb option table, which is
    /// the second parser ADR-614-1 refuses. The fail-closed rule needs none: a
    /// token that is not an existing regular file means the verb may reach
    /// further than its tokens say, so scan.
    ///
    /// **Mutation**: drop `|| unproven_reach` from the root-scan condition and
    /// this test goes red.
    #[test]
    fn a_pattern_argument_does_not_pass_for_a_path() {
        let root = project_root("pattern");
        std::fs::write(root.join(".env"), "K=v\n").unwrap();

        // None of these names an existing file, so none of them bounds the
        // verb's reach — the root does, and the root holds a `.env`.
        for command in ["grep -r foo", "grep -R foo", "grep foo", "cat"] {
            let v = verdict(&root, command);
            assert_eq!(
                v.kind,
                VerdictKind::Unknown,
                "{command:?} can reach past the files it names, and the root holds a \
                 boundary file: {:?} ({})",
                v.kind,
                v.reason
            );
        }

        // The benign twin, and the reason this rule is affordable: with **no**
        // boundary file under the root, the same commands are `Rooted`. The
        // scan is what decides, not the shape of the argument list — so an
        // ordinary repository pays nothing for this.
        let clean = project_root("pattern-clean");
        for command in ["grep -r foo", "grep foo src/main.rs", "cat"] {
            let v = verdict(&clean, command);
            assert_eq!(
                v.kind,
                VerdictKind::Rooted,
                "{command:?} in a boundary-free repo must stay Rooted ({})",
                v.reason
            );
        }
        // And an explicit read of a real file needs no scan at all — even in a
        // repository that DOES hold a boundary file, which is OQ-3's stated
        // balance and what the "at least one existing file" rule buys. Measured
        // on a 192,000-file repository, `grep foo <file>` went from 123ms and
        // `Unknown` under an all-tokens-must-resolve rule to 0.5ms and `Rooted`
        // under this one.
        assert_eq!(verdict(&clean, "cat src/main.rs").kind, VerdictKind::Rooted);
        assert_eq!(
            verdict(&root, "cat src/main.rs").kind,
            VerdictKind::Rooted,
            "an explicit file read is bounded by the file, even beside a `.env`"
        );
        assert_eq!(
            verdict(&root, "grep foo src/main.rs").kind,
            VerdictKind::Rooted,
            "a pattern plus an explicit file is bounded by the file"
        );
        assert_eq!(
            verdict(&root, "head -n 5 src/main.rs").kind,
            VerdictKind::Rooted,
            "a flag's argument resolves to nothing and must not force a root scan"
        );

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&clean).ok();
    }

    /// A symlink to a boundary file is a boundary touch, and a symlink out of
    /// the root is not rooted.
    ///
    /// LESSON-550's recurrence class: REQ-585 closed a symlink escape at the
    /// leaf and REQ-587 found it again one level up. The property that makes
    /// this safe here is that `resolve_token` goes through
    /// `canonical_through_existing_ancestor` — the same resolution
    /// `ToolContext::resolve` uses — so containment and the glob match are both
    /// decided on the **resolved** path, never on the spelling. Asserted rather
    /// than assumed, because "it canonicalizes" is exactly the kind of claim
    /// that stays true right up until a refactor takes the call out.
    #[test]
    fn a_symlink_is_resolved_before_the_glob_and_the_root_check() {
        #[cfg(unix)]
        {
            let root = project_root("symlink");
            std::fs::write(root.join(".env"), "K=v\n").unwrap();
            let outside = std::env::temp_dir().join("teton-symlink-outside-614");
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(outside.join("notes.txt"), "x").unwrap();

            // A link INSIDE the root pointing at a boundary file inside it.
            let to_env = root.join("innocent.txt");
            std::os::unix::fs::symlink(root.join(".env"), &to_env).unwrap();
            let v = verdict(&root, "cat innocent.txt");
            assert_eq!(
                v.kind,
                VerdictKind::BoundaryTouch,
                "a symlink to `.env` must resolve to `.env` before the glob runs ({})",
                v.reason
            );

            // A link INSIDE the root pointing OUT of it.
            let escape = root.join("escape.txt");
            std::os::unix::fs::symlink(outside.join("notes.txt"), &escape).unwrap();
            let out = verdict(&root, "cat escape.txt");
            assert_eq!(
                out.kind,
                VerdictKind::Unknown,
                "a symlink out of the root is not rooted ({})",
                out.reason
            );
            assert_eq!(
                out.reason,
                "a path argument resolves outside the session root"
            );

            std::fs::remove_dir_all(&root).ok();
            std::fs::remove_dir_all(&outside).ok();
        }
    }

    /// AC-9's other half: the opaque set is one pinned table. A verb removed
    /// from it must be caught here rather than silently becoming `Unknown` by
    /// the fallthrough — which would pass every other test in this module.
    #[test]
    fn the_opaque_table_is_pinned_and_disjoint_from_the_permissive_ones() {
        for verb in [
            "sh", "bash", "python", "python3", "node", "cargo", "npm", "make", "curl", "wget",
            "ssh", "scp", "eval", "xargs", "env",
        ] {
            assert!(OPAQUE.contains(&verb), "{verb} must be in the opaque table");
        }
        for permissive in READS_NOTHING.iter().chain(NAME_ONLY).chain(READS_CONTENT) {
            assert!(
                !OPAQUE.contains(permissive),
                "{permissive} is in both a permissive table and the opaque one"
            );
        }
    }

    /// ADR-614-2 / OQ-1: a non-project root is never `Rooted`, whatever the
    /// command. The benign twin is every `Rooted` assertion above, all of which
    /// run from a project root.
    #[test]
    fn a_non_project_root_is_never_rooted() {
        let root = project_root("nonproject");
        for kind in [RootKind::Home, RootKind::Plain, RootKind::FilesystemRoot] {
            let v = classify(&root, kind, &builtins(), Vec::new(), "ls -la");
            assert_eq!(v.kind, VerdictKind::Unknown, "{kind:?} must not be Rooted");
            assert_eq!(v.reason, "the session root is not a project");
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// BR-1(d): a content-reading verb pointed at a directory that holds a
    /// boundary file is `Unknown`, and the same verb pointed at a clean
    /// directory is `Rooted`. The pair is the rule; either alone is not.
    #[test]
    fn a_directory_read_is_scanned_and_a_clean_subtree_passes() {
        let root = project_root("dirscan");
        std::fs::create_dir_all(root.join("clean")).unwrap();
        std::fs::write(root.join("clean/a.txt"), "x").unwrap();
        std::fs::create_dir_all(root.join("dirty")).unwrap();
        std::fs::write(root.join("dirty/.env"), "K=v\n").unwrap();

        assert_eq!(verdict(&root, "grep foo clean").kind, VerdictKind::Rooted);
        assert_eq!(verdict(&root, "grep foo dirty").kind, VerdictKind::Unknown);
        // And a name-only verb passes over the dirty tree, because listing a
        // name is not reading a file (BR-1(d)'s own rationale).
        assert_eq!(verdict(&root, "ls dirty").kind, VerdictKind::Rooted);
        std::fs::remove_dir_all(&root).ok();
    }

    /// The scan must not inherit the discovery walk's skip set: `**/.npmrc` is a
    /// builtin boundary and `node_modules/<pkg>/.npmrc` is where it lives, but
    /// `node_modules` is in [`walk::WALK_SKIP_DIRS`]. A pruned scan would report
    /// "no boundary file here" about a tree `grep -r` reads in full.
    ///
    /// **Mutation**: build the scan from `WalkPolicy::default()` and this fails.
    #[test]
    fn the_scan_does_not_inherit_the_discovery_walks_skip_set() {
        let root = project_root("npmrc");
        std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        std::fs::write(root.join("node_modules/pkg/.npmrc"), "//r:_authToken=x\n").unwrap();
        assert_eq!(
            verdict(&root, "grep -r foo .").kind,
            VerdictKind::Unknown,
            "a boundary file under a normally-pruned directory must still be seen"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// The sentence [`UNMODELLED`] refuses with today. TASK-405 splits it into
    /// one sentence per `UnmodelledSyntax` class; until then every look-alike
    /// below carries this one, and the accepted forms must carry *none* of it
    /// (LESSON-550: assert the absence, not the remedy).
    const UNMODELLED_REASON: &str = "the command uses shell syntax this classifier does not model";

    /// Every `NullRedirect` form the entity names, attached where `sh` allows
    /// the attached spelling.
    const NULL_REDIRECTS: &[&str] = &[
        ">/dev/null",
        "1>/dev/null",
        "2>/dev/null",
        ">>/dev/null",
        "2>>/dev/null",
        "&>/dev/null",
        "</dev/null",
        "2>&1",
        "1>&2",
        ">&2",
    ];

    /// The same forms in the spaced spelling. Only the operator forms have one
    /// — a descriptor duplication is one word or it is not this form at all.
    const SPACED_NULL_REDIRECTS: &[&str] = &[
        "> /dev/null",
        "1> /dev/null",
        "2> /dev/null",
        ">> /dev/null",
        "2>> /dev/null",
        "&> /dev/null",
        "< /dev/null",
    ];

    /// Every other use of `>` and `<`, which REQ-620 BR-2 leaves exactly where
    /// REQ-614 put it. The three `/dev/null`-adjacent rows are LESSON-494's
    /// failure mode written out: one byte's difference between the gate and the
    /// shell that runs the command.
    const REDIRECT_LOOKALIKES: &[&str] = &[
        "> out.txt",
        ">> log",
        "< input",
        "2>/dev/nul",
        "2>/dev/null/x",
        "2>/dev/nullx",
        "2>$f",
        "> \"$f\"",
    ];

    /// The verbs the differential table is run against — one from each of the
    /// grammar's four tables plus a `git` subcommand, so a regression in the
    /// strip cannot hide behind a single arm of [`classify_segment`].
    const REDIRECT_TABLE_VERBS: &[&str] =
        &["ls", "cat README.md", "git status", "test -s x", "echo hi"];

    /// A project root for the redirect tables: clean of boundary files, holding
    /// the one file `cat README.md` names.
    fn redirect_root(tag: &str) -> PathBuf {
        let root = project_root(tag);
        std::fs::write(root.join("README.md"), "# fixture\n").unwrap();
        root
    }

    /// **BR-1 / BR-10: a null redirect is lifted as a whole word, before the
    /// unmodelled scan and before the split, and adds no reach.**
    ///
    /// The residue rows are the load-bearing half. `2>&1` contains `&`, which
    /// the splitter reads as a segment separator, so a strip that ran second
    /// would hand `2>` to [`classify_segment`] as a verb and `1` to the next
    /// segment as one — and the separators that really are separators have to
    /// survive the lift as their own words for TASK-404's splitter to see them.
    ///
    /// BR-10 is the last pair: the redirect exists precisely because the file
    /// might not, and the verdict may not move when it appears. The signature
    /// does most of that work — [`strip_null_redirects`] takes a `&str` and
    /// nothing else — but the classifier's answer is the property the REQ
    /// states, so it is the one asserted.
    ///
    /// **Mutation (run, red, reverted):** make
    /// [`strip_null_redirects`](super::shell_syntax::strip_null_redirects) a
    /// no-op — this reds first of seven, on the `ls 2>&1 && echo ok` residue
    /// row. Drop the whole-word rule (`from_operator` accepting any operator
    /// ending in `>`) — this reds, one of two, on the `ls>/dev/null` row.
    #[test]
    fn null_redirects_are_lifted_before_the_scan_and_the_split() {
        // The lift itself: whole words out, separators standing.
        for (command, residue, lifted) in [
            ("ls 2>&1 && echo ok", "ls && echo ok", 1),
            ("ls &>/dev/null || echo no", "ls || echo no", 1),
            ("ls 2>&1; ls", "ls ; ls", 1),
            ("ls 2>&1 | head", "ls | head", 1),
            ("cat x 2> /dev/null", "cat x", 2),
            ("2>/dev/null cat x", "cat x", 1),
            // Must not fire: a lone `&` is a separator, not a redirect, and a
            // form glued to its verb is a word this grammar does not model.
            ("ls & ls", "ls & ls", 0),
            ("ls>/dev/null", "ls>/dev/null", 0),
            ("ls > out.txt", "ls > out.txt", 0),
        ] {
            let stripped = strip_null_redirects(command);
            assert_eq!(
                stripped.residue, residue,
                "`{command}` should strip to `{residue}`"
            );
            assert_eq!(
                stripped.lifted, lifted,
                "`{command}` lifted the wrong count"
            );
        }

        let root = redirect_root("lifted");
        // A form on its own adds nothing and removes nothing: the verdict is
        // the one the bare command gets.
        let bare = verdict(&root, "ls");
        assert_eq!(bare.kind, VerdictKind::Rooted, "{}", bare.reason);
        for form in NULL_REDIRECTS.iter().chain(SPACED_NULL_REDIRECTS) {
            let command = format!("ls {form}");
            let v = verdict(&root, &command);
            assert_eq!(
                v.kind,
                VerdictKind::Rooted,
                "`{command}` should classify exactly as `ls` does ({})",
                v.reason
            );
            assert_eq!(v.reason, bare.reason, "`{command}` reached a different arm");
            assert!(v.sources.is_empty(), "`{command}` minted a source");
        }

        // BR-10: from the text alone. The redirect is there because the file
        // may not exist, and the verdict may not move when it does.
        let before = verdict(&root, "ls not-yet-there 2>/dev/null");
        std::fs::write(root.join("not-yet-there"), "now it is\n").unwrap();
        let after = verdict(&root, "ls not-yet-there 2>/dev/null");
        assert_eq!(before.kind, VerdictKind::Rooted, "{}", before.reason);
        assert_eq!(
            before.kind, after.kind,
            "the redirect's filesystem effect must not reach the verdict"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// **BR-2: every other use of `>` or `<` stays exactly as REQ-614 left it.**
    ///
    /// The must-not-fire half of BR-1 (LESSON-440). `2>/dev/nul`,
    /// `2>/dev/null/x` and `2>/dev/nullx` are the rows that matter: each is one
    /// byte from a form this grammar now accepts, and each is a real file write
    /// the classifier cannot prove anything about.
    ///
    /// **Mutation (run, red, reverted):** make the recogniser non-total — lift
    /// any word carrying `>` or `<` — and this reds on `ls > out.txt`, which
    /// comes back `Rooted`. It is one of six, and REQ-614's own
    /// [`tests::adversarial_spellings_are_all_unknown`] is another, on
    /// `cat <src/main.rs`: the same widening reaches both suites, which is what
    /// makes the recogniser's totality a property and not a comment. A no-op
    /// strip leaves this test green, correctly — it asserts the refusal a
    /// no-op preserves.
    #[test]
    fn every_other_redirect_stays_unmodelled() {
        let root = redirect_root("lookalikes");
        for verb in REDIRECT_TABLE_VERBS {
            for lookalike in REDIRECT_LOOKALIKES {
                let command = format!("{verb} {lookalike}");
                let v = verdict(&root, &command);
                assert_eq!(
                    v.kind,
                    VerdictKind::Unknown,
                    "`{command}` must stay unmodelled ({})",
                    v.reason
                );
                assert_eq!(
                    v.reason, UNMODELLED_REASON,
                    "`{command}` should carry the redirect-class reason"
                );
            }
        }
        // A here-doc and a process substitution, named by BR-2 and refused by
        // the same scan.
        for exotic in ["cat << EOF", "diff <(ls) <(ls)"] {
            assert_eq!(
                verdict(&root, exotic).reason,
                UNMODELLED_REASON,
                "`{exotic}` must stay unmodelled"
            );
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// **BR-3: `/dev/null` in a lifted redirect is not a path token.**
    ///
    /// LESSON-623's seam read the other way round — a path that is *not* a file
    /// access must not be scored as one. The assertion is a differential on one
    /// boundary glob that matches the device: named as a redirect target it
    /// contributes nothing, named as an argument it is a touch. A test that only
    /// checked the redirect form would pass against a classifier that never ran
    /// the matcher at all.
    ///
    /// **Mutation (run, red, reverted):** stop consuming the spaced form's
    /// `/dev/null` follower (drop the `words.next()` in `strip_line`'s spaced
    /// arm) — this reds on `cat README.md > /dev/null` as a `BoundaryTouch`,
    /// one of four. That is BR-3's failure mode exactly: the device left in
    /// the residue, resolved as a path, and scored against a glob.
    #[test]
    fn dev_null_is_never_a_path_token() {
        let root = redirect_root("devnull");
        // A user glob that matches the device, so a `/dev/null` that reached
        // path resolution would be visible as a boundary touch rather than as
        // a silent no-op.
        let mut boundaries = builtins();
        boundaries.push(PrivacyBoundary::user("**/null", BoundaryMode::LocalOnly));

        for form in NULL_REDIRECTS.iter().chain(SPACED_NULL_REDIRECTS) {
            let command = format!("cat README.md {form}");
            let v = verdict_full(&root, None, &boundaries, Vec::new(), &command);
            assert_eq!(
                v.kind,
                VerdictKind::Rooted,
                "`{command}` must not score the device as a file ({})",
                v.reason
            );
            assert!(
                !v.out_of_root_touch,
                "`{command}` recorded an out-of-root touch"
            );
            assert!(
                v.sources.iter().all(|id| !id.as_str().contains("null")),
                "`{command}` minted an id for the device"
            );
            assert!(
                !strip_null_redirects(&command).residue.contains("/dev/"),
                "`{command}` left the device in the residue"
            );
        }

        // The differential: the same bytes as an *argument* are a path token,
        // and the glob finds them.
        let v = verdict_full(&root, None, &boundaries, Vec::new(), "cat /dev/null");
        assert_eq!(
            v.kind,
            VerdictKind::BoundaryTouch,
            "the device named as an argument is an ordinary path ({})",
            v.reason
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// **BR-5 / AC-7: an opaque verb with a null redirect is still `Unknown`,
    /// with the opaque-verb reason.**
    ///
    /// The REQ's BR-5 sentence orders the strip *after* the opaque-verb check;
    /// ADR-620-2 orders it first, and the two agree on every observable,
    /// because lifting a redirect cannot turn an unrecognised verb into a
    /// recognised one. This test is what pins that: it asserts the *reason*,
    /// not just the kind, so a `curl` that started refusing as "unmodelled
    /// syntax" instead of "a network client" would red here even though the
    /// verdict is the same.
    ///
    /// **Mutation (run, red, reverted):** make
    /// [`strip_null_redirects`](super::shell_syntax::strip_null_redirects) a
    /// no-op and this reds on `python x.py 2>/dev/null` — not on the kind,
    /// which stays `Unknown`, but on the *reason*, which becomes the unmodelled
    /// sentence. A test asserting only the kind would have passed.
    #[test]
    fn an_opaque_verb_with_a_null_redirect_is_still_unknown() {
        let root = redirect_root("opaque");
        for command in [
            "python x.py 2>/dev/null",
            "curl example.com >/dev/null 2>&1",
            "sh -c ls 2>/dev/null",
            "cargo build 2>&1 | head",
        ] {
            let v = verdict(&root, command);
            assert_eq!(
                v.kind,
                VerdictKind::Unknown,
                "`{command}` must pin ({})",
                v.reason
            );
            assert_eq!(
                v.reason, "the command runs an interpreter, build tool or network client",
                "`{command}` should refuse on the verb, not on the redirect"
            );
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// **BR-8: a redirect never hides a boundary read.**
    ///
    /// The strip runs before any verb is read, so the boundary path is still
    /// resolved by [`classify_segment`] and BUG-216's precedence — a touch
    /// outranks an unknown — is untouched. Two boundary sets, because the
    /// builtin `**/.env` and a user glob reach the matcher by different
    /// spellings, and both orders of the redirect, because `sh` accepts a
    /// leading one and a model writes both.
    ///
    /// **Mutation (run, red, reverted):** make
    /// [`strip_null_redirects`](super::shell_syntax::strip_null_redirects) a
    /// no-op — this reds on `cat .env 2>/dev/null`, which comes back as the
    /// unmodelled `Unknown` a `/shell allow` would lift rather than the
    /// permanent touch it is.
    #[test]
    fn a_redirect_never_hides_a_boundary_read() {
        let root = redirect_root("boundary");
        std::fs::write(root.join(".env"), "SECRET=1\n").unwrap();
        std::fs::create_dir_all(root.join("secrets")).unwrap();
        std::fs::write(root.join("secrets/prod.env"), "TOKEN=2\n").unwrap();

        // The builtin `**/.env`, which needs no configuration to be in force.
        for command in [
            "cat .env 2>/dev/null",
            "2>/dev/null cat .env",
            "cat .env 2>&1",
            "cat .env </dev/null",
        ] {
            let v = verdict(&root, command);
            assert_eq!(
                v.kind,
                VerdictKind::BoundaryTouch,
                "`{command}` must still be a touch ({})",
                v.reason
            );
        }

        // A user glob, over the path AC-3 names. `**/.env` does not reach
        // `secrets/prod.env`, so this row would pass vacuously without it.
        let mut boundaries = builtins();
        boundaries.push(PrivacyBoundary::user("secrets/**", BoundaryMode::LocalOnly));
        for command in [
            "cat secrets/prod.env 2>/dev/null",
            "2>/dev/null cat secrets/prod.env",
        ] {
            let v = verdict_full(&root, None, &boundaries, Vec::new(), command);
            assert_eq!(
                v.kind,
                VerdictKind::BoundaryTouch,
                "`{command}` must still be a touch ({})",
                v.reason
            );
        }
        assert_eq!(
            verdict_full(
                &root,
                None,
                &builtins(),
                Vec::new(),
                "cat secrets/prod.env 2>/dev/null"
            )
            .kind,
            VerdictKind::Rooted,
            "must not fire: without the glob that covers it, the same read is an ordinary one"
        );

        // BUG-216's precedence, through a strip: a touch outranks an unknown
        // even when the unknown is in a later segment.
        let v = verdict(&root, "cat .env 2>/dev/null && python x.py");
        assert_eq!(
            v.kind,
            VerdictKind::BoundaryTouch,
            "a touch outranks an opaque verb ({})",
            v.reason
        );

        // Must not fire: an ordinary read with the same redirect is clean.
        let v = verdict(&root, "cat README.md 2>/dev/null");
        assert_eq!(v.kind, VerdictKind::Rooted, "{}", v.reason);
        std::fs::remove_dir_all(&root).ok();
    }

    /// **AC-1: the 2026-09-09 command, which pinned a session on a `2>&1`.**
    ///
    /// The command is the REQ's Description verbatim. Six `ls` calls, three
    /// `echo`s, a `which`, and nothing outside the root but the final `~/bin`
    /// probe — so without that segment it is `Rooted`, and with it the reason
    /// names the path, not the redirect. That last distinction is the whole
    /// point of AC-1: a user told "a redirect pinned your session" would delete
    /// the redirect and pin again on the next turn.
    ///
    /// **Mutation (run, red, reverted):** make
    /// [`strip_null_redirects`](super::shell_syntax::strip_null_redirects) a
    /// no-op — the first assertion reds with "the command uses shell syntax
    /// this classifier does not model", which is the 2026-09-09 pin verbatim
    /// and the behaviour this REQ exists to end.
    #[test]
    fn the_2026_09_09_command_is_rooted_without_its_home_probe() {
        let root = project_root("2026-09-09");
        let home = fixture_home("2026-09-09");
        std::fs::create_dir_all(root.join(".adlc/context")).unwrap();
        std::fs::create_dir_all(root.join(".adlc/partials")).unwrap();
        std::fs::write(root.join(".adlc/context/architecture.md"), "arch\n").unwrap();
        std::fs::write(root.join(".adlc/context/conventions.md"), "conv\n").unwrap();

        let without_the_probe = "ls .adlc/context/architecture.md .adlc/context/conventions.md 2>&1; echo ---; ls .adlc/ 2>/dev/null; echo ---; ls .adlc/partials/ 2>/dev/null | head; echo ---; ls tools/lint-skills/ 2>/dev/null; echo ---; which adlc-read";
        let v = verdict_with_home(&root, &home, without_the_probe);
        assert_eq!(
            v.kind,
            VerdictKind::Rooted,
            "the command that pinned the 2026-09-09 session reads nothing outside the root ({})",
            v.reason
        );

        let with_the_probe = format!("{without_the_probe}; ls ~/bin/adlc-read 2>/dev/null");
        let v = verdict_with_home(&root, &home, &with_the_probe);
        assert_eq!(
            v.kind,
            VerdictKind::Unknown,
            "the `~/bin` probe is still outside the root ({})",
            v.reason
        );
        assert_eq!(
            v.reason, "a path argument resolves outside the session root",
            "the reason must name the path, not the redirect"
        );
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    /// **AC-2: the differential table.**
    ///
    /// One fixture, three halves. Every accepted form — attached and spaced —
    /// on five verbs, each `Rooted` and each carrying *none* of the unmodelled
    /// sentence (LESSON-550: assert the absence, not the remedy). Every
    /// look-alike on the same five verbs, each `Unknown` with that sentence.
    /// And the `&`-bearing forms beside the separators they must not be
    /// mistaken for: `2>&1` and `&>/dev/null` are lifted whole, while `&&`,
    /// `||`, `;` and a lone `&` still separate segments.
    ///
    /// The `ls & frobnicate` row is the must-not-fire one and it is why the
    /// separator rows are not vacuous: `ls & ls` would pass against a splitter
    /// that had stopped splitting on `&` entirely, because both halves are the
    /// same benign verb. An unrecognised verb after the `&` proves a second
    /// segment was classified.
    ///
    /// **Mutation (run, red, reverted):** make
    /// [`strip_null_redirects`](super::shell_syntax::strip_null_redirects) a
    /// no-op and this reds on `ls >/dev/null`; make the recogniser non-total
    /// (lift any word carrying `>` or `<`) and it reds on `ls > /dev/null`
    /// resolving the device outside the root; leave the spaced form's follower
    /// in place and it reds again. Three of the four mutations in
    /// [`super::shell_syntax`]'s record reach this table, which is what a
    /// differential table is for.
    #[test]
    fn the_redirect_differential_table() {
        let root = redirect_root("differential");

        for verb in REDIRECT_TABLE_VERBS {
            for form in NULL_REDIRECTS.iter().chain(SPACED_NULL_REDIRECTS) {
                let command = format!("{verb} {form}");
                let v = verdict(&root, &command);
                assert_eq!(
                    v.kind,
                    VerdictKind::Rooted,
                    "`{command}` should be Rooted ({})",
                    v.reason
                );
                assert_ne!(
                    v.reason, UNMODELLED_REASON,
                    "`{command}` should not carry the refusal it was cleared of"
                );
            }
            for lookalike in REDIRECT_LOOKALIKES {
                let command = format!("{verb} {lookalike}");
                let v = verdict(&root, &command);
                assert_eq!(
                    v.kind,
                    VerdictKind::Unknown,
                    "`{command}` should be Unknown ({})",
                    v.reason
                );
                assert_eq!(v.reason, UNMODELLED_REASON, "`{command}`");
            }
        }

        // The `&`-bearing forms beside the separators they must not be mistaken
        // for.
        for command in [
            "ls 2>&1 && echo ok",
            "ls &>/dev/null || echo no",
            "ls 2>&1; ls",
            "ls 2>&1 | head",
            "ls & ls",
        ] {
            let v = verdict(&root, command);
            assert_eq!(
                v.kind,
                VerdictKind::Rooted,
                "`{command}` should be Rooted ({})",
                v.reason
            );
        }

        // Must not fire: a lone `&` is still a separator, so the word after it
        // is still read as a verb.
        let v = verdict(&root, "ls & frobnicate");
        assert_eq!(
            v.kind,
            VerdictKind::Unknown,
            "a lone `&` must still start a segment ({})",
            v.reason
        );
        assert_eq!(
            v.reason, "the command's verb is not one this classifier recognises",
            "the second segment's verb is what should have refused"
        );
        assert_eq!(
            strip_null_redirects("ls & frobnicate").lifted,
            0,
            "a lone `&` is not a redirect"
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
