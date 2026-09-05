//! **REQ-587 BR-10 / ADR-8 — the `skill` tool's egress boundary, both rules.**
//!
//! `boundary_coverage.rs` requires that every tool which can surface external or
//! file content carries a boundary test, and `skill` surfaces the most
//! consequential thing in the harness: a file the model is told to **follow**.
//! It is also the one tool whose provenance has to be set by hand, because
//! `ToolOutcome::ok` defaults to `Sources(∅)` — the `teton_docs` posture, right
//! for bodies compiled into the binary and **fail-open** for a body read off a
//! disk this session's jail cannot name.
//!
//! Two rules, asserted separately because they are two rules (BR-10, as
//! **REQ-619 BR-3/BR-6 amended the second**):
//!
//! - a **project** skill is under the session root, mints a root-relative
//!   identity, and pins the turn exactly as a `read` of that file would;
//! - a **user** skill (`~/.claude/…`) mints a `~`-scoped identity and is judged
//!   by the same globs on the same terms: it leaves under a boundary that names
//!   nothing it touches, and is refused **naming its own file** under one the
//!   user wrote over their skills directory.
//!
//! The second rule used to read *"has no root-relative identity at all, so its
//! block is `Unknown` and pins the turn wherever any boundary is configured"*.
//! That was REQ-587 BR-10's deliberate strictness, and with REQ-597's thirteen
//! builtins permanently in force its consequence was that every user-authored
//! skill pinned every repo-rooted session on every machine (BUG-214). REQ-619
//! retired it; the tests below flipped rather than went.
//!
//! What has **not** changed is the reason this file exists: `ToolOutcome::ok`
//! defaults to `Sources(∅)` — "touched no repo file" — and under that default a
//! `~/.claude` body matches no glob at all, so the refusal half of the user rule
//! is what catches it. The fixtures still assert both directions on the same
//! file, because a build that answered `Sources(∅)` would pass the leave half.
//!
//! Each test drives the **real** OpenAI-compatible adapter through the **real**
//! egress choke point in front of a capture transport, on `provenance_egress.rs`'s
//! shape: turn 1 is a scripted model-issued `skill` call, turn 2 is the one that
//! would carry its result to the wire.
//!
//! A control leg keeps the refusals meaning something: the same user skill, in a
//! session with **no** boundary configured, reaches the provider.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use teton_core::effort::{EffortLevel, ResolvedEffort};
use teton_core::entities::{BoundaryMode, PrivacyBoundary};
use teton_protocol::events::Event;
use teton_protocol::methods::RootKind;
use teton_protocol::SessionId;
use teton_providers::transport::{Transport, TransportError, TransportRequest, TransportResponse};
use teton_providers::{OpenAiCompatAdapter, OpenAiCompatConfig};

use tetond::broadcast::EventBus;
use tetond::egress::Egress;
use tetond::grants::GrantRegistry;
use tetond::harness::tools::register_skill_tool;
use tetond::harness::{
    build_system_prompt, context_provenance, run_session_turn_with_source, ContextManager,
    DutyRoute, HarnessConfig, HarnessError, NoopProvenanceHook, PendingPermissions,
    PermissionConfig, PermissionGate, PermissionPolicy, RemoteProviderSource, SessionEvents,
    ToolContext, ToolDuties, ToolRegistry,
};
use tetond::skills::{discover, RealFs, SkillRegistry};

/// The boundary-file secret that must never reach the capture transport.
const SECRET: &str = "API_KEY=sk-live-DO-NOT-LEAK-skillbnd-Qw4";

/// A marker the skill body carries, so a leg can tell "the expansion landed"
/// from "the tool refused and said so".
const BODY_MARKER: &str = "MARKER-skill-body-reached-the-model";

/// A marker the skill file's **frontmatter** carries, so the roster legs can
/// tell "the catalogue landed" from "something else did".
///
/// It is `description:` and not body text on purpose: the roster and every
/// refusal that carries it emit this string out of `SKILL.md` without any body
/// ever being expanded, which is exactly the file-authored content ADR-8's
/// argument was never applied to.
const LISTING_MARKER: &str = "MARKER-skill-description-reached-the-model";

