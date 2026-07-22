#!/usr/bin/env bash
# Render every repro fixture in obscura and Chromium side by side.
# Usage: ./run.sh [outdir]   (default: ./out)
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${OBSCURA_BIN:-$ROOT/target/release/obscura}"
CHROME="${CHROME_BIN:-chromium}"
DIR="$(cd "$(dirname "$0")" && pwd)"
OUT="${1:-$DIR/out}"
mkdir -p "$OUT"
STAGE="$(mktemp -d "$ROOT/.render-repros-chrome.XXXXXX")"
trap 'rm -rf -- "$STAGE"' EXIT
status=0
for f in "$DIR"/*.html; do
  n=$(basename "$f" .html)
  if ! OBSCURA_SHOT_W=900 OBSCURA_SHOT_H=1000 OBSCURA_ALLOW_PRIVATE_NETWORK=1 \
    timeout 60 "$BIN" fetch "file://$f" --screenshot "$OUT/$n.obscura.png" \
      --timeout 30000 --wait 2 >"$OUT/$n.obscura.log" 2>&1 || [[ ! -s "$OUT/$n.obscura.png" ]]; then
    echo "FAILED obscura: $n (see $OUT/$n.obscura.log)" >&2
    status=1
    continue
  fi

  # Chromium's snap package has a private /tmp, so stage under the checkout
  # and move the completed screenshot into OUT. A fresh profile prevents an
  # existing browser process from capturing the command and masking failure.
  chrome_shot="$STAGE/$n.png"
  if ! timeout 60 "$CHROME" --headless --disable-gpu --no-sandbox --hide-scrollbars \
    --disable-background-networking --user-data-dir="$STAGE/$n-profile" \
    --virtual-time-budget=2000 --force-device-scale-factor=1 --window-size=900,1000 \
    --screenshot="$chrome_shot" "file://$f" >"$OUT/$n.chrome.log" 2>&1 || [[ ! -s "$chrome_shot" ]]; then
    echo "FAILED chromium: $n (see $OUT/$n.chrome.log)" >&2
    status=1
    continue
  fi
  mv "$chrome_shot" "$OUT/$n.chrome.png"
  echo "rendered $n"
done
if [[ "$status" -eq 0 ]]; then
  echo "output in $OUT"
else
  echo "one or more renders failed; partial output in $OUT" >&2
fi
exit "$status"
