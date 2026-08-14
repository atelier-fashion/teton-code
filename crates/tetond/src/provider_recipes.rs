//! The vendor recipes Teton's own guidance hands a user (REQ-577 BR-2).
//!
//! A user who says "hook up Kimi for deep reasoning" needs three facts the local
//! model's weights do not reliably hold: the endpoint Moonshot actually serves,
//! which provider kind it speaks, and a model name that exists today. Guess any
//! one of them and the answer is a runnable-*looking* command whose failure
//! arrives a step away from its cause — a connection error, or a 404 on a model
//! that was retired two releases ago. That is BUG-165's texture: not a wrong
//! answer the user can see, but a right-shaped one they cannot debug.
//!
//! This module is the one copy. The bundled self-config guide, the `teton_docs`
//! providers topic, and the README's "Hooking up an external model" block stay
//! hand-written prose, and CI gates each of them against *this* list in both
//! directions (ADR-2) — drift is a test failure, not a doc bug. The README is
//! the reason that gate exists: it has shipped unpinned vendor facts since
//! before this REQ, and one of them had already gone stale by the time this
//! catalog was written.
//!
//! # Verified, not recalled (BR-3, LESSON-512)
//!
//! Every endpoint, kind, and example model below was read off the vendor's own
//! current public documentation on 2026-08-14, and each entry's comment names
//! the page and what it said. This is not ceremony: two of the values this REQ
//! was drafted with had already moved by implementation time — DeepSeek's
//! `deepseek-chat`/`deepseek-reasoner` pair and Moonshot's `kimi-k2` — so a
//! catalog written from memory would have shipped two dead model names on day
//! one. A named example in a spec is a test vector, and a test vector is checked.
//!
//! Endpoints move at roughly release cadence; model names move faster. So every
//! [`ProviderRecipe::example_model`] is exactly that — an example — and
//! `--model` always takes whatever the vendor serves. Staleness here degrades to
//! a slightly old example, never to a broken command shape.
//!
//! # Pure by construction
//!
//! [`recipe_catalog`] takes nothing and reads nothing — no config, no env, no
//! TTY, no daemon state. That is the [`crate::web_setup_catalog`] rule inherited
//! deliberately (LESSON-481): product data behind a gate is data the test suite
//! cannot see, and the entire value of one definition is that one test can pin
//! it. The signature is the proof; the test module below is the pin.
//!
//! # What may not live here
//!
//! Never a credential, and never a field that could hold one (the REQ-573 BR-6
//! rule). A recipe describes how to *register* a provider; the key reaches the
//! keychain through a separate, echo-off, human-gated step that this list never
//! touches. There is deliberately no auth field at all — not even a header
//! *shape*, which the web catalog needs and this one does not, because
//! `teton provider add` takes no header argument. The absent field is the
//! guarantee; the sweep in the tests below is the second net.

use teton_core::entities::ProviderKind;

/// One vendor's `teton provider add` recipe: everything the command needs and
/// nothing it does not.
///
/// The field set is the command's argument list, one for one — which is the
/// point. A recipe that could not be typed out as a runnable command would be
/// documentation, and documentation is what BUG-160 showed the model cannot use
/// when the hole in the template is the fact it lacked.
///
/// [`kind`](Self::kind) is [`ProviderKind`] rather than a string (ADR-1): a
/// recipe naming a kind `provider add` does not accept is then not a bug to be
/// caught by a test, it is a program that does not compile.
///
/// A `String` per field rather than `&'static str`, matching
/// [`crate::web_setup_catalog`]: the prose gates and the future `teton_docs`
/// topic own these values as data, and owning them here keeps the seam free of a
/// lifetime that exists only because the data happens to be static today.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRecipe {
    /// The id to suggest in the example command — the `<id>` of
    /// `teton provider add <id>`, and the name the routing step then binds a
    /// tier to. A suggestion, not a reservation: ids are the user's namespace.
    pub id_suggestion: String,
    /// The vendor's display name, spelled the way the vendor spells it.
    pub label: String,
    /// Which adapter the vendor speaks, and therefore whether
    /// [`endpoint`](Self::endpoint) is required.
    pub kind: ProviderKind,
    /// The base URL to pass as `--endpoint`, or `None` when the kind carries its
    /// own address and the flag must be omitted.
    pub endpoint: Option<String>,
    /// A model the vendor serves today, offered as an example to substitute —
    /// never as a recommendation and never as "the current best".
    pub example_model: String,
    /// One bounded clause for a fact the command shape alone does not say, or
    /// `None` when it says everything.
    pub notes: Option<String>,
}

