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
//! # The redirect widening, and the order it depends on (REQ-620 BR-1)
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
//! # The pipeline widening: a piped reader reads its stdin (REQ-620 BR-4)
//!
//! BR-1(d) walks the root whenever a content verb was not handed explicit
//! files, because "the reach is whatever the verb defaults to" and the default
//! used to be assumed to be the current directory. For a segment whose stdin is
//! a pipe that is simply false: `head -5` after a `|` reads the bytes the
//! previous segment produced, and the previous segment was classified on its
//! own paths. Walking the root for it is a walk for a read that cannot happen —
//! and on any repository with a build tree the walk exhausts its budget, so the
//! verdict was `Unknown` by exhaustion (the REQ's third open question).
//!
//! So the splitter records **which separator preceded each segment**
//! ([`split_segments`], ADR-620-2 step 3). Exactly one `|` makes a segment
//! [`SegmentPosition::Piped`]; `;`, `&&`, `||`, a lone `&`, `(`, `)` and a
//! newline all leave it [`SegmentPosition::First`], because after any of those
//! the segment's stdin is the terminal's. `||` and `&&` are therefore tokenised
//! **longest-match-first**: REQ-614's byte-wise `split` read `||` as two `|`
//! separators with an empty segment between them, which is harmless for a
//! verdict and wrong for a position — an *or* is not a pipe.
//!
//! [`classify_segment`] takes the position and, for a `Piped` segment reading
//! its default source, skips the root walk — but only when the verb is on
//! [`reads_only_its_stdin`]'s **allowlist**. That polarity is the whole of the
//! rule. It shipped at TASK-404 as a denylist of the recursive `grep`
//! spellings, and a denylist inside an allowlist grammar is a machine for
//! false negatives: `ls | grep --directories recurse SECRET`, `--dir recurse`,
//! `--dereference-recursive` and `--rec` are all recursion GNU `grep` accepts,
//! none of them was on the list, and each came back `Rooted` for a command
//! that reads every file under the root. The exemption is now granted to a
//! closed set of pure filters (`head`, `wc`, `sort`, …) plus `grep` when every
//! word of the segment is a short flag that cannot be recursion; `sed`, `awk`,
//! `diff`, `cat` and every unrecognised `grep` flag keep the walk.
//!
//! **The flag rule covers the filters too** (Phase-5 re-verify). Being named on
//! the allowlist was at first the *whole* test for everything but `grep`, so
//! `ls | wc --files0-from -`, `ls | sort --files0-from -` and
//! `ls | shasum -c -` — each of which reads a list of **paths** off its stdin
//! and then opens every one — took the exemption. A word starting with `--` now
//! denies it for every verb on the list, `grep` included, and the two checksum
//! verbs additionally refuse `-c`. `less` and `more` left the list in the same
//! pass: a pager takes `:e path` and `!cmd` from the terminal and honours
//! `LESSOPEN`, and none of that is visible here.
//!
//! The widening does not touch the walk, its budget or its skip set, and it
//! does not reach a `First` segment: `head -5` alone and `ls; head -5` still
//! read the root, and `cat missing | head` still walks in its *first* segment,
//! on `cat missing`. That is BR-4's stated limit rather than a gap.
//!
//! # The refusal names its class (REQ-620 BR-6)
//!
//! The [`UNMODELLED`] scan used to answer one sentence — "the command uses
//! shell syntax this classifier does not model" — for all thirteen characters.
//! That sentence reaches three surfaces (`skill_invoked.reach_reason`, the
//! `session_pinned` event, the CLI's pin notice) and told none of their readers
//! anything they could act on: the model that pinned the 2026-09-09 `/analyze`
//! session could not learn from it that a `2>&1` was the offending byte, and
//! the user reading the notice could not either.
//!
//! So the scan reports an [`UnmodelledSyntax`] class and the class names the
//! sentence. The class is the **first present in [`UNMODELLED_ORDER`]**, not
//! the first to appear in the command: `ls *.rs 'x'` reports the quote, because
//! a fixed order is the only way the reason is a function of the command rather
//! than of where a byte happens to fall.
//!
//! The sentences stay content-free the way every other reason here does, and
//! one step harder: [`UnmodelledSyntax::reason`] is a `const fn` over a closed
//! set, so what reaches an event is chosen at compile time and cannot be
//! assembled from the command (REQ-619 BR-7, LESSON-624's egress-capture
//! posture). The class travels onward as an explicit `Option<&'static str>`
//! beside the unknown bit — [`Verdict::unknown_reason`], then
//! `ToolProvenance::from_bits`, the egress `Provenance`, the taint sink and
//! `SessionPinned::reason` — rather than being re-derived at the notice from
//! the cause word, which would be a second classifier (ADR-620-4, LESSON-653).
//!
//! # Mutation record (conventions.md — show the test can fail)
//!
//! Swapping [`UNMODELLED_ORDER`] so `Glob` precedes `Quote` turns
//! [`tests::each_unmodelled_class_names_itself_and_nothing_else`] red on its
//! precedence row (`ls *.rs 'ZQX9'`) and **nothing else in this crate's lib
//! suite** — measured, 1 of 2,237: the eight per-class rows are each
//! single-class by construction, so the order is only observable where two
//! classes meet. Run 2026-09-09, red, reverted.
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
//! REQ-620's strip was mutated six ways and the counts live with the
//! recogniser ([`super::shell_syntax`]'s module docs), because that is the
//! module that owns the rule. Two results belong here. Making
//! [`super::shell_syntax::strip_null_redirects`] a no-op reds **ten** tests
//! in this module (re-measured at the Phase-5 re-verify, from eight; the two
//! that had been missed are
//! [`tests::a_redirect_glued_to_its_verb_is_unknown_and_one_glued_to_a_separator_is_not`]
//! and [`tests::the_toolkit_preamble_shapes_are_rooted_and_the_old_ones_are_not`])
//! and leaves [`tests::every_other_redirect_stays_unmodelled`]
//! green — correctly, since a no-op preserves exactly the refusal that test
//! asserts, and a widening that broke BR-2 would have to be a different
//! mutation. That different mutation is the third: lifting any word carrying
//! `>` or `<` reds `every_other_redirect_stays_unmodelled` *and* REQ-614's own
//! [`tests::adversarial_spellings_are_all_unknown`], on `cat <src/main.rs`.
//! One widening, both suites — which is the evidence that the REQ-614 grammar
//! is still the thing being widened rather than replaced.
//!
//! Making [`reads_only_its_stdin`] answer `true` unconditionally — the
//! allowlist accepting everything, which is the polarity C1 inverted — turns
//! **exactly one** test red: [`tests::the_piped_exemption_is_a_closed_allowlist`],
//! on the **first** of its must-fire rows. One test, because that table is the
//! only place the piped exemption's *boundary* is asserted;
//! [`tests::a_piped_reader_with_no_path_reads_stdin_not_the_root`] asserts the
//! exemption fires and a mutation that widens it cannot move that. And one
//! *row*, because the `assert_eq!` aborts the loop — the earlier reading of
//! this record said "all sixteen of its must-fire rows", which is what the
//! table would report if it collected failures and is not what a run shows.
//! Re-measured after the Phase-5 re-verify restructured the function.
//!
//! Dropping the `--` guard from the head of [`reads_only_its_stdin`] — the
//! Phase-5 re-verify's own addition — reds the same one test, on its
//! `wc --files0-from -` row. `grep`'s long options survive that mutation
//! through the per-word arm, so the guard's own coverage is exactly the filter
//! rows.

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
    /// [`Self::reason`], but only for an `Unknown` verdict — the shape
    /// [`ToolProvenance::from_bits`](crate::harness::ToolProvenance::from_bits)
    /// takes (REQ-620 ADR-620-4).
    ///
    /// `Some(reason)` **is** the unknown bit and its cause at once, which is
    /// what stops a caller from passing one without the other. A `Rooted` or
    /// `BoundaryTouch` verdict answers `None`: neither is opaque, and a
    /// boundary touch's cause is the path, already named by `privacy_block`.
    #[must_use]
    pub fn unknown_reason(&self) -> Option<&'static str> {
        match self.kind {
            VerdictKind::Unknown => Some(self.reason),
            VerdictKind::Rooted | VerdictKind::BoundaryTouch => None,
        }
    }

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

