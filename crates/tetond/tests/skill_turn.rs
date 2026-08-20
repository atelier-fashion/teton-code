//! REQ-585 TASK-204 — the turn ordering and the two-stage refusal (BR-4, BR-7,
//! BR-8, AC-13, AC-16; ADR-3, ADR-9, ADR-11).
//!
//! The claim this file exists to make is about **order**, and order is only
//! visible from the outside. So every behavioural test here drives
//! [`DaemonRuntime::run_prompt_turn`] itself — not a hand-seeded
//! `CarriedTurn::begin` fixture — over a real [`EventBus`], a real
//! [`SessionRegistry`] and a real skill file on disk, and reads what a client
//! would have received. Hand-building an expansion and asserting it arrived
//! leaves the producer unguarded: a daemon that stopped substituting
//! `$ARGUMENTS`, or that expanded *after* routing, would keep such a test green
//! (LESSON-544).
//!
//! ## What is pinned, and where
//!
//! | Claim | Test |
//! |---|---|
//! | the daemon resolves the name against its own registry (LESSON-520) | [`an_unknown_skill_name_is_refused_by_the_daemon_not_only_by_the_client`] |
//! | …and against the registry *as it stands after a `/cd`* | [`a_name_the_registry_lost_at_cd_is_refused_though_a_stale_snapshot_still_lists_it`] |
//! | a shadowed row is never the file that runs (BR-2) | [`a_shadowed_row_is_never_the_file_that_runs`] |
//! | BR-8(c): a refused turn seeds nothing and says nothing | [`a_refused_skill_turn_seeds_nothing_says_nothing_and_changes_no_health`] |
//! | AC-16: a typed oversized prompt still elides, loudly | [`a_typed_oversized_prompt_still_elides_loudly_on_the_route_that_refuses_a_skill`] |
//! | BR-4: the engine is handed the expansion that was measured | [`the_engine_is_handed_the_expansion_the_budget_measured`] |
//! | ADR-3: the naming attempt reads the expansion, not `""` | [`a_skill_turn_spends_the_naming_attempt_on_the_expansion_not_on_an_empty_string`] |
//! | BR-7: a project skill's expansion is pinned to its file | [`a_project_skills_expansion_is_pinned_to_the_file_it_came_from`] |
//! | ADR-9: a user skill outside the root is `unknown` | [`a_user_skill_outside_the_root_seeds_a_block_that_says_it_cannot_be_pinned`] |
//! | AC-13: frontmatter cannot escalate spend | [`frontmatter_asking_for_opus_at_max_effort_with_bash_star_changes_nothing`] |
//! | ADR-3: `prompt` and `skill` are exclusive; both-empty still runs | [`a_request_carrying_both_prompt_and_skill_is_invalid_params`] |
//!
//! ## Mutation table
//!
//! | Mutation | Test that fails |
//! |---|---|
//! | Stage A's refusal raised after `CarriedTurn::begin` | [`a_refused_skill_turn_seeds_nothing_says_nothing_and_changes_no_health`], [`the_two_refusals_bracket_the_consent_seam_and_precede_the_seed`] |
//! | Stage B's refusal raised after `CarriedTurn::begin` | [`the_two_refusals_bracket_the_consent_seam_and_precede_the_seed`] |
//! | Stage A moved below the TASK-205 consent seam | [`the_two_refusals_bracket_the_consent_seam_and_precede_the_seed`] |
//! | the expansion built *after* routing and naming | [`the_expansion_is_built_before_either_reader_of_the_prompt_text`], [`a_skill_turn_spends_the_naming_attempt_on_the_expansion_not_on_an_empty_string`], and `runtime::tests::skill_turn_readers` |
//! | the seeded block's provenance dropped | [`a_project_skills_expansion_is_pinned_to_the_file_it_came_from`], [`a_user_skill_outside_the_root_seeds_a_block_that_says_it_cannot_be_pinned`] |
//! | the daemon trusting the client's name | [`an_unknown_skill_name_is_refused_by_the_daemon_not_only_by_the_client`] |
//! | the `digest` duty reaching the turn path | [`the_digest_duty_has_one_production_call_site_and_the_turn_path_is_not_it`] |
//!
//! ## The three order claims that no behaviour can reach *yet*
//!
//! Stage B measures the same string Stage A did until TASK-205 folds real
//! dynamic output in, and the consent Stage A must precede does not exist yet.
//! A behavioural test for "Stage A is above the consent" cannot be written
//! against code that has no consent in it, and one for "Stage B is above
//! `CarriedTurn::begin`" cannot be distinguished from Stage A's while the two
//! measure the same bytes. Those two rows, and "expansion precedes routing" as a
//! structural fact, are therefore pinned by reading `run_prompt_turn`'s own
//! source — the instrument `call_sites.rs` uses for the same reason, and for the
//! same reason it is *additional to* rather than instead of the behavioural
//! tests above. See [`the_two_refusals_bracket_the_consent_seam_and_precede_the_seed`],
//! which pins where each stage **measures** and where each refusal is **raised**:
//! a check that measured above the seed and returned below it would commit the
//! very expansion it was refusing.
//!
//! ## What is *not* here
//!
//! The classifier's own prompt. `route` has no configurable counterpart, so it
//! resolves to the local tier or to nothing, and an integration test cannot
//! install a local engine — that assertion lives in
//! `runtime::tests::skill_turn_readers`, beside a recording engine. So does the
//! `/cd` grant drop, whose witness needs the runtime's private `session_gates`.
//!
//! ## Why this binary owns `HOME`
//!
//! Two of the four discovery roots are `~/.claude/skills` and
//! `~/.claude/commands`. Left at the developer's own home, every session here
//! would register whatever skills that machine happens to have — on the machine
//! this feature was written for, twenty of them. So the binary points `HOME` at
//! a fixture home once, before any daemon or probe exists.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::time::timeout;

