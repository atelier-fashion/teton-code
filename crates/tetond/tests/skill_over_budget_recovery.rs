//! **REQ-589 BR-14 / D-8 — an approval must not leave the session hitting the
//! same wall** (TASK-257).
//!
//! The second half of REQ-589's integration coverage. `skill_over_budget_offer.rs`
//! owns the *question*; this file owns what happens **after** it is answered, and
//! the claims it makes are the ones a user who approved once actually
//! cares about:
//!
//! | Claim | Test |
//! |---|---|
//! | AC-22 — an accepted turn refused at the window leaves the session carrying the **refusal** | [`the_next_turn_after_a_window_refusal_carries_the_refusal_that_replaced_the_expansion`] |
//! | AC-22's control — on the serving path the expansion is lost at the commit anyway | [`an_accepted_turn_that_serves_still_loses_the_expansion_at_the_commit`] |
//! | AC-23 — the next offer for the pair leads with the rejection this daemon watched, and neither suppresses nor pre-answers the question | [`a_watched_rejection_leads_the_next_offer_without_suppressing_or_pre_answering_it`] |
//! | AC-24 — after the `BindTierRemote` remedy is applied, an identical invocation meets **no offer at all** | [`the_applied_rebind_closes_the_circle_the_second_invocation_meets_no_offer`] |
//! | ASSUME-017 — the observed-rejection record has no client half | [`the_observed_rejection_record_has_no_client_half`] |
//!
//! ## What AC-22 asserts, and why it is not what AC-22 says
//!
//! AC-22 is worded "the next turn assembles **without** the expansion". That
//! assertion is **vacuous**, and this file establishes it by measurement rather
//! than by argument — the control test drives the same fixture on the path where
//! the accepted turn *succeeds*, and the expansion is gone from the session
//! there too. Two independent gates remove it whatever happens: REQ-586 BR-10's
//! budget re-assertion at the commit, and ordinary context pressure on the turn
//! after. On top of that, `run_prompt_turn`'s ordinary failure arm calls
//! `CarriedTurn::abandon`, which writes nothing at all — so with the withdrawal
//! deleted outright the expansion is *still* absent. An assertion that holds
//! whether or not the mechanism exists proves nothing (LESSON-520), and this one
//! is unusually easy to write by accident.
//!
//! So the load-bearing claim here is the **positive** one TASK-249 made
//! observable: the withdrawal *commits*, on exactly this one failure path, and
//! what it commits is the refusal in the expansion's place. Two assertions carry
//! it — the refusal is a **block in the session**, and it is in the **next
//! turn's assembled prompt** — and both were verified to redden against a tree
//! with `withdraw_accepted_expansion` removed. `withdraw the expansion` and
//! `write nothing at all` are indistinguishable by absence and distinguishable
//! only by presence.
//!
//! ## Everything is driven from real turns
//!
//! LESSON-544 / LESSON-552: no test here builds a `PermissionRequest`, a
//! `RemedyPlan` or a conversation by hand. Each drives `run_prompt_turn` — the
//! same entry point `session/prompt` reaches — over a real `DaemonRuntime` built
//! from a real config file, answers the offer through the real
//! `AddressedPermissionDelivery` seam, and reads the bytes a `MockProvider`
//! captured off the wire. The observed-rejection store is never inspected
//! directly: what is asserted is the **sentence** the next offer was composed
//! with, because a store nothing reads is a producer with no consumer.
//!
//! ## Only reachable (bound, verdict) cells
//!
//! architecture.md's reachability table is normative. Two cells are used:
//!
//! * `Window` + `ExceedsWindow` — a declared 20,000-token window the measurement
//!   blows. The daemon says it will very likely be rejected, the user proceeds,
//!   and the provider rejects it. AC-22's and AC-23's route. 20,000 rather than
//!   a smaller figure because anything under ~13,000 derives a *floored* pair
//!   (REQ-586's own floor), and a floored route is a different sentence about a
//!   different thing — true, reachable, and not what these tests are about.
//! * `LocalEngine` + `WindowUnknown` — the route the reported `/analyze` failure
//!   ran on, and the only bound whose remedy is BR-9's `BindTierRemote`. AC-24's
//!   route.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use teton_protocol::events::{
    BudgetBound, Event, PermissionOptionKind, PermissionRequest, PermissionSubject, RemedyKind,
    WindowVerdict, OPTION_ID_OVER_BUDGET_DECLINE, OPTION_ID_OVER_BUDGET_PROCEED_ONCE,
    OPTION_ID_OVER_BUDGET_REMEDY_ONLY,
};
use teton_protocol::jsonrpc::error_code;
use teton_protocol::methods::{PermissionOutcome, SkillInvocation, StopReason};
use teton_protocol::{SessionId, SessionMode, Tier};

use tetond::broadcast::{EventBus, Subscription};
use tetond::grants::{ConnectionId, GrantRegistry};
use tetond::harness::permissions::{AddressedPermissionDelivery, CommitmentAttestation};
use tetond::runtime::{ClientPresence, DaemonRuntime};
use tetond::sessions::SessionRegistry;
use tetond::skills::RealFs;

#[path = "e2e/harness.rs"]
mod daemon_harness;

use daemon_harness::{openai_turn, MockProvider, MockResponse};

// ---------------------------------------------------------------------------
// the sentences under test, quoted once
// ---------------------------------------------------------------------------

/// BR-14.2's lead, verbatim from `harness::budget`.
///
/// Quoted rather than imported because it is `const`-private there, and because
/// a test that imported the string it asserts would pass on any wording at all
/// — including an empty one. What is pinned here is the sentence a *user* reads.
const OBSERVED_REJECTION_LEAD: &str =
    "This skill was already rejected at the provider's window on this route, in this session — \
     that is an observation of what happened, not a decision about what happens now.";

/// BR-3's `ExceedsWindow` clause — the prediction the offer makes before the
/// user answers, and the one the provider then confirms.
const EXCEEDS_WINDOW_CLAUSE: &str =
    "This will blow the context window this route declares: proceeding without raising it will \
     very likely be rejected by the provider.";

/// The head of `ContextRefusal::sentence`, which is what the withdrawal leaves
/// in the expansion's place — one composer for the block the model reads and
/// the error the user reads (BR-5).
const WINDOW_REFUSAL_HEAD: &str = "refused this turn as larger than";

/// BR-7a's risk clause, which a `RaiseWindow` remedy may not shed — the half of
/// AC-23's "leads with the remedy" that says the remedy is still *whole* in the
/// second offer, not merely mentioned.
const RAISE_WINDOW_RISK: &str =
    "raising a declared window above the provider's real one does not enlarge that window, it \
     makes this daemon send requests the provider will reject, turning a refusal here into an \
     error there";

/// The closing question an offer carries where BR-7 grants the bound a remedy.
const CLOSING_WITH_REMEDY: &str =
    "Send it whole this once, take the durable fix, both, or neither?";

/// The clause that makes `-32023` different from `-32022`, and the one the
/// decline refusal keeps (AC-3).
const NOTHING_WAS_SENT: &str =
    "Nothing was sent and no provider saw this turn — a skill expansion is carried whole or \
     refused, never shortened into something you did not invoke.";

