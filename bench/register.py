#!/usr/bin/env python3
"""Register the ai-harness adapter into a benchmark checkout.

    python3 bench/register.py claw         /path/to/claw-swe-bench
    python3 bench/register.py harnessbench /path/to/harness-bench

Copying the adapter file in is not enough: each benchmark discovers adapters
through its own registry, and an unregistered adapter means a dispatch that
clones, copies, runs nothing, and reports success. These are the same edits
applied by hand to produce the first baseline sweep.

**Every patch fails loudly if its anchor is missing.** A missing anchor means
upstream restructured, and the only wrong response is to carry on — a benchmark
run that silently measures nothing is worse than one that does not start. Each
patch is also idempotent, so re-running against an already-registered checkout
is a no-op rather than a duplicate import.
"""

from __future__ import annotations

import shutil
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent

#: What the adapter is called in each benchmark's registry, and what model it
#: defaults to. The model is only a default — every runner takes an override.
CLAW_NAME = "ai-harness"
DEFAULT_MODEL = "z-ai/glm-5.3-flash"


class Missing(RuntimeError):
    """An anchor was not found: upstream moved, and this must not continue."""


def patch(path: Path, anchor: str, addition: str, *, marker: str) -> str:
    """Insert `addition` after `anchor`, unless `marker` is already present."""
    if not path.is_file():
        raise Missing(f"{path} does not exist")
    text = path.read_text()
    if marker in text:
        return f"  = {path.name}: already registered"
    if anchor not in text:
        raise Missing(
            f"{path}: could not find the anchor\n"
            f"    {anchor.strip()!r}\n"
            f"  Upstream has restructured. Re-derive the patch by hand and update\n"
            f"  bench/register.py — do not skip it, or the run will measure nothing."
        )
    path.write_text(text.replace(anchor, anchor + addition, 1))
    return f"  + {path.name}: registered"


def register_claw(repo: Path) -> list[str]:
    """Claw-SWE-Bench: a CLAWS entry and a CLAW_DEFAULTS entry."""
    shutil.copy2(HERE / "adapters/claw/ai_harness.py", repo / "claw_swebench/claws/")
    steps = ["  + ai_harness.py copied into claw_swebench/claws/"]

    steps.append(
        patch(
            repo / "claw_swebench/claws/__init__.py",
            anchor="from claw_swebench.claws.base import BaseClawAdapter",
            addition="\nfrom claw_swebench.claws.ai_harness import AiHarnessAdapter",
            marker="claw_swebench.claws.ai_harness",
        )
    )
    steps.append(
        patch(
            repo / "claw_swebench/claws/__init__.py",
            anchor='    "generic": GenericAgentAdapter,',
            addition=f'\n    "{CLAW_NAME}": AiHarnessAdapter,',
            marker=f'"{CLAW_NAME}": AiHarnessAdapter',
        )
    )
    steps.append(
        patch(
            repo / "claw_swebench/config.py",
            anchor='    "generic":  {"model": "glm-5.1",        "timeout": 3600, "max_turns": 300, "llm_no": 0},',
            addition=(
                f'\n    "{CLAW_NAME}": {{"model": "{DEFAULT_MODEL}", '
                f'"timeout": 3600, "max_turns": 300}},'
            ),
            marker=f'"{CLAW_NAME}":',
        )
    )
    return steps


def register_harnessbench(repo: Path) -> list[str]:
    """Harness-Bench: an adapter import and a `models:` entry."""
    shutil.copy2(
        HERE / "adapters/harnessbench/ai_harness.py",
        repo / "src/harnessbench/adapters/",
    )
    steps = ["  + ai_harness.py copied into src/harnessbench/adapters/"]

    steps.append(
        patch(
            repo / "src/harnessbench/adapters/__init__.py",
            anchor="from harnessbench.adapters.base import BaseAdapter",
            addition="\nfrom harnessbench.adapters.ai_harness import AiHarnessAdapter",
            marker="harnessbench.adapters.ai_harness",
        )
    )
    steps.append(
        patch(
            repo / "src/harnessbench/adapters/__init__.py",
            anchor='__all__ = [\n    "BaseAdapter",',
            addition='\n    "AiHarnessAdapter",',
            marker='"AiHarnessAdapter",',
        )
    )

    # The config is YAML in flow style — JSON-shaped, but with trailing commas
    # that json.loads rejects. Patched textually rather than parsed and
    # re-emitted: that needs no YAML dependency, and it leaves the user's own
    # formatting and comments exactly as they were.
    config = repo / "config/harness.yaml"
    if not config.is_file():
        example = repo / "config/harness.example.yaml"
        if not example.is_file():
            raise Missing(f"neither {config} nor {example} exists")
        shutil.copy2(example, config)
        steps.append("  + config/harness.yaml created from the example")

    entry = (
        f'\n    "{CLAW_NAME}": {{'
        f'\n      "adapter": "ai_harness",'
        f'\n      "command": "ai-harness",'
        f'\n      "model": "{DEFAULT_MODEL}",'
        f'\n      "confined": true,'
        f'\n      "timeout_sec": 1800'
        f"\n    }},"
    )
    steps.append(
        patch(
            config,
            anchor='"models": {',
            addition=entry,
            marker=f'"{CLAW_NAME}"',
        )
    )
    return steps


BENCHMARKS = {"claw": register_claw, "harnessbench": register_harnessbench}


def main(argv: list[str]) -> int:
    if len(argv) != 2 or argv[0] not in BENCHMARKS:
        print(__doc__)
        print(f"benchmarks: {', '.join(BENCHMARKS)}", file=sys.stderr)
        return 2
    name, repo = argv[0], Path(argv[1]).resolve()
    if not repo.is_dir():
        print(f"no such checkout: {repo}", file=sys.stderr)
        return 1

    print(f"Registering ai-harness into {name} at {repo}")
    try:
        for step in BENCHMARKS[name](repo):
            print(step)
    except Missing as err:
        print(f"\nFAILED: {err}", file=sys.stderr)
        return 1
    print("Done.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
