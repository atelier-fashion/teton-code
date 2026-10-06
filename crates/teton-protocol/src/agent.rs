//! Subagent dispatch vocabulary (REQ-623).
//!
//! The model calls one tool, `agent`, with a list of [`ChildTask`]s. The daemon
//! runs each as a **child turn** — the same turn loop as a prompt turn, a fresh
//! context, and bounds fixed before its first model call ([`ChildBounds`]) — and
//! hands back one [`ChildResult`] per task, each ending in exactly one of the
//! eight [`ChildStatus`]es. A call the tool will not start is refused whole,
//! before any child runs, with an [`AgentRefusal`].
//!
//! These are the *shapes*. The seven `agent_*` events that report a call's
//! progress live with the rest of the bus vocabulary in [`crate::events`], and
//! that is also where the two ids every child-scoped payload carries — a
//! [`ChildId`] and the parent's [`TurnId`](crate::TurnId) — are declared on the
//! payloads that carry them (REQ-623 ADR-3: on payloads, never on the envelope).
//!
//! # What is deliberately not here
//!
//! The spec's `ChildResult.provenance` — the set of provenance ids a child's
//! context touched — is **not** a field of [`ChildResult`]. `ProvenanceId` lives
//! in `teton-core` and has no `Deserialize` by design (a wire value must never
//! become an identity), and REQ-623 ADR-8 carries the set on the parent's
//! *result block*, daemon-side, where the egress check already reads
//! provenance. Serializing it here would put file identities into the bytes the
//! parent model reads, and hand a client a set it could not turn back into the
//! type that gates egress. The daemon's child outcome carries the set beside
//! this struct.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{ProviderId, Tier};

/// The daemon-minted identity of one child turn: `"<call_id>/<name>"`.
///
/// Unique within a session because `call_id` is, and `name` is unique within a
/// call ([`AgentRefusal::DuplicateName`] refuses the call otherwise).
///
/// # Opaque on purpose
///
/// There is a way to mint one ([`Self::new`]), a way to read it
/// ([`Self::as_str`], [`fmt::Display`]), and **no way to take one apart**. The
/// `name` half is model-authored and nothing stops it containing a `/`, so any
/// split would be a guess; and a consumer that wants the name already has it as
/// a field of `agent_child_started`. A reader that needs the child's name keys
/// on the id and looks the name up there — it never parses it back out.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChildId(String);

impl ChildId {
    /// Mint the id of the child named `name` in the `agent` call `call_id`.
    #[must_use]
    pub fn new(call_id: &str, name: &str) -> Self {
        Self(format!("{call_id}/{name}"))
    }

