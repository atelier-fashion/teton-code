//! Answering a model proposal, and the `teton model …` surface (REQ-547).
//!
//! [`crate::firstrun`] renders *what* the daemon proposed; this module collects
//! the answer and turns it into the one protocol message the daemon is waiting
//! for. Three rules shape it:
//!
//! - **BR-3 (never by accident).** Picking a catalog entry whose RAM floor
//!   exceeds the machine's RAM is allowed — the user's machine is the user's
//!   call — but only after an explicit warning and a *second* confirmation, and
//!   only then does `confirmed_above_ram_floor: true` ride the wire. The daemon
//!   enforces the same rule (it refuses the choice while the flag is false), so
//!   this is a legibility surface over a protocol guarantee, not the guarantee
//!   itself. Declining the warning sends **nothing**: the proposal stays open.
//! - **BR-5 (unattended).** `--yes` answers without asking anything at all,
//!   proven by a test that asserts the prompter was never called.
//! - **Late attach.** The proposal event is broadcast once and never replayed, so
//!   a client that attaches afterwards finds the open prompt through
//!   `model/status`'s `pending_request_id` and answers it from `model/list` —
//!   [`resolve_outstanding`]. That path claims only what it can see: `model/list`
//!   does not name the daemon's proposed entry, so it is offered as "accept as
//!   offered" rather than being guessed at and mis-named.
//!
//! Everything is a pure function of a protocol payload plus a [`Prompter`], so
//! every path above — including the double-confirm and the abort — is unit-tested
//! against scripted answers with no daemon and no socket.

use std::path::{Path, PathBuf};

use teton_protocol::events::{CatalogEntryView, ModelSelectionProposed};
use teton_protocol::methods::{
    InstallStatus, ModelConfirmOutcome, ModelConfirmParams, ModelListEntry, ModelListResult,
    ModelSelectionView, ModelStatusResult,
};
use teton_protocol::RequestId;

use crate::firstrun;
use crate::prompt::Prompter;
use crate::render::{LineKind, Surface};

/// Subdirectory of the daemon state directory the weights install into.
///
/// Mirrors `tetond`'s own `WEIGHTS_DIR`. The duplication is deliberate and is the
/// price of BR-11: [`teton_protocol::methods::InstallStateView`] carries no path,
/// so a client that wants to *show* the path derives it from the same state-dir
/// convention it already uses to find the socket, rather than having one sent.
const WEIGHTS_DIR: &str = "models";

/// The message shown whenever the user backs out without answering.
const LEFT_OPEN: &str =
    "left the proposal open — nothing was sent; sessions run remote-only until you answer.";

/// Collect the user's answer to a live `model_selection_proposed` event.
///
/// Returns the `model/confirm` to send, or `None` when nothing should be sent —
/// the user backed out of the BR-3 warning, chose to leave the prompt open, or
/// hit EOF. `None` is not a decline: declining is an explicit answer that the
/// daemon persists (BR-4), so it is never inferred from silence.
pub fn resolve_proposal(
    proposal: &ModelSelectionProposed,
    auto_accept: bool,
    surface: &mut dyn Surface,
    prompter: &mut dyn Prompter,
) -> Option<ModelConfirmParams> {
    firstrun::render_proposal(proposal, surface);
    let request_id = &proposal.request_id;

    if auto_accept {
        return auto_accepted(request_id, proposal.proposed.is_some(), surface);
    }

    // The proposal's own question first — REQ-544's `confirm_model`, finally
    // wired to the hook it was written for. A "no" here is not a decline; it
    // opens the override menu (BR-3).
    if let Some(proposed) = &proposal.proposed {
        if firstrun::confirm_model(
            &proposed.entry.name,
            Some(proposed.entry.size_bytes),
            prompter,
        ) {
            return Some(confirm(request_id, ModelConfirmOutcome::Accept));
        }
    }

    choose_from(
        request_id,
        &proposal.alternatives,
        proposal.probe.total_ram_bytes,
        false,
        surface,
        prompter,
    )
}

