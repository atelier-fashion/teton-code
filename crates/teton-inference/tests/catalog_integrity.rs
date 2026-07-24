//! BR-8/AC-8 — the shipped catalog must be honest.
//!
//! This repository once shipped a catalog of placeholder URLs and invented
//! digests. These are the mechanised checks that make repeating that a build
//! failure rather than a discovery. There are two halves, split by whether the
//! check needs the network, and the split is deliberate:
//!
//! | Half | Where | Runs | Catches |
//! |---|---|---|---|
//! | **structural** | this file | every `cargo test`, hermetic | unpinned/moving revisions, malformed or copy-pasted digests, placeholder URLs, self-contradicting entries |
//! | **upstream** | `tools/refresh-catalog.py --check`, a dedicated CI job | every CI run, needs HuggingFace | `sha256` ≠ `lfs.oid`, `size_bytes` ≠ `lfs.size`, a repo that went gated/private, a revision or file that vanished |
//!
//! The structural half is here rather than in the upstream half because it must
//! be *deterministic and offline*: it has to keep biting on a plane, and it must
//! never be the thing that goes yellow when HuggingFace has a bad afternoon. The
//! upstream half is not a Rust test because `teton-inference` is deliberately
//! transport-free — it has no HTTP client and gains nothing by acquiring one to
//! re-implement a generator that already exists (`tools/refresh-catalog.py`,
//! REQ-547 architecture D-1). Wiring that tool into CI beats duplicating it.
//!
//! Neither half verifies the *artifact*. Both verify that the catalog tells the
//! truth about the artifact. The bytes themselves are hashed and compared
//! against this same `sha256` at download time, by the downloader (BR-6).

use std::path::{Path, PathBuf};

use teton_inference::catalog::{Catalog, CatalogError};

/// The committed catalog *as text*, so tests can perturb the real file's bytes
/// rather than a synthetic entry that might drift from it.
const CATALOG_TOML: &str = include_str!("../data/models.toml");

/// The SHA-256 of an empty file — the digest most likely to be produced by an
/// integrity check that silently hashed nothing.
const EMPTY_FILE_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Substrings that only ever appear in a URL nobody has actually fetched.
const PLACEHOLDER_MARKERS: &[&str] = &[
    "example.com",
    "example.org",
    "placeholder",
    "todo",
    "changeme",
    "localhost",
    "127.0.0.1",
    "your-",
    "xxx",
];

/// The repository root, so tests can assert on files outside this crate.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("the crate manifest dir resolves to a repository checkout")
}

fn read_repo_file(relative: &str) -> String {
    let path = repo_root().join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "{} must exist and be readable — it is part of the BR-8 catalog \
             integrity gate: {err}",
            path.display()
        )
    })
}

// ---- structural half ------------------------------------------------------

#[test]
fn the_bundled_catalog_satisfies_every_invariant_the_downloader_depends_on() {
    Catalog::bundled()
        .validate()
        .expect("the bundled catalog must be valid; regenerate it with `tools/refresh-catalog.py`");
}

#[test]
fn every_entry_pins_an_immutable_revision_on_the_expected_host() {
    for model in Catalog::bundled().models {
        let name = &model.name;
        let source = model
            .source()
            .unwrap_or_else(|| panic!("{name}: `{}` is not a resolve URL", model.url));

        assert!(
            model.url.starts_with("https://huggingface.co/"),
            "{name}: models are hosted on HuggingFace over TLS (ADR-004), got `{}`",
            model.url
        );
        assert_eq!(
            source.revision, model.revision,
            "{name}: the URL and the `revision` field must pin the same commit"
        );
        assert_eq!(
            model.revision.len(),
            40,
            "{name}: `{}` is not a 40-hex commit SHA (BR-15)",
            model.revision
        );
        assert!(
            model
                .revision
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "{name}: revision `{}` is not lowercase hex (BR-15)",
            model.revision
        );
        assert!(
            source.file.ends_with(".gguf"),
            "{name}: the pinned file `{}` is not a GGUF artifact",
            source.file
        );
        assert!(
            !model.url.contains('?') && !model.url.contains('#'),
            "{name}: a query or fragment in `{}` is not part of the pinned \
             artifact's identity and would not survive the base-URL override",
            model.url
        );
        // `https://user:token@host/...` would put a credential in a file that
        // ships to every user. The model fetch is credential-free by design
        // (REQ-547 architecture D-2).
        let authority = model.url.trim_start_matches("https://");
        let authority = authority.split('/').next().unwrap_or_default();
        assert!(
            !authority.contains('@'),
            "{name}: the URL carries userinfo credentials in its authority"
        );
    }
}

