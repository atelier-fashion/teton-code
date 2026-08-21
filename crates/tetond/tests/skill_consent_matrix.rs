//! REQ-585 acceptance: **who** answered for a skill's dynamic context, and
//! **what** their answer bought (TASK-201).
//!
//! `web_consent_matrix.rs` is the instrument this copies. Both files ask one
//! question — *a grant on key A must not un-ask key B* — of a real
//! [`PermissionGate`], driven by a task that answers its prompts the way a
//! client does. Nothing here is mocked but the two seams the daemon owns: the
//! route an addressed request travels ([`Switchboard`]) and the connections it
//! could travel to.
//!
//! ## AC → test map
//!
//! | AC | Test |
//! |----|------|
//! | AC-8 (one prompt, every command) | [`one_consent_per_invocation_lists_every_command_verbatim_in_document_order`] |
//! | AC-8 (shell ↛ skill) | [`a_prior_shell_allow_always_does_not_answer_a_skill_request`] |
//! | AC-8 (skill ↛ shell) | [`a_skill_allow_always_does_not_answer_a_model_issued_shell_call`] |
//! | AC-8 (skill ↛ skill) | [`a_grant_on_one_skill_does_not_answer_another_skill`] |
//! | ADR-6 (source in the key) | [`a_project_grant_does_not_answer_the_user_skill_of_the_same_name`] |
//! | AC-8 (for this session) | [`allow_for_this_session_lasts_to_session_end_and_not_beyond`] |
//! | ADR-7 (addressed delivery) | [`an_unaddressed_attached_connection_never_receives_the_skill_request`] |
//! | ADR-7 (only the addressee answers) | [`an_answer_from_an_unaddressed_connection_is_refused`] |
//! | ADR-7 (no route ⇒ nobody asked) | [`a_gate_with_no_route_to_the_asking_connection_asks_no_one`] |
//! | AC-9 (level posture) | [`the_level_default_governs_a_skill_key_at_every_level`] |
//! | AC-9 (no human could be asked) | [`a_client_refusal_reaches_the_caller_as_a_reason_not_a_decline`] |
//! | ADR-6 (`/cd` drops project grants) | [`a_project_grant_is_dropped_when_the_session_root_moves`] |
//! | ADR-6 (four options, not five) | [`a_skill_prompt_offers_the_standard_four_and_never_the_permanent_one`] |
//!
//! ## REQ-587 — the third door, and a grant key that follows its arguments
//!
//! TASK-215 extends this instrument rather than starting a second one, because
//! the claims are the same claims: *an answer to one question must not answer
//! another*. The three questions the daemon can now ask about a skill —
//! `shell`, the skill's dynamic context, and the project-skill acknowledgment —
//! are pairwise isolated, and the digest makes the middle one finer still.
//!
//! | AC | Test |
//! |----|------|
//! | BR-4 (the door asks under its own key) | [`the_acknowledgment_asks_under_its_own_key_and_names_the_root_and_its_skills`] |
//! | BR-4 (level posture) | [`the_level_default_governs_the_acknowledgment_at_every_level`] |
//! | BR-4 (shadowing asks even at `full`) | [`a_shadowing_project_skill_is_acknowledged_even_at_full`] |
//! | BR-4 (bounded list) | [`the_acknowledgment_names_twenty_skills_and_counts_the_rest`] |
//! | BR-4 (per root) | [`an_acknowledgment_for_one_root_does_not_answer_another_root`] |
//! | BR-4 (isolation) | [`a_shell_grant_does_not_answer_the_acknowledgment`], [`an_acknowledgment_does_not_answer_a_skills_dynamic_context`] |
//! | ASSUME-017 (`/cd`) | [`the_acknowledgment_is_dropped_when_the_session_root_moves`] |
//! | BR-5 (the digest) | [`a_grant_for_one_argument_set_does_not_answer_another`] |
//! | BR-5 (who asked) | [`the_consent_reports_the_caller_that_invoked_the_skill`] |
//!
//! ## Mutation table (TASK-201)
//!
//! | Mutation | Test that fails |
//! |----------|-----------------|
//! | ask under `shell` instead of the skill's key | [`a_prior_shell_allow_always_does_not_answer_a_skill_request`], [`a_skill_allow_always_does_not_answer_a_model_issued_shell_call`] |
//! | drop the source from the key | [`a_project_grant_does_not_answer_the_user_skill_of_the_same_name`] |
//! | skip `drop_project_skill_grants` | [`a_project_grant_is_dropped_when_the_session_root_moves`] |
//! | publish the request instead of addressing it | [`an_unaddressed_attached_connection_never_receives_the_skill_request`] |
//! | let any connection answer | [`an_answer_from_an_unaddressed_connection_is_refused`] |
//! | fold a client refusal into a decline | [`a_client_refusal_reaches_the_caller_as_a_reason_not_a_decline`] |
//! | ask once per command | [`one_consent_per_invocation_lists_every_command_verbatim_in_document_order`] |
//!
//! ## Mutation table (TASK-215)
//!
//! | Mutation | Test that fails |
//! |----------|-----------------|
//! | reuse `authorize_skill` for the acknowledgment | [`the_acknowledgment_asks_under_its_own_key_and_names_the_root_and_its_skills`] (and `permissions.rs`'s `the_skill_door_refuses_the_project_acknowledgment_key`) |
//! | drop the digest from the grant key | [`a_grant_for_one_argument_set_does_not_answer_another`] |
//! | skip the `/cd` expiry of the acknowledgment | [`the_acknowledgment_is_dropped_when_the_session_root_moves`] |
//! | let the level's `allow` settle a shadowing acknowledgment | [`a_shadowing_project_skill_is_acknowledged_even_at_full`] |
//! | let a shadowing acknowledgment override `plan`'s `deny` | [`a_shadowing_project_skill_is_acknowledged_even_at_full`] |
//! | key the acknowledgment on something other than the root | [`an_acknowledgment_for_one_root_does_not_answer_another_root`] |
//! | list the project's skills unbounded | [`the_acknowledgment_names_twenty_skills_and_counts_the_rest`] |
//! | report every consent as the user's | [`the_consent_reports_the_caller_that_invoked_the_skill`] |
//!
//! ## What this file cannot prove (TASK-215)
//!
//! It **invents** a [`ConnectionId`] and hands it to the gate, so every test
//! here passes whether or not production has one to pass. That is the right
//! scope for a gate matrix — the gate is what is under test — and it is exactly
//! why TASK-217 asserts the addressee separately, from inside the turn loop. A
//! green run here is not evidence that a model-issued invocation can reach an
//! addressable connection.
//!
//! ## Falsification (LESSON-479)
//!
//! Every "it asked again" below is paired, in the same test, with the run that
//! must *not* ask — the same gate, the same key, one thing changed. Without the
//! pair, "a prompt appeared" is equally consistent with a gate that has stopped
//! remembering anything at all.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use teton_protocol::events::{
    Event, InvokedBy, PermissionOption, PermissionRequest, PermissionSubject,
    ProjectSkillTrustEntry, OPTION_ID_ENABLE_PERMANENT,
};
use teton_protocol::methods::{project_skill_trust_key, PermissionOutcome, RefusalReason};
use teton_protocol::permissions::PermissionLevel;
use teton_protocol::SessionId;

use tetond::broadcast::{EventBus, Subscription};
use tetond::grants::{ConnectionId, GrantRegistry};
use tetond::harness::permissions::{
    skill_grant_key, AddressedPermissionDelivery, ArgumentInterpolation, PendingPermissions,
    PermissionDecision, PermissionGate, SkillConsent,
};
use tetond::skills::{permission_key_for, SkillSource};

/// Long enough that a loaded CI box is not the reason a prompt is late, short
/// enough that a prompt which never comes fails instead of hanging.
///
/// The bound is load-bearing for the same reason `permissions.rs`'s
/// `answer_next` says its is, and this file found it the same way. Every
/// regression these tests guard shows up as a prompt that is never raised or
/// never delivered, so the consent `await` never returns and an unbounded test
/// **hangs**. Verified by mutation: routing the addressed request onto the bus
/// instead — the exact defect ADR-7 exists to prevent — turned this suite into a
/// wedged process until [`invoke`] grew this timeout, after which it names the
/// connection that was never asked. A hang reads as infrastructure trouble and
/// gets retried; a failure gets read.
const PROMPT_WAIT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// The two seams
// ---------------------------------------------------------------------------

