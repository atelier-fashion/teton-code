//! The append-only cost ledger (BR-2) and its egress metering seam.
//!
//! A daemon-local SQLite file (bundled SQLite — no system dependency) holding
//! one row per completed remote call. The store is **append-only**: the schema
//! installs triggers that abort any `UPDATE` or `DELETE`, so the billing history
//! is immutable by construction, and the only write path is [`CostLedger::record`].
//!
//! ## Two tables, one file (REQ-563 D-7)
//!
//! `cost_records` holds provider calls. `web_lookups` holds web lookups, in a
//! **sibling** table rather than as overloaded provider rows: a lookup has no
//! model, no token counts, and no provider id, and the columns it does have
//! (host, bytes, duration, outcome) mean nothing on a model call. Folding the
//! two would leave every row half-null and force every reader to know which half
//! applied. They share the file, the append-only triggers, and the `/cost`
//! aggregation.
//!
//! ## Privacy (BR-7)
//!
//! Every column is a token count or a piece of routing metadata — session id,
//! phase, provider id, model name, input/output token counts, computed cost —
//! or, on the lookup side, the destination **host** and the size of what came
//! back. There is deliberately **no column** that could carry prompt text, tool
//! arguments, a full URL, a search query, or a credential. A ledger row is safe
//! to read, export, or ship in a report.
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

use teton_protocol::events::{CostRecord, WebLookupKind, WebLookupOutcome};
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
    usd_micros     INTEGER,
    cached_tokens  INTEGER,
    reasoning_tokens INTEGER,
    probe          INTEGER
);
CREATE TRIGGER IF NOT EXISTS cost_records_no_update
    BEFORE UPDATE ON cost_records
    BEGIN SELECT RAISE(ABORT, 'cost ledger is append-only'); END;
CREATE TRIGGER IF NOT EXISTS cost_records_no_delete
    BEFORE DELETE ON cost_records
    BEGIN SELECT RAISE(ABORT, 'cost ledger is append-only'); END;

CREATE TABLE IF NOT EXISTS web_lookups (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    recorded_at_ms INTEGER NOT NULL,
    session_id     TEXT    NOT NULL,
    kind           TEXT    NOT NULL,
    host           TEXT    NOT NULL,
    bytes_in       INTEGER NOT NULL,
    duration_ms    INTEGER NOT NULL,
    outcome        TEXT    NOT NULL,
    usd_micros     INTEGER
);
CREATE TRIGGER IF NOT EXISTS web_lookups_no_update
    BEFORE UPDATE ON web_lookups
    BEGIN SELECT RAISE(ABORT, 'cost ledger is append-only'); END;
CREATE TRIGGER IF NOT EXISTS web_lookups_no_delete
    BEFORE DELETE ON web_lookups
    BEGIN SELECT RAISE(ABORT, 'cost ledger is append-only'); END;

CREATE TABLE IF NOT EXISTS web_overrides (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    recorded_at_ms INTEGER NOT NULL,
    session_id     TEXT    NOT NULL,
    tiers_restored TEXT    NOT NULL
);
CREATE TRIGGER IF NOT EXISTS web_overrides_no_update
    BEFORE UPDATE ON web_overrides
    BEGIN SELECT RAISE(ABORT, 'cost ledger is append-only'); END;
CREATE TRIGGER IF NOT EXISTS web_overrides_no_delete
    BEFORE DELETE ON web_overrides
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
///
/// A new *table* needs no entry here: `CREATE TABLE IF NOT EXISTS` in [`SCHEMA`]
/// does reach an existing file, which is how REQ-563's `web_lookups` arrives on
/// a `cost.db` written before it existed. Only a new column on a table that is
/// already there is invisible to the schema batch.
const ADDITIVE_COLUMNS: [(&str, &str); 4] = [
    (
        // REQ-558: the routing category the call was made for.
        "category",
        "ALTER TABLE cost_records ADD COLUMN category TEXT",
    ),
    (
        // REQ-564: prompt tokens whose KV was reused from the resident prefix.
        // Nullable and never backfilled: a remote row has no prefix cache, and a
        // row written before this column existed predates the concept — which is
        // the truth about it, not a zero.
        "cached_tokens",
        "ALTER TABLE cost_records ADD COLUMN cached_tokens INTEGER",
    ),
    (
        // REQ-559: the reasoning subset of `output_tokens`. Nullable and never
        // backfilled: a provider that reported no split told us nothing, and a
        // row written before this column existed predates the concept — `None`
        // is the truth about both, and a `0` would be an invented attribution.
        "reasoning_tokens",
        "ALTER TABLE cost_records ADD COLUMN reasoning_tokens INTEGER",
    ),
    (
        // REQ-581: `1` on a connection-test row, NULL on every turn and on
        // every row written before this column existed. Nullable and never
        // backfilled for the reason the two above are: a pre-REQ daemon had no
        // `provider/test` to run, so those rows predate the concept — NULL is
        // the truth about them, and the read maps it to "not a probe".
        "probe",
        "ALTER TABLE cost_records ADD COLUMN probe INTEGER",
    ),
];

/// How many trailing bytes of one chunk the usage scanner carries into the next,
/// so a usage key or its number split across a chunk boundary is still matched.
/// Comfortably larger than the longest key plus a token count.
const CARRY_BYTES: usize = 64;

impl super::LocalUsageMeter for CostLedger {
    fn local_call(
        &self,
        session_id: &teton_protocol::SessionId,
        attribution: &CostAttribution,
        input_tokens: u64,
        output_tokens: u64,
        cached_tokens: u64,
    ) {
        // Best-effort by contract: a ledger hiccup must never fail a turn the
        // user already received an answer for. The failure is logged, not
        // swallowed silently (LESSON-456).
        if let Err(err) = self.record_local_call(
            session_id.0.clone(),
            attribution,
            input_tokens,
            output_tokens,
            cached_tokens,
        ) {
            // The turn already happened; a ledger that could not be written is
            // an accounting failure, not a reason to fail it. The line names the
            // failure class and no part of the prompt (LedgerError is
            // content-free by construction).
            eprintln!("teton: a local turn could not be recorded in the cost ledger ({err})");
        }
    }
}

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
    /// Prompt tokens whose KV was reused from the resident prefix (REQ-564
    /// BR-9), or `None` for a call with no prefix cache.
    ///
    /// `None` on every remote row — a remote provider has no local KV, so
    /// "no cache" is the fact, not "nothing was reused". A *local* row records
    /// `Some(0)` on a miss, which is the different claim that a cache existed
    /// and did not serve. `cached_tokens` is a **component of**
    /// `input_tokens`, never a substitute: the prompt is still that many tokens
    /// whether or not their KV had to be recomputed.
    pub cached_tokens: Option<u64>,
    /// Of `output_tokens`, how many the provider attributed to reasoning
    /// (REQ-559 BR-10), or `None` where it reported none.
    ///
    /// `None` on every Anthropic row (that API reports no split), every local
    /// row, and every row written before this column existed. A **component
    /// of** `output_tokens`, never an addition: the completion is that many
    /// tokens whether or not the provider broke out the thinking share.
    pub reasoning_tokens: Option<u64>,
    /// Whether this row is a **connection test** rather than a turn (REQ-581
    /// BR-5).
    ///
    /// A `bool` here over the column's `Option<i64>` because, unlike
    /// `cached_tokens` and `reasoning_tokens`, the absent value carries no
    /// distinct meaning: a row that predates the column was written by a daemon
    /// with no `provider/test` to run, so "we do not know" and "not a probe"
    /// are the same fact. Stored as `1` / NULL, read back as `true` / `false`.
    ///
    /// A probe is billed like any other call — the flag counts it apart, it
    /// does not exempt it.
    pub probe: bool,
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
            cached_tokens: self.cached_tokens,
            reasoning_tokens: self.reasoning_tokens,
            probe: self.probe,
        }
    }
}

