//! REQ-611 — what a transcript record *is*, and what one line of the file looks
//! like (architecture ADR-2, BR-12, BR-14).
//!
//! # These are daemon types, not protocol events (ADR-2)
//!
//! `transcript_opened`, `prompt_submitted`, `tool_call_input`, `tool_result`,
//! `permission_decided`, `transcript_gap`, `transcript_resumed` and
//! `transcript_closed` are defined **here** and serialized straight to the file.
//! None of them is an [`teton_protocol::events::Event`] and none has a wire
//! name. BR-4 forbids widening the bus to carry a transcript's content: the
//! bus fans out to every attached client and every declared monitor, so a
//! `tool_result` variant on `Event` would change *who learns a session's
//! content*, which is the one thing this REQ must not do. The mapper's own
//! first draft put these in `events.rs`; ADR-2 exists to name that mistake.
//!
//! The one thing that *is* an event — `transcript_state` — carries `enabled`
//! and `reason` and no path, and lives in `teton-protocol` where it belongs.
//!
//! # The line, and why its four fields are reserved (BR-14)
//!
//! Every line is one JSON object with `n`, `ts`, `session_id` and `kind`, so a
//! reader with `serde_json` and no teton code can say what a line is, which
//! session it belongs to and where it sits in the file. [`Line`] therefore owns
//! those four names: a record body key that collides with one of them is
//! dropped rather than allowed to emit the key twice (no record kind or event
//! has such a field today — the guard is against the one somebody adds).
//!
//! `seq` is present only on bus-sourced records and is **expected to skip**:
//! it is minted daemon-wide, so other sessions' events interleave in the
//! numbering. `n` — per file, contiguous from 1 — is the contiguity guarantee
//! (LESSON-503, and the spec's Assumptions say so in as many words).

use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Map, Value};
use teton_protocol::events::{EventEnvelope, ToolCallStatus};
use teton_protocol::methods::{PromptBlock, SkillInvocation};
use teton_protocol::{RequestId, SessionId, TurnId};

/// The line-level keys [`Line`] owns (BR-14).
///
/// A body may not emit any of these: `session_id` and `seq` are lifted out of a
/// bus envelope onto the line, `event` is re-spelled as `kind`, and the rest are
/// the line's own vocabulary. Stripping is the fail-safe direction — a duplicate
/// key makes the line ambiguous to exactly the stock parser BR-14 promises.
const RESERVED_KEYS: &[&str] = &[
    "n",
    "ts",
    "session_id",
    "seq",
    "kind",
    "event",
    "truncated",
    "original_bytes",
];

/// Why a session's transcript stopped (REQ-611 System Model → Events,
/// `transcript_closed`).
///
/// A closed enum with no catch-all, for
/// [`teton_protocol::events::TranscriptStateReason`]'s reason: "the session
/// ended", "the user typed `/transcript off`", "the daemon went down" and "the
/// write failed" are four different stories about the same missing tail, and a
/// reader who cannot tell them apart cannot tell a clean end from a truncation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    /// The session was removed from the registry.
    SessionEnded,
    /// The user typed `/transcript off` for this session.
    SessionCommand,
    /// The daemon is shutting down (AC-18).
    DaemonShutdown,
    /// A write failed and the sink stopped for this session (BR-6, ADR-8).
    WriteFailure,
}

/// The `transcript_opened` payload (REQ-611 System Model → Events).
///
/// Six facts, and each is here because a reader coming back to the file weeks
/// later cannot recover it from anywhere else: which build wrote this, what
/// ground the session stood on, whether the *egress* side was redacting while
/// this file was being written unredacted (BR-11), what budget the truncation
/// markers in this file were measured against (BR-12), and where in the
/// daemon-wide bus numbering the file begins.
///
/// The session id is deliberately **not** a field: it is on every
/// [`Line`] already, and a second spelling is a second thing to keep true.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Opened {
    /// The daemon version that opened the file.
    pub daemon_version: String,
    /// The session root, in its display form.
    pub root: String,
    /// The `[privacy] redact` posture in force at open (BR-11).
    ///
    /// The scan gates *egress*, never a transcript write, so this records what
    /// the other side of the machine was doing rather than what happened to
    /// these bytes.
    pub redact: bool,
    /// The `[transcript] max_record_bytes` every truncation in this file was
    /// measured against (BR-12).
    pub max_record_bytes: usize,
    /// The bus sequence number at open, so a reader knows where the record
    /// begins in the daemon-wide numbering.
    pub seq_at_open: u64,
}

