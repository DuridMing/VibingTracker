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
echo "check.sh：全部通過"
