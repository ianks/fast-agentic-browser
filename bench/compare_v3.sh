#!/bin/bash
# v3: agent mode (fab gets the task; LLM as coprocessor) and vm (engine
# only) against the goal toolset arm and the control (LLM + chrome-devtools-mcp).
# All arms run concurrently, so machine load is shared equally. Results are
# merged per arm into bench/results/$TAG-<arm>.json for bench/summary.py.
cd "$(dirname "$0")/.."
# Each comparison runs its own copy of the binary, so rebuilding mid-run is safe.
mkdir -p target/bench-bin
cp "${B:-./target/release/fab}" "target/bench-bin/fab-${TAG:-v3}"
B="target/bench-bin/fab-${TAG:-v3}"
M=${M:-inception/mercury-2.5}
TAG=${TAG:-v3}
ARMS=${ARMS:-control goal agent vm}
SUITES=${SUITES:-"comp:bench/scenarios.toml hard:bench/hard.toml"}
run() { # suite arm label
  $B bench --suite "$1" --mode "$2" --planner-model "$M" --label "$3" ${EXTRA} > bench/results/$3.log 2>&1
  echo "done $3: $(/usr/bin/grep -cE '\] #[0-9]+ PASS' bench/results/$3.log)/$(/usr/bin/grep -cE '\] #[0-9]+ (PASS|FAIL)' bench/results/$3.log)"
}
for arm in $ARMS; do
  for s in $SUITES; do run "${s#*:}" $arm "$TAG-${s%%:*}-$arm" & done
done
wait
python3 - "$TAG" "$ARMS" "$SUITES" <<'PY'
import json, sys
tag, arms, suites = sys.argv[1], sys.argv[2].split(), [s.split(':')[0] for s in sys.argv[3].split()]
for a in arms:
    runs = []
    for s in suites:
        try: runs += json.load(open(f'bench/results/{tag}-{s}-{a}.json'))['runs']
        except FileNotFoundError: pass
    json.dump({'label': f'{tag}-{a}', 'mode': a, 'runs': runs}, open(f'bench/results/{tag}-{a}.json', 'w'))
PY
python3 bench/summary.py $(for a in $ARMS; do echo "$TAG-$a"; done)
