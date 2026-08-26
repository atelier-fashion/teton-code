#!/usr/bin/env python3
"""Regenerate `crates/tetond/tests/fixtures/token_corpus/token_counts.json`.

The daemon has no tokenizer at runtime: its context budget is estimated in
two currencies — whitespace words scaled by `REMOTE_TOKENS_PER_WORD` (3/2)
and bytes divided by the 2 B/token floor — and REQ-586 AC-3 pins both
allowances against a real corpus tokenized ONCE with a reference tokenizer
(architecture ADR-10). The token counts live in a committed fixture so the
Rust test (`crates/tetond/tests/token_corpus.rs`) needs no tokenizer, no
network and no new Cargo dependency; this script is the only sanctioned way
to write them. The counts are *derived data*, never hand-typed (LESSON-460).

Tokenizer
---------
`tiktoken` encoding `o200k_base` (the GPT-4o / o-series vocabulary). The
committed fixture was produced with tiktoken 0.14.0 on CPython 3.11; the
version used is recorded in every row (`tokenizer_version`). `o200k_base`
is a fixed BPE vocabulary, so a newer tiktoken should reproduce the same
counts — if it does not, that is worth a look before the fixture is rewritten.
Special-token strings (e.g. `<|endoftext|>`) are encoded as ordinary text
(`disallowed_special=()`), which is how a provider treats user-supplied text.

Per-sample counts
-----------------
* `words`  — whitespace-delimited pieces, by the SAME rule as the daemon's
  `approx_tokens` (`str::split_whitespace`, crates/tetond/src/harness/context.rs):
  runs of Unicode `White_Space` characters separate words. Implemented here
  with an explicit `White_Space` class (not `str.split()`, which also splits
  on U+001C..U+001F). The Rust test recomputes `words` and `bytes` from the
  sample files and refuses a fixture whose counts no longer match.
* `bytes`  — the file's size in bytes (UTF-8).
* `tokens` — `len(encoding.encode(text))`.

Samples (crates/tetond/tests/fixtures/token_corpus/)
-----------------------------------------------------
prose.txt      English prose (written for the fixture; no copyrighted text).
rust.rs        a real slice of crates/tetond/src/harness/context.rs
               (approx_tokens .. SummarizeOutcome).
minified.json  a minified `cargo metadata --no-deps` subset of this workspace
               (a real shell-tool result shape), workspace root rewritten to
               the CI checkout path so no local path is committed.
paths.txt      real `find crates -name '*.rs'` output (sorted), prefixed with
               the same CI checkout root, one absolute path per line.
base64.txt     4 KiB of base64 over /dev/urandom bytes, wrapped at 76 columns.
numeric_grid.txt
               REQ-590 AC-9: the token-dense **byte-light** quadrant — a
               small-integer matrix as `numpy.savetxt(fmt="%d")` writes one
               (a quantized intensity raster), 160 rows x 64 columns of
               space-separated single digits. Exactly 10,240 words / 20,480
               bytes, i.e. one turn at the full local word budget that the
               byte budget (32,768) admits with room to spare. Reproducible
               by construction, in integer arithmetic so no libm rounding can
               move a cell:

                   ROWS, COLS = 160, 64
                   "".join(
                       " ".join(
                           str((((x - 32) ** 2 + (y - 80) ** 2) // 53) % 10)
                           for x in range(COLS)
                       ) + "\\n"
                       for y in range(ROWS)
                   )

               The measured density is a property of the *format*, not of
               this particular field: a random 0-9 grid, a sparse 0/1 mask and
               a run-heavy grid of the same shape all tokenize to exactly
               2.000 tokens per whitespace word, because `o200k_base` gives
               every digit and every separating space its own token.

Usage
-----
    pip install tiktoken                       # once; not a repo dependency
    python3 tools/token_corpus/count.py        # rewrite token_counts.json
    python3 tools/token_corpus/count.py --check
        Exit 0 when the committed json is byte-identical to what this script
        emits now; exit 1 (listing the differing rows) when it is not — i.e. a
        sample file changed without the fixture being regenerated, or the
        fixture was hand-edited.

Either mode prints a per-sample table (words, bytes, tokens, bytes/token,
tokens/word) to stderr — the evidence the 3/2 ratio and 2 B/token floor are
judged against, recorded in the REQ-586 runbook (TASK-191).

CI never runs this script; the Rust test only reads the json.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

ENCODING_NAME = "o200k_base"

REPO_ROOT = Path(__file__).resolve().parents[2]
CORPUS_DIR = REPO_ROOT / "crates" / "tetond" / "tests" / "fixtures" / "token_corpus"
COUNTS_FILE = CORPUS_DIR / "token_counts.json"

# Unicode `White_Space` — exactly the set `char::is_whitespace` (and therefore
# `str::split_whitespace`) recognizes. Kept explicit so the word rule here
# cannot drift from the daemon's estimator.
_WORD_SPLIT = re.compile(
    "[\t\n\x0b\x0c\r \x85\xa0\u1680\u2000-\u200a\u2028\u2029\u202f\u205f\u3000]+"
)


def count_words(text: str) -> int:
    """Whitespace-word count by the daemon's `approx_tokens` rule."""
    return sum(1 for piece in _WORD_SPLIT.split(text) if piece)


