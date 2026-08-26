//! **REQ-589 acceptance: the over-budget offer, end to end** (TASK-253).
//!
//! Every test here drives a **real prompt turn** through
//! `DaemonRuntime::run_prompt_turn` — the same entry point `session/prompt`
//! drives — over a real config file on disk, a real router derivation, a real
//! permission gate answered by a client that selects by **option id**, and a
//! real socket for the provider. Nothing below is built from a struct literal.
//!
//! That is not a style preference. REQ-585 and REQ-587 each shipped Critical
//! defects past a green ~3,500-test suite because a new wire fact was pinned by
//! a hand-built value, which leaves the *producer* unguarded (LESSON-544,
//! LESSON-552). The rule this file keeps is: mutate a producer line and a named
//! test below must redden.
//!
//! ## AC → test map
//!
//! | AC | Test |
//! |---|---|
//! | AC-1 (the boundary, offered and answered) | [`the_reported_analyze_measurement_serves_on_both_halves_of_the_local_pair`] |
//! | **REQ-590 AC-12** (the reported measurement, served) | (same test, leg 4) |
//! | AC-1 (accepting carries it whole) | [`an_accepted_offer_puts_every_measured_byte_of_the_expansion_on_the_wire`] |
//! | AC-3 (decline is today's refusal) | [`declining_is_todays_refusal_in_every_byte`] |
//! | AC-4 (silence is never consent) | [`no_unanswered_offer_resolves_to_proceed`] |
//! | AC-6 (three verdicts, three sentences) | [`each_reachable_window_verdict_is_offered_and_pins_its_own_sentence`] |
//! | AC-7 (BR-7's remedy table) | [`every_bound_offers_exactly_the_remedy_the_table_names`] |
//! | AC-7a (the risk sentence) | [`a_raise_window_offer_cannot_be_rendered_without_its_risk`] |
//! | AC-7b (four combinations) | [`proceed_and_remedy_are_answered_independently_in_all_four_combinations`] |
//! | AC-9 (trust asked first, decline wins) | [`a_project_skills_trust_question_is_put_before_its_budget_question`] |
//! | AC-9 (a user skill raises no trust question) | [`a_user_authored_skill_is_asked_only_the_budget_question`] |
//! | AC-10 (no grant survives) | [`accepting_twice_asks_twice_and_no_grant_survives_the_invocation`] |
//! | AC-11 (no "no provider saw this turn") | [`the_accepted_path_never_says_no_provider_saw_this_turn`] |
//! | AC-18 (BR-11 on every not-sent path) | [`every_not_sent_path_reaches_no_provider_and_spends_nothing`] |
//!
//! The Phase-5 review pass added two tests and widened one, none of which an AC
//! names because they close defects rather than criteria:
//!
//! | Defect | Test |
//! |---|---|
//! | `RaiseCap` deleted a spend ceiling that cleared nothing (ADR-6 rule 2) | [`a_cap_is_only_offered_for_clearing_where_clearing_it_would_help`] |
//! | the remedy reverted a provider changed while the offer waited | [`a_provider_changed_while_the_offer_waits_is_not_reverted_by_the_remedy`] |
//! | the closing question offered options the prompt did not draw (ADR-1) | [`a_cap_is_only_offered_for_clearing_where_clearing_it_would_help`], [`a_raise_window_offer_cannot_be_rendered_without_its_risk`] |
//!
//! AC-5 (the model path is never offered a choice) is in `skill_tool_loop.rs`,
//! beside the model-invoked refusal it extends; the choke-point half of BR-11 is
//! in `egress_capture.rs`.
//!
//! ## Mutation table
//!
//! | Mutation | Test that fails |
//! |---|---|
//! | Stage A refuses instead of asking | every test here |
//! | the `skill_over_budget_offered` publish is dropped | [`every_not_sent_path_reaches_no_provider_and_spends_nothing`], [`each_reachable_window_verdict_is_offered_and_pins_its_own_sentence`] |
//! | the `skill_over_budget_accepted` publish is dropped | [`the_accepted_path_never_says_no_provider_saw_this_turn`] |
//! | the `skill_over_budget_remedy_applied` publish is dropped | [`proceed_and_remedy_are_answered_independently_in_all_four_combinations`] |
//! | `interpret_over_budget`'s `_ =>` fallback maps to `Allowed` | [`no_unanswered_offer_resolves_to_proceed`] |
//! | the `invoker: None` arm maps to proceed | [`no_unanswered_offer_resolves_to_proceed`] |
//! | `inputs.window` swapped for `inputs.cap` | [`each_reachable_window_verdict_is_offered_and_pins_its_own_sentence`] (the `UserCap` + `FitsWindow` cell turns `ExceedsWindow`) |
//! | `Remedy::for_bound`'s `RedactScan` arm gains a remedy | [`every_bound_offers_exactly_the_remedy_the_table_names`] |
//! | the accepted tail borrows the refusal's consequence clause | [`the_accepted_path_never_says_no_provider_saw_this_turn`] |
//! | the offer consults a grant | [`accepting_twice_asks_twice_and_no_grant_survives_the_invocation`] |
//! | the naming duty moves above Stage A | [`every_not_sent_path_reaches_no_provider_and_spends_nothing`] |
//! | the project-trust gate moves below Stage A | [`a_project_skills_trust_question_is_put_before_its_budget_question`] |
//! | the trust gate stops asking about `source` and asks for every skill | [`a_user_authored_skill_is_asked_only_the_budget_question`] |
//! | `apply_remedy` folded into the consent decision | [`proceed_and_remedy_are_answered_independently_in_all_four_combinations`] |
//! | the `clearing_the_cap_clears` gate dropped from `plan_over_budget_remedy` | [`a_cap_is_only_offered_for_clearing_where_clearing_it_would_help`] |
//! | the closing question read back off `Remedy::is_offered` | [`a_cap_is_only_offered_for_clearing_where_clearing_it_would_help`], [`a_raise_window_offer_cannot_be_rendered_without_its_risk`] |
//! | the `provider_identity_unchanged` guard dropped from the apply | [`a_provider_changed_while_the_offer_waits_is_not_reverted_by_the_remedy`] |
//!
//! ## Only reachable cells (LESSON-520)
//!
//! Verdict and bound are not independent axes; seven of the fifteen cells cannot
//! occur and a test for one would pass vacuously. Of the eight that do, this
//! file exercises seven — against the normative table in `architecture.md`:
//!
//! | Bound | Verdict | Where |
//! |---|---|---|
//! | `LocalEngine` | `WindowUnknown` | AC-1, AC-6, AC-7 |
//! | `DefaultUnknown` | `WindowUnknown` | AC-1's wire leg, AC-6, AC-7, AC-18 |
//! | `Window` | `FitsWindow` | AC-6, AC-7, AC-7a |
//! | `Window` | `ExceedsWindow` | AC-6 |
//! | `UserCap` | `FitsWindow` | AC-6, AC-7, AC-7b |
//! | `UserCap` | `ExceedsWindow` | AC-6 |
//! | `RedactScan` | `FitsWindow` | AC-6, AC-7 |
//!
//! `RedactScan` + `ExceedsWindow` is reachable and is deliberately not covered:
//! it says nothing about BR-7b that the `FitsWindow` cell does not, and the
//! `RedactScan` fixture is the expensive one (see [`redact_scan_route`]).
//!
//! ## What this file cannot prove
//!
//! * **`OverBudgetOffer::accepted_record` has no wire surface.** TASK-247 wrote
//!   it to the daemon's stderr record channel and flagged the gap. AC-11 is
//!   therefore asserted over everything the accepted turn *did* surface — the
//!   RPC result and every published event — paired with the declined leg on the
//!   same fixture, where the clause does appear. A reader who gives that record
//!   a surface should extend the assertion to it.
//! * **The local tier's engine is a script, not llama.cpp.** "The expansion was
//!   dispatched" is a claim about the turn reaching the engine and completing;
//!   whether a real 16k-window engine then serves 4,097 words is AC-15's
//!   question, and AC-15 is a runbook for a person. That is the whole reason
//!   AC-1 stops at *dispatches* — the criterion says so in as many words.
//!   REQ-590 AC-12 inherits the same limit: it asserts that this daemon no
//!   longer refuses the measurement, not that llama.cpp then serves it. REQ-590
//!   AC-14 is where a person checks the second half.
//!
//! ## AC-15 — the dogfood runbook
//!
//! Authored as part of TASK-253 and kept with the task, in
//! `.adlc/specs/REQ-589-over-budget-skill-expansion-offer/tasks/TASK-253-offer-integration-suite.md`,
//! in the form `docs/manual-verification.md` takes. It is executed at wrapup on
//! a real machine, and it is the first data point REQ-590 needs.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use teton_protocol::events::{
    BudgetBound, Event, PermissionOption, PermissionOptionKind, PermissionRequest,
    PermissionSubject, RemedyKind, WindowVerdict, OPTION_ID_OVER_BUDGET_DECLINE,
    OPTION_ID_OVER_BUDGET_PROCEED_AND_REMEDY, OPTION_ID_OVER_BUDGET_PROCEED_ONCE,
    OPTION_ID_OVER_BUDGET_REMEDY_ONLY,
};
use teton_protocol::jsonrpc::error_code;
use teton_protocol::methods::{PermissionOutcome, SkillInvocation};
use teton_protocol::{SessionId, SessionMode};

use tetond::broadcast::{EventBus, Subscription};
use tetond::grants::{ConnectionId, GrantRegistry};
use tetond::harness::permissions::AddressedPermissionDelivery;
use tetond::harness::PendingPermissions;
use tetond::runtime::{ClientPresence, DaemonRuntime};
use tetond::sessions::SessionRegistry;
use tetond::skills::RealFs;

#[path = "e2e/harness.rs"]
mod daemon_harness;

use daemon_harness::{openai_turn, MockProvider, MockResponse};

// ---------------------------------------------------------------------------
// the sentences under test, quoted once
// ---------------------------------------------------------------------------

/// BR-3's `ExceedsWindow` clause, verbatim from `harness::budget`.
const EXCEEDS_WINDOW_CLAUSE: &str =
    "This will blow the context window this route declares: proceeding without raising it will \
     very likely be rejected by the provider.";

/// BR-3's `FitsWindow` clause on a **window- or cap-bound** route (ADR-15): the
/// band it went past is the generation reservation, so the sentence promises
/// nothing about the reply.
const FITS_WINDOW_INTO_THE_RESERVATION: &str =
    "The prompt fits the context window this route declares, but the budget it went past is the \
     room held back for the reply — so it may leave the response very little to work with.";

/// BR-3's `FitsWindow` clause where the bound is neither (ADR-17 — `RedactScan`
/// alone reaches it, and the reservation sentence would be false there).
const FITS_WINDOW_CLAUSE: &str =
    "The prompt fits the context window this route declares; it is this daemon's own budget that \
     refused it.";

/// BR-3's `WindowUnknown` clause.
const WINDOW_UNKNOWN_CLAUSE: &str =
    "This route declares no context window, so this daemon cannot promise the send will fit; if \
     it does not, the turn ends with a context-length error rather than quietly losing anything.";

/// BR-7b's sentence, whose whole job is to not imply a fix exists.
const NO_REMEDY_CLAUSE: &str =
    "There is no durable fix to offer here: the byte ceiling that refused this is what bounds the \
     egress redaction scan, and raising it to fit one skill would trade a privacy guarantee for a \
     convenience. This choice is about this one turn and nothing else.";

/// BR-7a's risk sentence, which a `RaiseWindow` offer may not shed.
const RAISE_WINDOW_RISK: &str =
    "raising a declared window above the provider's real one does not enlarge that window, it \
     makes this daemon send requests the provider will reject, turning a refusal here into an \
     error there";

/// The clause that makes `-32023` different from `-32022`, and the one an
/// accepted turn may never carry (BR-5, AC-11).
const NOTHING_WAS_SENT: &str =
    "Nothing was sent and no provider saw this turn — a skill expansion is carried whole or \
     refused, never shortened into something you did not invoke.";

/// The offer's closing question where BR-7 grants the bound a remedy.
const CLOSING_WITH_REMEDY: &str =
    "Send it whole this once, take the durable fix, both, or neither?";

/// The closing question where it does not (BR-7b).
const CLOSING_ONE_TIME_ONLY: &str = "Send it whole this once, or refuse the turn?";

/// Every clause BR-3's arms can produce, so a leg that pins one can assert the
/// absence of the rest rather than only the presence of its own.
const EVERY_VERDICT_CLAUSE: [&str; 4] = [
    EXCEEDS_WINDOW_CLAUSE,
    FITS_WINDOW_INTO_THE_RESERVATION,
    FITS_WINDOW_CLAUSE,
    WINDOW_UNKNOWN_CLAUSE,
];

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

/// A throwaway tree, removed on drop, holding one fixture's config, its skill
/// and its fixture `HOME`.
struct Tree {
    root: PathBuf,
}

