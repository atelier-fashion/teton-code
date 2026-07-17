//! teton-protocol — client-daemon protocol types.
//!
//! Bespoke JSON-RPC 2.0 vocabulary (ADR-002) shared by the daemon and every
//! client. Kept dependency-light so it can be mirrored in TypeScript for the
//! VS Code extension.

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
