#!/bin/sh
# 整個專案的端到端驗收（#617）：裝 plugin 的兩個 Claude Code 同時跑，從 WebSocket 檢查合流與頻道。
# 需要：cargo、claude CLI。會實際呼叫模型（少量 token，預設 haiku，E2E_MODEL 可改）。
# 不碰正在跑的 collector（4800）與 .coding-lab：在 target/tmp/e2e-two-sessions/ 用隨機埠拉起專屬的 collector，跑完關掉（#749 T-7）。
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
cargo build --release -p coding-lab --manifest-path "$root/Cargo.toml"
exec python3 "$root/tests/e2e/two_sessions.py"