impl Tree {
    /// A fresh tree under `/tmp`, with a **fixed-width** name.
    ///
    /// The width is load-bearing rather than tidy: the root path reaches the
    /// system prompt, the system prompt is an input to every figure Stage A
    /// measures, and so two trees whose names differ in length do not share an
    /// overhead constant. AC-1 calibrates inside one tree for the same reason.
    fn new(tag: &str) -> Self {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        let root =
            PathBuf::from("/tmp").join(format!("ob{seq:02x}{:04x}", std::process::id() & 0xffff));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap_or_else(|e| panic!("{tag}: {e}"));
        // A project marker, so the root probes as `project` and the project half
        // of skill discovery is reached at all.
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

/// What the scripted local engine answers a turn with.
const SCRIPTED_REPLY: &str = "SCRIPTED-LOCAL-TURN-REPLY";

/// The process-wide seams every fixture in this binary runs under, installed
/// once before any runtime exists.
///
/// `TETON_LOCAL_SCRIPT` is what gives these daemons a **local tier at all**, and
/// with it the one route the reported failure ran on. A scripted engine is
/// exempt from the first-run consent flow (it fetches nothing), so no fixture
/// here has to answer a model proposal before it can reach a turn.
fn seams() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        assert!(
            std::env::var_os("TETON_CONFIG").is_none(),
            "TETON_CONFIG in the environment would override every fixture config in this file"
        );
        let base = PathBuf::from("/tmp").join(format!("obseam{:x}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let script = base.join("local_script.txt");
        std::fs::write(&script, format!("{SCRIPTED_REPLY}\n")).unwrap();
        std::env::set_var("TETON_TEST_SEAMS", "1");
        std::env::set_var("TETON_LOCAL_SCRIPT", &script);
        std::env::set_var("TETON_PROBE_RAM_BYTES", (16u64 << 30).to_string());
        std::env::set_var("TETON_PROBE_DISK_BYTES", "500000000000");
        std::env::set_var("TETON_PROBE_GPU", "apple-silicon");
    });
}

/// How this fixture's client answers the **project-skill trust** acknowledgment
/// REQ-589 ADR-10 raises above Stage A.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trust {
    Acknowledge,
    Decline,
}

/// How it answers the **offer**.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Answer {
    /// Select this option id.
    ///
    /// By **id**, never by [`PermissionOptionKind`]: two of the four ids share
    /// a kind, and telling `proceed_once` from `proceed_and_remedy` is the whole
    /// of AC-7b.
    Select(&'static str),
    /// Answer with an id the prompt never carried — `interpret_over_budget`'s
    /// `_ =>` arm, which BR-4 requires to fall to "not sent".
    Unrecognized,
    /// Close the prompt without choosing.
    Cancel,
}

/// The client every turn here comes from: it records every request it was shown
/// and answers the two questions a skill turn can raise.
struct Client {
    pending: Arc<PendingPermissions>,
    answer: Mutex<Answer>,
    trust: Trust,
    asked: Mutex<Vec<PermissionRequest>>,
    /// Something to do **while the offer is on screen and before it is
    /// answered**.
    ///
    /// The whole of the gap this file could not otherwise reach: the plan is
    /// built before the question is put and applied after the answer, with no
    /// timeout in between, so everything a user can do to their config in those
    /// minutes happens here.
    #[allow(clippy::type_complexity)]
    while_offered: Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl Client {
    fn new(pending: &Arc<PendingPermissions>, answer: Answer, trust: Trust) -> Arc<Self> {
        Arc::new(Self {
            pending: Arc::clone(pending),
            answer: Mutex::new(answer),
            trust,
            asked: Mutex::new(Vec::new()),
            while_offered: Mutex::new(None),
        })
    }

    /// Install the mid-flight action described on [`Client::while_offered`].
    fn while_offered(&self, action: impl Fn() + Send + 'static) {
        *self.while_offered.lock().expect("while_offered mutex") = Some(Box::new(action));
    }

    /// **Every** question this client was shown, in the order it was shown
    /// them — the unfiltered log.
    ///
    /// [`Client::offers`] below is the filtered view, and a filtered view can
    /// say nothing about BR-6: `deliver` dispatches on the request's *type*, so
    /// it answers the acknowledgment and the offer correctly whichever arrives
    /// first, and every assertion written through `offers()` survives an
    /// implementation that puts the budget question first. The order is a fact
    /// only this reader holds.
    fn asked(&self) -> Vec<PermissionRequest> {
        self.asked.lock().expect("asked mutex").clone()
    }

    /// Only the over-budget offers — the acknowledgment is a different question.
    fn offers(&self) -> Vec<PermissionRequest> {
        self.asked
            .lock()
            .expect("asked mutex")
            .iter()
            .filter(|r| matches!(r.subject, Some(PermissionSubject::SkillOverBudget { .. })))
            .cloned()
            .collect()
    }

    /// The one offer this fixture raised — counted, never assumed.
    fn sole_offer(&self) -> PermissionRequest {
        let offers = self.offers();
        assert_eq!(
            offers.len(),
            1,
            "expected exactly one over-budget offer, saw {}",
            offers.len()
        );
        offers.into_iter().next().expect("one offer")
    }

    fn answers(&self, answer: Answer) {
        *self.answer.lock().expect("answer mutex") = answer;
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
            .expect("asked mutex")
            .push(request.clone());
        let is_offer = matches!(
            request.subject,
            Some(PermissionSubject::SkillOverBudget { .. })
        );
        let outcome = if is_offer {
            // Before the answer, never after: what this simulates is the user
            // reaching for `config/set` while the question sits unanswered.
            if let Some(action) = self
                .while_offered
                .lock()
                .expect("while_offered mutex")
                .as_ref()
            {
                action();
            }
            match self.answer.lock().expect("answer mutex").clone() {
                Answer::Select(id) => {
                    // Asserted rather than defaulted: an id the prompt did not
                    // carry would mean the gate narrowed the option list, and
                    // quietly answering something else would hide that.
                    assert!(
                        request.options.iter().any(|o| o.option_id == id),
                        "the offer did not carry `{id}`: {:?}",
                        request.options
                    );
                    PermissionOutcome::Selected {
                        option_id: id.to_owned(),
                    }
                }
                Answer::Unrecognized => PermissionOutcome::Selected {
                    option_id: "over_budget_definitely_not_an_option".to_owned(),
                },
                Answer::Cancel => PermissionOutcome::Cancelled,
            }
        } else {
            let want = match self.trust {
                Trust::Acknowledge => PermissionOptionKind::AllowOnce,
                Trust::Decline => PermissionOptionKind::RejectOnce,
            };
            let option = request
                .options
                .iter()
                .find(|o| o.kind == want)
                .unwrap_or_else(|| panic!("the acknowledgment offers {want:?}: {request:?}"));
            PermissionOutcome::Selected {
                option_id: option.option_id.clone(),
            }
        };
        self.pending
            .resolve_from(&request.request_id, outcome, connection)
    }
}

/// Everything a fixture varies, so a leg reads as a table row rather than as a
/// seven-argument constructor.
struct Spec {
    tag: &'static str,
    config: String,
    body: String,
    arguments: String,
    answer: Answer,
    trust: Trust,
    /// A **project**-sourced skill is the shape the reported failure had, and
    /// the one ADR-10's acknowledgment sits above. A **user**-sourced one is
    /// what AC-3 needs: today's refusal carries no source marker, so only a
    /// user skill's offer and refusal share a byte-identical head.
    project_sourced: bool,
    /// Whether the daemon has a route to deliver an addressed request on. False
    /// is `SkillConsent::Unanswerable` — a real posture, not a broken fixture.
    addressed: bool,
    /// Whether the turn carries a connection. `None` is "nobody to ask".
    connected: bool,
}

impl Spec {
    fn new(tag: &'static str, config: String, body: String) -> Self {
        Self {
            tag,
            config,
            body,
            arguments: String::new(),
            answer: Answer::Select(OPTION_ID_OVER_BUDGET_DECLINE),
            trust: Trust::Acknowledge,
            project_sourced: true,
            addressed: true,
            connected: true,
        }
    }

    fn answering(mut self, answer: Answer) -> Self {
        self.answer = answer;
        self
    }

    fn with_arguments(mut self, arguments: String) -> Self {
        self.arguments = arguments;
        self
    }

    fn user_sourced(mut self) -> Self {
        self.project_sourced = false;
        self
    }

    fn declining_trust(mut self) -> Self {
        self.trust = Trust::Decline;
        self
    }

    fn unanswerable(mut self) -> Self {
        self.addressed = false;
        self
    }

    fn unconnected(mut self) -> Self {
        self.connected = false;
        self
    }
}

/// The dispatchable name every fixture registers its oversized skill under.
const SKILL: &str = "heavy";

/// One daemon over one config file, with one oversized skill and one client.
struct Fixture {
    runtime: Arc<DaemonRuntime>,
    events: Arc<EventBus>,
    sessions: SessionRegistry,
    client: Arc<Client>,
    connection: Option<ConnectionId>,
    arguments: String,
    /// Where a **user**-sourced fixture skill lives, and the home every
    /// discovery in this file runs against — the fixture's, never the runner's.
    home: PathBuf,
    project_sourced: bool,
    tree: Tree,
}

impl Fixture {
    fn new(spec: Spec) -> Self {
        seams();
        let tree = Tree::new(spec.tag);
        tree.write("config.toml", &spec.config);
        let home = tree.path().join("home");
        std::fs::create_dir_all(home.join(".claude/skills")).unwrap();

        let events = Arc::new(EventBus::new());
        let runtime = Arc::new(
            DaemonRuntime::from_env(tree.path(), &events).expect("the fixture daemon starts"),
        );
        let client = Client::new(runtime.pending(), spec.answer, spec.trust);
        if spec.addressed {
            runtime.install_addressed_delivery(
                Arc::clone(&client) as Arc<dyn AddressedPermissionDelivery>
            );
        }

        let fixture = Self {
            runtime,
            events,
            sessions: SessionRegistry::new(),
            client,
            connection: spec
                .connected
                .then(|| GrantRegistry::new().next_connection_id()),
            arguments: spec.arguments,
            home,
            project_sourced: spec.project_sourced,
            tree,
        };
        fixture.write_body(&spec.body);
        fixture
    }

    fn skill_dir(&self) -> PathBuf {
        let base = if self.project_sourced {
            self.tree.path().join(".claude/skills")
        } else {
            self.home.join(".claude/skills")
        };
        base.join(SKILL)
    }

    /// Write (or rewrite) the skill's body in place.
    ///
    /// AC-1 calibrates and then re-measures **inside one tree**, because the
    /// tree's path length reaches the system prompt and therefore the figures.
    fn write_body(&self, body: &str) {
        let dir = self.skill_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\ndescription: the oversized skill\n---\n\n{body}\n"),
        )
        .unwrap();
    }

    fn session(&self) -> SessionId {
        let id = self
            .sessions
            .create(SessionMode::Freeform, None, Some(self.tree.root.clone()))
            .expect("a freeform session")
            .session_id;
        let probed = self.runtime.session_root_for(Some(self.tree.path()));
        // The home is the **fixture's**, so the four discovery globs cover this
        // tree only — nothing here depends on whatever `~/.claude/skills` the
        // runner happens to have.
        self.sessions.set_skills(
            &id,
            tetond::skills::discover(Some(&self.home), &probed.path, probed.view.kind, &RealFs),
        );
        id
    }

    async fn invoke(
        &self,
        session: &SessionId,
    ) -> Result<teton_protocol::methods::PromptTurnResult, teton_protocol::jsonrpc::RpcError> {
        self.runtime
            .run_prompt_turn(
                &self.events,
                &self.sessions,
                session.clone(),
                SessionMode::Freeform,
                None,
                Some(self.tree.root.clone()),
                String::new(),
                Some(SkillInvocation {
                    name: SKILL.to_owned(),
                    raw_arguments: self.arguments.clone(),
                }),
                self.connection,
                ClientPresence::unwatched(),
            )
            .await
    }

    /// The config document as it stands **on disk**, which is where AC-7b's
    /// durable half is read from (LESSON-519: inspect the artifact, never a
    /// return code).
    fn config_on_disk(&self) -> String {
        std::fs::read_to_string(self.tree.path().join("config.toml")).expect("the config file")
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

/// One offer's subject, destructured — every test reads the wire fact, not a
/// value the test built.
struct Offer {
    bound: BudgetBound,
    verdict: WindowVerdict,
    measured_tokens: u64,
    measured_bytes: u64,
    budget_tokens: u64,
    budget_bytes: u64,
    sentence: String,
}

fn subject_of(request: &PermissionRequest) -> Offer {
    match request.subject.clone() {
        Some(PermissionSubject::SkillOverBudget {
            bound,
            window_verdict,
            measured_tokens,
            measured_bytes,
            budget_tokens,
            budget_bytes,
            sentence,
            ..
        }) => Offer {
            bound,
            verdict: window_verdict,
            measured_tokens,
            measured_bytes,
            budget_tokens,
            budget_bytes,
            sentence,
        },
        other => panic!("not an over-budget offer: {other:?}"),
    }
}

/// Which of a skill turn's questions a request *is*, as a word an assertion can
/// print — so a wrong order fails saying what the order actually was rather than
/// `false != true`.
fn question(request: &PermissionRequest) -> &'static str {
    match request.subject {
        Some(PermissionSubject::ProjectSkillTrust { .. }) => "project trust",
        Some(PermissionSubject::SkillOverBudget { .. }) => "over-budget offer",
        Some(PermissionSubject::SkillDynamicContext { .. }) => "dynamic context",
        _ => "some other question",
    }
}

/// The questions this client was shown, named and in order.
fn questions(client: &Client) -> Vec<&'static str> {
    client.asked().iter().map(question).collect()
}

fn option_ids(request: &PermissionRequest) -> Vec<String> {
    request
        .options
        .iter()
        .map(|o| o.option_id.clone())
        .collect()
}

fn offered(published: &[Event]) -> Vec<teton_protocol::events::SkillOverBudgetOffered> {
    published
        .iter()
        .filter_map(|e| match e {
            Event::SkillOverBudgetOffered(o) => Some(o.clone()),
            _ => None,
        })
        .collect()
}

