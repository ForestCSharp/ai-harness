"""ai-harness adapter for Harness-Bench.

Install into a checkout of https://github.com/Qihoo360/harness-bench:

    cp bench/adapters/harnessbench/ai_harness.py src/harnessbench/adapters/
    # register in src/harnessbench/adapters/__init__.py:
    #     from harnessbench.adapters.ai_harness import AiHarnessAdapter
    # and add an entry under `models:` in config/harness.yaml:
    #     ai-harness:
    #       adapter: ai_harness
    #       command: /usr/local/bin/ai-harness
    #       model: deepseek/deepseek-v4-pro
    #       confined: true

    PYTHONPATH=src python3 -m harnessbench.cli run-task \\
        --task 001-file --harness ai-harness

Unlike Claw-SWE-Bench, Harness-Bench runs the agent **on the host** against a
prepared workspace directory rather than inside a container, handing it an
isolated `HOME` in `ctx.sandbox`. Two consequences:

- **The real sandbox can stay on.** Seatbelt confines writes to the workspace
  root, which is exactly what this benchmark wants; `security_score` is a term
  in its combined score, so running confined is the configuration of interest
  rather than a handicap. `confined: false` in the model config is there for
  Linux, where the harness has no sandbox and would otherwise refuse to start.
- **Usage comes from our own record, not the proxy.** Harness-Bench can route
  traffic through `HARNESSBENCH_LLM_PROXY_URL` to count tokens, but ai-harness
  has no base-URL override. It reports its own exact figures instead — what the
  provider returned, rather than what a proxy inferred.
"""

from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path

from harnessbench.adapters.base import BaseAdapter
from harnessbench.models import AdapterRunContext, AdapterRunResult


class AiHarnessAdapter(BaseAdapter):
    name = "ai-harness"

    def run(self, ctx: AdapterRunContext) -> AdapterRunResult:
        command = str(ctx.model_config.get("command") or "ai-harness")
        confined = bool(ctx.model_config.get("confined", True))

        # Harness state under the isolated HOME, never under the workspace: the
        # oracle inspects the workspace, and a session directory sitting in it
        # is a difference the task did not ask for.
        state = Path(ctx.sandbox) / ".ai-harness"
        state.mkdir(parents=True, exist_ok=True)

        cmd = [
            command,
            "--headless",
            "--prompt",
            "-",
            "--workdir",
            str(ctx.workspace),
            "--sessions-dir",
            str(state / "sessions"),
            "--headless-timeout",
            str(ctx.timeout_sec),
        ]
        if not confined:
            cmd.extend(["--sandbox", "none"])
        if ctx.model_id:
            cmd.extend(["--model", ctx.model_id])
        max_iterations = ctx.model_config.get("max_iterations")
        if max_iterations:
            cmd.extend(["--max-iterations", str(max_iterations)])
        check = ctx.model_config.get("check")
        if check is not None:
            cmd.extend(["--check", str(check)])
        cmd.extend(str(arg) for arg in (ctx.model_config.get("extra_args") or []))

        env = os.environ.copy()
        env.update(ctx.env)
        env["HOME"] = str(ctx.sandbox)

        try:
            completed = subprocess.run(
                cmd,
                cwd=str(ctx.workspace),
                input=ctx.prompt,
                text=True,
                capture_output=True,
                # A little past the harness's own deadline, so it stops itself
                # and writes a record rather than being killed without one.
                timeout=ctx.timeout_sec + 60,
                env=env,
                check=False,
            )
            stdout, stderr = completed.stdout, completed.stderr
            returncode = completed.returncode
            timed_out = False
        except subprocess.TimeoutExpired as expired:
            stdout = _text(expired.stdout)
            stderr = _text(expired.stderr)
            returncode = -1
            timed_out = True

        record = _parse_record(stdout)
        ledger = record.get("ledger", {})
        prompt_tokens = int(ledger.get("prompt_tokens", 0))
        cached = int(ledger.get("cached_tokens", 0))

        return AdapterRunResult(
            # The harness's own verdict, not the process exit code: it exits 0
            # whenever it produced a record, including for a run that hit its
            # budget, because a non-zero status there would be
            # indistinguishable from the binary failing to start.
            ok=record.get("exit") == "complete",
            command=cmd,
            stdout=stdout,
            stderr=stderr,
            metadata={
                "returncode": returncode,
                "timed_out": timed_out,
                "exit": record.get("exit"),
                "iterations": record.get("iterations"),
                "wall_ms": record.get("wall_ms"),
                "input_tokens": prompt_tokens,
                "output_tokens": int(ledger.get("completion_tokens", 0)),
                "cache_read_tokens": cached,
                "cache_hit_rate": cached / prompt_tokens if prompt_tokens else 0.0,
                "requests": int(ledger.get("requests", 0)),
                "confined": confined,
                "unconfined_reported": record.get("unconfined"),
                # The trace this benchmark's rubric scores.
                "session_path": record.get("session_path"),
                # Harness-specific signals nothing else here can see.
                "protocol_errors": record.get("protocol_errors", {}).get("total", 0),
                "questions_declined": record.get("questions_declined", 0),
                "compactions": record.get("compactions", 0),
                "actions": record.get("actions", {}),
                "check": record.get("check", {}),
            },
        )


def _parse_record(stdout: str) -> dict:
    """The JSON run record from stdout, or an empty dict.

    Scanned from the first `{` rather than parsed whole, so a warning printed
    ahead of the record does not cost the whole accounting.
    """
    start = stdout.find("{")
    if start < 0:
        return {}
    try:
        return json.loads(stdout[start:])
    except json.JSONDecodeError:
        return {}


def _text(raw) -> str:
    if raw is None:
        return ""
    if isinstance(raw, bytes):
        return raw.decode("utf-8", "replace")
    return str(raw)
