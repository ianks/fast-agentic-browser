#!/bin/bash
# Main comparison: control (chrome-devtools-mcp) vs experiment (fab act/run/collect,
# legacy engine + P1 contract) vs goal (fab `do`, dvm engine), strict checks,
# chosen cheap planners × 3 goal wordings, plus the strong gate model on hard canonical goals.
cd "$(dirname "$0")/.."
B=./target/release/fab
CHEAP=inception/mercury-2.5,stepfun/step-3.7-flash
STRONG=openai/gpt-6-sol
T=${TAG:-v1}
run() { # suite arm models paraphrases label [extra]
  $B $6 bench --suite "$1" --mode "$2" --planner-model "$3" --paraphrases "$4" --label "$5" > bench/results/$5.log 2>&1
  echo "done $5: $(/usr/bin/grep -c ' PASS ' bench/results/$5.log)/$(/usr/bin/grep -cE ' (PASS|FAIL) ' bench/results/$5.log)"
}
for arm in control experiment goal; do
  extra=""; [ $arm = goal ] && extra="--set engine=dvm"
  run bench/scenarios.toml $arm $CHEAP 3 cmp-$T-comp-$arm "$extra" &
  run bench/hard.toml      $arm $CHEAP 3 cmp-$T-hard-$arm "$extra" &
done
wait
for arm in control experiment goal; do
  extra=""; [ $arm = goal ] && extra="--set engine=dvm"
  run bench/hard.toml $arm $STRONG 1 cmp-$T-hard-$arm-strong "$extra" &
done
wait
