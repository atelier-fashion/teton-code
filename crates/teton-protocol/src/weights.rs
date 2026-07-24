//! Shared conventions for where on-disk model weights live.
//!
//! `tetond` installs the weights and every client may *show* their path (never
//! receive it — BR-11 keeps a resolved path off the wire). Both sides must agree
//! on the subdirectory the weights sit in and how a model name maps to its file,
//! or the two drift by hand the way the socket path did before
//! [`crate::socket_path`] centralised it. A bare directory name and a filename
//! convention are not paths, credentials, or content, so sharing them here does
//! not violate BR-11 — it just removes the copy each binary used to keep in sync.

use std::path::{Path, PathBuf};

/// Subdirectory of the daemon state directory the model weights install into.
pub const WEIGHTS_DIR: &str = "models";

/// The file a model's verified weights install under: `<name>.gguf`.
#[must_use]
pub fn weights_file_name(model_name: &str) -> String {
    format!("{model_name}.gguf")
}

/// The full weights path for `model_name` under a daemon state directory.
///
/// Local display and installer use only; this path never crosses the protocol
/// boundary (BR-11).
#[must_use]
pub fn weights_path(base_dir: &Path, model_name: &str) -> PathBuf {
    base_dir
        .join(WEIGHTS_DIR)
        .join(weights_file_name(model_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weights_file_name_appends_the_gguf_extension() {
        assert_eq!(weights_file_name("tiny-small"), "tiny-small.gguf");
    }

    #[test]
    fn weights_path_joins_the_dir_and_the_file() {
        assert_eq!(
            weights_path(Path::new("/state/teton"), "qwen2.5-coder-7b"),
            PathBuf::from("/state/teton/models/qwen2.5-coder-7b.gguf")
        );
    }
}
