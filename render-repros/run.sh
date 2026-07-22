#!/usr/bin/env bash
# Render every repro fixture in obscura and Chromium side by side.
# Usage: ./run.sh [outdir]   (default: ./out)
set -u
BIN="${OBSCURA_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/release/obscura}"
DIR="$(cd "$(dirname "$0")" && pwd)"
OUT="${1:-$DIR/out}"
mkdir -p "$OUT"
for f in "$DIR"/*.html; do
  n=$(basename "$f" .html)
  OBSCURA_SHOT_W=900 OBSCURA_SHOT_H=1000 OBSCURA_ALLOW_PRIVATE_NETWORK=1 \
    timeout 60 "$BIN" fetch "file://$f" --screenshot "$OUT/$n.obscura.png" --timeout 30000 --wait 2 >/dev/null 2>&1
  timeout 60 google-chrome-stable --headless --disable-gpu --no-sandbox --hide-scrollbars \
    --force-device-scale-factor=1 --window-size=900,1000 --screenshot="$OUT/$n.chrome.png" "file://$f" >/dev/null 2>&1
  echo "rendered $n"
done
echo "output in $OUT"
