"""The fork's preview-publishing policy, asserted rather than trusted.

`build.rs` on this branch stamps the fork identity unconditionally, so every
binary reports `0.8.0-heb.1`. The preview workflow sets
`HERDR_BUILD_CHANNEL=preview` and publishes metadata saying `channel=preview`
with its own build id. A preview publish from this fork would therefore ship a
binary calling itself `heb` beside release metadata calling it `preview` — two
different answers to "what am I running?", which is worse than either alone.

Removing the automatic triggers is not the policy. `workflow_dispatch`-only
still permits a manual run, and "nobody presses the button" is not a guarantee.
Every job carries a repository guard that evaluates FALSE anywhere but
upstream, so a manual run here resolves to skipped jobs and publishes nothing.

This parses the workflow into a structure and asserts meaning. A substring
search over the file would pass on a guard that had been commented out, moved
into a `steps:` block where it governs one step instead of the job, or negated.

NO THIRD-PARTY PARSER. An earlier revision imported PyYAML, which no other
script here uses and nothing provisions - so the check depended on whatever
happened to be installed on the host and would simply error on a runner without
it. The scanner below understands only the two shapes this assertion needs, and
RAISES on anything it does not recognise. A narrow parser that refuses the
unfamiliar is safe; one that guesses would hand back a confident wrong model.
"""

from __future__ import annotations

import unittest
from pathlib import Path

WORKFLOW = Path(__file__).resolve().parent.parent / ".github/workflows/preview.yml"
# The CANONICAL upstream repository. `github.repository` reports the canonical
# `owner/name` at run time, never a redirect alias, so a guard naming an alias
# is false in EVERY repository - upstream included. That reads like a guard and
# behaves like a kill switch: it would silently stop upstream publishing while
# looking exactly like a working fork guard.
UPSTREAM = "herdrdev/herdr"

# Redirect aliases this repository has answered to. GitHub keeps serving them
# after a rename, so `git remote -v` and old links still show them long after
# they stopped being what Actions reports.
KNOWN_ALIASES = frozenset({"ogulcancelik/herdr"})

# The repository this branch is delivered to. The guard must be FALSE here.
FORK = "Louranicas/herdr"


class WorkflowShapeError(AssertionError):
    """The scanner met something it does not model. Refusing beats guessing."""


def _strip_comment(line: str) -> str:
    """Remove a trailing comment, respecting quotes.

    Naive splitting on `#` would truncate `if: "a == \'x#y\'"` and, worse,
    could make a guarded job look unguarded.
    """
    out, quote = [], None
    for ch in line:
        if quote:
            out.append(ch)
            if ch == quote:
                quote = None
        elif ch in "\"'":
            quote = ch
            out.append(ch)
        elif ch == "#":
            break
        else:
            out.append(ch)
    return "".join(out).rstrip()


def scan() -> dict:
    """A deliberately small model: top-level trigger keys, and job -> `if`."""
    text = WORKFLOW.read_text()
    if "\t" in text:
        raise WorkflowShapeError("tab in the workflow; this scanner models spaces only")

    triggers: list[str] = []
    jobs: dict[str, str | None] = {}
    section = None
    current_job = None

    for raw in text.split("\n"):
        line = _strip_comment(raw)
        if not line.strip():
            continue
        indent = len(line) - len(line.lstrip(" "))
        body = line.strip()

        if indent == 0:
            # `on:` is the trigger block; `jobs:` opens the job map.
            section = "on" if body in ("on:", '"on":', "'on':") else (
                "jobs" if body == "jobs:" else None
            )
            current_job = None
            continue

        if section == "on" and indent == 2 and body.endswith(":"):
            triggers.append(body[:-1].strip().strip("\"'"))
        elif section == "jobs" and indent == 2 and body.endswith(":"):
            current_job = body[:-1].strip().strip("\"'")
            jobs[current_job] = None
        elif section == "jobs" and indent == 4 and current_job and body.startswith("if:"):
            value = body[len("if:"):].strip()
            if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
                value = value[1:-1]
            jobs[current_job] = value

    if not jobs:
        raise WorkflowShapeError("no jobs found; the scanner did not understand this file")
    return {"triggers": triggers, "jobs": jobs}


