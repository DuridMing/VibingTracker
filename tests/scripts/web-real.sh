#!/bin/sh
# 用前端的模組唯讀連真的 collector（ws://127.0.0.1:4801），不改動 collector 或任何檔案。
#   check [秒數]：核對契約 §6 的欄位、串流順序、卡片內容（tests/e2e/web/realcheck.mjs）
#   diff [筆數]：抽查改動紀錄裡 Edit 的 diff 行號與目前檔案一致（tests/e2e/web/realdiff.mjs，#640 驗收）
#   complete [session 數] [起始時間 epoch ms]：串流（含載入更多）與 collector 的 JSONL 逐筆比對 pos（realcomplete.mjs，#639 驗收）
#     JSONL 在 ${CODING_LAB_HOME:-<主工作樹>/.coding-lab}/events
# 需要：podman、正在跑的 collector。前端目錄同 web-shot.sh（WEB_DIR）。
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
web=$(cd "${WEB_DIR:-$root/web}" && pwd)
# 事件裡的路徑相對於 session 的 cwd（主工作樹）：把主工作樹唯讀掛進容器，同一個路徑。
# 專案檔案的 SELinux 標籤是 user_home_t，容器預設讀不到；不想重標整個專案（:z 會改所有檔案的標籤），
# 所以這個容器關掉標籤隔離（label=disable），所有掛載都是唯讀。
main=$(dirname "$(git -C "$root" rev-parse --path-format=absolute --git-common-dir)")
case "${1:-}" in
  check) js=realcheck.mjs ;;
  diff) js=realdiff.mjs ;;
  complete) js=realcomplete.mjs ;;
  *) echo "用法：$0 check [秒數] | diff [筆數] | complete [session 數] [起始時間 epoch ms]" >&2; exit 2 ;;
esac
shift
extra=""
if [ "$js" = realcomplete.mjs ]; then
  home=$(cd "${CODING_LAB_HOME:-$main/.coding-lab}" && pwd)
  set -- "$home/events" "$@"
  # 主工作樹已經唯讀掛進去了；資料目錄在外面時才另外掛（同一個路徑掛兩次會讀不到）
  case "$home" in "$main"/*) ;; *) extra="-v $home:$home:ro" ;; esac
fi
exec podman run --rm --network host --security-opt label=disable \
  -v "$web:/web:ro" -v "$root/tests/e2e/web:/e2e:ro" -v "$main:$main:ro" $extra \
  docker.io/library/node:22.23.3-alpine node "/e2e/$js" "$@"