use teton_core::ProvenanceId;
use teton_protocol::events::{BudgetBound, Event};
use teton_protocol::jsonrpc::error_code;
use teton_protocol::methods::{
    ConfigUpdate, ProviderConfig, SessionPermissionsParams, SessionSetCwdParams, SkillInvocation,
    TierBindingConfig,
};
use teton_protocol::{
    Phase as ProtoPhase, ProviderId, ProviderKind as ProtoProviderKind, SessionId, SessionMode,
    Tier as ProtoTier, PROTOCOL_VERSION, PROTOCOL_VERSION_MAX, PROTOCOL_VERSION_MIN,
};

use tetond::broadcast::EventBus;
use tetond::harness::context::{BlockRole, Provenance};
use tetond::runtime::{ClientPresence, DaemonRuntime};
use tetond::sessions::SessionRegistry;
use tetond::skills::{RealFs, PENDING_PLACEHOLDER};
use tetond::{server, Daemon};

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

/// A throwaway directory tree, removed on drop.
struct Tree {
    root: PathBuf,
}

impl Tree {
    /// A fresh tree under `/tmp` with a short name: a daemon socket is bound
    /// beneath one of these and `sun_len` caps the path at ~104 bytes.
    fn new(tag: &str) -> Self {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        let root =
            PathBuf::from("/tmp").join(format!("tst{tag}{:x}{seq:x}", std::process::id() & 0xffff));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // A project marker, so the root probes as `project` rather than `plain`
        // and the project half of discovery is reached at all.
        std::fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
        Self { root }
    }

    fn write(&self, rel: &str, contents: &str) {
        let path = self.root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn path(&self) -> &Path {
        &self.root
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The `HOME` every discovery in this binary runs under.
///
/// Set once, before any daemon is constructed, and never changed: each test
/// calls this first, so the write happens while every other test is still
/// blocked inside the `OnceLock` initializer rather than beside a live read.
/// It is deliberately never dropped — it has to outlive every test in the
/// binary — so it is re-created from scratch on each run instead.
fn fixture_home() -> &'static Path {
    static HOME: OnceLock<Tree> = OnceLock::new();
    HOME.get_or_init(|| {
        let home = Tree::new("home");
        home.write(
            ".claude/skills/homeonly/SKILL.md",
            "---\ndescription: a user skill outside any repo\n---\n\nThe user skill's body.\n",
        );
        std::env::set_var("HOME", home.path());
        home
    })
    .path()
}

/// A skill file with `body` and no frontmatter keys beyond a description.
fn skill_file(description: &str, body: &str) -> String {
    format!("---\ndescription: {description}\n---\n\n{body}\n")
}

/// Roughly `bytes` worth of prose, as whitespace-separated words.
///
/// Four bytes per word (three characters and a space), so a caller quoting a
/// byte figure is quoting the guard that actually fires: the byte half of the
/// budget pair, not the word half.
fn filler(bytes: usize) -> String {
    let mut out = String::with_capacity(bytes + 4);
    while out.len() < bytes {
        out.push_str("abc ");
    }
    out
}

// ---------------------------------------------------------------------------
// a runtime with a route
// ---------------------------------------------------------------------------

/// A daemon runtime, its bus, its sessions and the mock vendor its one provider
/// points at.
struct Harness {
    runtime: Arc<DaemonRuntime>,
    events: Arc<EventBus>,
    sessions: SessionRegistry,
    vendor: Vendor,
}

impl Harness {
    /// A runtime whose turn-serving tiers are bound to one remote provider
    /// declaring `max_context = window`.
    ///
    /// The config is installed through `config/set`'s own path
    /// (`apply_config_update`), not by reaching into the runtime: the budget
    /// under test is the one `Router::budget_for` derives from a registered
    /// provider, and a hand-built `RouteBudget` would be the second derivation
    /// REQ-586 exists to prevent.
    ///
    /// **`reflex` is deliberately left unbound.** `route`, `redact` and `title`
    /// all hang off it (`Category::tier`), and this machine has no local tier —
    /// so those three duties resolve to nothing and issue no call. That is what
    /// makes "the vendor was never reached" a statement about the *turn* rather
    /// than about whichever duty happened to fire beside it: REQ-561's naming
    /// duty is started before any budget exists, and binding it here would put
    /// a bounded copy of the expansion on the wire for a turn BR-8 refuses.
    fn with_window(window: u32) -> Self {
        fixture_home();
        let vendor = Vendor::start();
        let runtime = Arc::new(DaemonRuntime::minimal());
        runtime
            .apply_config_update(ConfigUpdate::RegisterProvider(ProviderConfig {
                id: ProviderId::from("mock"),
                kind: ProtoProviderKind::OpenaiCompatible,
                endpoint: Some(vendor.endpoint.clone()),
                model: Some("mock-1".to_owned()),
                auth_ref: None,
                max_context: Some(window),
                context_budget_cap: None,
                floored_budget: None,
            }))
            .expect("registering a provider");
        for tier in [ProtoTier::Scan, ProtoTier::Build, ProtoTier::Think] {
            runtime
                .apply_config_update(ConfigUpdate::SetTierBinding(TierBindingConfig {
                    tier,
                    provider_id: ProviderId::from("mock"),
                    fallback_id: None,
                }))
                .expect("binding a tier");
        }
        Self {
            runtime,
            events: Arc::new(EventBus::new()),
            sessions: SessionRegistry::new(),
            vendor,
        }
    }

    /// A structured session rooted at `cwd`, with its skill registry derived
    /// from that root exactly as `session/create` derives it.
    fn session_at(&self, cwd: &Path) -> SessionId {
        let id = self
            .sessions
            .create(
                SessionMode::Structured,
                Some(ProtoPhase::Implement),
                Some(cwd.to_path_buf()),
            )
            .expect("a structured session takes a phase")
            .session_id;
        self.rebuild_skills(&id, cwd);
        id
    }

    fn rebuild_skills(&self, id: &SessionId, cwd: &Path) {
        let probed = self.runtime.session_root_for(Some(cwd));
        self.sessions.set_skills(
            id,
            tetond::skills::discover(
                Some(fixture_home()),
                &probed.path,
                probed.view.kind,
                &RealFs,
            ),
        );
    }

    /// Run one turn: typed text when `skill` is `None`, an invocation otherwise.
    async fn turn(
        &self,
        id: &SessionId,
        prompt: &str,
        skill: Option<SkillInvocation>,
    ) -> Result<teton_protocol::methods::PromptTurnResult, teton_protocol::jsonrpc::RpcError> {
        self.runtime
            .run_prompt_turn(
                &self.events,
                &self.sessions,
                id.clone(),
                SessionMode::Structured,
                Some(ProtoPhase::Implement),
                Some(
                    self.sessions
                        .get(id)
                        .and_then(|s| s.cwd)
                        .expect("the fixture always roots its sessions"),
                ),
                prompt.to_owned(),
                skill,
                ClientPresence::unwatched(),
            )
            .await
    }

    /// One `/name <rest>` invocation.
    fn invoke(name: &str, rest: &str) -> Option<SkillInvocation> {
        Some(SkillInvocation {
            name: name.to_owned(),
            raw_arguments: rest.to_owned(),
        })
    }
}

/// A single-threaded mock OpenAI-compatible vendor on a real socket.
///
/// Real, rather than a `Transport` double, because the claims here are about
/// `run_prompt_turn`'s arms — what it refuses before anything is dispatched, and
/// what it sends when it does not — and only a socket can settle "no packet
/// left".
struct Vendor {
    endpoint: String,
    hits: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<String>>>,
}

impl Vendor {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a mock vendor");
        let addr = listener.local_addr().expect("mock vendor address");
        let hits = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let served = Arc::clone(&hits);
        let captured = Arc::clone(&bodies);
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                served.fetch_add(1, Ordering::SeqCst);
                // Read until the request body has been seen. The daemon sends
                // one request and waits, so a single large read is enough for a
                // fixture; the body is only ever inspected for substrings.
                let mut raw = Vec::new();
                let mut buf = [0u8; 65_536];
                while let Ok(read) = stream.read(&mut buf) {
                    if read == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..read]);
                    if raw.windows(4).any(|w| w == b"\r\n\r\n") && read < buf.len() {
                        break;
                    }
                }
                captured
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&raw).into_owned());
                let body = concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                    "data: {\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\n",
                    "data: [DONE]\n\n",
                );
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
                let _ = stream.flush();
            }
        });
        Self {
            endpoint: format!("http://{addr}/v1/chat/completions"),
            hits,
            bodies,
        }
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    fn sent(&self) -> Vec<String> {
        self.bodies.lock().unwrap().clone()
    }
}

