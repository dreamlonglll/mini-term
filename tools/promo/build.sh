#!/usr/bin/env bash
# 宣传片一键产出:e2e 实拍(两套皮肤)→ 挑图 → 逐帧合成 → 配乐 → 编码交付物。
# 用法:tools/promo/build.sh [--skip-e2e]
#   --skip-e2e  复用 $WORK/shots 里已有的截图,只重做视频(调时间轴 / 字幕时用)
# 产物:docs/promo/mini-term-promo.mp4(1080p + 配乐);README 里的播放器要把它重新传到 GitHub(见 README.md)
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(git -C "$HERE" rev-parse --show-toplevel)
export WORK=${WORK:-$HERE/out}
OUT_DIR=${OUT_DIR:-$REPO/docs/promo}
mkdir -p "$WORK" "$OUT_DIR"

if [ "${1:-}" != --skip-e2e ]; then
  bash "$HERE/e2e/e2e.sh" blue-hour morning-mist     # 蓝调时分全流程,结尾实时换成晨雾
  bash "$HERE/e2e/e2e.sh" morning-mist blue-hour     # 晨雾全流程,结尾换回蓝调
fi

bash "$HERE/video/prepare-assets.sh" "$WORK" "$REPO"
node "$HERE/video/render.mjs" "$HERE/video/compose.html" "$WORK/master.mp4" 30
DUR=$(grep -oP 'window\.DURATION = \K[0-9.]+' "$HERE/video/scenes.js")
python3 "$HERE/video/music.py" "$WORK/music.wav" "$DUR"

# MP4 交付物:H.264 High + AAC,faststart(网页边下边播);体积压在 10 MB 内,
# 可以直接拖进 GitHub 网页编辑器换成站内播放器(免费账户的视频上限 10 MB)。
# 镜头全程缓慢推拉、几乎每帧都在变,CRF 31 + aq-mode=3 时 1080p 文字依旧清晰。
ffmpeg -y -loglevel error -i "$WORK/master.mp4" -i "$WORK/music.wav" -map 0:v -map 1:a \
  -c:v libx264 -preset veryslow -crf "${CRF:-31}" -x264-params aq-mode=3 -profile:v high -pix_fmt yuv420p \
  -c:a aac -b:a 96k -shortest -movflags +faststart "$OUT_DIR/mini-term-promo.mp4"

ls -lh "$OUT_DIR"
echo "下一步:把 $OUT_DIR/mini-term-promo.mp4 拖进 GitHub 任意评论框,换掉 README.md / README.en.md 里的 user-attachments 链接"
