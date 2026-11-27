#!/usr/bin/env bash
# Brings the Stemma history of the root of this repository in line with Git, after a `git pull`
# or a merge on `main` (docs/agents/stemma-alongside-git.md). Run it through `just stemma-sync`.
#
# 1. Refuses, before it calls `stemma`, to run in a Git worktree, outside the root of the Git
#    checkout, without a `.stemma` in the root, with an actor that is not `human ...`
#    (`STEMMA_ACTOR`), and when a file that Git tracks differs from `HEAD`.
# 2. Reads `stemma status --json`, which takes a snapshot of the tracked files (it writes a log
#    entry if one changed). Refuses after it when the working copy holds a conflict.
# 3. Tracks the files that `git ls-files` lists and `status` reports as untracked. A file that
#    `.stemmaignore` excludes is never reported as untracked, so it stays out, and so does a
#    file that Git does not track.
# 4. Commits when the working-copy commit has a change (a new, changed, or deleted file), with
#    the short hash and the subject of the Git `HEAD` in the message.
#
# A failing `stemma` call ends the script with its own exit code (the table of exit codes in
# docs/agents/using-stemma.md); a refusal of the script itself, and an output of `status` that
# it cannot read, end with 1.
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

# Runs `stemma status --json` into the file named by $1 and checks that the fields the script
# reads are arrays.
read_status() {
    "$stemma" status --json > "$1"
    local code=$?
    if [ "$code" -ne 0 ]; then
        echo "stemma-sync: \`stemma status\` ended with exit code $code" >&2
        exit "$code"
    fi
    jq -e '[.conflicts, .changes, .untracked, .leftovers] | all(type == "array")' "$1" \
        > /dev/null || fail "cannot read the output of \`stemma status --json\`"
}

command -v jq > /dev/null 2>&1 || fail "jq is missing (brew install jq)"
command -v "$stemma" > /dev/null 2>&1 || fail "\`$stemma\` is not on the PATH (just install)"

top=$(git rev-parse --show-toplevel 2> /dev/null) || fail "not in a Git checkout"
git_dir=$(cd "$(git rev-parse --absolute-git-dir)" && pwd -P) || fail "cannot find the Git directory"
common_dir=$(cd "$(git rev-parse --git-common-dir)" && pwd -P) \
    || fail "cannot find the common Git directory"
if [ "$git_dir" != "$common_dir" ]; then
    fail "this is a Git worktree; run the sync in the root of the repository only"
fi
top=$(cd "$top" && pwd -P) || fail "cannot open the root of the Git checkout"
if [ "$(pwd -P)" != "$top" ]; then
    fail "run the sync in the root of the Git checkout ($top), not in $(pwd -P)"
fi
[ -d .stemma ] || fail "no .stemma in the root; run \`stemma init\` first (docs/agents/stemma-alongside-git.md)"

case "${STEMMA_ACTOR:-}" in
    "human "*) ;;
    "") fail "STEMMA_ACTOR is not set; the sync needs a human actor (\"human <id>\")" ;;
    *) fail "the sync needs a human actor (\"human <id>\"), not \"$STEMMA_ACTOR\"" ;;
esac

head=$(git rev-parse --verify -q --short HEAD) || fail "Git has no commit yet"
subject=$(git log -1 --format=%s HEAD) || fail "cannot read the subject of HEAD"
git diff --quiet HEAD --
code=$?
case "$code" in
    0) ;;
    1) fail "files that Git tracks differ from HEAD; commit or discard them first" ;;
    *) fail "\`git diff\` ended with exit code $code" ;;
esac

scratch=$(mktemp -d) || fail "cannot create a temporary directory"
trap 'rm -rf "$scratch"' EXIT
status="$scratch/status.json"

read_status "$status"
[ "$(jq '.conflicts | length' "$status")" -eq 0 ] \
    || fail "the working copy has a conflict; resolve it before the sync"
jq -r '.leftovers[] | "stemma-sync: warning: left by an interrupted restore: \(.)"' \
    "$status" >&2

# The untracked paths of Stemma that Git tracks, separated by NUL. Both lists are raw names:
# `git ls-files -z` does not quote, and the JSON of `status` does not mask. The list of Git goes
# through a file, so that its size meets no limit of the command line.
git ls-files -z > "$scratch/git" || fail "\`git ls-files\` failed"
jq -j --rawfile git "$scratch/git" \
    '($git | split("\u0000")) as $known | (.untracked - (.untracked - $known))[] | ., "\u0000"' \
    "$status" > "$scratch/track" || fail "cannot read the untracked paths of \`stemma status\`"

paths=()
while IFS= read -r -d '' path; do
    paths+=("$path")
done < "$scratch/track"

# One call, so that its exit code is the one passed on; `track` prints `tracked <path>` for each
# new file.
if [ "${#paths[@]}" -gt 0 ]; then
    run_stemma track -- "${paths[@]}"
fi

read_status "$status"
if [ "$(jq '.changes | length' "$status")" -eq 0 ]; then
    echo "Nothing to commit at Git $head"
    exit 0
fi
jq -r '.changes[] | "\(.kind) \(.path)"' "$status"
run_stemma commit -m "Sync with Git $head: $subject"