/// Everything a subscription holds right now, drained without waiting on a
/// clock: the bus is in-process and the turn has already returned.
async fn drain(sub: &mut tetond::broadcast::Subscription) -> Vec<Event> {
    let mut out = Vec::new();
    while let Ok(Some(env)) = timeout(Duration::from_millis(100), sub.recv()).await {
        out.push(env.event);
    }
    out
}

// ---------------------------------------------------------------------------
// the daemon resolves the name, not the client
// ---------------------------------------------------------------------------

/// LESSON-520's shape: the client's `classify` runs over a *snapshot* of this
/// registry, so the name arriving here is normally one it already matched. That
/// is not a reason to trust it — a third-party client need hold no snapshot at
/// all — so the daemon resolves it again, against the registry it will actually
/// dispatch from.
///
/// Non-vacuity: the same turn with the registered name expands and runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_skill_name_is_refused_by_the_daemon_not_only_by_the_client() {
    let repo = Tree::new("unknown");
    repo.write(
        ".claude/skills/known/SKILL.md",
        &skill_file("a registered skill", "Do the known thing."),
    );
    let h = Harness::with_window(128_000);
    let session = h.session_at(repo.path());

    let err = h
        .turn(&session, "", Harness::invoke("nosuchskill", ""))
        .await
        .expect_err("a name this session does not dispatch must be refused");
    assert_eq!(err.code, error_code::INVALID_PARAMS, "{err:?}");
    assert!(err.message.contains("nosuchskill"), "{}", err.message);
    assert_eq!(
        h.sessions.conversation_snapshot(&session).blocks().len(),
        0,
        "a refused invocation must not seed a turn"
    );

    // A name that *is* registered runs, so the refusal above is the registry
    // answering rather than the skill path being broken.
    h.turn(&session, "", Harness::invoke("known", ""))
        .await
        .expect("a registered skill runs");
    assert!(h.vendor.hits() >= 1, "the registered skill reached a model");
}

/// A malformed name is refused **without being echoed**: the only string this
/// daemon reflects into a sentence is one that already matched
/// `^[a-z0-9][a-z0-9_-]{0,63}$` (LESSON-517).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_name_that_is_not_a_skill_name_is_refused_without_being_echoed() {
    let repo = Tree::new("badname");
    let h = Harness::with_window(128_000);
    let session = h.session_at(repo.path());

    let hostile = "../../etc/\u{1b}[2Jpasswd";
    let err = h
        .turn(&session, "", Harness::invoke(hostile, ""))
        .await
        .expect_err("a name that is not a skill name is refused");
    assert_eq!(err.code, error_code::INVALID_PARAMS, "{err:?}");
    assert!(
        !err.message.contains("passwd") && !err.message.contains('\u{1b}'),
        "the wire's own bytes were reflected into a message a terminal renders: {}",
        err.message
    );
}