/// One row of the `web_lookups` table — a destination host and sizes, never an
/// utterance (REQ-563 BR-7).
///
/// Recorded for **every** lookup attempt, whatever the outcome: BR-7 asks for
/// the free ones too, and a ledger that held only the lookups that succeeded
/// could not answer "what did this session try to reach". The blocked, refused
/// and cache-hit endings are [`WebLookupOutcome`] values on the same row shape
/// (architecture D-8), so the count of rows is the count of attempts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebLookupRow {
    /// Session that performed the lookup.
    pub session_id: String,
    /// Fetch or search.
    pub kind: WebLookupKind,
    /// The destination **host** — never the scheme, path, query string, or a
    /// credential. There is no column that could hold one.
    pub host: String,
    /// Bytes of content the lookup brought back; `0` for an ending that
    /// transferred nothing.
    pub bytes_in: u64,
    /// Wall-clock duration of the attempt in milliseconds — including a refusal,
    /// which is cheap and should look it.
    pub duration_ms: u64,
    /// How the lookup ended.
    pub outcome: WebLookupOutcome,
    /// Cost in integer micro-USD, or `None` when the backend is **unpriced**.
    ///
    /// Every lookup this build performs is genuinely free, so it records
    /// `Some(0)`: zero is a measured fact here, not the guess REQ-557 BR-9
    /// forbids. The column is nullable anyway so a later metered search backend
    /// whose price is *unknown* has the same honest "no price" value the
    /// provider table already uses — the distinction between free and unpriced
    /// is one this ledger has learned to keep, and adding the column now means
    /// no migration then (D-7).
    pub usd_micros: Option<i64>,
}

/// One row of the `web_overrides` table — the other half of BR-13's account
/// (REQ-563).
///
/// The `web_lookups` table records what a session *reached*; without this one it
/// could not record the moment a session's model-composed lookups were
/// re-enabled, which is the single most consequential thing a user can do to
/// this capability. Both tables are append-only under the same triggers, so
/// "when did this session stop being restricted" has an answer that no later
/// write can revise.
///
/// Content-free by the same rule as [`WebLookupRow`]: a session id and a list of
/// tier *names* out of the config vocabulary. There is no column that could hold
/// a URL, a query, or a reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebOverrideRow {
    /// Session whose restriction was lifted.
    pub session_id: String,
    /// The tiers the lift restored, lowest first, in the `[web] tier` spelling.
    ///
    /// A list rather than a single ceiling because that is what the user was
    /// told: the `web_taint_overridden` event names the same tiers, and a row
    /// that recorded only the ceiling would not match the sentence the user
    /// read.
    pub tiers_restored: Vec<&'static str>,
}

