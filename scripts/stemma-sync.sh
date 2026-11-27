#!/usr/bin/env bash
# Brings the Stemma history of the root of this repository in line with Git, after a `git pull`
# or a merge on `main` (docs/agents/stemma-alongside-git.md). Run it through `just stemma-sync`.
#
# 1. Refuses to run in a Git worktree, outside the root of the Git checkout, without a `.stemma`
#    in the root, with an actor that is not `human ...` (`STEMMA_ACTOR`), and when a file that
#    Git tracks differs from `HEAD`.
# 2. Tracks the files that `git ls-files` lists and `stemma status --json` reports as untracked.
#    A file that `.stemmaignore` excludes is never reported as untracked, so it stays out, and so
#    does a file that Git does not track.
# 3. Commits when the working-copy commit has a change (a new, changed, or deleted file), with
#    the short hash and the subject of the Git `HEAD` in the message.
#
# The exit code of a failed `stemma` call is passed through (chapter 12: 1 failed, 3 incomplete,
# 5 damage, 6 a lock is held); a refusal of the script itself ends with 1.
# `STEMMA_BIN` names the program to call (default: `stemma` on the PATH). Needs `jq`.
set -uo pipefail

stemma=${STEMMA_BIN:-stemma}

fail() {
    echo "stemma-sync: $*" >&2
    exit 1
}

# Runs `stemma` and leaves the script with its exit code if it fails.
run_stemma() {
    "$stemma" "$@"
    local code=$?
    if [ "$code" -ne 0 ]; then
        echo "stemma-sync: \`stemma $1\` ended with exit code $code" >&2
        exit "$code"
    fi
}

command -v jq > /dev/null 2>&1 || fail "jq is missing (brew install jq)"
command -v "$stemma" > /dev/null 2>&1 || fail "\`$stemma\` is not on the PATH (just install)"

top=$(git rev-parse --show-toplevel 2> /dev/null) || fail "not in a Git checkout"
git_dir=$(git rev-parse --absolute-git-dir) || exit 1
common_dir=$(cd "$(git rev-parse --git-common-dir)" && pwd -P) || exit 1
if [ "$(cd "$git_dir" && pwd -P)" != "$common_dir" ]; then
    fail "this is a Git worktree; run the sync in the root of the repository only"
fi
if [ "$(pwd -P)" != "$(cd "$top" && pwd -P)" ]; then
    fail "run the sync in the root of the Git checkout ($top), not in $(pwd -P)"
fi
[ -d .stemma ] || fail "no .stemma in the root; run \`stemma init\` first (docs/agents/stemma-alongside-git.md)"

case "${STEMMA_ACTOR:-}" in
    "human "*) ;;
    "") fail "STEMMA_ACTOR is not set; the sync needs a human actor (\"human <id>\")" ;;
    *) fail "the sync needs a human actor (\"human <id>\"), not \"$STEMMA_ACTOR\"" ;;
esac

git diff --quiet HEAD -- || fail "files that Git tracks differ from HEAD; commit or discard them first"

status=$("$stemma" status --json)
code=$?
[ "$code" -eq 0 ] || { echo "stemma-sync: \`stemma status\` ended with exit code $code" >&2; exit "$code"; }

if [ "$(jq '.conflicts | length' <<< "$status")" -ne 0 ]; then
    fail "the working copy has a conflict; resolve it before the sync"
fi
jq -r '.leftovers[] | "stemma-sync: warning: left by an interrupted restore: \(.)"' \
    <<< "$status" >&2

# The untracked paths of Stemma that Git tracks, separated by NUL. Both lists are raw names:
# `git ls-files -z` does not quote, and the JSON of `status` does not mask.
git_files=$(git ls-files -z | jq -Rs 'split("\u0000") | map(select(. != ""))') || exit 1
paths=()
while IFS= read -r -d '' path; do
    paths+=("$path")
done < <(jq -j --argjson git "$git_files" \
    '(.untracked - (.untracked - $git))[] | ., "\u0000"' <<< "$status")

# One call, so that its exit code is the one passed on; `track` prints `tracked <path>` for each
# new file.
if [ "${#paths[@]}" -gt 0 ]; then
    run_stemma track -- "${paths[@]}"
fi

status=$("$stemma" status --json)
code=$?
[ "$code" -eq 0 ] || { echo "stemma-sync: \`stemma status\` ended with exit code $code" >&2; exit "$code"; }

if [ "$(jq '.changes | length' <<< "$status")" -eq 0 ]; then
    echo "Nothing to commit: Stemma matches Git $(git rev-parse --short HEAD)"
    exit 0
fi
jq -r '.changes[] | "\(.kind) \(.path)"' <<< "$status"
run_stemma commit -m "Sync with Git $(git rev-parse --short HEAD): $(git log -1 --format=%s)"
