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
    rendering depends on runtime context this script does not have.
  * no `strategy.matrix` -> one context.
  * `strategy.matrix` a mapping with exactly one key whose value is a non-empty
    list of scalars, and no `include`/`exclude` -> one context per value,
    `f"{context} ({value})"`, in list order.
  * any other matrix shape (two or more dimensions, `include`/`exclude`, an
    expression string, a boolean value) -> underivable.

  An underivable job is *named and fails the run*. It is never dropped from the
  comparison: a job silently excluded is a job silently unrequired, which is
  REQ-608's own defect reappearing inside its fix.

  A job carrying `if:`, or a workflow whose `on.pull_request` carries
  `paths`/`paths-ignore`, is reported as a `::warning::` (BR-7's hazard: a
  required context that may never report deadlocks every merge) but does not by
  itself change the exit code.

Read path (ADR-608-2): `GET /repos/{owner}/{repo}/branches/{branch}`, whose
`protection.required_status_checks.contexts` is public on this repository. The
token — `$GITHUB_TOKEN`, optional — only lifts the anonymous rate limit shared by
every GitHub-hosted runner IP. `GET /repos/{owner}/{repo}/rules/branches/{branch}`
is read too: rulesets are *detected, not parsed* (LESSON-460 — no fixture written
from imagination), so a non-empty list stops the check rather than letting it
compare against half the truth.

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

Usage
-----
    python3 tools/ci/required-checks-parity.py [--workflow PATH] [--repo OWNER/REPO]
                                               [--branch NAME]

    --workflow PATH   default `.github/workflows/ci.yml`
    --repo OWNER/REPO default `$GITHUB_REPOSITORY`; absent from both is 75
    --branch NAME     default `main`
    $GITHUB_TOKEN     optional; rate limit only, no scope is required

Requires PyYAML (ADR-608-5), imported inside `main()` so a missing module is
reported as 75 with its remedy rather than as a traceback on exit code 1.
"""

from __future__ import annotations

import argparse
import http.client
import json
import os
import sys
import traceback
import urllib.error
import urllib.request

EXIT_PARITY = 0
EXIT_DRIFT = 1
EXIT_UNCHECKED = 75  # EX_TEMPFAIL

DEFAULT_WORKFLOW = os.path.join(".github", "workflows", "ci.yml")
DEFAULT_BRANCH = "main"

API_ROOT = "https://api.github.com"
HTTP_TIMEOUT_SECONDS = 20
USER_AGENT = "teton-code-required-checks-parity/1"
PYYAML_PIN = "PyYAML==6.0.2"

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
REMEDIES = """Two ways to resolve this, pick the one that matches intent:
  1. revert the protection edit — restore main's required checks to the set ci.yml defines
  2. update .github/workflows/ci.yml — make the defined jobs match the intended required set
