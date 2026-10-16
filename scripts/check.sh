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
# 前端純邏輯測試（web/test/）：主機沒有 Node，走 podman；沒有 podman 就跳過
echo "== web test"
if command -v podman >/dev/null 2>&1; then
  podman run --rm --pull=missing -v "$PWD/web:/web:Z,ro" -w /web \
    docker.io/library/node:22-alpine node --test
else
  echo "（沒有 podman，跳過前端測試）"
fi
echo "check.sh：全部通過"
