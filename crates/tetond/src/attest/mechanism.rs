//! macOS `LocalAuthentication`: the FFI that asks a human, and nothing else.
//!
//! Gated behind the non-default `presence` feature and `target_os = "macos"`.
//! **No policy lives here** (ADR-B) — binding, single-use and expiry are
//! [`super::policy`]'s, which CI compiles and tests. This module answers exactly
//! one question, "did a human just authenticate", and maps `LAError` onto the
//! policy module's refusal taxonomy.
//!
//! ## Everything here was verified empirically first (BR-12)
//!
//! The parameters below are not read off documentation; they are what the BR-12
//! spike observed against a binary confirmed `adhoc, linker-signed` with
//! `TeamIdentifier=not set` — the exact posture REQ-569 ADR-A called inert for
//! signature checks:
//!
//! - `canEvaluatePolicy(deviceOwnerAuthentication)` → `true`
//! - `evaluatePolicy` **blocked** for the full probe window with the runloop
//!   actively serviced, i.e. a real prompt waiting on a person
//! - `-[LAContext invalidate]` → `LAError -9` (`appCancel`)
//!
//! An inert mechanism returns an error in milliseconds. See `architecture.md`
//! §0.
//!
//! ## Threading: this blocks on a human, so it must never touch a tokio worker
//!
//! [`super::PresenceVerifier::verify`] is synchronous and can block for as long
//! as the user takes to answer. The daemon's standing rule (ADR-006 E-3,
//! LESSON-448, pinned by `tests/nonblocking_inference.rs`) is that anything
//! which parks for human or model time runs on the blocking pool — a
//! seconds-long wait on a tokio worker stalls unrelated RPCs. Callers run this
//! inside `spawn_blocking`.
//!
//! We drive a private run loop here rather than assuming the caller has one,
//! because on the blocking pool there is none.

use std::ffi::{c_char, c_void, CString};
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use teton_protocol::RequestId;

use crate::grants::ConnectionId;

use super::policy::{AttestationRefusal, PresenceAttestation};
use super::{AttestationMethod, MechanismAvailability, PresenceVerifier, UnavailableReason};

type Id = *mut c_void;
type Sel = *mut c_void;

#[link(name = "objc")]
extern "C" {
    fn objc_getClass(name: *const c_char) -> Id;
    fn sel_registerName(name: *const c_char) -> Sel;
    fn objc_msgSend();
}

// Force-links the frameworks so `LAContext` / `NSString` register with the
// runtime; without these the class lookup returns null.
#[link(name = "LocalAuthentication", kind = "framework")]
extern "C" {}
#[link(name = "Foundation", kind = "framework")]
extern "C" {}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRunLoopRunInMode(mode: *const c_void, seconds: f64, return_after_source: u8) -> i32;
    static kCFRunLoopDefaultMode: *const c_void;
}

extern "C" {
    static _NSConcreteStackBlock: *const c_void;
}

/// `LAPolicyDeviceOwnerAuthentication` — biometry, watch, **or** the login
/// credential.
///
/// Deliberately not the biometrics-only policy (`= 1`): OQ-1 chose this one so
/// biometry-absent hardware degrades to the device passcode rather than to
/// nothing.
const LA_POLICY_DEVICE_OWNER_AUTHENTICATION: isize = 2;

/// How long we leave the prompt up before giving up on it.
///
/// Matches [`crate::consent::CONSENT_TIMEOUT`], because this prompt sits inside
/// that window: an attestation that outlived the consent request it authorizes
/// could only ever mint nothing, and would leave a dialog on screen after the
/// thing it was for had already timed out.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(30);

// LAError codes, as observed in the BR-12 spike and mapped onto BR-7's endings.
const LA_ERROR_AUTHENTICATION_FAILED: i64 = -1;
const LA_ERROR_USER_CANCEL: i64 = -2;
const LA_ERROR_USER_FALLBACK: i64 = -3;
const LA_ERROR_SYSTEM_CANCEL: i64 = -4;
const LA_ERROR_PASSCODE_NOT_SET: i64 = -5;
const LA_ERROR_BIOMETRY_NOT_AVAILABLE: i64 = -6;
const LA_ERROR_BIOMETRY_NOT_ENROLLED: i64 = -7;
const LA_ERROR_APP_CANCEL: i64 = -9;

/// Reply state for the `evaluatePolicy` completion block.
///
/// Statics rather than a captured variable: the block ABI below would otherwise
/// need a heap-copied capture with a lifetime crossing the FFI boundary, and the
/// value being carried is a single settled outcome. `VERIFY_LOCK` serializes
/// prompts so two concurrent verifications cannot interleave through them.
static REPLY_STATE: AtomicI64 = AtomicI64::new(0); // 0 pending, 1 success, 2 failure
static REPLY_ERRCODE: AtomicI64 = AtomicI64::new(0);
static VERIFY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