/// A model the shipped vendor catalog recognizes, so BR-7c has a window to
/// propose and the remedy-bearing options actually appear: `kimi-k3` carries
/// `max_context = 1_000_000` (`provider_recipes.rs`, verified 2026-08-19).
const RECIPE_MODEL: &str = "kimi-k3";

/// The window that recipe declares. AC-24's whole arithmetic: the rebind writes
/// it, and it is large enough that the *same* expansion then fits — which is
/// what "the circle is closed" means as a number.
const RECIPE_WINDOW: u32 = 1_000_000;

/// The window AC-22's and AC-23's route declares.
///
/// Chosen so the derived pair is **not floored**: below roughly 13,000 the
/// derivation lands under the smallest budget that holds the system prompt and
/// REQ-586's floor raises it, which makes the bound say the declaration is not
/// in force. That is a true and reachable sentence about a different thing, and
/// it does not belong in a fixture whose subject is what happens *after* the
/// answer.
const DECLARED_WINDOW: u32 = 20_000;

/// The marker the fixture skill's body carries, so "the expansion reached the
/// provider" and "the expansion did not survive into the next prompt" are
/// claims about **bytes on the wire** rather than about a request count.
const EXPANSION_MARKER: &str = "OVERBUDGETEXPANSIONMARKERZQ";

/// A 400 body an OpenAI-compatible adapter classifies as
/// `ProviderError::ContextLengthExceeded` — the sniff keys on the quoted code
/// (`teton-providers`'s `OPENAI_CONTEXT_LENGTH_CODE`), which is why the code
/// rides the body rather than only the message.
fn context_length_refusal() -> MockResponse {
    MockResponse::status_with_body(
        400,
        r#"{"error":{"code":"context_length_exceeded","message":"maximum context length"}}"#,
    )
}

/// The turn a healthy provider answers with.
fn served_turn() -> MockResponse {
    MockResponse::ok(openai_turn("done", None, 5, 5))
}

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

/// A throwaway tree, removed on drop, holding one fixture's config, its project
/// skill and its fixture `HOME`.
///
/// The name is **fixed-width** on purpose: the root path reaches the system
/// prompt, the system prompt is an input to every figure Stage A measures, and
/// two trees whose names differ in length therefore do not share an overhead
/// constant.
struct Tree {
    root: PathBuf,
}

impl Tree {
    fn new() -> Self {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        let root =
            PathBuf::from("/tmp").join(format!("rc{seq:02x}{:04x}", std::process::id() & 0xffff));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("the fixture tree");
        // A project marker, so the root probes as `project` and the project half
        // of skill discovery is reached at all.
        std::fs::write(root.join("Cargo.toml"), "[package]\n").expect("the project marker");
        Self { root }
    }

    fn write(&self, rel: &str, contents: &str) {
        let path = self.root.join(rel);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the fixture directory");
        std::fs::write(path, contents).expect("the fixture file");
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

/// What the scripted local engine answers with.
const SCRIPTED_REPLY: &str = "SCRIPTED-LOCAL-TURN-REPLY";

/// The process-wide seams every fixture in this binary runs under, installed
/// once before any runtime exists.
///
/// `TETON_LOCAL_SCRIPT` is what gives these daemons a **local tier at all**, and
/// with it AC-24's route. A scripted engine is exempt from the first-run consent
/// flow (it fetches nothing), so no fixture here has to answer a model proposal
/// before it can reach a turn.
fn seams() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        assert!(
            std::env::var_os("TETON_CONFIG").is_none(),
            "TETON_CONFIG in the environment would override every fixture config in this file"
        );
        let base = PathBuf::from("/tmp").join(format!("rcseam{:x}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("the seam directory");
        let script = base.join("local_script.txt");
        std::fs::write(&script, format!("{SCRIPTED_REPLY}\n")).expect("the local script");
        std::env::set_var("TETON_TEST_SEAMS", "1");
        std::env::set_var("TETON_LOCAL_SCRIPT", &script);
        std::env::set_var("TETON_PROBE_RAM_BYTES", (16u64 << 30).to_string());
        std::env::set_var("TETON_PROBE_DISK_BYTES", "500000000000");
        std::env::set_var("TETON_PROBE_GPU", "apple-silicon");
    });
}

/// How this fixture's client answers the **offer**.
///
/// By option **id**, never by [`PermissionOptionKind`]: two of the four ids
/// share a kind, and telling `proceed_once` from `remedy_only` is the whole of
/// AC-24's first turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    Select(&'static str),
}

/// The client every turn here comes from: it records every request it was shown
/// and answers the two questions a project-skill turn can raise — ADR-10's trust
/// acknowledgment (always allow-once; it is not what any test here is about) and
/// the over-budget offer itself.
struct Client {
    pending: Arc<tetond::harness::PendingPermissions>,
    answer: Mutex<Answer>,
    asked: Mutex<Vec<PermissionRequest>>,
}

impl Client {
    fn new(pending: &Arc<tetond::harness::PendingPermissions>, answer: Answer) -> Arc<Self> {
        Arc::new(Self {
            pending: Arc::clone(pending),
            answer: Mutex::new(answer),
            asked: Mutex::new(Vec::new()),
        })
    }

    /// Only the over-budget offers — the trust acknowledgment is a different
    /// question and would otherwise inflate every count below.
    fn offers(&self) -> Vec<PermissionRequest> {
        self.asked
            .lock()
            .expect("asked mutex poisoned")
            .iter()
            .filter(|r| matches!(r.subject, Some(PermissionSubject::SkillOverBudget { .. })))
            .cloned()
            .collect()
    }

    /// Change what the next offer is answered with, so one session can be driven
    /// through two *different* answers — which is how "the record did not
    /// pre-answer the question" is shown rather than asserted.
    fn answers(&self, answer: Answer) {
        *self.answer.lock().expect("answer mutex poisoned") = answer;
    }
}

impl AddressedPermissionDelivery for Client {
    fn deliver(
        &self,
        connection: ConnectionId,
        _session_id: &SessionId,
        request: PermissionRequest,
    ) -> bool {
        self.asked
            .lock()
            .expect("asked mutex poisoned")
            .push(request.clone());
        let is_offer = matches!(
            request.subject,
            Some(PermissionSubject::SkillOverBudget { .. })
        );
        let outcome = if is_offer {
            match *self.answer.lock().expect("answer mutex poisoned") {
                Answer::Select(id) => {
                    // Asserted rather than defaulted: an id the prompt did not
                    // carry would mean the gate narrowed the option list, and
                    // quietly answering something else would hide that — which
                    // is exactly the failure AC-23's first negative assertion
                    // exists to catch.
                    assert!(
                        request.options.iter().any(|o| o.option_id == id),
                        "the offer did not carry `{id}`: {:?}",
                        request.options
                    );
                    PermissionOutcome::Selected {
                        option_id: id.to_owned(),
                    }
                }
            }
        } else {
            let option = request
                .options
                .iter()
                .find(|o| o.kind == PermissionOptionKind::AllowOnce)
                .unwrap_or_else(|| panic!("the acknowledgment offers allow-once: {request:?}"));
            PermissionOutcome::Selected {
                option_id: option.option_id.clone(),
            }
        };
        self.pending
            .resolve_from(&request.request_id, outcome, connection)
    }
}