fn accepted(published: &[Event]) -> Vec<teton_protocol::events::SkillOverBudgetAccepted> {
    published
        .iter()
        .filter_map(|e| match e {
            Event::SkillOverBudgetAccepted(a) => Some(a.clone()),
            _ => None,
        })
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

// ---------------------------------------------------------------------------
// route documents — one per reachable (bound, verdict) cell
// ---------------------------------------------------------------------------

/// A model the shipped vendor catalog recognizes, so BR-7c has a window to
/// propose: `kimi-k3` carries `max_context = 1_000_000`, verified 2026-08-19.
const RECIPE_MODEL: &str = "kimi-k3";

/// A model no shipped recipe matches, so BR-7c proposes nothing.
const UNRECOGNIZED_MODEL: &str = "claude-opus-4";

/// Every tier on the local engine: the route the reported `/analyze` failure ran
/// on. `bound: local engine`, and the reachability table gives it no verdict but
/// `WindowUnknown`.
fn local_route() -> String {
    let mut cfg = String::from("[[providers]]\nid = \"local\"\nkind = \"local\"\n\n");
    for tier in ["reflex", "scan", "build", "think"] {
        cfg.push_str(&format!(
            "[[tiers]]\ntier = \"{tier}\"\nprovider_id = \"local\"\n\n"
        ));
    }
    cfg
}

/// [`local_route`] with **exactly one** remote provider registered but unbound —
/// ADR-12's "propose by name" case, and the only shape in which the
/// `BindTierRemote` remedy carries options rather than being withheld.
fn local_route_with_one_remote(endpoint: &str) -> String {
    let mut cfg = String::from("[[providers]]\nid = \"local\"\nkind = \"local\"\n\n");
    cfg.push_str(&remote_provider(
        "frontier",
        endpoint,
        RECIPE_MODEL,
        None,
        None,
    ));
    for tier in ["reflex", "scan", "build", "think"] {
        cfg.push_str(&format!(
            "[[tiers]]\ntier = \"{tier}\"\nprovider_id = \"local\"\n\n"
        ));
    }
    cfg
}

fn remote_provider(
    id: &str,
    endpoint: &str,
    model: &str,
    max_context: Option<u32>,
    cap: Option<u32>,
) -> String {
    let mut cfg = format!(
        "[[providers]]\nid = \"{id}\"\nkind = \"openai-compatible\"\nendpoint = \"{endpoint}\"\n\
         model = \"{model}\"\n\n"
    );
    if max_context.is_some() || cap.is_some() {
        cfg.push_str("[providers.capabilities]\n");
        if let Some(w) = max_context {
            cfg.push_str(&format!("max_context = {w}\n"));
        }
        if let Some(c) = cap {
            cfg.push_str(&format!("context_budget_cap = {c}\n"));
        }
        cfg.push('\n');
    }
    cfg
}

/// A route whose turn-serving tiers are remote and whose `reflex` tier is not.
///
/// `reflex` stays local on purpose: `route`, `title` and `redact` all hang off
/// it, so binding it remotely would put a bounded copy of the expansion on the
/// wire for a turn BR-11 says nothing reaches a provider on. With it local, "the
/// provider was never reached" is a statement about the **turn**.
fn remote_route(endpoint: &str, model: &str, max_context: Option<u32>, cap: Option<u32>) -> String {
    remote_route_with_privacy(endpoint, model, max_context, cap, false)
}

fn remote_route_with_privacy(
    endpoint: &str,
    model: &str,
    max_context: Option<u32>,
    cap: Option<u32>,
    redact: bool,
) -> String {
    let mut cfg = String::new();
    if redact {
        cfg.push_str("[privacy]\nredact = true\n\n");
    }
    cfg.push_str("[[providers]]\nid = \"local\"\nkind = \"local\"\n\n");
    cfg.push_str(&remote_provider(
        "frontier",
        endpoint,
        model,
        max_context,
        cap,
    ));
    cfg.push_str("[[tiers]]\ntier = \"reflex\"\nprovider_id = \"local\"\n\n");
    for tier in ["scan", "build", "think"] {
        cfg.push_str(&format!(
            "[[tiers]]\ntier = \"{tier}\"\nprovider_id = \"frontier\"\n\n"
        ));
    }
    cfg
}

/// A provider that answers one turn and records every request it was given — the
/// egress capture every remote leg here reads.
fn vendor() -> MockProvider {
    MockProvider::start(
        Vec::new(),
        MockResponse::ok(openai_turn("done", None, 5, 5)),
    )
}

// ---------------------------------------------------------------------------
// bodies
// ---------------------------------------------------------------------------

/// A body of exactly `word_count` whitespace words and exactly `byte_count`
/// bytes, so a fixture can say which of the budget's two currencies it means to
/// press and be right (REQ-586's Phase-3 F-19).
fn sized_body(word_count: usize, byte_count: usize) -> String {
    assert!(word_count >= 1);
    let letters = byte_count
        .checked_sub(word_count - 1)
        .expect("the byte count must cover the separating spaces");
    assert!(
        letters >= word_count,
        "each of {word_count} words needs at least one letter, and {letters} is not enough"
    );
    let base = letters / word_count;
    let mut extra = letters % word_count;
    let mut out = String::with_capacity(byte_count);
    for i in 0..word_count {
        if i > 0 {
            out.push(' ');
        }
        let mut len = base;
        if extra > 0 {
            len += 1;
            extra -= 1;
        }
        out.extend(std::iter::repeat_n('a', len));
    }
    assert_eq!(out.len(), byte_count, "sized_body must hit its byte target");
    assert_eq!(
        out.split_whitespace().count(),
        word_count,
        "sized_body must hit its word target"
    );
    out
}

/// The head and tail markers a dispatched expansion carries, so "it reached the
/// wire whole" is a claim about bytes rather than about a request count.
const HEAD_MARKER: &str = "OVERBUDGET-HEAD-253";
const TAIL_MARKER: &str = "OVERBUDGET-TAIL-253";

/// [`sized_body`] with the two markers, whose own words and bytes are counted
/// into the totals so the figures stay exact.
fn marked_body(word_count: usize, byte_count: usize) -> String {
    let markers = format!("{HEAD_MARKER} {TAIL_MARKER} ");
    let filler = sized_body(word_count - 2, byte_count - markers.len());
    format!("{HEAD_MARKER} {filler} {TAIL_MARKER}")
}

// ---------------------------------------------------------------------------
// AC-1 — the reported failure, and REQ-590 AC-12 — where it lands today
// ---------------------------------------------------------------------------

/// The local route's budget, derived exactly as [`Router::budget_for`] derives
/// it — never restated as a literal, because the whole of REQ-590 is that this
/// pair is a derivation and not a constant any more.
fn local_pair() -> tetond::harness::budget::RouteBudget {
    tetond::harness::budget::derive(tetond::harness::BudgetInputs::local())
}

/// Stage A's own overhead on one fixture's tree, in each currency, measured off
/// a **real turn in that very tree**.
struct Overhead {
    words: u64,
    bytes: u64,
}

/// The calibration body's two figures, chosen only to be comfortably over the
/// local pair in **both** currencies so the turn raises an offer to read the
/// overhead off.
///
/// Over in both, not one, since ADR-9. The old figures (6,000 / 24,000) cleared
/// a 4,096-word budget by half and cleared nothing else; when REQ-590 raised the
/// word half to 10,240 they went on raising an offer only because the system
/// prompt pushed them past a byte half that had briefly fallen to 30,720. The
/// reversal put that back to 32,768 and every fixture resting on that accident
/// stopped raising an offer at all — which is a fixture testing the daemon's
/// silence and calling it the daemon's question. See [`over_the_local_pair`].
const CALIBRATION_WORDS: usize = 12_000;
const CALIBRATION_BYTES: usize = 48_000;

/// A body comfortably over the local route's derived pair in **both**
/// currencies, whatever that pair currently is.
///
/// Derived from [`local_pair`] rather than written as two literals. Five
/// fixtures in this file exist only to reach the over-budget door on the local
/// tier; none of them cares *which* guard opens it, and all five silently
/// stopped reaching it when ADR-9 moved the byte half back up. A test whose
/// subject is "what the daemon asks when a body does not fit" must not be able
/// to pass by the body fitting, and the only way to guarantee that is to size
/// the body against the number the daemon will actually compare it to.
///
/// A quarter past the word half at 4 bytes a word, rather than "+1": Stage A
/// measures the body **with** the system prompt, so the margin also has to
/// absorb an overhead this fixture does not own. The tests that need an exact
/// boundary calibrate that overhead instead — see [`calibrate`].
///
/// # Two ceilings pull in opposite directions, so both are asserted
///
/// The body must clear the budget and stay under discovery's per-file
/// [`SKILL_MAX_BYTES`](tetond::skills::SKILL_MAX_BYTES). Those two are closer
/// together than they look now that the word half is 10,240: at more than
/// ~6.4 bytes a word a full-budget body does not fit in a `SKILL.md` at all,
/// and the failure does not present as anything about a budget — the skill is
/// *skipped*, and every test here fails with "no skill `/heavy` you can
/// dispatch". Naively doubling both halves of the pair lands exactly on the
/// ceiling and does precisely that.
fn over_the_local_pair() -> String {
    let local = local_pair();
    let words = local.budget_tokens + local.budget_tokens / 4;
    let bytes = words * 4;
    assert!(
        bytes > local.budget_bytes,
        "the fixture must clear the byte half too — {bytes} B against {} B",
        local.budget_bytes
    );
    assert!(
        (bytes as u64) < tetond::skills::SKILL_MAX_BYTES * 9 / 10,
        "a {bytes} B body is within a tenth of discovery's {} B per-file ceiling; past it the \
         skill is skipped rather than measured and every test here fails with `no skill you can \
         dispatch` instead of anything about a budget",
        tetond::skills::SKILL_MAX_BYTES
    );
    sized_body(words, bytes)
}

/// Read Stage A's overhead off `fx`, leaving exactly one offer behind it.
///
/// Stage A measures the body *with the system prompt*, inside the user frame the
/// expansion is wrapped in, so a literal body cannot name its own measured
/// figures. The root path reaches the system prompt — which is why [`Tree`]
/// names are fixed-width and why this is measured per fixture rather than
/// written down once. A change to the system prompt moves the calibration; it
/// does not quietly end the reproduction, because every body below is sized
/// through this and then re-asserted against the figures it produced.
async fn calibrate(fx: &Fixture) -> Overhead {
    let session = fx.session();
    fx.invoke(&session)
        .await
        .expect_err("the calibration body must be over budget, or there is no offer to read");
    let probe = subject_of(&fx.client.sole_offer());
    assert_eq!(
        (probe.budget_tokens, probe.budget_bytes),
        (
            local_pair().budget_tokens as u64,
            local_pair().budget_bytes as u64
        ),
        "the turn must have run on the local route's derived pair"
    );
    Overhead {
        words: probe.measured_tokens - CALIBRATION_WORDS as u64,
        bytes: probe.measured_bytes - CALIBRATION_BYTES as u64,
    }
}

/// **REQ-590 AC-12 — the whole point of the REQ, on a turn that really runs it:
/// does a 4,097-word local turn at the reported body's size serve?**
///
/// It does, across the whole range of sizes the field report admits. That
/// sentence is what this test exists to be able to say, and for most of
/// REQ-590's implementation it was not true.
///
/// The reported failure was *one word* over: **4,097 words against 4,096**, with
/// room still free in the byte half, `bound: local engine`, on a route that
/// declares no window at all. REQ-590 gave that route a word budget derived from
/// the engine's own window — 10,240 — and left the byte half at
/// `LOCAL_BUDGET_BYTES`, 32,768. **Both** halves of the reported measurement are
/// therefore inside the budget, and the turn is served silently.
///
/// # What the record actually says about the byte half
///
/// **The field report gives a rounded byte figure, not an exact one**, and this
/// test is careful not to invent the difference. REQ-589's Description quotes
/// the daemon's own rendered sentence — "about 4,097 words / **31 KB**" — and
/// `bytes_figure` renders `(bytes + 500) / 1_000`, so `31 KB` means the true
/// count lies somewhere in **[30,500, 31,499]**. No exact byte count for that
/// body was ever recorded. The word half *is* exact: 4,097 is rendered without
/// rounding, which is why every leg below is written at that word count.
///
/// That interval is 999 bytes wide and it **straddles** D-4's 30,720, so the
/// record cannot settle whether D-4 would have refused the real body. What it
/// can settle is the shape of the trade, and that needs no measurement at all:
/// a window-derived byte half beats the constant only below **7.5 B/word**
/// (`30,720 / d = 4,096 ⇒ d = 7.5`, exact). At 4,097 words the crossover body
/// is 30,727.5 bytes — 228 above this interval's floor and 771 below its
/// ceiling. Over the whole interval D-4's pair is worth between **+0.7% and
/// −2.4%** against the old one. At best it barely helped; at worst it hurt.
///
/// # This test is the reversal, and it used to assert the opposite
///
/// D-4 originally took the window derivation for the byte half too, at which
/// point the local byte budget was **30,720** — below most of the interval
/// above. This test was written in that state, named
/// `…_and_the_byte_half_is_the_boundary_now`, and asserted the reported
/// measurement *fails* — a green test pinning the REQ's own motivating case as
/// still broken.
///
/// ADR-9 reversed D-4. The reversal's own justification does not rest on any
/// byte count: at **every** density the restored pair is non-regressive,
/// `min(10_240, 32_768/d) ≥ min(4_096, 32_768/d)`, which is BR-7 proved by
/// inspection. The name and the premise go with the reversal: what is asserted
/// below is a body of the reported size serving, in both currencies.
///
/// # Four legs on one fixture
///
/// | measured | outcome |
/// |---|---|
/// | 4,097 words / `budget_bytes + 1` | offered, declined → today's refusal |
/// | the same, accepted | dispatched whole |
/// | 4,097 words / `budget_bytes` | served — the boundary's other side |
/// | **4,097 words / a body inside the reported interval** | **served — the field report's size** |
///
/// Legs 1 and 3 differ by a **single byte**, and that pairing is what stops
/// either passing for a reason of its own: the serving leg cannot be a fixture
/// that merely drifted under budget, because its twin one byte larger is over.
/// Leg 4 is then the report's own size rather than a boundary probe — a body as
/// large as the field report's, asserted to serve on both halves.
///
/// Without leg 4 this file would pin the *boundary* and never the *case*; the
/// two are not the same claim, and it was the second one REQ-590 was opened for.
///
/// # What "dispatches" means here, and what it does not
///
/// AC-1 stops at *dispatch* in as many words, because asserting completion would
/// make the criterion untestable on the very route that motivated the REQ. What
/// is asserted: the accepted turn is not refused, it reaches the engine, and the
/// record it publishes carries the pair `skill_fit` measured rather than a
/// second measurement of something shortened.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_reported_analyze_measurement_serves_on_both_halves_of_the_local_pair() {
    /// The reported measurement's word half.
    const REPORTED_WORDS: u64 = 4_097;
    /// A byte size the reported body **could** have been — deliberately *not*
    /// named as a measurement, because no exact byte count for that body exists.
    ///
    /// The record renders `31 KB`, which under `bytes_figure`'s
    /// `(bytes + 500) / 1_000` means **[30,500, 31,499]**. This is that
    /// interval's midpoint, 31,000, and it is a *choice* among the 1,000 sizes
    /// the record admits, made for two reasons:
    ///
    /// * it re-renders as `31 KB`, so a body of this size would have produced
    ///   the sentence the user actually saw; and
    /// * it is above D-4's 30,720, which is what keeps leg 4 **discriminating**
    ///   — a point drawn from the bottom 23% of the interval would serve under
    ///   D-4 as well and this leg would stop telling the two states apart.
    ///
    /// The second reason is a property of the fixture, not evidence about the
    /// field: it does not show that D-4 refused the real body, only that D-4
    /// refuses a body the record equally admits. At ~7.57 B/word this size is
    /// code-shaped, which is what `/analyze` was pointed at.
    const A_BODY_INSIDE_THE_REPORTED_INTERVAL: u64 = 31_000;

    let local = local_pair();

    // The premise, asserted rather than recounted: 4,097 was one word over the
    // pair this route ran under, and that constant is still in the tree.
    assert_eq!(
        REPORTED_WORDS,
        tetond::harness::budget::LOCAL_BUDGET_TOKENS as u64 + 1,
        "the reported failure was one word over the no-better-fact pair, which is \
         the constant the local arm used to return"
    );

    // -- AC-12, in the arithmetic, before any turn runs ----------------------
    //
    // Both halves, each asserted to hold. The word half is where the report
    // refused; the byte half is where D-4 would have moved the refusal to.
    // Stating both is the difference between "the REQ moved a number" and "the
    // REQ fixed the case".
    assert!(
        REPORTED_WORDS <= local.budget_tokens as u64,
        "REQ-590 AC-12: the local word budget must hold the reported measurement — \
         {REPORTED_WORDS} words against {}",
        local.budget_tokens
    );
    assert!(
        A_BODY_INSIDE_THE_REPORTED_INTERVAL <= local.budget_bytes as u64,
        "REQ-590 AC-12 / ADR-9: the local byte budget must hold a body of the reported size too \
         — {A_BODY_INSIDE_THE_REPORTED_INTERVAL} B against {}. D-4's window-derived byte half \
         was 30,720, below most of the [30,500, 31,499] interval the record admits. If this \
         assertion is red, the REQ has once again moved the refusal instead of removing it",
        local.budget_bytes
    );
    // **Both halves, asserted as the fit AC-12 actually claims** — not as spare
    // room. An earlier form of this pinned the leftover in each currency as an
    // exact pair, which asserted a precision nobody has: the byte figure would
    // have been the budget minus a body size the record never recorded. What
    // AC-12 claims is that the reported measurement is inside the pair, and
    // that is what is asserted, in both currencies at once so neither half can
    // pass alone.
    assert!(
        REPORTED_WORDS <= local.budget_tokens as u64
            && A_BODY_INSIDE_THE_REPORTED_INTERVAL <= local.budget_bytes as u64,
        "REQ-590 AC-12: the reported measurement must fit **both** halves of the local pair — \
         {REPORTED_WORDS} words / {A_BODY_INSIDE_THE_REPORTED_INTERVAL} B against {} / {}. \
         Fitting one half and not the other is how the refusal moved currencies under D-4",
        local.budget_tokens,
        local.budget_bytes
    );

    let fx = Fixture::new(Spec::new(
        "ac1",
        local_route(),
        sized_body(CALIBRATION_WORDS, CALIBRATION_BYTES),
    ));
    let overhead = calibrate(&fx).await;

    // -- leg 1: one byte over, at the reported word count -> offered ----------
    let over = marked_body(
        usize::try_from(REPORTED_WORDS - overhead.words).unwrap(),
        usize::try_from(local.budget_bytes as u64 + 1 - overhead.bytes).unwrap(),
    );
    fx.write_body(&over);
    let reported = fx.session();
    let mut sub = fx.events.subscribe(512);
    let refusal = fx
        .invoke(&reported)
        .await
        .expect_err("one byte over the budget refuses when the offer is declined");

    let offers = fx.client.offers();
    assert_eq!(offers.len(), 2, "the second turn put the question again");
    let offer = subject_of(&offers[1]);
    assert_eq!(
        (offer.measured_tokens, offer.measured_bytes),
        (REPORTED_WORDS, local.budget_bytes as u64 + 1),
        "the calibrated body did not reproduce the intended pair — Stage A's \
         overhead is no longer linear in the body it measures"
    );
    assert_eq!(offer.bound, BudgetBound::LocalEngine, "the reported route");
    assert_eq!(
        offer.verdict,
        WindowVerdict::WindowUnknown,
        "the local tier declares no window, so no other verdict is reachable"
    );
    assert!(
        offer.measured_tokens < offer.budget_tokens,
        "the word half must have room to spare — {} against {} — or this leg is \
         not the byte boundary it claims to be",
        offer.measured_tokens,
        offer.budget_tokens
    );
    // The two figures as the sentence spells them. `bytes_figure` rounds to the
    // nearest KB, so a measurement one byte over a 32,768 B budget renders as
    // the *same* `33 KB` the budget does: at the boundary the sentence quotes
    // two identical byte figures and only the word halves differ — and here the
    // word halves are 4,097 against 10,240, which look like they fit. **A user
    // at this boundary cannot tell from the sentence which currency refused
    // them.** Pinned rather than avoided, because it is what they actually read;
    // recorded as a REQ-590 finding rather than fixed here, because changing the
    // figure's precision is a decision about every budget sentence, not this one.
    //
    // The bound closes it with REQ-590 AC-16's account of itself — the window
    // and the reservation that produced the word half, and the byte half named
    // as fixed so a reader does not try to reconcile 33 KB against 16,384.
    let quoted = "about 4,097 words / 33 KB, and the budget is 10,240 words / 33 KB \
                  (bound: local engine — the word half comes from the engine's 16,384-token \
                  window, less the 1,024 reserved for the reply; the byte half is fixed)";
    assert!(
        offer.sentence.contains(quoted),
        "the offer must quote the figures it measured: {}",
        offer.sentence
    );
    assert!(
        refusal.message.contains(quoted),
        "and so must the refusal a decline produces: {}",
        refusal.message
    );
    assert_eq!(
        refusal.code,
        error_code::SKILL_EXPANSION_TOO_LARGE,
        "a declined offer is today's -32023: {refusal:?}"
    );
    let published = drain(&mut sub);
    assert_eq!(
        offered(&published).len(),
        1,
        "the question was published exactly once for this turn"
    );
    assert!(
        accepted(&published).is_empty(),
        "a declined turn recorded an acceptance"
    );

    // -- leg 2: the same measurement, accepted --------------------------------
    fx.client
        .answers(Answer::Select(OPTION_ID_OVER_BUDGET_PROCEED_ONCE));
    let sent = fx.session();
    let mut sub = fx.events.subscribe(512);
    fx.invoke(&sent)
        .await
        .expect("accepting the offer dispatches the turn");
    let published = drain(&mut sub);
    let record = accepted(&published);
    assert_eq!(record.len(), 1, "the acceptance was recorded once");
    assert_eq!(
        (record[0].measured_tokens, record[0].measured_bytes),
        (REPORTED_WORDS, local.budget_bytes as u64 + 1),
        "the accepted record carries the pair `skill_fit` measured, not a second \
         measurement of a shortened expansion"
    );
    assert_eq!(record[0].window_verdict, WindowVerdict::WindowUnknown);
    assert!(
        published
            .iter()
            .any(|e| matches!(e, Event::SessionUpdate(_))),
        "the accepted turn reached the engine and produced an answer: {:?}",
        published.iter().map(Event::name).collect::<Vec<_>>()
    );

    // -- leg 3: one byte less — the boundary's other side ---------------------
    //
    // The same 4,097 words — the count leg 1 proved this calibration produces —
    // at exactly `budget_bytes`. One byte separates this from leg 1, which is
    // what makes leg 1's refusal and this leg's silence a statement about the
    // guard rather than about either fixture.
    let tag = "at the byte budget exactly";
    let offers_so_far = fx.client.offers().len();
    fx.write_body(&marked_body(
        usize::try_from(REPORTED_WORDS - overhead.words).unwrap(),
        usize::try_from(local.budget_bytes as u64 - overhead.bytes).unwrap(),
    ));
    let served = fx.session();
    let mut sub = fx.events.subscribe(512);
    fx.invoke(&served)
        .await
        .unwrap_or_else(|e| panic!("{tag}: a turn inside both halves must not be refused: {e:?}"));
    let published = drain(&mut sub);
    assert_eq!(
        fx.client.offers().len(),
        offers_so_far,
        "{tag}: a turn that fits must raise **no** over-budget offer — a turn that \
         served but asked anyway would pass a weaker test: {:?}",
        questions(&fx.client)
    );
    assert!(
        offered(&published).is_empty() && accepted(&published).is_empty(),
        "{tag}: and announce nothing about a budget it did not exceed: {:?}",
        published.iter().map(Event::name).collect::<Vec<_>>()
    );
    assert!(
        published
            .iter()
            .any(|e| matches!(e, Event::SessionUpdate(_))),
        "{tag}: the served turn reached the engine and produced an answer: {:?}",
        published.iter().map(Event::name).collect::<Vec<_>>()
    );

    // -- leg 4: the field report itself. **AC-12** ----------------------------
    //
    // Not a boundary probe — the size the user sent. 4,097 words (exact, as the
    // record renders it) at a byte count the record admits, run through the same
    // fixture the three legs above calibrated. It serves: no question, no
    // announcement, an answer.
    //
    // This is the leg the REQ is for. Legs 1–3 would all still pass with the
    // byte half at D-4's 30,720, because each is written *relative to*
    // `budget_bytes` and would simply move with it; **this one would not**, and
    // that is the whole reason it is written against an absolute size instead.
    // Read that as a property of the fixture: it shows D-4 refusing a body the
    // record equally admits, not D-4 refusing the body the user actually sent —
    // which the record cannot decide either way (see the constant's own note).
    // It remains the only assertion in the file that can tell the two states
    // apart.
    let tag = "the reported measurement";
    let offers_so_far = fx.client.offers().len();
    fx.write_body(&marked_body(
        usize::try_from(REPORTED_WORDS - overhead.words).unwrap(),
        usize::try_from(A_BODY_INSIDE_THE_REPORTED_INTERVAL - overhead.bytes).unwrap(),
    ));
    let served = fx.session();
    let mut sub = fx.events.subscribe(512);
    fx.invoke(&served).await.unwrap_or_else(|e| {
        panic!(
            "REQ-590 AC-12: the reported /analyze turn — {REPORTED_WORDS} words / \
             {A_BODY_INSIDE_THE_REPORTED_INTERVAL} bytes — must serve on the local tier. This \
             is the size of the field report this REQ exists for, and a refusal here means the \
             refusal has moved currencies rather than gone away: {e:?}"
        )
    });
    let published = drain(&mut sub);
    assert_eq!(
        fx.client.offers().len(),
        offers_so_far,
        "{tag}: AC-12 requires the reported turn to raise no over-budget offer, not merely to \
         survive one: {:?}",
        questions(&fx.client)
    );
    assert!(
        offered(&published).is_empty() && accepted(&published).is_empty(),
        "{tag}: and to announce nothing about a budget it did not exceed: {:?}",
        published.iter().map(Event::name).collect::<Vec<_>>()
    );
    assert!(
        published
            .iter()
            .any(|e| matches!(e, Event::SessionUpdate(_))),
        "{tag}: the reported turn reached the engine and produced an answer: {:?}",
        published.iter().map(Event::name).collect::<Vec<_>>()
    );
}

