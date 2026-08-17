//! Credential storage behind a trait (BR-7).
//!
//! API keys and tokens are stored **only** in the OS keychain; config files and
//! the protocol carry a *reference* (`auth_ref`), never the secret. `provider
//! add` calls [`Keychain::store`], gets back an `auth_ref` like
//! `keychain://teton/anthropic`, and puts only that reference in the
//! [`ProviderConfig`](teton_protocol::methods::ProviderConfig) it sends to the
//! daemon. The secret never touches a file, a log, or a `CostRecord`.
//!
//! The real store is the macOS Security framework, compiled only on macOS. Other
//! targets get [`UnsupportedKeychain`] (so the crate still builds on Linux CI),
//! and tests use an in-memory mock — no test ever reads or writes the real
//! keychain.

use std::fmt;

/// The keychain service name all Teton credentials are filed under.
// Used by the macOS backend and by tests; on other targets the real backend is
// absent, so this is deliberately unused there.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
pub const SERVICE: &str = "teton";

/// The scheme prefix of an `auth_ref`.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
const AUTH_REF_SCHEME: &str = "keychain://";

/// Something went wrong reaching or using the credential store. Which variants
/// are constructed depends on the active backend (macOS vs. unsupported) and on
/// whether tests are compiled, so the enum as a whole allows unused variants.
#[allow(dead_code)]
#[derive(Debug)]
pub enum KeychainError {
    /// This platform has no supported keychain backend.
    Unsupported,
    /// The backend rejected the operation. The message is backend-supplied and
    /// deliberately carries no secret material (BR-7: no credential in errors).
    Backend(String),
    /// No entry exists for the requested reference.
    NotFound,
    /// An `auth_ref` string was not the expected `keychain://service/account`
    /// shape.
    MalformedRef,
}

impl fmt::Display for KeychainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeychainError::Unsupported => {
                f.write_str("no OS keychain is available on this platform")
            }
            KeychainError::Backend(msg) => write!(f, "keychain backend error: {msg}"),
            KeychainError::NotFound => f.write_str("no keychain entry for that reference"),
            KeychainError::MalformedRef => f.write_str("malformed keychain reference"),
        }
    }
}

impl std::error::Error for KeychainError {}

/// The stable `auth_ref` for an account, e.g. `keychain://teton/anthropic`.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
#[must_use]
pub fn auth_ref_for(account: &str) -> String {
    format!("{AUTH_REF_SCHEME}{SERVICE}/{account}")
}

/// Splits an `auth_ref` back into `(service, account)`.
///
/// # Errors
///
/// Returns [`KeychainError::MalformedRef`] when `auth_ref` is not
/// `keychain://<service>/<account>`.
// The daemon is the production resolver; on the client side this parser only
// backs tests, so it is unused in non-test builds on every platform.
#[cfg_attr(not(test), allow(dead_code))]
pub fn parse_auth_ref(auth_ref: &str) -> Result<(String, String), KeychainError> {
    let rest = auth_ref
        .strip_prefix(AUTH_REF_SCHEME)
        .ok_or(KeychainError::MalformedRef)?;
    let (service, account) = rest.split_once('/').ok_or(KeychainError::MalformedRef)?;
    if service.is_empty() || account.is_empty() {
        return Err(KeychainError::MalformedRef);
    }
    Ok((service.to_owned(), account.to_owned()))
}

/// A credential store. The secret goes in; only the `auth_ref` comes back.
///
/// The CLI *writes* credentials and the daemon is the resolver that reads them
/// back at call time (surface parity, BR-4) — so [`Self::read`] is deliberately
/// not a resolution path. It exists for one thing: [`Self::store`] on an account
/// that already holds a credential is a **destructive** write, and the only way
/// to put the old one back if the operation it was written for fails is to have
/// read it first.
pub trait Keychain {
    /// Store `secret` for `account` under the Teton service and return the
    /// `auth_ref` that references it. The secret is never returned or logged.
    ///
    /// **Overwrites.** An account that already holds a credential loses it, and
    /// the caller is the only one that can know whether that matters — see
    /// [`Self::read`].
    ///
    /// # Errors
    ///
    /// Returns a [`KeychainError`] if the backend rejects the write.
    fn store(&self, account: &str, secret: &str) -> Result<String, KeychainError>;

