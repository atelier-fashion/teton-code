//! The session lifecycle — `session/clear`, `session/set_cwd`, and the root a
//! session stands on.
//!
//! REQ-599's ADR-4 planned this as its step 7 (`session lifecycle ->
//! runtime/session.rs`, ~900 lines). Seven commits landed and none of them was
//! that one: the steps were taken cheapest-seam-first and this was the most
//! entangled of the candidates, so the REQ ran out of steps before it ran out
//! of seams. REQ-602 TASK-306 reconciled the plan table and filed the deferral
//! as REQ-603, which is this module.
//!
//! # What is here, and how the set was chosen
//!
//! Five items. The set was derived by **reading the impl structure**, not by
//! searching rationale ids — REQ-599's ADR-1 measured that ids do not locate
//! seams, and LESSON-593 corrected that to "a weak *positive* signal only".
//! The derivation: cut `mod.rs` at the first column-0 `#[cfg(test)]`, enumerate
//! every method of its `impl DaemonRuntime` block with its span, record which
//! `self.<field>` each touches, cluster on what each *serves*, then check the
//! clustering against adjacency.
//!
//! The check is the finding: these six were **already contiguous** in `mod.rs`,
//! one unbroken run between `record_health` and `mcp_egress`. A seam that is
//! already a contiguous run is the strongest structural evidence available that
//! it is one subsystem, and no id search produced it.
//!
//! - `clear_session` — `session/clear`
//! - `jail_root` — the root fallback shared by create, `/cd` and every turn
//! - `session_root_for` — the one root derivation (REQ-583 ADR-1)
//! - `set_session_cwd` — `session/set_cwd`
//! - `drop_grants_expiring_on_root_change` — `set_session_cwd`'s grant-shedding
//!   half
//!
//! Two items sat inside that contiguous run and **stayed** in `mod.rs`.
//!
//! `DaemonRuntime::projects()` stayed on the authority of its own doc: it is "a
//! fact about the **machine**, not about any session". It was in the run by file
//! layout, not by subject.
//!
//! `store_session_skills` stayed for a worse reason, recorded here rather than
//! smoothed over. It **is** session lifecycle — the one skill-registry
//! derivation, funnelled through by `session/create` and `set_session_cwd`, and
//! it belongs in this module. It could not come because of a pre-existing
//! doc-attachment defect that moving it would have had to resolve inside a
//! relocation commit:
//!
//! The 18-line block opening "Derive `session_id`'s skill registry … the one
//! derivation" describes `store_session_skills`. In `mod.rs` it sits above
//! `projects()`, whose own one-line doc is that block's last line — so
//! `projects()` wears another function's rationale and `store_session_skills`
//! carries none. That is exactly the REQ-596/597 hazard
//! `traceability_sweep.rs`'s re-attachment arm exists to catch: an item wedged
//! between a doc comment and the item it explains.
//!
//! The sweep cannot catch this one, because it predates the sweep's own
//! baseline — the wedge is already present at `17c39ec`, so the baseline
//! records `AC-3`, `ADR-1`, `BR-1` and `REQ-585` as annotating `projects`, and
//! any correction reads to the guard as rationale moving *off* the item it
//! explains. Taking `store_session_skills` with its doc turned arm 2 red;
//! taking it without would have left an 18-line comment in `mod.rs` describing
//! a function in this file.
//!
//! Untangling that means changing what the guard's baseline is allowed to
//! assert, which is a change to a guard and deserves its own scrutiny rather
//! than a paragraph inside a relocation. Filed as a follow-up. `set_session_cwd`
//! reaches it as `Self::store_session_skills` across the module boundary, the
//! same way it reaches [`refused_claim_error`], and nothing about the call
//! changed.
//!
//! # A move, not a restructure
//!
//! Bodies are byte-identical to what they were in `mod.rs`. `run_prompt_turn`'s
//! control flow is REQ-600's and is out of scope here; a behaviour change buried
//! in a relocation diff is not reviewable, which is REQ-599's own reason for
//! splitting those apart and it applies unchanged.
//!
//! Each item came with the comment run above it, not just its `///` block —
//! `turn.rs` records losing a 58-line plain-`//` rationale run to exactly that
//! mistake (LESSON-594).
//!
//! **One doc block was re-attached, deliberately and not silently.** The 18-line
//! comment opening "Derive `session_id`'s skill registry … the one derivation"
//! describes `store_session_skills`, but in `mod.rs` it had come to sit above
//! `projects()`, whose own one-line doc was the block's last line — so
//! `store_session_skills` carried no doc at all. The move takes the block with
//! the function it describes and leaves the one-liner with `projects()`.
//! LESSON-599's rule is that a relocation's prose is the one part the compiler
//! and the whole suite cannot check, so a prose change inside a relocation gets
//! named or it gets missed.
//!
//! # Visibility: a corpus change, not a widening
//!
//! Not one qualifier is widened by this move. `clear_session`,
//! `session_root_for` and `set_session_cwd` were already `pub` and have callers
//! outside the crate; `jail_root` and
//! `drop_grants_expiring_on_root_change` stay private. What changed is the
//! *corpus*: `runtime_visibility.rs` excludes `mod.rs` and does not exclude this
//! file, so relocating already-wide items brought them into view. Established by
//! demoting and building, never by grepping for the name (LESSON-596).
//!
//! The one dependency this module keeps on `mod.rs`'s private surface is
//! [`refused_claim_error`], which `turn.rs` already reaches the same way — a
//! private item of the parent module is visible to its descendants, so it needs
//! no qualifier at all.
//!
//! # Which tests came, and which did not (REQ-599 BR-7)
//!
//! **One of the ten session-lifecycle tests moved.** BR-7 asks that a subsystem
//! take its `#[cfg(test)]` bodies along, and the honest report is that nine of
//! them could not, for a reason that is structural rather than convenient.
//!
//! The nine stay in `mod tests::conversation_carry`, whose module header states
//! its own membership rule: *"Every test in this module calls
//! `DaemonRuntime::run_prompt_turn` … against a scripted local engine, and
//! asserts on the context that engine was handed."* All nine drive a real prompt
//! turn before clearing a conversation or moving a root — because a cleared
//! conversation and a moved root only mean anything once a turn has built one.
//! They are conversation-carry tests whose *verb* happens to live here. Moving
//! them would mean lifting the turn-path fixture (`Scripted`, `RecordingEngine`,
//! `carry_runtime`, `prompt`, and four more) into `testsupport.rs` to serve two
//! homes, and would leave `conversation_carry` describing a rule it no longer
//! obeys.
//!
//! The tenth — `the_session_root_is_probed_from_the_cwd_or_the_daemon_fallback`
//! — does not meet that rule: it never calls `run_prompt_turn`. It moved, and it
//! is also the one the compiler forced, since it reads `jail_root`, which is
//! private to this module and unreachable from a parent. The classification and
//! the compiler agree, which is the check worth having.