/// Answer a proposal that was raised *before* this client attached.
///
/// The daemon broadcasts a proposal once; a late-attaching client learns of it
/// from `model/status.pending_request_id` and gets the machine and the catalog
/// from `model/list`. What `model/list` cannot say is which entry the daemon
/// proposed, so this renders the hardware reasoning it *does* have and offers
/// "accept as offered" alongside every explicitly named alternative.
pub fn resolve_outstanding(
    request_id: &RequestId,
    list: &ModelListResult,
    auto_accept: bool,
    surface: &mut dyn Surface,
    prompter: &mut dyn Prompter,
) -> Option<ModelConfirmParams> {
    surface.line(
        LineKind::Prompt,
        "a local-model proposal raised before this client attached is still awaiting an answer:",
    );
    firstrun::render_probe(&list.probe, surface);
    surface.line(
        LineKind::Info,
        &format!(
            "  proposed: the daemon's own pick for the {} band — accept it as offered, or name \
             one of these instead:",
            firstrun::band_label(list.probe.chosen_band)
        ),
    );
    render_catalog_rows(
        &list.models,
        selected_name(list.selection.as_ref()),
        surface,
    );

    if auto_accept {
        return auto_accepted(request_id, true, surface);
    }

    let entries: Vec<CatalogEntryView> = list.models.iter().map(|m| m.entry.clone()).collect();
    choose_from(
        request_id,
        &entries,
        list.probe.total_ram_bytes,
        true,
        surface,
        prompter,
    )
}

/// The BR-5 unattended answer: accept without asking anything.
///
/// With nothing proposed there is nothing to accept, so the prompt is left open
/// (sessions run remote-only, BR-1) rather than being answered with a `decline`
/// the user never asked for — a decline is persisted and suppresses re-prompting
/// (BR-4), which is far too big a consequence to infer from a `--yes`.
fn auto_accepted(
    request_id: &RequestId,
    has_proposal: bool,
    surface: &mut dyn Surface,
) -> Option<ModelConfirmParams> {
    if has_proposal {
        surface.line(
            LineKind::Notice,
            "auto-accept: installing the proposed model without prompting (BR-5).",
        );
        return Some(confirm(request_id, ModelConfirmOutcome::Accept));
    }
    surface.line(
        LineKind::Notice,
        "auto-accept: no catalog entry fits this machine, so there is nothing to accept — the \
         proposal stays open and sessions run remote-only.",
    );
    None
}

/// The override menu: pick an entry, decline the local tier, or leave it open.
fn choose_from(
    request_id: &RequestId,
    entries: &[CatalogEntryView],
    total_ram_bytes: u64,
    offer_accept: bool,
    surface: &mut dyn Surface,
    prompter: &mut dyn Prompter,
) -> Option<ModelConfirmParams> {
    if !offer_accept {
        surface.line(LineKind::Prompt, "choose a local model instead:");
        firstrun::render_alternatives(entries, total_ram_bytes, surface);
    }
    let question = match (offer_accept, entries.is_empty()) {
        (true, _) => {
            "  [a]ccept as offered, a number to install that model, [d]ecline the local tier, \
             or [q] to leave it open: "
        }
        (false, false) => {
            "  a number to install that model, [d]ecline the local tier, or [q] to leave it \
             open: "
        }
        (false, true) => "  [d]ecline the local tier, or [q] to leave the proposal open: ",
    };

    loop {
        let Some(answer) = prompter.ask(question) else {
            // EOF is not an answer: leave the prompt open rather than deciding
            // the local tier's fate on a Ctrl-D.
            surface.line(LineKind::Notice, LEFT_OPEN);
            return None;
        };
        match answer.trim().to_lowercase().as_str() {
            "a" | "accept" if offer_accept => {
                return Some(confirm(request_id, ModelConfirmOutcome::Accept))
            }
            "d" | "decline" => return Some(confirm(request_id, ModelConfirmOutcome::Decline)),
            "q" | "quit" | "" => {
                surface.line(LineKind::Notice, LEFT_OPEN);
                return None;
            }
            other => match parse_choice(other, entries.len()) {
                Some(index) => {
                    let entry = &entries[index];
                    return match choose_outcome(entry, total_ram_bytes, surface, prompter) {
                        Some(outcome) => Some(confirm(request_id, outcome)),
                        // BR-3: backing out of the warning aborts. Nothing is
                        // sent and the menu is not re-shown, so a second stray
                        // keystroke cannot install what was just refused.
                        None => {
                            surface.line(
                                LineKind::Notice,
                                &format!("not installing {}; nothing was sent.", entry.name),
                            );
                            None
                        }
                    };
                }
                None => surface.line(
                    LineKind::Prompt,
                    &format!(
                        "  not a choice — enter a number from 1 to {}, d, or q.",
                        entries.len()
                    ),
                ),
            },
        }
    }
}

