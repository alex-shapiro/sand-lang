#!/bin/sh
# Renders static/apple-touch-icon.png and static/og.png from static/logo.svg
# with headless Chrome. Set CHROME to the browser's executable.
set -eu

site=$(cd "$(dirname "$0")" && pwd)
chrome=${CHROME:-"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
logo="file://$site/static/logo.svg"

render() { # HTML WIDTH HEIGHT OUT
    printf '%s' "$1" > "$tmp/page.html"
    "$chrome" --headless --disable-gpu --hide-scrollbars --allow-file-access-from-files \
        --window-size="$2,$3" --screenshot="$4" "file://$tmp/page.html" > /dev/null 2>&1
}

render "<!doctype html><body style=\"margin:0;width:180px;height:180px;background:#f8f6f0;display:grid;place-items:center\">
<img src=\"$logo\" style=\"height:156px\"></body>" 180 180 "$site/static/apple-touch-icon.png"

render "<!doctype html><body style=\"margin:0;width:1200px;height:630px;background:#f8f6f0;display:flex;align-items:center;gap:70px;padding:0 100px;box-sizing:border-box;font-family:'Avenir Next',Montserrat,system-ui,sans-serif;color:#1b2a41\">
<img src=\"$logo\" style=\"height:420px\">
<div><div style=\"font-weight:800;font-size:150px;letter-spacing:.06em;line-height:1\">SAND</div>
<div style=\"font-size:40px;line-height:1.3;margin-top:24px;color:#3a4759;font-family:system-ui,-apple-system,sans-serif\">A statically typed scripting language that runs untrusted code in a sandbox.</div>
<div style=\"display:inline-block;margin-top:28px;font-size:26px;font-family:system-ui,-apple-system,sans-serif;background:#1b2a41;color:#f3dfb8;padding:8px 18px;border-radius:10px\">Alpha software</div></div>
</body>" 1200 630 "$site/static/og.png"
