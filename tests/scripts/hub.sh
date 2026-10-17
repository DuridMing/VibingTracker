#!/bin/sh
# P6 hub 的單機驗收（#663、#715）：兩個 collector 在不同埠與資料目錄當兩台上游，kill 一台看離線與重連。
# 需要：cargo、python3。不碰 ~/.coding-lab 與 4800／4801，不呼叫模型。
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
cargo build --release -p coding-lab --manifest-path "$root/Cargo.toml"
exec python3 "$root/tests/e2e/hub_sim.py" "$@"
