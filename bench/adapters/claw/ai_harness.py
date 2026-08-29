"""ai-harness adapter for Claw-SWE-Bench.

Install into a checkout of https://github.com/opensquilla/claw-swe-bench:

    cp bench/adapters/claw/ai_harness.py claw_swebench/claws/
    # register in claw_swebench/claws/__init__.py:
    #     from claw_swebench.claws.ai_harness import AiHarnessAdapter
    #     CLAWS["ai-harness"] = AiHarnessAdapter
    # and add to CLAW_DEFAULTS in claw_swebench/config.py:
    #     "ai-harness": {"model": "z-ai/glm-5.3-flash",
    #                    "timeout": 3600, "max_turns": 300},

Architecture, following ZeroClaw: ai-harness runs INSIDE the SWE-bench container
as a single bind-mounted binary with no runtime dependencies. The binary must be
built for the container's architecture — the images are `x86_64`, so on Apple
Silicon build with `--platform linux/amd64` (see bench/Dockerfile).

Three things this has to get right, each of which is a silent wrong answer
otherwise:

- **Keep `.ai_harness/` out of `/testbed`.** Patches are collected by diffing
  the repo, so harness state written into the workspace lands in the patch and
  fails the instance. Sessions are pointed at `/tmp` and the directory is
  scrubbed before collection — the same problem NanoBot solves by deleting its
  own files.
- **`docker exec -i`.** The task statement goes in on stdin, so the exec needs
  a pipe; without `-i` the harness reads EOF immediately and runs an empty turn.
- **Leave the prompt alone.** `prompt_template` is not overridden. The
  benchmark's claim is that the harness is the only variable, which holds only
  while every claw is asked exactly the same thing.
"""

import json
import logging
import os
import subprocess
import time
from pathlib import Path

from claw_swebench.claws.base import BaseClawAdapter, decode_output
from claw_swebench.config import API_KEY_ENV_VARS
from claw_swebench.types import AgentResult

logger = logging.getLogger(__name__)

SUBPROCESS_TIMEOUT_BUFFER = 120

#: Host path of the binary, overridable like every other claw runtime path.
AI_HARNESS_BIN = os.environ.get("AI_HARNESS_BIN", "/usr/local/bin/ai-harness")

#: Where it is mounted, and where its state lives — deliberately outside
#: /testbed so that nothing it writes can reach the diff.
CONTAINER_BIN = "/usr/local/bin/ai-harness"
STATE_DIR = "/tmp/ai-harness"

WORKSPACE = "/testbed"

#: Checks for languages the harness deliberately declines to guess at.
#:
#: It infers its own for Cargo, Go, npm scripts, tsconfig and Ruff, and where it
#: can, that inference is the better answer — it is what an interactive session
#: would get. This table covers only the rest, so that a run is not quietly
#: measuring the harness with its verification loop switched off for most of the
#: corpus. An empty string means "no check": honest, and better than a slow or
#: wrong one.
CHECK_BY_MARKER = (
    ("pom.xml", "mvn -q -o compile -DskipTests"),
    ("build.gradle", "gradle --offline -q compileJava"),
    ("Gemfile", ""),
    ("composer.json", ""),
)


