//! ADLC artifact storage under `.teton/` in the user's repo.
//!
//! Structured mode's artifacts (spec entity `TaskArtifact`) are plain Markdown
//! files the daemon reads and writes under `<repo>/.teton/<req_id>/`. They are the
//! REQ-544 differentiator's load-bearing detail: a well-specified task artifact is
//! what carries intelligence forward so a cheap model can execute the implement
//! phase. The store is deliberately thin — read, write, exists, and a
//! bundled-template scaffold for a fresh repo (OQ-5). No REQ counters, no global
//! state, no gate scripts: those are personal-toolkit conventions, out of scope
//! for the generic extraction (D-4).

use std::fs;
use std::io;
use std::path::PathBuf;

use teton_protocol::events::TaskArtifactRef;
use teton_protocol::Phase;

use super::templates;

/// The kind of ADLC artifact — one per structured phase that produces one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactKind {
    /// The requirement, authored in the spec phase.
    Requirement,
    /// The plan / architecture, authored in the architect phase.
    Plan,
    /// The task file, decomposed in the architect phase and consumed by the
    /// implement phase — the cheap-model-viability mechanism.
    Task,
}

impl ArtifactKind {
    /// The phase that produces this artifact (drives the [`TaskArtifactRef`]).
    #[must_use]
    pub fn producing_phase(self) -> Phase {
        match self {
            ArtifactKind::Requirement => Phase::Spec,
            ArtifactKind::Plan | ArtifactKind::Task => Phase::Architect,
        }
    }

    /// The on-disk filename for this artifact within its requirement directory.
    #[must_use]
    pub fn filename(self) -> &'static str {
        match self {
            ArtifactKind::Requirement => "requirement.md",
            ArtifactKind::Plan => "plan.md",
            ArtifactKind::Task => "task.md",
        }
    }
}

/// One ADLC artifact: which requirement and phase it belongs to, its
/// repo-relative path, and its content. Mirrors the spec entity `TaskArtifact`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskArtifact {
    /// Owning requirement id.
    pub req_id: String,
    /// Phase that produces this artifact.
    pub phase: Phase,
    /// Which artifact this is.
    pub kind: ArtifactKind,
    /// Repo-relative path (e.g. `.teton/demo/requirement.md`), forward-slashed so
    /// it matches privacy globs and the [`TaskArtifactRef`] wire form.
    pub path: String,
    /// The artifact's full Markdown content.
    pub content: String,
}

impl TaskArtifact {
    /// The wire reference carried on a `phase_transition` event.
    #[must_use]
    pub fn to_ref(&self) -> TaskArtifactRef {
        TaskArtifactRef {
            req_id: self.req_id.clone(),
            phase: self.phase,
            path: self.path.clone(),
        }
    }
}

/// Reads and writes ADLC artifacts under `<repo_root>/.teton/`.
#[derive(Debug, Clone)]
pub struct ArtifactStore {
    repo_root: PathBuf,
}

impl ArtifactStore {
    /// A store rooted at `repo_root` (artifacts live under `repo_root/.teton/`).
    #[must_use]
    pub fn new(repo_root: impl Into<PathBuf>) -> Self {
        Self {
            repo_root: repo_root.into(),
        }
    }

    /// The repo-relative path for an artifact, forward-slashed.
    #[must_use]
    pub fn rel_path(&self, req_id: &str, kind: ArtifactKind) -> String {
        format!(".teton/{req_id}/{}", kind.filename())
    }

    /// The absolute on-disk path for an artifact.
    #[must_use]
    pub fn abs_path(&self, req_id: &str, kind: ArtifactKind) -> PathBuf {
        self.repo_root
            .join(".teton")
            .join(req_id)
            .join(kind.filename())
    }

    /// Whether an artifact exists on disk.
    #[must_use]
    pub fn exists(&self, req_id: &str, kind: ArtifactKind) -> bool {
        self.abs_path(req_id, kind).is_file()
    }

