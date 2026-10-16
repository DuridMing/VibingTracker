#!/bin/sh
# resume／compact 接回的真實操作驗收（#651）：同一個 session resume 三次、compact 三次，和 transcript 逐行比對。
# 需要：release 版 coding-lab、正在跑的 collector（hook 埠 4800）、claude CLI。會實際呼叫模型（少量 token）。
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
cargo build --release -p coding-lab --manifest-path "$root/Cargo.toml"
exec python3 "$root/tests/e2e/lifecycle.py" "$@"