/// The dispatchable name every fixture registers its oversized skill under.
const SKILL: &str = "heavy";

/// One daemon over one config file, with one oversized project skill and one
/// client.
struct Fixture {
    runtime: Arc<DaemonRuntime>,
    events: Arc<EventBus>,
    sessions: SessionRegistry,
    client: Arc<Client>,
    connection: ConnectionId,
    /// The fixture's own `HOME`, so the four discovery globs cover this tree
    /// only and nothing here depends on the runner's `~/.claude/skills`.
    home: PathBuf,
    tree: Tree,
}

impl Fixture {
    fn new(config: &str, body: &str, answer: Answer) -> Self {
        Self::built(config, body, answer, None)
    }

    /// As [`Self::new`], with `attestation` wired as the runtime's
    /// [`CommitmentAttestation`] — REQ-591 D-1's seam.
    fn attesting(
        config: &str,
        body: &str,
        answer: Answer,
        attestation: Arc<StandingAttestation>,
    ) -> Self {
        Self::built(config, body, answer, Some(attestation))
    }

    fn built(
        config: &str,
        body: &str,
        answer: Answer,
        attestation: Option<Arc<StandingAttestation>>,
    ) -> Self {
        seams();
        let tree = Tree::new();
        tree.write("config.toml", config);
        let home = tree.path().join("home");
        std::fs::create_dir_all(home.join(".claude/skills")).expect("the fixture home");
        tree.write(
            &format!(".claude/skills/{SKILL}/SKILL.md"),
            &format!("---\ndescription: the oversized skill\n---\n\n{body}\n"),
        );

        let events = Arc::new(EventBus::new());
        let runtime = Arc::new(
            DaemonRuntime::from_env(tree.path(), &events).expect("the fixture daemon starts"),
        );
        let client = Client::new(runtime.pending(), answer);
        runtime
            .install_addressed_delivery(Arc::clone(&client) as Arc<dyn AddressedPermissionDelivery>);
        // Absent by default, which is the pre-D-1 posture and the one every
        // other test in this file is about: with no seam wired the remedy
        // writes on the connection's standing alone, exactly as a build with no
        // presence mechanism does.
        if let Some(attestation) = attestation {
            runtime.install_commitment_attestation(attestation as Arc<dyn CommitmentAttestation>);
        }

        Self {
            runtime,
            events,
            sessions: SessionRegistry::new(),
            client,
            connection: GrantRegistry::new().next_connection_id(),
            home,
            tree,
        }
    }

    /// A freeform session rooted in this tree, with this tree's skills
    /// discovered into it.
    fn session(&self) -> SessionId {
        let id = self
            .sessions
            .create(SessionMode::Freeform, None, Some(self.tree.root.clone()))
            .expect("a freeform session")
            .session_id;
        let probed = self.runtime.session_root_for(Some(self.tree.path()));
        self.sessions.set_skills(
            &id,
            tetond::skills::discover(Some(&self.home), &probed.path, probed.view.kind, &RealFs),
        );
        id
    }

    /// Type `/heavy` — the user-invoked path, the only one an offer is raised on
    /// (BR-2).
    async fn invoke(
        &self,
        session: &SessionId,
    ) -> Result<teton_protocol::methods::PromptTurnResult, teton_protocol::jsonrpc::RpcError> {
        self.turn(
            session,
            String::new(),
            Some(SkillInvocation {
                name: SKILL.to_owned(),
                raw_arguments: String::new(),
            }),
        )
        .await
    }

    /// A **real** second turn in the same session: an ordinary typed prompt,
    /// with no skill, assembled from whatever the session is now carrying.
    async fn say(
        &self,
        session: &SessionId,
        text: &str,
    ) -> Result<teton_protocol::methods::PromptTurnResult, teton_protocol::jsonrpc::RpcError> {
        self.turn(session, text.to_owned(), None).await
    }

    async fn turn(
        &self,
        session: &SessionId,
        prompt: String,
        skill: Option<SkillInvocation>,
    ) -> Result<teton_protocol::methods::PromptTurnResult, teton_protocol::jsonrpc::RpcError> {
        self.runtime
            .run_prompt_turn(
                &self.events,
                &self.sessions,
                session.clone(),
                SessionMode::Freeform,
                None,
                Some(self.tree.root.clone()),
                prompt,
                skill,
                Some(self.connection),
                ClientPresence::unwatched(),
            )
            .await
    }

    /// The config document as it stands **on disk** — where AC-24's durable half
    /// is read from (LESSON-519: inspect the artifact, never a return code).
    fn config_on_disk(&self) -> String {
        std::fs::read_to_string(self.tree.path().join("config.toml")).expect("the config file")
    }

    /// The same document **re-parsed by a fresh daemon**, which is the other
    /// half of LESSON-519's double-check: a file that reads right but does not
    /// load right is not a fix.
    fn config_as_reloaded(&self) -> teton_protocol::methods::ConfigSnapshot {
        let events = Arc::new(EventBus::new());
        DaemonRuntime::from_env(self.tree.path(), &events)
            .expect("the written config loads")
            .config_snapshot()
    }
}

/// Everything the bus carried, in order. `publish` is synchronous, so a drain
/// taken after the turn returns covers everything the turn published
/// (LESSON-450: no wall-clock polling).
fn drain(sub: &mut Subscription) -> Vec<Event> {
    let mut out = Vec::new();
    while let Some(envelope) = sub.try_recv() {
        out.push(envelope.event);
    }
    out
}

/// The head of every block a session is holding, for a failure message that
/// names what was there instead of only what was not.
fn block_heads(conversation: &tetond::sessions::Conversation) -> Vec<String> {
    conversation
        .blocks()
        .iter()
        .map(|block| block.text.chars().take(80).collect::<String>())
        .collect()
}

fn remedies(published: &[Event]) -> Vec<teton_protocol::events::SkillOverBudgetRemedyApplied> {
    published
        .iter()
        .filter_map(|e| match e {
            Event::SkillOverBudgetRemedyApplied(r) => Some(r.clone()),
            _ => None,
        })
        .collect()
}

/// One offer's subject, destructured — every assertion reads the wire fact,
/// never a value the test built.
fn subject_of(request: &PermissionRequest) -> (BudgetBound, WindowVerdict, String) {
    match request.subject.clone() {
        Some(PermissionSubject::SkillOverBudget {
            bound,
            window_verdict,
            sentence,
            ..
        }) => (bound, window_verdict, sentence),
        other => panic!("not an over-budget offer: {other:?}"),
    }
}

