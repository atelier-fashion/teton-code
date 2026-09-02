#!/usr/bin/env python3
"""Assert that `main`'s required status checks mirror the jobs `ci.yml` defines.

REQ-608. Branch protection is configuration held by the forge, not a file in the
tree, so the set of *required contexts* drifts from the set of *defined jobs*
silently — and in both directions. A required context nobody defines blocks every
merge; a defined job nobody requires blocks none. The second is how BUG-167 lived
on `main` for months: `feature-gated targets compile (all features)` ran on every
PR, reported red, and could not stop a merge (LESSON-464 — a control that exists
and that nothing fails when it is removed, weakened, or repositioned).

This check derives one set by parsing the workflow, reads the other from the
forge, and fails on either difference.

Derivation rule (ADR-608-4, BR-3), applied per job under `jobs:` in declaration
order. It is exact for what this repository actually uses and *refuses* the rest,
because a parser that guesses GitHub's rendering for a shape the repo does not
use works today and drifts silently tomorrow:

  * context = the job's `name:` when it is a string containing no `${{`;
    otherwise the job key. A `name:` carrying an expression is underivable — its
    rendering depends on runtime context this script does not have. So is a
    name that is empty, has surrounding whitespace, or carries a control
    character: the forge trims and re-flows what it renders, and a comparison
    on the raw string would be a guess.
  * a job that calls a reusable workflow (`uses:`) is underivable: the forge
    reports one check run per job of the *called* workflow, named
    `<caller> / <callee job>`, and this file does not read the callee.
  * no `strategy.matrix` -> one context.
  * `strategy.matrix` a mapping with exactly one key whose value is a non-empty
    list of strings or integers, and no `include`/`exclude` -> one context per
    value, `f"{context} ({value})"`, in list order.
  * any other matrix shape (two or more dimensions, `include`/`exclude`, an
    expression string, a boolean or float value) -> underivable.
  * two jobs deriving the same context are underivable together: the forge
    would hold two check runs against one required context, and one of them
    would be a job that reports and cannot block — REQ-608's own defect.

  An underivable job is *named and fails the run*. It is never dropped from the
  comparison: a job silently excluded is a job silently unrequired, which is
  REQ-608's own defect reappearing inside its fix.

  A job carrying `if:`, or a workflow whose `on.pull_request` carries
  `paths`/`paths-ignore`, is reported as a `::warning::` (BR-7's hazard: a
  required context that may never report deadlocks every merge) but does not by
  itself change the exit code.

Read path (ADR-608-2), in this order: the workflow is parsed and derived first
(local, no network — a broken workflow is reported without a round trip); then
`GET /repos/{owner}/{repo}/rules/branches/{branch}` — rulesets are *detected,
not parsed* (LESSON-460: no fixture written from imagination), so a non-empty
list — or a body that is not the documented list at all — stops the check
before any classic read is interpreted, and a migration to rulesets gets the
message written for it rather than "not protected"; then
`GET /repos/{owner}/{repo}/branches/{branch}`, whose
`protection.required_status_checks.contexts` is public on this repository. The
token — `$GITHUB_TOKEN`, optional — only lifts the anonymous rate limit shared by
every GitHub-hosted runner IP. Redirects are refused: a 3xx is reported as the
status it is, so the `Authorization` header can never follow one off-host.

Exit codes (ADR-608-3). A disagreement and a failed read are categorically
different events and never share a code:

    0   PARITY     the defined set and the required set are equal.
    1   DRIFT      they disagree. A real defect: the output names both sets,
                   both directions, and both remedies (BR-9).
    75  UNCHECKED  nothing was learned — the workflow could not be parsed, a job
                   was underivable, the forge could not be read, or the tool
                   crashed. EX_TEMPFAIL, matching tools/release/ and the
                   `catalog` and `tooling` jobs.

LESSON-442 is why `main()` ends in a blanket `except Exception` that returns 75:
Python exits 1 on an uncaught exception, and 1 *is* DRIFT. Without that handler
every unforeseen bug in this tool would be reported to CI as branch-protection
drift, sending the next reader to debug the forge instead of the script.

Rendering. Sets are derived in declaration order but *rendered sorted*, so two
runs against the same state print the same text. Every value that came from the
forge or from the workflow is printed through `_safe`, which escapes control
characters (C0, DEL, C1, and the Unicode line separators) and the backslash, so
the rendering is injective: GitHub Actions parses `::error::`-style workflow
commands at the start of any output line, so a context name carrying a newline
could otherwise forge an annotation or silence this job's own, and a name
carrying the literal text `\\x0a` must not render the same as one carrying a
line feed.

Usage
-----
    python3 tools/ci/required-checks-parity.py [--workflow PATH] [--repo OWNER/REPO]
                                               [--branch NAME]
    python3 tools/ci/required-checks-parity.py --pyyaml-pin

    --workflow PATH   default `.github/workflows/ci.yml`
    --repo OWNER/REPO default `$GITHUB_REPOSITORY`; absent from both is 75
    --branch NAME     default `main`
    --pyyaml-pin      print the PyYAML requirement this file expects and exit 0;
                      the CI job installs whatever this prints, so the pin has
                      one home
    $GITHUB_TOKEN     optional; rate limit only, no scope is required

Requires PyYAML (ADR-608-5), imported inside `main()` so a missing module is
reported as 75 with its remedy rather than as a traceback on exit code 1.
"""

