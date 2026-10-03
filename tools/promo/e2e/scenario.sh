#!/usr/bin/env bash
# 宣传片 e2e 场景:驱动正在运行的 mini-term(Xvfb :99,2 倍缩放),逐步截图。
# 用法:scenario.sh <shots_dir> [switch_to_theme_card]
#   switch_to_theme_card:最后在「设置 → 主题与语言」里点哪张皮肤卡(blue-hour | morning-mist),
#   截下实时换肤前后两张;省略则不换。
# 坐标一律是逻辑像素(1920×1080),对应 seed.py 种出来的布局;改了种子布局要同步这里。
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
SHOTS=$1; SWITCH_TO=${2:-}
export SHOTS LC_ALL=C.UTF-8 LANG=C.UTF-8
source "$HERE/lib.sh"

# ---- 布局坐标(逻辑像素)
ROW_ORBIT="124 109"; ROW_AURORA="124 142"
PANE_L="800 520"; PANE_RT="1550 300"; PANE_RB="1550 830"   # orbit-api:左大 / 右上 / 右下
AURORA_L="760 520"; AURORA_R="1500 520"                     # aurora-web:左右两格
ACT_GIT="22 127"; ACT_USAGE="22 214"; ACT_SETTINGS="22 249"; ACT_SSH="22 285"
TAB_SESSIONS="1572 50"; DRAWER_CLOSE="1902 50"
USAGE_CLOSE="1701 129"
FILE_ARCH="137 746"; WB_TERMINAL_TAB="430 48"; MERMAID="1150 370"
SET_THEME_PAGE="448 326"; SET_CLOSE="1501 134"
CARD_BLUE="728 567"; CARD_MIST="1035 567"

paste_line() { printf '%s' "$1" | xclip -selection clipboard -i >/dev/null 2>&1; key ctrl+v; sleep 0.35; key Return; }
run_ai() { # run_ai <cli> <提示词>:像人一样键入命令,等横幅出来再贴提示词回车
  typ "$1"; key Return; sleep 1.8
  paste_line "$2"
}
burst() { # burst <name> <n> <间隔秒>:连拍,事后挑最好的一张
  local i; for i in $(seq 1 "$2"); do shot "$1-$i"; sleep "$3"; done
}

focus
sleep 1
shot 00-start

# ---- 1. aurora-web:后台让 Codex 写单测(约 21s 后完成 → 弹「完成」toast)
click $ROW_AURORA; sleep 3.5
click $AURORA_L; sleep 0.3
run_ai codex "给 Dashboard 写单测"
click $AURORA_R; sleep 0.3
typ "gl"; key Return
sleep 1.5
shot 01-aurora

# ---- 2. 回到 orbit-api,三格各起一个会话
click $ROW_ORBIT; sleep 2.5
click $PANE_RT; sleep 0.3
run_ai codex "review 限流中间件"                 # ≈7s 后完成 → 绿勾
click $PANE_RB; sleep 0.3
run_ai claude "把 orbit-api 部署到 staging"       # ≈6s 后请求授权 → 橙色叹号
click $PANE_L; sleep 0.3
run_ai claude "修复订单查询的 N+1 问题"           # ≈18s,全程转圈
move 1200 1060                                    # 鼠标挪开,别挡画面也别触发悬停
sleep 5
burst 02-hero 8 1.2                               # aurora-web 完成的 toast 落在这段里
sleep 4
burst 03-done 2 1.5

# ---- 3. 项目行悬停预览(aurora-web 上跑过的 AI 会话)
move 96 138; sleep 0.25; move 108 141; sleep 0.25; move 120 143; sleep 2.2
shot 04-hover
move 1200 1060; sleep 0.8

# ---- 4. 批准部署 → 继续跑完
click $PANE_RB; sleep 0.3
typ "1"; key Return
sleep 2.5
shot 05-approved
sleep 4

# ---- 5. Git 抽屉 / AI 历史
click $ACT_GIT; sleep 3
shot 06-git
click $TAB_SESSIONS; sleep 3
shot 07-sessions
click $DRAWER_CLOSE; sleep 1.2

# ---- 6. 使用统计
click $ACT_USAGE; sleep 6
shot 08-usage
move 1400 470; sleep 1.5
shot 08-usage-hover
click $USAGE_CLOSE; sleep 1.2

# ---- 7. Markdown 预览 + mermaid 放大
click $FILE_ARCH; sleep 3.5
shot 09-markdown
click $MERMAID; sleep 2.5
shot 10-mermaid
key Escape; sleep 1
click $WB_TERMINAL_TAB; sleep 1.5

# ---- 8. 全局搜索 / 命令库 / 项目切换 / SSH
key ctrl+shift+f; sleep 1.5
click 637 188; sleep 0.6                          # 切到「内容」模式
typ "order"; key Return; sleep 3.5
shot 11-search
key Escape; sleep 1
click $PANE_L; sleep 0.3
key ctrl+shift+k; sleep 2
shot 12-commands
key Escape; sleep 1
key ctrl+shift+p; sleep 2
shot 13-switcher
key Escape; sleep 1
click $ACT_SSH; sleep 2.5
shot 14-ssh
key Escape; sleep 1

# ---- 9. 设置 → 主题与语言(→ 实时换肤)
click $ACT_SETTINGS; sleep 2.5
click $SET_THEME_PAGE; sleep 2.5
shot 15-settings-theme
if [ -n "$SWITCH_TO" ]; then
  if [ "$SWITCH_TO" = morning-mist ]; then click $CARD_MIST; else click $CARD_BLUE; fi
  sleep 3
  shot 16-settings-switched
  click $SET_CLOSE; sleep 2.5
  shot 17-switched-main
else
  click $SET_CLOSE; sleep 1.5
fi
echo "scenario done → $SHOTS"
