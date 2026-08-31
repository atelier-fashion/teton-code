//! REQ-599 step 2: rendering and persisting the config document.
//!
//! Extracted from `runtime.rs` whole — the first slice, chosen because it is
//! **contiguous** in the original file and already had a matching test module.
//! ADR-1 refuted the idea that rationale ids mark the seams; this is what ADR-2
//! uses instead, a group the code's own structure already separates.
//!
//! Nothing here changed but its address. The comparison that proves it is
//! `crates/tetond/tests/traceability_sweep.rs`, whose re-attachment arm asserts
//! every rationale id still annotates the item it explained before the move.

// Everything this slice needs from its former home. A glob rather than an
// enumerated list, deliberately: the point of step 2 is that nothing changed
// but the address, and an enumerated list would be a second place to edit every
// time the parent's imports move — drift with no benefit. Later steps narrow it
// once the parent has stopped shrinking.
use super::*;

/// Why the document a write would produce could not be derived.
///
/// Typed rather than `anyhow`, because two callers have to **classify** the
/// failure and not merely report it: `/web setup`'s preview and commit answer a
/// document that would not load with [`error_code::WEB_SETUP_INVALID`] — the
/// drift is in the user's file and the validator's sentence names the key to fix
/// — and everything else with an internal error.
///
/// Every variant's `Display` **embeds** the inner reason rather than attaching it
/// as context, because every caller formats with `{err}` and anyhow shows only
/// the outermost layer (LESSON-456, BUG-146: a write that fails must say what
/// failed, not "write failed").
#[derive(Debug, thiserror::Error)]
pub(super) enum RenderError {
    /// The file is there and could not be read. An unreadable file is not an
    /// empty one.
    ///
    /// The **kind** and not the whole `io::Error`, which is what the neighboring
    /// [`load_config`] already does for the same file (REQ-572 BR-11): the
    /// `Display` of an I/O error can carry the path it failed on, and a config
    /// path is a filesystem fact this daemon does not put in a message a client
    /// renders.
    #[error(
        "the existing configuration could not be read for editing, so nothing was written: {}",
        .0.kind()
    )]
    Read(std::io::Error),

    /// The document could not be edited — an unparseable file, in practice
    /// (BR-6). Carries [`teton_core::DeltaError`]'s own sentence unchanged.
    #[error("{0}")]
    Edit(#[from] teton_core::DeltaError),

    /// The edited bytes would not load. Built by [`load_failure_reason`], which
    /// is where the two halves of that are told apart: the validator's own
    /// sentence rides inside because it names the key the user has to fix (BR-4,
    /// AC-10), and a *parse* failure is reduced to its location.
    #[error("the edited configuration would not load, so nothing was written: {0}")]
    Invalid(String),
}

/// The sentence [`RenderError::Invalid`] carries for a [`Config::load`] failure
/// on the **edited** bytes.
///
/// # The validator's arm goes through verbatim
///
/// A `ConfigError` is a sentence this codebase writes — "`default_provider`
/// names provider 'ghost', which is not registered" — naming a key and a rule.
/// It is the whole value of BR-4's refusal: a user whose hand edit is the
/// obstacle is told which key to fix. Rewording it here would make the daemon's
/// two refusal paths (startup and write) describe the same file differently.
///
/// # The parser's arm is reduced to a location, deliberately
///
/// [`toml::de::Error`]'s `Display` reproduces the offending source line under a
/// caret, and its `message()` alone still quotes the offending *value* for a
/// type mismatch (`invalid type: string "…", expected u64`). Either one puts a
/// line of the user's config into an RPC error string that travels to a client
/// and into a transcript — and `[mcp_server.transport] env` values, `auth_ref`
/// and a pasted `search_endpoint` are exactly the lines that hold credentials
/// (REQ-563 BR-7's rule, which the whole web event family follows). So the
/// location is reported and the text is not.
///
/// The cost is real and it is the right trade: this arm means the delta engine
/// emitted TOML it cannot re-read, which is a bug in this daemon and not in the
/// user's file. It is near-unreachable, it must stay loud — the line and column
/// are enough to reproduce it against a document the reporter still has — and
/// it must not be the one path that leaks what every other path redacts.
pub(super) fn load_failure_reason(err: &teton_core::config::LoadError, edited: &str) -> String {
    match err {
        // `#[error(transparent)]` — this *is* the validator's sentence.
        teton_core::config::LoadError::Validate(_) => err.to_string(),
        teton_core::config::LoadError::Parse(parse) => {
            let at = parse
                .span()
                .map(|span| line_and_column(edited, span.start))
                .map_or_else(
                    || "somewhere this daemon could not locate".to_owned(),
                    |(line, column)| format!("line {line}, column {column}"),
                );
            format!(
                "the document this write derived is not parseable TOML at {at}, which is a bug \
                 in this daemon rather than in your file. The offending text is deliberately \
                 not quoted here: a config line can hold a credential."
            )
        }
    }
}

/// The 1-based line and column of `offset` in `text`.
///
/// [`teton_core::config_doc::position_of`]'s arithmetic, not a second copy of
/// it. This used to slice `&text[..offset]`, which **panics** when a span lands
/// mid-character — and a config document holds arbitrary UTF-8 in its comments,
/// so a byte offset into one is not a char boundary by construction. The panic
/// would fire inside the held config mutex, poisoning it, and every later
/// `lock().expect(…)` in this daemon would abort the process: a mislocated
/// column in a bug report turned into a daemon that stops serving. The shared
/// function walks bytes instead, and counts characters for the column, so an
/// offset that lands anywhere at all still answers.
pub(super) fn line_and_column(text: &str, offset: usize) -> (usize, usize) {
    teton_core::config_doc::position_of(text, offset)
}

/// Which config the delta's `current` side is taken from.
///
/// ADR-1's answer is [`Self::InMemory`] for every writer, and it is right for
/// every writer whose operation is *about* in-memory state. `/web setup` is not
/// one: its four fields are pinned by answers the user just gave, and a delta
/// that never mentions them writes a document that contradicts them. See
/// [`DeltaBase::DocumentPinsWebAnswers`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum DeltaBase {
    /// The caller's `current`, exactly as ADR-1 describes: the operation's
    /// footprint is where `current` and `candidate` differ, so drift at every
    /// other key is never in the delta and survives by construction (BR-5).
    InMemory,

    /// The caller's `current`, with the four fields `/web setup` collects —
    /// `tier`, `search_endpoint`, `search_key_ref`, `search_auth` — replaced by
    /// the **document's own** parse of them.
    ///
    /// The verify pass's finding, and the one case where a pure in-memory base
    /// silently drops an answer. The flow's candidate is memory-plus-answers, so
    /// an answer that happens to equal memory is not in `diff(current,
    /// candidate)` at all — and if the document has drifted at that field (the
    /// user commented `tier` out mid-session), the delta names nothing, the
    /// document keeps the drift, and the daemon still reports `applied: true`
    /// with a `WebSetupCompleted` naming the tier it did not write. Pinning
    /// these four to the document makes "the user answered it" and "the document
    /// says it" the same statement, which is what the confirm step promises.
    ///
    /// Only these four. `permission_allow`, `allowed_domains` and
    /// `cache_ttl_secs` ride along from memory and keep the in-memory base, so
    /// drift there is still absent from the delta and still survives (BR-5) —
    /// a setup answer is not an answer about consent (LESSON-495).
    DocumentPinsWebAnswers,
}

/// One derivation's output: the bytes a write would leave, and the two facts
/// about *how* they were derived that `/web setup` has to ask.
pub(super) struct EditedDocument {
    /// The document a write would produce — validated, not yet written.
    pub(super) text: String,

