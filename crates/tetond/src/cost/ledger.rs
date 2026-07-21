//! The append-only cost ledger (BR-2) and its egress metering seam.
//!
//! A daemon-local SQLite file (bundled SQLite — no system dependency) holding
//! one row per completed remote call. The store is **append-only**: the schema
//! installs triggers that abort any `UPDATE` or `DELETE`, so the billing history
//! is immutable by construction, and the only write path is [`CostLedger::record`].
//!
//! ## Privacy (BR-7)
//!
//! Every column is a token count or a piece of routing metadata — session id,
//! phase, provider id, model name, input/output token counts, computed cost.
//! There is deliberately **no column** that could carry prompt text, tool
//! arguments, or a credential. A ledger row is safe to read, export, or ship in
//! a report.
//!
//! ## Streamed-usage recording
//!
//! [`CostLedger`] implements [`CostMeter`], the seam the egress choke point calls
//! at the allowed-forward point. `meter_response` wraps the streaming body in a
//! [`MeteredBody`] that passes every chunk through untouched (so the adapter
//! still parses the real response) while a [`UsageScan`] reads the turn's token
//! usage out of the provider's own SSE payload. When the stream ends, the call
//! is priced and one row is written — exactly one `CostRecord` per completed
//! remote call, and none for a blocked one (a blocked call never reaches egress'
//! forward point).

use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{SystemTime, UNIX_EPOCH};

use futures::Stream;
use rusqlite::Connection;

use teton_protocol::events::CostRecord;
use teton_protocol::{Phase, ProviderId, SessionId};
use teton_providers::transport::{ByteStream, TransportError, TransportResponse};

use super::prices::PriceTable;
use super::{CostAttribution, CostEventSink, CostMeter};

/// The append-only schema. `IF NOT EXISTS` everywhere so opening an existing
/// ledger is idempotent; the two triggers enforce append-only at the storage
/// layer, not merely by API discipline.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS cost_records (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    recorded_at_ms INTEGER NOT NULL,
    session_id     TEXT    NOT NULL,
    phase          TEXT,
    provider_id    TEXT    NOT NULL,
    model          TEXT    NOT NULL,
    input_tokens   INTEGER NOT NULL,
    output_tokens  INTEGER NOT NULL,
    usd_micros     INTEGER
);
CREATE TRIGGER IF NOT EXISTS cost_records_no_update
    BEFORE UPDATE ON cost_records
    BEGIN SELECT RAISE(ABORT, 'cost ledger is append-only'); END;
CREATE TRIGGER IF NOT EXISTS cost_records_no_delete
    BEFORE DELETE ON cost_records
    BEGIN SELECT RAISE(ABORT, 'cost ledger is append-only'); END;
";

/// How many trailing bytes of one chunk the usage scanner carries into the next,
/// so a usage key or its number split across a chunk boundary is still matched.
/// Comfortably larger than the longest key plus a token count.
const CARRY_BYTES: usize = 64;

/// A failure interacting with the ledger store.
///
/// Content-free by construction: the [`Display`](std::fmt::Display) form is a
/// fixed string, never the underlying SQL or any row data, so it is safe to log
/// (BR-7 / conventions: no content in logs). The source error is retained in the
/// chain for local debugging only.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    /// The SQLite store returned an error (open, schema, insert, or query).
    #[error("cost ledger store error")]
    Sqlite(#[from] rusqlite::Error),
    /// The ledger mutex was poisoned by a panic in another holder.
    #[error("cost ledger mutex poisoned")]
    Poisoned,
}

/// One row of the cost ledger — token counts and metadata only (BR-7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerRow {
    /// Session that incurred the call.
    pub session_id: String,
    /// Lifecycle phase at call time; `None` for a freeform call.
    pub phase: Option<Phase>,
    /// Provider that served the call.
    pub provider_id: String,
    /// Concrete model billed.
    pub model: String,
    /// Prompt / input tokens.
    pub input_tokens: u64,
    /// Completion / output tokens.
    pub output_tokens: u64,
    /// Computed cost in integer micro-USD, or `None` when the model is
    /// **unpriced** (BR-2: never guessed). The report's source of truth for the
    /// priced/unpriced split.
    pub usd_micros: Option<i64>,
}