/// The vendor recipes this build ships.
///
/// Ordered as the OQ-1 roster resolves them, which is also how a reader should
/// meet them: the two native-kind vendors first (their commands are the shortest
/// and teach the shape), then the OpenAI-compatible hosted APIs, then the local
/// one. The order is pinned below, because every prose surface renders this list
/// in sequence and a reordering that nobody noticed would be a diff nobody could
/// review.
///
/// Documenting a vendor blesses none of them: these are recipes an agent reads
/// aloud to a user, never defaults the daemon applies (the REQ-563 BR-8 spirit).
#[must_use]
pub fn recipe_catalog() -> Vec<ProviderRecipe> {
    vec![
        // Verified 2026-08-14 against Anthropic's models overview
        // (platform.claude.com/docs/en/about-claude/models/overview): the
        // current Claude API ids are `claude-opus-5`, `claude-sonnet-5`,
        // `claude-fable-5` and `claude-haiku-4-5`; `claude-opus-5` is the one
        // the page tells an unsure reader to start with.
        ProviderRecipe {
            id_suggestion: "anthropic".to_owned(),
            label: "Anthropic".to_owned(),
            kind: ProviderKind::Anthropic,
            // Absent because the adapter carries the Messages API address, not
            // because it is unknown — the same distinction the web catalog draws
            // for a backend that wants no header. Passing `--endpoint` here is a
            // user error, so the recipe must not model one.
            endpoint: None,
            example_model: "claude-opus-5".to_owned(),
            notes: Some("no --endpoint: the anthropic kind knows its own address".to_owned()),
        },
        // Verified 2026-08-14 against OpenAI's API reference
        // (developers.openai.com/api/docs/api-reference/chat/create), whose curl
        // example posts to `https://api.openai.com/v1/chat/completions`, and its
        // model page for gpt-5.6, which states the `gpt-5.6` alias routes to
        // GPT-5.6 Sol (`gpt-5.6-sol`). The alias is the better example: it is
        // shorter to type and does not pin a user to one snapshot.
        ProviderRecipe {
            id_suggestion: "openai".to_owned(),
            label: "OpenAI".to_owned(),
            // The kind is named after this API because this API is the shape
            // every other entry below imitates; OpenAI gets no special adapter.
            kind: ProviderKind::OpenaiCompatible,
            endpoint: Some("https://api.openai.com/v1".to_owned()),
            example_model: "gpt-5.6".to_owned(),
            notes: None,
        },
        // Verified 2026-08-14 against Kimi's API overview
        // (platform.kimi.ai/docs/api/overview — platform.moonshot.ai now 301s
        // there, while the *API* host is unchanged), which tells developers to
        // set `base_url` to `https://api.moonshot.ai/v1` and to use the OpenAI
        // SDKs directly; the models overview names `kimi-k3` as the flagship.
        // The README's `kimi-k2` is the drift this REQ's prose gate exists to
        // catch — it was already stale when this line was written.
        ProviderRecipe {
            id_suggestion: "kimi".to_owned(),
            label: "Moonshot (Kimi)".to_owned(),
            kind: ProviderKind::OpenaiCompatible,
            endpoint: Some("https://api.moonshot.ai/v1".to_owned()),
            example_model: "kimi-k3".to_owned(),
            notes: None,
        },
        // Verified 2026-08-14 against DeepSeek's API docs (api-docs.deepseek.com):
        // the OpenAI-format `base_url` is `https://api.deepseek.com` and the
        // first-call example posts to `https://api.deepseek.com/chat/completions`
        // with `"model": "deepseek-v4-pro"`. The REQ's drafted
        // `deepseek-chat`/`deepseek-reasoner` no longer appear on the models
        // page; `deepseek-v4-flash` is the other current id.
        ProviderRecipe {
            id_suggestion: "deepseek".to_owned(),
            label: "DeepSeek".to_owned(),
            kind: ProviderKind::OpenaiCompatible,
            endpoint: Some("https://api.deepseek.com".to_owned()),
            example_model: "deepseek-v4-pro".to_owned(),
            // The one endpoint here that is not a `/v1`, which is exactly the
            // kind of small difference a user pattern-matches away and then
            // spends an afternoon on.
            notes: Some("base URL takes no /v1 suffix".to_owned()),
        },
        // Verified 2026-08-14 against Ollama's OpenAI-compatibility page
        // (docs.ollama.com/api/openai-compatibility): it serves
        // `http://localhost:11434/v1/` and says an API key is "required but
        // ignored" by the client libraries, i.e. the server authenticates
        // nothing. `llama3.2` is one of the models the page's own examples name.
        // The trailing slash is dropped here to match the other entries' base
        // form; both resolve to the same routes.
        ProviderRecipe {
            id_suggestion: "ollama".to_owned(),
            label: "Ollama".to_owned(),
            kind: ProviderKind::OpenaiCompatible,
            endpoint: Some("http://localhost:11434/v1".to_owned()),
            example_model: "llama3.2".to_owned(),
            // The only entry that needs no key at all, and the only endpoint
            // that is an example of a shape rather than an address — a user who
            // runs Ollama elsewhere substitutes their own host.
            notes: Some("local and keyless: serves the models you have pulled".to_owned()),
        },
        // Verified 2026-08-14 against xAI's overview (docs.x.ai/docs/overview),
        // whose quickstart constructs the OpenAI client with
        // `base_url="https://api.x.ai/v1"`, and its models page, which lists
        // `grok-4.6` first and tells the reader to use it "for everything else,
        // including code".
        ProviderRecipe {
            id_suggestion: "grok".to_owned(),
            label: "Grok (xAI)".to_owned(),
            kind: ProviderKind::OpenaiCompatible,
            endpoint: Some("https://api.x.ai/v1".to_owned()),
            example_model: "grok-4.6".to_owned(),
            notes: None,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeSet;

    /// What a maintainer standing over a red golden assertion needs told.
    ///
    /// The instruction matters more than the diff. A stale endpoint fails
    /// *silently* for the user — the command runs, the connection does not — so
    /// the tempting local fix when this test goes red (delete the line, the
    /// catalog is obviously right) is the one that removes the only thing
    /// standing between a moved URL and somebody's shell.
    fn drift(what: &str) -> String {
        format!(
            "{what} no longer matches what this module ships. Re-verify the fact against the \
             vendor's current public documentation, then update BOTH spellings — the catalog \
             entry and this assertion — and refresh the entry's `Verified <date>` comment. \
             Deleting the assertion is never the fix (REQ-577 BR-3)."
        )
    }

    /// **BR-3 and the golden pin: the shipped strings, byte for byte.**
    ///
    /// Written out a second time by hand rather than derived from the catalog,
    /// for the same reason the web-setup goldens are: every value here is a fact
    /// about a third party's API — Moonshot's host, DeepSeek's missing `/v1`,
    /// Ollama's port — which no rule in this repository can regenerate. The only
    /// honest guard is two independent spellings that must agree. A reworded
    /// label is meant to be a one-line diff here; a changed *endpoint* is meant
    /// to be a failure that sends someone back to the vendor's docs.
    #[test]
    fn the_catalog_ships_the_six_vendors_verbatim() {
        let catalog = recipe_catalog();

        let ids: Vec<&str> = catalog
            .iter()
            .map(|r| r.id_suggestion.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            ["anthropic", "openai", "kimi", "deepseek", "ollama", "grok"],
            "{}",
            drift("the roster, in the order every prose surface renders it")
        );

        let anthropic = &catalog[0];
        assert_eq!(
            anthropic.label,
            "Anthropic",
            "{}",
            drift("Anthropic's label")
        );
        assert_eq!(
            anthropic.kind,
            ProviderKind::Anthropic,
            "{}",
            drift("Anthropic's provider kind")
        );
        assert_eq!(
            anthropic.endpoint,
            None,
            "{}",
            drift("Anthropic's absent endpoint — the adapter carries the address")
        );
        assert_eq!(
            anthropic.example_model,
            "claude-opus-5",
            "{}",
            drift("Anthropic's example model")
        );
        assert_eq!(
            anthropic.notes.as_deref(),
            Some("no --endpoint: the anthropic kind knows its own address"),
            "{}",
            drift("Anthropic's note")
        );

        let openai = &catalog[1];
        assert_eq!(openai.label, "OpenAI", "{}", drift("OpenAI's label"));
        assert_eq!(
            openai.kind,
            ProviderKind::OpenaiCompatible,
            "{}",
            drift("OpenAI's provider kind")
        );
        assert_eq!(
            openai.endpoint.as_deref(),
            Some("https://api.openai.com/v1"),
            "{}",
            drift("OpenAI's endpoint")
        );
        assert_eq!(
            openai.example_model,
            "gpt-5.6",
            "{}",
            drift("OpenAI's example model")
        );
        assert_eq!(openai.notes, None, "{}", drift("OpenAI's absent note"));

        let kimi = &catalog[2];
        assert_eq!(
            kimi.label,
            "Moonshot (Kimi)",
            "{}",
            drift("Moonshot's label")
        );
        assert_eq!(
            kimi.kind,
            ProviderKind::OpenaiCompatible,
            "{}",
            drift("Moonshot's provider kind")
        );
        assert_eq!(
            kimi.endpoint.as_deref(),
            Some("https://api.moonshot.ai/v1"),
            "{}",
            drift("Moonshot's endpoint — the docs site moved to kimi.ai, the API host did not")
        );
        assert_eq!(
            kimi.example_model,
            "kimi-k3",
            "{}",
            drift("Moonshot's example model")
        );
        assert_eq!(kimi.notes, None, "{}", drift("Moonshot's absent note"));

        let deepseek = &catalog[3];
        assert_eq!(deepseek.label, "DeepSeek", "{}", drift("DeepSeek's label"));
        assert_eq!(
            deepseek.kind,
            ProviderKind::OpenaiCompatible,
            "{}",
            drift("DeepSeek's provider kind")
        );
        assert_eq!(
            deepseek.endpoint.as_deref(),
            Some("https://api.deepseek.com"),
            "{}",
            drift("DeepSeek's endpoint, which deliberately carries no /v1")
        );
        assert_eq!(
            deepseek.example_model,
            "deepseek-v4-pro",
            "{}",
            drift("DeepSeek's example model")
        );
        assert_eq!(
            deepseek.notes.as_deref(),
            Some("base URL takes no /v1 suffix"),
            "{}",
            drift("DeepSeek's note")
        );

        let ollama = &catalog[4];
        assert_eq!(ollama.label, "Ollama", "{}", drift("Ollama's label"));
        assert_eq!(
            ollama.kind,
            ProviderKind::OpenaiCompatible,
            "{}",
            drift("Ollama's provider kind")
        );
        assert_eq!(
            ollama.endpoint.as_deref(),
            Some("http://localhost:11434/v1"),
            "{}",
            drift("Ollama's endpoint")
        );
        assert_eq!(
            ollama.example_model,
            "llama3.2",
            "{}",
            drift("Ollama's example model")
        );
        assert_eq!(
            ollama.notes.as_deref(),
            Some("local and keyless: serves the models you have pulled"),
            "{}",
            drift("Ollama's note, which is the only place `keyless` is stated")
        );

        let grok = &catalog[5];
        assert_eq!(grok.label, "Grok (xAI)", "{}", drift("Grok's label"));
        assert_eq!(
            grok.kind,
            ProviderKind::OpenaiCompatible,
            "{}",
            drift("Grok's provider kind")
        );
        assert_eq!(
            grok.endpoint.as_deref(),
            Some("https://api.x.ai/v1"),
            "{}",
            drift("Grok's endpoint")
        );
        assert_eq!(
            grok.example_model,
            "grok-4.6",
            "{}",
            drift("Grok's example model")
        );
        assert_eq!(grok.notes, None, "{}", drift("Grok's absent note"));
    }

    /// An endpoint is required exactly when the kind does not carry its own.
    ///
    /// The two failures are asymmetric and both silent. A remote kind missing an
    /// endpoint yields a command with a hole in it — the BUG-160 shape this REQ
    /// exists to close. An `anthropic` entry *carrying* one yields a command with
    /// a flag the adapter ignores or rejects, which teaches the user a wrong fact
    /// about the CLI. The golden above pins today's six; this pins the rule, so
    /// a seventh vendor added later cannot get it wrong.
    #[test]
    fn an_endpoint_is_present_exactly_when_the_kind_needs_one() {
        for recipe in recipe_catalog() {
            let needs_endpoint = matches!(recipe.kind, ProviderKind::OpenaiCompatible);
            assert_eq!(
                recipe.endpoint.is_some(),
                needs_endpoint,
                "`{}` is kind {:?} but endpoint={:?}",
                recipe.id_suggestion,
                recipe.kind,
                recipe.endpoint
            );
            if let Some(endpoint) = &recipe.endpoint {
                assert!(
                    endpoint.starts_with("http://") || endpoint.starts_with("https://"),
                    "`{}`'s endpoint {endpoint:?} is not a URL a user could paste into \
                     `--endpoint`",
                    recipe.id_suggestion
                );
            }
        }
    }

    /// No recipe may name [`ProviderKind::Local`] or [`ProviderKind::Custom`].
    ///
    /// Reusing the entity enum makes "a recipe names a kind `provider add`
    /// accepts" a compile-time fact (ADR-1), but the enum is wider than this
    /// catalog's remit: `Local` is the on-device tier, which is configured by
    /// the consent-and-download flow rather than by registering a vendor, and
    /// `Custom` names an operator's own adapter, which no vendor recipe can
    /// describe. Either one here would be a recipe whose command does not exist.
    #[test]
    fn every_recipe_names_a_registerable_vendor_kind() {
        for recipe in recipe_catalog() {
            assert!(
                matches!(
                    recipe.kind,
                    ProviderKind::Anthropic | ProviderKind::OpenaiCompatible
                ),
                "`{}` is kind {:?}; a vendor recipe describes a `provider add` a user can run, \
                 and neither the local tier nor a custom adapter is registered that way",
                recipe.id_suggestion,
                recipe.kind
            );
        }
    }

    /// Suggested ids are what the routing step binds a tier to, so two entries
    /// sharing one would print a pair of commands where the second silently
    /// redefines the first — and the user's `think` tier would end up on
    /// whichever they happened to run last.
    #[test]
    fn the_id_suggestions_are_unique() {
        let catalog = recipe_catalog();
        let distinct: BTreeSet<&str> = catalog
            .iter()
            .map(|r| r.id_suggestion.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            distinct.len(),
            catalog.len(),
            "duplicate suggested provider id in: {:?}",
            catalog.iter().map(|r| &r.id_suggestion).collect::<Vec<_>>()
        );
    }

    /// Labels are equally load-bearing: they are how a user names the vendor to
    /// the agent ("hook up Kimi"), and how the prose gates match a guide line
    /// back to its entry. Two entries sharing a label would make that match
    /// ambiguous in a way only iteration order resolves.
    #[test]
    fn the_labels_are_unique() {
        let catalog = recipe_catalog();
        let distinct: BTreeSet<&str> = catalog
            .iter()
            .map(|r| r.label.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            distinct.len(),
            catalog.len(),
            "duplicate vendor label in: {:?}",
            catalog.iter().map(|r| &r.label).collect::<Vec<_>>()
        );
    }

    /// **BR-6, inherited: no field may carry a credential.**
    ///
    /// This catalog has no auth field to begin with, which is the real
    /// guarantee — but "there is nowhere to put one" is a claim about a struct
    /// that somebody will one day add a field to, and the moment a field is
    /// added is exactly the moment a sweep must not be quietly partial. So the
    /// field list is **destructured**, not hand-picked: a new
    /// [`ProviderRecipe`] field breaks this build until somebody decides whether
    /// it can carry a secret.
    ///
    /// The checks are shape-based rather than a name denylist, matching the web
    /// catalog's reasoning: a secret is a long opaque run, a URL with userinfo,
    /// or a query parameter *named* like one — none of which a field name
    /// predicts.
    #[test]
    fn no_field_carries_anything_secret_shaped() {
        let catalog = recipe_catalog();
        let recipe_count = catalog.len();

        let mut fields: Vec<String> = Vec::new();
        for recipe in catalog {
            let ProviderRecipe {
                id_suggestion,
                label,
                kind,
                endpoint,
                example_model,
                notes,
            } = recipe;
            fields.push(id_suggestion);
            fields.push(label);
            fields.push(example_model);
            fields.extend(endpoint);
            fields.extend(notes);
            // The one field swept by inspection rather than by content: a
            // four-variant enum has no room for a secret.
            let _ = kind;
        }

        // Non-vacuity, derived rather than a bare literal so it moves with the
        // catalog: every recipe contributes its three non-optional strings, so a
        // sweep reading fewer than that is reading something other than this
        // catalog.
        const ALWAYS_PRESENT_PER_RECIPE: usize = 3; // id_suggestion, label, example_model
        let floor = ALWAYS_PRESENT_PER_RECIPE * recipe_count;
        assert!(
            recipe_count >= 6,
            "the sweep has {recipe_count} recipes to sweep; the roster documents six"
        );
        assert!(
            fields.len() >= floor,
            "the field sweep collected {} values for {recipe_count} recipes, fewer than the \
             {floor} they always carry: {fields:?}",
            fields.len()
        );

        for field in &fields {
            assert!(
                !has_opaque_run(field),
                "{field:?} contains a run long enough to be a credential"
            );
            assert!(
                !has_userinfo(field),
                "{field:?} embeds credentials in a URL's userinfo"
            );
            assert!(
                !crate::runtime::endpoint_query_names_a_credential(field),
                "{field:?} carries a query parameter named like a credential; a recipe endpoint \
                 ends up in the user's config file and shell history"
            );
        }
    }

    /// Whether `text` holds an unbroken run of key-ish characters long enough to
    /// be a real key.
    ///
    /// The thresholds and their reasoning are [`crate::web_setup_catalog`]'s: a
    /// bare token is alphanumeric throughout and nothing legitimate reaches
    /// twenty such characters between separators, while a segmented key (a UUID,
    /// `sk-ant-…`, a dotted JWT) is measured with `-` and `.` treated as
    /// run-internal, which raises the ceiling above hostnames.
    ///
    /// Duplicated rather than shared because it is a *test* helper on the other
    /// side of a `#[cfg(test)]` boundary, and because a future legitimate value
    /// that trips it here gets a named per-value allowance beside the golden
    /// test — never a raised threshold, which is how a sweep stops catching
    /// anything.
    fn has_opaque_run(text: &str) -> bool {
        const BARE_RUN: usize = 20;
        const SEGMENTED_RUN: usize = 24;

        let bare = text
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .any(|run| run.len() >= BARE_RUN);
        let segmented = text
            .split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')))
            .any(|run| run.len() >= SEGMENTED_RUN);
        bare || segmented
    }

    /// Whether `text` carries userinfo before a host — the shape a credential
    /// takes when it hides inside an endpoint.
    ///
    /// **Any** non-empty userinfo, not just `user:pass@`: a bare-token userinfo
    /// (`https://a1b2c3tok@api.x.ai/v1`) is the form an API key actually takes
    /// when somebody pastes one into a URL, and it carries no colon at all. The
    /// scheme is optional for the same reason it is in the web catalog's copy —
    /// gating on `://` was how a scheme-less spelling swept clean there.
    fn has_userinfo(text: &str) -> bool {
        let after = text.split_once("://").map_or(text, |(_, after)| after);
        let authority = after.split(['/', '?', '#']).next().unwrap_or("");
        matches!(authority.split_once('@'), Some((userinfo, _)) if !userinfo.is_empty())
    }

    /// **The purity pin (LESSON-481).**
    ///
    /// Every test above calls the factory with no fixture, no environment and no
    /// daemon; this one says so out loud — two calls with nothing done between
    /// them return the same catalog. Add a config read or a TTY check and the
    /// signature would have to change to accommodate it, which is the point: the
    /// compiler refuses the gate before this test has to catch it.
    #[test]
    fn the_factory_needs_no_setup_and_answers_the_same_every_time() {
        assert_eq!(recipe_catalog(), recipe_catalog());
    }
}
