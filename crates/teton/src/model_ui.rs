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
//! - **Late attach.** The proposal event is broadcast once and never replayed —
//!   and the daemon may publish it before it accepts its first connection — so a
//!   client that attaches afterwards retrieves the *whole* proposal from
//!   `model/status.pending_proposal` and renders it through the very same
//!   [`resolve_proposal`] the live event takes. One rendering path, so the pick
//!   is named with its download size and RAM floor (BR-2) no matter when the
//!   client showed up. A client that sees both the event and the polled proposal
//!   de-duplicates on the shared `request_id` and prompts exactly once.
//!
//! Everything is a pure function of a protocol payload plus a [`Prompter`], so
//! every path above — including the double-confirm and the abort — is unit-tested
//! against scripted answers with no daemon and no socket.

use std::path::{Path, PathBuf};

use teton_protocol::events::{CatalogEntryView, ModelSelectionProposed, ProposedModel};
use teton_protocol::methods::{
    InstallStatus, ModelConfirmOutcome, ModelConfirmParams, ModelListEntry, ModelListResult,
    ModelSelectionView, ModelStatusResult,
};
use teton_protocol::RequestId;

use crate::firstrun;
use crate::prompt::Prompter;
use crate::render::{LineKind, Surface};

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
        return auto_accepted(
            request_id,
            proposal.proposed.as_ref(),
            proposal.probe.total_ram_bytes,
            surface,
        );
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
            match accept_outcome(
                &proposed.entry,
                proposal.probe.total_ram_bytes,
                surface,
                prompter,
            ) {
                Some(outcome) => return Some(confirm(request_id, outcome)),
                // The proposed entry needs more RAM than this machine has and the
                // user backed out of that warning. Falling through to the menu is
                // the same thing a "no" to the question above does — it offers
                // the smaller entries — and it is emphatically not a decline.
                None => surface.line(
                    LineKind::Notice,
                    &format!("not installing {}; nothing was sent.", proposed.entry.name),
                ),
            }
        }
    }

    choose_from(
        request_id,
        &proposal.alternatives,
        proposal.probe.total_ram_bytes,
        surface,
        prompter,
    )
}

/// Answer a proposal that was raised *before* this client attached.
///
/// Deliberately **not** a second rendering of the proposal: it prints the one
/// thing that differs — that this prompt predates the connection — and then hands
/// the identical payload to [`resolve_proposal`]. A late-attaching client and a
/// live one therefore see the same named pick, the same size, the same RAM floor
/// and the same menu, because they run the same code over the same bytes.
///
/// The previous shape reconstructed the prompt from `model/list`, which knows the
/// catalog but not the daemon's choice within it, so it could only offer "the
/// daemon's own pick for the <band> band". That is not the consent BR-2 asks for,
/// and no amount of careful wording made it one.
pub fn resolve_outstanding(
    proposal: &ModelSelectionProposed,
    auto_accept: bool,
    surface: &mut dyn Surface,
    prompter: &mut dyn Prompter,
) -> Option<ModelConfirmParams> {
    surface.line(
        LineKind::Notice,
        "a local-model proposal raised before this client attached is still awaiting an answer:",
    );
    resolve_proposal(proposal, auto_accept, surface, prompter)
}

/// The BR-5 unattended answer: accept without asking anything.
///
/// With nothing proposed there is nothing to accept, so the prompt is left open
/// (sessions run remote-only, BR-1) rather than being answered with a `decline`
/// the user never asked for — a decline is persisted and suppresses re-prompting
/// (BR-4), which is far too big a consequence to infer from a `--yes`.
///
/// An above-the-RAM-floor pick is left open for the opposite reason: BR-3 wants a
/// *second* confirmation for it, and `--yes` is one flag, not two answers (E-1).
/// `--yes` means "do not ask me about the ordinary case"; it cannot stand in for
/// the deliberation the oversized case exists to force.
fn auto_accepted(
    request_id: &RequestId,
    proposed: Option<&ProposedModel>,
    total_ram_bytes: u64,
    surface: &mut dyn Surface,
) -> Option<ModelConfirmParams> {
    let Some(proposed) = proposed else {
        surface.line(
            LineKind::Notice,
            "auto-accept: no catalog entry fits this machine, so there is nothing to accept — \
             the proposal stays open and sessions run remote-only.",
        );
        return None;
    };
    if proposed.entry.ram_floor_bytes > total_ram_bytes {
        surface.line(
            LineKind::Notice,
            &format!(
                "auto-accept declined to answer: the proposed model {} needs {} RAM and this \
                 machine has {}. An over-sized install needs a second, explicit confirmation \
                 (BR-3), which `--yes` is not — the proposal stays open. Answer it \
                 interactively, or choose it deliberately with `teton model set {}`.",
                proposed.entry.name,
                firstrun::format_bytes(proposed.entry.ram_floor_bytes),
                firstrun::format_bytes(total_ram_bytes),
                proposed.entry.name,
            ),
        );
        return None;
    }
    surface.line(
        LineKind::Notice,
        "auto-accept: installing the proposed model without prompting (BR-5).",
    );
    Some(confirm(request_id, ModelConfirmOutcome::Accept))
}

