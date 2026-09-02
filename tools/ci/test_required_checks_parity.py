#!/usr/bin/env python3
"""Known-bads for `required-checks-parity.py` (REQ-608 BR-6, ADR-608-6).

Every row of ADR-608-6's mutation table is a case here, and every case asserts
on the script's **rendered output** and its return value — never on an internal
structure. AC-10 says so explicitly, and LESSON-519 is why: asserting that the
source contains a message proves the string exists, not that a reader ever sees
it.

No test opens a socket. `main(argv, fetch=...)` takes the fetcher as an
argument (ADR-608-6's seam), so the only network code in this file is the fake
that never leaves the process. Grep it: there is no `urlopen` here.

Each docstring names the rule it discharges and — for the mutations that guard
the comparison itself — what actually went red when the mutation was first run,
because a green assertion nobody has seen fail is not evidence (conventions.md,
LESSON-569).

Run:  python3 -m unittest tools/ci/test_required_checks_parity.py -v
"""

import contextlib
import importlib.util
import io
import json
import os
import tempfile
import unittest

# The one non-stdlib import, and it is deliberate rather than incidental. The
# script under test needs PyYAML too, but it *handles* a missing one by
# returning 75 — which is the same code six cases here assert. Importing it hard
# at module scope means an environment without PyYAML fails loudly instead of
# passing this suite for the wrong reason.
import yaml

HERE = os.path.dirname(os.path.abspath(__file__))
REPO_ROOT = os.path.dirname(os.path.dirname(HERE))
CI_YML = os.path.join(REPO_ROOT, ".github", "workflows", "ci.yml")
SCRIPT = os.path.join(HERE, "required-checks-parity.py")

# The script's filename is hyphenated (it is invoked as `python3 tools/ci/...`,
# matching refresh-catalog.py), which is not an importable module name. Load it
# by path rather than renaming a file the CI job calls by name.
_spec = importlib.util.spec_from_file_location("required_checks_parity", SCRIPT)
parity = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(parity)

REPO = "atelier-fashion/teton-code"
BRANCH_URL = "https://api.github.com/repos/atelier-fashion/teton-code/branches/main"

# The seven contexts ci.yml defines today, in declaration order: `check`'s two
# matrix legs, then gated, catalog, e2e, audit, tooling. This list is a fixture
# of the current tree, not a second source of truth — TASK-357 adds an eighth
# (the parity job itself) and this literal is expected to grow with it.
CONTEXTS_TODAY = [
    "fmt · clippy · test (ubuntu-latest)",
    "fmt · clippy · test (macos-latest)",
    "feature-gated targets compile (all features)",
    "catalog integrity (BR-8/AC-8)",
    "acceptance suite (REQ-544 + REQ-547)",
    "dependency advisories (cargo audit)",
    "release tooling (actionlint · shellcheck · selftest)",
]

MINIMAL_WORKFLOW = """\
name: Fixture
on:
  pull_request:
    branches: [main]
jobs:
  alpha:
    name: alpha check
    runs-on: ubuntu-latest
    steps:
      - run: 'true'
  beta:
    name: beta check
    runs-on: ubuntu-latest
    steps:
      - run: 'true'
"""

TWO_DIMENSION_WORKFLOW = """\
name: Fixture
on:
  pull_request:
    branches: [main]
jobs:
  spread:
    name: spread
    strategy:
      matrix:
        os: [ubuntu-latest, macos-latest]
        toolchain: [stable, nightly]
    runs-on: ${{ matrix.os }}
    steps:
      - run: 'true'
"""

THROWAWAY_JOB = """
  throwaway:
    name: throwaway job (REQ-608 AC-4 known-bad)
    runs-on: ubuntu-latest
    steps:
      - run: 'true'
"""


def fake_fetch(
    contexts,
    protected=True,
    branch_status=200,
    branch_body=None,
    rules=None,
    rules_status=200,
    raises=None,
):
    """Build a `fetch(url, token)` that answers from memory.

    Distinguishes the two endpoints by path: `/rules/branches/` is the rulesets
    read, anything else is the branch read.
    """

    def _fetch(url, token):
        if raises is not None:
            raise raises
        if "/rules/branches/" in url:
            return rules_status, json.dumps(rules if rules is not None else [])
        if branch_body is not None:
            return branch_status, branch_body
        payload = {
            "name": "main",
            "protected": protected,
            "protection": {
                "required_status_checks": {
                    "enforcement_level": "non_admins",
                    "contexts": list(contexts),
                }
            },
        }
        return branch_status, json.dumps(payload)

    return _fetch