from __future__ import annotations

import argparse
import http.client
import json
import os
import re
import sys
import traceback
import urllib.error
import urllib.parse
import urllib.request
from typing import Callable, Dict, List, Optional, Sequence, Set, Tuple

EXIT_PARITY = 0
EXIT_DRIFT = 1
EXIT_UNCHECKED = 75  # EX_TEMPFAIL

DEFAULT_WORKFLOW = os.path.join(".github", "workflows", "ci.yml")
DEFAULT_BRANCH = "main"

API_ROOT = "https://api.github.com"
HTTP_TIMEOUT_SECONDS = 20
USER_AGENT = "teton-code-required-checks-parity/1"
PYYAML_PIN = "PyYAML==6.0.2"

# OWNER/REPO as GitHub spells it: one slash, no path or query characters on
# either side. The value is interpolated into a URL, so this is the only shape
# accepted (LESSON-008 — a cited identifier is untrusted input).
OWNER_REPO_RE = re.compile(r"^(?!\.\.?/)[A-Za-z0-9_.-]+/(?!\.\.?$)[A-Za-z0-9_.-]+$")

# `fetch(url, token) -> (status, body_text)`: the injection seam (ADR-608-6).
Fetch = Callable[[str, Optional[str]], Tuple[int, str]]

# "The conversation with the forge broke", as opposed to "the forge answered".
# Membership mirrors tools/refresh-catalog.py's TRANSPORT_ERRORS and is
# load-bearing (LESSON-442): `OSError` covers urllib's URLError, TimeoutError and
# a mid-response ConnectionResetError; `http.client.HTTPException` covers
# RemoteDisconnected, IncompleteRead and BadStatusLine. None of them is evidence
# about branch protection, so all of them are 75 and never 1.
#
# Ordering matters where these are caught alongside `urllib.error.HTTPError`,
# which is itself an `OSError`: a status code is an *answer* and must be reported
# as one, so its clause always comes first.
TRANSPORT_ERRORS = (OSError, http.client.HTTPException)

# Verbatim in both failure directions (BR-9 / AC-10). A repo-wide red whose cause
# takes ten minutes to work out is a worse outcome than the drift it reports, so
# the message names the two edits that resolve it and says which one lives where.
# The `parity` job's DRIFT annotation in ci.yml paraphrases this; the two are
# kept in step by hand, since YAML cannot import it.
REMEDIES = """Two ways to resolve this, pick the one that matches intent:
  1. revert the protection edit — restore main's required checks to the set ci.yml defines
  2. update .github/workflows/ci.yml — make the defined jobs match the intended required set
(main's required checks are edited by a repository admin under Settings > Branches; never by a workflow)"""

