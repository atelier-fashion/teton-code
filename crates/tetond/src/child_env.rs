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
//! ## The policy is pulled, not pushed
//!
//! `auth_ref = "env:<VAR>"` is a first-class credential form: the daemon knows
//! at config-load time exactly which variable names hold provider secrets, and
//! it used to never tell the scrubber. Now it does, through
//! [`set_child_env_policy_provider`] — a closure the daemon installs once at
//! bootstrap that reads the **live** config each time it is called. Not a
//! snapshot: a provider added mid-session is visible to the very next spawn, and
//! there is no second copy of the set to go stale (LESSON-539).
//!
//! REQ-607 widened what that closure returns from the credential set alone to a
//! whole [`ChildEnvPolicy`], because the `shell` path now needs a second fact
//! from the same config at the same instant — `[shell] allow_ssh_agent`. One
//! provider rather than two: a spawn cannot read half a policy, and nobody can
//! wire one reader and forget the other.
//!
//! ## The one variable an opt-in can add back
//!
//! REQ-596's rejections stand, and one of them — `SSH_AUTH_SOCK` — is the one
//! users feel, because `git push` over ssh needs it and fails saying
//! *"Permission denied (publickey)"*, which names ssh and never names Teton.
//! REQ-607 answers that twice over: [`WITHHELD_DIAGNOSED`] lets the `shell` tool
//! say who withheld it, and [`shell_env_allow`] lets a config author admit it —
//! that name and no other, on the `shell` path and no other.

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
/// | `SSH_AUTH_SOCK` | A `git push` over ssh wants it, so it passes the first half. It fails the second, and worse than by holding a credential: it is a handle to an agent that *lends* them. **Reachable by opt-in since REQ-607:** `[shell] allow_ssh_agent = true` admits it, and only it, to the `shell` path. The rejection above is still the default and still the reasoning — the key does not overturn the judgement, it lets someone who has read it accept the consequence. |
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

/// The one variable `[shell] allow_ssh_agent` admits (REQ-607 BR-5).
///
/// A named constant with two readers — [`shell_env_allow`] and
/// [`WITHHELD_DIAGNOSED`] — so the flag that admits it and the sentence that
/// explains its absence cannot come to disagree about which variable is at
/// stake (BR-6).
pub(crate) const SSH_AUTH_SOCK: &str = "SSH_AUTH_SOCK";

/// A variable the `shell` path withholds whose absence is worth *explaining*
/// when a command fails (REQ-607 BR-1).
///
/// Every `name` here is nameable in tool output, and that is a narrowing of
/// REQ-596 BR-5 recorded on both specs (amended 2026-09-01). The licence is
/// exactly this: names in the rejection table above are public in the source and
/// identical on every installation, so printing one discloses nothing about
/// *this* machine. A name discovered from the daemon's live environment, or
/// resolved from a configured `auth_ref = "env:<NAME>"`, is still unnameable,
/// and no credential *value* is nameable under any condition.
pub(crate) struct WithheldVar {
    /// The variable name. Must appear in the rejection table above — the test
    /// [`every_diagnosable_name_is_in_the_documented_rejection_table`]
    /// (tests::every_diagnosable_name_is_in_the_documented_rejection_table)
    /// is what keeps that true.
    pub name: &'static str,
    /// Programs whose failure this variable's absence plausibly explains.
    ///
    /// Matched in **command position** only — see the shell tool's matcher.
    pub programs: &'static [&'static str],
    /// The config key that admits it, spelled as a user would grep for it.
    ///
    /// `Option`, because BR-1's config-key clause is conditional: most of the
    /// rejection table has no opt-in and a row must not invent one. Naming a
    /// remedy no command can reach is BUG-205's failure mode, which is the very
    /// thing this table exists to avoid.
    pub opt_in_key: Option<&'static str>,
}

