//! The project registry's store: load, prune, write (REQ-584 Leg A, ADR-1/ADR-2).
//!
//! The daemon side of the split. [`teton_core::projects`] holds the value types
//! and the ranking with no I/O at all; this module owns the file, the pruning
//! predicate that needs a filesystem, and (in [`scan`]) the bounded dev-folder
//! scan.
//!
//! Shaped after [`crate::selection_store`], which solves the same problem —
//! a small daemon-owned document in the state dir — down to the atomic write
//! and the "a corrupt record is an absent record" rule.
//!
//! # Why every failure here is soft
//!
//! A registry is a **convenience cache** of a fact the daemon can always
//! recompute: BR-1 refills it from use and BR-3's scan refills it on demand. So
//! nothing in this module is allowed to fail a session. A daemon that refused
//! to start because `projects.json` was truncated, or a `session/create` that
//! errored because the state dir went read-only, would be failing closed on
//! something that is not a safety property — and would take away the surface
//! that exists to make the machine easier to use.

pub mod scan;

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use teton_core::projects::{ProjectRegistry, ProjectSource};
use teton_core::session_root::PROJECT_MARKERS;

/// The registry, its file, and the lock that serialises writers.
///
/// One per daemon. The in-memory copy is authoritative for the process; the
/// file is how it survives a restart.
#[derive(Debug)]
pub struct ProjectStore {
    /// `None` for an in-memory store (tests, and a daemon with no state dir).
    path: Option<PathBuf>,
    registry: Mutex<ProjectRegistry>,
}