_CONTROL_RE = re.compile("[\x00-\x1f\x7f-\x9f\u2028\u2029]")
_ESCAPE_RE = re.compile("[\\\\\x00-\x1f\x7f-\x9f\u2028\u2029]")


class Underivable(Exception):
    """A job whose check-run context this script's rule cannot produce.

    Carries the job key so a reader can tell a stated limitation of the
    derivation rule from a bug in it (BR-3).
    """

    def __init__(self, job_key: object, reason: str) -> None:
        super().__init__(f"job {job_key!r}: {reason}")
        self.job_key = job_key
        self.reason = reason


class Unverified(Exception):
    """The comparison could not be made. Nothing was learned either way (BR-5)."""


def _safe(value: object) -> str:
    """Render a value for output with every control character escaped.

    Names come from the forge and from the workflow, and this script prints
    them into a log that GitHub Actions scans for `::command::` lines. A
    newline inside a name is the whole attack; `\\x0a` is not. The backslash
    is escaped too, so the rendering is injective and a reader resolving drift
    can tell `a\\x0ab` (literal text) from `a<LF>b`.
    """

    def _one(match: "re.Match[str]") -> str:
        char = match.group(0)
        if char == "\\":
            return "\\\\"
        return f"\\x{ord(char):02x}" if ord(char) < 0x100 else f"\\u{ord(char):04x}"

    return _ESCAPE_RE.sub(_one, str(value))


def _plain_name(name: str) -> bool:
    """True when `name` is something the forge renders verbatim."""
    return bool(name) and name == name.strip() and not _CONTROL_RE.search(name)


def _scalar(value: object) -> Optional[str]:
    """Render one matrix value as GitHub renders it in a check-run name.

    Only shapes this repository actually uses are rendered. A boolean is refused
    rather than guessed at: `str(True)` is `'True'` and GitHub writes `true`, and
    the fix for that guess would be a fixture written from imagination
    (LESSON-460). A float is refused for the same reason — `3.10` parses to
    `3.1`. Callers turn a `None` return into an `Underivable`.
    """
    if isinstance(value, bool):
        return None
    if isinstance(value, str):
        return value if _plain_name(value) else None
    if isinstance(value, int):
        return str(value)
    return None


def _matrix_contexts(job_key: object, context: str, matrix: object) -> List[str]:
    """Expand a `strategy.matrix` into contexts, or raise `Underivable`."""
    if isinstance(matrix, str):
        raise Underivable(
            job_key,
            f"`strategy.matrix` is an expression ({matrix!r}); its legs are only "
            "known at run time, so the contexts it produces cannot be derived",
        )
    if not isinstance(matrix, dict):
        raise Underivable(
            job_key, f"`strategy.matrix` is a {type(matrix).__name__}, not a mapping"
        )

    for reserved in ("include", "exclude"):
        if reserved in matrix:
            raise Underivable(
                job_key,
                f"`strategy.matrix` carries `{reserved}:`; this check derives plain "
                "single-dimension matrices only (ADR-608-4)",
            )

    dimensions = list(matrix.keys())
    if len(dimensions) != 1:
        listed = ", ".join(repr(d) for d in dimensions) or "none"
        raise Underivable(
            job_key,
            f"`strategy.matrix` has {len(dimensions)} dimensions ({listed}); this "
            "check derives single-dimension matrices only, because GitHub's "
            "rendering of a cross product is not stated by ADR-608-4",
        )

    dimension = dimensions[0]
    values = matrix[dimension]
    if not isinstance(values, list) or not values:
        raise Underivable(
            job_key, f"`strategy.matrix.{dimension}` is not a non-empty list of scalars"
        )

    contexts = []
    for value in values:
        rendered = _scalar(value)
        if rendered is None:
            raise Underivable(
                job_key,
                f"`strategy.matrix.{dimension}` contains {value!r}, whose rendering "
                "in a check-run name this check does not derive",
            )
        contexts.append(f"{context} ({rendered})")
    return contexts


