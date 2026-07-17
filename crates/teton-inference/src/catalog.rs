//! The versioned local-model catalog.
//!
//! Per the task's Technical Notes, the catalog is **data, not code**: a
//! versioned TOML document mapping a model name to its GGUF URL, expected
//! SHA-256, download size, minimum RAM, and the OQ-3 hardware band it serves.
//! The daemon can drop in a newer catalog (bumping `version`) without a
//! `teton-inference` release. A default catalog is embedded in the binary via
//! [`Catalog::bundled`].
//!
//! The catalog also encodes the model *ordering* used for step-down: models are
//! ranked by `ram_floor_bytes` (ties broken by `size_bytes`), and both the probe
//! (`probe::decide`) and the benchmark/pressure step-down walk that order.

use serde::{Deserialize, Serialize};

use crate::probe::HardwareProfile;

/// The default catalog shipped in the binary. Kept in a data file so it reads as
/// data; validated by the unit tests below.
const BUNDLED_TOML: &str = include_str!("../data/models.toml");

/// The OQ-3 hardware band a model targets. Ordered smallest-to-largest so `<`
/// and `<=` express "no bigger than this band".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TierBand {
    /// 1.5B-3B class, for 8-16 GiB machines.
    Small,
    /// 7B class, for 16-32 GiB machines.
    Mid,
    /// 30B-A3B class, for 32 GiB+ machines (optional).
    Large,
}

/// One catalog entry.
///
/// `size_bytes` and `ram_floor_bytes` are what the probe and disk checks read;
/// `sha256` is the integrity check the downloader verifies. Not [`Eq`] only
/// because there is no reason to constrain future numeric fields; [`PartialEq`]
/// is enough for tests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelEntry {
    /// Catalog id, e.g. `qwen2.5-coder-3b`.
    pub name: String,
    /// GGUF download URL.
    pub url: String,
    /// Expected lowercase-hex SHA-256 of the downloaded file.
    pub sha256: String,
    /// Download size in bytes.
    pub size_bytes: u64,
    /// Minimum system RAM required to load the model, in bytes. This is a
    /// conservative *floor* that deliberately leaves headroom for the user's
    /// work (BR-8/BR-9: never degrade the machine), not the raw weight size.
    pub ram_floor_bytes: u64,
    /// The hardware band this model serves.
    pub band: TierBand,
}

impl ModelEntry {
    /// Does this model fit `profile` — enough RAM to load it and enough free
    /// disk to hold the download?
    #[must_use]
    pub fn fits(&self, profile: &HardwareProfile) -> bool {
        self.ram_floor_bytes <= profile.ram_bytes && self.size_bytes <= profile.free_disk_bytes
    }
}

/// A versioned set of model entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalog {
    /// Monotonic catalog version; lets the daemon detect a newer catalog.
    pub version: u32,
    /// The models, in author order.
    #[serde(default)]
    pub models: Vec<ModelEntry>,
}

impl Catalog {
    /// The default catalog embedded in the binary.
    ///
    /// # Panics
    /// Panics only if the in-repo `data/models.toml` is malformed — a build-time
    /// bug caught by the crate's own tests, never a runtime condition.
    #[must_use]
    pub fn bundled() -> Self {
        Self::from_toml(BUNDLED_TOML).expect("bundled model catalog must parse")
    }

    /// Parse a catalog from a TOML document.
    ///
    /// # Errors
    /// Returns the underlying TOML deserialization error on malformed input.
    pub fn from_toml(input: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(input)
    }

