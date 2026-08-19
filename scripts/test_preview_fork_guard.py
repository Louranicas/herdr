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


def _unquote(value: str) -> str:
    """Drop one matched pair of surrounding quotes, if present."""
    if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
        return value[1:-1]
    return value


def _mapping_entry(body: str) -> tuple[str, str]:
    """A scalar `key: value` entry, or a refusal.

    An entry with no value opens a nested block, which this scanner does not
    model. Returning an empty string for it would hand back a confident wrong
    answer about what the workflow sets.
    """
    key, separator, value = body.partition(":")
    if not separator:
        raise WorkflowShapeError(f"not a `key: value` entry: {body!r}")
    value = value.strip()
    if not value:
        raise WorkflowShapeError(f"nested mapping under {key.strip()!r} is not modelled")
    return _unquote(key.strip()), _unquote(value)


def _trigger_names_from_inline(value: str) -> list[str]:
    """Triggers written on the `on:` line itself.

    YAML gives three ways to say the same thing, and an earlier revision of
    this scanner modelled only the block-mapping one. `on: [push]` and
    `on: push` were therefore not "unrecognised" - they were INVISIBLE. The
    trigger list came back empty and the no-automatic-triggers assertion
    passed because it had nothing to object to. A check that cannot see the
    thing it forbids is worse than no check: it reports the policy as held.
    """
    if value.startswith("[") and value.endswith("]"):
        inner = value[1:-1].strip()
        if not inner:
            return []
        return [_unquote(item.strip()) for item in inner.split(",") if item.strip()]
    if any(ch in value for ch in "[]{}:,"):
        raise WorkflowShapeError(
            f"unmodelled inline trigger syntax on the `on:` line: {value!r}"
        )
    return [_unquote(value)]


def scan(text: str | None = None) -> dict:
    """A deliberately small model: top-level trigger keys, workflow-level
    `env`, and per job its `if` and its job-level `env`.

    `text` exists so the scanner's own blind spots can be tested against
    workflows this repository does not contain. Asserting only against the
    committed file tests the scanner exactly where it happens to work.
    """
    if text is None:
        text = WORKFLOW.read_text()
    if "\t" in text:
        raise WorkflowShapeError("tab in the workflow; this scanner models spaces only")

    triggers: list[str] = []
    workflow_env: dict[str, str] = {}
    jobs: dict[str, dict] = {}
    section = None
    current_job = None
    in_job_env = False

    for raw in text.split("\n"):
        line = _strip_comment(raw)
        if not line.strip():
            continue
        indent = len(line) - len(line.lstrip(" "))
        body = line.strip()

        if indent == 0:
            top_key, _, inline = body.partition(":")
            top_key = _unquote(top_key.strip())
            # `on:` is the trigger block; `jobs:` opens the job map; `env:` is
            # the workflow-wide environment every job inherits.
            section = "on" if top_key == "on" else (
                "jobs" if top_key == "jobs" else ("env" if top_key == "env" else None)
            )
            current_job = None
            in_job_env = False
            if section == "on" and inline.strip():
                # Written inline, so there is no block to walk into.
                triggers.extend(_trigger_names_from_inline(inline.strip()))
                section = None
            continue

        if section == "on" and indent == 2:
            if body.startswith("- "):
                # Block sequence: `on:` then `- push`.
                triggers.append(_unquote(body[2:].strip()))
            elif ":" in body:
                # `push:`, `push: {}`, and `push: {branches: [main]}` all name
                # the same trigger. Requiring a bare trailing colon would let
                # the configured forms slip past as unrecognised.
                triggers.append(_unquote(body.split(":", 1)[0].strip()))
            else:
                raise WorkflowShapeError(
                    f"unmodelled trigger entry under `on:`: {body!r}"
                )
        elif section == "env" and indent == 2:
            key, value = _mapping_entry(body)
            workflow_env[key] = value
        elif section == "jobs" and indent == 2 and body.endswith(":"):
            current_job = body[:-1].strip().strip("\"'")
            jobs[current_job] = {"if": None, "env": {}}
            in_job_env = False
        elif section == "jobs" and indent == 4 and current_job:
            # Only a key at JOB level opens the job's own `env`; the `env:` of
            # a step lives deeper and governs that step alone.
            in_job_env = body == "env:"
            if body.startswith("if:"):
                jobs[current_job]["if"] = _unquote(body[len("if:"):].strip())
        elif section == "jobs" and indent == 6 and current_job and in_job_env:
            key, value = _mapping_entry(body)
            jobs[current_job]["env"][key] = value

    if not jobs:
        raise WorkflowShapeError("no jobs found; the scanner did not understand this file")
    return {"triggers": triggers, "env": workflow_env, "jobs": jobs}


