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
//! is priced and one row is written — exactly one `CostRecord` per remote call
//! this daemon began to read, and none for a blocked one (a blocked call never
//! reaches egress' forward point) or one refused on its status before its body
//! was touched.
//!
//! "Ends" means completed **or dropped**: a stream abandoned mid-flight — a duty
//! cut off at its deadline, a cancelled turn — is a call that really went out,
//! and billing only the drained path left those off the ledger entirely. See
//! [`MeteredBody`] for where that line is drawn and what it does not fix.

use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{SystemTime, UNIX_EPOCH};

use futures::Stream;
use rusqlite::Connection;

use teton_protocol::events::CostRecord;
use teton_protocol::{Category, Phase, ProviderId, SessionId};
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
    category       TEXT,
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

/// Columns [`SCHEMA`] creates on a fresh ledger but an older build's file does
/// not have, with the DDL that adds each one.
///
/// `CREATE TABLE IF NOT EXISTS` is a no-op against an existing file, so a column
/// added to [`SCHEMA`] reaches a pre-existing `cost.db` only through here. Every
/// entry MUST be `ADD COLUMN`-shaped and nullable: the store is append-only, and
/// a migration that rewrote historical rows would both fail the no-update trigger
/// and invent an attribution nobody recorded. Rows written before a column
/// existed read back as `None`, which is the truth about them — they predate the
/// concept.
const ADDITIVE_COLUMNS: [(&str, &str); 1] = [(
    // REQ-558: the routing category the call was made for.
    "category",
    "ALTER TABLE cost_records ADD COLUMN category TEXT",
)];

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
    /// Lifecycle phase at call time; `None` for a freeform call. Retained
    /// alongside `category` for cost attribution (REQ-558 BR-11).
    pub phase: Option<Phase>,
    /// Routing category the call was made for (REQ-558); `None` for a call with
    /// no category attribution, and for every row a pre-REQ build wrote.
    pub category: Option<Category>,
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
            category: self.category,
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
        migrate(&conn)?;
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
        // REQ-557 ADR-A: priced by the model the provider DECLARED it calls, not
        // by the provider id. The provider is who served it; the model is what
        // costs money.
        let usd_micros = self
            .prices
            .price(&attribution.model, input_tokens, output_tokens);
        self.record(LedgerRow {
            session_id: session_id.into(),
            phase: attribution.phase,
            category: attribution.category,
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
            "SELECT session_id, phase, category, provider_id, model,
                    input_tokens, output_tokens, usd_micros
             FROM cost_records ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let phase: Option<String> = r.get(1)?;
                let category: Option<String> = r.get(2)?;
                Ok(LedgerRow {
                    session_id: r.get(0)?,
                    phase: phase.as_deref().and_then(phase_from_wire),
                    category: category.as_deref().and_then(category_from_wire),
                    provider_id: r.get(3)?,
                    model: r.get(4)?,
                    input_tokens: to_u64(r.get::<_, i64>(5)?),
                    output_tokens: to_u64(r.get::<_, i64>(6)?),
                    usd_micros: r.get(7)?,
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
            polled: false,
            recorded: false,
        };
        TransportResponse {
            status: response.status,
            body: Box::pin(metered),
        }
    }
}

/// A response body that records a `CostRecord` when the stream ends — by
/// completing **or** by being dropped.
///
/// Every chunk is yielded to the caller unchanged; a copy feeds the
/// [`UsageScan`]. Recording happens exactly once, on whichever comes first, so a
/// caller that drains the stream (to read the completion) always bills the call
/// once. Recording is best-effort: a store failure is swallowed so it can never
/// corrupt delivery of the actual model response.
///
/// ## Why the terminal `None` cannot be the only trigger (REQ-561 verify)
///
/// A metered body is not always drained. `tokio::time::timeout` **drops** the
/// future it is racing, and that future owns the provider's `TurnStream` and so
/// this body — so with recording keyed on `Poll::Ready(None)` alone, a call that
/// went out and burned tokens produced no row whenever the wait was cut short.
/// [`DUTY_DEADLINE`](crate::harness::duty::DUTY_DEADLINE) makes that a *routine*
/// outcome rather than an exotic one: a provider slow enough — or adversarial
/// enough — to sit past the deadline served every one of its calls off-ledger.
/// The same hole swallows a cancelled turn and any consumer that stops reading
/// early. Recording from [`Drop`] closes all of them at one site rather than at
/// each caller, which is where a cancellation-safety rule belongs.
///
/// ## `polled` is the line between "abandoned" and "never begun"
///
/// [`Drop`] records only a body the caller actually asked for a chunk. That
/// distinction is not fastidiousness — it is what keeps this fix from inventing
/// rows. A 4xx/5xx response is rejected on its **status**, before a byte of its
/// body is read (see the provider adapters' `stream_turn`), and that body is
/// then dropped unpolled. Recording it would append a 0-token, $0 row for a call
/// the provider refused, inflating `CostReport::calls` with requests that bought
/// nothing — a change to what the ledger *means*, made as a side effect of
/// closing a leak. One `bool` draws the line where the ledger already draws it:
/// a row means a response this daemon began to consume.
///
/// What [`Drop`] does **not** do is complete the token counts. A stream
/// abandoned before its provider reported usage records what the scan saw, which
/// for an OpenAI-compatible family that only reports in its terminal chunk is
/// zero — the same answer the drained path gives for a stream that carries no
/// usage at all. So this makes an interrupted call *visible*; it does not make
/// it *fully priced*, and no `Drop` impl could.
struct MeteredBody {
    inner: ByteStream,
    conn: Arc<Mutex<Connection>>,
    prices: Arc<PriceTable>,
    sink: Arc<dyn CostEventSink>,
    session_id: SessionId,
    provider_id: ProviderId,
    attribution: CostAttribution,
    scan: UsageScan,
    /// Whether the caller ever asked this body for a chunk. See the type doc:
    /// an unpolled body is a response that was refused on its status, not a call
    /// that was served and abandoned.
    polled: bool,
    recorded: bool,
}

impl MeteredBody {
    /// Write this call's row, at most once per body.
    ///
    /// The latch is checked here rather than at each of the two callers, so
    /// "exactly one `CostRecord` per call" holds by construction whichever of
    /// them fires first — and a third trigger added later cannot double-bill.
    fn record_once(&mut self) {
        if self.recorded {
            return;
        }
        self.recorded = true;
        self.finalize();
    }

    fn finalize(&self) {
        let usage = self.scan.usage();
        // REQ-557 ADR-A: priced by declared model (see `record_call`).
        let usd_micros = self
            .prices
            .price(&self.attribution.model, usage.input, usage.output);
        let row = LedgerRow {
            session_id: self.session_id.0.clone(),
            phase: self.attribution.phase,
            category: self.attribution.category,
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
        // Set before the inner poll, not after a successful one: a body that
        // answers `Pending` forever — the stalling provider the deadline exists
        // for — has still been begun, and is exactly the call `Drop` must bill.
        this.polled = true;
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                this.scan.feed(&chunk);
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
            Poll::Ready(None) => {
                this.record_once();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// The other end of the stream's life: a body that was begun and then abandoned
/// is billed here (see the type doc).
///
/// Nothing on this path can panic — [`UsageScan::usage`] answers zero for a
/// stream that reported nothing, the price table clamps, and `insert_and_emit`
/// maps a poisoned mutex to an error rather than unwrapping it — which matters
/// more here than on the polled path: a panic in a `Drop` running during
/// unwinding aborts the process instead of propagating. The one call that is not
/// this module's is `CostEventSink::cost_recorded`, which the completed-stream
/// path already makes from inside the same `finalize`.
impl Drop for MeteredBody {
    fn drop(&mut self) {
        if self.polled {
            self.record_once();
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
               (recorded_at_ms, session_id, phase, category, provider_id, model,
                input_tokens, output_tokens, usd_micros)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                now_ms(),
                row.session_id,
                row.phase.map(phase_to_wire),
                row.category.map(category_to_wire),
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

/// Bring a ledger written by an older build up to [`SCHEMA`], additively.
///
/// Adds any [`ADDITIVE_COLUMNS`] entry the table is missing and touches nothing
/// else. `ALTER TABLE … ADD COLUMN` is DDL, so it does not fire the row-level
/// `cost_records_no_update` trigger and the append-only guarantee survives
/// intact: no existing row is read, rewritten, or backfilled. They keep the NULL
/// the new column defaults to, which is the honest value for a call recorded
/// before the concept existed.
fn migrate(conn: &Connection) -> Result<(), rusqlite::Error> {
    for (column, ddl) in ADDITIVE_COLUMNS {
        if !has_column(conn, column)? {
            conn.execute_batch(ddl)?;
        }
    }
    Ok(())
}

/// Whether `cost_records` already has `column`.
fn has_column(conn: &Connection, column: &str) -> Result<bool, rusqlite::Error> {
    let mut stmt =
        conn.prepare("SELECT 1 FROM pragma_table_info('cost_records') WHERE name = ?1")?;
    stmt.exists([column])
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
    }
}

/// Parse a phase back from its wire form; unknown strings become `None`.
///
/// A ledger written by an older build can still hold `"freeform"`, and that row
/// now reads back as "no phase" — see the explicit arm below.
fn phase_from_wire(s: &str) -> Option<Phase> {
    Some(match s {
        "spec" => Phase::Spec,
        "architect" => Phase::Architect,
        "implement" => Phase::Implement,
        "review" => Phase::Review,
        "io" => Phase::Io,
        // `"freeform"` is the phase variant REQ-558 ADR-G retired. Its rows are
        // reattributed to the no-phase bucket, and that is a decision, not the
        // catch-all below happening to swallow it: freeform was never a
        // lifecycle position, the freeform path has always recorded `phase:
        // NULL`, and the only rows carrying the literal string came from a
        // structured session explicitly created at `Phase::Freeform`. Keep this
        // arm above the catch-all so a reader six months from now can tell a
        // human chose this rather than a `_ => None` swallowing a live value.
        "freeform" => return None,
        _ => return None,
    })
}

/// The lowercase wire form of a routing category (matches
/// `teton_protocol::Category`'s serde and its `as_str`).
fn category_to_wire(category: Category) -> &'static str {
    category.as_str()
}

/// Parse a category back from its stored wire form; unknown strings become
/// `None`.
///
/// This is the ledger reading back a column **it wrote itself**, not a parse of
/// anything a user or a model can author, so it does not reopen the hole
/// REQ-558 AC-3 closes: `teton_core::Category` still has no path in from text,
/// and the value produced here is the wire twin, which has no conversion into
/// the core type. Kept as a private function rather than a `FromStr` impl for
/// exactly that reason.
fn category_from_wire(s: &str) -> Option<Category> {
    Some(match s {
        "route" => Category::Route,
        "redact" => Category::Redact,
        "title" => Category::Title,
        "digest" => Category::Digest,
        "compact" => Category::Compact,
        "triage" => Category::Triage,
        "edit" => Category::Edit,
        "shell" => Category::Shell,
        "design" => Category::Design,
        "debug" => Category::Debug,
        "review" => Category::Review,
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
                &CostAttribution::new("claude-opus-4")
                    .with_phase(Phase::Review)
                    .with_category(Category::Review),
                1000,
                500,
            )
            .expect("record");
        let rows = ledger.all_records().expect("read");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session_id, "sess-1");
        assert_eq!(rows[0].phase, Some(Phase::Review));
        assert_eq!(rows[0].category, Some(Category::Review));
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
        assert_eq!(recorded[0].category, Some(Category::Review));
        assert_eq!(recorded[0].usd_micros, 15_000 + 37_500);
    }

    /// REQ-558 BR-11: a freeform call has a category and no phase, and a
    /// structured call has both. The two travel independently, so adding the
    /// category cannot quietly reattribute the phase rollup.
    #[test]
    fn a_freeform_call_records_a_category_without_a_phase() {
        let (ledger, sink) = ledger();
        ledger
            .record_call(
                "sess-2",
                "anthropic",
                &CostAttribution::new("claude-opus-4").with_category(Category::Design),
                10,
                5,
            )
            .expect("record");
        let rows = ledger.all_records().expect("read");
        assert_eq!(rows[0].phase, None);
        assert_eq!(rows[0].category, Some(Category::Design));
        assert_eq!(
            sink.records.lock().unwrap()[0].category,
            Some(Category::Design)
        );
    }

    /// Every category survives the store → read-back round trip, swept from
    /// `teton_core::Category::ALL` through the router's wire conversion rather
    /// than a hand-kept list. A category added to the core enum reaches this
    /// test without anyone remembering to extend it.
    #[test]
    fn every_category_round_trips_through_the_stored_wire_form() {
        for core in teton_core::Category::ALL {
            let category = crate::router::to_protocol_category(core);
            let stored = category_to_wire(category);
            assert_eq!(
                category_from_wire(stored),
                Some(category),
                "{core} does not survive the ledger round trip as '{stored}'"
            );
        }
        // A column value this build does not recognize reads as absent rather
        // than as some other category.
        assert_eq!(category_from_wire("summarize"), None);
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

    /// The `cost_records` table exactly as a build before REQ-558 created it —
    /// no `category` column. Reproduced verbatim (rather than derived from
    /// [`SCHEMA`]) because the point of the test is that *this* file, the one
    /// already on a user's disk, still opens.
    const PRE_REQ_SCHEMA: &str = "
CREATE TABLE cost_records (
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
CREATE TRIGGER cost_records_no_update
    BEFORE UPDATE ON cost_records
    BEGIN SELECT RAISE(ABORT, 'cost ledger is append-only'); END;
CREATE TRIGGER cost_records_no_delete
    BEFORE DELETE ON cost_records
    BEGIN SELECT RAISE(ABORT, 'cost ledger is append-only'); END;
";

    /// A unique on-disk path for one test (no `tempfile` dependency in this
    /// crate; this is the house pattern).
    fn scratch_db(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("teton-ledger-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir.join(format!("{tag}.db"))
    }

    /// A `cost.db` written before categories existed must still open, still read,
    /// and still accept new rows. Its historical rows get a NULL category — they
    /// predate the concept, and the append-only store has no business inventing
    /// an attribution for a call nobody categorized.
    #[test]
    fn a_pre_req_ledger_still_opens_and_its_rows_read_back_uncategorized() {
        let path = scratch_db("pre-req");
        let _ = std::fs::remove_file(&path);
        {
            let old = Connection::open(&path).expect("create pre-REQ ledger");
            old.execute_batch(PRE_REQ_SCHEMA).expect("pre-REQ schema");
            old.execute(
                "INSERT INTO cost_records
                   (recorded_at_ms, session_id, phase, provider_id, model,
                    input_tokens, output_tokens, usd_micros)
                 VALUES (1, 'old-session', 'review', 'anthropic', 'claude-opus-4', 900, 100, 42)",
                [],
            )
            .expect("historical row");
        }

        let sink = Arc::new(CapturingSink::default());
        let ledger = CostLedger::open(&path, PriceTable::bundled(), sink.clone())
            .expect("open pre-REQ file");

        // The historical row survives the migration untouched, with no category.
        let rows = ledger.all_records().expect("read");
        assert_eq!(rows.len(), 1, "the migration must not drop or rewrite rows");
        assert_eq!(rows[0].session_id, "old-session");
        assert_eq!(rows[0].phase, Some(Phase::Review));
        assert_eq!(rows[0].category, None, "a pre-REQ row has no category");
        assert_eq!(rows[0].input_tokens, 900);
        assert_eq!(rows[0].usd_micros, Some(42));

        // And the migrated file takes new categorized rows.
        ledger
            .record_call(
                "new-session",
                "anthropic",
                &CostAttribution::new("claude-opus-4").with_category(Category::Debug),
                10,
                20,
            )
            .expect("append after migration");
        let rows = ledger.all_records().expect("read");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].category, Some(Category::Debug));
        // Append-only still holds after the ALTER TABLE.
        {
            let guard = ledger.conn.lock().unwrap();
            assert!(guard
                .execute("UPDATE cost_records SET model = 'x'", [])
                .is_err());
        }

        // Re-opening is idempotent: the migration sees the column and does
        // nothing, rather than failing on a duplicate ADD COLUMN.
        let reopened = CostLedger::open(&path, PriceTable::bundled(), sink)
            .expect("re-open a migrated ledger");
        assert_eq!(reopened.all_records().expect("read").len(), 2);

        let _ = std::fs::remove_file(&path);
    }

    /// REQ-558 ADR-G: `Phase::Freeform` is retired, so a historical row storing
    /// the literal `"freeform"` reads back as **no phase** and rolls up into the
    /// `none` bucket. That reattribution is a decision, so it is pinned by a
    /// test and by an explicit arm in [`phase_from_wire`] — not left to the
    /// catch-all, where it would read as an oversight.
    #[test]
    fn a_stored_freeform_phase_reattributes_to_the_no_phase_bucket() {
        assert_eq!(
            phase_from_wire("freeform"),
            None,
            "the retired variant must not resolve to any live phase"
        );
        // Every phase that still exists is unaffected — the reattribution is
        // scoped to the one retired value, not a general loosening. Swept from
        // `teton_core::Phase::ALL` so a phase added later lands here without
        // anyone remembering to extend the list.
        for core in teton_core::phase::Phase::ALL {
            let phase = crate::router::to_protocol_phase(core);
            assert_eq!(
                phase_from_wire(phase_to_wire(phase)),
                Some(phase),
                "{core} does not survive the ledger round trip"
            );
        }

        // And end to end, against a row an older build could really have
        // written: it survives (the ledger is append-only), it is simply
        // unattributed.
        let (ledger, _sink) = ledger();
        {
            let guard = ledger.conn.lock().unwrap();
            guard
                .execute(
                    "INSERT INTO cost_records
                       (recorded_at_ms, session_id, phase, category, provider_id, model,
                        input_tokens, output_tokens, usd_micros)
                     VALUES (1, 'old', 'freeform', NULL, 'deepseek', 'deepseek-chat', 10, 5, 7)",
                    [],
                )
                .expect("a pre-ADR-G row");
        }
        let rows = ledger.all_records().expect("read");
        assert_eq!(rows.len(), 1, "the row is kept, not dropped");
        assert_eq!(rows[0].phase, None);
        assert_eq!(rows[0].usd_micros, Some(7), "its cost is still attributed");
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

    /// A body that never terminates: it yields its chunks and then answers
    /// `Pending` forever, which is what a stalled connection or a provider that
    /// simply stops finishing its stream produces. `drain` would hang on it —
    /// that is the point.
    fn stalling_body_from(chunks: Vec<&str>) -> ByteStream {
        let owned: Vec<Result<Vec<u8>, TransportError>> = chunks
            .into_iter()
            .map(|c| Ok(c.as_bytes().to_vec()))
            .collect();
        Box::pin(futures::stream::iter(owned).chain(futures::stream::pending()))
    }

    /// **REQ-561 verify — an abandoned stream is still a call that went out.**
    ///
    /// `tokio::time::timeout` drops the future it is racing, and that future
    /// owns the response body. Keyed on the terminal `None` alone, the row for
    /// this call is never written: the request left the machine, the provider
    /// reported the input tokens it charged for, and the ledger says nothing
    /// happened. Deleting the `Drop` impl fails here.
    #[tokio::test]
    async fn a_stream_abandoned_mid_flight_still_bills_what_the_provider_reported() {
        let (ledger, sink) = ledger();
        let ledger = Arc::new(ledger);
        // Anthropic-shaped: `message_start` carries the input tokens up front,
        // which is why the count is knowable before the stream ends.
        let body = stalling_body_from(vec![
            "event: message_start\ndata: {\"message\":{\"usage\":{\"input_tokens\":1200,\"output_tokens\":1}}}\n\n",
        ]);
        let response = TransportResponse { status: 200, body };
        let metered = CostMeter::meter_response(
            ledger.as_ref(),
            response,
            Some(SessionId::from("sess-stalled")),
            ProviderId::from("anthropic"),
            CostAttribution::new("claude-opus-4").with_category(Category::Title),
        );

        // Read what the provider did send, then give up on it — the shape a
        // deadline, a cancelled turn, or an early `break` leaves behind.
        {
            let mut body = metered.body;
            let first = body.next().await.expect("the provider did send a chunk");
            assert!(first.is_ok(), "non-vacuity: the body really was read");
            assert!(
                ledger.all_records().expect("read").is_empty(),
                "nothing is billed while the stream is still alive"
            );
        }

        let rows = ledger.all_records().expect("read");
        assert_eq!(rows.len(), 1, "an abandoned call is still one CostRecord");
        assert_eq!(rows[0].session_id, "sess-stalled");
        assert_eq!(rows[0].category, Some(Category::Title));
        assert_eq!(
            rows[0].input_tokens, 1200,
            "billed at what the provider had already reported, not at zero"
        );
        assert_eq!(
            sink.records.lock().unwrap().len(),
            1,
            "and a subscriber watching cost sees it, not just the store"
        );
    }

    /// The other half of that line, and the reason `Drop` is gated on `polled`.
    ///
    /// A 4xx/5xx response is refused on its **status** by every provider
    /// adapter, before a byte of its body is read; the body is then dropped
    /// unpolled. Billing it would append a 0-token, $0 row per rejected request
    /// — a call that bought nothing, inflating `CostReport::calls`. Removing the
    /// `polled` guard fails here.
    #[tokio::test]
    async fn a_response_refused_on_its_status_is_never_billed() {
        let (ledger, sink) = ledger();
        let ledger = Arc::new(ledger);
        let body = body_from(vec!["{\"error\":{\"type\":\"rate_limit_error\"}}"]);
        let response = TransportResponse { status: 429, body };
        let metered = CostMeter::meter_response(
            ledger.as_ref(),
            response,
            Some(SessionId::from("sess-429")),
            ProviderId::from("anthropic"),
            CostAttribution::new("claude-opus-4"),
        );
        // What `stream_turn` does with a >= 400: return an error and drop the
        // response, body unread.
        assert_eq!(metered.status, 429);
        drop(metered);

        assert!(
            ledger.all_records().expect("read").is_empty(),
            "a request the provider refused is not a metered call"
        );
        assert!(sink.records.lock().unwrap().is_empty());
    }

    /// The latch holds across both triggers: a drained stream that is then
    /// dropped bills once, not twice.
    #[tokio::test]
    async fn a_drained_stream_is_not_billed_again_when_it_is_dropped() {
        let (ledger, sink) = ledger();
        let ledger = Arc::new(ledger);
        let body = body_from(vec![
            "data: {\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n",
        ]);
        let response = TransportResponse { status: 200, body };
        let metered = CostMeter::meter_response(
            ledger.as_ref(),
            response,
            Some(SessionId::from("sess-once")),
            ProviderId::from("deepseek"),
            CostAttribution::new("deepseek-chat"),
        );
        drain(metered.body).await; // drains to the terminal `None`, then drops
        assert_eq!(
            ledger.all_records().expect("read").len(),
            1,
            "exactly one CostRecord per call, whichever trigger fires first"
        );
        assert_eq!(sink.records.lock().unwrap().len(), 1);
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