class ParityCase(unittest.TestCase):
    def run_main(self, workflow_path, fetch, branch="main", repo=REPO):
        """Run `main` with stdout captured. Returns `(exit_code, output)`.

        stderr is captured separately into `self.last_stderr` — the top-level
        handler prints a traceback there by design, and letting it into the
        runner's output would make a green suite look like a broken one.
        """
        buffer = io.StringIO()
        errors = io.StringIO()
        argv = ["--workflow", workflow_path, "--repo", repo, "--branch", branch]
        with contextlib.redirect_stdout(buffer):
            with contextlib.redirect_stderr(errors):
                code = parity.main(argv, fetch=fetch)
        self.last_stderr = errors.getvalue()
        return code, buffer.getvalue()

    def write_workflow(self, text):
        handle = tempfile.NamedTemporaryFile(
            "w", suffix=".yml", delete=False, encoding="utf-8"
        )
        handle.write(text)
        handle.close()
        self.addCleanup(os.unlink, handle.name)
        return handle.name

    def assertBothRemedies(self, output):
        self.assertIn("revert the protection edit", output)
        self.assertIn("update .github/workflows/ci.yml", output)
        self.assertIn("Two ways to resolve this, pick the one that matches intent:", output)


class TestDerivation(ParityCase):
    def test_matrix_expands_single_dimension(self):
        """BR-3 / AC-4: a single-dimension matrix yields one context per value.

        Asserted against the real `.github/workflows/ci.yml`, not a fixture, so
        the derivation is checked against the document branch protection is
        actually compared with. The seven-context expectation is the tree as it
        stands; TASK-357 adds the parity job's own context and this list grows.
        """
        with open(CI_YML, "r", encoding="utf-8") as handle:
            workflow = yaml.safe_load(handle)

        derived = parity.derive_contexts(workflow)

        self.assertIn("fmt · clippy · test (ubuntu-latest)", derived)
        self.assertIn("fmt · clippy · test (macos-latest)", derived)
        # Declaration order, matrix legs in list order (ADR-608-4).
        self.assertEqual(CONTEXTS_TODAY, derived)

    def test_multi_dimension_matrix_is_underivable(self):
        """BR-3: a two-dimension matrix is refused by name, exit 75, never guessed.

        The rule derives single-dimension matrices only; a cross product's
        rendering is not stated by ADR-608-4, and a parser that guessed would
        work now and drift silently. The job key must appear so a reader can
        tell a stated limitation from a bug.
        """
        path = self.write_workflow(TWO_DIMENSION_WORKFLOW)
        code, output = self.run_main(path, fake_fetch([]))

        self.assertEqual(parity.EXIT_UNCHECKED, code)
        self.assertIn("spread", output)
        self.assertIn("2 dimensions", output)
        self.assertNotIn("missing (defined, not required):", output)

    def test_conditional_job_warns_without_changing_the_verdict(self):
        """BR-7 / ADR-608-4: `if:` is a `::warning::`, not a failure.

        Under BR-4 every defined job is required, so a job that skips leaves a
        required context waiting forever. That hazard is announced; the exit
        code stays whatever the comparison says, because reding every run on a
        conditional job is a policy this REQ did not decide.
        """
        text = MINIMAL_WORKFLOW.replace(
            "  beta:\n    name: beta check\n",
            "  beta:\n    name: beta check\n    if: github.event_name == 'push'\n",
        )
        path = self.write_workflow(text)
        code, output = self.run_main(path, fake_fetch(["alpha check", "beta check"]))

        self.assertEqual(parity.EXIT_PARITY, code)
        self.assertIn("::warning title=Conditional job::", output)
        self.assertIn("'beta'", output)


