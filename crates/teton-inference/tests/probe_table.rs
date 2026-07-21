//! Table-driven probe tests over simulated hardware profiles (BR-9 / AC-8).
//!
//! Each row is a synthetic machine (RAM, free disk, GPU class) plus an optional
//! user pin, mapped to the model the probe must select — or to the disabled tier
//! for below-floor and resource-starved machines. This is the acceptance surface
//! for the OQ-3 decision table; it never touches real hardware.

use teton_inference::catalog::Catalog;
use teton_inference::probe::{decide, GpuClass, HardwareProfile, GIB};
use teton_inference::TierDecision;

/// Expected outcome for a table row.
#[derive(Debug)]
enum Expect {
    /// The probe selects this model.
    Model(&'static str),
    /// The local tier is disabled (remote-only).
    Disabled,
}

struct Row {
    name: &'static str,
    ram_gib: u64,
    disk_mb: u64,
    gpu: GpuClass,
    pin: Option<&'static str>,
    expect: Expect,
}

fn profile(ram_gib: u64, disk_mb: u64, gpu: GpuClass) -> HardwareProfile {
    HardwareProfile {
        // Use exact GiB so the 16/32 GiB boundary rows are unambiguous. Disk is
        // expressed in MB (10^6 bytes) so the catalog's download sizes — 1.5B
        // ~1100 MB, 3B ~2000 MB, 7B ~4700 MB — have unambiguous fit boundaries.
        ram_bytes: ram_gib * GIB,
        free_disk_bytes: disk_mb * 1_000_000,
        gpu,
    }
}

#[test]
fn probe_decision_table_matches_oq3() {
    let catalog = Catalog::bundled();

    let rows = [
        // --- Below the 8 GiB floor: disabled, sessions go remote-only (AC-8). ---
        Row {
            name: "4 GiB laptop",
            ram_gib: 4,
            disk_mb: 500_000,
            gpu: GpuClass::AppleSilicon,
            pin: None,
            expect: Expect::Disabled,
        },
        Row {
            name: "just under 8 GiB",
            ram_gib: 7,
            disk_mb: 500_000,
            gpu: GpuClass::Cpu,
            pin: None,
            expect: Expect::Disabled,
        },
        // --- Small band (8..=16 GiB) -> 1.5B-3B. ---
        Row {
            name: "8 GiB",
            ram_gib: 8,
            disk_mb: 500_000,
            gpu: GpuClass::AppleSilicon,
            pin: None,
            expect: Expect::Model("qwen2.5-coder-3b"),
        },
        Row {
            name: "16 GiB Apple Silicon (AC-8: selects <=3B)",
            ram_gib: 16,
            disk_mb: 500_000,
            gpu: GpuClass::AppleSilicon,
            pin: None,
            expect: Expect::Model("qwen2.5-coder-3b"),
        },
        // --- Mid band (16 < r <= 32 GiB) -> 7B. ---
        Row {
            name: "24 GiB",
            ram_gib: 24,
            disk_mb: 500_000,
            gpu: GpuClass::AppleSilicon,
            pin: None,
            expect: Expect::Model("qwen2.5-coder-7b"),
        },
        Row {
            name: "32 GiB (upper edge of mid)",
            ram_gib: 32,
            disk_mb: 500_000,
            gpu: GpuClass::AppleSilicon,
            pin: None,
            expect: Expect::Model("qwen2.5-coder-7b"),
        },
        // --- Large band (> 32 GiB) -> 30B-A3B (optional). ---
        Row {
            name: "48 GiB",
            ram_gib: 48,
            disk_mb: 500_000,
            gpu: GpuClass::AppleSilicon,
            pin: None,
            expect: Expect::Model("qwen3-coder-30b-a3b"),
        },
        Row {
            name: "64 GiB workstation",
            ram_gib: 64,
            disk_mb: 500_000,
            gpu: GpuClass::AppleSilicon,
            pin: None,
            expect: Expect::Model("qwen3-coder-30b-a3b"),
        },
        // --- Disk-constrained: step down within/below the band, or disable. ---
        Row {
            name: "32 GiB RAM but only 3000 MB free disk -> 3B fits, 7B does not",
            ram_gib: 32,
            disk_mb: 3000,
            gpu: GpuClass::AppleSilicon,
            pin: None,
            expect: Expect::Model("qwen2.5-coder-3b"),
        },
        Row {
            name: "32 GiB RAM but 1500 MB free disk -> only 1.5B (~1100 MB) fits",
            ram_gib: 32,
            disk_mb: 1500,
            gpu: GpuClass::AppleSilicon,
            pin: None,
            expect: Expect::Model("qwen2.5-coder-1.5b"),
        },
        Row {
            name: "32 GiB RAM but 1000 MB free disk -> nothing fits -> disabled",
            ram_gib: 32,
            disk_mb: 1000,
            gpu: GpuClass::AppleSilicon,
            pin: None,
            expect: Expect::Disabled,
        },
        // --- User pin overrides the probe (BR-9). ---
        Row {
            name: "16 GiB with a 7B pin -> honored despite the small band",
            ram_gib: 16,
            disk_mb: 500_000,
            gpu: GpuClass::AppleSilicon,
            pin: Some("qwen2.5-coder-7b"),
            expect: Expect::Model("qwen2.5-coder-7b"),
        },
        Row {
            name: "unknown pin -> ignored, probe decides",
            ram_gib: 24,
            disk_mb: 500_000,
            gpu: GpuClass::AppleSilicon,
            pin: Some("no-such-model"),
            expect: Expect::Model("qwen2.5-coder-7b"),
        },
    ];

    for row in rows {
        let decision = decide(
            &profile(row.ram_gib, row.disk_mb, row.gpu),
            &catalog,
            row.pin,
        );
        match row.expect {
            Expect::Model(expected) => {
                assert_eq!(
                    decision.model(),
                    Some(expected),
                    "row '{}': expected model {expected}, got {decision:?}",
                    row.name
                );
            }
            Expect::Disabled => {
                assert!(
                    decision.is_disabled(),
                    "row '{}': expected disabled tier, got {decision:?}",
                    row.name
                );
            }
        }
    }
}

#[test]
fn pinned_decision_is_flagged_as_pinned() {
    let catalog = Catalog::bundled();
    let decision = decide(
        &profile(16, 500_000, GpuClass::AppleSilicon),
        &catalog,
        Some("qwen2.5-coder-7b"),
    );
    assert!(matches!(
        decision,
        TierDecision::Selected { pinned: true, .. }
    ));
}
