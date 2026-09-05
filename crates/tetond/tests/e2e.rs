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
//! - [`conversation_carry`] — REQ-567: a `local-only` read in an early prompt
//!   blocks a later prompt's remote route exactly as a same-turn read does
//!   (AC-2, egress capture), and a second client's prompt carries the
//!   conversation the departed client left behind (AC-9, two connections under
//!   the real daemon lifetime).
//! - [`daemon_lifetime`] — REQ-565: the daemon exits with its last client.
//!   The only suite here that runs its daemon under the real shipped lifetime
//!   rather than pinning it, and the only one that asserts on the process.
//! - [`duty_taint`] — REQ-561 AC-5: a tainted session runs its harness duties on
//!   the local tier however they are bound, asserted by the bytes a mock
//!   provider received rather than by reading the resolved route, and paired
//!   with an untainted session on the same daemon that genuinely sends them.
//! - [`session_root`] — REQ-583: the session root over the socket. `session/create`
//!   answers with the derived root and refuses a bad cwd naming the path (AC-9's
//!   daemon half); `session/set_cwd` moves a live session's jail, clears its
//!   conversation and announces both ahead of its answer, at **every**
//!   permission level (AC-10's daemon half) — asserted by what a `read` under
//!   the old and the new root hands the model, in the bytes a mock provider
//!   received.
//! - [`shell_pin_shape`] — REQ-614 TASK-397 / BUG-214: what a prompt-turn pin
//!   *records and announces*. An opaque `shell` result pins with the liftable
//!   `unknown_shell` cause, `session_pinned` precedes the pinned local route,
//!   `/shell allow` lifts it once; a `Rooted` result pins nothing and its next
//!   send leaves; a typed user skill pins on its first send, liftably, and says
//!   so. All asserted on what a client received from the real daemon, because
//!   the in-process sink tests were green while the daemon never built the sink.
//! - [`state_dir`] — BUG-211: the daemon keeps its durable state under
//!   `$XDG_DATA_HOME/teton`, never beside the socket under the logout-cleared
//!   `$XDG_RUNTIME_DIR/teton`, and a daemon that finds legacy state beside the
//!   socket moves it once and says so on its stderr.

// The suite lives under `tests/e2e/`; `#[path]` keeps that layout while this
// top-level file remains the integration-test binary Cargo compiles.
#[path = "e2e/ac_matrix.rs"]
mod ac_matrix;
#[path = "e2e/consent_matrix.rs"]
mod consent_matrix;
#[path = "e2e/conversation_carry.rs"]
mod conversation_carry;
#[path = "e2e/daemon_lifetime.rs"]
mod daemon_lifetime;
#[path = "e2e/duty_taint.rs"]
mod duty_taint;
#[path = "e2e/harness.rs"]
mod harness;
#[path = "e2e/model_identity.rs"]
mod model_identity;
#[path = "e2e/privacy_fixes.rs"]
mod privacy_fixes;
#[path = "e2e/routing_categories.rs"]
mod routing_categories;
#[path = "e2e/session_root.rs"]
mod session_root;
#[path = "e2e/shell_pin_shape.rs"]
mod shell_pin_shape;
#[path = "e2e/state_dir.rs"]
mod state_dir;
