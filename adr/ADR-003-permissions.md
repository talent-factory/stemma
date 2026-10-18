# ADR-003: Location of Permissions

**Status:** Proposal, open.

## Decision (proposal)

- Roles and policies reside on the server and are enforced there.
- Signatures of commits and approvals reside in the repository and can be verified independently of the server.
- Policy files are per namespace and are themselves level 3 (ADR-004). They are signed.

## Open

- [ ] Agreement with the proposal?
- [ ] Peer-to-peer sync as a phase 4 goal, or deliberately excluded?

## Consequences

- From phase 2 onwards a hub is required, because protected branches are enforced there.
- Local work remains possible. Writing to protected branches requires the server.
- Peer-to-peer sync is conceivable later without losing enforcement, because the signatures reside in the repository.
