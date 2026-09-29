#!/bin/bash
# Site-shape cache: does knowing a site's structure help a NEW task there?
# Each suite has two tasks per app. Pass 1 learns site maps from each app's
# first task (shape=record); then each app's second task runs twice at the
# same time: with the maps (shape=use) and without (shape=off). Paired.
# Usage: SUITE=bench/heldout2.toml TAG=w2 ./bench/warm.sh
cd "$(dirname "$0")/.."
SUITE=${SUITE:-bench/heldout2.toml}
TAG=${TAG:-warm}
M=${M:-inception/mercury-2.5,stepfun/step-3.7-flash}
D="$PWD/target/shapes-$TAG"
rm -rf "$D"
mkdir -p target/bench-bin
cp target/release/fab "target/bench-bin/fab-$TAG"
B="target/bench-bin/fab-$TAG"
read FIRST SECOND < <(python3 - "$SUITE" <<'PY'
import sys, tomllib
seen, first, second = {}, [], []
for s in tomllib.load(open(sys.argv[1], 'rb'))['scenario']:
    n = seen.get(s['fixture'], 0); seen[s['fixture']] = n + 1
    (first if n == 0 else second).append(s['name'])
print(','.join(first), ','.join(second))
PY
)
common="--suite $SUITE --mode agent --planner-model $M --paraphrases 2 --jobs 4"
$B bench $common --only "$FIRST" --label $TAG-learn --set shape=record --set shape_dir="$D" > bench/results/$TAG-learn.log 2>&1
echo "learned: $(ls "$D" | wc -l | tr -d ' ') sites, $(du -sh "$D" | cut -f1)"
$B bench $common --only "$SECOND" --label $TAG-use --set shape=use --set shape_dir="$D" > bench/results/$TAG-use.log 2>&1 &
$B bench $common --only "$SECOND" --label $TAG-cold --set shape=off > bench/results/$TAG-cold.log 2>&1 &
wait
python3 bench/summary.py $TAG-cold $TAG-use
