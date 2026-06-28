#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HOOKS_DIR="$ROOT/.githooks"

if ! git -C "$ROOT" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    echo "install-git-hooks: $ROOT is not a Git worktree" >&2
    exit 1
fi

chmod +x "$HOOKS_DIR"/*
git -C "$ROOT" config core.hooksPath "$HOOKS_DIR"

echo "install-git-hooks: Git hooks installed to $HOOKS_DIR"