/// The `/cd` half of the registry's lifetime, seen from the turn path: after the
/// root moves, a name only the old root defined is refused — even though a
/// client that has not yet refreshed its snapshot still lists it.
///
/// This is also the inherited-seam test: the rebuild now happens **inside**
/// `set_session_cwd`, ahead of the `session_root_changed` publish, so the
/// registry a turn reads immediately after the move is the new one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_name_the_registry_lost_at_cd_is_refused_though_a_stale_snapshot_still_lists_it() {
    let before = Tree::new("cdfrom");
    before.write(
        ".claude/skills/onlyhere/SKILL.md",
        &skill_file("defined under the first root", "The first root's body."),
    );
    let after = Tree::new("cdto");
    after.write(
        ".claude/skills/overthere/SKILL.md",
        &skill_file("defined under the second root", "The second root's body."),
    );

    let h = Harness::with_window(128_000);
    let session = h.session_at(before.path());
    h.turn(&session, "", Harness::invoke("onlyhere", ""))
        .await
        .expect("the skill runs under the root that defines it");

    h.runtime
        .set_session_cwd(
            &SessionSetCwdParams {
                session_id: session.clone(),
                cwd: after.path().to_path_buf(),
            },
            &h.sessions,
            &h.events,
            &RealFs,
        )
        .expect("the move succeeds");

    // No `rebuild_skills` call here on purpose: the move is what re-derived the
    // registry, and that is the claim.
    let err = h
        .turn(&session, "", Harness::invoke("onlyhere", ""))
        .await
        .expect_err("the old root's skill is gone with the root");
    assert_eq!(err.code, error_code::INVALID_PARAMS, "{err:?}");
    h.turn(&session, "", Harness::invoke("overthere", ""))
        .await
        .expect("the new root's skill is what this session dispatches now");
}

/// BR-2: between two rows of one name the loser is *listed*, never run. The
/// daemon resolves through `SkillRegistry::dispatchable`, so the shadowed row
/// cannot be the file that expands — asserted on the preamble, which names the
/// file the body came from.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shadowed_row_is_never_the_file_that_runs() {
    let repo = Tree::new("shadow");
    repo.write(
        ".claude/skills/dup/SKILL.md",
        &skill_file("the winner", "WINNER-BODY-MARKER"),
    );
    repo.write(".claude/commands/dup.md", "LOSER-BODY-MARKER\n");

    let h = Harness::with_window(128_000);
    let session = h.session_at(repo.path());
    h.turn(&session, "", Harness::invoke("dup", ""))
        .await
        .expect("the name dispatches to its winner");

    let sent = h.vendor.sent().join("\n");
    assert!(
        sent.contains("WINNER-BODY-MARKER"),
        "the `skills/` row is what dispatches (BR-2): {}",
        &sent[..sent.len().min(600)]
    );
    assert!(
        !sent.contains("LOSER-BODY-MARKER"),
        "a shadowed row reached a model"
    );
}

// ---------------------------------------------------------------------------
// BR-8: the refusal, and its silence
// ---------------------------------------------------------------------------

/// **BR-8(c) and the four properties of REQ-586's sibling arm.**
///
/// The refusal runs before `CarriedTurn::begin`, which both pushes the user
/// block and arms the drop-commit — so if either check moved below that line the
/// expansion would be committed by the guard's own `Drop` on the way out, and
/// the block count below would be 1 rather than 0.
///
/// Every negative here is bounded by a positive control: the same route serves a
/// small skill in [`an_unknown_skill_name_is_refused_by_the_daemon_not_only_by_the_client`],
/// and the typed twin of this very fixture elides and reaches the vendor in
/// [`a_typed_oversized_prompt_still_elides_loudly_on_the_route_that_refuses_a_skill`],
/// so a passing negative cannot be the turn path merely being broken
/// (LESSON-479).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_skill_turn_seeds_nothing_says_nothing_and_changes_no_health() {
    let repo = Tree::new("toobig");
    repo.write(
        ".claude/skills/huge/SKILL.md",
        &skill_file("a body no small route can carry", &filler(40_000)),
    );
    // The shipped Ollama recipe's window: derived below the floor, so the budget
    // in force is *larger* than the declaration and the refusal has to say so.
    let h = Harness::with_window(4_096);
    let session = h.session_at(repo.path());
    let mut sub = h.events.subscribe(256);
    // The routing view a client reads, which is resolver-answered over the
    // health map: a provider demoted to `Unavailable` moves these rows. It is
    // the client-visible instrument for "the refusal changed no standing", and
    // the second turn below is its positive control.
    let before = h.runtime.config_snapshot();

    let err = h
        .turn(&session, "", Harness::invoke("huge", ""))
        .await
        .expect_err("a body that cannot fit is refused, not clamped");

    // Teton refused to send it — not a provider refusing a turn it saw.
    assert_eq!(err.code, error_code::SKILL_EXPANSION_TOO_LARGE, "{err:?}");
    assert!(err.message.contains("/huge"), "{}", err.message);
    assert!(
        err.message.contains("the body alone"),
        "the message must say which stage refused (BR-8d): {}",
        err.message
    );
    assert!(
        err.message
            .contains(&format!("bound: {}", BudgetBound::Window.words())),
        "the bound is spoken, never spelled (BR-8a): {}",
        err.message
    );
    assert!(
        err.message.contains("floored"),
        "a floored bound says it was floored (BR-8b): {}",
        err.message
    );

    // Nothing was seeded: `CarriedTurn::begin` was never reached, so its
    // drop-commit never armed.
    assert_eq!(
        h.sessions.conversation_snapshot(&session).blocks().len(),
        0,
        "a refused skill turn committed its expansion"
    );
    // Nothing was sent.
    assert_eq!(h.vendor.hits(), 0, "a refused turn reached a provider");

    // …including by the naming duty, which is a model call. It runs below
    // Stage A precisely so BR-8's sentence — "Nothing was sent and no provider
    // saw this turn" — is true of the *machine* and not only of the turn: on a
    // host with `reflex` bound remotely, naming a refused turn would put a
    // bounded copy of the expansion on the wire. An unspent claim is the
    // observable form of "the duty never started".
    assert!(
        h.sessions.claim_title(&session),
        "a refused skill turn spent the session's naming attempt, so the title          duty ran on an expansion that never did"
    );

    // And nothing was said. Drained and asserted empty, in the shape
    // `context_pressure.rs` uses: a report with nothing in it is the one that
    // says nothing.
    let published = drain(&mut sub).await;
    let pressure: Vec<_> = published
        .iter()
        .filter(|event| matches!(event, Event::ContextPressure(_)))
        .collect();
    assert!(
        pressure.is_empty(),
        "a refused turn emitted context pressure: {pressure:#?}"
    );
    let degraded: Vec<_> = published
        .iter()
        .filter(|event| matches!(event, Event::ProviderDegraded(_)))
        .collect();
    assert!(
        degraded.is_empty(),
        "nothing failed over, so nothing may say a provider was demoted: {degraded:#?}"
    );
    assert_eq!(
        h.runtime.config_snapshot(),
        before,
        "the refusal changed a provider's standing with the router"
    );
    // …and the provider is still the one this route takes: a typed turn on the
    // same session reaches it. Without this control, an equal snapshot could
    // just as well mean nothing routes at all (LESSON-479).
    h.turn(&session, "a short typed turn", None)
        .await
        .expect("the route the refusal did not touch still serves");
    assert_eq!(
        h.vendor.hits(),
        1,
        "exactly one request, and it is the typed turn's: the refusal neither \
         sent nor retried"
    );
}