/// The one prompt every fixture here opens with.
const PROMPT: &str = "Run the validation skill and summarize what it says.";

/// The one directory every suite that needs a real `$HOME` builds under
/// (REQ-619 verify, m7) — one named parent, rather than a differently-prefixed
/// family per suite that only its own author would recognize.
const FIXTURE_HOME_PARENT: &str = ".teton-test-fixtures";

/// What a suite says when it cannot find a `$HOME` to build under. Verbatim in
/// `provenance_egress.rs` and `harness::tools::skill` too, and a **panic** in
/// all three: the alternative is a test that reports success for having
/// asserted nothing (LESSON-594).
const NEEDS_A_HOME: &str = "this fixture needs a real $HOME: a user skill's identity is minted \
     against `session_root::home()`, so a run without one would be asserting \
     about a scope that does not exist (REQ-619 BR-3)";

// ---------------------------------------------------------------------------
// transport (the `provenance_egress.rs` shape)
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct CaptureSse {
    bodies: Arc<Mutex<VecDeque<String>>>,
    sent: Arc<Mutex<Vec<Vec<u8>>>>,
    calls: Arc<AtomicUsize>,
}

impl CaptureSse {
    fn with_bodies(bodies: Vec<String>) -> Self {
        Self {
            bodies: Arc::new(Mutex::new(bodies.into_iter().collect())),
            sent: Arc::new(Mutex::new(Vec::new())),
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }
    fn captured(&self) -> Vec<Vec<u8>> {
        self.sent.lock().unwrap().clone()
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Transport for CaptureSse {
    async fn execute(
        &self,
        request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.sent.lock().unwrap().push(request.body.clone());
        let body = self
            .bodies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| "data: [DONE]\n\n".to_owned());
        Ok(TransportResponse {
            location: None,
            status: 200,
            body: Box::pin(futures::stream::once(async move { Ok(body.into_bytes()) })),
        })
    }
}

/// One OpenAI-compatible streaming turn: a text delta, an optional tool call,
/// then usage + `[DONE]`.
fn sse_turn(text: &str, tool: Option<(&str, &str, &str)>) -> String {
    let mut s = String::new();
    let chunk = serde_json::json!({ "choices": [{ "delta": { "content": text } }] });
    s.push_str(&format!("data: {chunk}\n\n"));
    if let Some((id, name, args)) = tool {
        let chunk = serde_json::json!({
            "choices": [{ "delta": { "tool_calls": [{
                "index": 0, "id": id, "function": { "name": name, "arguments": args }
            }]}}]
        });
        s.push_str(&format!("data: {chunk}\n\n"));
        let finish =
            serde_json::json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] });
        s.push_str(&format!("data: {finish}\n\n"));
    } else {
        let finish = serde_json::json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] });
        s.push_str(&format!("data: {finish}\n\n"));
    }
    let usage = serde_json::json!({ "usage": { "prompt_tokens": 10, "completion_tokens": 5 } });
    s.push_str(&format!("data: {usage}\n\n"));
    s.push_str("data: [DONE]\n\n");
    s
}

// ---------------------------------------------------------------------------
// fixture
// ---------------------------------------------------------------------------

