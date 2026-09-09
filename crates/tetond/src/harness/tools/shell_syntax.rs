//! REQ-620 ADR-620-1: the one place in the daemon that reads "this redirect
//! reads nothing".
//!
//! Two gates ask the same question of the same bytes. The write gate
//! ([`super::super::root_gate::has_top_level_redirection`]) asks whether a `>`
//! creates a file, and has since REQ-596. The shell provenance classifier
//! ([`super::shell_provenance::classify`]) asks whether a command could have
//! read a protected byte, and since REQ-620 has to lift the same forms out
//! before it can read anything else. A second, hand-rolled reading of
//! `2>/dev/null` is the shape LESSON-494 forbids — the day one of them accepts
//! `2>/dev/nullx` and the other does not, one gate is wrong and no test says
//! which — so both call [`NullRedirect::parse`].
//!
//! # What is recognised, and nothing else
//!
//! The [`NullRedirect`] variants are exactly the `NullRedirect` entity of
//! REQ-620's System Model, and the recogniser is total over them: everything
//! else returns `None` and stays unmodelled (BR-2). `2>/dev/nul`,
//! `2>/dev/null/x`, `2>/dev/nullx`, `2>$f`, `> out.txt`, `>> log`, `< input`, a
//! here-doc and process substitution all fall through untouched, which is what
//! keeps this a widening the classifier can prove rather than a guess.
//!
//! # Whole words, because `&` is otherwise a separator
//!
//! `2>&1` and `&>/dev/null` contain `&`, which the classifier's splitter reads
//! as a segment separator. A splitter that saw them second would classify `2>`
//! as a verb and `1` as the next segment's, so [`strip_null_redirects`] runs
//! **before** both the unmodelled scan and the split (ADR-620-2, BR-1), over
//! whitespace-delimited words only. A form glued to its **verb**
//! (`ls>/dev/null`) is *not* lifted: it stays a word this module does not
//! recognise, the unmodelled scan sees its `>`, and the command is `Unknown`
//! exactly as it was before REQ-620. Every miss lands on the old answer, which
//! is the module-level rule [`super::shell_provenance`] opens with.
//!
//! The one shape that **is** peeled is a redirect glued to a following
//! **separator** — `ls 2>&1; echo`, `ls 2>&1|head`, `ls 2>&1;ls` — because a
//! model writes those far more often than the spaced spelling and the peel is
//! decidable without lexing: the head has to parse as a redirect *in its
//! entirety* or nothing is peeled at all, and the tail is re-emitted as its own
//! word so the splitter still sees it ([`split_glued_redirect`]). The spaced
//! form is stricter still — the operator word must be bare, so `>|`, `2>&` and
//! `2>;` never lift a following `/dev/null`; peeling a separator off a head
//! that does not parse on its own would invent a separator `sh` never saw.
//!
//! # `&>/dev/null` lifts to a `&` (Phase-5 verify, C2)
//!
//! [`NullRedirect::BothStreams`] is the one form whose residue is not empty.
//! `&>` is bash; the executor is `sh -c`, and `dash` — `/bin/sh` on the Linux
//! CI leg — reads the `&` as a command separator. Lifting the word whole made
//! `ls &>/dev/null grep -r SECRET . 1>&2` a single segment whose verb is `ls`,
//! and the `grep -r` was never verb-checked. Re-emitting the separator
//! reproduces `dash`'s parse and is strictly more conservative than `bash`'s.
//! See [`NullRedirect::residue_separator`].
//!
//! # `/dev/null` is consumed here and never travels
//!
//! BR-3: the spaced form's `/dev/null` follower is lifted together with its
//! operator word, so the residue [`strip_null_redirects`] returns cannot
//! contain the literal. It never reaches `resolve_token`, never mints a
//! `ProvenanceId`, and never matches a boundary glob — LESSON-623's seam read
//! the other way round, a path that is *not* a file access must not be scored
//! as one.
//!
//! # Mutation record (conventions.md — show the test can fail)
//!
//! Six mutations, run against `cargo test -p tetond --lib` and reverted. The
//! counts are the finding (LESSON-640); the reds outside this module are named
//! because a recogniser whose only guard is its own unit table is a recogniser
//! nothing downstream is holding to anything.
//!
//! **Re-measured 2026-09-09 at REQ-620's Phase-5 verify**, which rewrote
//! [`strip_line`] for C2 and M1 — conventions.md/LESSON-598 require every
//! recorded mutation to be re-run after a change to program structure, not
//! re-read. Three of the four earlier counts moved and all three moves are
//! *coverage arriving* rather than behaviour changing: the C2 test is new, and
//! it spells a command mutations 1–3 each disturb.
//!
//! 1. **[`strip_null_redirects`] made a no-op** (return the command unchanged,
//!    `lifted: 0`) — **9 red** (was 8 at TASK-405):
//!    [`tests::the_strip_lifts_words_and_leaves_the_separators_standing`], and
//!    in [`super::shell_provenance`]
//!    `null_redirects_are_lifted_before_the_scan_and_the_split` (first, on the
//!    `ls 2>&1 && echo ok` residue), `dev_null_is_never_a_path_token`,
//!    `an_opaque_verb_with_a_null_redirect_is_still_unknown`,
//!    `a_redirect_never_hides_a_boundary_read`,
//!    `the_2026_09_09_command_is_rooted_without_its_home_probe`,
//!    `the_redirect_differential_table`,
//!    `a_piped_reader_with_no_path_reads_stdin_not_the_root` (whose
//!    `ls 2>&1 | head` row stops being a pipeline once the `&` is a separator
//!    again) and — new at the verify —
//!    `the_both_streams_form_re_emits_the_separator_it_hides`. Two things
//!    stayed green and should have: `every_other_redirect_stays_unmodelled`,
//!    which asserts the refusal a no-op preserves, and `root_gate`'s benign
//!    table, whose rows are all bare or spaced forms the *gate* now reads
//!    through this same strip.
//! 2. **The whole-word rule dropped** — [`NullRedirect::from_operator`]
//!    relaxed to accept any operator ending in `>`, so `ls>/dev/null` lifts —
//!    **4 red** (was 2):
//!    [`tests::the_recogniser_accepts_the_entity_forms_and_nothing_else`],
//!    [`tests::the_strip_lifts_words_and_leaves_the_separators_standing`],
//!    `null_redirects_are_lifted_before_the_scan_and_the_split` and
//!    `the_both_streams_form_re_emits_the_separator_it_hides`.
//! 3. **The recogniser made non-total** — [`strip_line`] lifting any word
//!    carrying `>` or `<` — **8 red** (was 7):
//!    [`tests::the_strip_lifts_words_and_leaves_the_separators_standing`],
//!    `every_other_redirect_stays_unmodelled` (`ls > out.txt` came back
//!    `Rooted`), `dev_null_is_never_a_path_token`,
//!    `null_redirects_are_lifted_before_the_scan_and_the_split`,
//!    `the_redirect_differential_table`,
//!    `each_unmodelled_class_names_itself_and_nothing_else` (its `ls > ZQX9`
//!    row is the redirect class, and a lifted `>` leaves nothing to name),
//!    `the_both_streams_form_re_emits_the_separator_it_hides`, and — the one
//!    that matters most — REQ-614's own
//!    `adversarial_spellings_are_all_unknown`, on `cat <src/main.rs`.
//! 4. **The spaced form's `/dev/null` follower left in place** (drop the
//!    `words.next()` in [`strip_line`]'s spaced arm) — **4 red**, unchanged,
//!    and `dev_null_is_never_a_path_token` reds as a `BoundaryTouch` on
//!    `cat README.md > /dev/null`, which is BR-3's failure mode exactly: the
//!    device scored as a file access.
//! 5. **[`NullRedirect::BothStreams`] lifted to nothing** — the `&` dropped
//!    from [`NullRedirect::residue_separator`], which is the pre-verify
//!    behaviour C2 fixed — **3 red**:
//!    [`tests::the_strip_lifts_words_and_leaves_the_separators_standing`] on
//!    its three `&>` residue rows,
//!    `null_redirects_are_lifted_before_the_scan_and_the_split`, and
//!    `the_both_streams_form_re_emits_the_separator_it_hides`, whose
//!    `ls &>/dev/null grep -r SECRET . 1>&2` comes back one segment and
//!    `Rooted` on a root holding a `.env`. That last red *is* the privacy
//!    defect, so the count is small and the row is the finding.
//! 6. **The spaced arm allowed to peel separators off its head** —
//!    `parse_spaced(word.trim_end_matches(TRAILING_SEPARATORS), …)`, which is
//!    the unsound peel M1 removed — **1 red**:
//!    [`tests::the_strip_lifts_words_and_leaves_the_separators_standing`], on
//!    `ls >| /dev/null`. One, and that is the whole point of adding those three
//!    rows: nothing else in the crate spells an operator word that does not
//!    parse on its own.
//!
//! The write gate's own arm was mutated in
//! [`super::super::root_gate::has_top_level_redirection`]; the record is there.
//!
//! ## The integration reds (TASK-407, 2026-09-09)
//!
//! The four counts above are `--lib` counts and stay `--lib` counts. TASK-407
//! added the rule's first coverage *outside* the crate's unit tests — the
//! charter-level claim that a cleared command does not pin and the next
//! prompt's bytes reach the provider — so mutation 1 was re-run against the two
//! integration binaries that now hold it. **Four more red**, none of them in
//! `--lib`:
//!
//! - `tests/provenance_egress.rs`:
//!   `a_null_redirect_does_not_pin_and_the_next_prompt_reaches_the_provider`
//!   (turn 1 comes back `PrivacyBlocked` where `Ok` was asserted) and
//!   `a_redirect_does_not_hide_a_boundary_read` (the provenance degrades to
//!   `unknown` with an empty source set, and the block stops naming the file);
//! - `tests/e2e.rs`: `shell_pin_shape::a_cleared_shell_call_leaves_doctor_and_the_route_on_the_provider`
//!   and `shell_pin_shape::shell_allow_does_not_lift_a_boundary_hit_behind_a_redirect`.
//!
//! Mutations 2–4 were **not** re-run at TASK-407 and their counts are TASK-405's
//! unchanged: none of the four new tests spells a command those mutations move
//! (no glued operator, no non-`/dev/null` redirect, no spaced form), so
//! re-measuring them would have been three rebuilds to confirm a zero. Recorded
//! as not-run rather than as measured (LESSON-569).

