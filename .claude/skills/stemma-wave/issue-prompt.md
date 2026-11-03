Du setzt genau ein Sub-Issue eines Stemma-Epics um: STEMMA-{{ISSUE}} (Work-Item-ID {{ID}}).
Lies zuerst CLAUDE.md und die Beschreibung des Issues im Plane-Projekt Stemma
(MCP plane-tf, workitem retrieve_by_identifier). Das Issue steht bereits auf In Progress.
Du arbeitest in einem eigenen Git-Worktree auf einem eigenen Branch; bleibe darin und wechsle nie
ins Hauptverzeichnis. Rufe stemma nicht auf. Benenne die Rex-Session nicht um.

Vorgehen:
1. Umsetzen gemäss Scope und Acceptance des Issues, inklusive der Buch- und ADR-Anpassungen,
   die das Issue verlangt.
2. `just ci` muss grün sein; bei Änderungen an docs/ auch `just book`.
3. Commit mit /git-workflow:professional-commit-workflow, danach Push und PR gegen main.
   Die PR-Beschreibung ist englisch und enthält `Closes STEMMA-{{ISSUE}}`
   (bei Teilarbeit `Refs` mit Begründung). Keine Claude-Session-Zeile, kein Co-Authored-By.
4. Starte /pr-review-toolkit:review-pr auf den PR und arbeite berechtigte Findings ab
   (neuer Commit, Push).
5. Setze das Issue auf In Review (State-ID d567ec07-faf7-4af6-9f95-1ed2e47da941,
   Projekt-ID d988f244-ae6e-492d-a61e-c335c6c87de9).
6. Warte auf den manuellen Review von Daniel. Nicht mergen, ci.yml nicht per gh workflow run
   starten, das Issue nicht auf Done setzen.

Parallel laufen in eigenen Worktrees: {{PEERS}}.
Fasse keine Dateien an, die nicht zu deinem Issue gehören.
