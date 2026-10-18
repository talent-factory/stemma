# Stemma

Versionskontrollsystem in Rust. API-first, mit Git-artiger CLI, ohne Git-Repository-Kompatibilität.

**Status:** Phase 1, Grundgerüst. Noch keine Funktionalität.

## Aufbau

- `crates/stemma-core`: Kern (Objekte, Refs, Autorisierung). Alle Oberflächen rufen diesen Kern auf.
- `crates/stemma-cli`: Kommandozeile, Binary `stemma`.
- `docs/`: Buch (ConTeXt), Quellen folgen.
- `adr/`: Architekturentscheidungen. Siehe `adr/README.md`.

API-Server und MCP-Server folgen als eigene Crates, sobald der Kern steht.
