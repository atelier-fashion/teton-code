//! The narrow flat frontmatter parser (REQ-585 BR-5, ADR-5).
//!
//! A skill file may open with a delimited header:
//!
//! ```text
//! ---
//! name: status
//! description: Show current state of all ADLC work
//! argument-hint: [REQ-xxx]
//! ---
//!
//! # /status — …the body…
//! ```
//!
//! [`parse`] reads exactly that shape and nothing more. There is **no YAML
//! crate in this workspace** and this is not the requirement that adds one.
//!
//! # The grammar, whole
//!
//! 1. A file that does not begin with a line that is exactly `---` has **no**
//!    frontmatter: the whole file is the body and there are zero ignored keys.
//!    This is the common case, not an error — `.claude/commands/*.md` routinely
//!    has no header at all, and refusing those would refuse half the feature.
//! 2. Otherwise the block runs to the next line that is exactly `---`. No
//!    closing delimiter ⇒ [`Malformed`].
//! 3. Every line between must be blank, a flush-left `#` comment, or
//!    `key: value` where the key matches `^[a-z][a-z0-9-]*$`. An indented
//!    continuation, a nested block, a list item, a bare word — anything else ⇒
//!    [`Malformed`].
//! 4. `name`, `description`, `argument-hint` and the two **invocation flags**
//!    — `disable-model-invocation` and `user-invocable` (REQ-587 BR-3) — are
//!    read. Every other key lands in [`Parsed::ignored_keys`] and is inert
//!    (REQ-585 BR-5): a skill file grants nothing, so `allowed-tools`, `model`
//!    and `hooks` are *listed*, never honored.
//!
//! # A bad value is not a malformed file
//!
//! [`parse`] is **total**, and a *shape* it cannot read is refused whole (next
//! section). A *value* it cannot read, on a key it knows, is a different thing
//! and is answered differently: the file still registers, the flag takes its
//! **safe** reading, and the key is named in [`Parsed::ignored_keys`] — which
//! is the honest word for what happened (the key was not honored) and reaches
//! the one surface that renders that list, `/verbose`'s `ignored frontmatter:`
//! line. Refusing the file instead would let one typo take a working `/name`
//! away from its user; ignoring it silently would let one typo decide what the
//! model may run.
//!
//! The two safe readings are deliberately **asymmetric**, because the two flags
//! widen in opposite directions:
//!
//! ```text
//! key                       absent      true          false         anything else
//! disable-model-invocation  model may   model may not model may     model may NOT, named
//! user-invocable            user may    user may      user may not  user MAY, named
//! ```
//!
//! In both rows the unreadable value lands on the **narrower** capability for
//! the model and the **unchanged** one for the user: a typo in a repository's
//! frontmatter can hide a skill from the model, and can never hand the model
//! one the user meant to keep to themselves, nor take `/name` away from a user
//! who has it today.
//!
//! Only the two literals `true` and `false` are boolean, unquoted or wrapped in
//! one matching pair of quotes (the `unquote` step runs first, so
//! `user-invocable: "false"` is the literal). `True`, `yes`, `1` and the empty
//! value are not — this parser has never guessed at a value's intent, and a key
//! that decides what a model may run is the last place to start.
//!
//! # Why malformed is total
//!
//! A file this parser cannot read whole is **skipped whole** — never
//! half-parsed into a registered skill. `teton_core::config::parse_search_auth`
//! is the shipped precedent and its doc carries the argument: a header is a
//! *shape*, and half-parsing is how a value that looks accepted behaves
//! differently than it reads. Here the stakes are one step higher than a
//! header template — a half-parsed skill registers a name whose body the user
//! did not sanction, under a permission key that then remembers a grant for it.
//!
//! The narrowness is affordable precisely because nothing in the header is a
//! setting. The only cost of refusing a shape is that one skill is *named* as
//! skipped, with a reason the user can act on.