impl ProjectStore {
    /// Open the registry at `path`, pruning what is already gone (BR-2).
    ///
    /// A missing file, an unreadable one, or one this build cannot parse all
    /// read back as an **empty registry** — see the module note on why every
    /// failure here is soft. The unreadable and unparseable cases log one line,
    /// because they are the two that might mean something is wrong with the
    /// machine rather than that the user is new.
    #[must_use]
    pub fn open(path: &Path) -> Self {
        let registry = match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str::<ProjectRegistry>(&text) {
                Ok(mut registry) => {
                    // BR-2's read-time prune. A checkout deleted while the
                    // daemon was down must not appear in the very first result
                    // after it comes back.
                    registry.prune(&mut is_live_project);
                    registry
                }
                Err(err) => {
                    eprintln!(
                        "teton-code: the project registry could not be parsed ({err}); \
                         starting from an empty one — it refills from use and from \
                         `/projects` (REQ-584 BR-1, BR-3)"
                    );
                    ProjectRegistry::new()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => ProjectRegistry::new(),
            Err(err) => {
                eprintln!(
                    "teton-code: the project registry could not be read ({}); starting \
                     from an empty one",
                    err.kind()
                );
                ProjectRegistry::new()
            }
        };
        Self {
            path: Some(path.to_path_buf()),
            registry: Mutex::new(registry),
        }
    }

    /// A store that never touches a file. The default, and what tests use.
    #[must_use]
    pub fn in_memory() -> Self {
        Self {
            path: None,
            registry: Mutex::new(ProjectRegistry::new()),
        }
    }

    /// A snapshot of the registry, pruned (BR-2).
    ///
    /// Pruned on the way out as well as on the way in, so a result never names
    /// a checkout that was deleted since the last write — AC-2's surfacing
    /// half. The prune is kept in the snapshot only; persisting it is
    /// [`Self::save`]'s job, and a read has no business writing.
    #[must_use]
    pub fn snapshot(&self) -> ProjectRegistry {
        let mut registry = self.locked().clone();
        registry.prune(&mut is_live_project);
        registry
    }

    /// Record a landing at `path` (BR-1), and persist.
    ///
    /// Callers must have established that `path` is a `project`-kind root —
    /// this does not re-probe. The write is best-effort: a failure updates the
    /// in-memory registry anyway, so a daemon with a read-only state dir still
    /// answers correctly for its own lifetime.
    pub fn record(&self, path: PathBuf, source: ProjectSource) {
        {
            let mut registry = self.locked();
            registry.record(path, source, now_secs());
        }
        self.save();
    }

    /// Record several scan finds at once, then persist once (BR-3).
    ///
    /// One write for a whole scan rather than one per find — a scan that
    /// discovered forty projects should not rewrite the document forty times.
    pub fn record_all(&self, paths: Vec<PathBuf>, source: ProjectSource) {
        if paths.is_empty() {
            return;
        }
        {
            let mut registry = self.locked();
            let now = now_secs();
            for path in paths {
                registry.record(path, source, now);
            }
        }
        self.save();
    }

    /// Prune and write (BR-2's write-time half).
    ///
    /// Failures are swallowed on purpose (see the module note). The prune runs
    /// **before** the encode so corpses never reach the file, which is what
    /// keeps a long-lived daemon's document from growing with them.
    fn save(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let text = {
            let mut registry = self.locked();
            registry.prune(&mut is_live_project);
            match serde_json::to_string_pretty(&*registry) {
                Ok(text) => text,
                Err(_) => return,
            }
        };
        let _ = write_atomically(path, &text);
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, ProjectRegistry> {
        self.registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Default for ProjectStore {
    fn default() -> Self {
        Self::in_memory()
    }
}

/// Build a locator answer: registry first, then the scan **on demand** (BR-3).
///
/// The one composition, shared by the `projects` tool and `/projects` — BR-9's
/// one-renderer rule applies to the *facts* as much as to the wording, and two
/// assemblies of "which projects, ranked how, scanned when" would drift the way
/// two renderers would.
///
/// The scan runs only when this is called, which is the whole of BR-3: nothing
/// on the session-create path reaches here, and `ScanObserver` is how AC-4
/// proves it.
///
/// **The scan is skipped when the registry already answers the question.** A
/// user who asks `projects teton` on a machine that has launched from
/// `teton-code` should not pay for eleven directory reads to be told what is
/// already known — and on macOS that read is the one that can raise the
/// Documents dialog.
pub fn locator_view(
    store: &ProjectStore,
    home: Option<&Path>,
    budget: scan::ScanBudget,
    observer: &scan::ScanObserver,
    query: Option<&str>,
) -> teton_core::projects::LocatorView {
    use teton_core::projects::{LocatorRow, LocatorView, LookedIn};
    use teton_core::session_root::{bounded_field, display_for, DISPLAY_MAX_CHARS, NAME_MAX_CHARS};

    let mut registry = store.snapshot();
    let mut stopped_early = false;
    let mut scanned_folders: Vec<(PathBuf, usize)> = Vec::new();

    if registry.rank(query).is_empty() {
        let known: Vec<_> = registry.iter().collect();
        let folders = scan::dev_folders(home, &known);
        let result = scan::scan(&folders, home, budget, observer);
        stopped_early = result.stopped_early;
        scanned_folders = result.looked_in.clone();
        if !result.found.is_empty() {
            store.record_all(result.found, teton_core::projects::ProjectSource::Scanned);
            registry = store.snapshot();
        }
    }

    let now = now_secs();
    let ranked = registry.rank(query);
    // Ambiguity decides the recipe (BR-6): a name two projects answer to cannot
    // be a `/cd <name>`, so those rows carry their path instead.
    let mut name_counts: std::collections::BTreeMap<&str, usize> =
        std::collections::BTreeMap::new();
    for p in &ranked {
        *name_counts.entry(p.name.as_str()).or_default() += 1;
    }

    let matches = ranked
        .iter()
        .map(|p| {
            let display = bounded_field(&display_for(&p.path, home), DISPLAY_MAX_CHARS);
            let name = bounded_field(&p.name, NAME_MAX_CHARS);
            let ambiguous = name_counts.get(p.name.as_str()).copied().unwrap_or(0) > 1;
            LocatorRow {
                recipe: if ambiguous {
                    format!("/cd {display}")
                } else {
                    format!("/cd {name}")
                },
                name,
                display,
                source: match p.source {
                    teton_core::projects::ProjectSource::Launched => "launched",
                    teton_core::projects::ProjectSource::Scanned => "scanned",
                    teton_core::projects::ProjectSource::Unknown => "unknown",
                },
                last_used: teton_core::projects::relative_time(now, p.last_seen),
            }
        })
        .collect();

    // The folders the answer looked in. From the scan when one ran; otherwise
    // derived from where the known projects actually are, so a result never
    // claims to have looked somewhere it did not.
    let looked_in = if scanned_folders.is_empty() {
        let mut counts: std::collections::BTreeMap<PathBuf, usize> =
            std::collections::BTreeMap::new();
        for p in registry.iter() {
            if let Some(parent) = p.path.parent() {
                *counts.entry(parent.to_path_buf()).or_default() += 1;
            }
        }
        counts
            .into_iter()
            .map(|(path, count)| LookedIn {
                display: bounded_field(&display_for(&path, home), DISPLAY_MAX_CHARS),
                count,
            })
            .collect()
    } else {
        scanned_folders
            .into_iter()
            .map(|(path, count)| LookedIn {
                display: bounded_field(&display_for(&path, home), DISPLAY_MAX_CHARS),
                count,
            })
            .collect()
    };

    LocatorView {
        matches,
        looked_in,
        stopped_early,
        query: query.map(str::to_owned),
    }
}

/// Whether `path` is still a directory holding a REQ-583 project marker (BR-2).
///
/// The pruning predicate [`teton_core::projects::ProjectRegistry::prune`] asks
/// for. Both halves matter and BR-2 names both: a checkout that was *deleted*
/// and one that merely stopped being a project (`.git` removed) are equally
/// wrong answers to "where is my repo".
pub fn is_live_project(path: &Path) -> bool {
    path.is_dir()
        && PROJECT_MARKERS
            .iter()
            .any(|marker| path.join(marker).exists())
}

/// Write `text` to `path` via a sibling temp file and a rename.
///
/// The same shape [`crate::selection_store`] uses, and for the same reason: a
/// concurrent reader must never observe a half-written document. The temp file
/// is removed on a failed rename so a full disk does not leave litter.
fn write_atomically(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, text)?;
    std::fs::rename(&temp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })
}

/// Wall-clock seconds since the Unix epoch.
///
/// Saturates at `0` on a pre-epoch clock rather than panicking — a skewed clock
/// must not be able to take a session create down (`selection_store::now_ms`'s
/// rule, in seconds because the registry's keys are dates, not durations).
#[must_use]
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "teton-projects-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A directory that is a project, for the pruning predicate.
    fn project_at(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        dir
    }

    #[test]
    fn a_round_trip_preserves_every_field() {
        let dir = temp_dir("roundtrip");
        let repo = project_at(&dir, "repo");
        let file = dir.join("projects.json");

        let store = ProjectStore::open(&file);
        store.record(repo.clone(), ProjectSource::Launched);

        let reopened = ProjectStore::open(&file).snapshot();
        let entry = reopened.iter().next().expect("one entry");
        assert_eq!(entry.path, repo);
        assert_eq!(entry.name, "repo");
        assert_eq!(entry.source, ProjectSource::Launched);
        assert_eq!(entry.uses, 1);
        assert!(entry.first_seen > 0 && entry.last_seen >= entry.first_seen);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// **ADR-2, the fail-open decision.** Absent, garbage, and unreadable all
    /// read as empty — and none of them is an error.
    ///
    /// The garbage leg is the one worth having: a registry is a cache, and a
    /// daemon that refused to start over a truncated cache would be failing
    /// closed on something that is not a safety property.
    #[test]
    fn a_missing_or_corrupt_file_is_an_empty_registry_and_never_an_error() {
        let dir = temp_dir("corrupt");

        let absent = ProjectStore::open(&dir.join("nope.json"));
        assert!(absent.snapshot().is_empty(), "a missing file is empty");

        let garbage = dir.join("garbage.json");
        std::fs::write(&garbage, "{not json at all").unwrap();
        assert!(
            ProjectStore::open(&garbage).snapshot().is_empty(),
            "a truncated document is empty, not a startup failure"
        );

        // Valid JSON of the wrong shape is the same class of problem.
        let wrong = dir.join("wrong.json");
        std::fs::write(&wrong, r#"["not","a","registry"]"#).unwrap();
        assert!(ProjectStore::open(&wrong).snapshot().is_empty());

        // And it is still writable afterwards — the store is not poisoned.
        let repo = project_at(&dir, "after");
        let store = ProjectStore::open(&garbage);
        store.record(repo.clone(), ProjectSource::Launched);
        assert_eq!(ProjectStore::open(&garbage).snapshot().len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// **BR-2 / AC-2.** Pruning happens at read AND at write, and the two catch
    /// different things.
    #[test]
    fn a_dead_entry_is_pruned_at_both_read_and_write() {
        let dir = temp_dir("prune");
        let keep = project_at(&dir, "keep");
        let vanish = project_at(&dir, "vanish");
        let unmarked = project_at(&dir, "unmarked");
        let file = dir.join("projects.json");

        let store = ProjectStore::open(&file);
        store.record(keep.clone(), ProjectSource::Launched);
        store.record(vanish.clone(), ProjectSource::Launched);
        store.record(unmarked.clone(), ProjectSource::Launched);
        assert_eq!(store.snapshot().len(), 3);

        // One directory removed, one that merely stopped being a project —
        // BR-2 names both, and they are different failures.
        std::fs::remove_dir_all(&vanish).unwrap();
        std::fs::remove_dir_all(unmarked.join(".git")).unwrap();

        // Read-time: the live store's own snapshot already excludes them,
        // before anything has written.
        let snapshot = store.snapshot();
        assert_eq!(snapshot.len(), 1, "read-time prune: {snapshot:?}");
        assert_eq!(snapshot.iter().next().unwrap().path, keep);

        // Write-time: the next save must not carry the corpses to disk.
        store.record(keep.clone(), ProjectSource::Launched);
        let on_disk: teton_core::projects::ProjectRegistry =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(on_disk.len(), 1, "write-time prune: {on_disk:?}");

        // And a fresh open agrees.
        assert_eq!(ProjectStore::open(&file).snapshot().len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// **AC-2, the cap half**, through the store rather than the value type.
    #[test]
    fn a_registry_over_the_cap_comes_back_at_the_cap() {
        let dir = temp_dir("cap");
        let file = dir.join("projects.json");
        let store = ProjectStore::open(&file);

        let cap = teton_core::projects::MAX_KNOWN_PROJECTS;
        let mut paths = Vec::new();
        for i in 0..=cap {
            paths.push(project_at(&dir, &format!("p{i:03}")));
        }
        for p in paths {
            store.record(p, ProjectSource::Launched);
        }

        assert_eq!(store.snapshot().len(), cap, "the cap holds in memory");
        assert_eq!(
            ProjectStore::open(&file).snapshot().len(),
            cap,
            "and on disk"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A scan's finds cost one write, not one per find (BR-3's efficiency half).
    #[test]
    fn record_all_writes_once_and_records_every_path() {
        let dir = temp_dir("bulk");
        let file = dir.join("projects.json");
        let a = project_at(&dir, "a");
        let b = project_at(&dir, "b");

        let store = ProjectStore::open(&file);
        store.record_all(vec![a, b], ProjectSource::Scanned);

        let back = ProjectStore::open(&file).snapshot();
        assert_eq!(back.len(), 2);
        assert!(back.iter().all(|e| e.source == ProjectSource::Scanned));
        assert!(
            back.iter().all(|e| e.uses == 0),
            "a scan find is not a use — the two ranking tiers depend on it"
        );

        // An empty batch does not touch the file at all.
        let before = std::fs::metadata(&file).unwrap().len();
        store.record_all(vec![], ProjectSource::Scanned);
        assert_eq!(std::fs::metadata(&file).unwrap().len(), before);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// **BR-5 / AC-5.** The file lands where `DaemonPaths` says, and nowhere else.
    #[test]
    fn the_registry_is_written_to_the_daemon_paths_location() {
        let dir = temp_dir("location");
        let repo = project_at(&dir, "repo");
        let file = dir.join("projects.json");
        assert!(!file.exists());

        ProjectStore::open(&file).record(repo, ProjectSource::Launched);

        assert!(file.is_file(), "the registry must be written to its path");
        assert!(
            !dir.join("projects.json.tmp").exists(),
            "the atomic write's temp file must not survive"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// An in-memory store answers correctly and writes nothing.
    ///
    /// The state a daemon with a read-only state dir is in: the registry still
    /// works for this process's lifetime rather than the session failing.
    #[test]
    fn an_in_memory_store_records_without_a_file() {
        let dir = temp_dir("memory");
        let repo = project_at(&dir, "repo");
        let store = ProjectStore::in_memory();
        store.record(repo.clone(), ProjectSource::Launched);
        assert_eq!(store.snapshot().len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// **BR-1 / AC-1.** Only a `project`-kind root is recorded, and a second
    /// landing bumps rather than duplicates.
    ///
    /// Driven through `store_session_skills` — the one derivation ADR-4 put the
    /// hook inside — rather than through `ProjectStore::record` directly, so
    /// this fails if the hook is removed from that function. Calling `record`
    /// here would be a test of the store, which the tests above already are.
    #[test]
    fn only_a_project_root_is_recorded_and_a_second_landing_bumps_it() {
        use crate::runtime::DaemonRuntime;
        use crate::session_root::probe;
        use crate::sessions::SessionRegistry;
        use crate::skills::RealFs;
        use teton_protocol::methods::RootKind;

        let dir = temp_dir("ac1");
        let repo = project_at(&dir, "repo");
        let other = project_at(&dir, "other");
        let plain = dir.join("plain");
        std::fs::create_dir_all(&plain).unwrap();

        let sessions = SessionRegistry::new();
        let store = ProjectStore::in_memory();
        let fs = RealFs;

        let mut record_root = |path: &Path| {
            let id = sessions
                .create(
                    teton_protocol::SessionMode::Freeform,
                    None,
                    Some(path.to_path_buf()),
                )
                .expect("a freeform session")
                .session_id;
            let probed = crate::session_root::ProbedRoot {
                path: path.to_path_buf(),
                view: probe(path, None),
            };
            DaemonRuntime::store_session_skills(&sessions, &id, &probed, &fs, &store);
            probed.view.kind
        };

        assert_eq!(record_root(&repo), RootKind::Project);
        let snap = store.snapshot();
        assert_eq!(snap.len(), 1, "the project root is recorded: {snap:?}");
        let entry = snap.iter().next().unwrap();
        assert_eq!(entry.name, "repo");
        assert_eq!(entry.source, ProjectSource::Launched);
        assert_eq!(entry.uses, 1);
        let first_seen = entry.first_seen;

        // A second landing bumps `uses`, and does not duplicate.
        record_root(&repo);
        let snap = store.snapshot();
        assert_eq!(snap.len(), 1, "still one entry");
        let entry = snap.iter().next().unwrap();
        assert_eq!(entry.uses, 2);
        assert_eq!(entry.first_seen, first_seen, "first_seen does not move");

        // A different project is recorded too — this is the `set_cwd` shape,
        // which reaches the same function.
        record_root(&other);
        assert_eq!(store.snapshot().len(), 2);

        // And the three ways of not being a project record nothing. All three,
        // because BR-1 excludes three distinct RootKinds and a hook that tested
        // only `!= Home` would pass a two-legged version of this.
        let before = store.snapshot().len();
        assert_eq!(record_root(&plain), RootKind::Plain);
        assert_eq!(record_root(Path::new("/")), RootKind::FilesystemRoot);
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            // Probing with `home` supplied is what makes this the Home kind.
            let id = sessions
                .create(
                    teton_protocol::SessionMode::Freeform,
                    None,
                    Some(home.clone()),
                )
                .expect("session")
                .session_id;
            let probed = crate::session_root::ProbedRoot {
                path: home.clone(),
                view: probe(&home, Some(&home)),
            };
            assert_eq!(probed.view.kind, RootKind::Home);
            DaemonRuntime::store_session_skills(&sessions, &id, &probed, &fs, &store);
        }
        assert_eq!(
            store.snapshot().len(),
            before,
            "home, / and a marker-less directory record nothing"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The pruning predicate is the thing BR-2 actually means.
    #[test]
    fn is_live_project_wants_a_directory_that_still_holds_a_marker() {
        let dir = temp_dir("live");
        let repo = project_at(&dir, "repo");
        assert!(is_live_project(&repo));

        std::fs::remove_dir_all(repo.join(".git")).unwrap();
        assert!(
            !is_live_project(&repo),
            "a marker-less directory is not one"
        );

        assert!(!is_live_project(&dir.join("never-existed")));

        // A *file* at the path is not a project either.
        let file = dir.join("afile");
        std::fs::write(&file, "x").unwrap();
        assert!(!is_live_project(&file));

        std::fs::remove_dir_all(&dir).ok();
    }
}
