//! The first-run experience: the consent proposal and the model lifecycle.
//!
//! On a fresh machine the daemon probes the hardware, proposes a local model,
//! and **waits** (REQ-547 BR-1) — nothing is downloaded until a client answers.
//! This module renders that proposal with every element BR-2 demands (detected
//! RAM, free disk, GPU class, the chosen band, the plain-language reason, and the
//! proposed model's download size and RAM floor, plus the selectable
//! alternatives), because a bare model name is not consent. Collecting the answer
//! is [`crate::model_ui`]'s job; this module is rendering plus the accept/reject
//! question itself.
//!
//! Once a decision is in, the daemon runs the rest of the lifecycle — download,
//! post-download micro-benchmark, ready/step-down — and broadcasts each step as a
//! `model_lifecycle` event (REQ-544 BR-9), which is made legible here too.
//!
//! Every function is a pure function of a protocol payload rendered through the
//! [`Surface`] seam, so all of it is table-tested against scripted events with no
//! daemon and no model.

use teton_protocol::events::{
    CatalogEntryView, ChosenBand, FetchNotice, GpuClass, ModelLifecycleStage,
    ModelSelectionDecided, ModelSelectionProposed, ProbeReportView, SelectionSource, TierBand,
};

use crate::prompt::Prompter;
use crate::render::{LineKind, Surface};

/// Width of the textual download progress bar, in cells.
const BAR_WIDTH: usize = 24;

/// Render one lifecycle stage for `model_id` as a one-line notice.
pub fn render_lifecycle(model_id: &str, stage: &ModelLifecycleStage, surface: &mut dyn Surface) {
    let text = match stage {
        ModelLifecycleStage::Probed {
            ram_bytes,
            above_floor,
        } => {
            let floor = if *above_floor {
                "clears the local-tier floor"
            } else {
                "below the local-tier floor — will run remote-only"
            };
            format!("probe: {} RAM — {floor}", format_bytes(*ram_bytes))
        }
        ModelLifecycleStage::Download {
            downloaded_bytes,
            total_bytes,
        } => {
            format!(
                "download {model_id}: {}",
                progress_bar(*downloaded_bytes, *total_bytes)
            )
        }
        ModelLifecycleStage::Verifying { total_bytes } => {
            format!("verifying {model_id} ({total_bytes} bytes)")
        }
        ModelLifecycleStage::Benchmark {
            first_token_ms,
            tokens_per_sec,
        } => {
            format!(
                "benchmark {model_id}: first token {first_token_ms} ms, {tokens_per_sec:.1} tok/s"
            )
        }
        ModelLifecycleStage::AwaitingDecision { reason } => {
            format!("local model {model_id} awaiting your decision: {reason}")
        }
        ModelLifecycleStage::Ready => format!("local model {model_id} ready"),
        ModelLifecycleStage::SteppedDown {
            from_model,
            to_model,
            reason,
        } => format!("stepped down {from_model} → {to_model}: {reason}"),
        ModelLifecycleStage::Disabled { reason } => format!("local tier disabled: {reason}"),
    };
    surface.line(LineKind::Notice, &text);
}

/// A textual progress bar plus a `downloaded / total` byte readout. When the
/// total length is unknown, shows an indeterminate bar with the running count.
#[must_use]
pub fn progress_bar(downloaded: u64, total: Option<u64>) -> String {
    match total {
        Some(total) if total > 0 => {
            let filled = ((u128::from(downloaded) * BAR_WIDTH as u128) / u128::from(total))
                .min(BAR_WIDTH as u128) as usize;
            let percent = ((u128::from(downloaded) * 100) / u128::from(total)).min(100);
            let bar: String = "#".repeat(filled) + &".".repeat(BAR_WIDTH - filled);
            format!(
                "[{bar}] {percent:>3}%  ({} / {})",
                format_bytes(downloaded),
                format_bytes(total)
            )
        }
        _ => {
            let bar = ".".repeat(BAR_WIDTH);
            format!("[{bar}]   ?%  ({} / ?)", format_bytes(downloaded))
        }
    }
}

