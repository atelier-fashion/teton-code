#!/usr/bin/env python3
"""Regenerate `crates/teton-inference/data/models.toml` from the HuggingFace API.

The catalog is *derived data*, not hand-typed values. Every `sha256` and
`size_bytes` in the committed TOML comes from a real API response, so this
script is the only sanctioned way to edit those fields.

Provenance (REQ-547 architecture D-1)
-------------------------------------
`GET https://huggingface.co/api/models/<repo>/tree/<revision>` returns, per
file, `lfs.oid` — which *is* the SHA-256 of the artifact — and `lfs.size`.
Both are available without transferring a single byte of the multi-GB GGUF,
which is why authoring real digests is an API query rather than a download.

Revision pinning (REQ-547 BR-15)
--------------------------------
Every emitted URL pins a 40-hex commit SHA, never a moving ref like `main`.
A moving ref would silently invalidate the recorded `sha256` and turn the
download integrity check into spurious corruption failures.

Usage
-----
    python3 tools/refresh-catalog.py --check
        The BR-8/AC-8 catalog integrity gate. For every catalog entry it
        asserts, without downloading a single artifact byte:

          * the pinned `revision` is an immutable 40-hex commit SHA, never a
            moving ref like `main` (BR-15/AC-12) — checked before any network
            call, so a moving ref fails fast and locally;
          * the repository is public and ungated, from an *anonymous* request
            (a gated repo answers metadata but refuses the weights, so this is
            checked explicitly rather than inferred from a 200);
          * `sha256` equals HuggingFace's `lfs.oid` at that revision;
          * `size_bytes` equals HuggingFace's `lfs.size` at that revision;
          * the whole file is byte-identical to what this generator emits, so
            no hand-edit anywhere in it survives.

        This verifies that the *catalog is honest*. Byte-level verification of
        the artifact itself still happens at download time against this
        `sha256` (BR-6) — the two checks answer different questions.

    python3 tools/refresh-catalog.py --update
        Re-resolve each repository's `main` to its current commit SHA, re-derive
        every digest at that revision, and rewrite the TOML. Use this to adopt a
        newer upstream quantization. Review the diff — a changed `sha256` means
        the artifact changed.

    python3 tools/refresh-catalog.py --print
        Write the regenerated catalog to stdout instead of the file.

Exit codes
----------
A network failure and a digest mismatch are *categorically different events*
and never share an exit code, because an upstream outage must never be
mistakable for corruption (nor be silently ignored):

    0   VERIFIED    — every entry matches upstream at its pinned revision.
    1   MISMATCH    — a genuine integrity failure: a digest/size disagrees, the
                      revision is not pinned, the repo went gated/private, the
                      file or revision is gone, or the file was hand-edited.
    75  UNVERIFIED  — HuggingFace could not be reached (DNS/TCP/TLS/timeout, or
                      429/5xx after retries). Nothing is claimed about the
                      catalog either way. 75 is EX_TEMPFAIL: retry later.

Requires only the Python 3 standard library (no Rust dependency is added to the
workspace, so the tool cannot drag an HTTP client into `teton-inference`, which
is deliberately transport-free).
"""

from __future__ import annotations

import argparse
import difflib
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request

HF_API = "https://huggingface.co/api/models"
HF_RESOLVE = "https://huggingface.co"

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CATALOG_PATH = os.path.join(
    REPO_ROOT, "crates", "teton-inference", "data", "models.toml"
)

# Exit codes. See the module docstring: MISMATCH and UNVERIFIED must never be
# conflated, so they never share a code and never share a message vocabulary.
EXIT_VERIFIED = 0
EXIT_MISMATCH = 1
EXIT_UNVERIFIED = 75  # EX_TEMPFAIL

# HTTP statuses that mean "upstream is unwell", as opposed to "the catalog is
# wrong". Retried first; if they persist the result is UNVERIFIED, never a
# mismatch.
TRANSIENT_STATUSES = (408, 425, 429, 500, 502, 503, 504)

