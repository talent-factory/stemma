# Issue tracker: Plane

Issues and specs for this repo live in the Plane project **Stemma** (identifier `STEMMA`,
work items such as `STEMMA-12`). Use the `plane-tf` MCP server for all operations; there is no CLI.

- Web UI: https://plane.talent-factory.ch/tf/projects/d988f244-ae6e-492d-a61e-c335c6c87de9/issues
- Project id: `d988f244-ae6e-492d-a61e-c335c6c87de9` (pass it as `project_id` to every call)
- Tools: `mcp__plane-tf__workitem`, `label`, `state`, `cycle`, `module`, `workitem_relation`.
  Load them through `ToolSearch` first if they are deferred.

## Conventions

- **Create an issue**: `workitem create` with `project_id`, `name`,
  and `description_html` (or `description_stripped` for plain text).
  Set `priority` (`urgent`, `high`, `medium`, `low`, `none`) when it is known.
- **Read an issue**: `workitem retrieve_by_identifier` with `workitem_identifier="STEMMA-12"`.
  Comments are not part of that result; read them through the web UI if a skill needs them.
- **List issues**: `workitem list` with `project_id`.
  This Plane edition rejects `pql` filters, so filter the result on the client side
  (for example by `state` with `expand=state`).
  The issues of a phase come from `cycle list_workitems` (see below).
- **Update an issue**: `workitem update` with `project_id`, `workitem_id` (the UUID,
  not `STEMMA-12`) and only the fields that change.
- **Apply / remove labels**: `workitem manage_label` with `add_label_id` / `remove_label_id`.
  Labels are referenced by UUID: resolve a name with `label list` first.
  Create a missing label with `label create`.
- **Change state**: `workitem update` with `state` set to a state UUID from `state list`.
  States are Backlog (default), Todo, In Progress, Done, Cancelled.
- **Close**: set the state to Done (finished) or Cancelled (will not be actioned).
  There is no separate close action.

Work item types are disabled in this project, so there are no epics.
Group related work with parent and child items (`parent`) or with modules.

## Phases

Each phase of the roadmap is a Plane cycle, such as `Phase 1: Lokaler Kern`.
Phases are not labels: every phase from 1 to 6 has its cycle, and the former labels
`phase-1` to `phase-6` were removed.

- **Find the cycle**: `cycle list` and match the name; pass its `id` as `cycle_id`.
- **Issues of a phase**: `cycle list_workitems` with `project_id` and `cycle_id`.
- **Assign an issue to a phase**: `cycle manage_workitems` with `add_ids` (work item UUIDs).
  An issue belongs to at most one cycle, so one issue cannot sit in two phases.
  An issue that concerns two phases sits in the cycle where it is needed first,
  and its description names the other phase.
- **Close a phase**: `cycle complete`, then `cycle transfer_workitems` moves the unfinished
  issues into the cycle of the next phase. Completing needs the start and end date of the cycle.
- **Dates of a cycle**: the web UI sets the start and the end date only together;
  `cycle update` with `start_date` alone sets the start of a cycle that has no end yet.

## Pull requests as a triage surface

**PRs as a request surface: no.** _(Set to `yes` if this repo treats external PRs as feature
requests; `/triage` reads this flag.)_

## When a skill says "publish to the issue tracker"

Create a Plane work item in the project above.

## When a skill says "fetch the relevant ticket"

Run `workitem retrieve_by_identifier` with the identifier, for example `STEMMA-12`.

## Wayfinding operations

Used by `/wayfinder`. The **map** is a single work item with **child** work items as tickets.

- **Map**: a work item labelled `wayfinder:map`, holding the Notes / Decisions-so-far / Fog body.
- **Child ticket**: a work item whose `parent` is the map. Labels:
  `wayfinder:<type>` (`research`/`prototype`/`grilling`/`task`).
  Once claimed, the ticket is assigned to the driving dev.
- **Blocking**: `workitem_relation create` with `relation_type="blocked_by"`.
  A ticket is unblocked when every blocker is in the Done or Cancelled state.
- **Frontier query**: list the map's open children (`workitem list` with a `pql` filter on the
  parent and the open state groups), drop any with an open blocker or an assignee;
  first in map order wins.
- **Claim**: `workitem manage_assignee` with `add_user_id` set to the driving dev,
  the session's first write.
- **Resolve**: add the answer to the ticket (comment through the web UI,
  or append it to the description with `workitem update`), set the state to Done,
  then append a context pointer (gist + link) to the map's Decisions-so-far.
