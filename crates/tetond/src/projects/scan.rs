//! The bounded dev-folder scan (REQ-584 BR-3/BR-4, ADR-5).
//!
//! # Why this is not `walk::visit`
//!
//! BR-3 wants a **fixed list of roots, each read one or two levels deep**, under
//! a budget an order of magnitude smaller than a tool walk's. `visit` is a
//! recursive whole-tree walker with a 100,000-entry budget and no concept of a
//! depth bound. Threading one through it — for a caller whose shape is a
//! two-level loop over eleven directories — would complicate the walker every
//! tool depends on, to serve the one caller that does not want a walk.
//!
//! What REQ-583 BR-11 asked for is **one skip set, one media set, one budget
//! type** — and those are reused here verbatim. One *walker* was never the
//! requirement. The distinction matters because the sets are the safety
//! property (never enter `~/Library`, never follow a symlink) and the recursion
//! is only a traversal strategy.
//!
//! # On demand only
//!
//! BR-3 is explicit: never at launch, never on a timer, never during a turn
//! that did not ask. Nothing in this module is reachable from `session/create`
//! or from daemon start — [`ScanObserver`] exists so AC-4 can *prove* that
//! rather than assert it by inspection.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use teton_core::projects::{KnownProject, ProjectSource};

use crate::harness::tools::walk::{HOME_TOP_LEVEL_SKIPS, MEDIA_BUNDLE_SUFFIXES, WALK_SKIP_DIRS};

/// The conventional dev folders, `$HOME`-relative (REQ-584 BR-4, ADR-9/OQ-6).
///
/// **One table, platform-agnostic.** Linux adds nothing these names miss and
/// Windows is out of scope, so a per-platform table would be three copies of
/// one list with no difference between them. `~/Documents/GitHub` leads because
/// it is GitHub Desktop's default and therefore the single most likely place a
/// checkout actually is — which is where the 2026-08-18 incident's repo was.
///
/// Enumerated by name in a test (AC-3): the table is the surface BR-4 promises,
/// so silently dropping an entry has to be able to fail a build.
pub const DEV_FOLDERS: &[&str] = &[
    "Documents/GitHub",
    "Developer",
    "Projects",
    "projects",
    "src",
    "code",
    "dev",
    "repos",
    "work",
    "workspace",
    "GitHub",
];

/// The scan's bound (BR-3, ADR-5).
///
/// An order of magnitude under a tool walk's, as BR-3 requires: this reads a
/// handful of directories two levels deep, and a budget that let it run for ten
/// seconds would make "on demand" indistinguishable from "slow".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanBudget {
    /// Directory entries the scan may see before it stops.
    pub max_entries: usize,
    /// Wall clock it may spend.
    pub max_wall: Duration,
}

impl ScanBudget {
    /// ADR-5's default entry budget.
    pub const DEFAULT_MAX_ENTRIES: usize = 2_000;
    /// ADR-5's default wall-clock budget.
    pub const DEFAULT_MAX_WALL: Duration = Duration::from_secs(2);
}

impl Default for ScanBudget {
    fn default() -> Self {
        Self {
            max_entries: Self::DEFAULT_MAX_ENTRIES,
            max_wall: Self::DEFAULT_MAX_WALL,
        }
    }
}

/// What a scan found, and whether it saw everything (BR-3).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanResult {
    /// Project directories found, in discovery order.
    pub found: Vec<PathBuf>,
    /// The dev folders that exist and were read, with what each yielded.
    pub looked_in: Vec<(PathBuf, usize)>,
    /// Whether the budget ended the scan early — reported the way a tool walk
    /// reports one, because a partial answer presented as a complete one is
    /// the failure REQ-583 exists to prevent.
    pub stopped_early: bool,
}

/// Records whether a scan ran, so AC-4 can prove it did not.
///
/// A counter rather than a bool: "ran twice when it should have run once" is a
/// different defect from "ran when it should not have", and a bool cannot tell
/// them apart.
#[derive(Debug, Default)]
pub struct ScanObserver {
    runs: AtomicUsize,
}

impl ScanObserver {
    /// How many scans have run.
    #[must_use]
    pub fn runs(&self) -> usize {
        self.runs.load(Ordering::Relaxed)
    }

    fn note(&self) {
        self.runs.fetch_add(1, Ordering::Relaxed);
    }
}