/// **AC-1's other half: accepting carries the expansion whole, byte for byte.**
///
/// The local route has no provider to count requests at — its engine *is* the
/// provider — so the "carried whole" claim is made where an egress capture
/// exists: a real socket, and the request body searched for the expansion's own
/// bytes. A build that middle-elided an accepted expansion would reach the wire
/// with the head and the tail and none of the middle, which is exactly what this
/// counts.
///
/// The **declined** run of the same fixture goes first and is the control: same
/// skill, same route, one answer changed, and the socket sees nothing
/// (LESSON-479).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_accepted_offer_puts_every_measured_byte_of_the_expansion_on_the_wire() {
    const WORDS: usize = 6_000;
    const BYTES: usize = 24_000;

    let provider = vendor();
    let fx = Fixture::new(Spec::new(
        "ac1wire",
        remote_route(&provider.openai_endpoint(), UNRECOGNIZED_MODEL, None, None),
        marked_body(WORDS, BYTES),
    ));

    let declined = fx.session();
    fx.invoke(&declined).await.expect_err("declined");
    assert_eq!(
        provider.request_count(),
        0,
        "control: a declined offer put a request on the socket"
    );

    fx.client
        .answers(Answer::Select(OPTION_ID_OVER_BUDGET_PROCEED_ONCE));
    let sent = fx.session();
    let mut sub = fx.events.subscribe(512);
    fx.invoke(&sent).await.expect("accepting dispatches");

    let bodies: Vec<String> = provider
        .requests()
        .iter()
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .collect();
    assert!(!bodies.is_empty(), "nothing reached the provider at all");
    let carried = bodies
        .iter()
        .max_by_key(|b| b.matches(HEAD_MARKER).count())
        .expect("one request");
    assert!(
        carried.contains(HEAD_MARKER) && carried.contains(TAIL_MARKER),
        "the expansion reached the wire without one of its ends"
    );
    // The filler is one run of `a`s per word; a middle-elided expansion keeps the
    // ends and loses the count.
    let words_on_the_wire = carried.matches(" a").count();
    assert!(
        words_on_the_wire >= WORDS - 2,
        "the expansion reached the provider shortened — BR-1 carries it whole or \
         refuses it, never in between; the request held {words_on_the_wire} of \
         {WORDS} words"
    );

    let published = drain(&mut sub);
    let record = accepted(&published);
    assert_eq!(record.len(), 1);
    let offer = subject_of(&fx.client.offers()[1]);
    assert_eq!(
        (record[0].measured_tokens, record[0].measured_bytes),
        (offer.measured_tokens, offer.measured_bytes),
        "the accepted record and the question quote one measurement, not two"
    );
}

