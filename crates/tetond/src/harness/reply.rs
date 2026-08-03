//! Model-reply scanning: turn boundaries, tool-call extraction, and the
//! display gate (BUG-147).
//!
//! The local tier is a plain text engine driven by a flat transcript rendering
//! (`User:` / `Assistant:` / `Tool (name):` blocks). Left unchecked, a weak
//! model completes past its own turn: it fabricates tool results, fake future
//! turns, and batches of tool calls until the token cap cuts it mid-JSON — and
//! the raw mess used to be streamed to the user verbatim and folded back into
//! context, compounding every turn. This module is the containment:
//!
//! - [`ReplyScanner`] watches the token stream and says when the turn is
//!   *over*: the first complete top-level JSON object carrying a tool key, or
//!   the model starting to fabricate a transcript frame at a line start. The
//!   local source uses it to stop generation early (the [`Engine`] callback's
//!   `bool`), and to cut the fabricated tail off the reply text before it ever
//!   reaches context.
//! - [`parse_reply`] turns a (cut) reply into the turn decision, the clean
//!   length to fold into context, and a count of *extra* tool calls the
//!   one-tool-per-turn harness will not run — so the loop can tell the model
//!   instead of silently dropping them (the silent drop is what caused the
//!   re-emit loop).
//! - [`StreamGate`] sits between the token stream and the user-facing
//!   `agent_message` events: prose flows through live, but a candidate tool
//!   call is held back (the tool status line already presents it) and a
//!   fabricated frame suppresses the rest of the stream.
//!
//! [`Engine`]: teton_inference::Engine

use serde_json::Value;

/// Transcript-frame markers the model can only be *fabricating*: the flat
/// rendering writes these at line starts, so a generated one means the model is
/// continuing the transcript instead of speaking its own turn. `<tool-result`
/// is the untrusted-content envelope built-in results are framed with — a
/// generated one is a fake tool result.
const FRAME_MARKERS: &[&str] = &["User:", "Assistant:", "Tool (", "<tool-result"];

/// What the parser made of one model reply.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ParsedTurn {
    /// A well-formed call to a known tool.
    ToolCall {
        /// Tool name.
        name: String,
        /// Argument object.
        arguments: Value,
    },
    /// No tool call — the model's final answer.
    EndTurn(String),
    /// Something tool-call-shaped but invalid (unknown tool, non-object args).
    Malformed(String),
}

/// A parsed reply: the decision, how much of the text belongs in context, and
/// how many additional tool calls were present but will not run.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ParsedReply {
    /// The turn decision.
    pub turn: ParsedTurn,
    /// Byte length of the reply prefix that should be folded into context —
    /// for a tool call, through the end of the dispatched object; everything
    /// after it (further calls, trailing chatter) is noise.
    pub clean_len: usize,
    /// Tool-call-shaped objects after the first — present in the reply but
    /// not executed by the one-tool-per-turn harness.
    pub dropped_calls: u32,
}

/// Parse a model reply into a tool call, an end-of-turn answer, or a malformed
/// call. A reply is a tool call only if it contains a JSON object with a `tool`
/// (or `name`) key; anything else is treated as the final answer.
pub(crate) fn parse_reply(text: &str, known_tools: &[&str]) -> ParsedReply {
    let spans = json_object_spans(text);
    let mut first_call: Option<(usize, ParsedTurn)> = None;
    let mut dropped_calls = 0u32;

    for (start, end) in spans {
        let candidate = &text[start..end];
        let Ok(value) = serde_json::from_str::<Value>(candidate) else {
            continue;
        };
        let name = value
            .get("tool")
            .or_else(|| value.get("name"))
            .and_then(Value::as_str);
        let Some(name) = name else {
            // JSON without a tool key: not a tool call. Keep scanning in case a
            // real call follows; if none is found this becomes an end-of-turn.
            continue;
        };

        if first_call.is_some() {
            dropped_calls += 1;
            continue;
        }

        let arguments = value
            .get("arguments")
            .or_else(|| value.get("input"))
            .or_else(|| value.get("args"))
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

        let turn = if !known_tools.contains(&name) {
            ParsedTurn::Malformed(format!("`{name}` is not an available tool"))
        } else if !arguments.is_object() {
            ParsedTurn::Malformed(format!("arguments for `{name}` must be a JSON object"))
        } else {
            ParsedTurn::ToolCall {
                name: name.to_owned(),
                arguments,
            }
        };
        first_call = Some((end, turn));
    }

    match first_call {
        Some((end, turn)) => ParsedReply {
            turn,
            clean_len: end,
            dropped_calls,
        },
        None => ParsedReply {
            turn: ParsedTurn::EndTurn(text.trim().to_owned()),
            clean_len: text.len(),
            dropped_calls: 0,
        },
    }
}