fn option_ids(request: &PermissionRequest) -> Vec<String> {
    request
        .options
        .iter()
        .map(|o| o.option_id.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// route documents
// ---------------------------------------------------------------------------

/// A remote provider row, with the capabilities table where one is asked for.
fn remote_provider(id: &str, endpoint: &str, model: &str, max_context: Option<u32>) -> String {
    let mut cfg = format!(
        "[[providers]]\nid = \"{id}\"\nkind = \"openai-compatible\"\nendpoint = \"{endpoint}\"\n\
         model = \"{model}\"\n\n"
    );
    if let Some(window) = max_context {
        cfg.push_str(&format!(
            "[providers.capabilities]\nmax_context = {window}\n\n"
        ));
    }
    cfg
}

/// A route whose turn-serving tiers are remote and whose `reflex` tier is not.
///
/// `reflex` stays local on purpose: `route`, `title` and `redact` all hang off
/// it, so binding it remotely would interleave a duty's request with the turn's
/// in the captured egress and make "the second request is the second turn" a
/// claim about scheduling. With it local, every entry in `requests()` is a turn.
fn remote_route(endpoint: &str, max_context: u32) -> String {
    let mut cfg = String::from("[[providers]]\nid = \"local\"\nkind = \"local\"\n\n");
    cfg.push_str(&remote_provider(
        "frontier",
        endpoint,
        RECIPE_MODEL,
        Some(max_context),
    ));
    cfg.push_str("[[tiers]]\ntier = \"reflex\"\nprovider_id = \"local\"\n\n");
    for tier in ["scan", "build", "think"] {
        cfg.push_str(&format!(
            "[[tiers]]\ntier = \"{tier}\"\nprovider_id = \"frontier\"\n\n"
        ));
    }
    cfg
}

/// Every tier on the local engine, with **exactly one** remote provider
/// registered and unbound — the route the reported `/analyze` failure ran on,
/// and ADR-12's "propose by name" case, which is the only shape in which the
/// `BindTierRemote` remedy carries options rather than being withheld.
///
/// The remote declares **no** window, so BR-9's first write is a real
/// declaration rather than a re-statement: `rebind_window` falls through to the
/// shipped recipe for `kimi-k3`, which is the figure AC-24 then reads back off
/// disk.
fn local_route_with_one_remote(endpoint: &str) -> String {
    let mut cfg = String::from("[[providers]]\nid = \"local\"\nkind = \"local\"\n\n");
    cfg.push_str(&remote_provider("frontier", endpoint, RECIPE_MODEL, None));
    for tier in ["reflex", "scan", "build", "think"] {
        cfg.push_str(&format!(
            "[[tiers]]\ntier = \"{tier}\"\nprovider_id = \"local\"\n\n"
        ));
    }
    cfg
}

// ---------------------------------------------------------------------------
// bodies
// ---------------------------------------------------------------------------

/// A body of exactly `word_count` whitespace words and exactly `byte_count`
/// bytes, whose **first word is [`EXPANSION_MARKER`]**.
///
/// Sized in both currencies because the budget is a pair, and a fixture that
/// quotes a word count while the byte guard is what actually fired is testing
/// something other than what it claims (REQ-586 Phase-3 F-19). The marker is a
/// word like any other, so it costs the arithmetic nothing.
fn marked_body(word_count: usize, byte_count: usize) -> String {
    assert!(word_count >= 2, "the marker is one word and needs company");
    let letters = byte_count
        .checked_sub(word_count - 1)
        .expect("the byte count must cover the separating spaces");
    let filler_words = word_count - 1;
    let filler_letters = letters
        .checked_sub(EXPANSION_MARKER.len())
        .expect("the byte count must cover the marker");
    assert!(
        filler_letters >= filler_words,
        "each of {filler_words} filler words needs at least one letter"
    );
    let base = filler_letters / filler_words;
    let mut extra = filler_letters % filler_words;
    let mut out = String::with_capacity(byte_count);
    out.push_str(EXPANSION_MARKER);
    for _ in 0..filler_words {
        out.push(' ');
        let mut len = base;
        if extra > 0 {
            len += 1;
            extra -= 1;
        }
        out.extend(std::iter::repeat_n('a', len));
    }
    assert_eq!(
        out.len(),
        byte_count,
        "marked_body must hit its byte target"
    );
    assert_eq!(
        out.split_whitespace().count(),
        word_count,
        "marked_body must hit its word target"
    );
    out
}

/// The body AC-22 and AC-23 run: over the 20,000-token window's derived budget
/// (12,650 words / 37,952 B) in **both** currencies once the system head is
/// counted, and over the raw window under the projection BR-3's verdict uses —
/// the `ExceedsWindow` cell, un-floored.
fn oversized_for_a_declared_window() -> String {
    marked_body(12_000, 48_000)
}

/// The body AC-24 runs: over the local pair in **both** currencies, and
/// comfortably inside the 1,000,000-token window the rebind declares.
///
/// Sized off `derive(BudgetInputs::local())` rather than written as two
/// literals, because this fixture's whole job is to be over that pair and the
/// pair now moves. It was `(6,000, 24,000)` — half again over a 4,096-word
/// budget, and over nothing else once REQ-590 raised the word half to 10,240.
/// It went on drawing an offer only because the system prompt pushed it past a
/// byte half that had briefly fallen to 30,720, and stopped the moment ADR-9
/// put that back to 32,768. A fixture named `oversized_for_the_local_pair` that
/// is not oversized for the local pair takes both tests below with it, silently.
///
/// A quarter past the word half at 4 bytes a word absorbs Stage A's own
/// overhead, which this fixture does not own — the body is measured *with* the
/// system prompt — while staying clear of discovery's per-file
/// `SKILL_MAX_BYTES`. Those two ceilings are closer than they look: at 4 B/word
/// a quarter past the 21,162-word half is ~106 KB against a 128 KiB file
/// ceiling (the ceiling was 64 KiB while the pair was 10,240 w / 32,768 B, and
/// doubling that pair landed on it exactly), past which the skill is
/// **skipped** and these tests fail with "no skill `/heavy` you can dispatch"
/// rather than with anything about a budget.
fn oversized_for_the_local_pair() -> String {
    let local = tetond::harness::budget::derive(tetond::harness::BudgetInputs::local());
    let words = local.budget_tokens + local.budget_tokens / 4;
    let bytes = words * 4;
    assert!(
        bytes > local.budget_bytes && (bytes as u64) < tetond::skills::SKILL_MAX_BYTES * 9 / 10,
        "the fixture must clear the local byte half ({} B) and stay clear of discovery's {} B \
         per-file ceiling; it is {bytes} B",
        local.budget_bytes,
        tetond::skills::SKILL_MAX_BYTES
    );
    marked_body(words, bytes)
}

// ---------------------------------------------------------------------------
// AC-22 — the withdrawal, observed on the turn after it
// ---------------------------------------------------------------------------

/// **AC-22 / BR-14.1 — the next turn carries the refusal that replaced the
/// expansion.**
///
/// # This asserts presence, not absence, and that is the whole point
///
/// "The next turn assembles without the expansion" is satisfied by *three*
/// different things, only one of which is the mechanism: `run_prompt_turn`'s
/// ordinary failure arm abandons and writes nothing; REQ-586 BR-10's budget
/// re-assertion drops the oversized block at the commit; and ordinary context
/// pressure drops it again on the turn after. So the absence-only assertion is
/// green with `withdraw_accepted_expansion` deleted — measured, not assumed
/// (`an_accepted_turn_that_serves_still_loses_the_expansion_at_the_commit`).
/// TASK-249 is what makes AC-22 testable at all: it commits the withdrawal on
/// exactly this one failure path, so the mechanism has an observable
/// consequence — the refusal is *there*.
///
/// Two assertions carry it, and **both were verified to redden** against a tree
/// with the withdrawal call removed:
///
/// 1. the refusal is a **block in the committed conversation** after turn one —
///    which excludes the abandon path, where the session holds nothing;
/// 2. the refusal is in the **next turn's assembled prompt** — AC-22's own
///    "real second turn", read as bytes off the wire.
///
/// # And it is a real second turn
///
/// AC-22 asks for one in as many words, and the conversation snapshot alone
/// would be a claim about a data structure. The second turn is an ordinary
/// typed prompt driven through `run_prompt_turn`, and what assertion (2) reads
/// is the bytes the provider actually received. The snapshot is the *addition*
/// AC-22's wording allows ("rather than inspecting the block list alone"), not
/// the substitute it forbids.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_next_turn_after_a_window_refusal_carries_the_refusal_that_replaced_the_expansion() {
    // The accepted turn is refused at the window; every later request is served.
    let provider = MockProvider::start(vec![context_length_refusal()], served_turn());
    let fx = Fixture::new(
        &remote_route(&provider.openai_endpoint(), DECLARED_WINDOW),
        &oversized_for_a_declared_window(),
        Answer::Select(OPTION_ID_OVER_BUDGET_PROCEED_ONCE),
    );
    let session = fx.session();

    // Turn one: offered, accepted, sent whole, and refused by the provider.
    let refused = fx
        .invoke(&session)
        .await
        .expect_err("the provider refused the accepted turn at its window");
    assert_eq!(
        refused.code,
        error_code::CONTEXT_LENGTH_EXCEEDED,
        "the accepted turn must end as a window refusal, not as a generic failure — a \
         different code here means this test is exercising a different path: {refused:?}"
    );

    let (bound, verdict, sentence) = subject_of(
        fx.client
            .offers()
            .first()
            .expect("the oversized expansion raised an offer"),
    );
    assert_eq!(
        (bound, verdict),
        (BudgetBound::Window, WindowVerdict::ExceedsWindow),
        "the reachability table's `Window` + `ExceedsWindow` cell is the one this \
         fixture means to be in; any other cell makes the assertions below claims \
         about a route nobody configured"
    );
    assert!(
        sentence.contains(EXCEEDS_WINDOW_CLAUSE),
        "the offer predicted the refusal the provider then performed: {sentence}"
    );

    let sent_first = String::from_utf8_lossy(
        provider
            .requests()
            .first()
            .expect("the accepted turn reached the provider"),
    )
    .into_owned();
    assert!(
        sent_first.contains(EXPANSION_MARKER),
        "the premise of everything below: the approved expansion really did go out \
         whole (BR-1), so what the provider refused is what the user consented to send"
    );

    // **What the session is left holding, read before the next turn runs.**
    // The snapshot has to be taken *here*: turn two's own commit re-asserts the
    // budget and would have dropped an oversized block by then, so a snapshot
    // taken later cannot tell the paths apart. This is also the first of the two
    // assertions verified to redden with `withdraw_accepted_expansion` deleted —
    // that mutation leaves the session empty, because the ordinary failure arm
    // abandons.
    let committed = fx.sessions.conversation_snapshot(&session);
    assert!(
        !committed.blocks().is_empty(),
        "non-vacuity: a session holding nothing is the abandon path — the very outcome \
         this test exists to exclude"
    );
    assert!(
        committed
            .blocks()
            .iter()
            .any(|block| block.text.contains(WINDOW_REFUSAL_HEAD)),
        "the refusal must BE a block in the session, not merely be reported to the \
         user: {:?}",
        block_heads(&committed)
    );

    // Turn two: an ordinary typed prompt, in the same session.
    fx.say(&session, "and what did that come back with?")
        .await
        .expect("the session takes another turn — which is D-8's whole ask");

    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        2,
        "non-vacuity: the second turn never reached the provider, so nothing below is \
         about an assembled prompt"
    );
    let assembled = String::from_utf8_lossy(&requests[1]).into_owned();

    // **The load-bearing assertion**, and the one verified to redden: deleting
    // the `withdraw_accepted_expansion` call reverts this path to `abandon`,
    // the session carries nothing out of turn one, and this fails. A withdrawal
    // that wrote nothing and a turn that abandoned are indistinguishable by
    // absence; only presence tells them apart.
    assert!(
        assembled.contains(WINDOW_REFUSAL_HEAD),
        "the next turn must carry the refusal that replaced the expansion — without it \
         this path abandoned and wrote nothing, which is the circle D-8 exists to \
         close: {assembled}"
    );

    // **AC-22's other half is deliberately NOT asserted anywhere in this file,
    // and this is the note that says why.** "The next turn assembles without
    // the expansion" is **vacuous at every observation point**, measured rather
    // than assumed: the oversized block does not survive REQ-586 BR-10's budget
    // re-assertion at the commit itself, so it is gone from the session even on
    // the path where the turn *succeeded* and nothing withdrew anything — and
    // gone again from the next turn's prompt, dropped by ordinary pressure. An
    // assertion that holds whether or not the mechanism exists proves nothing
    // (LESSON-520). `an_accepted_turn_that_serves_still_loses_the_expansion_at_
    // the_commit` pins both halves of that fact, so nobody re-adds the
    // assertion here believing it means something.
    //
    // What is left is the whole of what is real: the refusal *is there*, in the
    // session and on the next turn's wire, and it is there only because the
    // withdrawal committed it.
}

