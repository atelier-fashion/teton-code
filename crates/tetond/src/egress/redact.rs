//! Redaction: the verdict data model, the deterministic credential pattern
//! pass, and the pure forward/block decision (REQ-562, ADR-4/ADR-5/ADR-6).
//!
//! This module is the **foundation** the redaction duty (`harness::redact`) and
//! the egress gate hook (`egress::Egress::send`) consume. Every line of *code*
//! in it is pure: no async body, no engine, no I/O, no events. That is what
//! makes AC-10's decision table exhaustively testable at unit cost, and it keeps
//! the one security-relevant rule of the feature — *which verdicts block* — in a
//! function with no collaborators to mock.
//!
//! [`RedactionGate`] is the one exception, and it is a *declaration* rather than
//! an implementation: the choke point needs a name for "the thing that produces
//! a verdict" and the verdict type lives here, so the trait lives beside it.
//! Nothing in this module implements it — `runtime::RedactionGateImpl` does,
//! where the router, the engine slot and the duty seam already are.
//!
//! ## The two passes and where confidence comes from (ADR-4, BR-4)
//!
//! Confidence is **derived structurally, never self-reported**. A 3B model's own
//! estimate of its certainty is the least trustworthy value in the pipeline, so
//! it is never consulted:
//!
//! - **Pattern pass** (this module): a deterministic sweep for five credential
//!   shapes. A hit is [`Confidence::High`] *by construction* —
//!   [`Finding::pattern`] is the only way to build one and it hard-codes the
//!   level.
//! - **Model pass** (`harness::redact`, TASK-069): a hit the model reported and
//!   the redactor could locate in the payload. It is [`Confidence::Low`] by
//!   construction — [`Finding::model`] hard-codes that level.
//!
//! There is no `Finding::new(kind, span, confidence)`: a caller cannot choose a
//! confidence level, so "a pattern hit is high and a model hit is low" is a
//! property of the constructors rather than a convention someone maintains.
//!
//! ## A finding never quotes what it matched (BR-6, ADR-5)
//!
//! [`Finding`] has **no text field at all**. Not a private one, not an
//! `Option<String>`, none. A secret detector that echoes the secret into an
//! event, a log line, or a turn-failure sentence has *moved* the secret, not
//! caught it. Because the type cannot carry the matched bytes, "a finding never
//! quotes the matched text" is structural — every downstream consumer inherits
//! it for free, and `a_finding_never_carries_the_matched_text` below turns red
//! if anyone adds one back (AC-8 mutation (c)).
//!
//! ## Fail closed, and say which failure it was (ADR-6, BR-3, BR-7)
//!
//! [`Outcome::Unavailable`] is the single "the scan could not run" state, and
//! [`decide`] maps it to [`EgressDecision::Block`]. A guard that cannot run does
//! not become a guard that passes everything. It rides with `scanned: false` so
//! the *reason* stays legible: the user is told the scan could not run, never
//! that it found something — different problems with different fixes.
//!
//! An over-cap payload ([`REDACT_INPUT_MAX_BYTES`]) is that case, **not** a
//! silent pass and **not** a truncate-and-scan: a partial scan reporting itself
//! as complete is precisely the lie BR-7 forbids. ADR-6 permits running the
//! pattern pass first on an over-cap payload so a real finding could outrank
//! "too large" as the reported reason; [`pattern_verdict`] deliberately does not
//! take that option. The terminal outcome is Block either way, and reporting
//! `Findings` (which carries `scanned: true`) would claim a completed scan of a
//! payload the model pass never saw.
//!
//! ## No regex crate
//!
//! The REQ forbids new crates and `regex` is not a declared dependency of any
//! crate in this workspace (it appears in `Cargo.lock` only transitively). The
//! five shapes are anchored on a literal prefix or a literal suffix, so they are
//! hand-rolled byte scans below. Every shape is pure ASCII, and an ASCII byte in
//! UTF-8 can never occur inside a multi-byte sequence — so the byte offsets
//! these scans produce are always valid `str` char boundaries.

use std::ops::Range;

use async_trait::async_trait;

/// The largest payload the redactor will scan, in bytes (ADR-6).
///
/// 64 KiB — roughly 32k tokens at the duty seam's 2-bytes-per-token convention,
/// which is about the most a mid-tier local model can actually scan in one call.
/// A payload above this is [`Outcome::Unavailable`] (→ Block), never truncated
/// and scanned (BR-7).
pub const REDACT_INPUT_MAX_BYTES: usize = 64 * 1024;

/// How much the pipeline trusts a finding — **derived, never self-reported**
/// (BR-4, ADR-4).
///
/// The two levels come from *which pass produced the finding*, not from any
/// judgment about the content: see [`Finding::pattern`] and [`Finding::model`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Confidence {
    /// A deterministic pattern hit. A string matching one of the five shapes
    /// essentially *is* the credential it looks like, so precision is near
    /// perfect — this level blocks.
    High,
    /// A model-only hit. Moderate recall, uncertain precision: the model catches
    /// what patterns structurally cannot, and can also invent. Reported, not
    /// blocking (BR-4) — blocking every low hit makes the feature unusable and
    /// trains users to disable it.
    Low,
}

