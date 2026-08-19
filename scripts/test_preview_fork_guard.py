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
"""

from __future__ import annotations

import unittest
from pathlib import Path

import yaml

WORKFLOW = Path(__file__).resolve().parent.parent / ".github/workflows/preview.yml"
UPSTREAM = "ogulcancelik/herdr"
# The repository this branch is delivered to. The guard must be FALSE here.
FORK = "Louranicas/herdr"


def load() -> dict:
    with WORKFLOW.open() as handle:
        return yaml.safe_load(handle)


def triggers(doc: dict) -> dict:
    # PyYAML resolves the bare key `on` to the boolean True (YAML 1.1), so the
    # trigger block hides under `True` unless it was quoted. Accept both rather
    # than depend on which.
    return doc.get("on", doc.get(True)) or {}


class PreviewForkGuard(unittest.TestCase):
    def setUp(self) -> None:
        self.doc = load()
        self.jobs = self.doc.get("jobs") or {}
        self.assertTrue(self.jobs, "the workflow defines no jobs")

    def test_no_automatic_triggers(self) -> None:
        """A push or schedule would publish without anyone deciding to."""
        fired_automatically = {"push", "pull_request", "schedule", "release"}
        present = set(triggers(self.doc))
        self.assertFalse(
            present & fired_automatically,
            f"preview publishing must not fire automatically; found {sorted(present & fired_automatically)}",
        )

    def test_every_job_carries_the_repository_guard(self) -> None:
        """At JOB level, so it governs the whole job rather than one step."""
        for name, job in self.jobs.items():
            condition = job.get("if")
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
        for name, job in self.jobs.items():
            condition = str(job.get("if"))
            # The guard's own clause, isolated from any `&&` conjunction.
            clause = next(
                (c.strip() for c in condition.split("&&") if "github.repository" in c),
                None,
            )
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

    def test_the_workflow_still_stamps_preview(self) -> None:
        """The reason the guard exists.

        If this workflow ever stopped setting `preview`, the incoherence would
        be gone and the guard could be reconsidered. Until then it must stay,
        and this records why — so a future reader does not remove the guard
        without noticing what it was protecting against.
        """
        env = self.doc.get("env") or {}
        job_env = {}
        for job in self.jobs.values():
            job_env.update(job.get("env") or {})
        channel = env.get("HERDR_BUILD_CHANNEL") or job_env.get("HERDR_BUILD_CHANNEL")
        self.assertEqual(
            channel,
            "preview",
            "this workflow no longer stamps `preview`; revisit the fork guard "
            "rather than leaving a guard whose reason has gone",
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
