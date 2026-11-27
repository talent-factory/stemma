//! The gate on a hub (ADR-019, ADR-025, STEMMA-303): the accepting operation judges a move of
//! `main` from its tip `T` to a merge `M` with the verdict that the local gate reaches
//! ([`judge_main`](Repository::judge_main)), from the commits, the register and the policy of
//! `T`, the further policies of the context, and the statements that the hub holds.
//!
//! The hub recomputes everything and takes nothing from the sender but `M` and the evidence line
//! that names the pull request: `M` has the single parent `T`, its tree is the clean merge of the
//! head onto `T`, and the level, the checks, the approvals, and the bounds are counted over the
//! held statements in the order of the hub. The hub merges nothing and creates no commit
//! (ADR-015): the tree merge writes blobs and trees only, which a purge treats as every object
//! that no reference reaches (requirement 4 of STEMMA-278).

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

use super::statement::{placed_in_order, Actors};
use super::{AcceptError, Record};
use crate::conflict::AnyCommit;
use crate::evidence;
use crate::object::CommitId;
use crate::policy::Policy;
use crate::protocol::held::HeldStatement;
use crate::protocol::record::Move;
use crate::protocol::refusal::{GateRefusal, RefusalReason};
use crate::protocol::statement::{Statement, StatementKind};
use crate::protocol::Reason;
use crate::register::{Identity, KeyState, ModeError, ProofError, Register};
use crate::repository::verdict::{Case, Proofs, Refusal, Rules, Verdict};
use crate::repository::{GateError, LevelError, Pause, PolicyError, Repository};
use crate::signature::PublicKey;
use crate::store::ObjectStore;
use crate::Actor;

/// The proofs of a hub: the register of `T` judges every signature, and a key that is no longer
/// active counts by the order of the hub. A statement counts with a retired key if it lies
/// before the record entry that brought the retirement to `main`, and with a revoked key if it
/// is no approval and lies before the entry that brought the revocation; the approvals of a
/// revoked key are void (ADR-003, ADR-025).
struct HubProofs<'a> {
    register: &'a Register,
    store: &'a dyn ObjectStore,
    record: &'a Record,
    /// The `record` line of every placed statement, by its number in the fold.
    order: &'a BTreeMap<u64, u64>,
    /// The registers of the commits that the record moved `main` to, as far as they are read.
    /// `None` for a register that could not be read.
    registers: RefCell<HashMap<CommitId, Option<Option<Register>>>>,
    /// The first register of the record that could not be read. The verdict counts the key as
    /// ended there, and the hub then fails instead of answering with the verdict: the refusal
    /// would read as final, though a repeat may find the store whole.
    unreadable: RefCell<Option<ModeError>>,
}

impl HubProofs<'_> {
    /// The number of the first record entry that moved `main` to a commit whose register no
    /// longer holds `key` of `identity` as active, after one that held it so, or `None`. A
    /// register before the key was added does not hold it and ends nothing. After that, every
    /// entry ends the key whose register does not hold it as active: one that retired or
    /// revoked it, one that removed the key or its identity, one without a register, and one
    /// whose register cannot be read. A store that fails must not widen the window in which a
    /// key counts, so the reading fails closed.
    fn ended_at(&self, identity: &Identity, key: &PublicKey) -> Option<u64> {
        let main = super::main_reference();
        let mut registers = self.registers.borrow_mut();
        let mut seen = false;
        self.record.entries.iter().find_map(|entry| {
            let commit = entry
                .update()
                .payload()
                .moves()
                .iter()
                .find(|moved| moved.name() == &main)
                .and_then(Move::new_commit)?;
            let register = registers.entry(commit).or_insert_with(|| {
                Register::at_commit(self.store, &commit)
                    .map_err(|error| {
                        self.unreadable.borrow_mut().get_or_insert(error);
                    })
                    .ok()
            });
            let state = register
                .as_ref()
                .and_then(Option::as_ref)
                .and_then(|register| register.entry(identity))
                .and_then(|held| held.keys().iter().find(|held| &held.key == key))
                .map(|held| held.state);
            let active = state == Some(KeyState::Active);
            // Unread or without a register, an entry ends the key even before it was seen, as
            // nothing tells when it was added.
            let unknown = !matches!(register, Some(Some(_)));
            let ended = (seen && !active) || unknown;
            seen |= active;
            ended.then_some(entry.number())
        })
    }
}