/// A throwaway pair of trees: a `home` (the stand-in for `~`) and a `repo`.
///
/// `home` is handed to `discover` as a **parameter**, never read from the
/// environment: a suite that set `HOME` would be a suite whose result depends on
/// what else is running in the same process (LESSON-540).
///
/// **The home half is built under the process's own `$HOME`** (REQ-619
/// TASK-401). A user skill's identity is minted against `session_root::home()`,
/// which reads `HOME`, and BR-3's claims here are about that id. The rule above
/// still holds and is the reason for the placement rather than an exception to
/// it: the way to put a fixture file *under* the home the daemon will use,
/// without writing process-wide state every other test in this binary reads, is
/// to build the fixture there.
///
/// **The repo half is not** (REQ-619 verify, m7). It used to sit beside the
/// home under `$HOME`, on the reasoning that a project id is relative to the
/// session root and never to the home — which is true, and is a statement about
/// *ids* rather than about where a file holding
/// `API_KEY=sk-live-DO-NOT-LEAK-…` belongs. Only the user skill needs the home;
/// a repository with a planted credential in it belongs in a temp root, where a
/// killed run leaves it somewhere the operating system clears. The guard below
/// removes both either way.
struct Fixture {
    /// The home half, under the real `$HOME` because the mint reads it.
    root: PathBuf,
    /// The repo half, under a temp root because nothing about it needs a home.
    repo_root: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| !home.as_os_str().is_empty())
            .expect(NEEDS_A_HOME);
        let root = home
            .join(FIXTURE_HOME_PARENT)
            .join(format!("skill-boundary-{tag}-{pid}-{seq}"));
        let repo_root = std::env::temp_dir().join(format!("teton-skillbnd-{tag}-{pid}-{seq}"));
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(repo_root.join("repo").join("secrets")).unwrap();
        std::fs::write(
            repo_root.join("repo").join("secrets").join("prod.env"),
            format!("{SECRET}\n"),
        )
        .unwrap();
        Self { root, repo_root }
    }

    /// The `~`-scoped id `<home>/.claude/skills/<name>/SKILL.md` mints.
    ///
    /// Composed from the fixture's own layout, never by calling the minter
    /// under test — an expectation built with `from_home_resolved` would agree
    /// with any implementation of it, including a wrong one.
    fn user_skill_id(&self, name: &str) -> String {
        let home = std::fs::canonicalize(std::env::var_os("HOME").map(PathBuf::from).unwrap())
            .expect("the home resolves");
        let file = std::fs::canonicalize(
            self.home()
                .join(".claude")
                .join("skills")
                .join(name)
                .join("SKILL.md"),
        )
        .expect("the fixture skill file");
        format!(
            "~/{}",
            file.strip_prefix(&home)
                .expect("the fixture is built under the home")
                .display()
        )
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn repo(&self) -> PathBuf {
        self.repo_root.join("repo")
    }

    fn skill(&self, base: &Path, name: &str) {
        self.skill_with(base, name, "");
    }

    /// [`Self::skill`] with `extra` appended to the body — a `` !`command` ``
    /// line, for the legs whose subject is a preamble's verdict (REQ-619 BR-1).
    fn skill_with(&self, base: &Path, name: &str, extra: &str) {
        let dir = base.join(".claude").join("skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!(
                "---\nname: {name}\ndescription: {LISTING_MARKER}\n---\n\
                 {BODY_MARKER}\nDo the thing.\n{extra}"
            ),
        )
        .unwrap();
    }

    fn registry(&self) -> Arc<SkillRegistry> {
        Arc::new(discover(
            Some(&self.home()),
            &self.repo(),
            RootKind::Plain,
            &RealFs,
        ))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).ok();
        std::fs::remove_dir_all(&self.repo_root).ok();
    }
}

fn boundaries(glob: &str) -> Vec<PrivacyBoundary> {
    vec![PrivacyBoundary {
        path_glob: glob.to_owned(),
        mode: BoundaryMode::LocalOnly,
        origin: Default::default(),
    }]
}