def sample_files() -> list[Path]:
    files = sorted(
        p
        for p in CORPUS_DIR.iterdir()
        # Dotfiles (a Finder `.DS_Store`) are not samples; the json is the output.
        if p.is_file() and not p.name.startswith(".") and p.name != COUNTS_FILE.name
    )
    if not files:
        sys.exit(f"error: no sample files under {CORPUS_DIR}")
    return files


def compute_rows() -> list[dict]:
    try:
        import tiktoken  # type: ignore[import-not-found]
    except ImportError:
        sys.exit("error: tiktoken is not installed — `pip install tiktoken` (it is not a repo dependency)")
    encoding = tiktoken.get_encoding(ENCODING_NAME)
    tokenizer_version = f"tiktoken {tiktoken.__version__}"
    rows = []
    for path in sample_files():
        raw = path.read_bytes()
        text = raw.decode("utf-8")  # strict: the Rust side reads with read_to_string
        rows.append(
            {
                "file": path.name,
                "words": count_words(text),
                "bytes": len(raw),
                "tokens": len(encoding.encode(text, disallowed_special=())),
                "tokenizer": ENCODING_NAME,
                "tokenizer_version": tokenizer_version,
            }
        )
    return rows


def render(rows: list[dict]) -> str:
    return json.dumps(rows, indent=2, ensure_ascii=True) + "\n"


def print_table(rows: list[dict]) -> None:
    print(
        f"{'sample':<15}{'words':>8}{'bytes':>8}{'tokens':>8}{'B/token':>10}{'tok/word':>10}",
        file=sys.stderr,
    )
    for row in rows:
        print(
            f"{row['file']:<15}{row['words']:>8}{row['bytes']:>8}{row['tokens']:>8}"
            f"{row['bytes'] / row['tokens']:>10.2f}{row['tokens'] / row['words']:>10.2f}",
            file=sys.stderr,
        )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--check",
        action="store_true",
        help="exit non-zero if the committed token_counts.json differs from a fresh count",
    )
    args = parser.parse_args()

    rows = compute_rows()
    print_table(rows)
    fresh = render(rows)

    if args.check:
        if not COUNTS_FILE.exists():
            print(f"error: {COUNTS_FILE} is missing", file=sys.stderr)
            return 1
        committed = COUNTS_FILE.read_text(encoding="utf-8")
        if committed == fresh:
            print(f"ok: {COUNTS_FILE.relative_to(REPO_ROOT)} is current", file=sys.stderr)
            return 0
        try:
            committed_rows = {r["file"]: r for r in json.loads(committed)}
        except (json.JSONDecodeError, KeyError, TypeError):
            committed_rows = {}
        for row in rows:
            old = committed_rows.get(row["file"])
            if old != row:
                print(f"stale: {row['file']}: committed {old} != fresh {row}", file=sys.stderr)
        for name in set(committed_rows) - {r["file"] for r in rows}:
            print(f"stale: {name}: committed row has no sample file", file=sys.stderr)
        print("error: token_counts.json is stale — rerun without --check to regenerate", file=sys.stderr)
        return 1

    COUNTS_FILE.write_text(fresh, encoding="utf-8")
    print(f"wrote {COUNTS_FILE.relative_to(REPO_ROOT)} ({len(rows)} samples)", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
