//! Persistence for the recorded model-selection decision (REQ-547 D-4).
//!
//! The decision is **machine state, not project config**: "which model this
//! machine installed" is not a property of a repository, so it lives in the
//! daemon's state directory beside the weights rather than in the user-authored
//! TOML (which holds only the *inputs* — the pin, the auto-accept opt-in, the
//! base-URL override).
//!
//! Persisting it is what turns BR-10's "a recorded decision is not re-litigated"
//! into a state read instead of a prompt, and what makes BR-4's "declining is
//! remembered across daemon starts" true.
//!
//! The record is deliberately small: [`ModelSelection`] carries no install path
//! (BR-11), so this file can never become a way for an absolute path to reach a
//! protocol payload — the projection has no field to leak.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use teton_core::entities::ModelSelection;

/// File name of the decision record inside the daemon state directory.
pub const SELECTION_FILE: &str = "model-selection.toml";

/// A failure while persisting the decision record.
///
/// Carries no path: an error surfaced to a client or a log must not name the
/// user's state directory (BR-11 / conventions: nothing content-bearing in logs).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SelectionStoreError {
    /// The record could not be encoded as TOML.
    #[error("could not encode the model-selection record")]
    Encode,
    /// The record could not be written to the daemon state directory.
    #[error("could not write the model-selection record to the daemon state directory")]
    Write,
}

/// The persisted model-selection decision (REQ-547 D-4).
///
/// Reads are served from an in-memory cache so the consent gate's BR-10 check
/// ("is this already decided?") costs nothing on the hot path; writes go through
/// a temp-file-plus-rename so an interrupted write can never leave a truncated
/// record that would read back as "no decision" *and* have destroyed the old one.
///
/// A store with no path ([`SelectionStore::in_memory`]) keeps the record for the
/// life of the process only — used by the minimal runtime and by tests that are
/// asserting on the decision flow rather than on durability.
#[derive(Debug)]
pub struct SelectionStore {
    path: Option<PathBuf>,
    cached: Mutex<Option<ModelSelection>>,
}

impl SelectionStore {
    /// Open the store in `base_dir` (the daemon state directory), loading any
    /// decision a previous daemon run recorded.
    ///
    /// A record that is absent, unreadable, or malformed reads back as "no
    /// decision", so the daemon re-proposes rather than refusing to start. That
    /// direction is deliberate: a corrupt record must not be able to strand the
    /// local tier in a state the user cannot answer their way out of.
    #[must_use]
    pub fn open(base_dir: &Path) -> Self {
        let path = base_dir.join(SELECTION_FILE);
        let cached = read_record(&path);
        Self {
            path: Some(path),
            cached: Mutex::new(cached),
        }
    }

    /// A store that keeps the decision in memory only.
    #[must_use]
    pub fn in_memory() -> Self {
        Self {
            path: None,
            cached: Mutex::new(None),
        }
    }

    /// The decision in force, or `None` when this machine has not decided yet.
    #[must_use]
    pub fn current(&self) -> Option<ModelSelection> {
        self.cached
            .lock()
            .expect("selection store mutex poisoned")
            .clone()
    }

    /// Record `selection` as the decision in force, replacing any earlier one.
    ///
    /// # Errors
    /// Returns a [`SelectionStoreError`] if the record could not be encoded or
    /// written. The in-memory view is updated either way, so a daemon whose state
    /// directory is read-only still honours the decision for its own lifetime
    /// rather than re-prompting on every turn.
    pub fn record(&self, selection: &ModelSelection) -> Result<(), SelectionStoreError> {
        *self.cached.lock().expect("selection store mutex poisoned") = Some(selection.clone());

        let Some(path) = &self.path else {
            return Ok(());
        };
        let text = toml::to_string(selection).map_err(|_| SelectionStoreError::Encode)?;
        write_atomically(path, &text)
    }

