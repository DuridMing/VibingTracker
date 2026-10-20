#!/usr/bin/env bash
# 整個專案的回放驗證：用 samples/ 重算 PLAN 依據的實測事實，並跑 merge 的 samples 回放測試。
# 升級 Claude Code 後跑一次；沒有 samples/ 時只提示、不失敗。比較慢且依賴本機樣本，所以不在 check.sh 裡。
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

if [ ! -d samples/sessions ]; then
  echo "沒有 samples/，略過（先用 tools/hook-capture 收樣本、scripts/redact-samples.py 遮蔽）"
  exit 0
fi
echo "== 實測事實（tests/replay/analyze-samples.py）"
python3 -I tests/replay/analyze-samples.py samples
echo "== merge 回放"
cargo test -q -p coding-lab --test merge samples_replay -- --nocapture
echo "replay.sh：全部通過"
