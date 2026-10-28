# Stemma — task runner (version control system in Rust)
#
# Prereqs:
#   just ≥ 1.18 (brew install just)   ·   Rust toolchain (rustup)   ·   ConTeXt (for the book)
#
# List all recipes by group:   `just`
# Single group:                `just --list | grep -A99 '\[Quality\]'`

set dotenv-load            # loads .env into the recipe environment (if present)
set shell := ["bash", "-uc"]
set positional-arguments

docs_dir := "docs"

# Default: grouped recipe list (file order preserved).
default:
    @just --list --unsorted

# ─────────────────────────── Development ───────────────────────────────────

# Build the whole workspace (debug).
[group('Development')]
build:
    cargo build --workspace

# Build the whole workspace (release).
[group('Development')]
build-release:
    cargo build --workspace --release

# Run the CLI.  Usage:  just run init   (without arguments it shows the help)
[group('Development')]
run *args:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ "$#" -eq 0 ]; then set -- --help; fi
    cargo run -p stemma-cli -- "$@"

# Install the CLI as `stemma` into the Cargo bin directory.
[group('Development')]
install:
    cargo install --path crates/stemma-cli --locked

# Fast type check without code generation.
[group('Development')]
check:
    cargo check --workspace --all-targets

# ─────────────────────────── Quality ──────────────────────────────────────

# Run tests.
[group('Quality')]
test:
    cargo test --workspace

# Run the linter (clippy, warnings as errors).
[group('Quality')]
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Format code (rustfmt).
[group('Quality')]
format:
    cargo fmt --all

# Check formatting without modifying files.
[group('Quality')]
format-check:
    cargo fmt --all -- --check

# Check that the documentation of the crates builds without a warning (broken links, links to
# private items).
[group('Quality')]
doc-check:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

# Check that the lines of the documentation sources that differ from `main` hold at most 100
# characters and end without a space. Verbatim blocks, code fences, and tables are exempt. The
# lines of the existing chapters that no change touches are left alone.
[group('Quality')]
doc-lines:
    #!/usr/bin/env bash
    set -euo pipefail
    base=$(git merge-base HEAD origin/main 2>/dev/null || git merge-base HEAD main 2>/dev/null || true)
    if [ -z "$base" ]; then
        echo "doc-lines: no main branch to compare with, skipped"
        exit 0
    fi
    status=0
    while IFS= read -r file; do
        [ -f "$file" ] || continue
        # The added line ranges of the file as "start count" pairs; a count of 0 is a pure deletion.
        ranges=$(git diff -U0 "$base" -- "$file" |
            sed -nE 's/^@@ .* \+([0-9]+)(,([0-9]+))? @@.*/\1 \3/p')
        RANGES="$ranges" awk -v file="$file" '
            BEGIN {
                n = split(ENVIRON["RANGES"], r, "\n")
                for (i = 1; i <= n; i++) {
                    split(r[i], p, " ")
                    count = (p[2] == "") ? 1 : p[2]
                    for (k = 0; k < count; k++) added[p[1] + k] = 1
                }
            }
            /\\start(typing|xtable)/ || /^```/ { if (!skip) { skip = 1; next } }
            skip { if (/\\stop(typing|xtable)/ || /^```/) skip = 0; next }
            /^\|/ { next }
            (FNR in added) && length($0) > 100 {
                printf "%s:%d: %d characters, at most 100\n", file, FNR, length($0); bad = 1
            }
            (FNR in added) && /[ \t]$/ {
                printf "%s:%d: trailing whitespace\n", file, FNR; bad = 1
            }
            END { exit bad }
        ' "$file" || status=1
    done < <(git diff --name-only --diff-filter=AM "$base" -- 'docs/*.tex' 'docs/*.md')
    exit $status

# All CI checks: format, lint, documentation, line length, tests.
[group('Quality')]
ci: format-check lint doc-check doc-lines test

