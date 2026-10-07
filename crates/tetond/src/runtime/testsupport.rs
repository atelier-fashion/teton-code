//! REQ-599: filesystem scratch helpers shared by the `runtime` module tree's
//! tests.
//!
//! Lifted out of `runtime/mod.rs`'s test module in step 2. They were reachable
//! there only through `super::`, which meant the first extracted module could
//! not take its tests with it — and BR-7 requires exactly that: a subsystem
//! moved to a new module takes its `#[cfg(test)]` bodies along, rather than
//! leaving them behind pointing at a module they no longer describe.
//!
//! `scratch_dir` has 27 call sites and `set_dir_readonly` 7, spread across test
//! modules that this REQ will land in different files, so a shared home is what
//! they already needed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use teton_core::config::Config;
use teton_protocol::methods::{ConfigUpdate, ProviderConfig};
use teton_protocol::{ProviderId, ProviderKind as ProtoProviderKind};

use crate::router::Router;
use crate::runtime::{apply_update, build_router};

/// A throwaway directory under the system temp dir, unique per test.
pub(super) fn scratch_dir(tag: &str) -> PathBuf {
    // pid + nanos alone can collide when two tests hit the same clock tick,
    // and this helper is shared by every `mod` below — including the ones
    // that seed a config file and then read it back, where a collision is
    // one test reading another's document. The counter is what the sibling
    // integration suites add for the same reason (`config_preservation.rs`,
    // `model_consent.rs`).
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "teton-loadcfg-{tag}-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Toggle a directory between `r-x` and `rwx` for the owner.
pub(super) fn set_dir_readonly(dir: &Path, readonly: bool) {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = if readonly { 0o555 } else { 0o755 };
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
}

// ---------------------------------------------------------------------------
// REQ-602 TASK-304 — config/router fixtures, shared for the same reason the
// scratch-dir helpers above are.
//
// They lived in `runtime/mod.rs`'s test module, reachable only through
// `super::`, which meant the four `snapshot_from_config` tests could not move
// to `views.rs` with the subject they describe (BR-7). Same shape as the
// original lift, one module tree later.
// ---------------------------------------------------------------------------

/// A stand-in for the machine's resolved transcript directory (REQ-611 AC-20).
///
/// `snapshot_from_config` takes the directory as an argument precisely so the
/// projection stays a function of its arguments; calling the real
/// [`crate::runtime::turn::effective_transcript_dir`] here would hand the suite
/// a different answer on every developer's machine, for no gain — the
/// composition itself is tested where it lives. Shared rather than written per
/// module for [`router_for_config`]'s reason.
pub(super) fn a_transcript_dir() -> &'static Path {
    Path::new("/var/tmp/teton-snapshot-test/transcripts")
}

/// A router over `config` with a healthy local tier — what `config/get`
/// builds, minus the daemon.
pub(super) fn router_for_config(config: &Config) -> Router {
    build_router(config, true, &BTreeMap::new())
}

/// A config with one usable remote provider registered.
pub(super) fn config_with_remote(id: &str) -> Config {
    let mut config = Config::default();
    apply_update(
        &mut config,
        ConfigUpdate::RegisterProvider(ProviderConfig {
            id: ProviderId::from(id),
            kind: ProtoProviderKind::OpenaiCompatible,
            endpoint: Some("https://api.deepseek.com/v1/chat/completions".to_owned()),
            model: Some("deepseek-chat".to_owned()),
            auth_ref: None,
            max_context: None,
            context_budget_cap: None,
            allow_cleartext: None,
            floored_budget: None,
        }),
    );
    config
}

// ---------------------------------------------------------------------------
// REQ-603 — the session-root scratch helper, shared for the same reason the two
// groups above are.
//
// It lived in `runtime/mod.rs`'s `tests::conversation_carry`, reachable only
// through `super::`, which meant `session.rs` could not take
// `the_session_root_is_probed_from_the_cwd_or_the_daemon_fallback` with the
// subject that test describes (BR-7). Nine of `conversation_carry`'s own tests
// still call it, so a shared home is what it already needed — the third time
// this module tree has met that shape.
// ---------------------------------------------------------------------------

