//! REQ-574 acceptance: **a daemon-side config write edits the document it
//! finds** (spec BR-1, TASK-138).
//!
//! The preservation invariant is one seam (`persist_config` →
//! `render_config_document` → `write_config_atomically`) with five caller paths,
//! and LESSON-502 is the reason this file exists rather than one test of the
//! seam: an invariant that holds at N call sites needs a witness at each, or the
//! one caller that forgot to route through the seam is the one nobody notices.
//! So each writer gets its own test here, driven through the entry point a user
//! actually reaches — a consent answer, a `/web setup` commit, a `config/set`
//! provider registration, a daemon start on a pre-upgrade config.
//!
//! ## The fixture is the README's own block (LESSON-512)
//!
//! [`README_WEB_BLOCK`] is the fenced `[web]` example from README.md, copied
//! byte-for-byte, and [`the_fixture_is_the_readmes_own_block_byte_for_byte`]
//! reads the README at test time and refuses to pass on a paraphrase. That is
//! the whole point of spec AC-1: the document a user who followed the docs in
//! order ends up with is the document the writers are asked about, not a
//! convenient miniature of it. Around it the fixture carries the two shapes the
//! schema has never heard of — an unknown key inside a known table
//! (`experimental_reranker`) and an unknown top-level table (`[experimental]`)
//! — because a re-serializing write drops both silently.
//!
//! ## Construction: a real `from_env` daemon over a scratch directory
//!
//! `DaemonRuntime`'s `config_path` is private, so an integration test cannot
//! hand one to a `minimal()` runtime (`web_setup_flow.rs` says so in its own
//! header). What it *can* do is the thing `main` does:
//! [`DaemonRuntime::from_env`] with a per-test `base_dir`, whose config path
//! falls back to `base_dir/config.toml` when `TETON_CONFIG` is unset. That
//! buys two things a hand-built runtime would not: the config path is
//! per-test rather than a process-global, and the **startup migrations run on
//! the real path** — which is the only way to witness them as writers rather
//! than as functions ([`the_model_migration_carries_a_commented_config_across_
//! the_upgrade`] and its routing sibling).
//!
//! ## AC → test map
//!
//! | AC | Test |
//! |----|------|
//! | AC-1 (`persist_web_tier`, README block verbatim) | [`a_consent_answer_moves_its_own_keys_and_leaves_the_readme_config_alone`] |
//! | AC-2 (`web_setup_commit`) + AC-3 (preview == written section) | [`a_setup_commit_writes_the_bytes_its_preview_showed_and_moves_nothing_else`] |
//! | AC-2 (`apply_config_update`) | [`registering_a_provider_leaves_the_web_table_and_its_comments_alone`] |
//! | AC-2 (REQ-557 model migration) | [`the_model_migration_carries_a_commented_config_across_the_upgrade`] |
//! | AC-2 (REQ-558 routing migration) + idempotence | [`the_routing_migration_retires_its_table_without_taking_the_rest_of_the_file`] |
//! | AC-5 (unparseable document, per RPC writer) | [`an_unparseable_document_is_refused_by_the_writers_that_would_have_rewritten_it`] |
//! | AC-6 (missing file → fresh `0600` document) | [`a_config_file_that_does_not_exist_yet_is_created_owner_only`] |
//! | AC-8 (read-back through the production loader) | every test above |
//! | AC-10 (parseable-but-invalid drift) | [`a_hand_edit_that_fails_validation_refuses_both_writers_and_survives_them`] |
//!
//! ### Covered elsewhere, deliberately not repeated here
//!
//! * **AC-5 through `persist_web_tier`**, and the seam's own missing-file and
//!   invalid-drift refusals — `runtime::tests::config_document_seam`
//!   (TASK-136), which can reach `persist_config` directly.
//! * **AC-3's digest half and AC-4** (a comment-only hand edit between preview
//!   and commit) — `runtime::tests::web_setup_flow` (TASK-137). The digest is
//!   re-asserted here once, against the README fixture, because the tie between
//!   "the preview showed this" and "the file says this" is the claim AC-3 makes
//!   about *bytes on disk* and this is the only suite that has them.
//! * **The delta engine's own properties** (insertion, removal with attached
//!   decor, array-wholesale) — `teton_core::config_doc` unit tests.
//!
//! No test here is feature-gated: they run in the default `cargo test
//! --workspace` leg, which is the only leg that exists in CI (BUG-166,
//! LESSON-515).

use std::path::PathBuf;
use std::sync::Arc;

