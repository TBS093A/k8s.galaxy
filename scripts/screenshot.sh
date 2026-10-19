#!/usr/bin/env bash
# Turn tmux captures into PNGs: scripts/screenshot.sh DIR renders DIR/*.ansi to DIR/*.png.
# Captures: tmux new-session -d -s kg -x 200 -y 52 "k8s-galaxy --demo"; tmux capture-pane -e -p -t kg > DIR/name.ansi
# Needs a headless Chromium (CHROME, default: Playwright's) and a Nerd Font for the glyphs.
set -euo pipefail
dir=$1
here=$(cd "$(dirname "$0")" && pwd)
chrome=${CHROME:-$(echo ~/.cache/ms-playwright/chromium_headless_shell-*/*/chrome-headless-shell | awk '{print $1}')}
for f in "$dir"/*.ansi; do
  b=${f%.ansi}
  python3 -I "$here/ansi2html.py" "$f" "$b.html"
  "$chrome" --headless --no-sandbox --hide-scrollbars --disable-gpu --window-size=1830,930 --screenshot="$b.png" "file://$b.html" > /dev/null 2>&1
done
