//! The one place a spawned child's environment is built (REQ-596).
//!
//! Two paths in this daemon hand a process to code the user did not write: the
//! `shell` tool runs a model-supplied command, and an MCP server is a
//! third-party `npx`/`uvx` package. Both used to decide independently what the
//! child inherits, and they disagreed — MCP composed from a positive allowlist,
//! `shell` filtered with a name-shaped denylist, and the denylist's own doc
//! comment conceded it was the weaker of the two. A credential whose variable
//! name missed every substring survived into `sh -c`, one `echo $VAR` from the
//! model's context and the next remote turn.
//!
//! So there is one composer, and the policy is its **parameters**:
//! [`compose_child_env`] takes the allowlist and the credential set from its
//! caller. Sharing is not achieved by widening one path's constant to cover the
//! other's needs — that would hand every spawned MCP server an increment made
//! for the shell tool, a regression in the path that was already right (BR-7.1).
//! The two constants are identical today; they still travel through the
//! parameter, so a later divergence is one line at a call site rather than a
//! fork of this function.
//!
//! ## Two signals, because they answer different questions
//!
//! [`SHELL_ENV_ALLOW`] reasons about **names**: a variable not on it is absent
//! whatever it holds, so a new variable in the daemon's environment can never
//! silently widen what a child sees (BR-2). [`looks_like_credential_url`]
//! reasons about **values**: it catches `scheme://user:pass@host`, the shape
//! `DATABASE_URL` takes, which no allowlist can see (BR-8). Neither subsumes
//! the other, and the second is the half of the old scrub that survived the
//! rewrite — deliberately, and with its tests (ADR-E).
//!
//! ## The credential set is pulled, not pushed
//!
//! `auth_ref = "env:<VAR>"` is a first-class credential form: the daemon knows
//! at config-load time exactly which variable names hold provider secrets, and
//! it used to never tell the scrubber. Now it does, through
//! [`set_credential_env_names_provider`] — a closure the daemon installs once at
//! bootstrap that reads the **live** config each time it is called. Not a
//! snapshot: a provider added mid-session is visible to the very next spawn, and
//! there is no second copy of the set to go stale (LESSON-539).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use teton_core::config::{is_recognized_auth_ref, Config};

/// The env var names the `shell` tool's child may inherit from the daemon.
///
/// The same twelve as `mcp::client::MCP_BASE_ENV_ALLOW`, and a separate
/// constant on purpose (BR-7.1): identical membership today, but widening one
/// must never widen the other.
///
/// BR-2.1's criterion for admitting a name is **a variable an ordinary
/// development command needs in order to run at all, which cannot hold a
/// credential** — both halves. Nothing outside the twelve cleared both, and the
/// rejections are recorded here because a reviewer cannot otherwise tell an
/// omission from a decision:
///
/// | Considered | Rejected because |
/// |---|---|
/// | `SSH_AUTH_SOCK` | A `git push` over ssh wants it, so it passes the first half. It fails the second, and worse than by holding a credential: it is a handle to an agent that *lends* them. |
/// | `CARGO_HOME`, `RUSTUP_HOME` | Needed only when the layout is non-default; `HOME` covers the default, so "needs it to run at all" is not met. |
/// | `PWD`, `OLDPWD`, `SHLVL` | `sh` sets these itself from the child's working directory. Passing them in is redundant. |
/// | `LC_NUMERIC`, `LC_TIME`, `LC_COLLATE`, `LC_MESSAGES` | `LANG`/`LC_ALL`/`LC_CTYPE` already cover encoding, the half that breaks a command outright. The rest change formatting, not whether it runs. |
/// | `EDITOR`, `PAGER`, `COLUMNS`, `LINES` | Interactive conveniences. Nothing in a non-interactive `sh -c` needs them. |
///
/// Withholding an unexpected variable can break a user's command. The
/// requirement prices that trade explicitly: the alternative is silently
/// leaking a credential.
pub(crate) const SHELL_ENV_ALLOW: &[&str] = &[
    "PATH", "HOME", "TMPDIR", "TZ", "TERM", "USER", "LOGNAME", "SHELL", "LANG", "LANGUAGE",
    "LC_ALL", "LC_CTYPE",
];