use teton_core::config::{Config, WebTier};
use teton_protocol::events::WebTier as WireWebTier;
use teton_protocol::jsonrpc::error_code;
use teton_protocol::methods::{
    ConfigUpdate, ProviderConfig, WebSetupCommitParams, WebSetupPreviewParams,
};
use teton_protocol::{ProviderId, ProviderKind, SessionId};
use tetond::broadcast::EventBus;
use tetond::runtime::DaemonRuntime;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// The README's `[web]` example, **verbatim** — README.md, the "Or write the
/// table by hand" block (spec AC-1, LESSON-512).
///
/// Not a paraphrase and not a trimmed version: a spec's named example is a test
/// case, and this one is the exact document the docs walk a user into writing
/// immediately before telling them about `/web setup`. The README carries a
/// drift note naming its copies; [`the_fixture_is_the_readmes_own_block_byte_
/// for_byte`] makes that note enforceable rather than aspirational.
const README_WEB_BLOCK: &str = r#"[web]
# "off" (default) | "fetch_user_url" | "fetch_any_url" | "search"
tier = "search"
search_endpoint = "https://api.search.brave.com/res/v1/web/search"
# A reference, never a raw key — the value lives in the OS keychain.
search_key_ref = "keychain://teton/web-search"
# The header the key rides, `{key}` marking the secret. Absent means
# `Authorization: Bearer {key}`, and it is refused with no key reference
# beside it — a header shape with no secret to place would do nothing.
search_auth = "X-Subscription-Token: {key}"
# Optional; constrains model-chosen destinations only. Absent = unrestricted,
# present but empty = nothing allowed. A URL you pasted yourself is exempt.
allowed_domains = ["docs.rs", "crates.io"]
# Cache freshness window in seconds; 0 means no caching. Defaults to 900.
cache_ttl_secs = 900
"#;

/// What sits above the README block: a file-level comment, a top-level key, and
/// a provider whose own comment is inside an **array of tables** — the one
/// construct ADR-1 re-renders wholesale when it changes, which
/// [`registering_a_provider_leaves_the_web_table_and_its_comments_alone`]
/// states as a cost rather than leaving it to be discovered.
///
/// The provider declares a `model`, which keeps the REQ-557 startup migration
/// out of these tests: a migration that fired at `from_env` would rewrite the
/// file before the writer under test ever ran. The migrations get their own
/// fixtures below, where firing is the point.
const CONFIG_PREAMBLE: &str = r#"# My machine. Hand-written, and staying that way.
effort = "high"

[[providers]]
# The one I actually pay for.
id = "anthropic"
kind = "anthropic"
endpoint = "https://api.anthropic.com"
model = "claude-opus-4"
auth_ref = "keychain:anthropic"

"#;

/// What sits below it: an unknown key **inside** the known `[web]` table and an
/// unknown **top-level table**, each under a comment of its own.
///
/// Both are invisible to `Config` — it ignores unknown keys at load, and the
/// spec keeps it that way (schema widening is explicitly out of scope) — so a
/// write that re-serializes the in-memory config drops them without a word.
/// They are here because "the daemon did not destroy what it could not
/// understand" is the half of BR-1 a schema-shaped test cannot see.
const CONFIG_TAIL: &str = r#"# Nothing in this build reads this key.
experimental_reranker = "colbert"

# Nothing in this build reads this table either.
[experimental]
knob = 3
"#;

/// The shared fixture: the README's block with the surroundings above and
/// below. Every non-migration witness starts from exactly this document.
fn readme_config() -> String {
    format!("{CONFIG_PREAMBLE}{README_WEB_BLOCK}{CONFIG_TAIL}")
}

/// A config as the pre-REQ-557 binary wrote it — no `model` on the provider,
/// because the field did not exist — with a user's comments around it.
///
/// `default_provider` and a `[[tiers]]` row are already set so that the
/// **routing** migration finds nothing to do: one writer per witness
/// (LESSON-502), or a failure names two candidates.
const PRE_REQ_557_CONFIG: &str = r#"# Written before the upgrade, and I would like to keep these notes.
effort = "high"
default_provider = "anthropic"

[[providers]]
# no model here — the field did not exist yet
id = "anthropic"
kind = "anthropic"
endpoint = "https://api.anthropic.com"
auth_ref = "keychain:anthropic"

[[tiers]]
tier = "build"
provider_id = "anthropic"

[web]
# a note the migration has no business touching
tier = "fetch_any_url"

# Nothing in this build reads this table.
[experimental]
knob = 3
"#;

/// A config as the pre-REQ-558 binary wrote it — a phase-keyed `[[routing]]`
/// table — with comments inside it and around it.
///
/// Both providers declare what they need to be usable, so the REQ-557 migration
/// stays out of this one for the same reason the fixture above keeps the
/// routing migration out of its own.
const PRE_REQ_558_CONFIG: &str = r#"# Written before the upgrade, and I would like to keep these notes.
default_provider = "cheap"

[[providers]]
id = "on-device"
kind = "local"

[[providers]]
id = "cheap"
kind = "openai-compatible"
endpoint = "https://api.deepseek.com"
model = "deepseek-chat"
auth_ref = "keychain:cheap"

# the retired table, and the comment that documents it
[[routing]]
phase = "implement"
provider_id = "cheap"

[[routing]]
phase = "io"
provider_id = "on-device"

[web]
# a note the migration has no business touching
tier = "fetch_any_url"

# Nothing in this build reads this table.
[experimental]
knob = 3
"#;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A daemon runtime with a config file of its own.
struct Daemon {
    runtime: DaemonRuntime,
    events: Arc<EventBus>,
    /// The config file `from_env` resolved — `dir/config.toml`.
    path: PathBuf,
    dir: PathBuf,
}