/// The `prompt_submitted` payload: a `session/prompt` this session accepted.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PromptSubmitted {
    /// The turn the prompt opened.
    pub turn_id: TurnId,
    /// The prompt blocks **as received** — the bus never carries these (BR-4),
    /// which is why they reach the sink in-process.
    pub prompt: Vec<PromptBlock>,
    /// The skill invocation this turn expanded, when the line was a `/name`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill: Option<SkillInvocation>,
}

/// The `tool_call_input` payload: what the harness dispatched.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolCallInput {
    /// Correlates with the matching [`ToolResult`].
    pub tool_call_id: String,
    /// The tool's name.
    pub tool: String,
    /// The input **as the harness parsed it** — a `session_update` carries a
    /// tool call's id, title and status and never its input, so this is not
    /// recoverable from the bus records beside it.
    pub input: Value,
}

/// The `tool_result` payload: what a tool handed back.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolResult {
    /// Correlates with the matching [`ToolCallInput`].
    pub tool_call_id: String,
    /// How the call ended.
    pub status: ToolCallStatus,
    /// The result text as the harness received it, subject to
    /// `max_record_bytes` (BR-12). This is the field a 1 MiB `read` truncates.
    pub output: String,
}

/// The `permission_decided` payload: how a `permission/respond` resolved.
///
/// The option id and the remembered flag, and **not** the remembered-grant key
/// (the spec's OQ-4 draft answer): the key is more informative than a reader
/// needs and naming it in a file the user may share turns a decision record
/// into a map of what the session is permitted to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PermissionDecided {
    /// The request this answered.
    pub request_id: RequestId,
    /// The option the user chose.
    pub option_id: String,
    /// Whether the answer was remembered for the session.
    pub remembered: bool,
}

/// One thing worth writing to a transcript.
///
/// The sink-local kinds (ADR-2) plus [`Record::BusEnvelope`], which carries a
/// published event's **wire form unchanged** — the same bytes a fully attached
/// client would have seen, which is what makes the file the session's stream
/// rather than a second rendering of it.
#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    /// The file was created for this session.
    Opened(Opened),
    /// `/transcript on` after an `off`, in the same session and the same file
    /// (AC-4).
    Resumed {
        /// The bus sequence number at resume.
        seq_at_resume: u64,
    },
    /// The sink fell behind and dropped records (BR-5). Written **before** the
    /// next record, so `n` never has a hole — the count is the honesty.
    Gap {
        /// How many records were dropped.
        dropped: u64,
        /// The last bus `seq` written before the hole, where one is known.
        seq_before: Option<u64>,
        /// The bus `seq` of the record that follows the hole, where it has one.
        seq_after: Option<u64>,
    },
    /// The file stopped taking records, and why.
    Closed {
        /// Why it stopped.
        reason: CloseReason,
        /// The final `n` — this record's own, so a reader can check the file is
        /// whole by comparing it with the last line they parsed.
        records: u64,
    },
    /// A prompt was accepted.
    PromptSubmitted(PromptSubmitted),
    /// A tool call was dispatched.
    ToolCallInput(ToolCallInput),
    /// A tool call returned.
    ToolResult(ToolResult),
    /// A permission request was answered.
    PermissionDecided(PermissionDecided),
    /// A session-scoped bus envelope, recorded verbatim.
    BusEnvelope(EventEnvelope),
}

impl Record {
    /// The `kind` this record writes — a sink-local name, or the envelope's
    /// wire `event` name for [`Record::BusEnvelope`].
    #[must_use]
    pub fn kind(&self) -> &str {
        match self {
            Record::Opened(_) => "transcript_opened",
            Record::Resumed { .. } => "transcript_resumed",
            Record::Gap { .. } => "transcript_gap",
            Record::Closed { .. } => "transcript_closed",
            Record::PromptSubmitted(_) => "prompt_submitted",
            Record::ToolCallInput(_) => "tool_call_input",
            Record::ToolResult(_) => "tool_result",
            Record::PermissionDecided(_) => "permission_decided",
            Record::BusEnvelope(envelope) => envelope.event_name(),
        }
    }