/// Every top-level `{...}` object as a byte span (`start..end`, end exclusive),
/// ignoring braces inside JSON strings. Robust to prose or code fences around
/// the object.
fn json_object_spans(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (i, &b) in bytes.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => {
                if depth == 0 {
                    start = i;
                }
                depth += 1;
            }
            b'}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    // Brace boundaries are ASCII, so the slice is UTF-8 safe.
                    out.push((start, i + 1));
                }
            }
            _ => {}
        }
    }
    out
}

/// Why the scanner ended the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// A top-level JSON object carrying a tool key closed at this byte
    /// (exclusive end); the object itself starts at `.0`.
    ToolObject { start: usize, end: usize },
    /// A fabricated transcript-frame marker begins at this byte.
    FrameMarker { at: usize },
}

/// Incremental scanner over a streaming model reply.
///
/// Feed it every token via [`ReplyScanner::push`]; it returns whether
/// generation should continue. After the stream ends (either because the
/// scanner stopped it or the model finished on its own), [`ReplyScanner::context_cut`]
/// is the byte length of the reply that belongs in context, and
/// [`ReplyScanner::flushable_len`] drives the display gate.
#[derive(Debug)]
pub(crate) struct ReplyScanner {
    buf: String,
    /// Bytes of `buf` fully processed by the state machine.
    pos: usize,
    depth: usize,
    in_string: bool,
    escaped: bool,
    obj_start: usize,
    stop: Option<Stop>,
}

impl ReplyScanner {
    pub(crate) fn new() -> Self {
        Self {
            buf: String::new(),
            pos: 0,
            depth: 0,
            in_string: false,
            escaped: false,
            obj_start: 0,
            stop: None,
        }
    }

    /// Scan a complete reply in one pass (the non-streaming entry point).
    pub(crate) fn scan_all(text: &str) -> Self {
        let mut scanner = Self::new();
        scanner.push(text);
        scanner
    }

    /// Append a streamed chunk and process it. Returns `false` when the turn is
    /// over and generation should stop.
    pub(crate) fn push(&mut self, chunk: &str) -> bool {
        if self.stop.is_some() {
            return false;
        }
        self.buf.push_str(chunk);
        self.process();
        self.stop.is_none()
    }

    fn process(&mut self) {
        while self.pos < self.buf.len() && self.stop.is_none() {
            let bytes = self.buf.as_bytes();
            let b = bytes[self.pos];

            if self.depth == 0 {
                // Frame-marker detection only applies at a line start outside
                // any JSON object (inside one, e.g. an `edit` writing a doc,
                // the text is argument content, not a fabricated frame).
                let line_start = self.pos == 0 || bytes[self.pos - 1] == b'\n';
                if line_start {
                    let tail = &self.buf[self.pos..];
                    if let Some(_marker) = FRAME_MARKERS.iter().find(|m| tail.starts_with(*m)) {
                        self.stop = Some(Stop::FrameMarker { at: self.pos });
                        return;
                    }
                    // The tail could still *become* a marker ("Use" → "User:").
                    // Wait for more bytes rather than advancing past it.
                    if !tail.is_empty() && FRAME_MARKERS.iter().any(|m| m.starts_with(tail)) {
                        return;
                    }
                }
                if b == b'{' {
                    self.obj_start = self.pos;
                    self.depth = 1;
                }
                self.pos += 1;
                continue;
            }

            // Inside a top-level object: JSON string/brace tracking.
            if self.in_string {
                if self.escaped {
                    self.escaped = false;
                } else if b == b'\\' {
                    self.escaped = true;
                } else if b == b'"' {
                    self.in_string = false;
                }
                self.pos += 1;
                continue;
            }
            match b {
                b'"' => self.in_string = true,
                b'{' => self.depth += 1,
                b'}' => {
                    self.depth -= 1;
                    if self.depth == 0 {
                        let end = self.pos + 1;
                        let body = &self.buf[self.obj_start..end];
                        if body.contains("\"tool\"") || body.contains("\"name\"") {
                            self.stop = Some(Stop::ToolObject {
                                start: self.obj_start,
                                end,
                            });
                            self.pos = end;
                            return;
                        }
                    }
                }
                _ => {}
            }
            self.pos += 1;
        }
    }