/// The null device, spelled once.
///
/// LESSON-494's rule applied to a literal: the crate greps clean for
/// `/dev/null` outside this module, its tests, and the prose that explains it.
const NULL_DEVICE: &str = "/dev/null";

/// The separator characters the classifier's splitter reads, for the trailing
/// peel described in the module docs.
///
/// `(`, `)` and `\n` are splitter characters too but are deliberately absent:
/// a `)` glued to a redirect is a subshell this grammar does not model, and a
/// newline is preserved structurally by [`strip_null_redirects`] rather than
/// peeled off a word.
const TRAILING_SEPARATORS: [char; 3] = [';', '&', '|'];

/// A redirect form that provably reads nothing and writes nothing the model
/// sees (REQ-620 BR-1; the `NullRedirect` entity).
///
/// The variants are the entity's `form` column. They carry no target, because
/// there is only ever one: the null device, or a file descriptor, neither of
/// which contributes a path token or any boundary evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NullRedirect {
    /// `[n]>/dev/null` — stdout, or descriptor `n`, to the null device.
    Write,
    /// `[n]>>/dev/null` — the appending spelling. Appending to the null device
    /// is the same nothing as truncating it, but `sh` accepts both and so does
    /// this.
    Append,
    /// `&>/dev/null` — stdout and stderr together.
    BothStreams,
    /// `</dev/null` — stdin from the null device: the command reads end-of-file
    /// and nothing else, which is strictly *less* reach than the terminal it
    /// replaces.
    Stdin,
    /// `[n]>&m`, `n` and `m` single digits — a file-descriptor duplication.
    /// `2>&1` and `1>&2`. Nothing is created; the target is a number, not a
    /// path.
    Duplicate,
}