#[test]
fn no_entry_carries_placeholder_or_copy_pasted_data() {
    let models = Catalog::bundled().models;
    assert!(!models.is_empty(), "an empty catalog verifies nothing");

    for model in &models {
        let name = &model.name;

        // A digest nobody derived tends to look like one: all one character, a
        // recognisable filler pattern, or the hash of nothing at all.
        let first = model.sha256.chars().next().expect("non-empty digest");
        assert!(
            !model.sha256.chars().all(|c| c == first),
            "{name}: sha256 `{}` is a single repeated character — that is not a \
             digest anyone computed",
            model.sha256
        );
        assert_ne!(
            model.sha256, EMPTY_FILE_SHA256,
            "{name}: sha256 is the digest of an empty file"
        );
        for filler in ["deadbeef", "0123456789abcdef", "abcdef0123456789"] {
            assert!(
                !model.sha256.contains(filler),
                "{name}: sha256 `{}` contains the filler pattern `{filler}`",
                model.sha256
            );
        }

        let lowered = model.url.to_ascii_lowercase();
        for marker in PLACEHOLDER_MARKERS {
            assert!(
                !lowered.contains(marker),
                "{name}: URL `{}` contains the placeholder marker `{marker}`",
                model.url
            );
        }

        // Every shipped quant is >100 MiB, and none of them is a round number.
        // A suspiciously round size is the signature of a hand-typed guess.
        assert!(
            model.size_bytes > 100 * 1024 * 1024,
            "{name}: size_bytes {} is too small to be a GGUF quantization",
            model.size_bytes
        );
        assert!(
            model.size_bytes % 100_000_000 != 0,
            "{name}: size_bytes {} is an exact multiple of 100 MB, which is what \
             an invented number looks like. If HuggingFace really reports this, \
             re-derive with `tools/refresh-catalog.py --update` and relax this \
             assertion with the API response in the commit message.",
            model.size_bytes
        );

        // A model needs room for its own weights plus working memory; a floor
        // below the artifact size would promise a load that cannot happen.
        assert!(
            model.ram_floor_bytes > model.size_bytes,
            "{name}: ram_floor_bytes {} does not exceed the {}-byte artifact",
            model.ram_floor_bytes,
            model.size_bytes
        );
    }

    // Two entries sharing a digest, URL or size means one was copied over the
    // other and never re-derived.
    for (index, model) in models.iter().enumerate() {
        for other in &models[index + 1..] {
            assert_ne!(
                model.sha256, other.sha256,
                "`{}` and `{}` share a sha256",
                model.name, other.name
            );
            assert_ne!(
                model.url, other.url,
                "`{}` and `{}` share a URL",
                model.name, other.name
            );
            assert_ne!(
                model.size_bytes, other.size_bytes,
                "`{}` and `{}` share a size_bytes",
                model.name, other.name
            );
        }
    }
}

#[test]
fn swapping_a_pin_for_a_moving_ref_fails_with_an_actionable_message() {
    // AC-12. Perturb the *committed file's own bytes* so this bites on the real
    // catalog's shape, not on a synthetic entry that could drift away from it.
    let pinned = Catalog::bundled().models[0].revision.clone();
    for moving in ["main", "master", "refs/heads/main"] {
        let mutated = CATALOG_TOML.replace(&pinned, moving);
        assert_ne!(mutated, CATALOG_TOML, "the mutation must change the file");

        let err = Catalog::from_toml(&mutated)
            .expect("still valid TOML")
            .validate()
            .expect_err("a moving ref must be rejected");

        assert!(
            matches!(err, CatalogError::MovingRevision { ref revision, .. } if revision == moving),
            "`{moving}` produced {err:?} instead of MovingRevision"
        );
        let message = err.to_string();
        assert!(message.contains("moving ref"), "{message}");
        assert!(message.contains("BR-15"), "{message}");
        assert!(
            message.contains("refresh-catalog.py"),
            "the message must name the remedy: {message}"
        );
    }
}

#[test]
fn the_catalog_advertises_itself_as_generated() {
    // The provenance note is load-bearing: it is what tells the next editor to
    // re-derive rather than hand-type, which is how the placeholder data got in.
    assert!(
        CATALOG_TOML.contains("GENERATED FILE"),
        "the catalog must announce that it is generated"
    );
    assert!(
        CATALOG_TOML.contains("refresh-catalog.py"),
        "the catalog must name the generator that produces it"
    );
}

// ---- the boundary between the two halves ----------------------------------

#[test]
fn a_one_character_digest_corruption_survives_structural_validation() {
    // This is the whole argument for the CI job. A digest that is *shaped* like
    // a digest but is not the artifact's digest is invisible offline: no local
    // check can distinguish it without asking HuggingFace what the artifact
    // actually hashes to. If this test ever starts failing, the structural half
    // grew a way to catch it and this file's docs need revisiting.
    let honest = Catalog::bundled().models[0].sha256.clone();
    let last = honest.chars().last().expect("non-empty digest");
    let flipped = if last == '0' { '1' } else { '0' };
    let corrupted = format!("{}{flipped}", &honest[..honest.len() - 1]);
    assert_ne!(corrupted, honest);

    let mutated = CATALOG_TOML.replace(&honest, &corrupted);
    Catalog::from_toml(&mutated)
        .expect("still valid TOML")
        .validate()
        .expect("a corrupted-but-well-formed digest passes structural validation");
}

#[test]
fn the_upstream_half_of_the_gate_exists_and_is_wired_into_ci() {
    // Deleting the CI job, or the tool it runs, would silently remove the only
    // check that compares the catalog against reality — the exact failure this
    // REQ exists to prevent. Make that removal turn a test red.
    let tool = read_repo_file("tools/refresh-catalog.py");
    assert!(
        tool.contains("--check"),
        "tools/refresh-catalog.py must expose the --check verification mode"
    );
    assert!(
        tool.contains("lfs") && tool.contains("oid"),
        "the verification must read HuggingFace's LFS metadata (D-1)"
    );

    let ci = read_repo_file(".github/workflows/ci.yml");
    assert!(
        ci.contains("refresh-catalog.py"),
        "CI must run the catalog integrity check on every run (BR-8/AC-8). If \
         this moved, update this assertion — do not delete it."
    );
    assert!(
        ci.contains("--check"),
        "CI must invoke the tool in --check mode, not --update"
    );
    // The outage/mismatch distinction is the part most easily lost in a
    // refactor of the workflow, so it is asserted rather than assumed.
    assert!(
        ci.contains("75"),
        "the CI job must handle exit code 75 (UNVERIFIED) distinctly from a \
         mismatch: an outage must never be reported as corruption"
    );
}