    /// Byte length of the reply that belongs in context: through the end of a
    /// completed tool-call object, or up to (excluding) a fabricated frame
    /// marker. The whole reply when no stop fired.
    pub(crate) fn context_cut(&self) -> usize {
        match self.stop {
            Some(Stop::ToolObject { end, .. }) => end,
            Some(Stop::FrameMarker { at }) => at,
            None => self.buf.len(),
        }
    }

    /// How much of the reply is safe to *display* right now: prose up to any
    /// held-back candidate (an open top-level object, a possible marker
    /// prefix), the tool-call object itself excluded (the tool status line
    /// presents it).
    pub(crate) fn flushable_len(&self) -> usize {
        match self.stop {
            Some(Stop::ToolObject { start, .. }) => start,
            Some(Stop::FrameMarker { at }) => at,
            None if self.depth > 0 => self.obj_start,
            // `pos` stalls at a line start that could still become a frame
            // marker, so it is exactly the safe boundary.
            None => self.pos,
        }
    }

    /// Whether the scanner ended the turn (vs. the model finishing on its own).
    pub(crate) fn stopped(&self) -> bool {
        self.stop.is_some()
    }
}

/// The display gate between the raw token stream and `agent_message` events.
///
/// Prose streams through live. A candidate tool call (any top-level JSON
/// object) is held back until it resolves: an object *without* a tool key
/// flushes (it was part of the answer), one *with* a tool key is dropped — the
/// tool status line already presents the call, and raw JSON in the transcript
/// is exactly the BUG-147 experience. A fabricated frame marker suppresses the
/// rest of the stream.
pub(crate) struct StreamGate {
    scanner: ReplyScanner,
    emitted: usize,
}

impl StreamGate {
    pub(crate) fn new() -> Self {
        Self {
            scanner: ReplyScanner::new(),
            emitted: 0,
        }
    }

    /// Feed a streamed chunk; returns the text (if any) now safe to display.
    pub(crate) fn push(&mut self, chunk: &str) -> Option<String> {
        self.scanner.push(chunk);
        self.flush_to(self.scanner.flushable_len())
    }

    /// The stream ended. `final_answer` is true when the turn's decision is an
    /// end-of-turn: any held text was the answer, not a tool call, so it
    /// flushes. On a tool call (or after a fabricated frame) the held tail is
    /// dropped.
    pub(crate) fn finish(mut self, final_answer: bool) -> Option<String> {
        if final_answer && !self.scanner.stopped() {
            self.flush_to(self.scanner.context_cut())
        } else {
            self.flush_to(self.scanner.flushable_len())
        }
    }