class AiHarnessAdapter(BaseClawAdapter):
    """Drives ai-harness headlessly inside SWE-bench containers."""

    name = "ai-harness"

    def __init__(self, model: str, timeout: int, max_turns: int | None = None):
        super().__init__(model=model, timeout=timeout, max_turns=max_turns)
        self._record: dict = {}

    # ------------------------------------------------------------------
    # Container integration
    # ------------------------------------------------------------------

    def container_run_args(self, instance_id: str) -> list[str]:
        return ["-v", f"{AI_HARNESS_BIN}:{CONTAINER_BIN}:ro"]

    def post_container_start(self, workspace) -> None:
        workspace.run_in_container(f"mkdir -p {STATE_DIR}")

    # ------------------------------------------------------------------
    # Task execution
    # ------------------------------------------------------------------

    def send_task(
        self,
        prompt: str,
        agent_id: str,
        container_name: str,
        artifact_dir: Path | None = None,
        instance_id: str | None = None,
    ) -> AgentResult:
        if artifact_dir:
            artifact_dir.mkdir(parents=True, exist_ok=True)

        stdout_path = artifact_dir / "agent_stdout.log" if artifact_dir else None
        stderr_path = artifact_dir / "agent_stderr.log" if artifact_dir else None

        cmd = ["docker", "exec", "-i"]
        for env_name in API_KEY_ENV_VARS:
            value = os.environ.get(env_name)
            if value:
                cmd.extend(["-e", f"{env_name}={value}"])
        cmd.extend(
            [
                container_name,
                CONTAINER_BIN,
                "--headless",
                # The container is the boundary; the harness refuses this
                # combination without --headless, so it cannot widen anything
                # outside a run like this one.
                "--sandbox",
                "none",
                # stdin, so a multi-line task statement survives the trip.
                "--prompt",
                "-",
                "--workdir",
                WORKSPACE,
                # Out of the repo, or the session lands in the patch.
                "--sessions-dir",
                f"{STATE_DIR}/sessions",
                "--model",
                self.model,
                "--max-iterations",
                str(self.max_turns or 300),
                "--headless-timeout",
                str(self.timeout),
            ]
        )
        check = self._check_for(container_name)
        if check is not None:
            cmd.extend(["--check", check])

        start_time = time.time()
        timed_out = False

        try:
            result = subprocess.run(
                cmd,
                input=prompt,
                capture_output=True,
                text=True,
                timeout=self.timeout + SUBPROCESS_TIMEOUT_BUFFER,
            )
            exit_code = result.returncode
            stdout = result.stdout
            stderr = result.stderr
        except subprocess.TimeoutExpired as e:
            timed_out = True
            exit_code = -1
            stdout = decode_output(e.stdout)
            stderr = decode_output(e.stderr)
            logger.warning(
                "ai-harness subprocess timed out after %ds",
                self.timeout + SUBPROCESS_TIMEOUT_BUFFER,
            )

        duration = time.time() - start_time

        # The run record is the harness's own account of what happened, and it
        # is on stdout. Parsed before cleanup, because cleanup removes the
        # session directory it names.
        self._record = _parse_record(stdout)
        _save_session(container_name, artifact_dir)
        _scrub_workspace(container_name)

        if stdout_path:
            stdout_path.write_text(stdout)
        if stderr_path:
            stderr_path.write_text(stderr)

        # Taken from the record rather than from whether stdout was empty. The
        # harness distinguishes "the model ended the turn" from "the iteration
        # budget ran out" from "the clock ran out", and collapsing those into
        # one boolean is how a bad budget hides behind a low score.
        finish_reason = _finish_reason(self._record, timed_out, exit_code)

        return AgentResult(
            success=finish_reason == "stop",
            timeout=timed_out or finish_reason == "timeout",
            exit_code=exit_code,
            finish_reason=finish_reason,
            stdout_path=stdout_path,
            stderr_path=stderr_path,
            session_id=self._record.get("session_path"),
            duration_seconds=round(duration, 1),
            usage=self._usage(),
        )

    # ------------------------------------------------------------------
    # Usage accounting
    # ------------------------------------------------------------------

    def collect_usage(self, workspace, artifact_dir: Path) -> dict:
        """Token accounting, straight from the record `send_task` already read.

        Nothing is copied out of the container: the harness measures its own
        prompt tokens exactly — every figure is what the provider reported, not
        an estimate off a trace — so there is nothing to reconcile.
        """
        if artifact_dir and self._record:
            (artifact_dir / "run_record.json").write_text(
                json.dumps(self._record, indent=2)
            )
        return self._usage()

    def backup_session(self, agent_id: str, dest: Path) -> None:
        """Already saved during `send_task`, before the workspace was scrubbed."""

    def delete_agent(self, agent_id: str) -> None:
        self._record = {}

    # ------------------------------------------------------------------
    # Helpers
    # ------------------------------------------------------------------

    def _usage(self) -> dict:
        ledger = self._record.get("ledger", {})
        prompt_tokens = int(ledger.get("prompt_tokens", 0))
        cached = int(ledger.get("cached_tokens", 0))
        return {
            "input_tokens": prompt_tokens,
            "output_tokens": int(ledger.get("completion_tokens", 0)),
            "cache_read_tokens": cached,
            "cache_hit_rate": cached / prompt_tokens if prompt_tokens else 0.0,
            "requests": int(ledger.get("requests", 0)),
            "wall_ms": self._record.get("wall_ms"),
            "exit": self._record.get("exit"),
            # Harness-specific, and the reason this adapter is worth having over
            # a generic one: nothing else in the benchmark can see how often the
            # model broke the protocol, asked a question nobody could answer, or
            # had its work rejected by the project check.
            "protocol_errors": self._record.get("protocol_errors", {}).get("total", 0),
            "questions_declined": self._record.get("questions_declined", 0),
            "compactions": self._record.get("compactions", 0),
            "check_runs": self._record.get("check", {}).get("runs", 0),
            "check_final_exit": self._record.get("check", {}).get("final_exit"),
            "actions": self._record.get("actions", {}),
        }

    @staticmethod
    def _check_for(container_name: str) -> str | None:
        """The check to force for this repo, or None to let the harness infer."""
        for marker, command in CHECK_BY_MARKER:
            probe = subprocess.run(
                ["docker", "exec", container_name, "test", "-f", f"{WORKSPACE}/{marker}"],
                capture_output=True,
                timeout=30,
            )
            if probe.returncode == 0:
                return command
        return None