/// **AC-16's contrast, and the reason it is in this file.** The refusal is for
/// skill turns *only*: the identical bytes typed by hand on the identical route
/// take REQ-586 BR-7's loud elision instead — the turn runs, the newest user
/// block is clamped, and the clamp is announced.
///
/// Same window, same size, one difference. Without this pair, "skills are
/// refused" and "everything is refused" look the same.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_typed_oversized_prompt_still_elides_loudly_on_the_route_that_refuses_a_skill() {
    let repo = Tree::new("typedbig");
    let h = Harness::with_window(4_096);
    let session = h.session_at(repo.path());
    let mut sub = h.events.subscribe(256);

    h.turn(&session, &filler(40_000), None)
        .await
        .expect("a typed oversized prompt is served, not refused");

    let published = drain(&mut sub).await;
    let elided = published.iter().any(|event| match event {
        Event::ContextPressure(pressure) => pressure.newest_user_elided,
        _ => false,
    });
    assert!(
        elided,
        "REQ-586 BR-7's elision must still fire for typed text: {published:#?}"
    );
    assert!(
        h.vendor.hits() >= 1,
        "an elided typed turn still reaches the provider"
    );
}

// ---------------------------------------------------------------------------
// BR-4: what the engine is handed
// ---------------------------------------------------------------------------

/// **LESSON-544.** Driven through `run_prompt_turn`, so the *producer* is under
/// test: a daemon that stopped substituting `$ARGUMENTS`, or that folded a
/// dynamic slot before Stage A measured it, reddens this. A fixture that seeded
/// `CarriedTurn::begin` by hand would not.
///
/// The `[dynamic context pending]` assertion is the non-consent half's own
/// shape, and it is deliberate: until TASK-205 runs the commands, the slot the
/// budget measured and the slot the model receives are the same string, which is
/// what makes Stage B's later divergence a real change rather than a rename.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_engine_is_handed_the_expansion_the_budget_measured() {
    let repo = Tree::new("expansion");
    repo.write(
        ".claude/skills/echoer/SKILL.md",
        &skill_file(
            "substitutes and scans",
            "Handle $ARGUMENTS carefully.\n\nContext: !`echo hello`\n",
        ),
    );
    let h = Harness::with_window(128_000);
    let session = h.session_at(repo.path());

    h.turn(
        &session,
        "",
        Harness::invoke("echoer", "REQ-585  \"quoted\""),
    )
    .await
    .expect("the skill runs");

    let sent = h.vendor.sent().join("\n");
    assert!(
        sent.contains("The user invoked /echoer"),
        "BR-4's preamble reaches the model: {}",
        &sent[..sent.len().min(800)]
    );
    // The rest of the line verbatim: interior whitespace preserved, quotes not
    // interpreted (AC-4). JSON-escaped on the wire, hence the escaped quotes.
    assert!(
        sent.contains(r#"Handle REQ-585  \"quoted\" carefully."#),
        "`$ARGUMENTS` is substituted verbatim: {}",
        &sent[..sent.len().min(800)]
    );
    assert!(
        sent.contains(PENDING_PLACEHOLDER),
        "an un-run dynamic slot reaches the model as the placeholder the budget \
         measured, never as silence: {}",
        &sent[..sent.len().min(800)]
    );
    assert!(
        !sent.contains("echo hello"),
        "the command text itself is not in a pending slot"
    );
}

/// **ADR-3, the naming half.** `worth_titling` declines a request shorter than
/// 16 bytes *without* spending the session's one attempt, so a skill turn
/// expanded after `spawn_title_session` would leave the claim untaken — the
/// session unnamed for its whole life. `claim_title` answering `false` is
/// therefore the assertion that the attempt was spent, and spent on something
/// substantial.
///
/// The control is the same runtime with a two-character typed prompt, where the
/// claim is still available.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_skill_turn_spends_the_naming_attempt_on_the_expansion_not_on_an_empty_string() {
    let repo = Tree::new("titling");
    repo.write(
        ".claude/skills/named/SKILL.md",
        &skill_file(
            "long enough to be worth a name",
            "Rename the world, please.",
        ),
    );
    let h = Harness::with_window(128_000);

    let invoked = h.session_at(repo.path());
    h.turn(&invoked, "", Harness::invoke("named", ""))
        .await
        .expect("the skill runs");
    assert!(
        !h.sessions.claim_title(&invoked),
        "the naming attempt was never taken, so the title duty was handed `\"\"` \
         — the expansion ran after the naming rather than before it"
    );

    let typed = h.session_at(repo.path());
    h.turn(&typed, "hi", None).await.expect("a short turn runs");
    assert!(
        h.sessions.claim_title(&typed),
        "control: a request too short to name must leave the attempt unspent, or \
         the assertion above says nothing"
    );
}