    /// Whether `text` is byte-identical to the document that was read.
    ///
    /// The only truthful "this write would change nothing" test under drift: a
    /// candidate equal to the in-memory config can still edit the file (the
    /// document drifted at a key the answers pin), and a candidate that differs
    /// from it can leave the file alone (the document drifted *to* the answer).
    ///
    /// **Byte-identical, and CRLF is a byte.** The engine re-renders a document
    /// authored with `\r\n` using `\n` on the first write it makes to it
    /// (`config_doc`'s recorded limitation). So the first commit over such a
    /// file reports `unchanged: false` — and `applied: true` — for an answer
    /// that changes nothing *semantically*: the diff is the invisible line
    /// endings. That is honest rather than a bug (the file really did change,
    /// and the user's next `git diff` will say so), it happens once per
    /// document, and the second commit over the same answers is a true no-op.
    pub(super) unchanged: bool,

    /// The `[web]` table of the delta's base — memory's, or, under
    /// [`DeltaBase::DocumentPinsWebAnswers`], memory's with the four pinned
    /// fields taken from the document.
    ///
    /// What the preview's warnings describe the change against, so "this
    /// replaces the current `[web]` table" is a sentence about the document the
    /// user is keeping rather than about a memory it has drifted from.
    pub(super) base_web: WebConfig,
}

/// `current` with the four fields `/web setup` pins replaced by the document's
/// own parse of them, or `None` when the document does not parse.
///
/// `None` is not a failure and is not reported: a document that will not parse
/// is refused by [`apply_config_delta`] a moment later with the parse named.
/// Falling back to the in-memory base keeps that the single place that speaks.
///
/// # Parse, not load — the gate is deliberately not doubled here
///
/// This asks the document *what it says*, and `Config::from_toml` answers that.
/// It used to ask `Config::load`, which also **validates** — and validity is a
/// property of the whole config, so an unrelated invalid key switched the
/// pinning off. The case that costs: a user comments out `search_endpoint`
/// under `tier = "search"` mid-session. The document is now invalid at `[web]`,
/// so pinning would fall back to memory; memory still holds the endpoint, the
/// answers repeat it, the delta is empty, and the edited-bytes gate then refuses
/// the write over the very drift re-running `/web setup` with the same answers
/// exists to heal. Parsing instead makes the document's missing endpoint part of
/// the base, so the answer is written back and the file heals.
///
/// Nothing is loosened by this: the bytes that would land still go through
/// `Config::load` at the end of the derivation (BR-4), so an edit that does not
/// heal the document is still refused, with the validator's own sentence.
pub(super) fn pinned_delta_base(current: &Config, document: &str) -> Option<Config> {
    let on_disk = Config::from_toml(document).ok()?;
    let mut base = current.clone();
    base.web.tier = on_disk.web.tier;
    base.web.search_endpoint = on_disk.web.search_endpoint;
    base.web.search_key_ref = on_disk.web.search_key_ref;
    base.web.search_auth = on_disk.web.search_auth;
    Some(base)
}

/// The document a write of `candidate` would leave at `path` — derived,
/// validated, and not yet written (REQ-574 BR-1/BR-4).
///
/// The derivation half of the write seam, and the **only** one:
/// [`persist_config`] hands what this returns straight to
/// [`write_config_atomically`], and `/web setup`'s preview and commit reach it
/// through [`render_persisted_document`]. So the bytes a user confirms and the
/// bytes that land are one computation rather than two computations that agree
/// — LESSON-451's rule (a seam fakes the boundary, never the commit path)
/// applied to serialization, and the reason ADR-3 asks for a single derivation.
///
/// Every daemon-side write used to hand [`write_config_atomically`] a whole
/// `Config` and get a fresh `Config::to_toml()` serialization on disk, so a
/// consent answer about `[web]` normalized key order, dropped every comment, and
/// silently discarded unknown keys the schema ignores at load. The README
/// teaches a hand-written, heavily commented `[web]` block *and* `/web setup` in
/// the same section, which made that collateral user-facing. Here the on-disk
/// text is read, the operation's semantic delta is applied to it
/// ([`apply_config_delta`]), and everything the delta does not name survives
/// byte-for-byte.
///
/// # The delta base is the caller's `current`, never the parse of the file
///
/// ADR-1, and the line the whole preservation property rests on: the delta is
/// `diff(current, candidate)`, and both come from the caller — which holds them
/// already, because every candidate here is built by clone-and-mutate. Diffing
/// the *document* against the candidate instead would classify a hand edit the
/// daemon has not seen (it stays blind to drift until restart, BR-5) as
/// "changed" and write the stale in-memory value back over it: the exact clobber
/// this REQ exists to remove. With `current`, drift at a key the operation does
/// not touch is never in the delta and survives by construction.
///
/// The one bounded exception is `base_rule`: `/web setup` pins four fields by
/// asking the user about them, and for *those four* the document's own parse is
/// the base ([`DeltaBase::DocumentPinsWebAnswers`]). Every other key, and every
/// other writer, keeps the rule above.
///
/// # The bytes that land are the bytes that were validated
///
/// The edited text is put through [`Config::load`] — parse *and*
/// `Config::validate`, the same gate startup runs — before anything is written
/// (BR-4). Callers validate their candidate in memory too, and this is not that
/// check twice: the document carries drift the candidate never saw, so "the
/// candidate validates" and "the file the daemon would boot on validates" are
/// different questions. A parseable hand edit that fails validation therefore
/// makes this refuse rather than overwrite it — a deliberate behavior change,
/// and the fail-safe one: the daemon neither destroys the user's edit nor writes
/// a document it would refuse to start on. `/web setup`'s preview inherits that
/// refusal, which is the honest answer there too: it has nothing truthful to
/// show for a document no commit could write.
///
/// # Refusal, never a silent rewrite
///
/// An unparseable document (a half-finished hand edit) refuses with the parse
/// failure named; falling back to a full re-serialization would make the write
/// succeed by destroying the edit in progress, which BR-6 forbids outright.
///
/// # A missing file, and a daemon with no file at all, are the same base
///
/// Neither is that case. A `path` of `None` — a daemon started with no config
/// file — still previews, because the question is "what would a write produce",
/// and the answer for both is a fresh document. In both, the edit base is the
/// empty document **and the delta base is `Config::default()`**, the parse of an
/// empty document, rather than the caller's `current`: that is what makes every
/// non-default key of the candidate get written and the fresh file's parse equal
/// the candidate (ADR-1, AC-6). Diffing against `current` there would produce a
/// document naming only what changed, which is not a config anyone could boot on.
pub(super) fn render_config_document(
    path: Option<&Path>,
    current: &Config,
    candidate: &Config,
    base_rule: DeltaBase,
) -> Result<EditedDocument, RenderError> {
    // Named so the missing-file base can be borrowed alongside `current`; a
    // default `Config` is the parse of an empty document, which is exactly what
    // the delta must diff against when there is no file yet (ADR-1).
    let fresh = Config::default();
    let (document, present) = match path.map(std::fs::read_to_string) {
        // A file holding only blank lines is a missing file that happens to
        // exist — the shape a truncated write leaves behind. The engine already
        // makes it the empty *edit* base; the matching *delta* base is this
        // caller's to choose, and choosing `fresh` is what makes the next write
        // heal it into a whole document instead of a diff against a state no
        // file ever held (`document_is_effectively_empty`'s own contract).
        Some(Ok(text)) => {
            let present = !document_is_effectively_empty(&text);
            (text, present)
        }
        None => (String::new(), false),
        Some(Err(err)) if err.kind() == std::io::ErrorKind::NotFound => (String::new(), false),
        // Any other read failure is a real one — an unreadable file is not an
        // empty one, and treating it as one would write a fresh document over
        // whatever is actually there.
        Some(Err(err)) => return Err(RenderError::Read(err)),
    };
    // A document that is not there pins nothing: the base stays `fresh`, which
    // is what makes every non-default key of the candidate get written (AC-6).
    let pinned = match base_rule {
        DeltaBase::DocumentPinsWebAnswers if present => pinned_delta_base(current, &document),
        DeltaBase::DocumentPinsWebAnswers | DeltaBase::InMemory => None,
    };
    let base = match (&pinned, present) {
        (Some(pinned), _) => pinned,
        (None, true) => current,
        (None, false) => &fresh,
    };
    let text = apply_config_delta(&document, base, candidate)?;
    Config::load(&text).map_err(|err| RenderError::Invalid(load_failure_reason(&err, &text)))?;
    Ok(EditedDocument {
        unchanged: text == document,
        base_web: base.web.clone(),
        text,
    })
}

