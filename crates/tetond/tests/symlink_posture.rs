//! REQ-571 AC-3/AC-4 — symlink posture splits by tool class (BR-5, ADR-C) —
//! AC-6, containment decided on a resolved path (BR-6), and AC-15, the
//! model-facing half of the posture (BR-11).
//!
//! A symlink is a second name for a file, and the two tool classes must answer
//! it differently:
//!
//! - **`read` / `edit`** name one file, so they resolve the link and are judged
//!   by its *target*: inside the root the target is the identity (AC-3), outside
//!   the root the call is refused by the jail.
//! - **`grep` / `glob`** report the name they arrived by, so they skip link
//!   entries entirely, wherever the link resolves (AC-4). Following one would
//!   surface a file under two names — two provenance ids for one identity, the
//!   thing ADR-A exists to prevent — and can cycle.
//!
//! The `read`/`edit` half is proven the way AC-3 words it: through the **real
//! egress choke point**, with a capture transport standing in for the network, so
//! the claim is "these bytes did not leave" rather than "this function returned
//! the string I expected". Each tool gets its own repo, `Egress`, and sink, so
//! neither tool's coverage rides on the other's (LESSON-502).
//!
//! A link is therefore the sharpest case for BR-11 as well: the file `read`
//! opens is genuinely not the one the request names, so unless the output says
//! both, the model is the only party to the exchange that does not know which
//! file it is holding. The AC-15 cases at the foot of this file assert that it
//! is told — and that being told changes nothing about the verdict, since the
//! displayed text and the enforced identity are separate values.
//!
//! AC-6 (BR-6) belongs here too, at the foot of the file: it is a claim about
//! the same resolution step, made about the leaf that does *not* exist yet.
//! Resolving only what exists left a hole — a link out of the root plus an
//! uncreated leaf under it — and closing it must not cost the repo the ability to
//! name a file before creating it, so each of those cases is asserted against the
//! other.
//!
//! Every fixture that asserts an absence pairs it with a positive control — a
//! live link proving the refusal is not merely a broken link, and a public turn
//! proving the wire was not simply closed (LESSON-479).

use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::json;

use teton_core::entities::{BoundaryMode, PrivacyBoundary};
use teton_core::ProvenanceId;
use teton_protocol::events::{PrivacyAction, PrivacyBlock, ProvenanceRejected};
use teton_protocol::SessionId;
use teton_providers::transport::{
    ByteStream, HttpMethod, Transport, TransportError, TransportRequest, TransportResponse,
};

use tetond::egress::{Egress, EgressContext, EgressError, PrivacyEventSink, Provenance};
use tetond::harness::tools::{EditTool, GlobTool, GrepTool, ReadTool};
use tetond::harness::{Tool, ToolContext, ToolOutcome, ToolProvenance};

/// The one identity every in-root spelling — including a link — must collapse to.
const CANONICAL_ID: &str = "secrets/prod.env";

/// Boundary-file bytes. Must never appear in a captured payload.
const SECRET_ENV: &str = "API_KEY=sk-live-DO-NOT-LEAK-abc123 NEEDLE-2f8c";

/// Bytes of a file that lives **outside** the jail. Only a followed link could
/// surface them, so seeing them anywhere is unambiguous.
const OUTSIDE_SECRET: &str = "OUTSIDE-ONLY-hunter2-NEVER-SHIP NEEDLE-2f8c";

/// Public content, and the positive control for every zero-leak assertion.
const PUBLIC_CONTENT: &str = "fn main() { /* NEEDLE-2f8c PUBLIC-MARKER-9f3a */ }";

/// A substring present in all three fixture files, so one `grep` sees every file
/// a walker could possibly reach.
const NEEDLE: &str = "NEEDLE-2f8c";

/// A repo root holding one boundary file and one public file, plus a sibling
/// directory outside the jail holding a file only a followed link can reach.
///
/// The counter, not the timestamp, guarantees uniqueness: `SystemTime::now()`
/// can return the same value for two calls within one clock tick.
struct LinkFixture {
    root: PathBuf,
    outside: PathBuf,
}

impl LinkFixture {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let stamp = format!(
            "{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let root = std::env::temp_dir().join(format!("teton-req571-link-{tag}-{stamp}"));
        let outside = std::env::temp_dir().join(format!("teton-req571-out-{tag}-{stamp}"));
        std::fs::create_dir_all(root.join("secrets")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join(CANONICAL_ID), SECRET_ENV).unwrap();
        std::fs::write(root.join("src/main.rs"), PUBLIC_CONTENT).unwrap();
        std::fs::write(outside.join("outside-secret.txt"), OUTSIDE_SECRET).unwrap();
        Self { root, outside }
    }

