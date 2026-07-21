//! A minimal Server-Sent Events framer shared by both adapters.
//!
//! Both the Anthropic Messages API and OpenAI-compatible chat/completions stream
//! responses as SSE, so the byte-chunk → event framing lives here once. The
//! framer is fed arbitrary byte chunks (which may split a line — or even a
//! multi-byte UTF-8 character — across a chunk boundary) and yields whole
//! events as their blank-line terminators arrive. It never panics on malformed
//! bytes: invalid UTF-8 is lossily decoded and left for the JSON parser to
//! reject downstream.

/// One parsed SSE event.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct SseEvent {
    /// The `event:` field, if present (Anthropic sets it; OpenAI does not).
    pub(crate) event: Option<String>,
    /// The concatenated `data:` field(s).
    pub(crate) data: String,
}

/// Incremental SSE line framer. Buffers partial input across chunk boundaries.
#[derive(Debug, Default)]
pub(crate) struct SseFramer {
    /// Raw bytes not yet forming a complete line.
    buf: Vec<u8>,
    /// `event:` accumulated for the in-progress event.
    event: Option<String>,
    /// `data:` accumulated for the in-progress event.
    data: String,
    /// Whether any field has been seen since the last dispatch.
    pending: bool,
}

impl SseFramer {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of raw bytes; return any events completed by it.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=nl).collect();
            line.pop(); // drop the '\n'
            if line.last() == Some(&b'\r') {
                line.pop(); // drop a trailing '\r'
            }
            if let Some(ev) = self.consume_line(&line) {
                out.push(ev);
            }
        }
        out
    }

    /// Flush at end of stream: emit any buffered final line and pending event,
    /// even if the stream did not end with a blank line.
    pub(crate) fn finish(&mut self) -> Option<SseEvent> {
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            let line = if line.last() == Some(&b'\r') {
                &line[..line.len() - 1]
            } else {
                &line[..]
            };
            if let Some(ev) = self.consume_line(line) {
                return Some(ev);
            }
        }
        self.take_event()
    }

    /// Process one complete line. Returns an event when the line is the blank
    /// separator that terminates one.
    fn consume_line(&mut self, line: &[u8]) -> Option<SseEvent> {
        let text = String::from_utf8_lossy(line);
        if text.is_empty() {
            return self.take_event();
        }
        if text.starts_with(':') {
            return None; // comment line
        }
        if let Some(value) = text.strip_prefix("event:") {
            self.event = Some(strip_one_leading_space(value).to_string());
            self.pending = true;
        } else if let Some(value) = text.strip_prefix("data:") {
            if !self.data.is_empty() {
                self.data.push('\n');
            }
            self.data.push_str(strip_one_leading_space(value));
            self.pending = true;
        }
        // Other SSE fields (id:, retry:) are irrelevant here and ignored.
        None
    }

    fn take_event(&mut self) -> Option<SseEvent> {
        if !self.pending {
            return None;
        }
        self.pending = false;
        Some(SseEvent {
            event: self.event.take(),
            data: std::mem::take(&mut self.data),
        })
    }
}

/// Per the SSE spec, a single leading space after the field colon is stripped.
fn strip_one_leading_space(value: &str) -> &str {
    value.strip_prefix(' ').unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_a_simple_event() {
        let mut f = SseFramer::new();
        let events = f.push(b"event: message_start\ndata: {\"a\":1}\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("message_start"));
        assert_eq!(events[0].data, "{\"a\":1}");
    }

    #[test]
    fn buffers_across_chunk_boundaries() {
        let mut f = SseFramer::new();
        // Split the payload mid-line and mid-field.
        assert!(f.push(b"event: mess").is_empty());
        assert!(f.push(b"age_start\ndata: {\"a\"").is_empty());
        let events = f.push(b":1}\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("message_start"));
        assert_eq!(events[0].data, "{\"a\":1}");
    }

    #[test]
    fn joins_multiple_data_lines_and_ignores_comments() {
        let mut f = SseFramer::new();
        let events = f.push(b": keep-alive\ndata: line1\ndata: line2\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "line1\nline2");
    }

    #[test]
    fn finish_flushes_event_without_trailing_blank_line() {
        let mut f = SseFramer::new();
        assert!(f.push(b"data: [DONE]\n").is_empty());
        let ev = f.finish().expect("pending event flushed on finish");
        assert_eq!(ev.data, "[DONE]");
    }

    #[test]
    fn handles_crlf_line_endings() {
        let mut f = SseFramer::new();
        let events = f.push(b"data: x\r\n\r\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "x");
    }
}
