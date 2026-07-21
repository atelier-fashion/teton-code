//! Daemon-side credential resolution (BR-7, REQ-544 M-3).
//!
//! The CLI *stores* a provider's secret in the OS keychain and writes only an
//! `auth_ref` — a reference like `keychain://teton/anthropic` — into the config
//! the daemon reads. The daemon is the *resolver*: at call time it turns that
//! reference back into the secret, injects it as the provider-appropriate
//! authorization header at the egress choke point, and forwards the request. The
//! secret never touches a file, a log, a `CostRecord`, or telemetry.
//!
//! The real store is the macOS Security framework, compiled only on macOS
//! (mirroring the CLI's `keychain.rs`). Other targets get [`UnsupportedBackend`]
//! so the daemon still builds on Linux CI, and tests inject a [`KeychainBackend`]
//! fake — **no test ever reads the real OS keychain** (the acceptance suite
//! configures providers with no `auth_ref`, so resolution is never invoked in
//! CI at all).
//!
//! ## Recognized reference forms
//!
//! Resolution dispatches on a positive scheme allowlist (the same allowlist the
//! config validator enforces, `teton_core::config`):
//!
//! - `keychain://<service>/<account>` — a generic-password lookup (the shape the
//!   CLI emits).
//! - `keychain:<account>` — shorthand for the default Teton service.
//! - `env:<VAR>` — read from the daemon's own environment.
//! - `op://…` — 1Password references are a recognized *config* form but not yet
//!   resolvable here; they surface a clear typed error rather than a panic.
//!
//! Every failure is a typed [`SecretError`] that names the *reference* (safe —
//! it is a pointer, not the credential) and a reason, and **never** the secret
//! value.

use std::fmt;

/// The keychain service Teton files credentials under when a reference does not
/// name one explicitly (mirrors the CLI's `SERVICE`).
const DEFAULT_SERVICE: &str = "teton";

/// A failure from the raw keychain backend. Content-free by construction — it
/// carries neither the secret nor the reference (the resolver attaches the
/// reference when mapping to a [`SecretError`]).
#[derive(Debug)]
pub enum BackendError {
    /// No entry exists for the requested `(service, account)`.
    NotFound,
    /// This platform has no supported keychain backend.
    Unsupported,
    /// The backend rejected the read. The message is backend-supplied and
    /// carries no secret material (BR-7).
    Backend(String),
}

/// The raw OS keychain backend — reads a generic password by `(service,
/// account)`. This is the injectable seam: production uses [`MacKeychainBackend`]
/// on macOS (or [`UnsupportedBackend`] elsewhere); tests inject a fake that
/// returns a canned secret so CI never touches the real store.
pub trait KeychainBackend: Send + Sync {
    /// Read the secret filed under `(service, account)`.
    ///
    /// # Errors
    /// Returns a [`BackendError`] when the entry is missing, the platform has no
    /// backend, or the backend rejects the read.
    fn get(&self, service: &str, account: &str) -> Result<String, BackendError>;
}

/// A credential could not be resolved from its `auth_ref`. Every variant names
/// the *reference* (a keychain pointer or env var name — safe to log) and a
/// reason, never the secret value (BR-7: no credential in errors or logs).
#[derive(Debug)]
pub enum SecretError {
    /// The reference has a recognized scheme but a malformed body.
    Malformed {
        /// The offending reference (safe — a pointer, not a secret).
        reference: String,
    },
    /// The reference's scheme is not one this resolver understands.
    UnknownScheme {
        /// The offending reference.
        reference: String,
    },
    /// A recognized scheme this resolver cannot yet resolve (e.g. `op://`).
    UnsupportedScheme {
        /// The offending reference.
        reference: String,
    },
    /// The reference is well-formed but its target does not exist (missing
    /// keychain entry or unset environment variable).
    NotFound {
        /// The reference that resolved to nothing.
        reference: String,
    },
    /// This platform has no keychain backend (non-macOS build).
    Unsupported {
        /// The reference that could not be resolved.
        reference: String,
    },
    /// The keychain backend rejected the read. `detail` is backend-supplied and
    /// content-free.
    Backend {
        /// The reference that failed to resolve.
        reference: String,
        /// A content-free backend detail.
        detail: String,
    },
}