impl LedgerRow {
    /// Project to the wire [`CostRecord`] emitted as `cost_recorded`.
    ///
    /// An unpriced row (`usd_micros == None`) projects its cost to `0` because
    /// the wire field is a non-optional integer. This is a lossy live-event
    /// detail only: the authoritative unpriced accounting lives in the ledger,
    /// and the meter (BR-2) derives from the stored rows via [`super::report`],
    /// not from the event stream.
    fn to_wire(&self) -> CostRecord {
        CostRecord {
            session_id: SessionId::from(self.session_id.clone()),
            phase: self.phase,
            provider_id: ProviderId::from(self.provider_id.clone()),
            model: self.model.clone(),
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            usd_micros: self.usd_micros.unwrap_or(0),
        }
    }
}

/// The append-only cost ledger: a bundled-SQLite store plus the price table used
/// to cost each call and the sink that broadcasts `cost_recorded`.
#[derive(Clone)]
pub struct CostLedger {
    conn: Arc<Mutex<Connection>>,
    prices: Arc<PriceTable>,
    sink: Arc<dyn CostEventSink>,
}

impl CostLedger {
    /// Open (creating if absent) the ledger at `path`, installing the schema.
    ///
    /// # Errors
    /// [`LedgerError::Sqlite`] if the file cannot be opened or the schema cannot
    /// be applied.
    pub fn open(
        path: impl AsRef<Path>,
        prices: PriceTable,
        sink: Arc<dyn CostEventSink>,
    ) -> Result<Self, LedgerError> {
        Self::from_connection(Connection::open(path)?, prices, sink)
    }

    /// Open an ephemeral in-memory ledger — for tests and for a daemon told not
    /// to persist.
    ///
    /// # Errors
    /// [`LedgerError::Sqlite`] if the in-memory database cannot be created.
    pub fn open_in_memory(
        prices: PriceTable,
        sink: Arc<dyn CostEventSink>,
    ) -> Result<Self, LedgerError> {
        Self::from_connection(Connection::open_in_memory()?, prices, sink)
    }

