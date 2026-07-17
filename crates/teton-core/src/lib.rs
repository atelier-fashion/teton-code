//! teton-core — router, session state, and cost ledger.
//!
//! Pure logic only: this crate holds no I/O dependencies (no async runtime,
//! no HTTP client) so the routing and privacy-boundary logic stays trivially
//! testable and cannot itself perform egress.

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
