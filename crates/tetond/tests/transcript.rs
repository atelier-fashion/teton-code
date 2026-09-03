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

use serde_json::{Map, Value};

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
    let dir = transcript_dir(workspace);
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