    /// The daemon-wide bus sequence number, for the records that have one.
    ///
    /// `None` for every sink-local kind: they never travelled on the bus, and a
    /// fabricated `seq` would be a claim about ordering the sink cannot make.
    #[must_use]
    pub fn seq(&self) -> Option<u64> {
        match self {
            Record::BusEnvelope(envelope) => Some(envelope.seq),
            _ => None,
        }
    }

    /// The session this record belongs to, for the records that name one.
    ///
    /// Only a bus envelope does: it arrives already scoped, and BR-7 makes that
    /// scope load-bearing — a file holds exactly one session's records, so the
    /// writer refuses an envelope whose scope is another session's or absent.
    #[must_use]
    pub fn envelope_session(&self) -> Option<&SessionId> {
        match self {
            Record::BusEnvelope(envelope) => envelope.session_id.as_ref(),
            _ => None,
        }
    }

    /// The record's payload as a JSON object, ready to flatten onto a [`Line`].
    ///
    /// A bus envelope is serialized in its wire form and then relieved of
    /// `session_id`, `seq` and `event`, which the line carries as `session_id`,
    /// `seq` and `kind`. Everything else the envelope had survives byte for
    /// byte.
    fn body(&self) -> Map<String, Value> {
        let value = match self {
            Record::Opened(payload) => serde_json::to_value(payload),
            Record::Resumed { seq_at_resume } => {
                Ok(serde_json::json!({ "seq_at_resume": seq_at_resume }))
            }
            Record::Gap {
                dropped,
                seq_before,
                seq_after,
            } => {
                let mut map = Map::new();
                map.insert("dropped".to_owned(), Value::from(*dropped));
                if let Some(seq) = seq_before {
                    map.insert("seq_before".to_owned(), Value::from(*seq));
                }
                if let Some(seq) = seq_after {
                    map.insert("seq_after".to_owned(), Value::from(*seq));
                }
                Ok(Value::Object(map))
            }
            Record::Closed { reason, records } => Ok(serde_json::json!({
                "reason": reason,
                "records": records,
            })),
            Record::PromptSubmitted(payload) => serde_json::to_value(payload),
            Record::ToolCallInput(payload) => serde_json::to_value(payload),
            Record::ToolResult(payload) => serde_json::to_value(payload),
            Record::PermissionDecided(payload) => serde_json::to_value(payload),
            Record::BusEnvelope(envelope) => serde_json::to_value(envelope),
        };
        let mut body = match value {
            Ok(Value::Object(map)) => map,
            // Neither arm is reachable for the types above — every payload is a
            // struct or an object literal, and none holds a non-string map key
            // or a non-finite float. Degrading to an empty body rather than
            // panicking is BR-6's posture applied one level down: a record the
            // sink cannot render must not take the daemon with it.
            _ => Map::new(),
        };
        for key in RESERVED_KEYS {
            body.remove(*key);
        }
        body
    }
}

/// One line of the transcript file (BR-14).
///
/// Serialized with `serde_json` and written with a trailing newline. The four
/// required fields come first in the struct; the record's own payload is
/// flattened in beside them.
#[derive(Debug, Clone, Serialize)]
pub struct Line {
    /// Per-file counter, contiguous from 1 (BR-14). **Not** the bus `seq`.
    pub n: u64,
    /// When the sink wrote the record, RFC 3339 UTC with milliseconds.
    pub ts: String,
    /// The session the file belongs to; identical on every line of one file.
    pub session_id: SessionId,
    /// The daemon-wide bus sequence number, on bus-sourced records only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// What this line is.
    pub kind: String,
    /// The record's payload.
    #[serde(flatten)]
    pub body: Map<String, Value>,
    /// Present and `true` only when a content field was cut (BR-12).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    /// How many bytes the cut field(s) held before the cut (BR-12).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_bytes: Option<u64>,
}

