//! REQ-578 ADR-2: **the composition rule and the vendor catalog are one
//! contract spelled twice, and this file makes them fail together.**
//!
//! Two modules now hold the same fact about every vendor Teton documents. The
//! catalog (`tetond::provider_recipes`) ships each vendor's absolute request
//! URL, path included, because Teton POSTs `--endpoint` verbatim. The
//! composition module (`teton_core::endpoint_composition`) holds the *kind's*
//! canonical request path, because a user who pastes the base URL their vendor
//! documents has to end up at that same absolute URL. Nothing in the language
//! ties the two together: the catalog's `…/v1/chat/completions` and the
//! module's `/chat/completions` are independent string literals in different
//! crates, and the day one moves without the other, a `provider add` composed
//! from a vendor's documented base URL silently stops matching the recipe the
//! product's own guidance prints. That failure reaches the user as a 404 on
//! their first turn — one step removed from its cause, which is the texture
//! BUG-170 and LESSON-523 are both about.
//!
//! ## Why the pin lives here and not in either module
//!
//! AC-6 freezes three files for this REQ — `provider_recipes.rs`,
//! `teton-providers/tests/conformance.rs`, and `web_setup_contracts.rs` —
//! precisely so the REQ-577 guarantees stay *evidence* rather than something
//! this REQ re-tuned to fit. The seam test's hand-written `required_suffix`
//! match therefore stays exactly as REQ-577 wrote it, and the module↔catalog
//! agreement is asserted from outside both, in a new file that touches neither.
//!
//! ## The three legs
//!
//! | Leg | Claim | Test |
//! |----|----|----|
//! | (a) | idempotence: a shipped recipe endpoint composes to itself, `changed: false` | [`every_recipe_endpoint_composes_to_itself`] |
//! | (b) | base→full: the vendor-documented base form composes to exactly the recipe endpoint, `changed: true` | [`every_recipe_base_form_composes_to_the_recipe_endpoint`] |
//! | (c) | the Anthropic default constant is the Anthropic recipe's endpoint | [`the_anthropic_default_is_the_anthropic_recipes_endpoint`] |
//!
//! ## Base forms are derived, never typed
//!
//! Leg (b) needs each vendor's documented base URL, and writing those out by
//! hand would put a *fourth* copy of six third-party facts in the repository —
//! one that goes stale silently, because nothing would fail when a vendor moved
//! a host. So the base form is derived from the recipe endpoint by stripping
//! the kind's [`canonical_request_path`]: the bare-`/v1` form falls out for the
//! vendors whose documented base carries a version segment (OpenAI, Moonshot,
//! Ollama, xAI) and the bare origin for the two whose base does not (DeepSeek,
//! whose docs give `https://api.deepseek.com`, and Anthropic, whose Messages
//! path is versioned as a unit). A vendor that changes host keeps this file
//! green; only genuine drift between the *rule* and the *catalog* turns it red.
//!
//! ## What this file deliberately does not assert (the ADR-2 known limit)
//!
//! A bare **origin** for a `/v1`-family vendor — `https://api.openai.com` —
//! composes to `https://api.openai.com/chat/completions`, which OpenAI does not
//! serve. That is BR-2 as specified, not a defect this test should paper over:
//! a kind-level rule cannot know whether `/v1` belongs, since DeepSeek's
//! documented base has none. The mitigations live elsewhere by design — vendors
//! document the base *with* `/v1` where it belongs, so the common paste
//! composes correctly; BR-4's echo shows the composed value before any key is
//! read; and doctor's BR-6 advisory names the full form. Adding an origin sweep
//! here would assert a per-vendor rule the module does not implement and is out
//! of this REQ's scope.
//!
//! ## AC-7's mutation vehicle
//!
//! Leg (b) is the one this REQ's mutation check aims at: stub `compose_endpoint`
//! to return its input verbatim and leg (b) fails on the missing canonical path
//! — the composed value is the base form, not the request URL — while legs (a)
//! and (c) stay green, because an identity function is idempotent and the
//! constants still agree. That asymmetry is the point: the failure names the
//! removed behaviour rather than an adjacent assertion.

use teton_core::entities::ProviderKind;
use teton_core::{canonical_request_path, compose_endpoint, ANTHROPIC_DEFAULT_ENDPOINT};
use tetond::provider_recipes::{recipe_catalog, ProviderRecipe};

/// The roster REQ-577 documents, as a non-vacuity floor.
///
/// Every sweep below counts what it actually asserted and checks it against
/// this, rather than trusting a `for` loop over a catalog it never proves it
/// read. A filter that quietly matched nothing — the classic way a sweep turns
/// into a green no-op — is then a failure with a number in it.
const ROSTER: usize = 6;