// ---------------------------------------------------------------------------
// BR-7 / ADR-9: the seeded block carries the file it came from
// ---------------------------------------------------------------------------

/// **BR-7.** Prompt text carries no file provenance today, so the expansion has
/// to carry the skill file's — a skill under a `local-only` boundary then pins
/// the turn exactly as a `read` of that file would. A *project* skill is under
/// the root and mints cleanly.
///
/// Asserted on the committed block rather than on a return value, because the
/// block is what egress inspects and what the next turn replays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_project_skills_expansion_is_pinned_to_the_file_it_came_from() {
    let repo = Tree::new("pinned");
    repo.write(
        ".claude/skills/pinme/SKILL.md",
        &skill_file("under the root", "Body under the root."),
    );
    let h = Harness::with_window(128_000);
    let session = h.session_at(repo.path());

    h.turn(&session, "", Harness::invoke("pinme", ""))
        .await
        .expect("the skill runs");

    let committed = h.sessions.conversation_snapshot(&session);
    let user = committed
        .blocks()
        .iter()
        .find(|block| block.role == BlockRole::User)
        .expect("the turn seeded a user block");
    let root = h.runtime.session_root_for(Some(repo.path())).path;
    let expected = ProvenanceId::from_resolved(&root, &root.join(".claude/skills/pinme/SKILL.md"))
        .expect("a project skill is under the root and mints");
    match &user.provenance {
        Provenance::User { sources, unknown } => {
            assert_eq!(
                sources,
                &BTreeSet::from([expected]),
                "the expansion must carry the skill file's identity, or a boundary \
                 glob has nothing to match it against"
            );
            assert!(
                !unknown,
                "a project skill mints, so nothing about it is unpinnable"
            );
        }
        other => panic!("a prompt turn seeds a user block: {other:?}"),
    }
}

/// **ADR-9's id-minting gap, decided rather than papered over.** A user skill at
/// `~/.claude/skills/x/SKILL.md` in a repo-rooted session has no repo-relative
/// identity, and `ProvenanceId::from_resolved` refuses rather than inventing one
/// (REQ-571 ADR-B). Its block therefore says `unknown`, which fails closed
/// wherever a boundary is configured — stricter than BR-7's letter and right in
/// the charter's direction: the alternative is a file outside the root silently
/// counting as unpinnable-but-fine.
///
/// The project twin above is the control: same runtime, same turn path, and the
/// difference is only where the file lives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_user_skill_outside_the_root_seeds_a_block_that_says_it_cannot_be_pinned() {
    let repo = Tree::new("unpinnable");
    let h = Harness::with_window(128_000);
    let session = h.session_at(repo.path());

    h.turn(&session, "", Harness::invoke("homeonly", ""))
        .await
        .expect("a user skill runs from a repo-rooted session");

    let committed = h.sessions.conversation_snapshot(&session);
    let user = committed
        .blocks()
        .iter()
        .find(|block| block.role == BlockRole::User)
        .expect("the turn seeded a user block");
    match &user.provenance {
        Provenance::User { sources, unknown } => {
            assert!(
                sources.is_empty(),
                "nothing under `~` has a repo-relative identity to mint: {sources:?}"
            );
            assert!(
                unknown,
                "an unmintable file must set `unknown`, or the turn silently \
                 counts as drawn from nothing at all"
            );
        }
        other => panic!("a prompt turn seeds a user block: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// AC-13: a file on disk cannot escalate spend
// ---------------------------------------------------------------------------

/// **AC-13's teeth (BR-5, OQ-5).** A skill declaring `model: opus`,
/// `effort: max` and `allowed-tools: Bash(*)` produces exactly the route, the
/// effort and the permission level a typed prompt does. Every one of those three
/// keys is inert; the body is a sentence the model reads, not a setting.
///
/// It needs a harness that can *see* a route, which is why it lives here rather
/// than in TASK-195's pure suite (LESSON-481): the claim is about
/// `route_decided`'s payload and the session's gate, neither of which a registry
/// unit test has.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frontmatter_asking_for_opus_at_max_effort_with_bash_star_changes_nothing() {
    let repo = Tree::new("greedy");
    repo.write(
        ".claude/skills/greedy/SKILL.md",
        "---\ndescription: asks for everything\nmodel: opus\neffort: max\n\
         allowed-tools: Bash(*)\n---\n\nDo the greedy thing.\n",
    );
    let h = Harness::with_window(128_000);

    let typed = h.session_at(repo.path());
    let mut typed_sub = h.events.subscribe(256);
    h.turn(&typed, "Do the greedy thing.", None)
        .await
        .expect("the typed twin runs");
    let typed_route = first_route(&drain(&mut typed_sub).await);

    let invoked = h.session_at(repo.path());
    let mut invoked_sub = h.events.subscribe(256);
    h.turn(&invoked, "", Harness::invoke("greedy", ""))
        .await
        .expect("the skill runs");
    let skill_route = first_route(&drain(&mut invoked_sub).await);

    assert_eq!(
        (
            &typed_route.provider_id,
            &typed_route.model,
            &typed_route.tier,
            &typed_route.effort
        ),
        (
            &skill_route.provider_id,
            &skill_route.model,
            &skill_route.tier,
            &skill_route.effort
        ),
        "a file on disk moved the route or the effort:\ntyped {typed_route:#?}\nskill {skill_route:#?}"
    );

    // And the permission level, read back from the gate that decides it rather
    // than from the request that asked nothing.
    let level_of = |id: &SessionId| {
        h.runtime
            .session_permissions(
                &SessionPermissionsParams {
                    session_id: id.clone(),
                    level: None,
                },
                &h.events,
            )
            .level
    };
    assert_eq!(
        level_of(&typed),
        level_of(&invoked),
        "`allowed-tools: Bash(*)` moved the session's permission level"
    );
}

/// The first `route_decided` in `published` — the turn's own, since duties
/// publish only when they run and nothing here runs one.
fn first_route(published: &[Event]) -> teton_protocol::events::RouteDecided {
    published
        .iter()
        .find_map(|event| match event {
            Event::RouteDecided(decided) => Some(decided.clone()),
            _ => None,
        })
        .expect("a served turn announces its route")
}

// ---------------------------------------------------------------------------
// ADR-3 at the wire: exactly one of `prompt`/`skill`
// ---------------------------------------------------------------------------

/// A request carrying **both** is `INVALID_PARAMS` — a combination that was
/// never valid, so nothing is narrowed. A **both-empty** request is deliberately
/// still served: `flatten_prompt(&[])` returns `""` and such a turn runs today,
/// and refusing it would narrow an existing method for third-party clients while
/// `PROTOCOL_VERSION` is asserted unchanged in the same test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_carrying_both_prompt_and_skill_is_invalid_params() {
    fixture_home();
    let repo = Tree::new("wire");
    let socket = temp_socket("skill-turn-wire");
    let listener = server::bind_listener(&socket).unwrap();
    let server_task = tokio::spawn(server::serve(listener, Arc::new(Daemon::new())));

    let mut client = TestClient::connect(&socket).await;
    client.handshake().await;
    let session = client.create_session_at(repo.path()).await;

    let both = client
        .call(
            "session/prompt",
            json!({
                "session_id": session,
                "prompt": [{"type": "text", "text": "typed"}],
                "skill": {"name": "anything", "raw_arguments": ""},
            }),
        )
        .await;
    assert_eq!(
        both["error"]["code"].as_i64(),
        Some(error_code::INVALID_PARAMS),
        "a turn is typed text or an invocation, never both: {both}"
    );

    // The pre-existing shape is untouched: no `skill` key, no blocks. It fails
    // for want of a provider on this bare daemon, which is a *turn* failure —
    // the point is that the request was accepted and run, not refused as
    // malformed.
    let empty = client
        .call(
            "session/prompt",
            json!({"session_id": session, "prompt": []}),
        )
        .await;
    assert_ne!(
        empty["error"]["code"].as_i64(),
        Some(error_code::INVALID_PARAMS),
        "a both-empty request runs today and must keep running: {empty}"
    );

    assert_eq!(
        PROTOCOL_VERSION, PROTOCOL_VERSION_MAX,
        "the wire's exclusivity rule is a refinement of an existing method, so \
         it must not have moved the protocol version"
    );

    server_task.abort();
    let _ = std::fs::remove_file(&socket);
}