impl Line {
    /// Render `record` as the line the writer will append.
    ///
    /// Truncation happens **here**, one level below every call site, because
    /// BR-12 is a rule about the file rather than about any one producer: a
    /// `tool_result` and a 1 MiB `agent_message_chunk` envelope are cut by the
    /// same code with the same marker, and a future producer inherits the rule
    /// rather than remembering it (architecture: *do the truncation in the
    /// writer task, not at the call site, so the rule has one home*).
    #[must_use]
    pub fn render(
        record: &Record,
        session_id: &SessionId,
        n: u64,
        at: SystemTime,
        max_record_bytes: usize,
    ) -> Self {
        let mut body = record.body();
        let cut = truncate_content(&mut body, max_record_bytes);
        Self {
            n,
            ts: rfc3339_utc(at),
            session_id: session_id.clone(),
            seq: record.seq(),
            kind: record.kind().to_owned(),
            body,
            truncated: cut.map(|_| true),
            original_bytes: cut,
        }
    }
}

/// Cut every string in `body` to `budget` bytes, returning the original size of
/// what was cut (BR-12).
///
/// **Marked, never silent** (LESSON-447): a caller that gets `Some(n)` back
/// writes `truncated: true` and `original_bytes: n`, and the same two fields
/// appear whether one byte or a mebibyte was cut. A field of *exactly* `budget`
/// bytes is not cut and carries no marker — the comparison is `>`, and AC-15
/// pins both sides of it.
///
/// Every string value is a content field, at any depth: a prompt's text sits
/// under `prompt[].text` and a streamed chunk's under `update.text`, so a
/// top-level-only rule would leave the two largest fields in the file uncut.
/// Recursion is bounded in practice by `serde_json`'s own 128-deep parse limit,
/// which every [`Value`] here has already passed through.
///
/// `original_bytes` is the **sum** over the fields that were cut. One field is
/// the ordinary case (a tool result, a chunk of model text) and then the sum is
/// that field's length, which is what AC-15 states.
fn truncate_content(body: &mut Map<String, Value>, budget: usize) -> Option<u64> {
    let mut original = 0u64;
    for value in body.values_mut() {
        original = original.saturating_add(truncate_value(value, budget));
    }
    (original > 0).then_some(original)
}

/// [`truncate_content`]'s recursion: returns the original length of everything
/// it cut beneath `value`, or `0` if it cut nothing.
fn truncate_value(value: &mut Value, budget: usize) -> u64 {
    match value {
        Value::String(text) => {
            if text.len() <= budget {
                return 0;
            }
            let original = text.len() as u64;
            text.truncate(floor_char_boundary(text, budget));
            original
        }
        Value::Array(items) => items.iter_mut().fold(0, |acc, item| {
            acc.saturating_add(truncate_value(item, budget))
        }),
        Value::Object(map) => map.values_mut().fold(0, |acc, item| {
            acc.saturating_add(truncate_value(item, budget))
        }),
        _ => 0,
    }
}

