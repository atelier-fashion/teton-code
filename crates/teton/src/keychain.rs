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
    /// read half of a read-before-write, and it has exactly one caller: the
    /// `/web setup` flow, which stores into the fixed `web-search` account and
    /// must be able to restore what it displaced when the commit that write was
    /// made for is refused (REQ-572 BR-11 — "removes any keychain entry the
    /// aborted flow run itself created", which a rotation did not create).
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
    /// The undo half of [`Self::store`], and it exists for one caller: the
    /// `/web setup` flow writes the key immediately before the commit RPC it was
    /// collected for, so a commit the daemon refuses must take the entry back
    /// out. Without this, a refused setup would leave a credential on the
    /// machine that no config references and nothing will ever read — a residue
    /// the user did not agree to and cannot see.
    ///
    /// # Errors
    ///
    /// Returns [`KeychainError::NotFound`] when there is no such entry (which
    /// is a state, not a failure — the caller has nothing left to clean up) and
    /// a [`KeychainError::Backend`] when the store refuses the removal.
    fn delete(&self, account: &str) -> Result<(), KeychainError>;
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
}

#[cfg(test)]
impl MockKeychain {
    pub fn new() -> Self {
        Self::default()
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
        self.entries
            .borrow_mut()
            .insert(account.to_owned(), secret.to_owned());
        Ok(auth_ref_for(account))
    }

    fn read(&self, account: &str) -> Result<Option<String>, KeychainError> {
        if let Some(message) = self.read_failure.borrow().as_deref() {
            return Err(KeychainError::Backend(message.to_owned()));
        }
        Ok(self.entries.borrow().get(account).cloned())
    }

    fn delete(&self, account: &str) -> Result<(), KeychainError> {
        // Recorded before the injected failure is consulted: a delete that was
        // *attempted* and refused is still an attempt, and the record is what
        // distinguishes "the flow never tried" from "the flow tried and the
        // store said no".
        self.deletes.borrow_mut().push(account.to_owned());
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
}