fn contains_bytes(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

/// What one scripted turn pair produced.
struct Run {
    result: Result<tetond::harness::TurnOutcome, HarnessError>,
    captured: Vec<Vec<u8>>,
    calls: usize,
    blocks: Vec<teton_protocol::events::PrivacyBlock>,
    ctx: ContextManager,
    provenance_is_unknown: bool,
    provenance_len: usize,
}

/// Drive turn 1 (the model's `skill` call for `name`) and turn 2 with `glob` as
/// the session's only boundary.
async fn run_skill_call(fx: &Fixture, name: &str, glob: Option<&str>) -> Run {
    run_skill_args(fx, &format!(r#"{{"name":"{name}"}}"#), glob).await
}

/// Drive turn 1 (the model's `skill` call, with `args` verbatim as its argument
/// object) and turn 2 (the one that would carry its result to the wire) with
/// `glob` as the session's only boundary.
///
/// Taken as a raw string rather than a name, because the non-expansion results
/// are reached by argument objects a name cannot express: `{}` is the roster,
/// and a name nothing registers is `unknown_skill`.
async fn run_skill_args(fx: &Fixture, args: &str, glob: Option<&str>) -> Run {
    let session_id = SessionId::from("skillbnd");
    let transport = CaptureSse::with_bodies(vec![
        sse_turn("Invoking the skill.", Some(("c1", "skill", args))),
        sse_turn("should never send", None),
    ]);
    let capture = transport.clone();

    let bus = Arc::new(EventBus::new());
    let egress = Egress::new(
        transport,
        glob.map(boundaries).unwrap_or_default(),
        bus.clone(),
    );
    let provider = OpenAiCompatAdapter::new(OpenAiCompatConfig::new(
        "deepseek",
        "https://api.deepseek.com/v1/chat/completions",
    ));
    let mut source = RemoteProviderSource::new(
        &provider,
        &egress,
        "deepseek",
        "deepseek-chat",
        session_id.clone(),
        ResolvedEffort::effort(EffortLevel::High),
    );

    let config = HarnessConfig::for_strong_model();
    let pending = Arc::new(PendingPermissions::new());
    let gate = Arc::new(PermissionGate::new(
        session_id.clone(),
        // Allow, so the BR-4 acknowledgment settles by level and nothing here
        // waits on a prompt no client is attached to answer.
        PermissionConfig::with_default(PermissionPolicy::Allow),
        Arc::clone(&bus),
        Arc::clone(&pending),
    ));

    let mut tools = ToolRegistry::with_builtins();
    let grants = GrantRegistry::default();
    assert!(
        register_skill_tool(
            &mut tools,
            fx.registry(),
            Arc::clone(&gate),
            Some(grants.next_connection_id()),
            tokio::runtime::Handle::current(),
            1_000,
        ),
        "the fixture registry holds a model-invocable skill, so the tool registers"
    );

    let mut ctx = ContextManager::new(
        build_system_prompt(&tools, &config),
        config.context_budget_tokens,
    );
    ctx.push_user(PROMPT);

    let tool_ctx = ToolContext::new(fx.repo());
    let events = SessionEvents::new(Arc::clone(&bus), session_id.clone());
    let mut hook = NoopProvenanceHook;
    let mut sub = bus.subscribe(256);

    let result = run_session_turn_with_source(
        &mut source,
        &tools,
        &tool_ctx,
        &gate,
        &events,
        &mut ctx,
        &config,
        &mut hook,
        &DutyRoute::unresolved("no digest route in this test"),
        &DutyRoute::unresolved("no compact route in this test"),
        &ToolDuties {
            triage: &DutyRoute::unresolved("no triage route in this test"),
            shell: &DutyRoute::unresolved("no shell route in this test"),
        },
    )
    .await;

    let mut blocks = Vec::new();
    while let Ok(Some(env)) =
        tokio::time::timeout(std::time::Duration::from_millis(50), sub.recv()).await
    {
        if let Event::PrivacyBlock(pb) = env.event {
            blocks.push(pb);
        }
    }

    let provenance = context_provenance(&ctx);
    Run {
        result,
        captured: capture.captured(),
        calls: capture.calls(),
        blocks,
        provenance_is_unknown: provenance.is_unknown(),
        provenance_len: provenance.len(),
        ctx,
    }
}

/// The expansion really landed in context — the positive control every negative
/// claim below needs (LESSON-479), because "nothing leaked" is trivially true of
/// a turn where the tool refused.
fn assert_the_expansion_landed(run: &Run) {
    use tetond::harness::context::Provenance;
    let folded = run
        .ctx
        .blocks()
        .iter()
        .rev()
        .find(|b| matches!(b.provenance, Provenance::Tool { .. }))
        .map(|b| b.text.clone())
        .expect("the skill result was folded into context");
    assert!(
        folded.contains(BODY_MARKER),
        "the skill body never reached the model, so this test asserts nothing about \
         what its provenance protects: {folded}"
    );
    assert!(
        folded.contains("<skill-body"),
        "BR-4's instructions frame is missing from the folded block: {folded}"
    );
    assert!(
        // REQ-619 TASK-401: the *envelope* the expansion itself wears, which is
        // the head of the block. A preamble's output is spliced further down
        // inside its own `<tool-result … trust="untrusted">` — that is REQ-585
        // BR-6 working, not this rule failing — so the scan is bounded to the
        // frame rather than run over the whole fold.
        !folded[..folded.find("<tool-result").unwrap_or(folded.len())]
            .contains("trust=\"untrusted\""),
        "an expansion must never wear the untrusted envelope — its closing sentence \
         forbids following the instructions the block *is* (BR-4): {folded}"
    );
}

// ---------------------------------------------------------------------------
// BR-10 rule one — a project skill mints and pins
// ---------------------------------------------------------------------------

/// **A project skill is under the root, mints a root-relative identity, and
/// pins the turn exactly as reading that file would.**
///
/// The boundary here names the project's own skill directory, which is what
/// makes the claim testable: the expansion's identity has to *match a glob*, and
/// only a minted, root-relative id can.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_project_skill_mints_its_identity_and_pins_the_next_remote_turn() {
    let fx = Fixture::new("project");
    fx.skill(&fx.repo(), "validate");

    let run = run_skill_call(&fx, "validate", Some(".claude/**")).await;
    assert_the_expansion_landed(&run);

    assert!(
        !run.provenance_is_unknown,
        "a project skill has a root-relative identity; `Unknown` would be the *user* \
         rule applied to the wrong source"
    );
    assert_eq!(
        run.provenance_len, 1,
        "exactly the one file the body came from"
    );
    match &run.result {
        Err(e) if e.is_privacy_blocked() => {}
        other => panic!("expected a privacy block, got {other:?}"),
    }
    assert_eq!(run.blocks.len(), 1, "exactly one privacy_block");
    assert_eq!(
        run.calls, 1,
        "turn 2 must never reach the transport at all: {} requests",
        run.calls
    );
    for body in &run.captured {
        assert!(
            !contains_bytes(body, BODY_MARKER),
            "the skill body leaked into captured egress"
        );
    }
}

// ---------------------------------------------------------------------------
// REQ-619 BR-3/BR-6 — a user skill leaves by default and is refused by a glob
// that names it
// ---------------------------------------------------------------------------

/// **A user skill's file has an identity, and egress judges it exactly as it
/// judges a project skill's.**
///
/// This was `a_user_skill_is_unknown_and_pins_under_a_boundary_it_never_touched`,
/// and it asserted REQ-587 BR-10's second rule: `~/.claude/…` had no
/// root-relative identity, so its expansion was `Unknown` and pinned the turn
/// wherever **any** boundary was configured — related or not, and stricter than
/// a `read` of the same bytes. REQ-619 BR-6 retires that clause. With REQ-597's
/// thirteen builtins permanently in force, its consequence was that every
/// user-authored skill pinned every repo-rooted session on every machine
/// (BUG-214), over a file that matched no glob and read nothing.
///
/// Both halves are here because the rule is the *pair*: a glob that names
/// nothing the skill touches must not refuse it, and a glob the user wrote over
/// their own skills directory must — naming the file, which is only possible
/// because the id exists. A build that answered `Unknown` again would pass the
/// second half and fail the first; a build that answered `Sources(∅)` — the
/// `ToolOutcome::ok` default this file was written to catch — would pass the
/// first and fail the second.
///
/// **Mutation (run, red, reverted):** restore
/// `(SkillSource::User, _) => ToolProvenance::Unknown` in `expand_and_fold` —
/// the leave half's `result.is_ok()` goes red. **6 red** across the workspace:
/// this test and `a_user_skill_reaches_the_provider_when_no_boundary_is_configured`
/// here, `skill.rs`'s
/// `a_user_skill_mints_a_home_scoped_id_and_a_project_skill_a_repo_scoped_one`,
/// `skill_turn.rs`'s `no_production_provenance_reads_spawned_any_more`, and
/// `provenance_egress.rs`'s two flipped legs. Second mutation (run, red,
/// reverted): make `skills::provenance_of` answer `None` for `SkillSource::User`
/// again — the leave half reddens as above and the refused half's block path
/// reads `<unknown-provenance>` instead of the file. **3 red**: both tests here
/// and `skill_turn.rs`'s
/// `a_user_skill_outside_the_root_seeds_a_block_with_its_home_scoped_identity`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_user_skill_leaves_under_a_boundary_it_never_touched_and_is_refused_by_one_that_names_it()
{
    // ── it leaves ───────────────────────────────────────────────────────────
    let fx = Fixture::new("userleave");
    fx.skill(&fx.home(), "validate");

    let run = run_skill_call(&fx, "validate", Some("secrets/**")).await;
    assert_the_expansion_landed(&run);
    assert!(
        !run.provenance_is_unknown,
        "a discovered user skill mints a `~`-scoped id (REQ-619 BR-3), so its \
         expansion is as pinnable as a project skill's and `Unknown` here would \
         be the retired rule back again"
    );
    assert_eq!(
        run.provenance_len, 1,
        "exactly the one file the body came from"
    );
    assert!(
        run.result.is_ok(),
        "a boundary naming nothing this skill touches must not refuse it — that \
         refusal is the pin BUG-214 filed: {:?}",
        run.result
    );
    assert!(run.blocks.is_empty(), "nothing matched, nothing blocked");
    assert_eq!(run.calls, 2, "both turns reached the transport");
    assert!(
        contains_bytes(&run.captured[1], BODY_MARKER),
        "the turn that left must really have carried the expansion, or the \
         leave claim is about an empty payload"
    );

    // ── and it is refused by a glob that names it ───────────────────────────
    //
    // The user's own row over their skills directory, in the ordinary path form
    // (REQ-619 OQ-1): `~` is an ordinary character to `globset` and every
    // builtin is already `**/`-anchored, so no new glob language is needed for a
    // `~`-scoped id to be matched.
    let fx = Fixture::new("usernamed");
    fx.skill(&fx.home(), "validate");
    let expected = fx.user_skill_id("validate");

    let run = run_skill_call(&fx, "validate", Some("**/.claude/skills/**")).await;
    assert_the_expansion_landed(&run);
    match &run.result {
        Err(e) if e.is_privacy_blocked() => {}
        other => panic!("a glob covering the skills directory must refuse it: {other:?}"),
    }
    assert_eq!(run.blocks.len(), 1, "exactly one privacy_block");
    assert_eq!(
        run.blocks[0].path, expected,
        "the block names the user's own file in the `~` scope — a sentinel here \
         would tell them nothing about which file to look at, and a \
         repo-relative spelling would be a second id for one file"
    );
    assert_eq!(run.calls, 1, "turn 2 must never reach the transport");
    for body in &run.captured {
        assert!(
            !contains_bytes(body, BODY_MARKER),
            "the skill body leaked past a boundary that names its file"
        );
    }
}

/// **The control: with no boundary configured, the same expansion reaches the
/// provider.**
///
/// Without this the refusals above would be satisfied by a build that refused
/// every second turn for any reason at all (LESSON-479).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_user_skill_reaches_the_provider_when_no_boundary_is_configured() {
    let fx = Fixture::new("control");
    fx.skill(&fx.home(), "validate");

    let run = run_skill_call(&fx, "validate", None).await;
    assert_the_expansion_landed(&run);

    // REQ-619 BR-3: the provenance is the same **minted id** in every session
    // above; what differs is whether a glob exists that matches it. Before this
    // REQ it was the same `Unknown` in every session, which is the assertion
    // this line replaces.
    assert!(
        !run.provenance_is_unknown,
        "the provenance is the same minted id here as under either boundary — \
         what differs is whether a glob matches it"
    );
    assert!(
        run.result.is_ok(),
        "with no boundary configured the turn carrying a skill expansion goes out: \
         {:?}",
        run.result
    );
    assert!(run.blocks.is_empty(), "no boundary, no privacy_block");
    assert_eq!(
        run.calls, 2,
        "both turns reached the transport: {} requests",
        run.calls
    );
}

/// **REQ-619 BR-9 — the no-boundary machine is unchanged, preamble and all.**
///
/// With no boundary configured, `shell_provenance::classify` answers from its
/// first line and every verdict is `Unknown` — so this expansion's provenance is
/// `Unknown` too, and it is *sent*, because the choke point does not inspect
/// when there is nothing to protect. That is the pre-REQ-614 posture REQ-614
/// BR-9 kept and this REQ keeps: a machine that opted out of the builtins and
/// wrote no rows of its own pays nothing for either feature.
///
/// The preamble is what makes the leg say something the control above does not:
/// an opaque verb is the strictest thing the fold can produce, and it still
/// leaves.
///
/// **Mutation (run, red, reverted):** drop `&& !self.boundaries.is_empty()`
/// from `Egress::send`'s inspection guard, so a machine with no rows inspects
/// anyway — the turn is refused and `result.is_ok()` goes red. **1 red**, this
/// test, which is what makes it the one that holds BR-9 on this path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_no_boundary_configured_a_user_skill_with_an_opaque_preamble_is_sent() {
    let fx = Fixture::new("nobnd");
    fx.skill_with(&fx.home(), "validate", "Out: !`sh -c 'echo x'`\n");

    let run = run_skill_call(&fx, "validate", None).await;
    assert_the_expansion_landed(&run);

    assert!(
        run.provenance_is_unknown,
        "with no boundary the classifier proves nothing, so the expansion is \
         `Unknown` — and the point of this test is that `Unknown` is *sent* here"
    );
    assert!(
        run.result.is_ok(),
        "BR-9: nothing to protect, nothing inspected, nothing pinned: {:?}",
        run.result
    );
    assert!(run.blocks.is_empty(), "no boundary, no privacy_block");
    assert_eq!(run.calls, 2, "both turns reached the transport");
}

