# ADR-002: Actor Model and Authorization Hook

**Status:** Accepted. Confirmed by the author on 2026-10-24 (STEMMA-9) and implemented in Phase 1.
The open items move to Phase 4 (agent identities) and to a later phase (a configuration file).

## Decision

- An actor is a human or an agent *(confirmed)*. Agents carry model version, session, and
  sponsor.
- Every commit and every operation log entry records the actor *(confirmed)*.
- **Every operation that changes a repository, or plans a change, goes through
  `authorize(actor, operation, target)`** *(confirmed)*. Phase 1: everything is allowed, but the
  call is mandatory. A plan counts, as it reads what the change would touch: `purge` without
  `--apply` and `op restore --dry-run` have an actor and call the hook. The commands that only
  read the log (`op log`, `op verify`) have no actor and call no hook; who may read a repository
  is the question of ADR-003, for the hub of Phase 5. Objects stored directly through
  `Repository::objects()` are not authorized; no command does that (ADR-007, ADR-009).
- **A write cannot start without the answer of the authorizer** *(confirmed)*. The answer is a
  value, `Allowed`, which only a `Decision::Allow` can make: its field is private to the module
  that holds it, so no other code can forge it. The functions that open a write take it as an
  argument: the three locks of the references (`lock_writer`, `lock_purge`, `take_over_lock`), the
  mutators of the `RefStore` trait (`set_head`, `update_branch`), and the installation of a new
  repository for `init`. A write path of `Repository` that never asks the authorizer has no
  `Allowed` to pass, so it does not compile. The proof does not say which operation was allowed,
  and it is bound to no target and no repository: it is the check that the question was asked,
  and the call that asks stays responsible for writing what it asked for. Two things
  are not covered by the type. The operation log is written through `Repository::append` and
  `Repository::append_purge`, which take the lock of the references; `FsOpLog::append` itself
  stays visible to the whole crate, because Rust cannot restrict an item to a sibling module, so a
  caller inside the crate could still skip the lock. And `Repository::apply_checkout` writes the
  working directory without an actor.
- The CLI, API, and MCP server call the same core *(confirmed)*. Phase 1 has the CLI; the API
  comes with the hub (Phase 5), the MCP server with the agent interfaces (Phase 6).
- **Roles belong to the policy model, not to the core code** *(confirmed)*, with one exception:
  only a human may purge (ADR-014). A purge cannot be undone, so the rule is fixed in the core,
  where every step of a purge (plan, `--apply`, `--resume`, `--abort`) refuses an agent before
  it asks the authorizer, and it is no policy that a configuration could lift.
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

Implementation: `crates/stemma-core/src/actor.rs` (actors), `crates/stemma-core/src/allowed.rs`
(the proof), `crates/stemma-cli/src/cli.rs` (the actor of the command line), and
`crates/stemma-core/src/lib.rs` (authorization hook).

## Open

- [ ] Agent identities: a dedicated role, or a role with a human sponsor (see ADR-004). Phase 4.
- [ ] A configuration file as a third source of the actor of the command line, after the flag and
      the variable. Later; Phase 1 has no configuration file (ADR-009).

## Consequences

- Actor fields exist from the first commit. Adding them later would break the format.
- An agent acts on behalf of a human, who is accountable for its actions.