/// **AC-22's control, on the same fixture — the accepted turn that *serves*,
/// and the trap it caught.**
///
/// Every assertion in the leg above is a presence claim about one string, and
/// it could hold for a reason that has nothing to do with the withdrawal. This
/// is the counterpart REQ-589's testing posture asks for (LESSON-519: pair
/// every refusal test with an accepted one on the same fixture). Exactly one
/// thing differs: the provider **serves** the accepted turn instead of refusing
/// it, so there is no window refusal and nothing is withdrawn.
///
/// It establishes two things, and the second is a correction to how AC-22 reads:
///
/// 1. **The refusal's presence discriminates.** Here the refusal is in neither
///    the committed conversation nor the next turn's prompt, so the leg above is
///    not passing on a string that is always there.
/// 2. **The expansion's *absence* discriminates nothing — at any observation
///    point — which is why no test in this file asserts it.** This was measured,
///    not reasoned: on this serving path the expansion is already gone from the
///    session the moment turn one commits, because REQ-586 BR-10's budget
///    re-assertion at the commit drops an oversized block like any other, and
///    it is gone from turn two's prompt for the same reason one gate later.
///    "The next turn assembles without the expansion" is therefore true on the
///    success path, on the withdrawal path, and — as the mutation run showed —
///    with the withdrawal deleted outright. That is LESSON-520's shape exactly,
///    and it is the assertion AC-22's wording invites.
///
/// So this test asserts those absences **as the facts they are**, on the path
/// where nothing withdrew anything. If either ever starts failing, the vacuity
/// has gone away and the leg above should gain the assertion it currently
/// documents its reasons for omitting.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_accepted_turn_that_serves_still_loses_the_expansion_at_the_commit() {
    // The one difference from the leg above: the provider serves it.
    let provider = MockProvider::start(Vec::new(), served_turn());
    let fx = Fixture::new(
        &remote_route(&provider.openai_endpoint(), DECLARED_WINDOW),
        &oversized_for_a_declared_window(),
        Answer::Select(OPTION_ID_OVER_BUDGET_PROCEED_ONCE),
    );
    let session = fx.session();

    fx.invoke(&session)
        .await
        .expect("the accepted, oversized turn served");

    let committed = fx.sessions.conversation_snapshot(&session);
    assert!(
        !committed.blocks().is_empty(),
        "non-vacuity: a served turn commits *something*, or the two absences below are \
         about an empty session rather than about a populated one"
    );
    // (1a) the leg above's conversation-level presence assertion discriminates.
    assert!(
        committed
            .blocks()
            .iter()
            .all(|block| !block.text.contains(WINDOW_REFUSAL_HEAD)),
        "nothing refused this turn at the window, so no refusal may be a block in this \
         session — if one were here unconditionally, the leg above would prove nothing: \
         {:?}",
        block_heads(&committed)
    );
    // (2a) …and the expansion is ALREADY gone, on a path where nothing withdrew
    // it. This is the measured fact behind the omission the leg above documents.
    assert!(
        committed
            .blocks()
            .iter()
            .all(|block| !block.text.contains(EXPANSION_MARKER)),
        "the accepted expansion survived the commit's budget re-assertion. If that is \
         now true, `the expansion is absent` has become a real discriminator and the \
         leg above should assert it — read its closing note before changing either \
         test: {:?}",
        block_heads(&committed)
    );

    fx.say(&session, "and what did that come back with?")
        .await
        .expect("and the session takes another turn");

    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        2,
        "non-vacuity: both turns must have reached the provider, or the assertions \
         below are about a prompt that was never assembled"
    );
    let assembled = String::from_utf8_lossy(&requests[1]).into_owned();

    // (1b) the leg above's load-bearing wire assertion discriminates.
    assert!(
        !assembled.contains(WINDOW_REFUSAL_HEAD),
        "nothing refused this turn at the window, so no refusal may reach the next \
         turn's prompt: {assembled}"
    );
    // (2b) and the same absence, one gate later, still on the serving path.
    assert!(
        !assembled.contains(EXPANSION_MARKER),
        "the expansion reached the next turn's prompt on the serving path. If that is \
         now true, the omission the leg above documents needs revisiting: {assembled}"
    );
}