def derive_contexts(workflow: object) -> List[str]:
    """Return the check-run contexts `workflow` defines, in declaration order.

    Raises `Underivable` for any job the stated rule cannot resolve, and
    `Unverified` when the document is not a workflow at all. It never drops a
    job it cannot derive (BR-3).
    """
    if not isinstance(workflow, dict):
        raise Unverified(
            f"the workflow file did not parse to a mapping (got {type(workflow).__name__})"
        )

    jobs = workflow.get("jobs")
    if not isinstance(jobs, dict) or not jobs:
        raise Unverified(
            "the workflow declares no `jobs:` mapping, so it defines no check-run "
            "contexts to compare against branch protection"
        )

    contexts: List[str] = []
    owners: Dict[str, object] = {}
    for job_key, job in jobs.items():
        if not isinstance(job, dict):
            raise Underivable(job_key, f"the job is a {type(job).__name__}, not a mapping")

        if "uses" in job:
            raise Underivable(
                job_key,
                "the job calls a reusable workflow (`uses:`); the forge reports one "
                "check run per job of the called workflow, named "
                "`<caller> / <callee job>`, and this check does not read the callee",
            )

        name = job.get("name")
        if name is None:
            context = str(job_key)
        elif not isinstance(name, str):
            raise Underivable(job_key, f"`name:` is a {type(name).__name__}, not a string")
        elif "${{" in name:
            raise Underivable(
                job_key,
                f"`name:` contains an expression ({name!r}); what the forge renders "
                "it to depends on runtime context this check does not have",
            )
        elif not _plain_name(name):
            raise Underivable(
                job_key,
                f"`name:` is {name!r} — empty, padded, or carrying a control "
                "character; the forge re-flows such a name and the comparison "
                "would be a guess",
            )
        else:
            context = name

        strategy = job.get("strategy")
        if strategy is None:
            derived = [context]
        elif not isinstance(strategy, dict):
            raise Underivable(
                job_key, f"`strategy:` is a {type(strategy).__name__}, not a mapping"
            )
        elif "matrix" not in strategy:
            derived = [context]
        else:
            derived = _matrix_contexts(job_key, context, strategy["matrix"])

        for item in derived:
            if item in owners:
                raise Underivable(
                    job_key,
                    f"derives the context {item!r}, which job {owners[item]!r} "
                    "already derives. Two check runs against one required context "
                    "means one of them reports and cannot block — REQ-608's own "
                    "defect",
                )
            owners[item] = job_key
            contexts.append(item)

    return contexts


def _triggers(workflow: dict) -> dict:
    """Return the workflow's `on:` mapping.

    YAML 1.1 resolves the bare key `on` to the boolean `True`, so
    `workflow["on"]` is `KeyError` on every GitHub workflow ever written. Both
    spellings are looked up; `test_path_filter_warns_under_yaml_true_key` is the
    case that goes red if this is ever "tidied" to one.
    """
    for key in ("on", True):
        value = workflow.get(key)
        if isinstance(value, dict):
            return value
    return {}


def derive_warnings(workflow: object) -> List[str]:
    """Return `::warning::` lines for jobs that may not report on some PRs.

    BR-7's hazard, not a failure (ADR-608-4): under BR-4 every defined job is
    required, so a job that skips leaves a required context "Expected — waiting
    for status to be reported" and deadlocks the merge. The exit code is
    deliberately unaffected — the first PR that skips one makes it obvious, and
    reding every run on a conditional job would be a policy this REQ did not
    decide.
    """
    warnings: List[str] = []
    if not isinstance(workflow, dict):
        return warnings

    pull_request = _triggers(workflow).get("pull_request")
    if isinstance(pull_request, dict):
        for filter_key in ("paths", "paths-ignore"):
            if filter_key in pull_request:
                warnings.append(
                    f"::warning title=Path-filtered workflow::`on.pull_request` "
                    f"carries `{filter_key}:`, so every job here can be skipped on a "
                    "PR that touches nothing matching it. A required context that "
                    "never reports blocks the merge forever (BR-7)."
                )

    jobs = workflow.get("jobs")
    if isinstance(jobs, dict):
        for job_key, job in jobs.items():
            if isinstance(job, dict) and "if" in job:
                warnings.append(
                    f"::warning title=Conditional job::job {_safe(repr(job_key))} "
                    "carries `if:`, so it may not report on some PRs. A required "
                    "context that never reports blocks the merge forever (BR-7)."
                )
    return warnings


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    """Refuse every redirect so the `Authorization` header never leaves the host.

    urllib re-sends request headers on a redirect. `api.github.com` does not
    redirect these endpoints today; the day it does, or the day a proxy does,
    the 3xx comes back as the status it is and the run is UNCHECKED — the same
    posture the daemon's egress client takes (ADR-004).
    """

    def redirect_request(self, req, fp, code, msg, headers, newurl):  # noqa: D102
        return None


