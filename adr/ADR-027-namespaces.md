# ADR-027: Namespaces

**Status:** Accepted for phase 5. Decided by the author on 2026-11-24 (STEMMA-276, with the
addendum of STEMMA-278) and carried into the ADRs by STEMMA-280. Nothing of it was measured or
built. `.stemma-namespace-policy` is a working name.

## Context

Locally there is one policy per repository (ADR-003). Points 6 and 7 of the hand-over in ADR-003
ask a hub for policies per namespace with their floor at level 3 (rules 5 and 8 of ADR-004) and
for the visibility of an owner override per course (rule 9). A hub also needs names, an answer to
who may create and read a repository, and a way to stop a repository after a takeover (ADR-003).

This ADR records the rules that a clone and the core must know. The files in which a hub keeps its
accounts and namespaces, its settings and its operation belong to stemma-hub.

## Decision

### A namespace is flat

A namespace is a record with an ID of its own and a name, and every repository belongs to exactly
one. The address of a repository is always `<hub>/<namespace>/<repository>`, two components, so it
cannot be confused with the endpoints below it (ADR-023). An organisation, a course and a project
are the same thing and no type; an organisation with three courses creates three namespaces.
Exactly one namespace policy holds for a repository. A later parent field stays possible, because
a name only points to an ID.

Rejected: a tree of organisation, course and project (a chain of policies per repository,
addresses of variable depth, and inherited reading as a new source of leaks), a pure path prefix
(a rename would change rights), and two fixed levels.

### A namespace repository holds the rules

Every namespace has exactly one Stemma repository of its own on the hub, under a reserved name. It
holds the namespace policy as a file and its register; the human owners of this register are the
owners of the namespace. A change takes the ordinary way: a pull request, level 3, the record and
the anchors. What changes often or what not every reader should see (names, readers, bounds) stays
data of the hub.

An account reserves the name of a namespace with a signed request, and the hub assigns the ID. The
namespace comes to be with the first push of its namespace repository, which the hub accepts only
if the register holds the key of the request as an owner: the rule of ADR-024 for every
repository. After that the owners are the human owners in this register alone, and the hub keeps
no second list of owners. A reservation that is not completed lapses. An agent has no account and
creates no namespace.

Rejected: a namespace repository for everything (entering thirty students as readers would be
level 3 commits), an ordinary repository that carries the policy, a namespace that only the
operator creates, and a personal namespace per account.

### The register stays the only source of roles

Owners of a namespace have rights above the repository only: create, rename, delete, readers,
bounds. The namespace policy may demand with one line that the register of every repository of the
namespace holds the human owners of the namespace repository as owners. The hub then only refuses:
a first push, and every update of `main`, whose register does not hold them. `op verify` comes to
the same result without the hub, because the role stands in the register.

The price: when an owner of the namespace changes, every repository brings its register into line
(level 3), and a removed owner stays entered until then.

Rejected: no demand (a course could not guarantee the owner override of rule 9 of ADR-004), and
owners of the namespace that count as owners everywhere without a register entry (the hub would
allow more than the register).

### Both policies are evaluated, and the stricter counts

The accepting operation (ADR-025) reads `.stemma-policy` of `T` and the namespace policy at the
tip of `main` of the namespace repository and takes the maximum per change: the level over the
touched paths, the number of approvals, the tighter bounds.

- The namespace policy is a file with a fixed name in the root of the namespace repository
  (working name `.stemma-namespace-policy`), apart from the `.stemma-policy` of that repository,
  which rules the namespace repository alone. It has the format `stemma-policy` with the same
  rules of validity, so by its form it can only raise.
- The core takes the name into the list that is always level 3 and that no policy lowers (rule 8
  of ADR-004). The file has no signature of its own: it changes as every level 3 file does.
- The version at the tip of `main` at the moment of the ref update counts, so a raise acts on open
  pull requests at once. The floors of the core hold in the combination, because a maximum never
  falls below them.
- The lines of the hub in a record entry name the commits of the policies as a list, and a fetch
  brings the namespace policy into the clone, so the local gate comes to the same verdict. A
  namespace policy and the demand for owners are enforced by the hub alone; a clone evaluates the
  namespace policy it last fetched.

Rejected: a containment check (whether one list of patterns is at least as strict as another is no
simple pure function, and every raise would need a level 3 commit in every repository), a
namespace policy that replaces the policy of the repository, and an additional signature of an
owner.

### Accounts, grants and reading

- An account is data of the whole hub and no register: a register answers who may change a
  repository, and an account never grants anything there. An account is the same in every
  namespace, and the binding to a register stays the equality of the key (ADR-024).
- A namespace knows two grants, each to an account name: `create` (the account may create
  repositories in the namespace, within the bounds) and `read` (it reads every private repository
  of the namespace). An owner of the namespace grants and withdraws with a signed statement, a new
  type of proof that binds the hub, the namespace ID, the account and the right. The hub checks it
  against the register at the tip of `main` of the namespace repository and appends it with its
  own signed lines; the list is the fold of these statements, in the order of the hub. It is
  visible to the owners and to every account in its own lines. Owners have
  both rights by themselves. There are no readers per single repository: the register serves that.