/// Turn a "yes, install the proposed model" into the outcome to send, gating an
/// above-the-floor proposal on BR-3's second confirmation (E-1).
///
/// The confirmed answer is a `choose`, not an `accept`: `accept` carries no
/// `confirmed_above_ram_floor` flag by construction, so it is the one answer the
/// daemon cannot honour for an over-sized entry. Naming the same entry through
/// `choose` is how the confirmation reaches the wire — and the daemon leaves the
/// proposal open on the refused `accept` precisely so this re-send works.
fn accept_outcome(
    entry: &CatalogEntryView,
    total_ram_bytes: u64,
    surface: &mut dyn Surface,
    prompter: &mut dyn Prompter,
) -> Option<ModelConfirmOutcome> {
    if entry.ram_floor_bytes <= total_ram_bytes {
        return Some(ModelConfirmOutcome::Accept);
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

/// The override menu: pick an entry, decline the local tier, or leave it open.
fn choose_from(
    request_id: &RequestId,
    entries: &[CatalogEntryView],
    total_ram_bytes: u64,
    surface: &mut dyn Surface,
    prompter: &mut dyn Prompter,
) -> Option<ModelConfirmParams> {
    surface.line(LineKind::Prompt, "choose a local model instead:");
    firstrun::render_alternatives(entries, total_ram_bytes, surface);
    // No "[a]ccept as offered" here: this menu is only reached after the user
    // has already declined the proposal's own question, which named the pick.
    // (It used to be offered on the late-attach path, which could not name what
    // "as offered" meant — that path is gone.)
    let question = if entries.is_empty() {
        "  [d]ecline the local tier, or [q] to leave the proposal open: "
    } else {
        "  a number to install that model, [d]ecline the local tier, or [q] to leave it open: "
    };

    loop {
        let Some(answer) = prompter.ask(question) else {
            // EOF is not an answer: leave the prompt open rather than deciding
            // the local tier's fate on a Ctrl-D.
            surface.line(LineKind::Notice, LEFT_OPEN);
            return None;
        };
        match answer.trim().to_lowercase().as_str() {
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
    if let Some(proposal) = &status.pending_proposal {
        // Name it. `teton model status` is a report, not a prompt, so it does not
        // ask — but a user told "something is awaiting an answer" without being
        // told *what* has learned nothing they can act on.
        let what = match &proposal.proposed {
            Some(proposed) => format!(
                "{} ({} download, needs {} RAM)",
                proposed.entry.name,
                firstrun::format_bytes(proposed.entry.size_bytes),
                firstrun::format_bytes(proposed.entry.ram_floor_bytes),
            ),
            None => "no fitting catalog entry — you would pick one yourself".to_owned(),
        };
        surface.line(
            LineKind::Notice,
            &format!(
                "proposal {} is awaiting an answer: {what} — run `teton` to answer it.",
                proposal.request_id
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
/// to find the socket — never received over the wire. The convention itself
/// (`models/<name>.gguf`) lives in `teton-protocol` so the daemon and the client
/// cannot drift.
#[must_use]
pub fn weights_path(base_dir: &Path, model_name: &str) -> PathBuf {
    teton_protocol::weights::weights_path(base_dir, model_name)
}

/// Scripted protocol payloads shared by this module's tests and `firstrun`'s.
#[cfg(test)]
pub(crate) mod testing {
    use teton_protocol::events::{
        CatalogEntryView, CatalogProvenance, ChosenBand, GpuClass, ModelSelectionProposed,
        ProbeReportView, ProposedModel, TierBand,
    };
    use teton_protocol::methods::{ModelListEntry, ModelListResult};

    /// 16 GiB, plenty of disk, Apple Silicon, mid band.
    pub const TOTAL_RAM: u64 = 16 * 1024 * 1024 * 1024;
    /// Free disk on the scripted machine.
    pub const FREE_DISK: u64 = 120 * 1024 * 1024 * 1024;

    /// One catalog entry, with a synthesized huggingface.co provenance so the
    /// rendered proposal has a source to show (H-2).
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
            provenance: CatalogProvenance {
                repo: format!("Qwen/{name}-GGUF"),
                host: "huggingface.co".to_owned(),
                revision: "f74adce".to_owned(),
            },
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
            fetch_notice: None,
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
            surface.any_line_contains(LineKind::Notice, "warning: qwen3-coder-30b needs 32.0 GiB"),
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

    /// A proposal whose *proposed* entry is over-sized — the pinned-oversized
    /// case a `[local_model] pinned` key produces.
    fn oversized_proposal() -> ModelSelectionProposed {
        let mut proposal = proposal();
        proposal.proposed = Some(ProposedModel {
            entry: super::testing::oversized_entry(),
            required_disk_bytes: 19_000_000_000,
        });
        proposal
    }

    fn answer_oversized(
        answers: &[&str],
        auto_accept: bool,
    ) -> (
        Option<ModelConfirmParams>,
        RecordingSurface,
        ScriptedPrompter,
    ) {
        let mut surface = RecordingSurface::new();
        let mut prompter = ScriptedPrompter::new(answers);
        let reply = resolve_proposal(
            &oversized_proposal(),
            auto_accept,
            &mut surface,
            &mut prompter,
        );
        (reply, surface, prompter)
    }

    /// E-1: saying yes to an over-sized *proposal* still costs a second answer.
    ///
    /// `confirm_model`'s question defaults to yes on an empty line, so without
    /// this a pinned entry the machine cannot hold was one Enter away from an
    /// 18 GB fetch. The confirmed answer rides as a `choose`, because `accept`
    /// has nowhere to carry the confirmation — and the daemon refuses an `accept`
    /// here for exactly that reason.
    #[test]
    fn accepting_an_over_sized_proposal_needs_the_second_confirmation_and_sends_choose() {
        let (reply, surface, prompter) = answer_oversized(&["", "y"], false);
        assert!(
            surface.any_line_contains(LineKind::Notice, "warning: qwen3-coder-30b needs 32.0 GiB"),
            "an over-sized proposal must warn before it installs: {:?}",
            surface.lines_of(LineKind::Notice)
        );
        assert_eq!(
            reply.expect("the confirmed answer is sent").outcome,
            ModelConfirmOutcome::Choose {
                name: "qwen3-coder-30b".to_owned(),
                confirmed_above_ram_floor: true,
            }
        );
        assert_eq!(prompter.asked, 2, "the proposal, then the confirmation");
    }

    /// Backing out of that warning is not a decline and not an install: it opens
    /// the menu, exactly as a "no" to the proposal's own question does.
    #[test]
    fn refusing_the_warning_on_an_over_sized_proposal_falls_through_to_the_menu() {
        // yes to the proposal, no to the warning, then alternative 1 (which fits).
        let (reply, surface, prompter) = answer_oversized(&["y", "n", "1"], false);
        assert!(surface.any_line_contains(LineKind::Notice, "not installing qwen3-coder-30b"));
        assert_eq!(
            reply.expect("the fitting alternative is sent").outcome,
            ModelConfirmOutcome::Choose {
                name: "qwen2.5-coder-3b".to_owned(),
                confirmed_above_ram_floor: false,
            }
        );
        assert_eq!(prompter.asked, 3, "proposal, warning, menu");
    }

    /// BR-5 vs BR-3: `--yes` is one flag, not two answers. It answers the
    /// ordinary case and declines to answer the one that exists to be deliberate.
    #[test]
    fn auto_accept_leaves_an_over_sized_proposal_open_rather_than_confirming_it() {
        let (reply, surface, prompter) = answer_oversized(&[], true);
        assert!(
            reply.is_none(),
            "`--yes` must not supply BR-3's second confirmation"
        );
        assert_eq!(prompter.asked, 0, "auto-accept never reads user input");
        assert!(
            surface.any_line_contains(LineKind::Notice, "second, explicit confirmation"),
            "the user must be told why nothing was answered: {:?}",
            surface.lines_of(LineKind::Notice)
        );
        // And the fitting case still auto-accepts, so this did not just disable
        // `--yes`.
        let (reply, _, _) = answer(&[], true);
        assert_eq!(
            reply
                .expect("a fitting proposal is still auto-accepted")
                .outcome,
            ModelConfirmOutcome::Accept
        );
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

    /// Answer an outstanding proposal retrieved through `model/status`.
    ///
    /// The payload is the *same* `ModelSelectionProposed` the live event carries,
    /// because that is now what `model/status.pending_proposal` returns.
    fn answer_outstanding(
        answers: &[&str],
        auto_accept: bool,
    ) -> (Option<ModelConfirmParams>, RecordingSurface) {
        let mut surface = RecordingSurface::new();
        let mut prompter = ScriptedPrompter::new(answers);
        let mut proposal = proposal();
        proposal.request_id = RequestId::from("req-late-1");
        let reply = resolve_outstanding(&proposal, auto_accept, &mut surface, &mut prompter);
        (reply, surface)
    }

    /// The defect this replaced: a late-attaching client could not name the
    /// daemon's pick, so it printed "the daemon's own pick for the mid band" and
    /// asked the user to accept a model they had never been shown. BR-2 says a
    /// bare name is not enough — a *missing* name is certainly not.
    #[test]
    fn a_late_attaching_client_renders_the_proposed_model_by_name_with_size_and_ram_floor() {
        let (reply, surface) = answer_outstanding(&[""], false);
        let reply = reply.expect("an accept is sent");
        assert_eq!(reply.outcome, ModelConfirmOutcome::Accept);
        assert_eq!(reply.request_id, RequestId::from("req-late-1"));

        let text = surface.lines_of(LineKind::Info).join("\n");
        // The pick, BY NAME, with its download size and its RAM floor.
        assert!(
            text.contains("proposed: qwen2.5-coder-7b"),
            "the proposed entry must be named: {text}"
        );
        assert!(text.contains("4.4 GiB download"), "download size: {text}");
        assert!(text.contains("needs 8.0 GiB RAM"), "RAM floor: {text}");
        // And nothing may describe the pick by its band any more.
        assert!(
            !text.contains("the daemon's own pick"),
            "the band-only stand-in must be gone: {text}"
        );
        // The hardware reasoning and the alternatives are there too (BR-2/BR-3).
        assert!(text.contains("16.0 GiB RAM"), "{text}");
        assert!(text.contains("band:     mid"), "{text}");
        assert!(text.contains("1. qwen2.5-coder-3b"), "{text}");
        assert!(surface.any_line_contains(LineKind::Notice, "before this client attached"));
    }

    /// The two delivery paths render identically, because they are one path.
    #[test]
    fn the_late_attach_rendering_matches_the_live_event_rendering() {
        let mut proposal = proposal();
        proposal.request_id = RequestId::from("req-late-1");

        let mut live = RecordingSurface::new();
        resolve_proposal(&proposal, true, &mut live, &mut ScriptedPrompter::new(&[]));

        let mut late = RecordingSurface::new();
        resolve_outstanding(&proposal, true, &mut late, &mut ScriptedPrompter::new(&[]));

        assert_eq!(
            live.lines_of(LineKind::Info),
            late.lines_of(LineKind::Info),
            "a client that attached late must see exactly what a live one sees"
        );
    }

    #[test]
    fn a_late_attaching_client_honours_the_same_double_confirmation() {
        // Alternative 2 is above this machine's RAM. Answering "n" to the
        // proposal's own question opens the override menu (it is not a decline).
        let (reply, surface) = answer_outstanding(&["n", "2", "y"], false);
        assert!(surface.any_line_contains(LineKind::Notice, "warning: qwen3-coder-30b"));
        assert_eq!(
            reply.unwrap().outcome,
            ModelConfirmOutcome::Choose {
                name: "qwen3-coder-30b".to_owned(),
                confirmed_above_ram_floor: true,
            }
        );

        let (refused, _) = answer_outstanding(&["n", "2", "n"], false);
        assert!(refused.is_none(), "refusing the warning sends nothing");
    }

    #[test]
    fn a_late_attaching_client_can_decline_or_leave_it_open() {
        let (declined, _) = answer_outstanding(&["n", "d"], false);
        assert_eq!(declined.unwrap().outcome, ModelConfirmOutcome::Decline);

        let (left_open, surface) = answer_outstanding(&["n", "q"], false);
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

        assert!(text.contains("16.0 GiB RAM"), "{text}");
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
            pending_proposal: None,
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
    fn render_status_names_the_outstanding_proposal_rather_than_just_flagging_one() {
        let mut proposal = proposal();
        proposal.request_id = RequestId::from("req-open-1");
        let status = ModelStatusResult {
            selection: None,
            install: None,
            pending_proposal: Some(proposal),
        };
        let mut surface = RecordingSurface::new();
        render_status(&status, None, &mut surface);
        assert!(surface.any_line_contains(LineKind::Info, "nothing is installed"));
        let notice = surface.lines_of(LineKind::Notice).join("\n");
        assert!(notice.contains("req-open-1"), "{notice}");
        assert!(notice.contains("qwen2.5-coder-7b"), "{notice}");
        assert!(notice.contains("4.4 GiB download"), "{notice}");
        assert!(notice.contains("needs 8.0 GiB RAM"), "{notice}");
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