/// Human-readable byte size with one decimal place, in binary units.
///
/// Re-exported from `teton-protocol` so the client renders bytes with the same
/// `GiB`/`MiB`/`KiB` labels the daemon's own sentences use — a single formatter
/// rather than a hand-copied one that once printed `GB` for the same 1024-based
/// number the daemon called `GiB`.
pub use teton_protocol::format_bytes;

/// Render a `model_selection_proposed` event in full (BR-2).
///
/// Every element the rule names is shown — the detected hardware, the band and
/// the sentence explaining it, the proposed model with its download size and RAM
/// floor, and every alternative the user may pick instead — because legibility
/// *is* the consent: a user cannot meaningfully accept a multi-gigabyte download
/// they were shown only the name of.
pub fn render_proposal(proposal: &ModelSelectionProposed, surface: &mut dyn Surface) {
    surface.line(
        LineKind::Prompt,
        "a local model is proposed for this machine:",
    );
    render_probe(&proposal.probe, surface);

    match &proposal.proposed {
        Some(proposed) => {
            let entry = &proposed.entry;
            surface.line(
                LineKind::Info,
                &format!(
                    "  proposed: {} [{}] — {} download, needs {} RAM{}, {} free disk to install",
                    entry.name,
                    tier_label(entry.band),
                    format_bytes(entry.size_bytes),
                    format_bytes(entry.ram_floor_bytes),
                    // E-1: the alternatives carry this flag and the *proposed*
                    // entry did not — yet the proposed entry is the one a single
                    // Enter installs. A `[local_model] pinned` key overrides the
                    // probe unconditionally, so the proposal can name an entry
                    // this machine cannot hold; the line that says so must be the
                    // line the user is answering.
                    above_ram_flag(entry, proposal.probe.total_ram_bytes),
                    format_bytes(proposed.required_disk_bytes),
                ),
            );
            surface.line(
                LineKind::Info,
                &format!("  source:   {}", provenance_line(entry)),
            );
        }
        None => surface.line(
            LineKind::Info,
            "  proposed: none — no catalog entry fits this machine; you may still pick one below.",
        ),
    }

    render_alternatives(
        &proposal.alternatives,
        proposal.probe.total_ram_bytes,
        surface,
    );
    render_fetch_notice(proposal.fetch_notice.as_ref(), surface);
    surface.line(
        LineKind::Notice,
        "nothing is downloaded until you answer (BR-1).",
    );
}

/// The provenance of a catalog entry as one line: who published it, from what
/// host, at which commit (H-2). Legibility *is* consent — a user approving a
/// multi-gigabyte transfer is entitled to see where the bytes come from, not just
/// the model's name.
#[must_use]
pub fn provenance_line(entry: &CatalogEntryView) -> String {
    let p = &entry.provenance;
    format!("{} on {} @ {}", p.repo, p.host, p.revision)
}

/// Surface a redirected fetch before the user answers (H-2).
///
/// When a `[local_model] base_url` mirror or a non-bundled catalog is in force,
/// the bytes do not come from the provenance host each entry shows — and a
/// redirect the user cannot see is exactly where consent means least. So it is
/// stated plainly, as a warning, not buried.
pub fn render_fetch_notice(notice: Option<&FetchNotice>, surface: &mut dyn Surface) {
    let Some(notice) = notice else {
        return;
    };
    match &notice.mirror_host {
        Some(host) => surface.line(
            LineKind::Notice,
            &format!(
                "WARNING: downloading from a configured mirror ({host}), not huggingface.co — \
                 the pinned artifact and its checksum are unchanged, but the bytes come from \
                 this host."
            ),
        ),
        // A `base_url` was set but named no parseable host: the fetch is still
        // redirected, so say so rather than stay silent.
        None if !notice.override_catalog => surface.line(
            LineKind::Notice,
            "WARNING: the model fetch is redirected to a configured mirror, not huggingface.co.",
        ),
        None => {}
    }
    if notice.override_catalog {
        surface.line(
            LineKind::Notice,
            "WARNING: these entries come from an override catalog (TETON_CATALOG), not the \
             catalog this build shipped with.",
        );
    }
}

