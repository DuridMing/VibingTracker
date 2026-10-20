#!/bin/sh
# resume／compact 接回的真實操作驗收（#651、#688）：同一個 session resume 三次、compact 三次，和 transcript 逐行比對。
# 需要：cargo、claude CLI。會實際呼叫模型（少量 token）。
# 不碰正在跑的 collector（4800）與 .coding-lab：用剛建好的 release 在 target/tmp/e2e-lifecycle/ 拉起專屬的 collector，跑完關掉。
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
cargo build --release -p coding-lab --manifest-path "$root/Cargo.toml"
exec python3 "$root/tests/e2e/lifecycle.py"
