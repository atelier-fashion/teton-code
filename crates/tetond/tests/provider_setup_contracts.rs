//! REQ-579 AC-5 / BR-4, ADR-4: **the vendor entry a client renders is the
//! vendor entry the bundled guide is gated against** (TASK-156).
//!
//! One catalog, three consumers. `crates/tetond/src/provider_recipes.rs` ships
//! [`recipe_catalog`], and from it come: the guide's resident recipe line, the
//! `teton_docs` `providers` topic and the README walkthrough (all prose, all
//! gated in `tests/web_setup_contracts.rs`), and — new in this REQ — the
//! `provider/setup_plan` answer the guided flow renders its vendor list from.
//! The prose gates were already bidirectional. What was missing is the leg from
//! the catalog to the **daemon's answer**, and without it "the entry the client
//! renders is the entry the model would have named" is a claim about two
//! functions rather than a property under test.
//!
//! ## What this suite adds that the in-crate tests do not
//!
//! Two unit tests already own the halves:
//!
//! * `provider_recipes::tests::every_recipe_maps_onto_a_wire_entry_field_for_field`
//!   pins the projection — [`recipe_catalog`] onto `recipe_entries()`, by
//!   destructuring, so a seventh recipe field fails the build.
//! * `runtime::tests::provider_setup::the_plan_serves_the_shipped_recipe_catalog_unaltered`
//!   pins the serve — `provider_setup_plan().catalog` against `recipe_entries()`.
//!
//! Neither is the composition, and the composition is the product claim. This
//! file walks it end to end from **outside the crate**: `recipe_catalog()` in
//! one hand, [`DaemonRuntime::provider_setup_plan`]'s answer in the other,
//! compared field by field with the recipe destructured. It stays green only
//! while both halves hold *and* the public surface still exposes what a client
//! reads. A refactor that quietly made the plan answer from a second list would
//! leave both unit tests green and this one red, which is the direction that
//! matters: the second list is how the CLI-side vendor roster BR-4 forbids
//! would arrive.
//!
//! ## What this suite deliberately does **not** check
//!
//! The guide↔catalog leg. `web_setup_contracts::the_bundled_guide_and_the_recipe_catalog_agree`
//! already asserts, per vendor and in both directions, that the guide's recipe
//! segment carries that recipe's `guide_spelling`, endpoint and example model —
//! which is the other half of AC-5, and the half ADR-2's lenient client-side
//! vendor resolver depends on (the model hands off with the spelling the guide
//! taught it). Restating it here would be a second copy of a gate whose failure
//! message already names the file to edit, and two copies of a rule are how the
//! copies come to disagree. It is cited, not repeated.
//!
//! ## AC → test map
//!
//! | AC | Test |
//! |----|------|
//! | AC-5 (typed catalog → the client's view, field for field) | [`every_recipe_reaches_the_plan_field_for_field`] |
//! | BR-4 / BR-14 (the roster is what ships, not what is unregistered) | [`the_plan_offers_the_whole_roster_over_a_configured_daemon`] |

use std::path::PathBuf;
use std::sync::Arc;

use teton_core::entities::ProviderKind;
use tetond::broadcast::EventBus;
use tetond::provider_recipes::{recipe_catalog, ProviderRecipe};
use tetond::runtime::DaemonRuntime;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A config with a provider already registered and a tier already bound — the
/// state [`the_plan_offers_the_whole_roster_over_a_configured_daemon`] is about.
///
/// `deepseek` is deliberately one of the catalog's own vendors: the interesting
/// case is not "some provider exists", it is "the vendor the user is about to
/// set up **again**", which is what a key rotation looks like from this method
/// (BR-14).
const CONFIGURED: &str = "\
[[providers]]
id = \"deepseek\"
kind = \"openai-compatible\"
endpoint = \"https://api.deepseek.com/chat/completions\"
model = \"deepseek-v4-pro\"
auth_ref = \"keychain://teton/deepseek\"

[[tiers]]
tier = \"scan\"
provider_id = \"deepseek\"
";

/// A throwaway directory under the system temp dir, unique per test — the
/// `config_preservation.rs` helper, for its reason: pid plus nanos alone can
/// collide when two tests hit one clock tick.
fn scratch_dir(tag: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "teton-provsetup-{tag}-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    dir
}

/// A daemon over a scratch config file, started the way `main` starts one.
///
/// [`DaemonRuntime::from_env`] rather than `minimal()` wherever the config has
/// to say something: `minimal()` holds `Config::default()` and cannot be seeded,
/// so a claim about a *configured* daemon made over one would be a claim about
/// an empty one.
fn daemon_over(tag: &str, document: &str) -> (DaemonRuntime, PathBuf) {
    assert!(
        std::env::var_os("TETON_CONFIG").is_none(),
        "this suite relies on `from_env` resolving `base_dir/config.toml`; a \
         TETON_CONFIG in the environment would point it at one shared file"
    );
    let dir = scratch_dir(tag);
    std::fs::write(dir.join("config.toml"), document).expect("seed the config file");
    let events = Arc::new(EventBus::new());
    let runtime = DaemonRuntime::from_env(&dir, &events).expect("the daemon starts");
    (runtime, dir)
}

// ---------------------------------------------------------------------------
// The catalog, end to end
// ---------------------------------------------------------------------------