- Who may read a repository (it is public, the register of `main`, or the grant `read`) reads all
  its branches and tags, the record, the statements and the anchors. What not everyone should see
  belongs in a repository of its own.
- The namespace repository is readable by everyone who may read a repository of the namespace, so
  without a token if one of them is public. Otherwise a clone could not recompute the namespace
  policy.
- An owner of the namespace may set "private only" with a signed statement. The hub then refuses
  to make a repository of this namespace public and treats those that are public as private while
  the setting holds. A namespace cannot make a repository public.

Rejected: a coarse role from the token of the identity provider, a list changed with the token
alone (the token is transport and no proof), readers per repository in addition, and reading per
reference (ADR-015: every clone holds the full store).

### Names and IDs

- A name is one component of `a-z`, `0-9` and the hyphen inside it, 1 to 64 characters, beginning
  with a letter or a digit. The hub refuses everything else with a typed error and normalises
  nothing. Loosening the rule later breaks nothing; tightening it would.
- Reserved are the names of the endpoints (`challenge`, `token`, `refs`, `fetch`, `objects`,
  `anchors`, `statements`, `record`), the fixed name of the namespace repository, and one name for
  later endpoints (working name `api`), for namespaces and repositories alike.
- Namespaces share one name space on the hub and repositories one per namespace; account names
  follow the same rule in a name space of their own.
- The ID of a repository is 128 random bits, written in lower-case hexadecimal, assigned by the
  hub when the repository is created explicitly, before the first push, and never used again. A
  namespace gets an ID of the same form.
- A repository is renamed by an owner of its namespace and a namespace by one of its owners, each
  with a signed statement. The ID, the record and prepared updates stay valid. The old name stays
  taken and answers readers with a typed error that names the new name, until an owner releases
  it; the client reports this and does not change its configuration on its own.

Rejected: the rule of a branch name of ADR-008 (it allows capitals), Unicode with normalisation, a
running number as the ID, and the hash of the first register as the ID.

### Deleting, freezing, and the limits of a namespace

- An owner of the namespace deletes a repository with a signed statement that names the ID. From
  then on the hub does not serve it and answers as for a missing one, but keeps it for a grace
  period in which an owner of the namespace can restore it. The ID is never assigned again, and
  the name stays taken until the period ends. A namespace can be deleted only when it holds no
  repository but its namespace repository. The
  owners of a repository do not delete it: creating and deleting are rights above the repository.
- A repository cannot change its namespace in phase 5.
- An owner of the namespace freezes a repository with a signed statement and a reason and lifts
  the freeze in the same way. While it holds, the hub refuses every ref update and every statement
  to this repository; reading goes on, and the state is visible to all readers. This only refuses,
  so `op verify` does not change.
- A role of the namespace never sets a repository back: the hub would allow a move that the
  register does not cover. The way back is ordinary proofs (ADR-003).
- Three bounds hold beside the quota per repository (an upper bound on the bytes of the whole
  store, named and unnamed objects together): the number of repositories per namespace, the
  sum of the bytes of all repositories of a namespace, and the number of namespaces that an
  account created. They are settings of the hub. The namespace policy sets no bounds: they are
  operation and no rule of a repository.

### An owner override is seen by the readers of the repository

The override reaches the hub as the signed statement of ADR-025 and is seen by exactly the readers
of the repository. In addition the hub serves per namespace a derived list of all overrides,
filtered to the repositories that the requester may read: a fold over the statements, with no new
format. A teacher sees the whole course, a student sees their own repositories, and nobody learns
that a foreign private repository exists. An override cannot be deleted or hidden on the hub, not
by an owner either. Rule 9 of ADR-004 is met.

Refused attempts are not shown per namespace in phase 5. The trigger for later is a course that
asks for it explicitly; it would then be a list of the hub that changes no format of the protocol.

### What the model keeps open for a further layer

Three properties allow a policy of a whole instance or of a customer as a third layer without a
break (point 9 of the hand-over in ADR-003, which stays out of scope):

1. The combination is the maximum over any number of policies of the same format, not a special
   rule for exactly two.
2. The lines of the hub in the record name the commits of the policies as a list, today with one
   entry.
3. Every further policy is again content of a Stemma repository with a register and level 3, never
   a setting of an operator.

## Consequences

- A course is a namespace: the teacher owns the namespace repository, sets the namespace policy
  and may demand to be an owner of every repository of the course.
- The policy format gets the line that demands the owners of the namespace, and rule 8 of ADR-004
  names one more file, which is a level 3 change of the core.
- New types of proof arise: the reservation of a namespace, the grant and its withdrawal, the
  rename, the deletion and its restoration, the freeze and its lifting, and "private only".
- A change of the owners of a namespace is expensive where the namespace demands them in every
  register.

## Open items

- [ ] How a level 3 change is approved in a namespace repository with a single human owner. The
      rule of phase 4 applies unchanged; the slice checks that it is workable for a course with
      one teacher.
- [ ] The change of a namespace by a repository. The model does not rule it out, because the ID
      stays, but it would need the consent of both sides and changes the namespace policy that
      holds without a commit.
- [ ] A list of refused attempts per namespace (trigger: a course asks for it).
- [ ] The settings of a hub (who may create a namespace, the bounds, the grace periods) and its
      file formats belong to stemma-hub.
