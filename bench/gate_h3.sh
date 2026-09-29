#!/bin/bash
# Final gate: blind held-out-3 (written by an isolated agent that never saw
# engine code or results; never used for tuning). All three arms, both cheap
# planners, canonical + one paraphrase, arms concurrent (shared machine load).
cd "$(dirname "$0")/.."
M="inception/mercury-2.5,stepfun/step-3.7-flash" TAG=${TAG:-g3} SUITES="heldout3:bench/heldout3.toml" ARMS="control goal agent" EXTRA="--jobs 4 --paraphrases 2" ./bench/compare_v3.sh
