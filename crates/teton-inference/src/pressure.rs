//! Runtime memory-pressure adaptation (BR-9).
//!
//! The daemon watches OS memory pressure and, rather than letting the resident
//! model force the machine into swap-thrash, it downgrades to a smaller model or
//! unloads the local tier entirely — reloading when pressure recovers. While the
//! tier is unloaded, inference requests get the typed
//! [`EngineError::Unavailable`](crate::engine::EngineError::Unavailable) signal
//! (the "local tier unavailable" string) so the router bypasses the local tier
//! and proceeds remote-only (BR-8).
//!
//! [`PressureController`] is a pure state machine fed [`MemoryPressure`] samples;
//! the real OS sampler (macOS `DISPATCH_MEMORYPRESSURE`, Linux PSI) lives in the
//! daemon and feeds it. All transitions emit `model_lifecycle`
//! [`LifecycleEvent`]s.

use crate::catalog::Catalog;
use crate::engine::EngineError;
use crate::lifecycle::LifecycleEvent;
use crate::probe::HardwareProfile;

/// OS memory-pressure level, ascending in severity.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum MemoryPressure {
    /// Ample memory; the base model can serve.
    Normal,
    /// Memory is tight; downgrade to a smaller model to relieve pressure.
    Warn,
    /// Memory is critically low; unload the local tier to avoid swap-thrash.
    Critical,
}

/// What the local tier is currently doing.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TierState {
    /// Serving the base (probe-selected) model.
    Serving,
    /// Serving a smaller model after a `Warn`-level downgrade.
    Downgraded(String),
    /// Unloaded; inference is unavailable until recovery.
    Unloaded(String),
}

/// A memory-pressure-aware controller over the local tier.
///
/// Constructed with the probe-selected `base_model` and the ordered downgrade
/// chain (largest-first) of smaller models that fit the machine.
#[derive(Debug, Clone)]
pub struct PressureController {
    base_model: String,
    downgrade_chain: Vec<String>,
    state: TierState,
}

impl PressureController {
    /// A controller serving `base_model`, with `downgrade_chain` (largest-first)
    /// as the models to fall back to under pressure.
    pub fn new(base_model: impl Into<String>, downgrade_chain: Vec<String>) -> Self {
        Self {
            base_model: base_model.into(),
            downgrade_chain,
            state: TierState::Serving,
        }
    }

    /// Build a controller for `base_model`, deriving the downgrade chain from
    /// `catalog` and `profile`.
    #[must_use]
    pub fn from_catalog(catalog: &Catalog, profile: &HardwareProfile, base_model: &str) -> Self {
        let chain = catalog.models_smaller_than(base_model, profile);
        Self::new(base_model, chain)
    }

    /// The model currently serving, or `None` when unloaded.
    #[must_use]
    pub fn current_model(&self) -> Option<&str> {
        match &self.state {
            TierState::Serving => Some(&self.base_model),
            TierState::Downgraded(model) => Some(model),
            TierState::Unloaded(_) => None,
        }
    }