impl fmt::Display for SecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretError::Malformed { reference } => write!(
                f,
                "auth_ref `{reference}` is malformed; expected \
                 `keychain://<service>/<account>`, `keychain:<account>`, or `env:<VAR>` (BR-7)"
            ),
            SecretError::UnknownScheme { reference } => write!(
                f,
                "auth_ref `{reference}` uses an unrecognized scheme; \
                 use a `keychain://`, `keychain:`, `env:`, or `op://` reference (BR-7)"
            ),
            SecretError::UnsupportedScheme { reference } => write!(
                f,
                "auth_ref `{reference}` uses a scheme this daemon cannot yet resolve; \
                 store the credential under a `keychain://` reference instead (BR-7)"
            ),
            SecretError::NotFound { reference } => write!(
                f,
                "no credential is stored for auth_ref `{reference}`; \
                 add it with `teton provider add` (BR-7)"
            ),
            SecretError::Unsupported { reference } => write!(
                f,
                "cannot resolve auth_ref `{reference}`: no OS keychain is available on this platform"
            ),
            SecretError::Backend { reference, detail } => write!(
                f,
                "keychain backend error resolving auth_ref `{reference}`: {detail}"
            ),
        }
    }
}

impl std::error::Error for SecretError {}

impl BackendError {
    /// Attach `reference` to a raw backend failure to make it a resolver-level
    /// [`SecretError`] (which the daemon may safely surface and log).
    fn with_reference(self, reference: &str) -> SecretError {
        match self {
            BackendError::NotFound => SecretError::NotFound {
                reference: reference.to_owned(),
            },
            BackendError::Unsupported => SecretError::Unsupported {
                reference: reference.to_owned(),
            },
            BackendError::Backend(detail) => SecretError::Backend {
                reference: reference.to_owned(),
                detail,
            },
        }
    }
}

/// Resolves an `auth_ref` to a secret through a [`KeychainBackend`].
///
/// The backend is held behind a trait so the resolver is unit-testable with a
/// fake — the daemon builds one with [`SecretResolver::with_default_backend`],
/// tests build one with [`SecretResolver::with_backend`].
pub struct SecretResolver {
    backend: Box<dyn KeychainBackend>,
}

impl SecretResolver {
    /// A resolver over the platform's default keychain backend (the real macOS
    /// Security framework; [`UnsupportedBackend`] elsewhere).
    #[must_use]
    pub fn with_default_backend() -> Self {
        Self {
            backend: default_backend(),
        }
    }

    /// A resolver over an explicit backend (tests inject a fake).
    #[must_use]
    pub fn with_backend(backend: Box<dyn KeychainBackend>) -> Self {
        Self { backend }
    }

    /// Resolve `auth_ref` to its secret.
    ///
    /// Dispatches on the reference scheme (positive allowlist). A resolution
    /// failure is a typed [`SecretError`] that names the reference and reason but
    /// never the secret — never a panic (BR-7).
    ///
    /// # Errors
    /// Returns a [`SecretError`] when the reference is malformed, its scheme is
    /// unknown/unsupported, its target does not exist, or the backend fails.
    pub fn resolve(&self, auth_ref: &str) -> Result<String, SecretError> {
        let reference = auth_ref.trim();

        if let Some(rest) = reference.strip_prefix("keychain://") {
            let (service, account) =
                rest.split_once('/').ok_or_else(|| SecretError::Malformed {
                    reference: reference.to_owned(),
                })?;
            if service.is_empty() || account.is_empty() {
                return Err(SecretError::Malformed {
                    reference: reference.to_owned(),
                });
            }
            return self
                .backend
                .get(service, account)
                .map_err(|e| e.with_reference(reference));
        }

        if let Some(account) = reference.strip_prefix("keychain:") {
            if account.is_empty() {
                return Err(SecretError::Malformed {
                    reference: reference.to_owned(),
                });
            }
            return self
                .backend
                .get(DEFAULT_SERVICE, account)
                .map_err(|e| e.with_reference(reference));
        }

        if let Some(var) = reference.strip_prefix("env:") {
            if var.is_empty() {
                return Err(SecretError::Malformed {
                    reference: reference.to_owned(),
                });
            }
            return std::env::var(var).map_err(|_| SecretError::NotFound {
                reference: reference.to_owned(),
            });
        }

        if reference.starts_with("op://") {
            return Err(SecretError::UnsupportedScheme {
                reference: reference.to_owned(),
            });
        }

        Err(SecretError::UnknownScheme {
            reference: reference.to_owned(),
        })
    }
}

/// The platform's default keychain backend.
#[must_use]
pub fn default_backend() -> Box<dyn KeychainBackend> {
    #[cfg(target_os = "macos")]
    {
        Box::new(MacKeychainBackend)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Box::new(UnsupportedBackend)
    }
}

/// The macOS Security-framework generic-password reader (BR-7, macOS-first).
#[cfg(target_os = "macos")]
pub struct MacKeychainBackend;

#[cfg(target_os = "macos")]
impl KeychainBackend for MacKeychainBackend {
    fn get(&self, service: &str, account: &str) -> Result<String, BackendError> {
        match security_framework::passwords::get_generic_password(service, account) {
            Ok(bytes) => String::from_utf8(bytes)
                .map_err(|_| BackendError::Backend("credential is not valid UTF-8".to_owned())),
            Err(e) => {
                // errSecItemNotFound (-25300) means "no such entry" — a clean
                // NotFound rather than an opaque backend error.
                if e.code() == -25300 {
                    Err(BackendError::NotFound)
                } else {
                    Err(BackendError::Backend(e.to_string()))
                }
            }
        }
    }
}