impl NullRedirect {
    /// Read one whitespace-delimited word as a null redirect.
    ///
    /// `None` for everything the entity does not name — that is the whole
    /// safety argument, so the fallthrough is the last arm of every match
    /// below and never a default.
    #[must_use]
    pub(crate) fn parse(word: &str) -> Option<Self> {
        if let Some(operator) = word.strip_suffix(NULL_DEVICE) {
            return Self::from_operator(operator);
        }
        Self::from_duplication(word)
    }

    /// Read the **spaced** form: an operator word and the word that follows it.
    ///
    /// `sh` accepts `2> /dev/null` and models write it occasionally, so it is
    /// modelled; the cost is this one extra entry point. An operator word whose
    /// follower is anything else — `2> out`, `> "$f"` — is not a null redirect,
    /// and both words are left where they are (ADR-620-1's Consequences).
    #[must_use]
    pub(crate) fn parse_spaced(operator: &str, target: &str) -> Option<Self> {
        if target != NULL_DEVICE {
            return None;
        }
        Self::from_operator(operator)
    }

    /// The operator half of a redirect *to* the null device, with the
    /// `/dev/null` already accounted for by the caller.
    fn from_operator(operator: &str) -> Option<Self> {
        let (descriptor, rest) = split_leading_digit(operator);
        match rest {
            // `>` and `n>`; `>>` and `n>>`.
            ">" => Some(Self::Write),
            ">>" => Some(Self::Append),
            // `&>` and `<` take no leading descriptor: `2&>` and `2<` are not
            // this form, and `sh` does not read them as one either.
            "&>" if descriptor.is_none() => Some(Self::BothStreams),
            "<" if descriptor.is_none() => Some(Self::Stdin),
            _ => None,
        }
    }