/// Which of [`UNMODELLED`]'s shapes refused a command (REQ-620 BR-6).
///
/// The `UnmodelledSyntax` entity. One sentence per class rather than one
/// sentence for all of them, because the single sentence — "the command uses
/// shell syntax this classifier does not model" — told a user, and a model,
/// nothing they could act on: the model that pinned the 2026-09-09 `/analyze`
/// session could not tell from it that a `2>&1` was the offending byte, and
/// nor could the user reading the pin notice.
///
/// # Content-free by construction
///
/// [`Self::reason`] is a `const fn` returning a `&'static str` from a closed
/// set, so the sentence a class names is fixed at compile time and cannot be
/// assembled from the command. That is the whole of BR-6's second clause
/// ("and only the class"): it is a property of the type, not a rule a
/// contributor has to keep. A sentence therefore names the *class* — "a quoted
/// string" — and never the byte, the position or the word it was found in.
/// (The `History` sentence used to read "a history expansion (`!`)", which was
/// the one place that claim was false of its own sentences; the parenthetical
/// went at the Phase-5 verify.)
///
/// # `pub(crate)`, and no ordering derive
///
/// Both narrowed at the Phase-5 verify. Nothing outside the crate names this
/// type — the sentences leave as `&'static str` on a provenance, an event and
/// a notice, never as a discriminant — and `PartialOrd`/`Ord` implied an order
/// this type does not have: the only order that means anything here is
/// [`UNMODELLED_ORDER`], which is a `&[UnmodelledSyntax]` and deliberately not
/// the declaration order a derive would have exposed. A comparison operator
/// that silently answered declaration order would be a second, wrong answer to
/// the question `UNMODELLED_ORDER` exists to answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnmodelledSyntax {
    /// `'` or `"` — a quoted string.
    Quote,
    /// A backtick, or `$(` — a command substitution.
    Substitution,
    /// `$` not opening a substitution — a shell variable.
    Variable,
    /// `>` or `<` that survived [`strip_null_redirects`] — a redirect to
    /// something other than the null device (REQ-620 BR-2).
    Redirect,
    /// `*`, `?` or `[` — a glob.
    Glob,
    /// `{` or `}` — a brace expansion.
    Brace,
    /// `\` — a backslash escape.
    Escape,
    /// `!` — a history expansion.
    History,
}

/// The order [`first_unmodelled_class`] reports in.
///
/// Fixed, and fixed *here* rather than derived from [`UNMODELLED`]'s order or
/// from where the character happens to fall in the command, so the reason a
/// given command draws is a function of the command alone and cannot move when
/// somebody rewrites the character list. A command carrying a quote and a glob
/// reports the quote whichever comes first in its text.
///
/// The order is severity-of-guessing: the classes at the top are the ones that
/// would make a lexer of this grammar (quoting, substitution, expansion), and
/// the ones at the bottom are the narrow spellings.
pub(crate) const UNMODELLED_ORDER: &[UnmodelledSyntax] = &[
    UnmodelledSyntax::Quote,
    UnmodelledSyntax::Substitution,
    UnmodelledSyntax::Variable,
    UnmodelledSyntax::Redirect,
    UnmodelledSyntax::Glob,
    UnmodelledSyntax::Brace,
    UnmodelledSyntax::Escape,
    UnmodelledSyntax::History,
];

impl UnmodelledSyntax {
    /// The content-free sentence this class refuses with — the value that rides
    /// `skill_invoked.reach_reason`, the `session_pinned` event and the CLI's
    /// pin notice (BR-6).
    ///
    /// Eight distinct sentences in one shape, so a reader who has seen one has
    /// read them all and the class is the only thing that varies.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Quote => "the command uses a quoted string this classifier does not model",
            Self::Substitution => {
                "the command uses a command substitution this classifier does not model"
            }
            Self::Variable => "the command uses a shell variable this classifier does not model",
            Self::Redirect => {
                "the command uses a redirect other than to /dev/null this classifier does not model"
            }
            Self::Glob => "the command uses a glob this classifier does not model",
            Self::Brace => "the command uses a brace expansion this classifier does not model",
            Self::Escape => "the command uses a backslash escape this classifier does not model",
            Self::History => "the command uses a history expansion this classifier does not model",
        }
    }

    /// This class's bit in [`first_unmodelled_class`]'s presence set.
    ///
    /// Written out rather than taken from `self as u16`. A discriminant is an
    /// accident of declaration order: a ninth class declared *above* an
    /// existing one would silently renumber every bit, and a ninth declared
    /// below would index past a fixed-width set. Spelling the bit makes both
    /// a compile error instead.
    const fn bit(self) -> u16 {
        match self {
            Self::Quote => 1 << 0,
            Self::Substitution => 1 << 1,
            Self::Variable => 1 << 2,
            Self::Redirect => 1 << 3,
            Self::Glob => 1 << 4,
            Self::Brace => 1 << 5,
            Self::Escape => 1 << 6,
            Self::History => 1 << 7,
        }
    }
}