/// What a maintainer standing over a red assertion in this file needs told.
///
/// The instruction matters more than the diff, and it is the *opposite* of the
/// tempting local fix. Both sides of this bridge look obviously right in
/// isolation: the catalog entry cites the vendor page it was read from, and the
/// composition rule cites the vendor page *it* was read from. So the fix is
/// never to delete the assertion or to bend one spelling to the other — it is
/// to re-read the vendor's current documentation and reconcile every spelling
/// that carries the fact.
fn reconcile(what: &str) -> String {
    format!(
        "{what}.\n\nThis is one contract with more than one spelling, and they have drifted. \
         The rule lives in `crates/teton-core/src/endpoint_composition.rs` \
         (`canonical_request_path` and its constants); the vendor request URLs live in \
         `crates/tetond/src/provider_recipes.rs`; the REQ-577 seam test's `required_suffix` \
         match in that same file is a third. Re-verify the path against the vendor's current \
         public documentation, then update ALL of them together and refresh the entry's \
         `Verified <date>` comment. Deleting this assertion — or either spelling — is never \
         the fix: what it stands between is a registration that validates cleanly and 404s on \
         the user's first turn, a step away from its cause (BUG-170, LESSON-523)."
    )
}

/// The recipe's endpoint, or a failure that sends the reader to the catalog's
/// own sweep rather than letting this file report a confusing second symptom.
fn endpoint_of(recipe: &ProviderRecipe) -> &str {
    recipe.endpoint.as_deref().unwrap_or_else(|| {
        panic!(
            "`{}` ships no endpoint, so there is nothing to compose. Every recipe names a \
             remote kind, and `Config::validate` demands an endpoint for those — \
             `an_endpoint_is_present_exactly_when_the_kind_needs_one` in \
             crates/tetond/src/provider_recipes.rs owns that claim and should be red too.",
            recipe.id_suggestion
        )
    })
}

/// The canonical request path for the kind this recipe names.
///
/// A `None` here is drift of its own kind: the catalog would be documenting a
/// vendor whose protocol the composition module has no path for, so a user
/// pasting that vendor's base URL would get it stored uncomposed.
fn canonical_for(recipe: &ProviderRecipe) -> &'static str {
    canonical_request_path(recipe.kind).unwrap_or_else(|| {
        panic!(
            "{}",
            reconcile(&format!(
                "`{}` is kind {:?}, for which `canonical_request_path` returns `None` — the \
                 catalog documents a vendor the composition rule has no request path for, so \
                 that vendor's base URL would be stored exactly as pasted",
                recipe.id_suggestion, recipe.kind
            ))
        )
    })
}

/// The **vendor-documented base form** of a recipe endpoint: the recipe's
/// absolute request URL with the kind's canonical request path stripped off.
///
/// Derived rather than written down, for the reason in this file's header — and
/// checked twice on the way: the endpoint has to *carry* the canonical path
/// (REQ-577's seam requirement, restated here because leg (b) is meaningless
/// without it), and what is left has to be a shape the module would still
/// compose onto, or leg (b) would be leg (a) wearing a different name.
fn base_form(recipe: &ProviderRecipe) -> String {
    let endpoint = endpoint_of(recipe);
    let canonical = canonical_for(recipe);

    let base = endpoint.strip_suffix(canonical).unwrap_or_else(|| {
        panic!(
            "{}",
            reconcile(&format!(
                "`{}`'s endpoint `{endpoint}` does not end with `{canonical}`, the request \
                 path {:?} providers serve, so there is no base form to derive from it",
                recipe.id_suggestion, recipe.kind
            ))
        )
    });

    assert!(
        !base.ends_with(canonical),
        "{}",
        reconcile(&format!(
            "`{}`'s endpoint `{endpoint}` carries `{canonical}` twice, so the base form \
             derived from it (`{base}`) is still a full request URL and leg (b) would be \
             asserting class (a) over again",
            recipe.id_suggestion
        ))
    );

    base.to_owned()
}

/// **Leg (a): every shipped recipe endpoint is a fixed point of the rule.**
///
/// This is AC-2's property stated against the real catalog rather than against
/// sample strings: the endpoints the product's own guidance prints are already
/// absolute request URLs, so running one through the registration seam has to
/// return it byte-identically and report `changed: false`. A `true` there is not
/// cosmetic — it is the flag `provider add` reads to decide whether to echo a
/// correction (BR-4), and the predicate doctor reads to decide whether to advise
/// (BR-6). A rule that "corrected" its own catalog would tell every user of a
/// documented recipe that their endpoint was wrong.
#[test]
fn every_recipe_endpoint_composes_to_itself() {
    let catalog = recipe_catalog();
    let mut swept = 0usize;

    for recipe in &catalog {
        let endpoint = endpoint_of(recipe);
        let composed = compose_endpoint(recipe.kind, Some(endpoint));

        assert_eq!(
            composed.stored.as_deref(),
            Some(endpoint),
            "{}",
            reconcile(&format!(
                "composing `{}`'s catalog endpoint `{endpoint}` returned {:?} instead of the \
                 endpoint itself. A shipped recipe is already the absolute URL the {:?} \
                 adapter POSTs, so BR-2's class (a) has to hand it back untouched",
                recipe.id_suggestion, composed.stored, recipe.kind
            ))
        );
        assert!(
            !composed.changed,
            "{}",
            reconcile(&format!(
                "composing `{}`'s catalog endpoint `{endpoint}` returned the same string but \
                 flagged it as changed, so `provider add` would echo a correction it never \
                 made (BR-4) and `doctor` would advise on an endpoint that is already right \
                 (BR-6)",
                recipe.id_suggestion
            ))
        );

        swept += 1;
    }

    assert!(
        swept >= ROSTER,
        "leg (a) swept {swept} recipes; the roster documents {ROSTER}. A sweep narrower than \
         the catalog is a green test that is not testing the catalog."
    );
}