/// Parse a 1-based menu selection into a 0-based index, rejecting anything out
/// of range (and everything non-numeric).
fn parse_choice(input: &str, len: usize) -> Option<usize> {
    let index = input.parse::<usize>().ok()?;
    (index >= 1 && index <= len).then_some(index - 1)
}

/// Turn a chosen entry into an outcome, gating an above-floor pick on BR-3's
/// second confirmation. `None` means the user refused the warning: send nothing.
fn choose_outcome(
    entry: &CatalogEntryView,
    total_ram_bytes: u64,
    surface: &mut dyn Surface,
    prompter: &mut dyn Prompter,
) -> Option<ModelConfirmOutcome> {
    if entry.ram_floor_bytes <= total_ram_bytes {
        return Some(ModelConfirmOutcome::Choose {
            name: entry.name.clone(),
            confirmed_above_ram_floor: false,
        });
    }
    confirm_above_ram_floor(
        &entry.name,
        entry.ram_floor_bytes,
        total_ram_bytes,
        surface,
        prompter,
    )
    .then(|| ModelConfirmOutcome::Choose {
        name: entry.name.clone(),
        confirmed_above_ram_floor: true,
    })
}

/// Warn that a pick exceeds the machine's RAM and ask a second time (BR-3).
///
/// Note the default: unlike [`firstrun::confirm_model`], an empty answer here
/// means **no**. The whole point of the second confirmation is that an over-sized
/// install cannot happen by pressing return.
#[must_use]
pub fn confirm_above_ram_floor(
    name: &str,
    ram_floor_bytes: u64,
    total_ram_bytes: u64,
    surface: &mut dyn Surface,
    prompter: &mut dyn Prompter,
) -> bool {
    surface.line(
        LineKind::Notice,
        &format!(
            "warning: {name} needs {} RAM and this machine has {}. It may fail to load or swap \
             heavily. Your machine, your call — but not by accident.",
            firstrun::format_bytes(ram_floor_bytes),
            firstrun::format_bytes(total_ram_bytes),
        ),
    );
    match prompter.ask(&format!("  install {name} anyway? [y/N] ")) {
        Some(answer) => matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"),
        None => false,
    }
}

/// Build a `model/confirm` for `request_id`.
fn confirm(request_id: &RequestId, outcome: ModelConfirmOutcome) -> ModelConfirmParams {
    ModelConfirmParams {
        request_id: request_id.clone(),
        outcome,
    }
}

// ---------------------------------------------------------------------------
// `teton model list` / `status` (AC-9)
// ---------------------------------------------------------------------------

/// Render `model/list`: the machine, the catalog with each entry's fit, and the
/// selection in force (AC-9).
pub fn render_list(list: &ModelListResult, surface: &mut dyn Surface) {
    surface.line(LineKind::Info, "local model catalog:");
    firstrun::render_probe(&list.probe, surface);
    render_catalog_rows(
        &list.models,
        selected_name(list.selection.as_ref()),
        surface,
    );
    render_selection(list.selection.as_ref(), surface);
}

/// Render `model/status`: the decision, the weights' install state, the locally
/// derived weights path, and any proposal still awaiting an answer (AC-9).
///
/// `install_path` is resolved by the caller from the daemon state directory —
/// it is a local display and never crosses the protocol boundary (BR-11).
pub fn render_status(
    status: &ModelStatusResult,
    install_path: Option<&Path>,
    surface: &mut dyn Surface,
) {
    render_selection(status.selection.as_ref(), surface);
    match &status.install {
        Some(install) => {
            surface.line(
                LineKind::Info,
                &format!(
                    "install:   {} — {}",
                    install.model_name,
                    install_label(install.status)
                ),
            );
            if let Some(path) = install_path {
                surface.line(LineKind::Info, &format!("weights:   {}", path.display()));
            }
        }
        None => surface.line(
            LineKind::Info,
            "install:   nothing selected, so nothing is installed.",
        ),
    }
    if let Some(request_id) = &status.pending_request_id {
        surface.line(
            LineKind::Notice,
            &format!(
                "a model proposal ({request_id}) is awaiting an answer — run `teton` to answer it."
            ),
        );
    }
}

