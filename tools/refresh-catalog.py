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

    python3 tools/refresh-catalog.py --update <name>
        Re-resolve exactly the named entry's repository `main` to its current
        commit SHA, re-derive its digest at that revision, and rewrite the TOML;
        every *other* entry stays at its committed pin. Per-entry on purpose
        (M-13): adopting a publisher's new commit — above all the third-party
        quantizer entry, whose bytes a new commit could change wholesale — must be
        a deliberate act, not a side effect of refreshing the Qwen entries. Review
        the diff — a changed `sha256` means the artifact changed.

    python3 tools/refresh-catalog.py --print
        Write the regenerated catalog to stdout instead of the file.

    --catalog PATH
        Read (and, for --update, write) that file instead of the committed
        catalog. Exists so the gate itself is testable: REQ-547 TASK-008 points
        it at a fixture pinning a moving ref and asserts the MISMATCH is raised
        with an actionable message (AC-12). A gate nobody has ever seen fail is
        not a gate.

Environment
-----------
    HF_ENDPOINT
        Base URL of the *metadata API*, default `https://huggingface.co`. The
        emitted download URLs stay canonical regardless: a check run against a
        mirror must still verify the catalog that ships, not rewrite it to point
        at the mirror. Set by the acceptance suite to a local mock so the gate's
        own behaviour — pass on agreement, MISMATCH on drift — is exercised
        hermetically, with the real network run remaining the CI job's job.

    TETON_CATALOG_RETRY_BASE_MS
        The first retry delay in milliseconds (default 1000). Timing only; the
        acceptance suite shortens it so the retry ladder for an unreachable host
        can be walked without spending its seconds.

Exit codes
----------
A network failure and a digest mismatch are *categorically different events*
and never share an exit code, because an upstream outage must never be
mistakable for corruption (nor be silently ignored):

    0   VERIFIED    — every entry matches upstream at its pinned revision.
    1   MISMATCH    — a genuine integrity failure: a digest/size disagrees, the
                      revision is not pinned, the repo went gated/private, the
                      file or revision is gone, or the file was hand-edited.
    75  UNVERIFIED  — HuggingFace could not be reached (DNS/TCP/TLS/timeout, a
                      reset or half-closed connection, a truncated body, or
                      429/5xx after retries), or this tool crashed before it
                      could reach a verdict. Nothing is claimed about the
                      catalog either way. 75 is EX_TEMPFAIL: retry later.

Note that 1 is also Python's exit code for an uncaught exception, so "anything
unexpected" must be routed to 75 deliberately (see the `__main__` guard) — the
default would report a bug in this tool, or an outage, as corruption.