COMMIT_SHA = re.compile(r"\A[0-9a-f]{40}\Z")

# Mirrors `MOVING_REFS` in crates/teton-inference/src/catalog.rs, so a moving
# ref gets an error naming the specific hazard rather than "not 40 hex chars".
MOVING_REFS = ("main", "master", "head", "latest", "dev", "develop")


class Unverified(Exception):
    """Upstream could not be reached. Says nothing about catalog correctness."""


class Mismatch(Exception):
    """The catalog disagrees with upstream, or with its own invariants."""

# ---------------------------------------------------------------------------
# The picks. Everything here is human judgement; everything else is derived.
#
# `ram_floor_bytes` is a conservative floor that deliberately leaves headroom
# for the user's own work (BR-8/BR-9: never degrade the machine), not the raw
# weight size. It is carried over from REQ-544 and is not API-derived.
#
# The quantization is q4_k_m throughout — the documented REQ-544 assumption.
# Model picks stay provisional pending REQ-544 OQ-3's dogfooding benchmark;
# what this file makes real is the data *pipeline*, not the final picks.
# ---------------------------------------------------------------------------
PICKS = [
    {
        "name": "qwen2.5-coder-1.5b",
        "repo": "Qwen/Qwen2.5-Coder-1.5B-Instruct-GGUF",
        "file": "qwen2.5-coder-1.5b-instruct-q4_k_m.gguf",
        "band": "small",
        "ram_floor_bytes": 3 * 1024**3,
        "note": "Qwen's own GGUF release (official, public, ungated).",
    },
    {
        "name": "qwen2.5-coder-3b",
        "repo": "Qwen/Qwen2.5-Coder-3B-Instruct-GGUF",
        "file": "qwen2.5-coder-3b-instruct-q4_k_m.gguf",
        "band": "small",
        "ram_floor_bytes": 5 * 1024**3,
        "note": (
            "Qwen's own GGUF release (official, public, ungated). NOTE: the 3B "
            "weights carry the Qwen Research licence, unlike the Apache-2.0 "
            "1.5B/7B — revisit before shipping this entry as a default."
        ),
    },
    {
        "name": "qwen2.5-coder-7b",
        "repo": "Qwen/Qwen2.5-Coder-7B-Instruct-GGUF",
        "file": "qwen2.5-coder-7b-instruct-q4_k_m.gguf",
        "band": "mid",
        "ram_floor_bytes": 9 * 1024**3,
        "note": (
            "Qwen's own GGUF release (official, public, ungated). The repo also "
            "ships a two-part split of this quant; the single-file artifact is "
            "pinned because the downloader fetches one URL."
        ),
    },
    {
        "name": "qwen3-coder-30b-a3b",
        "repo": "unsloth/Qwen3-Coder-30B-A3B-Instruct-GGUF",
        "file": "Qwen3-Coder-30B-A3B-Instruct-Q4_K_M.gguf",
        "band": "large",
        "ram_floor_bytes": 20 * 1024**3,
        "note": (
            "NOT an official Qwen repo: Qwen publishes no GGUF for "
            "Qwen3-Coder-30B-A3B-Instruct (huggingface.co/Qwen/"
            "Qwen3-Coder-30B-A3B-Instruct-GGUF does not exist). unsloth is the "
            "most-used third-party quantizer for this model and its Q4_K_M is a "
            "single file. The pinned revision is what makes trusting a "
            "third-party quantizer tractable: the bytes cannot change under us."
        ),
    },
]