unsafe fn cls(name: &str) -> Id {
    let c = CString::new(name).expect("a static class name has no interior nul");
    objc_getClass(c.as_ptr())
}

unsafe fn sel(name: &str) -> Sel {
    let c = CString::new(name).expect("a static selector has no interior nul");
    sel_registerName(c.as_ptr())
}

unsafe fn msg_id(recv: Id, s: Sel) -> Id {
    let f: unsafe extern "C" fn(Id, Sel) -> Id = std::mem::transmute(objc_msgSend as *const ());
    f(recv, s)
}

unsafe fn nsstring(s: &str) -> Id {
    let c = CString::new(s).unwrap_or_else(|_| CString::new("authenticate").unwrap());
    let f: unsafe extern "C" fn(Id, Sel, *const c_char) -> Id =
        std::mem::transmute(objc_msgSend as *const ());
    f(cls("NSString"), sel("stringWithUTF8String:"), c.as_ptr())
}

#[repr(C)]
struct BlockDescriptor {
    reserved: usize,
    size: usize,
}

static DESCRIPTOR: BlockDescriptor = BlockDescriptor {
    reserved: 0,
    size: std::mem::size_of::<ReplyBlock>(),
};

#[repr(C)]
struct ReplyBlock {
    isa: *const c_void,
    flags: i32,
    reserved: i32,
    invoke: unsafe extern "C" fn(*mut ReplyBlock, u8, Id),
    descriptor: *const BlockDescriptor,
}

unsafe extern "C" fn reply_invoke(_blk: *mut ReplyBlock, success: u8, error: Id) {
    if success != 0 {
        REPLY_STATE.store(1, Ordering::SeqCst);
        return;
    }
    if !error.is_null() {
        let f: unsafe extern "C" fn(Id, Sel) -> isize =
            std::mem::transmute(objc_msgSend as *const ());
        REPLY_ERRCODE.store(f(error, sel("code")) as i64, Ordering::SeqCst);
    }
    REPLY_STATE.store(2, Ordering::SeqCst);
}

/// Map an `LAError` onto BR-7's endings.
///
/// The three that must stay apart are failure, cancellation and timeout: they
/// have different remedies, and a user who cancelled by accident must not be
/// told the same thing as one whose hardware is missing.
fn refusal_for(code: i64) -> AttestationRefusal {
    match code {
        LA_ERROR_AUTHENTICATION_FAILED => AttestationRefusal::Failed,
        LA_ERROR_USER_CANCEL | LA_ERROR_SYSTEM_CANCEL | LA_ERROR_APP_CANCEL => {
            AttestationRefusal::Cancelled
        }
        // The user asked for the password route and we did not offer one; from
        // the daemon's side nobody authenticated, and nothing was cancelled by
        // the system — treat as a plain failure so the caller may re-ask.
        LA_ERROR_USER_FALLBACK => AttestationRefusal::Failed,
        LA_ERROR_PASSCODE_NOT_SET
        | LA_ERROR_BIOMETRY_NOT_AVAILABLE
        | LA_ERROR_BIOMETRY_NOT_ENROLLED => {
            AttestationRefusal::Unavailable(UnavailableReason::NoEnrolledCredential)
        }
        _ => AttestationRefusal::Failed,
    }
}

/// Asks a human through macOS `LocalAuthentication`.
#[derive(Debug, Default, Clone, Copy)]
pub struct LocalAuthenticationVerifier;

impl LocalAuthenticationVerifier {
    /// A verifier over the system's `LAContext`.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl PresenceVerifier for LocalAuthenticationVerifier {
    fn availability(&self) -> MechanismAvailability {
        unsafe {
            let class = cls("LAContext");
            if class.is_null() {
                return MechanismAvailability::Unavailable(UnavailableReason::PlatformUnsupported);
            }
            let ctx = msg_id(msg_id(class, sel("alloc")), sel("init"));
            if ctx.is_null() {
                return MechanismAvailability::Unavailable(UnavailableReason::PlatformUnsupported);
            }

            let mut err: Id = std::ptr::null_mut();
            let can: unsafe extern "C" fn(Id, Sel, isize, *mut Id) -> u8 =
                std::mem::transmute(objc_msgSend as *const ());
            let ok = can(
                ctx,
                sel("canEvaluatePolicy:error:"),
                LA_POLICY_DEVICE_OWNER_AUTHENTICATION,
                &mut err,
            ) != 0;
            let _ = msg_id(ctx, sel("release"));

            if ok {
                // `deviceOwnerAuthentication` may be satisfied by biometry or by
                // the login credential, and the OS does not tell us which until
                // the user answers. Reported as biometric where biometry is
                // enrolled; `verify` records what actually happened.
                MechanismAvailability::Available(AttestationMethod::OsBiometric)
            } else {
                MechanismAvailability::Unavailable(UnavailableReason::NoEnrolledCredential)
            }
        }
    }