// -- REQ-583: the session root through the runtime -------------------
//
// The banner came with the helper. It sat above `scratch_root` in
// `conversation_carry` and is the id `traceability_sweep.rs` records as
// annotating it — taking the `///` block alone would have left the rationale
// behind, which is LESSON-594 and the exact mistake `turn.rs`'s header records
// making once already. The banner also still stands over the REQ-583 tests that
// remained in `conversation_carry`; both statements are true.

/// A unique scratch directory; the caller removes it. Holds a project
/// marker when `project` is set, so the probe classifies it as one.
pub(super) fn scratch_root(tag: &str, project: bool) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "teton-runtime-root-{tag}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    if project {
        std::fs::write(dir.join("Cargo.toml"), "[package]\n").unwrap();
    }
    dir
}

/// The registry the daemon builds for one turn — `build_tools` itself — over
/// a session whose root holds one model-invocable skill, so `skill` is
/// registered and a child keeping it is a real claim (REQ-623 BR-2, BR-12,
/// BR-14).
///
/// `child` picks the toolset: a child's registry (`ToolSet::Child`, built
/// through a child's context) or a prompt turn's. `agent_enabled` is the
/// session's `[agent] enabled`. Shared by `turn.rs`'s registry tests and the
/// `agent` tool's own, which may not reach `runtime::*` in production but
/// must be able to assert what the runtime's registry holds.
pub(crate) async fn turn_registry(
    child: bool,
    agent_enabled: bool,
) -> crate::harness::tools::ToolRegistry {
    use std::sync::Arc;

    use teton_protocol::SessionMode;

    use crate::broadcast::EventBus;
    use crate::runtime::turn::{ParentTurn, ToolSet};
    use crate::runtime::DaemonRuntime;
    use crate::sessions::SessionRegistry;
    use crate::skills::{discover, RealFs};
    use crate::turn_context::TurnContext;

    let runtime = Arc::new(DaemonRuntime::minimal());
    runtime.config.lock().expect("config mutex").agent.enabled = agent_enabled;
    let root = scratch_root("toolset", true);
    std::fs::create_dir_all(root.join(".claude/skills/survey")).unwrap();
    std::fs::write(
        root.join(".claude/skills/survey/SKILL.md"),
        "---\ndescription: survey the tree\n---\n\nSurvey it.\n",
    )
    .unwrap();
    let probed = runtime.session_root_for(Some(&root));
    let skills = Arc::new(discover(None, &probed.path, probed.view.kind, &RealFs));
    let config = runtime.config.lock().expect("config mutex").clone();
    let events = Arc::new(EventBus::new());
    let sessions = SessionRegistry::new();
    let session_id = sessions
        .create(SessionMode::Freeform, None, Some(root))
        .expect("a session")
        .session_id;
    let router = runtime.turn_router(&config, &session_id);
    let gate = runtime.permission_gate_for(&session_id, &events, &config);
    let turn_id = teton_protocol::TurnId::from("turn-1");
    let child_turn = crate::harness::ChildTurn {
        child_id: teton_protocol::agent::ChildId::new("call-1", "c"),
        parent_turn_id: turn_id.clone(),
        max_turns: 12,
        spend: crate::cost::ChildSpend::new(
            teton_protocol::agent::ChildId::new("call-1", "c"),
            crate::cost::SharePool::new(None, &[]),
            None,
        ),
        model_calls: Arc::default(),
        route: crate::harness::child::ChildRouteCell::default(),
    };
    let tctx = TurnContext::new(&events, &session_id, &config, &router, &gate, None);
    let (tctx, toolset) = if child {
        (tctx.for_child(&child_turn), ToolSet::Child)
    } else {
        (
            tctx,
            ToolSet::Prompt(ParentTurn {
                turn_id: &turn_id,
                sessions: &sessions,
                mode: SessionMode::Freeform,
                phase: None,
                typed: true,
                prompt_spend: None,
            }),
        )
    };
    runtime.build_tools(tctx, skills, toolset).await
}