/// A file's frontmatter and its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    /// The `name` key, if declared. It does **not** decide the dispatchable
    /// spelling — the directory or stem does (BR-2) — and a `name` that
    /// differs is recorded as a note by the caller.
    pub name: Option<String>,
    /// The `description` key, if declared.
    pub description: Option<String>,
    /// The `argument-hint` key, if declared.
    pub argument_hint: Option<String>,
    /// Whether the model may invoke this skill through the `skill` tool
    /// (REQ-587 BR-3).
    ///
    /// `false` exactly when `disable-model-invocation` read `true` — the key is
    /// a **negative**, so this field is its inverse rather than its literal —
    /// or read a value that is not a boolean literal at all, which is the safe
    /// reading (see the module doc's table).
    pub model_invocable: bool,
    /// Whether the user may dispatch this skill by typing `/name` (REQ-587
    /// BR-3).
    ///
    /// `false` exactly when `user-invocable` read `false`. A value that is not
    /// a boolean literal leaves it `true`, which is the safe reading in this
    /// direction: a typo must not take a working command away from the person
    /// who wrote it.
    pub user_invocable: bool,
    /// Every other key, in the order it appeared. Inert; listed by `/verbose`
    /// so a user who wrote `model: opus` learns it did nothing rather than
    /// believing it did something.
    ///
    /// An invocation flag whose *value* this parser could not read is named
    /// here too, for the same reason and in the same words: the key was not
    /// honored. It is the only way either flag reaches this list — a flag that
    /// parsed left it with REQ-587 BR-3.
    pub ignored_keys: Vec<String>,
    /// Everything after the closing delimiter, verbatim — including a leading
    /// blank line (BR-13: the body is passed as written).
    pub body: String,
}

impl Default for Parsed {
    /// A file with no frontmatter at all: no keys, nothing ignored, and **both
    /// invocation flags on**.
    ///
    /// Hand-written because that last clause is the whole point and `derive`
    /// cannot say it: `bool::default()` is `false`, which would make every
    /// `.claude/commands/*.md` on the machine — the majority case, which
    /// declares no frontmatter whatsoever — invisible to the model and
    /// undispatchable by its owner.
    fn default() -> Self {
        Self {
            name: None,
            description: None,
            argument_hint: None,
            model_invocable: true,
            user_invocable: true,
            ignored_keys: Vec::new(),
            body: String::new(),
        }
    }
}

/// A frontmatter block this parser will not read.
///
/// The fields are for tests and logs. The **user-facing** word is one word —
/// `malformed frontmatter` ([`super::SkipReason::MalformedFrontmatter`]) — so
/// the two cannot drift: nothing renders this struct to a surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Malformed {
    /// 1-based line number of the offending line. The unterminated case names
    /// the opening delimiter, which is the line the user has to fix.
    pub line: usize,
    /// What was wrong, in the parser's own words.
    pub what: &'static str,
}

