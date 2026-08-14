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
/// The CLI only ever *writes* a credential — the daemon is the resolver that
/// reads it back at call time (surface parity, BR-4), so there is deliberately
/// no `retrieve` on this client-side trait.
pub trait Keychain {
    /// Store `secret` for `account` under the Teton service and return the
    /// `auth_ref` that references it. The secret is never returned or logged.
    ///
    /// # Errors
    ///
    /// Returns a [`KeychainError`] if the backend rejects the write.
    fn store(&self, account: &str, secret: &str) -> Result<String, KeychainError>;

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
}

#[cfg(test)]
impl MockKeychain {
    pub fn new() -> Self {
        Self::default()
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

    fn delete(&self, account: &str) -> Result<(), KeychainError> {
        self.deletes.borrow_mut().push(account.to_owned());
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
}