HEADER = """\
# Teton Code local-model catalog.
#
# GENERATED FILE — do not hand-edit. Regenerate with:
#
#     python3 tools/refresh-catalog.py --update   # adopt upstream's current main
#     python3 tools/refresh-catalog.py --check    # prove this file is derived
#
# Versioned *data*, not code: the daemon can replace this file with a newer
# catalog (bumping `version`) without a teton-inference release. Each entry maps
# a model name to its GGUF download URL, expected SHA-256, download size, and the
# minimum system RAM required to load it, plus the OQ-3 hardware band it serves:
#
#   small -> 1.5B-3B  (8-16 GiB machines)
#   mid   -> 7B       (16-32 GiB machines)
#   large -> 30B-A3B  (32 GiB+ machines, optional)
#
# `sha256` and `size_bytes` are not hand-typed. They are HuggingFace's `lfs.oid`
# (which *is* the artifact's SHA-256) and `lfs.size`, read from
# `GET /api/models/<repo>/tree/<revision>` — no multi-GB download is needed to
# author or to re-verify them (REQ-547 architecture D-1).
#
# Every `url` pins an immutable 40-hex commit SHA, mirrored in `revision`, never
# a moving ref like `main` (BR-15): a moving ref would silently invalidate the
# recorded `sha256` and turn the download integrity check into spurious
# corruption failures. `Catalog::validate` rejects any entry that drifts from
# this rule.
#
# Model picks remain provisional pending REQ-544 OQ-3's dogfooding benchmark.
"""


def http_get_json(url: str, attempts: int = 4):
    """GET `url` and parse JSON.

    Anonymous by design: sending no credential is what proves the repository is
    public and ungated (D-1 / BR-15). Transient statuses back off and retry;
    what survives is classified — `Unverified` for "upstream is unreachable or
    unwell", `Mismatch` for "upstream answered, and the answer says the catalog
    is wrong". A caller must never have to guess which happened.
    """
    delay = 1.0
    last = None
    for attempt in range(1, attempts + 1):
        try:
            request = urllib.request.Request(
                url, headers={"User-Agent": "teton-code-refresh-catalog/1"}
            )
            with urllib.request.urlopen(request, timeout=30) as response:
                return json.load(response)
        except urllib.error.HTTPError as err:
            last = err
            if err.code in TRANSIENT_STATUSES and attempt < attempts:
                time.sleep(delay)
                delay *= 2
                continue
            if err.code in TRANSIENT_STATUSES:
                raise Unverified(
                    f"{url} kept returning HTTP {err.code} after {attempts} "
                    f"attempts. HuggingFace is rate-limiting or unwell."
                ) from err
            if err.code in (401, 403):
                raise Mismatch(
                    f"{url} returned HTTP {err.code} to an anonymous request. "
                    f"The repository is private or gated, and the catalog may "
                    f"only name public, ungated repositories — a user with no "
                    f"HuggingFace account must be able to fetch every entry. "
                    f"Drop the entry or pick a public repo."
                ) from err
            if err.code in (404, 410):
                raise Mismatch(
                    f"{url} returned HTTP {err.code}. The repository was "
                    f"renamed or deleted, or the pinned revision was garbage "
                    f"collected upstream. The catalog now points at nothing; "
                    f"re-pin with --update after checking what happened."
                ) from err
            raise Mismatch(f"GET {url} failed with HTTP {err.code}") from err
        except (urllib.error.URLError, TimeoutError, json.JSONDecodeError) as err:
            last = err
            if attempt < attempts:
                time.sleep(delay)
                delay *= 2
                continue
    raise Unverified(
        f"could not reach {url} after {attempts} attempts ({last}). This is a "
        f"transport failure, NOT evidence about the catalog's contents."
    )


def assert_public_and_ungated(repo: str) -> dict:
    """Assert `repo` is anonymously fetchable, and return its metadata.

    A *gated* repository still serves its metadata to anonymous callers and
    only refuses the weights, so a 200 here is not on its own proof that the
    artifact is fetchable. The `gated`/`private`/`disabled` flags are therefore
    checked explicitly rather than inferred from the status code.
    """
    info = http_get_json(f"{HF_API}/{repo}")
    blocked = [
        flag for flag in ("gated", "private", "disabled") if info.get(flag)
    ]
    if blocked:
        raise Mismatch(
            f"{repo} is {'/'.join(blocked)} ({', '.join(f'{f}={info.get(f)!r}' for f in blocked)}). "
            f"The catalog may only name public, ungated repositories: a user "
            f"with no HuggingFace account and no token must be able to fetch "
            f"every entry. Drop the entry or pick a public repo."
        )
    return info


