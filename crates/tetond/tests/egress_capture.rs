//! AC-5 egress-capture harness (BR-1).
//!
//! The acceptance criterion is explicit that a privacy-boundary claim is proven
//! by *capturing outbound traffic*, not by reading code. So this test wires the
//! real [`Egress`] choke point in front of a **capture transport** — a mock
//! `Transport` that records every request it is asked to send instead of hitting
//! the network — drives a scripted session that reads both public and
//! `local-only` files, and asserts:
//!
//! 1. Zero bytes of any boundary file appear in *any* captured outbound payload.
//! 2. A deliberate attempt to route boundary content remotely produces a
//!    `privacy_block` event carrying the path, provider, and action.
//! 3. Provenance survives derivation: a *summary of* a boundary file is blocked
//!    even though it shares no bytes with the original.
//! 4. Error and event/telemetry surfaces carry no boundary content.
//!
//! The capture transport stands in for the network; because `teton-providers`
//! owns no HTTP client, in production the only thing behind the same seam is
//! `tetond`'s real client — so what this harness proves about the guard holds in
//! production too.
//!
//! ## REQ-571 AC-1/AC-2: the spelling matrix
//!
//! The blocks above are hand-built, which is exactly the shape of test that
//! could not have caught REQ-571's bug: they assert what egress does with a
//! provenance, never what a *tool* puts there. So the matrix at the bottom of
//! this file drives the real [`ReadTool`]/[`EditTool`] against a real temp repo,
//! folds each outcome through the daemon's own provenance path
//! (`tool_result_provenance`), and sends it through this same choke point. Every
//! spelling of one boundary file must produce a `privacy_block` **and** the
//! byte-identical `provenance_id` — the second half being what pins BR-2, since
//! a block against a divergent id is a block that happened to fire.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use teton_core::entities::{BoundaryMode, PrivacyBoundary};
use teton_core::ProvenanceId;
use teton_protocol::events::{Event, PrivacyAction, PrivacyBlock, ProvenanceRejected};
use teton_protocol::{ProviderId, SessionId};
use teton_providers::transport::{
    ByteStream, HttpMethod, Transport, TransportError, TransportRequest, TransportResponse,
};

use tetond::egress::provenance::{assembled_provenance, ContextBlock};
use tetond::egress::{Egress, EgressContext, EgressError, PrivacyEventSink, Provenance};

/// Mint the identity of a fixture file (REQ-571 ADR-A).
///
/// The provenance channel accepts only a [`ProvenanceId`], and an integration
/// test cannot reach the crate-internal fixture helper, so each test binary
/// states its own. A fixture naming a path that is not an identity is a broken
/// fixture, hence the panic.
fn source_id(path: &str) -> ProvenanceId {
    ProvenanceId::claimed(path).expect("fixture path must be a provenance id")
}

/// Secrets that must never appear in captured egress. Distinct markers so a leak
/// is unambiguous.
const SECRET_ENV: &str = "API_KEY=sk-live-DO-NOT-LEAK-abc123";
const SECRET_YAML: &str = "db_password: hunter2-NEVER-SHIP";

/// A `Transport` that records every request instead of sending it, and returns a
/// canned 200 with a short body stream. The record is shared so the test can
/// inspect it after the transport is moved into the [`Egress`].
#[derive(Default, Clone)]
struct CaptureTransport {
    sent: Arc<Mutex<Vec<TransportRequest>>>,
}

impl CaptureTransport {
    fn captured(&self) -> Vec<TransportRequest> {
        self.sent.lock().unwrap().clone()
    }
}

#[async_trait]
impl Transport for CaptureTransport {
    async fn execute(
        &self,
        request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        self.sent.lock().unwrap().push(request);
        let body: ByteStream = Box::pin(futures::stream::once(async {
            Ok(b"{\"ok\":true}".to_vec())
        }));
        Ok(TransportResponse {
            status: 200,
            location: None,
            body,
        })
    }
}

/// Captures `privacy_block` events for assertion.
#[derive(Default)]
struct CapturingSink {
    events: Mutex<Vec<(Option<SessionId>, PrivacyBlock)>>,
}

impl CapturingSink {
    fn events(&self) -> Vec<(Option<SessionId>, PrivacyBlock)> {
        self.events.lock().unwrap().clone()
    }
}