_OPENER = urllib.request.build_opener(_NoRedirect())


def _urllib_fetch(url: str, token: Optional[str]) -> Tuple[int, str]:
    """The real fetcher. `(status, body_text)`; transport failures propagate.

    A status code is an *answer*, so a 4xx/5xx — and, with `_NoRedirect`, a
    3xx — comes back as a status rather than an exception.
    `urllib.error.HTTPError` is caught first precisely because it is an
    `OSError` subclass and would otherwise be classified as a broken
    connection. Everything that really is a broken connection is left to
    raise, and `_get_json` turns it into `Unverified` naming the class
    (LESSON-442).
    """
    headers = {
        "User-Agent": USER_AGENT,
        "Accept": "application/vnd.github+json",
        "X-GitHub-Api-Version": "2022-11-28",
    }
    if token:
        headers["Authorization"] = f"Bearer {token}"

    request = urllib.request.Request(url, headers=headers)
    try:
        with _OPENER.open(request, timeout=HTTP_TIMEOUT_SECONDS) as response:
            body = response.read().decode("utf-8", "replace")
            return response.getcode(), body
    except urllib.error.HTTPError as err:
        try:
            body = err.read().decode("utf-8", "replace")
        except TRANSPORT_ERRORS:
            body = ""
        return err.code, body


def _get_json(fetch: Fetch, url: str, token: Optional[str], what: str) -> object:
    """Call `fetch` and parse JSON, or raise `Unverified` naming the URL."""
    try:
        status, body = fetch(url, token)
    except TRANSPORT_ERRORS as err:
        raise Unverified(
            f"could not reach {url} while reading {what}: {type(err).__name__} "
            f"({err}). This is a transport failure — nothing was learned about "
            "the required checks."
        ) from err

    if not 200 <= status < 300:
        raise Unverified(
            f"GET {url} returned HTTP {status} while reading {what}. The required "
            "checks were NOT read, so this is not evidence that they match."
        )

    try:
        return json.loads(body)
    except (json.JSONDecodeError, ValueError) as err:
        raise Unverified(
            f"GET {url} returned a body that is not JSON while reading {what}: {err}"
        ) from err