    fn from_connection(
        conn: Connection,
        prices: PriceTable,
        sink: Arc<dyn CostEventSink>,
    ) -> Result<Self, LedgerError> {
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            prices: Arc::new(prices),
            sink,
        })
    }

    /// The price table this ledger costs calls against (used by the report to
    /// reprice at the baseline frontier model).
    #[must_use]
    pub fn prices(&self) -> &PriceTable {
        &self.prices
    }

    /// Append one row and broadcast its `cost_recorded` event.
    ///
    /// # Errors
    /// [`LedgerError`] if the insert fails or the mutex is poisoned.
    pub fn record(&self, row: LedgerRow) -> Result<(), LedgerError> {
        insert_and_emit(&self.conn, self.sink.as_ref(), &row)
    }

    /// Price a call against the table and append it (the priced convenience over
    /// [`CostLedger::record`]). An unknown model is recorded unpriced (BR-2).
    ///
    /// # Errors
    /// [`LedgerError`] if the insert fails or the mutex is poisoned.
    pub fn record_call(
        &self,
        session_id: impl Into<String>,
        provider_id: impl Into<String>,
        attribution: &CostAttribution,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Result<(), LedgerError> {
        let provider_id = provider_id.into();
        let usd_micros = self.prices.price(
            &provider_id,
            &attribution.model,
            input_tokens,
            output_tokens,
        );
        self.record(LedgerRow {
            session_id: session_id.into(),
            phase: attribution.phase,
            provider_id,
            model: attribution.model.clone(),
            input_tokens,
            output_tokens,
            usd_micros,
        })
    }

    /// Every recorded row, in insertion order.
    ///
    /// # Errors
    /// [`LedgerError`] if the query fails or the mutex is poisoned.
    pub fn all_records(&self) -> Result<Vec<LedgerRow>, LedgerError> {
        let guard = self.conn.lock().map_err(|_| LedgerError::Poisoned)?;
        let mut stmt = guard.prepare(
            "SELECT session_id, phase, provider_id, model, input_tokens, output_tokens, usd_micros
             FROM cost_records ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let phase: Option<String> = r.get(1)?;
                Ok(LedgerRow {
                    session_id: r.get(0)?,
                    phase: phase.as_deref().and_then(phase_from_wire),
                    provider_id: r.get(2)?,
                    model: r.get(3)?,
                    input_tokens: to_u64(r.get::<_, i64>(4)?),
                    output_tokens: to_u64(r.get::<_, i64>(5)?),
                    usd_micros: r.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Aggregate the whole ledger into an AC-4 [`CostReport`](super::CostReport).
    ///
    /// # Errors
    /// [`LedgerError`] if reading the rows fails.
    pub fn report(&self) -> Result<super::CostReport, LedgerError> {
        Ok(super::report::aggregate(&self.all_records()?, &self.prices))
    }
}

impl CostMeter for CostLedger {
    fn meter_response(
        &self,
        response: TransportResponse,
        session_id: Option<SessionId>,
        provider_id: ProviderId,
        attribution: CostAttribution,
    ) -> TransportResponse {
        // A call with no session scope cannot be attributed to a CostRecord;
        // forward it untouched rather than record an orphan row.
        let Some(session_id) = session_id else {
            return response;
        };
        let metered = MeteredBody {
            inner: response.body,
            conn: Arc::clone(&self.conn),
            prices: Arc::clone(&self.prices),
            sink: Arc::clone(&self.sink),
            session_id,
            provider_id,
            attribution,
            scan: UsageScan::default(),
            recorded: false,
        };
        TransportResponse {
            status: response.status,
            body: Box::pin(metered),
        }
    }
}

/// A response body that records a `CostRecord` when the stream completes.
///
/// Every chunk is yielded to the caller unchanged; a copy feeds the
/// [`UsageScan`]. Recording happens exactly once, on the terminal `None`, so a
/// caller that drains the stream (to read the completion) always bills the call
/// once. Recording is best-effort: a store failure is swallowed so it can never
/// corrupt delivery of the actual model response.
struct MeteredBody {
    inner: ByteStream,
    conn: Arc<Mutex<Connection>>,
    prices: Arc<PriceTable>,
    sink: Arc<dyn CostEventSink>,
    session_id: SessionId,
    provider_id: ProviderId,
    attribution: CostAttribution,
    scan: UsageScan,
    recorded: bool,
}

impl MeteredBody {
    fn finalize(&self) {
        let usage = self.scan.usage();
        let usd_micros = self.prices.price(
            &self.provider_id.0,
            &self.attribution.model,
            usage.input,
            usage.output,
        );
        let row = LedgerRow {
            session_id: self.session_id.0.clone(),
            phase: self.attribution.phase,
            provider_id: self.provider_id.0.clone(),
            model: self.attribution.model.clone(),
            input_tokens: usage.input,
            output_tokens: usage.output,
            usd_micros,
        };
        // Best-effort: never let a ledger hiccup break the response stream.
        let _ = insert_and_emit(&self.conn, self.sink.as_ref(), &row);
    }
}

impl Stream for MeteredBody {
    type Item = Result<Vec<u8>, TransportError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // MeteredBody is Unpin (the inner stream is already `Pin<Box<..>>`), so a
        // plain `get_mut` projection is sound.
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                this.scan.feed(&chunk);
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
            Poll::Ready(None) => {
                if !this.recorded {
                    this.recorded = true;
                    this.finalize();
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Token usage extracted from a streamed response.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Usage {
    input: u64,
    output: u64,
}

/// An incremental, provider-agnostic usage extractor.
///
/// Both supported provider families report usage as JSON inside their SSE
/// stream: Anthropic (`"input_tokens"` / `"output_tokens"`) and OpenAI-compatible
/// (`"prompt_tokens"` / `"completion_tokens"`). The scanner reads those integers
/// out of the raw bytes as they flow, keeping only a small carry buffer so its
/// memory is O(1) in the response size, and takes the last value seen for each
/// key — which is the final tally for both families (Anthropic's terminal
/// `message_delta` and OpenAI's terminal usage chunk).
///
/// This is a deliberately simple MVP: it recognizes the two families this build
/// ships and yields `0` for a stream that carries no usage. A future adapter can
/// hand egress a precise per-turn usage value instead of relying on this scan.
#[derive(Debug, Default)]
struct UsageScan {
    carry: Vec<u8>,
    input: Option<u64>,
    output: Option<u64>,
}

/// Quoted usage keys that denote input (prompt) tokens.
const INPUT_KEYS: [&[u8]; 2] = [b"\"input_tokens\"", b"\"prompt_tokens\""];
/// Quoted usage keys that denote output (completion) tokens.
const OUTPUT_KEYS: [&[u8]; 2] = [b"\"output_tokens\"", b"\"completion_tokens\""];

impl UsageScan {
    fn feed(&mut self, chunk: &[u8]) {
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(chunk);
        for key in INPUT_KEYS {
            if let Some(v) = last_int_after(&buf, key) {
                self.input = Some(v);
            }
        }
        for key in OUTPUT_KEYS {
            if let Some(v) = last_int_after(&buf, key) {
                self.output = Some(v);
            }
        }
        let keep = buf.len().min(CARRY_BYTES);
        self.carry = buf.split_off(buf.len() - keep);
    }

    fn usage(&self) -> Usage {
        Usage {
            input: self.input.unwrap_or(0),
            output: self.output.unwrap_or(0),
        }
    }
}

/// The integer immediately following the *last* occurrence of `key` in
/// `haystack` (skipping a `:` and whitespace), or `None` if the key is absent or
/// not followed by a number.
fn last_int_after(haystack: &[u8], key: &[u8]) -> Option<u64> {
    let mut found = None;
    let mut from = 0;
    while let Some(rel) = find_sub(&haystack[from..], key) {
        let after = from + rel + key.len();
        if let Some(v) = parse_int_after(&haystack[after..]) {
            found = Some(v);
        }
        from = from + rel + 1;
    }
    found
}

/// Parse the leading integer of `bytes`, skipping an optional `:` and ASCII
/// whitespace first. Returns `None` if the first non-skipped byte is not a digit.
fn parse_int_after(bytes: &[u8]) -> Option<u64> {
    let mut i = 0;
    while i < bytes.len() && matches!(bytes[i], b':' | b' ' | b'\t' | b'\r' | b'\n') {
        i += 1;
    }
    let start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return None;
    }
    std::str::from_utf8(&bytes[start..i]).ok()?.parse().ok()
}

/// Index of the first occurrence of `needle` in `haystack` (naive scan; needles
/// are short constant keys).
fn find_sub(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
}

/// Insert one row inside the mutex, then broadcast its event *after* releasing
/// the lock (so a subscriber callback can never deadlock the store).
fn insert_and_emit(
    conn: &Mutex<Connection>,
    sink: &dyn CostEventSink,
    row: &LedgerRow,
) -> Result<(), LedgerError> {
    {
        let guard = conn.lock().map_err(|_| LedgerError::Poisoned)?;
        guard.execute(
            "INSERT INTO cost_records
               (recorded_at_ms, session_id, phase, provider_id, model,
                input_tokens, output_tokens, usd_micros)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                now_ms(),
                row.session_id,
                row.phase.map(phase_to_wire),
                row.provider_id,
                row.model,
                to_i64(row.input_tokens),
                to_i64(row.output_tokens),
                row.usd_micros,
            ],
        )?;
    }
    sink.cost_recorded(row.to_wire());
    Ok(())
}

/// Milliseconds since the Unix epoch (0 if the clock is before it).
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| to_i64(d.as_millis() as u64))
        .unwrap_or(0)
}

/// Saturating `u64 -> i64` for storage (token counts never approach the ceiling;
/// this only guards a corrupt/absurd value from wrapping negative).
fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Non-negative `i64 -> u64` for reads (stored counts are never negative).
fn to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

/// The snake_case wire form of a phase (matches `teton_protocol::Phase`'s serde).
fn phase_to_wire(phase: Phase) -> &'static str {
    match phase {
        Phase::Spec => "spec",
        Phase::Architect => "architect",
        Phase::Implement => "implement",
        Phase::Review => "review",
        Phase::Io => "io",
        Phase::Freeform => "freeform",
    }
}

/// Parse a phase back from its wire form; unknown strings become `None`.
fn phase_from_wire(s: &str) -> Option<Phase> {
    Some(match s {
        "spec" => Phase::Spec,
        "architect" => Phase::Architect,
        "implement" => Phase::Implement,
        "review" => Phase::Review,
        "io" => Phase::Io,
        "freeform" => Phase::Freeform,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use teton_protocol::events::CostRecord as WireRecord;

    /// A sink that captures every emitted `cost_recorded` record.
    #[derive(Default)]
    struct CapturingSink {
        records: Mutex<Vec<WireRecord>>,
    }

    impl CostEventSink for CapturingSink {
        fn cost_recorded(&self, record: WireRecord) {
            self.records.lock().unwrap().push(record);
        }
    }

    fn ledger() -> (CostLedger, Arc<CapturingSink>) {
        let sink = Arc::new(CapturingSink::default());
        let ledger = CostLedger::open_in_memory(PriceTable::bundled(), sink.clone())
            .expect("open in-memory ledger");
        (ledger, sink)
    }

    /// Build a byte stream from pre-split chunks (to exercise boundary handling).
    fn body_from(chunks: Vec<&str>) -> ByteStream {
        let owned: Vec<Result<Vec<u8>, TransportError>> = chunks
            .into_iter()
            .map(|c| Ok(c.as_bytes().to_vec()))
            .collect();
        Box::pin(futures::stream::iter(owned))
    }

    async fn drain(mut body: ByteStream) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some(chunk) = body.next().await {
            out.extend_from_slice(&chunk.expect("chunk ok"));
        }
        out
    }

    #[test]
    fn record_and_read_back_round_trips() {
        let (ledger, sink) = ledger();
        ledger
            .record_call(
                "sess-1",
                "anthropic",
                &CostAttribution::new("claude-opus-4").with_phase(Phase::Review),
                1000,
                500,
            )
            .expect("record");
        let rows = ledger.all_records().expect("read");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session_id, "sess-1");
        assert_eq!(rows[0].phase, Some(Phase::Review));
        assert_eq!(rows[0].provider_id, "anthropic");
        assert_eq!(rows[0].model, "claude-opus-4");
        assert_eq!(rows[0].input_tokens, 1000);
        assert_eq!(rows[0].output_tokens, 500);
        assert_eq!(rows[0].usd_micros, Some(15_000 + 37_500));
        // The event fired with the same attribution.
        let recorded = sink.records.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].session_id, SessionId::from("sess-1"));
        assert_eq!(recorded[0].phase, Some(Phase::Review));
        assert_eq!(recorded[0].usd_micros, 15_000 + 37_500);
    }

    #[test]
    fn unknown_model_is_recorded_unpriced_not_guessed() {
        let (ledger, sink) = ledger();
        ledger
            .record_call(
                "sess-1",
                "some-vllm",
                &CostAttribution::new("llama-3-70b"),
                2000,
                1000,
            )
            .expect("record");
        let rows = ledger.all_records().expect("read");
        assert_eq!(rows[0].usd_micros, None, "unknown model must be unpriced");
        // Token counts are still recorded (the report surfaces them as unpriced).
        assert_eq!(rows[0].input_tokens, 2000);
        assert_eq!(rows[0].output_tokens, 1000);
        // On the wire the non-optional cost projects to 0.
        assert_eq!(sink.records.lock().unwrap()[0].usd_micros, 0);
    }

    #[test]
    fn ledger_is_append_only() {
        let (ledger, _sink) = ledger();
        ledger
            .record_call(
                "s",
                "local",
                &CostAttribution::new("qwen2.5-coder-3b"),
                1,
                1,
            )
            .expect("record");
        let guard = ledger.conn.lock().unwrap();
        assert!(
            guard
                .execute("UPDATE cost_records SET model = 'x'", [])
                .is_err(),
            "UPDATE must be rejected by the append-only trigger"
        );
        assert!(
            guard.execute("DELETE FROM cost_records", []).is_err(),
            "DELETE must be rejected by the append-only trigger"
        );
    }

    #[tokio::test]
    async fn metering_a_stream_records_one_call_from_anthropic_usage() {
        let (ledger, sink) = ledger();
        let ledger = Arc::new(ledger);
        // Anthropic-shaped SSE: input in message_start, final output in
        // message_delta.
        let body = body_from(vec![
            "event: message_start\ndata: {\"message\":{\"usage\":{\"input_tokens\":1200,\"output_tokens\":1}}}\n\n",
            "event: message_delta\ndata: {\"usage\":{\"output_tokens\":340}}\n\n",
        ]);
        let response = TransportResponse { status: 200, body };
        let metered = CostMeter::meter_response(
            ledger.as_ref(),
            response,
            Some(SessionId::from("sess-9")),
            ProviderId::from("anthropic"),
            CostAttribution::new("claude-opus-4").with_phase(Phase::Implement),
        );
        let bytes = drain(metered.body).await;
        // Body passed through unchanged.
        assert!(bytes.windows(13).any(|w| w == b"message_start"));

        let rows = ledger.all_records().expect("read");
        assert_eq!(rows.len(), 1, "exactly one CostRecord per completed call");
        assert_eq!(rows[0].input_tokens, 1200);
        assert_eq!(rows[0].output_tokens, 340);
        assert_eq!(rows[0].phase, Some(Phase::Implement));
        assert_eq!(rows[0].session_id, "sess-9");
        assert_eq!(sink.records.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn metering_reads_openai_style_usage_split_across_chunks() {
        let (ledger, _sink) = ledger();
        let ledger = Arc::new(ledger);
        // The usage object is split so the number spans a chunk boundary.
        let body = body_from(vec![
            "data: {\"choices\":[]}\n\ndata: {\"usage\":{\"prompt_tokens\":80,\"completion_to",
            "kens\":4",
            "2}}\n\ndata: [DONE]\n\n",
        ]);
        let response = TransportResponse { status: 200, body };
        let metered = CostMeter::meter_response(
            ledger.as_ref(),
            response,
            Some(SessionId::from("s")),
            ProviderId::from("deepseek"),
            CostAttribution::new("deepseek-chat"),
        );
        drain(metered.body).await;
        let rows = ledger.all_records().expect("read");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].input_tokens, 80);
        assert_eq!(rows[0].output_tokens, 42);
    }

    #[tokio::test]
    async fn a_call_with_no_session_is_not_metered() {
        let (ledger, _sink) = ledger();
        let ledger = Arc::new(ledger);
        let body = body_from(vec![
            "data: {\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}",
        ]);
        let response = TransportResponse { status: 200, body };
        let metered = CostMeter::meter_response(
            ledger.as_ref(),
            response,
            None,
            ProviderId::from("anthropic"),
            CostAttribution::new("claude-opus-4"),
        );
        drain(metered.body).await;
        assert!(
            ledger.all_records().expect("read").is_empty(),
            "an unattributed (session-less) call must not be recorded"
        );
    }

    #[test]
    fn usage_scan_takes_the_last_value_for_each_key() {
        let mut scan = UsageScan::default();
        scan.feed(b"\"input_tokens\": 100, \"output_tokens\": 1");
        scan.feed(b", later \"output_tokens\": 250 final");
        let usage = scan.usage();
        assert_eq!(usage.input, 100);
        assert_eq!(usage.output, 250);
    }

    #[test]
    fn usage_scan_defaults_to_zero_without_usage() {
        let mut scan = UsageScan::default();
        scan.feed(b"event: ping\ndata: {}\n\n");
        assert_eq!(
            scan.usage(),
            Usage {
                input: 0,
                output: 0
            }
        );
    }
}
