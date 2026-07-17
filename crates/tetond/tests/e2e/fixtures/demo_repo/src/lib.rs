//! The demo library.

/// The answer the demo session is asked to change from 1 to 2.
pub const ANSWER: u32 = 1;

/// Doubles `n`.
#[must_use]
pub fn double(n: u32) -> u32 {
    n * 2
}