    /// The id as the string it is on the wire.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ChildId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Restore an id minted earlier by [`ChildId::new`] and stored as text — a
/// ledger column, a transcript line. Not a way to mint a fresh one, and not a
/// parse: the string is kept whole.
impl From<String> for ChildId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// One task in an `agent` call — the model-facing input (REQ-623 AC-1).
///
/// The wire form **is** the schema the tool advertises: `{task, name?, tier?,
/// context?}`. `task` is required; the other three are optional and omitted
/// when absent, so a task the model wrote as `{"task": "…"}` re-serializes as
/// exactly that.
///
/// Unknown keys are ignored rather than refused. The vendored skills phrase
/// dispatch in another harness's vocabulary (`subagent_type`,
/// `run_in_background`) and the spec expects the model to map that phrasing
/// onto this schema; a stray key on a call whose meaning is otherwise clear is
/// not worth refusing the call over. Whether the advertised JSON schema says
/// `additionalProperties: false` is the tool's decision, not this type's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildTask {
    /// The child's user-role prompt, verbatim.
    ///
    /// Required, and required **non-empty** — but an empty string parses here.
    /// The tool refuses it typed ([`AgentRefusal::EmptyTask`]), naming which
    /// task; a type that rejected it at parse time would turn that typed
    /// refusal into an anonymous argument error.
    pub task: String,
    /// The child's name, unique within the call; the daemon defaults it to
    /// `child-<n>` when absent. The spec bounds it at 40 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The tier the caller would *like* the child to run on — a request to the
    /// router, never a binding (BR-6). A request the router cannot honour is
    /// not a refusal; [`ChildResult::route`] says where the child ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<Tier>,
    /// Extra text the parent chooses to pass. Counts against the child's context
    /// budget, and is admitted together with `task` whole or not at all (BR-1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

/// The four numbers a child runs under (REQ-623 BR-7).
///
/// Derived once from the child's resolved route and the `[agent]` config,
/// stamped before the child's first model call, published on
/// `agent_child_started`, and echoed in [`ChildResult::bounds`] — the same
/// value, not a re-derivation (LESSON-501). The one figure that may move after
/// stamping is the spend ceiling, and only upward, by a sibling's release;
/// where it ended is [`ChildResult::spend_ceiling_final_micro_cents`], and this
/// struct keeps the stamped share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildBounds {
    /// Model calls the child may make — `agent.child_max_turns`, clamped to the
    /// parent's own `max_turns`.
    pub max_turns: u32,
    /// The child's context budget in bytes, derived from its route exactly as a
    /// prompt turn's is (REQ-586).
    pub context_budget_bytes: u64,
    /// The child's *initial* share of the prompt's spend ceiling, in micro-cents
    /// (the unit `spend_ceiling_micro_cents` uses everywhere, REQ-588) — the
    /// remaining headroom divided equally among the call's children, floored.
    ///
    /// Absent when the prompt has no ceiling: such a session gives children no
    /// ceiling (BR-8), and absent is what "no ceiling" already means on
    /// `route_decided`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend_ceiling_micro_cents: Option<u64>,
    /// The child's whole wall clock, in seconds. Time parked on a consent prompt
    /// does not count against it (BR-5).
    pub deadline_secs: u64,
}

/// How a child turn ended — exactly one of eight (REQ-623 BR-10).
///
/// A closed set with no catch-all: the parent model reasons about *which* of
/// these happened, so a status this build does not know is an error to decode,
/// not something to round down to the nearest one it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildStatus {
    /// The child produced its final text inside every bound.
    Completed,
    /// The child was refused — a project-skill gate, an unattended deny of a
    /// tool the task needed, or its own `task` + `context` over its budget.
    /// [`ChildResult::refusal`] names which.
    Refused,
    /// The parent turn was cancelled while the child ran.
    Cancelled,
    /// The child used all of [`ChildBounds::max_turns`]; its report is whatever
    /// final text it had produced, marked.
    TurnsExhausted,
    /// The child's context could not be fitted to its budget mid-run.
    BudgetExhausted,
    /// The child's next call would have exceeded its spend ceiling.
    SpendExhausted,
    /// The child's deadline passed; an in-flight tool call was cancelled.
    TimedOut,
    /// A provider or engine error after the child's own retry and reroute path
    /// was exhausted; [`ChildResult::error`] carries the code.
    Failed,
}

impl ChildStatus {
    /// Every status, in the spec entity table's order.
    pub const ALL: [ChildStatus; 8] = [
        ChildStatus::Completed,
        ChildStatus::Refused,
        ChildStatus::Cancelled,
        ChildStatus::TurnsExhausted,
        ChildStatus::BudgetExhausted,
        ChildStatus::SpendExhausted,
        ChildStatus::TimedOut,
        ChildStatus::Failed,
    ];

    /// The wire spelling — identical to the serde form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ChildStatus::Completed => "completed",
            ChildStatus::Refused => "refused",
            ChildStatus::Cancelled => "cancelled",
            ChildStatus::TurnsExhausted => "turns_exhausted",
            ChildStatus::BudgetExhausted => "budget_exhausted",
            ChildStatus::SpendExhausted => "spend_exhausted",
            ChildStatus::TimedOut => "timed_out",
            ChildStatus::Failed => "failed",
        }
    }
}