    /// The entry named `name`, if present.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&ModelEntry> {
        self.models.iter().find(|m| m.name == name)
    }

    /// The next smaller model that still fits `profile`, or `None` if `name` is
    /// already the smallest fitting model (or is absent).
    ///
    /// "Smaller" is strictly-lower `ram_floor_bytes`; the result is the *largest*
    /// such model that fits. Because each call returns a strictly-smaller model
    /// and the catalog is finite, repeatedly stepping down always terminates.
    #[must_use]
    pub fn step_down_from(&self, name: &str, profile: &HardwareProfile) -> Option<&ModelEntry> {
        let current = self.get(name)?;
        self.models
            .iter()
            .filter(|m| m.ram_floor_bytes < current.ram_floor_bytes && m.fits(profile))
            .max_by_key(|m| (m.ram_floor_bytes, m.size_bytes))
    }

    /// All models strictly smaller than `base` that fit `profile`, largest
    /// first. This is the ordered downgrade chain the pressure watcher walks.
    #[must_use]
    pub fn models_smaller_than(&self, base: &str, profile: &HardwareProfile) -> Vec<String> {
        let Some(base_entry) = self.get(base) else {
            return Vec::new();
        };
        let mut smaller: Vec<&ModelEntry> = self
            .models
            .iter()
            .filter(|m| m.ram_floor_bytes < base_entry.ram_floor_bytes && m.fits(profile))
            .collect();
        smaller.sort_by_key(|m| std::cmp::Reverse((m.ram_floor_bytes, m.size_bytes)));
        smaller.into_iter().map(|m| m.name.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{GpuClass, HardwareProfile};

    fn big_machine() -> HardwareProfile {
        HardwareProfile {
            ram_bytes: 64 * 1024 * 1024 * 1024,
            free_disk_bytes: 500 * 1_000_000_000,
            gpu: GpuClass::AppleSilicon,
        }
    }

    #[test]
    fn bundled_catalog_parses_and_has_the_expected_models() {
        let catalog = Catalog::bundled();
        assert_eq!(catalog.version, 1);
        let names: Vec<&str> = catalog.models.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "qwen2.5-coder-1.5b",
                "qwen2.5-coder-3b",
                "qwen2.5-coder-7b",
                "qwen3-coder-30b-a3b",
            ]
        );
    }

    #[test]
    fn bundled_shas_are_well_formed_hex() {
        for model in Catalog::bundled().models {
            assert_eq!(model.sha256.len(), 64, "{}", model.name);
            assert!(
                model.sha256.chars().all(|c| c.is_ascii_hexdigit()),
                "{} sha is not hex",
                model.name
            );
        }
    }

    #[test]
    fn bands_ascend_with_ram_floor() {
        assert!(TierBand::Small < TierBand::Mid);
        assert!(TierBand::Mid < TierBand::Large);
        // In the bundled catalog, larger bands carry larger RAM floors.
        let catalog = Catalog::bundled();
        let small = catalog.get("qwen2.5-coder-3b").unwrap();
        let mid = catalog.get("qwen2.5-coder-7b").unwrap();
        assert!(small.ram_floor_bytes < mid.ram_floor_bytes);
        assert_eq!(small.band, TierBand::Small);
        assert_eq!(mid.band, TierBand::Mid);
    }

    #[test]
    fn step_down_walks_strictly_smaller_and_terminates() {
        let catalog = Catalog::bundled();
        let profile = big_machine();
        // 7b -> 3b -> 1.5b -> None
        let a = catalog
            .step_down_from("qwen2.5-coder-7b", &profile)
            .unwrap();
        assert_eq!(a.name, "qwen2.5-coder-3b");
        let b = catalog.step_down_from(&a.name, &profile).unwrap();
        assert_eq!(b.name, "qwen2.5-coder-1.5b");
        assert!(catalog.step_down_from(&b.name, &profile).is_none());
    }

    #[test]
    fn step_down_skips_models_that_do_not_fit_disk() {
        let catalog = Catalog::bundled();
        // Enough RAM for anything, but only ~1.5 GB free disk: from 7b the only
        // smaller model that fits on disk is the 1.5b (~1.1 GB), not the 3b (~2 GB).
        let profile = HardwareProfile {
            ram_bytes: 64 * 1024 * 1024 * 1024,
            free_disk_bytes: 1_500_000_000,
            gpu: GpuClass::Cpu,
        };
        let next = catalog
            .step_down_from("qwen2.5-coder-7b", &profile)
            .unwrap();
        assert_eq!(next.name, "qwen2.5-coder-1.5b");
    }

    #[test]
    fn downgrade_chain_is_largest_first() {
        let catalog = Catalog::bundled();
        let chain = catalog.models_smaller_than("qwen2.5-coder-7b", &big_machine());
        assert_eq!(chain, ["qwen2.5-coder-3b", "qwen2.5-coder-1.5b"]);
        // Smallest model has an empty chain.
        assert!(catalog
            .models_smaller_than("qwen2.5-coder-1.5b", &big_machine())
            .is_empty());
    }

    #[test]
    fn unknown_model_has_no_step_down_and_no_chain() {
        let catalog = Catalog::bundled();
        assert!(catalog.step_down_from("ghost", &big_machine()).is_none());
        assert!(catalog
            .models_smaller_than("ghost", &big_machine())
            .is_empty());
    }

    #[test]
    fn catalog_round_trips_through_toml() {
        let catalog = Catalog::bundled();
        let toml_text = toml::to_string(&catalog).expect("serialize");
        let back = Catalog::from_toml(&toml_text).expect("deserialize");
        assert_eq!(catalog, back);
    }
}
