#!/usr/bin/env bash
# Remove the worktree and the local branch of a merged issue.
# Usage: cleanup-issue.sh <number>
set -euo pipefail

if [ "$#" -ne 1 ]; then
  echo "usage: $0 <number>" >&2
  exit 2
fi
number=$1

common=$(git rev-parse --path-format=absolute --git-common-dir)
root=${common%/.git}
worktree="$root/.claude/worktrees/stemma-$number"

if [ -d "$worktree" ]; then
  branch=$(git -C "$worktree" rev-parse --abbrev-ref HEAD)
  # Refuses a worktree with changes.
  git -C "$root" worktree remove "$worktree"
  # The branch was squash-merged, so Git does not see it as merged: force the delete.
  git -C "$root" branch -D "$branch"
  echo "removed worktree and branch $branch"
else
  echo "no worktree for STEMMA-$number"
fi
git -C "$root" worktree prune