# Fuzz one strict reader for SECONDS seconds: tree, commit, actor, index, entry, or journal.
# Needs nightly and cargo-fuzz (rustup toolchain install nightly; cargo install cargo-fuzz
# --locked), so it is not part of `ci`. New inputs go to fuzz/corpus/TARGET, a finding to
# fuzz/artifacts/TARGET; fuzz/seeds/TARGET holds the valid encodings that the fuzzer starts from.
# An input that takes longer than ten seconds is a finding, so that a hang cannot use up the run.
[group('Quality')]
fuzz target seconds="300":
    mkdir -p fuzz/corpus/{{target}}
    cargo +nightly fuzz run {{target}} fuzz/corpus/{{target}} fuzz/seeds/{{target}} -- \
        -max_total_time={{seconds}} -timeout=10

# Fuzz every strict reader in turn, SECONDS seconds each (see `fuzz`). Every target runs; the
# recipe fails at the end if one of them failed, and names those.
[group('Quality')]
fuzz-all seconds="60":
    #!/usr/bin/env bash
    set -uo pipefail
    trap 'exit 130' INT
    failed=()
    for target in tree commit actor index entry journal; do
        "{{just_executable()}}" fuzz "$target" {{seconds}} || failed+=("$target")
    done
    if (( ${#failed[@]} > 0 )); then
        echo "fuzzing failed for: ${failed[*]}" >&2
        exit 1
    fi

# Test coverage summary (needs `cargo install cargo-llvm-cov --locked`).
[group('Quality')]
coverage:
    cargo llvm-cov --workspace

# Test coverage as an HTML report in target/llvm-cov/html, opened in the browser.
[group('Quality')]
coverage-html:
    cargo llvm-cov --workspace --html --open

# Check the dependency tree: known vulnerabilities, licenses, bans, and sources (see `deny.toml`;
# needs `cargo install cargo-audit cargo-deny --locked`).
[group('Quality')]
deps:
    cargo audit
    cargo deny check

# Check the Rust code against the norma rules of `norma/rule-packs/` (needs `norma` on the PATH).
# Builds a fresh database on every run. Files under `tests/` are left out (see `norma/README.md`).
# Exit code 1 means that rules were violated; the recipe is not part of `just ci`.
[group('Quality')]
norma:
    rm -f target/norma.db
    norma import norma/rule-packs --db target/norma.db > /dev/null
    git ls-files -z '*.rs' ':!:**/tests/**' | xargs -0 norma validate --language rust --db target/norma.db

# ─────────────────────────── Documentation ─────────────────────────────────

# Build API documentation (rustdoc) and open it in the browser.
[group('Documentation')]
rustdoc:
    cargo doc --workspace --no-deps --open

# Convert the SVG figures of the book to PDF (needs rsvg-convert: brew install librsvg).
[group('Documentation')]
figures:
    #!/usr/bin/env bash
    set -euo pipefail
    for svg in {{docs_dir}}/images/src/*.svg; do
        [ -e "$svg" ] || continue
        rsvg-convert --format=pdf --output="{{docs_dir}}/images/$(basename "${svg%.svg}").pdf" "$svg"
    done

# Build the book (ConTeXt → docs/main.pdf).
[group('Documentation')]
book: figures
    cd {{docs_dir}} && context --nonstopmode main.tex

# Build the book and open the PDF.
[group('Documentation')]
book-open: book
    open {{docs_dir}}/main.pdf

# Rebuild the book on changes (requires fswatch).
[group('Documentation')]
book-watch:
    fswatch -o {{docs_dir}}/main.tex | xargs -n1 -I{} just book

# Remove ConTeXt intermediate files of the book.
[group('Documentation')]
book-clean:
    cd {{docs_dir}} && rm -f *.log *.tuc *.tua *.top *.pdf

# ─────────────────────────── Misc ─────────────────────────────────────

# Clean build artifacts and caches.
[group('Misc')]
clean: book-clean
    cargo clean