/// Render the probe's reasoning: the hardware it measured, the band it chose,
/// and the plain-language sentence explaining the choice (BR-2).
///
/// Shared by the proposal and by `teton model list`, so the machine is described
/// the same way wherever it is described.
pub fn render_probe(probe: &ProbeReportView, surface: &mut dyn Surface) {
    surface.line(
        LineKind::Info,
        &format!(
            "  hardware: {} RAM, {} free disk, {} acceleration",
            format_bytes(probe.total_ram_bytes),
            format_bytes(probe.free_disk_bytes),
            gpu_label(probe.gpu_class),
        ),
    );
    surface.line(
        LineKind::Info,
        &format!(
            "  band:     {} — {}",
            band_label(probe.chosen_band),
            probe.reason
        ),
    );
}

/// Render the numbered list of entries the user may choose instead (BR-3).
pub fn render_alternatives(
    alternatives: &[CatalogEntryView],
    total_ram_bytes: u64,
    surface: &mut dyn Surface,
) {
    if alternatives.is_empty() {
        surface.line(
            LineKind::Info,
            "  alternatives: none — the catalog offers nothing else.",
        );
        return;
    }
    surface.line(LineKind::Info, "  alternatives:");
    for (index, entry) in alternatives.iter().enumerate() {
        surface.line(
            LineKind::Info,
            &format!(
                "    {}. {}",
                index + 1,
                entry_summary(entry, total_ram_bytes)
            ),
        );
    }
}

/// The above-the-floor annotation for `entry` on this machine, or `""`.
///
/// One function so the proposed entry and every alternative are flagged by the
/// same rule — the proposed line used to omit it, which is the one line a single
/// keystroke acts on (E-1).
#[must_use]
fn above_ram_flag(entry: &CatalogEntryView, total_ram_bytes: u64) -> &'static str {
    if entry.ram_floor_bytes > total_ram_bytes {
        " — ABOVE this machine's RAM"
    } else {
        ""
    }
}

/// One catalog entry as a single line, annotated with its fit for this machine
/// and with **who published it**.
///
/// An entry above the machine's RAM is shown rather than hidden (BR-3: the user's
/// machine is the user's call) but is labelled so the extra confirmation it needs
/// is never a surprise.
///
/// The publisher is here for the same reason it is on the proposed line (H-2 /
/// E-7): this is the text the override menu is built from, so choosing entry `2`
/// approves a multi-gigabyte transfer from whoever quantized it. Naming the
/// source only for the daemon's own pick left the *deliberate* choice — the one
/// the user typed a number for — as the blind one.
#[must_use]
pub fn entry_summary(entry: &CatalogEntryView, total_ram_bytes: u64) -> String {
    format!(
        "{} [{}] — {} download, needs {} RAM{} — from {}",
        entry.name,
        tier_label(entry.band),
        format_bytes(entry.size_bytes),
        format_bytes(entry.ram_floor_bytes),
        above_ram_flag(entry, total_ram_bytes),
        provenance_line(entry),
    )
}

/// One-line summary of a recorded decision (`model_selection_decided`).
#[must_use]
pub fn format_decided(decided: &ModelSelectionDecided) -> String {
    let source = source_label(decided.source);
    if decided.declined_local {
        return format!("local tier declined ({source}) — sessions run remote-only.");
    }
    match &decided.model_name {
        Some(name) => format!("local model {name} selected ({source})."),
        // `model_name` is `None` exactly when declined, so this is a daemon that
        // sent neither — say so plainly rather than inventing a model.
        None => format!("model selection recorded with no model ({source})."),
    }
}