/// Parse `text` into a header and a body, or refuse it whole.
///
/// See the module documentation for the grammar and for why refusal is total.
///
/// # Errors
///
/// [`Malformed`] when a leading `---` opens a block that has no closing `---`,
/// or when a line inside the block is not blank, a comment or a `key: value`.
pub fn parse(text: &str) -> Result<Parsed, Malformed> {
    let mut lines = Lines::new(text);
    let Some(first) = lines.next() else {
        // An empty file: no frontmatter, an empty body. A skill whose body is
        // empty is a skill that expands to nothing, which is the user's
        // business and not a parse failure.
        return Ok(Parsed::default());
    };
    if trim_cr(first.text) != "---" {
        return Ok(Parsed {
            body: text.to_owned(),
            ..Parsed::default()
        });
    }

    let mut parsed = Parsed::default();
    let mut seen_model_flag = false;
    let mut seen_user_flag = false;
    // The opening delimiter was line 1, so the block's first line is line 2 —
    // the numbers a user reads off their editor's gutter.
    for (line_number, line) in (2..).zip(lines) {
        let content = trim_cr(line.text);
        if content == "---" {
            parsed.body = text[line.end..].to_owned();
            return Ok(parsed);
        }
        if content.trim().is_empty() {
            continue;
        }
        // An indented line is the shape a nested block, a list continuation and
        // a folded scalar all arrive in. Refusing it here is what keeps a
        // nested `tools:` block from parsing as an empty `tools` key with the
        // nesting silently discarded.
        if content.starts_with([' ', '\t']) {
            return Err(Malformed {
                line: line_number,
                what: "an indented line — this parser reads flat `key: value` only",
            });
        }
        if content.starts_with('#') {
            continue;
        }
        let Some((key, value)) = content.split_once(':') else {
            return Err(Malformed {
                line: line_number,
                what: "not a `key: value` line",
            });
        };
        if !is_key(key) {
            return Err(Malformed {
                line: line_number,
                what: "the key is not `^[a-z][a-z0-9-]*$`",
            });
        }
        let value = unquote(value.trim());
        match key {
            // First occurrence wins, and a repeat is not a second value: a
            // total parser refuses only shapes it cannot read, and a duplicate
            // key is readable — it just does not get to change an answer
            // already given.
            "name" => set_once(&mut parsed.name, value),
            "description" => set_once(&mut parsed.description, value),
            "argument-hint" => set_once(&mut parsed.argument_hint, value),
            // The same first-occurrence-wins rule, spelled with a `seen` flag
            // because the answer is a `bool` and there is no `None` in it to
            // test. A repeat decides nothing — including nothing about the
            // diagnostic: `user-invocable: false` followed by
            // `user-invocable: yes` is answered, so the second line is not a
            // key that went unhonored.
            "disable-model-invocation" => {
                if !seen_model_flag {
                    seen_model_flag = true;
                    parsed.model_invocable = match boolean(&value) {
                        // A negative key: `true` is the hiding value, so the
                        // stored answer is its inverse.
                        Some(hidden) => !hidden,
                        None => {
                            name_ignored(&mut parsed.ignored_keys, key);
                            // The safe reading: hidden from the model.
                            false
                        }
                    };
                }
            }
            "user-invocable" => {
                if !seen_user_flag {
                    seen_user_flag = true;
                    parsed.user_invocable = match boolean(&value) {
                        Some(allowed) => allowed,
                        None => {
                            name_ignored(&mut parsed.ignored_keys, key);
                            // The safe reading in this direction is the
                            // *unchanged* one: the user keeps `/name`.
                            true
                        }
                    };
                }
            }
            other => name_ignored(&mut parsed.ignored_keys, other),
        }
    }

    Err(Malformed {
        line: 1,
        what: "the frontmatter block has no closing `---`",
    })
}

/// Set `slot` only if it is empty — see the duplicate-key note at the call
/// site.
fn set_once(slot: &mut Option<String>, value: String) {
    if slot.is_none() {
        *slot = Some(value);
    }
}

/// Name `key` in the ignored list, once.
///
/// One home for that list's two writers — the keys this parser does not read at
/// all, and an invocation flag whose value it could not read — so a user who
/// wrote both sees one line in one order and never the same key twice.
fn name_ignored(keys: &mut Vec<String>, key: &str) {
    if !keys.iter().any(|seen| seen == key) {
        keys.push(key.to_owned());
    }
}

/// The two boolean literals, and nothing else.
///
/// `None` is "this is not a boolean", which the caller answers with the flag's
/// safe reading and a named key — never with a refusal of the file, and never
/// with a guess. `True`, `yes`, `on`, `1` and an empty value all land here on
/// purpose: the rest of this parser stores values verbatim, so the one place it
/// interprets one is the one place a lenient reading could silently widen what
/// a model may run (REQ-587 BR-3).
fn boolean(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// `^[a-z][a-z0-9-]*$`, written out. The key vocabulary is Teton's own plus
/// whatever other tools put in these files, all of which spell keys this way.
fn is_key(key: &str) -> bool {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_lowercase()
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Strip one matching pair of surrounding quotes, and only when the quote
/// character appears nowhere inside.
///
/// The one YAML nicety this parser keeps, because `description: "…: …"` is how
/// a real file writes a description containing a colon, and because the
/// conservative test means a value like `"a" and "b"` is left exactly as
/// written rather than half-stripped. No escape processing: there is no escape
/// syntax here to process.
fn unquote(value: &str) -> String {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            if !inner.contains(quote) {
                return inner.to_owned();
            }
        }
    }
    value.to_owned()
}