/// One addressed request, as it left the gate.
#[derive(Debug, Clone)]
struct Routed {
    /// The connection it was addressed to — the assertion ADR-7 is about.
    to: ConnectionId,
    request: PermissionRequest,
}

/// The daemon's connection routing, reduced to what this file must observe:
/// which connections are live, and what was put in front of them.
///
/// It stands in for the outbound frame channels the daemon already routes
/// REQ-569's consent prompts and BUG-177's lifecycle replay through. The
/// property under test is not how a frame reaches a socket; it is that exactly
/// one connection is named, and that the bus — which reaches *every* attached
/// connection — is not used at all.
struct Switchboard {
    live: HashSet<ConnectionId>,
    sent: mpsc::UnboundedSender<Routed>,
}

impl AddressedPermissionDelivery for Switchboard {
    fn deliver(
        &self,
        connection: ConnectionId,
        _session_id: &SessionId,
        request: PermissionRequest,
    ) -> bool {
        if !self.live.contains(&connection) {
            return false;
        }
        self.sent
            .send(Routed {
                to: connection,
                request,
            })
            .is_ok()
    }
}

/// A gate with everything a skill consent needs, plus the two windows onto it:
/// what was routed, and what was broadcast.
struct Session {
    gate: PermissionGate,
    pending: Arc<PendingPermissions>,
    /// Everything the bus carried. A skill request appearing here is the
    /// broadcast defect ADR-7 exists to prevent.
    bus_watch: Subscription,
    routed: mpsc::UnboundedReceiver<Routed>,
}

/// A session at `level` whose skill consents may be addressed to any of `live`.
fn session_at(level: PermissionLevel, id: &str, live: &[ConnectionId]) -> Session {
    let bus = Arc::new(EventBus::new());
    let pending = Arc::new(PendingPermissions::new());
    let bus_watch = bus.subscribe(64);
    let (sent, routed) = mpsc::unbounded_channel();
    let board = Arc::new(Switchboard {
        live: live.iter().copied().collect(),
        sent,
    });
    let gate = PermissionGate::with_level(
        SessionId::from(id),
        level,
        Vec::new(),
        Arc::clone(&bus),
        Arc::clone(&pending),
    )
    .with_addressed_delivery(board);
    Session {
        gate,
        pending,
        bus_watch,
        routed,
    }
}

/// A session with **no** addressed-delivery route wired — the shape of a gate
/// the daemon forgot to wire, and the one this file asserts asks nobody.
fn session_with_no_route(id: &str) -> Session {
    let bus = Arc::new(EventBus::new());
    let pending = Arc::new(PendingPermissions::new());
    let bus_watch = bus.subscribe(64);
    let (_sent, routed) = mpsc::unbounded_channel();
    let gate = PermissionGate::with_level(
        SessionId::from(id),
        PermissionLevel::Guarded,
        Vec::new(),
        Arc::clone(&bus),
        Arc::clone(&pending),
    );
    Session {
        gate,
        pending,
        bus_watch,
        routed,
    }
}

/// Mint `n` connection ids from one namespace, so no two of them collide.
fn connections(n: usize) -> Vec<ConnectionId> {
    let registry = GrantRegistry::new();
    (0..n).map(|_| registry.next_connection_id()).collect()
}

// ---------------------------------------------------------------------------
// Answering, the way a client does
// ---------------------------------------------------------------------------

/// Answers every addressed prompt **as the connection it was addressed to**,
/// from a script, and records what it saw.
///
/// When the script runs out it dismisses the prompt. That is deliberate: the
/// tests below count prompts, and a prompt nobody answers is a hang rather than
/// a count — the failure mode `PROMPT_WAIT` exists to avoid, reached from the
/// other side.
struct Answerer {
    seen: Arc<Mutex<Vec<Routed>>>,
    handle: JoinHandle<()>,
}

impl Answerer {
    fn spawn(
        mut routed: mpsc::UnboundedReceiver<Routed>,
        pending: Arc<PendingPermissions>,
        script: Vec<PermissionOutcome>,
    ) -> Self {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&seen);
        let mut script: VecDeque<PermissionOutcome> = script.into();
        let handle = tokio::spawn(async move {
            while let Some(prompt) = routed.recv().await {
                recorded
                    .lock()
                    .expect("answerer lock poisoned")
                    .push(prompt.clone());
                let outcome = script.pop_front().unwrap_or(PermissionOutcome::Cancelled);
                pending.resolve_from(&prompt.request.request_id, outcome, prompt.to);
            }
        });
        Self { seen, handle }
    }

    fn count(&self) -> usize {
        self.seen.lock().expect("answerer lock poisoned").len()
    }

    fn prompts(&self) -> Vec<Routed> {
        self.seen.lock().expect("answerer lock poisoned").clone()
    }

    fn stop(self) {
        self.handle.abort();
    }
}

/// Answers the next **broadcast** prompt — a model-issued tool call — with
/// `option_id`, and counts them.
///
/// The other half of the isolation matrix needs this: two of the claims below
/// are about a `shell` call, and `shell` still goes on the bus.
struct BusAnswerer {
    count: Arc<Mutex<usize>>,
    handle: JoinHandle<()>,
}

impl BusAnswerer {
    fn spawn(mut sub: Subscription, pending: Arc<PendingPermissions>, option_id: &str) -> Self {
        let count = Arc::new(Mutex::new(0));
        let counted = Arc::clone(&count);
        let option_id = option_id.to_owned();
        let handle = tokio::spawn(async move {
            while let Some(env) = sub.recv().await {
                if let Event::PermissionRequest(request) = env.event {
                    *counted.lock().expect("bus answerer lock poisoned") += 1;
                    pending.resolve(
                        &request.request_id,
                        PermissionOutcome::Selected {
                            option_id: option_id.clone(),
                        },
                    );
                }
            }
        });
        Self { count, handle }
    }

    fn count(&self) -> usize {
        *self.count.lock().expect("bus answerer lock poisoned")
    }

    fn stop(self) {
        self.handle.abort();
    }
}

fn selected(option_id: &str) -> PermissionOutcome {
    PermissionOutcome::Selected {
        option_id: option_id.to_owned(),
    }
}

/// Invoke `skill`'s dynamic context from `from`, under the key its own name and
/// source mint.
///
/// The key is taken from [`permission_key_for`] rather than spelled here, so a
/// change to the key's shape moves this file's assertions with it — and a
/// mutation that *drops* a component of the key changes what these tests
/// compare, which is exactly how the isolation claims catch it.
///
/// Bounded by [`PROMPT_WAIT`], because a consent nobody is asked never returns.
async fn invoke(
    gate: &PermissionGate,
    skill: &str,
    source: SkillSource,
    commands: &[&str],
    from: ConnectionId,
) -> SkillConsent {
    invoke_as(
        gate,
        skill,
        source,
        commands,
        ArgumentInterpolation::None,
        InvokedBy::User,
        from,
    )
    .await
}

/// [`invoke`] with the two facts REQ-587 added: whether the body interpolated
/// its arguments (which decides the grant key's spelling, BR-5) and who asked
/// (which the consent must report, BR-5).
///
/// The key still comes from the minter rather than being spelled here, for
/// [`invoke`]'s reason — a mutation that drops the digest changes what these
/// tests compare, which is how the isolation claims catch it.
async fn invoke_as(
    gate: &PermissionGate,
    skill: &str,
    source: SkillSource,
    commands: &[&str],
    interpolation: ArgumentInterpolation,
    invoked_by: InvokedBy,
    from: ConnectionId,
) -> SkillConsent {
    let commands: Vec<String> = commands.iter().map(|c| (*c).to_owned()).collect();
    let key = skill_grant_key(source, skill, &commands, interpolation);
    tokio::time::timeout(
        PROMPT_WAIT,
        gate.authorize_skill(&key, skill, source, commands, invoked_by, from),
    )
    .await
    .unwrap_or_else(|_| {
        panic!(
            "`/{skill}` ({source:?}) was never answered within {PROMPT_WAIT:?} — \
             the prompt was not delivered to the connection that asked, or was \
             delivered somewhere this test cannot see (ADR-7)"
        )
    })
}

