#!/usr/bin/env bash
# 用法:run-app.sh <data_dir> [log]
# 在 Xvfb($DISPLAY,默认 :99)上以 2 倍缩放、软件 Vulkan(lavapipe)启动 mini-term。
# 环境整个清空再给:HOME 指向演示目录,应用改不到本机真实的 ~/.claude 等配置。
DATA=$1; LOG=${2:-/dev/null}
BIN=${MT_BIN:?MT_BIN 未设置}
H=${DEMO_HOME:-/home/dev}
mkdir -p /tmp/xdg-demo && chmod 700 /tmp/xdg-demo
cd "$H"
exec env -i PATH="$H/.local/bin:/usr/local/bin:/usr/bin:/bin" HOME="$H" USER=dev LOGNAME=dev SHELL=/bin/bash \
  LANG=C.UTF-8 LC_ALL=C.UTF-8 DISPLAY="${DISPLAY:-:99}" XDG_RUNTIME_DIR=/tmp/xdg-demo GPUI_X11_SCALE_FACTOR="${SCALE:-2}" \
  VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json MT_APP_DATA_DIR="$DATA" \
  "$BIN" >"$LOG" 2>&1