Requires only the Python 3 standard library (no Rust dependency is added to the
workspace, so the tool cannot drag an HTTP client into `teton-inference`, which
is deliberately transport-free).
"""

from __future__ import annotations

import argparse
import difflib
import http.client
import json
import os
import re
import sys
import time
import traceback
import urllib.error
import urllib.parse
import urllib.request


def _validated_api_base(endpoint: str) -> str:
    """The `/api/models` base for `endpoint`, refusing a non-web scheme (M-9).

    `urlopen` will happily open `file://…`, which would let a hostile `HF_ENDPOINT`
    turn the generator into a local-file reader. So the scheme is constrained to
    `https`, with `http` allowed *only* on a loopback host — the hermetic tests
    point the tool at a `127.0.0.1` mock, and nothing else has a reason to speak
    plaintext to a metadata API.
    """
    endpoint = endpoint.rstrip("/")
    parsed = urllib.parse.urlparse(endpoint)
    host = parsed.hostname or ""
    loopback = host in ("localhost", "127.0.0.1", "::1")
    if parsed.scheme == "https" or (parsed.scheme == "http" and loopback):
        return f"{endpoint}/api/models"
    raise SystemExit(
        f"refusing HF_ENDPOINT {endpoint!r}: the metadata API must be https "
        f"(http is allowed only on a loopback host, for the hermetic tests). "
        f"A non-web scheme like file:// is never accepted."
    )


# The metadata API is redirectable (HF_ENDPOINT, mirroring the daemon's
# `[local_model] base_url`, BR-16); the *resolve* host is not. What the catalog
# records is the canonical download URL, so deriving it from a mirror would make
# `--check` rewrite the very field it is meant to verify.
HF_API = _validated_api_base(os.environ.get("HF_ENDPOINT", "https://huggingface.co"))
HF_RESOLVE = "https://huggingface.co"

# LFS oid == the artifact's SHA-256. Validated before it can enter the rendered
# TOML, so a hostile or MITM'd API response cannot inject catalog syntax through
# it (M-9).
SHA256_RE = re.compile(r"\A[0-9a-f]{64}\Z")

# The developer-authored fields interpolated into a URL or a filename. Restricted
# to a safe charset so an ill-advised edit to `PICKS` cannot smuggle TOML syntax,
# a path separator, or URL structure into the generated catalog (M-9).
SAFE_NAME_RE = re.compile(r"\A[A-Za-z0-9._-]+\Z")
SAFE_FILE_RE = re.compile(r"\A[A-Za-z0-9._-]+\Z")
SAFE_REPO_RE = re.compile(r"\A[A-Za-z0-9._-]+/[A-Za-z0-9._-]+\Z")

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

# Everything that means "the conversation with upstream broke", as opposed to
# "upstream answered and the answer indicts the catalog". These are retried and,
# if they persist, reported as UNVERIFIED — never as a MISMATCH.
#
# The membership here is load-bearing, and wider than it first looks:
#
#   * `OSError` covers `urllib.error.URLError` (DNS/TCP/TLS refusals),
#     `TimeoutError`, and — the one that used to escape — `ConnectionResetError`
#     and `BrokenPipeError`. A peer that resets mid-response raises the bare
#     `OSError` out of the socket layer; `urlopen` only wraps what happens while
#     it is *sending*, so a reset during `getresponse()` arrives unwrapped.
#   * `http.client.HTTPException` covers `RemoteDisconnected` (a proxy or LB
#     closing an idle connection), `IncompleteRead`, and `BadStatusLine`.
#   * `json.JSONDecodeError` covers a truncated or non-JSON body.
#
# Ordering matters: `urllib.error.HTTPError` is itself an `OSError`, so its own
# `except` clause has to come first — a 404 is evidence about the catalog and
# must stay a MISMATCH.
TRANSPORT_ERRORS = (OSError, http.client.HTTPException, json.JSONDecodeError)

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


def _retry_base_seconds() -> float:
    """The first backoff delay, in seconds.

    A test seam, mirroring the daemon's `TETON_DOWNLOAD_RETRY_BASE_MS`: it moves
    the *clock* only. The number of attempts, the doubling, and above all the
    classification of what survives them are not adjustable, so a test that
    shortens the ladder still exercises the same ladder. Anything unparseable
    falls back to the production value rather than failing the run — a bad
    environment variable must not be able to turn a check into an error.
    """
    raw = os.environ.get("TETON_CATALOG_RETRY_BASE_MS")
    if raw is None:
        return 1.0
    try:
        return max(0.0, int(raw) / 1000.0)
    except ValueError:
        return 1.0


def http_get_json(url: str, attempts: int = 4):
    """GET `url` and parse JSON.

    Anonymous by design: sending no credential is what proves the repository is
    public and ungated (D-1 / BR-15). Transient statuses back off and retry;
    what survives is classified — `Unverified` for "upstream is unreachable or
    unwell", `Mismatch` for "upstream answered, and the answer says the catalog
    is wrong". A caller must never have to guess which happened.

    Nothing may leave this function unclassified. An exception that escapes it
    would end the process on Python's default exit code — 1, which *is*
    EXIT_MISMATCH — and so would report a dropped connection as catalog
    corruption. `TRANSPORT_ERRORS` exists to make that impossible; the
    `__main__` guard is the backstop for anything it still misses.
    """
    delay = _retry_base_seconds()
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
        except TRANSPORT_ERRORS as err:
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
        oid = lfs["oid"]
        # M-9: the oid is interpolated verbatim into `sha256 = "…"` in the TOML.
        # A hostile or MITM'd response could otherwise inject a quote + newline +
        # arbitrary `[[models]]` entry here. It *is* a SHA-256, so it must look
        # like exactly one — 64 lowercase hex — or it does not enter the catalog.
        if not isinstance(oid, str) or not SHA256_RE.match(oid):
            raise Mismatch(
                f"{repo}@{revision}:{path} reported an LFS oid {oid!r} that is not "
                f"a SHA-256 (64 lowercase hex). Refusing to write it into the "
                f"catalog — the response may be corrupt or tampered with."
            )
        return oid, int(lfs["size"])
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


def validate_pick(pick: dict) -> None:
    """Refuse a `PICKS` entry whose author-supplied fields are not safely shaped.

    `name`, `file`, and `repo` are interpolated into a filename or a URL and then
    into the TOML. They are developer-authored, not API-derived, but validating
    them keeps an ill-advised edit — a path separator, a stray quote, URL
    structure — from ever reaching the rendered catalog (M-9).
    """
    if not SAFE_NAME_RE.match(pick["name"]):
        raise Mismatch(
            f"catalog pick name {pick['name']!r} is not a plain id "
            f"(letters, digits, `.`, `-`, `_`)."
        )
    if not SAFE_FILE_RE.match(pick["file"]):
        raise Mismatch(
            f"catalog pick file {pick['file']!r} for {pick['name']!r} is not a "
            f"plain filename (letters, digits, `.`, `-`, `_`)."
        )
    if not SAFE_REPO_RE.match(pick["repo"]):
        raise Mismatch(
            f"catalog pick repo {pick['repo']!r} for {pick['name']!r} is not an "
            f"`owner/name` slug."
        )


def derive_rows(update_target, existing: str) -> list:
    """Derive every catalog row from the API, at `main` or at the pins.

    `update_target` is `None` (verify/print at the committed pins), or a single
    entry name whose repo is re-resolved to its current `main` (M-13). Every
    *other* entry stays at its committed pin, so re-adopting a publisher's new
    commit — an especially deliberate act for the third-party quantizer entry — is
    per-entry, never a side effect of refreshing the others.

    In pinned mode each revision is asserted immutable *before* it is used in a
    URL, so a moving ref is rejected locally rather than silently re-derived into
    a catalog that would drift the next time upstream moves.
    """
    pins = {name: e.get("revision") for name, e in committed_entries(existing).items()}
    rows = []
    for pick in PICKS:
        validate_pick(pick)
        if pick["name"] == update_target:
            revision = resolve_main(pick["repo"])
        else:
            if pick["name"] not in pins:
                raise Mismatch(
                    f"{CATALOG_PATH} pins no revision for {pick['name']!r}; "
                    f"run --update {pick['name']} to author it."
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


def derive(update_target, existing: str) -> str:
    """Derive the catalog, re-resolving `update_target` (if any) at its main."""
    return render(derive_rows(update_target=update_target, existing=existing))


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
    rows = derive_rows(update_target=None, existing=existing)
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
        metavar="NAME",
        help=(
            "re-resolve entry NAME's repo to its current main and rewrite the "
            "catalog. Per-entry on purpose (M-13): adopting a publisher's new "
            "commit — especially the third-party quantizer — must be deliberate, "
            "not a side effect of refreshing the others"
        ),
    )
    mode.add_argument(
        "--print", dest="to_stdout", action="store_true", help="write to stdout"
    )
    parser.add_argument(
        "--catalog",
        metavar="PATH",
        help="catalog file to read/write instead of the committed one",
    )
    args = parser.parse_args()

    if args.catalog:
        global CATALOG_PATH  # noqa: PLW0603 — the path is the tool's one global input
        CATALOG_PATH = os.path.abspath(args.catalog)

    try:
        with open(CATALOG_PATH, encoding="utf-8") as handle:
            existing = handle.read()
    except FileNotFoundError:
        existing = ""

    # M-13: `--update NAME` re-resolves exactly one entry. Reject a name that is
    # not a known pick before any network call, so a typo cannot quietly no-op.
    if args.update is not None and args.update not in {pick["name"] for pick in PICKS}:
        known = ", ".join(pick["name"] for pick in PICKS)
        sys.stderr.write(
            f"error: --update {args.update!r} names no catalog entry. Known entries: {known}.\n"
        )
        return EXIT_MISMATCH

    try:
        if args.check:
            return check(existing)

        generated = derive(update_target=args.update, existing=existing)
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
    try:
        sys.exit(main())
    except Exception:  # noqa: BLE001 — deliberate: see below
        # Python exits 1 on an uncaught exception, and 1 is EXIT_MISMATCH: "the
        # catalog is provably wrong". A crash proves nothing about the catalog,
        # so that collision has to be broken explicitly or every unforeseen bug
        # in this tool reads to CI as catalog corruption. The traceback is kept
        # (it is the only way to fix the crash) but the verdict is UNVERIFIED.
        traceback.print_exc()
        sys.stderr.write(
            f"\nUNVERIFIED: {sys.argv[0]} failed unexpectedly (traceback above).\n"
            f"The catalog was NOT verified. This is NOT a digest mismatch and "
            f"NOT evidence of corruption — the tool crashed before it could "
            f"learn anything about the catalog's contents "
            f"(exit {EXIT_UNVERIFIED} = EX_TEMPFAIL).\n"
        )
        sys.exit(EXIT_UNVERIFIED)
