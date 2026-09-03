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
//! | AC-1 / BR-1 (`[web]` spelled inline) | [`an_inline_web_table_keeps_the_keys_the_consent_answer_is_not_about`] |
//! | AC-2 (`web_setup_commit`) + AC-3 (preview == written section) | [`a_setup_commit_writes_the_bytes_its_preview_showed_and_moves_nothing_else`] |
//! | AC-2 (`apply_config_update`) | [`registering_a_provider_leaves_the_web_table_and_its_comments_alone`] |
//! | BR-1 (unknown key *inside* `[[providers]]`, twice over) | [`an_unknown_key_inside_a_provider_entry_survives_a_registration`] |
//! | BR-5 (mid-session hand edit at an unrelated key) | [`a_hand_edit_mid_session_survives_a_provider_registration`] |
//! | BR-4/BR-5 (mid-session hand edit *inside* `[[providers]]`, the append branch's deliberate blind spot) | [`a_hand_added_provider_under_the_id_being_registered_refuses_the_write`] |
//! | AC-2 (REQ-557 model migration) | [`the_model_migration_carries_a_commented_config_across_the_upgrade`] |
//! | AC-2 (REQ-558 routing migration) + idempotence | [`the_routing_migration_retires_its_table_without_taking_the_rest_of_the_file`] |
//! | AC-5 (unparseable document, per RPC writer) | [`an_unparseable_document_is_refused_by_the_writers_that_would_have_rewritten_it`] |
//! | AC-6 (missing file → fresh `0600` document) | [`a_config_file_that_does_not_exist_yet_is_created_owner_only`] |
//! | AC-8 (read-back through the production loader) | every test above |
//! | AC-10 (parseable-but-invalid drift) | [`a_hand_edit_that_fails_validation_refuses_both_writers_and_survives_them`] |
//!
//! ## REQ-589: the same seam, carrying a going-forward remedy
//!
//! REQ-589's over-budget offer writes its durable fix through `config/set`
//! (ADR-4), so it becomes a **sixth** caller of the writer this file exists to
//! witness — and BR-9's fix is two writes whose *order* is the whole safety
//! argument (ADR-5). The last section adds that caller's three legs: the
//! ordered pair applied, a refused second write, and a refused write outright.
//!
//! | AC | Test |
//! |----|------|
//! | AC-13 (the remedy applied, bytes + re-parse) | [`the_ordered_rebind_declares_the_window_then_binds_the_tier_and_both_reach_disk`] |
//! | AC-8 / ADR-5 (the circle is unreachable from a partial failure) | [`a_refused_second_write_leaves_a_declared_window_on_an_unbound_tier_never_the_circle`] |
//! | AC-13 / LESSON-520 (the paired refusal) | [`a_refused_remedy_write_leaves_the_document_byte_identical`] |
//! | ADR-7 / BR-7c (the figure on disk is the figure the offer named, with its date) | [`the_window_written_to_disk_is_the_one_the_offer_named_with_its_date`] |
//!
//! ### Covered elsewhere, deliberately not repeated here
//!
//! * **AC-5 through `persist_web_tier`**, and the seam's own missing-file and
//!   invalid-drift refusals — `runtime::config_document::tests`
//!   (TASK-136), which can reach `persist_config` directly.
//! * **AC-3's digest half and AC-4** (a comment-only hand edit between preview
//!   and commit) — `runtime::tests::web_setup_flow` (TASK-137). The digest is
//!   re-asserted here once, against the README fixture, because the tie between
//!   "the preview showed this" and "the file says this" is the claim AC-3 makes
//!   about *bytes on disk* and this is the only suite that has them.
//! * **BR-5 through `/web setup`** — `runtime::tests::web_setup_flow`'s pinned
//!   -field group (an answer the document lost, a hand-deleted `[web]` table, a
//!   document that already holds the answer, a removal note describing the
//!   document). The drift leg added here is the *other* rule, the one every
//!   writer but `/web setup` follows: a key the operation never names is not in
//!   the delta at all.
//! * **The delta engine's own properties** (insertion, removal with attached
//!   decor, element-wise array editing, the reshaped-array fallback) —
//!   `teton_core::config_doc` unit tests.
//!
//! No test here is feature-gated: they run in the default `cargo test
//! --workspace` leg, which is the only leg that exists in CI (BUG-166,
//! LESSON-515).

use std::path::PathBuf;
use std::sync::Arc;

use teton_core::category::Tier;
use teton_core::config::{Config, WebTier};
use teton_core::entities::BoundaryOrigin;
use teton_protocol::events::{BudgetBound, WebTier as WireWebTier};
use teton_protocol::jsonrpc::error_code;
use teton_protocol::methods::{
    BoundaryOriginConfig, ConfigUpdate, PrivacyBoundaryConfig, ProviderConfig, TierBindingConfig,
    WebSetupCommitParams, WebSetupPreviewParams,
};
use teton_protocol::PrivacyMode;
use teton_protocol::{ProviderId, ProviderKind, SessionId, Tier as WireTier};
use tetond::broadcast::EventBus;
use tetond::harness::budget::{self, BudgetInputs, OverBudgetOffer, ProposedWindow, SkillStage};
use tetond::harness::context::Fit;
use tetond::harness::turn_loop::HarnessConfig;
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
/// a provider whose own comment is inside an **array of tables** — the construct
/// ADR-1 originally wrote off as one key, and the one
/// [`registering_a_provider_leaves_the_web_table_and_its_comments_alone`] now
/// holds to the same standard as the rest of the document.
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
    // pid + nanos alone can collide when two tests hit the same clock tick;
    // the counter is what the sibling integration suites' `temp_dir` helpers
    // add for exactly that reason (e.g. model_consent.rs).
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "teton-cfgpreserve-{tag}-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
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
        // REQ-586: a registration that declares no window leaves the stored
        // capabilities alone, so the canonical rendering below is byte-for-byte
        // what it was (`max_context = 0`, no cap line).
        max_context: None,
        context_budget_cap: None,
        allow_cleartext: None,
        floored_budget: None,
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
/// `runtime::config_document::tests` already witnesses this writer at the
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