impl PrivacyEventSink for CapturingSink {
    fn privacy_block(&self, session_id: Option<SessionId>, block: PrivacyBlock) {
        self.events.lock().unwrap().push((session_id, block));
    }
    // Required no-op: this AC-9 fixture captures blocks, not rejections.
    fn provenance_rejected(&self, _session_id: Option<SessionId>, _rejected: ProvenanceRejected) {}
}

fn local_only_boundaries() -> Vec<PrivacyBoundary> {
    vec![PrivacyBoundary {
        path_glob: "secrets/**".to_owned(),
        mode: BoundaryMode::LocalOnly,
    }]
}

/// Simulate the daemon's context-assembly step: serialize a set of context
/// blocks into a request body and compute the request's provenance from them.
fn assemble(url: &str, blocks: &[ContextBlock]) -> (TransportRequest, Provenance) {
    let joined = blocks
        .iter()
        .map(ContextBlock::content)
        .collect::<Vec<_>>()
        .join("\n");
    let request = TransportRequest {
        method: HttpMethod::Post,
        url: url.to_owned(),
        headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        body: joined.into_bytes(),
    };
    (request, assembled_provenance(blocks))
}

fn contains_bytes(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

/// AC-5, the whole criterion: a scripted session that touches `local-only` files
/// completes with zero boundary bytes in captured egress, and the deliberate
/// remote-routing attempt raises a `privacy_block`.
#[tokio::test]
async fn scripted_session_leaks_zero_boundary_bytes_and_blocks_deliberate_egress() {
    let capture = CaptureTransport::default();
    let sink = Arc::new(CapturingSink::default());
    let egress = Egress::new(capture.clone(), local_only_boundaries(), sink.clone());
    let ctx = EgressContext::new("anthropic").with_session("sess-captured");

    // Turn 1 — an ordinary turn over public files only. Must be allowed and reach
    // the (captured) wire.
    let (req, prov) = assemble(
        "https://api.anthropic.com/v1/messages",
        &[
            ContextBlock::synthetic("You are Teton Code."),
            ContextBlock::from_file(source_id("src/main.rs"), "fn main() { println!(\"hi\"); }"),
            ContextBlock::from_file(source_id("README.md"), "# Teton Code"),
        ],
    );
    let r1 = egress.send(req, &prov, &ctx).await;
    assert!(r1.is_ok(), "public turn must be allowed");

    // Turn 2 — DELIBERATE violation: the assembled context includes a read of a
    // `local-only` file. Must be blocked before any byte leaves.
    let (req, prov) = assemble(
        "https://api.anthropic.com/v1/messages",
        &[
            ContextBlock::synthetic("You are Teton Code."),
            ContextBlock::from_file(source_id("src/main.rs"), "fn main() {}"),
            ContextBlock::from_file(source_id("secrets/prod.env"), SECRET_ENV),
        ],
    );
    let r2 = egress.send(req, &prov, &ctx).await;
    match r2 {
        Err(EgressError::PrivacyBlocked {
            path,
            provider_id,
            action,
            // REQ-562 ADR-7: the provenance inspection is one of several things
            // that can refuse a payload here, and this is the one that says
            // *boundary*.
            cause,
        }) => {
            assert_eq!(path, "secrets/prod.env");
            assert_eq!(provider_id, ProviderId::from("anthropic"));
            assert_eq!(action, PrivacyAction::ReroutedToLocal);
            assert_eq!(cause, teton_protocol::events::BlockCause::Boundary);
        }
        other => panic!("turn 2 must be a privacy block, got {other:?}"),
    }

    // Turn 3 — provenance survives DERIVATION: a summary of a boundary file.
    let secret_block = ContextBlock::from_file(source_id("secrets/config.yaml"), SECRET_YAML);
    let summary = secret_block.derive("Summary: this file holds the production DB credentials.");
    assert!(
        !summary.content().contains("hunter2"),
        "the summary must genuinely not quote the secret"
    );
    let (req, prov) = assemble(
        "https://api.anthropic.com/v1/messages",
        &[ContextBlock::synthetic("You are Teton Code."), summary],
    );
    let r3 = egress.send(req, &prov, &ctx).await;
    assert!(
        matches!(r3, Err(EgressError::PrivacyBlocked { ref path, .. }) if path == "secrets/config.yaml"),
        "a summary of a boundary file must itself be blocked, got {r3:?}"
    );

    // --- Assertions over the whole session ---

    // (1) Exactly one request reached the wire (turn 1); turns 2 and 3 never did.
    let captured = capture.captured();
    assert_eq!(captured.len(), 1, "only the clean turn may be forwarded");

    // (2) Zero boundary bytes in ANY captured payload — the core AC-5 property.
    for req in &captured {
        assert!(
            !contains_bytes(&req.body, SECRET_ENV),
            "SECRET_ENV leaked into egress"
        );
        assert!(
            !contains_bytes(&req.body, SECRET_YAML),
            "SECRET_YAML leaked into egress"
        );
        assert!(
            !contains_bytes(&req.body, "hunter2"),
            "derived secret fragment leaked into egress"
        );
    }
    // Positive control: the allowed turn's public content did go out.
    assert!(contains_bytes(&captured[0].body, "fn main()"));

    // (3) Two privacy_block events, one per blocked turn, correctly attributed.
    let events = sink.events();
    assert_eq!(events.len(), 2, "one privacy_block per blocked turn");
    let paths: Vec<&str> = events.iter().map(|(_, b)| b.path.as_str()).collect();
    assert!(paths.contains(&"secrets/prod.env"));
    assert!(paths.contains(&"secrets/config.yaml"));
    for (session_id, block) in &events {
        assert_eq!(session_id.as_ref(), Some(&SessionId::from("sess-captured")));
        assert_eq!(block.provider_id, ProviderId::from("anthropic"));
        assert_eq!(block.action, PrivacyAction::ReroutedToLocal);
    }
}

/// AC-4: neither the typed error nor the serialized event carries boundary
/// content — the paths that surface a block to logs, clients, and telemetry are
/// content-free.
#[tokio::test]
async fn error_and_event_paths_exclude_boundary_content() {
    let sink = Arc::new(CapturingSink::default());
    let egress = Egress::new(
        CaptureTransport::default(),
        local_only_boundaries(),
        sink.clone(),
    );
    let ctx = EgressContext::new("anthropic");

    let (req, prov) = assemble(
        "https://api.anthropic.com/v1/messages",
        &[ContextBlock::from_file(
            source_id("secrets/prod.env"),
            SECRET_ENV,
        )],
    );
    let err = egress.send(req, &prov, &ctx).await.expect_err("must block");

    // The typed error renders with only path + provider + action.
    let rendered = err.to_string();
    assert!(rendered.contains("secrets/prod.env"));
    assert!(!rendered.contains("sk-live"));
    assert!(!rendered.contains("API_KEY"));

    // The event serializes (the shape that travels to clients / telemetry) with
    // no secret bytes. Serialize the tagged `Event` — the wire form a subscriber
    // actually receives.
    let (_, block) = sink.events().pop().expect("one event");
    let json = serde_json::to_string(&Event::PrivacyBlock(block)).expect("serialize event");
    assert!(json.contains("secrets/prod.env"));
    assert!(json.contains("privacy_block"));
    assert!(!json.contains("sk-live"));
    assert!(!json.contains("API_KEY"));
}

/// The adapter-facing scoped transport enforces the same boundary through the
/// object-safe `Transport` seam an adapter actually holds.
#[tokio::test]
async fn adapter_seam_is_enforced() {
    let capture = CaptureTransport::default();
    let sink = Arc::new(CapturingSink::default());
    let egress = Egress::new(capture.clone(), local_only_boundaries(), sink.clone());

    let scoped = egress.scoped(
        Provenance::tainted_by(source_id("secrets/prod.env")),
        EgressContext::new("anthropic"),
    );
    let request = TransportRequest {
        method: HttpMethod::Post,
        url: "https://api.anthropic.com/v1/messages".to_owned(),
        headers: vec![],
        body: SECRET_ENV.as_bytes().to_vec(),
    };
    // An adapter only sees `&dyn Transport`; the block manifests as the
    // dedicated, non-retryable `PrivacyBlocked` signal (REQ-544 M-1) — never a
    // connect refusal that could be misclassified as transient and retried — and
    // the request never reaches the wire.
    let err = Transport::execute(&scoped, request)
        .await
        .expect_err("scoped transport must refuse");
    assert_eq!(
        err,
        TransportError::PrivacyBlocked(teton_providers::BlockDetail::Boundary),
        "REQ-562 BR-3: and it names the inspection that refused it"
    );
    assert!(capture.captured().is_empty(), "nothing may reach the wire");
    assert_eq!(sink.events().len(), 1, "the block still emitted its event");
}

// ---------------------------------------------------------------------------
// REQ-560 AC-4 — permission levels are orthogonal to the privacy boundary
// ---------------------------------------------------------------------------

/// **`full` grants tool execution; it does not touch egress** (REQ-560 BR-3).
///
/// This is the criterion that keeps the two mechanisms orthogonal, and the spec
/// is explicit that it is **not satisfiable by code inspection** — LESSON-432 is
/// the precedent, where a guarantee's hole was invisible because the tests that
/// would have caught it were the ones nobody wrote. So the check is the same
/// shape as the AC-5 harness above: drive the real choke point and look at the
/// bytes.
///
/// The construction is deliberately blunt. A session is put at
/// [`PermissionLevel::Full`] — the level that stops asking about `shell`, the
/// most permissive thing a user can type — and then the exact traffic the AC-5
/// harness blocks is sent through the exact same `Egress`. If a level had leaked
/// into an egress predicate, `full` is the value that would have opened it.
///
/// Note what is **not** mocked away: this is the production `Egress`, with
/// production boundaries, and the level is genuinely set on a gate. The only
/// stand-in is the transport, which is the point of the harness.
#[tokio::test]
async fn a_full_permission_level_still_blocks_boundary_content_and_still_reports_it() {
    use teton_protocol::permissions::PermissionLevel;

    // A session really at `full`, through the daemon's own gate rather than a
    // stub — so this test is about the shipped classifier, not a paraphrase.
    let gate = tetond::harness::PermissionGate::with_level(
        SessionId::from("sess-at-full"),
        PermissionLevel::Full,
        Vec::new(),
        Arc::new(tetond::broadcast::EventBus::new()),
        Arc::new(tetond::harness::PendingPermissions::new()),
    );
    assert_eq!(
        gate.level(),
        Some(PermissionLevel::Full),
        "this test is only meaningful with the session actually at full"
    );
    // The level really is permissive about tools — the non-vacuity control. If
    // `full` did not allow `shell`, the assertions below would pass for the
    // wrong reason.
    assert_eq!(
        gate.authorize("shell", None).await,
        tetond::harness::PermissionDecision::Allowed,
        "full must allow a shell without asking, or this test proves nothing"
    );

    let capture = CaptureTransport::default();
    let sink = Arc::new(CapturingSink::default());
    let egress = Egress::new(capture.clone(), local_only_boundaries(), sink.clone());
    let ctx = EgressContext::new("anthropic").with_session("sess-at-full");

    // A public turn still goes out — the positive control, so "zero leaks"
    // cannot be satisfied by an egress that refuses everything.
    let (req, prov) = assemble(
        "https://api.anthropic.com/v1/messages",
        &[ContextBlock::from_file(
            source_id("src/main.rs"),
            "fn main() {}",
        )],
    );
    assert!(
        egress.send(req, &prov, &ctx).await.is_ok(),
        "a public turn must still be allowed at full"
    );

    // A boundary read is still blocked, at the most permissive level there is.
    let (req, prov) = assemble(
        "https://api.anthropic.com/v1/messages",
        &[ContextBlock::from_file(
            source_id("secrets/prod.env"),
            SECRET_ENV,
        )],
    );
    let blocked = egress.send(req, &prov, &ctx).await;
    assert!(
        matches!(
            blocked,
            Err(EgressError::PrivacyBlocked { ref path, .. }) if path == "secrets/prod.env"
        ),
        "full must not open the local-only boundary; got {blocked:?}"
    );

    // A derived summary is still blocked, so the level did not weaken
    // provenance either.
    let derived = ContextBlock::from_file(source_id("secrets/config.yaml"), SECRET_YAML)
        .derive("Summary: this file holds the production DB credentials.");
    let (req, prov) = assemble(
        "https://api.anthropic.com/v1/messages",
        &[ContextBlock::synthetic("You are Teton Code."), derived],
    );
    assert!(
        matches!(
            egress.send(req, &prov, &ctx).await,
            Err(EgressError::PrivacyBlocked { ref path, .. }) if path == "secrets/config.yaml"
        ),
        "full must not weaken provenance on a derived block"
    );

    // Zero boundary bytes in any captured payload, at `full`.
    let captured = capture.captured();
    assert_eq!(
        captured.len(),
        1,
        "only the public turn may be forwarded, whatever the permission level"
    );
    for req in &captured {
        for secret in [SECRET_ENV, SECRET_YAML, "hunter2"] {
            assert!(
                !contains_bytes(&req.body, secret),
                "boundary content reached the wire at permission level `full`"
            );
        }
    }
    assert!(
        contains_bytes(&captured[0].body, "fn main()"),
        "the positive control never went out, so the zero-leak claim is vacuous"
    );

    // …and `privacy_block` is still emitted, once per blocked turn. A level that
    // silenced the reporting would be as bad as one that opened the boundary.
    let events = sink.events();
    assert_eq!(
        events.len(),
        2,
        "privacy_block must still fire at full; got {events:?}"
    );
    let paths: Vec<&str> = events.iter().map(|(_, b)| b.path.as_str()).collect();
    assert!(paths.contains(&"secrets/prod.env"), "{paths:?}");
    assert!(paths.contains(&"secrets/config.yaml"), "{paths:?}");
    for (session_id, block) in &events {
        assert_eq!(session_id.as_ref(), Some(&SessionId::from("sess-at-full")));
        assert_eq!(block.action, PrivacyAction::ReroutedToLocal);
    }
}

// ---------------------------------------------------------------------------
// REQ-571 AC-1 / AC-2 — the BR-3 spelling matrix, driven through the real tools
// ---------------------------------------------------------------------------

/// The one identity every spelling of the boundary file must mint, and the one
/// value a `secrets/**` glob can match.
const CANONICAL_ID: &str = "secrets/prod.env";

/// Distinctive bytes of the **public** file, for the positive control: a
/// zero-leak assertion over an egress that forwarded nothing proves nothing
/// (LESSON-479).
const PUBLIC_CONTENT: &str = "fn main() { println!(\"PUBLIC-MARKER-9f3a\"); }";

/// A temp repo holding one boundary file and one public file.
///
/// Per-tool rather than shared: `read` and `edit` each get their own repo, their
/// own `Egress`, and their own sink, so neither tool's coverage rides on the
/// other's (LESSON-502). A single shared fixture is how one tool's regression
/// hides behind the other's assertions.
fn spelling_repo(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "teton-req571-{tag}-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(root.join("secrets")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("secrets/prod.env"), SECRET_ENV).unwrap();
    std::fs::write(root.join("src/main.rs"), PUBLIC_CONTENT).unwrap();
    root
}

/// The BR-3 spelling set as a model would put it in a tool argument: bare
/// relative, `./`-prefixed, `.//`, `././`, absolute-inside-root, and
/// `..`-traversing-but-inside-root.
///
/// Every one names the same file. Before REQ-571 the last two were tagged
/// verbatim — `/abs/root/secrets/prod.env` and `src/../secrets/prod.env` — and
/// neither is a value any repo-relative boundary glob matches.
fn br3_spellings(root: &std::path::Path) -> Vec<String> {
    let absolute = root
        .canonicalize()
        .unwrap()
        .join(CANONICAL_ID)
        .to_string_lossy()
        .into_owned();
    vec![
        CANONICAL_ID.to_owned(),
        format!("./{CANONICAL_ID}"),
        format!(".//{CANONICAL_ID}"),
        format!("././{CANONICAL_ID}"),
        absolute,
        format!("src/../{CANONICAL_ID}"),
    ]
}

/// The single identity a tool outcome claims, or a panic naming what it claimed
/// instead. A result with no identity, more than one, or `Unknown` is not a
/// tagged single-file read/edit and would make the matrix vacuous.
fn sole_identity(outcome: &tetond::harness::ToolOutcome) -> String {
    match &outcome.provenance {
        tetond::harness::ToolProvenance::Sources(ids) => {
            let ids: Vec<&str> = ids.iter().map(ProvenanceId::as_str).collect();
            assert_eq!(ids.len(), 1, "expected exactly one source, got {ids:?}");
            ids[0].to_owned()
        }
        tetond::harness::ToolProvenance::Unknown => {
            panic!("a first-party file tool must never report unknown provenance")
        }
    }
}

/// One turn built from a tool outcome, exactly as the daemon would scope it: the
/// body carries the file's bytes (what the model now holds) and the provenance
/// is the outcome's own, folded through the daemon's single bridge
/// `tool_result_provenance` rather than through a paraphrase of it.
fn turn_from(outcome: &tetond::harness::ToolOutcome, body: &str) -> (TransportRequest, Provenance) {
    let request = TransportRequest {
        method: HttpMethod::Post,
        url: "https://api.anthropic.com/v1/messages".to_owned(),
        headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        body: format!("{}\n{body}", outcome.content).into_bytes(),
    };
    (
        request,
        tetond::harness::digest::tool_result_provenance(&outcome.provenance),
    )
}

/// **REQ-571 AC-1.** Every BR-3 spelling of a `local-only` file, driven through
/// the real `read` tool, is refused at the real choke point — and every one is
/// refused under the *same* identity.
///
/// Both halves matter. "It was blocked" alone would pass for a tool that tagged
/// a different-but-still-matching path on each call; the byte-identical
/// `provenance_id` is what says the six spellings are one file, which is BR-2.
#[tokio::test]
async fn read_blocks_every_boundary_spelling_under_one_identity() {
    use tetond::harness::tools::ReadTool;
    use tetond::harness::{Tool, ToolContext};

    let root = spelling_repo("read");
    let ctx = ToolContext::new(&root);
    let capture = CaptureTransport::default();
    let sink = Arc::new(CapturingSink::default());
    let egress = Egress::new(capture.clone(), local_only_boundaries(), sink.clone());
    let egress_ctx = EgressContext::new("anthropic").with_session("sess-read-spellings");

    // The positive control first, so a later "nothing leaked" cannot be
    // satisfied by an egress that refuses everything (LESSON-479).
    let public = ReadTool.run(&ctx, &serde_json::json!({ "path": "src/main.rs" }));
    assert!(!public.is_error, "{}", public.content);
    assert_eq!(sole_identity(&public), "src/main.rs");
    let (req, prov) = turn_from(&public, PUBLIC_CONTENT);
    assert!(
        egress.send(req, &prov, &egress_ctx).await.is_ok(),
        "a public read must still reach the wire"
    );

    let mut identities = Vec::new();
    for spelling in br3_spellings(&root) {
        let out = ReadTool.run(&ctx, &serde_json::json!({ "path": spelling }));
        assert!(!out.is_error, "spelling {spelling:?}: {}", out.content);
        assert!(
            out.content.contains("sk-live"),
            "fixture: spelling {spelling:?} must actually surface the secret, \
             or the block below is about nothing"
        );
        identities.push(sole_identity(&out));

        let (req, prov) = turn_from(&out, SECRET_ENV);
        let blocked = egress.send(req, &prov, &egress_ctx).await;
        match blocked {
            Err(EgressError::PrivacyBlocked { ref path, .. }) => assert_eq!(
                path, CANONICAL_ID,
                "spelling {spelling:?} was blocked against a divergent identity"
            ),
            other => panic!("spelling {spelling:?} was not blocked: {other:?}"),
        }
    }

    // AC-1's second half: byte-identical, not merely all-matching.
    assert_eq!(
        identities.len(),
        6,
        "the whole BR-3 set must have been driven"
    );
    for (spelling, id) in br3_spellings(&root).iter().zip(&identities) {
        assert_eq!(
            id, CANONICAL_ID,
            "spelling {spelling:?} minted a second identity for one file"
        );
    }

    // Only the public read reached the wire, and it carried real content.
    let captured = capture.captured();
    assert_eq!(captured.len(), 1, "only the public read may be forwarded");
    assert!(
        contains_bytes(&captured[0].body, PUBLIC_CONTENT),
        "the positive control never went out, so the zero-leak claim is vacuous"
    );
    for req in &captured {
        for secret in [SECRET_ENV, "sk-live"] {
            assert!(
                !contains_bytes(&req.body, secret),
                "boundary bytes reached the wire from a `read`"
            );
        }
    }
    assert_eq!(sink.events().len(), 6, "one privacy_block per spelling");
    for (_, block) in sink.events() {
        assert_eq!(block.path, CANONICAL_ID);
        assert_eq!(block.action, PrivacyAction::ReroutedToLocal);
    }
    std::fs::remove_dir_all(&root).ok();
}

/// **REQ-571 AC-2.** The same matrix for `edit`, on its own fixture.
///
/// An edit surfaces no file body of its own, but the model that issued it holds
/// the text it replaced — so the turn is scoped by the edited file exactly as a
/// read is, and a `local-only` edit must pin the session however the path was
/// spelled.
#[tokio::test]
async fn edit_blocks_every_boundary_spelling_under_one_identity() {
    use tetond::harness::tools::EditTool;
    use tetond::harness::{Tool, ToolContext};

    let root = spelling_repo("edit");
    let ctx = ToolContext::new(&root);
    let capture = CaptureTransport::default();
    let sink = Arc::new(CapturingSink::default());
    let egress = Egress::new(capture.clone(), local_only_boundaries(), sink.clone());
    let egress_ctx = EgressContext::new("anthropic").with_session("sess-edit-spellings");

    // Positive control: an edit of a public file still goes remote, carrying the
    // file's content.
    let public = EditTool.run(
        &ctx,
        &serde_json::json!({
            "path": "src/main.rs",
            "old_string": "PUBLIC-MARKER-9f3a",
            "new_string": "PUBLIC-MARKER-9f3a-edited",
        }),
    );
    assert!(!public.is_error, "{}", public.content);
    assert_eq!(sole_identity(&public), "src/main.rs");
    let (req, prov) = turn_from(&public, PUBLIC_CONTENT);
    assert!(
        egress.send(req, &prov, &egress_ctx).await.is_ok(),
        "a public edit must still reach the wire"
    );

    let mut identities = Vec::new();
    for spelling in br3_spellings(&root) {
        // Restore the boundary file so each spelling performs a real edit.
        std::fs::write(root.join(CANONICAL_ID), SECRET_ENV).unwrap();
        let out = EditTool.run(
            &ctx,
            &serde_json::json!({
                "path": spelling,
                "old_string": "sk-live-DO-NOT-LEAK-abc123",
                "new_string": "sk-live-DO-NOT-LEAK-rotated",
            }),
        );
        assert!(!out.is_error, "spelling {spelling:?}: {}", out.content);
        identities.push(sole_identity(&out));

        let (req, prov) = turn_from(&out, SECRET_ENV);
        let blocked = egress.send(req, &prov, &egress_ctx).await;
        match blocked {
            Err(EgressError::PrivacyBlocked { ref path, .. }) => assert_eq!(
                path, CANONICAL_ID,
                "spelling {spelling:?} was blocked against a divergent identity"
            ),
            other => panic!("spelling {spelling:?} was not blocked: {other:?}"),
        }
    }

    assert_eq!(
        identities.len(),
        6,
        "the whole BR-3 set must have been driven"
    );
    for (spelling, id) in br3_spellings(&root).iter().zip(&identities) {
        assert_eq!(
            id, CANONICAL_ID,
            "spelling {spelling:?} minted a second identity for one file"
        );
    }

    let captured = capture.captured();
    assert_eq!(captured.len(), 1, "only the public edit may be forwarded");
    assert!(
        contains_bytes(&captured[0].body, PUBLIC_CONTENT),
        "the positive control never went out, so the zero-leak claim is vacuous"
    );
    for req in &captured {
        for secret in [SECRET_ENV, "sk-live"] {
            assert!(
                !contains_bytes(&req.body, secret),
                "boundary bytes reached the wire from an `edit`"
            );
        }
    }
    assert_eq!(sink.events().len(), 6, "one privacy_block per spelling");
    for (_, block) in sink.events() {
        assert_eq!(block.path, CANONICAL_ID);
        assert_eq!(block.action, PrivacyAction::ReroutedToLocal);
    }
    std::fs::remove_dir_all(&root).ok();
}
