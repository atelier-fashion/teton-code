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
        Re-derive the catalog *at the revisions already pinned in the committed
        TOML* and assert the result is byte-identical. This is the proof that
        the committed file is generated rather than hand-edited, and it doubles
        as an upstream-tamper check: a rewritten artifact at a pinned revision
        fails here. Exits non-zero on any difference.

    python3 tools/refresh-catalog.py --update
        Re-resolve each repository's `main` to its current commit SHA, re-derive
        every digest at that revision, and rewrite the TOML. Use this to adopt a
        newer upstream quantization. Review the diff — a changed `sha256` means
        the artifact changed.

    python3 tools/refresh-catalog.py --print
        Write the regenerated catalog to stdout instead of the file.

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
    """GET `url` and parse JSON, backing off on 429/503 (BR-16's rate limits)."""
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
            if err.code in (429, 503) and attempt < attempts:
                time.sleep(delay)
                delay *= 2
                continue
            if err.code in (401, 403, 404):
                raise SystemExit(
                    f"error: {url} returned HTTP {err.code}. The repository is "
                    f"missing, private, or gated — the catalog may only name "
                    f"public, ungated repositories. Not inventing a value."
                )
            raise SystemExit(f"error: GET {url} failed with HTTP {err.code}")
        except urllib.error.URLError as err:
            last = err
            if attempt < attempts:
                time.sleep(delay)
                delay *= 2
                continue
    raise SystemExit(
        f"error: could not reach {url} ({last}). Refusing to emit a catalog "
        f"with unverified digests."
    )


def resolve_main(repo: str) -> str:
    """The current commit SHA of `repo`'s default branch."""
    info = http_get_json(f"{HF_API}/{repo}")
    sha = info.get("sha")
    if not isinstance(sha, str) or not re.fullmatch(r"[0-9a-f]{40}", sha):
        raise SystemExit(f"error: {repo} reported a non-commit revision {sha!r}")
    if info.get("gated") or info.get("private") or info.get("disabled"):
        raise SystemExit(
            f"error: {repo} is gated/private/disabled; the catalog may only "
            f"name public, ungated repositories."
        )
    return sha


def lfs_metadata(repo: str, revision: str, path: str):
    """`(sha256, size_bytes)` for `path` in `repo` at `revision`, from LFS metadata."""
    tree = http_get_json(f"{HF_API}/{repo}/tree/{revision}?recursive=true")
    for entry in tree:
        if entry.get("path") != path:
            continue
        lfs = entry.get("lfs")
        if not lfs or "oid" not in lfs or "size" not in lfs:
            raise SystemExit(
                f"error: {repo}@{revision}:{path} carries no LFS metadata, so "
                f"its SHA-256 cannot be read without downloading it. Refusing "
                f"to guess."
            )
        return lfs["oid"], int(lfs["size"])
    raise SystemExit(
        f"error: {repo}@{revision} has no file {path!r}. Pick a file that "
        f"actually exists; do not invent a URL."
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


def pinned_revisions(text: str) -> dict:
    """Map catalog `name` -> pinned `revision` by reading the committed TOML."""
    revisions = {}
    name = None
    for line in text.splitlines():
        match = re.match(r'^name = "([^"]+)"$', line)
        if match:
            name = match.group(1)
            continue
        match = re.match(r'^revision = "([^"]+)"$', line)
        if match and name is not None:
            revisions[name] = match.group(1)
            name = None
    return revisions


def derive(update: bool, existing: str) -> str:
    """Derive the catalog, either at upstream's current main or at the pins."""
    pins = {} if update else pinned_revisions(existing)
    rows = []
    for pick in PICKS:
        if update:
            revision = resolve_main(pick["repo"])
        else:
            revision = pins.get(pick["name"])
            if revision is None:
                raise SystemExit(
                    f"error: {CATALOG_PATH} pins no revision for "
                    f"{pick['name']!r}; run --update to author it."
                )
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
    return render(rows)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument(
        "--check",
        action="store_true",
        help="re-derive at the pinned revisions and require a byte-identical file",
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

    generated = derive(update=args.update, existing=existing)

    if args.to_stdout:
        sys.stdout.write(generated)
        return 0

    if args.check:
        if generated == existing:
            print(f"ok: {CATALOG_PATH} is byte-identical to the derived catalog")
            return 0
        sys.stderr.writelines(
            difflib.unified_diff(
                existing.splitlines(keepends=True),
                generated.splitlines(keepends=True),
                fromfile="committed",
                tofile="derived",
            )
        )
        sys.stderr.write(
            "\nerror: the committed catalog is not what the HuggingFace API "
            "yields at its pinned revisions. Either it was hand-edited (run "
            "--update, or revert), or an upstream artifact changed under a "
            "pinned revision (investigate before adopting).\n"
        )
        return 1

    with open(CATALOG_PATH, "w", encoding="utf-8") as handle:
        handle.write(generated)
    print(f"wrote {CATALOG_PATH}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
