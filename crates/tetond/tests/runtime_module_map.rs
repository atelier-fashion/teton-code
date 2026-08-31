//! REQ-599 AC-12 — the architecture doc's module map matches the disk.
//!
//! A decomposition's map is the first thing to rot: modules get renamed, merged
//! or added, and the document describing them keeps saying what was true on the
//! day it was written. That is worse than no map, because a reader trusts it.
//!
//! So the map is a **checked** property. The table in
//! `.adlc/specs/REQ-599-decompose-the-turn-path/architecture.md` names one
//! module per row, and this asserts the set it names is exactly the set on disk
//! — in both directions, because the two failures are different:
//!
//! - a module named in the doc but absent from disk is a **stale map**;
//! - a module on disk but absent from the doc is an **undocumented module**,
//!   which is how a map starts being incomplete rather than wrong.
//!
//! Both are failures here. Neither is caught by anything else in the suite.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/<crate> has a workspace root two levels up")
        .to_path_buf()
}

/// The module basenames the architecture doc's map table names.
///
/// The table's rows open with a `| \`name.rs\` |` cell. Parsed rather than
/// duplicated, so the doc stays the single source and this test cannot drift
/// from it by being edited on its own.
fn documented_modules() -> BTreeSet<String> {
    let doc = workspace_root().join(".adlc/specs/REQ-599-decompose-the-turn-path/architecture.md");
    let text = std::fs::read_to_string(&doc)
        .unwrap_or_else(|err| panic!("unreadable architecture doc {}: {err}", doc.display()));

    let mut out = BTreeSet::new();
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with("| `") {
            continue;
        }
        let Some(rest) = line.strip_prefix("| `") else {
            continue;
        };
        let Some((name, _)) = rest.split_once('`') else {
            continue;
        };
        if name.ends_with(".rs") {
            out.insert(name.to_owned());
        }
    }
    out
}

/// The `.rs` files actually under `crates/tetond/src/runtime/`.
fn modules_on_disk() -> BTreeSet<String> {
    let dir = workspace_root().join("crates/tetond/src/runtime");
    let mut paths = Vec::new();
    rust_files_recursive(&dir, &mut paths);
    // Relative to `runtime/`, not the basename. A basename collapses
    // `runtime/nested/mod.rs` onto `runtime/mod.rs`, so a whole undocumented
    // subtree reads as the documented root module and the map check passes over
    // it. Found by TASK-301's nested-module fixture, which is what that fixture
    // is for: the recursion was added and the *comparison* still could not see
    // what recursion now found.
    paths
        .iter()
        .filter_map(|p| p.strip_prefix(&dir).ok())
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}

/// Every `.rs` file under `dir`, **recursively**.
///
/// A local copy, matching the pattern four sibling integration tests already
/// use (`traceability_sweep.rs`, `recipe_window_one_home.rs`,
/// `suppression_ratchet.rs`, `boundary_coverage.rs`): the canonical walker is
/// `call_sites::scan::rust_files`, which is `#[cfg(test)]`-gated inside the lib
/// and therefore unreachable from an integration test, which links the lib
/// compiled without that cfg.
///
/// Recursion is the point (REQ-602 BR-4): `runtime/` is a module *tree*, and the
/// first `runtime/foo/mod.rs` would leave a flat scan's corpus silently —
/// LESSON-594's "a sweep sees less and passes" arriving through a directory
/// rather than a rename.
fn rust_files_recursive(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(listing) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in listing.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files_recursive(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn the_architecture_docs_module_map_matches_the_modules_on_disk() {
    let documented = documented_modules();
    let on_disk = modules_on_disk();

    assert!(
        documented.len() >= 6,
        "vacuity floor: the map parser found only {} module rows in the architecture doc. \
         A parser that matches nothing agrees with any directory, which is the one way this \
         test could pass while telling you nothing.\nfound: {documented:?}",
        documented.len()
    );

    let stale: Vec<_> = documented.difference(&on_disk).cloned().collect();
    assert!(
        stale.is_empty(),
        "the architecture doc names modules that do not exist — a stale map is worse than no \
         map, because it is trusted: {stale:?}"
    );

    let undocumented: Vec<_> = on_disk.difference(&documented).cloned().collect();
    assert!(
        undocumented.is_empty(),
        "these modules exist but the architecture doc's map does not mention them, so the map \
         is incomplete rather than wrong — add a row: {undocumented:?}"
    );
}