/// **REQ-619 BR-10 — a project skill still mints and still pins as a `read`
/// would.**
///
/// The half this REQ does **not** move, asserted on the same instrument as the
/// half it does, so "unchanged" is a claim rather than an omission. Two legs:
/// under a glob covering the skills directory the turn is refused **naming the
/// file** — the id is repo-relative, exactly what a `read` of that path would
/// be judged on — and under a glob naming something else it leaves.
///
/// The sibling above is the user-scope twin, and the pair is what BR-3's
/// "distinct scopes" means in practice: the same relative path in the two roots
/// produces two different ids and is matched by two different globs.
///
/// **Mutation (run, red, reverted):** point `provenance_of`'s
/// `SkillSource::Project` arm at `from_home_resolved` — the refusal names a
/// `~`-scoped path and the equality goes red. **4 red** in this file: this test
/// and the three project legs below and above it, which is the blast radius a
/// scope confusion actually has.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_project_skill_still_mints_and_still_pins_as_a_read_would() {
    let fx = Fixture::new("projstill");
    fx.skill(&fx.repo(), "validate");

    let run = run_skill_call(&fx, "validate", Some("**/.claude/skills/**")).await;
    assert_the_expansion_landed(&run);
    match &run.result {
        Err(e) if e.is_privacy_blocked() => {}
        other => panic!("a project skill under a glob that names it must pin: {other:?}"),
    }
    assert_eq!(run.blocks.len(), 1, "exactly one privacy_block");
    assert_eq!(
        run.blocks[0].path, ".claude/skills/validate/SKILL.md",
        "a project skill's id is repo-relative and carries no scope marker, so \
         it can never collide with the user skill of the same relative path"
    );
    assert_eq!(run.calls, 1, "turn 2 must never reach the transport");

    // The leave half, on the same file: a glob that names something else.
    let fx = Fixture::new("projleave");
    fx.skill(&fx.repo(), "validate");
    let run = run_skill_call(&fx, "validate", Some("secrets/**")).await;
    assert_the_expansion_landed(&run);
    assert!(
        run.result.is_ok(),
        "a project skill under an unrelated boundary reaches the wire, exactly \
         as a `read` of its file would: {:?}",
        run.result
    );
    assert_eq!(run.calls, 2, "both turns reached the transport");
}