impl fmt::Display for ChildStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The route a child actually ran on, after the router and the privacy pin
/// (REQ-623 BR-6).
///
/// A structure rather than the sentence the spec's entity table sketches
/// ("tier + provider + model"), for the reason [`crate::events::PermissionSubject`]
/// is one: a client and a test select on the tier or the provider, and a
/// string they had to parse would be a format nobody promised to keep.
/// [`fmt::Display`] renders the sentence for anyone who wants it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildRoute {
    /// The tier the route was resolved under, when it was resolved under one —
    /// absent exactly where `route_decided`'s `tier` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<Tier>,
    /// The provider the child's calls went to.
    pub provider_id: ProviderId,
    /// The concrete model.
    pub model: String,
}

impl fmt::Display for ChildRoute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.tier {
            Some(tier) => write!(f, "{tier} {}/{}", self.provider_id, self.model),
            None => write!(f, "{}/{}", self.provider_id, self.model),
        }
    }
}

/// What one child hands back to the parent (REQ-623 BR-10).
///
/// The `agent` tool's result is a JSON array of these, framed as untrusted data
/// exactly as a `read` result is — a child's report is model output about
/// repository content, never instructions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildResult {
    /// The [`ChildTask::name`], or the default the daemon gave it.
    pub name: String,
    /// How the child ended.
    pub status: ChildStatus,
    /// The child's final assistant text, cut at `agent.report_max_bytes` with a
    /// typed marker when longer (BR-11). Empty unless [`ChildStatus::Completed`]
    /// or [`ChildStatus::TurnsExhausted`] — always present, because an empty
    /// string is the honest "nothing came back".
    #[serde(default)]
    pub report: String,
    /// The typed refusal code when [`Self::status`] is
    /// [`ChildStatus::Refused`] (e.g. `over_budget`); absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    /// The error code when [`Self::status`] is [`ChildStatus::Failed`]; absent
    /// otherwise. BR-10 has `failed` carry "the error code", and the entity
    /// table has `refusal` absent outside `refused`, so the code needs a field
    /// of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Model calls the child made.
    pub turns_used: u32,
    /// Where the child ran. Absent only for a child that ended before its route
    /// was resolved — cancelled while it waited to start, or a routing failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<ChildRoute>,
    /// The bounds the child ran under — the value `agent_child_started`
    /// published, echoed (BR-7, AC-11). Absent exactly where [`Self::route`]
    /// is: the bounds derive from the route, and a child with no route had
    /// none stamped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<ChildBounds>,
    /// The sum of the child's own cost records, in micro-cents.
    pub cost_micro_cents: u64,
    /// The child's spend ceiling when it ended — at least the stamped
    /// [`ChildBounds::spend_ceiling_micro_cents`], raised only by sibling
    /// releases (BR-8). Absent when the prompt had no ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend_ceiling_final_micro_cents: Option<u64>,
}

/// Why an `agent` call was refused **whole**, before any child started
/// (REQ-623 BR-3).
///
/// Each code carries the numbers that refused it, so the parent can correct the
/// call without guessing at a cap. Internally tagged under `kind`, the
/// codebase's spelling for a typed discriminant, and nested under the
/// `refusal` key of `agent_call_refused` so the tag cannot collide with any key
/// the envelope or the transcript line owns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentRefusal {
    /// The call named more tasks than `agent.max_children_per_call`.
    TooManyChildren {
        /// Tasks in the refused call.
        requested: u32,
        /// `agent.max_children_per_call`.
        cap: u32,
    },
    /// The call would push the parent prompt turn past
    /// `agent.max_children_per_turn`.
    ChildCapReached {
        /// Children this parent turn had already started before the call.
        started: u32,
        /// Tasks in the refused call.
        requested: u32,
        /// `agent.max_children_per_turn`.
        cap: u32,
    },
    /// Two tasks in the call had the same name.
    DuplicateName {
        /// The repeated name.
        name: String,
    },
    /// A task's `task` text was empty.
    EmptyTask {
        /// Its 0-based position in the call's `tasks`.
        index: u32,
    },
}

