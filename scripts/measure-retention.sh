#!/usr/bin/env bash
# Measures what the operation log keeps alive over a session (STEMMA-39, ADR-011 "Retention"):
# the first-parent history of a Git repository is replayed into new Stemma repositories.
#
# Usage: scripts/measure-retention.sh [GIT_REPOSITORY [REVISION]]   (default: this one, 11f06a4)
#
# - fine: every changed file of a commit is written alone and followed by one command, `track`
#   for a new file and `status` otherwise, as an agent that runs a command after every edit.
# - coarse: `track .` once per commit.
# - final: the tree of the last commit only, tracked once.
#
# No replay runs `commit`. Sizes are apparent sizes, the sum of the lengths of the files. The
# release build of `stemma` is built first, and each replay runs once in a new temporary
# directory, which is removed afterwards.
set -euo pipefail
export STEMMA_ACTOR="agent replay model=none session=replay sponsor=measure"

root=$(cd "$(dirname "$0")/.." && pwd)
source=${1:-$root}
revision=${2:-11f06a4}
cargo build --release --quiet --manifest-path "$root/Cargo.toml" -p stemma-cli
stemma="$root/target/release/stemma"
commits=$(git -C "$source" rev-list --reverse --first-parent "$revision")
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# Prints the number of files and the sum of their lengths below a directory.
files() {
    find "$1" -type f -print0 | xargs -0 wc -c | awk '
        $2 != "total" { n++; s += $1 } END { printf "%6d files  %10d bytes", n, s }'
}

# Prints the counts of the operations in the log and the sizes of the log and of the objects.
report() {
    local repository=$1 name=$2
    printf '%-7s log %s  objects %s\n' "$name" "$(files "$repository/.stemma/oplog")" \
        "$(files "$repository/.stemma/objects")"
    printf '%-7s %s\n' '' "$(grep -h '^operation' "$repository"/.stemma/oplog/* | sort \
        | uniq -c | awk '{ printf "%s %d  ", $3, $1 }')"
    printf '%-7s %s\n' '' "$(find "$repository/.stemma/objects" -type f -print0 \
        | xargs -0 -n 1 sh -c 'printf "%s %s\n" "$(head -c 6 "$0" | cut -d " " -f 1)" \
            "$(wc -c < "$0")"' | awk '{ n[$1]++; s[$1] += $2 }
            END { for (k in n) printf "%s %d (%d bytes)  ", k, n[k], s[k] }')"
}

# Removes everything in the directory except `.stemma`.
clear() {
    find "$1" -mindepth 1 -maxdepth 1 ! -name .stemma -exec rm -rf {} +
}

fine=$work/fine
"$stemma" init -C "$fine" > /dev/null
previous=$(git -C "$source" hash-object -t tree /dev/null)
for commit in $commits; do
    while IFS=$'\t' read -r state path; do
        if [ "$state" = D ]; then
            rm -f "$fine/$path"
            "$stemma" status -C "$fine" > /dev/null
        else
            mkdir -p "$(dirname "$fine/$path")"
            git -C "$source" show "$commit:$path" > "$fine/$path"
            if [ "$state" = A ]; then
                "$stemma" track -C "$fine" "$path" > /dev/null
            else
                "$stemma" status -C "$fine" > /dev/null
            fi
        fi
    done < <(git -C "$source" diff --no-renames --name-status "$previous" "$commit")
    previous=$commit
done

coarse=$work/coarse
"$stemma" init -C "$coarse" > /dev/null
for commit in $commits; do
    clear "$coarse"
    git -C "$source" archive "$commit" | tar -x -C "$coarse"
    "$stemma" track -C "$coarse" . > /dev/null
done

final=$work/final
"$stemma" init -C "$final" > /dev/null
git -C "$source" archive "$revision" | tar -x -C "$final"
"$stemma" track -C "$final" . > /dev/null

dates=$(git -C "$source" log --first-parent --format=%ad --date=short "$revision" | sort)
echo "$(echo "$commits" | wc -l | tr -d ' ') commits of $revision," \
    "$(echo "$dates" | head -n 1) to $(echo "$dates" | tail -n 1)"
report "$fine" fine
report "$coarse" coarse
report "$final" final