    fn flush_to(&mut self, upto: usize) -> Option<String> {
        if upto <= self.emitted {
            return None;
        }
        let out = self.scanner.buf[self.emitted..upto].to_owned();
        self.emitted = upto;
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOOLS: &[&str] = &["read", "edit", "grep"];

    #[test]
    fn parses_a_plain_tool_call() {
        let parsed = parse_reply(r#"{"tool":"read","arguments":{"path":"a.rs"}}"#, TOOLS);
        assert_eq!(
            parsed.turn,
            ParsedTurn::ToolCall {
                name: "read".to_owned(),
                arguments: serde_json::json!({ "path": "a.rs" }),
            }
        );
        assert_eq!(parsed.dropped_calls, 0);
    }

    #[test]
    fn parses_a_fenced_tool_call_with_prose() {
        let text =
            "I'll read the file.\n```json\n{\"tool\": \"read\", \"input\": {\"path\": \"a.rs\"}}\n```";
        let parsed = parse_reply(text, TOOLS);
        assert!(matches!(parsed.turn, ParsedTurn::ToolCall { .. }));
        // The clean cut keeps the prose and the call, drops the trailing fence.
        assert!(text[..parsed.clean_len].ends_with('}'));
    }

    #[test]
    fn plain_text_is_end_of_turn() {
        let parsed = parse_reply("All done. The file now returns 2.", TOOLS);
        assert_eq!(
            parsed.turn,
            ParsedTurn::EndTurn("All done. The file now returns 2.".to_owned())
        );
        assert_eq!(parsed.clean_len, "All done. The file now returns 2.".len());
    }

    #[test]
    fn unknown_tool_is_malformed_not_end_of_turn() {
        let parsed = parse_reply(r#"{"tool":"delete_everything","arguments":{}}"#, TOOLS);
        assert!(matches!(parsed.turn, ParsedTurn::Malformed(_)));
    }

    #[test]
    fn non_object_arguments_are_malformed() {
        let parsed = parse_reply(r#"{"tool":"read","arguments":"a.rs"}"#, TOOLS);
        assert!(matches!(parsed.turn, ParsedTurn::Malformed(_)));
    }

    #[test]
    fn braces_inside_strings_do_not_break_scanning() {
        let spans = json_object_spans(r#"{"tool":"grep","arguments":{"pattern":"a}b{c"}}"#);
        assert_eq!(spans.len(), 1);
    }

    #[test]
    fn extra_tool_calls_are_counted_and_cut_from_the_clean_text() {
        // BUG-147: the model batches several calls; only the first runs. The
        // parse reports how many were dropped and where the clean text ends.
        let text = "{\"tool\":\"read\",\"arguments\":{\"path\":\"a.rs\"}}\n\
                    {\"tool\":\"read\",\"arguments\":{\"path\":\"b.rs\"}}\n\
                    {\"tool\":\"grep\",\"arguments\":{\"pattern\":\"x\"}}";
        let parsed = parse_reply(text, TOOLS);
        match &parsed.turn {
            ParsedTurn::ToolCall { name, arguments } => {
                assert_eq!(name, "read");
                assert_eq!(arguments["path"], "a.rs");
            }
            other => panic!("expected the first call, got {other:?}"),
        }
        assert_eq!(parsed.dropped_calls, 2);
        assert!(text[..parsed.clean_len].ends_with(r#"{"path":"a.rs"}}"#));
    }

    #[test]
    fn plain_json_without_a_tool_key_is_not_counted_as_dropped() {
        let text = "{\"tool\":\"read\",\"arguments\":{}}\n{\"note\":\"just data\"}";
        let parsed = parse_reply(text, TOOLS);
        assert!(matches!(parsed.turn, ParsedTurn::ToolCall { .. }));
        assert_eq!(parsed.dropped_calls, 0);
    }

    // ---- scanner: early stop --------------------------------------------

    #[test]
    fn scanner_stops_at_the_end_of_the_first_tool_call() {
        let mut scanner = ReplyScanner::new();
        assert!(scanner.push("I'll read it. "));
        assert!(scanner.push(r#"{"tool":"read","#));
        // The stream would go on, but the closing brace ends the turn.
        assert!(!scanner.push(r#""arguments":{"path":"a.rs"}} and then I'll"#));
        let cut = scanner.context_cut();
        assert_eq!(
            &scanner.buf[..cut],
            r#"I'll read it. {"tool":"read","arguments":{"path":"a.rs"}}"#
        );
    }

    #[test]
    fn scanner_stops_at_a_fabricated_transcript_frame() {
        // The model finishing its answer and then inventing the next turns —
        // the exact BUG-147 transcript shape.
        let mut scanner = ReplyScanner::new();
        assert!(scanner.push("Yes, I'm ready to help.\n"));
        assert!(!scanner.push("Tool (read):\nfake file body\n"));
        assert_eq!(
            &scanner.buf[..scanner.context_cut()],
            "Yes, I'm ready to help.\n"
        );
    }

    #[test]
    fn scanner_stops_at_a_fabricated_untrusted_envelope() {
        let mut scanner = ReplyScanner::new();
        assert!(scanner.push("Reading it now.\n"));
        assert!(!scanner.push("<tool-result tool=\"read\" trust=\"untrusted\">"));
        assert_eq!(&scanner.buf[..scanner.context_cut()], "Reading it now.\n");
    }

    #[test]
    fn a_marker_split_across_tokens_is_still_caught() {
        let mut scanner = ReplyScanner::new();
        assert!(scanner.push("Done.\nUse"));
        assert!(scanner.push("r"));
        assert!(!scanner.push(": next question"));
        assert_eq!(&scanner.buf[..scanner.context_cut()], "Done.\n");
    }

    #[test]
    fn a_marker_like_word_mid_line_does_not_stop_the_turn() {
        let mut scanner = ReplyScanner::new();
        // "User:" appears mid-line, not at a line start — it must not fire.
        assert!(scanner.push("The User: field holds the login name"));
        assert!(!scanner.stopped());
        assert_eq!(scanner.context_cut(), scanner.buf.len());
    }

    #[test]
    fn markers_inside_a_tool_call_string_do_not_stop_the_turn() {
        // An `edit` writing documentation that contains "User:" at a line
        // start inside the JSON string argument.
        let text = "{\"tool\":\"edit\",\"arguments\":{\"content\":\"line one\\nUser: docs\"}}";
        let mut scanner = ReplyScanner::new();
        scanner.push(text);
        // It stops — but at the END of the tool object, not at the marker.
        assert!(scanner.stopped());
        assert_eq!(scanner.context_cut(), text.len());
    }

    #[test]
    fn a_reply_with_no_stop_keeps_everything() {
        let mut scanner = ReplyScanner::new();
        assert!(scanner.push("All done. The file now returns 2."));
        assert_eq!(scanner.context_cut(), scanner.buf.len());
        assert!(!scanner.stopped());
    }

    #[test]
    fn scan_all_cuts_the_fabricated_tail() {
        let text = "Answer.\nUser:\nfake question\nAssistant:\nfake reply";
        let scanner = ReplyScanner::scan_all(text);
        assert_eq!(&text[..scanner.context_cut()], "Answer.\n");
    }

    // ---- stream gate ----------------------------------------------------

    /// Drive a gate over `chunks` and return (streamed-live, flushed-at-end).
    fn run_gate(chunks: &[&str], final_answer: bool) -> (String, String) {
        let mut gate = StreamGate::new();
        let mut live = String::new();
        for chunk in chunks {
            if let Some(out) = gate.push(chunk) {
                live.push_str(&out);
            }
        }
        let tail = gate.finish(final_answer).unwrap_or_default();
        (live, tail)
    }

    #[test]
    fn gate_streams_prose_live() {
        let (live, tail) = run_gate(&["Hello ", "there, ", "working on it."], true);
        assert_eq!(live, "Hello there, working on it.");
        assert_eq!(tail, "");
    }

    #[test]
    fn gate_holds_back_a_tool_call_and_never_displays_it() {
        let (live, tail) = run_gate(
            &[
                "I'll read the file.\n",
                r#"{"tool":"read","#,
                r#""arguments":{"path":"a.rs"}}"#,
            ],
            false,
        );
        assert_eq!(live, "I'll read the file.\n");
        assert_eq!(tail, "", "the raw tool-call JSON must not reach the user");
    }

    #[test]
    fn gate_flushes_plain_json_that_was_part_of_the_answer() {
        let (live, tail) = run_gate(
            &["The config is:\n", "{\"port\": 8080}", "\nas requested."],
            true,
        );
        assert_eq!(
            format!("{live}{tail}"),
            "The config is:\n{\"port\": 8080}\nas requested."
        );
    }

    #[test]
    fn gate_suppresses_a_fabricated_frame_and_everything_after() {
        let (live, tail) = run_gate(
            &["Yes, ready.\n", "Tool (read):\n", "fake result body\n"],
            true,
        );
        assert_eq!(live, "Yes, ready.\n");
        assert_eq!(tail, "");
    }

    #[test]
    fn gate_flushes_an_unclosed_object_on_end_of_turn() {
        // A final answer that happens to end inside a brace (e.g. a code
        // snippet) still displays in full.
        let (live, tail) = run_gate(&["Use this: fn main() {"], true);
        assert_eq!(format!("{live}{tail}"), "Use this: fn main() {");
    }
}