def resolve_main(repo: str) -> str:
    """The current commit SHA of `repo`'s default branch."""
    info = assert_public_and_ungated(repo)
    sha = info.get("sha")
    if not isinstance(sha, str) or not COMMIT_SHA.match(sha):
        raise Mismatch(f"{repo} reported a non-commit revision {sha!r}")
    return sha


def assert_pinned(name: str, revision) -> str:
    """Assert `revision` is an immutable 40-hex commit SHA (BR-15/AC-12).

    Checked before any network call: a moving ref is a local, structural fault
    and must not need HuggingFace to be reachable to be reported.
    """
    if not isinstance(revision, str):
        raise Mismatch(f"catalog entry {name!r} has no revision at all")
    if revision.lower() in MOVING_REFS or revision.startswith("refs/"):
        raise Mismatch(
            f"catalog entry {name!r} pins the moving ref {revision!r} instead "
            f"of a commit SHA. A moving ref lets the artifact change while "
            f"`sha256` stays put, which surfaces as spurious corruption "
            f"failures on download (BR-15). Resolve it to an immutable commit "
            f"SHA with `python3 tools/refresh-catalog.py --update`."
        )
    if not COMMIT_SHA.match(revision):
        raise Mismatch(
            f"catalog entry {name!r} has revision {revision!r}, which is not a "
            f"commit SHA: a revision must be exactly 40 lowercase hex "
            f"characters (BR-15). Regenerate with "
            f"`python3 tools/refresh-catalog.py --update`."
        )
    return revision


def lfs_metadata(repo: str, revision: str, path: str):
    """`(sha256, size_bytes)` for `path` in `repo` at `revision`, from LFS metadata."""
    tree = http_get_json(f"{HF_API}/{repo}/tree/{revision}?recursive=true")
    for entry in tree:
        if entry.get("path") != path:
            continue
        lfs = entry.get("lfs")
        if not lfs or "oid" not in lfs or "size" not in lfs:
            raise Mismatch(
                f"{repo}@{revision}:{path} carries no LFS metadata, so its "
                f"SHA-256 cannot be read without downloading it. Refusing to "
                f"guess. (An upstream repo that stopped using LFS for this "
                f"file needs a new pin, not a hand-typed digest.)"
            )
        return lfs["oid"], int(lfs["size"])
    raise Mismatch(
        f"{repo}@{revision} has no file {path!r}. The file was renamed or "
        f"removed upstream at the pinned revision. Pick a file that actually "
        f"exists; do not invent a URL."
    )


def grouped(value: int) -> str:
    """`1117320768` -> `1_117_320_768` (TOML allows underscore separators)."""
    return f"{value:,}".replace(",", "_")


def wrap_comment(text: str, width: int = 79) -> list:
    """Wrap `text` into `# `-prefixed lines without importing textwrap's defaults."""
    lines = []
    current = "#"
    for word in text.split():
        candidate = f"{current} {word}"
        if len(candidate) > width and current != "#":
            lines.append(current)
            current = f"# {word}"
        else:
            current = candidate
    if current != "#":
        lines.append(current)
    return lines


def render(rows: list) -> str:
    """Render the full TOML document from derived rows."""
    out = [HEADER.rstrip("\n"), "", "version = 1", ""]
    for row in rows:
        out.extend(wrap_comment(row["note"]))
        out.append("[[models]]")
        out.append(f'name = "{row["name"]}"')
        out.append(f'url = "{row["url"]}"')
        out.append(f'revision = "{row["revision"]}"')
        out.append(f'sha256 = "{row["sha256"]}"')
        out.append(f"size_bytes = {grouped(row['size_bytes'])}")
        out.append(f"ram_floor_bytes = {grouped(row['ram_floor_bytes'])}")
        out.append(f'band = "{row["band"]}"')
        out.append("")
    return "\n".join(out).rstrip("\n") + "\n"


