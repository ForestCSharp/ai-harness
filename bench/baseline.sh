#!/usr/bin/env bash
#
# Run a fixed set of Claw-SWE-Bench instances and collect their run records.
#
# Used twice: once on current code to establish a control group, and again after
# a harness change to compare against it. The instance list and the metrics are
# fixed here rather than chosen per run, so a later comparison cannot quietly
# become a different experiment.
#
#   bench/baseline.sh before
#   bench/baseline.sh after
#
# Records land in bench/baselines/<label>/<instance>-<n>.json. Read them with
# bench/metrics.py, which computes the pre-registered figures.
#
# Environment (all have defaults; override to match your checkout):
#   CLAW_REPO    path to a claw-swe-bench checkout with the adapter installed
#   CLAW_PYTHON  python with `datasets` and `pyyaml` available
#   AI_HARNESS_BIN  the linux/amd64 binary to bind-mount (see bench/Dockerfile)
#   BENCH_TIMEOUT   per-instance seconds (default 900)

set -uo pipefail

LABEL="${1:-}"
if [[ -z "$LABEL" ]]; then
    echo "usage: bench/baseline.sh <label>   (e.g. 'before', 'after')" >&2
    exit 2
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$REPO_ROOT/bench/baselines/$LABEL"

CLAW_REPO="${CLAW_REPO:?set CLAW_REPO to a claw-swe-bench checkout}"
CLAW_PYTHON="${CLAW_PYTHON:-python3}"
AI_HARNESS_BIN="${AI_HARNESS_BIN:?set AI_HARNESS_BIN to the linux/amd64 binary}"
BENCH_TIMEOUT="${BENCH_TIMEOUT:-900}"

# Three repeats of one instance to see whether a single run is representative,
# plus two others to see whether the pattern is specific to that repository.
# Rust instances throughout, so the harness's own `cargo check` inference is
# engaged rather than sitting idle behind a language it declines to guess at.
RUNS=(
    "burntsushi__ripgrep-2209 1"
    "burntsushi__ripgrep-2209 2"
    "burntsushi__ripgrep-2209 3"
    "sharkdp__bat-2201 1"
    "sharkdp__bat-2650 1"
)

mkdir -p "$OUT"
echo "Writing records to $OUT"

for entry in "${RUNS[@]}"; do
    read -r instance n <<<"$entry"
    run_id="baseline-$LABEL-$instance-$n"
    dest="$OUT/$instance-$n.json"

    if [[ -f "$dest" ]]; then
        echo "== $instance #$n — already recorded, skipping"
        continue
    fi

    echo "== $instance #$n — starting $(date -u +%H:%M:%S)"
    # Serial on purpose. Container names are derived from the instance id, so two
    # runs of the same instance would collide; and under x86 emulation on Apple
    # Silicon, concurrent instances contend for CPU badly enough to distort the
    # wall-clock figure this is trying to measure.
    (
        cd "$CLAW_REPO" && \
        AI_HARNESS_BIN="$AI_HARNESS_BIN" \
        "$CLAW_PYTHON" run_infer.py \
            --claw ai-harness \
            --dataset multilingual \
            --run_id "$run_id" \
            --instance_ids "$instance" \
            --timeout "$BENCH_TIMEOUT" \
            --no_resume
    ) >"$OUT/$instance-$n.log" 2>&1

    record="$CLAW_REPO/artifacts/$run_id/$instance/run_record.json"
    if [[ -f "$record" ]]; then
        cp "$record" "$dest"
        echo "   recorded $(basename "$dest")"
    else
        # A missing record is itself a result — it means the harness never got
        # far enough to write one — so say so rather than failing the sweep and
        # losing the runs that did complete.
        echo "   NO RECORD (see $OUT/$instance-$n.log)"
    fi
done

echo
echo "Done. Summarise with:"
echo "  python3 bench/metrics.py $OUT"
