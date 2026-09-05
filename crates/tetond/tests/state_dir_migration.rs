//! BUG-211 — the daemon opens its durable stores under the data directory, and
//! a daemon that finds them where an older one left them (beside the socket)
//! moves them once.
//!
//! Driven through `DaemonRuntime::from_dirs`, the constructor the binary calls,
//! rather than through the migration function alone: the claim is not "files
//! move" but "the runtime then **reads the moved files**" — a config value
//! planted under the runtime directory is the value the daemon starts with,
//! and the ledger it opens is the one with history in it.
//!
//! | Claim | Test |
//! |---|---|
//! | the stores open under the data directory, and legacy state is moved there once | [`legacy_state_beside_the_socket_is_moved_to_the_data_dir_and_read_from_there`] |
//! | `from_env` — one directory for both — moves nothing and reads as before | [`from_env_over_one_directory_is_unchanged`] |
//!
//! `TETON_CONFIG` winning over a moved `config.toml` is the existing
//! precedence (`std::env::var_os("TETON_CONFIG")` is consulted first, and the
//! e2e harness runs every daemon that way); it is not re-asserted here because
//! setting a process-global variable would race the guard the other tests hold.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tetond::broadcast::EventBus;
use tetond::runtime::DaemonRuntime;

/// A scratch root no other test in this binary can collide with (the
/// counter, not the clock, guarantees it).
fn scratch(tag: &str) -> PathBuf {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "teton-bug211-{tag}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// A config with one value the runtime reports back verbatim — the transcript
/// switch — so "read from the moved file" is observable without opening the
/// file again ourselves.
fn legacy_config(runtime_dir: &Path, enabled: bool) {
    std::fs::create_dir_all(runtime_dir).unwrap();
    std::fs::write(
        runtime_dir.join("config.toml"),
        format!("[transcript]\nenabled = {enabled}\nretain_days = 0\n"),
    )
    .unwrap();
}

/// The suite must not run under a developer's `TETON_CONFIG`, which would
/// override the fallback every claim here is about.
fn assert_no_ambient_config() {
    assert!(
        std::env::var_os("TETON_CONFIG").is_none(),
        "these tests exercise the config-path fallback; unset TETON_CONFIG"
    );
}

#[tokio::test]
async fn legacy_state_beside_the_socket_is_moved_to_the_data_dir_and_read_from_there() {
    assert_no_ambient_config();
    let root = scratch("moves");
    let runtime_dir = root.join("run").join("teton");
    let data_dir = root.join("data").join("teton");
    legacy_config(&runtime_dir, false);
    std::fs::write(
        runtime_dir.join("model-selection.toml"),
        "model_name = \"x\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(runtime_dir.join("models")).unwrap();
    std::fs::write(runtime_dir.join("models/x.gguf"), vec![1u8; 512]).unwrap();

    let events = Arc::new(EventBus::new());
    let runtime = DaemonRuntime::from_dirs(&runtime_dir, &data_dir, &events).expect("starts");

    // The stores now live under the data directory and are gone from the
    // runtime one.
    for entry in ["config.toml", "model-selection.toml", "models/x.gguf"] {
        assert!(data_dir.join(entry).is_file(), "{entry} did not move");
        assert!(
            !runtime_dir.join(entry).exists(),
            "{entry} was left beside the socket"
        );
    }
    assert!(
        data_dir.join("cost.db").is_file(),
        "the ledger opens under the data directory"
    );
    assert!(
        !runtime_dir.join("cost.db").exists(),
        "no ledger is created beside the socket"
    );

    // …and the runtime read the moved config, not a default: the planted
    // `enabled = false` is what it reports, and the path it names is the
    // data-dir one.
    let posture = runtime
        .config_snapshot()
        .transcript
        .expect("the snapshot carries the transcript posture");
    assert!(
        !posture.enabled,
        "the runtime must have read the moved config.toml: {posture:?}"
    );
    assert!(
        Path::new(&posture.dir).starts_with(&data_dir),
        "transcripts live under the data directory too: {}",
        posture.dir
    );
    assert_eq!(
        runtime.config_path(),
        Some(data_dir.join("config.toml").as_path()),
        "the config path the runtime holds is the data-dir one"
    );

    // A second start finds nothing to move and reads the same file.
    drop(runtime);
    let again = DaemonRuntime::from_dirs(&runtime_dir, &data_dir, &events).expect("restarts");
    assert!(!again.config_snapshot().transcript.expect("posture").enabled);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn from_env_over_one_directory_is_unchanged() {
    assert_no_ambient_config();
    let root = scratch("one");
    let dir = root.join("teton");
    legacy_config(&dir, false);
    let events = Arc::new(EventBus::new());
    let runtime = DaemonRuntime::from_env(&dir, &events).expect("starts");
    assert!(
        dir.join("config.toml").is_file(),
        "nothing moved: one directory is both"
    );
    assert!(
        dir.join("cost.db").is_file(),
        "the ledger opens in that one directory"
    );
    assert!(
        !runtime
            .config_snapshot()
            .transcript
            .expect("posture")
            .enabled
    );
    let _ = std::fs::remove_dir_all(&root);
}