def committed_entries(text: str) -> dict:
    """Map catalog `name` -> its committed fields, by reading the TOML text.

    Deliberately a line reader rather than a TOML parser: the standard library
    has no TOML reader before 3.11, and the check must run on whatever Python a
    CI image happens to ship. It reads only the scalar fields it compares.
    """
    entries = {}
    current = None
    for line in text.splitlines():
        if line.strip() == "[[models]]":
            current = {}
            continue
        if current is None:
            continue
        match = re.match(r'^(\w+) = "([^"]*)"$', line)
        if match:
            current[match.group(1)] = match.group(2)
        else:
            match = re.match(r"^(\w+) = ([0-9_]+)$", line)
            if match:
                current[match.group(1)] = int(match.group(2).replace("_", ""))
        # `current` is stored by reference, so later fields of the same block
        # land in the entry already registered under its name.
        if "name" in current:
            entries.setdefault(current["name"], current)
    return entries


def derive_rows(update: bool, existing: str) -> list:
    """Derive every catalog row from the API, at `main` or at the pins.

    In pinned mode each revision is asserted immutable *before* it is used in a
    URL, so a moving ref is rejected locally rather than silently re-derived
    into a catalog that would drift the next time upstream moves.
    """
    pins = (
        {}
        if update
        else {name: e.get("revision") for name, e in committed_entries(existing).items()}
    )
    rows = []
    for pick in PICKS:
        if update:
            revision = resolve_main(pick["repo"])
        else:
            if pick["name"] not in pins:
                raise Mismatch(
                    f"{CATALOG_PATH} pins no revision for {pick['name']!r}; "
                    f"run --update to author it."
                )
            revision = assert_pinned(pick["name"], pins[pick["name"]])
        assert_public_and_ungated(pick["repo"])
        sha256, size_bytes = lfs_metadata(pick["repo"], revision, pick["file"])
        rows.append(
            {
                "name": pick["name"],
                "url": f"{HF_RESOLVE}/{pick['repo']}/resolve/{revision}/{pick['file']}",
                "revision": revision,
                "sha256": sha256,
                "size_bytes": size_bytes,
                "ram_floor_bytes": pick["ram_floor_bytes"],
                "band": pick["band"],
                "note": pick["note"],
            }
        )
    return rows


def derive(update: bool, existing: str) -> str:
    """Derive the catalog, either at upstream's current main or at the pins."""
    return render(derive_rows(update=update, existing=existing))


def field_mismatches(existing: str, rows: list) -> list:
    """Per-field disagreements between the committed catalog and upstream.

    Reported field by field, naming both values and their provenance, so a
    failure is diagnosable from the log alone — "which byte of which entry is
    wrong, and what does HuggingFace actually say" — rather than requiring the
    reader to interpret a whole-file diff.
    """
    committed = committed_entries(existing)
    problems = []
    for row in rows:
        entry = committed.get(row["name"])
        if entry is None:
            problems.append(
                f"MISMATCH  {row['name']}\n"
                f"    the generator emits this entry but the committed catalog "
                f"has no entry by that name."
            )
            continue
        for field, provenance in (
            ("sha256", "lfs.oid"),
            ("size_bytes", "lfs.size"),
            ("revision", "the pinned revision"),
            ("url", "the derived URL"),
        ):
            if entry.get(field) != row[field]:
                problems.append(
                    f"MISMATCH  {row['name']}  {field}\n"
                    f"    catalog     : {entry.get(field)!r}\n"
                    f"    HuggingFace : {row[field]!r}\n"
                    f"    ({provenance} for {row['name']} at revision "
                    f"{row['revision']})"
                )
    return problems