/// Drop a single trailing `\r`, so a file written on Windows is read rather
/// than refused whole for a line ending.
fn trim_cr(line: &str) -> &str {
    line.strip_suffix('\r').unwrap_or(line)
}

/// One line and the byte offset just past its terminator — enough to hand back
/// the body as a slice of the original text rather than re-joining it, which is
/// how a re-joined body comes to differ from the file by a line ending.
struct Line<'a> {
    text: &'a str,
    end: usize,
}

/// `str::lines`, plus the offsets.
struct Lines<'a> {
    text: &'a str,
    at: usize,
}

impl<'a> Lines<'a> {
    fn new(text: &'a str) -> Self {
        Self { text, at: 0 }
    }
}

impl<'a> Iterator for Lines<'a> {
    type Item = Line<'a>;

    fn next(&mut self) -> Option<Line<'a>> {
        if self.at >= self.text.len() {
            return None;
        }
        let rest = &self.text[self.at..];
        let (text, end) = match rest.find('\n') {
            Some(newline) => (&rest[..newline], self.at + newline + 1),
            None => (rest, self.text.len()),
        };
        self.at = end;
        Some(Line { text, end })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The common case: `.claude/commands/*.md` with no header at all. The
    /// whole file is the body and nothing was ignored — refusing these would
    /// refuse half of BR-1's four globs.
    #[test]
    fn a_file_with_no_leading_delimiter_is_all_body() {
        let text = "# Deploy\n\nRun the deploy checklist.\n";
        let parsed = parse(text).expect("no frontmatter is not a failure");
        assert_eq!(parsed.body, text, "the body is the file, byte for byte");
        assert_eq!(parsed.name, None);
        assert_eq!(parsed.description, None);
        assert_eq!(parsed.argument_hint, None);
        assert!(parsed.ignored_keys.is_empty(), "zero ignored keys");

        // A `---` that is not the first line does not open a block either.
        let later = "intro\n---\nname: x\n---\n";
        assert_eq!(parse(later).unwrap().body, later);
    }

    #[test]
    fn the_three_known_keys_are_read_and_the_body_starts_after_the_delimiter() {
        let parsed = parse(
            "---\nname: status\ndescription: Show current state\nargument-hint: [REQ-xxx]\n---\n\n# /status\nbody line\n",
        )
        .expect("a well-formed header parses");
        assert_eq!(parsed.name.as_deref(), Some("status"));
        assert_eq!(parsed.description.as_deref(), Some("Show current state"));
        assert_eq!(parsed.argument_hint.as_deref(), Some("[REQ-xxx]"));
        assert!(parsed.ignored_keys.is_empty());
        assert_eq!(
            parsed.body, "\n# /status\nbody line\n",
            "the body is verbatim from just past the closing delimiter"
        );
    }

    /// A description with a colon, an em dash and quotes survives whole: these
    /// are real shipped ADLC descriptions, not invented edge cases.
    #[test]
    fn a_value_keeps_its_colons_arrows_and_interior_quotes() {
        let parsed = parse(concat!(
            "---\n",
            "description: End-to-end pipeline: validate → fix → architect. Use when the user says \"proceed\".\n",
            "---\nbody",
        ))
        .unwrap();
        assert_eq!(
            parsed.description.as_deref(),
            Some(
                "End-to-end pipeline: validate → fix → architect. Use when the user says \"proceed\"."
            )
        );
    }

    #[test]
    fn one_matching_pair_of_surrounding_quotes_is_stripped_and_nothing_else_is() {
        let parsed = parse(
            "---\nname: \"quoted\"\ndescription: 'single'\nargument-hint: \"a\" and \"b\"\n---\n",
        )
        .unwrap();
        assert_eq!(parsed.name.as_deref(), Some("quoted"));
        assert_eq!(parsed.description.as_deref(), Some("single"));
        assert_eq!(
            parsed.argument_hint.as_deref(),
            Some("\"a\" and \"b\""),
            "a value that merely begins and ends with a quote is left as written"
        );
    }