/// Compose the environment a spawned child receives.
///
/// The **only** function in this workspace that builds one. A second
/// construction site is a second policy, and the two would drift; the region
/// check in this module's tests fails the build if one appears (AC-8).
///
/// Five steps, and the order is load-bearing:
///
/// 1. Keep only `daemon_vars` whose **name** is in `allow` (BR-2).
/// 2. Drop what survives if its **value** is a credential-bearing URL (BR-8).
///    Base slice only — never `declared`. A user who declares
///    `MY_DB=postgres://u:p@h` for their own MCP server declared it on purpose;
///    this step catches what the *daemon's* environment leaks in.
/// 3. Floor `PATH` (BR-4). Before `declared`, so a child that declares its own
///    `PATH` overrides the floor untouched — the existing MCP semantics,
///    preserved.
/// 4. Layer `declared` on top; a declared var overrides a base one.
/// 5. Remove every name in `credential_env_names`, **unconditionally and last**
///    (BR-1, BR-3).
///
/// Step 5 runs after step 4 rather than merely after step 1. BR-3 asks only that
/// the allowlist cannot re-admit a credential; running last means a *declared*
/// var cannot re-admit one either. Strictly stronger, and free.
pub(crate) fn compose_child_env<I>(
    daemon_vars: I,
    allow: &[&str],
    credential_env_names: &BTreeSet<String>,
    declared: &BTreeMap<String, String>,
) -> Vec<(String, String)>
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut base: Vec<(String, String)> = daemon_vars
        .into_iter()
        .filter(|(k, _)| allow.contains(&k.as_str()))
        .filter(|(_, v)| !looks_like_credential_url(v))
        .collect();
    crate::env_path::apply_path_floor(&mut base);

    let mut env: BTreeMap<String, String> = base.into_iter().collect();
    for (k, v) in declared {
        env.insert(k.clone(), v.clone());
    }
    for name in credential_env_names {
        env.remove(name);
    }
    env.into_iter().collect()
}

/// Whether `value` is a URL that embeds a credential in its userinfo, e.g.
/// `postgres://user:pass@host/db` — the shape `DATABASE_URL` often takes, which
/// a name-only check cannot catch (REQ-544 MED-1).
///
/// Moved here verbatim from the `shell` tool's retired scrub. It is the value
/// half of that scrub and it is **not** superseded by the allowlist: an
/// allowlist reasons about names and this reasons about values (BR-8). The name
/// half (`is_secret_key`) did retire — under a positive allowlist a name-shaped
/// denylist can only remove what the allowlist already excluded.
pub(crate) fn looks_like_credential_url(value: &str) -> bool {
    let Some((_scheme, after)) = value.split_once("://") else {
        return false;
    };
    // The authority ends at the first '/', '?', or '#'.
    let authority = after.split(['/', '?', '#']).next().unwrap_or("");
    match authority.split_once('@') {
        // A ':' in the userinfo before the '@' is an embedded password
        // (`user:pass@` or `:pass@`).
        Some((userinfo, _host)) => userinfo.contains(':'),
        None => false,
    }
}

/// How many `Config` fields [`credential_env_names_of`] reads.
///
/// The two gated by `is_recognized_auth_ref`: `providers[].auth_ref` and
/// `web.search_key_ref`. Written as a literal, and checked against a scan of
/// `teton-core`'s own source in this module's tests — the enumeration lives in
/// `tetond` while the fields live in `teton-core`, so nothing but a derived
/// guard keeps the two in step (BR-1.1).
#[cfg(test)]
pub(crate) const CREDENTIAL_REF_FIELDS: usize = 2;