// ---------------------------------------------------------------------------
// AC-23 — the memo, and the two boundaries it may not cross
// ---------------------------------------------------------------------------

/// **AC-23 / BR-14.2 — the next offer for the same pair leads with the rejection
/// this daemon watched, and BR-10's boundary holds on both sides of it.**
///
/// Two real invocations in one session, on one route, answered **differently**:
/// the first proceeds and is refused at the window; the second is offered again
/// and declines. That difference is what makes the negative half of this test
/// evidence rather than assertion — a record that pre-answered the question
/// would have sent the second turn on the strength of the first answer, and the
/// egress capture would show two requests instead of one.
///
/// | Claim | How it is read |
/// |---|---|
/// | the record reaches the sentence | the second offer's `sentence` opens with `OBSERVED_REJECTION_LEAD`, the first does not |
/// | …and leads it | the lead's index precedes the verdict clause's and the remedy's |
/// | …and the remedy still leads with it | BR-7a's risk clause and the with-remedy closing survive into the second offer |
/// | **it does not suppress the offer** | two offers, and the second carries the *same* option ids |
/// | **it does not pre-answer it** | the second answer is honoured: one request on the wire, and today's `-32023` refusal |
///
/// Deleting `ObservedWindowRejections::mark` leaves the second offer worded like
/// the first and reddens the lead assertions; widening the record into a
/// suppression or a consent reddens the two negatives.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_watched_rejection_leads_the_next_offer_without_suppressing_or_pre_answering_it() {
    let provider = MockProvider::start(vec![context_length_refusal()], served_turn());
    let fx = Fixture::new(
        &remote_route(&provider.openai_endpoint(), DECLARED_WINDOW),
        &oversized_for_a_declared_window(),
        Answer::Select(OPTION_ID_OVER_BUDGET_PROCEED_ONCE),
    );
    let session = fx.session();

    fx.invoke(&session)
        .await
        .expect_err("the provider refused the accepted turn at its window");

    // A *different* answer to the second question, so the outcome below is
    // evidence about which answer was acted on.
    fx.client
        .answers(Answer::Select(OPTION_ID_OVER_BUDGET_DECLINE));
    let declined = fx
        .invoke(&session)
        .await
        .expect_err("the second invocation was declined");

    // ── the record must NOT suppress the offer (BR-10) ───────────────────────
    let offers = fx.client.offers();
    assert_eq!(
        offers.len(),
        2,
        "a recorded observation may only make the next question better informed — it \
         may never answer it by not asking (D-1): {offers:#?}"
    );
    assert_eq!(
        option_ids(&offers[0]),
        option_ids(&offers[1]),
        "the second offer must carry every option the first did; a record that quietly \
         narrowed the list would be a consent wearing an observation's clothes"
    );

    // ── the record must NOT pre-answer it (BR-10) ────────────────────────────
    assert_eq!(
        provider.request_count(),
        1,
        "only the first, accepted turn may have reached the provider — a second request \
         here would mean the earlier `yes` was carried forward into a question the user \
         answered `no`"
    );
    assert_eq!(
        declined.code,
        error_code::SKILL_EXPANSION_TOO_LARGE,
        "declining is today's refusal, byte-identically (AC-3): {declined:?}"
    );
    assert!(
        declined.message.contains(NOTHING_WAS_SENT),
        "the decline keeps the clause that makes `-32023` different from `-32022`: {}",
        declined.message
    );

    // ── and it leads the sentence, with the remedy (BR-14.2) ─────────────────
    let first = subject_of(&offers[0]).2;
    let second = subject_of(&offers[1]).2;
    assert!(
        !first.contains(OBSERVED_REJECTION_LEAD),
        "the first offer had nothing to lead with — nothing had been observed yet: {first}"
    );
    let lead_at = second.find(OBSERVED_REJECTION_LEAD).unwrap_or_else(|| {
        panic!(
            "the second offer for this pair must name the rejection this daemon watched \
             happen: {second}"
        )
    });
    let verdict_at = second
        .find(EXCEEDS_WINDOW_CLAUSE)
        .expect("the offer still states its verdict");
    let remedy_at = second
        .find(RAISE_WINDOW_RISK)
        .expect("the offer still names its durable fix, whole");
    assert!(
        lead_at < verdict_at && lead_at < remedy_at,
        "a measured rejection outranks a prediction, so it *leads*: lead at {lead_at}, \
         verdict at {verdict_at}, remedy at {remedy_at} in: {second}"
    );
    assert!(
        second.contains(CLOSING_WITH_REMEDY),
        "and the remedy is still on offer after the lead, not displaced by it: {second}"
    );
}