use super::*;

impl DaemonRuntime {
    /// Empty a session's retained conversation and announce it (`session/clear`,
    /// REQ-567 BR-8 / architecture D-2).
    ///
    /// ## Why a clear takes the turn claim
    ///
    /// A turn owns the conversation from [`SessionRegistry::try_begin_turn`]
    /// until it commits, and [`SessionRegistry::commit_conversation`] replaces
    /// the **whole** vector (BR-6's atomic unit). So a clear that landed under an
    /// in-flight turn would be undone moments later by that turn's commit:
    /// history the user asked to drop would come back, and BR-8's "the next
    /// prompt starts from the system head alone" would be false with nothing on
    /// the wire to say so — the worst shape available, since the user was already
    /// told the clear succeeded.
    ///
    /// It is refused rather than queued or best-effort, through the **same**
    /// claim and the same [`refused_claim_error`] a concurrent `session/prompt`
    /// takes (D-3): one gate, one classifier, one sentence (LESSON-456). That is
    /// also where the unknown-session arm comes from — a clear for a session the
    /// registry does not have is `UNKNOWN_SESSION`, the code the server already
    /// answers that fact with, not a cheerful `blocks_dropped: 0`.
    ///
    /// The claim is held until this returns, so the announcement lands while the
    /// session is still claimed and a waiting client cannot slip a turn between
    /// the clear and the event that describes it. Nothing here awaits, so the
    /// registry lock is taken twice for two vector operations and released
    /// (LESSON-448).
    ///
    /// ## Idempotent, and announced anyway
    ///
    /// Clearing an empty session succeeds with `blocks_dropped: 0` and still
    /// publishes: the event is the user's *action*, not a state transition (the
    /// one place this departs from [`Self::web_override`]'s announce-on-the-edge
    /// rule), and every attached client has to stop describing a conversation the
    /// next prompt will not carry.
    ///
    /// ## Conversation only
    ///
    /// OQ-4, resolved: session taint, the user-pasted-URL set, and this session's
    /// remembered permission grants are all untouched — none of them is reachable
    /// from here, which is why the property is structural rather than checked. A
    /// routinely-typed clear must never silently widen egress or consent
    /// (LESSON-495).
    ///
    /// # Errors
    ///
    /// [`error_code::SESSION_BUSY`] while a turn holds the session, and
    /// [`error_code::UNKNOWN_SESSION`] for a session the registry does not have.
    pub fn clear_session(
        &self,
        params: &SessionClearParams,
        sessions: &SessionRegistry,
        events: &Arc<EventBus>,
    ) -> Result<SessionClearResult, RpcError> {
        // The claim id comes off the turn counter, so a clear and a prompt can
        // never mint the same one, and reads as what holds the session: a client
        // refused during a clear is told "already running turn clear-4", which is
        // true and actionable, where a shared `turn-` prefix would name a turn
        // the user never asked for.
        let _claim = sessions
            .try_begin_turn(
                &params.session_id,
                &teton_protocol::TurnId::from(format!(
                    "clear-{}",
                    self.turn_counter.fetch_add(1, Ordering::SeqCst)
                )),
            )
            .map_err(|err| refused_claim_error(&err))?;

        let blocks_dropped =
            u64::try_from(sessions.clear_conversation(&params.session_id)).unwrap_or(u64::MAX);
        events.publish(
            Some(params.session_id.clone()),
            Event::ContextCleared(ContextCleared { blocks_dropped }),
        );
        Ok(SessionClearResult { blocks_dropped })
    }