/// The numbered catalog rows, marking the current selection and each entry's
/// fit. The fits are the daemon's (`model/list` computes them against the probe)
/// so every client renders the same verdict rather than re-deriving it.
fn render_catalog_rows(
    models: &[ModelListEntry],
    selected: Option<&str>,
    surface: &mut dyn Surface,
) {
    if models.is_empty() {
        surface.line(LineKind::Info, "  the catalog is empty.");
        return;
    }
    for (index, model) in models.iter().enumerate() {
        let marker = if selected == Some(model.entry.name.as_str()) {
            "*"
        } else {
            " "
        };
        let mut notes = Vec::new();
        if !model.fits_ram {
            notes.push("above this machine's RAM");
        }
        if !model.fits_disk {
            notes.push("not enough free disk");
        }
        let fit = if notes.is_empty() {
            "fits".to_owned()
        } else {
            notes.join(", ")
        };
        surface.line(
            LineKind::Info,
            &format!(
                "{marker} {}. {} [{}] — {} download, needs {} RAM — {fit}",
                index + 1,
                model.entry.name,
                firstrun::tier_label(model.entry.band),
                firstrun::format_bytes(model.entry.size_bytes),
                firstrun::format_bytes(model.entry.ram_floor_bytes),
            ),
        );
    }
}

/// Render the decision in force (or its absence).
fn render_selection(selection: Option<&ModelSelectionView>, surface: &mut dyn Surface) {
    let text = match selection {
        None => "selection: none recorded yet — the daemon proposes one on first run.".to_owned(),
        Some(selection) if selection.declined_local => format!(
            "selection: local tier declined ({}) — sessions run remote-only.",
            firstrun::source_label(selection.source)
        ),
        Some(selection) => match &selection.model_name {
            Some(name) => format!(
                "selection: {name} ({})",
                firstrun::source_label(selection.source)
            ),
            None => format!(
                "selection: recorded with no model ({})",
                firstrun::source_label(selection.source)
            ),
        },
    };
    surface.line(LineKind::Info, &text);
}

/// The selected model's name, when one is selected.
fn selected_name(selection: Option<&ModelSelectionView>) -> Option<&str> {
    selection.and_then(|s| s.model_name.as_deref())
}

/// Plain-language label for an install state.
fn install_label(status: InstallStatus) -> &'static str {
    match status {
        InstallStatus::Absent => "absent (nothing downloaded yet)",
        InstallStatus::Partial => "partial (resumable; never loaded)",
        InstallStatus::Verified => "verified",
        InstallStatus::Corrupt => "corrupt (failed its integrity check; will be discarded)",
    }
}

/// Where `model_name`'s weights live, derived from the daemon state directory.
///
/// BR-11 keeps absolute paths out of every protocol payload, so the path shown by
/// `teton model status` is computed here from the same convention the client uses
/// to find the socket — never received over the wire.
#[must_use]
pub fn weights_path(base_dir: &Path, model_name: &str) -> PathBuf {
    base_dir
        .join(WEIGHTS_DIR)
        .join(format!("{model_name}.gguf"))
}

/// Scripted protocol payloads shared by this module's tests and `firstrun`'s.
#[cfg(test)]
pub(crate) mod testing {
    use teton_protocol::events::{
        CatalogEntryView, ChosenBand, GpuClass, ModelSelectionProposed, ProbeReportView,
        ProposedModel, TierBand,
    };
    use teton_protocol::methods::{ModelListEntry, ModelListResult};

    /// 16 GiB, plenty of disk, Apple Silicon, mid band.
    pub const TOTAL_RAM: u64 = 16 * 1024 * 1024 * 1024;
    /// Free disk on the scripted machine.
    pub const FREE_DISK: u64 = 120 * 1024 * 1024 * 1024;

