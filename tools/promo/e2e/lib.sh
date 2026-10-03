# e2e 辅助函数(source 进来用)。坐标一律是**逻辑像素**(1920×1080),内部乘缩放因子。
: "${DISPLAY:=:99}"; export DISPLAY
: "${SCALE:=2}"
: "${SHOTS:=$PWD/shots}"
mkdir -p "$SHOTS"

win() { xdotool search --onlyvisible --name 'Mini-Term' 2>/dev/null | head -1; }
focus() { local w; w=$(win); [ -n "$w" ] && xdotool windowfocus --sync "$w" 2>/dev/null; true; }
px() { echo $(( $1 * SCALE )); }
move() { xdotool mousemove "$(px "$1")" "$(px "$2")"; }
click() { move "$1" "$2"; sleep 0.15; xdotool click "${3:-1}"; }
dclick() { move "$1" "$2"; sleep 0.15; xdotool click --repeat 2 --delay 80 1; }
rclick() { click "$1" "$2" 3; }
key() { xdotool key --clearmodifiers "$@"; }
typ() { xdotool type --delay "${TYPE_DELAY:-35}" -- "$1"; }
wait_s() { sleep "$1"; }
shot() { # shot <name>:整屏抓图(设备像素,3840×2160)
  import -display "$DISPLAY" -window root "$SHOTS/$1.png" && echo "📸 $1"
}