/// Wire-name label for a detected accelerator class.
#[must_use]
pub fn gpu_label(class: GpuClass) -> &'static str {
    match class {
        GpuClass::AppleSilicon => "apple-silicon",
        GpuClass::Cuda => "cuda",
        GpuClass::Cpu => "cpu",
    }
}

/// Wire-name label for the band the probe chose for this machine.
#[must_use]
pub fn band_label(band: ChosenBand) -> &'static str {
    match band {
        ChosenBand::None => "none (below the local-tier floor)",
        ChosenBand::Small => "small",
        ChosenBand::Mid => "mid",
        ChosenBand::Large => "large",
    }
}

/// Wire-name label for the band a catalog entry serves.
#[must_use]
pub fn tier_label(band: TierBand) -> &'static str {
    match band {
        TierBand::Small => "small",
        TierBand::Mid => "mid",
        TierBand::Large => "large",
    }
}

/// Wire-name label for where a decision came from.
#[must_use]
pub fn source_label(source: SelectionSource) -> &'static str {
    match source {
        SelectionSource::Probe => "accepted the proposal",
        SelectionSource::UserOverride => "user override",
        SelectionSource::ConfigPin => "config pin",
        SelectionSource::AutoAccept => "auto-accept",
    }
}

/// Ask the user to confirm the proposed model (BR-1's explicit decision).
///
/// Written for REQ-544 and left unwired until REQ-547 gave the daemon a consent
/// hook to answer; [`crate::model_ui::resolve_proposal`] now drives it. An empty
/// answer or a leading `y` means yes; EOF means no — and a "no" here opens the
/// override menu rather than declining the local tier, so backing out of the
/// default can never be misread as declining local inference altogether.
#[must_use]
pub fn confirm_model(model_id: &str, size: Option<u64>, prompter: &mut dyn Prompter) -> bool {
    let size_hint = size.map_or_else(String::new, |b| format!(" ({})", format_bytes(b)));
    let question = format!("Download local model {model_id}{size_hint}? [Y/n] ");
    match prompter.ask(&question) {
        Some(answer) => {
            let a = answer.trim().to_lowercase();
            a.is_empty() || a.starts_with('y')
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_ui::testing::{entry, proposal};
    use crate::prompt::ScriptedPrompter;
    use crate::render::{LineKind, RecordingSurface};

    #[test]
    fn format_bytes_scales_units() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1_572_864), "1.5 MiB");
        assert_eq!(format_bytes(16 * 1024 * 1024 * 1024), "16.0 GiB");
    }

    #[test]
    fn progress_bar_tracks_percent_and_clamps() {
        let half = progress_bar(500, Some(1000));
        assert!(half.contains("50%"));
        assert!(half.contains("############............")); // 12 of 24 filled
                                                            // Over-100% input is clamped rather than overflowing the bar.
        let over = progress_bar(2000, Some(1000));
        assert!(over.contains("100%"));
        // Unknown total → indeterminate bar with a running count.
        let unknown = progress_bar(300, None);
        assert!(unknown.contains("?%"));
        assert!(unknown.contains("300 B"));
    }

    #[test]
    fn render_lifecycle_covers_every_stage() {
        let stages = [
            ModelLifecycleStage::Probed {
                ram_bytes: 16 * 1024 * 1024 * 1024,
                above_floor: true,
            },
            ModelLifecycleStage::AwaitingDecision {
                reason: "nothing is downloaded until you answer".to_owned(),
            },
            ModelLifecycleStage::Download {
                downloaded_bytes: 250,
                total_bytes: Some(1000),
            },
            ModelLifecycleStage::Verifying { total_bytes: 1000 },
            ModelLifecycleStage::Benchmark {
                first_token_ms: 250,
                tokens_per_sec: 42.5,
            },
            ModelLifecycleStage::Ready,
            ModelLifecycleStage::SteppedDown {
                from_model: "7b".to_owned(),
                to_model: "3b".to_owned(),
                reason: "benchmark exceeded the 1s latency duty".to_owned(),
            },
            ModelLifecycleStage::Disabled {
                reason: "machine below the 8GB floor".to_owned(),
            },
        ];
        let mut surface = RecordingSurface::new();
        for stage in &stages {
            render_lifecycle("qwen2.5-coder-3b", stage, &mut surface);
        }
        let notices = surface.lines_of(LineKind::Notice);
        assert_eq!(notices.len(), stages.len());
        assert!(surface.any_line_contains(LineKind::Notice, "probe:"));
        assert!(surface.any_line_contains(LineKind::Notice, "16.0 GiB"));
        assert!(surface.any_line_contains(LineKind::Notice, "awaiting your decision"));
        assert!(surface.any_line_contains(LineKind::Notice, "download"));
        assert!(surface.any_line_contains(LineKind::Notice, "verifying"));
        assert!(surface.any_line_contains(LineKind::Notice, "first token 250 ms"));
        assert!(surface.any_line_contains(LineKind::Notice, "ready"));
        assert!(surface.any_line_contains(LineKind::Notice, "stepped down 7b → 3b"));
        assert!(surface.any_line_contains(LineKind::Notice, "disabled"));
    }

    /// BR-2 in full: every element the rule names must be on screen. Asserted
    /// against a scripted proposal event through the rendering seam — the point
    /// is that a person can see *why* this model, not that a string was printed.
    #[test]
    fn render_proposal_shows_every_br2_element() {
        let mut surface = RecordingSurface::new();
        render_proposal(&proposal(), &mut surface);
        let text = surface.lines_of(LineKind::Info).join("\n");

        // Detected hardware: RAM, free disk, GPU class.
        assert!(text.contains("16.0 GiB"), "detected RAM missing: {text}");
        assert!(text.contains("120.0 GiB"), "free disk missing: {text}");
        assert!(text.contains("apple-silicon"), "gpu class missing: {text}");
        // The chosen band and the plain-language reason.
        assert!(
            text.contains("band:     mid"),
            "chosen band missing: {text}"
        );
        assert!(
            text.contains("16 GiB of RAM clears the mid band's floor"),
            "reason missing: {text}"
        );
        // The proposed model with its download size and RAM floor.
        assert!(
            text.contains("qwen2.5-coder-7b"),
            "proposed model missing: {text}"
        );
        assert!(text.contains("4.4 GiB"), "download size missing: {text}");
        assert!(
            text.contains("needs 8.0 GiB RAM"),
            "RAM floor missing: {text}"
        );
        // Every selectable alternative, numbered, with its own fit.
        assert!(
            text.contains("1. qwen2.5-coder-3b"),
            "alternative 1 missing: {text}"
        );
        assert!(
            text.contains("2. qwen3-coder-30b"),
            "alternative 2 missing: {text}"
        );
        assert!(
            text.contains("ABOVE this machine's RAM"),
            "an over-sized alternative must be labelled, not hidden: {text}"
        );
        // And the BR-1 promise itself.
        assert!(surface.any_line_contains(LineKind::Notice, "nothing is downloaded"));
    }

    /// H-2: the proposal must show *where the bytes come from* — publisher/repo,
    /// host, and the short revision — not only the model name.
    #[test]
    fn render_proposal_shows_the_source_provenance() {
        let mut surface = RecordingSurface::new();
        render_proposal(&proposal(), &mut surface);
        let text = surface.lines_of(LineKind::Info).join("\n");
        assert!(text.contains("source:"), "no source line: {text}");
        // The proposed 7B entry's synthesized provenance.
        assert!(
            text.contains("Qwen/qwen2.5-coder-7b-GGUF"),
            "publisher/repo missing: {text}"
        );
        assert!(text.contains("huggingface.co"), "host missing: {text}");
        assert!(text.contains("f74adce"), "revision missing: {text}");
        // Legibility, not a URL: no scheme rides the rendered proposal.
        assert!(!text.contains("://"), "a URL was rendered: {text}");
    }

    #[test]
    fn a_mirror_notice_warns_that_the_fetch_is_redirected() {
        // H-2: a redirected fetch the user cannot otherwise see must be surfaced.
        let mut proposal = proposal();
        proposal.fetch_notice = Some(FetchNotice {
            mirror_host: Some("hf-mirror.corp.internal".to_owned()),
            override_catalog: false,
        });
        let mut surface = RecordingSurface::new();
        render_proposal(&proposal, &mut surface);
        assert!(surface.any_line_contains(LineKind::Notice, "configured mirror"));
        assert!(surface.any_line_contains(LineKind::Notice, "hf-mirror.corp.internal"));
        assert!(surface.any_line_contains(LineKind::Notice, "not huggingface.co"));
    }

    #[test]
    fn an_override_catalog_notice_says_it_is_not_the_shipped_catalog() {
        let mut proposal = proposal();
        proposal.fetch_notice = Some(FetchNotice {
            mirror_host: None,
            override_catalog: true,
        });
        let mut surface = RecordingSurface::new();
        render_proposal(&proposal, &mut surface);
        assert!(surface.any_line_contains(LineKind::Notice, "override catalog"));
        assert!(surface.any_line_contains(LineKind::Notice, "TETON_CATALOG"));
    }

    #[test]
    fn no_fetch_notice_renders_no_redirect_warning() {
        let mut surface = RecordingSurface::new();
        render_proposal(&proposal(), &mut surface);
        assert!(!surface.any_line_contains(LineKind::Notice, "mirror"));
        assert!(!surface.any_line_contains(LineKind::Notice, "override catalog"));
    }

    #[test]
    fn render_proposal_without_a_fitting_entry_says_so_and_still_offers_alternatives() {
        let mut proposal = proposal();
        proposal.proposed = None;
        proposal.probe.chosen_band = ChosenBand::None;
        let mut surface = RecordingSurface::new();
        render_proposal(&proposal, &mut surface);
        let text = surface.lines_of(LineKind::Info).join("\n");
        assert!(text.contains("proposed: none"), "{text}");
        assert!(text.contains("below the local-tier floor"), "{text}");
        assert!(text.contains("1. qwen2.5-coder-3b"), "{text}");
    }

    #[test]
    fn render_proposal_with_no_alternatives_says_so_rather_than_rendering_an_empty_list() {
        let mut proposal = proposal();
        proposal.alternatives.clear();
        let mut surface = RecordingSurface::new();
        render_proposal(&proposal, &mut surface);
        assert!(surface.any_line_contains(LineKind::Info, "alternatives: none"));
    }

    #[test]
    fn entry_summary_flags_only_entries_above_this_machines_ram() {
        let fits = entry(
            "qwen2.5-coder-3b",
            TierBand::Small,
            2_104_932_800,
            5_368_709_120,
        );
        let over = entry(
            "qwen3-coder-30b",
            TierBand::Large,
            18_000_000_000,
            34_359_738_368,
        );
        let ram = 16 * 1024 * 1024 * 1024;
        assert!(!entry_summary(&fits, ram).contains("ABOVE"));
        assert!(entry_summary(&over, ram).contains("ABOVE this machine's RAM"));
    }

    /// H-2 / E-7: the override menu is built from these lines, so choosing an
    /// alternative approves a multi-gigabyte transfer from whoever published it.
    /// Naming the source only on the proposed line left the *deliberate* choice
    /// as the blind one.
    #[test]
    fn every_alternative_names_its_publisher_not_only_the_proposed_entry() {
        let mut surface = RecordingSurface::new();
        render_proposal(&proposal(), &mut surface);
        let text = surface.lines_of(LineKind::Info).join("\n");

        // The over-sized 30B alternative — the one an override menu makes it
        // easiest to pick by accident — names its repo on its own line.
        let line = text
            .lines()
            .find(|line| line.contains("qwen3-coder-30b"))
            .unwrap_or_else(|| panic!("the over-sized alternative is not rendered: {text}"));
        assert!(
            line.contains("Qwen/qwen3-coder-30b-GGUF"),
            "the alternative does not say who published it: {line}"
        );
        assert!(
            line.contains("huggingface.co"),
            "the alternative does not say what host it comes from: {line}"
        );
        // Still legibility, not a URL (BR-11).
        assert!(!text.contains("://"), "a URL was rendered: {text}");
    }

    /// E-1: the proposed line is the one a single Enter installs, so it carries
    /// the same above-the-floor flag its alternatives do.
    ///
    /// A `[local_model] pinned` key overrides the probe unconditionally, so the
    /// proposal can name an entry this machine cannot hold — and the annotation
    /// used to appear on every line *except* that one.
    #[test]
    fn a_proposed_entry_above_this_machines_ram_is_flagged_on_its_own_line() {
        let mut pinned = proposal();
        // The pinned-oversized case: the 30B entry, proposed.
        pinned.proposed = Some(teton_protocol::events::ProposedModel {
            entry: crate::model_ui::testing::oversized_entry(),
            required_disk_bytes: 19_000_000_000,
        });
        let mut surface = RecordingSurface::new();
        render_proposal(&pinned, &mut surface);
        let text = surface.lines_of(LineKind::Info).join("\n");
        let line = text
            .lines()
            .find(|line| line.contains("proposed:"))
            .unwrap_or_else(|| panic!("no proposed line: {text}"));
        assert!(
            line.contains("ABOVE this machine's RAM"),
            "the proposed entry needs more RAM than this machine has and the line \
             the user answers must say so: {line}"
        );

        // And the ordinary case is not decorated with a warning it has not earned.
        let mut surface = RecordingSurface::new();
        render_proposal(&proposal(), &mut surface);
        let text = surface.lines_of(LineKind::Info).join("\n");
        let line = text
            .lines()
            .find(|line| line.contains("proposed:"))
            .expect("no proposed line");
        assert!(!line.contains("ABOVE"), "{line}");
    }

    #[test]
    fn format_decided_covers_choice_and_decline() {
        let chosen = format_decided(&ModelSelectionDecided {
            request_id: None,
            model_name: Some("qwen2.5-coder-7b".to_owned()),
            declined_local: false,
            source: SelectionSource::Probe,
        });
        assert!(chosen.contains("qwen2.5-coder-7b"), "{chosen}");
        assert!(chosen.contains("accepted the proposal"), "{chosen}");

        let declined = format_decided(&ModelSelectionDecided {
            request_id: None,
            model_name: None,
            declined_local: true,
            source: SelectionSource::UserOverride,
        });
        assert!(declined.contains("declined"), "{declined}");
        assert!(declined.contains("remote-only"), "{declined}");

        let auto = format_decided(&ModelSelectionDecided {
            request_id: None,
            model_name: Some("qwen2.5-coder-1.5b".to_owned()),
            declined_local: false,
            source: SelectionSource::AutoAccept,
        });
        assert!(auto.contains("auto-accept"), "{auto}");
    }

    #[test]
    fn confirm_model_defaults_to_yes_on_empty_and_no_on_eof() {
        let mut yes = ScriptedPrompter::new(&[""]);
        assert!(confirm_model("qwen", Some(2_000_000_000), &mut yes));

        let mut explicit_no = ScriptedPrompter::new(&["n"]);
        assert!(!confirm_model("qwen", None, &mut explicit_no));

        let mut eof = ScriptedPrompter::new(&[]);
        assert!(!confirm_model("qwen", None, &mut eof));
    }
}