/// What kind of sensitive content a finding identifies.
///
/// The deterministic pattern pass only ever emits [`FindingKind::Credential`] —
/// all five of its shapes are credentials. The other variants exist for the
/// model pass, which is the half of the feature that can recognize a secret
/// paraphrased into prose or PII with no fixed shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FindingKind {
    /// A secret value with no credential-specific shape.
    Secret,
    /// An API key, access key, or bearer token.
    Credential,
    /// Personally identifying information.
    Pii,
    /// Sensitive, but the pass could not classify it further.
    Unknown,
}

impl FindingKind {
    /// A stable, content-free name for reports and events.
    ///
    /// Safe to render anywhere: it names the *class* of thing found, never the
    /// thing itself (BR-6).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            FindingKind::Secret => "secret",
            FindingKind::Credential => "credential",
            FindingKind::Pii => "pii",
            FindingKind::Unknown => "unknown",
        }
    }
}

/// One located piece of sensitive content: what class it is, where it is, and
/// how much the pipeline trusts the detection.
///
/// **There is no text field on this type, by design** (BR-6, ADR-5). A finding
/// carries a byte range into the scanned payload and nothing else, so no
/// consumer — event, log, error, CLI renderer — can echo the matched bytes even
/// by accident. A model-reported string that cannot be located in the payload is
/// a fabrication and is dropped rather than reported: a finding with no span is
/// not a finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    kind: FindingKind,
    span: Range<usize>,
    confidence: Confidence,
}

impl Finding {
    /// A finding from the deterministic pattern pass: [`Confidence::High`] **by
    /// construction** (ADR-4). There is no way to build a high-confidence
    /// finding from anywhere else.
    #[must_use]
    pub fn pattern(kind: FindingKind, span: Range<usize>) -> Self {
        Self {
            kind,
            span,
            confidence: Confidence::High,
        }
    }

    /// A finding from the local-model pass: [`Confidence::Low`] **by
    /// construction** (ADR-4). The model's own certainty is never consulted, so
    /// no argument for it exists.
    #[must_use]
    pub fn model(kind: FindingKind, span: Range<usize>) -> Self {
        Self {
            kind,
            span,
            confidence: Confidence::Low,
        }
    }

    /// The class of sensitive content.
    #[must_use]
    pub fn kind(&self) -> FindingKind {
        self.kind
    }

    /// The byte range in the scanned payload. Always a valid `str` char-boundary
    /// range for the text it was derived from.
    #[must_use]
    pub fn span(&self) -> &Range<usize> {
        &self.span
    }

    /// How much the pipeline trusts this finding.
    #[must_use]
    pub fn confidence(&self) -> Confidence {
        self.confidence
    }

    /// Whether this finding is high-confidence — the predicate [`decide`] blocks
    /// on (BR-4).
    #[must_use]
    pub fn is_high(&self) -> bool {
        matches!(self.confidence, Confidence::High)
    }
}

/// The three terminal states of a redaction scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Outcome {
    /// The scan ran to completion and found nothing.
    Clean,
    /// The scan ran to completion and found at least one thing.
    Findings,
    /// The scan could **not** run (no local tier, over-cap payload, engine
    /// error, or deadline). Fails closed (ADR-6, BR-3).
    Unavailable,
}

/// The result of a redaction scan, with its invariants enforced by construction.
///
/// The spec's two invariants are not maintained by discipline — the fields are
/// private and the only three constructors ([`RedactionVerdict::clean`],
/// [`RedactionVerdict::from_findings`], [`RedactionVerdict::unavailable`]) each
/// establish both:
///
/// - `findings` is non-empty **iff** the outcome is [`Outcome::Findings`];
/// - `scanned` is `false` **iff** the outcome is [`Outcome::Unavailable`].
///
/// The second is a biconditional on purpose: `scanned: false` is exactly the
/// claim "no scan happened here", so a completed scan can never carry it and an
/// aborted one can never omit it. That is what makes AC-3's "no configuration
/// makes the scan appear to have run when it did not" checkable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactionVerdict {
    outcome: Outcome,
    findings: Vec<Finding>,
    scanned: bool,
}

impl RedactionVerdict {
    /// The scan ran and found nothing.
    #[must_use]
    pub fn clean() -> Self {
        Self {
            outcome: Outcome::Clean,
            findings: Vec::new(),
            scanned: true,
        }
    }

    /// The scan ran and produced `findings`.
    ///
    /// An **empty** `findings` collapses to [`RedactionVerdict::clean`] rather
    /// than producing an `Outcome::Findings` with nothing in it: "the scan ran
    /// and found no findings" is precisely `Clean`, and allowing the degenerate
    /// pair would break the non-empty-iff-`Findings` invariant. Composing the
    /// two passes is therefore just concatenation — the caller does not branch
    /// on emptiness.
    #[must_use]
    pub fn from_findings(findings: Vec<Finding>) -> Self {
        if findings.is_empty() {
            return Self::clean();
        }
        Self {
            outcome: Outcome::Findings,
            findings,
            scanned: true,
        }
    }

