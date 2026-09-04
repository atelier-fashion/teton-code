//! **REQ-615 AC-8 / BR-9 — the 2026-09-04 session, replayed against the gates.**
//!
//! The REQ exists because of one transcript. A session launched from `~` ran
//! `cd ~/GitHub/teton-code && pwd` five times, was told the project path five
//! times, and ran every `ls`, `find`, `git status` and `glob` in between from
//! the home folder. Along the way it created `~/.adlc/context` and invoked
//! `/init` four times, because `/analyze`'s preamble found no `.adlc/` where it
//! was looking.
//!
//! This replays that tool sequence call-for-call and asserts **the harness's
//! answers**.
//!
//! # Why the assertions are on the outputs and never on the call count
//!
//! The obvious thing to assert — "the model stops after one `cd`" — cannot be
//! asserted here and would be vacuous if it were. The sequence below is a
//! script: how many `cd`-bearing calls it makes is a property of this file, not
//! of the daemon, so an assertion on that number would be the test checking its
//! own fixture (conventions.md: never let the expected value be computed by the
//! subject). What the daemon controls is what comes *back*, and that is what
//! every assertion below reads.
//!
//! The control leg is the same sequence at a **project** root (BR-9): the write
//! lands, the skill expands, and nothing is refused.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::json;
use tokio::runtime::Handle;

use teton_protocol::methods::RootKind;
use teton_protocol::SessionId;

use tetond::broadcast::EventBus;
use tetond::harness::permissions::{
    PendingPermissions, PermissionConfig, PermissionGate, PermissionPolicy,
};
use tetond::harness::tools::{EditTool, ShellTool, SkillTool, Tool, ToolContext};
use tetond::skills::{discover, RealFs, SkillRegistry};

/// A throwaway `home` with a `GitHub/teton-code` project inside it — the shape
/// the transcript's machine had.
struct Machine {
    root: PathBuf,
}

impl Machine {
    fn new(tag: &str) -> Self {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!("req615-{tag}-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(root.join("home/GitHub/teton-code/.git")).unwrap();
        Self { root }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn project(&self) -> PathBuf {
        self.root.join("home/GitHub/teton-code")
    }

    /// `/analyze` as it actually ships: no `requires:` key, and a preamble that
    /// reads `.adlc/`. The compatibility path BR-5 recognises it by.
    fn write_analyze_skill(&self) {
        let dir = self.home().join(".claude/skills/analyze");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: analyze\ndescription: codebase health audit\n---\n\
             !`cat .adlc/context/architecture.md 2>/dev/null || echo \"No architecture context found\"`\n\
             Audit the codebase.\n",
        )
        .unwrap();
    }

    fn registry(&self, repo: &Path) -> Arc<SkillRegistry> {
        Arc::new(discover(Some(&self.home()), repo, RootKind::Plain, &RealFs))
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).ok();
    }
}

fn gate() -> Arc<PermissionGate> {
    Arc::new(PermissionGate::new(
        SessionId::from("req615-replay"),
        PermissionConfig::with_default(PermissionPolicy::Allow),
        Arc::new(EventBus::new()),
        Arc::new(PendingPermissions::new()),
    ))
}