/// The fallback backend for platforms with no wired keychain: every read fails
/// cleanly rather than inventing a credential.
#[cfg(not(target_os = "macos"))]
pub struct UnsupportedBackend;

#[cfg(not(target_os = "macos"))]
impl KeychainBackend for UnsupportedBackend {
    fn get(&self, _service: &str, _account: &str) -> Result<String, BackendError> {
        Err(BackendError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// An in-memory keychain fake: proves resolution without touching the real
    /// OS store. Keyed by `(service, account)`.
    #[derive(Default)]
    struct FakeKeychain {
        entries: HashMap<(String, String), String>,
    }

    impl FakeKeychain {
        fn with(service: &str, account: &str, secret: &str) -> Self {
            let mut entries = HashMap::new();
            entries.insert((service.to_owned(), account.to_owned()), secret.to_owned());
            Self { entries }
        }
    }

    impl KeychainBackend for FakeKeychain {
        fn get(&self, service: &str, account: &str) -> Result<String, BackendError> {
            self.entries
                .get(&(service.to_owned(), account.to_owned()))
                .cloned()
                .ok_or(BackendError::NotFound)
        }
    }

    fn resolver_with(fake: FakeKeychain) -> SecretResolver {
        SecretResolver::with_backend(Box::new(fake))
    }

    #[test]
    fn resolves_a_full_keychain_reference() {
        let r = resolver_with(FakeKeychain::with("teton", "anthropic", "sk-ant-INJECTED"));
        assert_eq!(
            r.resolve("keychain://teton/anthropic").unwrap(),
            "sk-ant-INJECTED"
        );
    }

    #[test]
    fn resolves_the_shorthand_keychain_reference_under_the_default_service() {
        let r = resolver_with(FakeKeychain::with("teton", "deepseek", "sk-deepseek"));
        assert_eq!(r.resolve("keychain:deepseek").unwrap(), "sk-deepseek");
    }

    #[test]
    fn resolves_an_env_reference_without_touching_the_keychain() {
        // A distinctive var name so the test is hermetic.
        let var = format!("TETON_TEST_ENV_REF_{}", std::process::id());
        // SAFETY: single-threaded test setup; the var is process-unique.
        unsafe { std::env::set_var(&var, "env-secret-value") };
        let r = resolver_with(FakeKeychain::default());
        assert_eq!(
            r.resolve(&format!("env:{var}")).unwrap(),
            "env-secret-value"
        );
        unsafe { std::env::remove_var(&var) };
    }

    #[test]
    fn a_missing_keychain_entry_is_a_clear_typed_error_not_a_panic() {
        // REQ-544 M-3: resolution failure surfaces a typed error that names the
        // reference and a reason, never a panic and never the secret.
        let r = resolver_with(FakeKeychain::default());
        let err = r.resolve("keychain://teton/anthropic").unwrap_err();
        assert!(matches!(err, SecretError::NotFound { .. }));
        let msg = err.to_string();
        assert!(msg.contains("keychain://teton/anthropic"), "{msg}");
        assert!(msg.contains("BR-7"), "{msg}");
    }

    #[test]
    fn an_unset_env_reference_is_not_found() {
        let r = resolver_with(FakeKeychain::default());
        let err = r
            .resolve("env:TETON_DEFINITELY_UNSET_VAR_XYZZY")
            .unwrap_err();
        assert!(matches!(err, SecretError::NotFound { .. }));
    }

    #[test]
    fn unknown_and_unsupported_schemes_are_rejected_without_a_panic() {
        let r = resolver_with(FakeKeychain::default());
        assert!(matches!(
            r.resolve("op://vault/item").unwrap_err(),
            SecretError::UnsupportedScheme { .. }
        ));
        assert!(matches!(
            r.resolve("sk-ant-raw-key").unwrap_err(),
            SecretError::UnknownScheme { .. }
        ));
        assert!(matches!(
            r.resolve("keychain://teton/").unwrap_err(),
            SecretError::Malformed { .. }
        ));
        assert!(matches!(
            r.resolve("keychain:").unwrap_err(),
            SecretError::Malformed { .. }
        ));
    }

    #[test]
    fn error_display_never_echoes_a_secret() {
        // The reference is safe (a pointer); the secret never appears anywhere in
        // a resolver error, by construction (we cannot even produce one on the
        // failure path). This guards the invariant against future edits.
        let err = SecretError::NotFound {
            reference: "keychain://teton/anthropic".to_owned(),
        };
        let msg = err.to_string();
        assert!(!msg.contains("sk-"), "{msg}");
    }
}