impl Proofs for HubProofs<'_> {
    fn holds(&self, actor: &Actor, number: u64, statement: &Statement) -> Result<(), String> {
        let record = self
            .order
            .get(&number)
            .ok_or("the hub holds it in no line of its order")?;
        let key = statement.key().ok_or("the statement is not signed")?;
        let identity = Identity::of(actor);
        let before = |ended: Option<u64>| match ended {
            Some(ended) if *record < ended => Ok(()),
            Some(ended) => Err(format!(
                "its key no longer counted from record entry {ended} on"
            )),
            None => Err("its key is no longer active".to_owned()),
        };
        match self.register.verify_statement(actor, statement, None) {
            Ok(_) if self.register.is_active_key(&identity, key) => Ok(()),
            Ok(_) => before(self.ended_at(&identity, key)),
            Err(ProofError::KeyNoLongerCounts {
                state: KeyState::Revoked { .. },
                ..
            }) if statement.kind() != StatementKind::Approval => {
                before(self.ended_at(&identity, key))
            }
            Err(error) => Err(error.to_string()),
        }
    }
}

/// What the gate on a hub reads beside the move.
pub(super) struct HubState<'a> {
    pub(super) tip: CommitId,
    pub(super) record: &'a Record,
    pub(super) held: &'a [HeldStatement],
    pub(super) register: &'a Register,
    pub(super) policies: &'a [Policy],
    pub(super) now: i64,
}

impl Repository {
    /// The gate on a hub for a move of `main` (see the module). The inner error refuses the
    /// reference; the outer one is a failure of the hub.
    ///
    /// # Errors
    ///
    /// [`AcceptError::Policy`] if the policy of `T` cannot be read from the store, and
    /// [`AcceptError::Verdict`] if the store fails while the verdict is computed.
    pub(super) fn gate_on_hub(
        &self,
        moved: &Move,
        state: &HubState<'_>,
    ) -> Result<Result<(), RefusalReason>, AcceptError> {
        let refuse = |verdict: GateRefusal| Ok(Err(RefusalReason::Verdict(verdict)));
        let failed = |error: GateError| AcceptError::Verdict(Box::new(error));
        let tip = state.tip;
        // A deletion of `main` is no move from `T` to a merge.
        let Some(merged) = moved.new_commit() else {
            return Ok(Err(RefusalReason::Gate));
        };
        let merge_tree = match self.objects().get_any_commit(&merged) {
            Ok(Some(AnyCommit::Commit(commit))) if commit.parents() == [tip] => commit.tree(),
            Ok(Some(AnyCommit::Commit(_))) => return refuse(GateRefusal::Parents),
            // A commit that records an open conflict is no clean merge; `main` never holds one.
            Ok(Some(AnyCommit::Commit2(_))) => return refuse(GateRefusal::NotTheMerge),
            // The connectivity check found it; a purge may have taken it since (ADR-014).
            Ok(None) => {
                let reason = Reason::from_message(&"the merge commit is not in the store");
                return Ok(Err(RefusalReason::Incomplete(reason)));
            }
            Err(error) => return Err(failed(GateError::Read(error))),
        };
        let Some(opening) = moved.evidence() else {
            return refuse(GateRefusal::NoPull);
        };
        let actors = Actors::new(state.register, self.objects());
        let (placed, merges, order) = placed_in_order(state.held, &state.record.entries, &actors);
        let evidence = evidence::fold(&placed, &merges);
        let Some(pull) = evidence.pulls.iter().find(|pull| pull.id() == opening) else {
            return refuse(GateRefusal::NoPull);
        };
        if !pull.is_open() {
            return refuse(GateRefusal::NotOpen);
        }
        let heads: BTreeMap<_, _> = self.refs().branches()?.into_iter().collect();
        let writers: Vec<Actor> = match self.writers(*pull.head(), tip, state.record) {
            Ok(writers) => writers
                .iter()
                .map(|identity| actors.of(identity, tip))
                .collect(),
            Err(super::StatementError::History(error)) => {
                return Err(AcceptError::History(error));
            }
            Err(super::StatementError::Hub(error)) => return Err(*error),
            Err(other) => return Err(AcceptError::Statements(Box::new(other))),
        };
        // An actor whose register of its time cannot be read has no sponsor to judge the
        // independence of a check by; the hub fails instead of judging with it.
        if let Some(error) = actors.unreadable.take() {
            return Err(AcceptError::Register(Box::new(error)));
        }
        let proofs = HubProofs {
            register: state.register,
            store: self.objects(),
            record: state.record,
            order: &order,
            registers: RefCell::default(),
            unreadable: RefCell::default(),
        };
        let case = Case {
            tip,
            pull,
            evidence: &evidence,
            writers: &writers,
            heads: &heads,
            register: Some(state.register),
            proofs: &proofs,
            rules: Rules::Policies(state.policies),
            now: state.now,
        };
        let judged = self.judge_main(&case).map_err(failed)?;
        if let Some(error) = proofs.unreadable.take() {
            return Err(AcceptError::Register(Box::new(error)));
        }
        let refusal = match judged {
            Ok(Verdict::Merge(merge)) if merge.tree == merge_tree => {
                return self.usable(&merged);
            }
            Ok(Verdict::Merge(_)) => return refuse(GateRefusal::NotTheMerge),
            // The head is on `main` already: an `M` that changes nothing is no change, and one that
            // changes files is not the merge.
            Ok(Verdict::Landed) => {
                let tip_tree = self
                    .objects()
                    .get_commit(&tip)
                    .map_err(|error| failed(GateError::Read(error)))?
                    .map(|commit| commit.tree());
                if tip_tree == Some(merge_tree) {
                    return refuse(GateRefusal::Level(Reason::from_message(
                        &LevelError::NoChange,
                    )));
                }
                return refuse(GateRefusal::NotTheMerge);
            }
            Err(refusal) => refusal,
        };
        let verdict = match refusal {
            Refusal::BranchMoved { head } => GateRefusal::BranchMoved { head },
            Refusal::Conflict => GateRefusal::Conflict,
            Refusal::Policy(
                error @ (PolicyError::Missing | PolicyError::NotAFile | PolicyError::Invalid(_)),
            ) => GateRefusal::Policy(Reason::from_message(&error)),
            Refusal::Policy(error) => {
                return Err(AcceptError::Policy {
                    damaged: self.policy_is_damaged(&tip),
                    source: Box::new(error),
                })
            }
            Refusal::Level(error @ (LevelError::Path(_) | LevelError::NoChange)) => {
                GateRefusal::Level(Reason::from_message(&error))
            }
            Refusal::Level(error) => return Err(failed(GateError::Level(error))),
            Refusal::NotChecked { level, source } => GateRefusal::NotChecked { level, source },
            Refusal::NotApproved {
                level,
                source,
                agents,
                agents_needed,
                humans,
                humans_needed,
                dropped: _,
            } => GateRefusal::NotApproved {
                level,
                source,
                agents,
                agents_needed,
                humans,
                humans_needed,
            },
            Refusal::Paused(Pause::RedTip { red }) => GateRefusal::RedTip { red },
            Refusal::Paused(Pause::Unverified { count, max }) => GateRefusal::Unverified {
                count: count as u64,
                max,
            },
            Refusal::Paused(Pause::Reserved { holder, until }) => {
                let pull = evidence
                    .pulls
                    .iter()
                    .find(|pull| pull.number() == holder)
                    .expect("the holder of a reservation is a pull request of the fold")
                    .id();
                GateRefusal::Reserved { pull, until }
            }
        };
        refuse(verdict)
    }