(main's required checks are edited by a repository admin under Settings > Branches; never by a workflow)"""


class Underivable(Exception):
    """A job whose check-run context this script's rule cannot produce.

    Carries the job key so a reader can tell a stated limitation of the
    derivation rule from a bug in it (BR-3).
    """

    def __init__(self, job_key, reason):
        super().__init__("job {0!r}: {1}".format(job_key, reason))
        self.job_key = job_key
        self.reason = reason


class Unverified(Exception):
    """The comparison could not be made. Nothing was learned either way (BR-5)."""


def _scalar(value):
    """Render one matrix value as GitHub renders it in a check-run name.

    Only shapes this repository actually uses are rendered. A boolean is refused
    rather than guessed at: `str(True)` is `'True'` and GitHub writes `true`, and
    the fix for that guess would be a fixture written from imagination
    (LESSON-460). Callers turn a `None` return into an `Underivable`.
    """
    if isinstance(value, bool):
        return None
    if isinstance(value, str):
        return value
    if isinstance(value, (int, float)):
        return str(value)
    return None


def _matrix_contexts(job_key, context, matrix):
    """Expand a `strategy.matrix` into contexts, or raise `Underivable`."""
    if isinstance(matrix, str):
        raise Underivable(
            job_key,
            "`strategy.matrix` is an expression ({0!r}); its legs are only known "
            "at run time, so the contexts it produces cannot be derived".format(matrix),
        )
    if not isinstance(matrix, dict):
        raise Underivable(
            job_key,
            "`strategy.matrix` is a {0}, not a mapping".format(type(matrix).__name__),
        )

    for reserved in ("include", "exclude"):
        if reserved in matrix:
            raise Underivable(
                job_key,
                "`strategy.matrix` carries `{0}:`; this check derives plain "
                "single-dimension matrices only (ADR-608-4)".format(reserved),
            )

    dimensions = list(matrix.keys())
    if len(dimensions) != 1:
        raise Underivable(
            job_key,
            "`strategy.matrix` has {0} dimensions ({1}); this check derives "
            "single-dimension matrices only, because GitHub's rendering of a "
            "cross product is not stated by ADR-608-4".format(
                len(dimensions), ", ".join(repr(d) for d in dimensions) or "none"
            ),
        )

    values = matrix[dimensions[0]]
    if not isinstance(values, list) or not values:
        raise Underivable(
            job_key,
            "`strategy.matrix.{0}` is not a non-empty list of scalars".format(
                dimensions[0]
            ),
        )

    contexts = []
    for value in values:
        rendered = _scalar(value)
        if rendered is None:
            raise Underivable(
                job_key,
                "`strategy.matrix.{0}` contains {1!r}, whose rendering in a "
                "check-run name this check does not derive".format(
                    dimensions[0], value
                ),
            )
        contexts.append("{0} ({1})".format(context, rendered))
    return contexts


def derive_contexts(workflow):
    """Return the check-run contexts `workflow` defines, in declaration order.

    Raises `Underivable` for any job the stated rule cannot resolve, and
    `Unverified` when the document is not a workflow at all. It never drops a
    job it cannot derive (BR-3).
    """
    if not isinstance(workflow, dict):
        raise Unverified(
            "the workflow file did not parse to a mapping (got {0})".format(
                type(workflow).__name__
            )
        )

    jobs = workflow.get("jobs")
    if not isinstance(jobs, dict) or not jobs:
        raise Unverified(
            "the workflow declares no `jobs:` mapping, so it defines no check-run "
            "contexts to compare against branch protection"
        )

    contexts = []
    for job_key, job in jobs.items():
        if not isinstance(job, dict):
            raise Underivable(
                job_key, "the job is a {0}, not a mapping".format(type(job).__name__)
            )

        name = job.get("name")
        if name is None:
            context = str(job_key)
        elif not isinstance(name, str):
            raise Underivable(
                job_key,
                "`name:` is a {0}, not a string".format(type(name).__name__),
            )
        elif "${{" in name:
            raise Underivable(
                job_key,
                "`name:` contains an expression ({0!r}); what the forge renders it "
                "to depends on runtime context this check does not have".format(name),
            )
        else:
            context = name

        strategy = job.get("strategy")
        if strategy is None:
            contexts.append(context)
            continue
        if not isinstance(strategy, dict):
            raise Underivable(
                job_key,
                "`strategy:` is a {0}, not a mapping".format(type(strategy).__name__),
            )
        if "matrix" not in strategy:
            contexts.append(context)
            continue

        contexts.extend(_matrix_contexts(job_key, context, strategy["matrix"]))

    return contexts


def _triggers(workflow):
    """Return the workflow's `on:` mapping.

    YAML 1.1 resolves the bare key `on` to the boolean `True`, so
    `workflow["on"]` is `KeyError` on every GitHub workflow ever written. Both
    spellings are looked up; this is the single most likely place for a silent
    wrong answer in this file.
    """
    for key in ("on", True):
        value = workflow.get(key)
        if isinstance(value, dict):
            return value
    return {}


def derive_warnings(workflow):
    """Return `::warning::` lines for jobs that may not report on some PRs.

    BR-7's hazard, not a failure (ADR-608-4): under BR-4 every defined job is
    required, so a job that skips leaves a required context "Expected — waiting
    for status to be reported" and deadlocks the merge. The exit code is
    deliberately unaffected — the first PR that skips one makes it obvious, and
    reding every run on a conditional job would be a policy this REQ did not
    decide.
    """
    warnings = []
    if not isinstance(workflow, dict):
        return warnings

    pull_request = _triggers(workflow).get("pull_request")
    if isinstance(pull_request, dict):
        for filter_key in ("paths", "paths-ignore"):
            if filter_key in pull_request:
                warnings.append(
                    "::warning title=Path-filtered workflow::`on.pull_request` "
                    "carries `{0}:`, so every job here can be skipped on a PR that "
                    "touches nothing matching it. A required context that never "
                    "reports blocks the merge forever (BR-7).".format(filter_key)
                )

    jobs = workflow.get("jobs")
    if isinstance(jobs, dict):
        for job_key, job in jobs.items():
            if isinstance(job, dict) and "if" in job:
                warnings.append(
                    "::warning title=Conditional job::job {0!r} carries `if:`, so "
                    "it may not report on some PRs. A required context that never "
                    "reports blocks the merge forever (BR-7).".format(job_key)
                )
    return warnings


def _urllib_fetch(url, token):
    """The real fetcher. `(status, body_text)`; transport failures propagate.

    A status code is an *answer*, so a 4xx/5xx comes back as a status rather than
    an exception — `urllib.error.HTTPError` is caught first precisely because it
    is an `OSError` subclass and would otherwise be classified as a broken
    connection. Everything that really is a broken connection is left to raise,
    and `read_required` turns it into `Unverified` naming the class (LESSON-442).
    """
    headers = {
        "User-Agent": USER_AGENT,
        "Accept": "application/vnd.github+json",
        "X-GitHub-Api-Version": "2022-11-28",
    }
    if token:
        headers["Authorization"] = "Bearer {0}".format(token)

    request = urllib.request.Request(url, headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=HTTP_TIMEOUT_SECONDS) as response:
            body = response.read().decode("utf-8", "replace")
            return response.getcode(), body
    except urllib.error.HTTPError as err:
        try:
            body = err.read().decode("utf-8", "replace")
        except TRANSPORT_ERRORS:
            body = ""
        return err.code, body


def _get_json(fetch, url, token, what):
    """Call `fetch` and parse JSON, or raise `Unverified` naming the URL."""
    try:
        status, body = fetch(url, token)
    except TRANSPORT_ERRORS as err:
        raise Unverified(
            "could not reach {0} while reading {1}: {2} ({3}). This is a transport "
            "failure — nothing was learned about the required checks.".format(
                url, what, type(err).__name__, err
            )
        ) from err

    if not 200 <= status < 300:
        raise Unverified(
            "GET {0} returned HTTP {1} while reading {2}. The required checks were "
            "NOT read, so this is not evidence that they match.".format(
                url, status, what
            )
        )

    try:
        return json.loads(body)
    except (json.JSONDecodeError, ValueError) as err:
        raise Unverified(
            "GET {0} returned a body that is not JSON while reading {1}: {2}".format(
                url, what, err
            )
        ) from err


def read_required(fetch, owner_repo, branch, token=None):
    """Return the contexts `branch` protection requires, or raise `Unverified`.

    `fetch(url, token) -> (status, body_text)` is injected so the unit tests
    never open a socket (ADR-608-6). Every condition below fails closed: a read
    this script could not complete must never be reported as "matches" (BR-5,
    LESSON-510).
    """
    if not owner_repo or owner_repo.count("/") != 1 or not all(owner_repo.split("/")):
        raise Unverified(
            "repository {0!r} is not OWNER/REPO. Pass --repo or set "
            "$GITHUB_REPOSITORY.".format(owner_repo)
        )

    owner, repo = owner_repo.split("/")
    branch_url = "{0}/repos/{1}/{2}/branches/{3}".format(API_ROOT, owner, repo, branch)
    rules_url = "{0}/repos/{1}/{2}/rules/branches/{3}".format(
        API_ROOT, owner, repo, branch
    )

    payload = _get_json(fetch, branch_url, token, "branch protection")
    if not isinstance(payload, dict):
        raise Unverified(
            "GET {0} did not return a branch object".format(branch_url)
        )

    if not payload.get("protected"):
        raise Unverified(
            "branch {0!r} is not protected — nothing to compare against. Every job "
            "in ci.yml is unrequired and no check can block a merge.".format(branch)
        )

    protection = payload.get("protection")
    if not isinstance(protection, dict):
        raise Unverified(
            "GET {0} reports the branch protected but carries no `protection` "
            "object".format(branch_url)
        )

    required = protection.get("required_status_checks")
    if not isinstance(required, dict):
        raise Unverified(
            "`protection.required_status_checks` is absent from GET {0}, so the "
            "required contexts could not be read".format(branch_url)
        )

    contexts = required.get("contexts")
    if not isinstance(contexts, list):
        raise Unverified(
            "`protection.required_status_checks.contexts` is absent from GET {0}, "
            "so the required contexts could not be read".format(branch_url)
        )

    # Rulesets are DETECTED, NOT PARSED (ADR-608-2). `/rules/branches/main`
    # returned `[]` on this repository when REQ-608 measured it; writing a parser
    # now against a shape nobody here has ever produced would be a fixture built
    # from imagination (LESSON-460). A non-empty list means classic protection is
    # no longer the whole truth, and a partial answer must not be rendered as a
    # verdict. A failed rulesets read is 75 for the same reason: "could not
    # check" must never be downgraded to "no rulesets".
    rules = _get_json(fetch, rules_url, token, "rulesets")
    if isinstance(rules, list) and rules:
        raise Unverified(
            "rulesets present; this check reads classic protection only — extend "
            "it. GET {0} returned {1} rule(s), so `contexts` above is no longer "
            "the whole required set.".format(rules_url, len(rules))
        )

    return [str(context) for context in contexts]


def compare(defined, required):
    """Return `(missing, stale)` as sets.

    `missing` = defined but not required (REQ-608's own defect: a job that
    reports and cannot block). `stale` = required but not defined (a context that
    can never report, which blocks every merge). Both fail the run — BR-4 settles
    that here rather than leaving it to a caller.
    """
    defined_set = set(defined)
    required_set = set(required)
    return defined_set - required_set, required_set - defined_set


def _block(header, values):
    lines = [header]
    if values:
        lines.extend("  - {0}".format(value) for value in sorted(values))
    else:
        lines.append("  (none)")
    return lines


def render(defined, required, missing, stale, workflow_path, owner_repo, branch):
    """Return the rendered verdict as one string.

    Both sets are always shown, so a reader who has to resolve drift can see what
    each side actually holds without a second command. AC-10 is asserted against
    this text, not against the source of the message (LESSON-519).
    """
    lines = [
        "required-checks parity — {0} @ {1} (workflow: {2})".format(
            owner_repo, branch, workflow_path
        ),
        "",
    ]
    lines.extend(_block("defined by ci.yml:", defined))
    lines.append("")
    lines.extend(_block("required by main:", required))

    if missing or stale:
        lines.append("")
        lines.extend(_block("missing (defined, not required):", missing))
        lines.append("")
        lines.extend(_block("stale (required, not defined):", stale))
        lines.append("")
        lines.append(REMEDIES)

    return "\n".join(lines)


def _unchecked(message):
    """Render a 75 and return it. One vocabulary, never mistakable for drift."""
    print("UNCHECKED: {0}".format(message))
    print(
        "\nThe required checks were NOT compared. This is NOT evidence that they "
        "match ci.yml — nothing was learned either way (exit {0} = EX_TEMPFAIL).".format(
            EXIT_UNCHECKED
        )
    )
    return EXIT_UNCHECKED


def main(argv, fetch=None):
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
        args = parser.parse_args(argv)

        # ADR-608-5: imported here, not at module scope, so an absent PyYAML is a
        # 75 naming its remedy rather than an ImportError traceback on exit 1 —
        # which is DRIFT, and would send the reader to the forge.
        try:
            import yaml
        except ImportError as err:
            return _unchecked(
                "PyYAML is not importable ({0}), so {1} could not be parsed. "
                "Install it with:\n"
                "    python3 -m pip install --user '{2}'".format(
                    err, args.workflow, PYYAML_PIN
                )
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
                "could not read the workflow {0}: {1} ({2})".format(
                    args.workflow, type(err).__name__, err
                )
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
            render(
                defined,
                required,
                missing,
                stale,
                args.workflow,
                args.repo,
                args.branch,
            )
        )
        return EXIT_DRIFT if (missing or stale) else EXIT_PARITY

    except Underivable as err:
        return _unchecked(
            "{0}. The derivation rule is stated in this file's header; an "
            "underivable job is never dropped from the comparison, because a job "
            "silently excluded is a job silently unrequired (BR-3).".format(err)
        )
    except Unverified as err:
        return _unchecked(str(err))
    except Exception as err:  # noqa: BLE001 — deliberate, see below
        # LESSON-442. Python exits 1 on an uncaught exception and 1 is DRIFT, so
        # without this every crash in this tool would be reported to CI as branch
        # protection drift. The traceback is kept — it is the only way to fix the
        # crash — but the verdict is UNCHECKED.
        traceback.print_exc()
        return _unchecked(
            "{0} failed unexpectedly with {1}: {2} (traceback above). A crash is "
            "not a disagreement.".format(sys.argv[0], type(err).__name__, err)
        )


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