impl Daemon {
    /// Start a runtime over a scratch directory seeded with `document` (or with
    /// no config file at all, for the first-write case).
    ///
    /// This is `main`'s own construction, not a test double: the config is
    /// loaded and **migrated** exactly as a real start does it, so a migration
    /// witness below is watching the startup path rather than a function called
    /// by hand.
    fn start(tag: &str, document: Option<&str>) -> Self {
        assert!(
            std::env::var_os("TETON_CONFIG").is_none(),
            "these tests rely on `from_env` resolving `base_dir/config.toml`; \
             a TETON_CONFIG in the environment would point every daemon here at \
             one shared file"
        );
        let dir = scratch_dir(tag);
        let path = dir.join("config.toml");
        if let Some(document) = document {
            std::fs::write(&path, document).expect("seed the config file");
        }
        let events = Arc::new(EventBus::new());
        let runtime = DaemonRuntime::from_env(&dir, &events).expect("the daemon starts");
        Self {
            runtime,
            events,
            path,
            dir,
        }
    }

    /// The config file's current bytes.
    fn document(&self) -> String {
        std::fs::read_to_string(&self.path).expect("read the config file")
    }

    /// Replace the config file behind the daemon's back — a hand edit made
    /// while it runs (BR-5's premise, and AC-5/AC-10's).
    fn hand_edit(&self, document: &str) {
        std::fs::write(&self.path, document).expect("the hand edit lands");
    }

    /// The **next** start, over the file this daemon left — the only way to ask
    /// a startup migration whether it runs twice.
    fn restart(&self) -> DaemonRuntime {
        let events = Arc::new(EventBus::new());
        DaemonRuntime::from_env(&self.dir, &events).expect("the daemon restarts")
    }

    /// The bytes the file holds, put back through the **production loader**
    /// (spec AC-8): a write is only preserved if the document it left still
    /// means what the daemon thinks it does.
    fn reload(&self) -> Config {
        Config::load(&self.document()).expect("the written document loads")
    }