    /// Whether the register and the policy of the merge `M` can be used. Every later update
    /// reads the register of the tip of `main`, and every later move of `main` its policy, so a
    /// merge that the gate allows but that removes or breaks one of them would end the use of
    /// the repository on the hub: nothing could move `main` to repair it.
    fn usable(&self, merged: &CommitId) -> Result<Result<(), RefusalReason>, AcceptError> {
        let refuse = |error: &dyn std::fmt::Display| {
            Ok(Err(RefusalReason::Verdict(GateRefusal::Unusable(
                Reason::from_message(error),
            ))))
        };
        match Register::at_commit(self.objects(), merged) {
            Ok(Some(_)) => {}
            Ok(None) => return refuse(&"the merge holds no register"),
            Err(error @ (ModeError::Parse(_) | ModeError::NotRegisterFile)) => {
                return refuse(&error);
            }
            Err(error) => return Err(AcceptError::Register(Box::new(error))),
        }
        match self.policy_at(merged, true) {
            Ok(_) => Ok(Ok(())),
            Err(
                error @ (PolicyError::Missing | PolicyError::NotAFile | PolicyError::Invalid(_)),
            ) => refuse(&error),
            Err(error) => Err(AcceptError::Policy {
                damaged: self.policy_is_damaged(merged),
                source: Box::new(error),
            }),
        }
    }
}

#[cfg(test)]
mod tests;