    /// The path a session's tools are jailed to: its own `cwd`, or — for a
    /// client that sent none — the daemon's `repo_root` fallback (BUG-147).
    ///
    /// One function, so `session/create`'s answer, `session/set_cwd`'s
    /// `previous_display` and every turn's jail agree on what "no cwd" means:
    /// the root a create reports is the root the turn will jail to, by
    /// construction rather than by two call sites remembering the same
    /// fallback.
    fn jail_root<'a>(&'a self, session_cwd: Option<&'a Path>) -> &'a Path {
        session_cwd.unwrap_or(&self.repo_root)
    }

    /// The session root a session on `session_cwd` stands on, as every surface
    /// renders it (REQ-583 ADR-1): [`crate::session_root::probe`] over
    /// [`Self::jail_root`], with the daemon's own `HOME` — returned **with the
    /// path it was probed at** ([`ProbedRoot`]), so a turn builds its jail and
    /// its prompt from one value and the two cannot disagree.
    ///
    /// Called per use — at `session/create`, on `session/set_cwd` (twice: the
    /// old root for `previous_display`, the new one for the answer) and at the
    /// top of every turn — and never cached: the registry stores the path and
    /// nothing else, so kind, display, project name and branch are always
    /// derived from the path as it stands now.
    #[must_use]
    pub fn session_root_for(&self, session_cwd: Option<&Path>) -> ProbedRoot {
        ProbedRoot::probe(self.jail_root(session_cwd).to_path_buf(), home().as_deref())
    }