/// Raise BR-4's project-skill acknowledgment for `root`, from `from`.
///
/// The key comes from [`project_skill_trust_key`], never spelled here: a
/// mutation that reuses a skill's key changes what the gate remembers, and the
/// isolation legs below are what catch it.
async fn acknowledge(
    gate: &PermissionGate,
    root: &str,
    skills: &[ProjectSkillTrustEntry],
    shadows_user_skill: bool,
    from: ConnectionId,
) -> SkillConsent {
    tokio::time::timeout(
        PROMPT_WAIT,
        gate.authorize_project_skill_trust(
            &project_skill_trust_key(root),
            root,
            skills,
            shadows_user_skill,
            from,
        ),
    )
    .await
    .unwrap_or_else(|_| {
        panic!(
            "the project-skill acknowledgment for `{root}` was never answered \
             within {PROMPT_WAIT:?} — the prompt was not delivered to the \
             connection that asked (ADR-7)"
        )
    })
}

/// One model-invocable project skill, as the acknowledgment lists it.
fn entry(name: &str, shadows_user_skill: bool) -> ProjectSkillTrustEntry {
    ProjectSkillTrustEntry {
        name: name.to_owned(),
        shadows_user_skill,
    }
}

/// Assert the bus never carried a permission request. The bus is how *every*
/// attached connection receives events, so this is the whole of "an unaddressed
/// connection never received it" (ADR-7).
fn assert_nothing_was_broadcast(watch: &mut Subscription, why: &str) {
    while let Some(env) = watch.try_recv() {
        assert!(
            !matches!(env.event, Event::PermissionRequest(_)),
            "a skill consent reached the bus, where every attached connection \
             sees it: {why}"
        );
    }
}

// ---------------------------------------------------------------------------
// AC-8 — one prompt, every command
// ---------------------------------------------------------------------------

/// **AC-8 / ADR-7: one consent per invocation, listing every command verbatim
/// in document order.**
///
/// The count is the point. A prompt per command is REQ-560 BR-2's named
/// anti-pattern, and it is why the commands ride the subject as a *list*:
/// `Surface::line` destroys newlines, so a one-line description could not carry
/// three commands and an implementation that tried would reach for three
/// prompts instead.
///
/// The verbatim half is asserted against the exact strings handed in, including
/// the one carrying a newline and the one carrying a quote — a daemon that
/// "tidied" a command would be showing the user something other than what runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_consent_per_invocation_lists_every_command_verbatim_in_document_order() {
    let conns = connections(1);
    let Session {
        gate,
        pending,
        mut bus_watch,
        routed,
    } = session_at(PermissionLevel::Guarded, "one-prompt", &conns);
    let answerer = Answerer::spawn(routed, Arc::clone(&pending), vec![selected("allow_once")]);

    let commands = [
        "git status --short",
        "echo \"two\nlines\"",
        "date -u +%Y-%m-%d",
    ];
    let consent = invoke(&gate, "status", SkillSource::User, &commands, conns[0]).await;

    assert_eq!(consent, SkillConsent::Allowed);
    assert_eq!(answerer.count(), 1, "three commands, one question");

    let prompt = answerer.prompts().remove(0);
    assert_eq!(
        prompt.request.tool_name,
        permission_key_for(SkillSource::User, "status"),
        "the question is asked under the skill's own key"
    );
    match prompt.request.subject {
        Some(PermissionSubject::SkillDynamicContext {
            skill,
            source,
            commands: asked,
            invoked_by,
        }) => {
            assert_eq!(skill, "status");
            assert_eq!(source, SkillSource::User);
            assert_eq!(
                invoked_by,
                InvokedBy::User,
                "this is a user-typed invocation; a consent that reported the \
                 model here would be attributing the ask to the wrong caller"
            );
            assert_eq!(
                asked,
                commands.iter().map(|c| (*c).to_owned()).collect::<Vec<_>>(),
                "every command, verbatim, in document order"
            );
        }
        other => panic!(
            "a client must be able to recognize this request without parsing \
             the key (BR-11); subject was {other:?}"
        ),
    }
    assert_nothing_was_broadcast(&mut bus_watch, "the invocation's own prompt");
    answerer.stop();
}

/// **ADR-6: the standard four options, and never the fifth.**
///
/// `enable_permanent` is web-only because a tier is the one thing a consent
/// answer can write down. There is no `[skills] tier`, and an "always" over
/// file-supplied shell commands in a file re-read every session would be a far
/// larger promise than the prompt makes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_skill_prompt_offers_the_standard_four_and_never_the_permanent_one() {
    let conns = connections(1);
    let Session {
        gate,
        pending,
        bus_watch: _bus_watch,
        routed,
    } = session_at(PermissionLevel::Guarded, "four-options", &conns);
    let answerer = Answerer::spawn(routed, Arc::clone(&pending), vec![selected("allow_once")]);

    invoke(
        &gate,
        "status",
        SkillSource::User,
        &["git status"],
        conns[0],
    )
    .await;

    let options: Vec<PermissionOption> = answerer.prompts().remove(0).request.options;
    let ids: Vec<&str> = options.iter().map(|o| o.option_id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["allow_once", "allow_always", "reject_once", "reject_always"],
        "the standard four, in the standard order"
    );
    assert!(
        !ids.contains(&OPTION_ID_ENABLE_PERMANENT),
        "there is no `[skills] tier` for a permanent answer to write"
    );
    answerer.stop();
}

// ---------------------------------------------------------------------------
// Grant isolation — a grant on key A does not un-ask key B
// ---------------------------------------------------------------------------

/// **AC-8 / LESSON-495: a prior `shell` allow-always does not answer a skill
/// request.**
///
/// The mutation this catches is asking under `shell`. A user who said "yes, run
/// shell commands for this session" said it about the *model's* calls; a skill
/// file's `` !`command` `` slots are a different question with different text
/// under them, and a remembered answer is attached to its key rather than to
/// the sentence that produced it.
///
/// Falsification leg: the same gate, the same key, after a skill allow-always —
/// the second skill invocation must *not* ask, or "it asked" would be equally
/// consistent with a gate that had stopped remembering anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_prior_shell_allow_always_does_not_answer_a_skill_request() {
    let conns = connections(1);
    let Session {
        gate,
        pending,
        bus_watch,
        routed,
    } = session_at(PermissionLevel::Guarded, "shell-then-skill", &conns);
    let bus_answerer = BusAnswerer::spawn(bus_watch, Arc::clone(&pending), "allow_always");
    let skill_answerer = Answerer::spawn(
        routed,
        Arc::clone(&pending),
        vec![selected("allow_always"), selected("allow_once")],
    );

    // The user allows shell for the session — the model's calls.
    assert_eq!(
        gate.authorize("shell", None).await,
        PermissionDecision::Allowed
    );
    assert_eq!(bus_answerer.count(), 1);
    // ...and a second model-issued shell call is genuinely covered by it, so
    // "the skill still asked" below cannot be a gate that remembers nothing.
    assert_eq!(
        gate.authorize("shell", None).await,
        PermissionDecision::Allowed
    );
    assert_eq!(bus_answerer.count(), 1, "the shell grant was remembered");

    // The skill is asked about anyway.
    assert_eq!(
        invoke(
            &gate,
            "status",
            SkillSource::User,
            &["rm -rf /tmp/x"],
            conns[0]
        )
        .await,
        SkillConsent::Allowed
    );
    assert_eq!(
        skill_answerer.count(),
        1,
        "a shell allow-always must not un-ask a skill's dynamic context"
    );

    // Falsification: the skill's *own* allow-always does cover its next
    // invocation.
    assert_eq!(
        invoke(
            &gate,
            "status",
            SkillSource::User,
            &["rm -rf /tmp/x"],
            conns[0]
        )
        .await,
        SkillConsent::Allowed
    );
    assert_eq!(
        skill_answerer.count(),
        1,
        "the skill's own grant must answer its own next invocation"
    );

    skill_answerer.stop();
    bus_answerer.stop();
}

