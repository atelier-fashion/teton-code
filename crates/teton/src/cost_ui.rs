//! The cost meter (AC-4, BR-2).
//!
//! Two responsibilities, both derived **only** from the daemon's authoritative
//! numbers (BR-2: no estimated or unattributed spend is ever shown as actual):
//!
//! 1. [`CostMeter`] observes the [`CostRecord`]s that stream on the
//!    `cost_recorded` event during a live session, purely to count what happened
//!    this session (e.g. "recorded N model call(s)"). It performs no repricing.
//! 2. [`render_report_view`] renders the daemon's authoritative
//!    [`CostReportView`] — totals, the per-phase / per-provider roll-ups, and the
//!    savings-vs-frontier estimate with its methodology line (AC-4).
//!
//! ## Single source of truth for the savings estimate (REQ-544 M-7)
//!
//! The savings methodology (OQ-6) and its baseline frontier price live in the
//! daemon (`tetond::cost`), which computes the estimate over its persisted ledger
//! and projects it onto [`CostReportView`]. The CLI does **not** carry a baseline
//! price of its own and does **not** reprice anything — it renders the daemon's
//! `cost/query` report directly, so there is exactly one place a savings number is
//! computed. (The previous client-side `Baseline` literal — hand-synced to
//! `prices.toml` — was a second source of truth and has been removed.)

use teton_protocol::events::CostRecord;
use teton_protocol::methods::CostReportView;

use crate::render::{LineKind, Surface};

/// Accumulates the [`CostRecord`]s observed on the live event stream during a
/// session, so the CLI can report how many model calls happened this session.
///
/// It deliberately does **not** aggregate spend or estimate savings: the
/// authoritative totals and the savings figure come from the daemon's
/// `cost/query` report ([`render_report_view`]). This type only counts.
#[derive(Debug, Default)]
pub struct CostMeter {
    records: Vec<CostRecord>,
}

impl CostMeter {
    /// Ingest one recorded model call observed on the event stream.
    pub fn record(&mut self, record: CostRecord) {
        self.records.push(record);
    }