    /// The word this form leaves standing in the residue, if any.
    ///
    /// Every form but one lifts to nothing. [`Self::BothStreams`] lifts to a
    /// bare `&`, and that is a correctness fix rather than a nicety
    /// (REQ-620 Phase-5 verify, C2): `&>/dev/null` is **bash**, and the
    /// executor is `sh -c` ([`super::shell::run_bounded`]), which on the Linux
    /// CI leg is `dash`. `dash` reads the `&` as a command separator and the
    /// `>/dev/null` as a redirect on the command *before* it, so
    /// `ls &>/dev/null grep -r SECRET . 1>&2` runs two commands and the second
    /// is a recursive `grep`. Lifting the word whole made that one segment
    /// whose verb is `ls`, and the `grep` was never verb-checked at all.
    ///
    /// Re-emitting the `&` reproduces `dash`'s parse exactly and is strictly
    /// more conservative than `bash`'s, where the same text is one command:
    /// a spurious separator can only ever split a segment into two, and two
    /// segments are each classified in full. LESSON-494's rule points this way
    /// — where two shells disagree, the gate takes the reading that grants the
    /// least reach.
    const fn residue_separator(self) -> Option<&'static str> {
        match self {
            Self::BothStreams => Some("&"),
            Self::Write | Self::Append | Self::Stdin | Self::Duplicate => None,
        }
    }

    /// `[n]>&m` — the descriptor duplication, whole word, single digits.
    ///
    /// `>&-` (a close), `2>&11` (two digits) and `>&file` (bash's `>&` word
    /// form, which *is* a file write) are all `None`: the last of those is the
    /// reason the target must be exactly one digit rather than "not a slash".
    fn from_duplication(word: &str) -> Option<Self> {
        let (_, rest) = split_leading_digit(word);
        let target = rest.strip_prefix(">&")?;
        let (digit, remainder) = split_leading_digit(target);
        if digit.is_some() && remainder.is_empty() {
            Some(Self::Duplicate)
        } else {
            None
        }
    }
}