// ---------------------------------------------------------------------------
// AC-3 — the decline is today's refusal
// ---------------------------------------------------------------------------

/// **AC-3 — declining produces today's refusal, byte for byte, under -32023.**
///
/// The offer and its own decline are composed from **one** measurement through
/// **one** composer, so the claim available here is stronger than "both mention
/// the same numbers": everything up to the tail is the same bytes, and the tail
/// the decline takes is the pre-REQ-589 sentence exactly.
///
/// The skill is **user**-sourced deliberately. Today's refusal carries no source
/// marker, so a project skill's offer head reads `` `/heavy` (this repository's
/// skill) `` where its refusal reads `` `/heavy` `` — those two heads
/// legitimately differ, and only a user skill lets the equality be asserted as
/// bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn declining_is_todays_refusal_in_every_byte() {
    let fx = Fixture::new(Spec::new("ac3", local_route(), over_the_local_pair()).user_sourced());
    let session = fx.session();
    let refusal = fx.invoke(&session).await.expect_err("declined");
    assert_eq!(refusal.code, error_code::SKILL_EXPANSION_TOO_LARGE);

    let question = subject_of(&fx.client.sole_offer()).sentence;
    let shared: String = question
        .chars()
        .zip(refusal.message.chars())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a)
        .collect();
    assert!(
        shared.ends_with("; the byte half is fixed). "),
        "the question and its decline must share the whole head — the subject, \
         the stage clause, both figure pairs and the spoken bound, which since \
         REQ-590 AC-16 ends by accounting for the local pair. They diverge \
         at: {shared}"
    );
    assert!(
        shared.contains("(bound: local engine — "),
        "…and the bound they share is this route's: {shared}"
    );
    assert_eq!(
        refusal.message,
        format!("{shared}{NOTHING_WAS_SENT}"),
        "the decline is that head plus today's consequence clause, and nothing \
         else"
    );
    for clause in EVERY_VERDICT_CLAUSE {
        assert!(
            !refusal.message.contains(clause),
            "the refusal borrowed one of the offer's verdict clauses: {clause}"
        );
    }
    for tail in [
        CLOSING_WITH_REMEDY,
        CLOSING_ONE_TIME_ONLY,
        "The durable fix",
    ] {
        assert!(
            !refusal.message.contains(tail),
            "the refusal borrowed the offer's tail: {tail}"
        );
    }
    assert!(
        !refusal.message.contains("this repository's skill"),
        "a user skill's refusal must not carry a source marker today's refusal \
         never had: {}",
        refusal.message
    );
}

// ---------------------------------------------------------------------------
// AC-4 — silence is never consent
// ---------------------------------------------------------------------------

/// **AC-4 — no path maps an unanswered offer to proceed** (BR-4).
///
/// Four ways an offer can fail to become a yes, each driven from a real turn,
/// and each paired against the same fixture answering `proceed_once` — without
/// that control every leg below is equally consistent with a harness that cannot
/// dispatch at all.
///
/// The `Unrecognized` leg is `interpret_over_budget`'s `_ =>` arm: a client that
/// answers with an id the prompt never carried. Mapping that arm to `Allowed`
/// would send an oversized expansion on an answer nobody gave, and it reddens
/// here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_unanswered_offer_resolves_to_proceed() {
    // The control: this route, this body, this client — answered yes.
    let provider = vendor();
    let fx = Fixture::new(
        Spec::new(
            "ac4ctl",
            remote_route(&provider.openai_endpoint(), UNRECOGNIZED_MODEL, None, None),
            sized_body(6_000, 24_000),
        )
        .answering(Answer::Select(OPTION_ID_OVER_BUDGET_PROCEED_ONCE)),
    );
    let session = fx.session();
    fx.invoke(&session)
        .await
        .expect("control: an accepted offer on this fixture does dispatch");
    assert_eq!(
        provider.request_count(),
        1,
        "control: the accepted turn reached the provider"
    );

    // Leg (a): a human closed the prompt without choosing.
    fx.client.answers(Answer::Cancel);
    let cancelled = fx.session();
    let err = fx
        .invoke(&cancelled)
        .await
        .expect_err("a cancelled prompt is not a yes");
    assert_eq!(err.code, error_code::SKILL_EXPANSION_TOO_LARGE);
    assert!(err.message.contains(NOTHING_WAS_SENT), "{}", err.message);

    // Leg (b): an option id the prompt never carried.
    fx.client.answers(Answer::Unrecognized);
    let unknown = fx.session();
    let err = fx
        .invoke(&unknown)
        .await
        .expect_err("an unrecognized option is not a yes");
    assert_eq!(err.code, error_code::SKILL_EXPANSION_TOO_LARGE);
    assert!(err.message.contains(NOTHING_WAS_SENT), "{}", err.message);

    assert_eq!(
        provider.request_count(),
        1,
        "neither unanswered leg reached the provider"
    );

    // Leg (c): a daemon with no route to deliver an addressed request on —
    // `SkillConsent::Unanswerable`, which is what a non-interactive client is.
    let mute_provider = vendor();
    let mute = Fixture::new(
        Spec::new(
            "ac4mut",
            remote_route(
                &mute_provider.openai_endpoint(),
                UNRECOGNIZED_MODEL,
                None,
                None,
            ),
            sized_body(6_000, 24_000),
        )
        .user_sourced()
        .unanswerable()
        .answering(Answer::Select(OPTION_ID_OVER_BUDGET_PROCEED_ONCE)),
    );
    let session = mute.session();
    let err = mute
        .invoke(&session)
        .await
        .expect_err("an offer nobody can be shown is not a yes");
    assert_eq!(err.code, error_code::SKILL_EXPANSION_TOO_LARGE);
    assert!(err.message.contains(NOTHING_WAS_SENT), "{}", err.message);
    assert!(
        mute.client.offers().is_empty(),
        "the fixture's client was never reachable, so it saw nothing"
    );
    assert_eq!(mute_provider.request_count(), 0);

    // Leg (d): a turn with no connection at all — nobody to address.
    let lone_provider = vendor();
    let lone = Fixture::new(
        Spec::new(
            "ac4lon",
            remote_route(
                &lone_provider.openai_endpoint(),
                UNRECOGNIZED_MODEL,
                None,
                None,
            ),
            sized_body(6_000, 24_000),
        )
        .user_sourced()
        .unconnected()
        .answering(Answer::Select(OPTION_ID_OVER_BUDGET_PROCEED_ONCE)),
    );
    let session = lone.session();
    let mut sub = lone.events.subscribe(512);
    let err = lone
        .invoke(&session)
        .await
        .expect_err("a turn with nobody to ask is not a yes");
    assert_eq!(err.code, error_code::SKILL_EXPANSION_TOO_LARGE);
    assert!(err.message.contains(NOTHING_WAS_SENT), "{}", err.message);
    assert_eq!(lone_provider.request_count(), 0);
    assert!(
        offered(&drain(&mut sub)).is_empty(),
        "a question nobody was asked must not be recorded as one that was"
    );
}

// ---------------------------------------------------------------------------
// AC-6 — three verdicts, three sentences, and every one of them is offered
// ---------------------------------------------------------------------------

/// **AC-6 — all three window verdicts produce an offer, and each arm pins its
/// own wording** (BR-3, ADR-15, ADR-17).
///
/// Every cell here is one the reachability table says occurs; the nine that
/// cannot are not written, because a test for one passes vacuously (LESSON-520).
/// Each leg asserts its own clause **and the absence of the other three**, so an
/// arm that fell through to a neighbour reddens rather than reading as a pass.
///
/// The `UserCap` + `FitsWindow` cell is the one that discriminates
/// `budget_inputs_for(..).window` from `.cap` (ADR-15): the route declares
/// 200,000 and the user caps it at 6,000, so a build that compared the
/// measurement against the *cap* would call this `ExceedsWindow` and redden.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_reachable_window_verdict_is_offered_and_pins_its_own_sentence() {
    let cells: [(&str, BudgetBound, WindowVerdict, &str); 7] = [
        (
            "local engine / window unknown",
            BudgetBound::LocalEngine,
            WindowVerdict::WindowUnknown,
            WINDOW_UNKNOWN_CLAUSE,
        ),
        (
            "unknown window / window unknown",
            BudgetBound::DefaultUnknown,
            WindowVerdict::WindowUnknown,
            WINDOW_UNKNOWN_CLAUSE,
        ),
        (
            "window / fits",
            BudgetBound::Window,
            WindowVerdict::FitsWindow,
            FITS_WINDOW_INTO_THE_RESERVATION,
        ),
        (
            "window / exceeds",
            BudgetBound::Window,
            WindowVerdict::ExceedsWindow,
            EXCEEDS_WINDOW_CLAUSE,
        ),
        (
            "user cap / fits",
            BudgetBound::UserCap,
            WindowVerdict::FitsWindow,
            FITS_WINDOW_INTO_THE_RESERVATION,
        ),
        (
            "user cap / exceeds",
            BudgetBound::UserCap,
            WindowVerdict::ExceedsWindow,
            EXCEEDS_WINDOW_CLAUSE,
        ),
        (
            "redact scan / fits",
            BudgetBound::RedactScan,
            WindowVerdict::FitsWindow,
            FITS_WINDOW_CLAUSE,
        ),
    ];

    for (tag, want_bound, want_verdict, clause) in cells {
        let (fx, _provider) = route_for(want_bound, want_verdict);
        let session = fx.session();
        let mut sub = fx.events.subscribe(512);
        fx.invoke(&session)
            .await
            .expect_err("every cell here declines, and a decline refuses");
        let offer = subject_of(&fx.client.sole_offer());
        assert_eq!(offer.bound, want_bound, "{tag}: wrong bound");
        assert_eq!(offer.verdict, want_verdict, "{tag}: wrong verdict");
        assert!(
            offer.sentence.contains(clause),
            "{tag}: the offer must carry its own verdict clause: {}",
            offer.sentence
        );
        for other in EVERY_VERDICT_CLAUSE {
            if other == clause {
                continue;
            }
            assert!(
                !offer.sentence.contains(other),
                "{tag}: the offer also carried another arm's clause — `{other}` in \
                 {}",
                offer.sentence
            );
        }
        // BR-3's governing stance: the question is asked on every cell, including
        // the ones the daemon expects the provider to reject.
        assert_eq!(
            offered(&drain(&mut sub)).len(),
            1,
            "{tag}: the verdict selects a sentence, never whether to ask"
        );
    }
}

/// One fixture per reachable (bound, verdict) cell.
///
/// The arithmetic each row rests on, stated once. `derive` budgets from
/// `window − reservation` while `window_verdict` compares the measurement
/// against the **raw** declared window (ADR-15), so the band between them is
/// exactly where `FitsWindow` lives on a route that is over budget. Where the
/// derived byte pair would fall under REQ-586's floor the floor raises it, and a
/// floored route can no longer reach `FitsWindow` at all — which is why the two
/// `Window` cells use 20,000 and 8,000 rather than one window twice.
///
/// The provider is returned alongside so the caller keeps the socket alive.
fn route_for(bound: BudgetBound, verdict: WindowVerdict) -> (Fixture, Option<MockProvider>) {
    match (bound, verdict) {
        (BudgetBound::LocalEngine, WindowVerdict::WindowUnknown) => (
            Fixture::new(Spec::new("v6loc", local_route(), over_the_local_pair())),
            None,
        ),
        (BudgetBound::DefaultUnknown, WindowVerdict::WindowUnknown) => {
            let provider = vendor();
            let fx = Fixture::new(Spec::new(
                "v6def",
                remote_route(&provider.openai_endpoint(), UNRECOGNIZED_MODEL, None, None),
                sized_body(6_000, 24_000),
            ));
            (fx, Some(provider))
        }
        (BudgetBound::Window, WindowVerdict::FitsWindow) => {
            // 20,000 declared: the budget is (12,650 words / 37,952 B). The body
            // is over in **bytes only**, and half the measured bytes still sit
            // under the raw window.
            let provider = vendor();
            let fx = Fixture::new(Spec::new(
                "v6wfit",
                remote_route(
                    &provider.openai_endpoint(),
                    UNRECOGNIZED_MODEL,
                    Some(20_000),
                    None,
                ),
                sized_body(10_000, 31_000),
            ));
            (fx, Some(provider))
        }
        (BudgetBound::Window, WindowVerdict::ExceedsWindow) => {
            let provider = vendor();
            let fx = Fixture::new(Spec::new(
                "v6wexc",
                remote_route(
                    &provider.openai_endpoint(),
                    UNRECOGNIZED_MODEL,
                    Some(8_000),
                    None,
                ),
                sized_body(12_000, 48_000),
            ));
            (fx, Some(provider))
        }
        (BudgetBound::UserCap, WindowVerdict::FitsWindow) => {
            // The ADR-15 discriminator: a 200,000-token declaration under a
            // 6,000-token cap. Over budget, over the cap, and still comfortably
            // inside the window the provider actually declared.
            let provider = vendor();
            let fx = Fixture::new(Spec::new(
                "v6cfit",
                remote_route(
                    &provider.openai_endpoint(),
                    UNRECOGNIZED_MODEL,
                    Some(200_000),
                    Some(6_000),
                ),
                sized_body(6_000, 24_000),
            ));
            (fx, Some(provider))
        }
        (BudgetBound::UserCap, WindowVerdict::ExceedsWindow) => {
            let provider = vendor();
            let fx = Fixture::new(Spec::new(
                "v6cexc",
                remote_route(
                    &provider.openai_endpoint(),
                    UNRECOGNIZED_MODEL,
                    Some(8_000),
                    Some(6_000),
                ),
                sized_body(12_000, 48_000),
            ));
            (fx, Some(provider))
        }
        (BudgetBound::RedactScan, WindowVerdict::FitsWindow) => redact_scan_route(),
        (bound, verdict) => panic!(
            "{bound:?} + {verdict:?} is not a cell the reachability table admits — a \
             fixture for it would pass vacuously (LESSON-520)"
        ),
    }
}

