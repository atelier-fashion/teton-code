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
//!   server, the mock HuggingFace host, and the suite-wide BR-1 capture assertion.
//! - [`ac_matrix`] — one test per REQ-544 acceptance criterion.
//! - [`consent_matrix`] — one test per REQ-547 acceptance criterion (the
//!   first-run model-consent gate), against a mock model host and a fixture
//!   catalog whose digests are genuinely computed from the bytes served.
//! - [`model_identity`] — REQ-557: the one-shot model migration, the ADR-E
//!   startup posture (a missing model is unusable, not invalid), the nameable
//!   missing default, and the BR-8 egress-capture claim that none of it changed
//!   the privacy boundary.
//! - [`routing_categories`] — REQ-558: the taint backstop overriding every
//!   category binding (AC-6, egress capture), what every `route_decided`
//!   carries (AC-8), and the one-resolver rule across the surfaces that
//!   describe a routing state (AC-11).

// The suite lives under `tests/e2e/`; `#[path]` keeps that layout while this
// top-level file remains the integration-test binary Cargo compiles.
#[path = "e2e/ac_matrix.rs"]
mod ac_matrix;
#[path = "e2e/consent_matrix.rs"]
mod consent_matrix;
#[path = "e2e/harness.rs"]
mod harness;
#[path = "e2e/model_identity.rs"]
mod model_identity;
#[path = "e2e/privacy_fixes.rs"]
mod privacy_fixes;
#[path = "e2e/routing_categories.rs"]
mod routing_categories;