/// Every environment variable name a configured `env:<NAME>` credential
/// reference points at.
///
/// "Configured credential reference" means **every** field
/// `is_recognized_auth_ref` gates, not only `providers[].auth_ref` (BR-1.1).
/// Today that is two, and both resolve through the same `env:` arm of the
/// secret resolver (`crate::keychain`, `std::env::var`), so both name a live
/// variable in the daemon's process environment. Covering only the provider half
/// would ship the leak this module exists to close, in the field that merely
/// happens to have been written second.
///
/// Other schemes (`keychain:`, `op://`) name no environment variable and
/// contribute nothing. A malformed bare `env:` names nothing either.
pub(crate) fn credential_env_names_of(config: &Config) -> BTreeSet<String> {
    let refs = config
        .providers
        .iter()
        .filter_map(|p| p.auth_ref.as_deref())
        .chain(config.web.search_key_ref.as_deref());

    refs.filter(|r| is_recognized_auth_ref(r))
        .filter_map(|r| r.strip_prefix("env:"))
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The installed source of [`credential_env_names`].
type Provider = Box<dyn Fn() -> BTreeSet<String> + Send + Sync>;
static CREDENTIAL_ENV_NAMES: OnceLock<Provider> = OnceLock::new();

/// Install the daemon's live-config reader as the credential-name source.
///
/// Called once from the daemon's bootstrap, after the runtime exists, in the
/// same place and for the same reason as the lifetime work-claim wiring: the
/// closure needs the runtime it is being wired into.
///
/// A second call is ignored rather than fatal — `OnceLock` semantics — because
/// the only caller is bootstrap and a test double that lost a race should not
/// abort a daemon over which of two equivalent readers won.
pub fn set_credential_env_names_provider<F>(provider: F)
where
    F: Fn() -> BTreeSet<String> + Send + Sync + 'static,
{
    let _ = CREDENTIAL_ENV_NAMES.set(Box::new(provider));
}

/// The credential env names as of **now**, or an empty set if no provider is
/// installed.
///
/// Empty is safe rather than fail-open, and the reason is structural: under
/// [`compose_child_env`]'s step 1 a variable whose name is not on the allowlist
/// is already absent, whatever this returns. The set only bites for the
/// pathological intersection — a user who writes `auth_ref = "env:HOME"` — and
/// the daemon, the one context where such a config exists at all, always
/// installs the provider.
pub(crate) fn credential_env_names() -> BTreeSet<String> {
    CREDENTIAL_ENV_NAMES.get().map_or_else(BTreeSet::new, |f| f())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn names(env: &[(String, String)]) -> Vec<&str> {
        env.iter().map(|(k, _)| k.as_str()).collect()
    }

    fn value_of<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        env.iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    fn no_credentials() -> BTreeSet<String> {
        BTreeSet::new()
    }

    /// AC-3. A variable that is neither allowlisted nor a configured credential
    /// is absent — the allowlist's *direction*, which an extended denylist could
    /// not produce. `RANDOM_UNRELATED_VAR` matches no credential pattern at all;
    /// it is gone because it was never admitted, not because it was caught.
    #[test]
    fn a_var_that_is_neither_allowlisted_nor_a_credential_is_absent() {
        let composed = compose_child_env(
            vars(&[
                ("HOME", "/home/SENTINEL"),
                ("RANDOM_UNRELATED_VAR", "1"),
                ("EDITOR", "vi"),
            ]),
            SHELL_ENV_ALLOW,
            &no_credentials(),
            &BTreeMap::new(),
        );
        assert!(!names(&composed).contains(&"RANDOM_UNRELATED_VAR"));
        assert!(!names(&composed).contains(&"EDITOR"));
        assert!(names(&composed).contains(&"HOME"));
    }

    /// AC-3.1, the positive direction, and the reason it has to exist: AC-3 is
    /// satisfiable by an allowlist that admits *nothing*, and a shell tool that
    /// can run no ordinary command would pass every other criterion in this REQ.
    ///
    /// Composed from a **synthetic** daemon environment rather than
    /// `std::env::vars()`: a real machine need not have all twelve set, so
    /// asserting presence against the process environment would assert the
    /// machine's configuration rather than this function's behavior.
    ///
    /// `PATH` is excluded — the floor rewrites it, and that is AC-4's subject.
    #[test]
    fn every_allowlisted_name_reaches_the_child_with_the_daemons_value() {
        let daemon: Vec<(String, String)> = SHELL_ENV_ALLOW
            .iter()
            .map(|k| ((*k).to_owned(), format!("SENTINEL-value-for-{k}")))
            .collect();

        let composed = compose_child_env(
            daemon.clone(),
            SHELL_ENV_ALLOW,
            &no_credentials(),
            &BTreeMap::new(),
        );

        for (key, expected) in daemon.iter().filter(|(k, _)| k != "PATH") {
            assert_eq!(
                value_of(&composed, key),
                Some(expected.as_str()),
                "{key} is on the allowlist, so the child must receive the daemon's value"
            );
        }
    }

    /// AC-3.2. The expected membership is written out **literally** here rather
    /// than compared to `SHELL_ENV_ALLOW` — a constant compared to itself is an
    /// assertion that cannot fail. Adding a name to the constant without
    /// amending BR-2.1 fails this test, which is the point.
    #[test]
    fn the_shell_allowlist_is_exactly_br_2_1s_recorded_set() {
        let expected = [
            "PATH", "HOME", "TMPDIR", "TZ", "TERM", "USER", "LOGNAME", "SHELL", "LANG", "LANGUAGE",
            "LC_ALL", "LC_CTYPE",
        ];
        assert_eq!(
            SHELL_ENV_ALLOW, &expected,
            "SHELL_ENV_ALLOW's membership is BR-2.1's recorded set; a change here needs a \
             change to the requirement and a justification against BR-2.1's criterion"
        );
    }

    /// AC-4.2 and the BR-8 half of AC-5. The value signal still runs after the
    /// allowlist, and the pair of cases is what pins it to the *value* rather
    /// than to the name: the same allowlisted name is withheld when it holds a
    /// `scheme://user:pass@host` URL and admitted when it holds an ordinary one.
    ///
    /// **Mutation run (AC-5).** Deleting the
    /// `.filter(|(_, v)| !looks_like_credential_url(v))` line from
    /// `compose_child_env` makes the first half of this test fail:
    /// `withheld a credential URL from an allowlisted name` — left
    /// `Some("postgres://SENTINELUSER:SENTINELPASS@db.invalid/x")`, right `None`.
    /// The second half stays green, which is what proves the two halves are
    /// testing the value and not the name.
    #[test]
    fn an_allowlisted_name_holding_a_credential_url_is_withheld() {
        let withheld = compose_child_env(
            vars(&[("TMPDIR", "postgres://SENTINELUSER:SENTINELPASS@db.invalid/x")]),
            SHELL_ENV_ALLOW,
            &no_credentials(),
            &BTreeMap::new(),
        );
        assert_eq!(
            value_of(&withheld, "TMPDIR"),
            None,
            "withheld a credential URL from an allowlisted name"
        );

        let admitted = compose_child_env(
            vars(&[("TMPDIR", "/tmp/SENTINEL")]),
            SHELL_ENV_ALLOW,
            &no_credentials(),
            &BTreeMap::new(),
        );
        assert_eq!(
            value_of(&admitted, "TMPDIR"),
            Some("/tmp/SENTINEL"),
            "the same name with an ordinary value is admitted — so the rule above read the \
             value, not the name"
        );
    }

    /// BR-3. The allowlist cannot re-admit a configured credential: removal is
    /// unconditional and runs last. `HOME` is the worst case on purpose — it is
    /// allowlisted, ordinary, and something almost every command wants.
    #[test]
    fn a_credential_name_on_the_allowlist_is_still_removed() {
        let credentials: BTreeSet<String> = ["HOME".to_owned()].into_iter().collect();
        let composed = compose_child_env(
            vars(&[("HOME", "SENTINEL-secret"), ("TERM", "xterm")]),
            SHELL_ENV_ALLOW,
            &credentials,
            &BTreeMap::new(),
        );
        assert_eq!(value_of(&composed, "HOME"), None);
        assert_eq!(value_of(&composed, "TERM"), Some("xterm"));
    }

    /// BR-3, the stronger half: removal runs after `declared`, so a per-child
    /// declared variable cannot re-admit a configured credential either.
    #[test]
    fn a_declared_var_cannot_re_admit_a_credential_name() {
        let credentials: BTreeSet<String> = ["SENTINEL_CRED".to_owned()].into_iter().collect();
        let declared = BTreeMap::from([("SENTINEL_CRED".to_owned(), "SENTINEL-value".to_owned())]);
        let composed = compose_child_env(
            vars(&[("HOME", "/home/SENTINEL")]),
            SHELL_ENV_ALLOW,
            &credentials,
            &declared,
        );
        assert_eq!(value_of(&composed, "SENTINEL_CRED"), None);
    }

    /// A declared var overrides an inherited one, and a declared `PATH`
    /// overrides the floor untouched. Both are the MCP path's existing
    /// semantics, asserted here because this function now owns them.
    #[test]
    fn declared_overrides_inherited_including_path() {
        let declared = BTreeMap::from([
            ("HOME".to_owned(), "/declared/SENTINEL".to_owned()),
            ("PATH".to_owned(), "/declared/bin".to_owned()),
        ]);
        let composed = compose_child_env(
            vars(&[("HOME", "/inherited"), ("PATH", "/usr/bin")]),
            SHELL_ENV_ALLOW,
            &no_credentials(),
            &declared,
        );
        assert_eq!(value_of(&composed, "HOME"), Some("/declared/SENTINEL"));
        assert_eq!(value_of(&composed, "PATH"), Some("/declared/bin"));
    }

    /// The value rule is confined to the inherited slice. A user who declares a
    /// credential-bearing URL for their own MCP server declared it deliberately;
    /// vetoing it would break a working config to enforce a rule aimed at what
    /// the daemon's environment leaks in.
    #[test]
    fn the_value_rule_does_not_veto_a_declared_var() {
        let declared = BTreeMap::from([(
            "SENTINEL_DB".to_owned(),
            "postgres://SENTINELUSER:SENTINELPASS@db.invalid/x".to_owned(),
        )]);
        let composed = compose_child_env(
            vars(&[("HOME", "/home/SENTINEL")]),
            SHELL_ENV_ALLOW,
            &no_credentials(),
            &declared,
        );
        assert!(value_of(&composed, "SENTINEL_DB").is_some());
    }

    /// BR-1.1. Both fields `is_recognized_auth_ref` gates contribute, and only
    /// the `env:` scheme names a variable at all. Built through `Config::from_toml`
    /// rather than by hand so the derivation is exercised over the shape the
    /// parser actually produces.
    #[test]
    fn both_gated_fields_contribute_their_env_names() {
        let config = Config::from_toml(
            r#"
[[providers]]
id = "a"
kind = "openai-compatible"
endpoint = "https://a.invalid/v1"
model = "m"
auth_ref = "env:SENTINEL_PROVIDER_ENV"

[[providers]]
id = "b"
kind = "openai-compatible"
endpoint = "https://b.invalid/v1"
model = "m"
auth_ref = "keychain:teton/b"

[web]
search_key_ref = "env:SENTINEL_WEB_ENV"
"#,
        )
        .expect("the fixture config parses");

        let names = credential_env_names_of(&config);
        assert!(
            names.contains("SENTINEL_PROVIDER_ENV"),
            "providers[].auth_ref must contribute"
        );
        assert!(
            names.contains("SENTINEL_WEB_ENV"),
            "web.search_key_ref must contribute — covering only the provider half is the \
             leak this REQ exists to close, in the field that happens to have been written \
             second (BR-1.1)"
        );
        assert_eq!(names.len(), 2, "a keychain: ref names no environment variable");
    }

    /// Schemes that name no environment variable contribute nothing, and a
    /// malformed bare `env:` names nothing either — it is not a recognized
    /// reference at all, so it never reaches the prefix strip.
    #[test]
    fn non_env_and_malformed_refs_contribute_nothing() {
        let config = Config::from_toml(
            r#"
[[providers]]
id = "a"
kind = "openai-compatible"
endpoint = "https://a.invalid/v1"
model = "m"
auth_ref = "op://vault/item"
"#,
        )
        .expect("the fixture config parses");
        assert!(credential_env_names_of(&config).is_empty());
    }

    /// **The cross-crate drift guard (BR-1.1).**
    ///
    /// `credential_env_names_of` lives in `tetond`; the fields it reads live in
    /// `teton-core/src/config.rs`. Proximity cannot keep the two in step — and
    /// would not have even if they were co-located, since a third gated field
    /// could be added without anyone updating a neighbouring enumeration. So the
    /// guard is derived: count the `is_recognized_auth_ref` call sites in
    /// `config.rs` and assert the enumeration reads that many fields.
    ///
    /// A third gated field in `teton-core` fails this test until
    /// `credential_env_names_of` follows it, which is what makes BR-1.1's
    /// "covered without amending this rule" true rather than hoped for.
    ///
    /// **Mutation run (BR-1.1).** Adding a third
    /// `if !is_recognized_auth_ref(x) { }` to `config.rs`'s production region
    /// fails this test: `teton-core gates 3 credential-reference fields but
    /// child_env::credential_env_names_of reads 2` — left `3`, right `2`.
    #[test]
    fn the_enumeration_covers_every_field_teton_core_gates() {
        let config_rs = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../teton-core/src/config.rs");
        let source = crate::call_sites::scan::production_source(&config_rs);
        let code = crate::call_sites::scan::code_only(&source);

        let call_sites = crate::call_sites::scan::count(&code, "is_recognized_auth_ref(")
            - crate::call_sites::scan::count(&code, "pub fn is_recognized_auth_ref(");

        assert!(
            call_sites > 0,
            "the scan found no call sites at all, so it would pass vacuously — \
             config.rs moved or the predicate was renamed"
        );
        assert_eq!(
            call_sites, CREDENTIAL_REF_FIELDS,
            "teton-core gates {call_sites} credential-reference fields but \
             child_env::credential_env_names_of reads {CREDENTIAL_REF_FIELDS}. A new field \
             carrying an auth_ref must be added to that enumeration, or its `env:<VAR>` \
             survives into every shell child (BR-1.1)."
        );
    }

    /// Retained from the retired `shell` scrub along with the rule it guards
    /// (BR-8): the value signal survived the rewrite, and so did its test.
    #[test]
    fn credential_url_detection() {
        assert!(looks_like_credential_url(
            "postgres://user:pass@host/db"
        ));
        assert!(looks_like_credential_url("redis://:password@host:6379"));
        assert!(!looks_like_credential_url("https://example.com/path"));
        assert!(!looks_like_credential_url("postgres://host/db"));
        assert!(!looks_like_credential_url("/usr/local/bin"));
    }
}