class PreviewForkGuard(unittest.TestCase):
    def setUp(self) -> None:
        self.doc = scan()
        self.jobs = self.doc["jobs"]
        self.assertTrue(self.jobs, "the workflow defines no jobs")

    def test_no_automatic_triggers(self) -> None:
        """A push or schedule would publish without anyone deciding to."""
        fired_automatically = {"push", "pull_request", "schedule", "release"}
        present = set(self.doc["triggers"])
        self.assertFalse(
            present & fired_automatically,
            f"preview publishing must not fire automatically; found {sorted(present & fired_automatically)}",
        )

    def test_every_job_carries_the_repository_guard(self) -> None:
        """At JOB level, so it governs the whole job rather than one step."""
        for name, condition in self.jobs.items():
            self.assertIsNotNone(
                condition,
                f"job {name!r} has no `if`: a manual run would execute it on any fork",
            )
            self.assertIn(
                f"github.repository == '{UPSTREAM}'",
                str(condition),
                f"job {name!r} does not require the upstream repository: {condition!r}",
            )

    def test_the_guard_is_false_for_this_fork(self) -> None:
        """The point of the guard, evaluated rather than assumed.

        A guard naming the wrong repository, or negated, would still contain the
        upstream string and pass a substring check.
        """
        for name, condition in self.jobs.items():
            condition = str(condition)
            clause = _repository_clause(condition)
            self.assertIsNotNone(clause, f"job {name!r} has no repository clause")
            self.assertFalse(
                clause.lstrip().startswith("!"),
                f"job {name!r} negates the guard, which inverts the policy: {clause!r}",
            )
            self.assertEqual(
                clause,
                f"github.repository == '{UPSTREAM}'",
                f"job {name!r} guard is not an exact upstream equality: {clause!r}",
            )
            # Evaluate the clause's truth for both repositories.
            self.assertTrue(
                _evaluate(clause, UPSTREAM), f"job {name!r} guard is false upstream"
            )
            self.assertFalse(
                _evaluate(clause, FORK),
                f"job {name!r} guard is TRUE for {FORK}; this fork would publish",
            )

    def test_the_guard_does_not_name_a_redirect_alias(self) -> None:
        """A guard naming a stale alias is false upstream too.

        `ogulcancelik/herdr` still resolves - GitHub honours the redirect - so
        the name looks live in a browser and in `git remote -v` while
        `github.repository` reports `herdrdev/herdr`. Equality against the
        alias therefore never holds anywhere, which disables publishing rather
        than restricting it. Asserting the fork case alone would not catch
        this: a guard that is false everywhere is false for the fork too.
        """
        for name, condition in self.jobs.items():
            clause = _repository_clause(str(condition))
            self.assertIsNotNone(clause, f"job {name!r} has no repository clause")
            _, _, right = str(clause).partition("==")
            named = right.strip().strip("'\"")
            self.assertNotIn(
                named,
                KNOWN_ALIASES,
                f"job {name!r} guards on the redirect alias {named!r}; "
                f"`github.repository` reports {UPSTREAM!r}, so this condition "
                f"is false in every repository and publishes nowhere",
            )

    def test_the_scanner_understood_the_expected_jobs(self) -> None:
        """If the scanner silently modelled the wrong thing, every other
        assertion here would be checking a fiction."""
        self.assertEqual(
            set(self.jobs),
            {"preflight", "build", "publish"},
            f"scanner produced {sorted(self.jobs)}; the model does not match the file",
        )

    def test_the_workflow_still_stamps_preview(self) -> None:
        """The reason the guard exists.

        If this workflow ever stopped setting `preview`, the incoherence would
        be gone and the guard could be reconsidered. Until then it must stay,
        and this records why — so a future reader does not remove the guard
        without noticing what it was protecting against.
        """
        text = WORKFLOW.read_text()
        self.assertIn(
            "HERDR_BUILD_CHANNEL: preview",
            text,
            "this workflow no longer stamps `preview`; revisit the fork guard "
            "rather than leaving a guard whose reason has gone",
        )


def _repository_clause(condition: str) -> str | None:
    """The guard's own clause, isolated from any `&&` conjunction."""
    return next(
        (c.strip() for c in condition.split("&&") if "github.repository" in c),
        None,
    )


def _evaluate(clause: str, repository: str) -> bool:
    """Evaluate `github.repository == '<x>'` for a given repository."""
    left, _, right = clause.partition("==")
    self_ref = left.strip()
    wanted = right.strip().strip("'\"")
    assert self_ref == "github.repository", self_ref
    return repository == wanted


if __name__ == "__main__":
    unittest.main()
