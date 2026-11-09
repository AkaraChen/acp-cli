#!/bin/sh
# Render tools/og.html to assets/og.png (1200x630) with any headless Chromium.
set -e
cd "$(dirname "$0")/.."
chrome="${CHROME:-$(ls -d "$HOME"/.cache/ms-playwright/chromium-*/chrome-linux*/chrome 2>/dev/null | tail -1)}"
"$chrome" --headless --no-sandbox --disable-gpu --hide-scrollbars --allow-file-access-from-files \
  --force-device-scale-factor=1 --window-size=1200,630 --virtual-time-budget=5000 \
  --screenshot="$PWD/assets/og.png" "file://$PWD/tools/og.html"