/// The document `/web setup` describes and then writes — the three shapes the
/// flow renders it in, and the two facts about the derivation the commit and the
/// preview decide with (REQ-574 BR-3, ADR-3).
pub(super) struct RenderedCandidate {
    /// The whole edited document — byte-for-byte what the commit writes.
    pub(super) full_text: String,
    /// The `[web]` table as it appears *inside* `full_text`, decor and the
    /// user's own comments included. What the preview shows.
    pub(super) web_section: String,
    /// `sha256(full_text)` — see [`render_persisted_document`] for why the whole
    /// document and not the section.
    pub(super) digest: String,
    /// Whether `full_text` is byte-identical to the document on disk — the
    /// file half of the commit's no-op rule. See [`EditedDocument::unchanged`].
    pub(super) unchanged: bool,
    /// The `[web]` table the delta was diffed against, which under this flow's
    /// base rule is the document's own four pinned fields over memory's rest.
    /// The preview's warnings compare the candidate to *this*.
    pub(super) base_web: WebConfig,
}

/// Derive the document a `/web setup` commit would write, and the two views its
/// preview describes it with (BR-3, AC-3/AC-4).
///
/// One derivation for both seams. The preview slices and digests what this
/// returns; the commit calls it, compares the digest the user confirmed against
/// what it just derived, and writes that same `full_text`. There is no second
/// renderer to drift out of step with the writer (LESSON-451) and no second read
/// either — deriving here and again inside [`persist_config`] would digest one
/// document and write another whenever the file moved between the two reads,
/// which is a TOCTOU the daemon would have opened on itself.
///
/// # Why the digest covers the whole document
///
/// REQ-572's guard digested the candidate's canonical serialization, which
/// caught the race it was built for: the answers pin four fields and the rest of
/// the `[web]` table rides along from the live config, so another session's
/// "enable permanently" ([`DaemonRuntime::persist_web_tier`]) moves
/// `permission_allow` underneath a preview the user is still reading. Digesting
/// the **edited document** keeps that and delivers the rest of the promise: a
/// hand edit anywhere in the file — a comment on its own line, a key this flow
/// never touches — changes the bytes that would land, so it changes the digest
/// and the commit refuses with [`SETUP_DIGEST_STALE`] (BR-3, AC-4).
///
/// Deterministic because the derivation is: the same document and the same
/// candidate produce the same text on both calls, which is what makes an
/// inequality evidence of a *change* rather than of two renderings.
///
/// # The base is the document's own answer at the four pinned fields
///
/// This is the one caller that asks for [`DeltaBase::DocumentPinsWebAnswers`],
/// and the reason is that its candidate is memory-plus-answers: an answer that
/// happens to repeat what memory holds is not a *difference*, so a pure
/// `diff(current, candidate)` never mentions it — and a document that has
/// drifted at that field then keeps the drift while the commit reports the
/// answer as applied. Pinning those four to the document closes it; see that
/// variant for the full account.
///
/// # Errors
/// [`error_code::WEB_SETUP_INVALID`] when the edited document would not load,
/// carrying the validator's own sentence (BR-4, AC-10).
/// [`error_code::INTERNAL_ERROR`] when the document could not be derived at all
/// — an unparseable file (BR-6) or an unreadable one — or when it names no
/// `[web]` table.
///
/// That last arm is defensive rather than live. The flow refuses a `tier` of
/// `off`, so the candidate's table is never `WebConfig::is_unset`; and with this
/// caller's base rule the four pinned fields are diffed against the *document*,
/// so a document that names no `[web]` table (a user who deleted it mid-session)
/// disagrees with the candidate at `tier` and gets the section written back
/// rather than leaving the delta empty. Reaching the arm would mean the delta
/// engine dropped a key it was told to set, which is why it stays an internal
/// error and not a `WEB_SETUP_INVALID`: nothing about the user's file would be
/// the thing to fix.
pub(super) fn render_persisted_document(
    path: Option<&Path>,
    current: &Config,
    candidate: &Config,
) -> Result<RenderedCandidate, RpcError> {
    let edited =
        render_config_document(path, current, candidate, DeltaBase::DocumentPinsWebAnswers)
            .map_err(|err| match &err {
                RenderError::Invalid(_) => {
                    RpcError::new(error_code::WEB_SETUP_INVALID, err.to_string())
                }
                RenderError::Read(_) | RenderError::Edit(_) => RpcError::new(
                    error_code::INTERNAL_ERROR,
                    format!("the configuration could not be saved ({err})"),
                ),
            })?;
    let web_section = table_section(&edited.text, "web").ok_or_else(|| {
        RpcError::new(
            error_code::INTERNAL_ERROR,
            "the document this would write names no `[web]` table, so there is nothing to show",
        )
    })?;
    let digest = teton_inference::sha256_hex(edited.text.as_bytes());
    Ok(RenderedCandidate {
        full_text: edited.text,
        web_section,
        digest,
        unchanged: edited.unchanged,
        base_web: edited.base_web,
    })
}

/// The document a `/provider setup` commit would write, and everything the
/// preview and the commit each read off that one derivation (REQ-579 BR-9,
/// ADR-1).
///
/// [`RenderedCandidate`]'s sibling for the provider trio, and it carries a
/// superset of the preview's answer on purpose: every field here exists because
/// **one of the two seams** needs it, and having one derivation produce all of
/// them is what makes "the bytes the user confirmed are the bytes that land" a
/// property of the code path rather than of two functions agreeing (LESSON-451).
///
/// [`DaemonRuntime::provider_setup_preview`] projects `toml`, `digest`,
/// `dial_host`, `warnings` and `replaces` onto the wire.
/// [`DaemonRuntime::provider_setup_commit`] checks `digest`, decides its no-op
/// on `unchanged` **and** on `candidate_config`, hands `full_text` to the writer
/// unchanged, swaps `candidate_config` into memory, and reports `bindings`.
///
/// Every field is therefore read by one seam or the other — TASK-153 shipped
/// four of them ahead of the commit that consumes them, under an
/// `allow(dead_code)` this task deleted. They were *stated there* rather than
/// added here because the whole point of the shared derivation is that the
/// commit performs no derivation of its own: a commit that had to extend this
/// struct to get its bytes would be one derivation away from deriving them
/// itself, which is the drift ADR-1 and LESSON-451 exist to prevent.
pub(crate) struct RenderedProviderSetup {
    /// The `[[providers]]` row and the `[[tiers]]` rows this candidate writes,
    /// sliced out of `full_text` — what the preview shows, and therefore what
    /// the user confirms.
    ///
    /// Sliced rather than re-rendered ([`teton_core::array_element_section`]),
    /// so a comment the user wrote above their own `[[providers]]` row appears
    /// in the preview of the edit to it.
    pub(super) toml: String,
    /// The whole edited document — byte-for-byte what the commit writes.
    pub(super) full_text: String,
    /// `sha256(full_text)`.
    ///
    /// The **whole document** and not the section, for
    /// [`render_persisted_document`]'s reason: the whole config is what the
    /// commit writes, every field this flow does not collect rides along from
    /// the document and the live config, and either can move under a preview the
    /// user is still reading. A hand edit anywhere in the file therefore changes
    /// this, and the commit refuses to write bytes that no longer digest to what
    /// was confirmed.
    pub(super) digest: String,
    /// Whether `full_text` is byte-identical to the document on disk — the file
    /// half of the commit's no-op rule. See [`EditedDocument::unchanged`].
    pub(super) unchanged: bool,
    /// The host the endpoint parsed to under the **dial-time** parser
    /// (LESSON-529). The host alone: never userinfo, path, or query, because a
    /// pasted URL can carry a credential in its authority.
    pub(super) dial_host: String,
    /// Non-fatal notes about the candidate, as plain sentences (LESSON-517).
    pub(super) warnings: Vec<String>,
    /// The provider this candidate replaces, when its id is already taken
    /// (BR-14), or `None` for a fresh registration.
    pub(super) replaces: Option<ExistingProvider>,
    /// The tier rows that actually landed in `candidate_config` — what the
    /// commit reports, and what the preview's `toml` shows.
    pub(super) bindings: Vec<WireTierBinding>,
    /// The candidate config itself: what the commit compares against the live
    /// one for its no-op decision, and what it swaps into memory so routing is
    /// live in-session without a restart (BR-15).
    pub(super) candidate_config: Config,
}

