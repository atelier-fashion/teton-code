//! BUG-236: the one place in the daemon that reads "this quoted span is a
//! literal".
//!
//! The shell provenance classifier ([`super::shell_provenance::classify`])
//! refused every command carrying a `'` or a `"` before it read the verb, on
//! the argument that a grammar which tried to *handle* quoting would be a
//! half-written shell lexer (ADR-614-1). A simple quoted literal is also the
//! single most common thing a model writes into a shell command —
//! `find crates -name "*.toml"`, `grep -rn 'fn main' src` — and on 2026-10-08
//! three of the four children an `/analyze` turn fanned out each pinned the
//! session on one within fifteen seconds of starting. The parent inherited the
//! taint through their reports, was rerouted to the local tier, and the typed
//! skill no longer fit (BUG-236).
//!
//! # What is lifted, and nothing else
//!
//! A **simple quoted span** is `'X'` or `"X"` where `X` is non-empty, does not
//! begin with `~`, and contains none of `'`, `"`, `` ` ``, `$`, `\`, a newline
//! or either placeholder character below ([`is_simple`]). For exactly that
//! shape POSIX `sh`'s quote removal yields `X` with no parameter expansion, no
//! command substitution, no word splitting and no pathname expansion — in
//! single quotes because nothing is special inside them, in double quotes
//! because the three characters that *are* special there (`$`, `` ` ``, `\`)
//! are excluded. So the word the program receives is `X`, concatenated with
//! whatever unquoted text the same word carried (`--include="*.rs"` is one
//! word to `sh` and one word here). The classifier reads that word and no
//! other.
//!
//! Everything else stays unmodelled (REQ-614 BR-2): `"$HOME"`, `'it'\''s'`,
//! `"a\"b"`, `""`, `"~/x"`, an unterminated quote, a quote inside a
//! substitution. Each leaves at least one quote character standing in the
//! residue, the unmodelled scan sees it, and the command is `Unknown` with the
//! `Quote` class exactly as it was before this module existed.
//!
//! # Every quote is lifted, or the command is refused
//!
//! [`lift_quoted_literals`] pairs each opening quote with the next quote of
//! the same kind and lifts the span only when it is simple; a span that is not
//! simple keeps its opening quote in the residue and scanning resumes after it.
//! A residue can therefore reach the classifier's `Rooted` verdict only when
//! **every** quote in the command was the opening or closing half of a simple
//! span — any leftover quote is an unmodelled byte and refuses the whole
//! command. That is the soundness argument in one sentence: in the only case
//! that can clear, the spans this scanner lifted are exactly the spans `sh`'s
//! lexer would have read, because a simple span contains no quote of either
//! kind and no backslash, which are the only two things that could make the
//! two readers disagree about where a span ends.
//!
//! # The placeholder, and why the bytes never travel
//!
//! A lifted span is replaced in the residue by `\u{E000}<index>\u{E001}` — two
//! private-use characters around a decimal index into [`Lifted::literals`].
//! The residue is what [`super::shell_provenance`]'s unmodelled scan and
//! segment splitter read, so a glob character, a `|`, an `&&` or a `=` inside
//! a quoted span is never mistaken for shell syntax: `grep "a|b" src` is one
//! segment and `echo "a && cat .env"` runs no `cat`. The classifier restores
//! the literal ([`expand`]) **before** it reads a segment's words — the verb
//! table, the `find -exec` check, the `=` rule and path resolution all run on
//! the expanded word — so `cat ".env"` still names `.env` and still pins
//! `boundary_hit`, `"-exec"` is still `-exec`, and `"python3"` is still
//! opaque. A command that itself carries either placeholder character is
//! refused outright ([`lift_quoted_literals`] answers `None`), so nothing a
//! model types can forge an index.
//!
//! # Relation to the write gate's quote scanner
//!
//! `root_gate::top_level_positions` also reads quotes, for a different
//! question (does a `>` create a file?) and with `sh`'s full rules — a
//! backslash escapes inside double quotes there. This module is strictly
//! narrower: every span it lifts is one that scanner would also read as a
//! quoted span, and every span it declines is left for the unmodelled scan to
//! refuse. Where the two could disagree the classifier therefore answers
//! `Unknown`, which is LESSON-494's direction — two readers of one byte string
//! may differ only towards the gate that grants less.
//!
//! # Mutation record (conventions.md — show the test can fail)
//!
//! Recorded in [`super::shell_provenance`]'s module docs beside the other
//! grammar widenings, because the consumers that hold this lift to anything
//! live there; this module's own table ([`LIFT_ROWS`]) is the residue pin.