    fn verify(
        &self,
        subject: ConnectionId,
        request: &RequestId,
    ) -> Result<PresenceAttestation, AttestationRefusal> {
        // Availability first, so an unusable mechanism produces BR-8's posture
        // refusal rather than a prompt that cannot appear.
        let method = match self.availability() {
            MechanismAvailability::Available(method) => method,
            MechanismAvailability::Unavailable(reason) => {
                return Err(AttestationRefusal::Unavailable(reason))
            }
        };

        // One prompt at a time. Two dialogs racing would be a consent-fatigue
        // surface as well as a data race on the reply statics.
        let _serialized = VERIFY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        REPLY_STATE.store(0, Ordering::SeqCst);
        REPLY_ERRCODE.store(0, Ordering::SeqCst);

        unsafe {
            let ctx = msg_id(msg_id(cls("LAContext"), sel("alloc")), sel("init"));
            if ctx.is_null() {
                return Err(AttestationRefusal::Unavailable(
                    UnavailableReason::PlatformUnsupported,
                ));
            }

            let mut block = ReplyBlock {
                isa: &_NSConcreteStackBlock as *const _ as *const c_void,
                flags: 0,
                reserved: 0,
                invoke: reply_invoke,
                descriptor: &DESCRIPTOR as *const BlockDescriptor,
            };
            // No session id, no path, no prompt text — conventions forbid
            // content in anything a user or a log may see.
            let reason = nsstring("approve a request to attach to one of your Teton sessions");

            let evaluate: unsafe extern "C" fn(Id, Sel, isize, Id, *mut ReplyBlock) =
                std::mem::transmute(objc_msgSend as *const ());
            let started = Instant::now();
            evaluate(
                ctx,
                sel("evaluatePolicy:localizedReason:reply:"),
                LA_POLICY_DEVICE_OWNER_AUTHENTICATION,
                reason,
                &mut block,
            );

            // Drive a private run loop: on the blocking pool there is no other.
            while started.elapsed() < PROMPT_TIMEOUT && REPLY_STATE.load(Ordering::SeqCst) == 0 {
                CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.1, 1);
            }

            let outcome = match REPLY_STATE.load(Ordering::SeqCst) {
                1 => {
                    PresenceAttestation::verified(method, subject, request.clone(), Instant::now())
                        .ok_or(AttestationRefusal::Failed)
                }
                2 => Err(refusal_for(REPLY_ERRCODE.load(Ordering::SeqCst))),
                _ => {
                    // Nobody answered. Take the dialog down rather than leaving
                    // it on the user's screen attached to a dead request
                    // (BR-7: no partial state).
                    let _ = msg_id(ctx, sel("invalidate"));
                    Err(AttestationRefusal::TimedOut)
                }
            };
            let _ = msg_id(ctx, sel("release"));
            outcome
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BR-7: the LAError mapping keeps the endings apart.
    ///
    /// Table-driven over the codes the spike and the framework actually produce,
    /// because this mapping is the *only* thing standing between six distinct
    /// OS outcomes and one undifferentiated "denied".
    #[test]
    fn la_error_codes_map_onto_distinguishable_endings() {
        let cases = [
            (LA_ERROR_AUTHENTICATION_FAILED, AttestationRefusal::Failed),
            (LA_ERROR_USER_CANCEL, AttestationRefusal::Cancelled),
            (LA_ERROR_SYSTEM_CANCEL, AttestationRefusal::Cancelled),
            // The one the BR-12 spike actually observed on invalidate.
            (LA_ERROR_APP_CANCEL, AttestationRefusal::Cancelled),
            (LA_ERROR_USER_FALLBACK, AttestationRefusal::Failed),
            (
                LA_ERROR_PASSCODE_NOT_SET,
                AttestationRefusal::Unavailable(UnavailableReason::NoEnrolledCredential),
            ),
            (
                LA_ERROR_BIOMETRY_NOT_AVAILABLE,
                AttestationRefusal::Unavailable(UnavailableReason::NoEnrolledCredential),
            ),
            (
                LA_ERROR_BIOMETRY_NOT_ENROLLED,
                AttestationRefusal::Unavailable(UnavailableReason::NoEnrolledCredential),
            ),
        ];
        for (code, expected) in cases {
            assert_eq!(refusal_for(code), expected, "LAError {code}");
        }
    }

    /// An unknown code fails closed rather than being read as success.
    #[test]
    fn an_unrecognised_la_error_is_a_failure_not_a_pass() {
        assert_eq!(refusal_for(-12345), AttestationRefusal::Failed);
        assert_eq!(refusal_for(0), AttestationRefusal::Failed);
    }
}
