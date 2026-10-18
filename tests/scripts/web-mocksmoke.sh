#!/bin/sh
# mock collector → client → stream 模型的冒煙測試（tests/e2e/web/mocksmoke.mjs），不需要瀏覽器與 collector。
# 需要：podman。前端目錄同 web-shot.sh（WEB_DIR）。約 10 秒。
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
web=$(cd "${WEB_DIR:-$root/web}" && pwd)
exec podman run --rm -v "$web:/web:Z,ro" -v "$root/tests/e2e/web:/e2e:Z,ro" \
  docker.io/library/node:22.23.3-alpine node /e2e/mocksmoke.mjs