/// **AC-8, the other direction: a skill allow-always does not answer a
/// model-issued `shell` call.**
///
/// This is the half that costs the most if it is wrong. "Yes, run `/deploy`'s
/// three commands" would become "yes, run whatever the model asks a shell to
/// do, for the rest of the session".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_skill_allow_always_does_not_answer_a_model_issued_shell_call() {
    let conns = connections(1);
    let Session {
        gate,
        pending,
        bus_watch,
        routed,
    } = session_at(PermissionLevel::Guarded, "skill-then-shell", &conns);
    let bus_answerer = BusAnswerer::spawn(bus_watch, Arc::clone(&pending), "reject_once");
    let skill_answerer =
        Answerer::spawn(routed, Arc::clone(&pending), vec![selected("allow_always")]);

    assert_eq!(
        invoke(
            &gate,
            "deploy",
            SkillSource::User,
            &["./deploy.sh"],
            conns[0]
        )
        .await,
        SkillConsent::Allowed
    );
    assert_eq!(skill_answerer.count(), 1);

    // The model now asks for a shell. It must be asked about, and the answer
    // scripted here is a refusal — so a leaked grant would show up as an
    // *allow* with no prompt at all.
    assert_eq!(
        gate.authorize("shell", None).await,
        PermissionDecision::Denied,
        "a skill grant must not authorize the model's shell calls"
    );
    assert_eq!(
        bus_answerer.count(),
        1,
        "the shell call was asked about on its own key"
    );

    skill_answerer.stop();
    bus_answerer.stop();
}

/// **AC-8: an allow-always on one skill does not answer another.**
///
/// One key per skill, because "may `/status` run its commands?" and "may
/// `/canary` run its commands?" are different sentences with different text
/// under them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_grant_on_one_skill_does_not_answer_another_skill() {
    let conns = connections(1);
    let Session {
        gate,
        pending,
        bus_watch: _bus_watch,
        routed,
    } = session_at(PermissionLevel::Guarded, "status-vs-canary", &conns);
    let answerer = Answerer::spawn(
        routed,
        Arc::clone(&pending),
        vec![selected("allow_always"), selected("reject_once")],
    );

    assert_eq!(
        invoke(
            &gate,
            "status",
            SkillSource::User,
            &["git status"],
            conns[0]
        )
        .await,
        SkillConsent::Allowed
    );
    assert_eq!(answerer.count(), 1);

    // A different skill, same source, same session: asked, and the scripted
    // answer is a refusal, so a leaked grant would read as an unprompted allow.
    assert_eq!(
        invoke(
            &gate,
            "canary",
            SkillSource::User,
            &["./canary.sh"],
            conns[0]
        )
        .await,
        SkillConsent::Declined,
        "`skill:user:status` must not answer `skill:user:canary`"
    );
    assert_eq!(answerer.count(), 2);

    // Falsification: `/status` itself is still covered.
    assert_eq!(
        invoke(
            &gate,
            "status",
            SkillSource::User,
            &["git status"],
            conns[0]
        )
        .await,
        SkillConsent::Allowed
    );
    assert_eq!(answerer.count(), 2);

    answerer.stop();
}

/// **ADR-6: the source is half the key.**
///
/// `skill:project:x` and `skill:user:x` are two files. Dropping the source from
/// the key makes them one string, and this test is what notices — the second
/// invocation would be answered by the first's grant and never reach a prompt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_project_grant_does_not_answer_the_user_skill_of_the_same_name() {
    let conns = connections(1);
    let Session {
        gate,
        pending,
        bus_watch: _bus_watch,
        routed,
    } = session_at(PermissionLevel::Guarded, "project-vs-user", &conns);
    let answerer = Answerer::spawn(
        routed,
        Arc::clone(&pending),
        vec![selected("allow_always"), selected("reject_once")],
    );

    assert_eq!(
        invoke(
            &gate,
            "audit",
            SkillSource::Project,
            &["./audit.sh"],
            conns[0]
        )
        .await,
        SkillConsent::Allowed
    );
    assert_eq!(
        invoke(&gate, "audit", SkillSource::User, &["./audit.sh"], conns[0]).await,
        SkillConsent::Declined,
        "same name, different file: `skill:project:audit` must not answer \
         `skill:user:audit`"
    );
    assert_eq!(answerer.count(), 2, "both were asked about");

    answerer.stop();
}

/// **AC-8: "for this session" lasts to session end and not beyond.**
///
/// "Not beyond" is asserted against a second gate — which is what a second
/// session is, since grants live on the gate and nowhere else
/// (`web_consent_matrix.rs:719`'s shape).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn allow_for_this_session_lasts_to_session_end_and_not_beyond() {
    let conns = connections(1);
    let Session {
        gate,
        pending,
        bus_watch: _bus_watch,
        routed,
    } = session_at(PermissionLevel::Guarded, "session-scope", &conns);
    let answerer = Answerer::spawn(routed, Arc::clone(&pending), vec![selected("allow_always")]);

    for _ in 0..3 {
        assert_eq!(
            invoke(
                &gate,
                "status",
                SkillSource::User,
                &["git status"],
                conns[0]
            )
            .await,
            SkillConsent::Allowed
        );
    }
    assert_eq!(answerer.count(), 1, "asked once, honoured three times");
    answerer.stop();

    // A fresh session is a fresh gate, and the question is asked again.
    let next = session_at(PermissionLevel::Guarded, "session-scope-next", &conns);
    let next_answerer = Answerer::spawn(
        next.routed,
        Arc::clone(&next.pending),
        vec![selected("reject_once")],
    );
    assert_eq!(
        invoke(
            &next.gate,
            "status",
            SkillSource::User,
            &["git status"],
            conns[0]
        )
        .await,
        SkillConsent::Declined,
        "a session grant must not survive the session"
    );
    assert_eq!(next_answerer.count(), 1);
    next_answerer.stop();
}

// ---------------------------------------------------------------------------
// ADR-7 — the request is addressed, and only the addressee may answer
// ---------------------------------------------------------------------------

/// **ADR-7: an unaddressed attached connection never receives the request.**
///
/// Two clients attached to one session is a consented topology (REQ-570), and
/// `permission_request` today reaches every one of them. A pre-REQ-585 client
/// among them would see a request carrying a subject it has never heard of, fall
/// through to `prompter.ask`, and on a pipe read the user's next stdin line as
/// the answer — turning a pasted `y` into consent for shell commands. So the
/// claim is not "the right client was preferred"; it is that the request went to
/// exactly one connection and **never onto the bus**, which is the only
/// mechanism the other connection has for receiving it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unaddressed_attached_connection_never_receives_the_skill_request() {
    let conns = connections(2);
    let (invoker, bystander) = (conns[0], conns[1]);
    let Session {
        gate,
        pending,
        mut bus_watch,
        routed,
    } = session_at(PermissionLevel::Guarded, "two-clients", &conns);
    let answerer = Answerer::spawn(routed, Arc::clone(&pending), vec![selected("allow_once")]);

    assert_eq!(
        invoke(&gate, "status", SkillSource::User, &["git status"], invoker).await,
        SkillConsent::Allowed
    );

    let prompts = answerer.prompts();
    assert_eq!(prompts.len(), 1, "one delivery, not one per connection");
    assert_eq!(
        prompts[0].to, invoker,
        "the person who typed `/status` is the person asked to approve it"
    );
    assert_ne!(
        prompts[0].to, bystander,
        "the bystander is live and attached, and was still not asked"
    );
    assert_nothing_was_broadcast(
        &mut bus_watch,
        "a bystanding pre-REQ-585 client would have read it",
    );
    answerer.stop();
}

/// **ADR-7: an answer from an unaddressed connection is refused.**
///
/// Delivery and authorization are separate guards, and both are needed: a
/// client that learned the `request_id` some other way — a replayed frame, a
/// monitor, a future protocol addition — must not be able to answer a question
/// it was never asked. The waiter is left standing, so the connection that *was*
/// asked can still answer afterwards, which this test then does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_answer_from_an_unaddressed_connection_is_refused() {
    let conns = connections(2);
    let (invoker, intruder) = (conns[0], conns[1]);
    let Session {
        gate,
        pending,
        bus_watch: _bus_watch,
        mut routed,
    } = session_at(PermissionLevel::Guarded, "wrong-answerer", &conns);

    let answering = Arc::clone(&pending);
    let (consent, prompt) = tokio::join!(
        invoke(
            &gate,
            "deploy",
            SkillSource::User,
            &["./deploy.sh"],
            invoker
        ),
        async move {
            let prompt = tokio::time::timeout(PROMPT_WAIT, routed.recv())
                .await
                .expect("a prompt must be delivered — none arrived within the timeout")
                .expect("the route is open");
            let id = prompt.request.request_id.clone();

            // The intruder tries to allow it for the whole session.
            assert!(
                !answering.resolve_from(&id, selected("allow_always"), intruder),
                "a connection the request was not addressed to must not answer it"
            );
            // So does a caller that cannot name a connection at all — the
            // pre-REQ-585 entry point.
            assert!(
                !answering.resolve(&id, selected("allow_always")),
                "an answer that names no connection cannot be shown to be the \
                 addressee's"
            );
            assert_eq!(
                answering.pending_count(),
                1,
                "a refused answer must leave the prompt standing for whoever \
                 may rightfully answer it"
            );

            // The connection that was actually asked declines.
            assert!(answering.resolve_from(&id, selected("reject_once"), invoker));
            prompt
        }
    );

    assert_eq!(prompt.to, invoker);
    assert_eq!(
        consent,
        SkillConsent::Declined,
        "the addressee's answer is the one that decided it"
    );
    // And the intruder's `allow_always` bought nothing: the next invocation is
    // asked about again.
    assert_eq!(
        gate.remembered(&permission_key_for(SkillSource::User, "deploy")),
        None,
        "a refused answer must not be remembered as a grant"
    );
}

