#!/usr/bin/env bash
# 从一轮 e2e 截图里挑出视频要用的 18 张,压成 WebP 写进素材库 tools/promo/assets/(入库)。
# 合成页(scenes.js)只读素材库,所以只改字幕 / 节奏 / 配乐时不必重跑 e2e。
# 用法:prepare-assets.sh <e2e 产物目录(含 shots/blue-hour 与 shots/morning-mist)>
#   HERO_BH / HERO_MM:主视觉用第几张连拍(02-hero-N),挑 toast 与 diff 同框的那张
set -euo pipefail
SRC=$1
HERE=$(cd "$(dirname "$0")" && pwd)
A=$HERE/../assets
B=$SRC/shots/blue-hour; M=$SRC/shots/morning-mist
mkdir -p "$A/blue-hour" "$A/morning-mist"

# WebP q92:4K 截图逐像素看与 PNG 无差(PSNR ≈ 37 dB),体积约为 PNG 的四分之一
webp() { ffmpeg -y -loglevel error -i "$1" -c:v libwebp -quality 92 -compression_level 6 "$2"; }

webp "$B/02-hero-${HERO_BH:-3}.png"  "$A/blue-hour/hero.webp"
webp "$B/06-git.png"                "$A/blue-hour/git.webp"
webp "$B/07-sessions.png"           "$A/blue-hour/sessions.webp"
webp "$B/04-hover.png"              "$A/blue-hour/hover.webp"
webp "$B/08-usage-hover.png"        "$A/blue-hour/usage.webp"
webp "$B/09-markdown.png"           "$A/blue-hour/markdown.webp"
webp "$B/10-mermaid.png"            "$A/blue-hour/mermaid.webp"
webp "$B/11-search.png"             "$A/blue-hour/search.webp"
webp "$B/12-commands.png"           "$A/blue-hour/commands.webp"
webp "$B/13-switcher.png"           "$A/blue-hour/switcher.webp"
webp "$B/14-ssh.png"                "$A/blue-hour/ssh.webp"
webp "$B/15-settings-theme.png"     "$A/blue-hour/settings-theme.webp"
webp "$B/16-settings-switched.png"  "$A/blue-hour/settings-switched.webp"
webp "$B/17-switched-main.png"      "$A/blue-hour/switched-main.webp"
webp "$M/02-hero-${HERO_MM:-3}.png"  "$A/morning-mist/hero.webp"
webp "$M/09-markdown.png"           "$A/morning-mist/markdown.webp"
webp "$M/08-usage.png"              "$A/morning-mist/usage.webp"
webp "$M/06-git.png"                "$A/morning-mist/git.webp"
echo "素材库已更新:$(find "$A" -name '*.webp' | wc -l) 张 → $A"
