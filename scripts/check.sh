#!/usr/bin/env bash
# commit 前必跑：格式、lint、測試。任何一步失敗就停。
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

# 測試要用到的外部工具：缺的話一開始就說清楚，不要跑到一半才紅燈（#769 R-20）
# zip／unzip／sha256sum 只有 unix 上的 wrapper 下載測試用到（tests/plugin.rs 的 cfg(unix)），Windows 不檢查
need="cargo git tar"
case "$(uname -s 2>/dev/null)" in MINGW* | MSYS* | CYGWIN*) ;; *) need="$need zip unzip sha256sum" ;; esac
missing=""
for t in $need; do
  command -v "$t" >/dev/null 2>&1 || missing="$missing $t"
done
if [ -n "$missing" ]; then
  echo "check.sh 需要這些指令，這台機器沒有：$missing"
  echo "（Linux 用套件管理員裝；Windows 的 Git for Windows 沒有 zip／unzip，可從 MSYS2 或 GnuWin 補上）"
  exit 1
fi

echo "== cargo fmt"
cargo fmt --all --check
echo "== cargo clippy"
cargo clippy --workspace --all-targets --locked -- -D warnings
echo "== cargo test"
cargo test --workspace --locked
# 前端純邏輯測試（web/test/）：開發機沒有 Node，走 podman；沒有 podman 但有 Node（例如 Windows）就直接跑；兩個都沒有才跳過。
# 映像釘版本（#749 T-6）；離線又沒有這個映像時退到本機的 Node，再沒有才跳過並說清楚（不讓整個 check.sh 因為網路紅掉）
echo "== web test"
NODE_IMAGE=docker.io/library/node:22.23.3-alpine
if command -v podman >/dev/null 2>&1 && { podman image exists "$NODE_IMAGE" || podman pull -q "$NODE_IMAGE" >/dev/null 2>&1; }; then
  podman run --rm -v "$PWD/web:/web:Z,ro" -w /web "$NODE_IMAGE" node --test
elif command -v node >/dev/null 2>&1; then
  (cd web && node --test)
else
  echo "（沒有 podman 映像 $NODE_IMAGE（離線拉不到）也沒有 Node：跳過前端測試，連上網路後再跑一次）"
fi
# 端到端腳本裡不呼叫模型的純邏輯（tests/e2e/test_*.py）。
# Windows 的 python3 可能是 Microsoft Store 的佔位程式（在 PATH 裡但跑不動，exit 49）：要真的跑一次才算找到（#711）
echo "== e2e 腳本單元測試"
py=""
for candidate in python3 python "py -3"; do
  if $candidate -c pass >/dev/null 2>&1; then py=$candidate; break; fi
done
[ -n "$py" ] || { echo "找不到能跑的 Python（試過 python3、python、py -3）"; exit 1; }
$py -m unittest discover -s tests/e2e -p 'test_*.py'
echo "check.sh：全部通過"