/// **ADR-7: a gate with no route to the asking connection asks nobody.**
///
/// The fallback that must not exist. Publishing the request instead would put
/// it in front of every attached connection, which is the whole defect; so a
/// gate that cannot address it refuses, and says so in a way the placeholder can
/// distinguish from a decline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gate_with_no_route_to_the_asking_connection_asks_no_one() {
    let conns = connections(2);
    let Session {
        gate,
        pending,
        mut bus_watch,
        routed: _routed,
    } = session_with_no_route("no-route");

    assert_eq!(
        invoke(
            &gate,
            "status",
            SkillSource::User,
            &["git status"],
            conns[0]
        )
        .await,
        SkillConsent::Unanswerable
    );
    assert_nothing_was_broadcast(&mut bus_watch, "there is no fallback to the bus");
    assert_eq!(
        pending.pending_count(),
        0,
        "a prompt nobody can answer must not be left parked in the registry"
    );

    // A wired route that does not know the connection is the same answer, and
    // must leave the registry just as clean — the waiter is registered before
    // delivery is attempted, so this is the path that has to undo it.
    let unknown = session_at(PermissionLevel::Guarded, "unknown-conn", &conns[..1]);
    assert_eq!(
        invoke(
            &unknown.gate,
            "status",
            SkillSource::User,
            &["git status"],
            conns[1]
        )
        .await,
        SkillConsent::Unanswerable
    );
    assert_eq!(unknown.pending.pending_count(), 0);
}

// ---------------------------------------------------------------------------
// AC-9 — the level, and the answers that are not decisions
// ---------------------------------------------------------------------------

/// **AC-9 / ADR-6: the key rides the level's default, and `table_for` gains no
/// skill row.**
///
/// `guarded` asks, `edits` asks, `plan` denies without asking, `full` allows
/// without asking. The two silent legs are asserted against the *prompt count*
/// as well as the answer, because "allowed" and "allowed without being asked"
/// are different claims and only the second one is the level's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_level_default_governs_a_skill_key_at_every_level() {
    for (level, want) in [
        (PermissionLevel::Guarded, SkillConsent::Allowed),
        (PermissionLevel::Edits, SkillConsent::Allowed),
        (PermissionLevel::Plan, SkillConsent::DeniedByLevel),
        (PermissionLevel::Full, SkillConsent::Allowed),
    ] {
        let conns = connections(1);
        let Session {
            gate,
            pending,
            bus_watch: _bus_watch,
            routed,
        } = session_at(level, "levels", &conns);
        let answerer = Answerer::spawn(routed, Arc::clone(&pending), vec![selected("allow_once")]);

        let consent = invoke(
            &gate,
            "status",
            SkillSource::User,
            &["git status"],
            conns[0],
        )
        .await;
        assert_eq!(consent, want, "at {level}");

        let asked = matches!(level, PermissionLevel::Guarded | PermissionLevel::Edits);
        assert_eq!(
            answerer.count(),
            usize::from(asked),
            "at {level}: whether a human was asked is the level's answer, not \
             the grant map's"
        );
        // `plan`'s refusal is the level's, and the sentence the placeholder
        // carries comes from the level rather than from a second string.
        if level == PermissionLevel::Plan {
            let note = gate
                .denial_note(&permission_key_for(SkillSource::User, "status"))
                .expect("plan refused it, so the level has a sentence for it");
            assert!(note.contains(level.name()), "{note}");
        }
        answerer.stop();
    }
}

/// **AC-9: a client refusal reaches the caller as a *reason*, never as a
/// decline.**
///
/// On a pipe at a level that would ask, the client refuses **without reading
/// stdin** (BR-11) — nobody declined anything, and a placeholder saying "you
/// declined this" would be telling the user something false about a question
/// they were never shown. The dismissal leg is in the same test because it is
/// the thing this must not be confused with: `Cancelled` is a human, and it
/// *is* a decline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_refusal_reaches_the_caller_as_a_reason_not_a_decline() {
    for reason in [
        RefusalReason::NoTerminal,
        RefusalReason::UnrecognizedSubject,
    ] {
        let conns = connections(1);
        let Session {
            gate,
            pending,
            bus_watch: _bus_watch,
            routed,
        } = session_at(PermissionLevel::Guarded, "refusal", &conns);
        let answerer = Answerer::spawn(
            routed,
            Arc::clone(&pending),
            vec![PermissionOutcome::Refused { reason }],
        );

        assert_eq!(
            invoke(
                &gate,
                "status",
                SkillSource::User,
                &["git status"],
                conns[0]
            )
            .await,
            SkillConsent::Refused(reason),
            "the reason must survive the trip out of the gate"
        );
        // A refusal is not a decision, so nothing is remembered from it — the
        // next invocation asks again, on a terminal where someone can answer.
        assert_eq!(
            gate.remembered(&permission_key_for(SkillSource::User, "status")),
            None
        );
        answerer.stop();
    }

    // The dismissal: a human, and therefore a decline.
    let conns = connections(1);
    let Session {
        gate,
        pending,
        bus_watch: _bus_watch,
        routed,
    } = session_at(PermissionLevel::Guarded, "dismissal", &conns);
    let answerer = Answerer::spawn(
        routed,
        Arc::clone(&pending),
        vec![PermissionOutcome::Cancelled],
    );
    assert_eq!(
        invoke(
            &gate,
            "status",
            SkillSource::User,
            &["git status"],
            conns[0]
        )
        .await,
        SkillConsent::Declined,
        "a dismissed prompt is a human declining, not a client refusing"
    );
    answerer.stop();
}

// ---------------------------------------------------------------------------
// ADR-6 / LESSON-501 — project grants die at `/cd`
// ---------------------------------------------------------------------------

/// **ADR-6 / LESSON-501: a grant remembered in one repo does not authorize
/// another repo's commands after the root moves.**
///
/// `skill:project:audit` named one file when the user consented to it and names
/// a different one the instant `/cd` moves the session root — the grant map is
/// state carried past the thing that gave it meaning. `drop_project_skill_grants`
/// is the commit seam that re-asserts the invariant; skipping the call is what
/// this test fails on, because the second invocation would be answered by the
/// first repo's grant and never reach a prompt.
///
/// The user grant is asserted in the same test as the thing that must *not*
/// change: `~/.claude` does not move when the session root does, and a sweep
/// that took it too would ask a question the user has already answered about the
/// very same file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_project_grant_is_dropped_when_the_session_root_moves() {
    let conns = connections(1);
    let Session {
        gate,
        pending,
        bus_watch: _bus_watch,
        routed,
    } = session_at(PermissionLevel::Guarded, "cd-drops-project", &conns);
    let answerer = Answerer::spawn(
        routed,
        Arc::clone(&pending),
        vec![
            selected("allow_always"), // project audit, in repo A
            selected("allow_always"), // user status
            selected("reject_once"),  // project audit again, in repo B
        ],
    );

    assert_eq!(
        invoke(
            &gate,
            "audit",
            SkillSource::Project,
            &["./audit.sh"],
            conns[0]
        )
        .await,
        SkillConsent::Allowed
    );
    assert_eq!(
        invoke(
            &gate,
            "status",
            SkillSource::User,
            &["git status"],
            conns[0]
        )
        .await,
        SkillConsent::Allowed
    );
    assert_eq!(answerer.count(), 2);

    // `/cd`: the session root moves, and every project grant goes with it.
    assert_eq!(gate.drop_project_skill_grants(), 1);

    assert_eq!(
        invoke(
            &gate,
            "audit",
            SkillSource::Project,
            &["./audit.sh"],
            conns[0]
        )
        .await,
        SkillConsent::Declined,
        "the same key names a different repo's file now, and must be asked \
         about again"
    );
    assert_eq!(answerer.count(), 3);

    // The user skill is the same file it was, and is not asked about again.
    assert_eq!(
        invoke(
            &gate,
            "status",
            SkillSource::User,
            &["git status"],
            conns[0]
        )
        .await,
        SkillConsent::Allowed
    );
    assert_eq!(
        answerer.count(),
        3,
        "`~/.claude` does not move when the session root does"
    );

    answerer.stop();
}