def check(existing: str) -> int:
    """Run the integrity gate. Returns an exit code; never raises `Mismatch`."""
    rows = derive_rows(update=False, existing=existing)
    problems = field_mismatches(existing, rows)
    generated = render(rows)

    if problems:
        sys.stderr.write("\n\n".join(problems) + "\n\n")
        sys.stderr.write(
            f"error: CATALOG MISMATCH — {len(problems)} field(s) disagree with "
            f"HuggingFace's LFS metadata at the pinned revision(s). This is a "
            f"genuine integrity failure, NOT a network problem. Either the "
            f"catalog was hand-edited (revert it, or re-derive with --update), "
            f"or an artifact changed under a pinned revision (investigate "
            f"before adopting).\n"
        )
        return EXIT_MISMATCH

    if generated != existing:
        sys.stderr.writelines(
            difflib.unified_diff(
                existing.splitlines(keepends=True),
                generated.splitlines(keepends=True),
                fromfile="committed",
                tofile="derived",
            )
        )
        sys.stderr.write(
            "\nerror: CATALOG MISMATCH — every digest matches upstream, but the "
            "committed file is not byte-identical to what the generator emits "
            "(see the diff above: a comment, an ordering, or a non-derived "
            "field was hand-edited). Regenerate with --update rather than "
            "editing the file by hand.\n"
        )
        return EXIT_MISMATCH

    for row in rows:
        print(
            f"verified  {row['name']:<22} "
            f"sha256={row['sha256'][:12]}… size={row['size_bytes']:,} "
            f"@{row['revision'][:12]}…"
        )
    print(
        f"\nok: VERIFIED — {len(rows)} catalog entries match HuggingFace's LFS "
        f"metadata at their pinned revisions, and {CATALOG_PATH} is "
        f"byte-identical to the derived catalog.\n"
        f"note: this proves the *catalog* is honest. The artifact's own bytes "
        f"are verified against these digests at download time (BR-6)."
    )
    return EXIT_VERIFIED


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument(
        "--check",
        action="store_true",
        help=(
            "verify the catalog against HuggingFace at its pinned revisions "
            "(exit 0 verified, 1 mismatch, 75 unverified/unreachable)"
        ),
    )
    mode.add_argument(
        "--update",
        action="store_true",
        help="re-resolve each repo's main and rewrite the catalog",
    )
    mode.add_argument(
        "--print", dest="to_stdout", action="store_true", help="write to stdout"
    )
    args = parser.parse_args()

    try:
        with open(CATALOG_PATH, encoding="utf-8") as handle:
            existing = handle.read()
    except FileNotFoundError:
        existing = ""

    try:
        if args.check:
            return check(existing)

        generated = derive(update=args.update, existing=existing)
    except Unverified as err:
        # The one outcome that says nothing about the catalog. It gets its own
        # exit code and its own vocabulary so no reader — human or CI — can
        # mistake an outage for corruption, and so it cannot pass unnoticed.
        sys.stderr.write(
            f"UNVERIFIED: {err}\n"
            f"\nThe catalog was NOT verified. This is NOT a digest mismatch "
            f"and NOT evidence of corruption — nothing was learned about the "
            f"catalog's contents either way. Re-run when HuggingFace is "
            f"reachable (exit {EXIT_UNVERIFIED} = EX_TEMPFAIL).\n"
        )
        return EXIT_UNVERIFIED
    except Mismatch as err:
        sys.stderr.write(f"error: CATALOG MISMATCH — {err}\n")
        return EXIT_MISMATCH

    if args.to_stdout:
        sys.stdout.write(generated)
        return EXIT_VERIFIED

    with open(CATALOG_PATH, "w", encoding="utf-8") as handle:
        handle.write(generated)
    print(f"wrote {CATALOG_PATH}")
    return EXIT_VERIFIED


if __name__ == "__main__":
    sys.exit(main())