    /// The scan could not run. Carries no findings and `scanned: false`, and
    /// [`decide`] blocks on it (ADR-6, BR-3).
    #[must_use]
    pub fn unavailable() -> Self {
        Self {
            outcome: Outcome::Unavailable,
            findings: Vec::new(),
            scanned: false,
        }
    }

    /// Which terminal state the scan reached.
    #[must_use]
    pub fn outcome(&self) -> Outcome {
        self.outcome
    }

    /// The located findings — empty unless the outcome is [`Outcome::Findings`].
    #[must_use]
    pub fn findings(&self) -> &[Finding] {
        &self.findings
    }

    /// Whether a scan actually ran. `false` **only** for
    /// [`Outcome::Unavailable`], so a report can distinguish "found nothing"
    /// from "could not look" (BR-3).
    #[must_use]
    pub fn scanned(&self) -> bool {
        self.scanned
    }
}

/// What the egress choke point does with a payload after the scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EgressDecision {
    /// Hand the payload to the inner transport, **byte-for-byte unmodified**
    /// (BR-5, AC-9 — v1 detects, it never substitutes).
    Forward,
    /// Refuse the payload. The bytes are dropped, never sent, never logged.
    Block,
}

/// Map a verdict to the egress action (BR-4, ADR-4, ADR-6).
///
/// The whole rule, in one pure function:
///
/// | verdict                         | decision  | why                                |
/// |---------------------------------|-----------|------------------------------------|
/// | [`Outcome::Unavailable`]        | `Block`   | fail closed — BR-3                 |
/// | [`Outcome::Clean`]              | `Forward` | nothing found                      |
/// | `Findings` with any `High`      | `Block`   | a pattern hit essentially is a key |
/// | `Findings`, all `Low`           | `Forward` | the user's call — BR-4             |
///
/// A single high finding blocks regardless of how many low ones accompany it,
/// and the order findings arrive in is irrelevant. This is the rule that decides
/// whether the feature is usable or decorative, and AC-10 calls it the one a
/// later change is most likely to quietly retune — hence the table-driven test
/// below rather than a handful of examples.
#[must_use]
pub fn decide(verdict: &RedactionVerdict) -> EgressDecision {
    match verdict.outcome() {
        // A guard that cannot run does not become a guard that passes
        // everything (BR-3, ADR-6).
        Outcome::Unavailable => EgressDecision::Block,
        Outcome::Clean => EgressDecision::Forward,
        Outcome::Findings => {
            if verdict.findings().iter().any(Finding::is_high) {
                EgressDecision::Block
            } else {
                EgressDecision::Forward
            }
        }
    }
}

/// Something that can scan an outbound payload and return a verdict (ADR-1,
/// ADR-2).
///
/// The collaborator [`Egress`](crate::egress::Egress) holds, and the whole
/// interface between the choke point and the redactor: text in, verdict out.
/// The choke point knows nothing about routers, engines, prompts or contracts —
/// it knows [`decide`].
///
/// ## Total, like the scan behind it
///
/// There is no `Result`. Every way a scan can fail — no local tier, an over-cap
/// payload, an engine error, a deadline, an unreadable reply — is already
/// [`Outcome::Unavailable`], which [`decide`] blocks on (ADR-6, BR-3). An error
/// arm here would be a *second* spelling of "the scan could not run", and the
/// two spellings would eventually disagree about whether that means block.
///
/// ## Absence is the off state (ADR-2)
///
/// The gate is installed only when `[privacy] redact` is true, so there is no
/// `enabled` flag on this trait and no `if enabled` branch behind it. "Off" is
/// `Option::None` at the choke point: zero calls, no model interaction, nothing
/// that could claim a scan ran (AC-13). A flag *inside* an installed gate would
/// be a switch a mutation could flip to "on and permissive".
#[async_trait]
pub trait RedactionGate: Send + Sync {
    /// Scan `payload` — the lossy-UTF-8 text of the exact bytes that would go on
    /// the wire (ADR-1) — and say what was found.
    async fn scan(&self, payload: &str) -> RedactionVerdict;
}

/// The deterministic credential pattern pass over `text` (ADR-4).
///
/// Always runs to completion and always yields [`Confidence::High`],
/// [`FindingKind::Credential`] findings — all five shapes are credentials.
/// Returns an empty vector for a clean payload; that empty vector is the *result
/// of a scan*, which is what distinguishes it from [`Outcome::Unavailable`].
///
/// Spans are byte ranges into `text`, sorted ascending by start and free of
/// containment: when two shapes match the same secret (`AWS_API_KEY=AKIA…`
/// matches both the assignment shape and the AWS shape), the outer span is kept
/// and the nested one dropped, so one secret produces one finding.
#[must_use]
pub fn pattern_pass(text: &str) -> Vec<Finding> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    for shape in PREFIX_SHAPES {
        scan_prefix_shape(bytes, shape, &mut spans);
    }
    scan_env_assignment(bytes, &mut spans);
    suppress_contained(spans)
        .into_iter()
        .map(|span| Finding::pattern(FindingKind::Credential, span))
        .collect()
}