    /// One catalog entry.
    pub fn entry(
        name: &str,
        band: TierBand,
        size_bytes: u64,
        ram_floor_bytes: u64,
    ) -> CatalogEntryView {
        CatalogEntryView {
            name: name.to_owned(),
            band,
            size_bytes,
            ram_floor_bytes,
        }
    }

    /// The scripted probe report.
    pub fn probe() -> ProbeReportView {
        ProbeReportView {
            total_ram_bytes: TOTAL_RAM,
            free_disk_bytes: FREE_DISK,
            gpu_class: GpuClass::AppleSilicon,
            chosen_band: ChosenBand::Mid,
            reason: "16 GiB of RAM clears the mid band's floor with headroom to spare".to_owned(),
        }
    }

    /// An alternative that fits this machine.
    pub fn small_entry() -> CatalogEntryView {
        entry(
            "qwen2.5-coder-3b",
            TierBand::Small,
            2_104_932_800,
            5_368_709_120,
        )
    }

    /// An alternative whose RAM floor is above this machine (the BR-3 path).
    pub fn oversized_entry() -> CatalogEntryView {
        entry(
            "qwen3-coder-30b",
            TierBand::Large,
            18_000_000_000,
            34_359_738_368,
        )
    }

    /// A full proposal: mid-band pick plus one fitting and one over-sized
    /// alternative.
    pub fn proposal() -> ModelSelectionProposed {
        ModelSelectionProposed {
            request_id: "req-model-1".into(),
            probe: probe(),
            proposed: Some(ProposedModel {
                entry: entry(
                    "qwen2.5-coder-7b",
                    TierBand::Mid,
                    4_700_000_000,
                    8_589_934_592,
                ),
                required_disk_bytes: 5_200_000_000,
            }),
            alternatives: vec![small_entry(), oversized_entry()],
        }
    }

