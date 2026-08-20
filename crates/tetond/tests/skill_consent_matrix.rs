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
    Event, PermissionOption, PermissionRequest, PermissionSubject, OPTION_ID_ENABLE_PERMANENT,
};
use teton_protocol::methods::{PermissionOutcome, RefusalReason};
use teton_protocol::permissions::PermissionLevel;
use teton_protocol::SessionId;

use tetond::broadcast::{EventBus, Subscription};
use tetond::grants::{ConnectionId, GrantRegistry};
use tetond::harness::permissions::{
    AddressedPermissionDelivery, PendingPermissions, PermissionDecision, PermissionGate,
    SkillConsent,
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
    tokio::time::timeout(
        PROMPT_WAIT,
        gate.authorize_skill(
            &permission_key_for(source, skill),
            skill,
            source,
            commands.iter().map(|c| (*c).to_owned()).collect(),
            from,
        ),
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
        }) => {
            assert_eq!(skill, "status");
            assert_eq!(source, SkillSource::User);
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