// ---------------------------------------------------------------------------
// the order itself, read off the source
// ---------------------------------------------------------------------------

/// `path`'s production half — everything above its first `#[cfg(test)]` item.
///
/// The instrument `call_sites.rs` uses, for its reason: every module in this
/// crate puts test items last, so truncating there is exact today and
/// *conservative* if that changes — it can only shrink what a scan sees, which
/// makes an assertion fail loudly rather than pass wrongly. A file that is
/// missing is fatal rather than empty, so a rename cannot make these pass
/// vacuously.
fn production_source(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join(relative);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("unreadable source file {}: {err}", path.display()));
    match text.find("\n#[cfg(test)]") {
        Some(at) => text[..at].to_owned(),
        None => text,
    }
}

/// The body of `run_prompt_turn`, from its signature to the start of the next
/// item — so a marker that also appears elsewhere in this very large file
/// cannot satisfy an ordering claim about *this* function.
fn run_prompt_turn_body() -> String {
    let src = production_source("runtime.rs");
    let start = src
        .find("pub async fn run_prompt_turn(")
        .expect("`run_prompt_turn` is where the turn ordering lives");
    let rest = &src[start..];
    // The turn's own body ends at the next item declared at method indentation.
    let end = rest[1..]
        .find("\n    /// Run one attempt")
        .map_or(rest.len(), |at| at + 1);
    rest[..end].to_owned()
}

/// The offset of `needle` in `haystack`, or a failure naming what was not found.
fn at(haystack: &str, needle: &str) -> usize {
    haystack
        .find(needle)
        .unwrap_or_else(|| panic!("`{needle}` is not in `run_prompt_turn` any more"))
}

