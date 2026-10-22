# Stemma

Versionskontrollsystem in Rust. API-first, mit Git-artiger CLI, ohne Git-Repository-Kompatibilität.

**Status:** Phase 1, lokaler Kern. Es gibt die Befehle `init`, `track`, `commit`, `status`, `op log` und `op restore`; `purge` entfernt ein Geheimnis aus Verlauf und Log und zeigt ohne `--apply` nur den Plan (ADR-014). Die Arbeitskopie ist ein Commit, und jeder Befehl, der sie ändert, nimmt einen Snapshot (ADR-010). Die Befehle stehen in der Referenz des Buchs (`docs/chapters/12-cli-reference.tex`).

**Ein Akteur pro Arbeitskopie.** Ein Snapshot trägt den Akteur des Befehls ein und speichert alles, was im Verzeichnis liegt. Deshalb arbeitet jeder Akteur, Mensch oder Agent, in einer eigenen Arbeitskopie. Zwei Akteure in einem Verzeichnis sind kein unterstützter Betrieb, das Programm prüft es aber noch nicht (ADR-010).

## Der Name

*Stemma* (gr. στέμμα, Kranz) heisst in der Textkritik der Stammbaum der Handschriften. Woher der Name kommt und was er für das Projekt bedeutet, steht in der Einleitung des Buchs (`docs/chapters/01-introduction.tex`, Abschnitt «Der Name»).

## Aufbau

- `crates/stemma-core`: Kern (Objekte, Refs, Autorisierung). Alle Oberflächen rufen diesen Kern auf.
- `crates/stemma-cli`: Kommandozeile, Binary `stemma`.
- `docs/`: Buch (ConTeXt), Quellen folgen.
- `adr/`: Architekturentscheidungen. Siehe `adr/README.md`.

API-Server und MCP-Server folgen als eigene Crates, sobald der Kern steht.