def job_env(doc: dict, job: str, name: str) -> str | None:
    """The value a step in `job` would see for `name`.

    Job-level `env` wins over the workflow-level `env` it inherits, which is
    what GitHub Actions does - so a policy that moves between those two levels
    without changing meaning still reads the same here.
    """
    entry = doc["jobs"][job]
    if name in entry["env"]:
        return entry["env"][name]
    return doc["env"].get(name)


class PreviewForkGuard(unittest.TestCase):
    def setUp(self) -> None:
        self.doc = scan()
        self.jobs = self.doc["jobs"]
        self.assertTrue(self.jobs, "the workflow defines no jobs")

    def test_no_automatic_triggers(self) -> None:
        """A push or schedule would publish without anyone deciding to."""
        present = set(self.doc["triggers"])
        # Non-vacuity first. An empty trigger set satisfies "contains nothing
        # automatic" perfectly, so without this the assertion below reports the
        # policy as held precisely when the scanner has gone blind.
        self.assertTrue(
            present,
            "no triggers were parsed at all; the scanner cannot see this "
            "workflow's `on:` block, so it cannot be asserting anything about it",
        )
        fired_automatically = {"push", "pull_request", "schedule", "release"}
        self.assertFalse(
            present & fired_automatically,
            f"preview publishing must not fire automatically; found {sorted(present & fired_automatically)}",
        )

    def test_an_automatic_trigger_is_caught_however_it_is_written(self) -> None:
        """Fail-closed, against workflows this repository does not contain.

        Every form below is ordinary YAML that GitHub honours and that the
        previous scanner returned as NO TRIGGERS - so the no-automatic-triggers
        assertion passed on a workflow that publishes on every push. That is
        the exact failure this suite exists to prevent, so each form is fed
        through the real scanner and required to surface `push`.
        """
        jobs_block = "\njobs:\n  publish:\n    if: github.repository == 'herdrdev/herdr'\n"
        for label, on_block in [
            ("inline flow sequence", "on: [workflow_dispatch, push]"),
            ("inline scalar", "on: push"),
            ("block mapping", "on:\n  workflow_dispatch:\n  push:"),
            ("block mapping with empty flow map", "on:\n  workflow_dispatch:\n  push: {}"),
            ("block mapping with filters", "on:\n  push:\n    branches: [master]"),
            ("block sequence", "on:\n  - workflow_dispatch\n  - push"),
        ]:
            with self.subTest(form=label):
                doc = scan(on_block + jobs_block)
                self.assertIn(
                    "push",
                    doc["triggers"],
                    f"the {label} form hid an automatic trigger from the scanner",
                )

    def test_the_configured_workflow_is_still_seen(self) -> None:
        """The committed file must parse to exactly its one manual trigger.

        Pairs with the fail-closed cases above: those prove the scanner can
        see `push`, this proves it is not simply reporting triggers that are
        not there.
        """
        self.assertEqual(self.doc["triggers"], ["workflow_dispatch"])

    def test_unmodelled_trigger_syntax_raises_rather_than_scanning_empty(self) -> None:
        """Refusing beats guessing - and beats silently returning nothing."""
        with self.assertRaises(WorkflowShapeError):
            _trigger_names_from_inline("{push: {branches: [main]}}")
        with self.assertRaises(WorkflowShapeError):
            scan("on:\n  ???\njobs:\n  publish:\n    if: x\n")

    def test_every_job_carries_the_repository_guard(self) -> None:
        """At JOB level, so it governs the whole job rather than one step."""
        for name, job in self.jobs.items():
            condition = job["if"]
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
            condition = str(job["if"])
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
        for name, job in self.jobs.items():
            clause = _repository_clause(str(job["if"]))
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
        # An `env` model that quietly collapsed to nothing would make the stamp
        # assertion fail for a reason that has nothing to do with the workflow.
        self.assertTrue(self.doc["env"], "scanner read no workflow-level `env`")
        self.assertTrue(
            self.jobs["build"]["env"], "scanner read no job-level `env` for `build`"
        )

    def test_the_workflow_still_stamps_preview(self) -> None:
        """The reason the guard exists.

        If this workflow ever stopped setting `preview`, the incoherence would
        be gone and the guard could be reconsidered. Until then it must stay,
        and this records why — so a future reader does not remove the guard
        without noticing what it was protecting against.

        Asserted on the parsed model, in the environment the compiling job
        actually sees. A substring search over the file would pass on a
        commented-out line and fail on a stamp that merely moved from the job
        to the workflow, which changes nothing about what gets built.
        """
        self.assertEqual(
            job_env(self.doc, "build", "HERDR_BUILD_CHANNEL"),
            "preview",
            "the build job no longer stamps `preview`; revisit the fork guard "
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