impl AgentRefusal {
    /// The wire code — identical to the serialized `kind` tag.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            AgentRefusal::TooManyChildren { .. } => "too_many_children",
            AgentRefusal::ChildCapReached { .. } => "child_cap_reached",
            AgentRefusal::DuplicateName { .. } => "duplicate_name",
            AgentRefusal::EmptyTask { .. } => "empty_task",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::collections::BTreeSet;

    /// The object's keys, sorted — what a reader of the wire form sees.
    fn keys(value: &Value) -> Vec<&str> {
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
    }

    /// **BR-10: a child ends in exactly one of eight statuses, each with the
    /// spelling the spec's entity table gives it, and nothing else parses.**
    ///
    /// The expected strings are literals copied from the entity table, never
    /// read off the subject, so a derive that drifts cannot agree with itself.
    /// The rejection list is the near-misses a sloppy producer would emit: the
    /// Rust spelling, the shouted one, the separator-dropped one that
    /// `rename_all = "lowercase"` would produce, a kebab spelling, and
    /// synonyms. A ninth variant fails to build the exhaustive `match` below.
    ///
    /// **Mutation (run 2026-10-05):** `rename_all = "snake_case"` →
    /// `"lowercase"` on [`ChildStatus`] reds this test on `turns_exhausted`
    /// (the derive emits `turnsexhausted`) — and nothing else in the crate
    /// (1 of 247).
    #[test]
    fn child_status_eight_variants_round_trip() {
        let table = [
            (ChildStatus::Completed, "completed"),
            (ChildStatus::Refused, "refused"),
            (ChildStatus::Cancelled, "cancelled"),
            (ChildStatus::TurnsExhausted, "turns_exhausted"),
            (ChildStatus::BudgetExhausted, "budget_exhausted"),
            (ChildStatus::SpendExhausted, "spend_exhausted"),
            (ChildStatus::TimedOut, "timed_out"),
            (ChildStatus::Failed, "failed"),
        ];

        // `ALL` is the table, in the table's order.
        assert_eq!(
            ChildStatus::ALL.to_vec(),
            table.iter().map(|(s, _)| *s).collect::<Vec<_>>()
        );
        let distinct: BTreeSet<&str> = table.iter().map(|(_, w)| *w).collect();
        assert_eq!(distinct.len(), 8, "eight distinct wire spellings");

        for (status, wire) in table {
            let json = serde_json::to_value(status).expect("serializes");
            assert_eq!(json, Value::String(wire.to_owned()), "{status:?}");
            assert_eq!(status.as_str(), wire, "as_str agrees with serde");
            assert_eq!(status.to_string(), wire, "Display agrees with serde");
            let back: ChildStatus = serde_json::from_value(json).expect("round-trips");
            assert_eq!(back, status);
            // Compile-time half: a ninth variant does not build.
            match status {
                ChildStatus::Completed
                | ChildStatus::Refused
                | ChildStatus::Cancelled
                | ChildStatus::TurnsExhausted
                | ChildStatus::BudgetExhausted
                | ChildStatus::SpendExhausted
                | ChildStatus::TimedOut
                | ChildStatus::Failed => {}
            }
        }

        for bad in [
            "Completed",
            "COMPLETED",
            "TurnsExhausted",
            "turnsexhausted",
            "turns-exhausted",
            "timed-out",
            "timeout",
            "success",
            "error",
            "unknown",
            "",
        ] {
            assert!(
                serde_json::from_value::<ChildStatus>(Value::String(bad.to_owned())).is_err(),
                "`{bad}` must not parse as a ChildStatus"
            );
        }
    }

