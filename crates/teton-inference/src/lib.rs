//! teton-inference — local model lifecycle.
//!
//! llama.cpp embedding, hardware probe, benchmark, and runtime pressure
//! adaptation. The local tier disables itself below the hardware floor or
//! under memory pressure rather than degrading the machine (BR-8/BR-9).

/// Returns the crate version (equal to the workspace version).
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_reported() {
        assert!(!version().is_empty());
    }
}
