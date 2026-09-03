//! REQ-611 acceptance: the daemon writes the session's transcript itself.
//!
//! Every behavioural test here spawns the **real** `teton-code` binary against a
//! temp workspace whose `[transcript]` table names a directory inside that
//! workspace, drives it over the socket, and then reads the JSONL file **with a
//! stock JSON parser and no teton code** — which is AC-17's claim as a property
//! of the instrument rather than as a separate assertion. Nothing here inspects
//! the sink through its Rust API: the file is the deliverable, so the file is
//! what is read.
//!
//! ## Why the directory is configured rather than defaulted
//!
//! `[transcript] dir` is set on every fixture, and it is not a convenience. The
//! default is the machine's **data** directory (ADR-4) — on macOS
//! `~/Library/Application Support/teton/transcripts`, the developer's own — and
//! a suite that exercised the default would write the tester's home directory
//! and prune it. The `dir` key is a shipped setting on the same code path, so
//! the only thing not covered by choosing it here is `effective_dir`'s
//! defaulting branch, which is a pure function tested where it lives
//! (`teton_core::config`).
//!
//! ## AC → test map
//!
//! | rule | test |
//! |------|------|
//! | BR-2 (and AC-1's filesystem inspection) | [`a_session_created_under_enabled_true_opens_a_file_and_under_false_opens_nothing`] |
//! | BR-4 | [`no_turn_loop_publish_carries_prompt_or_tool_content`] |
//! | BR-10 | [`permission_decided_and_grant_records_carry_no_secret`] |
//! | BR-11 | [`the_transcript_module_never_calls_the_redactor`] |
//! | AC-2 | [`one_prompt_one_tool_call_yields_a_complete_file`] |
//! | AC-8 | [`two_sessions_never_share_a_file_and_daemon_events_appear_in_neither`] |
//! | AC-18 | [`orderly_shutdown_closes_the_file_and_sigkill_leaves_one_partial_line`] |
//!
//! BR-5's tap half (a full channel never delays a publish) is a unit test in
//! `broadcast.rs`, where the bus is: an integration test would have to force the
//! sink's channel full through a seam that does not exist on the wire, and would
//! be asserting about a fixture rather than about the bus.
//!
//! ## The two closing reasons, and a deviation worth stating
//!
//! The spec's AC-2 expects a file to end with `transcript_closed { session_ended
//! }`. **This daemon has no session-removal path**: `SessionRegistry` exposes no
//! removal at all, and a session lives as long as the daemon does (sessions
//! outlive their creating client by design — REQ-568). So the reason a shipped
//! transcript closes with is `daemon_shutdown`, and that is what these tests
//! assert. `CloseReason::SessionEnded` exists in the sink and is exercised by
//! its own unit tests; wiring it needs a session end to wire it to.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

#[path = "e2e/harness.rs"]
mod harness;

use harness::{
    category_block, openai_turn, remote_provider_block, tier_block, Daemon, DaemonOptions,
    MockProvider, MockResponse, Workspace,
};

/// How long a file, a record or a process exit is waited for. Generous for a
/// loaded CI runner, finite so that a test cannot pass by waiting.
const WINDOW: Duration = Duration::from_secs(20);

/// The four tiers, all bound to one provider — a fixture that does not depend on
/// which category a prompt happens to classify as.
fn every_tier_bound_to(provider: &str) -> String {
    ["reflex", "scan", "build", "think"]
        .iter()
        .map(|tier| tier_block(tier, provider))
        .collect()
}

/// The `[transcript]` table for a workspace-local directory.
///
/// `retain_days = 0` on every fixture: pruning is BR-13's subject and has its
/// own tests, and a suite that left it at the shipped 30 would be running a
/// deletion pass over a directory these tests are reading.
fn transcript_block(dir: &Path, enabled: bool) -> String {
    format!(
        "[transcript]\nenabled = {enabled}\ndir = \"{}\"\nretain_days = 0\n\n",
        dir.display()
    )
}

/// The deterministic machine every daemon here is spawned onto, so a probe of
/// the real host cannot pick a model and spend the test's budget loading it.
fn probe() -> DaemonOptions {
    DaemonOptions::default()
        .env("TETON_PROBE_RAM_BYTES", (16u64 << 30).to_string())
        .env("TETON_PROBE_DISK_BYTES", (500u64 << 30).to_string())
        .env("TETON_PROBE_GPU", "apple-silicon")
}

/// Where this workspace's transcripts go.
fn transcript_dir(workspace: &Workspace) -> PathBuf {
    workspace.root.join("transcripts")
}

/// Write a config binding every tier to `provider` and turning transcripts on
/// (or off), and return the transcript directory.
///
/// `duties` takes the `title` category, and that is not decoration. The session
/// naming duty runs on its **own detached task**, so a suite with one provider
/// has the turn and the duty racing for the same scripted response — which is
/// how the first draft of the AC-2 test got its tool call answered by the
/// namer and a plain "Done." handed to the turn. Giving the duty a provider of
/// its own removes the race rather than sleeping through it (LESSON-591: do not
/// pin detached-task positions; here, keep them out of the fixture entirely).
fn write_config(
    workspace: &Workspace,
    provider: &MockProvider,
    duties: &MockProvider,
    enabled: bool,
) -> PathBuf {
    write_config_in(
        workspace,
        provider,
        duties,
        enabled,
        &transcript_dir(workspace),
    )
}

/// [`write_config`] with the transcript directory named explicitly.
///
/// The AC-11 fixtures need a directory that is *not* the workspace default —
/// one whose parent is a file, one that already exists `0o755`, one inside the
/// session root — and each of those is a property of the path rather than of
/// the rest of the config. Split out so those three legs and the ordinary
/// fixture share one config writer: a second writer would be a second place for
/// the tier bindings to drift.
fn write_config_in(
    workspace: &Workspace,
    provider: &MockProvider,
    duties: &MockProvider,
    enabled: bool,
    dir: &Path,
) -> PathBuf {
    let dir = dir.to_path_buf();
    let mut config = String::new();
    config.push_str(&remote_provider_block(
        "mock",
        &provider.openai_endpoint(),
        "deepseek-chat",
    ));
    config.push_str(&remote_provider_block(
        "duties",
        &duties.openai_endpoint(),
        "deepseek-chat",
    ));
    config.push_str(&every_tier_bound_to("mock"));
    config.push_str(&category_block("title", "duties"));
    config.push_str(&transcript_block(&dir, enabled));
    workspace.write_config(&config);
    dir
}

/// A provider for the duties that run beside a turn, answering anything with
/// one short line.
fn duty_provider() -> MockProvider {
    MockProvider::always(openai_turn("Demo Session", None, 20, 4))
}

/// Spawn a daemon with a scripted local tier, so the first-run consent gate is
/// exempt and no proposal stands between the client and its turn.
fn spawn(workspace: &Workspace, options: DaemonOptions) -> Daemon {
    let script = workspace.write_script("unused — every tier is bound to the mock provider.");
    Daemon::spawn(workspace, options.script(script))
}

// ---------------------------------------------------------------------------
// Reading a transcript, the way a stranger would (AC-17)
// ---------------------------------------------------------------------------

/// Every `.jsonl` file in `dir`, sorted, or an empty list if `dir` is absent.
fn transcript_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(listing) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = listing
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    files.sort();
    files
}