/// The `RedactScan` cell, and the one fixture here that needs an argument
/// string.
///
/// The redact clamp is a fixed 88,196 bytes and `SKILL.md` is capped at 64 KiB
/// by discovery, so no skill **body** can reach past that ceiling on its own.
/// The expansion is therefore pushed over with `$ARGUMENTS`, which is bounded
/// only by the RPC frame. The declared window is 50,000 so the window-derived
/// byte pair (97,952 B) sits *above* the clamp — which is what makes the clamp
/// the bound rather than the window.
fn redact_scan_route() -> (Fixture, Option<MockProvider>) {
    let provider = vendor();
    let fx = Fixture::new(
        Spec::new(
            "v6red",
            remote_route_with_privacy(
                &provider.openai_endpoint(),
                UNRECOGNIZED_MODEL,
                Some(50_000),
                None,
                true,
            ),
            format!("{} $ARGUMENTS", sized_body(20_000, 60_000)),
        )
        .with_arguments(sized_body(7_000, 29_500)),
    );
    (fx, Some(provider))
}

// ---------------------------------------------------------------------------
// AC-7 / AC-7a — the remedy table
// ---------------------------------------------------------------------------

/// **AC-7 — BR-7's table, one row at a time, from a real classification.**
///
/// The `remedy_kind` asserted is the one the daemon **published**, not one the
/// test computed. `RedactScan` is the only row asserting that no durable write
/// is offered (BR-7b): it is the bound left remedy-less on privacy grounds, and
/// its prompt must carry the two-option question rather than the four-option
/// one, or the wording would gesture at a fix that does not exist.
///
/// The `LocalEngine` row is BR-9's pair, and it needs **exactly one** remote
/// provider registered: at zero there is nothing to bind to, and at two or more
/// ADR-12 withholds the option rather than choosing where a whole category's
/// spend goes. Both of those are the same `plan == None` the rest of this file's
/// local fixtures sit in — which is why this row is the one that registers a
/// provider.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_bound_offers_exactly_the_remedy_the_table_names() {
    // (bound, the kind BR-7 names, whether the prompt carries the durable
    // options, the phrase the write names)
    let rows: [(BudgetBound, RemedyKind, bool, &str); 5] = [
        (
            BudgetBound::DefaultUnknown,
            RemedyKind::DeclareWindow,
            true,
            "write `capabilities.max_context = 1000000` for `frontier`",
        ),
        (
            BudgetBound::UserCap,
            RemedyKind::RaiseCap,
            true,
            "write `capabilities.context_budget_cap = 0` for `frontier`, which removes \
             the ceiling you set rather than raising it",
        ),
        (
            BudgetBound::Window,
            RemedyKind::RaiseWindow,
            true,
            "raise `capabilities.max_context = 1000000` for `frontier`",
        ),
        (
            BudgetBound::LocalEngine,
            RemedyKind::BindTierRemote,
            true,
            // TASK-260: this row's fixture registers exactly one remote, which
            // is ADR-12's *propose by name* count — so BR-9's write is concrete
            // in both halves, `frontier` and its window, rather than the "a
            // remote provider" ADR-18 item 2 recorded.
            "bind the `build` tier to `frontier` and declare its \
             `capabilities.max_context = 1000000` in the same change",
        ),
        (
            BudgetBound::RedactScan,
            RemedyKind::NotOffered,
            false,
            NO_REMEDY_CLAUSE,
        ),
    ];

    for (bound, kind, durable, phrase) in rows {
        let (fx, _provider) = remedy_route(bound);
        let session = fx.session();
        let mut sub = fx.events.subscribe(512);
        fx.invoke(&session).await.expect_err("declined");
        let request = fx.client.sole_offer();
        let offer = subject_of(&request);
        assert_eq!(offer.bound, bound, "the fixture did not produce {bound:?}");

        let published = offered(&drain(&mut sub));
        assert_eq!(published.len(), 1, "{bound:?}: one offer, one record");
        assert_eq!(
            published[0].remedy_kind, kind,
            "{bound:?}: BR-7's table names {kind:?}"
        );
        assert!(
            offer.sentence.contains(phrase),
            "{bound:?}: the sentence must name the concrete write: {}",
            offer.sentence
        );

        let ids = option_ids(&request);
        if durable {
            let mut sorted = ids.clone();
            sorted.sort();
            assert_eq!(
                sorted,
                {
                    let mut want = vec![
                        OPTION_ID_OVER_BUDGET_PROCEED_ONCE.to_owned(),
                        OPTION_ID_OVER_BUDGET_PROCEED_AND_REMEDY.to_owned(),
                        OPTION_ID_OVER_BUDGET_REMEDY_ONLY.to_owned(),
                        OPTION_ID_OVER_BUDGET_DECLINE.to_owned(),
                    ];
                    want.sort();
                    want
                },
                "{bound:?}: a bound with a remedy offers all four answers: {ids:?}"
            );
            assert!(
                offer.sentence.ends_with(CLOSING_WITH_REMEDY),
                "{bound:?}: {}",
                offer.sentence
            );
        } else {
            assert_eq!(
                ids,
                vec![
                    OPTION_ID_OVER_BUDGET_PROCEED_ONCE.to_owned(),
                    OPTION_ID_OVER_BUDGET_DECLINE.to_owned(),
                ],
                "BR-7b: the redact clamp is the one bound that offers no durable \
                 write, and the prompt must not imply one exists"
            );
            assert!(
                offer.sentence.ends_with(CLOSING_ONE_TIME_ONLY),
                "BR-7b: the closing question must not gesture at a fix: {}",
                offer.sentence
            );
            assert!(
                !offer.sentence.contains("The durable fix is to"),
                "BR-7b: {}",
                offer.sentence
            );
        }
    }
}

/// A fixture for each BR-7 row, using the **recipe-backed** model wherever a
/// window value has to be proposed — BR-7c looks a value up and never invents
/// one, so a row whose write names a number needs a provider the shipped catalog
/// recognizes.
fn remedy_route(bound: BudgetBound) -> (Fixture, Option<MockProvider>) {
    match bound {
        BudgetBound::DefaultUnknown => {
            let provider = vendor();
            let fx = Fixture::new(Spec::new(
                "v7def",
                remote_route(&provider.openai_endpoint(), RECIPE_MODEL, None, None),
                sized_body(6_000, 24_000),
            ));
            (fx, Some(provider))
        }
        BudgetBound::UserCap => {
            let provider = vendor();
            let fx = Fixture::new(Spec::new(
                "v7cap",
                remote_route(
                    &provider.openai_endpoint(),
                    RECIPE_MODEL,
                    Some(200_000),
                    Some(6_000),
                ),
                sized_body(6_000, 24_000),
            ));
            (fx, Some(provider))
        }
        BudgetBound::Window => {
            let provider = vendor();
            let fx = Fixture::new(Spec::new(
                "v7win",
                remote_route(
                    &provider.openai_endpoint(),
                    RECIPE_MODEL,
                    Some(20_000),
                    None,
                ),
                sized_body(10_000, 31_000),
            ));
            (fx, Some(provider))
        }
        BudgetBound::LocalEngine => {
            let provider = vendor();
            let fx = Fixture::new(Spec::new(
                "v7loc",
                local_route_with_one_remote(&provider.openai_endpoint()),
                over_the_local_pair(),
            ));
            (fx, Some(provider))
        }
        BudgetBound::RedactScan => redact_scan_route(),
        other => panic!("BR-7's table has no row for {other:?}"),
    }
}

