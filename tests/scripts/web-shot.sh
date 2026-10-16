#!/bin/sh
# 前端 headless 截圖：起 tests/e2e/web/shotserver.py（127.0.0.1:${SHOT_PORT:-4803}），用 Firefox --screenshot 拍 /shot.html?<query>。
# 用法：tests/scripts/web-shot.sh <輸出.png> '<query>'
#   例：tests/scripts/web-shot.sh target/tmp/web-e2e/mock.png 'mock=12&prefill&delay=8'
#   query 見 web/js/main.js：mock=<n>、prefill、bench、isolate、freeze=…；delay=<秒> 是截圖前等多久。
# 前端目錄預設 <repo>/web；feature/web 還沒併進 develop 前用 WEB_DIR=<路徑>/worktrees/feature/web/web。
# 需要：python3、firefox。暫存檔放 target/tmp/web-e2e/，不用系統 /tmp。
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
web=$(cd "${WEB_DIR:-$root/web}" && pwd)
port=${SHOT_PORT:-4803}
[ $# -eq 2 ] || { echo "用法：$0 <輸出.png> '<query>'" >&2; exit 2; }
out=$1
query=$2
[ -f "$web/index.html" ] || { echo "找不到前端：$web/index.html（用 WEB_DIR 指定）" >&2; exit 2; }
tmp=$root/target/tmp/web-e2e
mkdir -p "$tmp" "$(dirname "$out")"
out=$(cd "$(dirname "$out")" && pwd)/$(basename "$out") # Firefox 的 --screenshot 不認相對路徑（不報錯、也不產生檔案）
profile=$tmp/ffprofile.$$
python3 -I "$root/tests/e2e/web/shotserver.py" "$web" "$port" "${RESULT_FILE:-$tmp/result.txt}" &
pid=$!
trap 'kill $pid 2>/dev/null; rm -rf "$profile"' EXIT INT TERM
mkdir -p "$profile"
# 等 server 起來
i=0
until curl -s -o /dev/null "http://127.0.0.1:$port/index.html"; do
  i=$((i + 1)); [ $i -lt 50 ] || { echo "shotserver 沒起來（埠 $port 被占用？）" >&2; exit 1; }
  sleep 0.1
done
timeout 300 firefox --headless --profile "$profile" --no-remote --window-size=1600,1000 \
  --screenshot "$out" "http://127.0.0.1:$port/shot.html?$query" >/dev/null 2>&1 || true
[ -s "$out" ] || { echo "沒有產生截圖：$out" >&2; exit 1; }
echo "截圖：$out"