    /// The credential currently filed for `account`, or `None` when there is
    /// none.
    ///
    /// **Not a resolver.** The daemon resolves `auth_ref`s at call time; nothing
    /// on this side reads a credential in order to *use* it. This is the
    /// read half of a read-before-write, reached only through
    /// [`PriorKey::read`]: the flows that store a credential immediately before
    /// the commit RPC it was collected for — `/web setup` (REQ-572 BR-11) and
    /// `teton provider add` (BUG-171) — must be able to restore what the store
    /// displaced when that commit is refused (BR-11's "removes any keychain
    /// entry the aborted flow run itself created", which a rotation did not
    /// create).
    ///
    /// `Ok(None)` and an error are deliberately different answers: "there is
    /// nothing here" licenses a delete, and "I could not find out" licenses
    /// neither a delete nor a restore.
    ///
    /// # Errors
    ///
    /// Returns a [`KeychainError`] when the backend refuses the read or hands
    /// back bytes that are not a UTF-8 credential. A missing entry is `Ok(None)`
    /// rather than [`KeychainError::NotFound`], because absence is the state
    /// this call exists to distinguish and not a failure to answer.
    fn read(&self, account: &str) -> Result<Option<String>, KeychainError>;

    /// Remove the entry for `account` (REQ-572 ADR-3).
    ///
    /// The undo half of [`Self::store`], reached only through
    /// [`PriorKey::undo`]: the flows that write a key immediately before the
    /// commit RPC it was collected for — `/web setup` and `teton provider add`
    /// (BUG-171) — must take the entry back out when the daemon refuses that
    /// commit. Without this, a refused setup would leave a credential on the
    /// machine that no config references and nothing will ever read — a residue
    /// the user did not agree to and cannot see.
    ///
    /// # Errors
    ///
    /// Returns [`KeychainError::NotFound`] when there is no such entry (which
    /// is a state, not a failure — the caller has nothing left to clean up) and
    /// a [`KeychainError::Backend`] when the store refuses the removal.
    fn delete(&self, account: &str) -> Result<(), KeychainError>;

    /// Whether this platform has a credential store at all.
    ///
    /// A **platform** fact, not a health check: it answers "is there a backend
    /// here", never "is the keychain currently unlocked". A locked or refusing
    /// store is still available, and its failure is the caller's to report.
    ///
    /// It exists so a flow that is about to *ask a human for a secret* can find
    /// out first. Every other way to learn this costs the user the question:
    /// [`Self::store`] answers it only after the key has been typed, and
    /// probing with a [`Self::read`] of some invented account would mean a
    /// collection flow touching an entry that is not its own. A walkthrough
    /// that asks for a credential and only then says it has nowhere to put it
    /// is the dead end REQ-579's degradation rule exists to prevent
    /// (requirement Assumptions): the honest answer is the out-of-band recipe,
    /// printed before the first question.
    ///
    /// Defaulted to `true` because every backend that exists is a backend: only
    /// [`UnsupportedKeychain`] — the platform that has none — overrides it.
    fn is_available(&self) -> bool {
        true
    }
}

/// Returns the platform's default keychain implementation.
#[must_use]
pub fn default_keychain() -> Box<dyn Keychain> {
    #[cfg(target_os = "macos")]
    {
        Box::new(MacKeychain)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Box::new(UnsupportedKeychain)
    }
}

/// The macOS Security-framework generic-password store (BR-7, macOS-first).
#[cfg(target_os = "macos")]
pub struct MacKeychain;

/// The Security-framework status for "no such item", which is the one backend
/// failure that is really a *state*: there is nothing to delete.
///
/// Named rather than matched as a bare number so the mapping below reads as the
/// classification it is, and matched on the code rather than on the message
/// string for the reason every classifier in this codebase is (LESSON-456).
#[cfg(target_os = "macos")]
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