class TestComparison(ParityCase):
    def test_parity_passes(self):
        """BR-2 / BR-4 / BR-5 benign path: equal sets exit 0 and render both.

        The fake required set is the real ci.yml's derived set, so this is the
        row ADR-608-6 marks **benign**. It is the case that proves the failing
        rows fail for the reason claimed rather than because the harness reds on
        everything.
        """
        code, output = self.run_main(CI_YML, fake_fetch(CONTEXTS_TODAY))

        self.assertEqual(parity.EXIT_PARITY, code)
        self.assertIn("defined by ci.yml:", output)
        self.assertIn("required by main:", output)
        for context in CONTEXTS_TODAY:
            self.assertIn(context, output)
        self.assertNotIn("missing (defined, not required):", output)
        self.assertNotIn("Two ways to resolve this", output)

    def test_missing_job_fails(self):
        """BR-2: a job ci.yml defines that protection does not require is exit 1.

        REQ-608's own defect in miniature: the job reports on every PR and can
        block none of them.
        """
        path = self.write_workflow(MINIMAL_WORKFLOW)
        code, output = self.run_main(path, fake_fetch(["alpha check"]))

        self.assertEqual(parity.EXIT_DRIFT, code)
        self.assertIn("missing (defined, not required):", output)
        self.assertIn("  - beta check", output)
        self.assertBothRemedies(output)

    def test_stale_context_fails(self):
        """BR-4: a required context nothing defines is exit 1, the mirror direction.

        Such a context sits at "Expected — waiting for status to be reported"
        and blocks every merge. The check adds the diagnosis, not the blocking.
        """
        path = self.write_workflow(MINIMAL_WORKFLOW)
        code, output = self.run_main(
            path, fake_fetch(["alpha check", "beta check", "gamma check"])
        )

        self.assertEqual(parity.EXIT_DRIFT, code)
        self.assertIn("stale (required, not defined):", output)
        self.assertIn("  - gamma check", output)
        self.assertBothRemedies(output)

    def test_deleting_a_required_context_goes_red(self):
        """BR-6 / AC-6 known-bad, executed: drop one context from the required set.

        The real ci.yml against its own derived set minus `feature-gated targets
        compile (all features)` — the actual BUG-167 shape, reproduced from the
        protection side.

        Mutation executed 2026-09-02, not reasoned about. `compare()` in
        required-checks-parity.py was replaced with `return set(), set()` and
        this case went red twice over:

            AssertionError: 1 != 0
              at self.assertEqual(parity.EXIT_DRIFT, code)

        and, with that first line commented out so the text assertions could be
        reached:

            AssertionError: 'missing (defined, not required):' not found in
            '...defined by ci.yml:\\n  - ...\\n  - feature-gated targets
            compile (all features)\\n...required by main:\\n...'

        i.e. the crippled run still printed both sets — with `feature-gated
        targets compile (all features)` visibly present in one and absent from
        the other — and rendered neither the `missing` block nor the remedies.
        `compare()` restored, the case passes.
        """
        required = [c for c in CONTEXTS_TODAY if not c.startswith("feature-gated")]
        code, output = self.run_main(CI_YML, fake_fetch(required))

        self.assertEqual(parity.EXIT_DRIFT, code)
        self.assertIn("missing (defined, not required):", output)
        self.assertIn("  - feature-gated targets compile (all features)", output)
        self.assertIn("stale (required, not defined):", output)
        self.assertBothRemedies(output)

    def test_added_job_in_fixture_goes_red(self):
        """AC-4: adding a job to a copy of ci.yml reds the check with no edit to it.

        The derivation is a real parse, so a new job appears in `missing`
        without anyone updating a second list. Asserted by mutating a fixture
        copy of the real workflow, not by reasoning about the parser.

        Mutation executed 2026-09-02 alongside the case above, with the same
        `compare()` -> `return set(), set()` cripple. Red first on:

            AssertionError: 1 != 0
              at self.assertEqual(parity.EXIT_DRIFT, code)

        and then, with the exit assertion commented out:

            AssertionError: 'missing (defined, not required):' not found in
            '...defined by ci.yml:\\n  - ...\\n  - throwaway job (REQ-608 AC-4
            known-bad)\\n\\nrequired by main:\\n  - ...'

        The derived set had picked the throwaway job up from the fixture
        unaided — that half is the parser working — while the comparison that
        turns it into a verdict had been removed. Restored, the case passes.
        """
        with open(CI_YML, "r", encoding="utf-8") as handle:
            text = handle.read()
        path = self.write_workflow(text.rstrip("\n") + "\n" + THROWAWAY_JOB)

        code, output = self.run_main(path, fake_fetch(CONTEXTS_TODAY))

        self.assertEqual(parity.EXIT_DRIFT, code)
        self.assertIn("missing (defined, not required):", output)
        self.assertIn("throwaway job (REQ-608 AC-4 known-bad)", output)
        self.assertBothRemedies(output)

    def test_both_directions_name_both_remedies(self):
        """BR-9 / AC-3 / AC-10: every failure direction names both remedies.

        Three renderings are checked — missing only, stale only, and both at
        once — because BR-9's whole point is that a reader hitting a repo-wide
        red must not have to work out which edit resolves it. Asserted on the
        rendered text, never on the source of the message (LESSON-519).
        """
        path = self.write_workflow(MINIMAL_WORKFLOW)

        cases = {
            "missing only": ["alpha check"],
            "stale only": ["alpha check", "beta check", "gamma check"],
            "both at once": ["alpha check", "gamma check"],
        }
        for label, required in cases.items():
            with self.subTest(direction=label):
                code, output = self.run_main(path, fake_fetch(required))
                self.assertEqual(parity.EXIT_DRIFT, code)
                self.assertBothRemedies(output)
                self.assertIn("missing (defined, not required):", output)
                self.assertIn("stale (required, not defined):", output)