/// Opens a placeholder in the residue. Private-use, so no shell command a
/// user or model writes carries it — and one that does is refused.
pub(crate) const LITERAL_OPEN: char = '\u{E000}';
/// Closes a placeholder, so the index's digits cannot run into unquoted text
/// that follows the span (`"a"1` is `a1` to `sh`, and index `0` then `1` here).
pub(crate) const LITERAL_CLOSE: char = '\u{E001}';

/// A command with its simple quoted spans lifted out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Lifted {
    /// The command with every simple quoted span replaced by its placeholder.
    /// Byte fidelity outside the spans is preserved — unlike
    /// [`super::shell_syntax::Stripped::residue`], this is not re-joined by
    /// word, because the strip that runs next needs the command's own
    /// whitespace to find its whole words.
    pub(crate) residue: String,
    /// The lifted spans' contents, in order; `residue`'s indices point here.
    pub(crate) literals: Vec<String>,
}

impl Lifted {
    /// How many spans were lifted — the work count a test asserts so a lift
    /// that silently stopped firing cannot pass as "the residue was unchanged"
    /// (LESSON-640).
    #[cfg(test)]
    #[must_use]
    pub(crate) fn lifted(&self) -> usize {
        self.literals.len()
    }
}

/// Lift every simple quoted span out of `command`.
///
/// `None` when the command carries a placeholder character of its own: there
/// is no honest residue for it, and the caller refuses the command. Runs
/// **before** [`super::shell_syntax::strip_null_redirects`], the unmodelled
/// scan and the segment split, so none of them ever sees a quoted byte.
#[must_use]
pub(crate) fn lift_quoted_literals(command: &str) -> Option<Lifted> {
    if command.contains([LITERAL_OPEN, LITERAL_CLOSE]) {
        return None;
    }
    let mut residue = String::with_capacity(command.len());
    let mut literals = Vec::new();
    let mut rest = command;
    while let Some(at) = rest.find(['\'', '"']) {
        residue.push_str(&rest[..at]);
        // `find` returned a char boundary and both quote characters are ASCII,
        // so `at + 1` is a boundary too.
        let quote = rest.as_bytes()[at] as char;
        let after = &rest[at + 1..];
        match after.find(quote) {
            Some(end) if is_simple(&after[..end]) => {
                literals.push(after[..end].to_owned());
                residue.push(LITERAL_OPEN);
                residue.push_str(&(literals.len() - 1).to_string());
                residue.push(LITERAL_CLOSE);
                rest = &after[end + 1..];
            }
            // Not simple, or unterminated: the quote stays, the unmodelled scan
            // refuses it, and scanning resumes after it so a later quote cannot
            // be paired across this one.
            _ => {
                residue.push(quote);
                rest = after;
            }
        }
    }
    residue.push_str(rest);
    Some(Lifted { residue, literals })
}

/// Whether `sh`'s quote removal of `'inner'` or `"inner"` is exactly `inner`
/// with nothing expanded — the module docs' definition.
fn is_simple(inner: &str) -> bool {
    !inner.is_empty()
        && !inner.starts_with('~')
        && !inner.contains(['\'', '"', '`', '$', '\\', '\n', LITERAL_OPEN, LITERAL_CLOSE])
}

/// Restore the literals a residue word carries, giving the word `sh` hands the
/// program.
///
/// `None` for a placeholder this function cannot read — an index past the
/// table, or an opener with no closer. Neither can be produced by
/// [`lift_quoted_literals`] over a command it accepted, so the caller treats
/// `None` as a refusal rather than a shape to recover from.
#[must_use]
pub(crate) fn expand(word: &str, literals: &[String]) -> Option<String> {
    if !word.contains(LITERAL_OPEN) {
        return Some(word.to_owned());
    }
    let mut out = String::with_capacity(word.len());
    let mut rest = word;
    while let Some(at) = rest.find(LITERAL_OPEN) {
        out.push_str(&rest[..at]);
        let after = &rest[at + LITERAL_OPEN.len_utf8()..];
        let end = after.find(LITERAL_CLOSE)?;
        let index: usize = after[..end].parse().ok()?;
        out.push_str(literals.get(index)?);
        rest = &after[end + LITERAL_CLOSE.len_utf8()..];
    }
    out.push_str(rest);
    Some(out)
}

