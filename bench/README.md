# Benchmarking the harness

Running `ai-harness` under published **harness** benchmarks — the ones that hold
the model fixed and vary the scaffold — so that changes to this repository can be
measured rather than argued about.

The premise, from Claw-SWE-Bench: varying the harness alone moved Pass@1 by
**12.5–27.4 points** on a fixed model, against 29.4 points across nine models on
a fixed harness. The scaffold is a model-tier-sized variable.

## Headless mode

Everything here rests on one flag. `--headless` runs a single prompt with no
terminal and prints a JSON record:

```bash
ai-harness --headless --prompt "fix the failing test in src/parser.rs"
```

```bash
echo "$TASK_STATEMENT" | ai-harness --headless --prompt - --headless-output run.json
```

It is the same loop the interactive harness runs — the same contract, protocol,
retry path, iteration budget, project check and compaction trigger — with the
screen and the keyboard removed. Two things it must do that a person otherwise
does: it approves its own actions (`--auto-approve` is forced, since there is no
modal to answer), and it *declines* any `<ai-harness-option>` question, so a run
that needed an answer ends as a run rather than hanging.

The record it writes:

| Field | What it is |
| --- | --- |
| `exit` | `complete` \| `budget` \| `timeout` \| `error` |
| `ledger` | prompt/completion/cached tokens, requests, time in flight |
| `actions` | reads, searches, fetches, shells, writes, denied |
| `protocol_errors` | total and by parser reason |
| `questions_declined` | non-zero means the model wanted a human |
| `compactions` | counted from `compaction-NNN.json` on disk |
| `check` | the command, how many times it ran, its final exit code |
| `unconfined` | whether the sandbox was off |

`protocol_errors` is the one no other harness reports, and it comes free:
`roll_back_retries` prunes only the model's copy of the conversation, so the
transcript keeps every malformed reply.

## `--sandbox=none`

Commands are confined on macOS (Seatbelt) and Linux (Landlock), so a benchmark
container on native Linux gets real confinement and needs nothing special. The
exception is **x86_64 emulation on Apple Silicon**, where Landlock is absent
(`ENOSYS`) — and SWE-bench images are x86_64, so on a Mac the containers have no
kernel confinement available. `--sandbox=none` turns confinement off **and is
refused unless `--headless` is also set** — the case it exists for is a per-task container that
is already the boundary, and at an interactive prompt there would be nothing
else confining anything. The record carries `unconfined: true` so a result can
never be read as confined when it was not.

## The three benchmarks

