//! teton-providers — provider adapters.
//!
//! Adapters for Anthropic and OpenAI-compatible providers (DeepSeek, Kimi,
//! Ollama, ...). Every remote call routes through the daemon's single egress
//! choke point; adapters never bypass the privacy boundary (BR-1).

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
