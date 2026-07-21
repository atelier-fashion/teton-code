//! The zero-config first-run experience (AC-1, AC-8 visibility).
//!
//! On a fresh machine the daemon runs the local-model lifecycle — hardware
//! probe, model download, post-download micro-benchmark, ready/step-down — and
//! broadcasts each step as a `model_lifecycle` event (BR-9). The CLI's whole job
//! here is to make that legible: render the probe summary, a live download
//! progress bar, and the benchmark result, so the user watches the machine set
//! itself up rather than staring at a silent prompt.
//!
//! The rendering is a pure function of the [`ModelLifecycleStage`], so it is
//! table-tested against scripted stages with no daemon and no model.

use teton_protocol::events::ModelLifecycleStage;

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
        ModelLifecycleStage::Benchmark {
            first_token_ms,
            tokens_per_sec,
        } => {
            format!(
                "benchmark {model_id}: first token {first_token_ms} ms, {tokens_per_sec:.1} tok/s"
            )
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

/// Human-readable byte size with one decimal place (binary units).
#[must_use]
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Ask the user to confirm a proposed model download. Zero-config first run
/// (AC-1) auto-proceeds without a gate, so this backs an opt-in confirm flow
/// that activates once a daemon download-consent hook exists; it is exercised by
/// tests today. An empty answer or a leading `y` means yes; EOF means no.
#[cfg_attr(not(test), allow(dead_code))]
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
    use crate::prompt::ScriptedPrompter;
    use crate::render::{LineKind, RecordingSurface};

    #[test]
    fn format_bytes_scales_units() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1_572_864), "1.5 MB");
        assert_eq!(format_bytes(16 * 1024 * 1024 * 1024), "16.0 GB");
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
            ModelLifecycleStage::Download {
                downloaded_bytes: 250,
                total_bytes: Some(1000),
            },
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
        assert!(surface.any_line_contains(LineKind::Notice, "16.0 GB"));
        assert!(surface.any_line_contains(LineKind::Notice, "download"));
        assert!(surface.any_line_contains(LineKind::Notice, "first token 250 ms"));
        assert!(surface.any_line_contains(LineKind::Notice, "ready"));
        assert!(surface.any_line_contains(LineKind::Notice, "stepped down 7b → 3b"));
        assert!(surface.any_line_contains(LineKind::Notice, "disabled"));
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