    /// A `model/list` result over the same machine and catalog.
    pub fn list_result() -> ModelListResult {
        ModelListResult {
            probe: probe(),
            models: vec![
                ModelListEntry {
                    entry: small_entry(),
                    fits_ram: true,
                    fits_disk: true,
                },
                ModelListEntry {
                    entry: entry(
                        "qwen2.5-coder-7b",
                        TierBand::Mid,
                        4_700_000_000,
                        8_589_934_592,
                    ),
                    fits_ram: true,
                    fits_disk: true,
                },
                ModelListEntry {
                    entry: oversized_entry(),
                    fits_ram: false,
                    fits_disk: false,
                },
            ],
            selection: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{list_result, proposal, TOTAL_RAM};
    use super::*;
    use crate::prompt::ScriptedPrompter;
    use crate::render::RecordingSurface;
    use teton_protocol::events::SelectionSource;
    use teton_protocol::methods::InstallStateView;

    /// Run `resolve_proposal` with scripted answers, returning the reply (if any)
    /// alongside the surface and prompter for assertions.
    fn answer(
        answers: &[&str],
        auto_accept: bool,
    ) -> (
        Option<ModelConfirmParams>,
        RecordingSurface,
        ScriptedPrompter,
    ) {
        let mut surface = RecordingSurface::new();
        let mut prompter = ScriptedPrompter::new(answers);
        let reply = resolve_proposal(&proposal(), auto_accept, &mut surface, &mut prompter);
        (reply, surface, prompter)
    }

    #[test]
    fn accepting_the_proposal_sends_accept() {
        // An empty answer to `confirm_model` means yes (its long-standing rule).
        let (reply, _, prompter) = answer(&[""], false);
        assert_eq!(
            reply.expect("an accept is sent").outcome,
            ModelConfirmOutcome::Accept
        );
        assert_eq!(prompter.asked, 1, "accepting takes exactly one question");
    }

    #[test]
    fn the_reply_correlates_with_the_proposals_request_id() {
        let (reply, _, _) = answer(&["y"], false);
        assert_eq!(reply.unwrap().request_id, RequestId::from("req-model-1"));
    }

    #[test]
    fn choosing_a_fitting_alternative_sends_choose_without_the_extra_flag() {
        // "n" to the proposal, then alternative 1 (which fits this machine).
        let (reply, _, prompter) = answer(&["n", "1"], false);
        assert_eq!(
            reply.expect("a choice is sent").outcome,
            ModelConfirmOutcome::Choose {
                name: "qwen2.5-coder-3b".to_owned(),
                confirmed_above_ram_floor: false,
            }
        );
        // No second confirmation was asked for — it fits.
        assert_eq!(prompter.asked, 2);
    }

    #[test]
    fn an_above_floor_choice_warns_and_is_only_sent_after_a_second_confirmation() {
        // "n" to the proposal, alternative 2 (over-sized), then an explicit yes.
        let (reply, surface, prompter) = answer(&["n", "2", "y"], false);
        assert!(
            surface.any_line_contains(LineKind::Notice, "warning: qwen3-coder-30b needs 32.0 GB"),
            "the over-sized pick must warn explicitly: {:?}",
            surface.lines_of(LineKind::Notice)
        );
        assert_eq!(
            reply.expect("the confirmed choice is sent").outcome,
            ModelConfirmOutcome::Choose {
                name: "qwen3-coder-30b".to_owned(),
                // BR-3: the daemon refuses this pick unless the flag is set, and
                // the flag is set only by the second confirmation.
                confirmed_above_ram_floor: true,
            }
        );
        assert_eq!(prompter.asked, 3, "proposal, menu, second confirmation");
    }

    #[test]
    fn declining_the_above_floor_warning_sends_nothing_and_does_not_reopen_the_menu() {
        let (reply, surface, prompter) = answer(&["n", "2", "n", "1"], false);
        assert!(
            reply.is_none(),
            "refusing the warning must abort without sending anything (AC-3)"
        );
        // The trailing "1" was never consumed: the abort is final, not a retry.
        assert_eq!(prompter.asked, 3);
        assert!(surface.any_line_contains(LineKind::Notice, "not installing qwen3-coder-30b"));
    }

    #[test]
    fn an_empty_answer_to_the_warning_is_a_no() {
        // Unlike the proposal's own [Y/n], the second confirmation defaults to no.
        let (reply, _, _) = answer(&["n", "2", ""], false);
        assert!(reply.is_none());
    }

    #[test]
    fn declining_the_local_tier_sends_decline() {
        let (reply, _, _) = answer(&["n", "d"], false);
        assert_eq!(
            reply.expect("a decline is sent").outcome,
            ModelConfirmOutcome::Decline
        );
    }

    #[test]
    fn quitting_the_menu_leaves_the_proposal_open_without_sending() {
        let (reply, surface, _) = answer(&["n", "q"], false);
        assert!(reply.is_none());
        assert!(surface.any_line_contains(LineKind::Notice, "left the proposal open"));
    }

    #[test]
    fn eof_leaves_the_proposal_open_rather_than_declining() {
        // EOF at the menu: a Ctrl-D must never be read as "run remote-only
        // forever" (BR-4 persists a decline).
        let (reply, surface, _) = answer(&["n"], false);
        assert!(reply.is_none());
        assert!(surface.any_line_contains(LineKind::Notice, "left the proposal open"));
    }

    #[test]
    fn an_unparsable_menu_answer_re_asks_rather_than_guessing() {
        let (reply, surface, prompter) = answer(&["n", "nonsense", "9", "1"], false);
        assert_eq!(
            reply.expect("the eventual valid choice is sent").outcome,
            ModelConfirmOutcome::Choose {
                name: "qwen2.5-coder-3b".to_owned(),
                confirmed_above_ram_floor: false,
            }
        );
        assert_eq!(prompter.asked, 4);
        assert!(surface.any_line_contains(LineKind::Prompt, "not a choice"));
    }

    #[test]
    fn auto_accept_answers_with_no_prompt_at_all() {
        let (reply, surface, prompter) = answer(&[], true);
        assert_eq!(
            reply.expect("auto-accept still answers").outcome,
            ModelConfirmOutcome::Accept
        );
        assert_eq!(prompter.asked, 0, "AC-5: no user input is read");
        assert!(surface.any_line_contains(LineKind::Notice, "auto-accept"));
        // The proposal is still rendered — unattended is not invisible (BR-2).
        assert!(surface.any_line_contains(LineKind::Info, "qwen2.5-coder-7b"));
    }

    #[test]
    fn auto_accept_with_nothing_proposed_leaves_the_prompt_open() {
        let mut proposal = proposal();
        proposal.proposed = None;
        let mut surface = RecordingSurface::new();
        let mut prompter = ScriptedPrompter::new(&[]);
        let reply = resolve_proposal(&proposal, true, &mut surface, &mut prompter);
        assert!(
            reply.is_none(),
            "there is nothing to accept, and a decline must never be inferred"
        );
        assert!(surface.any_line_contains(LineKind::Notice, "nothing to accept"));
    }

    #[test]
    fn a_proposal_with_no_alternatives_still_offers_decline() {
        let mut proposal = proposal();
        proposal.alternatives.clear();
        let mut surface = RecordingSurface::new();
        let mut prompter = ScriptedPrompter::new(&["n", "d"]);
        let reply = resolve_proposal(&proposal, false, &mut surface, &mut prompter);
        assert_eq!(reply.unwrap().outcome, ModelConfirmOutcome::Decline);
    }

    // -----------------------------------------------------------------------
    // late attach: the proposal event was broadcast before this client existed
    // -----------------------------------------------------------------------

    /// Answer an outstanding proposal discovered through `model/status`.
    fn answer_outstanding(
        answers: &[&str],
        auto_accept: bool,
    ) -> (Option<ModelConfirmParams>, RecordingSurface) {
        let mut surface = RecordingSurface::new();
        let mut prompter = ScriptedPrompter::new(answers);
        let reply = resolve_outstanding(
            &RequestId::from("req-late-1"),
            &list_result(),
            auto_accept,
            &mut surface,
            &mut prompter,
        );
        (reply, surface)
    }

    #[test]
    fn a_late_attaching_client_can_accept_the_outstanding_proposal() {
        let (reply, surface) = answer_outstanding(&["a"], false);
        let reply = reply.expect("an accept is sent");
        assert_eq!(reply.outcome, ModelConfirmOutcome::Accept);
        assert_eq!(reply.request_id, RequestId::from("req-late-1"));
        // It still renders the hardware reasoning (BR-2) from `model/list`.
        assert!(surface.any_line_contains(LineKind::Info, "16.0 GB RAM"));
        assert!(surface.any_line_contains(LineKind::Info, "band:     mid"));
        assert!(surface.any_line_contains(LineKind::Prompt, "before this client attached"));
    }

    #[test]
    fn a_late_attaching_client_honours_the_same_double_confirmation() {
        // Entry 3 of the catalog is above this machine's RAM.
        let (reply, surface) = answer_outstanding(&["3", "y"], false);
        assert!(surface.any_line_contains(LineKind::Notice, "warning: qwen3-coder-30b"));
        assert_eq!(
            reply.unwrap().outcome,
            ModelConfirmOutcome::Choose {
                name: "qwen3-coder-30b".to_owned(),
                confirmed_above_ram_floor: true,
            }
        );

        let (refused, _) = answer_outstanding(&["3", "n"], false);
        assert!(refused.is_none(), "refusing the warning sends nothing");
    }

    #[test]
    fn a_late_attaching_client_can_decline_or_leave_it_open() {
        let (declined, _) = answer_outstanding(&["d"], false);
        assert_eq!(declined.unwrap().outcome, ModelConfirmOutcome::Decline);

        let (left_open, surface) = answer_outstanding(&["q"], false);
        assert!(left_open.is_none());
        assert!(surface.any_line_contains(LineKind::Notice, "left the proposal open"));
    }

    #[test]
    fn a_late_attaching_client_auto_accepts_without_prompting() {
        let (reply, _) = answer_outstanding(&[], true);
        assert_eq!(reply.unwrap().outcome, ModelConfirmOutcome::Accept);
    }

    // -----------------------------------------------------------------------
    // `teton model list` / `status`
    // -----------------------------------------------------------------------

    #[test]
    fn render_list_shows_fit_per_entry_and_marks_the_current_selection() {
        let mut list = list_result();
        list.selection = Some(ModelSelectionView {
            model_name: Some("qwen2.5-coder-7b".to_owned()),
            source: SelectionSource::UserOverride,
            declined_local: false,
            decided_at_ms: 1_700_000_000_000,
        });
        let mut surface = RecordingSurface::new();
        render_list(&list, &mut surface);
        let text = surface.lines_of(LineKind::Info).join("\n");

        assert!(text.contains("16.0 GB RAM"), "{text}");
        assert!(text.contains("qwen2.5-coder-3b"), "{text}");
        assert!(
            text.contains("* 2. qwen2.5-coder-7b"),
            "the selection must be marked: {text}"
        );
        assert!(text.contains("— fits"), "{text}");
        assert!(text.contains("above this machine's RAM"), "{text}");
        assert!(text.contains("not enough free disk"), "{text}");
        assert!(
            text.contains("selection: qwen2.5-coder-7b (user override)"),
            "{text}"
        );
    }

    #[test]
    fn render_list_without_a_decision_says_none_recorded() {
        let mut surface = RecordingSurface::new();
        render_list(&list_result(), &mut surface);
        assert!(surface.any_line_contains(LineKind::Info, "selection: none recorded yet"));
    }

    #[test]
    fn render_list_reports_a_declined_local_tier() {
        let mut list = list_result();
        list.selection = Some(ModelSelectionView {
            model_name: None,
            source: SelectionSource::UserOverride,
            declined_local: true,
            decided_at_ms: 1,
        });
        let mut surface = RecordingSurface::new();
        render_list(&list, &mut surface);
        assert!(surface.any_line_contains(LineKind::Info, "local tier declined"));
    }

    #[test]
    fn render_status_reports_install_state_and_a_locally_derived_path() {
        let status = ModelStatusResult {
            selection: Some(ModelSelectionView {
                model_name: Some("qwen2.5-coder-7b".to_owned()),
                source: SelectionSource::Probe,
                declined_local: false,
                decided_at_ms: 1,
            }),
            install: Some(InstallStateView {
                model_name: "qwen2.5-coder-7b".to_owned(),
                status: InstallStatus::Verified,
            }),
            pending_request_id: None,
        };
        let path = weights_path(Path::new("/state/teton"), "qwen2.5-coder-7b");
        let mut surface = RecordingSurface::new();
        render_status(&status, Some(&path), &mut surface);
        let text = surface.lines_of(LineKind::Info).join("\n");
        assert!(text.contains("selection: qwen2.5-coder-7b"), "{text}");
        assert!(text.contains("verified"), "{text}");
        assert!(
            text.contains("/state/teton/models/qwen2.5-coder-7b.gguf"),
            "{text}"
        );
    }

    #[test]
    fn render_status_surfaces_an_outstanding_proposal() {
        let status = ModelStatusResult {
            selection: None,
            install: None,
            pending_request_id: Some(RequestId::from("req-open-1")),
        };
        let mut surface = RecordingSurface::new();
        render_status(&status, None, &mut surface);
        assert!(surface.any_line_contains(LineKind::Info, "nothing is installed"));
        assert!(surface.any_line_contains(LineKind::Notice, "req-open-1"));
    }

    #[test]
    fn weights_path_mirrors_the_daemon_state_dir_convention() {
        assert_eq!(
            weights_path(Path::new("/run/user/1000/teton"), "qwen2.5-coder-3b"),
            PathBuf::from("/run/user/1000/teton/models/qwen2.5-coder-3b.gguf")
        );
    }

    #[test]
    fn confirm_above_ram_floor_defaults_to_no_and_accepts_only_an_explicit_yes() {
        let mut surface = RecordingSurface::new();
        for (answers, expected) in [
            (vec!["y"], true),
            (vec!["yes"], true),
            (vec!["n"], false),
            (vec![""], false),
            (vec![], false), // EOF
        ] {
            let mut prompter = ScriptedPrompter::new(&answers);
            assert_eq!(
                confirm_above_ram_floor(
                    "m",
                    32_000_000_000,
                    TOTAL_RAM,
                    &mut surface,
                    &mut prompter
                ),
                expected,
                "answers: {answers:?}"
            );
        }
    }
}
