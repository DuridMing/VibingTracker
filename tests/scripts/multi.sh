#!/bin/sh
# 多 session 長時間的真實驗收（#658）：N 個裝 plugin 的 Claude Code 同時跑 D 分鐘，逐行比對、Edit 行號抽查、collector CPU，
# 中途 SIGKILL collector。需要：cargo、claude CLI。會實際呼叫模型（預設 haiku，N=16、D=30 約 500 次 claude -p）。
# 不碰正在跑的 collector（4800）與 .coding-lab：資料目錄在 target/tmp/e2e-multi/。
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
cargo build --release -p coding-lab --manifest-path "$root/Cargo.toml"
exec python3 "$root/tests/e2e/multi_sessions.py" "$@"