// ---------------------------------------------------------------------------
// REQ-587 BR-4 / ADR-7 — the third door
// ---------------------------------------------------------------------------

/// **BR-4 / ADR-7: the acknowledgment asks its own question, under its own key,
/// naming the root and the project's model-invocable skills.**
///
/// The key is the whole claim. LESSON-495's rule is that a remembered answer is
/// attached to its key and frees every later request whose key matches, so
/// "may the model run this repository's skills as instructions?" and "may
/// `/deploy`'s commands run?" sharing a string would let a `y` to one answer
/// the other. `project_skill_trust:<root>` is deliberately not a `skill:` key,
/// which is also the mechanical reason `authorize_skill` cannot carry this
/// question at all — its own guard rejects the key (pinned in `permissions.rs`
/// by `the_skill_door_refuses_the_project_acknowledgment_key`).
///
/// The subject is asserted too, because BR-11 requires a client to recognize
/// the request **without parsing the key**, and because BR-4 requires the user
/// to be answering about a *named set* rather than a category. Shadowing rides
/// as a bool the client renders, never as prose the daemon pre-marked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_acknowledgment_asks_under_its_own_key_and_names_the_root_and_its_skills() {
    let conns = connections(1);
    let Session {
        gate,
        pending,
        mut bus_watch,
        routed,
    } = session_at(PermissionLevel::Guarded, "ack-key", &conns);
    let answerer = Answerer::spawn(routed, Arc::clone(&pending), vec![selected("allow_once")]);

    let skills = [entry("validate", true), entry("canary", false)];
    let consent = acknowledge(&gate, "~/dev/teton", &skills, false, conns[0]).await;

    assert_eq!(consent, SkillConsent::Allowed);
    assert_eq!(answerer.count(), 1, "one question about the whole set");

    let prompt = answerer.prompts().remove(0);
    assert_eq!(
        prompt.request.tool_name,
        project_skill_trust_key("~/dev/teton"),
        "the acknowledgment asks under its own key, never a skill's and never a \
         tool's name"
    );
    assert_ne!(
        prompt.request.tool_name,
        permission_key_for(SkillSource::Project, "validate"),
        "a skill's key here would remember this answer against that skill's \
         question"
    );
    match prompt.request.subject {
        Some(PermissionSubject::ProjectSkillTrust { root, skills, more }) => {
            assert_eq!(root, "~/dev/teton", "home-relative, never an absolute path");
            assert!(
                !root.contains("/Users/"),
                "the root on a client's refusal line must carry no username"
            );
            assert_eq!(
                skills,
                vec![entry("validate", true), entry("canary", false)],
                "the user answers about a named set, in registry order"
            );
            assert_eq!(more, 0, "nothing was left out of a two-name list");
        }
        other => panic!(
            "a client must be able to recognize this request without parsing \
             the key (BR-11); subject was {other:?}"
        ),
    }
    // The four standard options, and never the fifth: durable project trust is
    // wholly Deferred (OQ-3), so there is nothing for `enable_permanent` to
    // write and nothing that survives this session.
    let ids: Vec<&str> = prompt
        .request
        .options
        .iter()
        .map(|o| o.option_id.as_str())
        .collect();
    assert_eq!(
        ids,
        vec!["allow_once", "allow_always", "reject_once", "reject_always"],
    );
    assert!(!ids.contains(&OPTION_ID_ENABLE_PERMANENT));

    assert_nothing_was_broadcast(&mut bus_watch, "the acknowledgment's own prompt");
    answerer.stop();
}

/// **BR-4: `guarded`/`edits` ask once, `plan` denies, `full` allows — from the
/// level's *default*, with no row of the acknowledgment's own.**
///
/// The two silent legs assert the prompt count as well as the answer, because
/// "allowed" and "allowed without being asked" are different claims and only
/// the second one is the level's. The "once" half is in the same test as the
/// thing it must not be confused with: the second invocation at `guarded` is
/// answered by the session grant, so a gate that had simply stopped asking
/// would be indistinguishable without it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_level_default_governs_the_acknowledgment_at_every_level() {
    for (level, want) in [
        (PermissionLevel::Guarded, SkillConsent::Allowed),
        (PermissionLevel::Edits, SkillConsent::Allowed),
        (PermissionLevel::Plan, SkillConsent::DeniedByLevel),
        (PermissionLevel::Full, SkillConsent::Allowed),
    ] {
        let conns = connections(1);
        let Session {
            gate,
            pending,
            bus_watch: _bus_watch,
            routed,
        } = session_at(level, "ack-levels", &conns);
        let answerer = Answerer::spawn(
            routed,
            Arc::clone(&pending),
            vec![selected("allow_always"), selected("reject_once")],
        );

        let skills = [entry("validate", false)];
        assert_eq!(
            acknowledge(&gate, "~/dev/teton", &skills, false, conns[0]).await,
            want,
            "at {level}"
        );

        let asked = matches!(level, PermissionLevel::Guarded | PermissionLevel::Edits);
        assert_eq!(
            answerer.count(),
            usize::from(asked),
            "at {level}: whether a human was asked is the level's answer, not \
             the grant map's"
        );

        // Once per session per root: the second invocation is answered by the
        // grant, and the scripted `reject_once` is never reached.
        assert_eq!(
            acknowledge(&gate, "~/dev/teton", &skills, false, conns[0]).await,
            want,
            "at {level}: the second invocation"
        );
        assert_eq!(
            answerer.count(),
            usize::from(asked),
            "at {level}: the acknowledgment is asked once per session per root"
        );

        if level == PermissionLevel::Plan {
            let note = gate
                .denial_note(&project_skill_trust_key("~/dev/teton"))
                .expect("plan refused it, so the level has a sentence for it");
            assert!(note.contains(level.name()), "{note}");
        }
        answerer.stop();
    }
}

/// **BR-4: a project skill that shadows a user skill is acknowledged even at
/// `full` — and `plan` still denies it.**
///
/// A shadowed name is the one case a `full` session can be surprised by: the
/// model asks for `validate` meaning the file the user installed and gets a
/// body the repository substituted. So the level's `allow` stops settling this
/// one question. Both halves are here because the override is only safe if it
/// is allow-only: an implementation that let a shadowing invocation *override*
/// `plan`'s deny would have built the second path around the gate REQ-560 BR-1
/// forbids, and it would fail on the `plan` leg below rather than passing
/// quietly.
///
/// The falsification leg is the non-shadowing invocation at `full` in the same
/// test: without it, "it asked" is equally consistent with a gate that has
/// stopped honouring `full` at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shadowing_project_skill_is_acknowledged_even_at_full() {
    let conns = connections(1);
    let Session {
        gate,
        pending,
        bus_watch: _bus_watch,
        routed,
    } = session_at(PermissionLevel::Full, "ack-shadow-full", &conns);
    let answerer = Answerer::spawn(
        routed,
        Arc::clone(&pending),
        vec![selected("allow_always"), selected("reject_once")],
    );

    let skills = [entry("validate", true), entry("canary", false)];

    // Not shadowing: `full` allows, and nobody is asked. The unattended posture.
    assert_eq!(
        acknowledge(&gate, "~/dev/teton", &skills, false, conns[0]).await,
        SkillConsent::Allowed
    );
    assert_eq!(
        answerer.count(),
        0,
        "`full` already runs every model-chosen `shell` command in this \
         repository unprompted; an ordinary project skill is no louder"
    );

    // Shadowing: asked, even here.
    assert_eq!(
        acknowledge(&gate, "~/dev/teton", &skills, true, conns[0]).await,
        SkillConsent::Allowed
    );
    assert_eq!(
        answerer.count(),
        1,
        "a shadowed name is the one swap a `full` session can be surprised by, \
         and BR-4 acknowledges it once per session per root"
    );

    // ...and once, not every time: the `allow_always` answered above covers the
    // next shadowing invocation, so the scripted `reject_once` is never reached.
    assert_eq!(
        acknowledge(&gate, "~/dev/teton", &skills, true, conns[0]).await,
        SkillConsent::Allowed
    );
    assert_eq!(answerer.count(), 1, "once per session per root, even here");
    answerer.stop();

    // The override is allow-only. At `plan` the level still denies a shadowing
    // acknowledgment, and denies it *without asking* — an override that reached
    // past `deny` would be a hole rather than a narrowing.
    let conns = connections(1);
    let Session {
        gate,
        pending,
        bus_watch: _bus_watch,
        routed,
    } = session_at(PermissionLevel::Plan, "ack-shadow-plan", &conns);
    let answerer = Answerer::spawn(routed, Arc::clone(&pending), vec![selected("allow_once")]);
    assert_eq!(
        acknowledge(&gate, "~/dev/teton", &skills, true, conns[0]).await,
        SkillConsent::DeniedByLevel,
        "`plan`'s promise is that nothing changes; shadowing does not lift it"
    );
    assert_eq!(
        answerer.count(),
        0,
        "the level settled it, and asked nobody"
    );
    answerer.stop();
}