    /// How many records have been observed this session.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// True when no records have been observed this session.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

/// Format integer micro-USD as an exact dollar string, e.g. `525000` →
/// `$0.525000`. No float rounding — the six fractional digits are the micro part.
#[must_use]
pub fn format_usd(micros: i64) -> String {
    let negative = micros < 0;
    let magnitude = micros.unsigned_abs();
    let dollars = magnitude / 1_000_000;
    let fraction = magnitude % 1_000_000;
    let sign = if negative { "-" } else { "" };
    format!("{sign}${dollars}.{fraction:06}")
}

/// Render the daemon's authoritative [`CostReportView`] to a surface: total,
/// priced/unpriced call counts, the per-phase and per-provider tables, then the
/// labeled savings estimate with its methodology line (AC-4).
///
/// Every figure here — including the baseline and savings — comes verbatim from
/// the daemon's `cost/query` report; the CLI computes none of it (REQ-544 M-7).
pub fn render_report_view(report: &CostReportView, surface: &mut dyn Surface) {
    surface.line(LineKind::Cost, "── cost summary ──");
    surface.line(
        LineKind::Cost,
        &format!(
            "total: {} over {} call(s)",
            format_usd(report.total_usd_micros),
            report.total_calls
        ),
    );
    // REQ-559 BR-11: effort is a dial, and this is its gauge. Rendered next to
    // the totals it is a subset of, so it is never read as an addition to them.
    render_reasoning_split(
        surface,
        report.reasoning_tokens,
        report.calls_reporting_reasoning,
        report.total_calls,
    );
    if report.unpriced_calls > 0 {
        surface.line(
            LineKind::Cost,
            &format!(
                "  ({} priced, {} unpriced — an unpriced call is recorded but never assigned a guessed cost)",
                report.priced_calls, report.unpriced_calls
            ),
        );
        // REQ-557 AC-7b: name the models, and say what to do about them. A count
        // alone tells a user something went uncosted but not what to price, which
        // left them reading config or logs to find out.
        if !report.unpriced_models.is_empty() {
            surface.line(
                LineKind::Cost,
                &format!(
                    "  no price on file for: {} — add an entry for each to the price table to \
                     see its spend",
                    report.unpriced_models.join(", ")
                ),
            );
        }
    }

    render_probes(surface, report.probe_calls);

    render_group(surface, "per phase", &report.per_phase);
    render_group(surface, "per provider", &report.per_provider);
    render_web_lookups(surface, &report.web_per_session);

    surface.line(
        LineKind::Cost,
        &format!(
            "estimated savings vs {}: {} (baseline {} − actual {})",
            report.baseline_model,
            format_usd(report.savings_usd_micros),
            format_usd(report.baseline_usd_micros),
            format_usd(report.total_usd_micros),
        ),
    );
    // The methodology line is mandatory (AC-4): it labels the figure an estimate
    // and states exactly how the daemon computed it.
    surface.line(
        LineKind::Cost,
        &format!("  (estimate) {}", report.methodology),
    );
}

/// Render one named roll-up group (per-phase or per-provider) from the daemon's
/// report, or a "(no attributed calls)" note when the group is empty.
fn render_group(
    surface: &mut dyn Surface,
    label: &str,
    groups: &[teton_protocol::methods::CostGroupView],
) {
    if groups.is_empty() {
        surface.line(LineKind::Cost, &format!("{label}: (no attributed calls)"));
        return;
    }
    surface.line(LineKind::Cost, &format!("{label}:"));
    for row in groups {
        surface.line(
            LineKind::Cost,
            &format!(
                "  {:<12} {:>4} call(s)  {:>7} in / {:>7} out  {}",
                row.key,
                row.calls,
                row.input_tokens,
                row.output_tokens,
                format_usd(row.usd_micros),
            ),
        );
    }
}

/// Render the connection-test count, when there is one (REQ-581 BR-5).
///
/// A **subset** of the totals above and drawn immediately under them for that
/// reason: a probe is an ordinary model call, sent down the same path and priced
/// from the same table, so the daemon counts it in `total_calls` and this line
/// only says how many of those calls nobody asked a question for. It is never a
/// second tally to add on.
///
/// Silent at zero, like [`render_web_lookups`] and unlike [`render_group`]: a
/// user who has never run `/provider test` would otherwise carry a permanent
/// `probes: 0` on the surface they read for their spending — and `0` is also
/// what a daemon predating the field reports by default, so a line here would be
/// asserting something about a build that cannot answer.
fn render_probes(surface: &mut dyn Surface, probe_calls: u64) {
    if probe_calls == 0 {
        return;
    }
    surface.line(
        LineKind::Cost,
        &format!(
            "  probes: {probe_calls} connection test(s) — billed like any call, counted apart"
        ),
    );
}

/// Render the reasoning-token split (REQ-559 BR-11 / AC-9).
///
/// Says "unreported" where the providers reported nothing, and says how much of
/// the ledger the figure covers where they reported some. It never prints `0`
/// for an unreported split, and never scales a partial figure up to look like a
/// whole-ledger total — both would be displaying an estimate as an actual, which
/// REQ-544 BR-2 forbids for exactly this reason.
pub(crate) fn render_reasoning_split(
    surface: &mut dyn Surface,
    reasoning_tokens: Option<u64>,
    calls_reporting: u64,
    total_calls: u64,
) {
    let Some(reasoning) = reasoning_tokens else {
        surface.line(
            LineKind::Cost,
            "  thinking:    unreported (no provider reported a reasoning split)",
        );
        return;
    };
    let scope = if calls_reporting >= total_calls {
        String::new()
    } else {
        // The rest are unreported, NOT zero — say which, or the number reads as
        // a total.
        format!(" (across {calls_reporting} of {total_calls} calls; the rest unreported)")
    };
    surface.line(
        LineKind::Cost,
        &format!("  thinking:    {reasoning} output token(s){scope}"),
    );
}

/// Render the per-session web-lookup roll-up (REQ-563 BR-7 / AC-6).
///
/// Its own section rather than a column on the call tables, mirroring the
/// separate ledger table it comes from: calls and lookups are different counts,
/// and a reader must not see them summed. Every lookup is here whatever its
/// ending — including the cache hits and the refusals, which cost nothing and
/// are exactly what BR-7's "including zero-cost ones" is about.
///
/// Silent when empty, unlike [`render_group`]'s "(no attributed calls)". Web
/// lookup is off by default (BR-1), so on almost every machine this list is
/// empty forever; a permanent "(none)" line would be noise on a surface people
/// read for their spending. The section appearing at all is itself information.
fn render_web_lookups(
    surface: &mut dyn Surface,
    groups: &[teton_protocol::methods::WebTotalsView],
) {
    if groups.is_empty() {
        return;
    }
    surface.line(LineKind::Cost, "web lookups:");
    for row in groups {
        surface.line(
            LineKind::Cost,
            &format!(
                "  {:<12} {:>4} lookup(s)  {:>9} bytes in",
                row.key, row.lookups, row.bytes_in,
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::RecordingSurface;
    use teton_protocol::methods::{CostGroupView, WebTotalsView};
    use teton_protocol::{ProviderId, SessionId};

    fn record(phase: Option<teton_protocol::Phase>, usd_micros: i64) -> CostRecord {
        CostRecord {
            session_id: SessionId::from("s1"),
            phase,
            category: None,
            provider_id: ProviderId::from("deepseek"),
            model: "coder".to_owned(),
            input_tokens: 100,
            output_tokens: 50,
            usd_micros,
            cached_tokens: None,
            reasoning_tokens: None,
            probe: false,
        }
    }

    #[test]
    fn format_usd_is_exact_and_handles_sign() {
        assert_eq!(format_usd(525_000), "$0.525000");
        assert_eq!(format_usd(0), "$0.000000");
        assert_eq!(format_usd(1_500_000), "$1.500000");
        assert_eq!(format_usd(-42), "-$0.000042");
    }

    #[test]
    fn cost_meter_only_counts_observed_records() {
        let mut meter = CostMeter::default();
        assert!(meter.is_empty());
        meter.record(record(Some(teton_protocol::Phase::Implement), 3_000));
        meter.record(record(None, 100));
        assert_eq!(meter.len(), 2);
        assert!(!meter.is_empty());
    }

    #[test]
    fn render_report_view_prints_daemon_totals_savings_and_methodology() {
        // The CLI renders the daemon's authoritative report verbatim — it does not
        // reprice or recompute the savings (REQ-544 M-7).
        let report = CostReportView {
            total_usd_micros: 3_000,
            total_calls: 2,
            priced_calls: 1,
            unpriced_calls: 1,
            unpriced_models: vec!["llama-3-70b".to_owned()],
            savings_usd_micros: 522_000,
            baseline_usd_micros: 525_000,
            baseline_model: "anthropic/claude-opus-4".to_owned(),
            methodology: "Estimate, not a measurement. Repriced at the baseline.".to_owned(),
            per_phase: vec![CostGroupView {
                key: "implement".to_owned(),
                calls: 1,
                input_tokens: 4_000,
                output_tokens: 2_000,
                usd_micros: 3_000,
            }],
            per_provider: vec![CostGroupView {
                key: "deepseek".to_owned(),
                calls: 1,
                input_tokens: 4_000,
                output_tokens: 2_000,
                usd_micros: 3_000,
            }],
            web_per_session: Vec::new(),
            reasoning_tokens: None,
            calls_reporting_reasoning: 0,
            probe_calls: 0,
        };

        let mut surface = RecordingSurface::new();
        render_report_view(&report, &mut surface);

        assert!(surface.any_line_contains(LineKind::Cost, "total:"));
        assert!(surface.any_line_contains(LineKind::Cost, "implement"));
        assert!(surface.any_line_contains(LineKind::Cost, "deepseek"));
        assert!(surface.any_line_contains(LineKind::Cost, "unpriced"));
        assert!(surface.any_line_contains(LineKind::Cost, "estimated savings"));
        assert!(surface.any_line_contains(LineKind::Cost, "(estimate)"));
        assert!(surface.any_line_contains(LineKind::Cost, "not a measurement"));
        // The baseline model and savings come from the daemon's report, verbatim.
        assert!(surface.any_line_contains(LineKind::Cost, "anthropic/claude-opus-4"));
        assert!(surface.any_line_contains(LineKind::Cost, "$0.522000"));
    }

    /// REQ-557 AC-7b: `teton cost` names every unpriced model and says what to do
    /// about it. A count alone ("1 unpriced") tells the user something went
    /// uncosted but not what to price.
    #[test]
    fn unpriced_models_are_named_with_the_remedy() {
        let report = CostReportView {
            total_usd_micros: 3_000,
            total_calls: 3,
            priced_calls: 1,
            unpriced_calls: 2,
            unpriced_models: vec!["llama-3-70b".to_owned(), "mistral-large".to_owned()],
            savings_usd_micros: 0,
            baseline_usd_micros: 3_000,
            baseline_model: "anthropic/claude-opus-4".to_owned(),
            methodology: "Estimate, not a measurement.".to_owned(),
            per_phase: Vec::new(),
            per_provider: Vec::new(),
            web_per_session: Vec::new(),
            reasoning_tokens: None,
            calls_reporting_reasoning: 0,
            probe_calls: 0,
        };

        let mut surface = RecordingSurface::new();
        render_report_view(&report, &mut surface);

        assert!(
            surface.any_line_contains(LineKind::Cost, "llama-3-70b"),
            "every unpriced model must be named"
        );
        assert!(
            surface.any_line_contains(LineKind::Cost, "mistral-large"),
            "every unpriced model must be named, not just the first"
        );
        assert!(
            surface.any_line_contains(LineKind::Cost, "price table"),
            "the rendering must state the remedy, not merely that something was \
             unpriced"
        );
    }

    /// The unpriced line is absent when there is nothing to report — an empty
    /// "no price on file for:" would be noise on a fully-priced session.
    #[test]
    fn a_fully_priced_report_says_nothing_about_unpriced_models() {
        let report = CostReportView {
            total_usd_micros: 3_000,
            total_calls: 1,
            priced_calls: 1,
            unpriced_calls: 0,
            unpriced_models: Vec::new(),
            savings_usd_micros: 0,
            baseline_usd_micros: 3_000,
            baseline_model: "anthropic/claude-opus-4".to_owned(),
            methodology: "Estimate, not a measurement.".to_owned(),
            per_phase: Vec::new(),
            per_provider: Vec::new(),
            web_per_session: Vec::new(),
            reasoning_tokens: None,
            calls_reporting_reasoning: 0,
            probe_calls: 0,
        };

        let mut surface = RecordingSurface::new();
        render_report_view(&report, &mut surface);

        assert!(!surface.any_line_contains(LineKind::Cost, "no price on file"));
        assert!(!surface.any_line_contains(LineKind::Cost, "unpriced"));
    }

    /// REQ-563 AC-6: a session with recorded lookups shows the count and the
    /// bytes. Both numbers, because "3 lookups" without a size says nothing
    /// about what came back and a size without a count says nothing about how
    /// often the machine talked to the network.
    #[test]
    fn web_lookups_render_with_their_count_and_bytes() {
        let report = CostReportView {
            web_per_session: vec![
                WebTotalsView {
                    key: "sess-alpha".to_owned(),
                    lookups: 3,
                    bytes_in: 12_345,
                },
                WebTotalsView {
                    key: "sess-beta".to_owned(),
                    lookups: 1,
                    bytes_in: 0,
                },
            ],
            ..fully_priced()
        };

        let mut surface = RecordingSurface::new();
        render_report_view(&report, &mut surface);

        assert!(surface.any_line_contains(LineKind::Cost, "web lookups:"));
        assert!(surface.any_line_contains(LineKind::Cost, "sess-alpha"));
        assert!(surface.any_line_contains(LineKind::Cost, "3 lookup(s)"));
        assert!(surface.any_line_contains(LineKind::Cost, "12345 bytes in"));
        // Every session with a row appears, not just the first.
        assert!(surface.any_line_contains(LineKind::Cost, "sess-beta"));
        // A lookup that brought nothing back is still a lookup (BR-7).
        assert!(surface.any_line_contains(LineKind::Cost, "1 lookup(s)"));
        // Lookups are never folded into the call counts: they are different
        // things and a reader must not see them summed.
        assert!(!surface.any_line_contains(LineKind::Cost, "3 call(s)"));
    }

    /// Web lookup is off by default (BR-1), so on almost every machine this
    /// section would be a permanent empty heading on a surface people read for
    /// their spending. It is simply absent instead.
    #[test]
    fn a_report_with_no_lookups_says_nothing_about_the_web() {
        let mut surface = RecordingSurface::new();
        render_report_view(&fully_priced(), &mut surface);
        assert!(!surface.any_line_contains(LineKind::Cost, "web lookups"));
        // …unlike the call roll-ups, which do announce themselves when empty.
        assert!(surface.any_line_contains(LineKind::Cost, "per phase:"));
    }

    /// A report with nothing else in it, so the web assertions above are about
    /// the web lines and not about whatever else happened to be rendered.
    fn fully_priced() -> CostReportView {
        CostReportView {
            total_usd_micros: 3_000,
            total_calls: 1,
            priced_calls: 1,
            unpriced_calls: 0,
            unpriced_models: Vec::new(),
            savings_usd_micros: 0,
            baseline_usd_micros: 3_000,
            baseline_model: "anthropic/claude-opus-4".to_owned(),
            methodology: "Estimate, not a measurement.".to_owned(),
            per_phase: vec![CostGroupView {
                key: "implement".to_owned(),
                calls: 1,
                input_tokens: 10,
                output_tokens: 10,
                usd_micros: 3_000,
            }],
            per_provider: Vec::new(),
            web_per_session: Vec::new(),
            reasoning_tokens: None,
            calls_reporting_reasoning: 0,
            probe_calls: 0,
        }
    }

    // ---- REQ-581: the probe count (BR-5) ----------------------------------

    /// BR-5: a connection test is a model call and is billed as one, so it is
    /// already inside `total_calls`. What the probe line adds is the sentence
    /// that keeps a user from reading a call they never asked a question for as
    /// a turn — and it must say the call *was* billed, not that it sits outside
    /// the total.
    #[test]
    fn a_report_with_probes_counts_them_apart_without_adding_them_on() {
        let report = CostReportView {
            probe_calls: 1,
            ..fully_priced()
        };

        let mut surface = RecordingSurface::new();
        render_report_view(&report, &mut surface);

        assert!(
            surface.any_line_contains(LineKind::Cost, "probes: 1 connection test(s)"),
            "the probe count must be named: {:?}",
            surface.lines_of(LineKind::Cost)
        );
        // The two halves of BR-5 in one line: billed like any call, and counted
        // apart from the turns.
        assert!(surface.any_line_contains(LineKind::Cost, "billed like any call"));
        assert!(surface.any_line_contains(LineKind::Cost, "counted apart"));
        // Non-vacuity for the "subset, not an addition" claim: the total still
        // reads as the daemon reported it, with no probe arithmetic applied.
        assert!(surface.any_line_contains(LineKind::Cost, "over 1 call(s)"));
    }

    /// Almost every machine has never run `/provider test`, and `0` is also what
    /// a daemon predating the field reports by default — so the line is absent
    /// rather than reporting a zero about a build that cannot answer.
    #[test]
    fn a_report_with_no_probes_says_nothing_about_them() {
        let mut surface = RecordingSurface::new();
        render_report_view(&fully_priced(), &mut surface);
        assert!(!surface.any_line_contains(LineKind::Cost, "probes"));
        assert!(!surface.any_line_contains(LineKind::Cost, "connection test"));
    }

    // ---- REQ-559: the thinking split (BR-11, AC-9) ------------------------

    /// BR-11: a `0` standing in for "the provider didn't tell us" is displaying
    /// an estimate as an actual (REQ-544 BR-2). The word is what distinguishes
    /// them, so the word is what is asserted.
    #[test]
    fn an_unreported_split_renders_the_word_never_a_zero() {
        let mut surface = RecordingSurface::new();
        render_reasoning_split(&mut surface, None, 0, 7);
        let line = surface.lines_of(LineKind::Cost).join("\n");
        assert!(line.contains("unreported"), "{line}");
        assert!(
            !line.contains(" 0 "),
            "an unreported split must not be rendered as zero: {line}",
        );
    }

    /// A split that covers only part of the ledger says so. Without the scope
    /// clause the number reads as a whole-ledger total, which is the same
    /// estimate-as-actual failure one step subtler.
    #[test]
    fn a_partial_split_says_how_much_of_the_ledger_it_covers() {
        let mut surface = RecordingSurface::new();
        render_reasoning_split(&mut surface, Some(1_200), 2, 7);
        let line = surface.lines_of(LineKind::Cost).join("\n");
        assert!(line.contains("1200"), "{line}");
        assert!(line.contains("2 of 7"), "{line}");
        assert!(line.contains("unreported"), "{line}");
    }

    /// When every call reported, the figure is a total and says nothing extra.
    #[test]
    fn a_complete_split_renders_without_a_scope_caveat() {
        let mut surface = RecordingSurface::new();
        render_reasoning_split(&mut surface, Some(900), 3, 3);
        let line = surface.lines_of(LineKind::Cost).join("\n");
        assert!(line.contains("900"), "{line}");
        assert!(!line.contains("unreported"), "{line}");
    }
}
