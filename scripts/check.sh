#!/usr/bin/env bash
# commit 前必跑：格式、lint、測試。任何一步失敗就停。
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

echo "== cargo fmt"
cargo fmt --all --check
echo "== cargo clippy"
cargo clippy --workspace --all-targets --locked -- -D warnings
echo "== cargo test"
cargo test --workspace --locked
# 前端純邏輯測試（web/test/）：開發機沒有 Node，走 podman；沒有 podman 但有 Node（例如 Windows）就直接跑；兩個都沒有才跳過
echo "== web test"
if command -v podman >/dev/null 2>&1; then
  podman run --rm --pull=missing -v "$PWD/web:/web:Z,ro" -w /web \
    docker.io/library/node:22-alpine node --test
elif command -v node >/dev/null 2>&1; then
  (cd web && node --test)
else
  echo "（沒有 podman 也沒有 Node，跳過前端測試）"
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
