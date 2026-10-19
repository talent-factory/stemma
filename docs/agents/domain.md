# Domain Docs

How the engineering skills should consume this repo's domain documentation when exploring the codebase.

## Before exploring, read these

- **`GLOSSARY.md`** at the repo root.
- **`adr/`**: read ADRs that touch the area you're about to work in. This repo keeps its ADRs in `adr/` at the repo root, not in `docs/adr/`, and names them `ADR-NNN-<slug>.md`. `adr/README.md` lists them.
- **`docs/HANDOVER.md`** and the book under `docs/` carry the design background. Read them when the task touches the design, and treat the code and ADRs as authoritative where they differ.

If any of these files don't exist, **proceed silently**. Don't flag their absence; don't suggest creating them upfront. The `/domain-modeling` skill (reached via `/grill-with-docs` and `/improve-codebase-architecture`) creates them lazily when terms or decisions actually get resolved.

## File structure

Single-context repo:

```
/
├── GLOSSARY.md
├── adr/
│   ├── README.md
│   ├── ADR-001-hash-function.md
│   └── ADR-002-actors.md
├── crates/
└── docs/
```

New ADRs continue the existing numbering (`ADR-009-<slug>.md`), in English like all other source files.

## Use the glossary's vocabulary

When your output names a domain concept (in an issue title, a refactor proposal, a hypothesis, a test name), use the term as defined in `GLOSSARY.md`. Don't drift to synonyms the glossary explicitly avoids.

If the concept you need isn't in the glossary yet, that's a signal: either you're inventing language the project doesn't use (reconsider) or there's a real gap (note it for `/domain-modeling`).

## Flag ADR conflicts

If your output contradicts an existing ADR, surface it explicitly rather than silently overriding:

> _Contradicts ADR-007 (object storage), but worth reopening because…_
