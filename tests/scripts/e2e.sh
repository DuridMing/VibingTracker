#!/bin/sh
# 整個專案的端到端驗收（#617）：裝 plugin 的兩個 Claude Code 同時跑，從 WebSocket 檢查合流與頻道。
# 需要：release 版 coding-lab、正在跑的 collector（hook 埠 4800）、claude CLI。會實際呼叫模型（少量 token）。
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
cargo build --release -p coding-lab --manifest-path "$root/Cargo.toml"
exec python3 "$root/tests/e2e/two_sessions.py" "$@"