    /// **AC-1's wire half: a task is `{task, name?, tier?, context?}`.**
    ///
    /// `task` alone parses and re-serializes as exactly `{task}`; the full form
    /// carries all four keys and no others; the tier is one of the four
    /// lowercase names and nothing else; a missing `task` is a parse error. An
    /// empty `task` *parses* — refusing it is the tool's typed
    /// [`AgentRefusal::EmptyTask`], and a parse error would lose which task it
    /// was. A key from another harness's vocabulary is ignored, not refused.
    ///
    /// **Mutations (run 2026-10-05):** dropping `skip_serializing_if` from
    /// [`ChildTask::name`] reds this test on the minimal key set (`"name":
    /// null` appears); adding `#[serde(deny_unknown_fields)]` to [`ChildTask`]
    /// reds it on the foreign-key row. Each reds this test and nothing else in
    /// the crate (1 of 247).
    #[test]
    fn child_task_schema_shape() {
        let minimal: ChildTask =
            serde_json::from_value(json!({"task": "audit src/cost for dead code"}))
                .expect("task alone is a whole task");
        assert_eq!(
            minimal,
            ChildTask {
                task: "audit src/cost for dead code".to_owned(),
                name: None,
                tier: None,
                context: None,
            }
        );
        assert_eq!(
            keys(&serde_json::to_value(&minimal).unwrap()),
            ["task"],
            "absent optionals emit no key"
        );

        let full: ChildTask = serde_json::from_value(json!({
            "task": "review the diff for correctness",
            "name": "correctness",
            "tier": "build",
            "context": "the diff touches crates/tetond/src/cost",
        }))
        .expect("the full form parses");
        assert_eq!(full.name.as_deref(), Some("correctness"));
        assert_eq!(full.tier, Some(Tier::Build));
        assert_eq!(
            full.context.as_deref(),
            Some("the diff touches crates/tetond/src/cost")
        );
        let wire = serde_json::to_value(&full).unwrap();
        assert_eq!(keys(&wire), ["context", "name", "task", "tier"]);
        assert_eq!(wire["tier"], "build");
        assert_eq!(serde_json::from_value::<ChildTask>(wire).unwrap(), full);

        for (spelling, tier) in [
            ("reflex", Tier::Reflex),
            ("scan", Tier::Scan),
            ("build", Tier::Build),
            ("think", Tier::Think),
        ] {
            let task: ChildTask =
                serde_json::from_value(json!({"task": "t", "tier": spelling})).unwrap();
            assert_eq!(task.tier, Some(tier), "{spelling}");
        }
        for bad in ["frontier", "Build", "BUILD", ""] {
            assert!(
                serde_json::from_value::<ChildTask>(json!({"task": "t", "tier": bad})).is_err(),
                "tier `{bad}` must not parse"
            );
        }

        assert!(
            serde_json::from_value::<ChildTask>(json!({"name": "no-task"})).is_err(),
            "`task` is required"
        );
        assert!(
            serde_json::from_value::<ChildTask>(json!({"task": null})).is_err(),
            "`task` is a string, not an optional"
        );

        let empty: ChildTask = serde_json::from_value(json!({"task": ""})).unwrap();
        assert!(
            empty.task.is_empty(),
            "the tool refuses it typed, not serde"
        );

        let foreign: ChildTask = serde_json::from_value(json!({
            "task": "t",
            "subagent_type": "code-reviewer",
            "run_in_background": true,
        }))
        .expect("a stray key is ignored");
        assert_eq!(keys(&serde_json::to_value(&foreign).unwrap()), ["task"]);
    }

    /// The id is `call_id/name`, a bare string on the wire, and the same string
    /// through `Display`, `as_str` and a restore from storage.
    ///
    /// **Mutations (run 2026-10-05):** minting with `:` instead of `/` reds
    /// three: this test, and the two `events.rs` tests that assert the id as it
    /// appears on the wire (`agent_events_round_trip_their_wire_names`,
    /// `child_ids_are_optional_and_omitted_when_none`). Dropping `#[serde(transparent)]` reds
    /// **nothing** — `serde_json` already writes a one-field tuple struct as its
    /// inner value — so the attribute states the wire form rather than
    /// providing it, as it does on every `id_newtype!` id; the bare-string
    /// assertion is on the property, not the attribute.
    #[test]
    fn child_id_is_call_id_slash_name_and_a_bare_string_on_the_wire() {
        let id = ChildId::new("toolu_01", "audit-1");
        assert_eq!(id.as_str(), "toolu_01/audit-1");
        assert_eq!(id.to_string(), "toolu_01/audit-1");
        assert_eq!(
            serde_json::to_value(&id).unwrap(),
            json!("toolu_01/audit-1")
        );
        let back: ChildId = serde_json::from_value(json!("toolu_01/audit-1")).unwrap();
        assert_eq!(back, id);
        assert_eq!(ChildId::from("toolu_01/audit-1".to_owned()), id);
        // A name holding the separator is kept whole — which is why there is
        // no split.
        assert_eq!(ChildId::new("c", "a/b").as_str(), "c/a/b");
    }

