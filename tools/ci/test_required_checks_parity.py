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

# The eight contexts ci.yml defines today, in declaration order: `check`'s two
# matrix legs, then gated, catalog, e2e, audit, tooling, parity. This list is a
# fixture of the current tree, not a second source of truth: the derivation
# under test parses ci.yml, so adding a job to that file reds the suite here
# until this literal is updated to match — which is the intended friction, and
# is why the runbook in conventions.md (ADR-608-8) treats a job add or rename
# as one coordinated change.
#
# The last entry is the parity job's own context, added by TASK-357 in the same
# commit that added the job. The check is required to see itself (ADR-608-1):
# if an admin ever un-requires it, it reports its own name under `missing` on
# every PR rather than going quiet.
CONTEXTS_TODAY = [
    "fmt · clippy · test (ubuntu-latest)",
    "fmt · clippy · test (macos-latest)",
    "feature-gated targets compile (all features)",
    "catalog integrity (BR-8/AC-8)",
    "acceptance suite (REQ-544 + REQ-547)",
    "dependency advisories (cargo audit)",
    "release tooling (actionlint · shellcheck · selftest)",
    "required checks mirror ci.yml (REQ-608)",
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
    rules_body=None,
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
            if rules_body is not None:
                return rules_status, rules_body
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
        actually compared with. The expectation is `CONTEXTS_TODAY` — the eight
        contexts the tree defines as of TASK-357 — and it grows with the next
        job added there, deliberately: see that literal's comment.
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

    def test_path_filter_warns_under_yaml_true_key(self):
        """BR-7 / ADR-608-4: `on.pull_request.paths` is announced, exit unchanged.

        YAML 1.1 parses the bare key `on` as the boolean `True`, so this case
        is also the guard on `_triggers`. Mutation executed 2026-09-02 during
        verify: with the lookup reduced to `workflow.get("on")` alone the whole
        suite stayed green — the reflector's finding — and this case was
        written to close it. Re-run under that mutation it fails on:

            AssertionError: '::warning title=Path-filtered workflow::' not
            found in '...'

        Restored, it passes. The exit code stays PARITY: reding on a path
        filter is a policy this REQ did not decide.
        """
        text = MINIMAL_WORKFLOW.replace(
            "    branches: [main]\n", "    branches: [main]\n    paths: ['src/**']\n"
        )
        path = self.write_workflow(text)
        with open(path, "r", encoding="utf-8") as handle:
            parsed = yaml.safe_load(handle)
        self.assertIn(True, parsed, "PyYAML must resolve bare `on` to True for this case to mean anything")

        code, output = self.run_main(path, fake_fetch(["alpha check", "beta check"]))

        self.assertEqual(parity.EXIT_PARITY, code)
        self.assertIn("::warning title=Path-filtered workflow::", output)
        self.assertIn("`paths:`", output)

    def test_job_without_name_uses_its_key(self):
        """BR-3: a job with no `name:` derives its job key as the context.

        Mutation executed 2026-09-02: `context = str(job_key)` -> `"UNNAMED"`
        left the suite green (reflector). Under that mutation this case fails
        on `AssertionError: ['alpha check', 'beta'] != ['alpha check', 'UNNAMED']`.
        """
        text = MINIMAL_WORKFLOW.replace("    name: beta check\n", "")
        derived = parity.derive_contexts(yaml.safe_load(text))
        self.assertEqual(["alpha check", "beta"], derived)

    def test_expression_name_is_underivable(self):
        """BR-3: `name:` carrying `${{ }}` is refused by name, exit 75.

        Mutation executed 2026-09-02: `elif "${{" in name:` -> `elif False:`
        left the suite green (reflector). Under it this case reports DRIFT
        (exit 1) for a context that can never match — the confusion LESSON-442
        exists to prevent — and fails on `AssertionError: 75 != 1`.
        """
        text = MINIMAL_WORKFLOW.replace(
            "    name: beta check\n", "    name: beta ${{ github.ref_name }}\n"
        )
        path = self.write_workflow(text)
        code, output = self.run_main(path, fake_fetch(["alpha check"]))

        self.assertEqual(parity.EXIT_UNCHECKED, code)
        self.assertIn("'beta'", output)
        self.assertIn("expression", output)

    def test_boolean_and_float_matrix_values_are_underivable(self):
        """BR-3 / ADR-608-4: `[true, false]` and `[3.10]` are refused, not rendered.

        `str(True)` is `'True'` where GitHub writes `true`; `3.10` parses to
        `3.1`. Both would produce a permanent bogus DRIFT. Mutation executed
        2026-09-02: `_scalar` returning `str(value)` for a bool left the suite
        green (reflector); under it this case fails on `AssertionError: 75 != 1`.
        """
        for values in ("[true, false]", "[3.10, 3.9]"):
            with self.subTest(values=values):
                text = MINIMAL_WORKFLOW.replace(
                    "    name: beta check\n",
                    f"    name: beta check\n    strategy:\n      matrix:\n        flag: {values}\n",
                )
                path = self.write_workflow(text)
                code, output = self.run_main(
                    path, fake_fetch(["alpha check", "beta check (True)", "beta check (3.1)"])
                )
                self.assertEqual(parity.EXIT_UNCHECKED, code)
                self.assertIn("'beta'", output)
                self.assertIn("strategy.matrix.flag", output)

    def test_matrix_include_is_underivable(self):
        """BR-3: a matrix carrying `include:` is refused by name, exit 75."""
        text = MINIMAL_WORKFLOW.replace(
            "    name: beta check\n",
            "    name: beta check\n    strategy:\n      matrix:\n        os: [ubuntu-latest]\n        include:\n          - os: macos-latest\n",
        )
        path = self.write_workflow(text)
        code, output = self.run_main(path, fake_fetch([]))

        self.assertEqual(parity.EXIT_UNCHECKED, code)
        self.assertIn("`include:`", output)

    def test_reusable_workflow_job_is_underivable(self):
        """BR-3: a `uses:` job is refused — the forge names its check runs
        `<caller> / <callee job>`, one per callee job, and this check does not
        read the callee.

        Mutation executed 2026-09-02 (verify round 1, before the refusal
        existed): `derive_contexts` on this fixture returned `['alpha check',
        'beta check']` and the run reached DRIFT for a context that can never
        report. Under that code this case fails on `AssertionError: 75 != 1`.
        """
        text = MINIMAL_WORKFLOW.replace(
            "  beta:\n    name: beta check\n    runs-on: ubuntu-latest\n    steps:\n      - run: 'true'\n",
            "  beta:\n    name: beta check\n    uses: ./.github/workflows/other.yml\n",
        )
        path = self.write_workflow(text)
        code, output = self.run_main(path, fake_fetch(["alpha check", "beta check"]))

        self.assertEqual(parity.EXIT_UNCHECKED, code)
        self.assertIn("reusable workflow", output)
        self.assertIn("'beta'", output)

    def test_duplicate_contexts_are_underivable(self):
        """BR-3 / BR-4: two jobs deriving one context are refused together.

        `compare` works on sets, so before this refusal `["same", "same"]`
        against `["same"]` was exit 0 — a defined job reported green with no
        required context of its own, BUG-167's shape surviving the fix. Both
        job keys are named.

        Mutation executed 2026-09-02 (verify round 2, test-auditor): the
        duplicate `Underivable` raise removed. This case fails on
        `AssertionError: 75 != 0`. Restored, green.
        """
        text = MINIMAL_WORKFLOW.replace("    name: beta check\n", "    name: alpha check\n")
        path = self.write_workflow(text)
        code, output = self.run_main(path, fake_fetch(["alpha check"]))

        self.assertEqual(parity.EXIT_UNCHECKED, code)
        self.assertIn("'alpha'", output)
        self.assertIn("'beta'", output)
        self.assertIn("already derives", output)

    def test_control_characters_in_names_cannot_forge_a_workflow_command(self):
        """Security (verify finding, 2026-09-02): a newline inside a name is
        escaped on render, never printed raw. GitHub Actions parses `::error::`
        at the start of any output line, so a required context — or a job
        `name:` — carrying `\\n::notice ...` could forge an annotation. The
        defined side is refused outright; the required side is rendered with
        the control character spelled out.
        """
        forged = "gamma check\n::notice title=forged::PARITY"
        path = self.write_workflow(MINIMAL_WORKFLOW)
        code, output = self.run_main(path, fake_fetch(["alpha check", "beta check", forged]))

        self.assertEqual(parity.EXIT_DRIFT, code)
        self.assertNotIn("\n::notice", output)
        self.assertIn("gamma check\\x0a::notice", output)

        text = MINIMAL_WORKFLOW.replace(
            "    name: beta check\n", '    name: "beta check\\n::notice title=forged::PARITY"\n'
        )
        path = self.write_workflow(text)
        code, output = self.run_main(path, fake_fetch(["alpha check"]))
        self.assertEqual(parity.EXIT_UNCHECKED, code)
        self.assertNotIn("\n::notice", output)


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

        Mutation executed 2026-09-02 (verify): `REMEDIES` replaced with `""`.
        All three sub-tests went red on `AssertionError: 'revert the protection
        edit' not found in ...` while the exit code stayed 1 — the verdict
        survives the mutation, the remedies do not, which is what this case
        guards. Restored, green.
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

        Mutation executed 2026-09-02 (verify): `_get_json`'s non-2xx branch
        replaced with `pass` (the body then fails JSON parsing on the fake's
        `Bad credentials` object only by accident, so the fake was also given a
        JSON body). The case went red on `AssertionError: 75 != 1` — a 401
        rendered as DRIFT, the exact inversion BR-5 forbids. Restored, green.
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
        # Rulesets are read first (ADR-608-2), so the first read to break is
        # the one named; both URLs share this prefix.
        self.assertIn("https://api.github.com/repos/atelier-fashion/teton-code/", output)
        self.assertIn("transport failure", output)

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

    def test_rulesets_read_failure_fails_closed(self):
        """BR-5 / ADR-608-2: a failed *rulesets* read is 75, distinct from
        "rulesets present". "Could not check" must never be downgraded to "no
        rulesets" (test-auditor finding, 2026-09-02: this row had no case).
        """
        code, output = self.run_main(
            CI_YML, fake_fetch(CONTEXTS_TODAY, rules_status=500, rules_body="{}")
        )
        self.assertEqual(parity.EXIT_UNCHECKED, code)
        self.assertIn("500", output)
        self.assertIn("/rules/branches/main", output)
        self.assertNotIn("rulesets present", output)

    def test_rulesets_non_list_body_fails_closed(self):
        """BR-5: a 200 whose body is not the documented list is 75.

        Verify finding, 2026-09-02: `if isinstance(rules, list) and rules:` let
        `{"message": "Not Found"}` through as "no rulesets" and the run reached
        a verdict. Under that code this case fails on `AssertionError: 75 != 0`.
        """
        code, output = self.run_main(
            CI_YML, fake_fetch(CONTEXTS_TODAY, rules_body=json.dumps({"message": "Not Found"}))
        )
        self.assertEqual(parity.EXIT_UNCHECKED, code)
        self.assertIn("did not return the list", output)

    def test_rulesets_are_read_before_classic_protection(self):
        """ADR-608-2: a repository that moved to rulesets is told so, not told
        its branch is unprotected (reflector finding, 2026-09-02).
        """
        code, output = self.run_main(
            CI_YML,
            fake_fetch([], protected=False, rules=[{"id": 1, "type": "required_status_checks"}]),
        )
        self.assertEqual(parity.EXIT_UNCHECKED, code)
        self.assertIn("rulesets present", output)
        self.assertNotIn("not protected", output)

    def test_non_json_body_fails_closed(self):
        """BR-5 / ADR-608-2: a 200 with a non-JSON body is 75 naming the URL."""
        code, output = self.run_main(
            CI_YML, fake_fetch([], branch_body="<html>rate limited</html>")
        )
        self.assertEqual(parity.EXIT_UNCHECKED, code)
        self.assertIn("not JSON", output)
        self.assertIn(BRANCH_URL, output)

    def test_required_status_checks_absent_fails_closed(self):
        """BR-5 / ADR-608-2: `protected: true` with no `required_status_checks` is 75."""
        body = json.dumps({"name": "main", "protected": True, "protection": {"enabled": True}})
        code, output = self.run_main(CI_YML, fake_fetch([], branch_body=body))
        self.assertEqual(parity.EXIT_UNCHECKED, code)
        self.assertIn("required_status_checks", output)

    def test_protected_key_absent_fails_closed(self):
        """BR-5: a branch object with no `protected` key at all is 75, not a pass."""
        body = json.dumps({"name": "main"})
        code, output = self.run_main(CI_YML, fake_fetch([], branch_body=body))
        self.assertEqual(parity.EXIT_UNCHECKED, code)
        self.assertIn("not protected", output)

    def test_workflow_file_missing_fails_closed(self):
        """BR-5: an absent `--workflow` path is 75 naming the path."""
        code, output = self.run_main("/nonexistent/REQ-608/ci.yml", fake_fetch([]))
        self.assertEqual(parity.EXIT_UNCHECKED, code)
        self.assertIn("/nonexistent/REQ-608/ci.yml", output)
        self.assertIn("could not read the workflow", output)

    def test_unparseable_yaml_fails_closed(self):
        """BR-5: a workflow that does not parse is 75 naming the parse error."""
        path = self.write_workflow("jobs: [\n  unterminated\n")
        code, output = self.run_main(path, fake_fetch([]))
        self.assertEqual(parity.EXIT_UNCHECKED, code)
        self.assertIn("could not read the workflow", output)
        self.assertIn("Error", output)

    def test_bad_repo_or_branch_input_fails_closed(self):
        """BR-5 / LESSON-008: `--repo` and `--branch` are validated before any
        read. A repo with an extra path segment, whitespace, or a `.`/`..`
        segment is refused; a branch that is empty, padded, or carries a
        control character is refused. `?` and `/` in a branch are legitimate
        and are percent-quoted, not refused (`release/1.2` is a real branch).
        The fake records its calls so the case also proves the validator runs
        before, not after, the read.
        """
        calls = []

        def recording_fetch(url, token):
            calls.append(url)
            return fake_fetch([])(url, token)

        bad = (
            ("atelier-fashion/teton-code/extra", "main"),
            ("a b/c", "main"),
            ("../..", "main"),
            ("./repo", "main"),
            ("owner/..", "main"),
            (REPO, ""),
            (REPO, " main"),
            (REPO, "main\n"),
        )
        for repo, branch in bad:
            with self.subTest(repo=repo, branch=branch):
                code, output = self.run_main(CI_YML, recording_fetch, branch=branch, repo=repo)
                self.assertEqual(parity.EXIT_UNCHECKED, code)
        self.assertEqual([], calls, "the validator must refuse before any read")

        # A legitimate branch name is quoted, read, and compared (the fake
        # requires nothing, so the verdict is DRIFT — the point is the URL).
        code, _ = self.run_main(CI_YML, recording_fetch, branch="release/1.2?x=1")
        self.assertEqual(parity.EXIT_DRIFT, code)
        self.assertTrue(any("branches/release%2F1.2%3Fx%3D1" in url for url in calls))

    def test_non_string_required_context_fails_closed(self):
        """BR-5 (verify round 2, Major): a forge body whose `contexts` holds a
        non-string is 75, never DRIFT. Before this guard `[{"context": "a"},
        null, 5]` was stringified and compared, and the run exited 1 — an
        uninterpretable read reported as a disagreement.
        """
        body = json.dumps({"name": "main", "protected": True, "protection": {
            "required_status_checks": {"contexts": [{"context": "a"}, None, 5]}}})
        code, output = self.run_main(CI_YML, fake_fetch([], branch_body=body))
        self.assertEqual(parity.EXIT_UNCHECKED, code)
        self.assertIn("not a string", output)
        self.assertNotIn("missing (defined, not required):", output)

    def test_remaining_refusal_branches_are_named(self):
        """BR-3 / BR-5 sweep (verify round 2, test-auditor's uncovered list):
        every refusal branch the script has, exercised once each, asserting
        75 and the text that names the cause. Conventions.md: an invariant
        with more than one enforcement point needs a sweep, not a fix.
        """
        minimal = yaml.safe_load(MINIMAL_WORKFLOW)

        def with_beta(**fields):
            doc = json.loads(json.dumps(minimal))
            doc["jobs"]["beta"] = {**doc["jobs"]["beta"], **fields}
            return doc

        derivation_cases = {
            "workflow not a mapping": (["not", "a", "mapping"], parity.Unverified, "did not parse to a mapping"),
            "no jobs mapping": ({"name": "x"}, parity.Unverified, "no `jobs:` mapping"),
            "job is null": ({**minimal, "jobs": {"beta": None}}, parity.Underivable, "not a mapping"),
            "name is an int": (with_beta(name=7), parity.Underivable, "not a string"),
            "strategy not a mapping": (with_beta(strategy="x"), parity.Underivable, "`strategy:` is a str"),
            "matrix is an expression": (with_beta(strategy={"matrix": "${{ fromJSON(needs.x.outputs.m) }}"}), parity.Underivable, "expression"),
            "matrix not a mapping": (with_beta(strategy={"matrix": [1]}), parity.Underivable, "not a mapping"),
            "dimension not a list": (with_beta(strategy={"matrix": {"os": "ubuntu-latest"}}), parity.Underivable, "non-empty list"),
            "dimension empty": (with_beta(strategy={"matrix": {"os": []}}), parity.Underivable, "non-empty list"),
        }
        for label, (doc, exc, text) in derivation_cases.items():
            with self.subTest(case=label):
                with self.assertRaises(exc) as ctx:
                    parity.derive_contexts(doc)
                self.assertIn(text, str(ctx.exception))

        with self.subTest(case="integer matrix values render"):
            doc = with_beta(strategy={"matrix": {"n": [1, 2]}})
            self.assertEqual(["alpha check", "beta check (1)", "beta check (2)"], parity.derive_contexts(doc))

        read_cases = {
            "branch payload not an object": (json.dumps([1]), "did not return a branch object"),
            "protection not an object": (json.dumps({"protected": True, "protection": "yes"}), "no `protection` object"),
            "contexts key absent": (json.dumps({"protected": True, "protection": {"required_status_checks": {"strict": True}}}), "`protection.required_status_checks.contexts` is absent"),
        }
        for label, (body, text) in read_cases.items():
            with self.subTest(case=label):
                code, output = self.run_main(CI_YML, fake_fetch([], branch_body=body))
                self.assertEqual(parity.EXIT_UNCHECKED, code)
                self.assertIn(text, output)

    def test_escaping_is_injective(self):
        """Security (verify round 2): a name holding the literal text `a\\x0ab`
        and a name holding a real line feed render differently, and the
        Unicode line separators are escaped like the ASCII ones.
        """
        self.assertNotEqual(parity._safe("a\\x0ab"), parity._safe("a\nb"))
        self.assertEqual("a\\\\x0ab", parity._safe("a\\x0ab"))
        self.assertEqual("a\\u2028b", parity._safe("a\u2028b"))
        self.assertEqual("a\\x85b", parity._safe("a\x85b"))
        self.assertFalse(parity._plain_name("a\u2029b"))

    def test_pyyaml_pin_flag_has_one_home(self):
        """ADR-608-5: `--pyyaml-pin` prints the requirement the CI job installs,
        so the pin is not restated in ci.yml (quality finding, 2026-09-02).
        """
        buffer = io.StringIO()
        with contextlib.redirect_stdout(buffer):
            code = parity.main(["--pyyaml-pin"], fetch=fake_fetch([]))
        self.assertEqual(parity.EXIT_PARITY, code)
        self.assertEqual(parity.PYYAML_PIN + "\n", buffer.getvalue())
        with open(CI_YML, "r", encoding="utf-8") as handle:
            self.assertIn("--pyyaml-pin", handle.read())

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
