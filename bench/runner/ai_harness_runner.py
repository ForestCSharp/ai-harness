"""Invoke the ai-harness binary headlessly and parse its run record.

The three benchmark adapters in `bench/adapters/` all need the same thing: run
one prompt in a workspace, wait, and read what happened. Only this module knows
the command line, so a flag that changes shape is one edit rather than three
that drift apart.

Nothing here interprets the *task*. Scoring, patch extraction, and oracle checks
belong to whichever benchmark is driving; this is the bit in the middle.
"""

from __future__ import annotations

import json
import os
import subprocess
import tempfile
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Mapping, Sequence

DEFAULT_BINARY = os.environ.get("AI_HARNESS_BIN", "ai-harness")

# Matches `--headless-timeout`'s default. Kept here as well so a runner that
# never passes one still gets a bounded run rather than an unbounded wait.
DEFAULT_TIMEOUT_SECONDS = 1800


@dataclass
class HarnessRun:
    """One headless run: what the harness reported, and how the process ended."""

    record: dict[str, Any] = field(default_factory=dict)
    stdout: str = ""
    stderr: str = ""
    returncode: int = 0
    #: True when the *wrapper* killed the process, as opposed to the harness
    #: stopping itself at `--headless-timeout` and reporting `exit: "timeout"`.
    #: The two are different failures and a benchmark result should not blur
    #: them: one is the harness behaving correctly at its bound, the other is
    #: the harness not coming back.
    killed: bool = False

    @property
    def exit(self) -> str:
        return str(self.record.get("exit", "error"))

    @property
    def ok(self) -> bool:
        """Whether the model ended the turn itself, rather than hitting a bound."""
        return self.exit == "complete"

    @property
    def tokens_in(self) -> int:
        return int(self.record.get("ledger", {}).get("prompt_tokens", 0))

    @property
    def tokens_out(self) -> int:
        return int(self.record.get("ledger", {}).get("completion_tokens", 0))

    @property
    def cached_tokens(self) -> int:
        return int(self.record.get("ledger", {}).get("cached_tokens", 0))

    @property
    def requests(self) -> int:
        return int(self.record.get("ledger", {}).get("requests", 0))

    @property
    def cache_hit_rate(self) -> float:
        """Cached prompt tokens as a fraction of all prompt tokens.

        One of the metrics Claw-SWE-Bench reports, and the one most sensitive to
        harness changes: the whole conversation is re-sent every round-trip, so
        where the cache breakpoints land is worth a large fraction of the bill.
        """
        return self.cached_tokens / self.tokens_in if self.tokens_in else 0.0

    def cost_usd(self, price_in: float | None, price_out: float | None) -> float | None:
        """Dollar cost, or None when prices were not supplied.

        Rates change and differ per model, so they are passed in rather than
        baked in — the same argument `--price-in` is built on.
        """
        if price_in is None or price_out is None:
            return None
        return (self.tokens_in * price_in + self.tokens_out * price_out) / 1_000_000


def run_harness(
    prompt: str,
    workdir: str | Path,
    *,
    binary: str = DEFAULT_BINARY,
    model: str | None = None,
    max_iterations: int | None = None,
    timeout: int = DEFAULT_TIMEOUT_SECONDS,
    check: str | None = None,
    unconfined: bool = True,
    env: Mapping[str, str] | None = None,
    extra_args: Sequence[str] = (),
) -> HarnessRun:
    """Run one prompt to completion in `workdir`.

    The prompt goes in on **stdin** rather than as an argument. A task statement
    is usually a paragraph with newlines and quoting in it, and an argv round
    trip through a shell is where that gets mangled.

    `unconfined` defaults True because every caller is a benchmark runner
    handing the harness its own container. The harness refuses that combination
    outside `--headless`, so this cannot widen anything interactively.
    """
    workdir = Path(workdir)
    argv: list[str] = [binary, "--headless", "--prompt", "-"]
    if unconfined:
        argv += ["--sandbox", "none"]
    if model:
        argv += ["--model", model]
    if max_iterations is not None:
        argv += ["--max-iterations", str(max_iterations)]
    if check is not None:
        # An empty string reads as "no check", which is how a caller turns the
        # verification loop off for a language the image has no fast check for.
        argv += ["--check", check]
    # Leave the harness a little less than the wrapper's patience, so it stops
    # itself and writes a record instead of being killed without one.
    argv += ["--headless-timeout", str(max(timeout - 30, 60))]
    argv += list(extra_args)

    environment = dict(os.environ)
    if env:
        environment.update(env)

    with tempfile.TemporaryDirectory() as scratch:
        record_path = Path(scratch) / "record.json"
        argv += ["--headless-output", str(record_path)]

        killed = False
        try:
            completed = subprocess.run(
                argv,
                cwd=workdir,
                input=prompt,
                capture_output=True,
                text=True,
                timeout=timeout,
                env=environment,
                check=False,
            )
            stdout, stderr, returncode = (
                completed.stdout,
                completed.stderr,
                completed.returncode,
            )
        except subprocess.TimeoutExpired as expired:
            killed = True
            stdout = _text(expired.stdout)
            stderr = _text(expired.stderr)
            returncode = -1

        record: dict[str, Any] = {}
        if record_path.is_file():
            try:
                record = json.loads(record_path.read_text())
            except json.JSONDecodeError as err:
                record = {"exit": "error", "parse_error": str(err)}
        elif not record:
            # No record at all means the binary never got far enough to write
            # one — a bad flag, a missing key, a killed process. Say so in the
            # record's own vocabulary so callers have one shape to read.
            record = {
                "exit": "timeout" if killed else "error",
                "stderr": stderr[-4000:],
            }

    return HarnessRun(
        record=record,
        stdout=stdout,
        stderr=stderr,
        returncode=returncode,
        killed=killed,
    )


def _text(raw: Any) -> str:
    if raw is None:
        return ""
    if isinstance(raw, bytes):
        return raw.decode("utf-8", "replace")
    return str(raw)
