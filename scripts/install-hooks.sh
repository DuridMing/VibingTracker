#!/usr/bin/env bash
# 把 scripts/pre-commit 裝成這個 repo 的 git pre-commit hook（symlink，之後改腳本會直接生效）。
# OpenProject 回填用的 post-commit hook 另外用 AItracking 的 integrations/git/install.sh 安裝。
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
hook="$(git rev-parse --git-path hooks)/pre-commit"

if [ -e "$hook" ] && [ ! -L "$hook" ]; then
  echo "$hook already exists and is not a symlink; not overwriting" >&2
  exit 1
fi
ln -sf "$root/scripts/pre-commit" "$hook"
echo "installed: $hook -> scripts/pre-commit"