def read_required(
    fetch: Fetch, owner_repo: str, branch: str, token: Optional[str] = None
) -> List[str]:
    """Return the contexts `branch` protection requires, or raise `Unverified`.

    `fetch(url, token) -> (status, body_text)` is injected so the unit tests
    never open a socket (ADR-608-6). Every condition below fails closed: a read
    this script could not complete must never be reported as "matches" (BR-5,
    LESSON-510). Rulesets are read *first* so that a repository which has moved
    to them is told so, rather than told its branch is unprotected.
    """
    if not owner_repo or not OWNER_REPO_RE.match(owner_repo):
        raise Unverified(
            f"repository {owner_repo!r} is not OWNER/REPO. Pass --repo or set "
            "$GITHUB_REPOSITORY."
        )
    if not branch or not _plain_name(branch):
        raise Unverified(f"branch {branch!r} is not a usable branch name.")

    owner, repo = owner_repo.split("/")
    ref = urllib.parse.quote(branch, safe="")
    branch_url = f"{API_ROOT}/repos/{owner}/{repo}/branches/{ref}"
    rules_url = f"{API_ROOT}/repos/{owner}/{repo}/rules/branches/{ref}"

    # Rulesets are DETECTED, NOT PARSED (ADR-608-2). `/rules/branches/main`
    # returned `[]` on this repository when REQ-608 measured it; writing a parser
    # now against a shape nobody here has ever produced would be a fixture built
    # from imagination (LESSON-460). A non-empty list means classic protection is
    # no longer the whole truth, and a partial answer must not be rendered as a
    # verdict. A failed read, or a body that is not the list this endpoint
    # documents, is 75 for the same reason: "could not check" must never be
    # downgraded to "no rulesets".
    rules = _get_json(fetch, rules_url, token, "rulesets")
    if not isinstance(rules, list):
        raise Unverified(
            f"GET {rules_url} did not return the list this endpoint documents "
            f"(got {type(rules).__name__}), so whether rulesets govern the branch "
            "could not be determined."
        )
    if rules:
        raise Unverified(
            "rulesets present; this check reads classic protection only — extend "
            f"it. GET {rules_url} returned {len(rules)} rule(s), so classic "
            "protection is no longer the whole required set."
        )

    payload = _get_json(fetch, branch_url, token, "branch protection")
    if not isinstance(payload, dict):
        raise Unverified(f"GET {branch_url} did not return a branch object")

    if not payload.get("protected"):
        raise Unverified(
            f"branch {branch!r} is not protected — nothing to compare against. "
            "Every job in ci.yml is unrequired and no check can block a merge."
        )

    protection = payload.get("protection")
    if not isinstance(protection, dict):
        raise Unverified(
            f"GET {branch_url} reports the branch protected but carries no "
            "`protection` object"
        )

    required = protection.get("required_status_checks")
    if not isinstance(required, dict):
        raise Unverified(
            f"`protection.required_status_checks` is absent from GET {branch_url}, "
            "so the required contexts could not be read"
        )

    contexts = required.get("contexts")
    if not isinstance(contexts, list):
        raise Unverified(
            f"`protection.required_status_checks.contexts` is absent from GET "
            f"{branch_url}, so the required contexts could not be read"
        )

    for context in contexts:
        if not isinstance(context, str):
            raise Unverified(
                f"`protection.required_status_checks.contexts` from GET {branch_url} "
                f"holds a {type(context).__name__} ({context!r}), not a string; an "
                "uninterpretable read is not a disagreement"
            )
    return list(contexts)


def compare(defined: Sequence[str], required: Sequence[str]) -> Tuple[Set[str], Set[str]]:
    """Return `(missing, stale)` as sets.

    `missing` = defined but not required (REQ-608's own defect: a job that
    reports and cannot block). `stale` = required but not defined (a context that
    can never report, which blocks every merge). Both fail the run — BR-4 settles
    that here rather than leaving it to a caller. Duplicates in `defined` are
    refused upstream by `derive_contexts`, so set semantics lose nothing here.
    """
    defined_set = set(defined)
    required_set = set(required)
    return defined_set - required_set, required_set - defined_set


def _block(header: str, values: Sequence[str]) -> List[str]:
    lines = [header]
    if values:
        lines.extend(f"  - {_safe(value)}" for value in sorted(values))
    else:
        lines.append("  (none)")
    return lines


def render(
    defined: Sequence[str],
    required: Sequence[str],
    missing: Set[str],
    stale: Set[str],
    workflow_path: str,
    owner_repo: str,
    branch: str,
) -> str:
    """Return the rendered verdict as one string.

    Both sets are always shown, sorted, so a reader who has to resolve drift can
    see what each side actually holds without a second command. AC-10 is
    asserted against this text, not against the source of the message
    (LESSON-519).
    """
    lines = [
        f"required-checks parity — {_safe(owner_repo)} @ {_safe(branch)} "
        f"(workflow: {_safe(workflow_path)})",
        "",
    ]
    lines.extend(_block("defined by ci.yml:", defined))
    lines.append("")
    lines.extend(_block("required by main:", required))

    if missing or stale:
        lines.append("")
        lines.extend(_block("missing (defined, not required):", sorted(missing)))
        lines.append("")
        lines.extend(_block("stale (required, not defined):", sorted(stale)))
        lines.append("")
        lines.append(REMEDIES)

    return "\n".join(lines)


