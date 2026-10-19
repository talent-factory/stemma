# Stemma: Handover für die nächste Session

Stand: 19. Oktober 2026, `main` bei Commit `6c0bc47` (PR #26).

Dieses Dokument ist eine Momentaufnahme für den Wechsel zwischen Sessions. Der Git-Verlauf, `CLAUDE.md` und die ADRs im Ordner `adr/` gelten vor ihm. Bei einem Widerspruch hat das Repository recht. Nach grösseren Meilensteinen wird es nachgeführt.

## Auf einen Blick

Das Datenmodell des lokalen Kerns steht: Objekte, Index, Objektspeicher, strikte Leser, Schreiben und Lesen von Trees und Commits. Die CLI ist noch ein leeres Skelett. Als Nächstes folgen die Referenzen (`main`, HEAD) und die Repository-Schicht mit `stemma init`.

- **Repository:** GitHub `talent-factory/stemma`, lokal `~/GitRepository/stemma` auf Daniels Mac.
- **Phase:** 1 von 6. Das Buch gliedert die Phasen so: lokaler Kern, Diff, Branches und Merge, Zusammenarbeit, Netzwerk, Agenten-Schnittstellen.
- **Parallele Sessions:** Mehrere Sessions arbeiten am Repository, auch gleichzeitig. Vor jeder Arbeit zuerst den Stand im Repository lesen, nicht diesem Dokument allein trauen.

## Projekt und Ziele

Stemma ist ein API-first Versionskontrollsystem in Rust. Es wird von Daniel Senften (Talent Factory GmbH) gebaut, ein Buch dokumentiert die Entstehung parallel. Der Name kommt vom Stammbaum (lat. *stemma*), die Herleitung steht im `README.md`.

**Ziele**

- **API-first:** `stemma-core` ist der Kern. CLI, später API-Server und MCP-Server rufen nur ihn auf.
- **Git-ähnliche CLI:** Befehlsnamen und Bedienung folgen Git, damit sich das System vertraut anfühlt. Das Repository-Format ist bewusst nicht Git-kompatibel, GitHub funktioniert nicht als Remote.
- **Moderner Hash:** BLAKE3, mit Algorithmus-Präfix (`b3:`), damit später ein anderer Hash möglich bleibt. SHA-1 wird nicht verwendet.
- **Agenten als Akteure:** Mensch oder Agent (mit Modell, Session und Sponsor) sind von Anfang an im Datenmodell.
- **Ein Schreibbesitzer pro Branch:** Nie zwei Personen oder Agenten gleichzeitig auf demselben Branch. Der Weg über einen PR ist Pflicht, Agenten dürfen reviewen und freigeben, aber nicht sich selbst.
- **Verteilt, mit offenem Blick auf Zentrales:** Daniel interessiert sich für Web3, ist aber offen für die Vorteile einer zentralen Instanz. Das ist noch nicht entschieden.
- **RBAC im Hinterkopf:** Rollen und Rechte sind geplant, noch nicht umgesetzt (ADR-003).

**Nutzergruppen in Etappen:** 1. Daniel allein, 2. Mitarbeitende von Talent Factory, 3. Studierende (Unterricht an der FFHS, Fächer DUA25 und AISE), 4. andere Firmen. Primäre Oberfläche ist die CLI, später ein MCP-Server. Eine Web-Oberfläche höchstens für Administratoren.

**Nicht Ziel:** Git-Format-Kompatibilität, GitHub als Remote, eine Web-Oberfläche für alle.

**Ideenquellen:** jj (Change-IDs, Konflikte als Daten, Operation Log), Pijul, Sapling. Das interne Versionskontrollsystem von Anthropic ist nur aus Presseberichten bekannt, nicht öffentlich. Coglans *Building Git* dient ausschliesslich als Ideenquelle, Text und Code sind neu geschrieben.

## Entscheidungen (ADRs)

Nur ADR-001 ist formal angenommen. Die übrigen sechs tragen in ihrer Datei noch «Proposal», auch wenn Daniel im Gespräch wesentliche Punkte bestätigt hat. Die Dateien im Ordner `adr/` sind englisch und massgeblich.

| ADR | Thema | Status in der Datei | Bestätigt | Offen |
| --- | --- | --- | --- | --- |
| 001 | Hash-Funktion | Accepted (BLAKE3) | BLAKE3, Präfix `b3:` | Standards oder Zertifizierung für eine Nutzergruppe? |
| 002 | Akteure und Autorisierungs-Hook | Proposal, teilweise bestätigt | Agenten von Anfang an als Akteure; Rollen gemeinsam modellieren | Agenten-Identität: eigene Rolle oder Rolle mit menschlichem Sponsor |
| 003 | Ort der Berechtigungen | Proposal, offen | nichts ausdrücklich | Einverständnis mit dem Vorschlag; P2P-Sync als Phase-4-Ziel oder ausgeschlossen; signiertes Protokoll der Ref-Updates auf dem Hub |
| 004 | Branch-Besitz, Review, Risikostufen | Proposal, offen zur Entscheidung | Owner-Override erlaubt; Agenten dürfen reviewen und freigeben; Zustimmung zu den vorgeschlagenen Lösungen | Pfadlisten je Risikostufe; Agenten-Identität; strengere Böden für Firmenkunden |
| 005 | Objektkodierung, Namen, Identitätszeichen | Proposal, teilweise bestätigt | viele Punkte als *confirmed* markiert, Zeitstempel ohne Committer, Operation-Log bleibt lokal | keine offenen Punkte mehr in der Liste |
| 006 | Trunk-based Development | Proposal, Richtung bestätigt | kurzlebige Branches, ein Slice je Branch und PR | Squash als einzige Merge-Methode; maximale Branch-Lebensdauer; Branch-Protection; CI-Trigger |
| 007 | Objektspeicher | Proposal, teilweise bestätigt | lose Objekte, keine Kompression, Ordner `b3/` im Pfad | Packfiles; Garbage Collection; Streaming; Formatdatei prüfen; Verhalten unter Windows |

**Aufgabe:** Die Status-Zeilen der ADR-Dateien nachziehen. Daniel entscheidet, welche Proposals zu *Accepted* werden. Das passt in einen kleinen Docs-PR.

## Design-Prinzipien

Drei Regeln prägen alles andere: Jede Änderung hat einen Akteur, jeder Branch hat genau einen Schreibbesitzer, und jedes Format ist eindeutig und durch Tests fixiert.

**Akteure (ADR-002)**

- Ein `Actor` ist ein Mensch (`id`) oder ein Agent (`id`, `model`, `session`, `sponsor`). Er wird nur über `Actor::human` und `Actor::agent` gebaut, die jedes Feld prüfen. Ein vorhandener Actor ist damit gültig.
- Die Felder sind nicht-leeres, sichtbares ASCII ohne Leerzeichen. Umlaute und andere Schriften werden abgelehnt, damit sich niemand mit ähnlich aussehenden Zeichen als jemand anderes ausgeben kann.
- Jede Operation im Kern ruft `authorize(actor, operation, target)` auf. In Phase 1 ist das `AllowAll`. Der Aufruf ist trotzdem Pflicht und liegt in den Repository-Operationen, nicht im Objektspeicher.

**Branch-Besitz und Review (ADR-004)**

- Ein Branch hat genau einen Schreibbesitzer (Mensch oder Agent). Der Server erzwingt das mit einem Lease mit Heartbeat und Ablauf.
- Der Owner-Override ist nur der Owner-Rolle erlaubt, mit Begründung und Audit-Eintrag. Er umgeht nie das Merge-Gate.
- Geschützte Branches wie `main` ändern sich nur über PRs. Agenten dürfen reviewen und freigeben, aber nie sich selbst. Freigaben sind signiert.
- Risikostufen 0 bis 3 richten sich nach der Wirkung der Änderung und folgen aus allen berührten Pfaden. Die Policy-Datei ist selbst Stufe 3. Neue direkte Laufzeit-Abhängigkeiten sind Stufe 2, Krypto-Crates Stufe 3, automatisierte Patch-Updates Stufe 1 mit `cargo-audit`, `cargo-deny` und Lockfile-Diff. Wer Tests abschwächt oder löscht, landet automatisch in Stufe 2.
- Jeder Namespace hat eine eigene Policy mit einem Boden, den man nicht senken kann. Ein Owner-Override in Kursen braucht einen unlöschbaren Audit-Eintrag, den Studierende sehen.

**Datenmodell (ADR-001, 005, 007)**

- Objekt-ID: `b3:` plus 64 Kleinbuchstaben-Hex. Gehasht wird `<kind> <länge>`, ein NUL-Byte und der Körper. Gleicher Inhalt in verschiedenen Objektarten ergibt verschiedene IDs.
- Typisierte IDs `BlobId`, `TreeId`, `CommitId` helfen beim Schreiben von Code. Die Art steckt im Hash, nicht in der ID. Eine ID aus Text oder aus dem Speicher ist so lange untypisiert, bis `resolve_blob`, `resolve_tree` oder `resolve_commit` die Art bestätigt haben. `get_tree` und `get_commit` tun das selbst.
- Ein Commit hat Baum, Eltern, Autor (ein Actor) und Zeitstempel in Sekunden. Es gibt keinen Committer. Das Operation-Log bleibt lokal und wird nicht mit der Historie ausgetauscht.
- Namen sind UTF-8, werden nicht normalisiert und sind streng geprüft: keine Steuer-, Format- oder standardmässig unsichtbaren Zeichen, keine reinen Punkt-Namen. `.stemma` ist samt Aliasen reserviert, damit ein Checkout keine Repository-Daten überschreibt. Backslash und Doppelpunkt sind im Baum erlaubt und Sache des Checkouts unter Windows.
- Die Leser sind streng. Was `encode` nicht geschrieben hätte, wird abgelehnt, damit ein Objekt genau eine Kodierung und eine ID hat. Ein unvereinbares Format bekommt einen neuen Objektnamen (zum Beispiel `commit2`) und einen ADR. Golden-Vector-Tests fixieren jede Kodierung.
- Der Objektspeicher hält lose Objekte unter `objects/b3/xx/…` ohne Kompression, schreibt über eine temporäre Datei mit `fsync` und `rename` und hasht beim Lesen neu. Beschädigte Objekte werden gemeldet, nie ausgeliefert.

**Entwicklungsmodell (ADR-006)**

Trunk-based Development: `main` ist geschützt und immer grün. Jede Arbeitsscheibe ist ein eigener kurzlebiger Branch mit einem PR, der per Squash gemergt wird. Danach beginnt die nächste Scheibe auf einem neuen Branch von `main`, nie auf dem gemergten. Unfertiges bleibt unverdrahtet oder hinter einem Feature-Flag.

## Repository-Stand

`stemma-core` hat rund 7'800 Zeilen Rust einschliesslich Tests. `stemma-cli` gibt nur eine Versionszeile aus, es gibt noch keinen Befehl.

**Module in `crates/stemma-core/src`**

| Datei | Aufgabe | Wichtige Elemente |
| --- | --- | --- |
| `actor.rs` | Wer handelt | `Actor::human`, `Actor::agent`, `Actor::view`, `FromStr` und `Display` (die Zeile im Commit), `ActorError`, `ActorField`, `ActorMalformed` |
| `object.rs` | Objekte und Regeln | `ObjectId`, `BlobId`, `TreeId`, `CommitId`, `Blob`, `Tree` mit `parse`, `TreeEntry::file` und `::directory`, `FileMode`, `EntryMode`, `Commit` mit `parse`, `TreeMalformed`, `CommitMalformed`, `TimeError`, Namensregeln, `METADATA_DIR` |
| `index.rs` | Staging-Bereich | `Index` mit `insert`, `remove`, `get`, `encode`, `parse`, `verify(&store)`, `write_tree(&store)`, `IndexMalformed`; Pfade nach Komponenten sortiert, Datei und Verzeichnis können nicht kollidieren |
| `store.rs` | Objektspeicher | `ObjectStore` (versiegelt), `FsObjectStore`, `RawObject`, `resolve_blob`, `resolve_tree`, `resolve_commit`, `get_tree`, `get_commit`, `ReadError`, `CorruptReason`, `StoreError` |
| `verify.rs` | Behauptungen prüfen | `Tree::verify`, `Commit::verify` prüfen die Arten, die Modus, Baum-Header und Eltern behaupten, einen Schritt tief gegen einen Speicher |
| `lib.rs` | Einstieg | `Authorizer`, `AllowAll`, `Decision` |
| `testutil.rs` | Testhilfen | nur für Tests |

**Was bisher gemergt ist (nach PR-Nummer)**

| PR | Inhalt |
| --- | --- |
| #1 | Objektmodell mit BLAKE3 (Blob, Tree, Commit) |
| #2, #3 | ADR-001 angenommen, ADR-006 neu; Index mit kanonischer Kodierung |
| #4 bis #7, #10 | Namensregeln geschärft (Joiner, Richtungszeichen, unsichtbare Zeichen, `.stemma` samt Aliasen, Variation Selectors) |
| #8 | Objektspeicher mit geprüftem Lesen (ADR-007) |
| #9, #14 | GitHub-Actions-Workflow (nur von Hand gestartet), `just coverage` |
| #11, #12 | Index nach Komponenten sortiert, Sortierung in ADR-005 bestätigt |
| #13, #15 | typisierte IDs; `resolve_*` im Objektspeicher |
| #16 bis #18, #24 | Actor als eigener Typ und seine Aufräumarbeiten; Begründung, warum es keinen Committer gibt; Operation-Log bleibt lokal |
| #19, #21 | BibTeX-Quelle im Buch; Folgepunkte aus Reviews in ADR-005 |
| #20, #22 | `Index::verify`; strikte Leser für Tree, Commit und Actor-Zeile |
| #23 | Abschnitt «Der Name» im README |
| #25 | `Index::write_tree`: prüft den Index selbst und schreibt die Trees |
| #26 | `get_tree` und `get_commit` im Speicher; `Tree::verify` und `Commit::verify` |

**Werkzeuge**

- `just`-Rezepte: `build`, `build-release`, `run`, `install`, `check`, `test`, `lint`, `format`, `format-check`, `ci`, `coverage`, `coverage-html`, `rustdoc`, `book`, `book-open`, `book-watch`, `book-clean`, `clean`.
- GitHub Actions (`.github/workflows/ci.yml`) startet nur von Hand (`gh workflow run ci.yml --ref <branch>`). Jobs: Check unter Linux sowie Tests unter macOS und Windows. Die PR-Trigger stehen als Kommentar in der Datei und sind in ADR-006 als offen vermerkt.
- Buch: `docs/main.tex`, elf Kapitel in `docs/chapters/` (Einleitung, Grundlagen, Architektur, sechs Phasen, Lehre, Ausblick), Literatur in `docs/references.bib`. Der Text bleibt deutsch, bis eine Übersetzung beschlossen wird.

## Phasen und Aufgaben

Von sechs Phasen hat Phase 1 begonnen: Ihr Datenmodell ist fertig, die Befehle fehlen noch. Die Aufgaben stammen aus den Buchkapiteln 04 bis 09, Phase 1 ist feiner zerlegt. Der Status beruht auf dem Code, nicht auf Absichten.

**Überblick**

| Phase | Ziel | Aufgaben | Erledigt | Status |
| --- | --- | --- | --- | --- |
| Fundament | Repository, Regeln, Werkzeuge, ADRs | 6 | 6 | Erledigt |
| 1 Lokaler Kern | Ein lokales Repository mit einem Nutzer: anlegen, speichern, committen, Status | 15 | 7 (1 teilweise) | In Arbeit |
| 2 Diff und Ausgabe | Änderungen zwischen Arbeitskopie, Index und Commits lesbar anzeigen | 4 | 0 | Nicht begonnen |
| 3 Branches und Merge | Parallele Arbeit lokal: Branches, Wechsel, Drei-Wege-Merge | 4 | 0 | Nicht begonnen |
| 4 Akteure und Zusammenarbeit | Schreibhoheit, PRs und Freigaben werden erzwungen | 7 | 0 | Nicht begonnen |
| 5 Netzwerk und Hub | Synchronisation mit einem Server, der Berechtigungen durchsetzt | 5 | 0 | Nicht begonnen |
| 6 Agenten-Schnittstellen | API- und MCP-Server, jeder Aufruf autorisiert und protokolliert | 4 | 0 | Nicht begonnen |

**Aufgaben je Phase**

| Phase | Aufgabe | Status | Bemerkung |
| --- | --- | --- | --- |
| Fundament | Repository, `CLAUDE.md`, `justfile`, Konventionen | Erledigt | |
| Fundament | ADR-001 bis ADR-007 geschrieben | Erledigt | nur ADR-001 ist angenommen, die übrigen sind Proposals |
| Fundament | Buch-Gerüst mit elf Kapiteln, `references.bib` | Erledigt | Kapitel 04 bis 09 noch ohne Stand der Umsetzung |
| Fundament | GitHub-Actions-Workflow (Linux, macOS, Windows) | Erledigt | startet nur von Hand, PR-Trigger offen (ADR-006) |
| Fundament | Trunk-based Development mit Squash-PRs | Erledigt | ADR-006 noch Proposal |
| Fundament | README mit Herkunft des Namens | Erledigt | PR #23 |
| 1 | Objektmodell: Blob, Tree, Commit, BLAKE3-IDs, typisierte IDs | Erledigt | PR #1, #13 |
| 1 | Objektspeicher: lose Objekte, geprüftes Lesen | Erledigt | PR #8, #15; Packfiles und Streaming offen (ADR-007) |
| 1 | Strikte Leser für Tree, Commit und Actor-Zeile | Erledigt | PR #22 |
| 1 | Akteur-Felder in jedem Commit (ADR-002) | Erledigt | `Actor` als eigener Typ, PR #16, #24 |
| 1 | Index als Datenmodell: Kodierung, Pfadregeln, `verify` | Erledigt | PR #3, #11, #20; Format `stemma-index 1` entschieden |
| 1 | Objekt aus dem Speicher lesen und decodieren, Arten prüfen | Erledigt | PR #26: `get_tree`, `get_commit`, `Tree::verify`, `Commit::verify` |
| 1 | `Index::write_tree(&store)` | Erledigt | PR #25: ruft `verify` selbst auf |
| 1 | Autorisierungs-Hook `authorize` | Teilweise | Trait und `AllowAll` vorhanden, aber noch keine Operation ruft ihn auf |
| 1 | Referenzen für `main` und HEAD | Offen | nächster Schritt; noch kein Modul `refs` |
| 1 | Repository-Schicht und `stemma init` | Offen | Layout von `.stemma/` entworfen, Standard-Branch konfigurierbar, Namensregel unbeantwortet |
| 1 | `stemma add` | Offen | Dateien lesen, Blobs schreiben, Index speichern |
| 1 | `stemma commit` | Offen | Zeitstempel ist im Commit vorgesehen |
| 1 | `stemma status` | Offen | braucht Arbeitskopie, Index und HEAD |
| 1 | Operation Log | Offen | bleibt lokal (ADR-005); Format noch nicht entworfen |
| 1 | Entscheidung Arbeitskopie als Commit (wie jj) oder klassischer Index | Offen | der Index ist gebaut, die Frage ist nicht entschieden |
| 2 | Myers-Diff mit Tests gegen bekannte Fälle | Nicht begonnen | |
| 2 | Hunks mit Kontextzeilen | Nicht begonnen | |
| 2 | Farbausgabe und Pager | Nicht begonnen | maschinenlesbare Ausgabe für Agenten noch offen |
| 2 | Trennung Darstellung (Stufe 1) und Berechnung im Merge-Pfad (Stufe 2) | Nicht begonnen | ADR-004; Binär- und Grossdateien offen |
| 3 | `branch`, `checkout`, `log` | Nicht begonnen | |
| 3 | Drei-Wege-Merge über den gemeinsamen Vorfahren | Nicht begonnen | Stufe 2 nach ADR-004 |
| 3 | Konfliktmodell | Nicht begonnen | Daten im Commit oder Markierung ist offen; Rebase ja oder nein ebenso |
| 3 | Verhalten bei Branch in einem anderen Worktree | Nicht begonnen | |
| 4 | Lease pro Branch mit Heartbeat und Ablauf | Nicht begonnen | Dauer und abgestürzte Agenten offen |
| 4 | Owner-Override mit Begründung und Audit | Nicht begonnen | umgeht nie das Merge-Gate |
| 4 | Pull Request als Objekt mit Reviewern, Prüfungen, Merge-Regeln | Nicht begonnen | |
| 4 | Signierte Freigaben mit Akteur, Modellversion, Commit, Ergebnis | Nicht begonnen | |
| 4 | Verbot der Selbstfreigabe | Nicht begonnen | |
| 4 | Risikostufen 0 bis 3 in einer Policy-Datei (selbst Stufe 3) | Nicht begonnen | Pfadlisten je Stufe offen |
| 4 | Automatische Eskalation bei gelockerten Tests | Nicht begonnen | |
| 5 | Übertragung einzelner Objekte über HTTP | Nicht begonnen | Einzelobjekte oder Pakete mit Deltas offen |
| 5 | Remote-Referenzen und Push mit Prüfung der Schreibhoheit | Nicht begonnen | |
| 5 | Server mit Speicherung von Objekten und Referenzen | Nicht begonnen | Authentifizierung für Menschen und Agenten offen |
| 5 | Namespaces für Organisationen, Kurse, Projekte | Nicht begonnen | |
| 5 | Durchsetzung der Policies nach ADR-003 | Nicht begonnen | ADR-003 noch offen; P2P-Sync: Buch sagt Phase 7, ADR-003 sagt Phase 4 |
| 6 | API-Server mit denselben Operationen wie die CLI | Nicht begonnen | |
| 6 | MCP-Server als Adapter | Nicht begonnen | Werkzeug-Granularität offen |
| 6 | Protokollierung jedes Aufrufs mit Akteur, Modellversion, Session | Nicht begonnen | |
| 6 | Begrenzung der Aufrufe pro Akteur | Nicht begonnen | Token und Ablauf für Agenten offen |

**Zu den Zahlen:** «Erledigt» zählt nur, was im Code steht und getestet ist. «Teilweise» zählt nicht mit. Die Zahl der Aufgaben je Phase ist eine Schätzung aus den Kapiteln und ändert sich, sobald eine Phase konkret geplant wird.

## Arbeitsweise und Konventionen

Die verbindlichen Regeln stehen in `CLAUDE.md` im Repository und gehen jeder Voreinstellung vor. Vor jeder Arbeit lesen.

**Sprache**

- Code, Bezeichner, Kommentare, Fehlermeldungen, Dateinamen, Commit-Nachrichten und PR-Texte sind englisch (akademisches Niveau). Beispiel: `docs/images/`, nicht `docs/bilder/`.
- Ausnahme: Der Prosatext des Buchs (`docs/main.tex` und `docs/chapters/`) und das Root-`README.md` bleiben deutsch. Das Buch wird später übersetzt, nicht vorher. Dateinamen und ConTeXt-Code sind trotzdem englisch. Dieses Handover ist ebenfalls deutsch, weil es für Daniel und für die Gespräche mit ihm geschrieben ist.
- Mit Daniel wird deutsch gesprochen, mit Schweizer Rechtschreibung («ss» statt «ß»).

**Commits und PRs**

- Format: `<Emoji> <typ>: <Beschreibung>`, Imperativ, höchstens 72 Zeichen in der ersten Zeile. Bisher verwendet: ✨ feat, 📚 docs, 🐛 und 🔒 fix, ♻️ refactor, 🔧 chore, 👷 ci.
- Nie eine `Claude-Session:`-Zeile oder einen anderen Link auf ein Transkript in Commits, PR-Beschreibungen oder Dateien des Repositorys. Wenn ein Werkzeug sie vorschlägt, weglassen. Die Zeile `Co-Authored-By:` wird verwendet.
- `CLAUDE.md` nennt den Skill `git-workflow:professional-commit-workflow` für Commits. In einer Cloud-Session ist er nicht verfügbar, dort werden Commits von Hand nach denselben Regeln geschrieben.
- Branch-Namen folgen dem Commit-Typ: `feat/…`, `fix/…`, `docs/…`, `refactor/…`. Eine Scheibe ist ein Branch, ein PR, ein Squash-Commit. Nach dem Merge ein neuer Branch von `main`.

**Qualitätssicherung**

- `just ci` (Format-Check, Clippy mit `-D warnings`, Tests) muss vor jedem Commit grün sein. `cargo fmt` vorher laufen lassen, sonst scheitert der Format-Check.
- Fehlertypen: Die Meldung eines Fehlers ist vollständig und enthält die Meldung der Ursache. `Error::source` liefert deshalb die Ursache der Ursache und nie die Ursache selbst, damit eine Fehlerkette jede Meldung nur einmal zeigt. Die Ursache bleibt als Feld der Variante erreichbar; `StoreError::io_error()` und `store_error()` an den Wrappern liefern sie typisiert, denn über `source()` findet `downcast_ref::<io::Error>()` sie nicht mehr. Eine CLI gibt einen Fehler mit `{}` aus und läuft die Kette nicht ab. Die Regel steht in der Crate-Doku von `lib.rs`; `properties.rs` prüft sie an einem Testspeicher (`FaultyStore`), der auf jedem System gleich scheitert.
- `just book` muss ohne Fehler und Warnungen durchlaufen, wenn das Buch geändert wurde.
- ConTeXt MkIV, nicht LaTeX: `{\bf …}` statt `\textbf`, `\startxtable` statt `\starttable`.
- Die Regel gilt für jeden Commit. Wer bei einer reinen Dokumentationsänderung `just ci` auslässt, sagt das ausdrücklich im PR.

**Zusammenarbeit zwischen Claude und Daniel**

- Claude schreibt Code und Dokumente und committet im Ordner `~/GitRepository/stemma`. In einer Cloud-Session ist das über die Gerätebrücke erreichbar.
- Eine Cloud-Umgebung hat weder Rust noch ConTeXt und keine GitHub-Zugangsdaten. Daniel führt deshalb auf dem Mac `cargo fmt && just ci` aus und schickt die Ausgabe. Erst danach wird committet.
- Push, `gh pr create` und Merge macht Daniel auf dem Mac. Claude liefert die fertigen Befehle und den PR-Text.
- Der Ordner in der Cloud-Session ist derselbe wie Daniels Arbeitskopie. Ein `git switch` dort ändert auch seine ausgecheckte Version. Branch-Wechsel vorher ansagen.
- Daniel möchte, dass Ideen kritisch hinterfragt werden, Arbeit in kleinen Scheiben läuft und teure Entscheidungen vorher geklärt werden.

## Nächste Schritte und offene Entscheidungen

Der nächste Schritt sind die Referenzen für `main` und HEAD, danach die Repository-Schicht mit `stemma init`. Jeder Punkt ist eine eigene Scheibe mit eigenem Branch und PR.

**Reihenfolge**

1. Stand prüfen: aktuellen `main` holen, `ls adr` und `git log --oneline -15` lesen, offene PRs auf dem Mac ansehen.
2. Referenzen: Modul für `refs/heads/<name>` und HEAD (symbolischer Verweis), mit Regel für Branch-Namen.
3. Repository-Schicht und `stemma init`: legt `.stemma/` an mit `format`, `objects/`, `tmp/`, `refs/heads/` und `HEAD`. Der Standard-Branch ist **konfigurierbar** (so hat Daniel entschieden), zum Beispiel über `--initial-branch`; ohne Angabe `main`. Ein zweites `init` im selben Ordner meldet einen Fehler und ändert nichts. Der Speicher soll die Formatdatei prüfen (ADR-007).
4. `stemma add`, danach `commit`, `status`, `log` als weitere Scheiben. Jede Repository-Operation ruft den Autorisierungs-Hook auf.
5. Status-Zeilen der ADRs nachziehen (siehe Abschnitt Entscheidungen) und ADR-Entwürfe für das, was dabei neu entschieden wird.
6. Buch nachführen: Kapitel 04 (Phase 1) zum aktuellen Stand, den Abschnitt «Der Name» in die Einleitung übernehmen (angeboten, noch nicht beantwortet), eine zitierfähige Quelle zur Stemmatologie ergänzen. Danach `just book`.
7. CI: Entscheiden, wann der Workflow wieder für jeden PR und jeden Push auf `main` läuft (ADR-006).

**Noch nicht entschieden**

| Frage | Stand |
| --- | --- |
| Regel für Branch-Namen | Vorschlag: nur `A-Z a-z 0-9 . _ -` und `/` als Trenner, kein führender Punkt oder Bindestrich. Daniel hat noch nicht geantwortet. Lockern ändert keine ID. |
| Verteilt oder zentral | Web3-Interesse, aber offen für zentrale Vorteile. ADR-003 enthält die Frage nach P2P-Sync. |
| Agenten-Identität | eigene Rolle oder Rolle mit menschlichem Sponsor (ADR-002 und 004) |
| Working Copy als Commit (wie jj), Konfliktmodell, Rebase | nicht behandelt |
| Lease-Dauer, Fast-Track für triviale Änderungen | nicht behandelt |
| Pfadlisten je Risikostufe | Kategorien da, Verzeichnisse nicht (ADR-004) |
| Packfiles, Garbage Collection, Streaming | offen (ADR-007) |
| Verhalten des Speichers unter Windows | fünf Tests laufen nur unter Unix (ADR-007) |
| Squash als einzige Merge-Methode, Branch-Lebensdauer, Branch-Protection | offen (ADR-006) |

## Stolpersteine und Lehren

Die meisten Probleme kamen aus drei Quellen: einer Umgebung ohne Compiler, einem gemeinsam genutzten Arbeitsordner und mehreren Sessions, die gleichzeitig am selben Repository arbeiten.

| Problem | Was passiert ist | Was dagegen hilft |
| --- | --- | --- |
| Kein Compiler in der Cloud-Session | Code konnte nicht gebaut oder formatiert werden. Beim Durchlesen fielen zwei Compile-Fehler auf, die Formatierung musste Daniel prüfen. | Code vor der Übergabe sorgfältig lesen, danach `cargo fmt && just ci` auf dem Mac abwarten, erst dann committen. |
| Kein Push aus der Cloud-Session | Es gibt keine GitHub-Zugangsdaten, `git fetch` scheitert. `gh` gibt es nur auf Daniels Mac. | Push und PR-Befehle fertig an Daniel geben. |
| Löschen im Geräte-Ordner verboten | `git` lässt `.lock`- und `tmp_obj`-Dateien liegen, die Daniels Git-Befehle blockieren können. | Alte Sperrdateien mit `mv` in einen neuen Namen direkt unter `.git/` umbenennen (nie unter `.git/refs`), statt sie zu löschen. Daniel darf sie danach löschen. |
| Gemeinsamer Arbeitsordner | Ein `git switch` in der Cloud-Session wechselt auch Daniels ausgecheckten Branch. | Vorher fragen oder ansagen, auf welchem Branch gearbeitet wird. |
| Parallele Sessions | Während einer Session sind PRs entstanden, die nicht von ihr stammen. ADR-005 war schon für die Objektkodierung vergeben, als «Trunk-based» als ADR-005 geplant war. Es wurde ADR-006. Auch ein Handover wird schnell alt: Zwischen Schreiben und Ablegen dieser Datei sind drei PRs gemergt worden. | Vor dem Anlegen von Branches und ADRs `git log`, `ls adr` und `adr/README.md` lesen. |
| Vorgaben überschrieben | Anfangs wurde Daniels `docs/main.tex` mit eigener Fassung überschrieben (Präambel weg), und es standen `\starttable`, `\textbf` und deutsche Kapitel-Dateinamen drin. | Datei vor dem Ändern lesen, `CLAUDE.md` beachten. Das ist inzwischen behoben. |
| Session-Links in Commits | Das Werkzeug schlägt eine `Claude-Session:`-Zeile vor. `CLAUDE.md` verbietet sie. | Die Zeile weglassen. `CLAUDE.md` geht vor. |
| Zusammenfassungen von Webseiten | Beim Beleg zur Namensherkunft nannte eine Zusammenfassung andere Stellenangaben als die Erinnerung (Plinius, Juvenal). | Stellenangaben nur mit Beleg in den Text schreiben. Sonst nur das Buch oder die Satire nennen. Im README steht Juvenal, Satire 8, und Plinius nur mit Buch 35. |
| Formate sind fest | Kodierungen sind persistent. Eine Änderung ändert jede betroffene ID. | Golden-Vector-Tests nicht anpassen, ohne einen ADR zu schreiben. Ein unvereinbares Format bekommt einen neuen Objektnamen. |

## Referenzen

| Was | Wo |
| --- | --- |
| Repository | GitHub `talent-factory/stemma`, lokal `~/GitRepository/stemma` |
| Regeln für die Arbeit | `CLAUDE.md` im Wurzelverzeichnis |
| Überblick, Herkunft des Namens | `README.md` (deutsch) |
| Entscheidungen | `adr/README.md` und `adr/ADR-001` bis `ADR-007` (englisch) |
| Buch | `docs/main.tex`, `docs/chapters/`, `docs/references.bib`; Bau mit `just book` |
| Aufgaben | `justfile` (`just` zeigt alle Rezepte) |
| CI | `.github/workflows/ci.yml`, Start mit `gh workflow run ci.yml --ref <branch>` |

**Externe Quellen**

- [jj-Dokumentation](https://docs.jj-vcs.dev/latest/): Change-IDs, Konflikte als Daten, Operation-Log.
- Namensherkunft: [Wiktionary, *stemma*](https://en.wiktionary.com/wiki/stemma), [Stemma: a term evolves in Latin](https://www.piggin.net/stemmahist/latin.htm), [Juvenal, Satire 8](https://pages.pomona.edu/~cmc24747/sources/juvenal/juv_8.htm).
- Coglan, *Building Git*: dient nur als Ideenquelle.

## Startprompt für die neue Session

Diesen Text in die neue Session kopieren. Er verweist auf dieses Dokument und auf die Dateien, die den Stand verbindlich festhalten.

```markdown
Du setzt die Arbeit an Stemma fort, einem API-first Versionskontrollsystem in Rust (Talent Factory GmbH, Daniel Senften). Das Repository liegt lokal unter ~/GitRepository/stemma und auf GitHub unter talent-factory/stemma.

Lies zuerst, in dieser Reihenfolge:
1. docs/HANDOVER.md, CLAUDE.md, README.md und adr/README.md im Repository, danach die ADRs, die für die Aufgabe relevant sind.
2. Den Repository-Stand: git status, git branch -a, git log --oneline -15. Andere Sessions arbeiten parallel; ADR-Nummern, Branches und PRs können sich seit dem Handover geändert haben. Das Repository gilt, nicht das Handover.

Arbeitsweise:
- Mit Daniel auf Deutsch (Schweizer Rechtschreibung, ss statt ß). Code, Kommentare, Commits und PR-Texte auf Englisch. Das Buch (docs/) und das Root-README bleiben deutsch.
- Trunk-based: eine kleine Scheibe pro Branch und PR, Branch von aktuellem main, nach dem Merge ein neuer Branch.
- Commits im Format "<Emoji> <typ>: <Beschreibung>", Imperativ, höchstens 72 Zeichen. Keine Claude-Session-Zeilen.
- Vor jedem Commit muss `cargo fmt && just ci` auf Daniels Mac grün sein. Falls du keinen Compiler hast, schreibe den Code sorgfältig, bitte Daniel um die Ausgabe und committe erst danach. Push, PR und Merge macht Daniel; liefere ihm die fertigen Befehle.
- Der Arbeitsordner ist Daniels Arbeitskopie. Sage an, bevor du den Branch wechselst.
- Stelle Daniel Fragen, wenn eine Entscheidung teuer zu ändern ist. Hinterfrage Ideen kritisch.

Nächste Aufgabe: die Referenzen für main und HEAD (Modul refs), danach die Repository-Schicht mit `stemma init` und konfigurierbarem Standard-Branch. Frage Daniel zuerst nach der Regel für Branch-Namen (siehe Abschnitt «Noch nicht entschieden» in docs/HANDOVER.md).
```