/// The rows the advisory may speak about — one today.
///
/// This is *not* the rejection table. The rejection table records every name
/// considered and refused; this records the subset whose absence produces a
/// failure a user would otherwise misattribute. `CARGO_HOME` is rejected and
/// absent from here because a command that needs it fails in a way that names
/// it; `SSH_AUTH_SOCK` is here because `git push` fails saying *"Permission
/// denied (publickey)"*, which names ssh and never names Teton.
///
/// Adding a row is the whole cost of serving another variable: nothing else in
/// the advisory path is keyed on `SSH_AUTH_SOCK` specifically.
pub(crate) const WITHHELD_DIAGNOSED: &[WithheldVar] = &[WithheldVar {
    name: SSH_AUTH_SOCK,
    // `git` because the failure a user actually hits is `git push`; the rest
    // because they are the other ways an agent-backed connection is made from a
    // one-line command.
    programs: &["ssh", "git", "scp", "sftp", "rsync"],
    opt_in_key: Some("[shell] allow_ssh_agent"),
}];

/// The names the `shell` child may inherit, given the one opt-in (REQ-607 BR-5).
///
/// [`SHELL_ENV_ALLOW`] plus, when the flag is on, [`SSH_AUTH_SOCK`]. **Nothing
/// else, ever** — this function is why `allow_ssh_agent` cannot become the
/// general `extra_env` that REQ-596's OQ-2 left open.
///
/// The opt-in adds to a **copy**. `SHELL_ENV_ALLOW` is BR-2.1's recorded set and
/// REQ-596 AC-3.2 asserts its literal membership; a flag that mutated it would
/// make that assertion a function of runtime config.
///
/// This is also the whole of the flag's reach. `compose_child_env` takes the
/// allowlist as a **parameter** (REQ-596 BR-7.1), and the MCP spawn path passes
/// its own constant — so there is no path by which this can widen what a
/// third-party `npx` package inherits (REQ-607 BR-7). That is a property of the
/// shape rather than of a test defending it.
pub(crate) fn shell_env_allow(allow_ssh_agent: bool) -> Vec<&'static str> {
    let mut allow = SHELL_ENV_ALLOW.to_vec();
    if allow_ssh_agent {
        allow.push(SSH_AUTH_SOCK);
    }
    allow
}

/// Compose the environment a spawned child receives.
///
/// The **only** function in this workspace that builds one. A second
/// construction site is a second policy, and the two would drift; the region
/// check in this module's tests fails the build if one appears (AC-8).
///
/// Five steps, and the order is load-bearing:
///
/// 1. Keep only `daemon_vars` whose **name** is in `allow` (BR-2).
/// 2. Floor `PATH` (BR-4). Before `declared`, so a child that declares its own
///    `PATH` overrides the floor untouched — the existing MCP semantics,
///    preserved.
/// 3. Drop what survives if its **value** is a credential-bearing URL (BR-8) —
///    *after* the floor, because the floor unconditionally re-adds a `PATH` and
///    would otherwise put back the one name on the allowlist this rule is most
///    likely to have to reach. Base slice only, never `declared`: a user who
///    declares `MY_DB=postgres://u:p@h` for their own MCP server declared it on
///    purpose; this step catches what the *daemon's* environment leaks in.
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
        .collect();
    crate::env_path::apply_path_floor(&mut base);
    // After the floor, not before. `apply_path_floor` *unconditionally* pushes a
    // `PATH`, so a `PATH` withheld by this rule before the floor ran would be
    // put straight back — and `PATH` is on the allowlist, which is exactly the
    // case BR-8 is about. Filtering afterwards leaves no name the rule cannot
    // reach.
    base.retain(|(_, v)| !looks_like_credential_url(v));

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

/// Everything a spawn needs to know from the **live** config (REQ-607 ADR-C).
///
/// Two facts, one type, because they are read at the same instant by the same
/// caller and a spawn that had one without the other would be composing an
/// environment from half a policy. Bundling them also makes "install one
/// provider and forget the other" unrepresentable — and a forgotten one would
/// fail in the *safe* direction, which is the worst kind, because nothing would
/// ever report it.
///
/// This is the same argument [`crate::runtime::DaemonRuntime::boundary_posture`]
/// makes for reading both boundary facts under one lock: two readings across a
/// concurrent `config/set` can disagree, and one derivation is what stops that.
#[derive(Debug, Clone, Default)]
pub struct ChildEnvPolicy {
    /// Every variable name a configured `env:<NAME>` credential reference points
    /// at — removed from the child unconditionally and last (REQ-596 BR-1/BR-3).
    pub credential_env_names: BTreeSet<String>,
    /// Whether `[shell] allow_ssh_agent` is set (REQ-607 BR-5).
    ///
    /// Read by the `shell` spawn path **only**, through [`shell_env_allow`]. The
    /// MCP spawn path never consults this type.
    pub allow_ssh_agent: bool,
}