/// The first class of [`UNMODELLED`] syntax present in `command`, in
/// [`UNMODELLED_ORDER`], or `None` when every byte is modelled.
///
/// One walk, collecting which classes are present into a bit set, then one
/// lookup in the order: the alternative — eight walks, short-circuiting on the
/// first hit — reports the same class and reads as eight rules instead of one.
/// `$` is the only character whose class depends on its neighbour (`$(` is a
/// substitution and `$f` a variable), which is why the walk is over a peekable
/// iterator rather than over [`UNMODELLED`] itself.
///
/// Every character in [`UNMODELLED`] maps to exactly one class — asserted by
/// [`tests::each_unmodelled_class_names_itself_and_nothing_else`], so a
/// character added to that list without a class here is a compile-green
/// refusal with no sentence, which this function would otherwise report as
/// "modelled".
fn first_unmodelled_class(command: &str) -> Option<UnmodelledSyntax> {
    // [`UNMODELLED`] is still the definition of "this grammar does not model
    // it"; the classes below only *explain* which of its characters was found.
    // Reading the list here rather than letting the arms below stand in for it
    // is what keeps the two in one relationship instead of two lists that drift
    // — and it is the early-out for the overwhelmingly common command that
    // carries none of them.
    if !command.chars().any(|c| UNMODELLED.contains(&c)) {
        return None;
    }
    let mut present: u16 = 0;
    let mut chars = command.chars().peekable();
    while let Some(ch) = chars.next() {
        let class = match ch {
            '\'' | '"' => UnmodelledSyntax::Quote,
            '`' => UnmodelledSyntax::Substitution,
            '$' => {
                if chars.peek() == Some(&'(') {
                    UnmodelledSyntax::Substitution
                } else {
                    UnmodelledSyntax::Variable
                }
            }
            '>' | '<' => UnmodelledSyntax::Redirect,
            '*' | '?' | '[' => UnmodelledSyntax::Glob,
            '{' | '}' => UnmodelledSyntax::Brace,
            '\\' => UnmodelledSyntax::Escape,
            '!' => UnmodelledSyntax::History,
            _ => continue,
        };
        present |= class.bit();
    }
    UNMODELLED_ORDER
        .iter()
        .copied()
        .find(|class| present & class.bit() != 0)
}

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

    // REQ-620 BR-6: the scan names the class it refused on rather than
    // answering one sentence for all eight. The order is
    // [`UNMODELLED_ORDER`]'s, not the command's.
    if let Some(class) = first_unmodelled_class(command) {
        return Verdict::unknown(class.reason());
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

    // ADR-620-2 step 3: the split records the separator that preceded each
    // segment, because the root-walk rule inside `classify_segment` needs to
    // know what the segment's stdin is and nothing else in the segment's text
    // says. Empty segments are skipped exactly as REQ-614 skipped them.
    for (position, segment) in split_segments(command) {
        if segment.trim().is_empty() {
            continue;
        }
        match classify_segment(&scope, position, segment, &mut sources, &mut evidence) {
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

/// What a segment's **stdin** is, which is the only thing the separator before
/// it tells this grammar (REQ-620 BR-4).
///
/// Not "is there a pipe anywhere in the command": `ls | grep foo; head -5` has
/// a pipe and its third segment is [`Self::First`], because a `;` hands the
/// next command the terminal's stdin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SegmentPosition {
    /// The start of the command, or after `;`, `&&`, `||`, a lone `&`, `(`,
    /// `)` or a newline. Stdin is the terminal's or nothing, so a content verb
    /// with no explicit file reads the root — REQ-614 BR-1(d), unchanged.
    First,
    /// After exactly one `|`. Stdin is the previous segment's stdout, and that
    /// segment was classified on its own paths.
    Piped,
}

/// Split a command into segments, each carrying the position its preceding
/// separator gave it (ADR-620-2 step 3).
///
/// Separators are tokenised **longest-match-first**, which is the whole reason
/// this replaced REQ-614's `command.split(['|', ';', '&', '(', ')', '\n'])`.
/// That split saw `||` as two `|` separators with an empty segment between, and
/// `&&` as two `&`s — invisible in a verdict, since the empty segment is
/// skipped and both spellings separate, and wrong the moment a segment's
/// position is read from its separator. An *or* is not a pipe.
///
/// Empty segments are returned rather than filtered, so the caller keeps
/// REQ-614's `trim().is_empty()` skip and this function stays a pure statement
/// about separators.
///
/// Byte indexing is safe here without a char-boundary check: every separator is
/// ASCII, so no match can begin inside a multi-byte character, and the only
/// indices this slices at are match positions.
fn split_segments(command: &str) -> Vec<(SegmentPosition, &str)> {
    let bytes = command.as_bytes();
    let mut segments = Vec::new();
    let mut position = SegmentPosition::First;
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let separator = match bytes[i] {
            b'|' if bytes.get(i + 1) == Some(&b'|') => Some((2, SegmentPosition::First)),
            b'&' if bytes.get(i + 1) == Some(&b'&') => Some((2, SegmentPosition::First)),
            b'|' => Some((1, SegmentPosition::Piped)),
            b';' | b'&' | b'(' | b')' | b'\n' => Some((1, SegmentPosition::First)),
            _ => None,
        };
        let Some((width, next)) = separator else {
            i += 1;
            continue;
        };
        segments.push((position, &command[start..i]));
        position = next;
        i += width;
        start = i;
    }
    segments.push((position, &command[start..]));
    segments
}

/// The verbs whose **only** input, when they were handed no existing file, is
/// their stdin — the closed allowlist BR-4's piped exemption is drawn over.
///
/// Every entry is also in [`READS_CONTENT`] (asserted by
/// [`tests::the_piped_allowlist_is_a_subset_of_the_content_verbs`]): the
/// exemption can only ever remove the root walk from a verb that would
/// otherwise have taken it. What is *not* here is the point — `sed` and `awk`
/// take a `-f script` that can open any path, `diff` needs two operands, and
/// `cat` names paths rather than patterns, so a `cat` whose file does not exist
/// is a typo the walk should still account for. All four keep the walk.
///
/// # Membership is necessary and not sufficient (Phase-5 re-verify)
///
/// Being on this list *used* to be the whole test, and the list was therefore
/// flag-blind: `ls | wc --files0-from -` and `ls | sort --files0-from -` read a
/// NUL-separated list of **file names** off stdin and then open every one of
/// them, and `ls | shasum -c -` reads a checksum list and opens every file it
/// names. Each took the exemption and came back `rooted` for a command that can
/// read any file under the root. So [`reads_only_its_stdin`] carries a guard
/// over the **whole** allowlist — no word may start with `--`, for any verb —
/// and a `-c` refusal for the two checksum verbs. The verb names below say
/// which programs *may* qualify; the flags decide whether this call does.
///
/// **`less` and `more` are not here** and were, until the same pass. Both are
/// pagers rather than filters: they take commands from the terminal (`:e path`
/// opens another file, `!cmd` shells out) and `less` honours `LESSOPEN`, an
/// environment-set preprocessor that runs on whatever it is handed. Nothing in
/// this grammar can see any of that, and a pager on the end of a pipe is not a
/// shape a model writes for output it means to consume — so they keep the walk,
/// which is the pre-REQ-620 answer for both.
const PIPED_STDIN_ONLY: &[&str] = &[
    "head", "tail", "wc", "sort", "uniq", "cut", "nl", "tr", "md5", "shasum",
];

/// Whether a `Piped` segment's verb reads **nothing but its stdin** — the
/// condition BR-4's exemption is granted on.
///
/// # The polarity is an allowlist, and that is the whole safety argument
///
/// This function used to be `reads_tree`, a **denylist** of the recursive
/// `grep` spellings, inside a module whose opening paragraph says an allowlist
/// is the only shape a reach grammar may take. It was the leak that shape
/// exists to prevent: `ls | grep --directories recurse SECRET`,
/// `--dir recurse`, `--dereference-recursive` and `--rec` are all spellings GNU
/// `grep` accepts for recursion, none of them was in the denylist, and each
/// skipped the root walk and came back `Rooted` for a command that reads every
/// file under the root. (The doc comment claimed "the forms are the ones GNU
/// and BSD `grep` accept"; GNU `grep` accepts `--directories=recurse`,
/// `--dereference-recursive`, and any unambiguous abbreviation of a long
/// option, so the claim was false as written and the denylist could not have
/// been completed by adding rows.)
///
/// So the question is inverted. The exemption is granted only to
/// [`PIPED_STDIN_ONLY`], plus `grep` and its two aliases, and in both cases only
/// when the **flags** agree.
///
/// # The long-option guard sits over the whole allowlist (Phase-5 re-verify)
///
/// The rule was first written as three `grep`-only clauses, and the verb list
/// above was consulted with no reference to its arguments at all. That was the
/// same defect one verb over: `wc --files0-from -` and `sort --files0-from -`
/// read a NUL-separated list of **file names** off stdin and open every one,
/// and `shasum -c -` reads a checksum list and opens every file it names. All
/// three were on the list, all three took the exemption, and all three came
/// back `Rooted` for a command that reads any file under the root.
///
/// So `--` is refused for **every** verb, `grep` included, and the reason is
/// the same one that inverted the polarity: a long option is a reach nobody has
/// enumerated, and an allowlist may not pass what it has not read. On top of
/// that:
///
/// * no word is a single-`-` cluster carrying `d`, `r` or `R` for a `grep` —
///   `-r`, `-R`, `-rn`, `-nR`, `-d recurse`, and (the spelling the first version
///   missed) `-nd recurse`, `-id skip`, `-drecurse`, where `-d` is *inside* a
///   cluster rather than a word of its own;
/// * `shasum` and `md5` refuse `-c`, which is the "verify a checksum list"
///   mode — the one short flag on this allowlist that opens files.
///
/// Every other content verb, and every `grep` flag this list does not
/// enumerate, keeps the root walk. A miss is therefore the pre-REQ-620 answer
/// (`Unknown` on a root the walk cannot clear), which is the module's standing
/// rule: the classifier may only be more permissive than the old daemon by an
/// amount it can prove.
fn reads_only_its_stdin(verb: &str, rest: &[&str]) -> bool {
    // One guard, over the whole allowlist and before any verb is consulted: a
    // long option is a reach this grammar has not read, whoever it belongs to.
    if rest.iter().any(|word| word.starts_with("--")) {
        return false;
    }
    if PIPED_STDIN_ONLY.contains(&verb) {
        // The one short flag on the filter half that opens files: `-c` puts
        // both checksum verbs into "read this list and verify every path in it".
        return !(matches!(verb, "shasum" | "md5") && rest.contains(&"-c"));
    }
    if !matches!(verb, "grep" | "egrep" | "fgrep") {
        return false;
    }
    rest.iter().all(|word| {
        match word.strip_prefix('-') {
            // `d` is read inside the cluster, not as a whole word: `-nd recurse`
            // is `-n -d recurse` to `grep` and was recursion this exemption
            // granted (Phase-5 re-verify).
            Some(flags) => !flags.chars().any(|c| matches!(c, 'd' | 'r' | 'R')),
            // A pattern, or any other operand: not a flag, so not recursion.
            None => true,
        }
    })
}

enum SegmentVerdict {
    Rooted,
    BoundaryTouch,
    Unknown(&'static str),
}

/// One `|`/`;`/`&`-separated segment, and the stdin its separator gave it.
///
/// **The fallthrough is `Unknown`** — see the module docs. Every verb this does
/// not recognise, every path form it cannot resolve, every scan that ran out of
/// budget lands here.
fn classify_segment(
    scope: &Scope<'_>,
    position: SegmentPosition,
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

    // BR-1(d): a content-reading verb reads its **default source** when it
    // names no path at all, and may be reading it when none of its tokens named
    // an existing file (see `named_an_existing_file` above). This is the
    // grammar's answer to "was that token a pattern or a path?", which it
    // refuses to decide with a per-verb option table (ADR-614-1): given at
    // least one existing file the verb was given explicit files, and given none
    // its reach is whatever it falls back to.
    let reads_its_default_source = paths.is_empty() || !named_an_existing_file;

    // REQ-620 BR-4 / ADR-620-3. For a `Piped` segment that default source is
    // the previous segment's stdout — bytes the previous segment was already
    // classified on — so the root walk below is a walk for a read that cannot
    // happen. `reads_only_its_stdin` is the allowlist that grants the
    // exemption: everything it does not name — `sed`, `awk`, `diff`, `cat`, and
    // every `grep` flag that could be recursion — keeps the walk.
    //
    // The exemption is expressed over the same "was it given explicit files"
    // question BR-1(d) asks, because that is what BR-4's "no path argument"
    // means in this module's vocabulary: `git log | grep fix` and
    // `cat README.md | grep foo` name a *pattern*, not a path, and the REQ
    // lists both as reading their stdin. A segment that *did* name an existing
    // file never reaches this line anyway — it was given files, and they are in
    // `sources`.
    let reads_stdin_not_the_root = position == SegmentPosition::Piped
        && reads_its_default_source
        && reads_only_its_stdin(verb, &rest);

    if reads_content
        && reads_its_default_source
        && !saw_directory
        && !reads_stdin_not_the_root
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

/// Whether `phrase` is a verb this grammar recognises as reading names,
/// contents, or nothing — i.e. one it can classify without falling through to
/// `Unknown` (REQ-620 TASK-406).
///
/// **One reader, and it is a cross-check, not a gate.** Nothing in the
/// classifier calls this: [`classify_segment`] consults the tables directly and
/// its fallthrough is what actually makes an unrecognised verb `Unknown`. It
/// exists so that the model-facing paragraph in
/// [`SHELL_REACH_CONTRACT`](super::shell::SHELL_REACH_CONTRACT) — which names
/// example verbs to a model that will then write them — can be held against
/// these tables by a test in the module that owns the paragraph. A contract
/// naming a verb this grammar refuses would teach the model to pin itself,
/// which is worse than a contract that names none (LESSON-542: a grammar taught
/// to the model must be read on every path it can answer through).
///
/// `git <sub>` is accepted as a two-word phrase against [`GIT_NAME_ONLY`],
/// because that is the shape the contract names (`git status`) and the shape
/// [`classify_segment`] reads.
///
/// `#[cfg(test)]` because the cross-check is its only reader: production code
/// must go through [`classify_segment`], whose fallthrough is the real gate, and
/// a second table-reader on the production path is how a grammar comes to have
/// two answers (LESSON-494).
#[cfg(test)]
#[must_use]
pub(crate) fn is_recognised_verb(phrase: &str) -> bool {
    let mut words = phrase.split_whitespace();
    let Some(verb) = words.next() else {
        return false;
    };
    let sub = words.next();
    if words.next().is_some() {
        return false;
    }
    match (verb, sub) {
        ("git", Some(sub)) => GIT_NAME_ONLY.contains(&sub),
        (verb, None) => {
            READS_NOTHING.contains(&verb)
                || NAME_ONLY.contains(&verb)
                || READS_CONTENT.contains(&verb)
        }
        _ => false,
    }
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
            // AC-9's flipped rows, added at the Phase-5 verify: two toolkit
            // shapes that were `Unknown` before REQ-620 and are `Rooted` now.
            // The AC says the rows that flip must "say why" — these two are
            // that, and without them the table asserts only the shapes the
            // toolkit had already been rewritten to avoid.
            //
            // The first is the redirect widening (BR-1): the `2>/dev/null` is
            // lifted and `cat <path> || echo none` is the grammar it always
            // was. The second is the pipeline widening (BR-4): `head` after a
            // `|` reads the previous segment's output, not the root.
            "cat .adlc/context/architecture.md 2>/dev/null || echo none",
            "ls .adlc/partials/ | head",
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
        // The old shapes, still `Unknown` — and the first three **for the
        // quote**, which is the assertion the Phase-5 verify added. Each of
        // them also carries a `2>/dev/null` or a glob, so a table asserting
        // only `Unknown` would have gone on passing had REQ-620's strip
        // wrongly cleared them: the reason is what says the quote is what is
        // still refusing the command.
        for (old, class) in [
            (
                r#"test -s .adlc/ETHOS.md && cat .adlc/ETHOS.md || echo "No ethos found — run /init to vendor .adlc/ETHOS.md""#,
                Some(UnmodelledSyntax::Quote),
            ),
            (
                r#"cat .adlc/context/architecture.md 2>/dev/null || echo "No architecture context found""#,
                Some(UnmodelledSyntax::Quote),
            ),
            (
                r#"grep -rl 'status: draft\|status: approved' .adlc/specs/*/requirement.md 2>/dev/null | head -20 || echo "No active specs""#,
                Some(UnmodelledSyntax::Quote),
            ),
            // These two carry no unmodelled byte at all: the first is refused
            // on the `git` subcommand table and the second on a `~/` path
            // outside the root, so neither has a class to name.
            ("git diff main --stat || echo No diff available", None),
            (
                "cat .adlc/templates/task-template.md || cat ~/.claude/skills/templates/task-template.md || echo none",
                None,
            ),
        ] {
            let v = verdict(&root, old);
            assert_eq!(v.kind, VerdictKind::Unknown, "{old:?} should be Unknown");
            if let Some(class) = class {
                assert_eq!(
                    v.reason,
                    class.reason(),
                    "{old:?} is refused on its quote, not on its redirect"
                );
            }
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

    /// Whether `reason` is one of the eight [`UnmodelledSyntax`] sentences.
    ///
    /// The accepted redirect forms must carry **none** of them (LESSON-550:
    /// assert the absence, not the remedy), and "none of the eight" is the
    /// absence to assert now that the refusal is not one sentence.
    fn is_unmodelled_reason(reason: &str) -> bool {
        UNMODELLED_ORDER.iter().any(|c| c.reason() == reason)
    }

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
    /// REQ-614 put it, beside the [`UnmodelledSyntax`] class each one refuses
    /// on. The three `/dev/null`-adjacent rows are LESSON-494's failure mode
    /// written out: one byte's difference between the gate and the shell that
    /// runs the command.
    ///
    /// The last two rows are why the class is a column rather than a constant.
    /// `2>$f` carries a redirect **and** a variable and `> "$f"` carries a
    /// redirect, a variable and a quote; under [`UNMODELLED_ORDER`] they report
    /// the variable and the quote, because the order is fixed and does not ask
    /// which byte the reader was thinking about. Both are still `Unknown`,
    /// which is the half BR-2 is about.
    const REDIRECT_LOOKALIKES: &[(&str, UnmodelledSyntax)] = &[
        ("> out.txt", UnmodelledSyntax::Redirect),
        (">> log", UnmodelledSyntax::Redirect),
        ("< input", UnmodelledSyntax::Redirect),
        ("2>/dev/nul", UnmodelledSyntax::Redirect),
        ("2>/dev/null/x", UnmodelledSyntax::Redirect),
        ("2>/dev/nullx", UnmodelledSyntax::Redirect),
        ("2>$f", UnmodelledSyntax::Variable),
        ("> \"$f\"", UnmodelledSyntax::Quote),
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
    /// **Mutation (run, re-measured 2026-09-09, red, reverted):** make
    /// [`strip_null_redirects`](super::shell_syntax::strip_null_redirects) a
    /// no-op — this reds first of eight, on the `ls 2>&1 && echo ok` residue
    /// row. Drop the whole-word rule (`from_operator` accepting any operator
    /// ending in `>`) — this reds, one of two, on the `ls>/dev/null` row.
    #[test]
    fn null_redirects_are_lifted_before_the_scan_and_the_split() {
        // The lift itself: whole words out, separators standing. The rows
        // are `shell_syntax`'s own (`STRIP_ROWS`), read from one place since
        // the Phase-5 verify: this table and that one held six rows in common
        // and had already come to disagree on `ls &>/dev/null || echo no`.
        for (command, residue, lifted) in super::super::shell_syntax::STRIP_ROWS.iter().copied() {
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

    /// **The glued forms, at the verdict rather than at the strip.**
    ///
    /// *(Phase-5 verify.)* `shell_syntax`'s own table asserts what the strip
    /// leaves behind; this asserts what the **classifier** answers, which is
    /// the value every consumer reads. Two claims, and they point opposite
    /// ways on purpose:
    ///
    /// * A redirect glued to its **verb** (`ls>/dev/null`, `ls>&1`) is a word
    ///   the recogniser does not accept, so the unmodelled scan sees its `>`
    ///   and refuses the command on the redirect class — the pre-REQ-620
    ///   answer, and the REQ's Deferred section says so.
    /// * A redirect glued to a following **separator** (`ls 2>&1;ls`,
    ///   `ls 2>&1|head`) *is* peeled, and the command classifies as though the
    ///   separator had been spaced. That is M1's widening, and it is asserted
    ///   here because the write gate depends on it: the pre-REQ-620 gate
    ///   allowed `cmd 2>&1|head` at a home root and the first REQ-620 gate
    ///   refused it.
    ///
    /// **Mutation (run, red, reverted):** delete [`split_glued_redirect`]'s
    /// call from `strip_line` — **7 red**, this test's second half among them,
    /// with `root_gate`'s two tables (the benign separator-glued rows and the
    /// cross-gate differential), `shell_syntax`'s strip table, and three of
    /// this module's redirect tests. The first half stays green, correctly:
    /// dropping the peel cannot make a verb-glued form *more* modelled.
    #[test]
    fn a_redirect_glued_to_its_verb_is_unknown_and_one_glued_to_a_separator_is_not() {
        let root = redirect_root("glued");

        for command in ["ls>/dev/null", "ls>&1", "cat README.md>/dev/null"] {
            let v = verdict(&root, command);
            assert_eq!(
                v.kind,
                VerdictKind::Unknown,
                "`{command}` is glued to its verb and stays unmodelled ({})",
                v.reason
            );
            assert_eq!(
                v.reason,
                UnmodelledSyntax::Redirect.reason(),
                "`{command}` is refused by the scan on its surviving `>`"
            );
        }

        for command in ["ls 2>&1;ls", "ls 2>&1|head", "ls 2>&1; ls"] {
            let v = verdict(&root, command);
            assert_eq!(
                v.kind,
                VerdictKind::Rooted,
                "`{command}` peels at the separator and classifies as spaced ({})",
                v.reason
            );
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// **Phase-5 verify, C2: `&>` is bash, and the executor's `sh` may be
    /// `dash`.**
    ///
    /// `dash` is `/bin/sh` on the Linux CI leg, and it reads the `&` of
    /// `&>/dev/null` as a command separator with the `>/dev/null` attached to
    /// the command before it. So
    /// `ls &>/dev/null grep -r SECRET . 1>&2` is **two** commands there, and
    /// the second is a recursive `grep` over the session root. Lifting the word
    /// whole made the residue a single segment whose verb was `ls`; the `grep`
    /// arrived as an argument, met no verb check, and the command came back
    /// `Rooted` on a root holding a `.env`.
    ///
    /// Re-emitting the separator reproduces `dash`'s parse. It is also strictly
    /// more conservative than `bash`'s, where the same text is one command: a
    /// separator can only ever split a segment in two, and both halves are
    /// classified in full.
    ///
    /// **Mutation (run, red, reverted):** drop the [`NullRedirect::BothStreams`]
    /// arm from `NullRedirect::residue_separator` so the form lifts to nothing
    /// again — this test reds on both assertions, and
    /// `shell_syntax::tests::the_strip_lifts_words_and_leaves_the_separators_standing`
    /// reds on its three `&>` residue rows.
    #[test]
    fn the_both_streams_form_re_emits_the_separator_it_hides() {
        let root = piped_root("both-streams");
        const COMMAND: &str = "ls &>/dev/null grep -r SECRET . 1>&2";

        let residue = strip_null_redirects(COMMAND).residue;
        let segments: Vec<&str> = split_segments(&residue)
            .into_iter()
            .map(|(_, segment)| segment.trim())
            .filter(|segment| !segment.is_empty())
            .collect();
        assert_eq!(
            segments,
            vec!["ls", "grep -r SECRET ."],
            "`&>` must leave the separator `dash` reads it as"
        );

        let v = verdict(&root, COMMAND);
        assert_eq!(
            v.kind,
            VerdictKind::Unknown,
            "the second command is a recursive grep over a root holding a .env ({})",
            v.reason
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
    /// **Mutation (run, re-measured 2026-09-09, red, reverted):** make the
    /// recogniser non-total — lift any word carrying `>` or `<` — and this
    /// reds on `ls > out.txt`, which comes back `Rooted`. It is one of seven,
    /// and REQ-614's own
    /// [`tests::adversarial_spellings_are_all_unknown`] is another, on
    /// `cat <src/main.rs`: the same widening reaches both suites, which is what
    /// makes the recogniser's totality a property and not a comment. A no-op
    /// strip leaves this test green, correctly — it asserts the refusal a
    /// no-op preserves.
    #[test]
    fn every_other_redirect_stays_unmodelled() {
        let root = redirect_root("lookalikes");
        for verb in REDIRECT_TABLE_VERBS {
            for (lookalike, class) in REDIRECT_LOOKALIKES {
                let command = format!("{verb} {lookalike}");
                let v = verdict(&root, &command);
                assert_eq!(
                    v.kind,
                    VerdictKind::Unknown,
                    "`{command}` must stay unmodelled ({})",
                    v.reason
                );
                assert_eq!(
                    v.reason,
                    class.reason(),
                    "`{command}` should carry {class:?}'s sentence"
                );
            }
        }
        // A here-doc and a process substitution, named by BR-2 and refused by
        // the same scan — both on the redirect class, since neither carries a
        // quote, a substitution or a variable.
        for exotic in ["cat << EOF", "diff <(ls) <(ls)"] {
            assert_eq!(
                verdict(&root, exotic).reason,
                UnmodelledSyntax::Redirect.reason(),
                "`{exotic}` must stay unmodelled"
            );
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// A string that appears **only** in a test command, never in this file's
    /// sentences, so "the reason carries no byte of the command" can be
    /// asserted rather than reasoned about (LESSON-624's egress-capture
    /// posture, applied to a reason instead of a payload).
    const COMMAND_MARKER: &str = "ZQX9";

    /// One command per [`UnmodelledSyntax`] class, each carrying **that class
    /// alone** plus [`COMMAND_MARKER`].
    ///
    /// Single-class on purpose: a row that carried two would assert the
    /// precedence rule instead of the class-to-sentence mapping, and the
    /// precedence is asserted separately below where a mutation to
    /// [`UNMODELLED_ORDER`] can be seen to move it.
    const ONE_PER_CLASS: &[(&str, UnmodelledSyntax)] = &[
        ("ls 'ZQX9'", UnmodelledSyntax::Quote),
        ("ls $(echo ZQX9)", UnmodelledSyntax::Substitution),
        ("ls $ZQX9", UnmodelledSyntax::Variable),
        ("ls > ZQX9", UnmodelledSyntax::Redirect),
        ("ls ZQX9*", UnmodelledSyntax::Glob),
        ("ls {ZQX9}", UnmodelledSyntax::Brace),
        ("ls ZQX9\\x", UnmodelledSyntax::Escape),
        ("ls ZQX9!", UnmodelledSyntax::History),
    ];

    /// One command per **adjacent** pair of [`UNMODELLED_ORDER`], carrying
    /// exactly those two classes and answering the earlier one.
    ///
    /// Seven rows for eight entries. Adjacency is what makes the table a pin on
    /// the whole order: any single transposition of the order moves exactly one
    /// of these rows, whereas a table of far-apart pairs is cleared by
    /// transpositions in between (Phase-5 verify — the two precedence rows this
    /// replaced left `Brace`/`Escape` free to swap with nothing going red).
    const ADJACENT_PAIRS: &[(&str, UnmodelledSyntax)] = &[
        // Quote > Substitution
        ("echo \"x\" `y`", UnmodelledSyntax::Quote),
        // Substitution > Variable
        ("echo $(x) $y", UnmodelledSyntax::Substitution),
        // Variable > Redirect
        ("echo $x > y", UnmodelledSyntax::Variable),
        // Redirect > Glob
        ("echo > y *", UnmodelledSyntax::Redirect),
        // Glob > Brace
        ("echo * {a,b}", UnmodelledSyntax::Glob),
        // Brace > Escape
        ("echo {a} \\x", UnmodelledSyntax::Brace),
        // Escape > History
        ("echo \\x !y", UnmodelledSyntax::Escape),
    ];

    /// **BR-6 / AC-6: eight classes, eight distinct sentences, each naming only
    /// its own class — and none of them naming the command.**
    ///
    /// Four claims, and each is a different way the refusal could go wrong:
    ///
    /// 1. **The mapping.** Each single-class command draws its own class's
    ///    sentence. A table rather than eight asserts, so a class added to the
    ///    enum without a row here is a missing row rather than a silent gap.
    /// 2. **Distinctness.** Eight sentences, eight distinct strings. Two
    ///    classes sharing a sentence would be the pre-REQ-620 defect in
    ///    miniature — a reason that does not tell the reader which byte to fix.
    /// 3. **Precedence.** `ls *.rs 'ZQX9'` holds a glob *before* a quote in the
    ///    text and reports the quote, because [`UNMODELLED_ORDER`] decides and
    ///    the command's byte order does not. The `$…>` row proves the order is
    ///    read past its first entry.
    /// 4. **Content-freeness.** No sentence contains [`COMMAND_MARKER`], and no
    ///    sentence contains the command that produced it. The `&'static str`
    ///    type is what makes this true; the assertion is what makes it *stay*
    ///    true if somebody ever reaches for `format!`.
    ///
    /// The totality row is the fifth thing and belongs here rather than in its
    /// own test: [`UNMODELLED`] is the list [`classify_with_budget`] no longer
    /// reads, so a character added to it with no arm in
    /// [`first_unmodelled_class`] would be a *refusal that stopped happening*.
    ///
    /// **Mutation (run, red, reverted):** swap [`UNMODELLED_ORDER`] so `Glob`
    /// precedes `Quote` — this reds on claim 3's first row (`ls *.rs 'ZQX9'`
    /// reports the glob) and on nothing else in the workspace, because the
    /// per-class rows are single-class by construction and every other suite
    /// asserts a `kind` rather than a sentence.
    ///
    /// **Re-measured at the Phase-5 verify, over every adjacent transposition:**
    /// `Quote`↔`Substitution`, `Glob`↔`Brace`, `Brace`↔`Escape` and
    /// `Escape`↔`History` were each swapped in turn and each reds **1**, this
    /// test, on its own [`ADJACENT_PAIRS`] row. Before those rows existed the
    /// `Brace`↔`Escape` swap reddened **nothing at all** — two precedence
    /// asserts pinned two comparisons and left the rest of the order free.
    #[test]
    fn each_unmodelled_class_names_itself_and_nothing_else() {
        let root = redirect_root("classes");

        // 1. The mapping, through the classifier rather than through
        //    `first_unmodelled_class` alone — the sentence has to reach a
        //    `Verdict`, which is what every consumer reads.
        for (command, class) in ONE_PER_CLASS {
            let v = verdict(&root, command);
            assert_eq!(
                v.kind,
                VerdictKind::Unknown,
                "`{command}` must stay unmodelled ({})",
                v.reason
            );
            assert_eq!(
                v.reason,
                class.reason(),
                "`{command}` should carry {class:?}'s sentence"
            );
            assert_eq!(
                v.unknown_reason(),
                Some(class.reason()),
                "an Unknown verdict's reason is the one that rides the pin: `{command}`"
            );
        }
        assert_eq!(
            ONE_PER_CLASS.len(),
            UNMODELLED_ORDER.len(),
            "one command per class, so a new class cannot ship untested"
        );

        // 2. Eight distinct sentences.
        let sentences: BTreeSet<&str> = UNMODELLED_ORDER.iter().map(|c| c.reason()).collect();
        assert_eq!(
            sentences.len(),
            UNMODELLED_ORDER.len(),
            "each class must name itself, not share a sentence: {sentences:?}"
        );

        // 3. Precedence: the order decides, not the command's byte order.
        assert_eq!(
            verdict(&root, "ls *.rs 'ZQX9'").reason,
            UnmodelledSyntax::Quote.reason(),
            "a command with a quote and a glob reports the quote"
        );
        assert_eq!(
            verdict(&root, "ls $ZQX9 > out").reason,
            UnmodelledSyntax::Variable.reason(),
            "a variable outranks a redirect"
        );
        // ... and every **adjacent** pair of the order, so the order is pinned
        // along its whole length rather than at two points (Phase-5 verify).
        // Two rows left seven of the eight entries free to move: swapping
        // `Brace` and `Escape` reddened nothing at all.
        //
        // Each command carries exactly the two classes its row names, so the
        // assertion is about which of *those two* wins and nothing else — and
        // an adjacent pair is the only comparison that can distinguish two
        // orders that differ by one transposition.
        for (command, expected) in ADJACENT_PAIRS {
            let v = verdict(&root, command);
            assert_eq!(
                v.reason,
                expected.reason(),
                "`{command}` holds two classes and {expected:?} comes first in \
                 UNMODELLED_ORDER ({})",
                v.reason
            );
        }
        assert_eq!(
            ADJACENT_PAIRS.len(),
            UNMODELLED_ORDER.len() - 1,
            "one row per adjacent pair, so a ninth class cannot ship with its \
             position unasserted"
        );

        // 4. Content-freeness: the reason names the class and nothing the
        //    command said.
        for (command, _) in ONE_PER_CLASS {
            let reason = verdict(&root, command).reason;
            assert!(
                !reason.contains(COMMAND_MARKER),
                "`{command}`'s reason carried a byte planted in the command: {reason}"
            );
            assert!(
                !reason.contains(command),
                "`{command}`'s reason quoted the command: {reason}"
            );
        }

        // 5. Totality: every character `UNMODELLED` refuses on has a class, so
        //    the scan cannot silently stop refusing one.
        for ch in UNMODELLED {
            let probe = format!("ls x{ch}");
            assert!(
                first_unmodelled_class(&probe).is_some(),
                "`{ch}` is in UNMODELLED with no class in `first_unmodelled_class`, \
                 so a command carrying it would be read as fully modelled"
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
                assert!(
                    !is_unmodelled_reason(v.reason),
                    "`{command}` should not carry any refusal it was cleared of ({})",
                    v.reason
                );
            }
            for (lookalike, class) in REDIRECT_LOOKALIKES {
                let command = format!("{verb} {lookalike}");
                let v = verdict(&root, &command);
                assert_eq!(
                    v.kind,
                    VerdictKind::Unknown,
                    "`{command}` should be Unknown ({})",
                    v.reason
                );
                assert_eq!(v.reason, class.reason(), "`{command}`");
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

    /// The reason a segment that reads the root carries once the walk **hits**.
    /// Asserting it rather than the bare `Unknown` is what keeps the piped rows
    /// below from passing because the fixture happened to be refused for some
    /// other reason.
    const READS_THE_ROOT_REASON: &str =
        "the command reads the root and it could hold a protected file";

    /// A project root the boundary walk **hits**: it holds a `.env`, which
    /// `DEFAULT_BOUNDARIES` matches. It also holds the one file the piped rows
    /// name explicitly.
    ///
    /// LESSON-485 — a fixture that cannot reach the discriminating state is not
    /// a test. Without the `.env` every row below would be `Rooted`, the
    /// `Rooted` rows for the reason they claim and the `Unknown` rows for no
    /// reason at all, and deleting the whole of BR-4 would leave the test green.
    fn piped_root(tag: &str) -> PathBuf {
        let root = project_root(tag);
        std::fs::write(root.join(".env"), "SECRET=1\n").unwrap();
        std::fs::write(root.join("README.md"), "# fixture\n").unwrap();
        root
    }

    /// **BR-4 / AC-5: a content verb after a `|` reads its stdin, and after
    /// anything else it still reads the root.**
    ///
    /// The two halves are the rule; either alone is not. A piped `head -5` has
    /// no file to read but the bytes `ls src` produced, and `ls src` was
    /// classified — so walking the root for it is a walk for a read that cannot
    /// happen, and on this fixture it is a walk that *finds something*. The
    /// second half is the must-not-fire one (LESSON-440) and it is where the
    /// separator table earns its keep: `;`, `&&`, `||` and a lone `&` all hand
    /// the next command the terminal's stdin, so `ls || head -5` is a `First`
    /// segment reading the root even though the command contains two `|` bytes.
    ///
    /// `git log | grep fix` names a token, `fix`, and it is still a stdin read:
    /// `fix` is a *pattern*, and BR-1(d) already answers "pattern or path?" with
    /// "did any token name an existing file" rather than a per-verb option table
    /// (ADR-614-1). The REQ lists this command and `cat README.md | grep foo`
    /// among the reads-its-stdin shapes for that reason.
    ///
    /// **Mutation (run, red, reverted):** make [`split_segments`] return
    /// [`SegmentPosition::Piped`] for every separator — this reds here, on
    /// `ls; head -5` coming back `Rooted`, and in
    /// [`tests::only_a_single_pipe_makes_the_next_segment_piped`]. Two, and no
    /// more: the first segment of a command has no separator before it, so
    /// `head -5` alone is `First` under the mutation too and cannot catch it.
    #[test]
    fn a_piped_reader_with_no_path_reads_stdin_not_the_root() {
        let root = piped_root("piped");

        for command in [
            "ls src | head -5",
            "ls src | wc -l",
            "git log | grep fix",
            "cat README.md | grep foo",
            // The residue of a stripped redirect is still a pipe (TASK-403).
            "ls src 2>/dev/null | head -5",
            // Two pipes: both readers are piped.
            "ls src | grep foo | wc -l",
        ] {
            let v = verdict(&root, command);
            assert_eq!(
                v.kind,
                VerdictKind::Rooted,
                "`{command}` reads the previous segment's output, not the root ({})",
                v.reason
            );
        }

        // The other half: after anything but a single `|`, stdin is the
        // terminal's and BR-1(d) is unchanged. The first row is the vacuity
        // floor — it proves the walk on this fixture *hits*, so the rows above
        // are `Rooted` by the exemption and not by a clean tree.
        for command in [
            "head -5",
            "ls; head -5",
            "ls && head -5",
            "ls || head -5",
            "ls & head -5",
            "ls\nhead -5",
        ] {
            let v = verdict(&root, command);
            assert_eq!(
                v.kind,
                VerdictKind::Unknown,
                "`{command}` runs `head` on the terminal's stdin ({})",
                v.reason
            );
            assert_eq!(
                v.reason, READS_THE_ROOT_REASON,
                "`{command}` should be refused by the root walk"
            );
        }

        // BR-4's stated limit, not a gap: the exemption is about the piped
        // segment, and `cat missing` is the *first* one.
        let v = verdict(&root, "cat missing | head");
        assert_eq!(
            v.kind,
            VerdictKind::Unknown,
            "`cat missing` walks in its own right ({})",
            v.reason
        );
        assert_eq!(v.reason, READS_THE_ROOT_REASON);

        // And a boundary is not hidden by a pipe: BUG-216's precedence holds.
        let v = verdict(&root, "cat .env | head -5");
        assert_eq!(
            v.kind,
            VerdictKind::BoundaryTouch,
            "a protected file named before the pipe is still a touch ({})",
            v.reason
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// **BR-4's exemption is a closed allowlist, and everything off it walks.**
    ///
    /// *(Phase-5 verify, C1. This test shipped at TASK-404 as
    /// `recursive_grep_reads_the_tree_whatever_its_stdin`, asserting a
    /// **denylist** of the recursive `grep` spellings. The first four must-fire
    /// rows below are the spellings that denylist missed: GNU `grep` accepts
    /// `--directories recurse`, its `--dir` abbreviation,
    /// `--dereference-recursive`, and `--rec` for `--recursive`, and each of
    /// them came back `Rooted` for a command that reads every file under a root
    /// holding a `.env`. A denylist inside an allowlist grammar cannot be
    /// completed by adding rows, so the question was inverted.)*
    ///
    /// The must-not-fire half is the load-bearing half (LESSON-440): the
    /// exemption still has to fire, or BR-4 is not implemented at all. `-i`,
    /// `-n`, `-c`, `-v`, `-w` and `-H` are the short flags that cannot mean
    /// recursion, and `head`/`wc`/`sort`/`cut` are the pure filters the
    /// allowlist names directly.
    ///
    /// Two later rounds of must-fire rows, each a hole in the version above it:
    ///
    /// * `--color` and `sed -r` are the tightenings the **inversion** bought and
    ///   TASK-404 asserted the other way — a long option this grammar does not
    ///   enumerate, and a verb that is not a filter (`sed -f` takes a script
    ///   that can open any path).
    /// * `-nd`, `-id`, `-drecurse` and the three `--files0-from`/`-c` rows are
    ///   the **Phase-5 re-verify**'s. The first three are `-d` *inside* a
    ///   cluster, which the "no word is `-d`" clause read as a plain short flag;
    ///   the others are the flag-blindness of the filter half, where being named
    ///   on the allowlist was the whole test and `wc --files0-from -` therefore
    ///   read a list of paths off stdin and opened every one.
    ///
    /// **Mutation (run, red, reverted):** make [`reads_only_its_stdin`] return
    /// `true` unconditionally — the allowlist accepts everything — and the
    /// **first** must-fire row here reds. One row, not every row: the
    /// `assert_eq!` aborts the loop, so what a run reports is the first
    /// disagreement and not a census. Counts are in the module docs.
    ///
    /// **Mutation (run, red, reverted, Phase-5 re-verify):** drop the `--`
    /// guard from the head of [`reads_only_its_stdin`] — the `wc`/`sort`
    /// `--files0-from` rows red (`grep`'s long options are still caught by the
    /// per-word arm, so the guard's own coverage is exactly those rows).
    #[test]
    fn the_piped_exemption_is_a_closed_allowlist() {
        let root = piped_root("piped-allowlist");

        for command in [
            // The four spellings the denylist missed (C1).
            "ls src | grep --directories recurse foo",
            "ls src | grep --dir recurse foo",
            "ls src | grep --dereference-recursive foo",
            "ls src | grep --rec foo",
            // The spellings it did carry.
            "ls src | grep -d recurse foo",
            "ls src | grep -r foo",
            "ls src | grep -rn foo",
            "ls src | grep -nR foo",
            "ls src | grep --recursive foo",
            "ls src | egrep -R foo",
            "ls src | fgrep -r foo",
            // The tightenings: an unenumerated long option, and a verb that is
            // not a filter.
            "ls src | grep --color foo",
            "ls src | grep -d skip foo",
            "ls src | sed -r foo",
            "ls src | awk -f prog",
            "ls src | cat missing",
            // Phase-5 re-verify: `-d` glued into a short cluster. `grep` reads
            // `-nd recurse` as `-n -d recurse`, and the clause that looked for
            // the whole word `-d` never saw it.
            "ls src | grep -nd recurse foo",
            "ls src | grep -id skip foo",
            "ls src | grep -drecurse foo",
            // Phase-5 re-verify: the filter half was flag-blind. Each of these
            // verbs is on the allowlist, and each of these flags makes it read
            // a list of **paths** and open every one of them.
            "ls src | wc --files0-from -",
            "ls src | sort --files0-from -",
            "ls src | shasum -c -",
            // The pagers, dropped from the allowlist in the same pass: `less`
            // takes `:e path` and `!cmd` from the terminal and honours
            // `LESSOPEN`, none of which this grammar can see.
            "ls src | less",
            "ls src | more",
        ] {
            let v = verdict(&root, command);
            assert_eq!(
                v.kind,
                VerdictKind::Unknown,
                "`{command}` is off the allowlist and keeps the root walk ({})",
                v.reason
            );
            // Not merely `Unknown`: `Unknown` **by the walk**. A row refused by
            // the unmodelled scan or by an unrecognised verb would pass this
            // table while saying nothing about the allowlist.
            assert_eq!(
                v.reason, READS_THE_ROOT_REASON,
                "`{command}` should be refused by the root walk"
            );
        }

        for command in [
            "ls src | grep -i foo",
            "ls src | grep -c foo",
            "ls src | grep -n foo",
            "ls src | grep -v foo",
            "ls src | grep -w foo",
            "ls src | grep -H foo",
            "ls src | head -5",
            "ls src | wc -l",
            "ls src | sort",
            "ls src | cut -f 2",
        ] {
            let v = verdict(&root, command);
            assert_eq!(
                v.kind,
                VerdictKind::Rooted,
                "must not fire: `{command}` reads its stdin ({})",
                v.reason
            );
        }

        // The exemption is about the *walk*, not about the position: a `First`
        // recursive grep was `Unknown` before this REQ and still is.
        assert_eq!(
            verdict(&root, "grep -r foo").reason,
            READS_THE_ROOT_REASON,
            "an unpiped recursive grep is unchanged"
        );

        // `.` is a **path** in this grammar, not a regex, so a piped
        // `grep -c .` names the root directory explicitly and is refused by
        // BR-1(d)'s directory scan before the allowlist is consulted at all.
        // Recorded here rather than left as a surprising row in either table.
        assert_eq!(
            verdict(&root, "ls src | grep -c .").reason,
            "a directory the command reads could hold a protected file",
            "`.` names the root, and a named directory is scanned"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// **[`is_recognised_verb`] has a must-not-fire half.**
    ///
    /// *(Phase-5 verify, M4.)* Its only caller —
    /// [`super::shell::tests::the_reach_contract_is_one_paragraph_the_description_and_the_test_share`]
    /// — asks it about four verbs the contract names and asserts `true` for
    /// each. A function that answered `true` for everything would pass that
    /// cross-check exactly as it does now, and the cross-check exists to catch
    /// a contract offering the model a verb the grammar refuses. So the
    /// negative half is asserted here, where the tables are.
    ///
    /// The `git` rows are the shape that needs its own claim: `git status` is a
    /// two-word phrase read against [`GIT_NAME_ONLY`], and a bare `git` and a
    /// `git commit` are both false — the first names no subcommand, the second
    /// names one that reads content.
    #[test]
    fn an_unrecognised_verb_is_not_a_recognised_one() {
        for phrase in ["ls", "cat", "grep", "git status", "echo", "test"] {
            assert!(
                is_recognised_verb(phrase),
                "`{phrase}` is in one of this module's permissive tables"
            );
        }
        for phrase in [
            "python",
            "curl",
            "git commit",
            "git",
            "frobnicate",
            "",
            "git status --short",
            "bin/ls",
        ] {
            assert!(
                !is_recognised_verb(phrase),
                "`{phrase}` is not a verb this classifier recognises"
            );
        }
    }

    /// The allowlist can only ever *remove* a walk a content verb would have
    /// taken — so every entry has to be a content verb.
    ///
    /// A name here that [`READS_CONTENT`] does not carry would be a row with no
    /// effect at all (a `NAME_ONLY` or `READS_NOTHING` verb never reaches the
    /// walk), which is the shape a later author mistakes for coverage.
    #[test]
    fn the_piped_allowlist_is_a_subset_of_the_content_verbs() {
        for verb in PIPED_STDIN_ONLY {
            assert!(
                READS_CONTENT.contains(verb),
                "`{verb}` is on the piped allowlist but is not a content verb"
            );
        }
        for verb in ["sed", "awk", "diff", "cat", "less", "more"] {
            assert!(
                !PIPED_STDIN_ONLY.contains(&verb),
                "`{verb}` takes a file, a script or terminal commands and must keep the walk"
            );
        }
    }

    /// The splitter, on its own: only a single `|` pipes.
    ///
    /// REQ-614's `split(['|', ';', '&', '(', ')', '\n'])` read `||` as two `|`
    /// separators around an empty segment. The verdict could not tell — the
    /// empty segment is skipped and both spellings separate — so this is the
    /// unit that says the two-character separators are one token each, at the
    /// level where the difference is observable.
    ///
    /// **Mutation (run, red, reverted):** return [`SegmentPosition::Piped`] for
    /// every separator and this reds on the `a || b` row, alongside
    /// [`tests::a_piped_reader_with_no_path_reads_stdin_not_the_root`].
    #[test]
    fn only_a_single_pipe_makes_the_next_segment_piped() {
        use SegmentPosition::{First, Piped};

        assert_eq!(
            split_segments("a | b"),
            vec![(First, "a "), (Piped, " b")],
            "a single `|` pipes"
        );
        assert_eq!(
            split_segments("a || b"),
            vec![(First, "a "), (First, " b")],
            "an `or` is not a pipe, and it is one separator, not two"
        );
        assert_eq!(
            split_segments("a && b"),
            vec![(First, "a "), (First, " b")],
            "an `and` is one separator, not two"
        );
        for (command, expected) in [
            ("a", vec![(First, "a")]),
            ("a ; b", vec![(First, "a "), (First, " b")]),
            ("a & b", vec![(First, "a "), (First, " b")]),
            ("a\nb", vec![(First, "a"), (First, "b")]),
            (
                "a | b | c",
                vec![(First, "a "), (Piped, " b "), (Piped, " c")],
            ),
            // A pipe after an `or`: the `or` ends a segment as `First` and the
            // `|` that follows pipes the one after it.
            (
                "a || b | c",
                vec![(First, "a "), (First, " b "), (Piped, " c")],
            ),
            // Subshell parens separate and never pipe; the empty and
            // whitespace-only segments they leave are the caller's
            // `trim().is_empty()` skip.
            (
                "(a) | b",
                vec![(First, ""), (First, "a"), (First, " "), (Piped, " b")],
            ),
            // A trailing separator leaves an empty last segment, carrying the
            // position it would have given a segment that was there.
            ("a |", vec![(First, "a "), (Piped, "")]),
            // Must not fire, and the premise is what is being recorded
            // (Phase-5 verify): a **leading** `|` makes the first segment
            // empty and the second `Piped`, which would exempt `head -5` from
            // the root walk on a command that reads nothing at all. It is safe
            // because `sh` rejects `| head -5` outright — there is no producer,
            // so nothing runs and there is nothing to leak. The row exists so
            // that premise is written down rather than assumed: were the
            // executor ever to accept the form, this is the segment that would
            // need a `First`.
            ("| head -5", vec![(First, ""), (Piped, " head -5")]),
        ] {
            assert_eq!(split_segments(command), expected, "`{command}`");
        }
    }
}
