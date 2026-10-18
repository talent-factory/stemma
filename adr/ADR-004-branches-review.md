# ADR-004: Branch-Ownership, Review und Risikostufen

**Status:** Vorschlag, offen zur Entscheidung.

## Entscheidung

1. Jeder Branch hat genau einen Schreibeigentümer (Mensch oder Agent). Der Server erzwingt das über einen Lease mit Heartbeat und Ablaufzeit.
2. Owner-Override nur für die Rolle Owner, mit Begründung und Audit-Eintrag. Der Override umgeht nie das Merge-Gate.
3. Geschützte Branches wie `main` werden nur über PRs verändert.
4. Agenten dürfen reviewen und freigeben. Keine Selbstfreigabe. Freigaben sind signiert.
5. Risikostufen 0 bis 3 nach Wirkung der Änderung. Die Stufe ergibt sich aus allen berührten Pfaden. Die Policy-Datei ist selbst Stufe 3.
6. Diff-Anzeige Stufe 1, Diff-Berechnung im Merge-Pfad Stufe 2.
7. Neue direkte Laufzeitabhängigkeiten Stufe 2. Automatisierte Patch-Updates Stufe 1 mit `cargo-audit`, `cargo-deny` und Lockfile-Diff. Kryptografie-Crates Stufe 3.
8. Jeder Namespace hat eine eigene Policy mit nicht unterschreitbarem Boden für Stufe 3.
9. Owner-Override in Kursen nur mit unlöschbarem, für Studierende sichtbarem Audit-Eintrag.
10. Lockerung oder Löschung bestehender Assertions eskaliert automatisch auf Stufe 2.

## Risikostufen

| Stufe | Beispiele | Freigabe |
|---|---|---|
| 0 | Doku, Beispiele, Formatierung, reine Tests | Ein unabhängiger Agent, Merge durch Agent |
| 1 | Neue CLI-Befehle, Ausgabeformate, Logik ohne Persistenz | Zwei unabhängige Agenten, Merge durch Agent |
| 2 | Objektformat, Hash, Ref-Updates, Lease, Merge, Netzwerk, Migrationen, neue Abhängigkeiten | Zwei Agenten plus menschlicher Maintainer |
| 3 | RBAC, Signaturen und Schlüssel, Namespace-Trennung, CI/Release, Policy-Dateien | Nur menschlicher Owner, zusätzlich ADR |
