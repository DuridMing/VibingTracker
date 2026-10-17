#!/bin/sh
# 多 session 長時間的真實驗收（#658）：N 個裝 plugin 的 Claude Code 同時跑 D 分鐘，逐行比對、Edit 行號抽查、collector CPU，
# 中途 SIGKILL collector。需要：cargo、claude CLI。會實際呼叫模型（預設 haiku，N=16、D=30 約 500 次 claude -p）。
# 不碰正在跑的 collector（4800）與 .coding-lab：資料目錄在 target/tmp/e2e-multi/。
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
cargo build --release -p coding-lab --manifest-path "$root/Cargo.toml"
# Windows 的 python3 可能是 Microsoft Store 的佔位程式，要真的跑得動才算（同 scripts/check.sh，#717）
for py in python3 python "py -3"; do
    if $py -c pass >/dev/null 2>&1; then exec $py "$root/tests/e2e/multi_sessions.py" "$@"; fi
done
echo "找不到能跑的 Python（試過 python3、python、py -3）" >&2
exit 1