    /// Link `name` inside the root to the outside file, and assert the link is
    /// **live** — a refusal below must be the jail's doing, not a dangling link's.
    fn link_out(&self, name: &str) {
        let link = self.root.join(name);
        symlink(self.outside.join("outside-secret.txt"), &link).unwrap();
        assert_eq!(
            std::fs::read_to_string(&link).unwrap(),
            OUTSIDE_SECRET,
            "fixture: the escaping link must actually resolve, or every assertion \
             below is about a broken link instead of the jail"
        );
    }

    /// Link `name` inside the root to the boundary file, and assert it is live.
    fn link_in(&self, name: &str) {
        let link = self.root.join(name);
        symlink(CANONICAL_ID, &link).unwrap();
        assert_eq!(
            std::fs::read_to_string(&link).unwrap(),
            SECRET_ENV,
            "fixture: the in-root link must actually resolve to the boundary file"
        );
    }

    /// Link `name` inside the root to the outside **directory**, and assert it is
    /// live. The AC-6 shape: the link resolves, so a leaf named under it can be
    /// one that does not exist yet.
    fn link_out_dir(&self, name: &str) {
        let link = self.root.join(name);
        symlink(&self.outside, &link).unwrap();
        assert_eq!(
            link.canonicalize().unwrap(),
            self.outside.canonicalize().unwrap(),
            "fixture: the directory link must actually leave the root, or every \
             assertion below is about a path that was never dangerous"
        );
    }

    /// Link `name` inside the root to a file outside the root that does **not**
    /// exist — the dangling case, where the link entry is real but resolves
    /// nowhere.
    fn dangling_link_out(&self, name: &str) {
        let link = self.root.join(name);
        symlink(self.outside.join("never-created.txt"), &link).unwrap();
        assert!(
            link.symlink_metadata().is_ok(),
            "fixture: the link entry itself must exist"
        );
        assert!(
            link.canonicalize().is_err(),
            "fixture: the link must dangle, or this is the already-covered \
             resolved-link case"
        );
    }

    fn cleanup(&self) {
        std::fs::remove_dir_all(&self.root).ok();
        std::fs::remove_dir_all(&self.outside).ok();
    }
}

/// A `Transport` that records every request instead of sending it, so "zero
/// boundary bytes left" is asserted over captured payloads.
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
        origin: Default::default(),
    }]
}