def _parse_record(stdout: str) -> dict:
    """The JSON run record from stdout, or an empty dict.

    Scanned from the first `{` rather than parsed whole: a warning printed
    before the record would otherwise make the whole thing unreadable, and
    losing the accounting to a stray line would be a poor trade.
    """
    start = stdout.find("{")
    if start < 0:
        return {}
    try:
        return json.loads(stdout[start:])
    except json.JSONDecodeError:
        logger.warning("could not parse the ai-harness run record from stdout")
        return {}


def _finish_reason(record: dict, timed_out: bool, exit_code: int) -> str:
    if timed_out:
        return "timeout"
    match record.get("exit"):
        case "complete":
            return "stop"
        case "timeout":
            return "timeout"
        case "budget":
            # Its own outcome upstream would be better, but "error" is the
            # honest bucket: the turn did not finish on the model's terms.
            return "error"
        case "error":
            return "error"
    return "error" if exit_code != 0 else "empty"


def _save_session(container_name: str, artifact_dir: Path | None) -> None:
    """Copy the session out before the workspace is scrubbed.

    The transcript is the only place the *shape* of a failure survives: a score
    says an instance failed, the transcript says it spent nine round-trips
    re-reading one file.
    """
    if not artifact_dir:
        return
    try:
        result = subprocess.run(
            ["docker", "exec", container_name, "tar", "cf", "-", "-C", STATE_DIR, "sessions"],
            capture_output=True,
            timeout=120,
        )
        if result.returncode == 0 and result.stdout:
            (artifact_dir / "sessions.tar").write_bytes(result.stdout)
    except Exception:
        logger.warning("could not save the ai-harness session", exc_info=True)


def _scrub_workspace(container_name: str) -> None:
    """Remove harness state from /testbed before the patch is collected.

    `--sessions-dir` already keeps the bulk of it out, but memory notes, job
    logs and checkpoints are rooted at the working directory, and any one of
    them in the diff is a failed instance for a reason that has nothing to do
    with the model.
    """
    subprocess.run(
        [
            "docker",
            "exec",
            container_name,
            "bash",
            "-c",
            f"cd {WORKSPACE} && rm -rf .ai_harness",
        ],
        capture_output=True,
        timeout=30,
    )
