# ADR-001: Hash-Funktion und Hash-Agilität

**Status:** Vorschlag, offen.

## Entscheidung (Vorschlag)

- BLAKE3 als Standard-Hash. Schnell, parallelisierbar, ausgereifte Rust-Implementierung.
- SHA-256 als Alternative, falls Standardisierung oder Zertifizierung gefordert wird.
- Jede Objekt-ID trägt den Algorithmus als Präfix, zum Beispiel `b3:…`.

## Offen

- [ ] Endgültig: BLAKE3 oder SHA-256?
- [ ] Werden Standard-Signaturformate oder Zertifizierung verlangt?