fn contains_bytes(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

/// Every identity a tool outcome claims, in sorted order. `Unknown` is a panic:
/// a first-party file tool that cannot say what it touched would make each
/// assertion below vacuous.
fn identities(outcome: &ToolOutcome) -> Vec<String> {
    match &outcome.provenance {
        ToolProvenance::Sources(ids) => ids
            .iter()
            .map(|id| ProvenanceId::as_str(id).to_owned())
            .collect(),
        ToolProvenance::Unknown => {
            panic!("a first-party file tool must never report unknown provenance")
        }
    }
}

/// The single identity a tool outcome claims, or a panic naming what it claimed
/// instead.
fn sole_identity(outcome: &ToolOutcome) -> String {
    let ids = identities(outcome);
    assert_eq!(ids.len(), 1, "expected exactly one source, got {ids:?}");
    ids[0].clone()
}

/// One turn built from a tool outcome exactly as the daemon would scope it: the
/// body carries the bytes the model now holds, and the provenance is the
/// outcome's own, folded through the daemon's single bridge rather than a
/// paraphrase of it.
fn turn_from(outcome: &ToolOutcome, body: &str) -> (TransportRequest, Provenance) {
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

/// Assert the captured traffic is exactly the positive control and carries no
/// protected bytes of either kind.
fn assert_only_the_public_turn_went_out(captured: &[TransportRequest]) {
    assert_eq!(
        captured.len(),
        1,
        "only the public turn may reach the wire, got {} requests",
        captured.len()
    );
    assert!(
        contains_bytes(&captured[0].body, PUBLIC_CONTENT),
        "the positive control never went out, so the zero-leak claim is vacuous"
    );
    for req in captured {
        for secret in [SECRET_ENV, "sk-live", OUTSIDE_SECRET, "hunter2"] {
            assert!(
                !contains_bytes(&req.body, secret),
                "protected bytes reached the wire"
            );
        }
    }
}

/// **AC-3, `read`.** A link inside the repo pointing at a boundary-protected file
/// is attributed to the file it resolves to, the turn carrying it is blocked at
/// the real choke point, and no captured payload holds the protected bytes.
///
/// The identity assertion is the load-bearing half. "It was blocked" would also
/// pass for a tool that tagged the link's own name and got lucky with a glob;
/// what BR-5 requires is that `notes.txt` and `secrets/prod.env` are *one*
/// identity, so a boundary written about the target governs every name it has.
#[tokio::test]
async fn read_attributes_an_in_root_link_to_its_target_and_blocks_the_turn() {
    let fx = LinkFixture::new("read-inside");
    fx.link_in("notes.txt");
    let ctx = ToolContext::new(&fx.root);
    let capture = CaptureTransport::default();
    let sink = Arc::new(CapturingSink::default());
    let egress = Egress::new(capture.clone(), local_only_boundaries(), sink.clone());
    let egress_ctx = EgressContext::new("anthropic").with_session("sess-read-link");

    // Positive control first, so a later "nothing leaked" cannot be satisfied by
    // an egress that refuses everything (LESSON-479).
    let public = ReadTool.run(&ctx, &json!({ "path": "src/main.rs" }));
    assert!(!public.is_error, "{}", public.content);
    assert_eq!(sole_identity(&public), "src/main.rs");
    let (req, prov) = turn_from(&public, PUBLIC_CONTENT);
    assert!(
        egress.send(req, &prov, &egress_ctx).await.is_ok(),
        "a public read must still reach the wire"
    );

    let out = ReadTool.run(&ctx, &json!({ "path": "notes.txt" }));
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains("sk-live"),
        "fixture: the link read must actually surface the target's bytes, or the \
         block below is about nothing"
    );
    assert_eq!(
        sole_identity(&out),
        CANONICAL_ID,
        "the link's own name is not the identity — its resolved target is"
    );

    let (req, prov) = turn_from(&out, SECRET_ENV);
    match egress.send(req, &prov, &egress_ctx).await {
        Err(EgressError::PrivacyBlocked { ref path, .. }) => assert_eq!(path, CANONICAL_ID),
        other => panic!("a read through a link to a boundary file must block: {other:?}"),
    }

    assert_only_the_public_turn_went_out(&capture.captured());
    let events = sink.events();
    assert_eq!(events.len(), 1, "one privacy_block for the link read");
    assert_eq!(
        events[0].0.as_ref(),
        Some(&SessionId::from("sess-read-link"))
    );
    assert_eq!(events[0].1.path, CANONICAL_ID);
    assert_eq!(events[0].1.action, PrivacyAction::ReroutedToLocal);
    fx.cleanup();
}

/// **AC-3, `edit`.** The same claim for `edit`, on its own fixture and its own
/// egress (LESSON-502).
///
/// An edit surfaces no file body of its own, but the model that issued it holds
/// the text it replaced — and the write really does land on the link's target, so
/// the turn is scoped by that target exactly as a read is.
#[tokio::test]
async fn edit_attributes_an_in_root_link_to_its_target_and_blocks_the_turn() {
    let fx = LinkFixture::new("edit-inside");
    fx.link_in("notes.txt");
    let ctx = ToolContext::new(&fx.root);
    let capture = CaptureTransport::default();
    let sink = Arc::new(CapturingSink::default());
    let egress = Egress::new(capture.clone(), local_only_boundaries(), sink.clone());
    let egress_ctx = EgressContext::new("anthropic").with_session("sess-edit-link");

    let public = EditTool::default().run(
        &ctx,
        &json!({
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

    let out = EditTool::default().run(
        &ctx,
        &json!({
            "path": "notes.txt",
            "old_string": "sk-live-DO-NOT-LEAK-abc123",
            "new_string": "sk-live-DO-NOT-LEAK-rotated",
        }),
    );
    assert!(!out.is_error, "{}", out.content);
    // The edit landed on the *target*, not on the link — which is what makes the
    // attribution below a statement about a real write.
    assert!(
        std::fs::read_to_string(fx.root.join(CANONICAL_ID))
            .unwrap()
            .contains("rotated"),
        "fixture: the edit must have gone through the link to the boundary file"
    );
    assert_eq!(
        sole_identity(&out),
        CANONICAL_ID,
        "the link's own name is not the identity — its resolved target is"
    );

    let (req, prov) = turn_from(&out, SECRET_ENV);
    match egress.send(req, &prov, &egress_ctx).await {
        Err(EgressError::PrivacyBlocked { ref path, .. }) => assert_eq!(path, CANONICAL_ID),
        other => panic!("an edit through a link to a boundary file must block: {other:?}"),
    }

    assert_only_the_public_turn_went_out(&capture.captured());
    let events = sink.events();
    assert_eq!(events.len(), 1, "one privacy_block for the link edit");
    assert_eq!(
        events[0].0.as_ref(),
        Some(&SessionId::from("sess-edit-link"))
    );
    assert_eq!(events[0].1.path, CANONICAL_ID);
    fx.cleanup();
}

/// A link whose target is outside the repo root is refused by `read` — the jail,
/// not the model, decides — and the refusal itself carries none of the file's
/// bytes.
#[test]
fn read_refuses_a_link_that_resolves_outside_the_root() {
    let fx = LinkFixture::new("read-outside");
    fx.link_out("escape.txt");
    let ctx = ToolContext::new(&fx.root);

    let out = ReadTool.run(&ctx, &json!({ "path": "escape.txt" }));
    assert!(
        out.is_error,
        "a link out of the jail must be refused, got: {}",
        out.content
    );
    assert!(
        out.content.contains("is outside the session root"),
        "the refusal must be the jail's, not an I/O accident: {}",
        out.content
    );
    assert!(
        !out.content.contains("OUTSIDE-ONLY"),
        "the refusal must not quote the file it refused"
    );
    fx.cleanup();
}

/// The same for `edit`, and the outside file is left untouched — a refusal that
/// still wrote would be the worse half of this bug.
#[test]
fn edit_refuses_a_link_that_resolves_outside_the_root() {
    let fx = LinkFixture::new("edit-outside");
    fx.link_out("escape.txt");
    let ctx = ToolContext::new(&fx.root);

    let out = EditTool::default().run(
        &ctx,
        &json!({
            "path": "escape.txt",
            "old_string": "hunter2",
            "new_string": "OWNED",
        }),
    );
    assert!(
        out.is_error,
        "a link out of the jail must be refused, got: {}",
        out.content
    );
    assert!(
        out.content.contains("is outside the session root"),
        "the refusal must be the jail's: {}",
        out.content
    );
    assert_eq!(
        std::fs::read_to_string(fx.outside.join("outside-secret.txt")).unwrap(),
        OUTSIDE_SECRET,
        "the refused edit must not have written through the link"
    );
    fx.cleanup();
}

/// **AC-4, `grep`.** Both links are skipped: the outside file's content never
/// appears, and neither link name is reported under an in-jail relative path.
///
/// The in-root link is the subtler case. Its target *is* reported — as itself,
/// once, because the walk reaches the real file directly — so what the assertions
/// pin is that one file yielded exactly one identity rather than a second one
/// spelled `inside-link.env`.
#[test]
fn grep_skips_symlinks_wherever_they_resolve() {
    let fx = LinkFixture::new("grep");
    fx.link_in("inside-link.env");
    fx.link_out("outside-link.txt");
    let ctx = ToolContext::new(&fx.root);

    let out = GrepTool.run(&ctx, &json!({ "pattern": NEEDLE }));
    assert!(!out.is_error, "{}", out.content);
    // Positive control: the walk really did run and really did match files, so
    // the absences below are absences and not an empty search.
    assert!(
        out.content.contains("src/main.rs:"),
        "the walk must have found the public file: {}",
        out.content
    );
    assert!(
        out.content.contains("secrets/prod.env:"),
        "the walk must have found the boundary file under its own name: {}",
        out.content
    );

    assert!(
        !out.content.contains("inside-link.env"),
        "an in-root link was reported, giving one file a second identity: {}",
        out.content
    );
    assert!(
        !out.content.contains("outside-link.txt"),
        "a link out of the jail was reported under an in-jail path: {}",
        out.content
    );
    assert!(
        !out.content.contains("OUTSIDE-ONLY"),
        "content from outside the jail was surfaced: {}",
        out.content
    );
    assert_eq!(
        identities(&out),
        vec!["secrets/prod.env".to_owned(), "src/main.rs".to_owned()],
        "exactly the two real files, each once"
    );
    fx.cleanup();
}

/// **AC-4, `glob`.** The same claim for the enumerator, where the reported name
/// *is* the minted identity.
#[test]
fn glob_skips_symlinks_wherever_they_resolve() {
    let fx = LinkFixture::new("glob");
    fx.link_in("inside-link.env");
    fx.link_out("outside-link.txt");
    let ctx = ToolContext::new(&fx.root);

    let out = GlobTool.run(&ctx, &json!({ "pattern": "**" }));
    assert!(!out.is_error, "{}", out.content);
    let listed: Vec<&str> = out.content.lines().collect();
    // Positive control: `**` really does enumerate, so the absences below mean
    // something.
    assert!(
        listed.contains(&"src/main.rs") && listed.contains(&CANONICAL_ID),
        "the walk must have listed both real files: {listed:?}"
    );
    assert!(
        !listed.contains(&"inside-link.env"),
        "an in-root link was listed, giving one file a second identity: {listed:?}"
    );
    assert!(
        !listed.contains(&"outside-link.txt"),
        "a link out of the jail was listed under an in-jail path: {listed:?}"
    );
    assert_eq!(
        identities(&out),
        vec!["secrets/prod.env".to_owned(), "src/main.rs".to_owned()],
        "exactly the two real files, each once"
    );
    fx.cleanup();
}

/// **AC-4, the cycle.** `a -> b`, `b -> a`, plus a directory link pointing at its
/// own ancestor — the shape that turns a link-following walk into an unbounded
/// descent. Both walkers terminate, and the bound is asserted rather than left to
/// the test runner's patience.
///
/// The cycle is unreachable *because* of the skip, which is the point: this test
/// is what fails if a future contributor teaches either walker to follow links
/// without also inventing a visited-set.
#[test]
fn walkers_terminate_on_a_symlink_cycle() {
    /// Generous enough that a slow machine never flakes, tight enough that an
    /// unbounded walk cannot pass.
    const BUDGET: Duration = Duration::from_secs(10);

    let fx = LinkFixture::new("cycle");
    symlink("b", fx.root.join("a")).unwrap();
    symlink("a", fx.root.join("b")).unwrap();
    // A directory link to its own parent: the case that recurses forever if the
    // walkers ever traverse links.
    std::fs::create_dir_all(fx.root.join("deep")).unwrap();
    symlink("..", fx.root.join("deep/up")).unwrap();

    let started = Instant::now();
    let grepped = GrepTool.run(&ToolContext::new(&fx.root), &json!({ "pattern": NEEDLE }));
    let globbed = GlobTool.run(&ToolContext::new(&fx.root), &json!({ "pattern": "**" }));
    let elapsed = started.elapsed();

    assert!(
        elapsed < BUDGET,
        "the walk over a symlink cycle took {elapsed:?}, over the {BUDGET:?} budget"
    );
    // Both walks completed a real traversal rather than bailing out early.
    assert!(!grepped.is_error, "{}", grepped.content);
    assert!(
        grepped.content.contains("src/main.rs:"),
        "the grep must still have walked the tree: {}",
        grepped.content
    );
    assert!(!globbed.is_error, "{}", globbed.content);
    let listed: Vec<&str> = globbed.content.lines().collect();
    assert!(
        listed.contains(&"src/main.rs"),
        "the glob must still have walked the tree: {listed:?}"
    );
    // And the cycle's members are absent from both, under every name.
    for name in ["a", "b", "deep/up"] {
        assert!(
            !listed.contains(&name),
            "cycle member {name:?} was listed: {listed:?}"
        );
        assert!(
            !grepped.content.contains(&format!("{name}:")),
            "cycle member {name:?} was searched: {}",
            grepped.content
        );
    }
    fx.cleanup();
}

/// **AC-15, `read`.** Reading through a link shows the name asked for *and* the
/// name that answered, so the model holding `secrets/prod.env`'s bytes is not
/// told it read `notes.txt`.
///
/// The link is the case that motivates BR-11: `read` returns line-numbered
/// content and nothing else, so before this there was no name in the output at
/// all — the model kept the one from its own request, which is precisely the
/// name that is wrong here.
#[test]
fn read_shows_both_the_link_and_the_target_it_resolved_to() {
    let fx = LinkFixture::new("show-read");
    fx.link_in("notes.txt");
    let ctx = ToolContext::new(&fx.root);

    let out = ReadTool.run(&ctx, &json!({ "path": "notes.txt" }));
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains("notes.txt"),
        "the request the model made is missing from the answer: {}",
        out.content
    );
    assert!(
        out.content.contains(CANONICAL_ID),
        "the file actually read is missing from the answer: {}",
        out.content
    );
    // Display is display: the identity egress judges this turn by is the target's
    // alone, exactly as it was before the note existed.
    assert_eq!(sole_identity(&out), CANONICAL_ID);
    fx.cleanup();
}

/// **AC-15, `edit`.** The same for a write that landed through a link — the more
/// consequential half, since the model is about to verify a change to a file it
/// may believe is somewhere else.
#[test]
fn edit_shows_both_the_link_and_the_target_it_wrote() {
    let fx = LinkFixture::new("show-edit");
    fx.link_in("notes.txt");
    let ctx = ToolContext::new(&fx.root);

    let out = EditTool::default().run(
        &ctx,
        &json!({
            "path": "notes.txt",
            "old_string": "sk-live-DO-NOT-LEAK-abc123",
            "new_string": "sk-live-DO-NOT-LEAK-rotated",
        }),
    );
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains("notes.txt") && out.content.contains(CANONICAL_ID),
        "the success line must name both the request and the file written: {}",
        out.content
    );
    // The write really did land on the target, so the name shown is a fact about
    // this edit and not a decoration.
    assert!(
        std::fs::read_to_string(fx.root.join(CANONICAL_ID))
            .unwrap()
            .contains("rotated"),
        "fixture: the edit must have gone through the link"
    );
    assert_eq!(sole_identity(&out), CANONICAL_ID);
    fx.cleanup();
}

/// **AC-15, the common case — byte-identical.** When the request already spells
/// the file that answered it, both tools render exactly what they rendered
/// before BR-11.
///
/// Asserted by exact comparison against the literal rendering, never
/// `contains`: "still contains the old text" is what a stray note bolted on top
/// would also satisfy, and the overwhelming majority of real calls take this
/// path — a note here would be a per-turn tax on every read in every session.
#[test]
fn a_matching_request_renders_byte_identically() {
    let fx = LinkFixture::new("byte-identical");
    // Links exist in the tree but are not what is asked for: a plain request
    // stays plain even in a repo that contains links.
    fx.link_in("notes.txt");
    fx.link_out("escape.txt");
    let ctx = ToolContext::new(&fx.root);

    let out = ReadTool.run(&ctx, &json!({ "path": "src/main.rs" }));
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(out.content, format!("     1\t{PUBLIC_CONTENT}\n"));

    let out = EditTool::default().run(
        &ctx,
        &json!({
            "path": "src/main.rs",
            "old_string": "PUBLIC-MARKER-9f3a",
            "new_string": "PUBLIC-MARKER-9f3a-edited",
        }),
    );
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        out.content,
        "edited `src/main.rs`: replaced 1 occurrence. Verify the change before finishing."
    );
    fx.cleanup();
}

/// **BR-11, display is not provenance.** One file read twice — once under a name
/// that diverges, once under its own — renders *differently* and is judged
/// *identically*: same minted id, same block, same reported path.
///
/// This is the structural claim exercised end to end. `with_paths` takes
/// [`ProvenanceId`]s and not strings (ADR-A), so no text assembled for the model
/// can reach the boundary matcher; what this test adds is that the property
/// survives at the real choke point, which is where a future "helpful" change
/// that derived the tagged path from the displayed one would be caught.
#[tokio::test]
async fn divergent_display_cannot_move_the_boundary_verdict() {
    let fx = LinkFixture::new("display-verdict");
    fx.link_in("notes.txt");
    let ctx = ToolContext::new(&fx.root);
    let capture = CaptureTransport::default();
    let sink = Arc::new(CapturingSink::default());
    let egress = Egress::new(capture.clone(), local_only_boundaries(), sink.clone());
    let egress_ctx = EgressContext::new("anthropic").with_session("sess-display");

    // Positive control first (LESSON-479).
    let public = ReadTool.run(&ctx, &json!({ "path": "src/main.rs" }));
    assert!(!public.is_error, "{}", public.content);
    let (req, prov) = turn_from(&public, PUBLIC_CONTENT);
    assert!(
        egress.send(req, &prov, &egress_ctx).await.is_ok(),
        "a public read must still reach the wire"
    );

    let via_link = ReadTool.run(&ctx, &json!({ "path": "notes.txt" }));
    let direct = ReadTool.run(&ctx, &json!({ "path": CANONICAL_ID }));
    assert!(!via_link.is_error, "{}", via_link.content);
    assert!(!direct.is_error, "{}", direct.content);
    assert_ne!(
        via_link.content, direct.content,
        "fixture: the two spellings must actually render differently, or the \
         equality below is a claim about nothing"
    );
    assert_eq!(
        via_link.provenance, direct.provenance,
        "the displayed text moved the identity — display and provenance are \
         supposed to be separate values"
    );

    for (label, out) in [
        ("through the link", &via_link),
        ("by its own name", &direct),
    ] {
        let (req, prov) = turn_from(out, SECRET_ENV);
        match egress.send(req, &prov, &egress_ctx).await {
            Err(EgressError::PrivacyBlocked { ref path, .. }) => {
                assert_eq!(path, CANONICAL_ID, "read {label}");
            }
            other => panic!("a read {label} must block identically: {other:?}"),
        }
    }

    assert_only_the_public_turn_went_out(&capture.captured());
    let events = sink.events();
    assert_eq!(events.len(), 2, "one privacy_block per blocked spelling");
    for (_, block) in &events {
        assert_eq!(block.path, CANONICAL_ID);
    }
    fx.cleanup();
}

/// **AC-6 (BR-6), `read`.** A leaf that does not exist *yet*, named under a link
/// that leaves the root, is refused — and the same tool, in the same repo, still
/// accepts a leaf that does not exist yet under a real directory.
///
/// The pairing is the test. Before BR-6 the jail canonicalized and, when that
/// failed because the target was not there, fell back to the **lexical** path —
/// which spells `escape-dir/new.txt` inside the root, passes `starts_with`, and
/// leaves the OS to follow the link on open. Refusing everything unresolvable
/// would also close that hole and would break creating files in the repo, so the
/// second half of this test is what says the fix is the right one.
#[test]
fn read_refuses_a_new_leaf_through_an_escaping_link_and_keeps_accepting_one_in_the_repo() {
    let fx = LinkFixture::new("read-new-leaf");
    fx.link_out_dir("escape-dir");
    let ctx = ToolContext::new(&fx.root);

    // Control: the link really does reach outside content, so the refusals below
    // are about a live escape route and not a dead one.
    let existing = ReadTool.run(&ctx, &json!({ "path": "escape-dir/outside-secret.txt" }));
    assert!(existing.is_error, "{}", existing.content);
    assert!(
        existing.content.contains("is outside the session root"),
        "an existing file through a directory link must be the jail's refusal: {}",
        existing.content
    );

    // AC-6: the leaf does not exist, so canonicalizing the whole path fails.
    let out = ReadTool.run(&ctx, &json!({ "path": "escape-dir/new.txt" }));
    assert!(
        out.is_error,
        "a new leaf under an escaping link must be refused, got: {}",
        out.content
    );
    assert!(
        out.content.contains("is outside the session root"),
        "the refusal must be the jail's, decided on the resolved path: {}",
        out.content
    );
    assert!(
        !out.content.contains("OUTSIDE-ONLY"),
        "the refusal must not quote anything from outside the jail"
    );

    // The over-refusal guard: a not-yet-existing file under a genuine in-root
    // directory clears the jail. `read` then fails on the *open*, which is a
    // different sentence — the one a `write` tool would not fail on at all.
    let missing = ReadTool.run(&ctx, &json!({ "path": "src/not-created-yet.rs" }));
    assert!(missing.is_error, "{}", missing.content);
    assert!(
        !missing.content.contains("is outside the session root"),
        "a new file in the repo was refused by the jail — the fix over-refuses: {}",
        missing.content
    );
    assert!(
        missing.content.contains("could not read"),
        "the failure must be the open, not the jail: {}",
        missing.content
    );
    fx.cleanup();
}

/// **AC-6, `edit`.** The write half, stated as the thing that actually matters:
/// nothing lands outside the root.
///
/// `edit` cannot exploit this today — it reads before it writes, so the open
/// fails first — which is exactly why this is closed prospectively. The assertion
/// is therefore about the jail's verdict *and* about the outside directory being
/// untouched, so the day a `write`/`create` tool joins the set, the refusal it
/// inherits is already proven.
#[test]
fn edit_refuses_to_reach_a_new_leaf_through_an_escaping_link() {
    let fx = LinkFixture::new("edit-new-leaf");
    fx.link_out_dir("escape-dir");
    let ctx = ToolContext::new(&fx.root);

    // Positive control: this tool, in this repo, does edit files.
    let public = EditTool::default().run(
        &ctx,
        &json!({
            "path": "src/main.rs",
            "old_string": "PUBLIC-MARKER-9f3a",
            "new_string": "PUBLIC-MARKER-9f3a-edited",
        }),
    );
    assert!(!public.is_error, "{}", public.content);

    let out = EditTool::default().run(
        &ctx,
        &json!({
            "path": "escape-dir/new.txt",
            "old_string": "anything",
            "new_string": "OWNED",
        }),
    );
    assert!(
        out.is_error,
        "a new leaf under an escaping link must be refused, got: {}",
        out.content
    );
    assert!(
        out.content.contains("is outside the session root"),
        "the refusal must be the jail's: {}",
        out.content
    );
    assert!(
        !fx.outside.join("new.txt").exists(),
        "the refused edit created a file outside the repo root"
    );
    assert_eq!(
        std::fs::read_to_string(fx.outside.join("outside-secret.txt")).unwrap(),
        OUTSIDE_SECRET,
        "the refused edit must not have written through the link"
    );
    fx.cleanup();
}

/// **The dangling link — TASK-120 flagged it, BR-6 closes it.** The link entry
/// exists, so `canonicalize` fails on the link *itself*; the discarded lexical
/// fallback therefore minted `notes.txt` — an in-jail identity — for a file that
/// lives outside the root the moment anyone creates it.
///
/// Nothing leaked then (the read failed next) and nothing leaks now; what changed
/// is that the jail no longer answers a path it cannot resolve, so no id is minted
/// under a name the daemon never opened (ADR-B). Paired with a live link, whose
/// read still succeeds — the refusal is about resolution failing, not about links.
#[test]
fn a_dangling_link_is_refused_rather_than_minted_under_its_own_name() {
    let fx = LinkFixture::new("dangling");
    fx.link_in("live.txt");
    fx.dangling_link_out("notes.txt");
    let ctx = ToolContext::new(&fx.root);

    // Control: a link that resolves is still answered, by its target's identity.
    let live = ReadTool.run(&ctx, &json!({ "path": "live.txt" }));
    assert!(!live.is_error, "{}", live.content);
    assert_eq!(sole_identity(&live), CANONICAL_ID);

    for out in [
        ReadTool.run(&ctx, &json!({ "path": "notes.txt" })),
        EditTool::default().run(
            &ctx,
            &json!({
                "path": "notes.txt",
                "old_string": "anything",
                "new_string": "OWNED",
            }),
        ),
    ] {
        assert!(
            out.is_error,
            "a dangling link must be refused, got: {}",
            out.content
        );
        assert!(
            out.content.contains("broken symlink"),
            "the refusal must name why the path has no answer: {}",
            out.content
        );
        assert!(
            matches!(&out.provenance, ToolProvenance::Sources(ids) if ids.is_empty()),
            "a refused path minted an identity: {:?}",
            out.provenance
        );
    }
    // And nothing was created at the link's target.
    assert!(
        !fx.outside.join("never-created.txt").exists(),
        "the refused call wrote through the dangling link"
    );
    fx.cleanup();
}

// ---------------------------------------------------------------------------
// REQ-585 BR-1 — the third posture: skill discovery
// ---------------------------------------------------------------------------
//
// This file's subject is that a symlink is a second name for a file and each
// tool class has to answer that differently. REQ-585 adds a third class, and it
// splits the question in a way neither of the other two does:
//
// | class | a symlinked **root** | a symlinked **entry** |
// |---|---|---|
// | `read` / `edit` | followed, judged by the target | followed, judged by the target |
// | `grep` / `glob` | (walked from the root given) | skipped, wherever it resolves |
// | skill discovery | **followed** | **never followed** |
//
// The root half is not a concession: the dogfood machine's `~/.claude/skills`
// *is* a symlink into a checkout, and a loader that refused it would find
// nothing on the machine this REQ was written for. The entry half is the
// narrowing, and its reason is the one ADR-A gives for `grep`/`glob`: a followed
// entry link registers one file under two names — two `/`-commands, two
// permission keys (`skill:user:<name>`) and two identities for one skill.
//
// The contrast is asserted rather than described: the very bytes discovery
// declines to register a second time are bytes `read` hands over on request.

/// **BR-1's narrowing, beside the postures it narrows from.** A symlinked root
/// is followed; a symlinked entry under it is skipped with its reason, even when
/// its target is a perfectly good skill directory inside the same tree — which
/// `read` will open by that same name, and does, in the second half of this test.
#[test]
fn skill_discovery_follows_a_symlinked_root_and_never_a_symlinked_entry() {
    let base = std::env::temp_dir().join(format!(
        "teton-skilllink-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    let _ = std::fs::remove_dir_all(&base);
    let shelf = base.join("shelf");
    let home = base.join("home");
    let repo = base.join("repo");
    std::fs::create_dir_all(shelf.join("one")).unwrap();
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(
        shelf.join("one/SKILL.md"),
        "---\ndescription: the real one\n---\nSKILL-BODY-MARKER\n",
    )
    .unwrap();
    // The dogfood shape: the whole `skills` root is a link into a checkout.
    symlink(&shelf, home.join(".claude/skills")).unwrap();
    // …and inside it, an entry that is a link to a directory holding a
    // genuinely valid skill. Nothing about the target is wrong; being reached
    // by a second name is.
    let entry_link = shelf.join("two");
    symlink(shelf.join("one"), &entry_link).unwrap();

    // Positive controls: both links really resolve, so neither half below is
    // about a broken link.
    assert!(
        home.join(".claude/skills/one/SKILL.md")
            .canonicalize()
            .is_ok(),
        "fixture: the root link must resolve"
    );
    assert!(
        entry_link.join("SKILL.md").canonicalize().is_ok(),
        "fixture: the entry link must resolve to a real skill file"
    );

    let registry = tetond::skills::discover(
        Some(&home),
        &repo,
        teton_protocol::methods::RootKind::Project,
        &tetond::skills::RealFs,
    );

    // The root was followed …
    let names: Vec<&str> = registry
        .skills()
        .iter()
        .map(|skill| skill.name.as_str())
        .collect();
    assert_eq!(
        names,
        vec!["one"],
        "the skill behind the symlinked root must register, and the one behind \
         the symlinked entry must not — a second name for a file is a second \
         `/`-command and a second permission key for it"
    );
    // … and the entry was not, with its reason named rather than dropped.
    let skipped: Vec<(std::path::PathBuf, String)> = registry
        .skipped()
        .iter()
        .map(|entry| (entry.path.clone(), entry.reason.to_string()))
        .collect();
    assert_eq!(
        skipped,
        vec![(
            // Named as discovery *opened* it — through the root it was globbed
            // under, not through the checkout the root resolves into. That is
            // the path a user would go and look at.
            home.join(".claude/skills/two"),
            "symlink not followed".to_owned(),
        )],
        "the skipped entry is named with why, and it is the only diagnostic"
    );

    // The contrast that makes this a *narrowing*: `read`, jailed to the same
    // tree, opens the file through the very name discovery refused — and hands
    // back the target's bytes under the target's identity, as AC-3 requires.
    let ctx = ToolContext::new(&shelf);
    let out = ReadTool.run(&ctx, &json!({ "path": "two/SKILL.md" }));
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains("SKILL-BODY-MARKER"),
        "the read through the link must surface the target's bytes: {}",
        out.content
    );
    assert_eq!(
        sole_identity(&out),
        "one/SKILL.md",
        "`read` resolves the link and is judged by its target — the posture \
         discovery deliberately does not share"
    );

    std::fs::remove_dir_all(&base).ok();
}
