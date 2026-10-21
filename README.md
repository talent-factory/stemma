# Stemma

Versionskontrollsystem in Rust. API-first, mit Git-artiger CLI, ohne Git-Repository-Kompatibilität.

**Status:** Phase 1, lokaler Kern. Es gibt die Befehle `init`, `track`, `commit`, `status`, `op log` und `op restore`. Die Arbeitskopie ist ein Commit, und jeder Befehl, der sie ändert, nimmt einen Snapshot (ADR-010). Die Befehle stehen in der Referenz des Buchs (`docs/chapters/12-cli-reference.tex`).

**Ein Akteur pro Arbeitskopie.** Ein Snapshot trägt den Akteur des Befehls ein und speichert alles, was im Verzeichnis liegt. Deshalb arbeitet jeder Akteur, Mensch oder Agent, in einer eigenen Arbeitskopie. Zwei Akteure in einem Verzeichnis sind kein unterstützter Betrieb, das Programm prüft es aber noch nicht (ADR-010).

## Der Name

*Stemma* (gesprochen [ˈʃtɛma], Plural *Stemmata*) ist ein altes Wort für Stammbaum. Auf dem Weg dorthin hat es mehrere Bedeutungen angesammelt, und jede davon passt zu einem Versionskontrollsystem.

**Vom Kranz zur Ahnentafel.** Das Wort stammt aus dem Griechischen: στέμμα (*stémma*) ist ein Kranz, eine Girlande, etwas, das man um etwas windet. In der Apostelgeschichte (14,13) sind es die Kränze, die man vor die Stadttore bringt. Im Rom der Republik bewahrten Adelsfamilien Bilder ihrer Vorfahren (*imagines*) im Haus auf, und der Kranz, mit dem man diese Bilder schmückte, hiess ebenfalls *stemma*. Plinius der Ältere erwähnt in seiner Naturgeschichte (Buch 35) *stemmata*, deren Linien zu den gemalten Ahnenbildern führten (*stemmata vero lineis discurrebant ad imagines pictas*). Ob damit schon ein Diagramm im heutigen Sinn gemeint war, ist unklar. Aus dem Kranz um die Ahnenbilder wurde jedenfalls die Ahnentafel selbst, also der Stammbaum.

**Die Frage des Juvenal.** Der Satiriker Juvenal eröffnet seine achte Satire mit einem Spott auf den Adelsstolz: *Stemmata quid faciunt?*, was nützen Stammbäume? Seine Antwort lautet sinngemäss: nichts, solange man nur auf die Herkunft pocht und sie nicht belegen kann. Ein Versionskontrollsystem kann diese Frage technisch beantworten. Jeder Commit nennt seine Eltern, und die Kennung eines Commits ist ein Hash über genau diese Angabe. Wer die Herkunft ändert, ändert die Kennung. Die Ahnentafel ist damit nicht behauptet, sondern prüfbar.

**Der Stammbaum der Handschriften.** In der Textkritik heisst ein Diagramm, das zeigt, wie erhaltene Abschriften eines Werks voneinander abstammen, *stemma codicum*. Die Methode ist im 19. Jahrhundert mit dem Namen Karl Lachmanns verbunden. Ihr Gedanke: Beim Abschreiben entstehen Fehler, und Abschriften, die dieselben auffälligen Fehler teilen, gehen vermutlich auf eine gemeinsame Vorlage zurück. Am Ende steht der Versuch, den Archetypus zu rekonstruieren, den Text, von dem alle Abschriften abstammen. Greift ein Schreiber auf mehrere Vorlagen zurück, spricht man von Kontamination, und aus dem Baum wird ein Geflecht.

Das ist die Lage eines Versionskontrollsystems, nur mit einem Unterschied: Der Textkritiker muss die Abstammung aus den Fehlern erschliessen, eine Hypothese. Stemma zeichnet sie auf, als Tatsache.

**Was der Name für das Projekt bedeutet**

| Textkritik | Stemma |
|---|---|
| Abschrift (Handschrift) | Commit |
| Abstammung von einer Vorlage | Elternverweis des Commits |
| Kontamination (mehrere Vorlagen) | Merge-Commit mit mehreren Eltern |
| Schreiber, dessen Hand man bestimmt | Akteur, Mensch oder Agent mit Auftraggeber (ADR-002) |
| Abstammung wird erschlossen | Abstammung wird aufgezeichnet und durch den Hash geprüft |

Praktisch spielt mit: Der Name hat sechs Buchstaben, lässt sich gut tippen, und das Programm heisst wie das Verzeichnis, in dem ein Repository seine Daten ablegt (`stemma`, `.stemma`).

Quellen zur Herkunft des Wortes: [Wiktionary, *stemma*](https://en.wiktionary.com/wiki/stemma), [Stemma: a term evolves in Latin](https://www.piggin.net/stemmahist/latin.htm) und [Juvenal, Satire 8](https://pages.pomona.edu/~cmc24747/sources/juvenal/juv_8.htm).

## Aufbau

- `crates/stemma-core`: Kern (Objekte, Refs, Autorisierung). Alle Oberflächen rufen diesen Kern auf.
- `crates/stemma-cli`: Kommandozeile, Binary `stemma`.
- `docs/`: Buch (ConTeXt), Quellen folgen.
- `adr/`: Architekturentscheidungen. Siehe `adr/README.md`.

API-Server und MCP-Server folgen als eigene Crates, sobald der Kern steht.