def _unchecked(message: str) -> int:
    """Render a 75 and return it. One vocabulary, never mistakable for drift.

    Printing is best-effort: if stdout itself is gone (`BrokenPipeError`), the
    verdict must still be 75 — a second exception escaping from here would exit
    1, which is DRIFT (LESSON-442).
    """
    try:
        print(f"UNCHECKED: {_safe(message)}")
        print(
            "\nThe required checks were NOT compared. This is NOT evidence that they "
            f"match ci.yml — nothing was learned either way (exit {EXIT_UNCHECKED} "
            "= EX_TEMPFAIL)."
        )
        # Flush here, inside the guard: a closed pipe otherwise surfaces at
        # interpreter shutdown, where CPython exits 120 and the 75 is lost.
        sys.stdout.flush()
    except OSError:
        pass
    return EXIT_UNCHECKED


def main(argv: Sequence[str], fetch: Optional[Fetch] = None) -> int:
    """Compare the two sets and return 0 / 1 / 75.

    `fetch` is the injection seam (ADR-608-6): tests pass a fake, and nothing
    else in this file reaches the network.
    """
    try:
        parser = argparse.ArgumentParser(
            prog="required-checks-parity.py",
            description=(
                "Assert that a branch's required status checks mirror the jobs "
                "the workflow defines (REQ-608)."
            ),
        )
        parser.add_argument("--workflow", default=DEFAULT_WORKFLOW)
        parser.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY"))
        parser.add_argument("--branch", default=DEFAULT_BRANCH)
        parser.add_argument(
            "--pyyaml-pin",
            action="store_true",
            help="print the PyYAML requirement this script expects and exit",
        )
        args = parser.parse_args(argv)

        if args.pyyaml_pin:
            print(PYYAML_PIN)
            return EXIT_PARITY

        # ADR-608-5: imported here, not at module scope, so an absent PyYAML is a
        # 75 naming its remedy rather than an ImportError traceback on exit 1 —
        # which is DRIFT, and would send the reader to the forge.
        try:
            import yaml
        except ImportError as err:
            return _unchecked(
                f"PyYAML is not importable ({err}), so {args.workflow} could not "
                "be parsed. Install it with:\n"
                f"    python3 -m pip install --user '{PYYAML_PIN}'"
            )

        if not args.repo:
            return _unchecked(
                "no repository to read: pass --repo OWNER/REPO or set "
                "$GITHUB_REPOSITORY."
            )

        try:
            with open(args.workflow, "r", encoding="utf-8") as handle:
                workflow = yaml.safe_load(handle)
        except (OSError, yaml.YAMLError) as err:
            return _unchecked(
                f"could not read the workflow {args.workflow}: "
                f"{type(err).__name__} ({err})"
            )

        defined = derive_contexts(workflow)

        for warning in derive_warnings(workflow):
            print(warning)

        required = read_required(
            fetch or _urllib_fetch,
            args.repo,
            args.branch,
            os.environ.get("GITHUB_TOKEN"),
        )

        missing, stale = compare(defined, required)
        print(
            render(defined, required, missing, stale, args.workflow, args.repo, args.branch)
        )
        sys.stdout.flush()
        return EXIT_DRIFT if (missing or stale) else EXIT_PARITY

    except Underivable as err:
        return _unchecked(
            f"{err}. The derivation rule is stated in this file's header; an "
            "underivable job is never dropped from the comparison, because a job "
            "silently excluded is a job silently unrequired (BR-3)."
        )
    except Unverified as err:
        return _unchecked(str(err))
    except Exception as err:  # noqa: BLE001 — deliberate, see below
        # LESSON-442. Python exits 1 on an uncaught exception and 1 is DRIFT, so
        # without this every crash in this tool would be reported to CI as branch
        # protection drift. The traceback is kept — it is the only way to fix the
        # crash — but the verdict is UNCHECKED.
        try:
            traceback.print_exc()
        except OSError:
            pass
        return _unchecked(
            f"{sys.argv[0]} failed unexpectedly with {type(err).__name__}: {err} "
            "(traceback above). A crash is not a disagreement."
        )


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