#[cfg(target_os = "macos")]
impl Keychain for MacKeychain {
    fn store(&self, account: &str, secret: &str) -> Result<String, KeychainError> {
        security_framework::passwords::set_generic_password(SERVICE, account, secret.as_bytes())
            .map_err(|e| KeychainError::Backend(e.to_string()))?;
        Ok(auth_ref_for(account))
    }

    fn read(&self, account: &str) -> Result<Option<String>, KeychainError> {
        match security_framework::passwords::get_generic_password(SERVICE, account) {
            Ok(bytes) => String::from_utf8(bytes)
                .map(Some)
                // The bytes are not shown, quoted or logged — only the fact that
                // they are not a credential this side could put back.
                .map_err(|_| {
                    KeychainError::Backend("the stored credential is not valid UTF-8".to_owned())
                }),
            // The one backend failure that is really a *state*, classified on
            // the code rather than the message (LESSON-456), exactly as the
            // delete below does.
            Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
            Err(e) => Err(KeychainError::Backend(e.to_string())),
        }
    }

    fn delete(&self, account: &str) -> Result<(), KeychainError> {
        security_framework::passwords::delete_generic_password(SERVICE, account).map_err(|e| {
            if e.code() == ERR_SEC_ITEM_NOT_FOUND {
                KeychainError::NotFound
            } else {
                KeychainError::Backend(e.to_string())
            }
        })
    }
}

/// The fallback for platforms without a wired keychain backend. Every operation
/// fails cleanly with [`KeychainError::Unsupported`] rather than silently writing
/// a secret to disk.
#[cfg(not(target_os = "macos"))]
pub struct UnsupportedKeychain;

#[cfg(not(target_os = "macos"))]
impl Keychain for UnsupportedKeychain {
    fn store(&self, _account: &str, _secret: &str) -> Result<String, KeychainError> {
        Err(KeychainError::Unsupported)
    }

    /// `Err`, never `Ok(None)`: this platform cannot tell an empty store from a
    /// store it cannot see, and answering "there is nothing there" would license
    /// a caller to delete an entry it never established was absent.
    fn read(&self, _account: &str) -> Result<Option<String>, KeychainError> {
        Err(KeychainError::Unsupported)
    }

    fn delete(&self, _account: &str) -> Result<(), KeychainError> {
        Err(KeychainError::Unsupported)
    }

    /// The one implementation that says no — this is the platform the flag
    /// exists to name.
    fn is_available(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// The read-before-write undo (REQ-572 ADR-3/BR-11; generalized for BUG-171)
// ---------------------------------------------------------------------------

/// What the keychain held for an account *before* a flow stored anything there.
///
/// Read once, immediately before the store, because the store is what destroys
/// the answer — and the account is captured *inside* the value, so the undo
/// cannot be aimed at a different entry than the one that was inspected (one
/// spelling; a second copy of the account is how a flow comes to delete an
/// entry it never read). Which state this is decides what a refused commit
/// owes the machine, and the three are genuinely three, not a `bool` with a
/// failure case: "nothing was here" licenses a delete, "this was here" obliges
/// a restore, and "I could not find out" licenses neither.
///
/// Built for `/web setup` (REQ-572 BR-11) and adopted by `teton provider add`
/// (BUG-171) — the two places a credential is typed at a terminal and stored
/// for a commit the daemon may still refuse.
pub struct PriorKey {
    /// The account the pre-store read inspected — the only entry the undo
    /// will touch.
    account: String,
    held: Held,
}

/// The three answers a pre-store read can produce.
enum Held {
    /// The account was empty. The flow's store created the entry, so the undo
    /// is to remove it (BR-11's "any keychain entry the aborted flow run
    /// itself created").
    Absent,
    /// The account already held a credential. Whatever references that entry is
    /// still pointing at it, so the undo is to put those exact bytes back — a
    /// delete here destroys a working setup the user never agreed to give up.
    Present(String),
    /// The store could not be read. Both undos are unsafe: the delete might
    /// take out a credential in use, and there is nothing to restore.
    Unreadable(KeychainError),
}

/// Hand-written because `Present` holds the displaced credential's plaintext,
/// and a derived `Debug` would print a live key into any `{:?}`, panic message,
/// or future test assertion — the exact residue the REQ-572 AC-5 sweep exists
/// to catch.
impl fmt::Debug for PriorKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.held {
            Held::Absent => write!(f, "PriorKey::Absent(`{}`)", self.account),
            Held::Present(_) => write!(f, "PriorKey::Present(`{}`, <redacted>)", self.account),
            Held::Unreadable(err) => write!(f, "PriorKey::Unreadable(`{}`, {err})", self.account),
        }
    }
}

