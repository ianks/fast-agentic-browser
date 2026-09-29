#!/bin/bash
# v2: fab arms with all post-v1 fixes (dvm engine for both), paired against
# v1 control (unchanged toolset). Then the held-out suite, all three arms.
cd "$(dirname "$0")/.."
B=${B:-./target/release/fab}
CHEAP=inception/mercury-2.5,stepfun/step-3.7-flash
STRONG=openai/gpt-6-sol
run() { # suite arm models paraphrases label
  $B --set engine=dvm bench --suite "$1" --mode "$2" --planner-model "$3" --paraphrases "$4" --label "$5" > bench/results/$5.log 2>&1
  echo "done $5: $(/usr/bin/grep -cE '\] #[0-9]+ PASS' bench/results/$5.log)/$(/usr/bin/grep -cE '\] #[0-9]+ (PASS|FAIL)' bench/results/$5.log)"
}
for arm in experiment goal; do
  run bench/scenarios.toml $arm $CHEAP 3 cmp-v2-comp-$arm &
  run bench/hard.toml      $arm $CHEAP 3 cmp-v2-hard-$arm &
done
wait
for arm in experiment goal; do run bench/hard.toml $arm $STRONG 1 cmp-v2-hard-$arm-strong & done
# Held-out (never tuned on): canonical goals, both cheap planners, all three arms.
for arm in control experiment goal; do run bench/heldout.toml $arm $CHEAP 1 heldout-v2-$arm & done
wait