// ---------------------------------------------------------------------------
// BR-10 at the other three doors — the roster and the refusals that carry it
// ---------------------------------------------------------------------------

/// The `listed`/refusal reply really landed in context — the positive control
/// the negative claims below need (LESSON-479).
fn assert_the_roster_landed(run: &Run) {
    use tetond::harness::context::Provenance;
    let folded = run
        .ctx
        .blocks()
        .iter()
        .rev()
        .find(|b| matches!(b.provenance, Provenance::Tool { .. }))
        .map(|b| b.text.clone())
        .expect("the skill result was folded into context");
    assert!(
        folded.contains(LISTING_MARKER),
        "the file-authored description never reached the model, so this test asserts \
         nothing about what its provenance protects: {folded}"
    );
    assert!(
        !folded.contains("<skill-body"),
        "a roster is a catalogue, not an expansion — the instructions frame belongs \
         to neither of these two results: {folded}"
    );
}

/// **The roster a listing call returns pins the turn to the files it names.**
///
/// ADR-8's fail-open argument was applied to the *expansion* only. The `listed`
/// reply is built with `ToolOutcome::ok` and carried `Sources(∅)` — "touched no
/// repo file" — while emitting every model-invocable skill's `name`,
/// `argument_hint` and `description` straight out of `SKILL.md`. With a
/// `.claude/**` boundary configured, `skill {}` therefore handed back every
/// skill file's description with an empty source set, clearing
/// `context_provenance` so the next remote call carried it off the machine.
///
/// The boundary here names the skill directory, which is what makes the claim
/// testable: only a minted, root-relative id can match a glob.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listing_pins_the_turn_to_the_skill_files_it_names() {
    let fx = Fixture::new("listing");
    fx.skill(&fx.repo(), "validate");

    let run = run_skill_args(&fx, "{}", Some(".claude/**")).await;
    assert_the_roster_landed(&run);

    assert!(
        !run.provenance_is_unknown,
        "every listed row is a project skill under the root, so the roster mints"
    );
    assert_eq!(
        run.provenance_len, 1,
        "exactly the one file the roster's one row was read from"
    );
    match &run.result {
        Err(e) if e.is_privacy_blocked() => {}
        other => panic!(
            "the roster carries file-authored bytes and must pin the turn exactly as \
             the body does: {other:?}"
        ),
    }
    assert_eq!(run.blocks.len(), 1, "exactly one privacy_block");
    assert_eq!(run.calls, 1, "turn 2 must never reach the transport");
    for body in &run.captured {
        assert!(
            !contains_bytes(body, LISTING_MARKER),
            "a skill file's description leaked into captured egress"
        );
    }
}

