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

# All CI checks: format, lint, documentation, tests.
[group('Quality')]
ci: format-check lint doc-check test

# Test coverage summary (needs `cargo install cargo-llvm-cov --locked`).
[group('Quality')]
coverage:
    cargo llvm-cov --workspace

# Test coverage as an HTML report in target/llvm-cov/html, opened in the browser.
[group('Quality')]
coverage-html:
    cargo llvm-cov --workspace --html --open

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
