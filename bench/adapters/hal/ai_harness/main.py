"""HAL agent wrapper for ai-harness.

Install into a checkout of https://github.com/princeton-pli/hal-harness:

    cp -r bench/adapters/hal/ai_harness <hal-harness>/agents/
    hal-eval --benchmark swebench_verified_mini \\
             --agent_dir agents/ai_harness \\
             --agent_function main.run \\
             --agent_name "ai-harness" \\
             -A model=deepseek/deepseek-v4-pro

HAL owns the outer loop — task selection, Docker or VM isolation, Weave tracing,
and the cost-controlled leaderboard. This file turns one task into one headless
run and hands back whatever that benchmark wants as a submission.

The submission shape is per-benchmark, and getting it wrong scores zero on every
instance rather than failing loudly:

- **SWE-bench** wants a **git patch string**. HAL's own docs are explicit about
  this ("Return a dictionary mapping instance IDs to git patch strings"), and it
  is *not* what the harness prints. So the patch is taken from the workspace
  with `git diff` after the run — the same way Claw-SWE-Bench collects one, and
  for the same reason: the repository is the answer, not anything the model said.
- **Everything else** takes the model's own final text.

Detection is on the task fields rather than a flag, since HAL passes the
benchmark's own records straight through.
"""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "runner"))

from ai_harness_runner import run_harness  # noqa: E402

#: HAL puts the files for supported benchmarks in the working directory.
DEFAULT_WORKDIR = "."

#: Fields that mean this is a SWE-bench-shaped task wanting a patch back.
SWEBENCH_MARKERS = ("base_commit", "FAIL_TO_PASS", "test_patch")


def run(input: dict[str, dict], **kwargs: Any) -> dict[str, str]:
    """Run every task in `input`, returning one submission per task id."""
    model = kwargs.get("model") or kwargs.get("model_name") or os.environ.get(
        "OPENROUTER_MODEL"
    )
    max_iterations = int(kwargs.get("max_iterations", 100))
    timeout = int(kwargs.get("timeout", 1800))
    workdir = Path(kwargs.get("workdir", DEFAULT_WORKDIR))

    results: dict[str, str] = {}
    for task_id, task in input.items():
        run_result = run_harness(
            _prompt_for(task),
            workdir,
            model=model,
            max_iterations=max_iterations,
            timeout=timeout,
            # Out of the repository: for a patch-scored benchmark, a session
            # directory inside the workspace lands in the diff.
            extra_args=("--sessions-dir", "/tmp/ai-harness-sessions"),
        )
        if _wants_patch(task):
            _scrub(workdir)
            results[task_id] = _git_patch(workdir)
        else:
            results[task_id] = _answer(run_result)
    return results


def _wants_patch(task: dict[str, Any]) -> bool:
    return any(marker in task for marker in SWEBENCH_MARKERS)


def _git_patch(workdir: Path) -> str:
    """The working tree as a patch, staged first so new files are included.

    `git add -A` before diffing because an untracked file is invisible to a
    plain `git diff`, and a fix that adds a module would silently submit an
    empty patch.
    """
    try:
        subprocess.run(
            ["git", "add", "-A"], cwd=workdir, capture_output=True, timeout=120
        )
        done = subprocess.run(
            ["git", "diff", "--cached"],
            cwd=workdir,
            capture_output=True,
            text=True,
            timeout=120,
        )
        return done.stdout
    except (subprocess.SubprocessError, OSError):
        return ""


def _scrub(workdir: Path) -> None:
    """Remove harness state from the workspace before the patch is taken.

    `--sessions-dir` already keeps the bulk of it out; memory notes, job logs
    and checkpoints are rooted at the working directory, and any one of them in
    the diff is a failed instance for a reason unrelated to the model.
    """
    try:
        subprocess.run(
            ["rm", "-rf", str(workdir / ".ai_harness")],
            capture_output=True,
            timeout=60,
        )
    except (subprocess.SubprocessError, OSError):
        pass


def _prompt_for(task: dict[str, Any]) -> str:
    """The task statement, under whichever key this benchmark uses.

    HAL normalises less than the benchmarks it wraps, so the field name varies —
    SWE-bench uses `problem_statement`, others use `description` or `prompt`.
    Falls back to the task's own repr rather than sending an empty prompt: a run
    against nothing looks like a harness failure, where a run against something
    badly formatted looks like what it is.
    """
    for key in (
        "problem_statement",
        "description",
        "prompt",
        "question",
        "task",
        "instruction",
    ):
        value = task.get(key)
        if isinstance(value, str) and value.strip():
            return value
    return str(task)


def _answer(run_result: Any) -> str:
    """The submission for a non-patch benchmark.

    A run that hit its iteration budget or its clock says so rather than
    returning nothing: HAL's reliability dashboard separates failing from
    abstaining, and a silent empty string lands in the wrong one.
    """
    if run_result.ok:
        return run_result.stdout.strip() or "completed"
    return f"[ai-harness stopped: {run_result.exit}] {run_result.stdout.strip()}"