    /// A result round-trips with every optional present and with none; the
    /// absent ones emit no key, and the report is always present.
    #[test]
    fn child_result_round_trips_with_and_without_optionals() {
        let completed = ChildResult {
            name: "audit-1".to_owned(),
            status: ChildStatus::Completed,
            report: "no dead code found".to_owned(),
            refusal: None,
            error: None,
            turns_used: 4,
            route: Some(ChildRoute {
                tier: Some(Tier::Build),
                provider_id: ProviderId::from("anthropic"),
                model: "claude-sonnet".to_owned(),
            }),
            bounds: Some(ChildBounds {
                max_turns: 12,
                context_budget_bytes: 397_952,
                spend_ceiling_micro_cents: Some(125_000),
                deadline_secs: 600,
            }),
            cost_micro_cents: 41_000,
            spend_ceiling_final_micro_cents: Some(150_000),
        };
        let wire = serde_json::to_value(&completed).unwrap();
        assert_eq!(wire["status"], "completed");
        assert_eq!(wire["route"]["tier"], "build");
        assert_eq!(wire["bounds"]["deadline_secs"], 600);
        assert!(wire.get("refusal").is_none() && wire.get("error").is_none());
        assert_eq!(
            serde_json::from_value::<ChildResult>(wire).unwrap(),
            completed
        );

        let cancelled = ChildResult {
            name: "audit-2".to_owned(),
            status: ChildStatus::Cancelled,
            report: String::new(),
            refusal: None,
            error: None,
            turns_used: 0,
            route: None,
            bounds: None,
            cost_micro_cents: 0,
            spend_ceiling_final_micro_cents: None,
        };
        let wire = serde_json::to_value(&cancelled).unwrap();
        assert_eq!(
            keys(&wire),
            ["cost_micro_cents", "name", "report", "status", "turns_used"]
        );
        assert_eq!(
            serde_json::from_value::<ChildResult>(wire).unwrap(),
            cancelled
        );

        assert_eq!(
            completed.route.as_ref().unwrap().to_string(),
            "build anthropic/claude-sonnet"
        );
    }

    /// The four refusal codes the spec names, each carrying its numbers under
    /// a `kind` tag that agrees with [`AgentRefusal::code`]; a fifth code does
    /// not parse.
    #[test]
    fn agent_refusal_codes_are_the_four_the_spec_names() {
        for (refusal, code) in [
            (
                AgentRefusal::TooManyChildren {
                    requested: 6,
                    cap: 5,
                },
                "too_many_children",
            ),
            (
                AgentRefusal::ChildCapReached {
                    started: 5,
                    requested: 4,
                    cap: 8,
                },
                "child_cap_reached",
            ),
            (
                AgentRefusal::DuplicateName {
                    name: "audit".to_owned(),
                },
                "duplicate_name",
            ),
            (AgentRefusal::EmptyTask { index: 2 }, "empty_task"),
        ] {
            assert_eq!(refusal.code(), code);
            let wire = serde_json::to_value(&refusal).unwrap();
            assert_eq!(wire["kind"], code, "{wire}");
            assert_eq!(
                serde_json::from_value::<AgentRefusal>(wire).unwrap(),
                refusal
            );
        }
        let wire = serde_json::to_value(AgentRefusal::TooManyChildren {
            requested: 6,
            cap: 5,
        })
        .unwrap();
        assert_eq!(
            wire,
            json!({"kind": "too_many_children", "requested": 6, "cap": 5})
        );
        assert!(serde_json::from_value::<AgentRefusal>(json!({"kind": "nested_agent"})).is_err());
    }
}