    /// Forget the recorded decision (the daemon re-proposes on the next start).
    ///
    /// Used by `model/set`-style flows that deliberately re-open the question;
    /// it is never reached by a failed install, which keeps its record so the
    /// failure can never be mistaken for a decline (BR-12).
    pub fn clear(&self) {
        *self.cached.lock().expect("selection store mutex poisoned") = None;
        if let Some(path) = &self.path {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl Default for SelectionStore {
    fn default() -> Self {
        Self::in_memory()
    }
}

/// Read and parse the record at `path`, treating every failure as "no decision".
fn read_record(path: &Path) -> Option<ModelSelection> {
    let text = std::fs::read_to_string(path).ok()?;
    toml::from_str(&text).ok()
}

/// Write `text` to `path` via a sibling temp file and a rename, so a reader never
/// observes a half-written record.
fn write_atomically(path: &Path, text: &str) -> Result<(), SelectionStoreError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|_| SelectionStoreError::Write)?;
    }
    let temp = path.with_extension("toml.tmp");
    std::fs::write(&temp, text).map_err(|_| SelectionStoreError::Write)?;
    std::fs::rename(&temp, path).map_err(|_| {
        let _ = std::fs::remove_file(&temp);
        SelectionStoreError::Write
    })
}

/// Wall-clock milliseconds since the Unix epoch, for `ModelSelection.decided_at_ms`.
///
/// Saturates at `0` on a clock before the epoch rather than panicking: a skewed
/// clock must not be able to take the daemon down mid-decision.
#[must_use]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use teton_core::entities::SelectionSource;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "teton-selection-{tag}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_unwritten_store_reports_no_decision() {
        let dir = temp_dir("empty");
        assert!(SelectionStore::open(&dir).current().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_recorded_decision_survives_a_reopen() {
        let dir = temp_dir("roundtrip");
        let store = SelectionStore::open(&dir);
        let selection = ModelSelection::accepted("qwen2.5-coder-7b", SelectionSource::Probe, 1_700);
        store.record(&selection).unwrap();

        // BR-10 / AC-4: a *later daemon start* reads the same decision back.
        let reopened = SelectionStore::open(&dir);
        assert_eq!(reopened.current(), Some(selection));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_decline_survives_a_reopen_and_names_no_model() {
        let dir = temp_dir("declined");
        let store = SelectionStore::open(&dir);
        store.record(&ModelSelection::declined(42)).unwrap();

        let reopened = SelectionStore::open(&dir).current().unwrap();
        assert!(reopened.declined_local);
        assert_eq!(reopened.model_name, None);
        assert!(!reopened.installs_local_model());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_malformed_record_reads_back_as_undecided() {
        let dir = temp_dir("corrupt");
        std::fs::write(dir.join(SELECTION_FILE), b"this is not toml = = =").unwrap();
        // Rather than refusing to start, the daemon re-proposes.
        assert!(SelectionStore::open(&dir).current().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_forgets_the_record_on_disk() {
        let dir = temp_dir("clear");
        let store = SelectionStore::open(&dir);
        store.record(&ModelSelection::declined(1)).unwrap();
        store.clear();
        assert!(store.current().is_none());
        assert!(SelectionStore::open(&dir).current().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_in_memory_store_holds_a_decision_without_a_path() {
        let store = SelectionStore::in_memory();
        assert!(store.current().is_none());
        store.record(&ModelSelection::declined(7)).unwrap();
        assert!(store.current().unwrap().declined_local);
    }

    #[test]
    fn the_persisted_record_carries_no_install_path() {
        // BR-11 by construction: `ModelSelection` has no path field, so the
        // record cannot become a route for one into a protocol payload.
        let dir = temp_dir("nopath");
        let store = SelectionStore::open(&dir);
        store
            .record(&ModelSelection::accepted(
                "qwen2.5-coder-3b",
                SelectionSource::AutoAccept,
                9,
            ))
            .unwrap();
        let text = std::fs::read_to_string(dir.join(SELECTION_FILE)).unwrap();
        assert!(!text.contains('/'), "record leaked a path: {text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