// ---------------------------------------------------------------------------
// AC-24 — the circle, closed
// ---------------------------------------------------------------------------

/// **AC-24 / BR-9 / D-9 — the applied rebind removes the dead end rather than
/// explaining it. The single most important test in this REQ.**
///
/// The reported `/analyze` failure sat in a circle: an oversized skill on a
/// local-engine route, refused, with nothing the refusal named that the user
/// could take. This drives that route end to end and asserts the circle is
/// **gone** — not that a better sentence was printed about it.
///
/// 1. `/heavy` on a local-engine route raises the offer, whose remedy is BR-9's
///    `BindTierRemote`; the answer is `over_budget_remedy_only` — write the fix,
///    do not send this turn.
/// 2. Both halves of ADR-5's ordered pair land: `capabilities.max_context` for
///    `frontier` **and** the tier binding, verified on disk *and* by re-loading
///    the document into a fresh daemon (LESSON-519's double-check).
/// 3. An **identical** second invocation — the same skill, the same bytes —
///    reaches **no offer at all**, and the turn serves.
///
/// The third step is the one that matters. `offers().len() == 1` after two
/// invocations is only meaningful beside the proof that the second invocation
/// really ran and really reached the provider carrying the whole expansion,
/// which is why the egress capture is read for the marker rather than counted.
/// Reverse ADR-5's two writes and the tier ends up bound to a provider declaring
/// `max_context = 0`, which derives the same default pair under
/// `bound: unknown window` — the identical circle, one hop over — and this test
/// reddens on the offer count.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_applied_rebind_closes_the_circle_the_second_invocation_meets_no_offer() {
    let provider = MockProvider::start(Vec::new(), served_turn());
    let fx = Fixture::new(
        &local_route_with_one_remote(&provider.openai_endpoint()),
        &oversized_for_the_local_pair(),
        Answer::Select(OPTION_ID_OVER_BUDGET_REMEDY_ONLY),
    );
    let mut sub = fx.events.subscribe(512);
    let session = fx.session();

    // ── the circle, entered ──────────────────────────────────────────────────
    let refused = fx
        .invoke(&session)
        .await
        .expect_err("`remedy_only` writes the fix and refuses this turn");
    assert_eq!(
        refused.code,
        error_code::SKILL_EXPANSION_TOO_LARGE,
        "the remedy-only answer refuses the turn it was asked about: {refused:?}"
    );
    let (bound, _, _) = subject_of(
        fx.client
            .offers()
            .first()
            .expect("the local-engine route raised an offer"),
    );
    assert_eq!(
        bound,
        BudgetBound::LocalEngine,
        "this is the route the reported failure ran on; on any other bound the remedy \
         below is not BR-9's"
    );

    // ── the remedy, applied ──────────────────────────────────────────────────
    let applied = remedies(&drain(&mut sub));
    assert_eq!(applied.len(), 1, "one remedy, announced once: {applied:#?}");
    assert_eq!(
        applied[0].remedy_kind,
        RemedyKind::BindTierRemote,
        "the `LocalEngine` row's remedy is BR-9's pair: {:?}",
        applied[0]
    );
    assert_eq!(
        applied[0].provider_id.as_ref().map(|id| id.0.as_str()),
        Some("frontier"),
        "the write is addressed to the provider the tier is being bound *to*: {:?}",
        applied[0]
    );

    let document = fx.config_on_disk();
    assert!(
        document.contains(&format!("max_context = {RECIPE_WINDOW}")),
        "BR-9's first write — the window — must be on disk, or the second one bound a \
         tier to a provider declaring nothing: {document}"
    );

    // Re-parsed rather than only re-read: a document that reads right but does
    // not load right is not a fix (LESSON-519).
    let reloaded = fx.config_as_reloaded();
    let declared = reloaded
        .providers
        .iter()
        .find(|p| p.id.0 == "frontier")
        .expect("`frontier` survived the write");
    assert_eq!(
        declared.max_context,
        Some(RECIPE_WINDOW),
        "the reloaded document must declare the window the record announced: {declared:?}"
    );
    let rebound: Vec<Tier> = reloaded
        .tiers
        .iter()
        .filter(|t| t.provider_id.as_ref().map(|id| id.0.as_str()) == Some("frontier"))
        .map(|t| t.tier)
        .collect();
    assert!(
        !rebound.is_empty(),
        "BR-9's second write — the tier binding — must be on disk too; half the pair is \
         the circle, not the fix: {:?}",
        reloaded.tiers
    );

    // ── and the circle is gone ───────────────────────────────────────────────
    let served = fx
        .invoke(&session)
        .await
        .expect("the identical invocation now serves, because the route fits it");
    assert_eq!(
        served.stop_reason,
        StopReason::EndTurn,
        "non-vacuity: the second invocation must have produced a turn that ran to the \
         end, not one that stopped for a reason of its own: {served:?}"
    );
    assert_eq!(
        fx.client.offers().len(),
        1,
        "**AC-24.** After the remedy, an identical invocation must meet no offer at all: \
         the route now fits it, and a second offer here means the fix explained the dead \
         end instead of removing it: {:#?}",
        fx.client.offers()
    );
    let sent = String::from_utf8_lossy(
        provider
            .requests()
            .last()
            .expect("the fitting turn reached the provider"),
    )
    .into_owned();
    assert!(
        sent.contains(EXPANSION_MARKER),
        "non-vacuity: the second invocation reached the rebound provider carrying the \
         whole expansion — without this, `no offer` could mean `no turn`"
    );
}

// ---------------------------------------------------------------------------
// ASSUME-017 — one store, daemon-side, asserted structurally
// ---------------------------------------------------------------------------