/// **A `[web]` table the user spelled inline is edited inline, unknown keys and
/// all** (spec BR-1, AC-8).
///
/// TOML gives the same table two spellings, and a writer that only knows the
/// `[header]` one has a second way to destroy a document: replacing
/// `web = { … }` wholesale drops every key inside it the schema cannot see. The
/// engine recurses into both (`config_doc`'s
/// `an_inline_table_keeps_the_keys_the_delta_never_names`); this is the witness
/// that a *writer* gets that behaviour, on the one shape where the whole table
/// is a single line and "only the keys this write is about moved" is a claim
/// about the characters in it.
///
/// The answer has two keys here — the consent record, which is an insertion,
/// and the ceiling, which the answer raises — so both the insert and the assign
/// paths through the inline table are exercised.
#[test]
fn an_inline_web_table_keeps_the_keys_the_consent_answer_is_not_about() {
    // The inline table sits above every section header: a bare key written
    // below one would belong to *that* table, which is TOML rather than
    // anything this test is about.
    let before = r#"# My machine. Hand-written, and staying that way.
effort = "high"
# the whole table on one line, because I like it that way
web = { tier = "fetch_user_url", search_endpoint = "https://api.search.brave.com/res/v1/web/search", search_key_ref = "keychain://teton/web-search", experimental_reranker = "colbert" }

[[providers]]
id = "anthropic"
kind = "anthropic"
endpoint = "https://api.anthropic.com"
model = "claude-opus-4"
auth_ref = "keychain:anthropic"

# Nothing in this build reads this table either.
[experimental]
knob = 3
"#;
    let daemon = Daemon::start("consent-inline", Some(before));
    assert_eq!(
        daemon.reload().web.tier,
        WebTier::FetchUserUrl,
        "the inline spelling must load, or this test is about a document the \
         daemon never understood"
    );

    daemon
        .runtime
        .persist_web_tier(WebTier::FetchAnyUrl)
        .expect("the consent answer lands");

    let after = daemon.document();
    // One line changed — the inline table's — and the rest of the file is
    // untouched, the comment above the key included.
    let (removed, added) = line_diff(before, &after);
    assert_eq!(removed.len(), 1, "{after}");
    assert_eq!(added.len(), 1, "{after}");
    assert!(
        added[0].starts_with("web = {"),
        "the table keeps the spelling the user gave it:\n{after}"
    );
    assert!(
        added[0].contains(r#"experimental_reranker = "colbert""#),
        "a key inside the inline table that this build cannot see survives the \
         write (BR-1):\n{after}"
    );
    assert!(
        added[0].contains(r#"search_key_ref = "keychain://teton/web-search""#),
        "and so does every key the answer is not about:\n{after}"
    );
    assert!(
        after.contains("# the whole table on one line, because I like it that way"),
        "{after}"
    );

    // AC-8: and the one line the write did change means what the answer asked.
    let reloaded = daemon.reload();
    assert_eq!(reloaded.web.tier, WebTier::FetchAnyUrl);
    assert_eq!(reloaded.web.permission_allow, vec![WebTier::FetchAnyUrl]);
    assert_eq!(
        reloaded.web.search_key_ref.as_deref(),
        Some("keychain://teton/web-search")
    );
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
/// It is also the integration-level witness that BR-1 reaches *inside*
/// `[[providers]]`. ADR-1 originally wrote arrays off as one key, which meant a
/// registration re-rendered the array wholesale and deleted the comment inside
/// the entry the user already had. The amended rule diffs the array element-wise:
/// an append leaves every existing element unread and unmoved, so this write
/// deletes **nothing at all** — the entry's own comment survives, and the only
/// insertion is the entry the registration is about. The `[web]` block, the
/// unknown key inside it and the unknown top-level table are byte-identical, as
/// they always were.
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
        // An append reads nothing of what is already there, so it deletes
        // nothing — the comment inside the existing entry included (BR-1).
        &[],
        &[
            // Only the entry the registration is actually about, with the
            // capabilities block the canonical rendering of a *new* element
            // carries. Nothing is written out for the entry already there.
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
    assert!(
        after.contains("# The one I actually pay for."),
        "the comment inside the existing `[[providers]]` entry survives an \
         append (BR-1):\n{after}"
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

/// **A registration that says nothing about the window leaves the stored one
/// alone — and one that declares it writes it** (REQ-586 AC-5, ADR-7), seen
/// from the file a real daemon wrote.
///
/// The wire fields are `Option` for additivity: an older client's
/// re-registration arrives with neither, and it must not zero the
/// `max_context` a user hand-authored (or `/provider setup` recorded). The
/// in-crate merge test proves the property of `apply_update`; this one proves
/// the daemon's writer carries it to disk — LESSON-502's rule, and why both
/// exist.
#[test]
fn a_field_less_registration_preserves_the_stored_window_and_a_declared_one_writes_it() {
    let before = "\
# Hand-written, and staying that way.

[[providers]]
id = \"anthropic\"
kind = \"anthropic\"
endpoint = \"https://api.anthropic.com/v1/messages\"
model = \"claude-opus-4\"
auth_ref = \"keychain:anthropic\"
[providers.capabilities]
max_context = 200000
";
    let daemon = Daemon::start("window-merge", Some(before));
    let register = |max_context: Option<u32>, cap: Option<u32>| {
        ConfigUpdate::RegisterProvider(ProviderConfig {
            id: ProviderId::from("anthropic"),
            kind: ProviderKind::Anthropic,
            endpoint: Some("https://api.anthropic.com/v1/messages".to_owned()),
            model: Some("claude-opus-5".to_owned()),
            auth_ref: Some("keychain:anthropic".to_owned()),
            max_context,
            context_budget_cap: cap,
            allow_cleartext: None,
            floored_budget: None,
        })
    };

    // (1) No fields → the stored window survives the re-registration.
    daemon
        .runtime
        .apply_config_update(register(None, None))
        .expect("the field-less re-registration lands");
    let after = daemon.document();
    assert!(
        after.contains("max_context = 200000"),
        "a registration without the window fields zeroed the hand-authored \
         window (REQ-586 ADR-7, `None` preserves):\n{after}"
    );
    assert_eq!(
        daemon.reload().providers[0].capabilities.max_context,
        200_000,
        "and the loaded config agrees with the document"
    );

    // (2) `Some` writes: the window is edited in place, and a declared cap
    // gains its line (the canonical rendering skips a zero cap, so this is
    // the line-insertion path, not just a value edit).
    daemon
        .runtime
        .apply_config_update(register(Some(128_000), Some(64_000)))
        .expect("the declaring re-registration lands");
    let after = daemon.document();
    assert!(
        after.contains("max_context = 128000") && !after.contains("max_context = 200000"),
        "a declared window must replace the stored one:\n{after}"
    );
    assert!(
        after.contains("context_budget_cap = 64000"),
        "a declared cap must reach the document:\n{after}"
    );
    let reloaded = daemon.reload();
    assert_eq!(reloaded.providers[0].capabilities.max_context, 128_000);
    assert_eq!(
        reloaded.providers[0].capabilities.context_budget_cap,
        64_000
    );
    daemon.cleanup();
}

/// **An unknown key inside a `[[providers]]` entry survives a registration, and
/// survives the next one** (spec BR-1, AC-2, AC-8).
///
/// The half of BR-1 no schema-shaped assertion can see, in the one place it was
/// hardest to keep: *inside* an array element. `Config` drops unknown keys at
/// load, so under the original wholesale array rule a registration re-rendered
/// `[[providers]]` from the in-memory config and this key went without a word.
/// The engine has its own witness
/// (`config_doc::registering_a_provider_appends_and_reads_nothing_of_the_
/// entries_already_there`); this is the one that says the daemon's writer
/// actually reaches it — LESSON-502's rule, and the reason both exist.
///
/// The second registration is not a repetition. An appended element brings a
/// nested `[providers.capabilities]` section, and a second append has to render
/// past *that* rather than merely past the last `[[providers]]` header;
/// getting it wrong re-parents the first entry's sub-table onto the new one and
/// leaves a document that no longer parses, so the write is refused and
/// `teton provider add` fails outright the second time it is used.
#[test]
fn an_unknown_key_inside_a_provider_entry_survives_a_registration() {
    let before = readme_config().replace(
        "auth_ref = \"keychain:anthropic\"\n",
        "auth_ref = \"keychain:anthropic\"\n\
         # Nothing in this build reads this key.\nnickname = \"the good one\"\n",
    );
    assert!(
        before.contains(r#"nickname = "the good one""#),
        "the fixture must actually carry an unknown key inside the array"
    );
    let daemon = Daemon::start("register-unknown", Some(&before));

    daemon
        .runtime
        .apply_config_update(register("cheap"))
        .expect("the first registration lands");
    let once = daemon.document();
    assert!(
        once.contains(r#"nickname = "the good one""#)
            && once.contains("# Nothing in this build reads this key."),
        "an unknown key inside `[[providers]]` survives an append:\n{once}"
    );

    daemon
        .runtime
        .apply_config_update(register("local"))
        .expect("the second registration lands too");
    let twice = daemon.document();
    assert!(
        twice.contains(r#"nickname = "the good one""#),
        "and survives the next one:\n{twice}"
    );

    let reloaded = daemon.reload();
    assert_eq!(reloaded.providers.len(), 3);
    assert_eq!(reloaded.web.tier, WebTier::Search);
    daemon.cleanup();
}

/// **A hand edit made while the daemon runs survives an unrelated write**
/// (spec BR-5, AC-8, AC-10's premise).
///
/// The clobber BR-5 names, driven through the writer most likely to cause it:
/// the daemon reads the config once at start and stays blind to the file until
/// a restart, so its in-memory `cache_ttl_secs` still says 900 while the file
/// says 42. A write that diffed the *document* against the candidate would call
/// that key "changed" and put 900 back; a write that re-serialized the whole
/// config would do it without even noticing. The delta is `diff(current,
/// candidate)` instead (ADR-1), so a key the registration never names cannot
/// enter it.
///
/// `config_doc::a_hand_edit_the_daemon_never_read_rides_along_untouched` is the
/// engine's leg of this; here the drift is a real edit to a real file behind a
/// running daemon, and the write is one a user reaches with
/// `teton provider add`.
#[test]
fn a_hand_edit_mid_session_survives_a_provider_registration() {
    let seeded = readme_config();
    let daemon = Daemon::start("register-drift", Some(&seeded));
    assert_eq!(
        daemon.reload().web.cache_ttl_secs,
        900,
        "the daemon started on the fixture's value"
    );

    // The user edits the file the daemon is not watching.
    let drifted = seeded.replace("cache_ttl_secs = 900", "cache_ttl_secs = 42");
    assert_ne!(drifted, seeded, "the fixture must hold the cache line");
    daemon.hand_edit(&drifted);

    daemon
        .runtime
        .apply_config_update(register("cheap"))
        .expect("the registration lands");

    // Measured against the *drifted* document: the only change is the entry the
    // registration is about, so the hand edit is still there and nothing else
    // moved either.
    let after = daemon.document();
    let (removed, added) = line_diff(&drifted, &after);
    assert!(removed.is_empty(), "the write deleted nothing:\n{after}");
    assert!(
        added.contains(&r#"id = "cheap""#.to_owned()),
        "the registration landed:\n{after}"
    );
    assert!(
        !added.iter().any(|line| line.contains("cache_ttl_secs")),
        "a key this write is not about must not be rewritten (BR-5):\n{after}"
    );

    let reloaded = daemon.reload();
    assert_eq!(
        reloaded.web.cache_ttl_secs, 42,
        "the document is the truth about a key the operation never named"
    );
    assert_eq!(reloaded.providers.len(), 2);
    daemon.cleanup();
}

/// **A provider hand-added mid-session under the id being registered makes the
/// write refuse, and the file survives it** (spec BR-4/BR-5, AC-10).
///
/// The append branch of the array rule is the one branch that reads *no*
/// existing element — it pushes past them — which is what makes it safe under a
/// reorder and also what makes it blind to a hand-added element. The per-index
/// branch closes that blindness itself, by checking the document's element
/// carries the identity key the delta's index was computed against
/// (`config_doc::identity_field`). This test pins the deliberate decision **not**
/// to give the append branch the same guard: the collision it can produce is a
/// duplicate id, the edited-bytes gate already refuses exactly that, and the
/// refusal names the key in the validator's own words. A second guard would buy
/// a different sentence for a case already covered, at the cost of turning a
/// safe append into a wholesale rewrite whenever the document's array is longer
/// than memory's.
///
/// So: the daemon starts holding one provider, the user hand-adds a second one
/// mid-session, and then registers a provider with that same id. The write is
/// refused, the message says which id is doubled, and both the file and the live
/// config are exactly where they were.
#[test]
fn a_hand_added_provider_under_the_id_being_registered_refuses_the_write() {
    let seeded = readme_config();
    let daemon = Daemon::start("register-hand-added-twin", Some(&seeded));
    assert_eq!(
        daemon.reload().providers.len(),
        1,
        "the daemon started holding one provider"
    );

    // The user adds the provider by hand, in the file the daemon is not
    // watching — and then reaches for `teton provider add` for the same id,
    // having forgotten (or not known) that the edit already landed.
    let drifted = seeded.replace(
        "[web]\n",
        "[[providers]]\n\
         # added by hand, five minutes ago\n\
         id = \"cheap\"\n\
         kind = \"openai-compatible\"\n\
         endpoint = \"https://api.deepseek.com\"\n\
         model = \"deepseek-chat\"\n\
         auth_ref = \"keychain:cheap\"\n\n\
         [web]\n",
    );
    assert_ne!(drifted, seeded, "the fixture must hold a `[web]` header");
    daemon.hand_edit(&drifted);

    let refused = daemon
        .runtime
        .apply_config_update(register("cheap"))
        .expect_err("a registration that would double an id must not be written");

    assert_eq!(refused.code, error_code::CONFIG_REJECTED);
    assert!(
        refused
            .message
            .contains("provider 'cheap' is defined more than once"),
        "the refusal must carry the validator's own sentence, which names the id \
         the user has to resolve: {}",
        refused.message
    );
    assert_eq!(
        daemon.document(),
        drifted,
        "a refused write reached the file"
    );
    assert_eq!(
        daemon.runtime.config_snapshot().providers.len(),
        1,
        "the in-memory swap happens after the write, never before"
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
        // The migrated key lives in an array of tables, and the amended ADR-1
        // rule edits the element in place rather than re-rendering the array —
        // so the migration deletes nothing, not even the comment that says the
        // field did not exist yet.
        &[],
        // One key, which is what the migration is about.
        &[r#"model = "claude-fable-5""#],
    );
    assert!(
        after.contains("# no model here — the field did not exist yet"),
        "a per-element edit leaves the element's own comments in place \
         (BR-1):\n{after}"
    );

    let reloaded = daemon.reload();
    assert_eq!(
        reloaded.providers[0].model.as_deref(),
        Some("claude-fable-5"),
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
/// `runtime::config_document::tests`; the two writers here are the ones
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
/// own witness is in `runtime::config_document::tests`; this is the same
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

// ---------------------------------------------------------------------------
// REQ-589 — the going-forward remedy, read back off the file the daemon wrote
// ---------------------------------------------------------------------------
//
// AC-13 and ADR-5, from the disk side. The *ordering* of BR-9's pair is pinned
// at its own seam by `runtime::tests::…::the_rebind_declares_the_window_first_
// so_the_circle_is_unreachable`, which is where `RemedyWrites::apply` can be
// driven with a failing applier; `RemedyWrites` and `plan_over_budget_remedy`
// are private to `runtime`, so nothing here can reach them. What this suite
// owns is the other half: **what the file is left holding**, in each case,
// inspected as bytes and re-parsed through the production loader — the double
// check `a_field_less_registration_preserves_the_stored_window_and_a_declared_
// one_writes_it` above sets the pattern for (LESSON-519), on the writer path a
// user's config actually goes through.
//
// The three legs are one matched set on one fixture, and none of them means
// anything alone (LESSON-520):
//
// * the pair applied in ADR-5's order writes both halves and moves nothing else;
// * a refused **second** write leaves the harmless half — a declared window on a
//   tier bound where it was — and the same fixture demonstrates that the
//   **reverse** order's first write really does reach the forbidden state, so
//   the "no circle" assertion discriminates between the two orders rather than
//   passing for some unrelated reason;
// * a refused write leaves the document byte-identical, which is only evidence
//   because the accepted leg proves the same shape does write.

/// The machine the reported `/analyze` failure ran on, as a config: one remote
/// provider registered with a model and **no declared window**, and no
/// `[[tiers]]` row at all — so every tier still falls through to the local
/// engine, which is what makes the route's bound `LocalEngine` and its BR-7
/// remedy the two-part rebind.
///
/// Every provider declares a model (or is local), which keeps the REQ-557
/// startup migration out of these tests for the reason [`CONFIG_PREAMBLE`]
/// records: a migration that fired at `from_env` would rewrite the document
/// before the writer under test ever ran, and a byte-identical assertion would
/// be measuring the migration.
const REBIND_FIXTURE: &str = r#"# The machine that hit this. Hand-written, and staying that way.
effort = "high"

[[providers]]
# Registered months ago. Nothing has ever been routed to it.
id = "kimi"
kind = "openai-compatible"
endpoint = "https://api.moonshot.ai/v1/chat/completions"
model = "kimi-k3"
auth_ref = "keychain:kimi"

[[providers]]
id = "local"
kind = "local"

# Nothing in this build reads this table.
[experimental]
knob = 3
"#;

/// A measurement over the local pair in both currencies — the shape
/// `would_seed_fit` hands the offer when a skill expansion does not fit.
fn twice_the_local_pair() -> Fit {
    Fit {
        tokens: budget::LOCAL_BUDGET_TOKENS * 2,
        bytes: budget::LOCAL_BUDGET_BYTES * 2,
        fits: false,
    }
}

/// The window BR-9's first write declares, **from the one home** (BR-7c,
/// LESSON-546): [`budget::proposed_window`] reads the shipped vendor catalog by
/// **model**, and carries back the date that figure was last read off the
/// vendor's own documentation (ADR-7).
///
/// Nothing here invents, rounds or scales a number, and the test asserts the
/// value that reaches disk *equals this one* rather than a literal of its own —
/// a pinned figure here would be a second home for it.
fn catalogued_window_for_kimi(measured: Fit) -> ProposedWindow {
    budget::proposed_window(
        Some("kimi-k3"),
        BudgetInputs {
            // Undeclared — the fixture's whole premise, and why the remedy has
            // a window to write at all.
            window: 0,
            cap: 0,
            // ADR-1's reservation: the `max_tokens` the adapters send.
            reservation: HarnessConfig::default().gen_params.max_tokens,
            is_local: false,
            redact_scan: false,
            provider_id: Some("kimi"),
        },
        measured,
    )
    .expect(
        "`kimi-k3` is in the shipped catalog and its window clears the measurement — if this \
         stops holding, the catalog moved and the remedy stopped being offerable, which is a \
         change to make deliberately",
    )
}

/// BR-9's **first** write: `kimi` re-registered field-wise with its window
/// declared (REQ-586 ADR-7's merge — `None` on the fields it is not about).
///
/// The identity is re-stated rather than omitted because `RegisterProvider`
/// replaces those fields wholesale and `apply_config_update` refuses a remote
/// provider with no model; that is `field_wise_registration`'s own reasoning,
/// mirrored here because the type it builds is private to `runtime`.
fn declare_kimis_window(max_context: u32) -> ConfigUpdate {
    ConfigUpdate::RegisterProvider(ProviderConfig {
        id: ProviderId::from("kimi"),
        kind: ProviderKind::OpenaiCompatible,
        endpoint: Some("https://api.moonshot.ai/v1/chat/completions".to_owned()),
        model: Some("kimi-k3".to_owned()),
        auth_ref: Some("keychain:kimi".to_owned()),
        max_context: Some(max_context),
        context_budget_cap: None,
        allow_cleartext: None,
        floored_budget: None,
    })
}

/// BR-9's **second** write: the `build` tier bound to `kimi`.
///
/// `fallback` is `None` in the remedy — a fallback is a second routing decision
/// and nobody was asked for one. It is a parameter here only so the refusal leg
/// can be refused at one of the daemon's real gates; see that test.
fn bind_build_to_kimi(fallback: Option<&str>) -> ConfigUpdate {
    ConfigUpdate::SetTierBinding(TierBindingConfig {
        tier: WireTier::Build,
        provider_id: ProviderId::from("kimi"),
        fallback_id: fallback.map(ProviderId::from),
    })
}

/// `kimi`'s declared window, as the production loader reads it back.
fn window_on_disk(config: &Config) -> u32 {
    config
        .providers
        .iter()
        .find(|p| p.id == "kimi")
        .expect("a field-wise registration must not lose the provider")
        .capabilities
        .max_context
}

/// The provider the `build` tier is bound to, or `None` when no row binds it.
fn build_tier_binding(config: &Config) -> Option<String> {
    config
        .tiers
        .iter()
        .find(|binding| binding.tier == Tier::Build)
        .map(|binding| binding.provider_id.clone())
}

/// **AC-13, ADR-5 — the ordered pair, both halves, on disk.**
///
/// The accepted counterpart of the two refusal legs below: it is what makes
/// their "nothing was written" / "only the harmless half was written"
/// assertions mean something, because it proves these exact payloads do write
/// on this exact fixture (LESSON-520).
///
/// Verified **both** ways, per `a_field_less_registration_preserves_the_stored_
/// window_and_a_declared_one_writes_it`: the bytes the file holds, and what the
/// production loader makes of them. A return code is not consulted for either
/// claim (LESSON-519).
#[test]
fn the_ordered_rebind_declares_the_window_then_binds_the_tier_and_both_reach_disk() {
    let daemon = Daemon::start("ob-rebind-applied", Some(REBIND_FIXTURE));
    let before = daemon.document();
    let measured = twice_the_local_pair();
    let proposal = catalogued_window_for_kimi(measured);

    // FIRST (ADR-5): the window.
    daemon
        .runtime
        .apply_config_update(declare_kimis_window(proposal.tokens))
        .expect("the window declaration lands");
    // SECOND: the binding.
    daemon
        .runtime
        .apply_config_update(bind_build_to_kimi(None))
        .expect("the tier binding lands");

    // --- the file's bytes -------------------------------------------------
    //
    // Both writes, and **nothing else** — the mechanical form of BR-1, which is
    // this suite's own duty and which a remedy applied from inside a consent
    // answer is not excused from. The empty removal list is the load-bearing
    // half: no comment, no key and no unknown table was rewritten on the way,
    // so the two hand-authored comments and `[experimental]` are still there by
    // the same evidence that says the window landed.
    let window_line = format!("max_context = {}", proposal.tokens);
    let after = daemon.document();
    assert_only_these_lines_changed(
        &before,
        &after,
        &[],
        &[
            // FIRST — the window, into a capabilities table this hand-written
            // config never had.
            "[providers.capabilities]",
            &window_line,
            "",
            "",
            // SECOND — the binding.
            "[[tiers]]",
            r#"tier = "build""#,
            r#"provider_id = "kimi""#,
        ],
    );

    // --- and the same file, re-parsed ------------------------------------
    let reloaded = daemon.reload();
    assert_eq!(
        window_on_disk(&reloaded),
        proposal.tokens,
        "the loaded config must agree with the document:\n{after}"
    );
    assert_eq!(
        build_tier_binding(&reloaded).as_deref(),
        Some("kimi"),
        "the loaded config must agree with the document:\n{after}"
    );

    // ADR-7's date is **not** asserted here, and the reason is a finding rather
    // than an omission: `Remedy::BindTierRemote`'s clause names neither the
    // figure nor its provenance — it says "declare that provider's
    // `capabilities.max_context`" and stops, which is the same gap ADR-18 item 2
    // records for the provider's name. The window/date tie is therefore pinned
    // on the bound whose label does carry both, in
    // [`the_window_written_to_disk_is_the_one_the_offer_named_with_its_date`].

    daemon.cleanup();
}

/// **AC-13, ADR-5 — a refused SECOND write leaves the harmless half, and the
/// circle stays unreachable.**
///
/// The forbidden state AC-8 names is a newly-bound remote tier whose provider
/// declares `max_context = 0`: the user pays a remote provider and derives the
/// *same* default pair under `bound: unknown window`, which is the circle the
/// reported `/analyze` failure was already sitting in. In ADR-5's order that
/// state is unreachable from a partial failure; the last block of this test
/// demonstrates — on the same fixture, from the same file — that the reverse
/// order's first write really does reach it, which is what makes the assertion
/// above it discriminating rather than decorative.
///
/// **What refuses the second write is not the point, and is stated rather than
/// implied.** The remedy's own binding carries `fallback_id: None` and has no
/// failure mode of its own, so the refusal is induced at one of the daemon's
/// real gates — `Config::validate`'s unregistered-fallback rule — rather than
/// mocked. ADR-5's claim is about the state a failure *leaves*, not about its
/// cause, and this is that state.
#[test]
fn a_refused_second_write_leaves_a_declared_window_on_an_unbound_tier_never_the_circle() {
    let daemon = Daemon::start("ob-rebind-partial", Some(REBIND_FIXTURE));
    let measured = twice_the_local_pair();
    let proposal = catalogued_window_for_kimi(measured);

    daemon
        .runtime
        .apply_config_update(declare_kimis_window(proposal.tokens))
        .expect("the window declaration lands");
    let after_first = daemon.document();

    let refused = daemon
        .runtime
        .apply_config_update(bind_build_to_kimi(Some("a-provider-nobody-registered")))
        .expect_err("the second write was made to fail at a real gate");
    assert_eq!(refused.code, error_code::CONFIG_REJECTED, "{refused:?}");

    // A refused write leaves the file exactly as the *first* write left it —
    // the gate runs above `persist_config`, and this is what proves it did.
    let after = daemon.document();
    assert_eq!(
        after, after_first,
        "a refused second write must not touch the document at all"
    );

    // --- bytes ---
    assert!(
        after.contains(&format!("max_context = {}", proposal.tokens)),
        "the harmless half landed and stays landed:\n{after}"
    );
    assert!(
        !after.contains("[[tiers]]"),
        "**the forbidden state**: a partial failure bound the tier. In ADR-5's order it \
         cannot — the binding is the write that did not happen:\n{after}"
    );
    // --- and the re-parse ---
    let reloaded = daemon.reload();
    assert_eq!(window_on_disk(&reloaded), proposal.tokens);
    assert_eq!(
        build_tier_binding(&reloaded),
        None,
        "no `[[tiers]]` row may bind `build` after a failed second write:\n{after}"
    );
    // The state is not merely "no circle" — it is a route that never moved, so
    // there is nothing for the user to undo and no spend they did not choose.
    assert_eq!(
        budget::derive(BudgetInputs::local()).bound,
        BudgetBound::LocalEngine,
        "the tier is still the local one it was, which is what makes the half-applied \
         pair harmless"
    );

    // --- why the order is that way round, on disk ------------------------
    //
    // The same fixture, the reverse order, its first write applied alone —
    // which is exactly the state a failure between the reversed pair leaves.
    let circled = Daemon::start("ob-rebind-reversed", Some(REBIND_FIXTURE));
    circled
        .runtime
        .apply_config_update(bind_build_to_kimi(None))
        .expect("the reverse order's first write is a perfectly valid binding");
    let document = circled.document();
    let reloaded = circled.reload();
    assert_eq!(
        build_tier_binding(&reloaded).as_deref(),
        Some("kimi"),
        "the reverse order binds the tier first:\n{document}"
    );
    assert_eq!(
        window_on_disk(&reloaded),
        0,
        "…to a provider that still declares no window — **the forbidden state**, on \
         disk:\n{document}"
    );
    let circle = budget::derive(BudgetInputs {
        window: window_on_disk(&reloaded),
        cap: 0,
        reservation: HarnessConfig::default().gen_params.max_tokens,
        is_local: false,
        redact_scan: false,
        provider_id: Some("kimi"),
    });
    assert_eq!(
        circle.bound,
        BudgetBound::DefaultUnknown,
        "a tier bound to a provider with no declared window derives the default pair under \
         `bound: unknown window` — which is why that state must be unreachable, not merely \
         unlikely"
    );
    assert!(
        measured.tokens > circle.budget_tokens,
        "and the same measurement overflows it again: {} vs {} — the user would have paid a \
         remote provider to meet the identical refusal (BR-9)",
        measured.tokens,
        circle.budget_tokens
    );
    circled.cleanup();

    daemon.cleanup();
}

/// **AC-13 / LESSON-520 — a refused remedy write leaves the document
/// byte-identical, and that is evidence only because the accepted leg exists.**
///
/// The payload is the remedy's own second write with one field changed to
/// something the daemon refuses, so it is a payload that **would** persist:
/// `the_ordered_rebind_declares_the_window_then_binds_the_tier_and_both_reach_
/// disk` above writes the identical binding on the identical fixture. That is
/// the pairing LESSON-520 requires — without it, "the file did not change"
/// passes for a payload that could never have changed it, and cannot tell a
/// gate from a parser.
///
/// The refusal here lands *above* `persist_config`: `Config::validate` refuses
/// the candidate, so the write never reaches the document at all. The bytes are
/// what says so, not the error code (LESSON-519).
#[test]
fn a_refused_remedy_write_leaves_the_document_byte_identical() {
    let daemon = Daemon::start("ob-rebind-refused", Some(REBIND_FIXTURE));
    let before = daemon.document();

    let refused = daemon
        .runtime
        .apply_config_update(bind_build_to_kimi(Some("a-provider-nobody-registered")))
        .expect_err("an unregistered fallback is refused");
    assert_eq!(refused.code, error_code::CONFIG_REJECTED, "{refused:?}");

    assert_eq!(
        daemon.document(),
        before,
        "a refused remedy write must leave config.toml byte-identical"
    );
    let reloaded = daemon.reload();
    assert_eq!(
        build_tier_binding(&reloaded),
        None,
        "and the loaded config must agree — nothing was bound"
    );
    assert_eq!(window_on_disk(&reloaded), 0, "and nothing was declared");
    daemon.cleanup();
}

/// The circle itself, as a config: the `build` tier already bound to a remote
/// provider that declares **no** window, so the route derives the default pair
/// under `bound: unknown window`. This is the state ADR-5's ordering exists to
/// keep unreachable — and, once a user is in it, the bound whose BR-7 remedy is
/// `DeclareWindow`: one write, addressed to a named provider, with a figure.
const DECLARE_WINDOW_FIXTURE: &str = r#"# Bound last month. The window was never declared.
effort = "high"

[[providers]]
id = "kimi"
kind = "openai-compatible"
endpoint = "https://api.moonshot.ai/v1/chat/completions"
model = "kimi-k3"
auth_ref = "keychain:kimi"

[[tiers]]
tier = "build"
provider_id = "kimi"
"#;

/// **ADR-7 / BR-7c — the number written to disk is the number the offer named,
/// and the date it was read was named with it.**
///
/// `verified_on` is deliberately not a config key: a provenance claim written
/// into a file the user hand-edits would go stale silently and could not be
/// corrected by re-reading the catalog. ADR-7 records it where a human can act
/// on it instead — in the option label that offers the write. So "recorded
/// alongside the written window" is a tie between two artifacts, and this test
/// is that tie: the label the user was shown carries the figure **and** the
/// date, and the figure it carries is the one the loader reads back off disk.
///
/// Both come from [`budget::proposed_window`], the one home (LESSON-546). The
/// test pins no literal window and no literal date — a second copy of either
/// here would be the drift the one-home rule exists to prevent — so a catalog
/// re-verification moves both sides together and this stays green, while a
/// change that let the label and the write disagree cannot.
#[test]
fn the_window_written_to_disk_is_the_one_the_offer_named_with_its_date() {
    let measured = twice_the_local_pair();
    // The route as the router stamps it: a remote provider with an undeclared
    // window derives the default pair and says so.
    let inputs = BudgetInputs {
        window: 0,
        cap: 0,
        reservation: HarnessConfig::default().gen_params.max_tokens,
        is_local: false,
        redact_scan: false,
        provider_id: Some("kimi"),
    };
    let budget = budget::derive(inputs);
    assert_eq!(
        budget.bound,
        BudgetBound::DefaultUnknown,
        "the fixture must be the bound whose remedy is a single, addressed window write"
    );
    let proposal = catalogued_window_for_kimi(measured);

    // What the user is shown, composed by the one composer (ADR-16).
    let offer = OverBudgetOffer::new(
        "analyze",
        SkillStage::Body,
        measured,
        &budget,
        inputs.window,
        Some(proposal.clone()),
        None,
    );
    let labels = offer.option_labels();
    let remedy = labels
        .remedy
        .as_ref()
        .expect("`DefaultUnknown` with a catalogued figure carries BR-7's remedy");
    for label in [&remedy.proceed_and_remedy, &remedy.remedy_only] {
        assert!(
            label.contains(&format!("capabilities.max_context = {}", proposal.tokens)),
            "ADR-1: the label must name the concrete write, figure included:\n{label}"
        );
        assert!(
            label.contains(&proposal.verified_on) && label.contains(&proposal.vendor),
            "ADR-7: the date that figure was read off the vendor's docs must ride beside \
             it, or a later `/doctor` cannot tell an inherited window from a measured \
             one:\n{label}"
        );
    }

    // And what actually lands, read back off the file (LESSON-519).
    let daemon = Daemon::start("ob-declare-window", Some(DECLARE_WINDOW_FIXTURE));
    let before = daemon.document();
    daemon
        .runtime
        .apply_config_update(declare_kimis_window(proposal.tokens))
        .expect("the window declaration lands");

    let after = daemon.document();
    let window_line = format!("max_context = {}", proposal.tokens);
    assert_only_these_lines_changed(
        &before,
        &after,
        &[],
        &["[providers.capabilities]", &window_line, ""],
    );
    assert_eq!(
        window_on_disk(&daemon.reload()),
        proposal.tokens,
        "the figure the label promised is the figure the loader reads back:\n{after}"
    );

    // The remedy did what it was for: the same measurement now fits.
    let repaired = budget::derive(BudgetInputs {
        window: window_on_disk(&daemon.reload()),
        ..inputs
    });
    assert_eq!(repaired.bound, BudgetBound::Window);
    assert!(
        measured.tokens <= repaired.budget_tokens && measured.bytes <= repaired.budget_bytes,
        "BR-7c's second rule, seen from the file: a proposed window that would not clear \
         the measurement is never offered, so the one that was offered must clear it — \
         {measured:?} against {}/{}",
        repaired.budget_tokens,
        repaired.budget_bytes
    );
    daemon.cleanup();
}

// ---------------------------------------------------------------------------
// REQ-597 AC-10 — an existing config gains the builtin set without a rewrite
// ---------------------------------------------------------------------------

/// A config authored before REQ-597: two boundary rows, a comment inside the
/// block, and no `[privacy]` table at all.
fn pre_req597_config() -> String {
    "\
[[providers]]
id = \"cheap\"
kind = \"openai-compatible\"
endpoint = \"https://api.deepseek.com\"
model = \"deepseek-chat\"

# the one thing I actually care about keeping private
[[boundaries]]
path_glob = \"secrets/**\"
mode = \"local-only\"

[[boundaries]]
path_glob = \"docs/**\"
mode = \"redact-then-remote\"
"
    .to_owned()
}

/// **REQ-597 AC-10.** A machine that already declared boundaries gains the
/// builtin set on upgrade **without a config rewrite**, and its own rows stay
/// byte-unchanged on disk.
///
/// The write is driven by an **unrelated** `config/set` — a provider
/// registration — because that is the shape of the real risk. Simply reading
/// the config back would prove nothing about the writer; what could go wrong
/// here is that the *next* thing the user does, about something else entirely,
/// quietly rewrites their boundaries table.
///
/// Two mechanisms protect the file, and this test covers them in two legs
/// because — measured, not assumed — one leg does not reach both:
///
/// - **Leg 1 (an unrelated write)**: builtin rows never enter
///   `Config.boundaries` (ADR-1), or `canonical_document` would diff them as
///   rows the user is missing and write all thirteen into their file.
/// - **Leg 2 (a write that touches the block)**: `origin` skips serialization
///   for a `User` row (ADR-3), or a newly added `[[boundaries]]` entry carries
///   an `origin = "user"` line the user never wrote.
///
/// Leg 2 exists because leg 1 turned out not to guard ADR-3 at all. Removing
/// `skip_serializing_if` leaves leg 1 green: `apply_config_delta` diffs an
/// array of tables element-wise, so an element nothing changed is never
/// re-rendered and never gains the key. Only a write that actually emits a
/// boundary element can surface it — which is the difference between a test
/// that guards a mechanism and one that merely runs near it (LESSON-569).
///
/// The assertion is on the file's **bytes**, not on a parsed round trip: a
/// parsed comparison would read `origin = "user"` back as the same value it
/// already had and report no change at all.
///
/// **Mutations**: populate `Config.boundaries` with the builtin set at load →
/// leg 1 fails. Remove `skip_serializing_if` from `PrivacyBoundary::origin` →
/// leg 2 fails.
#[test]
fn an_existing_config_gains_the_builtin_set_without_rewriting_its_own_rows() {
    let before = pre_req597_config();
    let daemon = Daemon::start("req597-upgrade", Some(&before));

    // The upgrade half: this machine is protected by its own two rows *and* by
    // the shipped set, without having been asked to change anything.
    let effective = daemon.reload().effective_boundaries();
    assert_eq!(
        effective.len(),
        2 + teton_core::config::DEFAULT_BOUNDARIES.len(),
        "an upgraded machine keeps its rows and gains the builtin set"
    );
    assert_eq!(
        effective[0].path_glob, "secrets/**",
        "the user's rows come first"
    );
    assert_eq!(effective[1].path_glob, "docs/**");
    assert!(
        effective
            .iter()
            .any(|b| b.path_glob == "**/.ssh/**" && b.origin == BoundaryOrigin::Builtin),
        "the builtin rows are present and labelled"
    );

    // The no-rewrite half: an unrelated write touches only its own keys.
    daemon
        .runtime
        .apply_config_update(register("second"))
        .expect("the registration lands");
    let after = daemon.document();

    assert_only_these_lines_changed(
        &before,
        &after,
        // Nothing is deleted: an append reads nothing of what is already there.
        &[],
        // The registration and nothing else: the whole new `[[providers]]`
        // element as the canonical rendering emits it, blank separators
        // included. Every line of the `[[boundaries]]` block is absent from
        // this list, which is the assertion — the write did not touch it.
        &[
            "[[providers]]",
            r#"id = "second""#,
            r#"kind = "openai-compatible""#,
            r#"endpoint = "https://api.deepseek.com""#,
            r#"model = "deepseek-chat""#,
            r#"auth_ref = "keychain:cheap""#,
            "",
            "[providers.capabilities]",
            "tool_call_tier = \"native\"",
            "parallel_calls = false",
            "max_context = 0",
            "",
        ],
    );

    // Said again directly, because it is the failure mode with teeth and a
    // reader should not have to derive it from the list above.
    assert!(
        !after.contains("origin"),
        "a user's boundary rows must not grow an `origin` key on disk:\n{after}"
    );
    for glob in teton_core::config::DEFAULT_BOUNDARIES {
        assert!(
            !after.contains(glob),
            "builtin {glob} was written into the user's config file:\n{after}"
        );
    }
    assert!(
        after.contains("# the one thing I actually care about keeping private"),
        "the comment inside the boundaries block survived:\n{after}"
    );

    // Leg 2: a write that really does emit a boundary element. This is the only
    // shape in which `origin`'s serialization can reach the file, so it is the
    // only shape that can guard ADR-3.
    daemon
        .runtime
        .apply_config_update(ConfigUpdate::SetPrivacyBoundary(PrivacyBoundaryConfig {
            path_glob: "vendor/**".to_owned(),
            mode: PrivacyMode::LocalOnly,
            // What `config/set` carries for a row a user just added. The daemon
            // must not persist it.
            origin: BoundaryOriginConfig::User,
        }))
        .expect("adding a boundary lands");
    let written = daemon.document();

    assert!(
        written.contains(r#"path_glob = "vendor/**""#),
        "the added row must actually be written, or the assertion below is \
         about nothing:\n{written}"
    );
    assert!(
        !written.contains("origin"),
        "a boundary row written through `config/set` must carry no `origin` \
         key — it is a reporting field, not a config one:\n{written}"
    );
    for glob in teton_core::config::DEFAULT_BOUNDARIES {
        assert!(
            !written.contains(glob),
            "builtin {glob} reached the user's file through a boundary write:\n{written}"
        );
    }

    daemon.cleanup();
}

/// REQ-611 AC-19 (integration leg): a `config.toml` that names `[transcript]`
/// keeps it byte-for-byte across an unrelated `config/set`, and one that
/// never named it does not gain the table.
///
/// **Mutation (run 2026-09-03):** dropping `skip_serializing_if` from
/// `Config.transcript` left this test **green** — `apply_config_delta` diffs
/// the caller's pre-mutation `Config` against its candidate, so an untouched
/// table can never enter the delta whatever the schema says (the same finding
/// TASK-360 recorded). The mutation with teeth is a write that *touches* the
/// table: making `apply_update`'s `RegisterProvider` arm also set
/// `config.transcript.enabled = true` reddened the "not invented" leg
/// (`[transcript]` appeared in the bare document); restored.
#[test]
fn a_transcript_table_survives_an_unrelated_write_and_is_never_invented() {
    let named = format!(
        "{}\n# my transcript posture, hand-written\n[transcript]\nenabled = true\nretain_days = 7\n",
        readme_config()
    );
    let daemon = Daemon::start("transcript-kept", Some(&named));
    daemon
        .runtime
        .apply_config_update(register("cheap"))
        .expect("the registration lands");
    let after = daemon.document();
    for line in [
        "# my transcript posture, hand-written",
        "[transcript]",
        "enabled = true",
        "retain_days = 7",
    ] {
        assert!(
            after.contains(line),
            "`{line}` survives an unrelated write:\n{after}"
        );
    }
    assert_eq!(
        after.matches("[transcript]").count(),
        1,
        "exactly one table, never duplicated:\n{after}"
    );

    let bare = readme_config();
    let daemon = Daemon::start("transcript-absent", Some(&bare));
    daemon
        .runtime
        .apply_config_update(register("cheap"))
        .expect("the registration lands");
    let after = daemon.document();
    assert!(
        !after.contains("[transcript]"),
        "a config that never named `[transcript]` does not gain it on an unrelated write:\n{after}"
    );
}
