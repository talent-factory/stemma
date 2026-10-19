# ADR-002: Actor Model and Authorization Hook

**Status:** Proposal, partially confirmed.

## Decision (proposal)

- An actor is a human or an agent. Agents carry model version, session, and sponsor.
- Every commit and every operation log entry records the actor.
- Authorization in the core goes through `authorize(actor, operation, target)`. Phase 1: everything is allowed, but the call is mandatory.
- The CLI, API, and MCP server call the same core.
- Roles belong to the policy model, not to the core code.
- An actor is valid by construction *(confirmed)*: `Actor::human` and `Actor::agent` check every
  field (the rules are in ADR-005), so the authorization hook and the commit encoding never see an
  invalid actor.

- **The command line takes the actor from `--actor` or `STEMMA_ACTOR`** *(confirmed)*. The text is
  the actor line of a commit (`human <id>` or
  `agent <id> model=<model> session=<session> sponsor=<sponsor>`), read by `Actor::from_str`. The
  flag goes before the variable. An empty flag or variable is an error of its own ("the actor is
  empty"), and an empty flag does not fall back to the variable. Without an actor a command
  fails with exit code 1 and creates nothing. There is no default such as the name of the
  operating system user: an agent must never act as a human without saying so. The text is not
  checked beyond the actor rules, so it proves nothing about who runs the command (ADR-003 and
  ADR-004 propose what would).

Implementation: `crates/stemma-core/src/actor.rs` (actors), `crates/stemma-cli/src/cli.rs` (the
actor of the command line), and `crates/stemma-core/src/lib.rs`
(authorization hook).

## Open

- [ ] Agent identities: a dedicated role, or a role with a human sponsor (see ADR-004).
- [ ] A configuration file as a third source of the actor of the command line, after the flag and
      the variable.

## Consequences

- Actor fields exist from the first commit. Adding them later would break the format.
- An agent acts on behalf of a human, who is accountable for its actions.