/// A command with its null redirects lifted out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stripped {
    /// The command minus every lifted word, words rejoined with single spaces
    /// and newlines preserved.
    ///
    /// Byte fidelity is not a property of this value and must not be relied on:
    /// the classifier splits on whitespace and on `| ; & ( ) \n`, so collapsing
    /// runs of spaces and tabs changes nothing it can observe. Newlines *are*
    /// preserved, because they are one of those separators — joining across a
    /// newline would merge two commands into one segment and hand the second
    /// one's verb to the first as an argument.
    pub(crate) residue: String,
    /// How many **words** were lifted. The spaced form counts two (the
    /// operator and its `/dev/null`), so this is a work count and not a count
    /// of redirects.
    ///
    /// Read by nothing that decides a verdict — it exists so a test can assert
    /// the strip fired rather than inferring it from an unchanged residue
    /// (LESSON-640: a fixture sized by arithmetic must assert the arithmetic).
    pub(crate) lifted: usize,
}

/// Lift every null redirect out of `command`, whole words only.
///
/// Runs before the unmodelled scan and before the segment split (ADR-620-2
/// steps 1–2). See the module docs for why that order is the whole correctness
/// argument.
#[must_use]
pub(crate) fn strip_null_redirects(command: &str) -> Stripped {
    let mut lifted = 0;
    let mut lines: Vec<String> = Vec::new();
    // Newline is a segment separator for the classifier, so each line is
    // stripped on its own and the lines are rejoined with the newline intact.
    for line in command.split('\n') {
        lines.push(strip_line(line, &mut lifted));
    }
    Stripped {
        residue: lines.join("\n"),
        lifted,
    }
}

/// [`strip_null_redirects`] for one newline-free line.
fn strip_line(line: &str, lifted: &mut usize) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let mut words = line.split_whitespace().peekable();
    while let Some(word) = words.next() {
        // 1. The whole word is a redirect.
        if let Some(form) = NullRedirect::parse(word) {
            *lifted += 1;
            kept.extend(form.residue_separator());
            continue;
        }
        // 2. A redirect glued to a following separator — `2>&1;ls`,
        //    `2>&1|head`, `&>/dev/null&&ls`. The head has to parse as a
        //    redirect *in its entirety* and the tail has to begin with a
        //    separator character, so the peel is decidable without lexing and
        //    the tail is re-emitted as its own word for the splitter to read.
        if let Some((form, tail)) = split_glued_redirect(word) {
            *lifted += 1;
            kept.extend(form.residue_separator());
            kept.push(tail);
            continue;
        }
        // 3. The spaced form. The operator word must be **bare** — `>`, `>>`,
        //    `2>`, `&>`, `<` and nothing else. Peeling a separator run off a
        //    head that does not parse on its own would invent a separator `sh`
        //    never saw: `>|`, `2>&` and `2>;` are not operators, and reading
        //    them as one plus a `|`/`&`/`;` would hand the splitter a pipeline
        //    the shell would have refused outright.
        let follower = words
            .peek()
            .copied()
            .map(split_at_first_separator)
            .and_then(|(target, next_tail)| {
                NullRedirect::parse_spaced(word, target).map(|form| (form, next_tail))
            });
        if let Some((form, next_tail)) = follower {
            words.next();
            *lifted += 2;
            kept.extend(form.residue_separator());
            if !next_tail.is_empty() {
                kept.push(next_tail);
            }
            continue;
        }
        kept.push(word);
    }
    kept.join(" ")
}

/// A word that is a [`NullRedirect`] glued to a trailing run beginning with a
/// separator character — the head's form and the tail, or `None`.
///
/// The head must parse **whole**, which is what stops this from turning a word
/// the recogniser rejects into one it accepts: `ls>/dev/null` has no parsing
/// prefix followed by a separator and stays exactly where it is (the Deferred
/// note in the REQ, and mutation 2 below). The longest candidate wins so that
/// `2>&1` is preferred to any shorter prefix; `2>&11` has no separator after
/// its parsing prefix and is not peeled at all.
fn split_glued_redirect(word: &str) -> Option<(NullRedirect, &str)> {
    (1..word.len())
        .rev()
        .filter(|&at| word.is_char_boundary(at))
        .filter(|&at| word[at..].starts_with(TRAILING_SEPARATORS))
        .find_map(|at| NullRedirect::parse(&word[..at]).map(|form| (form, &word[at..])))
}