    /// Move a live session's root and clear its conversation, announcing both
    /// (`session/set_cwd`, REQ-583 BR-7 / ADR-4).
    ///
    /// Modelled on [`Self::clear_session`], and it takes the **same turn claim**
    /// for the same reason: a turn owns the conversation until it commits, and
    /// it also holds a `ToolContext` built from the root as it stood when the
    /// turn began — so a root that moved underneath would leave that turn's
    /// tools jailed to a directory the session no longer stands on, with the
    /// user already told the move succeeded. Refused as `SESSION_BUSY` through
    /// [`refused_claim_error`], the one classifier a concurrent prompt and a
    /// clear share (LESSON-456); an unknown session is `UNKNOWN_SESSION` from
    /// the same call.
    ///
    /// ## Order: claim → validate → mutate → clear → announce → answer
    ///
    /// Validation ([`validate_session_cwd`], the validator `session/create`'s
    /// `cwd` goes through — BR-6/BR-7's "one grammar") runs **after** the claim
    /// and **before** any mutation, so a refusal leaves both the path and the
    /// conversation exactly as they were, and a busy session says it is busy
    /// before it says anything about the path. `previous_display` is probed
    /// off the old path before it is overwritten — carried on the event for
    /// clients (a transcript that wants to say where the session stood, a
    /// monitor diffing roots); the daemon itself does not render it.
    ///
    /// ## Why the conversation is cleared (OQ-2, resolved)
    ///
    /// Every carried block's provenance identity is relative to the root it was
    /// minted under, and a carried identity judged under a new root names a
    /// different file — so the conversation cannot be carried safely, and the
    /// disposition is a clear, reported in the existing `context_cleared` shape
    /// every attached client already renders. Idempotent in the same sense as a
    /// clear: an empty session moves with `blocks_dropped: 0` and still
    /// announces, because the event is the user's action.
    ///
    /// ## The skill registry is re-derived *here*, ahead of the announcement
    ///
    /// The project half of the registry hangs off the root, so the root moving
    /// re-derives it (REQ-585 BR-1, AC-14). It runs inside this method, before
    /// `session_root_changed` is published, rather than in the handler after
    /// this method returns: same-connection ordering was already safe (the
    /// reader loop is serial), but a **second** attached client reacting to that
    /// event within microseconds would call `skills/list` and be answered from
    /// the pre-move registry. `skills_fs` is the daemon's discovery seam, passed
    /// in because it lives on `Daemon` — the runtime performs the rebuild, the
    /// daemon still owns what discovery reads.
    ///
    /// And the same move drops every remembered `skill:project:*` grant
    /// (ADR-6). A grant remembered under `skill:project:<name>` in one repo
    /// authorizes a file that no longer exists at that name, and a rebuilt
    /// registry beside a stale grant is LESSON-501 exactly — carried state that
    /// sheds its invariants silently. The user half is untouched: those names
    /// still mean the same files.
    ///
    /// ## Two events, both session-scoped, in this order
    ///
    /// `context_cleared` first, then `session_root_changed` — a client learns
    /// the conversation went before it learns where the session now stands, and
    /// both precede the response (the server's fence rule). Both ride
    /// `Some(session_id)`: the display is content-class on the wire
    /// (`server.rs`'s `reduce_for` omits `cwd` from a reduced row), so
    /// `forward_events` filters them for a connection not entitled to the
    /// session's content.
    ///
    /// Nothing under `harness/` names this method or its params: a model must
    /// never be able to move its own jail — the same posture that keeps
    /// clearing off the tool surface.
    ///
    /// # Errors
    ///
    /// [`error_code::SESSION_BUSY`] while a turn holds the session,
    /// [`error_code::UNKNOWN_SESSION`] for a session the registry does not
    /// have, and [`error_code::INVALID_PARAMS`] — naming the path — for a cwd
    /// that is relative, missing, or not a directory.
    pub fn set_session_cwd(
        &self,
        params: &SessionSetCwdParams,
        sessions: &SessionRegistry,
        events: &Arc<EventBus>,
        skills_fs: &dyn crate::skills::DirLister,
    ) -> Result<SessionSetCwdResult, RpcError> {
        // `cd-N` off the turn counter, for `clear_session`'s reason: a refused
        // peer is told "already running turn cd-4", which names what actually
        // holds the session.
        let _claim = sessions
            .try_begin_turn(
                &params.session_id,
                &teton_protocol::TurnId::from(format!(
                    "cd-{}",
                    self.turn_counter.fetch_add(1, Ordering::SeqCst)
                )),
            )
            .map_err(|err| refused_claim_error(&err))?;

        // Validate before touching anything: a refusal leaves the root and the
        // conversation exactly as they were.
        //
        // REQ-584 BR-8: **the path reading is tried first, always.** Only when
        // it fails, and only when the client said the argument was a bare name,
        // is the registry consulted — which is what keeps `/cd src` meaning
        // `./src` wherever `./src` exists, and keeps REQ-583's grammar table
        // passing unchanged.
        let cwd = match validate_session_cwd(&params.cwd) {
            Ok(()) => params.cwd.clone(),
            Err(refusal) => {
                let Some(name) = params.name_hint.as_deref() else {
                    return Err(RpcError::new(
                        error_code::INVALID_PARAMS,
                        refusal.to_string(),
                    ));
                };
                match self.projects.snapshot().resolve_name(name) {
                    teton_core::projects::NameResolution::Unique(project) => {
                        // Validated like any other root — a registry entry is a
                        // remembered path, not a licence to skip the check.
                        validate_session_cwd(&project.path).map_err(|refusal| {
                            RpcError::new(error_code::INVALID_PARAMS, refusal.to_string())
                        })?;
                        project.path.clone()
                    }
                    teton_core::projects::NameResolution::Ambiguous(candidates) => {
                        // Names the candidates and moves nowhere: picking one
                        // would move the session somewhere the user did not
                        // choose, which is worse than asking again.
                        let listed = candidates
                            .iter()
                            .map(|p| {
                                teton_core::session_root::bounded_field(
                                    &teton_core::session_root::display_for(
                                        &p.path,
                                        home().as_deref(),
                                    ),
                                    teton_core::session_root::DISPLAY_MAX_CHARS,
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(", ");
                        return Err(RpcError::new(
                            error_code::INVALID_PARAMS,
                            format!(
                                "`{name}` names more than one known project: {listed} —                                  `/cd <path>` picks one"
                            ),
                        ));
                    }
                    teton_core::projects::NameResolution::None => {
                        return Err(RpcError::new(
                            error_code::INVALID_PARAMS,
                            teton_core::session_root::cd_two_reading_refusal(name),
                        ));
                    }
                }
            }
        };

        // The claim above proved the session exists, so `get` cannot miss —
        // but the fallback reads as what it is rather than as an unwrap.
        let previous_cwd = sessions.get(&params.session_id).and_then(|s| s.cwd);
        let previous_display = self.session_root_for(previous_cwd.as_deref()).view.display;

        if !sessions.set_cwd(&params.session_id, cwd.clone()) {
            return Err(RpcError::new(
                error_code::UNKNOWN_SESSION,
                format!("no session `{}`", params.session_id),
            ));
        }
        // `cwd`, not `params.cwd`: after a BR-8 registry resolution those are
        // different paths, and probing the requested one would derive the root,
        // the skills and the recorded project from a directory that does not
        // exist.
        let moved_to = self.session_root_for(Some(&cwd));

        // REQ-585 BR-1/AC-14 and ADR-6, both before the announcement below, and
        // both through the one derivation `session/create` also takes — a second
        // spelling of the four globs would be LESSON-528's shape at the seam
        // where the two answers have to agree.
        Self::store_session_skills(
            sessions,
            &params.session_id,
            &moved_to,
            skills_fs,
            &self.projects,
        );
        // REQ-612 BR-1 / ADR-3, in this block and for this block's reason: the
        // repository's notes are read at the root the session stands on, so the
        // root moving re-reads them — under the new root's boundary matcher and
        // with the new root's `TETON.md`, or with none. It lands **before** the
        // two publishes below, which is the ordering REQ-585 established for the
        // skill registry one line above: a second attached client reacting to
        // `session_root_changed` must not be able to read the pre-move notes.
        //
        // The store publishes `repo_context_state` itself when the state moved,
        // outside the registry lock — so a `/cd` out of a repository with notes
        // announces that the block was dropped, and a `/cd` between two plain
        // directories announces nothing.
        self.store_session_repo_context(sessions, &params.session_id, &moved_to, events);
        self.drop_grants_expiring_on_root_change(&params.session_id);

        let root = moved_to.view;
        let blocks_dropped =
            u64::try_from(sessions.clear_conversation(&params.session_id)).unwrap_or(u64::MAX);
        events.publish(
            Some(params.session_id.clone()),
            Event::ContextCleared(ContextCleared { blocks_dropped }),
        );
        events.publish(
            Some(params.session_id.clone()),
            Event::SessionRootChanged(SessionRootChanged {
                previous_display,
                root: root.clone(),
            }),
        );
        Ok(SessionSetCwdResult {
            root,
            blocks_dropped,
        })
    }

    /// Forget every remembered consent this session's **root** gave meaning to,
    /// and say how many went (REQ-585 ADR-6, TASK-201; REQ-587 TASK-215).
    ///
    /// The `/cd` half of the per-skill permission key. `skill:<source>:<name>`
    /// encodes the whole question only for as long as `<name>` means the same
    /// file: after the root moves it names a different one, so a grant that
    /// survived would authorize another repo's commands under a name the user
    /// approved somewhere else.
    ///
    /// **Renamed from `drop_project_skill_grants` (REQ-587 TASK-217), because
    /// the old name is now narrower than the effect.** TASK-215 widened the
    /// gate's own drop from a `skill:project:` prefix to
    /// `expires_on_session_root_change`, the shared predicate, which also
    /// catches BR-4's project-skill *acknowledgment* — a key that is neither a
    /// skill grant nor spelled with that prefix. A name that said "project
    /// skill grants" would leave the next reader believing the acknowledgment
    /// survives a `/cd`, which is precisely the belief ASSUME-017 was written
    /// to prevent.
    ///
    /// The method it delegates to still carries the REQ-585 name:
    /// `harness/permissions.rs` is TASK-215's file, and renaming a shipped
    /// public method from here would be a change made outside the boundary that
    /// owns its argument. The residue is one hop wide and is named here rather
    /// than left for a reader to notice.
    ///
    /// **A missing gate is nothing to do, not a gate to mint.** Reaching for
    /// [`Self::permission_gate_for`] here would create a session's gate at
    /// `/cd` time — snapshotting `[web] permission_allow` earlier than the first
    /// turn does — to then drop zero grants from it. A session that has never
    /// run a turn has remembered no answers.
    fn drop_grants_expiring_on_root_change(&self, session_id: &SessionId) -> usize {
        self.session_gates
            .lock()
            .expect("session gate mutex poisoned")
            .get(session_id)
            .map_or(0, |gate| gate.drop_project_skill_grants())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::testsupport::scratch_root;

    /// **REQ-583 ADR-1: one derivation.** `session_root_for` probes the
    /// session's own cwd when it has one and the daemon's `repo_root`
    /// fallback when it does not — the same fallback the turn jails to
    /// (BUG-147) — with the daemon's `HOME`, and hands back the path it
    /// probed beside the answer. What `session/create` answers, what `/cd`
    /// carries as `previous_display`, and what every turn puts on
    /// `HarnessConfig.session_root` and into its jail are readings of this
    /// one function.
    #[test]
    fn the_session_root_is_probed_from_the_cwd_or_the_daemon_fallback() {
        let runtime = DaemonRuntime::minimal();
        let home = crate::session_root::home();
        let project = scratch_root("derive", true);

        let derived = runtime.session_root_for(Some(&project));
        assert_eq!(
            derived.view,
            crate::session_root::probe(&project, home.as_deref()),
            "a session with a cwd stands on that cwd"
        );
        assert_eq!(
            derived.path, project,
            "the jail path rides beside the view it was probed for"
        );
        assert_eq!(
            derived.view.kind,
            teton_protocol::methods::RootKind::Project,
            "the marker makes it a project root: {derived:?}"
        );

        let fallback = runtime.session_root_for(None);
        assert_eq!(
            fallback.view,
            crate::session_root::probe(&runtime.repo_root, home.as_deref()),
            "a session without a cwd stands on the daemon's repo_root — \
             the value the turn jails to"
        );
        assert_eq!(
            fallback.path, runtime.repo_root,
            "and the jail's fallback IS that repo_root"
        );
        assert_eq!(
            runtime.jail_root(None),
            runtime.repo_root.as_path(),
            "and the jail's fallback IS that repo_root"
        );
        assert_eq!(runtime.jail_root(Some(&project)), project.as_path());

        let _ = std::fs::remove_dir_all(&project);
    }
}
