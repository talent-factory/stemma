#!/usr/bin/env bash
# Start one issue: a Git worktree on a new branch and a Rex tab with a Claude session in it.
# Usage: launch-issue.sh <number> <work-item-id> <slug> [<peers>]
set -euo pipefail

if [ "$#" -lt 3 ]; then
  echo "usage: $0 <number> <work-item-id> <slug> [<peers>]" >&2
  exit 2
fi
number=$1
item_id=$2
slug=$3
peers=${4:-none}

: "${REX_SESSION:?REX_SESSION is not set; run this inside a Rex terminal}"

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
common=$(git rev-parse --path-format=absolute --git-common-dir)
root=${common%/.git}
worktree="$root/.claude/worktrees/stemma-$number"
branch="feat/stemma-$number-$slug"

git -C "$root" fetch -q origin main
git -C "$root" worktree add -q -b "$branch" "$worktree" origin/main

template=$(<"$here/issue-prompt.md")
prompt=${template//'{{ISSUE}}'/$number}
prompt=${prompt//'{{ID}}'/$item_id}
prompt=${prompt//'{{PEERS}}'/$peers}

prompt_file=$(mktemp "${TMPDIR:-/tmp}/stemma-prompt-$number.XXXXXX")
printf '%s\n' "$prompt" >"$prompt_file"

# The prompt file is passed as $1 of the inner shell, so no quoting of the prompt is needed.
rex window new -s "$REX_SESSION" "STEMMA-$number" -c "$worktree" --focus=false --json \
  -- sh -c 'claude "$(cat "$1")"' sh "$prompt_file"