impl PriorKey {
    /// Read `account`, classifying a missing entry as a state rather than a
    /// failure.
    ///
    /// A read failure does **not** stop the calling flow. It is a transient
    /// backend condition on the one platform that has a backend at all, the
    /// user has asked for this key to be stored, and refusing here would trade
    /// a hypothetical loss for a certain one. What it does is downgrade the
    /// undo to "leave it alone and say so".
    pub fn read(keychain: &dyn Keychain, account: &str) -> Self {
        let held = match keychain.read(account) {
            Ok(Some(existing)) => Held::Present(existing),
            Ok(None) => Held::Absent,
            Err(err) => Held::Unreadable(err),
        };
        Self {
            account: account.to_owned(),
            held,
        }
    }

    /// Whether the store this value was read ahead of displaced a credential
    /// that was already filed there.
    pub fn displaced(&self) -> bool {
        matches!(self.held, Held::Present(_))
    }

    /// Undo the store this value was read ahead of, given what it displaced.
    pub fn undo(&self, keychain: &dyn Keychain) -> Cleanup {
        match &self.held {
            Held::Absent => Cleanup::Deleted(keychain.delete(&self.account)),
            Held::Present(previous) => {
                Cleanup::Restored(keychain.store(&self.account, previous).map(|_| ()))
            }
            // The reason travels with the decision: "left alone" without
            // "because your keychain would not answer" reads as the flow
            // shrugging.
            Held::Unreadable(err) => Cleanup::LeftInPlace(err.to_string()),
        }
    }
}

/// What the failure path did about the entry a flow wrote.
#[derive(Debug)]
pub enum Cleanup {
    /// The entry the flow created was removed — or the removal was refused.
    Deleted(Result<(), KeychainError>),
    /// The credential the flow displaced was put back — or could not be.
    Restored(Result<(), KeychainError>),
    /// Nothing was touched, because nothing could be shown to be the safe move.
    /// Carries why the store could not be read.
    LeftInPlace(String),
}

/// An in-memory keychain for tests: it proves a secret was captured out of the
/// config path without ever touching the real OS store.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct MockKeychain {
    entries: std::cell::RefCell<std::collections::HashMap<String, String>>,
    /// Every account [`Keychain::delete`] was called for, in order — including
    /// the calls that found nothing.
    ///
    /// Recorded rather than merely applied, because REQ-572's residual rule is
    /// about the call being *made*: a flow that abandoned a stored key would
    /// leave an empty map either way, and only the record tells "never stored"
    /// from "stored and taken back out".
    deletes: std::cell::RefCell<Vec<String>>,
    /// When set, every [`Keychain::delete`] fails with this backend message
    /// instead of removing anything.
    ///
    /// A mock that can only succeed makes the *reporting* of a failed cleanup
    /// untestable, and that reporting is the whole remedy the user has: they are
    /// the only one who can remove an entry the flow could not. Without this
    /// seam the recovery instruction is a line nothing ever reads.
    delete_failure: std::cell::RefCell<Option<String>>,
    /// When set, every [`Keychain::read`] fails with this backend message.
    ///
    /// The other half of the same argument: a store that cannot find out what it
    /// is about to displace must neither delete nor restore, and that third
    /// branch is only reachable through a read that fails.
    read_failure: std::cell::RefCell<Option<String>>,
    /// When set, this mock stands in for [`UnsupportedKeychain`] — a platform
    /// with no backend at all.
    ///
    /// Spelled as the *negative* so `Default` still produces the ordinary,
    /// working store every other test wants.
    ///
    /// It exists because the real thing cannot be constructed here:
    /// `UnsupportedKeychain` is `#[cfg(not(target_os = "macos"))]`, so on the
    /// platform this project is developed and tested on, the no-keychain
    /// degradation would otherwise be reachable only from CI on another target
    /// — which is to say, not covered where it is written.
    unavailable: bool,
}

