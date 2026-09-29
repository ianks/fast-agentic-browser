#!/bin/bash
# Final gate: the blind held-out-2 suite (written by an isolated agent that
# never saw engine code or results; never used for tuning). All three arms,
# both cheap planners, canonical + one paraphrase. Arms run concurrently so
# they share machine load.
cd "$(dirname "$0")/.."
M="inception/mercury-2.5,stepfun/step-3.7-flash" TAG=${TAG:-g2} SUITES="heldout2:bench/heldout2.toml" ARMS="control goal agent" EXTRA="--jobs 4 --paraphrases 2" ./bench/compare_v3.sh
