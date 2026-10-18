# ADR-002: Akteur-Modell und Autorisierungs-Hook

**Status:** Vorschlag, teilweise bestätigt.

## Entscheidung (Vorschlag)

- Akteur ist Mensch oder Agent. Agenten führen Modellversion, Session und Sponsor mit.
- Jeder Commit und jeder Operation-Log-Eintrag trägt den Akteur.
- Autorisierung im Core über `authorize(actor, operation, target)`. Phase 1: alles erlaubt, Aufruf trotzdem Pflicht.
- CLI, API und MCP-Server rufen denselben Core auf.
- Rollen gehören zum Policy-Modell, nicht in den Core-Code.

Umsetzung im Code: `crates/stemma-core/src/lib.rs`.

## Offen

- [ ] Agenten-Identitäten: eigene Rolle oder Rolle mit menschlichem Sponsor (siehe ADR-004).

## Konsequenzen

- Akteur-Felder gibt es ab dem ersten Commit. Ein späteres Nachrüsten wäre ein Formatbruch.
- Ein Agent handelt für einen Menschen, der dessen Handlungen verantwortet.
