//! REQ-599: filesystem scratch helpers shared by the `runtime` module tree's
//! tests.
//!
//! Lifted out of `runtime/mod.rs`'s test module in step 2. They were reachable
//! there only through `super::`, which meant the first extracted module could
//! not take its tests with it — and BR-7 requires exactly that: a subsystem
//! moved to a new module takes its `#[cfg(test)]` bodies along, rather than
//! leaving them behind pointing at a module they no longer describe.
//!
//! `scratch_dir` has 27 call sites and `set_dir_readonly` 7, spread across test
//! modules that this REQ will land in different files, so a shared home is what
//! they already needed.

use std::path::{Path, PathBuf};

/// A throwaway directory under the system temp dir, unique per test.
pub(crate) fn scratch_dir(tag: &str) -> PathBuf {
    // pid + nanos alone can collide when two tests hit the same clock tick,
    // and this helper is shared by every `mod` below — including the ones
    // that seed a config file and then read it back, where a collision is
    // one test reading another's document. The counter is what the sibling
    // integration suites add for the same reason (`config_preservation.rs`,
    // `model_consent.rs`).
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "teton-loadcfg-{tag}-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Toggle a directory between `r-x` and `rwx` for the owner.
pub(crate) fn set_dir_readonly(dir: &Path, readonly: bool) {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = if readonly { 0o555 } else { 0o755 };
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
}