/// The separator between tier names in the `tiers_restored` column.
///
/// One column holding a short, closed list beats a join table for a value that
/// is read by a human and never queried by element: the vocabulary is four
/// words, and none of them contains this character.
const TIER_LIST_SEPARATOR: char = ',';

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
            // This convenience is used where the caller holds only aggregate
            // counts; the metered stream path (`MeteredBody::finalize`) is what
            // carries a real reasoning split.
            reasoning_tokens: None,
            // A remote provider has no local KV: "no cache" rather than
            // "nothing reused".
            cached_tokens: None,
            // Carried from the attribution rather than defaulted: the caller is
            // the only layer that knows whether it asked a question or tested a
            // connection (REQ-581 BR-5).
            probe: attribution.probe,
        })
    }

    /// Append a **local-tier** usage row (REQ-564 BR-9).
    ///
    /// Local inference is a usage record, not a spend record. The model is
    /// absent from the price table, so [`Prices::price`] returns `None` and the
    /// row is recorded **unpriced** — deliberately not `Some(0)`, which would
    /// tell the meter that a call was priced and cost nothing. Unpriced is the
    /// truth and keeps the report's priced/unpriced split honest.
    ///
    /// A sibling of [`CostLedger::record_call`] rather than a widened signature:
    /// every remote caller would otherwise have to pass a `None` that can only
    /// ever be `None`.
    ///
    /// # Errors
    /// [`LedgerError`] if the insert fails or the mutex is poisoned.
    pub fn record_local_call(
        &self,
        session_id: impl Into<String>,
        attribution: &CostAttribution,
        input_tokens: u64,
        output_tokens: u64,
        cached_tokens: u64,
    ) -> Result<(), LedgerError> {
        let usd_micros = self
            .prices
            .price(&attribution.model, input_tokens, output_tokens);
        self.record(LedgerRow {
            session_id: session_id.into(),
            phase: attribution.phase,
            category: attribution.category,
            // The local tier names itself here, as it does everywhere the tier
            // comes from the engine rather than a `[[providers]]` entry
            // (REQ-557 ADR-D).
            provider_id: "local".to_owned(),
            model: attribution.model.clone(),
            input_tokens,
            output_tokens,
            usd_micros,
            cached_tokens: Some(cached_tokens),
            // The local engine reports no reasoning split (BR-6).
            reasoning_tokens: None,
            probe: attribution.probe,
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
                    input_tokens, output_tokens, usd_micros, cached_tokens,
                    reasoning_tokens, probe
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
                    cached_tokens: r.get::<_, Option<i64>>(8)?.map(to_u64),
                    reasoning_tokens: r.get::<_, Option<i64>>(9)?.map(to_u64),
                    // Only the `1` this build writes means "probe". A NULL (a
                    // turn, or a row from before the column existed) and a `0`
                    // both read as false — the same posture
                    // `category_from_wire` takes toward a value it does not
                    // recognize: read what was recorded, never guess past it.
                    probe: r.get::<_, Option<i64>>(10)? == Some(1),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Append one web-lookup row (REQ-563 BR-7).
    ///
    /// Storage only: unlike [`CostLedger::record`] this broadcasts nothing. The
    /// `web_lookup` event is published by the lookup seam, which is the layer
    /// that knows the session scope and has already decided the outcome; making
    /// the store emit it would mean growing [`CostEventSink`] a method for an
    /// event the store cannot fully describe, and would put one event on two
    /// emitters.
    ///
    /// # Errors
    /// [`LedgerError`] if the insert fails or the mutex is poisoned.
    pub fn record_web_lookup(&self, row: &WebLookupRow) -> Result<(), LedgerError> {
        let guard = self.conn.lock().map_err(|_| LedgerError::Poisoned)?;
        guard.execute(
            "INSERT INTO web_lookups
               (recorded_at_ms, session_id, kind, host, bytes_in, duration_ms,
                outcome, usd_micros)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                now_ms(),
                row.session_id,
                web_kind_to_wire(row.kind),
                row.host,
                to_i64(row.bytes_in),
                to_i64(row.duration_ms),
                web_outcome_to_wire(row.outcome),
                row.usd_micros,
            ],
        )?;
        Ok(())
    }

    /// Append one `web_overrides` row (REQ-563 BR-13).
    ///
    /// Storage only, exactly like [`CostLedger::record_web_lookup`]: the
    /// `web_taint_overridden` event is published by the RPC handler, which is
    /// the layer that knows whether the lift was a transition at all.
    ///
    /// # Errors
    /// [`LedgerError`] if the insert fails or the mutex is poisoned.
    pub fn record_web_override(&self, row: &WebOverrideRow) -> Result<(), LedgerError> {
        let guard = self.conn.lock().map_err(|_| LedgerError::Poisoned)?;
        guard.execute(
            "INSERT INTO web_overrides (recorded_at_ms, session_id, tiers_restored)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![
                now_ms(),
                row.session_id,
                row.tiers_restored.join(&TIER_LIST_SEPARATOR.to_string()),
            ],
        )?;
        Ok(())
    }

    /// Every recorded web override, in insertion order.
    ///
    /// The stored tier names are handed back as owned strings rather than
    /// re-resolved to the `&'static str` vocabulary: a name this build does not
    /// recognize is a hand-edited store, and reporting what is actually written
    /// there is more useful than dropping the row (the posture
    /// [`CostLedger::all_web_lookups`] takes for a whole row, at the granularity
    /// this column allows).
    ///
    /// # Errors
    /// [`LedgerError`] if the query fails or the mutex is poisoned.
    pub fn all_web_overrides(&self) -> Result<Vec<(String, Vec<String>)>, LedgerError> {
        let guard = self.conn.lock().map_err(|_| LedgerError::Poisoned)?;
        let mut stmt =
            guard.prepare("SELECT session_id, tiers_restored FROM web_overrides ORDER BY id")?;
        let rows = stmt
            .query_map([], |r| {
                let session_id: String = r.get(0)?;
                let tiers: String = r.get(1)?;
                Ok((
                    session_id,
                    tiers
                        .split(TIER_LIST_SEPARATOR)
                        .filter(|t| !t.is_empty())
                        .map(str::to_owned)
                        .collect(),
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Every recorded web lookup, in insertion order.
    ///
    /// A row whose `kind` or `outcome` this build does not recognize is skipped
    /// rather than guessed at — the same posture [`category_from_wire`] takes.
    /// It cannot happen from a *newer* daemon writing the file (they share the
    /// vocabulary), only from a hand-edited store, and inventing an outcome for
    /// one would misreport what a session did.
    ///
    /// # Errors
    /// [`LedgerError`] if the query fails or the mutex is poisoned.
    pub fn all_web_lookups(&self) -> Result<Vec<WebLookupRow>, LedgerError> {
        let guard = self.conn.lock().map_err(|_| LedgerError::Poisoned)?;
        let mut stmt = guard.prepare(
            "SELECT session_id, kind, host, bytes_in, duration_ms, outcome, usd_micros
             FROM web_lookups ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let kind: String = r.get(1)?;
                let outcome: String = r.get(5)?;
                Ok(
                    match (web_kind_from_wire(&kind), web_outcome_from_wire(&outcome)) {
                        (Some(kind), Some(outcome)) => Some(WebLookupRow {
                            session_id: r.get(0)?,
                            kind,
                            host: r.get(2)?,
                            bytes_in: to_u64(r.get::<_, i64>(3)?),
                            duration_ms: to_u64(r.get::<_, i64>(4)?),
                            outcome,
                            usd_micros: r.get(6)?,
                        }),
                        _ => None,
                    },
                )
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().flatten().collect())
    }

    /// Aggregate the whole ledger into an AC-4 [`CostReport`](super::CostReport).
    ///
    /// # Errors
    /// [`LedgerError`] if reading the rows fails.
    pub fn report(&self) -> Result<super::CostReport, LedgerError> {
        Ok(super::report::aggregate(
            &self.all_records()?,
            &self.all_web_lookups()?,
            &self.prices,
        ))
    }
}

impl CostMeter for CostLedger {
    fn meter_response(
        &self,
        response: TransportResponse,
        session_id: Option<SessionId>,
        provider_id: ProviderId,
        attribution: CostAttribution,
        spend: Option<Arc<super::PromptSpend>>,
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
            // REQ-559: the status the provider answered with. `polled` alone is
            // no longer sufficient — see `should_record`.
            status: response.status,
            polled: false,
            recorded: false,
            // REQ-588 ADR-1/ADR-2: the prompt's accumulator, fed at `finalize`
            // where the call's ACTUAL cost is known. Feeding it anywhere
            // earlier would be the pre-flight estimate ADR-2 rejected.
            spend,
        };
        TransportResponse {
            status: response.status,
            // Carried through rather than dropped: the meter wraps the *body*,
            // and a wrapper that quietly erased a response field would make
            // metering change what the caller can see about the response.
            location: response.location,
            body: Box::pin(metered),
        }
    }

    fn can_price(&self, model: &str) -> bool {
        // The same table `finalize` prices with, asked the same question one
        // step earlier — so "we will be able to count this" and "we counted
        // it" cannot come to disagree.
        self.prices.entry(model).is_some()
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
/// ## Two conditions, and the second used to be implied by the first
///
/// [`Drop`] records only a body the caller actually asked for a chunk, and only
/// a response the provider did **not** refuse. Both are what keep this from
/// inventing rows: a 0-token, $0 row for a call the provider rejected would
/// inflate `CostReport::calls` with requests that bought nothing — a change to
/// what the ledger *means*, made as a side effect of closing a leak.
///
/// `polled` originally carried both, because a 4xx/5xx was rejected on its
/// status before a byte of its body was read, so a refused body was always
/// unpolled. **REQ-559 ended that**: BR-12's refusal classification reads a
/// bounded prefix of a 400 body to decide whether the provider is rejecting the
/// effort field, which polls it. The status check is therefore not redundant
/// with `polled` — it is the condition `polled` was standing in for, now stated
/// directly, and a guard keyed on a condition that stops holding once a feature
/// lands is LESSON-443's shape exactly.
///
/// `polled` still earns its place: it separates "abandoned mid-stream" from
/// "never begun" on a 2xx, which the status cannot.
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
    /// The response status, so a refusal is never billed however its body is
    /// consumed (REQ-559).
    status: u16,
    attribution: CostAttribution,
    scan: UsageScan,
    /// Whether the caller ever asked this body for a chunk. See the type doc:
    /// an unpolled body is a response that was refused on its status, not a call
    /// that was served and abandoned.
    polled: bool,
    recorded: bool,
    spend: Option<Arc<super::PromptSpend>>,
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
        // Latched even when the row is suppressed, so a suppressed body cannot
        // be reconsidered by a later trigger.
        self.recorded = true;
        if !self.should_record() {
            return;
        }
        self.finalize();
    }

    /// Whether this response is billable at all (REQ-559).
    ///
    /// A refusal buys nothing, so it is not a call — however its body was
    /// consumed. Kept separate from `polled` because the two answer different
    /// questions and only one of them is still implied by the other.
    fn should_record(&self) -> bool {
        self.status < 400
    }

    fn finalize(&self) {
        let usage = self.scan.usage();
        // REQ-557 ADR-A: priced by declared model (see `record_call`).
        let usd_micros = self
            .prices
            .price(&self.attribution.model, usage.input, usage.output);
        // REQ-588 ADR-2: the ceiling counts what was ACTUALLY spent, which is
        // only knowable here. An unpriced call feeds the sticky flag instead of
        // the total, because a total that silently absorbed unknowns would
        // claim a precision it does not have.
        if let Some(spend) = &self.spend {
            match usd_micros {
                Some(micros) if micros >= 0 => spend.add(micros.unsigned_abs()),
                // A negative price is nonsense rather than a credit; treat it
                // as unpriced rather than reducing the total.
                Some(_) | None => spend.note_unpriced(),
            }
        }
        let row = LedgerRow {
            session_id: self.session_id.0.clone(),
            phase: self.attribution.phase,
            category: self.attribution.category,
            provider_id: self.provider_id.0.clone(),
            model: self.attribution.model.clone(),
            input_tokens: usage.input,
            output_tokens: usage.output,
            usd_micros,
            // The metered stream is the remote choke point; no local KV here.
            cached_tokens: None,
            // A subset of `output_tokens`, never added to it: the price above is
            // computed from `usage.output` alone, exactly as before this field
            // existed, so totals are byte-identical (BR-10).
            reasoning_tokens: usage.reasoning,
            // REQ-581 BR-5: the connection test goes out through this same
            // choke point and is billed by this same code — the attribution is
            // the only thing that differs, and it says so here.
            probe: self.attribution.probe,
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
    /// REQ-559 BR-10: the reasoning subset of `output`, or `None` where the
    /// stream reported none. Deliberately `Option` while the other two are
    /// plain `u64`: a missing total is meaningfully zero, a missing split is
    /// not.
    reasoning: Option<u64>,
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
    /// REQ-559 BR-10: the reasoning subset of `output`, when the stream reports
    /// one. Stays `None` for a stream that does not — unreported, not zero.
    reasoning: Option<u64>,
}

/// Quoted usage keys that denote input (prompt) tokens.
const INPUT_KEYS: [&[u8]; 2] = [b"\"input_tokens\"", b"\"prompt_tokens\""];
/// Quoted usage keys that denote output (completion) tokens.
///
/// The trailing quote is load-bearing: without it `"completion_tokens"` would
/// also match inside `"completion_tokens_details"`, the very object
/// [`REASONING_KEYS`] reads, and the output total would be overwritten by
/// whatever integer happened to follow that key.
const OUTPUT_KEYS: [&[u8]; 2] = [b"\"output_tokens\"", b"\"completion_tokens\""];
/// Quoted usage key denoting the reasoning subset of the output tokens
/// (REQ-559 BR-10). Reported by OpenAI-compatible endpoints inside
/// `completion_tokens_details`; Anthropic reports no equivalent, so an
/// Anthropic stream simply never matches and the value stays `None`.
///
/// The key plus its integer is far shorter than [`CARRY_BYTES`] (64), so a
/// split across a chunk boundary is still matched.
const REASONING_KEYS: [&[u8]; 1] = [b"\"reasoning_tokens\""];

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
        for key in REASONING_KEYS {
            if let Some(v) = last_int_after(&buf, key) {
                self.reasoning = Some(v);
            }
        }
        let keep = buf.len().min(CARRY_BYTES);
        self.carry = buf.split_off(buf.len() - keep);
    }

    fn usage(&self) -> Usage {
        Usage {
            input: self.input.unwrap_or(0),
            output: self.output.unwrap_or(0),
            // NOT `unwrap_or(0)`: a stream that reported no split told us
            // nothing, and a `0` here would claim the provider did no thinking
            // (BR-10, BR-11).
            reasoning: self.reasoning,
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
                input_tokens, output_tokens, usd_micros, cached_tokens,
                reasoning_tokens, probe)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
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
                row.cached_tokens.map(to_i64),
                row.reasoning_tokens.map(to_i64),
                // NULL rather than `0` for a turn, so a row this build writes is
                // indistinguishable from one written before the column existed
                // — which is the truth: neither is a probe.
                row.probe.then_some(1_i64),
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
        // REQ-613 TASK-381: Draft arm.
        "draft" => Category::Draft,
        _ => return None,
    })
}

/// The stored form of a lookup kind.
///
/// Hand-written rather than derived from serde for the reason the category pair
/// above is: the column is a *storage* format that must stay readable by every
/// future build, and a serde rename made for the wire would silently rewrite it.
/// The two happen to agree today, and the sweep in the tests is what keeps them
/// agreeing on purpose rather than by luck.
fn web_kind_to_wire(kind: WebLookupKind) -> &'static str {
    match kind {
        WebLookupKind::Fetch => "fetch",
        WebLookupKind::Search => "search",
    }
}

/// Parse a lookup kind back from its stored form; unknown strings become `None`.
fn web_kind_from_wire(s: &str) -> Option<WebLookupKind> {
    Some(match s {
        "fetch" => WebLookupKind::Fetch,
        "search" => WebLookupKind::Search,
        _ => return None,
    })
}

/// The stored form of a lookup outcome (see [`web_kind_to_wire`]).
fn web_outcome_to_wire(outcome: WebLookupOutcome) -> &'static str {
    match outcome {
        WebLookupOutcome::Completed => "completed",
        WebLookupOutcome::CacheHit => "cache_hit",
        WebLookupOutcome::BlockedPrivacy => "blocked_privacy",
        WebLookupOutcome::BlockedRedact => "blocked_redact",
        WebLookupOutcome::RefusedDomain => "refused_domain",
        WebLookupOutcome::RefusedTier => "refused_tier",
        WebLookupOutcome::TaintRestricted => "taint_restricted",
        WebLookupOutcome::Offline => "offline",
    }
}

/// Parse a lookup outcome back from its stored form; unknown strings become
/// `None`.
fn web_outcome_from_wire(s: &str) -> Option<WebLookupOutcome> {
    Some(match s {
        "completed" => WebLookupOutcome::Completed,
        "cache_hit" => WebLookupOutcome::CacheHit,
        "blocked_privacy" => WebLookupOutcome::BlockedPrivacy,
        "blocked_redact" => WebLookupOutcome::BlockedRedact,
        "refused_domain" => WebLookupOutcome::RefusedDomain,
        "refused_tier" => WebLookupOutcome::RefusedTier,
        "taint_restricted" => WebLookupOutcome::TaintRestricted,
        "offline" => WebLookupOutcome::Offline,
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
                "sess-under-test",
                "anthropic",
                &CostAttribution::new("claude-fable-5")
                    .with_phase(Phase::Review)
                    .with_category(Category::Review),
                1000,
                500,
            )
            .expect("record");
        let rows = ledger.all_records().expect("read");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session_id, "sess-under-test");
        assert_eq!(rows[0].phase, Some(Phase::Review));
        assert_eq!(rows[0].category, Some(Category::Review));
        assert_eq!(rows[0].provider_id, "anthropic");
        assert_eq!(rows[0].model, "claude-fable-5");
        assert_eq!(rows[0].input_tokens, 1000);
        assert_eq!(rows[0].output_tokens, 500);
        assert_eq!(rows[0].usd_micros, Some(10_000 + 25_000));
        // The event fired with the same attribution.
        let recorded = sink.records.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].session_id, SessionId::from("sess-under-test"));
        assert_eq!(recorded[0].phase, Some(Phase::Review));
        assert_eq!(recorded[0].category, Some(Category::Review));
        assert_eq!(recorded[0].usd_micros, 10_000 + 25_000);
    }

    /// REQ-558 BR-11: a freeform call has a category and no phase, and a
    /// structured call has both. The two travel independently, so adding the
    /// category cannot quietly reattribute the phase rollup.
    #[test]
    fn a_freeform_call_records_a_category_without_a_phase() {
        let (ledger, sink) = ledger();
        ledger
            .record_call(
                "sess-under-test",
                "anthropic",
                &CostAttribution::new("claude-fable-5").with_category(Category::Design),
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
                "sess-under-test",
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
        assert_eq!(
            rows[0].cached_tokens, None,
            "a pre-REQ row predates the prefix cache — None is the truth about              it, not zero"
        );
        assert_eq!(
            rows[0].reasoning_tokens, None,
            "REQ-559: and it predates the reasoning split too. Never backfilled — \
             a row written before the column has no thinking share to report, and \
             a 0 would invent one",
        );
        assert!(
            !rows[0].probe,
            "REQ-581: and it predates the connection test. A daemon with no \
             `provider/test` recorded no probes, so `false` is the truth about \
             the row, read off the NULL the migration left it",
        );
        assert_eq!(rows[0].input_tokens, 900);
        assert_eq!(rows[0].usd_micros, Some(42));

        // And the migrated file takes new categorized rows.
        ledger
            .record_call(
                "new-session",
                "anthropic",
                &CostAttribution::new("claude-fable-5").with_category(Category::Debug),
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

    /// A `cost.db` written before `web_lookups` existed gains the table on the
    /// next open, with its provider rows untouched.
    ///
    /// The additive-column machinery does not apply here and does not need to:
    /// `CREATE TABLE IF NOT EXISTS` in [`SCHEMA`] *does* reach an existing file.
    /// This pins that, because the alternative — a missing table discovered at
    /// the first lookup, in a `/cost` query — is a failure a user meets rather
    /// than a test does.
    #[test]
    fn a_ledger_written_before_web_lookups_gains_the_table_on_open() {
        let path = scratch_db("pre-web");
        let _ = std::fs::remove_file(&path);
        {
            let old = Connection::open(&path).expect("create pre-REQ-563 ledger");
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
        let ledger =
            CostLedger::open(&path, PriceTable::bundled(), sink).expect("open pre-REQ-563 file");
        assert!(
            ledger.all_web_lookups().expect("read").is_empty(),
            "a file that predates the table has no lookups, and that is not an error"
        );
        ledger
            .record_web_lookup(&web_row("new-session", WebLookupOutcome::Completed, 512))
            .expect("the new table accepts rows");
        assert_eq!(ledger.all_web_lookups().expect("read").len(), 1);
        // The historical provider row is untouched by the new table's arrival.
        assert_eq!(ledger.all_records().expect("read").len(), 1);

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

    /// A representative lookup row.
    fn web_row(session: &str, outcome: WebLookupOutcome, bytes_in: u64) -> WebLookupRow {
        WebLookupRow {
            session_id: session.to_owned(),
            kind: WebLookupKind::Fetch,
            host: "docs.rs".to_owned(),
            bytes_in,
            duration_ms: 120,
            outcome,
            usd_micros: Some(0),
        }
    }

    #[test]
    fn a_web_lookup_records_and_reads_back_round_trips() {
        let (ledger, sink) = ledger();
        let row = web_row("sess-web", WebLookupOutcome::Completed, 4096);
        ledger.record_web_lookup(&row).expect("record lookup");
        assert_eq!(ledger.all_web_lookups().expect("read"), vec![row]);
        // A lookup is not a model call: it lands in its own table and does not
        // appear as a `cost_records` row or a `cost_recorded` event.
        assert!(ledger.all_records().expect("read").is_empty());
        assert!(sink.records.lock().unwrap().is_empty());
    }

    /// Every outcome survives the store → read-back round trip, swept from
    /// [`WebLookupOutcome::ALL`] rather than a hand-kept list — an outcome added
    /// to the protocol enum reaches this test without anyone remembering to
    /// extend it (the `Category::ALL` sweep above sets the pattern).
    #[test]
    fn every_lookup_kind_and_outcome_round_trips_through_the_stored_wire_form() {
        for kind in WebLookupKind::ALL {
            let stored = web_kind_to_wire(kind);
            assert_eq!(web_kind_from_wire(stored), Some(kind), "kind '{stored}'");
        }
        for outcome in WebLookupOutcome::ALL {
            let stored = web_outcome_to_wire(outcome);
            assert_eq!(
                web_outcome_from_wire(stored),
                Some(outcome),
                "{outcome:?} does not survive the ledger round trip as '{stored}'"
            );
        }
        // A column value this build does not recognize reads as absent rather
        // than as some other outcome.
        assert_eq!(web_outcome_from_wire("blocked"), None);
        assert_eq!(web_kind_from_wire("crawl"), None);
    }

    /// A stored row this build cannot read is skipped, not guessed at: reporting
    /// an unknown outcome as `completed` would claim a lookup succeeded when
    /// nothing here knows that it did.
    #[test]
    fn a_lookup_row_with_an_unreadable_outcome_is_skipped_not_guessed() {
        let (ledger, _sink) = ledger();
        ledger
            .record_web_lookup(&web_row("s", WebLookupOutcome::Completed, 10))
            .expect("record");
        {
            let guard = ledger.conn.lock().unwrap();
            guard
                .execute(
                    "INSERT INTO web_lookups
                       (recorded_at_ms, session_id, kind, host, bytes_in, duration_ms,
                        outcome, usd_micros)
                     VALUES (1, 's', 'fetch', 'docs.rs', 5, 5, 'from-the-future', 0)",
                    [],
                )
                .expect("a row this build cannot read");
        }
        let rows = ledger.all_web_lookups().expect("read");
        assert_eq!(rows.len(), 1, "the unreadable row is skipped");
        assert_eq!(rows[0].outcome, WebLookupOutcome::Completed);
    }

    /// REQ-563 BR-7 in the schema: there is no column a full URL, a search
    /// query, or a credential could ride in. Asserted against the live table
    /// definition rather than the `WebLookupRow` struct, because the storage is
    /// what outlives this build — a column added here would persist utterances
    /// on disk long after whatever wrote them.
    #[test]
    fn the_web_lookups_table_has_no_column_that_could_hold_an_utterance() {
        let (ledger, _sink) = ledger();
        let guard = ledger.conn.lock().unwrap();
        let mut stmt = guard
            .prepare("SELECT name FROM pragma_table_info('web_lookups') ORDER BY name")
            .expect("pragma");
        let columns: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .expect("query")
            .collect::<Result<Vec<_>, _>>()
            .expect("rows");
        assert_eq!(
            columns,
            vec![
                "bytes_in",
                "duration_ms",
                "host",
                "id",
                "kind",
                "outcome",
                "recorded_at_ms",
                "session_id",
                "usd_micros",
            ],
            "the lookup schema gained a column: {columns:?}"
        );
    }

    /// AC-6: `/cost` answers how many lookups a session made and how many bytes
    /// came back, end to end from the store rather than from hand-built rows.
    #[test]
    fn the_cost_report_counts_a_sessions_lookups_and_bytes() {
        let (ledger, _sink) = ledger();
        ledger
            .record_web_lookup(&web_row("sess-web", WebLookupOutcome::Completed, 4096))
            .expect("record");
        ledger
            .record_web_lookup(&web_row("sess-web", WebLookupOutcome::CacheHit, 4096))
            .expect("record");
        // Refused: still a lookup (BR-7), still no bytes.
        ledger
            .record_web_lookup(&web_row("sess-web", WebLookupOutcome::RefusedTier, 0))
            .expect("record");

        let report = ledger.report().expect("report");
        let web = report
            .web_per_session
            .iter()
            .find(|w| w.key == "sess-web")
            .expect("the session's lookups are reported");
        assert_eq!(web.lookups, 3);
        assert_eq!(web.bytes_in, 8192);
        assert_eq!(report.total.calls, 0, "no model calls were made");
    }

    /// Trigger parity with the provider table: the lookup history is immutable
    /// by construction, not by API discipline.
    #[test]
    fn the_web_lookups_table_is_append_only() {
        let (ledger, _sink) = ledger();
        ledger
            .record_web_lookup(&web_row("s", WebLookupOutcome::Completed, 1))
            .expect("record");
        let guard = ledger.conn.lock().unwrap();
        assert!(
            guard
                .execute("UPDATE web_lookups SET host = 'evil.example'", [])
                .is_err(),
            "UPDATE must be rejected by the append-only trigger"
        );
        assert!(
            guard.execute("DELETE FROM web_lookups", []).is_err(),
            "DELETE must be rejected by the append-only trigger"
        );
    }

    /// The `web_overrides` table round-trips a lift, and is append-only under
    /// the same triggers its two neighbours are (REQ-563 BR-13's ledger half).
    ///
    /// A row here says a session's model-composed lookups were re-enabled on the
    /// user's say-so — the most consequential thing a person can do to this
    /// capability — so a later write must not be able to revise or erase it.
    #[test]
    fn the_web_overrides_table_records_a_lift_and_is_append_only() {
        let (ledger, _sink) = ledger();
        ledger
            .record_web_override(&WebOverrideRow {
                session_id: "sess-web".to_owned(),
                tiers_restored: vec!["fetch_user_url", "fetch_any_url"],
            })
            .expect("record");

        assert_eq!(
            ledger.all_web_overrides().expect("read"),
            vec![(
                "sess-web".to_owned(),
                vec!["fetch_user_url".to_owned(), "fetch_any_url".to_owned()]
            )]
        );

        // An empty restore list reads back as empty rather than as one blank
        // tier — the degenerate split a naive `split(',')` would produce.
        ledger
            .record_web_override(&WebOverrideRow {
                session_id: "sess-none".to_owned(),
                tiers_restored: Vec::new(),
            })
            .expect("record");
        assert_eq!(
            ledger.all_web_overrides().expect("read")[1].1,
            Vec::<String>::new()
        );

        let guard = ledger.conn.lock().unwrap();
        assert!(
            guard
                .execute("UPDATE web_overrides SET session_id = 'x'", [])
                .is_err(),
            "UPDATE must be rejected by the append-only trigger"
        );
        assert!(
            guard.execute("DELETE FROM web_overrides", []).is_err(),
            "DELETE must be rejected by the append-only trigger"
        );
    }

    /// The override table holds no column that could carry an utterance either
    /// — the same rule `web_lookups` is held to, asserted the same way.
    #[test]
    fn the_web_overrides_table_has_no_column_that_could_hold_an_utterance() {
        let (ledger, _sink) = ledger();
        let guard = ledger.conn.lock().unwrap();
        let mut stmt = guard
            .prepare("SELECT name FROM pragma_table_info('web_overrides') ORDER BY name")
            .expect("prepare");
        let columns: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .expect("query")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect");
        assert_eq!(
            columns,
            vec!["id", "recorded_at_ms", "session_id", "tiers_restored"],
            "the override schema gained a column: {columns:?}"
        );
    }

    /// A `cost.db` written before `web_overrides` existed gains the table on
    /// open, for the reason `web_lookups` does: `CREATE TABLE IF NOT EXISTS`
    /// reaches an existing file, so a new *table* needs no `ADDITIVE_COLUMNS`
    /// entry.
    #[test]
    fn a_ledger_written_before_web_overrides_gains_the_table_on_open() {
        let path = scratch_db("pre-overrides");
        let _ = std::fs::remove_file(&path);
        {
            let old = Connection::open(&path).expect("create pre-REQ-563 ledger");
            old.execute_batch(PRE_REQ_SCHEMA).expect("pre-REQ schema");
        }
        let ledger = CostLedger::open(
            &path,
            PriceTable::bundled(),
            Arc::new(CapturingSink::default()),
        )
        .expect("open an older ledger");
        assert!(ledger.all_web_overrides().expect("read").is_empty());
        ledger
            .record_web_override(&WebOverrideRow {
                session_id: "s".to_owned(),
                tiers_restored: vec!["search"],
            })
            .expect("append after the table arrives");
        assert_eq!(ledger.all_web_overrides().expect("read").len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    /// BR-9: a local turn is a **usage** record, not a spend record.
    ///
    /// `usd_micros` must be `None` (unpriced), never `Some(0)`. The difference
    /// is not cosmetic: `Some(0)` tells the meter a call was priced and cost
    /// nothing, which would quietly report every local turn as free spend
    /// rather than as spend that was never priced at all.
    #[test]
    fn a_local_turn_is_recorded_unpriced_with_its_cached_token_count() {
        let (ledger, _sink) = ledger();
        ledger
            .record_local_call(
                "s1",
                &CostAttribution::new("qwen2.5-coder-3b"),
                16_000,
                120,
                15_872,
            )
            .expect("record a local turn");

        let rows = ledger.all_records().expect("read");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].provider_id, "local");
        assert_eq!(rows[0].model, "qwen2.5-coder-3b");
        assert_eq!(
            rows[0].usd_micros, None,
            "a local model is absent from the price table, so the row is \
             unpriced — not priced at zero"
        );
        assert_eq!(rows[0].cached_tokens, Some(15_872));
        assert_eq!(
            rows[0].input_tokens, 16_000,
            "cached tokens are a COMPONENT of the prompt, never a deduction \
             from it"
        );
    }

    /// A remote row carries `None`, not `Some(0)`: a remote provider has no
    /// local KV, so "no cache exists" is the fact — reporting zero reused
    /// tokens would imply one existed and served nothing.
    #[test]
    fn a_remote_row_has_no_cached_token_count_at_all() {
        let (ledger, _sink) = ledger();
        ledger
            .record_call(
                "s1",
                "anthropic",
                &CostAttribution::new("claude-fable-5"),
                10,
                20,
            )
            .expect("record a remote call");
        assert_eq!(ledger.all_records().expect("read")[0].cached_tokens, None);
    }

    /// REQ-581 BR-5: a `cost.db` written before the connection test existed
    /// gains the `probe` column on the next open, keeps every historical row as
    /// it was, and is still append-only afterwards.
    ///
    /// The `cached_tokens` precedent in full: the migration is DDL, so it
    /// neither reads nor rewrites a row, and the `ALTER TABLE` cannot have
    /// dropped the trigger that makes that guarantee enforceable rather than
    /// merely intended.
    #[test]
    fn a_pre_req_ledger_gains_the_probe_column_and_stays_append_only() {
        let path = scratch_db("pre-probe");
        let _ = std::fs::remove_file(&path);
        {
            let old = Connection::open(&path).expect("create pre-REQ-581 ledger");
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
            .expect("open a pre-REQ-581 file");

        {
            let guard = ledger.conn.lock().unwrap();
            assert!(
                has_column(&guard, "probe").expect("pragma"),
                "the migration must add the column an older file lacks"
            );
            // The append-only trigger survived the ALTER TABLE.
            assert!(
                guard
                    .execute("UPDATE cost_records SET model = 'x'", [])
                    .is_err(),
                "UPDATE must still be rejected after the migration"
            );
        }

        let rows = ledger.all_records().expect("read");
        assert_eq!(rows.len(), 1, "the migration must not drop or rewrite rows");
        assert!(!rows[0].probe, "a historical row is not a probe");
        assert_eq!(rows[0].input_tokens, 900);
        assert_eq!(rows[0].usd_micros, Some(42));
        assert_eq!(
            ledger.report().expect("report").probe_calls,
            0,
            "a ledger that predates the concept reports no probes"
        );

        // Re-opening is idempotent: the migration sees the column and does
        // nothing rather than failing on a duplicate ADD COLUMN.
        let reopened = CostLedger::open(&path, PriceTable::bundled(), sink)
            .expect("re-open a migrated ledger");
        assert_eq!(reopened.all_records().expect("read").len(), 1);
        {
            let guard = reopened.conn.lock().unwrap();
            assert!(has_column(&guard, "probe").expect("pragma"));
        }

        let _ = std::fs::remove_file(&path);
    }

    /// REQ-581 BR-5: a probe row round-trips as a probe, a turn round-trips as a
    /// turn, and the report counts only the first — while both stay in the call
    /// total, because a connection test really did spend.
    #[test]
    fn a_probe_row_round_trips_and_the_report_counts_only_probes() {
        let (ledger, sink) = ledger();
        ledger
            .record_call(
                "s1",
                "anthropic",
                &CostAttribution::new("claude-fable-5").with_category(Category::Review),
                1000,
                500,
            )
            .expect("record a turn");
        ledger
            .record_call("s1", "kimi", &CostAttribution::new("kimi-k2").probe(), 8, 4)
            .expect("record a probe");

        let rows = ledger.all_records().expect("read");
        assert_eq!(rows.len(), 2);
        assert!(!rows[0].probe, "a turn is stored as a turn");
        assert!(rows[1].probe, "and the probe reads back as one");
        assert_eq!(
            rows[1].input_tokens, 8,
            "a probe is an ordinary row: it carries its real token counts"
        );

        // The flag reaches the client on the live event too, not only the store.
        let emitted = sink.records.lock().unwrap();
        assert!(!emitted[0].probe);
        assert!(emitted[1].probe);
        drop(emitted);

        let report = ledger.report().expect("report");
        assert_eq!(report.probe_calls, 1, "only the probe is counted as one");
        assert_eq!(
            report.total.calls, 2,
            "and it is still a call — the count is a subset, not a deduction"
        );
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
        let response = TransportResponse {
            status: 200,
            location: None,
            body,
        };
        let metered = CostMeter::meter_response(
            ledger.as_ref(),
            response,
            Some(SessionId::from("sess-under-test")),
            ProviderId::from("anthropic"),
            CostAttribution::new("claude-fable-5").with_phase(Phase::Implement),
            None,
        );
        let bytes = drain(metered.body).await;
        // Body passed through unchanged.
        assert!(bytes.windows(13).any(|w| w == b"message_start"));

        let rows = ledger.all_records().expect("read");
        assert_eq!(rows.len(), 1, "exactly one CostRecord per completed call");
        assert_eq!(rows[0].input_tokens, 1200);
        assert_eq!(rows[0].output_tokens, 340);
        assert_eq!(rows[0].phase, Some(Phase::Implement));
        assert_eq!(rows[0].session_id, "sess-under-test");
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
        let response = TransportResponse {
            status: 200,
            location: None,
            body,
        };
        let metered = CostMeter::meter_response(
            ledger.as_ref(),
            response,
            Some(SessionId::from("s")),
            ProviderId::from("deepseek"),
            CostAttribution::new("deepseek-chat"),
            None,
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
        let response = TransportResponse {
            status: 200,
            location: None,
            body,
        };
        let metered = CostMeter::meter_response(
            ledger.as_ref(),
            response,
            None,
            ProviderId::from("anthropic"),
            CostAttribution::new("claude-fable-5"),
            None,
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
        let response = TransportResponse {
            status: 200,
            location: None,
            body,
        };
        let metered = CostMeter::meter_response(
            ledger.as_ref(),
            response,
            Some(SessionId::from("sess-stalled")),
            ProviderId::from("anthropic"),
            CostAttribution::new("claude-fable-5").with_category(Category::Title),
            None,
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

    /// REQ-559 found the hole in `polled`'s reasoning. That flag was a **proxy**
    /// for "the provider refused this on its status, so nobody read the body" —
    /// true when it was written, and no longer true: BR-12's refusal
    /// classification reads a bounded prefix of a 400 body to decide whether the
    /// provider is rejecting the effort field. Polling it would flip `polled` and
    /// bill a 0-token, $0 row for every refused request, inflating
    /// `CostReport::calls` with calls that bought nothing.
    ///
    /// The guard now keys on the thing it always meant — the status — so reading
    /// an error body to classify it cannot invent a row. LESSON-443's shape: a
    /// guard keyed on a condition that stops holding once a feature lands.
    #[tokio::test]
    async fn reading_a_4xx_body_to_classify_it_still_bills_nothing() {
        let (ledger, sink) = ledger();
        let ledger = Arc::new(ledger);
        let body = body_from(vec![
            "{\"error\":{\"message\":\"Unrecognized request argument supplied: reasoning_effort\"}}",
        ]);
        let response = TransportResponse {
            status: 400,
            location: None,
            body,
        };
        let metered = CostMeter::meter_response(
            ledger.as_ref(),
            response,
            Some(SessionId::from("s")),
            ProviderId::from("mystery"),
            CostAttribution::new("mystery-1"),
            None,
        );
        // Exactly what REQ-559's `classify_client_error` does: read the body.
        drain(metered.body).await;
        assert!(
            ledger.all_records().expect("read").is_empty(),
            "a refused request must not be billed, even once its body is read",
        );
        assert!(sink.records.lock().unwrap().is_empty());
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
        let response = TransportResponse {
            status: 429,
            location: None,
            body,
        };
        let metered = CostMeter::meter_response(
            ledger.as_ref(),
            response,
            Some(SessionId::from("sess-under-test")),
            ProviderId::from("anthropic"),
            CostAttribution::new("claude-fable-5"),
            None,
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
        let response = TransportResponse {
            status: 200,
            location: None,
            body,
        };
        let metered = CostMeter::meter_response(
            ledger.as_ref(),
            response,
            Some(SessionId::from("sess-once")),
            ProviderId::from("deepseek"),
            CostAttribution::new("deepseek-chat"),
            None,
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
                output: 0,
                reasoning: None,
            }
        );
    }

    // ---- REQ-559: reasoning-token attribution (BR-10, AC-9) ----------------

    /// The load-bearing half of BR-10: parsing the split must not move the
    /// totals. Reasoning tokens are **already inside** `completion_tokens`, so
    /// this is an attribution change and not a totals change — the same bytes
    /// must produce the same `output_tokens` they produced before the field
    /// existed.
    #[test]
    fn reading_the_reasoning_split_does_not_move_the_totals() {
        let with_details = b"\"prompt_tokens\": 80, \"completion_tokens\": 42, \
            \"completion_tokens_details\": {\"reasoning_tokens\": 30}";
        let without = b"\"prompt_tokens\": 80, \"completion_tokens\": 42";

        let mut a = UsageScan::default();
        a.feed(with_details);
        let mut b = UsageScan::default();
        b.feed(without);

        assert_eq!(a.usage().input, b.usage().input);
        assert_eq!(
            a.usage().output,
            b.usage().output,
            "the reasoning split must not change the output total (BR-10)",
        );
        assert_eq!(a.usage().output, 42);
        assert_eq!(a.usage().reasoning, Some(30));
        // Unreported is `None`, never `0` — a provider that told us nothing is
        // not a provider that reported zero thinking (BR-10, BR-11).
        assert_eq!(b.usage().reasoning, None);
        assert!(
            a.usage().reasoning.unwrap() <= a.usage().output,
            "reasoning is a subset of output, never an addition",
        );
    }

    /// `"completion_tokens_details"` contains `completion_tokens` as a prefix.
    /// The trailing quote in `OUTPUT_KEYS` is what stops the scanner reading the
    /// nested object's first integer as the output total — a one-character
    /// invariant worth its own test.
    #[test]
    fn the_details_object_does_not_capture_the_output_total() {
        let mut scan = UsageScan::default();
        // The details object comes LAST and holds a smaller number, so a scanner
        // that matched the prefix would report 30 instead of 42.
        scan.feed(
            b"\"completion_tokens\": 42, \"completion_tokens_details\": {\"reasoning_tokens\": 30}",
        );
        assert_eq!(scan.usage().output, 42);
        assert_eq!(scan.usage().reasoning, Some(30));
    }

    /// The key plus its integer is well under `CARRY_BYTES`, so a split across a
    /// chunk boundary is still matched — the same guarantee the totals have.
    #[test]
    fn the_reasoning_split_survives_a_chunk_boundary() {
        let mut scan = UsageScan::default();
        scan.feed(b"data: {\"usage\":{\"completion_tokens\":42,\"completion_tokens_deta");
        scan.feed(b"ils\":{\"reasoning_toke");
        scan.feed(b"ns\":30}}}\n\n");
        assert_eq!(scan.usage().reasoning, Some(30));
        assert_eq!(scan.usage().output, 42);
    }

    /// AC-9 end to end through the metering seam: a response carrying the field
    /// produces a row whose `reasoning_tokens` is that value and whose
    /// `output_tokens` is unchanged.
    #[tokio::test]
    async fn a_metered_stream_records_the_reasoning_split() {
        let (ledger, _sink) = ledger();
        let ledger = Arc::new(ledger);
        let body = body_from(vec![
            "data: {\"choices\":[]}\n\ndata: {\"usage\":{\"prompt_tokens\":80,\"completion_tokens\":42,",
            "\"completion_tokens_details\":{\"reasoning_tokens\":30}}}\n\ndata: [DONE]\n\n",
        ]);
        let response = TransportResponse {
            status: 200,
            location: None,
            body,
        };
        let metered = CostMeter::meter_response(
            ledger.as_ref(),
            response,
            Some(SessionId::from("s")),
            ProviderId::from("deepseek"),
            CostAttribution::new("deepseek-chat"),
            None,
        );
        drain(metered.body).await;
        let rows = ledger.all_records().expect("read");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].output_tokens, 42, "totals unchanged (BR-10)");
        assert_eq!(rows[0].reasoning_tokens, Some(30));
    }

    /// And a response without the field records `None`, which is what
    /// `teton cost` renders as "unreported" rather than as `0`.
    #[tokio::test]
    async fn an_unreported_split_is_recorded_as_none_not_zero() {
        let (ledger, _sink) = ledger();
        let ledger = Arc::new(ledger);
        let body = body_from(vec![
            "data: {\"usage\":{\"prompt_tokens\":80,\"completion_tokens\":42}}\n\ndata: [DONE]\n\n",
        ]);
        let response = TransportResponse {
            status: 200,
            location: None,
            body,
        };
        let metered = CostMeter::meter_response(
            ledger.as_ref(),
            response,
            Some(SessionId::from("s")),
            ProviderId::from("anthropic"),
            CostAttribution::new("claude-opus-5"),
            None,
        );
        drain(metered.body).await;
        let rows = ledger.all_records().expect("read");
        assert_eq!(rows[0].reasoning_tokens, None);
    }
}
