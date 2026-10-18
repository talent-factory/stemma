# ADR-002: Actor Model and Authorization Hook

**Status:** Proposal, partially confirmed.

## Decision (proposal)

- An actor is a human or an agent. Agents carry model version, session, and sponsor.
- Every commit and every operation log entry records the actor.
- Authorization in the core goes through `authorize(actor, operation, target)`. Phase 1: everything is allowed, but the call is mandatory.
- The CLI, API, and MCP server call the same core.
- Roles belong to the policy model, not to the core code.

Implementation: `crates/stemma-core/src/lib.rs`.

## Open

- [ ] Agent identities: a dedicated role, or a role with a human sponsor (see ADR-004).

## Consequences

- Actor fields exist from the first commit. Adding them later would break the format.
- An agent acts on behalf of a human, who is accountable for its actions.