/// The installed source of [`child_env_policy`].
type Provider = Box<dyn Fn() -> ChildEnvPolicy + Send + Sync>;
static CHILD_ENV_POLICY: OnceLock<Provider> = OnceLock::new();

/// Install the daemon's live-config reader as the child-environment policy
/// source.
///
/// Called once from the daemon's bootstrap, after the runtime exists, in the
/// same place and for the same reason as the lifetime work-claim wiring: the
/// closure needs the runtime it is being wired into.
///
/// A second call is ignored rather than fatal — `OnceLock` semantics — because
/// the only caller is bootstrap and a test double that lost a race should not
/// abort a daemon over which of two equivalent readers won.
pub fn set_child_env_policy_provider<F>(provider: F)
where
    F: Fn() -> ChildEnvPolicy + Send + Sync + 'static,
{
    let _ = CHILD_ENV_POLICY.set(Box::new(provider));
}

/// The child-environment policy as of **now**, or the default if no provider is
/// installed.
///
/// The default is safe in **both** fields, and each for its own reason.
///
/// An empty credential set is safe rather than fail-open, structurally: under
/// [`compose_child_env`]'s step 1 a variable whose name is not on the allowlist
/// is already absent, whatever this returns. The set only bites for the
/// pathological intersection — a user who writes `auth_ref = "env:HOME"` — and
/// the daemon, the one context where such a config exists at all, always
/// installs the provider.
///
/// `allow_ssh_agent: false` is safe because it is the shipped default: an
/// uninstalled provider withholds the agent exactly as an unset config key does,
/// so the failure mode of "nobody wired the bootstrap" is the *secure* posture
/// rather than a silently widened child.
pub(crate) fn child_env_policy() -> ChildEnvPolicy {
    CHILD_ENV_POLICY
        .get()
        .map_or_else(ChildEnvPolicy::default, |f| f())
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
        env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
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
            vars(&[(
                "TMPDIR",
                "postgres://SENTINELUSER:SENTINELPASS@db.invalid/x",
            )]),
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

    /// The value rule reaches `PATH` too, which it only does because it runs
    /// **after** the floor. Written before the floor, this case passes
    /// vacuously: `apply_path_floor` unconditionally pushes a `PATH` back, so
    /// the withheld variable reappears with the credential still in it. `PATH`
    /// is on the allowlist, so BR-8 has to hold for it like any other name.
    ///
    /// **Mutation run.** Moving `base.retain(...)` back above
    /// `apply_path_floor` fails this test, and the observed failure is more
    /// interesting than the predicted one: left
    /// `Some("/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin")`, right
    /// `None`. The variable comes back — which is the assertion — but the
    /// credential text did *not* survive, because `floored_path` happened to
    /// drop an inherited entry that names no directory. That is precisely why
    /// the rule must not sit above the floor: whether a credential leaks would
    /// then depend on a *usability* helper's incidental treatment of a malformed
    /// entry, and a security property resting on that is a property nobody can
    /// state. Nothing else moves.
    #[test]
    fn the_value_rule_reaches_path_because_it_runs_after_the_floor() {
        let composed = compose_child_env(
            vars(&[("PATH", "ldap://SENTINELUSER:SENTINELPASS@dir.invalid")]),
            SHELL_ENV_ALLOW,
            &no_credentials(),
            &BTreeMap::new(),
        );
        assert_eq!(
            value_of(&composed, "PATH"),
            None,
            "a credential-bearing PATH must be withheld like any other value; the floor \
             must not put it back"
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

    /// REQ-607 BR-7 — the benign path for a daemon nobody wired.
    ///
    /// `child_env_policy()` falls back to [`ChildEnvPolicy::default`] when no
    /// provider is installed, so the default *is* the behaviour of an
    /// unbootstrapped daemon. Both fields must be safe there: no credential
    /// names is structurally harmless (the allowlist already excludes anything
    /// a credential is likely to be called), and `allow_ssh_agent: false` is the
    /// shipped posture — so "nobody wired the bootstrap" withholds the agent
    /// rather than silently widening the child.
    ///
    /// **Asserted on the default rather than by calling `child_env_policy()`
    /// with nothing installed.** `CHILD_ENV_POLICY` is a process-global
    /// `OnceLock` and other tests in this binary install a provider, so an
    /// "uninstalled" assertion would pass or fail on test *order*. The default
    /// is the value that branch returns, and it is the thing worth pinning.
    #[test]
    fn an_uninstalled_policy_provider_withholds_the_agent_and_names_no_credentials() {
        let fallback = ChildEnvPolicy::default();
        assert!(
            !fallback.allow_ssh_agent,
            "an unbootstrapped daemon admitted the ssh agent — the uninstalled-provider \
             fallback must be the secure posture, not the permissive one"
        );
        assert!(fallback.credential_env_names.is_empty());

        // And the fallback composes a child that really lacks the socket, so
        // this is a claim about the environment rather than about a struct.
        let composed = compose_child_env(
            vars(&[("PATH", "/usr/bin"), (SSH_AUTH_SOCK, "/tmp/SENTINEL.sock")]),
            &shell_env_allow(fallback.allow_ssh_agent),
            &fallback.credential_env_names,
            &BTreeMap::new(),
        );
        assert!(value_of(&composed, SSH_AUTH_SOCK).is_none());
    }

    /// REQ-607 BR-5 / BR-6 — the opt-in adds **one** name to the allowlist.
    ///
    /// Asserted as a set difference in both directions, so a widened
    /// `shell_env_allow` fails here and not only at the composed-map level:
    /// nothing gained beyond `SSH_AUTH_SOCK`, and nothing lost.
    ///
    /// The second half is the one that would rot silently. `SHELL_ENV_ALLOW` is
    /// REQ-596 BR-2.1's recorded set and REQ-596 AC-3.2 asserts its literal
    /// membership; if the opt-in ever mutated the constant instead of a copy,
    /// that assertion would become a function of runtime config.
    #[test]
    fn the_opt_in_adds_one_name_to_the_allowlist_and_no_other() {
        let off: BTreeSet<&str> = shell_env_allow(false).into_iter().collect();
        let on: BTreeSet<&str> = shell_env_allow(true).into_iter().collect();

        let gained: Vec<&&str> = on.difference(&off).collect();
        let lost: Vec<&&str> = off.difference(&on).collect();
        assert_eq!(
            gained,
            vec![&SSH_AUTH_SOCK],
            "the opt-in admitted something other than the ssh agent socket"
        );
        assert!(
            lost.is_empty(),
            "the opt-in withdrew names it should have left alone: {lost:?}"
        );

        // The flag adds to a copy — the recorded constant is untouched.
        assert_eq!(
            off,
            SHELL_ENV_ALLOW.iter().copied().collect::<BTreeSet<&str>>(),
            "shell_env_allow(false) is no longer REQ-596 BR-2.1's recorded set"
        );
        assert!(
            !SHELL_ENV_ALLOW.contains(&SSH_AUTH_SOCK),
            "the opt-in mutated SHELL_ENV_ALLOW itself, so REQ-596 AC-3.2's membership \
             assertion is now a function of runtime config"
        );
    }

    /// REQ-607 AC-12 / BR-4 — every name the advisory may speak is in the
    /// **documented** rejection table.
    ///
    /// BR-4 narrows REQ-596 BR-5 by exactly this much: a name is nameable in
    /// tool output *because* it is in that table, public in the source and
    /// identical on every installation. A code table that drifted from the doc
    /// table would name something the narrowing does not license — and the
    /// drift is silent, because nothing else reads the doc comment.
    ///
    /// The slice is **bounded to the table** rather than searched over the whole
    /// file (conventions.md, REQ-600): `SSH_AUTH_SOCK` also appears as a
    /// constant and inside `WITHHELD_DIAGNOSED`, and an unbounded search would
    /// find those and pass with the table row deleted. The corpus is
    /// `production_source`, which cuts at the first column-0 `#[cfg(test)]`, so
    /// this test's own text cannot satisfy it.
    ///
    /// **Mutation run, both halves.** Renaming the rejection table's
    /// `SSH_AUTH_SOCK` row to `SSH_AGENT_HANDLE_MUTANT` fails this test with
    /// "WITHHELD_DIAGNOSED names SSH_AUTH_SOCK but the rejection table does
    /// not" — 1 assertion. Then, with that mutation still in place, widening
    /// `table` back to the whole `source` makes it **pass** (1 passed, 0
    /// failed), because the constant and `WITHHELD_DIAGNOSED` both spell the
    /// name elsewhere in the file. So the bound is not tidiness; it is the only
    /// reason this check can fail at all.
    #[test]
    fn every_diagnosable_name_is_in_the_documented_rejection_table() {
        let source = crate::call_sites::scan::production_source(
            &crate::call_sites::scan::daemon_src().join("child_env.rs"),
        );

        // Bound the slice to the rejection table: from its header row to the
        // constant the doc comment is attached to.
        let start = source
            .find("/// | Considered | Rejected because |")
            .expect("the rejection table's header row");
        let end = source[start..]
            .find("pub(crate) const SHELL_ENV_ALLOW")
            .expect("the constant the rejection table documents")
            + start;
        let table = &source[start..end];

        let rows = table.lines().filter(|l| l.trim_start().starts_with("/// |"));
        assert!(
            rows.count() >= 5,
            "the bounded slice found fewer rejection-table rows than the table is known to \
             have; the bounds have drifted and this check is passing vacuously"
        );

        let mut checked = 0_usize;
        for var in WITHHELD_DIAGNOSED {
            assert!(
                table.contains(var.name),
                "WITHHELD_DIAGNOSED names {} but the rejection table does not. BR-4 makes \
                 that table the nameable set, so an advisory naming this variable would \
                 disclose a name REQ-596 BR-5's narrowing does not license.",
                var.name
            );
            checked += 1;
        }
        assert!(
            checked >= 1,
            "the diagnosis table is empty, so this check proved nothing"
        );
    }

    /// How far above a `.envs(` the composer call may sit and still count as the
    /// same region. Wide enough for a multi-line binding, narrow enough that a
    /// composer call in a *different* function cannot vouch for this one.
    const COMPOSER_REGION_LINES: usize = 30;

    /// AC-8. A spawned child's environment has exactly one construction site,
    /// and it is [`compose_child_env`].
    ///
    /// # Why this is a region check and not a count
    ///
    /// Counting `.envs(` call sites, or counting `compose_child_env` calls,
    /// proves nothing about whether they are the *same* code: relocating a
    /// required call keeps every count identical (conventions.md, LESSON-568).
    /// So each `.envs(` is checked against its own neighbourhood — either its
    /// argument is a direct `compose_child_env(...)` call, or it is an
    /// identifier bound by one within [`COMPOSER_REGION_LINES`] above it. A
    /// hand-built vector fails wherever it is written.
    ///
    /// **Mutation run.** Adding to `run_bounded`
    /// `let sneaky: Vec<(String, String)> = vec![]; cmd.envs(sneaky);`
    /// fails this test with `harness/tools/shell.rs: the environment passed to
    /// .envs(sneaky) is not composed by compose_child_env`. Deleting the
    /// `let child_env = ... compose_child_env(` binding while leaving
    /// `.envs(child_env)` fails it the same way. Both were run and both went
    /// red; the site was then removed.
    #[test]
    fn a_childs_environment_has_exactly_one_construction_site() {
        let mut checked = 0_usize;

        for (path, source) in crate::call_sites::scan::production_sources() {
            let code = crate::call_sites::scan::code_only(&source);
            let lines: Vec<&str> = code.lines().collect();

            for (i, line) in lines.iter().enumerate() {
                let Some(at) = line.find(".envs(") else {
                    continue;
                };
                checked += 1;

                let arg = line[at + ".envs(".len()..]
                    .rsplit_once(')')
                    .map_or("", |(before, _)| before)
                    .trim();

                // Direct form: `.envs(compose_child_env(...))`.
                if arg.contains("compose_child_env") {
                    continue;
                }

                // Identifier form: the binding must be a composer call, and it
                // must be *near* — a call in another function does not vouch.
                let from = i.saturating_sub(COMPOSER_REGION_LINES);
                let bound_by_composer = lines[from..i].iter().any(|l| {
                    l.contains(&format!("let {arg} =")) && l.contains("compose_child_env")
                });
                assert!(
                    bound_by_composer,
                    "{path}: the environment passed to .envs({arg}) is not composed by \
                     compose_child_env. A second construction site is a second policy, and \
                     the two drift — which is how the `shell` tool and the MCP spawn path \
                     ended up disagreeing about credentials in the first place (AC-8)."
                );
            }
        }

        assert!(
            checked >= 2,
            "the scan found {checked} `.envs(` sites; it must see at least the shell and \
             MCP spawn paths, or it is passing vacuously"
        );
    }

    /// AC-8's other half, and the hole the `.envs(` check alone leaves.
    ///
    /// The check above asks whether an environment *handed to a child* was
    /// composed. It says nothing about a spawn that hands the child no
    /// environment at all — and a `Command` without `env_clear()` inherits the
    /// daemon's whole environment, credentials included. That spawn has no
    /// `.envs(` for the region check to find, so it would pass in silence: the
    /// worst possible failure mode for a guard whose job is to notice a second
    /// way in.
    ///
    /// So every process spawn in production source must clear first. A spawn is
    /// identified by `.spawn()` appearing in the same builder chain rather than
    /// by the imported type name — `skills::dynamic::Command` is a parsed
    /// dynamic-context command that never becomes a process, and the two are
    /// spelled identically at the call site. The first draft of this test keyed
    /// on the import instead and silently saw only one of the two real spawns;
    /// the vacuity floor below is what caught that, which is the whole reason it
    /// is here.
    ///
    /// **Mutation run.** Deleting `.env_clear()` from `run_bounded` fails with
    /// `harness/tools/shell.rs: a process spawn does not call env_clear()`.
    #[test]
    fn every_process_spawn_clears_the_inherited_environment_first() {
        /// How much of the builder chain after `Command::new(` is considered
        /// part of the same spawn.
        const BUILDER_CHAIN_CHARS: usize = 1_200;

        let mut spawns = 0_usize;

        for (path, source) in crate::call_sites::scan::production_sources() {
            let code = crate::call_sites::scan::code_only(&source);

            for (at, _) in code.match_indices("Command::new(") {
                let chain = &code[at..code.len().min(at + BUILDER_CHAIN_CHARS)];
                // Not a process spawn — `skills::dynamic::Command` shares the
                // name and never reaches the OS.
                if !chain.contains(".spawn()") {
                    continue;
                }
                spawns += 1;
                assert!(
                    chain.contains("env_clear()"),
                    "{path}: a process spawn does not call env_clear(), so the child \
                     inherits the daemon's entire environment — every configured credential \
                     included. Compose it with child_env::compose_child_env instead (AC-8)."
                );
            }
        }

        assert_eq!(
            spawns, 2,
            "the scan must see exactly the two known process spawns (the `shell` tool and \
             the MCP stdio server). A third is a new way for a child to inherit the \
             daemon's environment and needs its own answer to AC-8; zero or one means the \
             scan stopped seeing a file and every assertion above it passed vacuously."
        );
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
        assert_eq!(
            names.len(),
            2,
            "a keychain: ref names no environment variable"
        );
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
        let config_rs =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../teton-core/src/config.rs");
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
        assert!(looks_like_credential_url("postgres://user:pass@host/db"));
        assert!(looks_like_credential_url("redis://:password@host:6379"));
        assert!(!looks_like_credential_url("https://example.com/path"));
        assert!(!looks_like_credential_url("postgres://host/db"));
        assert!(!looks_like_credential_url("/usr/local/bin"));
    }
}