/// One configured provider, as the flow describes it before offering to replace
/// it (REQ-579 BR-14).
///
/// Three non-secret fields: no `auth_ref`, no endpoint. The model is read
/// through [`ModelProvider::declared_model`] — the one place that decides what
/// "declared" means, so a provider carrying `model = " "` is described as having
/// none here exactly as the usability pass and the router already describe it
/// (BUG-155).
pub(super) fn existing_provider(provider: &ModelProvider) -> ExistingProvider {
    ExistingProvider {
        id: ProviderId::from(provider.id.as_str()),
        kind: to_proto_kind(provider.kind),
        model: provider.declared_model().map(str::to_owned),
    }
}

/// The tier rows a candidate config actually holds for the tiers a candidate
/// asked about — what landed, in the order it was asked for.
///
/// Read back off the built config rather than echoed from the request, for
/// [`SessionPermissionsResult::level`]'s reason: two bindings naming one tier
/// are **one** row after [`apply_update`]'s replace-or-insert, and a surface
/// that reported the request would describe two rows the file does not have.
/// The lookup is by `tier`, which is the key that mutation matched on
/// (LESSON-522).
pub(super) fn landed_bindings(config: &Config, asked: &[WireTierBinding]) -> Vec<WireTierBinding> {
    let mut tiers: Vec<Tier> = Vec::with_capacity(asked.len());
    for binding in asked {
        let tier = to_core_tier(binding.tier);
        if !tiers.contains(&tier) {
            tiers.push(tier);
        }
    }
    tiers
        .into_iter()
        .filter_map(|tier| {
            config
                .tiers
                .iter()
                .find(|row| row.tier == tier)
                .map(|row| WireTierBinding {
                    tier: to_protocol_tier(row.tier),
                    provider_id: ProviderId::from(row.provider_id.as_str()),
                })
        })
        .collect()
}

/// The rows a `/provider setup` commit would write, sliced out of the document
/// it would write them into (REQ-579 BR-9).
///
/// The provider row followed by one row per landed binding, separated by a blank
/// line — the shape they have in the file, because they are cut from the file.
/// A second renderer is exactly what ADR-1 refuses: the preview is defined as
/// the writer's own slice.
///
/// # The one live way a row can be missing, and why refusing is right
///
/// This flow uses [`DeltaBase::InMemory`] — the rule every writer but `/web
/// setup` keeps — so a row whose candidate value **equals what memory holds** is
/// not in `diff(current, candidate)` at all. That is normally invisible, because
/// the row is then already in the document. It is visible under one condition:
/// the user hand-deleted that row while this daemon was running (it stays blind
/// to the file until restart, REQ-574 BR-5) and then answered with exactly what
/// memory still holds. The delta names nothing, the document still lacks the
/// row, and there is nothing truthful to show.
///
/// `/web setup` solved the same class for its four scalar fields by pinning them
/// to the document ([`DeltaBase::DocumentPinsWebAnswers`]); the array-of-tables
/// equivalent is a base whose *lengths* differ from the candidate's, which the
/// delta engine answers with a wholesale array replacement — trading a rare
/// refusal for a routine loss of the user's comments. So this refuses instead,
/// loudly and with the remedy named (BR-6: degradation is a refusal, never a
/// silent rewrite), and the pinning question is left open rather than answered
/// badly.
///
/// # Errors
/// [`error_code::PROVIDER_SETUP_INVALID`] when the derived document names no row
/// this candidate is about — the drift above.
pub(super) fn provider_setup_section(
    document: &str,
    id: &str,
    bindings: &[WireTierBinding],
) -> Result<String, RpcError> {
    let mut sections = vec![teton_core::array_element_section(document, "providers", id)
        .ok_or_else(|| config_drifted(format!("`{id}` provider row")))?];
    for binding in bindings {
        let tier = to_core_tier(binding.tier);
        sections.push(
            teton_core::array_element_section(document, "tiers", tier.as_str())
                .ok_or_else(|| config_drifted(format!("`{}` tier row", tier.as_str())))?,
        );
    }
    Ok(sections.join("\n"))
}

/// The one sentence both halves of the [`DeltaBase::InMemory`] drift are refused
/// with (REQ-579 BR-6, REQ-574 BR-5).
///
/// Spelled once and shared rather than written twice: the *cause* a user has to
/// act on is identical whether the document lost the row or changed it, and two
/// wordings of one remedy is the drift LESSON-456 is about — at the level of the
/// message rather than the code.
pub(super) fn config_drifted(what: String) -> RpcError {
    RpcError::new(
        error_code::PROVIDER_SETUP_INVALID,
        format!(
            "this daemon's configuration and the file on disk disagree about the {what}, so \
             there is nothing truthful to preview. Nothing was written. The file has been \
             hand-edited since this daemon read it — restart it, or change one of your \
             answers so the row is rewritten."
        ),
    )
}

/// The **second** shape of the drift [`provider_setup_section`] records, refused
/// for the same reason and with the same sentence (REQ-579 BR-6).
///
/// That function catches the row the document has *lost*. This catches the row
/// the document still has and has **changed**: the user hand-edits `model` on
/// their `kimi` row while this daemon is running (it stays blind to the file
/// until restart, REQ-574 BR-5) and then answers with exactly what memory still
/// holds. `diff(current, candidate)` is empty, the document is not edited, and
/// the two seams would otherwise report:
///
/// - a **preview** slicing the document's own row — so the user reads
///   `model = "kimi-k5"` under a walkthrough in which they answered `kimi-k2`;
/// - a **commit** answering `applied: false`, "the config already says exactly
///   this", which is true of memory and false of the file the daemon is about to
///   leave in place.
///
/// Both are worse than a refusal, because both are confident. Asked only when
/// [`EditedDocument::unchanged`] holds: once the delta writes anything, the rows
/// this flow is about are the derivation's own.
///
/// The comparison is between **parsed configs**, never between the rendered text
/// and the document's bytes: `model='kimi-k2'` and `model = "kimi-k2"` are one
/// value written two ways, and an `anthropic` row with no `endpoint` key at all
/// is compared against the endpoint [`compose_endpoint`] supplies for that kind
/// — the same default the derivation itself ran through. A byte comparison would
/// refuse all three for having been hand-authored.
///
/// **Every field the answer reports is compared**, not only the two the
/// walkthrough asks about last. `auth_ref` and `kind` are on that list because a
/// document that has drifted at either is a document this daemon would describe
/// wrongly in the direction that matters: a `kimi` row whose credential the user
/// hand-edited to `env:SOME_TOKEN` would be reported as registered under
/// `keychain://teton/kimi` and then dialed with the *other* secret, and a row
/// whose `kind` was hand-edited speaks a different request path than the
/// preview's `[[providers]]` block says. Both are the confident-and-wrong answer
/// the whole function exists to refuse.
///
/// # Errors
/// [`error_code::PROVIDER_SETUP_INVALID`] carrying [`config_drifted`]'s sentence
/// when the document names a different provider row or a different tier row than
/// the candidate does. [`error_code::INTERNAL_ERROR`] if the derived document
/// does not load, which [`render_config_document`] has already established it
/// does.
pub(super) fn document_agrees_with_candidate(
    document: &str,
    id: &str,
    kind: ProtoProviderKind,
    model: &str,
    endpoint: &str,
    key_ref: &str,
    bindings: &[WireTierBinding],
) -> Result<(), RpcError> {
    let on_disk = Config::load(document).map_err(|err| {
        RpcError::new(
            error_code::INTERNAL_ERROR,
            format!("the configuration this daemon derived would not load ({err})"),
        )
    })?;
    let row = on_disk
        .providers
        .iter()
        .find(|provider| provider.id == id)
        .ok_or_else(|| config_drifted(format!("`{id}` provider row")))?;
    // `declared_model` rather than the raw field, so a row carrying `model = " "`
    // is "has none" here exactly as it is everywhere else (BUG-155).
    //
    // `auth_ref` and `kind` are compared for the reason in the doc above: they
    // are fields the answer *reports*, and a document that drifted at either
    // would be described by a preview that read memory and a commit that said
    // "already registered". The credential comparison is against the reference
    // this seam already pinned to `keychain://teton/<id>`, so a `None` on disk
    // and any other reference are both refusals.
    if row.declared_model() != Some(model)
        || row.endpoint.as_deref() != Some(endpoint)
        || row.auth_ref.as_deref() != Some(key_ref)
        || to_proto_kind(row.kind) != kind
    {
        return Err(config_drifted(format!("`{id}` provider row")));
    }
    for binding in bindings {
        let tier = to_core_tier(binding.tier);
        let agrees = on_disk
            .tiers
            .iter()
            .find(|row| row.tier == tier)
            .is_some_and(|row| row.provider_id == binding.provider_id.0);
        if !agrees {
            return Err(config_drifted(format!("`{}` tier row", tier.as_str())));
        }
    }
    Ok(())
}

