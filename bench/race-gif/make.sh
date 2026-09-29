#!/usr/bin/env bash
# Records one race scenario side by side and writes docs/race.gif.
#   bench/race-gif/make.sh [scenario] [suite]
set -euo pipefail
cd "$(dirname "$0")/../.."
SCENARIO=${1:-tennis_customize_october}
SUITE=${2:-bench/demo.toml}
WORK=$(mktemp -d)
node bench/race-gif/record.mjs "$WORK/rec" target/release/fab-bench race --suite "$SUITE" --only "$SCENARIO" \
  --right agent --planner-model inception/mercury-2.5 --pause 1500 --hold 3
python3 bench/race-gif/compose.py "$WORK/rec" "$WORK/frames" 8 560
ffmpeg -hide_banner -loglevel error -y -framerate 8 -i "$WORK/frames/%05d.png" \
  -vf "split[a][b];[a]palettegen=max_colors=128:stats_mode=diff[p];[b][p]paletteuse=dither=bayer:bayer_scale=4:diff_mode=rectangle" \
  docs/race.gif
ls -la docs/race.gif
echo "work files: $WORK"