/// **Every recipe this build ships reaches the plan, field for field**
/// (REQ-579 AC-5, BR-4, ADR-4).
///
/// The recipe is **destructured**, which is what makes this total rather than
/// thorough: a seventh field on `ProviderRecipe` stops this file compiling until
/// somebody decides whether the wire carries it. A hand-picked field list would
/// go on passing while the new fact never reached a user — and "never reached a
/// user" is the whole failure mode, because a client renders exactly what this
/// method hands it (BR-4 forbids a second roster CLI-side to fall back on).
///
/// The kind is compared in the **reverse** direction, reading the wire kind back
/// as a domain kind through `teton_core`'s own `From`. Asserting
/// `entry.kind == to_proto_kind(recipe.kind)` would re-run the mapping under
/// test and agree with itself; the round trip is a second, independent opinion,
/// and the two agree only when the mapping is right. Getting it wrong composes
/// the Anthropic Messages recipe onto the OpenAI-compatible adapter, which
/// registers cleanly and 404s on the first turn (BUG-170's shape).
///
/// Order is asserted too, by zipping rather than by lookup: the catalog's order
/// is the one every prose surface renders, and a plan that reordered it would
/// hand the numbered list of AC-3 different numbers than the guide's list.
#[test]
fn every_recipe_reaches_the_plan_field_for_field() {
    let catalog = recipe_catalog();
    // Non-vacuity, twice over: an empty catalog would satisfy the zip below, and
    // a catalog shorter than the shipped roster would satisfy it over a subset.
    assert!(
        catalog.len() >= 6,
        "the recipe catalog ships {} entries; the sweep below is narrower than the roster \
         it is supposed to cover",
        catalog.len()
    );

    let runtime = DaemonRuntime::minimal();
    let served = runtime.provider_setup_plan().catalog;
    assert_eq!(
        served.len(),
        catalog.len(),
        "the daemon serves {} catalog entries for {} shipped recipes, so a vendor this \
         build knows how to register reaches no client — and the flow cannot offer what \
         it is never handed (BR-4)",
        served.len(),
        catalog.len()
    );

    for (recipe, entry) in catalog.into_iter().zip(served) {
        // Destructured, not field-picked: a new recipe field stops compiling
        // here rather than silently staying daemon-side.
        let ProviderRecipe {
            id_suggestion,
            label,
            guide_spelling,
            kind,
            endpoint,
            example_model,
            notes,
        } = recipe;

        assert_eq!(
            entry.id_suggestion, id_suggestion,
            "the plan's roster is not the catalog's, in the catalog's order"
        );
        assert_eq!(entry.label, label, "`{id_suggestion}`'s label");
        assert_eq!(
            entry.guide_spelling, guide_spelling,
            "`{id_suggestion}`'s guide spelling. The lenient vendor resolver (ADR-2) \
             matches `/provider setup <vendor>` against this, and the bundled guide is \
             gated against the same string by \
             `web_setup_contracts::the_bundled_guide_and_the_recipe_catalog_agree` — so a \
             client that never receives it cannot resolve the spelling the model taught \
             the user"
        );
        assert_eq!(
            ProviderKind::from(entry.kind),
            kind,
            "`{id_suggestion}`'s kind does not survive the round trip through the wire \
             enum, so the flow would ask the wrong questions and compose the wrong request \
             path"
        );
        assert_eq!(
            entry.endpoint, endpoint,
            "`{id_suggestion}`'s endpoint — the absolute request URL, verbatim. This is a \
             third-party fact this repository verified once; a client that renders a \
             different one renders an unverified URL"
        );
        assert_eq!(
            entry.example_model, example_model,
            "`{id_suggestion}`'s example model — the default the model question offers \
             (BR-6)"
        );
        assert_eq!(
            entry.notes, notes,
            "`{id_suggestion}`'s note: the fact the command shape does not carry (Ollama \
             still asks for a key, DeepSeek's path has no `/v1`), which reaches the user \
             only if it is rendered"
        );
    }
}

/// **The roster served is what the build ships, not what is left unregistered**
/// (BR-4, BR-14).
///
/// The tempting shape for a "set up a provider" plan is to offer the vendors the
/// user has *not* got yet, and it is wrong in the case that matters most: a key
/// rotation, where the vendor to offer is precisely the one already registered
/// (BR-14 makes replacing it explicit rather than impossible). Filtering would
/// also make the plan's answer depend on the config, and the whole point of
/// ADR-4 is that this list is the shipped catalog — the same one the guide is
/// gated against — so that what the flow offers and what the model names cannot
/// come apart per machine.
///
/// Driven over a real seeded config through `from_env`, because the claim is
/// about a daemon that *has* providers; over `minimal()` it would hold
/// vacuously.
#[test]
fn the_plan_offers_the_whole_roster_over_a_configured_daemon() {
    let (runtime, dir) = daemon_over("roster", CONFIGURED);
    let plan = runtime.provider_setup_plan();

    // The fixture is doing its job: the daemon really did load the provider the
    // filter-shaped bug would have removed from the roster.
    assert_eq!(
        plan.existing
            .iter()
            .map(|p| p.id.0.as_str())
            .collect::<Vec<_>>(),
        ["deepseek"],
        "the seeded provider did not load, so nothing below is being asked of a \
         configured daemon"
    );

    let served: Vec<&str> = plan
        .catalog
        .iter()
        .map(|entry| entry.id_suggestion.as_str())
        .collect();
    let shipped: Vec<String> = recipe_catalog()
        .into_iter()
        .map(|recipe| recipe.id_suggestion)
        .collect();
    assert_eq!(
        served,
        shipped.iter().map(String::as_str).collect::<Vec<_>>(),
        "the plan's roster changed on a daemon that has a provider registered. The \
         catalog is the shipped list (ADR-4): an already-registered vendor stays on it, \
         because rotating that vendor's key is the same flow (BR-14), and filtering it \
         out would make the offer differ from what the bundled guide names."
    );

    std::fs::remove_dir_all(&dir).ok();
}