/// The keychain service prefix every Teton credential reference carries.
///
/// The CLI composes this string from its own `keychain::SERVICE` and
/// `AUTH_REF_SCHEME` (`crates/teton/src/keychain.rs`, `auth_ref_for`), and this
/// daemon's [`crate::keychain`] resolves it back with the same service name. It
/// is spelled once here because that constructor lives in the **binary** crate:
/// a library may not depend on it, and inventing a second public home for one
/// format string is a worse trade than one named constant with this comment.
///
/// What catches a rename is therefore on the **CLI side**, not this one: this
/// module's own tests would pass a daemon that had drifted away from the client,
/// because they compose both halves from this constant. The pins are
/// `keychain::auth_ref_matches_the_protocol_shape` (which asserts
/// `auth_ref_for("anthropic") == "keychain://teton/anthropic"` against the
/// literal) and `provider_setup_ui`'s `the_key_reaches_the_keychain_and_nothing_else`
/// (which asserts the reference the walk puts on the wire is
/// `keychain://teton/kimi`), plus `crates/tetond/tests/provider_setup_flow.rs`,
/// where a real daemon is handed the literal a real client would send.
const KEYCHAIN_AUTH_REF_PREFIX: &str = "keychain://teton/";

/// The **one** credential reference `/provider setup` accepts for `id` — the
/// keychain row the CLI half of this flow writes the key into (REQ-579 BR-2,
/// ADR-5).
///
/// # Why this seam is narrower than [`Config::validate`]
///
/// [`teton_core::is_recognized_auth_ref`] admits `env:VAR`, `op://vault/item`,
/// `keychain:<account>` and any `keychain://<service>/<account>`, and it is
/// right to: it validates **hand-written configs**, where a user who keeps their
/// key in an environment variable or a 1Password vault is doing something
/// legitimate that Teton supports.
///
/// This is not that seam. Here the reference is not something a user wrote in a
/// file this daemon merely loads — it is a field on an RPC whose *whole flow*
/// consists of a client reading a key echo-off and writing it to
/// `keychain://teton/<the id being registered>`. Any other value means the
/// candidate names a secret this flow did not collect, and one commit composes
/// an attacker-chosen endpoint, a tier binding, and a credential reference the
/// caller did not have to possess: `env:<a secret the daemon's environment
/// holds>` resolves at dial time and is sent to the endpoint in the same
/// candidate. Accepting only the row this flow itself writes makes that
/// composition unexpressible **on this seam, for a reference the caller does not
/// already own** (BR-2, REQ-579 System Model: "a keychain reference only … the
/// same account `teton provider add` uses").
///
/// # What this does *not* close
///
/// The claim is deliberately narrow, because two neighbouring compositions
/// survive it and a comment that said "unexpressible" would be read as covering
/// them:
///
/// 1. **Redirecting a key the caller already has.** A commit naming an
///    *already-registered* id with a new endpoint passes this rule — the
///    reference is that id's own — and repoints the existing
///    `keychain://teton/<id>` credential at the new host. That is a real
///    capability of this flow, not a hole in it: it is how a user moves a
///    provider to a new address. What makes it safe to allow is that it is
///    **stated**: the preview carries `replaces` (the id, kind and prior model)
///    and `dial_host` (the destination), the client renders both before the
///    confirm, and the completion event repeats the host to every client on the
///    session (BR-14, BR-15, AC-12).
/// 2. **`config/set`.** [`ConfigUpdate::RegisterProvider`] over that method
///    still admits every reference [`teton_core::is_recognized_auth_ref`] does,
///    `env:` and `op://` included. That is pre-existing and out of this REQ's
///    scope — it is REQ-576/BR-10(b) territory, where the presence gate on
///    config writes lives.
///
/// A user who *wants* `env:` for a provider still has `teton provider add` and
/// their own config file. What they cannot do is reach them through a
/// walkthrough that never asked.
pub(super) fn keychain_auth_ref_for(id: &str) -> String {
    format!("{KEYCHAIN_AUTH_REF_PREFIX}{id}")
}

/// Whether `url`'s authority carries userinfo — a credential pasted into the
/// endpoint (REQ-579 BR-9, LESSON-528/529).
///
/// Asked of the **dial-time** parser, which is the whole point: `reqwest::Url`
/// is what the request builder and the egress origin check read this string
/// with, so "there is userinfo here" is a fact about the request that will be
/// made and not about a second reading of the same characters. A hand-written
/// authority splitter is exactly the mirrored predicate LESSON-528 is about.
///
/// A URL that does not parse is not warned about: it has already been refused by
/// [`is_absolute_http_url`] before any warning is composed, and a `false` here
/// cannot make a refused candidate land.
pub(super) fn endpoint_carries_userinfo(url: &str) -> bool {
    reqwest::Url::parse(url.trim())
        .is_ok_and(|parsed| !parsed.username().is_empty() || parsed.password().is_some())
}

/// Replace `path` with `text` **atomically** — a sibling temp file, flushed to
/// disk, then renamed over the target.
///
/// The mechanism half of the write seam: [`persist_config`] decides what the
/// bytes are, this decides how they land. Nothing else in the daemon writes the
/// config file (BR-2 / REQ-572 BR-11).
///
/// BUG-155. The previous `std::fs::write` truncated the user's config in place.
/// That is not merely untidy, it is fail-OPEN: every `Config` field is
/// `#[serde(default)]`, so a zero-length or truncated file still *loads*, and
/// because `providers` serializes before `boundaries`, a partial write is very
/// likely to be valid TOML carrying the user's remote providers and none of
/// their `local-only` privacy boundaries. The daemon would then start, report
/// nothing, and route turns remotely with boundary enforcement silently gone —
/// which is precisely the outcome `load_config`'s refusal-to-start exists to
/// prevent, reached through a different door.
///
/// REQ-557 is what made this urgent: the migration turned this from a write
/// behind an explicit user action into an unattended write on the first start
/// after upgrade, for every existing install. Same shape as
/// [`crate::selection_store`]'s `write_atomically`.
///
/// # The replacement carries the original's permissions
///
/// `File::create` yields a umask-derived mode, normally `0644`. Rewriting a
/// user's config through a temp file would therefore *widen* it: a config
/// deliberately set to `0600` comes back world-readable, silently, on the first
/// upgraded start — and this file can hold real secrets, because
/// `McpTransport::Stdio { env }` stores arbitrary environment values with no
/// validation, which is exactly where an API key ends up.
///
/// So the original's mode is read and applied to the temp file **before** the
/// rename, which is the only ordering that leaves no window: set it after and
/// the file is briefly readable under its real name. With no original to read
/// (a first write), the fallback is `0600` rather than the umask default —
/// the same choice [`crate::auth`] makes for the socket and its directory, and
/// for the same reason: a file that may hold a credential does not get its
/// permissions from an inherited umask.
///
/// # Errors
/// Returns the underlying I/O error. The caller decides whether that is fatal;
/// the on-disk file is left untouched either way.
pub(super) fn write_config_atomically(path: &Path, text: &str) -> anyhow::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Read before writing: once the temp file exists there is nothing left to
    // learn from, and after the rename it is too late.
    let mode = std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o7777)
        .unwrap_or(0o600);
    let temp = path.with_extension("toml.tmp");
    {
        let mut file = std::fs::File::create(&temp)?;
        // Before any content is written, so the bytes are never on disk under
        // a wider mode than the file they are replacing.
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        file.write_all(text.as_bytes())?;
        // Durability before visibility: without the sync, the rename can land
        // while the contents are still only in the page cache, so a power loss
        // yields an empty file under the real name — the exact fail-open state
        // this function exists to prevent.
        file.sync_all()?;
    }
    std::fs::rename(&temp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })?;
    Ok(())
}