    /// Write an artifact's content, creating the requirement directory as needed.
    ///
    /// # Errors
    /// Any filesystem error creating the directory or writing the file.
    pub fn write(
        &self,
        req_id: &str,
        kind: ArtifactKind,
        content: &str,
    ) -> io::Result<TaskArtifact> {
        let abs = self.abs_path(req_id, kind);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&abs, content)?;
        Ok(self.artifact(req_id, kind, content.to_owned()))
    }

    /// Load an artifact if it exists, returning `None` when it is absent or
    /// unreadable (never a silent stub — the caller decides what a missing
    /// artifact means).
    #[must_use]
    pub fn load(&self, req_id: &str, kind: ArtifactKind) -> Option<TaskArtifact> {
        let content = fs::read_to_string(self.abs_path(req_id, kind)).ok()?;
        Some(self.artifact(req_id, kind, content))
    }

    /// Scaffold the generic requirement / plan / task artifacts for a fresh
    /// requirement from the bundled templates (OQ-5).
    ///
    /// Each is written as a stub with `{{id}}` / `{{title}}` filled in and its
    /// content placeholders intact — so a repo with no prior `.teton/` can enter
    /// structured mode, while the phase gate still requires a spec/architect turn
    /// to author each artifact before it will advance.
    ///
    /// # Errors
    /// Any filesystem error writing an artifact.
    pub fn scaffold(&self, req_id: &str, title: &str) -> io::Result<Vec<TaskArtifact>> {
        let mut out = Vec::with_capacity(3);
        for kind in [
            ArtifactKind::Requirement,
            ArtifactKind::Plan,
            ArtifactKind::Task,
        ] {
            let content = templates::render(templates::template_for(kind), req_id, title);
            out.push(self.write(req_id, kind, &content)?);
        }
        Ok(out)
    }

    /// The `.teton/` directory this store manages (may not exist yet).
    #[must_use]
    pub fn teton_dir(&self) -> PathBuf {
        self.repo_root.join(".teton")
    }

    fn artifact(&self, req_id: &str, kind: ArtifactKind, content: String) -> TaskArtifact {
        TaskArtifact {
            req_id: req_id.to_owned(),
            phase: kind.producing_phase(),
            kind,
            path: self.rel_path(req_id, kind),
            content,
        }
    }
}

/// Whether `content` is a minimally valid, *authored* artifact: non-empty and no
/// longer carrying template placeholders. A scaffolded-but-unedited stub (which
/// still contains `{{...}}`) is invalid — the phase gate refuses to advance on it
/// rather than let an unwritten artifact pass silently.
#[must_use]
pub fn is_authored(content: &str) -> bool {
    !content.trim().is_empty() && !content.contains("{{")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_repo() -> PathBuf {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "teton-artifacts-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn write_then_load_round_trips_with_repo_relative_path() {
        let repo = temp_repo();
        let store = ArtifactStore::new(&repo);
        let written = store
            .write(
                "demo",
                ArtifactKind::Requirement,
                "# Requirement demo\n\nreal content",
            )
            .expect("write");
        assert_eq!(written.path, ".teton/demo/requirement.md");
        assert_eq!(written.phase, Phase::Spec);

        let loaded = store.load("demo", ArtifactKind::Requirement).expect("load");
        assert_eq!(loaded, written);
        assert!(store.exists("demo", ArtifactKind::Requirement));
        fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn scaffold_creates_all_three_stubs_in_a_fresh_repo() {
        // OQ-5: a repo with no prior `.teton/` gets the bundled generic templates.
        let repo = temp_repo();
        let store = ArtifactStore::new(&repo);
        assert!(!store.teton_dir().exists());

        let scaffolded = store
            .scaffold("demo", "Add retry backoff")
            .expect("scaffold");
        assert_eq!(scaffolded.len(), 3);
        assert!(store.exists("demo", ArtifactKind::Requirement));
        assert!(store.exists("demo", ArtifactKind::Plan));
        assert!(store.exists("demo", ArtifactKind::Task));

        // Metadata is filled, but the stub is not yet an authored artifact.
        let req = store.load("demo", ArtifactKind::Requirement).unwrap();
        assert!(req.content.contains("Add retry backoff"));
        assert!(!is_authored(&req.content), "a fresh stub is not authored");
        fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn is_authored_rejects_empty_and_stub_content() {
        assert!(!is_authored(""));
        assert!(!is_authored("   \n  "));
        assert!(!is_authored("## Description\n\n{{description}}"));
        assert!(is_authored(
            "## Description\n\nThe real, authored description."
        ));
    }

    #[test]
    fn missing_artifact_loads_as_none() {
        let repo = temp_repo();
        let store = ArtifactStore::new(&repo);
        assert!(store.load("demo", ArtifactKind::Task).is_none());
        assert!(!store.exists("demo", ArtifactKind::Task));
        fs::remove_dir_all(&repo).ok();
    }
}
