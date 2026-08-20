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
//! 4. `name`, `description` and `argument-hint` are read. Every other key lands
//!    in [`Parsed::ignored_keys`] and is inert (BR-5): a skill file grants
//!    nothing, so `allowed-tools`, `model` and `hooks` are *listed*, never
//!    honored.
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parsed {
    /// The `name` key, if declared. It does **not** decide the dispatchable
    /// spelling — the directory or stem does (BR-2) — and a `name` that
    /// differs is recorded as a note by the caller.
    pub name: Option<String>,
    /// The `description` key, if declared.
    pub description: Option<String>,
    /// The `argument-hint` key, if declared.
    pub argument_hint: Option<String>,
    /// Every other key, in the order it appeared. Inert; listed by `/verbose`
    /// so a user who wrote `model: opus` learns it did nothing rather than
    /// believing it did something.
    pub ignored_keys: Vec<String>,
    /// Everything after the closing delimiter, verbatim — including a leading
    /// blank line (BR-13: the body is passed as written).
    pub body: String,
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
            other => {
                let other = other.to_owned();
                if !parsed.ignored_keys.contains(&other) {
                    parsed.ignored_keys.push(other);
                }
            }
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

    /// Every key that is not one of the three is listed and does nothing. This
    /// is BR-5's whole claim: a skill file is content, not configuration.
    #[test]
    fn every_other_key_is_listed_and_inert() {
        let parsed = parse(
            "---\nname: x\nallowed-tools: Bash\nmodel: opus\ndisable-model-invocation: true\nmodel: sonnet\n---\nbody",
        )
        .unwrap();
        assert_eq!(
            parsed.ignored_keys,
            vec!["allowed-tools", "model", "disable-model-invocation"],
            "in file order, each named once"
        );
        assert_eq!(parsed.name.as_deref(), Some("x"));
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