/// The lift's residue table: `(command, residue with each placeholder spelled
/// as ⟨n⟩, literals)`.
///
/// `#[cfg(test)]` and `pub(crate)` for the reason
/// [`super::shell_syntax::STRIP_ROWS`] is: one table, read here and by the
/// classifier's own suite, so a row added for one is a row the other runs.
#[cfg(test)]
pub(crate) const LIFT_ROWS: &[(&str, &str, &[&str])] = &[
    // The 2026-10-08 commands, one per child that pinned.
    (
        r#"find crates -maxdepth 2 -name "*.toml" | head -20 && echo --- && ls crates/tetond/src/egress/ 2>/dev/null"#,
        "find crates -maxdepth 2 -name ⟨0⟩ | head -20 && echo --- && ls crates/tetond/src/egress/ 2>/dev/null",
        &["*.toml"],
    ),
    (
        "find crates -name '*.rs' -path '*/src/*' | xargs wc -l",
        "find crates -name ⟨0⟩ -path ⟨1⟩ | xargs wc -l",
        &["*.rs", "*/src/*"],
    ),
    (
        "grep -rnE 'std::fs|std::net' crates/teton-core/src --include='*.rs' | grep -v '^.*://' | head -50",
        "grep -rnE ⟨0⟩ crates/teton-core/src --include=⟨1⟩ | grep -v ⟨2⟩ | head -50",
        &["std::fs|std::net", "*.rs", "^.*://"],
    ),
    // Whitespace, separators and shell syntax inside a span are literal.
    (
        r#"echo "No ethos found — run /init""#,
        "echo ⟨0⟩",
        &["No ethos found — run /init"],
    ),
    (r#"echo "a && cat .env""#, "echo ⟨0⟩", &["a && cat .env"]),
    (r#"grep "a|b" src"#, "grep ⟨0⟩ src", &["a|b"]),
    (r#"echo "2>/dev/null""#, "echo ⟨0⟩", &["2>/dev/null"]),
    // Glued to unquoted text: one word to `sh`, one word here.
    (r#"cat src/"main.rs""#, "cat src/⟨0⟩", &["main.rs"]),
    (r#"cat "src"/main.rs"#, "cat ⟨0⟩/main.rs", &["src"]),
    (r#"echo "a"1"#, "echo ⟨0⟩1", &["a"]),
    (r#"echo "x"'y'"#, "echo ⟨0⟩⟨1⟩", &["x", "y"]),
    // A quoted verb is lifted too; the classifier expands it and reads the
    // program `sh` runs.
    (r#""cat" .env"#, "⟨0⟩ .env", &["cat"]),
    // Must not fire: each leaves a quote in the residue.
    (r#"echo "$HOME""#, r#"echo "$HOME""#, &[]),
    (r#"echo "`ls`""#, r#"echo "`ls`""#, &[]),
    // The span `sh` reads is `a\"b`; this scanner pairs the opener with the
    // escaped quote, finds a backslash inside, declines, and resumes after the
    // opener — so the trailing `b` is lifted and the residue keeps a `"` and
    // a `\`. The leftover quote is what refuses the command; the partial lift
    // can never clear it.
    (r#"echo "a\"b""#, r#"echo "a\⟨0⟩"#, &["b"]),
    (r#"echo 'a\b'"#, r#"echo 'a\b'"#, &[]),
    (r#"echo "it's""#, r#"echo "it's""#, &[]),
    // Same shape the other way round: the single-quoted span holds a `"`, is
    // declined, and the inner `"hi"` lifts on its own — with both `'` left
    // standing to refuse the command.
    (r#"echo 'say "hi"'"#, "echo 'say ⟨0⟩'", &["hi"]),
    (r#"echo """#, r#"echo """#, &[]),
    (r#"cat "~/.ssh/id_rsa""#, r#"cat "~/.ssh/id_rsa""#, &[]),
    ("ls 'a", "ls 'a", &[]),
    (r#"echo "x"'"#, "echo ⟨0⟩'", &["x"]),
    ("echo 'a\nb'", "echo 'a\nb'", &[]),
    // No quotes at all: the residue is the command.
    ("ls -la src", "ls -la src", &[]),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Spell a residue's placeholders as `⟨n⟩` so the table above is readable.
    pub(crate) fn spelled(residue: &str) -> String {
        residue
            .replace(LITERAL_OPEN, "⟨")
            .replace(LITERAL_CLOSE, "⟩")
    }

    /// The table, row by row: residue, literals, and the work count.
    ///
    /// **Mutation (run 2026-10-08, red, reverted):** make [`is_simple`] accept
    /// `$` — the `"$HOME"` row reds here, with four more across the
    /// classifier's suite (**5** in all; the count and the names are in
    /// `shell_provenance`'s module docs). Make it accept an empty span — the
    /// `""` row reds here, plus two of the classifier's (**3**).
    #[test]
    fn simple_spans_are_lifted_and_every_other_quote_stays() {
        for (command, residue, literals) in LIFT_ROWS {
            let lifted = lift_quoted_literals(command)
                .unwrap_or_else(|| panic!("`{command}` carries no placeholder character"));
            assert_eq!(spelled(&lifted.residue), *residue, "`{command}`: residue");
            assert_eq!(
                lifted.literals,
                literals.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
                "`{command}`: literals"
            );
            assert_eq!(lifted.lifted(), literals.len(), "`{command}`: work count");
        }
    }

    /// The soundness sentence from the module docs, over every row: a residue
    /// with no quote character left in it has lifted every quote the command
    /// had, and a residue that kept one has not cleared the command.
    #[test]
    fn a_quote_free_residue_means_every_quote_was_lifted() {
        for (command, _, _) in LIFT_ROWS {
            let lifted = lift_quoted_literals(command).unwrap();
            let quotes_in_command = command.chars().filter(|c| *c == '\'' || *c == '"').count();
            let quotes_in_residue = lifted
                .residue
                .chars()
                .filter(|c| *c == '\'' || *c == '"')
                .count();
            assert_eq!(
                quotes_in_command - quotes_in_residue,
                2 * lifted.lifted(),
                "`{command}`: each lifted span accounts for exactly two quote characters"
            );
        }
    }

    /// Expansion gives back the word `sh` hands the program, placeholder by
    /// placeholder, glued text included — and refuses what the lift cannot
    /// have produced.
    ///
    /// The `"a"1` row is why the placeholder has a closer: without
    /// [`LITERAL_CLOSE`] the index `0` would run into the `1` and read as
    /// literal `01`, which is not in the table. Pinned by the row rather than
    /// by a mutation that was run (LESSON-569: do not claim a mutation you did
    /// not run).
    #[test]
    fn expansion_restores_the_word_sh_hands_the_program() {
        for (command, expected_words) in [
            (
                r#"find crates -name "*.toml""#,
                vec!["find", "crates", "-name", "*.toml"],
            ),
            (r#"cat src/"main.rs""#, vec!["cat", "src/main.rs"]),
            (r#"echo "a"1"#, vec!["echo", "a1"]),
            (r#"echo "x"'y'"#, vec!["echo", "xy"]),
            (r#"echo "a && cat .env""#, vec!["echo", "a && cat .env"]),
            (r#""cat" .env"#, vec!["cat", ".env"]),
            (r#"find . "-exec" cat"#, vec!["find", ".", "-exec", "cat"]),
        ] {
            let lifted = lift_quoted_literals(command).unwrap();
            let words: Vec<String> = lifted
                .residue
                .split_whitespace()
                .map(|w| expand(w, &lifted.literals).unwrap())
                .collect();
            assert_eq!(words, expected_words, "`{command}`");
        }

        let literals = vec!["x".to_owned()];
        assert_eq!(expand("plain", &literals).as_deref(), Some("plain"));
        assert_eq!(
            expand(&format!("{LITERAL_OPEN}7{LITERAL_CLOSE}"), &literals),
            None,
            "an index past the table is a refusal"
        );
        assert_eq!(
            expand(&format!("{LITERAL_OPEN}0"), &literals),
            None,
            "an opener with no closer is a refusal"
        );
    }

    /// A command carrying a placeholder character of its own has no honest
    /// residue and is refused before anything reads it.
    ///
    /// **Mutation (run 2026-10-08, red, reverted):** drop the `contains`
    /// guard at the top of [`lift_quoted_literals`] — this reds, and the
    /// classifier's `a_forged_placeholder_is_refused_not_expanded` reds with
    /// it (**2**).
    #[test]
    fn a_command_carrying_the_placeholder_is_refused() {
        assert_eq!(
            lift_quoted_literals(&format!("cat {LITERAL_OPEN}0{LITERAL_CLOSE}")),
            None
        );
        assert_eq!(lift_quoted_literals(&format!("ls {LITERAL_CLOSE}")), None);
        assert_eq!(
            lift_quoted_literals(&format!("echo '{LITERAL_OPEN}'")),
            None,
            "inside a span too — the guard is over the whole command"
        );
    }
}
