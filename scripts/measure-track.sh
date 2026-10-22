#!/usr/bin/env bash
# Measures the first `track .` of a working directory of many small files, and a snapshot
# (`status`) of it unchanged: series A of STEMMA-37, as used again in STEMMA-40.
#
# Usage: scripts/measure-track.sh [COUNT...]   (default: 100 1000 10000)
#
# Each file holds 1 KiB of random data, 100 files to a directory. The release build of `stemma`
# is built first. Each count runs once in a new temporary directory, which is removed afterwards.
set -euo pipefail
export STEMMA_ACTOR="human measure"

root=$(cd "$(dirname "$0")/.." && pwd)
cargo build --release --quiet --manifest-path "$root/Cargo.toml" -p stemma-cli
stemma="$root/target/release/stemma"
counts=("$@")
if [ ${#counts[@]} -eq 0 ]; then
    counts=(100 1000 10000)
fi

# Prints the seconds that the command takes, with two decimals.
seconds() {
    local start end
    start=$(perl -MTime::HiRes=time -e 'printf "%.6f", time')
    "$@" > /dev/null || { echo "failed: $*" >&2; exit 1; }
    end=$(perl -MTime::HiRes=time -e 'printf "%.6f", time')
    perl -e "printf '%.2f', $end - $start"
}

printf '%8s  %12s  %12s  %10s\n' files 'track .' 'status' '.stemma'
for count in "${counts[@]}"; do
    dir=$(mktemp -d)
    (
        cd "$dir"
        "$stemma" init > /dev/null
        for ((i = 0; i < count; i++)); do
            sub="d$((i / 100))"
            mkdir -p "$sub"
            head -c 1024 /dev/urandom > "$sub/f$i"
        done
        track=$(seconds "$stemma" track .)
        status=$(seconds "$stemma" status)
        size=$(du -sh .stemma | cut -f1)
        printf '%8d  %11ss  %11ss  %10s\n' "$count" "$track" "$status" "$size"
    )
    rm -rf "$dir"
done