#[cfg(test)]
impl MockKeychain {
    pub fn new() -> Self {
        Self::default()
    }

    /// A store that stands in for a platform with no keychain backend.
    ///
    /// Every operation fails with [`KeychainError::Unsupported`], exactly as
    /// [`UnsupportedKeychain`]'s do — so a flow that ignored
    /// [`Keychain::is_available`] and asked for a key anyway still cannot
    /// pretend it stored one.
    pub fn unavailable() -> Self {
        Self {
            unavailable: true,
            ..Self::default()
        }
    }

    /// Make every subsequent [`Keychain::delete`] fail with `message`.
    pub fn fail_delete_with(&self, message: &str) {
        *self.delete_failure.borrow_mut() = Some(message.to_owned());
    }

    /// Make every subsequent [`Keychain::read`] fail with `message`.
    pub fn fail_read_with(&self, message: &str) {
        *self.read_failure.borrow_mut() = Some(message.to_owned());
    }

    /// The secret stored for `account`, if any — lets a test assert the key
    /// landed in the keychain and not in the config.
    pub fn stored_secret(&self, account: &str) -> Option<String> {
        self.entries.borrow().get(account).cloned()
    }

    /// Whether the store holds nothing at all — the state every aborted flow
    /// must leave behind (REQ-572 AC-6).
    pub fn is_empty(&self) -> bool {
        self.entries.borrow().is_empty()
    }

    /// The accounts [`Keychain::delete`] was called for, in order.
    pub fn deletes(&self) -> Vec<String> {
        self.deletes.borrow().clone()
    }
}

#[cfg(test)]
impl Keychain for MockKeychain {
    fn store(&self, account: &str, secret: &str) -> Result<String, KeychainError> {
        if self.unavailable {
            return Err(KeychainError::Unsupported);
        }
        self.entries
            .borrow_mut()
            .insert(account.to_owned(), secret.to_owned());
        Ok(auth_ref_for(account))
    }

    fn read(&self, account: &str) -> Result<Option<String>, KeychainError> {
        if self.unavailable {
            return Err(KeychainError::Unsupported);
        }
        if let Some(message) = self.read_failure.borrow().as_deref() {
            return Err(KeychainError::Backend(message.to_owned()));
        }
        Ok(self.entries.borrow().get(account).cloned())
    }

    fn is_available(&self) -> bool {
        !self.unavailable
    }