/// **AC-7a — a `RaiseWindow` offer cannot be rendered without BR-7a's risk.**
///
/// Raising `capabilities.max_context` above the provider's real window does not
/// enlarge that window; it converts a local refusal into a remote error. The
/// risk sentence rides `RemedyClause::render`, which is the *only* rendering
/// there is — so it reaches the question and **both** durable option labels, and
/// no path produces one without the other.
///
/// Asserted on both a recipe-backed proposal (a number is named) and an
/// unrecognized provider (BR-7c proposes nothing and no durable option is
/// offered), because the two take different arms of the write phrase and only
/// the consequence is shared.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_raise_window_offer_cannot_be_rendered_without_its_risk() {
    for (tag, model, proposal_expected) in [
        ("v7arec", RECIPE_MODEL, true),
        ("v7anon", UNRECOGNIZED_MODEL, false),
    ] {
        let provider = vendor();
        let fx = Fixture::new(Spec::new(
            tag,
            remote_route(&provider.openai_endpoint(), model, Some(20_000), None),
            sized_body(10_000, 31_000),
        ));
        let session = fx.session();
        let mut sub = fx.events.subscribe(512);
        fx.invoke(&session).await.expect_err("declined");
        let request = fx.client.sole_offer();
        let offer = subject_of(&request);
        assert_eq!(offer.bound, BudgetBound::Window, "{tag}");
        assert_eq!(
            offered(&drain(&mut sub))[0].remedy_kind,
            RemedyKind::RaiseWindow,
            "{tag}"
        );
        assert!(
            offer.sentence.contains(RAISE_WINDOW_RISK),
            "{tag}: the question must state what raising a window risks: {}",
            offer.sentence
        );

        let durable: Vec<&PermissionOption> = request
            .options
            .iter()
            .filter(|o| {
                o.option_id == OPTION_ID_OVER_BUDGET_PROCEED_AND_REMEDY
                    || o.option_id == OPTION_ID_OVER_BUDGET_REMEDY_ONLY
            })
            .collect();
        if proposal_expected {
            assert_eq!(
                durable.len(),
                2,
                "{tag}: a recipe-backed window has a value to write, so both \
                 durable answers are offered: {:?}",
                option_ids(&request)
            );
            for option in durable {
                assert!(
                    option.label.contains(RAISE_WINDOW_RISK),
                    "{tag}: a remedy label cannot shed the risk it carries: {}",
                    option.label
                );
                assert!(
                    option.label.contains("capabilities.max_context = 1000000"),
                    "{tag}: ADR-1's rule — every remedy label names the concrete \
                     write: {}",
                    option.label
                );
            }
        } else {
            assert!(
                durable.is_empty(),
                "{tag}: BR-7c proposes nothing for a provider no recipe matches, so \
                 no option may promise a write it cannot make: {:?}",
                option_ids(&request)
            );
            assert!(
                offer
                    .sentence
                    .contains("this daemon ships no figure for it and will not invent one"),
                "{tag}: and the sentence says so: {}",
                offer.sentence
            );
            // **ADR-1: the sentence may not offer an answer the prompt has no
            // row for.** This leg is the live BR-7c case — the classification
            // says `RaiseWindow`, the plan is `None`, and the question used to
            // close "Send it whole this once, take the durable fix, both, or
            // neither?" above two options, neither of which is the durable fix.
            assert!(
                offer.sentence.ends_with(CLOSING_ONE_TIME_ONLY),
                "{tag}: no remedy row was drawn, so the closing must not offer one: {}",
                offer.sentence
            );
            assert!(
                !offer.sentence.contains(CLOSING_WITH_REMEDY),
                "{tag}: {}",
                offer.sentence
            );
            // The fix is still *named* — BR-7c's posture is to state it and ask
            // for the value — so this is not the sentence going quiet.
            assert!(
                offer.sentence.contains("The durable fix is to"),
                "{tag}: {}",
                offer.sentence
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The review pass (REQ-589 Phase 5)
// ---------------------------------------------------------------------------

/// **ADR-6 rule 2: a spend ceiling is never cleared for a refusal that would
/// stand anyway** (REQ-589 review pass).
///
/// The reviewer's construction, driven through a real turn. One provider,
/// `max_context = 30000` and `context_budget_cap = 10000`, so every leg stamps
/// `bound: user cap` and classifies `RaiseCap`. The legs differ only in the
/// size of the expansion:
///
/// * **hopeless** — past the pair the *declared window* derives on its own.
///   Clearing the cap writes `0`, the route re-derives at `bound: window`, and
///   the very next invocation meets the identical refusal — with the user's
///   spend ceiling deleted for nothing. No option may offer that, and the
///   closing question must not gesture at it either.
/// * **resolvable** — over the cap and comfortably inside the window behind it.
///   This is what BR-7 wrote the remedy for, and it is the non-vacuity guard:
///   the same fixture, the same bound, the same classification, and both
///   durable options present.
///
/// **Mutation**: drop the `clearing_the_cap_clears` gate in
/// `plan_over_budget_remedy` and the hopeless leg fails at the option list;
/// point the closing question back at `Remedy::is_offered` and it fails at the
/// sentence.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cap_is_only_offered_for_clearing_where_clearing_it_would_help() {
    // (tag, body words, body bytes, the clearing is worth offering)
    let legs: [(&'static str, usize, usize, bool); 2] = [
        ("v5hope", 25_000, 55_000, false),
        ("v5fix", 8_000, 30_000, true),
    ];

    for (tag, words, bytes, offerable) in legs {
        let provider = vendor();
        let fx = Fixture::new(Spec::new(
            tag,
            remote_route(
                &provider.openai_endpoint(),
                RECIPE_MODEL,
                Some(30_000),
                Some(10_000),
            ),
            sized_body(words, bytes),
        ));
        let session = fx.session();
        let mut sub = fx.events.subscribe(512);
        fx.invoke(&session).await.expect_err("declined");
        let request = fx.client.sole_offer();
        let offer = subject_of(&request);
        let published = drain(&mut sub);

        // Both legs are the same cell of the reachability table, and both are
        // classified `RaiseCap`: whatever separates them below is not the bound
        // and not BR-7's table.
        assert_eq!(offer.bound, BudgetBound::UserCap, "{tag}");
        assert_eq!(
            offered(&published)[0].remedy_kind,
            RemedyKind::RaiseCap,
            "{tag}"
        );

        let durable: Vec<&PermissionOption> = request
            .options
            .iter()
            .filter(|o| {
                o.option_id == OPTION_ID_OVER_BUDGET_PROCEED_AND_REMEDY
                    || o.option_id == OPTION_ID_OVER_BUDGET_REMEDY_ONLY
            })
            .collect();

        if offerable {
            assert_eq!(
                durable.len(),
                2,
                "{tag}: clearing this cap does clear the measurement, so BR-7's remedy \
                 stands: {:?}",
                option_ids(&request)
            );
            for option in durable {
                // ADR-1, on the one write that deletes rather than sets.
                assert!(
                    option
                        .label
                        .contains("write `capabilities.context_budget_cap = 0` for `frontier`"),
                    "{tag}: the label must name the concrete write: {}",
                    option.label
                );
                assert!(
                    option
                        .label
                        .contains("removes the ceiling you set rather than raising it"),
                    "{tag}: and that the write is a removal: {}",
                    option.label
                );
            }
            assert!(
                offer.sentence.ends_with(CLOSING_WITH_REMEDY),
                "{tag}: {}",
                offer.sentence
            );
        } else {
            assert!(
                durable.is_empty(),
                "{tag}: clearing this cap leaves the expansion over the window-derived \
                 budget, so no option may spend the user's spend ceiling on it: {:?}",
                option_ids(&request)
            );
            assert!(
                offer.sentence.ends_with(CLOSING_ONE_TIME_ONLY),
                "{tag}: no remedy row was drawn, so the closing must not offer one: {}",
                offer.sentence
            );
        }

        // Neither leg wrote anything: this test is about what is *offered*.
        assert!(remedies(&published).is_empty(), "{tag}");
        assert!(
            fx.config_on_disk().contains("context_budget_cap = 10000"),
            "{tag}: the ceiling is still on disk"
        );
    }
}

/// **A provider that moved while the offer waited is not silently restored to
/// what the offer was composed from** (REQ-589 review pass).
///
/// `RegisterProvider` replaces `endpoint`, `model` and `auth_ref` wholesale —
/// only the two capability fields merge field-wise — and the plan captured all
/// three from the config snapshot the *question* was built under. The answer
/// arrives after an await on a human that has no timeout, so `config/set` and
/// `teton provider add` are open the whole time.
///
/// This drives exactly that: the offer goes up, the user re-registers
/// `frontier` against a different model while it is on screen, and only then
/// answers *"do not send it, but take the durable fix"*. Unguarded, the remedy's
/// write restores the model the offer was composed from — a change silently
/// undone, with the next turn calling the old registration.
///
/// What must happen instead: the remedy is refused, nothing is written, and the
/// user's change stands. `previous_value` is not merely absent here — the whole
/// record is, because no write landed.
///
/// **Mutation**: remove the `provider_identity_unchanged` guard from
/// `apply_over_budget_remedy` and this fails on the very first assertion, with
/// the stale model back on disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_provider_changed_while_the_offer_waits_is_not_reverted_by_the_remedy() {
    use teton_protocol::methods::{ConfigUpdate, ProviderConfig};
    use teton_protocol::{ProviderId, ProviderKind};

    let provider = vendor();
    let endpoint = provider.openai_endpoint();
    let fx = Fixture::new(
        Spec::new(
            "v5race",
            remote_route(&endpoint, RECIPE_MODEL, Some(200_000), Some(6_000)),
            sized_body(6_000, 24_000),
        )
        .answering(Answer::Select(OPTION_ID_OVER_BUDGET_REMEDY_ONLY)),
    );
    let before = fx.config_on_disk();
    assert!(
        before.contains(RECIPE_MODEL) && before.contains("context_budget_cap = 6000"),
        "the fixture must start from the registration the offer is composed under: {before}"
    );

    // The user, at 10:02, with the question from 10:00 still on screen.
    let runtime = Arc::clone(&fx.runtime);
    let moved_to = endpoint.clone();
    fx.client.while_offered(move || {
        runtime
            .apply_config_update(ConfigUpdate::RegisterProvider(ProviderConfig {
                id: ProviderId::from("frontier"),
                kind: ProviderKind::OpenaiCompatible,
                endpoint: Some(moved_to.clone()),
                model: Some(UNRECOGNIZED_MODEL.to_owned()),
                auth_ref: None,
                max_context: None,
                context_budget_cap: None,
                floored_budget: None,
            }))
            .expect("the user's own re-registration lands");
    });

    let session = fx.session();
    let mut sub = fx.events.subscribe(512);
    fx.invoke(&session)
        .await
        .expect_err("remedy_only does not send the turn");
    let published = drain(&mut sub);

    // The offer was really raised and really carried the remedy, or there is
    // nothing here to have gone wrong.
    let request = fx.client.sole_offer();
    assert!(
        option_ids(&request).contains(&OPTION_ID_OVER_BUDGET_REMEDY_ONLY.to_owned()),
        "non-vacuity: the fixture must offer the durable fix: {:?}",
        option_ids(&request)
    );

    let after = fx.config_on_disk();
    assert!(
        after.contains(UNRECOGNIZED_MODEL),
        "the user's change was undone by a write planned before they made it: {after}"
    );
    assert!(
        !after.contains(RECIPE_MODEL),
        "the model the offer was composed under came back: {after}"
    );
    // The remedy failed rather than half-applied, and said so rather than
    // publishing a record of a write that did not happen.
    assert!(
        after.contains("context_budget_cap = 6000"),
        "the cap was cleared by a plan whose provider had moved: {after}"
    );
    assert!(
        remedies(&published).is_empty(),
        "a durable write was recorded that must not have happened: {:?}",
        remedies(&published)
    );
    // And the document still loads, with both facts as the user left them.
    let reparsed = teton_core::Config::load(&after).expect("the config still loads");
    let frontier = reparsed
        .providers
        .iter()
        .find(|p| p.id == "frontier")
        .expect("the provider is still registered");
    assert_eq!(frontier.declared_model(), Some(UNRECOGNIZED_MODEL));
    assert_eq!(frontier.capabilities.context_budget_cap, 6_000);
}

// ---------------------------------------------------------------------------
// AC-7b — the two answers are independent
// ---------------------------------------------------------------------------

/// **AC-7b — `proceed` and `apply_remedy` are honored independently, in all four
/// combinations.**
///
/// The `UserCap` route is the one whose remedy this build applies in-line, so it
/// is where each of the four cells can be read at both surfaces: the socket for
/// whether the turn ran, and **the config file on disk** for whether the limit
/// moved (LESSON-519 — inspect the artifact, not a return code; and re-parse it,
/// because a file that no longer loads is not a remedy).
///
/// `remedy_only` is the cell that matters most and the one a weaker
/// implementation collapses: the limit is raised and the oversized turn still
/// does not run. `proceed_once` is its mirror — the turn runs and the config is
/// byte-identical afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn proceed_and_remedy_are_answered_independently_in_all_four_combinations() {
    // (tag, option id, the turn ran, the config moved)
    let cells: [(&'static str, &'static str, bool, bool); 4] = [
        ("v7bonce", OPTION_ID_OVER_BUDGET_PROCEED_ONCE, true, false),
        (
            "v7bboth",
            OPTION_ID_OVER_BUDGET_PROCEED_AND_REMEDY,
            true,
            true,
        ),
        ("v7bfix", OPTION_ID_OVER_BUDGET_REMEDY_ONLY, false, true),
        ("v7bno", OPTION_ID_OVER_BUDGET_DECLINE, false, false),
    ];

    for (tag, option, ran, wrote) in cells {
        let provider = vendor();
        let fx = Fixture::new(
            Spec::new(
                tag,
                remote_route(
                    &provider.openai_endpoint(),
                    RECIPE_MODEL,
                    Some(200_000),
                    Some(6_000),
                ),
                sized_body(6_000, 24_000),
            )
            .answering(Answer::Select(option)),
        );
        let before = fx.config_on_disk();
        assert!(
            before.contains("context_budget_cap = 6000"),
            "{tag}: the fixture must start with the cap it is about to be asked to \
             clear: {before}"
        );
        let session = fx.session();
        let mut sub = fx.events.subscribe(512);
        let outcome = fx.invoke(&session).await;
        let published = drain(&mut sub);

        if ran {
            outcome.unwrap_or_else(|e| panic!("{tag}: the turn should have run: {e:?}"));
            assert_eq!(
                provider.request_count(),
                1,
                "{tag}: the turn was dispatched"
            );
            assert_eq!(accepted(&published).len(), 1, "{tag}: recorded as accepted");
        } else {
            let err = outcome.err().unwrap_or_else(|| {
                panic!("{tag}: this cell does not run the turn, and it returned Ok")
            });
            assert_eq!(err.code, error_code::SKILL_EXPANSION_TOO_LARGE, "{tag}");
            assert_eq!(
                provider.request_count(),
                0,
                "{tag}: a cell that does not proceed reached a provider"
            );
            assert!(accepted(&published).is_empty(), "{tag}");
        }

        let after = fx.config_on_disk();
        let applied = remedies(&published);
        if wrote {
            assert_ne!(
                after, before,
                "{tag}: the durable half was answered yes and the file did not move"
            );
            assert!(
                !after.contains("context_budget_cap = 6000"),
                "{tag}: the cap the user asked to clear is still on disk: {after}"
            );
            // Read it back through a parse as well as a string search
            // (`config_preservation.rs`'s double-check).
            let reparsed = teton_core::Config::load(&after)
                .unwrap_or_else(|e| panic!("{tag}: the written config no longer loads: {e}"));
            assert!(
                reparsed
                    .providers
                    .iter()
                    .any(|p| p.id == "frontier" && p.capabilities.context_budget_cap == 0),
                "{tag}: the written document must parse back to a cleared cap"
            );
            assert_eq!(applied.len(), 1, "{tag}: one durable write, one record");
            assert_eq!(applied[0].remedy_kind, RemedyKind::RaiseCap, "{tag}");
            assert_eq!(applied[0].previous_value, "6000", "{tag}");
            assert_eq!(applied[0].new_value, "0 (no cap)", "{tag}");
        } else {
            assert_eq!(
                after, before,
                "{tag}: an answer that took no remedy still wrote to the config"
            );
            assert!(
                applied.is_empty(),
                "{tag}: a durable write was recorded that never happened: {applied:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// AC-10 — nothing is remembered
// ---------------------------------------------------------------------------

/// **AC-10 — accepting twice in one session prompts twice, and no grant survives
/// the invocation** (BR-10).
///
/// BR-10's non-persistence is two guards, not one: nothing is written, **and**
/// nothing already written is read. Both are visible from here — the third
/// invocation changes the answer and is refused, which a remembered "yes" would
/// have settled before the client was ever shown the question.
///
/// The counts are equalities, so a build that stopped asking fails exactly as a
/// build that asked once and remembered would.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepting_twice_asks_twice_and_no_grant_survives_the_invocation() {
    let provider = vendor();
    let fx = Fixture::new(
        Spec::new(
            "ac10",
            remote_route(&provider.openai_endpoint(), UNRECOGNIZED_MODEL, None, None),
            sized_body(6_000, 24_000),
        )
        .answering(Answer::Select(OPTION_ID_OVER_BUDGET_PROCEED_ONCE)),
    );
    let session = fx.session();
    let mut sub = fx.events.subscribe(512);

    fx.invoke(&session).await.expect("the first accept runs");
    assert_eq!(fx.client.offers().len(), 1, "the first invocation asked");

    fx.invoke(&session).await.expect("the second accept runs");
    assert_eq!(
        fx.client.offers().len(),
        2,
        "the second invocation in the same session must ask again — there is no \
         \"don't ask me again\" for the override, and the fix for being asked \
         repeatedly is the remedy beside it"
    );
    assert_eq!(
        offered(&drain(&mut sub)).len(),
        2,
        "and both questions were recorded"
    );
    assert_eq!(provider.request_count(), 2, "both turns were dispatched");

    // The read half of BR-10: a third invocation, answered differently, is
    // refused. A grant remembered from either yes would have settled it.
    fx.client
        .answers(Answer::Select(OPTION_ID_OVER_BUDGET_DECLINE));
    let err = fx
        .invoke(&session)
        .await
        .expect_err("the third answer decides the third turn");
    assert_eq!(err.code, error_code::SKILL_EXPANSION_TOO_LARGE);
    assert_eq!(fx.client.offers().len(), 3, "and it was put to the client");
    assert_eq!(
        provider.request_count(),
        2,
        "the declined third turn reached no provider"
    );
}

// ---------------------------------------------------------------------------
// AC-11 — the accepted path is not the refusal
// ---------------------------------------------------------------------------

/// **AC-11 — the accepted path never emits "no provider saw this turn"** (BR-5).
///
/// That clause is what makes `-32023` different from `-32022`, and it becomes
/// false the moment a human proceeds. Asserted **negatively**, over everything
/// the accepted turn surfaced — the RPC result and every event it published —
/// and paired with the declined run of the same fixture, where the clause is
/// present. Without the pair, "the clause was absent" is equally consistent with
/// a build that stopped composing sentences at all.
///
/// What this cannot cover: `OverBudgetOffer::accepted_record` is written to the
/// daemon's stderr record channel and has no wire surface (TASK-247 flagged the
/// gap). Give it one and this assertion should extend to it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_accepted_path_never_says_no_provider_saw_this_turn() {
    let provider = vendor();
    let fx = Fixture::new(Spec::new(
        "ac11",
        remote_route(&provider.openai_endpoint(), UNRECOGNIZED_MODEL, None, None),
        sized_body(6_000, 24_000),
    ));

    // The pair: the same fixture, declined, where the clause must appear.
    let declined = fx.session();
    let refusal = fx.invoke(&declined).await.expect_err("declined");
    assert!(
        refusal.message.contains(NOTHING_WAS_SENT),
        "control: a decline is today's refusal and carries the clause: {}",
        refusal.message
    );

    fx.client
        .answers(Answer::Select(OPTION_ID_OVER_BUDGET_PROCEED_ONCE));
    let sent = fx.session();
    let mut sub = fx.events.subscribe(512);
    fx.invoke(&sent).await.expect("accepting dispatches");
    let published = drain(&mut sub);
    assert_eq!(accepted(&published).len(), 1, "the turn was accepted");

    for event in &published {
        let rendered = serde_json::to_string(event).expect("every event serializes");
        assert!(
            !rendered.contains("no provider saw this turn"),
            "an accepted turn published the refusal's own clause on `{}`: {rendered}",
            event.name()
        );
    }
    // And the question itself does not pre-empt its own answer with it.
    let offer = subject_of(&fx.client.offers()[1]);
    assert!(
        !offer.sentence.contains("no provider saw this turn"),
        "the offer is a question about a send that has not happened yet, not a \
         report that nothing was sent: {}",
        offer.sentence
    );
}

// ---------------------------------------------------------------------------
// AC-9 — the trust question comes first
// ---------------------------------------------------------------------------

/// **AC-9 / BR-6 / D-10 — a project-sourced skill's trust question is put
/// *before* its budget question, and a declined acknowledgment ends the turn
/// before the budget question is ever put.**
///
/// BR-6's whole content is an ordering, and D-10 says why it is the order it is:
/// asking the budget question first "would have a user authorize an over-budget
/// send of bytes from a repository they have not yet said they trust — a file on
/// disk would be choosing when it gets a consent prompt."
///
/// # Why this is asserted through [`Client::asked`] and not [`Client::offers`]
///
/// Nothing else in this file states an order. [`Client::deliver`] dispatches on
/// the request's **type**: any `SkillOverBudget` goes to the offer branch and
/// anything else to the acknowledgment branch, so it answers both questions
/// correctly whichever one arrives first — and `offers()`, the reader every
/// other test here goes through, filters the acknowledgment out entirely. Put
/// Stage A above the trust gate and the only other test that notices is AC-18's
/// trust-declined row, which fails on a side effect (an offer reached a client
/// that should have seen none) rather than on the rule; the order itself is a
/// fact only the raw log holds.
///
/// # The two legs
///
/// **Acknowledged** — both questions are put, and the raw log's order is the
/// assertion. **Declined** — the trust refusal wins and the budget question is
/// never put at all, which is BR-6's last sentence and the half that actually
/// protects the user. That leg's client is set to *accept* an over-budget send
/// (`over_budget_proceed_once`), so the absence of an offer is a statement about
/// the daemon rather than about a fixture that would have refused anyway: under
/// the inverted order this client authorizes exactly the send D-10 forbids.
///
/// **Mutation:** move `accept_invocation`'s `SkillSource::Project` trust block
/// below Stage A's `offer_or_refuse_over_budget` call in `run_prompt_turn`, and
/// both legs fail — the first on the order, the second on an offer that was put
/// to a repository the session went on to refuse.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_project_skills_trust_question_is_put_before_its_budget_question() {
    // -- leg 1: acknowledged — both questions, in BR-6's order ---------------
    let fx = Fixture::new(Spec::new("ac9ord", local_route(), over_the_local_pair()));
    let session = fx.session();
    let refusal = fx
        .invoke(&session)
        .await
        .expect_err("the offer is declined, so the turn is refused");
    assert_eq!(
        refusal.code,
        error_code::SKILL_EXPANSION_TOO_LARGE,
        "leg 1 must reach the budget door for its order to mean anything: {refusal:?}"
    );
    assert_eq!(
        questions(&fx.client),
        ["project trust", "over-budget offer"],
        "BR-6/D-10: the repository is acknowledged before an over-budget send \
         from it is authorized — a user asked the other way round would be \
         approving bytes from a repository they had not yet said they trust"
    );
    match fx.client.asked()[0].subject.clone() {
        Some(PermissionSubject::ProjectSkillTrust { root, skills, .. }) => {
            let dir = fx
                .tree
                .path()
                .file_name()
                .expect("the fixture tree has a name")
                .to_string_lossy()
                .into_owned();
            assert!(
                root.contains(&dir),
                "the first question names the repository the skill came from: \
                 `{root}` does not name `{dir}`"
            );
            assert!(
                skills.iter().any(|entry| entry.name == SKILL),
                "and covers the skill that was typed: {skills:?}"
            );
        }
        other => panic!("the first question is the acknowledgment: {other:?}"),
    }

    // -- leg 2: declined — the trust refusal wins, and there is no offer ------
    let declined = Fixture::new(
        Spec::new("ac9dec", local_route(), over_the_local_pair())
            .declining_trust()
            .answering(Answer::Select(OPTION_ID_OVER_BUDGET_PROCEED_ONCE)),
    );
    let session = declined.session();
    let mut sub = declined.events.subscribe(512);
    let refusal = declined
        .invoke(&session)
        .await
        .expect_err("a declined acknowledgment refuses the turn");
    let published = drain(&mut sub);
    assert_eq!(
        questions(&declined.client),
        ["project trust"],
        "the budget question was put to a session that had just refused to trust \
         the repository the bytes come from"
    );
    assert_eq!(
        refusal.code,
        error_code::CONSENT_DENIED,
        "declining trust is a consent denial, not a budget refusal: {refusal:?}"
    );
    assert!(
        refusal
            .message
            .contains("has not acknowledged: you declined it"),
        "and it is the trust sentence that is returned: {}",
        refusal.message
    );
    for clause in EVERY_VERDICT_CLAUSE {
        assert!(
            !refusal.message.contains(clause),
            "a trust refusal must not carry a budget verdict — the turn never \
             reached the measurement: {}",
            refusal.message
        );
    }
    for closing in [CLOSING_WITH_REMEDY, CLOSING_ONE_TIME_ONLY] {
        assert!(
            !refusal.message.contains(closing),
            "nor a question, having asked none: {}",
            refusal.message
        );
    }
    assert!(
        offered(&published).is_empty(),
        "no offer may be recorded on a turn that was refused above Stage A: {:?}",
        published.iter().map(Event::name).collect::<Vec<_>>()
    );
}

/// **AC-9's other half — a user-authored skill raises no acknowledgment at all,
/// and its only question is the budget one** (BR-6: "for a **user-authored**
/// skill the current order stands").
///
/// The negative that gives the test above its meaning: without it, a daemon that
/// asked the trust question for *every* skill would satisfy the ordering while
/// putting a repository-trust prompt in front of a file the user wrote
/// themselves — BR-4's question is about repository text, and a user's own
/// `~/.claude/skills` is not that.
///
/// Same route, same body, same over-budget measurement; one field changed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_user_authored_skill_is_asked_only_the_budget_question() {
    let fx = Fixture::new(Spec::new("ac9usr", local_route(), over_the_local_pair()).user_sourced());
    let session = fx.session();
    let refusal = fx
        .invoke(&session)
        .await
        .expect_err("the offer is declined, so the turn is refused");
    assert_eq!(
        refusal.code,
        error_code::SKILL_EXPANSION_TOO_LARGE,
        "the same budget door the project fixture reaches: {refusal:?}"
    );
    assert_eq!(
        questions(&fx.client),
        ["over-budget offer"],
        "a skill the user installed themselves raises no repository-trust \
         question — only the budget one"
    );
}

// ---------------------------------------------------------------------------
// AC-18 — BR-11, on every not-sent path
// ---------------------------------------------------------------------------

/// One way an over-budget turn can end without being sent, and the two facts
/// BR-11's record keeps apart.
///
/// "The question was put" and "a human saw it" are **not** the same fact.
/// `skill_over_budget_offered` is published when the daemon puts the question,
/// which happens whenever there is a connection to address it to — even where
/// no delivery route can carry it there. Only the `invoker: None` arm and a
/// trust refusal above Stage A publish nothing, because on those the question is
/// never put at all. REQ-585 AC-9's "nobody was asked and nobody decided"
/// distinction lives exactly here.
struct NotSentLeg {
    /// How this path is named in an assertion message.
    leg: &'static str,
    /// The fixture, over the route document handed in.
    build: fn(String) -> Spec,
    /// The daemon put the question — `skill_over_budget_offered` is published.
    question_raised: bool,
    /// A human saw it — the client's delivery route carried it there.
    client_saw_the_question: bool,
}

/// **AC-18 — every not-sent path reaches no provider, emits no
/// `context_pressure`, degrades nothing, and does not spend the session's naming
/// duty** (BR-11).
///
/// This is the invariant that makes the refusal `-32023` rather than `-32022`.
/// Without a test it is a comment.
///
/// Four not-sent paths, all driven from real turns on the same route shape:
/// **declined**, **unanswerable** (no delivery route), **never offered** (no
/// connection to address), and **trust-declined** (ADR-10's acknowledgment
/// refused above Stage A, so no budget question is ever reached).
///
/// The naming duty is checked by **claiming** it. `claim_title` is the
/// synchronous act of spending it, so a `true` afterwards is a race-free proof
/// the turn left it alone — with no dependence on whether a detached task had a
/// chance to run.
///
/// The accepted control runs last, on the same shape, and inverts every clause:
/// the provider was reached and the claim is gone. Without it, none of the
/// assertions above distinguishes a refusal from an instrument that never
/// records.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_not_sent_path_reaches_no_provider_and_spends_nothing() {
    let legs = [
        NotSentLeg {
            leg: "declined",
            build: |cfg| {
                Spec::new("a18dec", cfg, sized_body(6_000, 24_000))
                    .answering(Answer::Select(OPTION_ID_OVER_BUDGET_DECLINE))
            },
            question_raised: true,
            client_saw_the_question: true,
        },
        NotSentLeg {
            leg: "unanswerable",
            build: |cfg| {
                Spec::new("a18una", cfg, sized_body(6_000, 24_000))
                    .user_sourced()
                    .unanswerable()
                    .answering(Answer::Select(OPTION_ID_OVER_BUDGET_PROCEED_ONCE))
            },
            question_raised: true,
            client_saw_the_question: false,
        },
        NotSentLeg {
            leg: "never offered",
            build: |cfg| {
                Spec::new("a18non", cfg, sized_body(6_000, 24_000))
                    .user_sourced()
                    .unconnected()
                    .answering(Answer::Select(OPTION_ID_OVER_BUDGET_PROCEED_ONCE))
            },
            question_raised: false,
            client_saw_the_question: false,
        },
        NotSentLeg {
            leg: "trust declined",
            build: |cfg| {
                Spec::new("a18tru", cfg, sized_body(6_000, 24_000))
                    .declining_trust()
                    .answering(Answer::Select(OPTION_ID_OVER_BUDGET_PROCEED_ONCE))
            },
            question_raised: false,
            client_saw_the_question: false,
        },
    ];

    for NotSentLeg {
        leg,
        build,
        question_raised,
        client_saw_the_question,
    } in legs
    {
        let provider = vendor();
        let fx = Fixture::new(build(remote_route(
            &provider.openai_endpoint(),
            UNRECOGNIZED_MODEL,
            None,
            None,
        )));
        let session = fx.session();
        let mut sub = fx.events.subscribe(512);
        let err = fx
            .invoke(&session)
            .await
            .err()
            .unwrap_or_else(|| panic!("{leg}: a not-sent path must not return Ok"));
        let published = drain(&mut sub);

        assert_eq!(
            provider.request_count(),
            0,
            "{leg}: a turn that was not sent reached a provider — `{}`",
            err.message
        );
        assert!(
            !published
                .iter()
                .any(|e| matches!(e, Event::ContextPressure(_))),
            "{leg}: a refused turn was not clamped, and saying otherwise describes \
             a turn that never ran: {:?}",
            published.iter().map(Event::name).collect::<Vec<_>>()
        );
        assert!(
            !published
                .iter()
                .any(|e| matches!(e, Event::ProviderDegraded(_))),
            "{leg}: nothing failed over, so nothing may say a provider was demoted"
        );
        assert!(
            fx.sessions.claim_title(&session),
            "{leg}: the naming attempt was spent on a turn that never ran — the \
             duty sits below the gate precisely so a refused turn does not pay for \
             it"
        );
        assert_eq!(
            !fx.client.offers().is_empty(),
            client_saw_the_question,
            "{leg}: whether the question reached a human"
        );
        assert_eq!(
            offered(&published).len(),
            usize::from(question_raised),
            "{leg}: a question that was never put must not be recorded as one \
             that was — and one that was put and could not be delivered still \
             happened"
        );
    }

    // The control, on the same shape: accepted.
    let provider = vendor();
    let fx = Fixture::new(
        Spec::new(
            "a18ctl",
            remote_route(&provider.openai_endpoint(), UNRECOGNIZED_MODEL, None, None),
            sized_body(6_000, 24_000),
        )
        .answering(Answer::Select(OPTION_ID_OVER_BUDGET_PROCEED_ONCE)),
    );
    let session = fx.session();
    fx.invoke(&session).await.expect("control: accepted");
    assert_eq!(
        provider.request_count(),
        1,
        "control: the accepted turn did reach the provider"
    );
    assert!(
        !fx.sessions.claim_title(&session),
        "control: a turn that ran spends the naming attempt, so the `true`s above \
         are about the refusals rather than about a duty that never fires"
    );
}