    fn cleanup(self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A throwaway directory under the system temp dir, unique per test.
fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "teton-cfgpreserve-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    dir
}

/// The preview/commit answers, as the CLI sends them.
fn setup_params(tier: WireWebTier, endpoint: Option<&str>) -> WebSetupPreviewParams {
    WebSetupPreviewParams {
        session_id: SessionId::from("preservation"),
        tier,
        search_endpoint: endpoint.map(str::to_owned),
        search_key_ref: Some("keychain://teton/web-search".to_owned()),
        search_auth: Some("X-Subscription-Token: {key}".to_owned()),
    }
}

/// The same answers as a commit, carrying the digest the preview handed back.
fn commit_params(preview: &WebSetupPreviewParams, digest: Option<String>) -> WebSetupCommitParams {
    WebSetupCommitParams {
        session_id: preview.session_id.clone(),
        tier: preview.tier,
        search_endpoint: preview.search_endpoint.clone(),
        search_key_ref: preview.search_key_ref.clone(),
        search_auth: preview.search_auth.clone(),
        expect_digest: digest,
    }
}

/// A provider registration, as `teton provider add` sends it.
fn register(id: &str) -> ConfigUpdate {
    ConfigUpdate::RegisterProvider(ProviderConfig {
        id: ProviderId::from(id),
        kind: ProviderKind::OpenaiCompatible,
        endpoint: Some("https://api.deepseek.com".to_owned()),
        model: Some("deepseek-chat".to_owned()),
        auth_ref: Some("keychain:cheap".to_owned()),
    })
}

// ---------------------------------------------------------------------------
// Assertions
// ---------------------------------------------------------------------------

/// **Exactly these lines changed, and nothing else moved** — the mechanical
/// form of BR-1's "a write touches only its keys".
///
/// The claim is stronger than a spot check of surviving comments, and cheaper
/// than re-parsing field by field: whatever a diff of the two documents reports
/// as deleted and inserted must be exactly what the operation is about. Because
/// the diff is a longest-common-subsequence alignment, "everything else is
/// unchanged **and in the same order**" is not a second assertion — it is what
/// is left over when these two lists match. A normalized key order shows up here
/// as a pile of unexpected moves; a dropped comment shows up as a deletion.
fn assert_only_these_lines_changed(before: &str, after: &str, removed: &[&str], added: &[&str]) {
    let (actually_removed, actually_added) = line_diff(before, after);
    assert_eq!(
        actually_removed, removed,
        "the write deleted lines it is not about\n--- before ---\n{before}\n--- after \
         ---\n{after}"
    );
    assert_eq!(
        actually_added, added,
        "the write inserted lines it is not about\n--- before ---\n{before}\n--- after \
         ---\n{after}"
    );
}

/// The lines `after` deletes from `before`, and the lines it inserts, in
/// document order — a plain longest-common-subsequence diff.
///
/// Everything not reported is a line the two documents share **in the same
/// relative order**, which is the property the assertion above rests on.
fn line_diff(before: &str, after: &str) -> (Vec<String>, Vec<String>) {
    let old: Vec<&str> = before.lines().collect();
    let new: Vec<&str> = after.lines().collect();
    // `common[i][j]` is the length of the longest common subsequence of
    // `old[i..]` and `new[j..]`, filled from the back so the walk below can go
    // forwards and report deletions and insertions in document order.
    let mut common = vec![vec![0usize; new.len() + 1]; old.len() + 1];
    for i in (0..old.len()).rev() {
        for j in (0..new.len()).rev() {
            common[i][j] = if old[i] == new[j] {
                common[i + 1][j + 1] + 1
            } else {
                common[i + 1][j].max(common[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let (mut removed, mut added) = (Vec::new(), Vec::new());
    while i < old.len() && j < new.len() {
        if old[i] == new[j] {
            i += 1;
            j += 1;
        } else if common[i + 1][j] >= common[i][j + 1] {
            removed.push(old[i].to_owned());
            i += 1;
        } else {
            added.push(new[j].to_owned());
            j += 1;
        }
    }
    removed.extend(old[i..].iter().map(|line| (*line).to_owned()));
    added.extend(new[j..].iter().map(|line| (*line).to_owned()));
    (removed, added)
}

/// **Everything survived except `retired`, in the order it was written** — the
/// claim for a write that *appends* (both startup migrations), where an exact
/// line delta would be a transcription of the rows the migration exists to add.
///
/// `retired` is the contiguous block the operation is allowed to take, quoted
/// from the fixture so the expectation reads as the thing that disappeared.
/// What is left must appear in `after` as a subsequence: new rows may land
/// anywhere, but no surviving line may vanish and none may overtake another.
fn assert_lines_survive_in_order(before: &str, after: &str, retired: &str) {
    assert!(
        before.contains(retired),
        "the fixture does not hold the block this write is allowed to \
         retire:\n{before}"
    );
    let expected = before.replace(retired, "");
    let mut remaining = after.lines();
    for line in expected.lines() {
        assert!(
            remaining.any(|candidate| candidate == line),
            "`{line}` did not survive the write, or was reordered past a line \
             that follows it\n--- before ---\n{before}\n--- after ---\n{after}"
        );
    }
}

/// The `[web]` table as it appears in `text`, decor included — an independent
/// walk, so an assertion against it is not the production slicer agreeing with
/// itself.
///
/// The rule it reproduces is toml_edit's: a table runs from its header to the
/// next one, and the blank lines and comments immediately *above* that next
/// header belong to it, not to this table.
fn web_section(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines
        .iter()
        .position(|line| *line == "[web]")
        .expect("the document names a `[web]` table");
    let end = lines[start + 1..]
        .iter()
        .position(|line| line.starts_with('['))
        .map_or(lines.len(), |offset| start + 1 + offset);
    let mut section = &lines[start..end];
    while section
        .last()
        .is_some_and(|line| line.trim().is_empty() || line.trim_start().starts_with('#'))
    {
        section = &section[..section.len() - 1];
    }
    let mut rendered = section.join("\n");
    rendered.push('\n');
    rendered
}

// ---------------------------------------------------------------------------
// The fixture is the README's own block
// ---------------------------------------------------------------------------

/// **The test vector is the documented example, byte-for-byte** (spec AC-1,
/// LESSON-512).
///
/// AC-1 asks for the README's block "not a paraphrase of it", and the only way
/// to keep that true across a doc edit is to read the README. So this does: it
/// finds the one fenced `toml` block that starts a `[web]` table and compares
/// it to [`README_WEB_BLOCK`]. Editing the fence without moving the fixture
/// turns this red, which is exactly what the README's own drift note promises
/// happens.
#[test]
fn the_fixture_is_the_readmes_own_block_byte_for_byte() {
    let readme_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../README.md");
    let readme = std::fs::read_to_string(&readme_path).unwrap_or_else(|err| {
        panic!(
            "the README is the source of this fixture and must be readable at \
             {}: {err}",
            readme_path.display()
        )
    });
    let blocks: Vec<&str> = readme
        .split("```toml\n")
        .skip(1)
        .filter_map(|rest| rest.split_once("```").map(|(block, _)| block))
        .filter(|block| block.starts_with("[web]"))
        .collect();
    assert_eq!(
        blocks.len(),
        1,
        "the README should teach exactly one hand-written `[web]` block; found \
         {}",
        blocks.len()
    );
    assert_eq!(
        blocks[0], README_WEB_BLOCK,
        "the README's `[web]` example moved and this fixture did not — AC-1 \
         wants the README's own block as the preservation test vector"
    );

    // And the fixture really does carry it whole, rather than merely resembling
    // it: the block is a contiguous substring of the document under test.
    assert!(
        readme_config().contains(README_WEB_BLOCK),
        "the shared fixture must embed the README block verbatim"
    );
    // Non-vacuity for every witness below: the documented example is a config
    // the daemon accepts, so a refusal in these tests is about the write and
    // never about the fixture.
    Config::load(&readme_config()).expect("the README's own example must load and validate");
}

// ---------------------------------------------------------------------------
// Writer 1 — `persist_web_tier` (REQ-563's "enable permanently")
// ---------------------------------------------------------------------------

/// **A consent answer writes its own two keys and nothing else** (spec AC-1,
/// AC-8).
///
/// The writer a user reaches by answering "enable permanently" at a lookup
/// prompt, asked of the README's document. Before REQ-574 this call
/// re-serialized the whole in-memory config: every comment in the block above
/// went, the key order became `Config`'s, and both unknowns were dropped —
/// while the user was doing nothing more than answering a prompt about one
/// tier.
///
/// Two legs, because the answer has two keys and the fixture only exercises one
/// of them:
///
/// 1. the README's document as written (`tier = "search"`), where the answer is
///    a pure insertion into `[web]`;
/// 2. the same document with the tier lowered — one value changed, comments and
///    all — where the answer *raises* the ceiling, so `tier` moves too.
///
/// `runtime::tests::config_document_seam` already witnesses this writer at the
/// seam on a fixture of its own; what is added here is the README block and the
/// exact-delta assertion AC-1 asks for.
#[test]
fn a_consent_answer_moves_its_own_keys_and_leaves_the_readme_config_alone() {
    let before = readme_config();
    let daemon = Daemon::start("consent", Some(&before));

    daemon
        .runtime
        .persist_web_tier(WebTier::Search)
        .expect("the consent answer lands");

    let after = daemon.document();
    assert_only_these_lines_changed(&before, &after, &[], &[r#"permission_allow = ["search"]"#]);
    assert!(
        web_section(&after).contains(r#"permission_allow = ["search"]"#),
        "the answer's key must land in the table it belongs to:\n{after}"
    );

    // AC-8: the bytes mean what the answer asked for, read back through the
    // loader a restart would use — and everything the answer was not about
    // still means what the user wrote.
    let reloaded = daemon.reload();
    assert_eq!(reloaded.web.permission_allow, vec![WebTier::Search]);
    assert_eq!(reloaded.web.tier, WebTier::Search);
    assert_eq!(
        reloaded.web.search_auth.as_deref(),
        Some("X-Subscription-Token: {key}"),
        "BUG-165's header shape is the one value in this block a canonical \
         rewrite would have been most likely to lose"
    );
    assert_eq!(
        reloaded.web.allowed_domains,
        Some(vec!["docs.rs".to_owned(), "crates.io".to_owned()])
    );
    assert_eq!(reloaded.web.cache_ttl_secs, 900);
    daemon.cleanup();

    // Leg 2: the same document, one tier lower, where the answer raises the
    // ceiling as well as recording the consent.
    let lowered = before.replace(r#"tier = "search""#, r#"tier = "fetch_user_url""#);
    assert_ne!(lowered, before, "the fixture must hold the tier line");
    let daemon = Daemon::start("consent-raise", Some(&lowered));

    daemon
        .runtime
        .persist_web_tier(WebTier::Search)
        .expect("the consent answer lands");

    let raised = daemon.document();
    assert_only_these_lines_changed(
        &lowered,
        &raised,
        &[r#"tier = "fetch_user_url""#],
        &[r#"tier = "search""#, r#"permission_allow = ["search"]"#],
    );
    let reloaded = daemon.reload();
    assert_eq!(reloaded.web.tier, WebTier::Search);
    assert_eq!(reloaded.web.permission_allow, vec![WebTier::Search]);
    daemon.cleanup();
}

// ---------------------------------------------------------------------------
// Writer 2 — `web_setup_commit` (REQ-572's guided flow)
// ---------------------------------------------------------------------------

/// **A setup commit writes the document it previewed, and moves one line of it**
/// (spec AC-2, AC-3, AC-8).
///
/// The flow REQ-572 added, run end to end over the README's own config: preview,
/// carry the digest, commit. Three claims, and the second is what ties AC-2's
/// preservation to AC-3's byte-equality on a document that actually has
/// comments in it:
///
/// 1. only the tier line differs — the user's block, its comments, its key
///    order and both unknowns are the same bytes afterwards;
/// 2. the preview's `toml` is the `[web]` section of the file the commit went
///    on to write, byte-for-byte, **including the user's comments inside
///    `[web]`** — which is only true because both come out of one derivation
///    (ADR-3); before REQ-574 the preview was a fresh serialization and showed
///    no comment at all;
/// 3. the digest the preview handed the client is the digest of the whole
///    written file.
#[test]
fn a_setup_commit_writes_the_bytes_its_preview_showed_and_moves_nothing_else() {
    let before = readme_config();
    let daemon = Daemon::start("setup-commit", Some(&before));

    // The answers keep the backend the block already names and lower the
    // ceiling by one rung — `search` is refused on a machine with no local
    // model (REQ-563 BR-14), and this suite has none.
    let params = setup_params(
        WireWebTier::FetchAnyUrl,
        Some("https://api.search.brave.com/res/v1/web/search"),
    );
    let preview = daemon
        .runtime
        .web_setup_preview(&params)
        .expect("the preview renders");
    assert!(
        preview.toml.contains("# A reference, never a raw key"),
        "the preview must show the user their own comments, or `/web setup` is \
         describing a document that does not exist:\n{}",
        preview.toml
    );

    let result = daemon
        .runtime
        .web_setup_commit(
            &commit_params(&params, Some(preview.digest.clone())),
            &daemon.events,
        )
        .expect("the commit lands");
    assert!(result.applied);

    let after = daemon.document();
    assert_only_these_lines_changed(
        &before,
        &after,
        &[r#"tier = "search""#],
        &[r#"tier = "fetch_any_url""#],
    );
    assert_eq!(
        preview.toml,
        web_section(&after),
        "AC-3: the preview's `toml` is the `[web]` section of the file the \
         commit wrote, or the user confirmed bytes that never landed"
    );
    assert_eq!(
        preview.digest,
        teton_inference::sha256_hex(after.as_bytes()),
        "AC-3: the digest covers the bytes that landed, not a parallel \
         serialization of them"
    );

    let reloaded = daemon.reload();
    assert_eq!(reloaded.web.tier, WebTier::FetchAnyUrl);
    assert_eq!(
        reloaded.web.search_key_ref.as_deref(),
        Some("keychain://teton/web-search")
    );
    assert_eq!(reloaded.web.cache_ttl_secs, 900);
    daemon.cleanup();
}

// ---------------------------------------------------------------------------
// Writer 3 — `apply_config_update` (`config/set`)
// ---------------------------------------------------------------------------

/// **Registering a provider does not cost the user their `[web]` block** (spec
/// AC-2, AC-8).
///
/// `teton provider add` is the write with the least to do with `[web]`, which
/// is precisely why it is the one that used to hurt: a registration
/// re-serialized the whole document, so a user who had hand-written the README's
/// block lost all of it to an unrelated command.
///
/// It also states ADR-1's one deliberate cost out loud. Arrays of tables are
/// diffed as a single key, so a `[[providers]]` append re-renders the array
/// wholesale: the comment *inside* the existing provider entry goes, and the
/// defaulted `[providers.capabilities]` sub-tables arrive. Everything outside
/// the array — the file header, `effort`, the whole `[web]` table with its
/// comments and unknown key, and the unknown top-level table — is byte-identical.
/// Element-wise array diffing was rejected in architecture (it needs an identity
/// function per array type and mis-targets when on-disk order has drifted), so
/// this is the documented behaviour rather than a defect, and pinning it here is
/// what makes a future narrowing visible.
#[test]
fn registering_a_provider_leaves_the_web_table_and_its_comments_alone() {
    let before = readme_config();
    let daemon = Daemon::start("register", Some(&before));

    daemon
        .runtime
        .apply_config_update(register("cheap"))
        .expect("the registration lands");

    let after = daemon.document();
    assert_only_these_lines_changed(
        &before,
        &after,
        // ADR-1: the comment inside the changed array does not survive it.
        &["# The one I actually pay for."],
        &[
            // The existing entry's defaulted capabilities, now written out...
            "[providers.capabilities]",
            r#"tool_call_tier = "native""#,
            "parallel_calls = false",
            "max_context = 0",
            "",
            // ...and the entry the registration is actually about.
            "[[providers]]",
            r#"id = "cheap""#,
            r#"kind = "openai-compatible""#,
            r#"endpoint = "https://api.deepseek.com""#,
            r#"model = "deepseek-chat""#,
            r#"auth_ref = "keychain:cheap""#,
            "",
            "[providers.capabilities]",
            r#"tool_call_tier = "native""#,
            "parallel_calls = false",
            "max_context = 0",
            "",
        ],
    );
    // Said again as the property the test is named for, so a future change to
    // the array-rendering rule cannot quietly take the `[web]` block with it.
    assert_eq!(
        web_section(&after),
        format!("{README_WEB_BLOCK}# Nothing in this build reads this key.\nexperimental_reranker = \"colbert\"\n"),
        "a provider registration must leave the `[web]` table byte-identical"
    );

    let reloaded = daemon.reload();
    assert_eq!(reloaded.providers.len(), 2);
    assert_eq!(reloaded.web.tier, WebTier::Search);
    assert_eq!(
        reloaded.web.search_auth.as_deref(),
        Some("X-Subscription-Token: {key}")
    );
    daemon.cleanup();
}

// ---------------------------------------------------------------------------
// Writers 4 and 5 — the startup migrations
// ---------------------------------------------------------------------------

/// **The model migration carries the user's notes across the upgrade** (spec
/// AC-2, AC-8).
///
/// The unattended write: it runs on the first start after an upgrade, for every
/// existing install, with nobody watching. Under the old seam that start
/// canonicalized the config file of every user who had ever commented it — the
/// widest-blast-radius instance of the bug this REQ removes, and the one a user
/// would have had no way to attribute.
///
/// Driven through [`DaemonRuntime::from_env`], so the writer under test is the
/// startup path rather than a migration function called by hand.
#[test]
fn the_model_migration_carries_a_commented_config_across_the_upgrade() {
    let daemon = Daemon::start("migrate-models", Some(PRE_REQ_557_CONFIG));

    let after = daemon.document();
    assert_only_these_lines_changed(
        PRE_REQ_557_CONFIG,
        &after,
        // The migrated key lives in an array of tables, so the array is
        // re-rendered and its inner comment travels with it (ADR-1).
        &["# no model here — the field did not exist yet"],
        &[
            // The key the migration resolved, and the capabilities block the
            // array's wholesale re-rendering writes out with it.
            r#"model = "claude-opus-4""#,
            "[providers.capabilities]",
            r#"tool_call_tier = "native""#,
            "parallel_calls = false",
            "max_context = 0",
            "",
        ],
    );

    let reloaded = daemon.reload();
    assert_eq!(
        reloaded.providers[0].model.as_deref(),
        Some("claude-opus-4"),
        "the migration must actually have run — otherwise this test asserts \
         preservation across a write that never happened"
    );
    assert_eq!(reloaded.web.tier, WebTier::FetchAnyUrl);
    daemon.cleanup();
}

/// **The routing migration retires its table without taking the rest of the
/// file** (spec AC-2, AC-8, and the idempotence REQ-558 relies on).
///
/// The second unattended write, and the one that *removes* rather than adds. It
/// pins OQ-1's resolution at the writer: the comment attached to the retired
/// `[[routing]]` table goes with the table it documents, while every
/// free-standing comment elsewhere — including the one inside `[web]`, a table
/// this migration has no business touching — survives in place.
///
/// The second leg is why a preservation change cannot break the migration's own
/// contract: a second start writes **nothing**, and with the file carrying
/// comments this is byte-equality rather than the marker-comment proxy the
/// in-crate test needs.
#[test]
fn the_routing_migration_retires_its_table_without_taking_the_rest_of_the_file() {
    let daemon = Daemon::start("migrate-routing", Some(PRE_REQ_558_CONFIG));

    let after = daemon.document();
    assert_lines_survive_in_order(
        PRE_REQ_558_CONFIG,
        &after,
        // The retired table and the comment that documents it — the whole of
        // what this migration is allowed to take.
        r#"# the retired table, and the comment that documents it
[[routing]]
phase = "implement"
provider_id = "cheap"

[[routing]]
phase = "io"
provider_id = "on-device"

"#,
    );
    assert!(
        !after.contains("[[routing]]") && !after.contains("# the retired table"),
        "the retired table and its attached comment go together (OQ-1):\n{after}"
    );
    assert!(
        after.contains("[[categories]]") && after.contains("[[tiers]]"),
        "the migration must actually have run:\n{after}"
    );
    assert!(
        after.contains("# a note the migration has no business touching"),
        "a comment in a table the migration never names must survive it:\n{after}"
    );

    let reloaded = daemon.reload();
    assert_eq!(reloaded.web.tier, WebTier::FetchAnyUrl);
    assert!(reloaded.legacy_routing.is_empty());
    assert!(!reloaded.categories.is_empty());

    // A second start finds nothing to migrate and writes nothing — the property
    // that keeps a migration one-shot. Byte-equality, because the file it would
    // rewrite is one whose comments a rewrite would visibly cost.
    let restarted = daemon.restart();
    assert_eq!(
        daemon.document(),
        after,
        "a second start rewrote a config it had nothing to migrate"
    );
    assert!(
        restarted
            .web_setup_plan()
            .current_web
            .is_some_and(|web| web.tier == WireWebTier::FetchAnyUrl),
        "and the restarted daemon reads the same `[web]` table back"
    );
    daemon.cleanup();
}

// ---------------------------------------------------------------------------
// Refusals — AC-5 and AC-10 at the writers that would have overwritten
// ---------------------------------------------------------------------------

/// **An unparseable document is refused by every writer, with the parse failure
/// named** (spec AC-5, BR-6).
///
/// The failure this replaces was the quiet one: the old seam serialized memory
/// over whatever was on disk, so a half-finished hand edit was "repaired" by
/// being destroyed. Refusal is the fail-safe answer — but only if it says what
/// is wrong, because a bare "the configuration could not be saved" leaves a user
/// with a daemon that will not write and no way to find out why (LESSON-456,
/// BUG-146).
///
/// `persist_web_tier` is witnessed at the seam in
/// `runtime::tests::config_document_seam`; the two writers here are the ones
/// that task left for this one, each asserted at its own error code because the
/// classification is part of the contract a client renders.
#[test]
fn an_unparseable_document_is_refused_by_the_writers_that_would_have_rewritten_it() {
    let daemon = Daemon::start("unparseable", Some(&readme_config()));
    // A hand edit caught mid-keystroke: an unterminated string.
    let broken = readme_config().replace("tier = \"search\"", "tier = \"search");
    daemon.hand_edit(&broken);

    let err = daemon
        .runtime
        .apply_config_update(register("cheap"))
        .expect_err("a registration must not land on a broken document");
    assert_eq!(err.code, error_code::CONFIG_REJECTED);
    assert!(
        err.message.contains("could not be parsed for editing")
            && err.message.contains("TOML parse error")
            && err.message.contains("nothing was applied"),
        "the refusal must carry the parse failure and say nothing was applied: {}",
        err.message
    );

    let params = setup_params(WireWebTier::FetchAnyUrl, None);
    let err = daemon
        .runtime
        .web_setup_commit(&commit_params(&params, None), &daemon.events)
        .expect_err("a setup commit must not land on a broken document");
    assert_eq!(err.code, error_code::INTERNAL_ERROR);
    assert!(
        err.message.contains("could not be saved")
            && err.message.contains("could not be parsed for editing")
            && err.message.contains("TOML parse error"),
        "the refusal must carry the parse failure: {}",
        err.message
    );

    assert_eq!(
        daemon.document(),
        broken,
        "the user's half-finished edit was rewritten by a refusal"
    );
    daemon.cleanup();
}

/// **A hand edit that parses but does not validate is refused, not overwritten**
/// (spec AC-10, BR-4's stated consequence).
///
/// The drift is at a key neither operation touches and both candidates are
/// clean, so this is the one case that separates "the candidate validates" from
/// "the file the daemon would boot on validates" — the validator runs on the
/// **edited bytes**, which is what makes the difference visible at all.
///
/// The alternative is worse in both directions: writing the candidate would
/// erase the user's edit *and* leave a document the daemon refuses to start on.
/// Refusing keeps both — and the message is the validator's own sentence, which
/// names the key to fix.
#[test]
fn a_hand_edit_that_fails_validation_refuses_both_writers_and_survives_them() {
    let daemon = Daemon::start("invalid-drift", Some(&readme_config()));
    // Parses cleanly; fails `Config::validate` at a key `[web]` knows nothing
    // about.
    let drifted = format!("default_provider = \"ghost\"\n{}", readme_config());
    daemon.hand_edit(&drifted);

    let refused = daemon
        .runtime
        .persist_web_tier(WebTier::FetchUserUrl)
        .expect_err("a consent answer must not write a document that would not load");
    assert!(
        refused.contains("would not load")
            && refused.contains("default_provider names provider 'ghost'"),
        "the refusal must carry the validator's own sentence: {refused}"
    );

    let params = setup_params(
        WireWebTier::FetchAnyUrl,
        Some("https://api.search.brave.com/res/v1/web/search"),
    );
    let err = daemon
        .runtime
        .web_setup_commit(&commit_params(&params, None), &daemon.events)
        .expect_err("a setup commit must not write a document that would not load");
    assert_eq!(
        err.code,
        error_code::WEB_SETUP_INVALID,
        "the drift is in the user's file, so the client is told to fix it \
         rather than shown an internal error: {}",
        err.message
    );
    assert!(
        err.message
            .contains("default_provider names provider 'ghost'"),
        "the refusal must carry the validator's own sentence: {}",
        err.message
    );

    assert_eq!(
        daemon.document(),
        drifted,
        "the invalid hand edit was overwritten by a refusal — the one outcome \
         BR-4 rules out"
    );
    // And the preview refuses for the same reason, so the flow never offers to
    // write bytes the commit would reject.
    let err = daemon
        .runtime
        .web_setup_preview(&params)
        .expect_err("the preview has nothing truthful to show for such a file");
    assert_eq!(err.code, error_code::WEB_SETUP_INVALID);
    daemon.cleanup();
}

// ---------------------------------------------------------------------------
// AC-6 — the first write
// ---------------------------------------------------------------------------

/// **A config file that does not exist yet is created, owner-only, and parses
/// back to the candidate** (spec AC-6, BR-6).
///
/// The daemon has a config path and no file — a fresh install that has never
/// been configured — so a missing file is not an error but an empty edit base.
/// The delta base is `Config::default()` (the parse of an empty document)
/// rather than the caller's `current`, which is what makes the fresh document
/// *complete* instead of a diff against a state no file ever held.
///
/// `0600` is the load-bearing half: this file can hold secret-adjacent material
/// (`McpTransport::Stdio { env }` stores arbitrary environment values), so a
/// created one does not get its permissions from an inherited umask. The seam's
/// own witness is in `runtime::tests::config_document_seam`; this is the same
/// claim reached through the writer a user's first "enable permanently" answer
/// actually takes.
///
/// What this cannot see, and where it is seen: a daemon with no config file
/// starts on `Config::default()`, so here `current` and the default base are the
/// same value and choosing the wrong one changes nothing. The seam test
/// falsifies that by passing a `current` that differs — the reason it stays
/// there rather than being folded into this file.
#[test]
fn a_config_file_that_does_not_exist_yet_is_created_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;

    let daemon = Daemon::start("first-write", None);
    assert!(
        !daemon.path.exists(),
        "the premise is a daemon with a config path and no file"
    );

    daemon
        .runtime
        .persist_web_tier(WebTier::FetchUserUrl)
        .expect("the first write creates the file");

    let mut expected = Config::default();
    expected.web.tier = WebTier::FetchUserUrl;
    expected.web.permission_allow = vec![WebTier::FetchUserUrl];
    assert_eq!(
        daemon.reload(),
        expected,
        "the fresh document's parse must equal the candidate, not a diff \
         against a state no file ever held"
    );
    assert_eq!(
        std::fs::metadata(&daemon.path)
            .expect("stat")
            .permissions()
            .mode()
            & 0o7777,
        0o600,
        "a config this daemon created gets owner-only, not the umask default"
    );
    daemon.cleanup();
}