/// Split a redirect **target** word at its first separator character.
///
/// Only ever applied to the spaced form's follower, whose one accepted value
/// (`/dev/null`) contains no separator character — so the split can only ever
/// peel a glued tail off the device and never cut the device itself.
fn split_at_first_separator(word: &str) -> (&str, &str) {
    match word.find(TRAILING_SEPARATORS) {
        Some(at) => (&word[..at], &word[at..]),
        None => (word, ""),
    }
}

/// `("2>", …)` → `(Some('2'), ">")`; anything not starting with an ASCII digit
/// keeps the whole word.
///
/// ASCII only: `sh`'s descriptor numbers are ASCII, and a non-ASCII digit in a
/// redirect is a word this recogniser should reject rather than normalise.
fn split_leading_digit(word: &str) -> (Option<char>, &str) {
    match word.chars().next() {
        Some(c) if c.is_ascii_digit() => (Some(c), &word[c.len_utf8()..]),
        _ => (None, word),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **BR-1 / BR-2: the recogniser is total over the entity's forms and
    /// returns `None` for everything else.**
    ///
    /// The look-alike half is the load-bearing half (LESSON-440): a recogniser
    /// validated only against the spellings it was written for ships broken and
    /// passes its own suite. Every row here is a spelling the classifier must
    /// keep refusing, and the two `/dev/null`-adjacent ones — `2>/dev/nul` and
    /// `2>/dev/nullx` — are LESSON-494's exact failure mode: two parsers that
    /// disagree on one byte.
    ///
    /// **Mutation (run, red, reverted):** relax [`NullRedirect::from_operator`]
    /// to accept any operator ending in `>` — this test and
    /// `shell_provenance::tests::null_redirects_are_lifted_before_the_scan_and_the_split`
    /// red, on the `ls>/dev/null` row.
    #[test]
    fn the_recogniser_accepts_the_entity_forms_and_nothing_else() {
        use NullRedirect::{Append, BothStreams, Duplicate, Stdin, Write};
        let accepted = [
            (">/dev/null", Write),
            ("1>/dev/null", Write),
            ("2>/dev/null", Write),
            ("2>>/dev/null", Append),
            (">>/dev/null", Append),
            ("&>/dev/null", BothStreams),
            ("</dev/null", Stdin),
            ("2>&1", Duplicate),
            ("1>&2", Duplicate),
            (">&2", Duplicate),
        ];
        for (word, form) in accepted {
            assert_eq!(
                NullRedirect::parse(word),
                Some(form),
                "`{word}` is a NullRedirect entity form"
            );
        }

        let rejected = [
            // BR-2's look-alikes.
            ("2>/dev/nul", "one byte short of the device"),
            ("2>/dev/null/x", "a path *under* the device name"),
            ("2>/dev/nullx", "a path that merely starts like it"),
            ("2>$f", "a variable target"),
            (">", "a bare operator, with no follower here"),
            (">>", "likewise"),
            ("<", "likewise"),
            ("&>", "likewise"),
            ("out.txt", "an ordinary word"),
            ("/dev/null", "the device as a bare argument, not a redirect"),
            // Descriptor duplication's edges.
            (">&-", "a descriptor close"),
            ("2>&11", "two digits"),
            (">&file", "bash's word form, which writes a file"),
            ("2>&", "no target"),
            ("12>/dev/null", "a two-digit descriptor"),
            ("2&>/dev/null", "`&>` takes no leading descriptor"),
            // The whole-word rule.
            ("ls>/dev/null", "glued to its verb"),
            ("x>&1", "likewise, for a duplication"),
            ("2>&1;ls", "glued to a following command"),
            ("", "the empty word"),
        ];
        for (word, why) in rejected {
            assert_eq!(
                NullRedirect::parse(word),
                None,
                "`{word}` must stay unmodelled ({why})"
            );
        }

        // The spaced form, and its follower rule.
        assert_eq!(
            NullRedirect::parse_spaced("2>", "/dev/null"),
            Some(Write),
            "the spaced form is one `sh` accepts"
        );
        assert_eq!(
            NullRedirect::parse_spaced("<", "/dev/null"),
            Some(Stdin),
            "and from the device as well as to it"
        );
        for (operator, target, why) in [
            (">", "out.txt", "an operator with an ordinary follower"),
            (">", "/dev/nullx", "a follower that merely starts like it"),
            (
                "2>&1",
                "/dev/null",
                "a complete redirect is not an operator",
            ),
            ("ls", "/dev/null", "a verb is not an operator"),
        ] {
            assert_eq!(
                NullRedirect::parse_spaced(operator, target),
                None,
                "`{operator} {target}` must stay unmodelled ({why})"
            );
        }
    }

    /// **BR-1 / BR-3: the strip lifts words, keeps separators, and consumes the
    /// device.**
    ///
    /// The residue rows are the contract [`super::super::shell_provenance`]'s
    /// splitter is written against. The `ls & ls` row is the must-not-fire one:
    /// a lone `&` is a separator and stays.
    ///
    /// **Mutation (run, re-measured 2026-09-09, red, reverted):** make
    /// [`strip_null_redirects`] a no-op — this test reds first among eight;
    /// lift any word carrying `>` or `<` — this test reds on
    /// `cat x 2> /dev/null`, among seven; leave the spaced form's follower in
    /// place — this test reds, among four. The full counts are in the module
    /// docs.
    #[test]
    fn the_strip_lifts_words_and_leaves_the_separators_standing() {
        for (command, residue, lifted) in [
            ("ls 2>&1 && echo", "ls && echo", 1),
            ("ls 2>&1; ls", "ls ; ls", 1),
            // C2: `&>` is bash, and `dash` reads the `&` as a separator, so the
            // separator is re-emitted where the word stood.
            ("ls &>/dev/null || echo no", "ls & || echo no", 1),
            (
                "ls &>/dev/null grep -r SECRET . 1>&2",
                "ls & grep -r SECRET .",
                2,
            ),
            ("cat x &> /dev/null", "cat x &", 2),
            ("cat x 2> /dev/null", "cat x", 2),
            ("cat x 2> /dev/null; ls", "cat x ; ls", 2),
            ("2>/dev/null cat x", "cat x", 1),
            // Glued to a following separator: peeled, because the head parses
            // whole and the tail is re-emitted for the splitter.
            ("ls 2>&1|head", "ls |head", 1),
            ("ls 2>&1;ls", "ls ;ls", 1),
            ("cat x 2> /dev/null;ls", "cat x ;ls", 2),
            ("ls & ls", "ls & ls", 0),
            ("ls > out.txt", "ls > out.txt", 0),
            ("ls 2> out", "ls 2> out", 0),
            ("ls\ncat x", "ls\ncat x", 0),
            ("ls 2>&1\ncat x", "ls\ncat x", 1),
            ("ls</dev/null", "ls</dev/null", 0),
            // The spaced arm's soundness rows: none of these heads parses as a
            // redirect on its own, so peeling the separator off them would
            // invent one the shell never saw.
            ("ls >| /dev/null", "ls >| /dev/null", 0),
            ("ls 2>& /dev/null", "ls 2>& /dev/null", 0),
            ("ls 2>; /dev/null", "ls 2>; /dev/null", 0),
        ] {
            let stripped = strip_null_redirects(command);
            assert_eq!(
                stripped.residue, residue,
                "`{command}` should strip to `{residue}`"
            );
            assert_eq!(
                stripped.lifted, lifted,
                "`{command}` should lift {lifted} word(s)"
            );
        }

        // BR-3, stated as a property over every accepted form: the residue
        // never spells the device, so no later stage can resolve it as a path.
        // `&>` leaves the separator C2 requires and nothing else.
        for (form, residue) in [
            ("2>/dev/null", "cat README.md"),
            ("2>> /dev/null", "cat README.md"),
            ("&>/dev/null", "cat README.md &"),
            ("</dev/null", "cat README.md"),
            ("> /dev/null", "cat README.md"),
        ] {
            let stripped = strip_null_redirects(&format!("cat README.md {form}"));
            assert_eq!(stripped.residue, residue);
            assert!(
                !stripped.residue.contains("/dev/"),
                "`{form}` left the device in the residue"
            );
        }
    }
}