/// **BR-4 / LESSON-517: at most twenty names, and the tail is a count.**
///
/// An unbounded prompt puts a cloned repository's entire skill directory in
/// front of a user answering one question. `+5 more` and `and some more` are
/// different facts, and the user is being asked to trust the whole set — so the
/// tail rides as a number rather than as a truncation flag.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_acknowledgment_names_twenty_skills_and_counts_the_rest() {
    let conns = connections(1);
    let Session {
        gate,
        pending,
        bus_watch: _bus_watch,
        routed,
    } = session_at(PermissionLevel::Guarded, "ack-bounded", &conns);
    let answerer = Answerer::spawn(routed, Arc::clone(&pending), vec![selected("allow_once")]);

    let skills: Vec<_> = (0..25)
        .map(|n| entry(&format!("skill-{n}"), n == 0))
        .collect();
    assert_eq!(
        acknowledge(&gate, "~/dev/teton", &skills, false, conns[0]).await,
        SkillConsent::Allowed
    );

    match answerer.prompts().remove(0).request.subject {
        Some(PermissionSubject::ProjectSkillTrust { skills, more, .. }) => {
            assert_eq!(skills.len(), 20, "bounded by the daemon, at the door");
            assert_eq!(more, 5, "the tail is a count");
            assert_eq!(skills.first().map(|e| e.name.as_str()), Some("skill-0"));
            assert_eq!(skills.last().map(|e| e.name.as_str()), Some("skill-19"));
            assert!(
                skills[0].shadows_user_skill,
                "each shadowing entry stays marked through the bound"
            );
        }
        other => panic!("expected a project-skill-trust subject, got {other:?}"),
    }
    answerer.stop();
}

// ---------------------------------------------------------------------------
// REQ-587 — what must *not* happen
// ---------------------------------------------------------------------------

/// **A `shell` grant does not answer the acknowledgment.**
///
/// "Yes, run shell commands this session" is an answer about the model's own
/// calls. It is not permission for repository text to reach the model labelled
/// *instructions* — which grants no effect at all, and is therefore not even
/// the same kind of question.
///
/// Falsification leg: the second `shell` call in the same test *is* covered by
/// the grant, so "the acknowledgment still asked" cannot be a gate that has
/// stopped remembering anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shell_grant_does_not_answer_the_acknowledgment() {
    let conns = connections(1);
    let Session {
        gate,
        pending,
        bus_watch,
        routed,
    } = session_at(PermissionLevel::Guarded, "shell-then-ack", &conns);
    let bus_answerer = BusAnswerer::spawn(bus_watch, Arc::clone(&pending), "allow_always");
    let answerer = Answerer::spawn(
        routed,
        Arc::clone(&pending),
        vec![selected("allow_always"), selected("allow_once")],
    );

    assert_eq!(
        gate.authorize("shell", None).await,
        PermissionDecision::Allowed
    );
    assert_eq!(
        gate.authorize("shell", None).await,
        PermissionDecision::Allowed
    );
    assert_eq!(bus_answerer.count(), 1, "the shell grant was remembered");

    let skills = [entry("validate", false)];
    assert_eq!(
        acknowledge(&gate, "~/dev/teton", &skills, false, conns[0]).await,
        SkillConsent::Allowed
    );
    assert_eq!(
        answerer.count(),
        1,
        "a `shell` allow-always must not answer BR-4's question"
    );

    // Falsification: the acknowledgment's *own* grant does cover the next one.
    assert_eq!(
        acknowledge(&gate, "~/dev/teton", &skills, false, conns[0]).await,
        SkillConsent::Allowed
    );
    assert_eq!(answerer.count(), 1);

    answerer.stop();
    bus_answerer.stop();
}

/// **An acknowledgment does not answer a skill's dynamic context, in either
/// direction.**
///
/// The two questions are deliberately different: BR-4's grants no effect —
/// `shell`, `edit` and the dynamic-context key gate effects exactly as they did
/// — while REQ-585 BR-6's authorizes file-supplied commands to run. An
/// implementation that folded them into one key would let "yes, this repository
/// is trusted" silently run `/deploy`'s three commands, which is precisely the
/// widening BR-4's position paragraph disclaims.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_acknowledgment_does_not_answer_a_skills_dynamic_context() {
    let conns = connections(1);
    let Session {
        gate,
        pending,
        bus_watch: _bus_watch,
        routed,
    } = session_at(PermissionLevel::Guarded, "ack-vs-dynamic", &conns);
    let answerer = Answerer::spawn(
        routed,
        Arc::clone(&pending),
        vec![
            selected("allow_always"), // the acknowledgment
            selected("allow_always"), // `/deploy`'s commands
            selected("reject_once"),  // never reached, if the grants hold
        ],
    );

    let skills = [entry("deploy", false)];
    assert_eq!(
        acknowledge(&gate, "~/dev/teton", &skills, false, conns[0]).await,
        SkillConsent::Allowed
    );
    assert_eq!(answerer.count(), 1);

    // The commands are still asked about.
    assert_eq!(
        invoke_as(
            &gate,
            "deploy",
            SkillSource::Project,
            &["./deploy.sh"],
            ArgumentInterpolation::None,
            InvokedBy::Model,
            conns[0],
        )
        .await,
        SkillConsent::Allowed
    );
    assert_eq!(
        answerer.count(),
        2,
        "the acknowledgment grants no effect; the commands are a second question"
    );

    // ...and the other direction: a dynamic-context grant does not acknowledge
    // a second root's skills. (Same session, a root that was never answered
    // about — see `an_acknowledgment_for_one_root_does_not_answer_another_root`
    // for the per-root claim on its own.)
    assert_eq!(
        acknowledge(&gate, "~/dev/teton", &skills, false, conns[0]).await,
        SkillConsent::Allowed
    );
    assert_eq!(
        answerer.count(),
        2,
        "each question keeps its own answer, and neither was asked twice"
    );
    answerer.stop();
}

/// **BR-4: a grant for root A does not answer root B.**
///
/// The acknowledgment is per **root** because that is what it is about: the
/// user trusted *this* repository's committed skills, and a clone in the next
/// directory is a different repository with different authors. The key carries
/// the root for exactly the reason a skill key carries its source.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_acknowledgment_for_one_root_does_not_answer_another_root() {
    let conns = connections(1);
    let Session {
        gate,
        pending,
        bus_watch: _bus_watch,
        routed,
    } = session_at(PermissionLevel::Guarded, "ack-per-root", &conns);
    let answerer = Answerer::spawn(
        routed,
        Arc::clone(&pending),
        vec![selected("allow_always"), selected("reject_once")],
    );

    let skills = [entry("validate", false)];
    assert_eq!(
        acknowledge(&gate, "~/dev/teton", &skills, false, conns[0]).await,
        SkillConsent::Allowed
    );
    assert_eq!(answerer.count(), 1);

    // A second root is a second question, and this one is declined.
    assert_eq!(
        acknowledge(&gate, "~/dev/someone-elses-clone", &skills, false, conns[0]).await,
        SkillConsent::Declined,
        "the first repository's answer must not carry to a second"
    );
    assert_eq!(answerer.count(), 2);

    // Falsification: the first root is still answered, so "it asked" above was
    // about the root and not about the gate forgetting.
    assert_eq!(
        acknowledge(&gate, "~/dev/teton", &skills, false, conns[0]).await,
        SkillConsent::Allowed
    );
    assert_eq!(answerer.count(), 2);
    answerer.stop();
}

