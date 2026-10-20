#!/bin/sh
# 前端互動自我測試（tests/e2e/web/selftest.js）：mock＋prefill，在 headless Firefox 用真的 DOM 點擊、捲動、按鍵，
# 驗合流串流（展開、篩選、新事件、載入更多、3000 行上限）、「即時」浮動視窗、鍵盤、改動紀錄與 diff。
# 結果由頁面 POST 回 shotserver，全部 PASS 才 exit 0。用法：tests/scripts/web-selftest.sh [截圖.png]
# 前端目錄與需求同 web-shot.sh（WEB_DIR、python3、firefox）。約 45 秒。
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
tmp=$root/target/tmp/web-e2e
mkdir -p "$tmp"
result=$tmp/selftest.txt
rm -f "$result"
out=${1:-$tmp/selftest.png}
RESULT_FILE=$result "$root/tests/scripts/web-shot.sh" "$out" "mock=16&prefill&selftest&delay=40"
[ -s "$result" ] || { echo "沒有收到自我測試結果（看截圖：$out）" >&2; exit 1; }
cat "$result"
grep -q '^DONE' "$result" || { echo "自我測試沒有跑完" >&2; exit 1; }
if grep -q '^FAIL\|^ERROR' "$result"; then exit 1; fi
echo "全部通過"
