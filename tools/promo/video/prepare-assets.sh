#!/usr/bin/env bash
# 把 e2e 截图挑选、改名进 video/assets/(视频合成页只认这些固定文件名)。
# 用法:prepare-assets.sh <e2e 产物目录(含 shots/blue-hour 与 shots/morning-mist)> <仓库根>
set -euo pipefail
SRC=$1; REPO=$2
HERE=$(cd "$(dirname "$0")" && pwd)
A=$HERE/assets; mkdir -p "$A/bh" "$A/mm"
B=$SRC/shots/blue-hour; M=$SRC/shots/morning-mist
pick() { cp "$1" "$2"; }
pick "$B/02-hero-${HERO_BH:-3}.png"   "$A/bh/hero.png"
pick "$B/02-hero-${HERO_BH:-3}.png"   "$A/bh/hero-toast.png"
pick "$B/06-git.png"                 "$A/bh/git.png"
pick "$B/07-sessions.png"            "$A/bh/sessions.png"
pick "$B/04-hover.png"               "$A/bh/hover.png"
pick "$B/08-usage-hover.png"         "$A/bh/usage.png"
pick "$B/09-markdown.png"            "$A/bh/markdown.png"
pick "$B/10-mermaid.png"             "$A/bh/mermaid.png"
pick "$B/11-search.png"              "$A/bh/search.png"
pick "$B/12-commands.png"            "$A/bh/commands.png"
pick "$B/13-switcher.png"            "$A/bh/switcher.png"
pick "$B/14-ssh.png"                 "$A/bh/ssh.png"
pick "$B/15-settings-theme.png"      "$A/bh/settings-theme.png"
pick "$B/16-settings-switched.png"   "$A/bh/settings-switched.png"
pick "$B/17-switched-main.png"       "$A/bh/switched-main.png"
pick "$M/02-hero-${HERO_MM:-3}.png"   "$A/mm/hero.png"
pick "$M/09-markdown.png"            "$A/mm/markdown.png"
pick "$M/08-usage.png"               "$A/mm/usage.png"
pick "$M/06-git.png"                 "$A/mm/git.png"
cp "$REPO/docs/icon.png" "$A/icon.png"
cp "$REPO/theme/blue-hour/background.jpg" "$A/bh-bg.jpg"
echo "assets ready: $(ls "$A"/bh | wc -l) + $(ls "$A"/mm | wc -l)"
