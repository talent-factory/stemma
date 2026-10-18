# ADR-003: Ort der Berechtigungen

**Status:** Vorschlag, offen.

## Entscheidung (Vorschlag)

- Rollen und Policies liegen auf dem Server und werden dort durchgesetzt.
- Signaturen von Commits und Freigaben liegen im Repository und lassen sich unabhängig vom Server prüfen.
- Policy-Dateien sind pro Namespace und selbst Stufe 3 (ADR-004). Sie werden signiert.

## Offen

- [ ] Zustimmung zum Vorschlag?
- [ ] Peer-to-Peer-Sync als Phase-4-Ziel oder bewusst ausgeschlossen?