/// **A typed refusal that carries the roster carries its provenance too.**
///
/// `unknown_skill` folds [`render_listing`]'s output into its own sentence, so
/// it is the same file-authored bytes reaching the model through a different
/// door — and `ToolOutcome::error` has the same `Sources(∅)` default
/// `ToolOutcome::ok` does. A refusal is where a reader is least likely to look
/// for a leak, which is the reason it gets its own leg rather than a comment.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refusal_that_carries_the_roster_pins_the_turn_too() {
    let fx = Fixture::new("refusal");
    fx.skill(&fx.repo(), "validate");

    let run = run_skill_args(&fx, r#"{"name":"no-such-skill"}"#, Some(".claude/**")).await;
    assert_the_roster_landed(&run);

    assert!(
        !run.provenance_is_unknown,
        "the refusal names the same project file the roster does"
    );
    assert_eq!(run.provenance_len, 1);
    match &run.result {
        Err(e) if e.is_privacy_blocked() => {}
        other => panic!("a refusal carrying the roster carries its files: {other:?}"),
    }
    assert_eq!(run.blocks.len(), 1, "exactly one privacy_block");
    assert_eq!(run.calls, 1, "turn 2 must never reach the transport");
    for body in &run.captured {
        assert!(
            !contains_bytes(body, LISTING_MARKER),
            "a skill file's description leaked into captured egress through a refusal"
        );
    }
}

/// **The control for the two legs above: with no boundary, the roster reaches
/// the provider.**
///
/// Without it both refusals would be satisfied by a build that refused every
/// second turn for any reason at all (LESSON-479).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listing_reaches_the_provider_when_no_boundary_is_configured() {
    let fx = Fixture::new("listctl");
    fx.skill(&fx.repo(), "validate");

    let run = run_skill_args(&fx, "{}", None).await;
    assert_the_roster_landed(&run);

    assert!(
        run.result.is_ok(),
        "with no boundary configured the turn carrying a roster goes out: {:?}",
        run.result
    );
    assert!(run.blocks.is_empty(), "no boundary, no privacy_block");
    assert_eq!(run.calls, 2, "both turns reached the transport");
}