    /// Every key that is not one of the five is listed and does nothing. This
    /// is REQ-585 BR-5's claim, minus the two keys REQ-587 BR-3 made
    /// meaningful: a skill file is still content and not configuration, and
    /// `allowed-tools`, `model` and `hooks` still grant nothing.
    #[test]
    fn every_other_key_is_listed_and_inert() {
        let parsed = parse(
            "---\nname: x\nallowed-tools: Bash\nmodel: opus\nhooks: none\nmodel: sonnet\n---\nbody",
        )
        .unwrap();
        assert_eq!(
            parsed.ignored_keys,
            vec!["allowed-tools", "model", "hooks"],
            "in file order, each named once"
        );
        assert_eq!(parsed.name.as_deref(), Some("x"));
        assert!(
            parsed.model_invocable && parsed.user_invocable,
            "a file that declares neither flag is invocable by both"
        );
    }

    /// **REQ-587 BR-3.** The two invocation flags are read, and reading them is
    /// exactly what takes them out of the inert list.
    ///
    /// The negative key is the one worth pinning both ways: the field is the
    /// *inverse* of `disable-model-invocation`, so a reading that forgot the
    /// negation would pass a fixture that only ever wrote `true`.
    #[test]
    fn the_two_invocation_flags_are_read_and_leave_the_ignored_list() {
        let hidden = parse(
            "---\nname: beta\ndisable-model-invocation: true\nuser-invocable: true\n---\nbody",
        )
        .unwrap();
        assert!(!hidden.model_invocable, "`true` hides it from the model");
        assert!(hidden.user_invocable, "and says nothing about the user");

        let model_only = parse("---\nname: delta\nuser-invocable: false\n---\nbody").unwrap();
        assert!(!model_only.user_invocable, "`false` is model-only");
        assert!(
            model_only.model_invocable,
            "a model-only skill is the model's — that is the whole state"
        );

        let both_said_out_loud =
            parse("---\ndisable-model-invocation: false\nuser-invocable: true\n---\nbody").unwrap();
        assert!(both_said_out_loud.model_invocable);
        assert!(both_said_out_loud.user_invocable);

        for parsed in [&hidden, &model_only, &both_said_out_loud] {
            assert!(
                parsed.ignored_keys.is_empty(),
                "a flag that parsed is honored, not listed as ignored: {:?}",
                parsed.ignored_keys
            );
        }

        // Both flags off is a real state — invocable by nobody — and it parses
        // like any other, because refusing it here would hide it rather than
        // name it (BR-3).
        let nobody =
            parse("---\ndisable-model-invocation: true\nuser-invocable: false\n---\nbody").unwrap();
        assert!(!nobody.model_invocable && !nobody.user_invocable);
    }

    /// **BR-3's safe values, which are asymmetric on purpose.** A value that is
    /// not a boolean literal is not a malformed *file*: the header still parses,
    /// the flag takes the reading that cannot widen what the model may run, and
    /// the key is named — so a typo is visible instead of silent.
    #[test]
    fn a_flag_value_that_is_not_a_boolean_takes_the_safe_reading_and_is_named() {
        for value in ["yes", "True", "1", "", "  ", "maybe"] {
            let text = format!("---\ndisable-model-invocation: {value}\n---\nbody");
            let parsed = parse(&text).expect("a bad value is not a malformed file");
            assert!(
                !parsed.model_invocable,
                "`disable-model-invocation: {value}` must read as hidden"
            );
            assert_eq!(
                parsed.ignored_keys,
                vec!["disable-model-invocation"],
                "and must be named rather than silently ignored"
            );

            let text = format!("---\nuser-invocable: {value}\n---\nbody");
            let parsed = parse(&text).expect("a bad value is not a malformed file");
            assert!(
                parsed.user_invocable,
                "`user-invocable: {value}` must leave the user's `/name` alone"
            );
            assert_eq!(parsed.ignored_keys, vec!["user-invocable"]);
        }

        // The body still arrives, which is the point of not refusing the file.
        assert_eq!(
            parse("---\nuser-invocable: nope\n---\nbody").unwrap().body,
            "body"
        );

        // `unquote` runs first, so a quoted literal is a literal — the same
        // nicety `description: "…"` gets, and the reason it is worth a line
        // here is that the alternative reads as a typo and is not one.
        let quoted = parse("---\nuser-invocable: \"false\"\n---\n").unwrap();
        assert!(!quoted.user_invocable);
        assert!(quoted.ignored_keys.is_empty());
    }