/// The bounded pattern-pass entry point: cap check, then the pass (BR-7, ADR-6).
///
/// A payload longer than [`REDACT_INPUT_MAX_BYTES`] is
/// [`RedactionVerdict::unavailable`] — the scan does not run, is not truncated,
/// and does not report a partial result as a complete one. Everything at or
/// under the cap is scanned, and the resulting verdict carries `scanned: true`
/// whether or not anything was found.
#[must_use]
pub fn pattern_verdict(text: &str) -> RedactionVerdict {
    if text.len() > REDACT_INPUT_MAX_BYTES {
        return RedactionVerdict::unavailable();
    }
    RedactionVerdict::from_findings(pattern_pass(text))
}

// ---------------------------------------------------------------------------
// The five shapes, hand-rolled (no regex crate — see the module docs).
// ---------------------------------------------------------------------------

/// A credential shape anchored on a literal prefix followed by a run of body
/// bytes — four of the five shapes have this form.
struct PrefixShape {
    /// The literal prefix that anchors the match.
    prefix: &'static [u8],
    /// Minimum body length for a match (the `{20,}` / `{16}` in the shape).
    min_body: usize,
    /// `Some(n)` ends the match after `n` body bytes (the exact `{16}` in
    /// `AKIA[A-Z0-9]{16}`); `None` is greedy (`{20,}`).
    body_cap: Option<usize>,
    /// Which bytes may appear in the body.
    is_body: fn(u8) -> bool,
}

/// `sk-[A-Za-z0-9_-]{20,}` — OpenAI/Anthropic-style secret keys.
fn is_sk_body(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// `AKIA[A-Z0-9]{16}` — AWS access key ids.
fn is_upper_alnum(b: u8) -> bool {
    b.is_ascii_uppercase() || b.is_ascii_digit()
}

/// `ghp_[A-Za-z0-9]{36,}` — GitHub personal access tokens.
fn is_alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}

/// `Bearer [A-Za-z0-9._-]{20,}` — an `Authorization` bearer token, including the
/// dots of a JWT.
fn is_bearer_body(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-'
}

const PREFIX_SHAPES: &[PrefixShape] = &[
    PrefixShape {
        prefix: b"sk-",
        min_body: 20,
        body_cap: None,
        is_body: is_sk_body,
    },
    PrefixShape {
        prefix: b"AKIA",
        min_body: 16,
        body_cap: Some(16),
        is_body: is_upper_alnum,
    },
    PrefixShape {
        prefix: b"ghp_",
        min_body: 36,
        body_cap: None,
        is_body: is_alnum,
    },
    PrefixShape {
        prefix: b"Bearer ",
        min_body: 20,
        body_cap: None,
        is_body: is_bearer_body,
    },
];

/// Collect every non-overlapping, leftmost-longest match of `shape` in `bytes`.
fn scan_prefix_shape(bytes: &[u8], shape: &PrefixShape, out: &mut Vec<Range<usize>>) {
    let plen = shape.prefix.len();
    let mut i = 0usize;
    while i + plen <= bytes.len() {
        if &bytes[i..i + plen] != shape.prefix {
            i += 1;
            continue;
        }
        let body_start = i + plen;
        // A capped shape stops at exactly `cap` body bytes, mirroring `{16}`.
        let limit = match shape.body_cap {
            Some(cap) => bytes.len().min(body_start + cap),
            None => bytes.len(),
        };
        let mut j = body_start;
        while j < limit && (shape.is_body)(bytes[j]) {
            j += 1;
        }
        if j - body_start >= shape.min_body {
            out.push(i..j);
            i = j;
        } else {
            i += 1;
        }
    }
}

/// The literal suffixes of the assignment shape
/// `[A-Z_]+_(API_KEY|TOKEN)\s*[=:]\s*\S+`.
const ENV_SUFFIXES: &[&[u8]] = &[b"_API_KEY", b"_TOKEN"];

/// A byte of the `[A-Z_]+` name that must precede the suffix.
fn is_name_byte(b: u8) -> bool {
    b.is_ascii_uppercase() || b == b'_'
}

/// A byte matching the shape's `\s` (`[\t\n\v\f\r ]`). `u8::is_ascii_whitespace`
/// omits the vertical tab, so it is added explicitly.
fn is_shape_space(b: u8) -> bool {
    b.is_ascii_whitespace() || b == 0x0b
}

