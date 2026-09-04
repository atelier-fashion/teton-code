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

use std::collections::BTreeSet;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::Duration;

use teton_core::boundary::BoundaryMatcher;
use teton_core::entities::PrivacyBoundary;
use teton_core::provenance_id::ProvenanceId;
use teton_protocol::methods::RootKind;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VerdictKind {
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

/// A classification of one `shell` invocation.
#[derive(Debug, Clone)]
pub(crate) struct Verdict {
    pub(crate) kind: VerdictKind,
    /// The repo-relative canonical ids of every path argument that resolved
    /// inside the session root. Empty for every kind but [`VerdictKind::Rooted`].
    pub(crate) sources: BTreeSet<ProvenanceId>,
    /// Why this verdict was reached. `&'static str`, so it cannot carry command
    /// text or file content (see the module docs).
    ///
    /// Rendered on the daemon's stderr by `ShellTool::run` for any non-`Rooted`
    /// verdict — the answer to "why did *that* command pin my session". It is a
    /// sentence rather than a bare discriminant for that reason, and a
    /// `&'static str` so logging it cannot leak what the command said.
    pub(crate) reason: &'static str,
}

impl Verdict {
    fn unknown(reason: &'static str) -> Self {
        Self {
            kind: VerdictKind::Unknown,
            sources: BTreeSet::new(),
            reason,
        }
    }
}

/// Verbs that read no file at all. A timeout on one of these is still `Rooted`
/// (REQ-614 AC-4).
const READS_NOTHING: &[&str] = &[
    "pwd", "sleep", "true", "false", "date", "whoami", "uname", "hostname", "id", "echo", "printf",
];

/// Verbs that surface **names**, not file contents. Listing a name is not
/// reading a file, so these pass BR-1(d) without a subtree scan.
const NAME_ONLY: &[&str] = &[
    "ls", "find", "du", "basename", "dirname", "stat", "file", "which",
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
fn classify_with_budget(
    root: &Path,
    root_kind: RootKind,
    boundaries: &[PrivacyBoundary],
    denied_prefixes: Vec<PathBuf>,
    command: &str,
    budget: WalkBudget,
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

    if command.chars().any(|c| UNMODELLED.contains(&c)) {
        return Verdict::unknown("the command uses shell syntax this classifier does not model");
    }

    let matcher = match BoundaryMatcher::new(boundaries) {
        Ok(m) => m,
        // A boundary set that does not compile is the fail-closed case
        // `context_taint_cause` already treats as sensitive.
        Err(_) => return Verdict::unknown("the configured boundary set does not compile"),
    };

    let mut sources = BTreeSet::new();
    let mut saw_boundary = false;

    for segment in command.split(['|', ';', '&', '(', ')', '\n']) {
        if segment.trim().is_empty() {
            continue;
        }
        match classify_segment(
            root,
            &matcher,
            &denied_prefixes,
            budget,
            segment,
            &mut sources,
        ) {
            SegmentVerdict::Rooted => {}
            SegmentVerdict::BoundaryTouch => saw_boundary = true,
            SegmentVerdict::Unknown(reason) => {
                // A boundary touch outranks an unknown: it is the more severe
                // consequence (permanent, unliftable), so a command that both
                // names a protected file and does something opaque must pin
                // permanently rather than liftably.
                if saw_boundary {
                    return Verdict {
                        kind: VerdictKind::BoundaryTouch,
                        sources,
                        reason: "a path argument matches a privacy boundary",
                    };
                }
                return Verdict::unknown(reason);
            }
        }
    }

    if saw_boundary {
        return Verdict {
            kind: VerdictKind::BoundaryTouch,
            // Non-empty when the boundary path was **in-root**: the caller maps
            // that to `Sources`, so the block names the file. Empty for an
            // out-of-root touch, which is what `ToolProvenance::BoundaryTouch`
            // is for.
            sources,
            reason: "a path argument matches a privacy boundary",
        };
    }
    Verdict {
        kind: VerdictKind::Rooted,
        sources,
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
    root: &Path,
    matcher: &BoundaryMatcher<'_>,
    denied_prefixes: &[PathBuf],
    budget: WalkBudget,
    segment: &str,
    sources: &mut BTreeSet<ProvenanceId>,
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
    let verb = raw_verb.rsplit('/').next().unwrap_or(raw_verb);

    if OPAQUE.contains(&verb) {
        return SegmentVerdict::Unknown(
            "the command runs an interpreter, build tool or network client",
        );
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

    let mut saw_boundary = false;
    let mut saw_directory = false;

    for token in &paths {
        let resolved = resolve_token(root, token);
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
            if under_denied_prefix(denied_prefixes, abs) {
                return SegmentVerdict::Unknown(
                    "a path argument is inside a directory tools may not read",
                );
            }
        }
        match resolved {
            Resolved::InsideRoot(abs, id) => {
                if matcher.match_path(id.as_str()).is_some() {
                    saw_boundary = true;
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
                        && !subtree_is_boundary_free(&abs, denied_prefixes, matcher, budget)
                    {
                        return SegmentVerdict::Unknown(
                            "a directory the command reads could hold a protected file",
                        );
                    }
                } else {
                    sources.insert(id);
                }
            }
            Resolved::RootItself(abs) => {
                saw_directory = true;
                if reads_content
                    && !subtree_is_boundary_free(&abs, denied_prefixes, matcher, budget)
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
                // what lets `**/.ssh/**` reach `Users/x/.ssh/config` (AC-5).
                let spelling = abs.to_string_lossy();
                let stripped = spelling.strip_prefix('/').unwrap_or(&spelling);
                if matcher.match_path(stripped).is_some() {
                    saw_boundary = true;
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

    if saw_boundary {
        return SegmentVerdict::BoundaryTouch;
    }

    // BR-1(d): a content-reading verb given no path at all reads whatever is
    // under the root.
    if reads_content
        && !saw_directory
        && paths.is_empty()
        && !subtree_is_boundary_free(root, denied_prefixes, matcher, budget)
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

fn resolve_token(root: &Path, token: &str) -> Resolved {
    let joined = if let Some(tail) = token.strip_prefix("~/") {
        match crate::session_root::home() {
            Some(home) => home.join(tail),
            None => return Resolved::Unresolvable,
        }
    } else if token == "~" {
        match crate::session_root::home() {
            Some(home) => home,
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
        Err(_) => Resolved::RootItself(checked),
    }
}

/// Whether **no** file under `dir` matches a boundary glob.
///
/// ADR-614-5: a scan that exhausts its budget has not shown the absence of a
/// boundary file — it stopped looking — so it answers `false`, the same as a
/// scan that found one. The two are not distinguished because the caller does
/// the same thing with both, and giving them separate values would invite a
/// later author to treat "stopped looking" as "found nothing".
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
    !hit && report.truncated_by.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use teton_core::config::DEFAULT_BOUNDARIES;
    use teton_core::entities::PrivacyBoundary;

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
}
