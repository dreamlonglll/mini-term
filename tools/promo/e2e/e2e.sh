#!/usr/bin/env bash
# 一键跑一套宣传片 e2e:建演示项目 → 种数据目录 → 起 Xvfb + mini-term → 跑场景截图 → 关应用。
# 用法:e2e.sh <blue-hour|morning-mist> [最后切到哪张皮肤卡]
# 环境变量:
#   WORK       产物目录(默认 tools/promo/out/),截图落在 $WORK/shots/<皮肤>/
#   DEMO_HOME  演示用 HOME(默认 /home/dev;项目路径会出现在画面里)
#   MT_BIN     mini-term 可执行文件(默认 target/debug/mini-term)
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
THEME=$1; SWITCH=${2:-}
REPO=${REPO:-$(git -C "$HERE" rev-parse --show-toplevel)}
WORK=${WORK:-$HERE/../out}
DEMO_HOME=${DEMO_HOME:-/home/dev}
export MT_BIN=${MT_BIN:-$REPO/target/debug/mini-term} DEMO_HOME
DATA=$WORK/data-$THEME; SHOTS=$WORK/shots/$THEME
export DISPLAY=${DISPLAY:-:99}

[ -x "$MT_BIN" ] || { echo "找不到 $MT_BIN,先 cargo build -p mt-app" >&2; exit 1; }
if ! xdpyinfo >/dev/null 2>&1; then
  Xvfb "$DISPLAY" -screen 0 3840x2160x24 -nolisten tcp -dpi 192 >/dev/null 2>&1 &
  sleep 2
fi
pkill -x mini-term 2>/dev/null || true; sleep 1

bash "$HERE/make-projects.sh" "$DEMO_HOME" "$REPO"
python3 "$HERE/seed.py" "$DATA" "$DEMO_HOME" "$REPO" --theme "$THEME" --hook
rm -rf "$SHOTS"; mkdir -p "$SHOTS"

"$HERE/run-app.sh" "$DATA" "$WORK/app-$THEME.log" &
for _ in $(seq 1 60); do xdotool search --onlyvisible --name 'Mini-Term' >/dev/null 2>&1 && break; sleep 0.5; done
sleep 7   # 首帧 + 当前项目的 PTY 起好、提示符画出来

bash "$HERE/scenario.sh" "$SHOTS" "$SWITCH"
pkill -x mini-term 2>/dev/null || true
pkill -x xclip 2>/dev/null || true
echo "e2e($THEME) 完成:$(ls "$SHOTS" | wc -l) 张截图 → $SHOTS"