/// Collect every match of `[A-Z_]+_(API_KEY|TOKEN)\s*[=:]\s*\S+`.
///
/// Anchored on the literal suffix, then extended left over the greedy `[A-Z_]+`
/// (which must contribute at least one byte, so a bare `API_KEY=x` is *not* a
/// match — the shape requires a qualified name) and right over the separator and
/// the value.
fn scan_env_assignment(bytes: &[u8], out: &mut Vec<Range<usize>>) {
    for suffix in ENV_SUFFIXES {
        let slen = suffix.len();
        let mut i = 0usize;
        while i + slen <= bytes.len() {
            if &bytes[i..i + slen] != *suffix {
                i += 1;
                continue;
            }
            // `[A-Z_]+` immediately left of the suffix's leading underscore.
            let mut start = i;
            while start > 0 && is_name_byte(bytes[start - 1]) {
                start -= 1;
            }
            if start == i {
                i += 1;
                continue;
            }
            let mut j = i + slen;
            while j < bytes.len() && is_shape_space(bytes[j]) {
                j += 1;
            }
            // `[=:]`
            if j >= bytes.len() || (bytes[j] != b'=' && bytes[j] != b':') {
                i += slen;
                continue;
            }
            j += 1;
            while j < bytes.len() && is_shape_space(bytes[j]) {
                j += 1;
            }
            // `\S+`
            let value_start = j;
            while j < bytes.len() && !is_shape_space(bytes[j]) {
                j += 1;
            }
            if j == value_start {
                i += slen;
                continue;
            }
            out.push(start..j);
            i = j;
        }
    }
}

