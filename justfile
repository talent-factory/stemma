# Stemma — Task Runner (Versionskontrollsystem in Rust)
#
# Prereqs:
#   just ≥ 1.18 (brew install just)   ·   Rust-Toolchain (rustup)   ·   ConTeXt (für das Buch)
#
# Alle Recipes gruppiert auflisten:   `just`
# Nur eine Gruppe:                     `just --list | grep -A99 '\[Qualität\]'`

set dotenv-load            # lädt .env in die Recipe-Umgebung (falls vorhanden)
set shell := ["bash", "-uc"]
set positional-arguments

docs_dir := "docs"

# Default: gruppierte Recipe-Liste (Dateireihenfolge erhalten).
default:
    @just --list --unsorted

# ─────────────────────────── Development ───────────────────────────────────

# Gesamten Workspace bauen (Debug).
[group('Development')]
build:
    cargo build --workspace

# Gesamten Workspace bauen (Release).
[group('Development')]
build-release:
    cargo build --workspace --release

# CLI ausführen.  Usage:  just run init
[group('Development')]
run *args:
    cargo run -p stemma-cli -- "$@"

# CLI als `stemma` ins Cargo-bin-Verzeichnis installieren.
[group('Development')]
install:
    cargo install --path crates/stemma-cli --locked

# Schnelle Typprüfung ohne Codegenerierung.
[group('Development')]
check:
    cargo check --workspace --all-targets

# ─────────────────────────── Qualität ──────────────────────────────────────

# Tests ausführen.
[group('Qualität')]
test:
    cargo test --workspace

# Linter ausführen (clippy, Warnungen als Fehler).
[group('Qualität')]
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Code formatieren (rustfmt).
[group('Qualität')]
format:
    cargo fmt --all

# Formatierung prüfen, ohne Dateien zu ändern.
[group('Qualität')]
format-check:
    cargo fmt --all -- --check

# Alle CI-Checks: Format, Lint, Tests.
[group('Qualität')]
ci: format-check lint test

# ─────────────────────────── Dokumentation ─────────────────────────────────

# API-Dokumentation bauen (rustdoc) und im Browser öffnen.
[group('Dokumentation')]
rustdoc:
    cargo doc --workspace --no-deps --open

# Buch bauen (ConTeXt → docs/main.pdf).
[group('Dokumentation')]
book:
    cd {{docs_dir}} && context --nonstopmode main.tex

# Buch bauen und PDF öffnen.
[group('Dokumentation')]
book-open: book
    open {{docs_dir}}/main.pdf

# Buch bei Änderungen automatisch neu bauen (benötigt fswatch).
[group('Dokumentation')]
book-watch:
    fswatch -o {{docs_dir}}/main.tex | xargs -n1 -I{} just book

# ConTeXt-Zwischendateien des Buches entfernen.
[group('Dokumentation')]
book-clean:
    cd {{docs_dir}} && rm -f *.log *.tuc *.tua *.top *.pdf

# ─────────────────────────── Sonstiges ─────────────────────────────────────

# Build-Artefakte und Caches aufräumen.
[group('Sonstiges')]
clean: book-clean
    cargo clean