/// **AC-8: the 2026-09-04 sequence is answered by the gates.**
///
/// Four calls from the transcript, in the order it recorded them. Each
/// assertion is on the harness's answer.
///
/// Mutation: revert any one of BR-2, BR-4 or BR-5 — the corresponding leg goes
/// red, and the `.adlc` inspection is the one that cannot be satisfied by an
/// error message alone.
// `flavor = "multi_thread"`: `Tool::run` is the sync→async bridge and uses
// `block_in_place`, which a current-thread runtime refuses.
#[tokio::test(flavor = "multi_thread")]
async fn the_2026_09_04_sequence_is_answered_by_the_gates() {
    let machine = Machine::new("replay");
    machine.write_analyze_skill();

    // The session root is the home folder — the whole premise of the defect.
    let ctx = ToolContext::new(machine.home())
        .with_root_kind(RootKind::Home)
        .with_known_projects(vec!["teton-code".to_owned()]);
    let shell = ShellTool::default();

    // 1. `cd /teton-code && pwd` — the first thing the session ran, against a
    //    directory that does not exist.
    let first = shell.run(&ctx, &json!({ "command": "cd /teton-code && pwd" }));
    assert!(
        first
            .content
            .contains("the next command starts there again"),
        "a failed `cd` still ran in the root, and the model needs that fact as \
         much as a successful one does:\n{}",
        first.content
    );

    // 2. `cd ~/GitHub/teton-code && pwd` — the call that printed the project
    //    path and convinced the model it had moved. It still prints the path;
    //    what changed is that the result now says the move did not stick.
    let moved = shell.run(
        &ctx,
        &json!({ "command": format!("cd {} && pwd", machine.project().display()) }),
    );
    assert!(
        moved
            .content
            .contains(&machine.project().display().to_string()),
        "the command still does what it did — this REQ adds a fact, it does not \
         take the output away:\n{}",
        moved.content
    );
    assert!(
        moved
            .content
            .contains("the next command starts there again"),
        "and the result now contradicts the reading the model took from it:\n{}",
        moved.content
    );

    // 3. `mkdir -p .adlc/context .adlc/specs` — the call that put a project
    //    skeleton in the user's home folder.
    let scaffold = shell.run(
        &ctx,
        &json!({ "command": "mkdir -p .adlc/context .adlc/specs" }),
    );
    assert!(scaffold.is_error, "{}", scaffold.content);
    assert!(
        !machine.home().join(".adlc").exists(),
        "AC-3's instruction: inspect the artifact. `~/.adlc` must not exist, \
         which an error message alone cannot demonstrate"
    );
    assert!(
        scaffold.content.contains("/cd <name>"),
        "the refusal names the act that fixes it:\n{}",
        scaffold.content
    );

    // 3b. The same rule through the other door.
    let edited = EditTool::default().run(
        &ctx,
        &json!({ "path": "notes.md", "old_string": "a", "new_string": "b" }),
    );
    assert!(edited.is_error, "{}", edited.content);

    // 4. `/analyze` — the invocation whose preamble found no `.adlc/` and sent
    //    the model to `/init` four times.
    let skill = SkillTool::new(
        machine.registry(&machine.home()),
        gate(),
        None,
        Handle::current(),
        5_000,
    );
    // Through `Tool::run`, the door a model-issued call actually comes in by.
    let analyzed = skill.run(&ctx, &json!({ "name": "analyze", "args": "" }));
    assert!(analyzed.is_error, "{}", analyzed.content);
    assert!(
        analyzed.content.contains("needs_project"),
        "`/analyze` needs a repository, and this session is not in one:\n{}",
        analyzed.content
    );
    assert!(
        analyzed.content.contains("/cd teton-code"),
        "and the refusal names the project the machine already knows about:\n{}",
        analyzed.content
    );
    assert!(
        !analyzed.content.contains("No architecture context found"),
        "the preamble must not have run — its fallback string reaching the model \
         is the original defect, one layer down:\n{}",
        analyzed.content
    );
}

/// **BR-9: a project session is unchanged by REQ-615.**
///
/// The benign path for the whole REQ, and the one that says the gates are about
/// *location* rather than about tightening the harness. Every call the test
/// above saw refused is made again from a project root and must succeed.
///
/// Mutation: widen `gates_writes` to any kind beyond `Home`/`FilesystemRoot` —
/// this goes red.
#[tokio::test(flavor = "multi_thread")]
async fn a_project_session_is_unchanged_by_req_615() {
    let machine = Machine::new("project");
    machine.write_analyze_skill();
    std::fs::create_dir_all(machine.project().join(".adlc/context")).unwrap();
    std::fs::write(
        machine.project().join(".adlc/context/architecture.md"),
        "the real architecture\n",
    )
    .unwrap();
    std::fs::write(machine.project().join("notes.md"), "a\n").unwrap();

    let ctx = ToolContext::new(machine.project()).with_root_kind(RootKind::Project);

    let scaffold =
        ShellTool::default().run(&ctx, &json!({ "command": "mkdir -p .adlc/specs/REQ-999" }));
    assert!(!scaffold.is_error, "{}", scaffold.content);
    assert!(machine.project().join(".adlc/specs/REQ-999").exists());

    let edited = EditTool::default().run(
        &ctx,
        &json!({ "path": "notes.md", "old_string": "a", "new_string": "b" }),
    );
    assert!(!edited.is_error, "{}", edited.content);
    assert_eq!(
        std::fs::read_to_string(machine.project().join("notes.md")).unwrap(),
        "b\n"
    );

    let skill = SkillTool::new(
        machine.registry(&machine.project()),
        gate(),
        None,
        Handle::current(),
        5_000,
    );
    // Through `Tool::run`, the door a model-issued call actually comes in by.
    let analyzed = skill.run(&ctx, &json!({ "name": "analyze", "args": "" }));
    assert!(
        !analyzed.content.contains("needs_project"),
        "a project session expands the shipped ADLC skills exactly as before:\n{}",
        analyzed.content
    );
}