    fn delete(&self, account: &str) -> Result<(), KeychainError> {
        // Recorded before the injected failure is consulted: a delete that was
        // *attempted* and refused is still an attempt, and the record is what
        // distinguishes "the flow never tried" from "the flow tried and the
        // store said no".
        self.deletes.borrow_mut().push(account.to_owned());
        if self.unavailable {
            return Err(KeychainError::Unsupported);
        }
        if let Some(message) = self.delete_failure.borrow().as_deref() {
            return Err(KeychainError::Backend(message.to_owned()));
        }
        match self.entries.borrow_mut().remove(account) {
            Some(_) => Ok(()),
            None => Err(KeychainError::NotFound),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_ref_matches_the_protocol_shape() {
        assert_eq!(auth_ref_for("anthropic"), "keychain://teton/anthropic");
    }

    #[test]
    fn parse_auth_ref_round_trips_and_rejects_junk() {
        let (service, account) = parse_auth_ref("keychain://teton/deepseek").unwrap();
        assert_eq!(service, "teton");
        assert_eq!(account, "deepseek");
        assert!(matches!(
            parse_auth_ref("not-a-ref"),
            Err(KeychainError::MalformedRef)
        ));
        assert!(matches!(
            parse_auth_ref("keychain://teton/"),
            Err(KeychainError::MalformedRef)
        ));
    }

    #[test]
    fn mock_keychain_stores_the_secret_and_returns_a_reference() {
        let kc = MockKeychain::new();
        let auth_ref = kc.store("anthropic", "sk-secret-value").unwrap();
        // Only a reference comes back; the secret is captured in the store.
        assert_eq!(auth_ref, "keychain://teton/anthropic");
        assert_eq!(
            kc.stored_secret("anthropic").as_deref(),
            Some("sk-secret-value")
        );
        // The reference parses back to its (service, account) parts (the shape
        // the daemon relies on to resolve it).
        assert_eq!(
            parse_auth_ref(&auth_ref).unwrap(),
            ("teton".to_owned(), "anthropic".to_owned())
        );
    }

    /// REQ-572 ADR-3: a stored key can be taken back out, and the removal is
    /// observable — which is what lets the `/web setup` tests prove a refused
    /// commit left nothing behind rather than merely asserting an empty map that
    /// was never written to.
    #[test]
    fn a_stored_secret_can_be_deleted_and_the_call_is_recorded() {
        let kc = MockKeychain::new();
        kc.store("web-search", "sk-search-value").unwrap();
        assert!(!kc.is_empty());

        kc.delete("web-search").unwrap();
        assert!(kc.is_empty(), "the entry must be gone, not merely marked");
        assert_eq!(kc.deletes(), vec!["web-search".to_owned()]);
        assert!(kc.stored_secret("web-search").is_none());

        // Deleting what is not there is a state, not a failure — and it is still
        // recorded, because "the flow tried to clean up" is the fact under test.
        assert!(matches!(
            kc.delete("web-search"),
            Err(KeychainError::NotFound)
        ));
        assert_eq!(kc.deletes().len(), 2);
    }

    /// A read tells "nothing is filed here" from "I could not find out", which
    /// is the distinction the whole restore-or-delete decision rests on: only
    /// the first of the two licenses a delete.
    #[test]
    fn a_read_distinguishes_an_empty_account_from_an_unreadable_store() {
        let kc = MockKeychain::new();
        assert_eq!(kc.read("web-search").unwrap(), None);

        kc.store("web-search", "sk-first").unwrap();
        assert_eq!(kc.read("web-search").unwrap().as_deref(), Some("sk-first"));

        // A store over an existing account replaces it — which is why the read
        // has to happen first.
        kc.store("web-search", "sk-second").unwrap();
        assert_eq!(kc.read("web-search").unwrap().as_deref(), Some("sk-second"));

        kc.fail_read_with("the keychain is locked");
        assert!(matches!(
            kc.read("web-search"),
            Err(KeychainError::Backend(_))
        ));
    }

    /// BUG-171: a store over an empty account owes the machine a delete — and
    /// the undo aims at the account the read inspected, which travels inside
    /// the value rather than being passed in a second time.
    #[test]
    fn a_prior_read_of_an_empty_account_licenses_a_delete() {
        let kc = MockKeychain::new();
        let prior = PriorKey::read(&kc, "opus");
        assert!(!prior.displaced());

        kc.store("opus", "sk-typed-this-run").unwrap();
        let cleanup = prior.undo(&kc);
        assert!(matches!(cleanup, Cleanup::Deleted(Ok(()))));
        assert!(kc.is_empty(), "the entry this flow created must be gone");
        assert_eq!(kc.deletes(), vec!["opus".to_owned()]);
    }

    /// BUG-171: a store over an occupied account owes the machine a restore of
    /// the exact displaced bytes — a delete here would destroy a credential
    /// something else may still reference.
    #[test]
    fn a_prior_read_of_an_occupied_account_obliges_a_restore() {
        let kc = MockKeychain::new();
        kc.store("opus", "sk-the-one-that-was-there").unwrap();

        let prior = PriorKey::read(&kc, "opus");
        assert!(prior.displaced());

        kc.store("opus", "sk-typed-this-run").unwrap();
        let cleanup = prior.undo(&kc);
        assert!(matches!(cleanup, Cleanup::Restored(Ok(()))));
        assert_eq!(
            kc.stored_secret("opus").as_deref(),
            Some("sk-the-one-that-was-there"),
            "the displaced credential must be back, byte for byte"
        );
        assert!(
            kc.deletes().is_empty(),
            "a restore is a store, not a delete"
        );
    }

    /// BUG-171: a read the backend refused licenses neither undo — the entry is
    /// left where it is and the reason rides along for the caller to say.
    #[test]
    fn an_unreadable_prior_leaves_the_entry_alone() {
        let kc = MockKeychain::new();
        kc.fail_read_with("the keychain is locked");
        let prior = PriorKey::read(&kc, "opus");
        assert!(!prior.displaced());

        *kc.read_failure.borrow_mut() = None;
        kc.store("opus", "sk-typed-this-run").unwrap();
        let cleanup = prior.undo(&kc);
        match &cleanup {
            Cleanup::LeftInPlace(why) => assert!(why.contains("locked"), "{why}"),
            other => panic!("expected LeftInPlace, got {other:?}"),
        }
        assert_eq!(
            kc.stored_secret("opus").as_deref(),
            Some("sk-typed-this-run"),
            "neither undo may run when the prior state is unknown"
        );
        assert!(kc.deletes().is_empty());
    }

    /// The displaced plaintext must never reach a `{:?}` — the hand-written
    /// `Debug` is the only thing standing between `Present` and a panic message
    /// that prints a live key.
    #[test]
    fn a_prior_key_debug_redacts_the_displaced_credential() {
        let kc = MockKeychain::new();
        kc.store("opus", "sk-the-one-that-was-there").unwrap();
        let rendered = format!("{:?}", PriorKey::read(&kc, "opus"));
        assert!(
            !rendered.contains("sk-the-one-that-was-there"),
            "a displaced credential leaked into Debug output"
        );
        assert!(rendered.contains("opus") && rendered.contains("redacted"));
    }

    /// The delete-failure seam: without it, the branch that tells the user their
    /// keychain still holds an entry nobody will read is unreachable from a test.
    #[test]
    fn an_injected_delete_failure_is_reported_and_leaves_the_entry() {
        let kc = MockKeychain::new();
        kc.store("web-search", "sk-search-value").unwrap();
        kc.fail_delete_with("the keychain is locked");

        assert!(matches!(
            kc.delete("web-search"),
            Err(KeychainError::Backend(_))
        ));
        assert_eq!(
            kc.stored_secret("web-search").as_deref(),
            Some("sk-search-value"),
            "a refused delete must leave the entry where it was"
        );
        assert_eq!(
            kc.deletes(),
            vec!["web-search".to_owned()],
            "an attempted-and-refused delete is still an attempt"
        );
    }

    /// Availability is a **platform** fact, and the mock's two constructors are
    /// the two platforms: a store that works, and one that has no backend at
    /// all. A locked store is still available — its failure is a different
    /// event, and a flow that conflated them would print the out-of-band recipe
    /// to a macOS user whose keychain was merely locked.
    #[test]
    fn an_unavailable_keychain_says_so_and_refuses_every_operation() {
        let working = MockKeychain::new();
        assert!(working.is_available());
        working.fail_read_with("the keychain is locked");
        working.fail_delete_with("the keychain is locked");
        assert!(
            working.is_available(),
            "a store that refuses an operation still exists"
        );

        let none = MockKeychain::unavailable();
        assert!(!none.is_available());
        assert!(matches!(
            none.store("kimi", "sk-typed-this-run"),
            Err(KeychainError::Unsupported)
        ));
        assert!(matches!(none.read("kimi"), Err(KeychainError::Unsupported)));
        assert!(matches!(
            none.delete("kimi"),
            Err(KeychainError::Unsupported)
        ));
        assert!(
            none.is_empty(),
            "a refused store keeps nothing, so a flow cannot believe it stored one"
        );
    }
}
