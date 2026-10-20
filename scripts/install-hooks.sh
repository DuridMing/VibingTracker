#!/usr/bin/env bash
# 把 scripts/pre-commit 裝成這個 repo 的 git pre-commit hook（symlink，之後改腳本會直接生效）。
# OpenProject 回填用的 post-commit hook 另外用 AItracking 的 integrations/git/install.sh 安裝。
set -euo pipefail

# --path-format 要 git 2.31 以上（#769 R-20）
ver=$(git --version | sed 's/[^0-9]*\([0-9][0-9]*\)\.\([0-9][0-9]*\).*/\1 \2/')
set -- $ver
if [ "${1:-0}" -lt 2 ] || { [ "${1:-0}" -eq 2 ] && [ "${2:-0}" -lt 31 ]; }; then
  echo "install-hooks.sh 需要 git 2.31 以上（現在是 $(git --version)）" >&2
  exit 1
fi

# hooks 目錄是所有 worktree 共用的：連結要指向主工作樹（git-common-dir 的上一層），
# 不能用 --show-toplevel，否則在 worktree 裡執行、之後刪掉 worktree，pre-commit 就斷了（#687 R35）
common="$(git rev-parse --path-format=absolute --git-common-dir)"
root="$(dirname "$common")"
hook="$common/hooks/pre-commit"

if [ -e "$hook" ] && [ ! -L "$hook" ]; then
  echo "$hook already exists and is not a symlink; not overwriting" >&2
  exit 1
fi
ln -sf "$root/scripts/pre-commit" "$hook"
echo "installed: $hook -> scripts/pre-commit"