/// Every directory the scan should read (BR-4).
///
/// The conventional table **plus evidence**: the parent of every `launched`
/// project is a dev folder by demonstration, even when it is not in the table —
/// which is how a machine that keeps checkouts somewhere unusual becomes
/// searchable after one launch.
///
/// Deduplicated and sorted, so a scan's output order does not depend on the
/// registry's iteration order (LESSON-540's shape again).
#[must_use]
pub fn dev_folders(home: Option<&Path>, known: &[&KnownProject]) -> Vec<PathBuf> {
    let mut folders: Vec<PathBuf> = Vec::new();
    if let Some(home) = home {
        folders.extend(DEV_FOLDERS.iter().map(|rel| home.join(rel)));
    }
    folders.extend(
        known
            .iter()
            .filter(|p| p.source == ProjectSource::Launched)
            .filter_map(|p| p.path.parent().map(Path::to_path_buf)),
    );
    folders.sort();
    folders.dedup();
    folders
}

/// Read `folders` two levels deep for project markers (BR-3).
///
/// `observer` is notified once per call — this is the seam AC-4 reads to prove
/// no scan happens at session create, at daemon start, or during a turn that
/// did not ask for one.
#[must_use]
pub fn scan(
    folders: &[PathBuf],
    home: Option<&Path>,
    budget: ScanBudget,
    observer: &ScanObserver,
) -> ScanResult {
    observer.note();
    let started = Instant::now();
    let mut seen = 0usize;
    let mut result = ScanResult::default();

    for folder in folders {
        // A dev folder that does not exist is skipped **silently** (BR-4) —
        // most machines have two or three of the eleven, and naming the other
        // eight would make every result a list of absences.
        if !folder.is_dir() {
            continue;
        }
        // The exclusions apply to the **roots** too, not only to what the
        // traversal descends into. BR-3's "never enters" is unconditional, and
        // the path that makes this load-bearing is BR-4's evidence rule: a
        // launch from `~/Library/Foo/repo` would otherwise make `~/Library/Foo`
        // a dev folder and put the whole tree back in scope. The conventional
        // table never produces one of these; a machine's own history can.
        if skip_dir(folder, home) {
            continue;
        }
        let before = result.found.len();

        // Depth 1: the folder's own children. Depth 2: each child's children.
        // Written as two explicit passes rather than a recursion with a
        // counter, because "two" is the rule and a counter invites a third.
        let Some(level_one) = read_children(folder, &mut seen, &budget, started, &mut result)
        else {
            result.looked_in.push((folder.clone(), 0));
            result.stopped_early = true;
            break;
        };

        for child in level_one {
            if is_project(&child) {
                result.found.push(child.clone());
                // A project is a leaf for this scan. Descending into it would
                // find its vendored checkouts, which are not the user's
                // projects and would crowd out the ones that are.
                continue;
            }
            if skip_dir(&child, home) {
                continue;
            }
            let Some(level_two) = read_children(&child, &mut seen, &budget, started, &mut result)
            else {
                result.stopped_early = true;
                break;
            };
            for grandchild in level_two {
                if is_project(&grandchild) {
                    result.found.push(grandchild);
                }
            }
        }

        result
            .looked_in
            .push((folder.clone(), result.found.len() - before));
        if result.stopped_early {
            break;
        }
    }
    result
}

/// One directory's child directories, or `None` when the budget ran out.
///
/// An unreadable directory is an empty list, not a stop: on macOS the first
/// read of `~/Documents` may be refused until the user answers the consent
/// dialog, and BR-3 says that dialog is expected — not a reason to abandon the
/// other ten folders.
fn read_children(
    dir: &Path,
    seen: &mut usize,
    budget: &ScanBudget,
    started: Instant,
    result: &mut ScanResult,
) -> Option<Vec<PathBuf>> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Some(Vec::new());
    };
    let mut children = Vec::new();
    for entry in entries.flatten() {
        *seen += 1;
        if *seen > budget.max_entries || started.elapsed() > budget.max_wall {
            result.stopped_early = true;
            return None;
        }
        // Symlinks are never followed (BR-3). `file_type` does not traverse, so
        // this is decided before anything opens the target.
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() || !file_type.is_dir() {
            continue;
        }
        children.push(entry.path());
    }
    children.sort();
    Some(children)
}

/// Whether this directory is one REQ-583's name sets exclude.
///
/// The sets are reused verbatim; only the traversal is this module's. The home
/// top-level rule keys on *position* exactly as the walker's does — `~/Library`
/// is skipped, a `Library/` inside a checkout is not.
fn skip_dir(path: &Path, home: Option<&Path>) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return true;
    };
    if WALK_SKIP_DIRS.contains(&name) {
        return true;
    }
    if MEDIA_BUNDLE_SUFFIXES
        .iter()
        .any(|suffix| name.ends_with(suffix))
    {
        return true;
    }
    let directly_under_home = home.is_some_and(|h| path.parent() == Some(h));
    directly_under_home && HOME_TOP_LEVEL_SKIPS.contains(&name)
}