/// **ASSUME-017 / ADR-9 — the observed-rejection record has no client half, and
/// that is a STRUCTURAL claim rather than a behavioural one.**
///
/// # Why this is not a behavioural test
///
/// TASK-246 corrected the draft on exactly this point. The record never crosses
/// the wire: what travels is the *sentence* composed from it
/// ([`a_watched_rejection_leads_the_next_offer_without_suppressing_or_pre_answering_it`]
/// pins that). So there is no client half to drive, and a behavioural test here
/// would assert that a thing which cannot happen did not happen — a vacuous pass
/// (LESSON-520). What *can* be asserted is the absence of the place a second
/// store would have to live.
///
/// `SessionGrants` is the CLI's one piece of session-scoped permission memory.
/// If a client ever memoized "this skill on this route was rejected", that is
/// where it would go — and ASSUME-017 records what the second store cost the
/// last time: a client-side memo of a daemon decision answered a question the
/// daemon had already forgotten, and the user never saw the prompt.
///
/// # Read as source text, because that is the only way from this crate
///
/// `tetond` does not depend on `teton`, and the fields are private in any case,
/// so no compile-time construction can express "this struct has no such field".
/// The struct's own field block is read and asserted over. If the anchor ever
/// moves, this fails loudly and asks to be re-pointed — it must not be deleted,
/// because deleting it is what makes the second store silent.
#[test]
fn the_observed_rejection_record_has_no_client_half() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../teton/src/session_ui.rs");
    let source = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "the CLI's session state could not be read at {}: {err}. This test is the only \
             guard on ASSUME-017's one-store rule — re-point it, do not delete it.",
            path.display()
        )
    });

    let start = source
        .find("pub struct SessionGrants {")
        .unwrap_or_else(|| {
            panic!(
                "`SessionGrants` is no longer declared in {} — the anchor moved. Re-point this \
             test at wherever the CLI now keeps session-scoped permission memory; the rule \
             it guards did not move.",
                path.display()
            )
        });
    let body = &source[start..];
    let end = body
        .find("\n}")
        .expect("the struct declaration is closed by a brace at column zero");
    let fields = &body[..end];

    assert!(
        !fields.contains("SkillOverBudget"),
        "the CLI grew a `SkillOverBudget` field on `SessionGrants`. ADR-9 puts the \
         observed-rejection record in ONE store, daemon-side: a client-side copy outlives \
         the daemon's own memory and replays a stale record into a later session, which is \
         the harm ASSUME-017 was written for. Found:\n{fields}"
    );
    assert!(
        !fields.to_lowercase().contains("over_budget")
            && !fields.to_lowercase().contains("rejection"),
        "the CLI grew session-scoped memory of an over-budget answer or a window \
         rejection. BR-10: nothing about an over-budget answer is remembered anywhere, \
         and the observation is remembered only daemon-side. Found:\n{fields}"
    );
    // Non-vacuity: an empty or mis-sliced field block would satisfy every
    // absence above for the wrong reason.
    assert!(
        fields.contains("allow_always") && fields.contains("reject_always"),
        "the field block was mis-sliced — the two grant sets `SessionGrants` is made of \
         are not in it, so the absences above are about nothing:\n{fields}"
    );
}

// ---------------------------------------------------------------------------
// REQ-591 D-1 — the remedy is a machine-wide commitment
// ---------------------------------------------------------------------------

/// A [`CommitmentAttestation`] that answers as instructed and records who it was
/// asked about (REQ-591 D-1).
struct StandingAttestation {
    refusal: Option<&'static str>,
    asked: Mutex<Vec<ConnectionId>>,
}

impl StandingAttestation {
    fn new(refusal: Option<&'static str>) -> Arc<Self> {
        Arc::new(Self {
            refusal,
            asked: Mutex::new(Vec::new()),
        })
    }

    fn asked(&self) -> Vec<ConnectionId> {
        self.asked.lock().expect("attestation mutex").clone()
    }
}

impl CommitmentAttestation for StandingAttestation {
    fn attest_daemon_wide_commitment(&self, addressee: ConnectionId) -> Result<(), String> {
        self.asked
            .lock()
            .expect("attestation mutex")
            .push(addressee);
        match self.refusal {
            Some(why) => Err(why.to_owned()),
            None => Ok(()),
        }
    }
}

/// **D-1 — the remedy writes `config.toml`, so it needs what `config/set` needs.**
///
/// ADR-4 routes every remedy through [`DaemonRuntime::apply_config_update`],
/// `config/set`'s own body, and says it inherits that method's posture
/// "verbatim". It inherited the body and not the two gates around it:
/// `refuse_daemon_wide` (REQ-570 BR-10(a)) and `refuse_unattested_commitment`
/// (BR-10(b)) live in `server.rs::handle_config_set`, and the remedy reaches the
/// body from the other side — through a `permission/respond` frame that
/// `handle_permission_respond` never presence-checks.
/// `config_set_attestation.rs` pinned that gap as a fact. This closes it, on the
/// same seam that closes it for the acknowledgment's own durable row, because
/// the two are one question and answering them differently is how a gate comes
/// to mean two things.
///
/// **The pairing is the test** (LESSON-520): the same fixture, the same route,
/// the same oversized body, the same `remedy_only` answer. Only what the seam
/// says changes. So a build that stopped consulting the seam fails the refused
/// leg, and one that never wrote fails the attested leg.
///
/// The durable claim is read **off the file and re-parsed** on both legs
/// (LESSON-519, BR-9), because a refusal that left a half-written document would
/// satisfy a "the write returned an error" assertion and be a worse outcome than
/// the gap.
///
/// **What a refusal deliberately does not do** is change the turn. The human
/// answering the offer decided whether to send *this* expansion; only the
/// going-forward fix is a fact about the machine. Both legs therefore refuse the
/// turn identically — `remedy_only` means "do not send this one" — and the
/// difference between them is entirely on disk.
///
/// **Mutation:** delete the `commitment_attestation` block from
/// `apply_over_budget_remedy` and the refused leg goes red on the window it did
/// not expect.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_remedy_is_written_only_where_a_verified_human_stands_behind_it() {
    for (refusal, expect_window, what) in [
        (None, true, "a verified human"),
        (
            Some("no human answered the sensor"),
            false,
            "a mechanism that was not satisfied",
        ),
    ] {
        let provider = MockProvider::start(Vec::new(), served_turn());
        let attestation = StandingAttestation::new(refusal);
        let fx = Fixture::attesting(
            &local_route_with_one_remote(&provider.openai_endpoint()),
            &oversized_for_the_local_pair(),
            Answer::Select(OPTION_ID_OVER_BUDGET_REMEDY_ONLY),
            Arc::clone(&attestation),
        );
        let mut sub = fx.events.subscribe(512);
        let session = fx.session();

        let refused = fx
            .invoke(&session)
            .await
            .expect_err("`remedy_only` refuses the turn it was asked about");
        assert_eq!(
            refused.code,
            error_code::SKILL_EXPANSION_TOO_LARGE,
            "{what}: the turn's own answer is the human's and no presence check \
             touches it: {refused:?}"
        );

        assert_eq!(
            attestation.asked(),
            vec![fx.connection],
            "{what}: the subject is the connection that answered the offer"
        );

        // Read off the file, then through the production loader — the two halves
        // of LESSON-519's check, on the leg that wrote and on the leg that did
        // not.
        let document = fx.config_on_disk();
        assert_eq!(
            document.contains(&format!("max_context = {RECIPE_WINDOW}")),
            expect_window,
            "{what}: the remedy's durable half is a machine-wide commitment, and \
             this is the document it did or did not reach:\n{document}"
        );
        let declared = fx
            .config_as_reloaded()
            .providers
            .iter()
            .find(|p| p.id.0 == "frontier")
            .expect(
                "`frontier` survives either way — a refusal writes nothing, it \
                     does not corrupt",
            )
            .max_context;
        assert_eq!(
            declared == Some(RECIPE_WINDOW),
            expect_window,
            "{what}: and the production loader agrees with the bytes: {declared:?}"
        );

        // The announcement follows the write, not the answer: a
        // `skill_over_budget_remedy_applied` on the refused leg would be BR-10's
        // own defect — a surface claiming something that did not happen.
        let applied = remedies(&drain(&mut sub));
        assert_eq!(
            applied.len(),
            usize::from(expect_window),
            "{what}: the event announces the write and must not outlive it: {applied:#?}"
        );
    }
}
