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
//! # An endpoint here is a whole request URL, not a base URL
//!
//! This is the fact the first version of this file got wrong, and it is worth
//! stating before the data rather than after it. Teton's `--endpoint` is the
//! **absolute URL the adapter POSTs to, verbatim** — pinned against a capturing
//! transport by `a_configured_endpoint_is_the_request_url_verbatim`
//! (`teton-providers/tests/conformance.rs`), which drives both adapters with a
//! sentinel URL and asserts it comes back out of the request unchanged. Nothing
//! anywhere joins a path onto it. So a recipe carrying a vendor's *`base_url`* — the value
//! their SDK quickstart hands to an OpenAI client, which the client then
//! appends `/chat/completions` to — describes a provider that registers
//! cleanly, passes validation, and 404s on its first turn. That failure lands a
//! step away from its cause, which is the same BUG-165 texture this module was
//! written to end, arriving through the module itself (BUG-170).
//!
//! Every endpoint below is therefore the vendor's documented request URL,
//! path included, and the seam test at the bottom of this file asserts the
//! shape against the adapter each `kind` selects rather than trusting the
//! spelling to stay right.
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
//! The endpoints were re-read a **second** time the same day, against the same
//! vendor pages, once the base-URL/request-URL confusion above was found. Three
//! of the six vendors had moved their documentation *host* since the first
//! pass (`platform.moonshot.ai` → `platform.kimi.ai`, `docs.claude.com` →
//! `platform.claude.com`, and OpenAI's `platform.openai.com/docs/api-reference`
//! → `developers.openai.com/api/reference`, the old path now answering 403
//! rather than redirecting). None of the *API* hosts moved with them, which is
//! the distinction the per-entry comments record.
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
    /// The same vendor, spelled the way the **bundled guide's** recipe line
    /// spells it — `Moonshot/Kimi` where [`label`](Self::label) is
    /// `Moonshot (Kimi)`.
    ///
    /// A second spelling is here rather than in a test's lookup table because
    /// it is product data with a product reason: the guide is resident prompt
    /// under a byte ceiling with double-digit margin, so it abbreviates the two
    /// vendors whose names carry a parenthetical, and it uses a slash because
    /// both halves are names a user might type. That is a decision about what
    /// ships, not a formatting accident, and the surface that owns it is this
    /// one.
    ///
    /// What it buys the gates is exactness. Without it, the guide↔catalog check
    /// can pair a vendor with an endpoint only by *position*, which passes a
    /// line that swapped two vendors' names over their neighbours' URLs — six
    /// verified facts, two of them attached to the wrong company. With it, the
    /// pairing is a string comparison against data the catalog ships.
    pub guide_spelling: String,
    /// Which adapter the vendor speaks, and therefore which URL shape
    /// [`endpoint`](Self::endpoint) has to be.
    pub kind: ProviderKind,
    /// The **absolute request URL** to pass as `--endpoint` — what the adapter
    /// POSTs to, character for character, with no path joined on.
    ///
    /// Not a base URL. A vendor's quickstart `base_url` is what an OpenAI SDK
    /// appends `/chat/completions` to; Teton has no such step, so a base URL
    /// here is a provider that registers and then 404s (BUG-170).
    ///
    /// `Option` rather than `String` because the *type* admits a kind that
    /// carries its own address, and `ProviderKind` is wider than this catalog.
    /// Today every registerable vendor kind is
    /// [`ProviderKind::is_remote`](teton_core::entities::ProviderKind::is_remote),
    /// and `Config::validate` rejects a remote provider whose endpoint is
    /// missing or blank — so today every entry is `Some`, and the test below
    /// derives that requirement from `is_remote` rather than restating it.
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
        // Verified 2026-08-14 (round 2) against Anthropic's Messages API
        // reference (platform.claude.com/docs/en/api/messages — docs.claude.com
        // now 301s there), whose curl example posts to
        // `https://api.anthropic.com/v1/messages`. Model id from the same
        // site's models overview: `claude-opus-5` is a current API id and the
        // one an unsure reader is told to start with. (The reference page's own
        // example body still names `claude-opus-4-6`, which that overview lists
        // under legacy models — a reminder that a vendor's curl block and its
        // model table are two facts, checked separately.)
        ProviderRecipe {
            id_suggestion: "anthropic".to_owned(),
            label: "Anthropic".to_owned(),
            guide_spelling: "Anthropic".to_owned(),
            kind: ProviderKind::Anthropic,
            // Round 1 shipped `None` here on the theory that "the adapter
            // carries the Messages API address". It does not: `AnthropicAdapter`
            // is constructed from `provider.endpoint` like every other kind, and
            // `Config::validate` refuses a remote provider without one. The
            // recipe that omitted it produced a `provider add` that stored the
            // user's key in the keychain and *then* had the registration
            // rejected (BUG-170).
            endpoint: Some("https://api.anthropic.com/v1/messages".to_owned()),
            example_model: "claude-opus-5".to_owned(),
            // The one vendor here whose path is not `/chat/completions`: this
            // is the Messages API, a different protocol, which is exactly what
            // `--kind anthropic` selects. A reader pattern-matching the five
            // neighbours onto this line gets a 404.
            notes: Some("the Messages API path, not /chat/completions".to_owned()),
        },
        // Verified 2026-08-14 (round 2) against OpenAI's API reference
        // (developers.openai.com/api/reference — platform.openai.com's old
        // `/docs/api-reference` path now answers 403 rather than redirecting),
        // whose curl example posts to
        // `https://api.openai.com/v1/chat/completions`. The models page states
        // that `gpt-5.6` is an alias onto GPT-5.6 Sol (`gpt-5.6-sol`); the alias
        // is the better example because it does not pin a user to one snapshot.
        ProviderRecipe {
            id_suggestion: "openai".to_owned(),
            label: "OpenAI".to_owned(),
            guide_spelling: "OpenAI".to_owned(),
            // The kind is named after this API because this API is the shape
            // every other entry below imitates; OpenAI gets no special adapter.
            kind: ProviderKind::OpenaiCompatible,
            endpoint: Some("https://api.openai.com/v1/chat/completions".to_owned()),
            example_model: "gpt-5.6".to_owned(),
            notes: None,
        },
        // Verified 2026-08-14 (round 2) against Kimi's chat API page
        // (platform.kimi.ai/docs/api/chat — platform.moonshot.ai now 301s
        // there, while the *API* host is unchanged), whose curl example posts to
        // `https://api.moonshot.ai/v1/chat/completions`. The same page's model
        // list carries `kimi-k3` alongside the `kimi-k2.x` line. The README's
        // `kimi-k2` was the drift this REQ's prose gate exists to catch — it was
        // already stale when this line was first written.
        ProviderRecipe {
            id_suggestion: "kimi".to_owned(),
            label: "Moonshot (Kimi)".to_owned(),
            guide_spelling: "Moonshot/Kimi".to_owned(),
            kind: ProviderKind::OpenaiCompatible,
            endpoint: Some("https://api.moonshot.ai/v1/chat/completions".to_owned()),
            example_model: "kimi-k3".to_owned(),
            notes: None,
        },
        // Verified 2026-08-14 (round 2) against DeepSeek's API docs
        // (api-docs.deepseek.com): the "Your First API Call" curl posts to
        // `https://api.deepseek.com/chat/completions`, and the parameters table
        // gives the OpenAI-format `base_url` as `https://api.deepseek.com` with
        // no `/v1`. The `/v1` spelling is **not** in the current documentation
        // at all — the old "v1 has no relationship with the model's version"
        // note is gone — though it still routes (an unauthenticated POST to
        // either form answers 401, not 404). Undocumented-but-working is not a
        // fact this catalog ships, so the documented form is the one here.
        // `deepseek-v4-pro` is current (GA 2026-08-13, `deepseek-v4-flash` is
        // the other); the REQ's drafted `deepseek-chat`/`deepseek-reasoner` pair
        // was retired outright on 2026-07-24 and is dead rather than deprecated.
        ProviderRecipe {
            id_suggestion: "deepseek".to_owned(),
            label: "DeepSeek".to_owned(),
            guide_spelling: "DeepSeek".to_owned(),
            kind: ProviderKind::OpenaiCompatible,
            endpoint: Some("https://api.deepseek.com/chat/completions".to_owned()),
            example_model: "deepseek-v4-pro".to_owned(),
            // The one OpenAI-compatible URL here with no `/v1` segment, which is
            // exactly the kind of small difference a user pattern-matches away
            // from its five neighbours and then spends an afternoon on. Stated
            // as a fact about *this path* rather than about a "base URL": the
            // round-1 note said the base URL took no suffix, which was true of a
            // value Teton never asks for and read as licence to hand the adapter
            // a base URL.
            notes: Some("the only /chat/completions path here with no /v1".to_owned()),
        },
        // Verified 2026-08-14 (round 2) against Ollama's OpenAI-compatibility
        // page (docs.ollama.com/openai), whose curl posts to
        // `http://localhost:11434/v1/chat/completions` and whose Python example
        // carries `'api_key': 'ollama',  # required but ignored`. `llama3.2` is
        // a live library tag (ollama.com/library/llama3.2). The host is an
        // example of a shape rather than an address — a user running Ollama
        // elsewhere substitutes their own.
        ProviderRecipe {
            id_suggestion: "ollama".to_owned(),
            label: "Ollama".to_owned(),
            guide_spelling: "Ollama".to_owned(),
            kind: ProviderKind::OpenaiCompatible,
            endpoint: Some("http://localhost:11434/v1/chat/completions".to_owned()),
            example_model: "llama3.2".to_owned(),
            // Round 1 called this entry "keyless", which is true of the *server*
            // and false of the *command*: `teton provider add` reads a secret
            // for every kind but `local` (teton/src/main.rs), so a user told
            // there is "no key step at all" meets a prompt the recipe said would
            // not come. Both halves are stated, because only saying the second
            // would make Ollama look like it needs an account.
            notes: Some(
                "Ollama ignores the key, but provider add still asks for one — any \
                 placeholder does"
                    .to_owned(),
            ),
        },
        // Verified 2026-08-14 (round 2) against xAI's API reference
        // (docs.x.ai/docs/api-reference), whose curl posts to
        // `https://api.x.ai/v1/chat/completions`, and its models page, which
        // lists `grok-4.6` alongside `grok-4.5` and `grok-4.3` and calls it the
        // most capable of them.
        ProviderRecipe {
            id_suggestion: "grok".to_owned(),
            label: "Grok (xAI)".to_owned(),
            guide_spelling: "Grok/xAI".to_owned(),
            kind: ProviderKind::OpenaiCompatible,
            endpoint: Some("https://api.x.ai/v1/chat/completions".to_owned()),
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
            anthropic.guide_spelling,
            "Anthropic",
            "{}",
            drift("Anthropic's guide spelling")
        );
        assert_eq!(
            anthropic.kind,
            ProviderKind::Anthropic,
            "{}",
            drift("Anthropic's provider kind")
        );
        assert_eq!(
            anthropic.endpoint.as_deref(),
            Some("https://api.anthropic.com/v1/messages"),
            "{}",
            drift("Anthropic's Messages API request URL")
        );
        assert_eq!(
            anthropic.example_model,
            "claude-opus-5",
            "{}",
            drift("Anthropic's example model")
        );
        assert_eq!(
            anthropic.notes.as_deref(),
            Some("the Messages API path, not /chat/completions"),
            "{}",
            drift("Anthropic's note")
        );

        let openai = &catalog[1];
        assert_eq!(openai.label, "OpenAI", "{}", drift("OpenAI's label"));
        assert_eq!(
            openai.guide_spelling,
            "OpenAI",
            "{}",
            drift("OpenAI's guide spelling")
        );
        assert_eq!(
            openai.kind,
            ProviderKind::OpenaiCompatible,
            "{}",
            drift("OpenAI's provider kind")
        );
        assert_eq!(
            openai.endpoint.as_deref(),
            Some("https://api.openai.com/v1/chat/completions"),
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
            kimi.guide_spelling,
            "Moonshot/Kimi",
            "{}",
            drift("Moonshot's guide spelling — the guide abbreviates under its byte ceiling")
        );
        assert_eq!(
            kimi.kind,
            ProviderKind::OpenaiCompatible,
            "{}",
            drift("Moonshot's provider kind")
        );
        assert_eq!(
            kimi.endpoint.as_deref(),
            Some("https://api.moonshot.ai/v1/chat/completions"),
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
            deepseek.guide_spelling,
            "DeepSeek",
            "{}",
            drift("DeepSeek's guide spelling")
        );
        assert_eq!(
            deepseek.kind,
            ProviderKind::OpenaiCompatible,
            "{}",
            drift("DeepSeek's provider kind")
        );
        assert_eq!(
            deepseek.endpoint.as_deref(),
            Some("https://api.deepseek.com/chat/completions"),
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
            Some("the only /chat/completions path here with no /v1"),
            "{}",
            drift("DeepSeek's note")
        );

        let ollama = &catalog[4];
        assert_eq!(ollama.label, "Ollama", "{}", drift("Ollama's label"));
        assert_eq!(
            ollama.guide_spelling,
            "Ollama",
            "{}",
            drift("Ollama's guide spelling")
        );
        assert_eq!(
            ollama.kind,
            ProviderKind::OpenaiCompatible,
            "{}",
            drift("Ollama's provider kind")
        );
        assert_eq!(
            ollama.endpoint.as_deref(),
            Some("http://localhost:11434/v1/chat/completions"),
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
            Some("Ollama ignores the key, but provider add still asks for one — any placeholder does"),
            "{}",
            drift("Ollama's note, which is the only place the key step is qualified")
        );

        let grok = &catalog[5];
        assert_eq!(grok.label, "Grok (xAI)", "{}", drift("Grok's label"));
        assert_eq!(
            grok.guide_spelling,
            "Grok/xAI",
            "{}",
            drift("Grok's guide spelling — the guide abbreviates under its byte ceiling")
        );
        assert_eq!(
            grok.kind,
            ProviderKind::OpenaiCompatible,
            "{}",
            drift("Grok's provider kind")
        );
        assert_eq!(
            grok.endpoint.as_deref(),
            Some("https://api.x.ai/v1/chat/completions"),
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

    /// An endpoint is required exactly when
    /// [`ProviderKind::is_remote`] says the daemon will demand one.
    ///
    /// **Keyed off `is_remote`, not off a hand-written match**, and that is the
    /// whole substance of this test. Round 1 spelled the rule out here as
    /// `matches!(kind, OpenaiCompatible)` — a second, independent opinion about
    /// which kinds need an endpoint — and it disagreed with the one that
    /// decides: `Config::validate` demands an endpoint for every `is_remote()`
    /// kind, and `Anthropic` is one. The catalog shipped `endpoint: None` for
    /// Anthropic and this test *agreed with it*, because both sides were the
    /// same author's belief rather than the daemon's rule. A recipe is only
    /// checked against the config it has to produce when the check reads the
    /// predicate the config layer reads (BUG-170).
    ///
    /// The two failures it now catches are asymmetric and both silent. A remote
    /// kind missing an endpoint yields a `provider add` that stores the user's
    /// key and is then refused by `config/set`. A non-remote kind carrying one
    /// yields a flag the adapter never reads.
    #[test]
    fn an_endpoint_is_present_exactly_when_the_kind_needs_one() {
        for recipe in recipe_catalog() {
            assert_eq!(
                recipe.endpoint.is_some(),
                recipe.kind.is_remote(),
                "`{}` is kind {:?}, whose `is_remote()` is {} — and `Config::validate` \
                 demands an endpoint for exactly the remote kinds — but this recipe has \
                 endpoint={:?}",
                recipe.id_suggestion,
                recipe.kind,
                recipe.kind.is_remote(),
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

    /// **The class-closer: every recipe is a `provider add` that both the config
    /// layer and the adapter accept** (BUG-170).
    ///
    /// The golden above pins the strings and the sweep above pins the rule, and
    /// round 1 had both — green, complete, and describing two commands that
    /// could not work. What neither of them did was carry a recipe across the
    /// seam it exists to be pushed through. This test does exactly that, twice:
    ///
    /// 1. **Would `provider add` survive?** The recipe is assembled into the
    ///    [`ModelProvider`] that `teton provider add` builds — same kind, same
    ///    endpoint, same model, an `auth_ref` in the keychain's own form — and
    ///    dropped into a [`Config`] that must then [`Config::validate`]. This is
    ///    the check `config/set` runs before persisting, so a recipe that fails
    ///    here is a recipe whose command is refused *after* the user's key is
    ///    already in the keychain.
    ///
    /// 2. **Would the first turn reach anything?** The endpoint is POSTed
    ///    verbatim, so the path has to be the one that adapter's protocol
    ///    serves: `/chat/completions` for the OpenAI shape, `/v1/messages` for
    ///    Anthropic's. A vendor `base_url` passes every other test in this file
    ///    and 404s here.
    ///
    ///    "Verbatim" is not this test's claim to make, and it used to be
    ///    asserted here only as a sentence citing two lines of adapter source —
    ///    which is code inspection standing in for a wire fact, and the wrong
    ///    half of the belief that produced BUG-170 in the first place. It is
    ///    pinned where it belongs, against a capturing transport:
    ///    `teton-providers/tests/conformance.rs`,
    ///    `a_configured_endpoint_is_the_request_url_verbatim`. If that test ever
    ///    goes red, the suffix table below is describing a contract that no
    ///    longer holds and this test is worse than useless — it would be
    ///    enforcing a path the adapter no longer requests.
    ///
    /// The failure messages state the verbatim-POST contract rather than naming
    /// the expected suffix, because the next person to see one will be adding a
    /// seventh vendor from its quickstart page — and a quickstart page is where
    /// base URLs come from.
    #[test]
    fn every_recipe_is_a_registration_the_daemon_accepts_and_an_adapter_can_post() {
        use teton_core::config::Config;
        use teton_core::entities::{ModelProvider, ProviderCapabilities};

        let catalog = recipe_catalog();
        assert!(
            catalog.len() >= 6,
            "the catalog ships {} recipes; this sweep is narrower than the roster",
            catalog.len()
        );

        for recipe in &catalog {
            // Exactly what `build_provider_registration` assembles, minus the
            // keychain round trip: `auth_ref` is the `keychain://teton/<id>`
            // shape `Keychain::store` returns, because `validate` rejects an
            // unrecognized scheme and a `None` here would test a different
            // command than the one the recipes tell a user to run.
            let provider = ModelProvider {
                id: recipe.id_suggestion.clone(),
                kind: recipe.kind,
                endpoint: recipe.endpoint.clone(),
                model: Some(recipe.example_model.clone()),
                auth_ref: Some(format!("keychain://teton/{}", recipe.id_suggestion)),
                capabilities: ProviderCapabilities::default(),
            };
            let config = Config {
                providers: vec![provider],
                ..Config::default()
            };
            config.validate().unwrap_or_else(|err| {
                panic!(
                    "`teton provider add {}` built from this recipe produces a config the \
                     daemon refuses: {err}. `config/set` validates before it persists, and \
                     `provider add` reads the user's key into the keychain *before* it \
                     calls — so this recipe's command takes a credential and then fails. \
                     Fix the recipe in crates/tetond/src/provider_recipes.rs.",
                    recipe.id_suggestion
                )
            });

            let endpoint = recipe.endpoint.as_deref().unwrap_or_else(|| {
                panic!(
                    "`{}` has no endpoint; the sweep above should have caught that first",
                    recipe.id_suggestion
                )
            });
            // The suffix each adapter's protocol actually serves. Written as a
            // match on `kind` so a new kind cannot be added without deciding
            // what its request path looks like.
            let required_suffix = match recipe.kind {
                ProviderKind::Anthropic => "/v1/messages",
                ProviderKind::OpenaiCompatible => "/chat/completions",
                other => panic!(
                    "`{}` is kind {other:?}, which no vendor recipe may name",
                    recipe.id_suggestion
                ),
            };
            assert!(
                endpoint.ends_with(required_suffix),
                "`{}`'s endpoint is `{endpoint}`, and Teton POSTs `--endpoint` **verbatim** \
                 — nothing joins a path onto it. A {:?} provider's request URL therefore \
                 has to end `{required_suffix}`; a vendor's `base_url` (the value their SDK \
                 quickstart appends a path to) registers cleanly and 404s on the first \
                 turn, a step away from its cause. Re-read the vendor's own curl example \
                 and ship the URL it posts to.",
                recipe.id_suggestion,
                recipe.kind
            );
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

    /// The guide spellings carry a stronger requirement than uniqueness: **no
    /// one may be a substring of another.**
    ///
    /// The guide↔catalog gate identifies a vendor's segment by looking for its
    /// spelling and by checking that no *other* vendor's spelling is in the same
    /// segment. If `Grok` and `Grok/xAI` both existed, every Grok/xAI segment
    /// would contain `Grok` too and the gate would fail on a correct guide —
    /// which is the kind of false failure that gets a check deleted rather than
    /// understood. Non-empty because a `""` spelling is a substring of
    /// everything and would fail the whole roster at once.
    #[test]
    fn no_guide_spelling_is_a_substring_of_another() {
        let catalog = recipe_catalog();
        for recipe in &catalog {
            assert!(
                !recipe.guide_spelling.trim().is_empty(),
                "`{}` has no guide spelling; the guide has to name every vendor it lists",
                recipe.id_suggestion
            );
        }
        for a in &catalog {
            for b in &catalog {
                if a.id_suggestion == b.id_suggestion {
                    continue;
                }
                assert!(
                    !b.guide_spelling.contains(a.guide_spelling.as_str()),
                    "`{}`'s guide spelling {:?} contains `{}`'s {:?}, so the guide gate \
                     cannot tell their recipe segments apart and would fail on a correct \
                     guide. Pick spellings that are distinguishable, not merely different.",
                    b.id_suggestion,
                    b.guide_spelling,
                    a.id_suggestion,
                    a.guide_spelling
                );
            }
        }
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
                guide_spelling,
                kind,
                endpoint,
                example_model,
                notes,
            } = recipe;
            fields.push(id_suggestion);
            fields.push(label);
            fields.push(guide_spelling);
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
        const ALWAYS_PRESENT_PER_RECIPE: usize = 4; // id_suggestion, label, guide_spelling, example_model
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