/// **ASSUME-017: the acknowledgment dies at `/cd`, in the same sweep and at the
/// same moment as a project skill's grant.**
///
/// The daemon's grant map is state carried past the thing that gave it meaning
/// (LESSON-501). `project_skill_trust:~/dev/teton` names the repository the
/// user trusted; after `/cd` the session is in another one, and an
/// acknowledgment that survived would let the model run a *second* repository's
/// skills as instructions on an answer given about a first — BR-4's harm,
/// reached through BR-4's own door.
///
/// One predicate, two families, because the client memoizes the same keys and
/// consults its copy before drawing any prompt: two stores that disagreed about
/// which keys expire would auto-answer the new root's question with the old
/// root's answer, and no human would see anything. Skipping the sweep is what
/// this test fails on — the second acknowledgment would be answered by the
/// first root's grant and never reach a prompt.
///
/// The user-skill grant is asserted in the same test as the thing that must
/// *not* change: `~/.claude` does not move when the session root does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_acknowledgment_is_dropped_when_the_session_root_moves() {
    let conns = connections(1);
    let Session {
        gate,
        pending,
        bus_watch: _bus_watch,
        routed,
    } = session_at(PermissionLevel::Guarded, "ack-cd", &conns);
    let answerer = Answerer::spawn(
        routed,
        Arc::clone(&pending),
        vec![
            selected("allow_always"), // the acknowledgment, in repo A
            selected("allow_always"), // a user skill's commands
            selected("reject_once"),  // the acknowledgment again, in repo B
        ],
    );

    let skills = [entry("validate", false)];
    assert_eq!(
        acknowledge(&gate, "~/dev/teton", &skills, false, conns[0]).await,
        SkillConsent::Allowed
    );
    assert_eq!(
        invoke(
            &gate,
            "status",
            SkillSource::User,
            &["git status"],
            conns[0]
        )
        .await,
        SkillConsent::Allowed
    );
    assert_eq!(answerer.count(), 2);

    // `/cd`: the session root moves, and everything that named it goes.
    assert_eq!(
        gate.drop_project_skill_grants(),
        1,
        "the acknowledgment is one of the grants a root move invalidates"
    );

    assert_eq!(
        acknowledge(&gate, "~/dev/teton", &skills, false, conns[0]).await,
        SkillConsent::Declined,
        "the same key names a different repository now, and must be asked about \
         again"
    );
    assert_eq!(answerer.count(), 3);

    // The user skill is the same file it was, and is not asked about again.
    assert_eq!(
        invoke(
            &gate,
            "status",
            SkillSource::User,
            &["git status"],
            conns[0]
        )
        .await,
        SkillConsent::Allowed
    );
    assert_eq!(
        answerer.count(),
        3,
        "`~/.claude` does not move when the session root does"
    );
    answerer.stop();
}

// ---------------------------------------------------------------------------
// REQ-587 BR-5 — the grant key follows the arguments, and reports who asked
// ---------------------------------------------------------------------------

/// **BR-5 / OQ-9: when a command interpolates the arguments, a grant for one
/// argument set does not answer another — for both callers.**
///
/// "Allow `/deploy` for this session" is sound while the commands cannot
/// change. When the body carries `` !`./deploy.sh $ARGUMENTS` ``, a later
/// caller chooses part of what the remembered grant runs — and as of this REQ
/// one of the callers is the **model**. So the grant key carries a digest of
/// the substituted command set, and a user-typed `/deploy staging` and a
/// model-issued `deploy prod` are two questions with two answers.
///
/// Dropping the digest from the minter is what this test fails on: `prod` would
/// be answered by `staging`'s grant and never reach a prompt. The falsification
/// leg is the repeat of `staging` in the same test — the grant *is* remembered
/// for the same argument set, so "it asked again" cannot be a gate that has
/// stopped remembering.
///
/// The non-interpolating leg is here too, because it is the same decision seen
/// from the other side: a body whose commands cannot change still keys per
/// skill, exactly as REQ-585 BR-6 does, and a digest applied unconditionally
/// would re-ask a question the user already answered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_grant_for_one_argument_set_does_not_answer_another() {
    let conns = connections(1);
    let Session {
        gate,
        pending,
        bus_watch: _bus_watch,
        routed,
    } = session_at(PermissionLevel::Guarded, "digest-key", &conns);
    let answerer = Answerer::spawn(
        routed,
        Arc::clone(&pending),
        vec![
            selected("allow_always"), // `./deploy.sh staging`, typed by the user
            selected("reject_once"),  // `./deploy.sh prod`, chosen by the model
        ],
    );

    // The user types `/deploy staging` and allows it for the session.
    assert_eq!(
        invoke_as(
            &gate,
            "deploy",
            SkillSource::User,
            &["./deploy.sh staging"],
            ArgumentInterpolation::Substituted,
            InvokedBy::User,
            conns[0],
        )
        .await,
        SkillConsent::Allowed
    );
    assert_eq!(answerer.count(), 1);

    // Falsification: the same argument set is covered by that grant.
    assert_eq!(
        invoke_as(
            &gate,
            "deploy",
            SkillSource::User,
            &["./deploy.sh staging"],
            ArgumentInterpolation::Substituted,
            InvokedBy::Model,
            conns[0],
        )
        .await,
        SkillConsent::Allowed
    );
    assert_eq!(
        answerer.count(),
        1,
        "the same substituted command set is the same question, whoever asked"
    );

    // The model picks different arguments: a different question, asked again —
    // and declined.
    assert_eq!(
        invoke_as(
            &gate,
            "deploy",
            SkillSource::User,
            &["./deploy.sh prod"],
            ArgumentInterpolation::Substituted,
            InvokedBy::Model,
            conns[0],
        )
        .await,
        SkillConsent::Declined,
        "a grant answered for `staging` must not run `prod`"
    );
    assert_eq!(answerer.count(), 2);
    answerer.stop();

    // The other side of the one rule: a body with no interpolating command keys
    // per skill, so one answer covers the session however the commands are
    // spelled at the call site.
    let conns = connections(1);
    let Session {
        gate,
        pending,
        bus_watch: _bus_watch,
        routed,
    } = session_at(PermissionLevel::Guarded, "plain-key", &conns);
    let answerer = Answerer::spawn(
        routed,
        Arc::clone(&pending),
        vec![selected("allow_always"), selected("reject_once")],
    );
    assert_eq!(
        invoke(
            &gate,
            "status",
            SkillSource::User,
            &["git status"],
            conns[0]
        )
        .await,
        SkillConsent::Allowed
    );
    assert_eq!(
        invoke(
            &gate,
            "status",
            SkillSource::User,
            &["git status"],
            conns[0]
        )
        .await,
        SkillConsent::Allowed
    );
    assert_eq!(
        answerer.count(),
        1,
        "a skill whose commands cannot change is answered once for the session"
    );
    answerer.stop();
}

/// **BR-5: the consent says who asked.**
///
/// "You asked for `deploy`" and "the model decided to run `deploy`" carry the
/// same command list and are different questions; the human at `guarded` is
/// entitled to know which one is on the screen. A gate that reported every
/// consent as the user's would attribute the model's choice to the person
/// answering it — and `InvokedBy::User` is the wire default, so the wrong
/// answer here is the silent one (ADR-8).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_consent_reports_the_caller_that_invoked_the_skill() {
    for want in [InvokedBy::User, InvokedBy::Model] {
        let conns = connections(1);
        let Session {
            gate,
            pending,
            bus_watch: _bus_watch,
            routed,
        } = session_at(PermissionLevel::Guarded, "who-asked", &conns);
        let answerer = Answerer::spawn(routed, Arc::clone(&pending), vec![selected("allow_once")]);

        assert_eq!(
            invoke_as(
                &gate,
                "deploy",
                SkillSource::Project,
                &["./deploy.sh"],
                ArgumentInterpolation::None,
                want,
                conns[0],
            )
            .await,
            SkillConsent::Allowed
        );

        match answerer.prompts().remove(0).request.subject {
            Some(PermissionSubject::SkillDynamicContext { invoked_by, .. }) => {
                assert_eq!(invoked_by, want, "the consent must name the caller");
            }
            other => panic!("expected a skill-dynamic-context subject, got {other:?}"),
        }
        answerer.stop();
    }
}