/// The largest index `<= budget` that is a UTF-8 character boundary of `text`.
///
/// Hand-rolled because `str::floor_char_boundary` is still unstable. Cutting
/// *below* the budget rather than at it is the only safe direction: a cut
/// through a multi-byte character would produce a string Rust cannot hold and
/// JSON no reader can trust, so a record with a multi-byte character straddling
/// the boundary is a byte or three under `max_record_bytes` rather than over.
fn floor_char_boundary(text: &str, budget: usize) -> usize {
    let mut index = budget.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// `at` as an RFC 3339 UTC timestamp with milliseconds, e.g.
/// `2026-09-03T10:11:12.345Z` (BR-14, the `ts` field).
///
/// Hand-rolled rather than pulled from a date crate: the REQ's External
/// Dependencies say "none", and this is the whole of what a transcript needs
/// from a calendar. A clock before the epoch renders as the epoch — a
/// nonsensical clock must not produce a nonsensical *file*, and every record's
/// `n` still orders the file regardless.
#[must_use]
pub fn rfc3339_utc(at: SystemTime) -> String {
    let since_epoch = at.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since_epoch.as_secs();
    let millis = since_epoch.subsec_millis();
    let (year, month, day, hour, minute, second) = civil_utc(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// `at` as the compact UTC stamp a transcript file name opens with, e.g.
/// `20260903T101112Z` — the `\d{8}T\d{6}Z` half of the pattern
/// [`super::retention::prune`] matches (BR-13).
#[must_use]
pub fn file_stamp_utc(at: SystemTime) -> String {
    let secs = at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let (year, month, day, hour, minute, second) = civil_utc(secs);
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}

/// Break Unix seconds into a UTC civil date and time.
///
/// Howard Hinnant's `civil_from_days`, which is exact for every date in the
/// proleptic Gregorian calendar and needs no table. UTC has no leap seconds in
/// this representation (Unix time elides them), so the day is always 86400
/// seconds long here.
fn civil_utc(secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let days = secs / 86_400;
    let time_of_day = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    (
        year,
        month,
        day,
        time_of_day / 3_600,
        (time_of_day / 60) % 60,
        time_of_day % 60,
    )
}

/// Days since 1970-01-01 to a civil `(year, month, day)`.
///
/// Hinnant's era-based algorithm. Only non-negative day counts occur here —
/// [`rfc3339_utc`] clamps a pre-epoch clock to the epoch before calling — so
/// the era arithmetic stays in unsigned range.
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    // Shift the epoch to 0000-03-01, which puts the leap day at the end of the
    // year and makes the month arithmetic below a single linear formula.
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use teton_protocol::events::{Event, SessionUpdate, SessionUpdatePayload};

    /// A session id shaped like one [`crate::sessions`] mints.
    fn session() -> SessionId {
        SessionId::from("sess-0123456789abcdefghjkmnpqrs")
    }

    /// `at` as a [`SystemTime`], seconds since the epoch.
    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + std::time::Duration::from_secs(secs)
    }

    /// AC-15 / BR-12 — a field one byte over the budget is cut and marked, and a
    /// field of *exactly* the budget is neither.
    ///
    /// Both halves on one fixture, because the defect BR-12 guards against is a
    /// boundary that moved: an off-by-one either marks an untouched record
    /// (telling a reader bytes are missing when none are) or cuts one silently.
    /// The third leg pins LESSON-447's "same words whether a byte or a mebibyte
    /// was cut": the marker fields do not change shape with the size of the cut.
    ///
    /// **Shown to fail** (mutation, restored): relaxing
    /// [`truncate_value`]'s `text.len() <= budget` to `text.len() < budget`
    /// makes the exact-size leg red —
    /// `exactly max_record_bytes must carry no truncation marker`.
    #[test]
    fn truncation_is_marked_and_exact_size_is_not() {
        const BUDGET: usize = 1024;

        let exact = Record::ToolResult(ToolResult {
            tool_call_id: "call-1".to_owned(),
            status: ToolCallStatus::Completed,
            output: "x".repeat(BUDGET),
        });
        let line = Line::render(&exact, &session(), 1, at(0), BUDGET);
        assert_eq!(
            line.truncated, None,
            "exactly max_record_bytes must carry no truncation marker"
        );
        assert_eq!(line.original_bytes, None);
        assert_eq!(
            line.body["output"].as_str().map(str::len),
            Some(BUDGET),
            "an exactly-budget field must reach the file whole"
        );

        let over = Record::ToolResult(ToolResult {
            tool_call_id: "call-1".to_owned(),
            status: ToolCallStatus::Completed,
            output: "x".repeat(BUDGET + 1),
        });
        let line = Line::render(&over, &session(), 2, at(0), BUDGET);
        assert_eq!(line.truncated, Some(true), "one byte over must be marked");
        assert_eq!(
            line.original_bytes,
            Some(BUDGET as u64 + 1),
            "original_bytes states the size before the cut"
        );
        assert_eq!(
            line.body["output"].as_str().map(str::len),
            Some(BUDGET),
            "the field is cut to the budget, not to the budget minus a marker"
        );

        // AC-15's mebibyte: the same two fields, differing only in the number.
        let huge = Record::ToolResult(ToolResult {
            tool_call_id: "call-1".to_owned(),
            status: ToolCallStatus::Completed,
            output: "x".repeat(1_048_576),
        });
        let line = Line::render(&huge, &session(), 3, at(0), BUDGET);
        assert_eq!(line.truncated, Some(true));
        assert_eq!(line.original_bytes, Some(1_048_576));
        assert_eq!(line.body["output"].as_str().map(str::len), Some(BUDGET));
    }

    /// BR-12 — a nested content field is cut too, and a multi-byte character is
    /// never cut through.
    ///
    /// The two largest fields in a real transcript are both nested: a prompt's
    /// text under `prompt[].text` and a streamed chunk's under `update.text`. A
    /// top-level-only rule would pass the test above and leave those uncut.
    #[test]
    fn nested_and_multibyte_content_is_cut_safely() {
        // Fifteen, not sixteen: the characters below are four bytes each, so a
        // budget of 16 would fall on a boundary by luck and prove nothing. At 15
        // a naive `truncate` panics and only the boundary walk survives.
        const BUDGET: usize = 15;

        let record = Record::PromptSubmitted(PromptSubmitted {
            turn_id: TurnId::from("turn-1"),
            prompt: vec![PromptBlock::Text {
                text: "🎿".repeat(10),
            }],
            skill: None,
        });
        let line = Line::render(&record, &session(), 1, at(0), BUDGET);
        assert_eq!(line.truncated, Some(true));
        let text = line.body["prompt"][0]["text"]
            .as_str()
            .expect("prompt text survives as a string");
        assert!(
            text.len() <= BUDGET,
            "a cut field must not exceed the budget: {} bytes",
            text.len()
        );
        assert_eq!(
            text, "🎿🎿🎿",
            "the cut lands on a character boundary at or below the budget"
        );
    }

    /// ADR-2 / BR-7 — a bus envelope is recorded in its wire form under its own
    /// event name, with `session_id` and `seq` lifted onto the line.
    #[test]
    fn a_bus_envelope_records_its_wire_form_under_its_event_name() {
        let envelope = EventEnvelope::new(
            41,
            Some(session()),
            Event::SessionUpdate(SessionUpdate {
                update: SessionUpdatePayload::AgentMessageChunk {
                    text: "hello".to_owned(),
                },
            }),
        );
        let line = Line::render(&Record::BusEnvelope(envelope), &session(), 7, at(0), 4096);

        assert_eq!(line.kind, "session_update");
        assert_eq!(line.seq, Some(41));
        assert_eq!(line.n, 7);
        let rendered = serde_json::to_value(&line).expect("a line serializes");
        assert_eq!(rendered["update"]["kind"], "agent_message_chunk");
        assert_eq!(rendered["update"]["text"], "hello");
        assert_eq!(
            rendered.get("event"),
            None,
            "the envelope's `event` is re-spelled as the line's `kind`"
        );
        assert_eq!(
            rendered["session_id"], "sess-0123456789abcdefghjkmnpqrs",
            "the line owns session_id; the body's copy is stripped"
        );
    }

    /// BR-14 / TASK-367 — every `kind` a [`Record`] can write is described in
    /// `docs/transcript-format.md`.
    ///
    /// The enum is the source of truth and the document is what has to keep up,
    /// so the names are read off [`Record::kind`]'s own arms rather than
    /// re-typed here. `kind` matches without a wildcard, so a ninth sink-local
    /// variant cannot compile without an arm; the arm's literal is then picked
    /// up by the scan below and has to reach the document before this test is
    /// green again. A hand-written list would go stale in exactly the case the
    /// test exists for.
    ///
    /// The scan is bounded and floored, per the conventions' rules for a check
    /// that reads its own source: the corpus is cut at this file's
    /// `#[cfg(test)]` so the scanner cannot match its own patterns, it is then
    /// sliced to the one function body rather than to the rest of the file, and
    /// the eight sink-local kinds are a vacuity floor — a slice that stopped
    /// matching the function would otherwise pass forever, having found nothing
    /// to check.
    ///
    /// [`Record::BusEnvelope`] contributes no literal: its `kind` is the
    /// envelope's own wire name, which `teton-protocol` defines and the
    /// document describes as a class. The last leg pins the worked example the
    /// document uses for it.
    ///
    /// **Shown to fail** (mutation, restored): renaming every `transcript_gap`
    /// mention in the document makes it red — `` `transcript_gap` is written by
    /// the sink but not described in … ``. Renaming only the *row* in the kind
    /// table does not, because the prose above it names the kind too; the
    /// assertion is "the document says this word", which is the weakest claim
    /// worth making mechanically and the strongest one a substring can carry.
    /// Raising `FLOOR` to 9 shows the other arm red at "the scan found 8".
    #[test]
    fn every_record_kind_is_documented_in_the_format_doc() {
        /// The sink-local kinds that exist today; a floor, not a count.
        const FLOOR: usize = 8;

        let doc_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/transcript-format.md");
        let doc = std::fs::read_to_string(&doc_path).unwrap_or_else(|err| {
            panic!(
                "the format document is the contract this test checks and must be \
                 readable at {}: {err}",
                doc_path.display()
            )
        });

        let kinds = sink_local_kinds();
        assert!(
            kinds.len() >= FLOOR,
            "the scan found {} kind literal(s) in `Record::kind`, fewer than the \
             {FLOOR} sink-local kinds that exist — the slice has stopped matching \
             the function and is checking nothing",
            kinds.len()
        );
        for kind in &kinds {
            assert!(
                doc.contains(&format!("`{kind}`")),
                "`{kind}` is written by the sink but not described in {}",
                doc_path.display()
            );
        }

        // The envelope arm names no kind of its own — a bus record's `kind` is
        // its event's wire name — so the document carries that form as a class,
        // and this is the example it works through.
        let envelope = Record::BusEnvelope(EventEnvelope::new(
            1,
            Some(session()),
            Event::SessionUpdate(SessionUpdate {
                update: SessionUpdatePayload::AgentMessageChunk {
                    text: String::new(),
                },
            }),
        ));
        assert!(
            doc.contains(&format!("`{}`", envelope.kind())),
            "a bus record's `kind` is its event's wire name, and the document must \
             work the form through at least the `{}` example",
            envelope.kind()
        );
    }

    /// The `kind` literals [`Record::kind`] maps its sink-local variants to,
    /// read out of this file's own source.
    ///
    /// See `every_record_kind_is_documented_in_the_format_doc` for why the
    /// names are scanned rather than listed, and for the two bounds on the
    /// corpus.
    fn sink_local_kinds() -> Vec<String> {
        const MARKER: &str = "pub fn kind(&self) -> &str {";

        let source = include_str!("record.rs");
        let production = source
            .split_once("\n#[cfg(test)]")
            .map_or(source, |(before, _)| before);
        let start = production
            .find(MARKER)
            .expect("`Record::kind` is the source of truth this scan reads");
        let body = &production[start..];
        let end = body
            .find("\n    }\n")
            .expect("`Record::kind`'s body ends at a column-4 closing brace");

        body[..end]
            .split("=> \"")
            .skip(1)
            .filter_map(|arm| arm.split_once('"').map(|(kind, _)| kind.to_owned()))
            .collect()
    }

    /// BR-14 — the timestamp is RFC 3339 UTC, and the file stamp is the
    /// `\d{8}T\d{6}Z` form [`super::super::retention`] matches.
    ///
    /// Fixed instants rather than `now()`: an oracle that recomputes the answer
    /// with the code under test proves nothing (LESSON-569), so the expected
    /// strings are written out by hand. 1 234 567 890 is 2009-02-13T23:31:30Z,
    /// a value published in enough places to check independently; the second is
    /// a leap day, which a naive month table gets wrong.
    #[test]
    fn utc_timestamps_render_as_rfc3339_and_as_a_file_stamp() {
        assert_eq!(rfc3339_utc(at(1_234_567_890)), "2009-02-13T23:31:30.000Z");
        assert_eq!(file_stamp_utc(at(1_234_567_890)), "20090213T233130Z");
        // 2024-02-29T12:00:00Z — a leap day in a leap century-of-four.
        assert_eq!(rfc3339_utc(at(1_709_208_000)), "2024-02-29T12:00:00.000Z");
        assert_eq!(file_stamp_utc(at(1_709_208_000)), "20240229T120000Z");
        assert_eq!(rfc3339_utc(at(0)), "1970-01-01T00:00:00.000Z");
        // 2000-02-29 — the century that *is* a leap year, which the 100/400
        // rules disagree about.
        assert_eq!(file_stamp_utc(at(951_782_400)), "20000229T000000Z");
    }
}