/// **The write seam edits the document it finds** (REQ-574 TASK-136).
///
/// The properties of [`persist_config`] itself, asserted at the seam and at
/// one RPC writer above it. The per-writer preservation suite — all five
/// writers, the README's `[web]` block verbatim — is TASK-138's; what is
/// here is the seam's own contract: refuse loudly, write fresh when there is
/// no file, and never touch what the delta does not name.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::testsupport::{scratch_dir, set_dir_readonly};

    /// A hand-written config with the shapes preservation is *about*:
    /// comments above and beside keys, a deliberate key order, and — added
    /// by the tests that need it — keys this schema does not know.
    const HAND_WRITTEN_CONFIG: &str = r#"# my teton config, written by hand
# (and I would like to keep these notes)

[web]
# search is on because I set up a backend below
tier = "search"
search_endpoint = "https://api.search.brave.com/res/v1/web/search"  # brave
search_key_ref = "keychain://teton/web-search"
search_auth = "X-Subscription-Token: {key}"
"#;

    /// A runtime whose config is the given document — memory and disk
    /// agreeing, which is what a real start produces.
    fn runtime_over(tag: &str, document: &str) -> (DaemonRuntime, std::path::PathBuf) {
        let dir = scratch_dir(tag);
        let path = dir.join("config.toml");
        std::fs::write(&path, document).expect("seed the config");
        let mut runtime = DaemonRuntime::minimal();
        if let Ok(config) = Config::load(document) {
            runtime.config = std::sync::Mutex::new(config);
        }
        runtime.config_path = Some(path.clone());
        runtime.data_dir = dir;
        (runtime, path)
    }

    /// **A document that cannot be parsed is not overwritten** (BR-6, AC-5).
    ///
    /// The failure mode this replaces is the quiet one: the old seam
    /// serialized the in-memory config over whatever was there, so a
    /// half-finished hand edit was *repaired* by being destroyed. Refusal is
    /// the fail-safe answer, and the refusal has to say what is wrong with
    /// the file — a bare "could not be saved" leaves the user with a daemon
    /// that will not write and no idea why (LESSON-456, BUG-146).
    ///
    /// Two levels, because the message has to survive the trip: the seam
    /// itself, and `persist_web_tier`'s wrapper around it.
    #[test]
    fn an_unparseable_document_refuses_the_write_and_names_the_parse_failure() {
        let broken = "[web]\ntier = \"search\nsearch_endpoint = \"https://x.example/api\"\n";
        let (runtime, path) = runtime_over("persist-unparseable", broken);

        let mut candidate = Config::default();
        candidate.web.permission_allow.push(WebTier::FetchUserUrl);
        let err = super::persist_config(&path, &Config::default(), &candidate)
            .expect_err("an unparseable document must not be written over");
        let seam_message = format!("{err}");
        assert!(
            seam_message.contains("could not be parsed for editing"),
            "the refusal must name the parse failure, not just the write: {seam_message}"
        );

        // And through the RPC writer, whose own sentence wraps it.
        let refused = runtime
            .persist_web_tier(WebTier::FetchUserUrl)
            .expect_err("the consent answer must not land on a broken document");
        assert!(
            refused.contains("could not be saved")
                && refused.contains("could not be parsed for editing"),
            "the writer's sentence must carry the inner reason: {refused}"
        );

        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            broken,
            "the user's half-finished edit was rewritten by the refusal"
        );
        assert!(
            runtime
                .config
                .lock()
                .expect("config mutex")
                .web
                .permission_allow
                .is_empty(),
            "the in-memory swap happens after the write, never before"
        );
    }

    /// **A missing file is not an error** (BR-6, AC-6): the edit base is the
    /// empty document, so the fresh file's parse *is* the candidate — and it
    /// is owner-only, because this file can hold secret-adjacent material
    /// and a created one must not inherit the umask.
    #[test]
    fn a_missing_file_is_written_fresh_at_owner_only_and_parses_back_to_the_candidate() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = scratch_dir("persist-missing");
        let path = dir.join("nested").join("config.toml");

        let mut candidate = Config::default();
        candidate.web.tier = WebTier::FetchAnyUrl;
        candidate.web.permission_allow.push(WebTier::FetchUserUrl);

        // The delta base is `Config::default()` — the parse of an empty
        // document — and NOT the caller's `current`, which is what makes the
        // written file complete rather than a diff against a state no file
        // ever held. Passing a `current` that differs from the default is
        // the falsification: with the wrong base, `tier` never gets written.
        let mut current = Config::default();
        current.web.tier = WebTier::FetchAnyUrl;
        super::persist_config(&path, &current, &candidate).expect("a fresh document is written");

        let written = load_config(Some(&path)).expect("the fresh document loads");
        assert_eq!(written.web.tier, WebTier::FetchAnyUrl);
        assert_eq!(written.web.permission_allow, vec![WebTier::FetchUserUrl]);
        assert_eq!(
            std::fs::metadata(&path).expect("stat").permissions().mode() & 0o7777,
            0o600,
            "a config this daemon created gets owner-only, not the umask default"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A write touches only its keys** (BR-1/BR-5), asserted at the seam
    /// through the writer a user actually reaches: comments, key order, an
    /// unknown key inside a known table and an unknown top-level table all
    /// survive a consent answer, and the only difference is the answer's own
    /// key.
    ///
    /// One witness here; the per-writer suite is TASK-138's.
    #[test]
    fn a_web_tier_write_leaves_every_key_it_is_not_about_alone() {
        let seed = format!(
            "{HAND_WRITTEN_CONFIG}max_page_bytes_from_the_future = 4096\n\n\
             # a table this schema has never heard of\n\
             [experiment]\nknob = true\n"
        );
        let (runtime, path) = runtime_over("persist-preserves", &seed);

        runtime
            .persist_web_tier(WebTier::FetchUserUrl)
            .expect("the consent answer lands");

        let after = std::fs::read_to_string(&path).expect("read");
        for surviving in [
            "# my teton config, written by hand",
            "# (and I would like to keep these notes)",
            "# search is on because I set up a backend below",
            "# brave",
            "# a table this schema has never heard of",
            "max_page_bytes_from_the_future = 4096",
            "[experiment]",
            "knob = true",
            "search_auth = \"X-Subscription-Token: {key}\"",
        ] {
            assert!(
                after.contains(surviving),
                "a consent answer destroyed `{surviving}`:\n{after}"
            );
        }
        assert!(
            after.find("tier =").expect("tier is still there")
                < after
                    .find("search_endpoint =")
                    .expect("endpoint is still there"),
            "the user's key order was normalized:\n{after}"
        );

        // The one difference, stated as a difference: the seed plus the
        // answer's key is the whole change.
        let added = after.replace("permission_allow = [\"fetch_user_url\"]\n", "");
        assert_eq!(
            added, seed,
            "the write changed something other than the key it is about"
        );

        // And the meaning is what the user asked for, read back through the
        // production loader.
        let reloaded = load_config(Some(&path)).expect("the edited document loads");
        assert_eq!(reloaded.web.permission_allow, vec![WebTier::FetchUserUrl]);
        assert_eq!(reloaded.web.tier, WebTier::Search);
    }

    /// **A hand edit that parses but does not validate is refused, not
    /// overwritten** (BR-4's stated consequence, AC-10).
    ///
    /// The validator runs on the *edited bytes*, so "the candidate
    /// validates" cannot stand in for "the file the daemon would boot on
    /// validates": the drift here is in a key the operation never touches,
    /// and the candidate is clean. Today's alternative would be to write the
    /// candidate over the user's invalid edit — silently erasing it — which
    /// is the worse half of a bad pair. Refusal keeps both the edit and the
    /// daemon's ability to start.
    #[test]
    fn a_hand_edit_that_fails_validation_refuses_the_write_and_survives_it() {
        // The daemon started on a config it would boot on...
        let (runtime, path) =
            runtime_over("persist-invalid-drift", "[web]\ntier = \"fetch_any_url\"\n");
        assert!(runtime
            .config
            .lock()
            .expect("config mutex")
            .validate()
            .is_ok());
        // ...and the user then hand-edited the file into one it would not,
        // in a key this operation never touches. The candidate is clean;
        // only the bytes that would land are not.
        let drifted = "default_provider = \"ghost\"\n\n[web]\ntier = \"fetch_any_url\"\n";
        std::fs::write(&path, drifted).expect("the hand edit lands");

        let refused = runtime
            .persist_web_tier(WebTier::FetchUserUrl)
            .expect_err("a document that would not load must not be written");
        assert!(
            refused.contains("would not load"),
            "the refusal must say the edited document is the problem: {refused}"
        );
        assert!(
            refused.contains("default_provider names provider 'ghost'"),
            "and must carry the validator's own sentence, which names the \
             key the user has to fix: {refused}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            drifted,
            "the invalid hand edit was overwritten by the refusal"
        );
        assert!(
            runtime
                .config
                .lock()
                .expect("config mutex")
                .web
                .permission_allow
                .is_empty(),
            "the in-memory swap happens after the write, never before"
        );
    }

    /// **A span that lands mid-character is located, not panicked on**
    /// (REQ-574 BR-6's mechanics).
    ///
    /// [`super::line_and_column`] used to slice `&text[..offset]`, which
    /// **panics** on a byte offset that is not a char boundary — and a
    /// config document holds arbitrary UTF-8 in its comments, so the offsets
    /// a parser hands back are byte offsets into exactly that. Worse than a
    /// crash on its own: this runs on the refusal path, under the held
    /// config mutex, so the panic poisons the mutex and every later
    /// `lock().expect(…)` in this daemon aborts the process. A mislocated
    /// column is a cosmetic defect in a bug report; a poisoned config mutex
    /// is a daemon that stops serving.
    ///
    /// So every offset into a multi-byte document must answer, including the
    /// ones inside a character and the ones past the end.
    #[test]
    fn a_span_that_lands_mid_character_is_located_rather_than_panicked_on() {
        const MULTI_BYTE: &str = "# ✅ の note\n[web]\ntier = \"off\"\n";
        for offset in 0..=MULTI_BYTE.len() + 8 {
            let (line, column) = super::line_and_column(MULTI_BYTE, offset);
            assert!(
                line >= 1 && column >= 1,
                "offset {offset} answered ({line}, {column})"
            );
        }

        // And it is still the right answer for the offsets the old slice
        // could handle.
        assert_eq!(super::line_and_column(MULTI_BYTE, 0), (1, 1));
        let web_at = MULTI_BYTE.find("[web]").expect("the fixture names [web]");
        assert_eq!(super::line_and_column(MULTI_BYTE, web_at), (2, 1));
        let quote_at = MULTI_BYTE.find('"').expect("the fixture quotes a value");
        assert_eq!(super::line_and_column(MULTI_BYTE, quote_at), (3, 8));
        // Columns count characters, not bytes: `note` is the 7th character
        // of that comment and the 11th byte of it, and a reader looking for
        // column 11 in a line 10 characters long finds nothing.
        let note_at = MULTI_BYTE.find("note").expect("the fixture has a note");
        assert_eq!(super::line_and_column(MULTI_BYTE, note_at), (1, 7));
    }

    /// **The pinned base is the document's four answered fields over
    /// memory's everything else** (ADR-1's one bounded exception).
    ///
    /// Asserted directly, because the flow-level witnesses can only see the
    /// consequence: the four fields are the ones `/web setup` asks about, and
    /// every other key — including the two `[web]` keys the flow carries
    /// along without asking — must keep coming from memory, or a setup answer
    /// would silently become an answer about consent (LESSON-495).
    #[test]
    fn the_pinned_base_takes_the_answered_fields_from_the_document_and_the_rest_from_memory() {
        let mut memory = Config::default();
        memory.web.tier = WebTier::Search;
        memory.web.search_endpoint = Some("https://memory.example/search".to_owned());
        memory.web.search_key_ref = Some("keychain://teton/memory".to_owned());
        memory.web.search_auth = Some("X-Memory: {key}".to_owned());
        memory.web.cache_ttl_secs = 900;
        memory.web.permission_allow = vec![WebTier::Search];
        memory.web.allowed_domains = Some(vec!["docs.rs".to_owned()]);
        memory.effort = teton_core::effort::EffortLevel::High;

        const DOCUMENT: &str = "\
[web]
tier = \"fetch_any_url\"
search_endpoint = \"https://document.example/search\"
search_key_ref = \"keychain://teton/document\"
search_auth = \"X-Document: {key}\"
cache_ttl_secs = 42
permission_allow = [\"fetch_user_url\"]
";
        let base = super::pinned_delta_base(&memory, DOCUMENT).expect("the document parses");

        assert_eq!(base.web.tier, WebTier::FetchAnyUrl);
        assert_eq!(
            base.web.search_endpoint.as_deref(),
            Some("https://document.example/search")
        );
        assert_eq!(
            base.web.search_key_ref.as_deref(),
            Some("keychain://teton/document")
        );
        assert_eq!(base.web.search_auth.as_deref(), Some("X-Document: {key}"));
        // Everything else is memory's, so drift there is still absent from
        // the delta and still survives the write (BR-5).
        assert_eq!(base.web.cache_ttl_secs, 900);
        assert_eq!(base.web.permission_allow, vec![WebTier::Search]);
        assert_eq!(base.web.allowed_domains, memory.web.allowed_domains);
        assert_eq!(base.effort, teton_core::effort::EffortLevel::High);

        // Parse, not load. A document that is *invalid* — `search` with the
        // endpoint commented out, the shape a mid-session hand edit leaves —
        // still pins, because pinning is how the answer that heals it gets
        // written. Validating here instead would fall back to memory, leave
        // the delta empty, and refuse the write at the edited-bytes gate:
        // `/web setup` declining to fix the very drift it was re-run for.
        let invalid = super::pinned_delta_base(&memory, "[web]\ntier = \"search\"\n")
            .expect("an invalid document still parses");
        assert!(
            invalid.web.search_endpoint.is_none(),
            "the document's missing endpoint is what the answer is written against"
        );
        assert!(
            Config::load("[web]\ntier = \"search\"\n").is_err(),
            "non-vacuity: that document really would not load"
        );

        // Unparseable is the one case with no answer here; the delta engine
        // refuses a moment later, naming the parse failure.
        assert!(super::pinned_delta_base(&memory, "[web]\ntier = ").is_none());
    }

    /// **A file that is there and cannot be read is not treated as an empty
    /// one** (BR-6, AC-5) — the `RenderError::Read` arm, which nothing
    /// exercised.
    ///
    /// The failure this guards is the same shape as the unparseable one: a
    /// read error swallowed into "there is no document" would derive a fresh
    /// document from the empty base and write it over a config the daemon
    /// could not even look at. So the refusal has to name the failure
    /// *class*, and it names only that: the `io::Error`'s own `Display`
    /// carries `(os error 13)` and the neighboring [`super::load_config`]
    /// deliberately formats `kind()` alone (REQ-572 BR-11) — two sentences
    /// about one file should not disagree about how much they say.
    #[test]
    fn an_unreadable_file_refuses_the_write_and_names_the_failure_class() {
        use std::os::unix::fs::PermissionsExt as _;

        let seed = "[web]\ntier = \"fetch_any_url\"\n";
        let (runtime, path) = runtime_over("persist-unreadable", seed);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))
            .expect("close the file");

        // Root reads everything, and so do some filesystems. Ask rather than
        // assume: a test that silently passes for the wrong reason is worse
        // than one that says it did not run.
        if std::fs::read_to_string(&path).is_ok() {
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            return;
        }

        let refused = runtime
            .persist_web_tier(WebTier::FetchUserUrl)
            .expect_err("a file that cannot be read must not be written over");
        assert!(
            refused.contains("could not be read for editing")
                && refused.contains("permission denied"),
            "the refusal must name the failure class: {refused}"
        );
        assert!(
            !refused.contains("os error"),
            "the message carries the error's kind and nothing finer (REQ-572 \
             BR-11): {refused}"
        );

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("reopen the file");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            seed,
            "a refused write reached the file"
        );
        assert!(
            runtime
                .config
                .lock()
                .expect("config mutex")
                .web
                .permission_allow
                .is_empty(),
            "the in-memory swap happens after the write, never before"
        );
    }

    /// **A write that cannot land leaves memory where it was** (AC-5),
    /// asserted at an RPC writer rather than at a migration.
    ///
    /// `a_migration_that_cannot_be_saved_leaves_the_config_byte_for_byte_
    /// intact` pins the same mechanism for a startup path, where the answer
    /// is warn-and-continue. Here the answer is a refusal the user reads,
    /// and the thing that must not move is the live config: reporting a
    /// consent answer as durable when the file rejected it is the silent
    /// downgrade REQ-563's `Persistent` scope depends on not happening.
    #[test]
    fn a_write_that_cannot_land_refuses_and_moves_nothing() {
        let seed = "[web]\ntier = \"fetch_any_url\"\n";
        let (runtime, path) = runtime_over("persist-readonly-dir", seed);
        let dir = path.parent().expect("the scratch directory").to_owned();

        set_dir_readonly(&dir, true);
        // Same question as above, asked of the directory: root creates files
        // in a `r-x` directory, and this test would then assert nothing.
        if std::fs::File::create(dir.join(".probe")).is_ok() {
            let _ = std::fs::remove_file(dir.join(".probe"));
            set_dir_readonly(&dir, false);
            return;
        }

        let refused = runtime
            .persist_web_tier(WebTier::FetchUserUrl)
            .expect_err("a write that cannot create its temp file must be reported");
        set_dir_readonly(&dir, false);

        assert!(
            refused.contains("could not be saved") && refused.contains("denied"),
            "the refusal must carry the inner reason, not just the failure \
             (LESSON-456): {refused}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            seed,
            "the atomic write left a partial document behind"
        );
        assert!(
            runtime
                .config
                .lock()
                .expect("config mutex")
                .web
                .permission_allow
                .is_empty(),
            "the answer was reported as durable while the file rejected it"
        );
    }

    /// **A startup migration whose document cannot be edited warns and keeps
    /// the session** (AC-5's second sentence, BR-6).
    ///
    /// The migration's failure tolerance is deliberate and asymmetric to the
    /// RPC writers': refusing to start because a hand edit is half-finished
    /// would strand the user, so the in-memory migration stands for this
    /// session, the file is left exactly as found, and the absence guard
    /// makes the next start try again.
    ///
    /// The warning's *text* is asserted through the value it interpolates
    /// rather than by capturing stderr: `eprintln!` writes to a
    /// process-global fd, and redirecting it from a test that runs beside
    /// others is a race, not a witness. `persist_config` over the same three
    /// inputs is the `{err}` that warning carries, so its sentence is the
    /// warning's sentence.
    #[test]
    fn a_migration_that_cannot_edit_the_document_warns_and_keeps_the_session() {
        let broken = "default_provider = \"cheap\n\n[[providers]]\nid = \"cheap\"\n";
        let dir = scratch_dir("migrate-unparseable");
        let path = dir.join("config.toml");
        std::fs::write(&path, broken).expect("seed a half-finished hand edit");

        // A config the REQ-557 pass has something to do to: one provider it
        // cannot resolve a model for (which is what makes the pass run at
        // all) and one it can default to.
        let provider = |id: &str, model: Option<&str>| ModelProvider {
            id: id.to_owned(),
            kind: ProviderKind::OpenaiCompatible,
            endpoint: Some("https://api.deepseek.com".to_owned()),
            model: model.map(str::to_owned),
            auth_ref: Some(format!("keychain:{id}")),
            allow_cleartext: false,
            capabilities: ProviderCapabilities::default(),
        };
        let mut config = Config {
            providers: vec![
                provider("cheap", Some("deepseek-chat")),
                provider("ghost", None),
            ],
            ..Config::default()
        };
        let before = config.clone();

        super::migrate_and_report_provider_models(&mut config, Some(&path), &PriceTable::bundled());

        assert_eq!(
            config.default_provider.as_deref(),
            Some("cheap"),
            "the in-memory migration must still stand: a failed write costs a \
             warning, never the session's routing"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            broken,
            "the migration rewrote a document it could not parse — the silent \
             repair-by-destruction BR-6 forbids"
        );

        // The sentence the warning carries, taken from the call it makes.
        let err = super::persist_config(&path, &before, &config)
            .expect_err("the same write, refused the same way");
        assert!(
            err.to_string().contains("could not be parsed for editing"),
            "the migration's warning must name the parse failure, or a user \
             whose file is the obstacle is never told which file: {err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A parse failure of the edited bytes is located, never quoted**
    /// (BR-7's rule applied to the one message that could reproduce a config
    /// line).
    ///
    /// This arm means the delta engine emitted TOML it cannot re-read — a
    /// bug in this daemon, not in the user's file, and unreachable through
    /// any input a test can hand the flow. So the classifier is asked
    /// directly, with the failure it would be handed: a *value*-shaped one,
    /// where `toml`'s own `message()` quotes the offending value even
    /// without its source line, and the value is the shape of a token.
    ///
    /// The other arm is asserted here too, because the two are a pair: the
    /// validator's sentence must survive **verbatim** (it names the key the
    /// user has to fix, BR-4/AC-10), and
    /// `a_hand_edit_that_fails_validation_refuses_the_write_and_survives_it`
    /// pins the same sentence arriving through the RPC writer.
    #[test]
    fn a_parse_failure_of_the_edited_bytes_is_located_and_never_quoted() {
        let planted = "ghp_FAKE_NOT_A_REAL_SECRET";
        let edited = format!("[web]\ntier = \"fetch_any_url\"\ncache_ttl_secs = \"{planted}\"\n");
        let err = Config::load(&edited).expect_err("a string is not a number of seconds");
        let reason = super::load_failure_reason(&err, &edited);

        assert!(
            reason.contains("line 3"),
            "a near-unreachable failure still has to be locatable: {reason}"
        );
        assert!(
            !reason.contains(planted),
            "the offending value reached an error string that travels to a \
             client and into a transcript: {reason}"
        );
        // Belt and braces: the whole assignment, which is what the parser's
        // own `Display` prints under a caret.
        assert!(
            !reason.contains("cache_ttl_secs ="),
            "the offending source line was reproduced: {reason}"
        );

        // And the validator's arm, unchanged and unwrapped.
        let invalid = "default_provider = \"ghost\"\n";
        let validate = Config::load(invalid).expect_err("an unregistered default is refused");
        assert!(
            super::load_failure_reason(&validate, invalid)
                .contains("default_provider names provider 'ghost'"),
            "the validator's own sentence must go through verbatim: it names \
             the key the user has to fix"
        );
    }
}