/// **The mutation table's structural rows.** BR-8's order is
/// `expand → route → Stage A → consent → Stage B → CarriedTurn::begin`, and
/// three of those relations cannot yet be reached behaviourally: the consent
/// Stage A must precede does not exist until TASK-205, and Stage B measures the
/// same bytes Stage A does until TASK-205 folds real output in.
///
/// So they are asserted where they *are* a fact today — in the source of the one
/// function that owns the order. Moving Stage A below the seam, moving either
/// refusal below the seed, or dropping the seam marker each redden this.
#[test]
fn the_two_refusals_bracket_the_consent_seam_and_precede_the_seed() {
    let body = run_prompt_turn_body();
    let stage_a = at(&body, "SkillStage::Body");
    let seam = at(&body, "TASK-205 SEAM");
    let stage_b = at(&body, "SkillStage::WithDynamicContext");
    let seed = at(&body, "CarriedTurn::begin(");

    assert!(
        stage_a < seam,
        "Stage A must refuse a body that cannot fit BEFORE the user is asked to \
         approve anything (BR-8d)"
    );
    assert!(
        seam < stage_b,
        "Stage B measures what the commands produced, so it belongs below the \
         seam that produces it"
    );
    assert!(
        stage_b < seed,
        "`CarriedTurn::begin` pushes the user block and arms the drop-commit, so \
         a refusal below it has already committed the expansion (BR-8c)"
    );

    // Where the *measurement* sits is half the claim; where the refusal is
    // **raised** is the other half. A check that measured above the seed and
    // returned below it would satisfy every assertion so far and still commit
    // the expansion it was refusing, so both raises are pinned too — and the
    // count is an equality, so a third refusal added without a decision here is
    // caught as loudly as a missing one.
    let raises: Vec<usize> = body
        .match_indices("error_code::SKILL_EXPANSION_TOO_LARGE")
        .map(|(at, _)| at)
        .collect();
    assert_eq!(
        raises.len(),
        2,
        "BR-8 has exactly two stages, so `run_prompt_turn` raises `-32023` twice"
    );
    assert!(
        raises.iter().all(|raise| *raise < seed),
        "a refusal is raised below `CarriedTurn::begin`, which has already pushed \
         the user block and armed the drop-commit (BR-8c)"
    );
}

/// **Expansion precedes routing**, as a structural fact to go with the
/// behavioural one. `dispatch_route` runs the freeform classifier over the
/// prompt text and `spawn_title_session` spends the session's one naming attempt
/// on it; a skill turn's `prompt` is empty, so an expansion built after either
/// classifies and names from `""`.
///
/// The naming half is proven behaviourally by
/// [`a_skill_turn_spends_the_naming_attempt_on_the_expansion_not_on_an_empty_string`];
/// this adds the classifier's half, which no integration test can observe (the
/// `route` category resolves to the local tier or to nothing, and an integration
/// test cannot install a local engine).
#[test]
fn the_expansion_is_built_before_either_reader_of_the_prompt_text() {
    let body = run_prompt_turn_body();
    let expand = at(&body, "accept_invocation(");
    let classify = at(&body, "dispatch_route(");
    let title = at(&body, "spawn_title_session(");

    assert!(
        expand < classify,
        "routing ran over the prompt text before the expansion existed, so every \
         invocation is classified from `\"\"`"
    );
    assert!(
        expand < title,
        "the naming attempt ran over the prompt text before the expansion \
         existed, so every invocation names its session from `\"\"`"
    );
}

/// **BR-4's last clause.** The expansion is a *prompt*, not a tool result, so
/// the `digest` duty never touches it: REQ-586 scaled the summarization
/// thresholds with the route budget, and a skill body sits squarely inside the
/// band that would trigger one — a `digest` that reached the expansion would
/// condense the turn into a summary of itself, which BR-8 forbids in as many
/// words.
///
/// Pinned as a fact about call sites rather than about behaviour, because the
/// only way to observe it behaviourally is to *have* the bug.
#[test]
fn the_digest_duty_has_one_production_call_site_and_the_turn_path_is_not_it() {
    let fold = production_source("harness/turn_loop.rs");
    assert_eq!(
        fold.matches("summarize_if_large(").count(),
        1,
        "the tool-result fold is `summarize_if_large`'s one production call site"
    );

    let runtime = production_source("runtime.rs");
    assert_eq!(
        runtime.matches("summarize_if_large(").count(),
        0,
        "the turn path called the digest duty; a skill expansion is carried \
         whole or refused, never condensed (BR-4)"
    );
}

// ---------------------------------------------------------------------------
// a minimal JSON-RPC client
// ---------------------------------------------------------------------------

/// The counter, not the timestamp, guarantees uniqueness: `SystemTime::now()`
/// can return the same value for two calls within one clock tick.
fn temp_socket(tag: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    std::env::temp_dir().join(format!(
        "teton-{tag}-{}-{}.sock",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    ))
}

/// A newline-delimited JSON-RPC client over the daemon socket.
struct TestClient {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next_id: i64,
}

impl TestClient {
    async fn connect(path: &Path) -> Self {
        let stream = UnixStream::connect(path).await.unwrap();
        let (read_half, write_half) = stream.into_split();
        Self {
            reader: BufReader::new(read_half),
            writer: write_half,
            next_id: 1,
        }
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let mut text = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .unwrap();
        text.push('\n');
        self.writer.write_all(text.as_bytes()).await.unwrap();
        self.writer.flush().await.unwrap();
        loop {
            let mut line = String::new();
            let read = timeout(Duration::from_secs(10), self.reader.read_line(&mut line))
                .await
                .expect("timed out waiting for a frame")
                .unwrap();
            assert!(read > 0, "connection closed unexpectedly");
            let frame: Value = serde_json::from_str(&line).unwrap();
            if frame.get("id").and_then(Value::as_i64) == Some(id) {
                return frame;
            }
        }
    }

    async fn handshake(&mut self) {
        let answer = self
            .call(
                "handshake",
                json!({
                    "client_kind": "cli",
                    "client_name": "skill-turn-test-client",
                    "client_version": "0.1.0",
                    "protocol_min": PROTOCOL_VERSION_MIN,
                    "protocol_max": PROTOCOL_VERSION_MAX,
                    "monitor": false,
                }),
            )
            .await;
        assert!(answer.get("result").is_some(), "handshake failed: {answer}");
    }

    async fn create_session_at(&mut self, cwd: &Path) -> String {
        let created = self
            .call("session/create", json!({"mode": "freeform", "cwd": cwd}))
            .await;
        created["result"]["session_id"]
            .as_str()
            .unwrap_or_else(|| panic!("session/create failed: {created}"))
            .to_owned()
    }
}