/// Whether `dir` holds a REQ-583 project marker.
fn is_project(dir: &Path) -> bool {
    super::is_live_project(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::AtomicU64;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "teton-scan-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn project_at(path: &Path) -> PathBuf {
        std::fs::create_dir_all(path.join(".git")).unwrap();
        path.to_path_buf()
    }

    /// **AC-3 / BR-4.** The table is enumerated by name.
    ///
    /// The point of BR-4 is that the conventional list is a *surface*: a name
    /// silently dropped from it makes a machine unsearchable with no other
    /// symptom. Prose cannot fail a build; this can.
    #[test]
    fn the_conventional_dev_folder_table_is_what_it_says_it_is() {
        assert_eq!(
            DEV_FOLDERS,
            &[
                "Documents/GitHub",
                "Developer",
                "Projects",
                "projects",
                "src",
                "code",
                "dev",
                "repos",
                "work",
                "workspace",
                "GitHub",
            ]
        );
        assert_eq!(
            DEV_FOLDERS[0], "Documents/GitHub",
            "GitHub Desktop's default leads — it is where the incident's repo was"
        );
    }

    /// **BR-4's evidence half.** A launched project's parent is a dev folder.
    #[test]
    fn dev_folders_are_the_table_plus_the_parents_of_launched_projects() {
        let home = PathBuf::from("/home/u");
        let launched = KnownProject::new(
            PathBuf::from("/opt/odd/place/repo"),
            ProjectSource::Launched,
            1,
        );
        let scanned = KnownProject::new(
            PathBuf::from("/elsewhere/other/repo"),
            ProjectSource::Scanned,
            1,
        );

        let folders = dev_folders(Some(&home), &[&launched, &scanned]);

        assert!(
            folders.contains(&PathBuf::from("/opt/odd/place")),
            "a launched project's parent is a dev folder by evidence: {folders:?}"
        );
        assert!(
            !folders.contains(&PathBuf::from("/elsewhere/other")),
            "a SCANNED project's parent is not — it is not evidence of where \
             the user keeps things, only of where this scan already looked"
        );
        assert!(folders.contains(&home.join("Documents/GitHub")));
        assert_eq!(folders.len(), DEV_FOLDERS.len() + 1);

        // Deduped and sorted, so output order does not follow registry order.
        let mut sorted = folders.clone();
        sorted.sort();
        assert_eq!(folders, sorted);
    }

    /// **AC-3, the traversal.** Depth 1 and 2 yes, depth 3 no.
    #[test]
    fn the_scan_reaches_two_levels_and_not_three() {
        let root = temp_dir("depth");
        let dev = root.join("dev");
        project_at(&dev.join("one"));
        project_at(&dev.join("org/two"));
        project_at(&dev.join("org/deeper/three"));

        let observer = ScanObserver::default();
        let out = scan(
            std::slice::from_ref(&dev),
            None,
            ScanBudget::default(),
            &observer,
        );

        assert!(out.found.contains(&dev.join("one")), "depth 1: {out:?}");
        assert!(out.found.contains(&dev.join("org/two")), "depth 2: {out:?}");
        assert!(
            !out.found.iter().any(|p| p.ends_with("three")),
            "depth 3 is out of reach, which is the bound BR-3 names: {out:?}"
        );
        assert!(!out.stopped_early);
        assert_eq!(observer.runs(), 1);

        std::fs::remove_dir_all(&root).ok();
    }

    /// **AC-3, the exclusions.** All four, in one fixture.
    #[test]
    fn the_scan_enters_neither_the_skip_set_nor_a_bundle_nor_a_symlink() {
        let home = temp_dir("excl");
        let dev = home.join("dev");
        std::fs::create_dir_all(&dev).unwrap();

        // A project behind each exclusion. None may be found.
        project_at(&dev.join("node_modules/vendored"));
        project_at(&dev.join("shots.photoslibrary/inside"));
        project_at(&home.join("Library/hidden"));
        let real = temp_dir("excl-target");
        project_at(&real.join("linked"));
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, dev.join("link")).unwrap();

        // …and one that must be found, so the test is not passing by finding
        // nothing at all.
        project_at(&dev.join("visible"));

        let observer = ScanObserver::default();
        let out = scan(
            &[dev.clone(), home.join("Library")],
            Some(&home),
            ScanBudget::default(),
            &observer,
        );

        let names: Vec<String> = out.found.iter().map(|p| p.display().to_string()).collect();
        assert!(
            names.iter().any(|n| n.ends_with("visible")),
            "non-vacuity: the scan must find the one project it should: {names:?}"
        );
        for forbidden in ["vendored", "inside", "hidden", "linked"] {
            assert!(
                !names.iter().any(|n| n.ends_with(forbidden)),
                "the scan reached `{forbidden}`, which one of BR-3's exclusions \
                 forbids: {names:?}"
            );
        }

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&real).ok();
    }

    /// **AC-3, the budget.** It stops, and it says so.
    #[test]
    fn the_scan_stops_at_its_budget_and_reports_it() {
        let root = temp_dir("budget");
        let dev = root.join("dev");
        for i in 0..40 {
            project_at(&dev.join(format!("p{i:02}")));
        }

        let observer = ScanObserver::default();
        let tiny = ScanBudget {
            max_entries: 5,
            max_wall: Duration::from_secs(2),
        };
        let out = scan(std::slice::from_ref(&dev), None, tiny, &observer);
        assert!(
            out.stopped_early,
            "a scan that ran out must say so rather than present a partial \
             answer as a complete one: {out:?}"
        );

        // Non-vacuity: with room, the same fixture is complete.
        let full = scan(&[dev], None, ScanBudget::default(), &observer);
        assert!(!full.stopped_early);
        assert_eq!(full.found.len(), 40);

        std::fs::remove_dir_all(&root).ok();
    }

    /// **AC-4, the structural half.** No session-create path mentions the scanner.
    ///
    /// The behavioural half — a full create plus one unrelated turn leaving
    /// `ScanObserver::runs()` at zero — lives beside the tool that can actually
    /// run a scan. This one is the guard that stays true while that wiring is
    /// added: BR-3's "never at launch" is a claim about which code calls this
    /// module, and the cheapest way to keep it is to assert that the launch
    /// path does not.
    ///
    /// Fails **open** on a read error rather than panicking (BUG-159).
    #[test]
    fn nothing_on_the_session_create_path_reaches_the_scanner() {
        let Ok(runtime) =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/runtime.rs"))
        else {
            return;
        };
        let Some(store_fn) = runtime.split("pub(crate) fn store_session_skills(").nth(1) else {
            return;
        };
        // The body ends at the next top-level `    }` — enough to cover the
        // one derivation both create and set_cwd funnel through.
        let body = store_fn.split("\n    }").next().unwrap_or(store_fn);
        for forbidden in ["projects::scan", "scan::scan", "ScanBudget"] {
            assert!(
                !body.contains(forbidden),
                "`{forbidden}` reached the one derivation `session/create` takes \
                 — BR-3 says the scan runs on demand only, never at launch"
            );
        }
    }

    /// A missing dev folder is skipped silently (BR-4) — most machines have
    /// two or three of the eleven, and naming the absences would drown the
    /// result.
    #[test]
    fn a_missing_dev_folder_is_not_reported() {
        let observer = ScanObserver::default();
        let out = scan(
            &[PathBuf::from("/nope/not/here")],
            None,
            ScanBudget::default(),
            &observer,
        );
        assert!(out.found.is_empty());
        assert!(
            out.looked_in.is_empty(),
            "absent folders are not `looked_in`"
        );
        assert!(!out.stopped_early);
    }

    /// A project is a leaf: the scan does not descend into one.
    #[test]
    fn a_project_is_not_descended_into() {
        let root = temp_dir("leaf");
        let dev = root.join("dev");
        let outer = project_at(&dev.join("outer"));
        project_at(&outer.join("vendor/inner"));

        let observer = ScanObserver::default();
        let out = scan(&[dev], None, ScanBudget::default(), &observer);

        assert!(out.found.contains(&outer));
        assert!(
            !out.found.iter().any(|p| p.ends_with("inner")),
            "a checkout vendored inside a project is not one of the user's \
             projects, and would crowd out the ones that are: {out:?}"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// An unreadable folder does not abandon the rest (the macOS TCC shape).
    #[test]
    fn an_unreadable_folder_does_not_end_the_scan() {
        let root = temp_dir("unreadable");
        let blocked = root.join("blocked");
        std::fs::create_dir_all(&blocked).unwrap();
        let open = root.join("open");
        project_at(&open.join("found"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
        }

        let observer = ScanObserver::default();
        let out = scan(
            &[blocked.clone(), open],
            None,
            ScanBudget::default(),
            &observer,
        );

        assert!(
            out.found.iter().any(|p| p.ends_with("found")),
            "a folder the OS refused must not take the other folders with it — \
             on macOS the first read of ~/Documents is exactly this: {out:?}"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755));
        }
        std::fs::remove_dir_all(&root).ok();
    }
}
