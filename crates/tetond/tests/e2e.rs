//! The end-to-end acceptance suite (TASK-014).
//!
//! This integration-test binary spawns the **real** `tetond` binary and drives
//! it as a protocol client against temp fixtures, verifying every REQ-544
//! acceptance criterion (AC-1..AC-9) against real subsystems with mock
//! transports/engines where a live model or a live API would otherwise be
//! required. It is the "Verify, Don't Trust" gate for the whole MVP and runs in
//! CI with no model weights and no live API keys.
//!
//! - [`harness`] — spawn/drive support, the mock provider (egress-capture) HTTP
//!   server, and the suite-wide BR-1 capture assertion.
//! - [`ac_matrix`] — one test per acceptance criterion.

// The suite lives under `tests/e2e/`; `#[path]` keeps that layout while this
// top-level file remains the integration-test binary Cargo compiles.
#[path = "e2e/ac_matrix.rs"]
mod ac_matrix;
#[path = "e2e/harness.rs"]
mod harness;
#[path = "e2e/privacy_fixes.rs"]
mod privacy_fixes;