class TestFailsClosed(ParityCase):
    def test_read_401_fails_closed(self):
        """BR-5 / AC-5: a rejected credential is 75, naming the status and the URL.

        "Could not read" must never be rendered as "matches" (LESSON-510). The
        URL is in the message so the next reader can reproduce the read.
        """
        code, output = self.run_main(
            CI_YML,
            fake_fetch(
                [], branch_status=401, branch_body=json.dumps({"message": "Bad credentials"})
            ),
        )

        self.assertEqual(parity.EXIT_UNCHECKED, code)
        self.assertIn("401", output)
        self.assertIn(BRANCH_URL, output)
        self.assertIn("NOT evidence that they match", output)

    def test_unprotected_branch_fails_closed(self):
        """BR-5: `protected: false` is 75 "not protected", never a silent pass.

        An unprotected branch has an empty required set, which would compare as
        "everything is missing" — technically true and useless. The condition is
        named instead.
        """
        code, output = self.run_main(CI_YML, fake_fetch([], protected=False))

        self.assertEqual(parity.EXIT_UNCHECKED, code)
        self.assertIn("not protected", output)

    def test_rulesets_present_fails_closed(self):
        """BR-5: a non-empty ruleset list is 75 — classic protection is not the whole truth.

        Rulesets are detected, not parsed (ADR-608-2, LESSON-460): no parser is
        written against a shape this repository has never produced. A partial
        answer must not be rendered as a verdict.
        """
        code, output = self.run_main(
            CI_YML,
            fake_fetch(CONTEXTS_TODAY, rules=[{"id": 1, "type": "required_status_checks"}]),
        )

        self.assertEqual(parity.EXIT_UNCHECKED, code)
        self.assertIn("rulesets", output)
        self.assertNotIn("missing (defined, not required):", output)

    def test_transport_error_fails_closed(self):
        """BR-5 / LESSON-442: a dropped connection is 75 naming the class, not drift.

        `OSError` covers urllib's URLError, TimeoutError and a mid-response
        reset. None of them is evidence about branch protection, so none may
        share an exit code with a real disagreement.
        """
        code, output = self.run_main(
            CI_YML, fake_fetch([], raises=OSError("Connection reset by peer"))
        )

        self.assertEqual(parity.EXIT_UNCHECKED, code)
        self.assertIn("OSError", output)
        self.assertIn(BRANCH_URL, output)

    def test_unforeseen_exception_is_75_not_1(self):
        """AC-5 / LESSON-442: an unforeseen crash is 75, never 1.

        Python exits 1 on an uncaught exception and 1 *is* DRIFT, so without
        `main`'s top-level handler every bug in this tool would be reported to
        CI as branch-protection drift. `RuntimeError` is deliberately outside
        `TRANSPORT_ERRORS`, so it reaches that handler and nothing else.
        """
        code, output = self.run_main(
            CI_YML, fake_fetch([], raises=RuntimeError("something nobody foresaw"))
        )

        self.assertEqual(parity.EXIT_UNCHECKED, code)
        self.assertNotEqual(parity.EXIT_DRIFT, code)
        self.assertIn("RuntimeError", output)
        self.assertIn("A crash is not a disagreement", output)
        # The verdict is downgraded but the traceback is kept: it is the only
        # way to fix the crash the 75 is reporting.
        self.assertIn("Traceback", self.last_stderr)
        self.assertIn("something nobody foresaw", self.last_stderr)

    def test_missing_repo_input_fails_closed(self):
        """BR-5, ADR-608-2 row 1: no `--repo` and no `$GITHUB_REPOSITORY` is 75.

        The first row of the fail-closed table. Nothing was read, so nothing may
        be concluded.
        """
        buffer = io.StringIO()
        saved = os.environ.pop("GITHUB_REPOSITORY", None)
        try:
            with contextlib.redirect_stdout(buffer):
                code = parity.main(
                    ["--workflow", CI_YML], fetch=fake_fetch(CONTEXTS_TODAY)
                )
        finally:
            if saved is not None:
                os.environ["GITHUB_REPOSITORY"] = saved

        self.assertEqual(parity.EXIT_UNCHECKED, code)
        self.assertIn("GITHUB_REPOSITORY", buffer.getvalue())


if __name__ == "__main__":
    unittest.main()