/// **Leg (b): the vendor's documented base URL composes to the recipe's URL.**
///
/// The claim this whole REQ exists to make, checked against the six vendors the
/// product actually documents: a user who pastes what their vendor's quickstart
/// hands an OpenAI-compatible SDK ends up with the exact endpoint Teton's own
/// recipe would have given them, and is told so (`changed: true` is what drives
/// BR-4's echo).
///
/// The base forms are derived by [`base_form`], which means this test says
/// nothing about hosts and everything about *paths* — the only thing the
/// composition rule decides.
///
/// **AC-7's mutation target.** Stub `compose_endpoint` to return its input and
/// this is the leg that goes red, on the missing canonical path.
#[test]
fn every_recipe_base_form_composes_to_the_recipe_endpoint() {
    let catalog = recipe_catalog();
    let mut swept = 0usize;

    for recipe in &catalog {
        let endpoint = endpoint_of(recipe);
        let canonical = canonical_for(recipe);
        let base = base_form(recipe);
        let composed = compose_endpoint(recipe.kind, Some(base.as_str()));

        assert_eq!(
            composed.stored.as_deref(),
            Some(endpoint),
            "{}",
            reconcile(&format!(
                "composing `{}`'s base form `{base}` produced {:?}, not the catalog endpoint \
                 `{endpoint}`. The vendor's documented base URL is what a user pastes; \
                 `{canonical}` is the request path {:?} providers serve, and completing the \
                 one into the other is the rule's entire job",
                recipe.id_suggestion, composed.stored, recipe.kind
            ))
        );
        assert!(
            composed.changed,
            "{}",
            reconcile(&format!(
                "composing `{}`'s base form `{base}` reached `{endpoint}` but reported nothing \
                 changed, so `provider add` would silently rewrite what the user typed. BR-4 \
                 exists because the moment to show a user the URL that will be called is the \
                 moment it is decided — not after a 404",
                recipe.id_suggestion
            ))
        );

        swept += 1;
    }

    assert!(
        swept >= ROSTER,
        "leg (b) swept {swept} recipes; the roster documents {ROSTER}. A sweep narrower than \
         the catalog is a green test that is not testing the catalog."
    );
}

/// **Leg (c): BR-3's default is the catalog's Anthropic address.**
///
/// `ANTHROPIC_DEFAULT_ENDPOINT` is written into a user's config when they
/// register `--kind anthropic` with no `--endpoint`, and the catalog's Anthropic
/// recipe is the endpoint the same user would be handed if they asked the agent
/// how to hook Claude up. Two paths to one registration, so one string: if they
/// ever disagree, a user's config depends on which route they took.
#[test]
fn the_anthropic_default_is_the_anthropic_recipes_endpoint() {
    let catalog = recipe_catalog();
    let mut swept = 0usize;

    for recipe in &catalog {
        if recipe.kind != ProviderKind::Anthropic {
            continue;
        }
        let endpoint = endpoint_of(recipe);

        assert_eq!(
            endpoint,
            ANTHROPIC_DEFAULT_ENDPOINT,
            "{}",
            reconcile(&format!(
                "`{}` ships `{endpoint}` while `ANTHROPIC_DEFAULT_ENDPOINT` is \
                 `{ANTHROPIC_DEFAULT_ENDPOINT}`. These are the same registration reached two \
                 ways — `provider add --kind anthropic` with no `--endpoint`, and the recipe \
                 Teton's guidance prints — so a user's stored config would depend on which \
                 route they took",
                recipe.id_suggestion
            ))
        );

        swept += 1;
    }

    assert!(
        swept >= 1,
        "no recipe names {:?}, so this leg asserted nothing. The default endpoint constant \
         exists because the catalog documents that vendor; if the catalog no longer does, the \
         constant and its BR-3 default need a decision, not a skipped test.",
        ProviderKind::Anthropic
    );
}