    /// A repeat decides nothing — including nothing about the diagnostic. The
    /// second line is not a key that went unhonored; the first line answered.
    #[test]
    fn a_repeated_invocation_flag_does_not_change_an_answer_already_given() {
        let parsed = parse(
            "---\nuser-invocable: false\nuser-invocable: true\ndisable-model-invocation: true\ndisable-model-invocation: bogus\n---\n",
        )
        .unwrap();
        assert!(!parsed.user_invocable, "the first occurrence won");
        assert!(!parsed.model_invocable, "the first occurrence won");
        assert!(
            parsed.ignored_keys.is_empty(),
            "the repeat's unreadable value names nothing: {:?}",
            parsed.ignored_keys
        );
    }

    #[test]
    fn a_repeated_known_key_does_not_change_an_answer_already_given() {
        let parsed = parse("---\nname: first\nname: second\n---\n").unwrap();
        assert_eq!(parsed.name.as_deref(), Some("first"));
    }

    #[test]
    fn blank_lines_and_flush_left_comments_are_allowed_inside_the_block() {
        let parsed = parse("---\n\n# a comment\nname: x\n\n---\nbody").unwrap();
        assert_eq!(parsed.name.as_deref(), Some("x"));
        assert_eq!(parsed.body, "body");
    }

    /// The four malformed shapes ADR-5 names, each refused whole rather than
    /// half-read. A caller that gets `Err` skips the file; there is no partial
    /// value to be tempted by, because none is returned.
    #[test]
    fn an_unterminated_indented_nested_or_list_block_is_malformed() {
        let unterminated = parse("---\nname: x\ndescription: y\n");
        assert_eq!(
            unterminated.unwrap_err().line,
            1,
            "no closing `---`: the opening delimiter is the line to fix"
        );
        // A file that opens a block and then just carries on in prose is
        // refused too — at whichever line first fails, which is the earlier
        // and more useful complaint.
        assert!(parse("---\nname: x\nprose, not a key\n").is_err());

        let indented = parse("---\nname: x\n  continued: here\n---\n");
        assert_eq!(indented.unwrap_err().line, 3, "an indented continuation");

        let nested = parse("---\ntools:\n  - Bash\n---\n");
        assert_eq!(nested.unwrap_err().line, 3, "a nested block");

        let list = parse("---\n- Bash\n---\n");
        assert_eq!(list.unwrap_err().line, 2, "a list item");

        let bare = parse("---\njust a sentence\n---\n");
        assert_eq!(bare.unwrap_err().line, 2, "no colon at all");

        let shouty = parse("---\nName: x\n---\n");
        assert_eq!(
            shouty.unwrap_err().line,
            2,
            "the key vocabulary is lowercase"
        );
    }

    /// An unterminated block is malformed even though its first line parses —
    /// the pin for "never half-parsed": there is no `Ok` with `name: x` in it.
    #[test]
    fn an_unterminated_block_yields_no_partial_value() {
        assert!(parse("---\nname: usable\n").is_err());
    }

    #[test]
    fn a_crlf_file_is_read_rather_than_refused() {
        let parsed = parse("---\r\nname: x\r\n---\r\nbody\r\n").expect("CRLF is not malformed");
        assert_eq!(parsed.name.as_deref(), Some("x"));
        assert_eq!(parsed.body, "body\r\n");
    }

    #[test]
    fn an_empty_file_is_an_empty_body() {
        assert_eq!(parse(""), Ok(Parsed::default()));
    }

    /// A header with no body at all still parses — `---\nname: x\n---` with
    /// nothing after it is a skill that expands to nothing, not a broken file.
    #[test]
    fn a_closing_delimiter_at_end_of_file_leaves_an_empty_body() {
        let parsed = parse("---\nname: x\n---").unwrap();
        assert_eq!(parsed.name.as_deref(), Some("x"));
        assert_eq!(parsed.body, "");
    }
}