    /// Whether the local tier can currently serve inference.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.current_model().is_some()
    }

    /// The currently-serving model, or the typed "local tier unavailable" error
    /// when unloaded — the signal the router keys on to bypass the local tier
    /// (BR-8).
    ///
    /// # Errors
    /// Returns [`EngineError::Unavailable`] while the tier is unloaded.
    pub fn ensure_available(&self) -> Result<&str, EngineError> {
        match &self.state {
            TierState::Unloaded(reason) => Err(EngineError::unavailable(reason.clone())),
            TierState::Serving => Ok(&self.base_model),
            TierState::Downgraded(model) => Ok(model),
        }
    }

    /// Feed a memory-pressure sample and return any resulting lifecycle events.
    pub fn observe(&mut self, level: MemoryPressure) -> Vec<LifecycleEvent> {
        match level {
            MemoryPressure::Critical => self
                .unload("critical memory pressure: unloaded the local tier to avoid swap-thrash"),
            MemoryPressure::Warn => self.relieve(),
            MemoryPressure::Normal => self.recover(),
        }
    }

    /// Unload the tier (idempotent).
    fn unload(&mut self, reason: &str) -> Vec<LifecycleEvent> {
        if matches!(self.state, TierState::Unloaded(_)) {
            return Vec::new();
        }
        self.state = TierState::Unloaded(reason.to_owned());
        vec![LifecycleEvent::Disabled {
            reason: reason.to_owned(),
        }]
    }

    /// Relieve pressure by downgrading one step; unload if already at the
    /// smallest model.
    fn relieve(&mut self) -> Vec<LifecycleEvent> {
        let Some(current) = self.current_model().map(str::to_owned) else {
            // Already unloaded; stay unloaded until recovery.
            return Vec::new();
        };
        match self.next_smaller(&current) {
            Some(next) => {
                self.state = TierState::Downgraded(next.clone());
                vec![LifecycleEvent::SteppedDown {
                    from_model: current,
                    to_model: next,
                    reason: "memory pressure: downgraded to a smaller local model".to_owned(),
                }]
            }
            None => self
                .unload("memory pressure and no smaller model available: unloaded the local tier"),
        }
    }

    /// Recover to the base model when pressure clears (idempotent).
    fn recover(&mut self) -> Vec<LifecycleEvent> {
        if matches!(self.state, TierState::Serving) {
            return Vec::new();
        }
        self.state = TierState::Serving;
        vec![LifecycleEvent::Ready {
            model_id: self.base_model.clone(),
        }]
    }

    /// The next model strictly smaller than `current` in the downgrade chain.
    fn next_smaller(&self, current: &str) -> Option<String> {
        if current == self.base_model {
            return self.downgrade_chain.first().cloned();
        }
        let idx = self.downgrade_chain.iter().position(|m| m == current)?;
        self.downgrade_chain.get(idx + 1).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;
    use crate::probe::{GpuClass, HardwareProfile};

    fn big_machine() -> HardwareProfile {
        HardwareProfile {
            ram_bytes: 64 * 1024 * 1024 * 1024,
            free_disk_bytes: 500 * 1_000_000_000,
            gpu: GpuClass::AppleSilicon,
        }
    }

    fn controller() -> PressureController {
        PressureController::from_catalog(&Catalog::bundled(), &big_machine(), "qwen2.5-coder-7b")
    }

    #[test]
    fn critical_pressure_unloads_and_emits_disabled() {
        let mut ctrl = controller();
        assert!(ctrl.is_available());
        let events = ctrl.observe(MemoryPressure::Critical);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], LifecycleEvent::Disabled { .. }));
        assert!(!ctrl.is_available());
    }

    #[test]
    fn inference_during_unload_returns_typed_unavailable() {
        let mut ctrl = controller();
        ctrl.observe(MemoryPressure::Critical);
        let err = ctrl.ensure_available().unwrap_err();
        match err {
            EngineError::Unavailable { reason } => assert!(reason.contains("memory pressure")),
            other => panic!("expected Unavailable, got {other:?}"),
        }
        assert!(ctrl
            .ensure_available()
            .unwrap_err()
            .to_string()
            .starts_with("local tier unavailable"));
    }

    #[test]
    fn recovery_reloads_the_base_model() {
        let mut ctrl = controller();
        ctrl.observe(MemoryPressure::Critical);
        assert!(!ctrl.is_available());
        let events = ctrl.observe(MemoryPressure::Normal);
        assert_eq!(events.len(), 1);
        match &events[0] {
            LifecycleEvent::Ready { model_id } => assert_eq!(model_id, "qwen2.5-coder-7b"),
            other => panic!("expected Ready, got {other:?}"),
        }
        assert_eq!(ctrl.ensure_available().unwrap(), "qwen2.5-coder-7b");
    }

    #[test]
    fn warn_downgrades_to_a_smaller_model() {
        let mut ctrl = controller();
        let events = ctrl.observe(MemoryPressure::Warn);
        assert_eq!(events.len(), 1);
        match &events[0] {
            LifecycleEvent::SteppedDown {
                from_model,
                to_model,
                ..
            } => {
                assert_eq!(from_model, "qwen2.5-coder-7b");
                assert_eq!(to_model, "qwen2.5-coder-3b");
            }
            other => panic!("expected SteppedDown, got {other:?}"),
        }
        assert_eq!(ctrl.current_model(), Some("qwen2.5-coder-3b"));
    }

    #[test]
    fn repeated_warn_walks_down_then_unloads_at_the_bottom() {
        let mut ctrl = controller();
        // 7b -> 3b
        ctrl.observe(MemoryPressure::Warn);
        assert_eq!(ctrl.current_model(), Some("qwen2.5-coder-3b"));
        // 3b -> 1.5b
        ctrl.observe(MemoryPressure::Warn);
        assert_eq!(ctrl.current_model(), Some("qwen2.5-coder-1.5b"));
        // 1.5b is the smallest: another warn unloads.
        let events = ctrl.observe(MemoryPressure::Warn);
        assert!(matches!(
            events.as_slice(),
            [LifecycleEvent::Disabled { .. }]
        ));
        assert!(!ctrl.is_available());
    }

    #[test]
    fn downgrade_then_recover_returns_to_base() {
        let mut ctrl = controller();
        ctrl.observe(MemoryPressure::Warn);
        assert_eq!(ctrl.current_model(), Some("qwen2.5-coder-3b"));
        let events = ctrl.observe(MemoryPressure::Normal);
        assert!(matches!(events.as_slice(), [LifecycleEvent::Ready { .. }]));
        assert_eq!(ctrl.current_model(), Some("qwen2.5-coder-7b"));
    }

    #[test]
    fn idempotent_transitions_emit_no_duplicate_events() {
        let mut ctrl = controller();
        // Already serving: Normal is a no-op.
        assert!(ctrl.observe(MemoryPressure::Normal).is_empty());
        // Two criticals in a row: only the first unloads.
        assert_eq!(ctrl.observe(MemoryPressure::Critical).len(), 1);
        assert!(ctrl.observe(MemoryPressure::Critical).is_empty());
    }

    #[test]
    fn smallest_base_model_unloads_directly_on_warn() {
        // A machine whose base is already the smallest model has an empty chain.
        let mut ctrl = PressureController::from_catalog(
            &Catalog::bundled(),
            &big_machine(),
            "qwen2.5-coder-1.5b",
        );
        let events = ctrl.observe(MemoryPressure::Warn);
        assert!(matches!(
            events.as_slice(),
            [LifecycleEvent::Disabled { .. }]
        ));
        assert!(!ctrl.is_available());
    }
}