/// The file `session` writes to, once it exists **and holds its first record**.
///
/// Both halves are needed. `Writer::open` creates the file and then writes
/// `transcript_opened` into it, so between those two syscalls a poller that
/// stopped at "the path exists" reads an empty file — which is a race a test
/// loses on a loaded runner and wins on a quiet one, the worst kind.
fn await_file(dir: &Path, session: &str) -> PathBuf {
    let deadline = Instant::now() + WINDOW;
    loop {
        let candidate = transcript_files(dir).into_iter().find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains(session))
        });
        if let Some(path) = candidate {
            let has_a_record = std::fs::read_to_string(&path).is_ok_and(|text| {
                text.lines()
                    .any(|line| serde_json::from_str::<Value>(line).is_ok())
            });
            if has_a_record {
                return path;
            }
        }
        assert!(
            Instant::now() < deadline,
            "no transcript for session {session} appeared in {}",
            dir.display()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// One JSONL file's complete records, plus whatever trailing bytes did not
/// parse as a line.
///
/// The split is BR-14's contract made mechanical: **a partial trailing line is
/// the only malformation a reader must expect**, so a helper that silently
/// tolerated one anywhere would make AC-18's "at most one partial line"
/// unassertable. Anything unparseable that is *not* last is a panic here.
fn read_transcript(path: &Path) -> (Vec<Value>, Option<String>) {
    let text = std::fs::read_to_string(path).expect("the transcript is readable");
    let mut records = Vec::new();
    let mut partial = None;
    let lines: Vec<&str> = text.lines().collect();
    for (index, line) in lines.iter().enumerate() {
        match serde_json::from_str::<Value>(line) {
            Ok(value) => records.push(value),
            Err(err) => {
                assert_eq!(
                    index,
                    lines.len() - 1,
                    "only the LAST line may be partial; line {} of {} did not parse ({err}): {line}",
                    index + 1,
                    lines.len()
                );
                partial = Some((*line).to_owned());
            }
        }
    }
    (records, partial)
}

/// Wait until `session`'s transcript holds a record of every named kind, then
/// return the whole file.
fn await_kinds(dir: &Path, session: &str, kinds: &[&str]) -> Vec<Value> {
    let path = await_file(dir, session);
    let deadline = Instant::now() + WINDOW;
    loop {
        let (records, _) = read_transcript(&path);
        let missing: Vec<&&str> = kinds
            .iter()
            .filter(|kind| !records.iter().any(|r| r["kind"] == ***kind))
            .collect();
        if missing.is_empty() {
            return records;
        }
        assert!(
            Instant::now() < deadline,
            "{} never held {missing:?}; it holds {:?}",
            path.display(),
            kinds_of(&records)
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// The `kind` of every record, in file order — the shape every failure message
/// here prints, because "which records are in the file" is the first question.
fn kinds_of(records: &[Value]) -> Vec<&str> {
    records
        .iter()
        .filter_map(|r| r["kind"].as_str())
        .collect::<Vec<_>>()
}

/// The index of the first record of `kind`.
fn first_index(records: &[Value], kind: &str) -> Option<usize> {
    records.iter().position(|r| r["kind"] == kind)
}

/// BR-14's envelope: every line carries `n`, `ts`, `session_id` and `kind`, and
/// `n` runs 1..k with no holes (AC-2, AC-17).
fn assert_well_formed(records: &[Value], session: &str) {
    assert!(!records.is_empty(), "an empty transcript proves nothing");
    for (index, record) in records.iter().enumerate() {
        let n = u64::try_from(index + 1).expect("a test file is not that long");
        assert_eq!(
            record["n"].as_u64(),
            Some(n),
            "n must be contiguous from 1; record {index} is {record}"
        );
        assert!(
            record["ts"].as_str().is_some_and(|ts| ts.ends_with('Z')),
            "every record carries a UTC timestamp: {record}"
        );
        assert_eq!(
            record["session_id"].as_str(),
            Some(session),
            "every record in a file names the same session: {record}"
        );
        assert!(
            record["kind"].as_str().is_some(),
            "every record is self-describing: {record}"
        );
    }
}

// ---------------------------------------------------------------------------
// Source-scanning support for the two structural claims
// ---------------------------------------------------------------------------

/// A production source file, cut at its first column-0 `#[cfg(test)]`.
///
/// Conventions: a check whose own patterns appear in its own corpus matches
/// itself, and its vacuity floors can then never fire. Every structural claim
/// below is about **shipped** code, so the test module is not part of it.
fn production_source(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("unreadable {}: {err}", path.display()));
    match text.find("\n#[cfg(test)]") {
        Some(cut) => text[..cut].to_owned(),
        None => text,
    }
}

/// The slice of `source` from `start` through the first `end` after it.
///
/// Conventions: **bound the slice to the item you mean.** `end` is the item's
/// closing brace at its own indentation (`"\n}\n"` for a free function,
/// `"\n    }\n"` for a method), so the region is one body and never "the rest of
/// the file", which after a decomposition is other functions.
fn body<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
    let from = source
        .find(start)
        .unwrap_or_else(|| panic!("no item starting {start:?} — has it been renamed?"));
    let rest = &source[from..];
    let to = rest
        .find(end)
        .unwrap_or_else(|| panic!("no {end:?} closing the item starting {start:?}"));
    &rest[..to + end.len()]
}

// ---------------------------------------------------------------------------
// BR-2 / AC-1 — the durable default decides whether a file exists at all
// ---------------------------------------------------------------------------

/// **BR-2: `enabled = true` opens a file at session creation; `enabled = false`
/// creates nothing at all.**
///
/// The `true` leg sends **no prompt**. BR-2's durable half is read when the
/// session is created, so the file and its `transcript_opened` record must
/// exist before the session has done anything — a sink that opened lazily on
/// the first record would pass a test that prompted first.
///
/// The `false` leg runs a whole turn and then inspects the **filesystem**: no
/// directory, no file. That is AC-1's own instruction — assert by inspecting
/// the filesystem, not from the absence of a log line — and it is why the
/// negative leg drives a turn rather than merely creating a session.
///
/// **Mutation (run, red):** make `DaemonRuntime::transcript_session_created`
/// ignore its config and pass `enabled: true`. The `false` leg fails on the
/// directory existing. Restored.
#[test]
fn a_session_created_under_enabled_true_opens_a_file_and_under_false_opens_nothing() {
    // --- enabled = true: a file exists before the first prompt ---
    let provider = MockProvider::always(openai_turn("Done.", None, 120, 20));
    let duties = duty_provider();
    let recording = Workspace::new("transcript-on");
    let dir = write_config(&recording, &provider, &duties, true);
    let daemon = spawn(&recording, probe());
    let mut client = daemon.connect();
    let session = client.create_session("freeform", None);

    let path = await_file(&dir, &session);
    let (records, partial) = read_transcript(&path);
    assert_eq!(partial, None, "a live file has no partial line here");
    assert_eq!(
        records.first().map(|r| r["kind"].clone()),
        Some(Value::from("transcript_opened")),
        "the first record names the file's beginning: {:?}",
        kinds_of(&records)
    );
    assert_well_formed(&records, &session);
    drop(client);
    drop(daemon);

    // --- enabled = false: nothing on disk, after a whole turn ---
    let silent = Workspace::new("transcript-off");
    let silent_dir = write_config(&silent, &provider, &duties, false);
    let silent_daemon = spawn(&silent, probe());
    let mut silent_client = silent_daemon.connect();
    let silent_session = silent_client.create_session("freeform", None);
    let response = silent_client.prompt(&silent_session, "Say something.");
    assert!(
        response.get("result").is_some(),
        "the turn must run normally with no transcript: {response}"
    );
    silent_client.drain_events(Duration::from_millis(200));
    drop(silent_client);
    drop(silent_daemon);

    assert!(
        !silent_dir.exists(),
        "an opted-out daemon creates no transcript directory, found {}",
        silent_dir.display()
    );
}

// ---------------------------------------------------------------------------
// BR-4 — the content the sink gets is never added to the bus to get there
// ---------------------------------------------------------------------------

/// **BR-4: no publish in the turn path carries prompt text, tool arguments or a
/// tool result.**
///
/// A structural claim, and structural is the right instrument: the hazard is a
/// *future* edit that reaches for `self.bus.publish` inside a hand-off because
/// the bus handle is right there. Bounded to the bodies that gained hand-offs
/// (conventions: bound the slice to the item you mean) — the two loop functions
/// that call them, and the three `SessionEvents` methods themselves.
///
/// The pattern is `.publish(`, not the word "publish": `run_the_allowed_tool`'s
/// comments discuss events the *tool* published, and a word-match would be
/// asserting about prose. Its non-vacuity floor is the whole-file count, which
/// must be non-zero — the file does publish, elsewhere, so the pattern matches
/// real code.
///
/// **Mutation (run, red):** add `self.bus.publish(Some(self.session_id.clone()),
/// Event::SessionUpdate(...))` to `SessionEvents::tool_result`. The
/// `tool_result` region assertion fails. Restored.
#[test]
fn no_turn_loop_publish_carries_prompt_or_tool_content() {
    let source = production_source("src/harness/turn_loop.rs");
    assert!(
        source.matches(".publish(").count() > 0,
        "non-vacuity: the turn loop does publish somewhere, or this pattern is \
         not the one this codebase writes"
    );

    // The three hand-offs, and the shared `record` they go through.
    for (start, marker) in [
        ("    pub fn prompt_submitted(", "Record::PromptSubmitted"),
        ("    pub fn tool_input(", "Record::ToolCallInput"),
        ("    pub fn tool_result(", "Record::ToolResult"),
        ("    fn record(&self, record:", "sink.record("),
    ] {
        let region = body(&source, start, "\n    }\n");
        assert!(
            region.contains(marker),
            "{start} must still be the hand-off it is bounded as (looked for {marker})"
        );
        assert_eq!(
            region.matches(".publish(").count(),
            0,
            "BR-4: {start} must reach the sink and nothing else:\n{region}"
        );
    }

    // The two call sites, whose arguments are the content in question.
    for (start, marker) in [
        ("async fn serve_tool_call(", "events.tool_input("),
        ("async fn run_the_allowed_tool(", "events.tool_result("),
    ] {
        let region = body(&source, start, "\n}\n");
        assert!(
            region.contains(marker),
            "{start} must still carry its hand-off ({marker}) — a relocated call \
             keeps this test green while covering nothing (LESSON-598)"
        );
        assert_eq!(
            region.matches(".publish(").count(),
            0,
            "BR-4: {start} must not publish anything itself"
        );
    }
}

// ---------------------------------------------------------------------------
// BR-11 — the redact scan does not gate a transcript write
// ---------------------------------------------------------------------------

/// **BR-11: nothing in the transcript module, or in the hand-offs that feed it,
/// calls the redactor.**
///
/// The scan exists for egress and a transcript never egresses; running a
/// model-backed scan on every record would add a model call to a local write for
/// no boundary gain. The compensating controls are BR-1, BR-8, BR-9 and BR-10,
/// and `transcript_opened` records the redact posture so a reader knows what the
/// egress side was doing.
///
/// The task's shorthand is `grep -c redact … == 0`, which is not the check that
/// can be written: `SinkConfig::redact` is a *field* holding that posture, and
/// the module's prose names `[privacy] redact` to explain itself. So this keys
/// on the hazard — the call forms this codebase actually writes for the
/// redactor — and proves the search string real by finding it in the module that
/// does call it (LESSON-479).
///
/// **Mutation (run, red):** add `use crate::harness::redact::REDACT_DUTY;` to
/// `transcript/writer.rs`. The `redact::` form is found and the writer's leg
/// fails, naming the file and the form. Restored.
#[test]
fn the_transcript_module_never_calls_the_redactor() {
    const CALL_FORMS: &[&str] = &[
        "redact::",
        "RedactionVerdict",
        "RedactionGate",
        "harness::redact",
        "REDACT_DUTY",
    ];

    // The positive control: a module that does call the redactor.
    let caller = production_source("src/egress/lookup.rs");
    assert!(
        CALL_FORMS.iter().any(|form| caller.contains(form)),
        "non-vacuity: none of {CALL_FORMS:?} appears in a module that calls the \
         redactor, so this test would pass against a transcript that did"
    );

    for file in [
        "src/transcript/mod.rs",
        "src/transcript/record.rs",
        "src/transcript/writer.rs",
        "src/transcript/retention.rs",
    ] {
        let source = production_source(file);
        for form in CALL_FORMS {
            assert!(
                !source.contains(form),
                "BR-11: {file} reaches the redactor via {form}"
            );
        }
    }

    // And the hand-offs, which are where a scan would most plausibly be added:
    // they hold the prompt text and the tool result in the clear.
    let turn_loop = production_source("src/harness/turn_loop.rs");
    for start in [
        "    pub fn prompt_submitted(",
        "    pub fn tool_input(",
        "    pub fn tool_result(",
    ] {
        let region = body(&turn_loop, start, "\n    }\n");
        for form in CALL_FORMS {
            assert!(
                !region.contains(form),
                "BR-11: {start} reaches the redactor via {form}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// AC-2 — one prompt, one tool call, one complete file
// ---------------------------------------------------------------------------

/// **AC-2: a prompt that triggers a tool call produces a complete file.**
///
/// The daemon runs under its **real** lifetime and the client then leaves, so
/// the file is read after an orderly teardown and the whole of it — first record
/// to last — is in hand. Everything AC-2 lists is asserted: the record kinds
/// from both sources (in-process hand-offs and bus envelopes), `n` contiguous
/// from 1, and the three order relations the turn actually guarantees.
///
/// **Only those three.** A tool-using turn makes two model calls and prices
/// each, and several of these events are published from detached tasks — pinning
/// a fixed interleaving would be pinning the scheduler (LESSON-591).
///
/// The closing record's reason is `daemon_shutdown` rather than the spec's
/// `session_ended`; see this file's header for why that is the shipped answer.
///
/// **Mutation (run, red):** move the `prompt_submitted` hand-off out of
/// `run_prompt_turn`'s entry and into `run_attempts`, immediately after
/// `emit_route_decided` — "record the prompt where the route is announced". The
/// file then reads `route_decided, prompt_submitted` and the first order
/// relation fails, which is the point of asserting an order at all: every
/// required kind is still present in the mutated file. Restored.
#[test]
fn one_prompt_one_tool_call_yields_a_complete_file() {
    let provider = MockProvider::start(
        vec![MockResponse::ok(openai_turn(
            "Let me read the README.",
            Some(("c1", "read", r#"{"path":"README.md"}"#)),
            120,
            20,
        ))],
        MockResponse::ok(openai_turn("The README describes the demo.", None, 140, 30)),
    );

    let duties = duty_provider();
    let workspace = Workspace::new("transcript-ac2");
    let dir = write_config(&workspace, &provider, &duties, true);
    let mut daemon = spawn(&workspace, probe().real_lifetime());
    let mut client = daemon.connect();
    let session = client.create_session("freeform", None);
    let response = client.prompt(&session, "Summarize the README for me.");
    assert!(
        response.get("result").is_some(),
        "the turn must complete: {response}"
    );
    client.drain_events(Duration::from_millis(300));

    // Everything the turn produced is in the file before the daemon leaves;
    // waiting for the last of it here keeps the exit below from being the thing
    // that makes the test pass.
    await_kinds(
        &dir,
        &session,
        &[
            "prompt_submitted",
            "route_decided",
            "tool_call_input",
            "tool_result",
            "session_update",
            "cost_recorded",
        ],
    );

    let path = await_file(&dir, &session);
    drop(client);
    let status = daemon
        .wait_for_exit(WINDOW)
        .expect("the daemon exits on its own when its last client leaves");
    assert!(status.success(), "log:\n{}", daemon.log());

    let (records, partial) = read_transcript(&path);
    assert_eq!(partial, None, "an orderly exit leaves no partial line");
    assert_well_formed(&records, &session);
    let kinds = kinds_of(&records);

    assert_eq!(
        kinds.first(),
        Some(&"transcript_opened"),
        "the file opens with its own beginning: {kinds:?}"
    );
    assert_eq!(
        kinds.last(),
        Some(&"transcript_closed"),
        "the file ends with its own end: {kinds:?}"
    );

    // Both sources are represented: two in-process hand-offs the bus has never
    // carried, and the envelopes the tap recorded verbatim.
    for kind in [
        "prompt_submitted",
        "tool_call_input",
        "tool_result",
        "route_decided",
        "session_update",
        "cost_recorded",
    ] {
        assert!(
            kinds.contains(&kind),
            "AC-2 requires a {kind} record; the file holds {kinds:?}"
        );
    }
    // The streamed reply, which arrives as `session_update` envelopes.
    assert!(
        records
            .iter()
            .any(|r| r["kind"] == "session_update" && r["update"]["kind"] == "agent_message_chunk"),
        "the model's streamed text is in the file: {kinds:?}"
    );

    // Relation 1: the prompt precedes the route decided for it.
    let prompt_at = first_index(&records, "prompt_submitted").expect("asserted present above");
    let route_at = first_index(&records, "route_decided").expect("asserted present above");
    assert!(
        prompt_at < route_at,
        "prompt_submitted (n={}) must precede route_decided (n={}): {kinds:?}",
        prompt_at + 1,
        route_at + 1
    );

    // Relation 2: a call's input precedes its own result, matched by id.
    let input_at = records
        .iter()
        .position(|r| r["kind"] == "tool_call_input" && r["tool"] == "read")
        .expect("the scripted read was dispatched");
    let result_at = records
        .iter()
        .position(|r| {
            r["kind"] == "tool_result" && r["tool_call_id"] == records[input_at]["tool_call_id"]
        })
        .expect("the read returned");
    assert!(
        input_at < result_at,
        "a tool's input must precede its result: {kinds:?}"
    );

    // Relation 3: every price follows a model call that could have incurred it.
    for (index, record) in records.iter().enumerate() {
        if record["kind"] == "cost_recorded" {
            assert!(
                index > route_at,
                "cost_recorded at n={} precedes every route_decided: {kinds:?}",
                index + 1
            );
        }
    }

    // BR-7: the file holds one session's records and no daemon-scoped event.
    for daemon_scoped in ["model_lifecycle", "daemon_client_attach", "daemon_lifetime"] {
        assert!(
            !kinds.contains(&daemon_scoped),
            "a daemon-scoped {daemon_scoped} must not be in a session's file: {kinds:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// AC-8 — one file per session, and nothing daemon-wide in either
// ---------------------------------------------------------------------------

/// **AC-8: two sessions recording at once produce two files with no cross-talk,
/// and no daemon-scoped event appears in either.**
///
/// Both sessions run on one daemon, one bus and one writer, which is the
/// arrangement that could confuse them. Each prompt carries a marker only that
/// session asked for, so the negative ("B's content is not in A's file") is
/// decided by content rather than by counting.
///
/// **Mutation (run, red):** make the writer's `on_record` key its file by the
/// first session it knows rather than by the record's own — the "one file per
/// daemon" defect this test exists to catch. The second session's file is left
/// holding nothing but its `transcript_opened` and the wait for its records
/// fails. Restored.
///
/// **A mutation that did *not* go red, recorded because it says something:**
/// removing the writer's `record.envelope_session() != Some(session_id)` refusal
/// changes nothing here. The tap records every envelope under **the envelope's
/// own** `session_id`, so no bus-sourced record can reach a foreign file to be
/// refused. That check is the second of LESSON-502's two seams — unreachable
/// from the bus path today, and the one that would catch a future in-process
/// caller handing `record` the wrong session.
#[test]
fn two_sessions_never_share_a_file_and_daemon_events_appear_in_neither() {
    let provider = MockProvider::always(openai_turn("Understood.", None, 120, 20));
    let duties = duty_provider();
    let workspace = Workspace::new("transcript-ac8");
    let dir = write_config(&workspace, &provider, &duties, true);
    let daemon = spawn(&workspace, probe());
    let mut client = daemon.connect();

    let first = client.create_session("freeform", None);
    let second = client.create_session("freeform", None);
    client.prompt(&first, "MARKER-ALPHA: say something.");
    client.prompt(&second, "MARKER-BRAVO: say something else.");
    client.drain_events(Duration::from_millis(300));

    let alpha = await_kinds(&dir, &first, &["prompt_submitted", "cost_recorded"]);
    let bravo = await_kinds(&dir, &second, &["prompt_submitted", "cost_recorded"]);

    assert_eq!(
        transcript_files(&dir).len(),
        2,
        "two recording sessions, two files"
    );
    assert_well_formed(&alpha, &first);
    assert_well_formed(&bravo, &second);

    // No cross-talk, decided on content the other session named.
    let alpha_text = serde_json::to_string(&alpha).expect("records re-serialize");
    let bravo_text = serde_json::to_string(&bravo).expect("records re-serialize");
    assert!(
        alpha_text.contains("MARKER-ALPHA") && !alpha_text.contains("MARKER-BRAVO"),
        "the first session's file holds its own prompt and not the other's"
    );
    assert!(
        bravo_text.contains("MARKER-BRAVO") && !bravo_text.contains("MARKER-ALPHA"),
        "the second session's file holds its own prompt and not the other's"
    );
    assert!(
        !alpha_text.contains(second.as_str()) && !bravo_text.contains(first.as_str()),
        "neither file names the other session's id"
    );

    // BR-7: nothing daemon-scoped, in either file. Both daemons published these
    // — `daemon_client_attach` on this very connection's handshake — so the
    // absence is a filter working rather than an event that never happened.
    for records in [&alpha, &bravo] {
        let kinds = kinds_of(records);
        for daemon_scoped in ["model_lifecycle", "daemon_client_attach", "daemon_lifetime"] {
            assert!(
                !kinds.contains(&daemon_scoped),
                "daemon-scoped {daemon_scoped} in a session file: {kinds:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// BR-10 — the decision record, and what it must not carry
// ---------------------------------------------------------------------------

/// **BR-10: a permission answer is recorded as the decision and nothing else.**
///
/// The scripted turn calls `edit`, which asks at the shipped `guarded` level, and
/// the harness client answers `allow_once` the way a user would. Three claims:
///
/// 1. the `permission_decided` record carries exactly `request_id`, `option_id`
///    and `remembered` on top of the four envelope keys — **no grant key**,
///    which is the spec's OQ-4 draft answer: the key is not a credential, but
///    naming it in a file the user may share turns a decision record into a map
///    of what the session is permitted to do;
/// 2. the `permission_request` that preceded it is recorded as the **wire form
///    unchanged** — asserted by rebuilding the client's own received frame from
///    the file's line, which is a much stronger claim than "the fields look
///    right";
/// 3. no credential-shaped string appears anywhere in the transcript directory,
///    and no daemon-scoped `session_grant_minted` record appears at all.
///
/// Claim 3's sweep is meaningful **because this fixture's turn touches no
/// boundary file**: BR-11 is explicit that a tool result the session asked for
/// *is* recorded as the harness received it, so an `sk-` in a file whose session
/// read nothing secret could only be one the daemon put there itself.
///
/// **Mutation (run, red):** make
/// `harness::permissions::option_remembers_for_session` return `true` for every
/// id. The record then claims an `allow_once` was remembered for the session —
/// a file telling its reader the session is permitted more than it is — and
/// claim 1 fails. Restored.
#[test]
fn permission_decided_and_grant_records_carry_no_secret() {
    let provider = MockProvider::start(
        vec![MockResponse::ok(openai_turn(
            "I will change the constant.",
            Some((
                "c1",
                "edit",
                r#"{"path":"src/lib.rs","old_string":"pub const ANSWER: u32 = 1;","new_string":"pub const ANSWER: u32 = 2;"}"#,
            )),
            120,
            20,
        ))],
        MockResponse::ok(openai_turn("Done.", None, 140, 30)),
    );

    let duties = duty_provider();
    let workspace = Workspace::new("transcript-br10");
    let dir = write_config(&workspace, &provider, &duties, true);
    let daemon = spawn(&workspace, probe());
    let mut client = daemon.connect();
    let session = client.create_session("freeform", None);
    client.prompt(&session, "Set ANSWER to 2.");
    client.drain_events(Duration::from_millis(300));

    let records = await_kinds(
        &dir,
        &session,
        &["permission_request", "permission_decided"],
    );
    let kinds = kinds_of(&records);

    // 1. The decision, and its exact field set.
    let decided = records
        .iter()
        .find(|r| r["kind"] == "permission_decided")
        .expect("asserted present above");
    assert_eq!(decided["option_id"], "allow_once", "{decided}");
    assert_eq!(decided["remembered"], false, "{decided}");
    let keys: Vec<&String> = decided
        .as_object()
        .expect("a record is an object")
        .keys()
        .collect();
    let mut sorted: Vec<&str> = keys.iter().map(|k| k.as_str()).collect();
    sorted.sort_unstable();
    assert_eq!(
        sorted,
        vec![
            "kind",
            "n",
            "option_id",
            "remembered",
            "request_id",
            "session_id",
            "ts"
        ],
        "BR-10: the decision record carries the decision and nothing else, \
         and in particular no remembered-grant key: {decided}"
    );

    // 2. The request it answered, recorded as the wire form unchanged.
    let recorded = records
        .iter()
        .find(|r| r["kind"] == "permission_request")
        .expect("asserted present above");
    assert_eq!(
        recorded["request_id"], decided["request_id"],
        "the decision answers the request recorded before it: {kinds:?}"
    );
    let observed = client
        .events_named("permission_request")
        .into_iter()
        .find(|event| event["request_id"] == recorded["request_id"])
        .cloned()
        .expect("the client received the prompt it answered");
    assert_eq!(
        wire_form_of(recorded),
        observed,
        "a bus-sourced record must be the envelope the client saw, verbatim"
    );

    // 3. Nothing credential-shaped, and nothing daemon-scoped, anywhere here.
    assert!(
        !kinds.contains(&"session_grant_minted"),
        "a grant mint is daemon-scoped and belongs in no session's file: {kinds:?}"
    );
    for path in transcript_files(&dir) {
        let text = std::fs::read_to_string(&path).expect("readable");
        for shape in ["sk-", "Bearer ", "Authorization", "keychain:"] {
            assert!(
                !text.contains(shape),
                "credential-shaped {shape:?} in {}",
                path.display()
            );
        }
    }
}

/// Rebuild the wire envelope a bus-sourced record was written from.
///
/// The line carries `n` and `ts` the sink minted and spells the envelope's
/// `event` as `kind`; everything else — `seq`, `session_id`, and the payload —
/// is the envelope's own. Reversing exactly those three is what makes "recorded
/// verbatim" checkable by equality rather than by inspection.
fn wire_form_of(record: &Value) -> Value {
    let mut object: Map<String, Value> = record
        .as_object()
        .expect("a record is an object")
        .clone()
        .into_iter()
        .filter(|(key, _)| key != "n" && key != "ts" && key != "kind")
        .collect();
    object.insert("event".to_owned(), record["kind"].clone());
    Value::Object(object)
}

// ---------------------------------------------------------------------------
// AC-18 — the two ways a daemon can stop
// ---------------------------------------------------------------------------

/// **AC-18: an orderly shutdown closes the file; a `SIGKILL` leaves at most one
/// partial trailing line.**
///
/// Both legs run the same session shape so the difference between them is only
/// how the process ended. The orderly leg asserts the *last line* is
/// `transcript_closed { daemon_shutdown }` — not merely that one exists — which
/// is what makes it a claim about the teardown order: the close is written
/// before the socket is unlinked and before `_exit` skips every destructor.
///
/// The kill leg asserts what BR-14 permits and no more. Every record is flushed
/// as it is written, so in practice the file is whole; the assertion is `<= 1`
/// unparseable line because that is the contract a reader is given, and a
/// stricter one would be a claim about buffering rather than about the format.
///
/// **Mutation (run, red):** delete `daemon.runtime.shutdown_transcripts().await`
/// from `main::shutdown`. The orderly leg's file ends with the turn's last
/// record and no `transcript_closed`, so the assertion on the last line fails.
/// Restored.
#[test]
fn orderly_shutdown_closes_the_file_and_sigkill_leaves_one_partial_line() {
    // --- orderly: the daemon leaves with its last client ---
    let provider = MockProvider::always(openai_turn("Done.", None, 120, 20));
    let duties = duty_provider();
    let workspace = Workspace::new("transcript-ac18");
    let dir = write_config(&workspace, &provider, &duties, true);
    let mut daemon = spawn(&workspace, probe().real_lifetime());
    let mut client = daemon.connect();
    let session = client.create_session("freeform", None);
    client.prompt(&session, "Say something.");
    client.drain_events(Duration::from_millis(200));
    let path = await_file(&dir, &session);

    drop(client);
    let status = daemon
        .wait_for_exit(WINDOW)
        .expect("the daemon exits on its own when its last client leaves");
    assert!(status.success(), "log:\n{}", daemon.log());

    let (records, partial) = read_transcript(&path);
    assert_eq!(partial, None, "an orderly exit leaves no partial line");
    let last = records.last().expect("the file has records");
    assert_eq!(
        last["kind"],
        "transcript_closed",
        "the last line of an orderly shutdown says the file is finished: {:?}",
        kinds_of(&records)
    );
    assert_eq!(
        last["reason"], "daemon_shutdown",
        "and says which way the daemon went: {last}"
    );
    assert_eq!(
        last["records"].as_u64(),
        Some(u64::try_from(records.len()).expect("a test file is not that long")),
        "the closing record states the final n: {last}"
    );

    // --- SIGKILL: no signal handler, no flush, no destructor ---
    let killed_workspace = Workspace::new("transcript-ac18-kill");
    let killed_dir = write_config(&killed_workspace, &provider, &duties, true);
    let killed = spawn(&killed_workspace, probe());
    let mut killed_client = killed.connect();
    let killed_session = killed_client.create_session("freeform", None);
    killed_client.prompt(&killed_session, "Say something.");
    killed_client.drain_events(Duration::from_millis(200));
    let killed_path = await_file(&killed_dir, &killed_session);

    // `Daemon::drop` is a `kill(2)`, which is SIGKILL: uncatchable, so the
    // daemon runs none of its teardown.
    drop(killed_client);
    drop(killed);

    let text = std::fs::read_to_string(&killed_path).expect("the transcript survives the kill");
    let unparseable = text
        .lines()
        .filter(|line| serde_json::from_str::<Value>(line).is_err())
        .count();
    assert!(
        unparseable <= 1,
        "a SIGKILL may leave at most one partial trailing line, found {unparseable} in {}",
        killed_path.display()
    );
    let (killed_records, _) = read_transcript(&killed_path);
    assert!(
        !killed_records.is_empty(),
        "the killed daemon still wrote what it had"
    );
    assert_ne!(
        killed_records.last().map(|r| r["kind"].clone()),
        Some(Value::from("transcript_closed")),
        "nothing closed this file — which is what makes the orderly leg above a \
         claim about the teardown rather than about the writer"
    );
}

// ---------------------------------------------------------------------------
// TASK-364 — the two switches, from the wire
// ---------------------------------------------------------------------------

/// Call `session/transcript` and return the whole response object.
fn transcript_call(client: &mut harness::Client, session: &str, action: &str) -> Value {
    client.call(
        "session/transcript",
        json!({ "session_id": session, "action": action }),
    )
}

/// The `result` of a `session/transcript` call that must have succeeded.
fn transcript_result(client: &mut harness::Client, session: &str, action: &str) -> Value {
    let response = transcript_call(client, session, action);
    assert!(
        response.get("result").is_some(),
        "session/transcript {action} must succeed: {response}"
    );
    response["result"].clone()
}

/// Every `transcript_state` event this client has seen, as `(enabled, reason)`.
fn state_events(client: &harness::Client) -> Vec<(bool, String)> {
    client
        .events_named("transcript_state")
        .into_iter()
        .map(|event| {
            (
                event["enabled"].as_bool().unwrap_or_default(),
                event["reason"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

/// **AC-3 (daemon half): `on` records from the switch forward, and writes no
/// config.**
///
/// Three claims in one fixture because they are one act: a session started
/// under `enabled = false` is switched on mid-conversation, and afterwards the
/// file must hold what came *after* and nothing that came before, the attached
/// connection must have been told, and `config.toml` must be untouched — BR-2's
/// "never written to disk" is the half a user cannot see, so it is asserted on
/// the **bytes** rather than inferred from the absence of a `config/set`
/// (LESSON-519).
///
/// The two prompts carry deliberately distinctive strings, because "the
/// pre-switch conversation is absent" is a claim about *content*: a check that
/// merely counted records would pass against a sink that backfilled the
/// retained conversation with different `n`s.
///
/// **Mutation (run, red):** make `session_transcript`'s `On` arm pass the
/// session's `seq_at_open` instead of the live sequence — no; the mutation
/// actually run was cruder and closer to the claim: `sink.set_enabled(&id,
/// enabled, ..)` changed to `sink.set_enabled(&id, true, ..)` for both arms
/// leaves this test green and reddens `off_closes_and_on_resumes_the_same_file`.
/// The mutation that reddens *this* test is deleting the `On => Some(true)` arm
/// (making it `None`, a no-op read): the file never appears and `await_file`
/// times out. Restored.
#[test]
fn on_records_from_the_switch_forward_and_writes_no_config() {
    let provider = MockProvider::always(openai_turn("Done.", None, 120, 20));
    let duties = duty_provider();
    let ws = Workspace::new("transcript-ac3");
    let dir = write_config(&ws, &provider, &duties, false);
    let daemon = spawn(&ws, probe());
    let mut client = daemon.connect();
    let session = client.create_session("freeform", None);

    // Before the switch: a whole turn that must leave no trace.
    client.prompt(&session, "BEFORE-THE-SWITCH-marker");
    client.drain_events(Duration::from_millis(200));
    assert!(
        transcript_files(&dir).is_empty(),
        "a session started under `enabled = false` has no file yet"
    );

    let config_before = std::fs::read(&ws.config_path).expect("the config exists");
    let switched = transcript_result(&mut client, &session, "on");
    assert_eq!(
        switched["enabled"].as_bool(),
        Some(true),
        "the switch reports the state it produced: {switched}"
    );
    let path = switched["path"]
        .as_str()
        .unwrap_or_else(|| panic!("`on` answers with the file it opened: {switched}"))
        .to_owned();

    // BR-15: the news reaches the attached connection, with the reason that is
    // true of it.
    let announced = client
        .wait_for_event("transcript_state", WINDOW)
        .expect("`on` announces the change to the session's attached connections");
    assert_eq!(announced["enabled"].as_bool(), Some(true), "{announced}");
    assert_eq!(
        announced["reason"].as_str(),
        Some("session_command"),
        "{announced}"
    );
    assert!(
        announced.get("path").is_none(),
        "BR-15: the event is news, never location: {announced}"
    );

    // After the switch: this one is recorded.
    client.prompt(&session, "AFTER-THE-SWITCH-marker");
    let records = await_kinds(&dir, &session, &["prompt_submitted"]);
    let text = std::fs::read_to_string(&path).expect("the transcript is readable");
    assert!(
        text.contains("AFTER-THE-SWITCH-marker"),
        "the turn after the switch is recorded:\n{text}"
    );
    assert!(
        !text.contains("BEFORE-THE-SWITCH-marker"),
        "AC-3: nothing is backfilled from the retained conversation:\n{text}"
    );
    assert_eq!(
        records.first().map(|r| r["kind"].clone()),
        Some(Value::from("transcript_opened")),
        "a switched-on file still begins with its opening record: {:?}",
        kinds_of(&records)
    );
    assert_well_formed(&records, &session);

    // AC-3 / BR-16: the session switch is session-lifetime, so the file on disk
    // is byte-identical. Read, not inferred.
    assert_eq!(
        std::fs::read(&ws.config_path).ok().as_deref(),
        Some(config_before.as_slice()),
        "`/transcript on` must not write a byte of config.toml"
    );
}

/// **AC-4 (daemon half): `off` closes with `session_command`, and a later `on`
/// resumes the same file.**
///
/// The benign-path flag is `no` in the Verification table for a reason: this is
/// the criterion whose failure mode is a *second* file, and a suite that only
/// checked "a file exists after `on`" would never see it. So the path is
/// captured before the `off` and compared afterwards, and `n` is asserted
/// contiguous across the pause — a resumed file with `n` restarting at 1 is the
/// other way to get this wrong.
///
/// **Mutation (run, red):** in `WriterTask::on_set_enabled`, replace the
/// `ensure_open(session_id, Some(seq))` resume call with a fresh
/// `Writer::open`. A second `.jsonl` appears and the `same file` assertion
/// fails on the path. Restored.
#[test]
fn off_closes_and_on_resumes_the_same_file() {
    let provider = MockProvider::always(openai_turn("Done.", None, 120, 20));
    let duties = duty_provider();
    let ws = Workspace::new("transcript-ac4");
    let dir = write_config(&ws, &provider, &duties, true);
    let daemon = spawn(&ws, probe());
    let mut client = daemon.connect();
    let session = client.create_session("freeform", None);
    client.prompt(&session, "WHILE-RECORDING-marker");
    let path = await_file(&dir, &session);

    let stopped = transcript_result(&mut client, &session, "off");
    assert_eq!(
        stopped["enabled"].as_bool(),
        Some(false),
        "`off` reports the state it produced: {stopped}"
    );
    let announced = client
        .wait_for_event("transcript_state", WINDOW)
        .expect("`off` announces the change");
    assert_eq!(announced["enabled"].as_bool(), Some(false), "{announced}");
    assert_eq!(
        announced["reason"].as_str(),
        Some("session_command"),
        "{announced}"
    );

    let (closed, _) = read_transcript(&path);
    let last = closed.last().expect("the file has records");
    assert_eq!(
        last["kind"],
        "transcript_closed",
        "`off` closes the file rather than merely stopping: {:?}",
        kinds_of(&closed)
    );
    assert_eq!(
        last["reason"], "session_command",
        "and says which switch closed it: {last}"
    );

    // A turn while off adds nothing. The status call after it is the
    // synchronisation point — it flushes the sink — so this is a claim about
    // the file rather than about how fast the test ran.
    let bytes_when_closed = std::fs::read(&path).expect("readable");
    client.prompt(&session, "WHILE-OFF-marker");
    let idle = transcript_result(&mut client, &session, "status");
    assert_eq!(
        idle["enabled"].as_bool(),
        Some(false),
        "status while off says so: {idle}"
    );
    assert_eq!(
        std::fs::read(&path).ok().as_deref(),
        Some(bytes_when_closed.as_slice()),
        "AC-4: a prompt while off adds no line"
    );
    assert!(
        !String::from_utf8_lossy(&bytes_when_closed).contains("WHILE-OFF-marker"),
        "and the turn it ran is nowhere in the file"
    );

    // And back on: the same file, resumed, with `n` continuing.
    let resumed = transcript_result(&mut client, &session, "on");
    assert_eq!(
        resumed["path"].as_str(),
        Some(path.display().to_string().as_str()),
        "AC-4: `on` resumes the SAME file rather than starting a second one: {resumed}"
    );
    assert_eq!(
        transcript_files(&dir).len(),
        1,
        "one session, one file, across the pause: {:?}",
        transcript_files(&dir)
    );
    let (records, _) = read_transcript(&path);
    assert!(
        records.iter().any(|r| r["kind"] == "transcript_resumed"),
        "the resume is written down: {:?}",
        kinds_of(&records)
    );
    assert!(
        records.len() > closed.len(),
        "the resumed file grew rather than being rewritten"
    );
    assert_well_formed(&records, &session);
}

/// **AC-5 / BR-15: the status answer names the file on the asking connection,
/// and nothing carrying that name reaches anybody else.**
///
/// The second connection is genuinely *attached* — it asks, the owner's reader
/// thread grants — because an unattached one receives no session-scoped frame
/// at all and the absence assertion would be vacuous. The non-vacuity floor is
/// explicit: the watcher must have received a `transcript_state` frame, so what
/// is being asserted is that the frames it got carry no path, not that it got
/// none.
///
/// Asserted on the **raw wire text** ([`harness::Client::raw_wire`]), not on the
/// parsed event list: the claim is about every byte the daemon sent this
/// connection, including responses and the frames the harness classifier drops.
///
/// **Mutation (run, red):** add `pub path: Option<String>` to
/// `events::TranscriptState` and populate it from the sink's status in
/// `session_transcript`. The watcher's raw-wire assertion fails, naming the
/// leaked path. Restored.
#[test]
fn status_answers_the_asker_and_the_state_event_carries_no_path() {
    let provider = MockProvider::always(openai_turn("Done.", None, 120, 20));
    let duties = duty_provider();
    let ws = Workspace::new("transcript-ac5");
    let dir = write_config(&ws, &provider, &duties, true);
    let daemon = spawn(&ws, probe());
    let mut owner = daemon.connect().with_auto_consent();
    let session = owner.create_session("freeform", None);
    owner.prompt(&session, "Say something.");
    let path = await_file(&dir, &session);
    let path_text = path.display().to_string();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("the transcript has a name")
        .to_owned();

    // A second connection, attached with the owner's consent.
    let mut watcher = daemon.connect();
    let attached = watcher.call("session/attach", json!({ "session_id": session.clone() }));
    assert!(
        attached.get("result").is_some(),
        "the watcher must actually attach, or the absence below proves nothing: \
         {attached}\ndaemon log:\n{}",
        daemon.log()
    );

    // The routed answer: enabled, path, count — to the asker.
    let status = transcript_result(&mut owner, &session, "status");
    assert_eq!(status["enabled"].as_bool(), Some(true), "{status}");
    assert_eq!(
        status["path"].as_str(),
        Some(path_text.as_str()),
        "BR-15: the asking connection is the one surface that names the file: {status}"
    );
    assert!(
        status["records"].as_u64().is_some_and(|n| n > 0),
        "the count is the file's own: {status}"
    );
    assert!(
        status["degraded"].is_null(),
        "a healthy session reports no degraded reason: {status}"
    );

    // Two state changes, so the watcher has something to have received.
    transcript_result(&mut owner, &session, "off");
    transcript_result(&mut owner, &session, "on");
    watcher
        .wait_for_event("transcript_state", WINDOW)
        .expect("an attached watcher is told that the record stopped and started");
    watcher.drain_events(Duration::from_millis(300));

    let seen = state_events(&watcher);
    assert!(
        !seen.is_empty(),
        "non-vacuity: the watcher must have received the news it is being checked \
         for the absence of a path in"
    );
    let wire = watcher.raw_wire();
    assert!(
        !wire.contains(&path_text),
        "AC-5: no frame to a second connection may carry the transcript path \
         ({path_text}) — wire:\n{wire}"
    );
    assert!(
        !wire.contains(&file_name),
        "AC-5: nor its file name ({file_name}) — wire:\n{wire}"
    );
    // The owner's own event stream is held to the same rule: the path came back
    // on the response, and the event must not have carried it either.
    for event in owner.events_named("transcript_state") {
        assert!(
            event.get("path").is_none(),
            "BR-15: `transcript_state` has no path field: {event}"
        );
    }
}

/// **BR-3 / AC-7: nothing the model can emit reaches the toggle, and an
/// unattached connection is refused.**
///
/// Two halves, and the first is the one the AC insists on: prove the *surface
/// is absent* rather than that a call to it is refused. So the tool registry is
/// enumerated — names and rendered docs — and asked whether anything in it is
/// about transcripts. A model's only vocabulary is that registry; a name it
/// cannot emit is a capability it cannot reach, and no runtime check has to
/// hold for that to stay true.
///
/// The second half is BR-3's other clause: `session/transcript` exists on the
/// client channel, so a *connection* that is not driving the session must not
/// reach it either. Non-vacuous because the same call from the attached owner
/// is asserted to succeed.
///
/// **Mutation (run, red):** register a tool named `transcript` in
/// `ToolRegistry::with_builtins`. The name enumeration fails. And separately:
/// delete the `may_drive` check from `handle_session_transcript` — the stranger
/// leg then gets a `result` instead of `NOT_ATTACHED` and fails. Both restored.
/// **Mutation (run 2026-09-03):** removing the `may_drive` check from
/// `handle_session_transcript` turned this test red at the unattached-
/// connection leg (the bystander's `session/transcript` was answered instead
/// of refused `NOT_ATTACHED`). Restored; green again.
#[test]
fn no_tool_reaches_the_transcript_toggle_and_an_unattached_connection_is_refused() {
    use tetond::harness::tools::ToolRegistry;

    // --- the surface is absent from the model's whole vocabulary ---
    let registry = ToolRegistry::with_builtins();
    let names = registry.names();
    assert!(
        !names.is_empty(),
        "non-vacuity: an empty registry would satisfy any absence claim"
    );
    for name in &names {
        assert!(
            !name.contains("transcript"),
            "BR-3: no tool may name or alias the transcript toggle; found `{name}` \
             among {names:?}"
        );
    }
    let docs = registry.docs(None);
    assert!(
        !docs.to_lowercase().contains("transcript"),
        "BR-3: nor may a tool's description offer it — a model reads these:\n{docs}"
    );

    // --- and the client method is gated on driving the session ---
    let provider = MockProvider::always(openai_turn("Done.", None, 120, 20));
    let duties = duty_provider();
    let ws = Workspace::new("transcript-ac7");
    let dir = write_config(&ws, &provider, &duties, true);
    let daemon = spawn(&ws, probe());
    let mut owner = daemon.connect();
    let session = owner.create_session("freeform", None);
    let _ = await_file(&dir, &session);

    let mut stranger = daemon.connect();
    for action in ["status", "on", "off"] {
        let refused = transcript_call(&mut stranger, &session, action);
        assert_eq!(
            refused["error"]["code"].as_i64(),
            Some(teton_protocol::jsonrpc::error_code::NOT_ATTACHED),
            "BR-3: `{action}` from a connection that does not drive the session must \
             be refused: {refused}"
        );
    }
    // Non-vacuity: the very same call from the attached owner works.
    let allowed = transcript_result(&mut owner, &session, "status");
    assert_eq!(allowed["enabled"].as_bool(), Some(true), "{allowed}");
}

/// **BR-9 / AC-11: an uncreatable or too-wide directory is refused with
/// `dir_refused`, and a fresh directory inside the session root opens.**
///
/// Three fixtures, because the rule has three shapes and only the third is
/// benign. The first two are the refusals BR-9 names — a directory that cannot
/// be created at all, and one that already exists wider than owner-only — and
/// both must leave the session *running*: a transcript that cannot be written
/// is not a reason to fail a user's turn (ADR-8).
///
/// The third leg is the one the spec had to settle: a `dir` **inside the
/// session root** is accepted, not refused. What keeps it unreadable is the
/// jail denial (TASK-368), not a containment check here, and adding one would
/// be a second rule for one property.
///
/// **Mutation (run, red):** delete the `mode & NON_OWNER_BITS` check from
/// `writer::prepare_dir`. The `0o755` leg then opens a file and the
/// `dir_refused` assertion fails; the in-root leg stays green, which is the
/// right split. Restored.
#[test]
fn an_uncreatable_or_wide_dir_is_refused_and_a_fresh_in_root_dir_opens() {
    use std::os::unix::fs::PermissionsExt as _;

    let provider = MockProvider::always(openai_turn("Done.", None, 120, 20));
    let duties = duty_provider();

    // --- leg 1: a directory that cannot be created (its parent is a file) ---
    let blocked = Workspace::new("transcript-ac11-uncreatable");
    let occupied = blocked.root.join("not-a-directory");
    std::fs::write(&occupied, b"this path is a file\n").expect("write the obstruction");
    let blocked_dir = occupied.join("transcripts");
    write_config_in(&blocked, &provider, &duties, false, &blocked_dir);
    let blocked_daemon = spawn(&blocked, probe());
    let mut blocked_client = blocked_daemon.connect();
    let blocked_session = blocked_client.create_session("freeform", None);

    let refused = transcript_result(&mut blocked_client, &blocked_session, "on");
    assert_eq!(
        refused["enabled"].as_bool(),
        Some(false),
        "AC-11: a directory that cannot be created leaves the session unrecorded: {refused}"
    );
    assert!(
        refused["degraded"].as_str().is_some(),
        "and says why, on the asking connection: {refused}"
    );
    let announced = blocked_client
        .wait_for_event("transcript_state", WINDOW)
        .expect("the refusal is announced");
    assert_eq!(
        (announced["enabled"].as_bool(), announced["reason"].as_str()),
        (Some(false), Some("dir_refused")),
        "AC-11: the reason is the one that is true of it: {announced}"
    );
    let ran = blocked_client.prompt(&blocked_session, "Say something.");
    assert!(
        ran.get("result").is_some(),
        "AC-11: the session runs normally without a transcript: {ran}"
    );
    assert!(
        !blocked_dir.exists(),
        "nothing was created at the refused path"
    );
    drop(blocked_client);
    drop(blocked_daemon);

    // --- leg 2: a directory that already exists, wider than owner-only ---
    let wide = Workspace::new("transcript-ac11-wide");
    let wide_dir = wide.root.join("wide-transcripts");
    std::fs::create_dir_all(&wide_dir).expect("create the wide directory");
    std::fs::set_permissions(&wide_dir, std::fs::Permissions::from_mode(0o755)).expect("widen it");
    write_config_in(&wide, &provider, &duties, false, &wide_dir);
    let wide_daemon = spawn(&wide, probe());
    let mut wide_client = wide_daemon.connect();
    let wide_session = wide_client.create_session("freeform", None);

    let wide_refused = transcript_result(&mut wide_client, &wide_session, "on");
    assert_eq!(
        wide_refused["enabled"].as_bool(),
        Some(false),
        "BR-9: an existing directory wider than owner-only is not silently reused: \
         {wide_refused}"
    );
    let wide_announced = wide_client
        .wait_for_event("transcript_state", WINDOW)
        .expect("the refusal is announced");
    assert_eq!(
        (
            wide_announced["enabled"].as_bool(),
            wide_announced["reason"].as_str()
        ),
        (Some(false), Some("dir_refused")),
        "{wide_announced}"
    );
    assert!(
        transcript_files(&wide_dir).is_empty(),
        "BR-9: refused means nothing was written into it: {:?}",
        transcript_files(&wide_dir)
    );
    drop(wide_client);
    drop(wide_daemon);

    // --- leg 3 (benign): a fresh directory inside the session root opens ---
    let inside = Workspace::new("transcript-ac11-inroot");
    let inside_dir = inside.repo.join(".teton-transcripts");
    write_config_in(&inside, &provider, &duties, false, &inside_dir);
    let inside_daemon = spawn(&inside, probe());
    let mut inside_client = inside_daemon.connect();
    let inside_session = inside_client.create_session("freeform", None);

    let opened = transcript_result(&mut inside_client, &inside_session, "on");
    assert_eq!(
        opened["enabled"].as_bool(),
        Some(true),
        "AC-11: a `dir` inside the session root is ACCEPTED — the read refusal is \
         the jail's (TASK-368), not this method's: {opened}"
    );
    assert!(
        opened["degraded"].is_null(),
        "and it is not degraded: {opened}"
    );
    let path = await_file(&inside_dir, &inside_session);
    assert!(
        path.starts_with(&inside.repo),
        "the file really is inside the session root: {}",
        path.display()
    );
}

/// **BR-6 / AC-10: a write failure is announced once, degrades the status, and
/// spares the turn.**
///
/// The failure is made real rather than injected. `transcript::writer`'s
/// `Faults` seam is `#[cfg(test)]` and therefore unreachable from an
/// integration binary, and a `chmod` cannot be trusted to fail for a suite that
/// may run as root — so the fixture makes the transcript's own path
/// **unopenable by any uid**: it is replaced with a directory while the file is
/// closed, and the following `on` has to reopen it. `open_owner_only` refuses a
/// path that is not a file, `ensure_open`'s resume arm reports that as
/// `write_failure`, and the session degrades exactly as a full disk would.
///
/// The deviation from AC-10's literal wording — "the directory made unwritable
/// mid-session" — is deliberate and is the honest version of it: an *already
/// open* file descriptor keeps accepting writes however the directory is
/// chmod'd, so a test that only widened permissions would be asserting nothing.
/// The reopen is the shipped path a real I/O failure takes.
///
/// **Mutation (run, red):** remove the `if already { return; }` guard from
/// `WriterTask::degrade`. The "exactly one" assertion fails with two
/// `transcript_state { write_failure }` events. Restored.
#[test]
fn write_failure_announces_once_degrades_status_and_spares_the_turn() {
    let provider = MockProvider::always(openai_turn("Done.", None, 120, 20));
    let duties = duty_provider();
    let ws = Workspace::new("transcript-ac10");
    let dir = write_config(&ws, &provider, &duties, true);
    let daemon = spawn(&ws, probe());
    let mut client = daemon.connect();
    let session = client.create_session("freeform", None);
    client.prompt(&session, "Say something.");
    let path = await_file(&dir, &session);

    // Close the file, then make its path unopenable for anybody.
    transcript_result(&mut client, &session, "off");
    std::fs::remove_file(&path).expect("remove the transcript");
    std::fs::create_dir(&path).expect("put a directory where the file was");

    let failed = transcript_result(&mut client, &session, "on");
    assert_eq!(
        failed["enabled"].as_bool(),
        Some(false),
        "BR-6: a session whose file cannot be written reports the truth: {failed}"
    );
    let reason = failed["degraded"]
        .as_str()
        .unwrap_or_else(|| panic!("the degraded reason reaches the asker: {failed}"))
        .to_owned();
    assert!(
        !reason.is_empty(),
        "the reason is a sentence, not an empty string"
    );

    client
        .wait_for_event_where(
            "transcript_state",
            |event| event["reason"] == "write_failure",
            WINDOW,
        )
        .expect("BR-6: the failure is announced in front of a human");
    client.drain_events(Duration::from_millis(300));
    let failures = state_events(&client)
        .into_iter()
        .filter(|(enabled, reason)| !enabled && reason == "write_failure")
        .count();
    assert_eq!(
        failures,
        1,
        "AC-10: exactly one — a notice that repeats is one users learn to read \
         past: {:?}",
        state_events(&client)
    );

    // The status is honest from here on, and a turn still runs.
    let status = transcript_result(&mut client, &session, "status");
    assert_eq!(status["enabled"].as_bool(), Some(false), "{status}");
    assert_eq!(
        status["degraded"].as_str(),
        Some(reason.as_str()),
        "AC-10: `/transcript` reports the degraded reason: {status}"
    );
    let ran = client.prompt(&session, "Say something else.");
    assert!(
        ran.get("result").is_some(),
        "AC-10: the turn is not failed by the transcript failing: {ran}"
    );

    // No further write attempts: the obstruction is untouched, and a second
    // `on` does not re-announce.
    client.drain_events(Duration::from_millis(300));
    assert!(
        path.is_dir() && std::fs::read_dir(&path).into_iter().flatten().count() == 0,
        "nothing was written through the obstruction at {}",
        path.display()
    );
    let retried = transcript_result(&mut client, &session, "on");
    assert_eq!(
        retried["enabled"].as_bool(),
        Some(false),
        "BR-6: a degraded session does not come back on: {retried}"
    );
    client.drain_events(Duration::from_millis(300));
    assert_eq!(
        state_events(&client)
            .into_iter()
            .filter(|(enabled, reason)| !enabled && reason == "write_failure")
            .count(),
        1,
        "still exactly one: {:?}",
        state_events(&client)
    );
}

/// **BR-2: a durable change applies to sessions created afterwards, and to no
/// session already running.**
///
/// The two lifetimes, on one daemon. A session created under `enabled = false`
/// must be *unmoved* by a `config/set` that flips the durable default — its
/// effective state stays off and it writes no file — while the very next
/// session created on the same daemon records from its first record. Asserting
/// both on one daemon is what makes this about the *rule* rather than about two
/// differently-configured processes.
///
/// The durable write itself is proven on disk by
/// `config_set_attestation.rs::set_transcript_enabled_writes_on_accept_and_nothing_on_refuse`;
/// what is asserted here is which sessions it reaches.
///
/// **Mutation (run, red):** make `transcript_session_created` read a cached
/// startup value instead of the live config — concretely, capture
/// `config.transcript.enabled` into a field at construction and read that. The
/// later session then gets no file and the second half fails. Restored.
#[test]
fn a_durable_change_applies_to_later_sessions_only() {
    let provider = MockProvider::always(openai_turn("Done.", None, 120, 20));
    let duties = duty_provider();
    let ws = Workspace::new("transcript-br2");
    let dir = write_config(&ws, &provider, &duties, false);
    let daemon = spawn(&ws, probe());
    let mut client = daemon.connect();

    let before = client.create_session("freeform", None);
    client.prompt(&before, "Say something.");
    client.drain_events(Duration::from_millis(200));

    let applied = client.call(
        "config/set",
        json!({ "update": { "op": "set_transcript_enabled", "enabled": true } }),
    );
    assert_eq!(
        applied["result"]["applied"].as_bool(),
        Some(true),
        "the durable default must apply: {applied}\ndaemon log:\n{}",
        daemon.log()
    );
    let snapshot = client.config_get();
    assert_eq!(
        snapshot["transcript"]["enabled"].as_bool(),
        Some(true),
        "AC-20: `config/get` reports the posture doctor renders: {snapshot}"
    );

    // BR-2: the running session is untouched.
    let untouched = transcript_result(&mut client, &before, "status");
    assert_eq!(
        untouched["enabled"].as_bool(),
        Some(false),
        "BR-2: a durable change does not move a session that is already running: \
         {untouched}"
    );
    client.prompt(&before, "Say something else.");
    client.drain_events(Duration::from_millis(200));
    assert!(
        transcript_files(&dir).is_empty(),
        "and it still writes nothing: {:?}",
        transcript_files(&dir)
    );

    // …and the next session created reads the new default.
    let after = client.create_session("freeform", None);
    let path = await_file(&dir, &after);
    assert!(
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.contains(&after)),
        "the file belongs to the session created after the change: {}",
        path.display()
    );
    assert_eq!(
        transcript_files(&dir).len(),
        1,
        "exactly one session records: {:?}",
        transcript_files(&dir)
    );
    let recording = transcript_result(&mut client, &after, "status");
    assert_eq!(
        recording["enabled"].as_bool(),
        Some(true),
        "BR-2: the session created afterwards starts from the new default: {recording}"
    );
}