| | What it measures | Where |
| --- | --- | --- |
| **Claw-SWE-Bench** | Pass@1, cost, wall-clock, cache hit rate over 350 instances in 8 languages | [opensquilla/claw-swe-bench](https://github.com/opensquilla/claw-swe-bench) |
| **HAL** | 11 benchmarks behind one cost-controlled runner, plus a reliability dashboard | [princeton-pli/hal-harness](https://github.com/princeton-pli/hal-harness) |
| **Harness-Bench** | 106 offline tasks scored `outcome x process x security` | [Qihoo360/harness-bench](https://github.com/Qihoo360/harness-bench) |

Harness-Bench is the only one of the three where the sandbox and the approval
model are not purely a handicap — `security_score` is a term in its result — so
it is worth running with `--sandbox=none` and without.

### Claw-SWE-Bench

```bash
docker build --platform linux/amd64 -f bench/Dockerfile -t ai-harness:bench .
docker create --name x ai-harness:bench && docker cp x:/opt/ai-harness/ai-harness ./ai-harness && docker rm x
```

```bash
git clone https://github.com/opensquilla/claw-swe-bench && cd claw-swe-bench
git checkout fcece5f4c0817430ce953b52c80c931a40cd9b83   # the verified commit
pip install -r requirements.txt
python3 ../bench/register.py claw .
```

`register.py` copies the adapter in **and registers it** — `CLAWS`,
`CLAW_DEFAULTS`, and the equivalents for Harness-Bench. Copying alone is not
enough: an unregistered adapter means a run that clones, copies, executes
nothing, and reports success. It is idempotent, and it fails loudly if an anchor
has moved, because a silently skipped patch produces exactly that empty run.

```bash
AI_HARNESS_BIN=/abs/path/to/ai-harness python run_infer.py \
  --claw ai-harness --dataset multilingual --run_id r1 \
  --instance_ids burntsushi__ripgrep-2209
```

Patches are extracted by diffing the repository after the run, so the harness
never has to emit a diff — it only has to edit files. The adapter therefore
leaves `prompt_template` alone, which the benchmark requires: its claim only
holds while every claw is asked the same thing.

### HAL

```bash
cp -r bench/adapters/hal/ai_harness <hal-harness>/agents/
hal-eval --benchmark swebench_verified_mini \
         --agent_dir agents/ai_harness --agent_function main.run \
         --agent_name "ai-harness" -A model=$OPENROUTER_MODEL
```

### Harness-Bench

```bash
git checkout 1025086a446653702b80cfb48babbeec35db6b2c   # the verified commit
python3 bench/register.py harnessbench <harness-bench>
PYTHONPATH=src python3 -m harnessbench.cli run-task --task 001-file --harness ai-harness
```

`confined: true` is the default and the interesting setting — this benchmark
runs on the host, so the real sandbox applies and `security_score` is a term in
its combined score. Set it false only on a kernel without Landlock.

This one runs the agent **on the host** against a prepared workspace, with an
isolated `HOME`, rather than in a container. So the real sandbox can stay on —
and should, since `security_score` is a term in its combined score. `confined:
false` exists for Linux, where there is no sandbox and the harness would
otherwise refuse to start.

## Running a sweep

```bash
CLAW_REPO=... CLAW_PYTHON=... AI_HARNESS_BIN=... bench/baseline.sh before
python3 bench/metrics.py bench/baselines/before
```

**Keep the machine awake.** On macOS a sweep that outlasts the idle-sleep timer
does not fail — it *stretches*. Sleeping suspends the Docker VM, so container
processes stop accruing time; the harness's own `Instant` deadline and Python's
`subprocess` timeout both use monotonic clocks that pause with it, so neither
fires. Everything resumes correctly on wake, but a 75-minute sweep can take many
hours of wall clock, and `docker ps` will show a container "Up About an hour"
whose PID 1 reports fourteen minutes. That divergence is the tell.

`baseline.sh` re-execs itself under `caffeinate -i` on macOS. To pin an
already-running sweep:

```bash
caffeinate -i -w $(pgrep -f baseline.sh | head -1)
```

## Status of each adapter

Claw-SWE-Bench and Harness-Bench were written against their **real** base
classes in a local checkout, and both register and construct under their own
runners. The interfaces differ substantially from what the papers describe, so
this matters:

| | Real interface | Verified against |
| --- | --- | --- |
| Claw | `send_task(prompt, agent_id, container_name, artifact_dir, instance_id) -> AgentResult` | source; registers, constructs, **ran a real instance** |
| Harness-Bench | `run(ctx: AdapterRunContext) -> AdapterRunResult` | source; registers, constructs |
| HAL | `run(input, **kwargs) -> dict[str, str]` | source; not executed |

The HAL return shape is settled: `agents/README.md` and its worked example both
say `dict[str, str]`, and for SWE-bench specifically it says outright *"Return a
dictionary mapping instance IDs to git patch strings."* That is **not** what the
harness prints, so the adapter takes the patch from the workspace with `git
diff` — the same way Claw collects one. Returning the model's prose there would
have scored zero on every instance without erroring.

## Three things that would silently produce wrong numbers

Found by running it rather than by reading, and all three are handled:

- **Harness state in the patch.** Claw collects a patch by diffing `/testbed`,
  so anything the harness writes there lands in the submission and fails the
  instance for a reason that has nothing to do with the model. `--sessions-dir`
  points at `/tmp`, and the adapter scrubs `.ai_harness/` before collection.
  NanoBot's adapter solves the same problem by deleting its own files.
- **Architecture.** SWE-bench images are `x86_64`. A binary built on Apple
  Silicon will not execute in them, so build with `--platform linux/amd64`.
- **`docker exec -i`.** The task statement arrives on stdin; without `-i` the
  harness reads EOF and runs an empty turn — which looks like a model failure
  rather than a plumbing one.

## Layout

```
bench/
├── Dockerfile                        the binary, built once, bind-mounted per task
├── runner/ai_harness_runner.py       the only place that knows the CLI
└── adapters/
    ├── claw/ai_harness.py
    ├── hal/ai_harness/main.py
    └── harnessbench/ai_harness.py
```

## CI

`ci.yml` is free and runs on everything, forks included: fmt, clippy, tests on
Linux *and* macOS (only macOS exercises the sandbox suite), the Docker build,
and an import check on the adapters.

`bench.yml` spends money, so fork PRs are skipped by an explicit guard rather
than left to fail on a missing key. Its `smoke` job runs one real headless turn
per push and PR from this repo; the full suites are `workflow_dispatch` only,
because a 350-instance sweep per commit is a different order of spend.
`BENCH_MODEL` pins the model as a repository variable.

### Giving CI a key

Four controls, in descending order of how much they actually protect you:

1. **A separate OpenRouter key with a recurring spend cap.** Create one just for
   CI — not the key in your local `.env` — and set a limit that resets (say $20
   monthly). This is the control that bounds the damage from a leak, a loop, or
   a runaway dispatch, and it holds regardless of everything below. Revoking it
   does not disturb local development.
2. **Store it as a repository secret** named `OPENROUTER_API_KEY`, under
   Settings → Secrets and variables → Actions. Encrypted at rest, injected as an
   env var, masked in logs.
3. **Approve the expensive job.** `suite` declares `environment: bench`. Add
   required reviewers to that environment under Settings → Environments and a
   dispatch will wait for a human before it can reach the key. Until you do, the
   environment exists but gates nothing.
4. **The key is scoped to the step that uses it**, not the job — so it is absent
   from the environment of `checkout`, the toolchain installer, the cache action
   and the artifact uploader. Third-party actions are the realistic path by which
   a secret leaves a public repository.

**What none of this stops:** anyone with *write* access can exfiltrate a secret
by adding a workflow step that transforms it — log masking only catches the
literal string. The trust boundary is who can push workflows, not who can read
the repository. That is why the spend cap is first on this list and the storage
mechanism is second.