/// Sort spans ascending and drop any span fully contained in another.
///
/// Two shapes matching the same secret (`AWS_API_KEY=AKIA…`, or
/// `Bearer sk-…`) must report *one* finding, not two — otherwise a single
/// credential inflates the report and the nested span points at part of a thing
/// already named. Sorting by start ascending then end descending means a kept
/// span always starts at or before any span that follows it, so a candidate
/// ending at or before the widest end seen so far is contained in it.
fn suppress_contained(mut spans: Vec<Range<usize>>) -> Vec<Range<usize>> {
    spans.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
    let mut kept: Vec<Range<usize>> = Vec::with_capacity(spans.len());
    let mut max_end = 0usize;
    for span in spans {
        if span.end <= max_end {
            continue;
        }
        max_end = span.end;
        kept.push(span);
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Verdict invariants — enforced by construction, not by discipline.
    // -----------------------------------------------------------------------

    fn a_high() -> Finding {
        Finding::pattern(FindingKind::Credential, 10..30)
    }

    fn a_low() -> Finding {
        Finding::model(FindingKind::Pii, 40..52)
    }

    /// The two invariants, checked as the biconditionals the spec states.
    fn assert_invariants(verdict: &RedactionVerdict) {
        assert_eq!(
            !verdict.findings().is_empty(),
            verdict.outcome() == Outcome::Findings,
            "findings must be non-empty iff the outcome is Findings"
        );
        assert_eq!(
            !verdict.scanned(),
            verdict.outcome() == Outcome::Unavailable,
            "scanned must be false iff the outcome is Unavailable"
        );
    }

    #[test]
    fn every_constructor_establishes_both_verdict_invariants() {
        let verdicts = [
            RedactionVerdict::clean(),
            RedactionVerdict::from_findings(vec![a_high()]),
            RedactionVerdict::from_findings(vec![a_low(), a_high()]),
            // The degenerate input: an empty findings list.
            RedactionVerdict::from_findings(Vec::new()),
            RedactionVerdict::unavailable(),
        ];
        for verdict in &verdicts {
            assert_invariants(verdict);
        }
    }

    #[test]
    fn an_empty_findings_list_collapses_to_clean_never_to_an_empty_findings_outcome() {
        // The discriminating pair: the same constructor, one row with findings
        // and one without. If `from_findings` ever produced `Findings` with an
        // empty vector, the first assertion fires.
        let empty = RedactionVerdict::from_findings(Vec::new());
        assert_eq!(empty.outcome(), Outcome::Clean);
        assert!(empty.scanned(), "an empty result is still a completed scan");

        let non_empty = RedactionVerdict::from_findings(vec![a_low()]);
        assert_eq!(non_empty.outcome(), Outcome::Findings);
        assert_eq!(non_empty.findings().len(), 1);
    }

    #[test]
    fn unavailable_is_the_only_verdict_that_reports_no_scan() {
        // AC-3: no construction makes the scan appear to have run when it did
        // not, and none makes a completed scan look like it did not run.
        assert!(!RedactionVerdict::unavailable().scanned());
        assert!(RedactionVerdict::clean().scanned());
        assert!(RedactionVerdict::from_findings(vec![a_high()]).scanned());
        assert!(RedactionVerdict::unavailable().findings().is_empty());
    }

    #[test]
    fn confidence_is_fixed_by_which_pass_built_the_finding() {
        // ADR-4/BR-4: derived, never self-reported. There is no constructor that
        // takes a confidence argument, so these are the only two levels
        // obtainable and each comes from exactly one pass.
        assert_eq!(
            Finding::pattern(FindingKind::Credential, 0..4).confidence(),
            Confidence::High
        );
        assert!(Finding::pattern(FindingKind::Credential, 0..4).is_high());
        assert_eq!(
            Finding::model(FindingKind::Secret, 0..4).confidence(),
            Confidence::Low
        );
        assert!(!Finding::model(FindingKind::Secret, 0..4).is_high());
    }

    #[test]
    fn finding_kinds_render_as_stable_content_free_names() {
        let names = [
            (FindingKind::Secret, "secret"),
            (FindingKind::Credential, "credential"),
            (FindingKind::Pii, "pii"),
            (FindingKind::Unknown, "unknown"),
        ];
        for (kind, expected) in names {
            assert_eq!(kind.as_str(), expected);
        }
    }

    // -----------------------------------------------------------------------
    // The pattern pass: five shapes, their near-misses, and a clean payload.
    // -----------------------------------------------------------------------

    /// One row of the pattern table. `expect` is the exact substring the pass
    /// must locate — the expected span is derived from its position in `text`,
    /// so a row pins *where* the finding is, not merely that there was one.
    struct ShapeCase {
        name: &'static str,
        text: &'static str,
        expect: Option<&'static str>,
    }

    const SHAPE_CASES: &[ShapeCase] = &[
        ShapeCase {
            name: "sk- secret key",
            text: "here is a key sk-ABCDEFGHIJKLMNOPQRSTUVWX and more prose",
            expect: Some("sk-ABCDEFGHIJKLMNOPQRSTUVWX"),
        },
        ShapeCase {
            name: "AKIA access key id",
            text: "aws id AKIAIOSFODNN7EXAMPLE trailing text",
            expect: Some("AKIAIOSFODNN7EXAMPLE"),
        },
        ShapeCase {
            name: "ghp_ personal access token",
            text: "token ghp_abcdefghijklmnopqrstuvwxyz0123456789 end",
            expect: Some("ghp_abcdefghijklmnopqrstuvwxyz0123456789"),
        },
        ShapeCase {
            name: "Bearer authorization token",
            text: "authorization: Bearer eyJhbGciOiJIUzI1NiJ9.abc",
            expect: Some("Bearer eyJhbGciOiJIUzI1NiJ9.abc"),
        },
        ShapeCase {
            name: "*_API_KEY assignment",
            text: "export AWS_API_KEY=wJalrXUtnFEMI/K7MDENG",
            expect: Some("AWS_API_KEY=wJalrXUtnFEMI/K7MDENG"),
        },
        ShapeCase {
            name: "*_TOKEN assignment with a colon and spaces",
            text: "config GITHUB_TOKEN : hunter2seventeen",
            expect: Some("GITHUB_TOKEN : hunter2seventeen"),
        },
        // The clean row runs through the *same* function as every row above.
        // That is the non-vacuity: the call that returns nothing here is the
        // call that returned a located span there, so "found nothing" cannot be
        // "was never invoked" (LESSON-485).
        ShapeCase {
            name: "clean prose",
            text: "the quick brown fox reads src/main.rs and returns ok",
            expect: None,
        },
        // Near-misses: each is one shape with its run length below the minimum,
        // which is the state that discriminates "the pass matches this shape"
        // from "the pass matches anything vaguely similar".
        ShapeCase {
            name: "sk- below the 20-byte minimum",
            text: "sk-tooshort here",
            expect: None,
        },
        ShapeCase {
            name: "AKIA below the 16-byte body",
            text: "AKIA123 here",
            expect: None,
        },
        ShapeCase {
            name: "ghp_ below the 36-byte body",
            text: "ghp_abcdefghijklmnop here",
            expect: None,
        },
        ShapeCase {
            name: "Bearer below the 20-byte token",
            text: "Bearer shorttoken",
            expect: None,
        },
        ShapeCase {
            name: "assignment with no value",
            text: "AWS_API_KEY=",
            expect: None,
        },
        ShapeCase {
            name: "unqualified API_KEY has no [A-Z_]+ prefix",
            text: "API_KEY=value",
            expect: None,
        },
    ];

    #[test]
    fn the_pattern_pass_locates_each_shape_and_nothing_else() {
        for case in SHAPE_CASES {
            let findings = pattern_pass(case.text);
            match case.expect {
                Some(expected) => {
                    assert_eq!(
                        findings.len(),
                        1,
                        "{}: expected exactly one finding, got {findings:?}",
                        case.name
                    );
                    let start = case
                        .text
                        .find(expected)
                        .unwrap_or_else(|| panic!("{}: fixture is malformed", case.name));
                    assert_eq!(
                        *findings[0].span(),
                        start..start + expected.len(),
                        "{}: span must cover exactly the credential",
                        case.name
                    );
                    // Confidence and kind are structural for this pass.
                    assert_eq!(findings[0].confidence(), Confidence::High, "{}", case.name);
                    assert_eq!(findings[0].kind(), FindingKind::Credential, "{}", case.name);
                }
                None => assert!(
                    findings.is_empty(),
                    "{}: expected no finding, got {findings:?}",
                    case.name
                ),
            }
        }
    }

    #[test]
    fn a_clean_payload_yields_a_scanned_clean_verdict_not_a_skipped_one() {
        // AC-2's non-vacuity pairing: the clean payload's verdict must prove the
        // pass RAN (`scanned: true`, outcome Clean) rather than being the
        // "could not look" state, and the identical entry point must still find
        // a planted credential.
        let clean = pattern_verdict("the quick brown fox reads src/main.rs and returns ok");
        assert_eq!(clean.outcome(), Outcome::Clean);
        assert!(clean.scanned(), "a clean verdict must claim the scan ran");
        assert_eq!(decide(&clean), EgressDecision::Forward);

        let dirty = pattern_verdict("prose then sk-ABCDEFGHIJKLMNOPQRSTUVWX");
        assert_eq!(dirty.outcome(), Outcome::Findings);
        assert!(dirty.scanned());
        assert_eq!(decide(&dirty), EgressDecision::Block);
    }

    #[test]
    fn spans_are_byte_offsets_that_stay_on_char_boundaries() {
        // Every shape is ASCII, and an ASCII byte never occurs inside a
        // multi-byte UTF-8 sequence — so a span derived from a byte scan must
        // still slice a payload containing multi-byte characters.
        let text = "héllo wörld sk-ABCDEFGHIJKLMNOPQRSTUVWX ünd mehr";
        let findings = pattern_pass(text);
        assert_eq!(findings.len(), 1);
        let span = findings[0].span().clone();
        let expected = "sk-ABCDEFGHIJKLMNOPQRSTUVWX";
        assert_eq!(
            span,
            text.find(expected).unwrap()..text.find(expected).unwrap() + expected.len()
        );
        // Would panic if the offsets were not char boundaries.
        assert_eq!(&text[span], expected);
    }

    #[test]
    fn one_secret_matched_by_two_shapes_reports_one_finding() {
        // `AWS_API_KEY=AKIA…` matches the assignment shape and the AWS shape.
        // The nested span is dropped so a single credential is named once.
        let text = "AWS_API_KEY=AKIAIOSFODNN7EXAMPLE";
        let findings = pattern_pass(text);
        assert_eq!(findings.len(), 1, "got {findings:?}");
        assert_eq!(*findings[0].span(), 0..text.len());
    }

    #[test]
    fn several_distinct_credentials_are_reported_in_ascending_span_order() {
        let text = "AKIAIOSFODNN7EXAMPLE and sk-ABCDEFGHIJKLMNOPQRSTUVWX and more";
        let findings = pattern_pass(text);
        assert_eq!(findings.len(), 2, "got {findings:?}");
        assert!(
            findings[0].span().start < findings[1].span().start,
            "findings must be ordered by position"
        );
        assert_eq!(*findings[0].span(), 0..20);
        let sk = text.find("sk-").unwrap();
        assert_eq!(*findings[1].span(), sk..text.len() - " and more".len());
    }

    #[test]
    fn the_pattern_pass_is_deterministic() {
        let text =
            "AWS_API_KEY=wJalrXUtnFEMI and AKIAIOSFODNN7EXAMPLE and Bearer eyJhbGciOiJIUzI1NiJ9";
        let first = pattern_pass(text);
        for _ in 0..8 {
            assert_eq!(pattern_pass(text), first);
        }
    }

    #[test]
    fn a_finding_never_carries_the_matched_text() {
        // BR-6 / ADR-5, and AC-8's mutation (c): adding a text field to
        // `Finding` — even a private one — turns this red, because `Debug` is
        // derived and would start rendering the secret.
        const SENTINEL: &str = "sk-ZZQUUXSENTINELCREDENTIAL0123";
        let verdict = pattern_verdict(&format!("please summarize {SENTINEL} for me"));
        // Non-vacuity: the sentinel really was detected, so the absence below is
        // about the type and not about an empty result.
        assert_eq!(verdict.outcome(), Outcome::Findings);
        assert_eq!(verdict.findings().len(), 1);

        let rendered = format!("{verdict:?}");
        assert!(
            !rendered.contains("QUUXSENTINEL"),
            "a finding must never carry the matched text: {rendered}"
        );
        assert!(
            !rendered.contains(SENTINEL),
            "a finding must never carry the matched text: {rendered}"
        );
    }

    // -----------------------------------------------------------------------
    // The input cap (BR-7, ADR-6).
    // -----------------------------------------------------------------------

    #[test]
    fn the_input_cap_is_sixty_four_kibibytes() {
        assert_eq!(REDACT_INPUT_MAX_BYTES, 65_536);
    }

    #[test]
    fn an_over_cap_payload_is_unavailable_and_blocks_never_forwards() {
        // BR-7 / AC-8 mutation (b): a payload too large to scan is the "cannot
        // run" case. The payload here is deliberately CLEAN — that is the
        // discriminating state. A clean payload is the one case that would
        // Forward if the bound were removed, so removing the cap turns this red
        // rather than leaving it accidentally green.
        let over = "x".repeat(REDACT_INPUT_MAX_BYTES + 1);
        let verdict = pattern_verdict(&over);
        assert_eq!(verdict.outcome(), Outcome::Unavailable);
        assert!(
            !verdict.scanned(),
            "an over-cap payload must not claim a scan ran"
        );
        assert!(verdict.findings().is_empty());
        assert_eq!(
            decide(&verdict),
            EgressDecision::Block,
            "an over-cap payload must never forward"
        );
    }

    #[test]
    fn a_payload_at_exactly_the_cap_is_scanned() {
        // The non-vacuity twin: one byte under the over-cap fixture, the same
        // clean content, and the scan runs. Without this, the test above would
        // also pass if `pattern_verdict` returned `Unavailable` for everything.
        let at_cap = "x".repeat(REDACT_INPUT_MAX_BYTES);
        let verdict = pattern_verdict(&at_cap);
        assert_eq!(verdict.outcome(), Outcome::Clean);
        assert!(verdict.scanned());
        assert_eq!(decide(&verdict), EgressDecision::Forward);
    }

    #[test]
    fn an_over_cap_payload_carrying_a_credential_still_reports_could_not_scan() {
        // ADR-6: the block's cause must say the scan could not run, never that
        // it found something — different problems with different fixes. A
        // truncate-and-scan would report `Findings` (and `scanned: true`) here,
        // claiming a completed scan of bytes the model pass never saw.
        let mut over = String::from("sk-ABCDEFGHIJKLMNOPQRSTUVWX ");
        over.push_str(&"x".repeat(REDACT_INPUT_MAX_BYTES));
        let verdict = pattern_verdict(&over);
        assert_eq!(verdict.outcome(), Outcome::Unavailable);
        assert!(!verdict.scanned());
        assert_eq!(decide(&verdict), EgressDecision::Block);
    }

    // -----------------------------------------------------------------------
    // AC-10: confidence drives the action.
    // -----------------------------------------------------------------------

    /// One row of the decision table. `findings` is built from a confidence
    /// list so each row names its discriminating state explicitly (LESSON-485):
    /// what changes between a Block row and a Forward row is the presence of a
    /// `High`, and nothing else.
    struct DecisionCase {
        name: &'static str,
        verdict: fn() -> RedactionVerdict,
        expected: EgressDecision,
    }

    fn findings_of(levels: &[Confidence]) -> RedactionVerdict {
        let findings = levels
            .iter()
            .enumerate()
            .map(|(i, level)| {
                let span = i * 10..i * 10 + 8;
                match level {
                    Confidence::High => Finding::pattern(FindingKind::Credential, span),
                    Confidence::Low => Finding::model(FindingKind::Pii, span),
                }
            })
            .collect();
        RedactionVerdict::from_findings(findings)
    }

    const DECISION_CASES: &[DecisionCase] = &[
        DecisionCase {
            name: "clean",
            verdict: RedactionVerdict::clean,
            expected: EgressDecision::Forward,
        },
        DecisionCase {
            name: "unavailable",
            verdict: RedactionVerdict::unavailable,
            expected: EgressDecision::Block,
        },
        DecisionCase {
            name: "single high",
            verdict: || findings_of(&[Confidence::High]),
            expected: EgressDecision::Block,
        },
        DecisionCase {
            // The row AC-10 names explicitly: a low-confidence-only payload is
            // NOT blocked.
            name: "single low",
            verdict: || findings_of(&[Confidence::Low]),
            expected: EgressDecision::Forward,
        },
        DecisionCase {
            name: "mixed high then low",
            verdict: || findings_of(&[Confidence::High, Confidence::Low]),
            expected: EgressDecision::Block,
        },
        DecisionCase {
            // Order must not matter: the same multiset as the row above.
            name: "mixed low then high",
            verdict: || findings_of(&[Confidence::Low, Confidence::High]),
            expected: EgressDecision::Block,
        },
        DecisionCase {
            name: "mixed all low",
            verdict: || findings_of(&[Confidence::Low, Confidence::Low, Confidence::Low]),
            expected: EgressDecision::Forward,
        },
        DecisionCase {
            name: "mixed all high",
            verdict: || findings_of(&[Confidence::High, Confidence::High]),
            expected: EgressDecision::Block,
        },
        DecisionCase {
            name: "many low around one high",
            verdict: || {
                findings_of(&[
                    Confidence::Low,
                    Confidence::Low,
                    Confidence::High,
                    Confidence::Low,
                ])
            },
            expected: EgressDecision::Block,
        },
    ];

    #[test]
    fn confidence_drives_the_egress_decision() {
        for case in DECISION_CASES {
            let verdict = (case.verdict)();
            assert_invariants(&verdict);
            assert_eq!(
                decide(&verdict),
                case.expected,
                "{}: verdict {verdict:?}",
                case.name
            );
        }
    }

    #[test]
    fn the_decision_table_covers_both_outcomes() {
        // Guards the table itself: a table where every row expects the same
        // decision would pass vacuously no matter what `decide` did.
        assert!(DECISION_CASES
            .iter()
            .any(|c| c.expected == EgressDecision::Block));
        assert!(DECISION_CASES
            .iter()
            .any(|c| c.expected == EgressDecision::Forward));
    }

    #[test]
    fn adding_a_low_finding_never_changes_a_forward_into_a_block() {
        // The property behind BR-4, stated once rather than per-row: low
        // findings are reportable, not blocking.
        let lows = findings_of(&[Confidence::Low; 5]);
        assert_eq!(decide(&lows), EgressDecision::Forward);
        assert_eq!(lows.outcome(), Outcome::Findings);
        assert_eq!(lows.findings().len(), 5);
    }
}
