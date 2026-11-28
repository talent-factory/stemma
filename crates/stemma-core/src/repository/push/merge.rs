//! The merge of a pull request in a repository whose `main` follows a remote (ADR-019, ADR-025,
//! STEMMA-309): `main` follows the receipt of the hub.
//!
//! The clone builds `M` and the hub judges it again; the hub never merges (ADR-015). One attempt
//! runs these steps:
//!
//! 1. A fetch brings the remote references, and with them the tip `T` of `main` on the hub.
//! 2. The local verdict about `main` judges the pull request on `T`
//!    ([`judge_main`](Repository::judge_main)), with the policy of `T` and the statements of the
//!    clone, as the local gate does. A refusal ends the call: nothing was sent.
//! 3. `M` is written with the single parent `T`, and nothing moves.
//! 4. The ref update moves `main` on the hub from `T` to `M`, with the hash of the opening as its
//!    evidence, through the steps of a push: the upload, the signature, the receipt, and one
//!    upload more after a refusal for want of objects.
//! 5. Only after the receipt, under the lock of the references, one entry `push` moves the local
//!    `main` and the remote reference of `main` to `M`, and then `pr-merge` is written. The local
//!    `main` moves only if it still has the value that it had when the attempt began, and the
//!    remote reference only if it still has the state that the update was signed against.
//!
//! Before step 2 the local `main` must be `T` or an ancestor of it: a local `main` that holds
//! commits that the hub does not is refused ([`RemoteMergeError::Diverged`]), as the move to `M`
//! would drop them.
//!
//! A stale update (another writer moved `main` on the hub) starts again at step 1, at most
//! [`MAX_ATTEMPTS`] times, and the update is signed again each time: the loop of ADR-019 across
//! the network. A refusal of the hub leaves nothing to take back, as nothing moved; `M` stays
//! unnamed in the store, as the objects of a failed attempt of the local gate do. So the local
//! `main` never names an `M` that the hub did not accept.
//!
//! A pull request whose change the hub holds already (an earlier call got the receipt and
//! stopped before its entries, or lost the answer) is recorded as merged without an update
//! ([`RemoteMerge::repaired`]): its fetch brought the remote reference along, and the local
//! `main` moves to the tip of the hub with an entry `gate-merge`, as the local gate moves it.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::io;

use super::{authorize_push, remote_ref_name, AnchorFailure, PushError, Uploaded, PIECE_BYTES};
use crate::change::ChangeId;
use crate::merge_base::merge_bases;
use crate::object::{Commit, CommitId};
use crate::oplog::{Change, Moved, OpLogError};
use crate::oplog::{Entry, OperationName, PR_MERGE};
use crate::protocol::anchor::HeldAnchor;
use crate::protocol::record::{HubAddress, Move, RecordEntry};
use crate::protocol::QualifiedName;
use crate::pull::TARGET;
use crate::refs::{BranchName, RefKind};
use crate::refs::{Logged, RefError};
use crate::register::{Mode, Register};
use crate::remote::RemoteName;
use crate::repository::fetch::{FetchError, Fetched};
use crate::repository::gate::MAX_ATTEMPTS;
use crate::repository::verdict::{Case, LogProofs, Rules, Verdict};
use crate::repository::{now, GateError, PullError, Repository, UpdateError};
use crate::signature::Signer;
use crate::store::ObjectStore;
use crate::transport::Transport;
use crate::{Actor, Allowed, Authorizer};

/// A pull request that the hub merged, and whose merge the clone recorded (see
/// [`merge_gate_remote`](Repository::merge_gate_remote)).
#[derive(Debug)]
#[non_exhaustive]
pub struct RemoteMerge {
    /// The commit `M` that `main` names on the hub after the update: one parent, the tip `T`.
    /// For a repair it is the tip of the hub, which holds the change already.
    pub commit: CommitId,
    /// The tip `T` of `main` on the hub before the update, the only parent of `commit`; for a
    /// repair the same commit as `commit`.
    pub tip: CommitId,
    /// How many attempts the call needed: 1 if no other writer moved `main` on the hub.
    pub attempts: u32,
    /// The entry that moved the local `main` or the remote reference: `push` after a receipt,
    /// `gate-merge` for a repair. `None` if nothing had to move. Whether the local `main` moved
    /// is [`main_followed`](Self::main_followed).
    pub pushed: Option<Box<Entry>>,
    /// Whether the local `main` names `commit`: it followed, or it named it already. `false` if
    /// another writer of the clone moved it in between, which then keeps its value.
    pub main_followed: bool,
    /// The entry `pr-merge`.
    pub merged: Box<Entry>,
    /// The receipt of the hub, or `None` for a repair.
    pub receipt: Option<Box<RecordEntry>>,
    /// What the uploads carried, over every attempt.
    pub uploaded: Uploaded,
    /// The confirmation of the anchor that was sent after the entries, or why it is missing;
    /// `None` for a repair, which sends none.
    pub anchor: Option<Result<HeldAnchor, AnchorFailure>>,
    /// Whether the call only recorded the merge of a change that the hub held already.
    pub repaired: bool,
}

impl Repository {
    /// Lands the open pull request `number` on the `main` of the remote `remote` and then on the
    /// local `main` (ADR-019, ADR-025): fetches, judges the change locally on the tip `T` of
    /// `main` on the hub by the policy of `T` and `mode`, as
    /// [`merge_gate_by_policy`](Self::merge_gate_by_policy) does, builds `M` without a move,
    /// sends the ref update with the opening as its evidence, and only after the receipt moves
    /// the local `main` and the remote reference with one entry `push` and writes `pr-merge`.
    /// The anchor of the log follows, as after a push. On `Stale` the call fetches and repeats,
    /// signed anew, at most [`MAX_ATTEMPTS`] times; a change that the hub holds already is
    /// recorded as merged without an update, and the local `main` follows the hub. Without a
    /// remote the gate stays [`merge_gate_by_policy`](Self::merge_gate_by_policy).
    ///
    /// The authorizer is asked for `pr-merge` with the target `pull <number>` and for `push`
    /// with the target `remote <name>`, whose proof opens the lock for the moves, and the fetch
    /// of every attempt asks for `fetch`. The verdict reads the statements of the clone; the
    /// hub counts the statements that it holds, so the opening and the evidence must have been
    /// submitted to it. The local `main`
    /// moves without a working copy: the caller brings the worktree along with
    /// [`follow_main`](Self::follow_main).
    ///
    /// # Errors
    ///
    /// [`RemoteMergeError::Gate`] for a refusal of the authorizer, a refusal of the local
    /// verdict, and a failure of the clone before anything was sent;
    /// [`RemoteMergeError::Diverged`] if the local `main` holds commits that `main` on the hub
    /// does not; [`RemoteMergeError::Fetch`] for a fetch that failed; [`RemoteMergeError::Push`]
    /// for a remote without a pinned key and for the update, including a refusal of the hub,
    /// after which nothing is to take back; [`RemoteMergeError::Unknown`] if the ref update got
    /// no answer or its receipt could not be read or checked;
    /// [`RemoteMergeError::Contended`] if `main` on the hub moved in every attempt; and
    /// [`RemoteMergeError::Unrecorded`] and [`RemoteMergeError::PullNotRecorded`] if the
    /// receipt came and the entries could not be written. On a bare repository the call is
    /// refused before anything else, with the error [`Bare`](crate::refs::RefError::Bare) of
    /// the references.
    #[allow(clippy::too_many_arguments)]
    pub fn merge_gate_remote(
        &self,
        number: u64,
        mode: &Mode,
        remote: &RemoteName,
        hub: &HubAddress,
        transport: &mut dyn Transport,
        signer: &dyn Signer,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<RemoteMerge, RemoteMergeError> {
        self.require_oplog("merge through the gate")
            .map_err(|error| RemoteMergeError::Gate(GateError::Refs(error)))?;
        let register = match mode {
            Mode::Open => None,
            Mode::Enforcing(register) => Some(register),
        };
        let pr_merge = OperationName::try_new(PR_MERGE).expect("an operation name");
        // The record of the merge is asked for as well; its proof is not needed, as `pr-merge`
        // is written under the lock that the proof of the push opens (as in the local gate).
        drop(
            self.authorize(actor, authorizer, &pr_merge, &format!("pull {number}"))
                .map_err(|error| RemoteMergeError::Gate(denied(error)))?,
        );
        let allowed = authorize_push(remote, actor, authorizer).map_err(|error| match error {
            PushError::Update(error) => RemoteMergeError::Gate(denied(error)),
            other => RemoteMergeError::Push(Box::new(other)),
        })?;
        let mut uploaded = Uploaded::default();
        for attempt in 1..=MAX_ATTEMPTS {
            let attempted = self.remote_attempt(
                number,
                register,
                remote,
                hub,
                transport,
                signer,
                actor,
                authorizer,
                &allowed,
                &mut uploaded,
            )?;
            if let Some(mut merged) = attempted {
                merged.attempts = attempt;
                merged.uploaded = uploaded;
                if !merged.repaired {
                    merged.anchor = Some(self.send_anchor(remote, hub, transport, signer));
                }
                return Ok(merged);
            }
        }
        Err(RemoteMergeError::Contended {
            number,
            attempts: MAX_ATTEMPTS,
        })
    }

    /// One attempt: `None` if the hub answered `Stale` and the loop starts again.
    #[allow(clippy::too_many_arguments)]
    fn remote_attempt(
        &self,
        number: u64,
        register: Option<&Register>,
        remote: &RemoteName,
        hub: &HubAddress,
        transport: &mut dyn Transport,
        signer: &dyn Signer,
        actor: &Actor,
        authorizer: &dyn Authorizer,
        allowed: &Allowed,
        uploaded: &mut Uploaded,
    ) -> Result<Option<RemoteMerge>, RemoteMergeError> {
        let gate = RemoteMergeError::Gate;
        // Step 1: the state of the hub.
        let _fetched: Fetched = self
            .fetch(remote, transport, actor, authorizer)
            .map_err(|error| RemoteMergeError::Fetch(Box::new(error)))?;
        let config = self
            .pinned_remote(remote)
            .map_err(|error| RemoteMergeError::Push(Box::new(error)))?;
        let main = BranchName::try_new(TARGET).expect("`main` is a branch name");
        let main_name = QualifiedName::new(RefKind::Branch, main.clone());
        let at = self
            .refs()
            .remote_ref(&remote_ref_name(remote, &main_name))
            .map_err(|error| gate(GateError::Refs(error)))?
            .ok_or(gate(GateError::NoMain))?;
        let tip = at.commit;
        // The local `main` follows the hub: it may be behind `T`, and never beside it.
        let local = self
            .refs()
            .branch(&main)
            .map_err(|error| gate(GateError::Refs(error)))?;
        if let Some(local) = local.filter(|local| *local != tip) {
            let bases = merge_bases(self.objects(), local, tip)
                .map_err(|error| gate(GateError::History(error)))?;
            if bases != [local] {
                return Err(RemoteMergeError::Diverged { local, tip });
            }
        }

        // Step 2: the local verdict on `T`, as the local gate reaches it.
        let evidence = self
            .evidence(register)
            .map_err(|error| gate(GateError::Pull(PullError::from(error))))?;
        let pull = super::super::pull::open_pull_of(&evidence.pulls, number)
            .map_err(|error| gate(GateError::Pull(error)))?;
        let head = self
            .branch_head(pull.branch())
            .map_err(|error| gate(GateError::Pull(error)))?;
        if head != *pull.head() {
            return Err(gate(GateError::BranchMoved { number }));
        }
        let heads: BTreeMap<BranchName, CommitId> = self
            .refs()
            .branches()
            .map_err(|error| gate(GateError::Refs(error)))?
            .into_iter()
            .collect();
        let writers = match register {
            Some(_) => self
                .pull_authors(&head)
                .map_err(|error| gate(GateError::Pull(error)))?,
            None => Vec::new(),
        };
        let proofs = LogProofs(register);
        let case = Case {
            tip,
            pull,
            evidence: &evidence,
            writers: &writers,
            heads: &heads,
            register,
            proofs: &proofs,
            rules: Rules::Policies(&[]),
            now: now(),
        };
        let tree = match self.judge_main(&case).map_err(gate)? {
            Ok(Verdict::Merge(merge)) => merge.tree,
            Ok(Verdict::Landed) => {
                return self
                    .record_remote_landed(number, head, tip, local, register, actor, allowed)
                    .map(Some)
            }
            Err(refusal) => return Err(gate(GateError::refused(refusal, number, tip, pull))),
        };
        let opening = pull.id();

        // Step 3: `M`, without a move.
        let message = format!("Merge pull request {number} ({})", pull.branch());
        let change = ChangeId::generate().map_err(|error| gate(GateError::ChangeId(error)))?;
        let commit = Commit::try_new(tree, vec![tip], change, actor.clone(), now(), message)
            .expect("a commit with one parent has no duplicate parent");
        let merged = self
            .objects()
            .put_commit(&commit)
            .map_err(|error| gate(GateError::Store(error)))?;

        // Step 4: the ref update, signed again in every attempt.
        let moved = Move::try_new(
            main_name,
            Some(at.entry),
            Some(tip),
            Some(merged),
            Some(opening),
        )
        .expect("a new commit `M` differs from its parent `T`");
        let receipt = match self.send_update(
            remote,
            &config,
            hub,
            vec![moved],
            transport,
            signer,
            PIECE_BYTES,
            uploaded,
        ) {
            Ok(receipt) => receipt,
            Err(PushError::Stale(_)) => return Ok(None),
            Err(error) if error.outcome_unknown() => {
                return Err(RemoteMergeError::Unknown(Box::new(error)))
            }
            Err(error) => return Err(RemoteMergeError::Push(Box::new(error))),
        };

        // Step 5: `main` follows the receipt, then `pr-merge`.
        let unrecorded = |source: PushError| RemoteMergeError::Unrecorded {
            receipt: Box::new(receipt.clone()),
            commit: merged,
            source: Box::new(source),
        };
        let lock = self
            .refs_store()
            .lock_writer(allowed)
            .map_err(|error| unrecorded(PushError::Refs(error)))?;
        let (pushed, main_followed) = self
            .record_push(
                &lock,
                remote,
                &config,
                &receipt,
                Some((main, local, merged)),
                actor,
            )
            .map_err(unrecorded)?;
        let entry = self
            .record_pull_merge_locked(&lock, number, actor, head, register)
            .map_err(|error| RemoteMergeError::PullNotRecorded {
                pushed: pushed.clone().map(Box::new),
                main_followed,
                receipt: Box::new(receipt.clone()),
                commit: merged,
                source: Box::new(error),
            })?;
        drop(lock);
        Ok(Some(RemoteMerge {
            commit: merged,
            tip,
            attempts: 1,
            pushed: pushed.map(Box::new),
            main_followed,
            merged: Box::new(entry),
            receipt: Some(Box::new(receipt)),
            uploaded: Uploaded::default(),
            anchor: None,
            repaired: false,
        }))
    }

    /// The repair: the hub holds the change already. Under the lock, after the pull request is
    /// found open and its branch unchanged once more, the local `main` moves from `local`, its
    /// value when the attempt began, to the tip of the hub with an entry `gate-merge`, and then
    /// `pr-merge` is written. A local `main` that another writer moved in between keeps its
    /// value.
    #[allow(clippy::too_many_arguments)]
    fn record_remote_landed(
        &self,
        number: u64,
        head: CommitId,
        tip: CommitId,
        local: Option<CommitId>,
        register: Option<&Register>,
        actor: &Actor,
        allowed: &Allowed,
    ) -> Result<RemoteMerge, RemoteMergeError> {
        let gate = RemoteMergeError::Gate;
        let lock = self
            .refs_store()
            .lock_writer(allowed)
            .map_err(|error| gate(GateError::Refs(error)))?;
        let pulls = self
            .pull_requests(register)
            .map_err(|error| gate(GateError::Pull(error)))?;
        let pull = super::super::pull::open_pull_of(&pulls, number)
            .map_err(|error| gate(GateError::Pull(error)))?;
        if self
            .branch_head(pull.branch())
            .map_err(|error| gate(GateError::Pull(error)))?
            != head
        {
            return Err(gate(GateError::BranchMoved { number }));
        }
        let main = BranchName::try_new(TARGET).expect("`main` is a branch name");
        let mut moved = None;
        let mut main_followed = true;
        if local != Some(tip) {
            let operation =
                OperationName::try_new(super::super::gate::OPERATION).expect("an operation name");
            let logged = self.refs_store().update_branch_after(
                &lock,
                &main,
                local,
                tip,
                self.objects(),
                || {
                    let change = Change::try_new(
                        now(),
                        actor.clone(),
                        operation.clone(),
                        None,
                        None,
                        vec![(main.clone(), Moved::from_to(local, Some(tip)))],
                    )
                    .map_err(OpLogError::Entry)?;
                    self.append(&lock, &change)
                },
            );
            moved = match logged {
                // Another writer of the clone moved `main`: it keeps its value, which may be the
                // tip of the hub already.
                Err(RefError::Stale { found, .. }) => {
                    main_followed = found == Some(tip);
                    None
                }
                Err(error) => return Err(gate(GateError::Refs(error))),
                Ok(Logged::Moved(entry)) => Some(Box::new(entry)),
                Ok(Logged::Unchanged) => None,
                Ok(other) => {
                    return Err(gate(GateError::Update(
                        other.into_result().expect_err("a failed move is an error"),
                    )))
                }
            };
        }
        let entry = self
            .record_pull_merge_locked(&lock, number, actor, head, register)
            .map_err(|source| match &moved {
                // `main` moved, as in the local gate: the entry of the move stands.
                Some(entry) => gate(GateError::PullNotRecorded {
                    moved: entry.clone(),
                    commit: tip,
                    source: Box::new(source),
                }),
                None => gate(GateError::Pull(source)),
            })?;
        Ok(RemoteMerge {
            commit: tip,
            tip,
            attempts: 1,
            pushed: moved,
            main_followed,
            merged: Box::new(entry),
            receipt: None,
            uploaded: Uploaded::default(),
            anchor: None,
            repaired: true,
        })
    }
}

/// The refusal of the authorizer as an error of the gate.
fn denied(error: UpdateError) -> GateError {
    match error {
        UpdateError::Denied { operation, target } => GateError::Denied { operation, target },
        other => GateError::Update(other),
    }
}

/// Why [`Repository::merge_gate_remote`] failed. Only [`Unrecorded`](Self::Unrecorded) and
/// [`PullNotRecorded`](Self::PullNotRecorded) come after the receipt. After
/// [`Unknown`](Self::Unknown) the hub may have moved; every other variant moved nothing on the
/// hub. No variant moved anything in the clone but objects and the remote references that a
/// fetch moved, except the ones after the receipt and a [`Gate`](Self::Gate) error of a repair
/// whose entry `gate-merge` was written ([`entry`](Self::entry) names it).
#[derive(Debug)]
#[non_exhaustive]
pub enum RemoteMergeError {
    /// The authorizer refused, the local verdict refused the change, or the clone failed before
    /// anything was sent (see [`GateError`]). In a repair, where nothing is sent, it is also a
    /// failure under the lock: the move of the local `main` with its entry `gate-merge` failed
    /// ([`GateError::Update`]), or `pr-merge` could not be written after it
    /// ([`GateError::PullNotRecorded`]); the next call records the merge.
    Gate(GateError),
    /// The fetch of an attempt failed.
    Fetch(Box<FetchError>),
    /// The remote has no pinned key or cannot be read, the upload or the signature failed, or
    /// the hub refused the update (see [`PushError`]). Nothing moved, so nothing is to take
    /// back.
    Push(Box<PushError>),
    /// The local `main` holds commits that `main` on the hub does not: it is neither the tip of
    /// the hub nor an ancestor of it. Nothing was sent.
    Diverged {
        /// The local `main`.
        local: CommitId,
        /// The tip of `main` on the hub.
        tip: CommitId,
    },
    /// The ref update got no answer, or the answer of success could not be read or checked
    /// ([`PushError::outcome_unknown`]), so whether the hub accepted the merge is not known.
    /// Nothing moved in the clone; the next call fetches, and finds the change on the hub if it
    /// was accepted.
    Unknown(Box<PushError>),
    /// `main` on the hub moved in each of the attempts.
    Contended {
        /// The number of the pull request.
        number: u64,
        /// The attempts made.
        attempts: u32,
    },
    /// The hub accepted the merge, and recording it in the clone failed: the lock, the
    /// configuration of the remote, the entry `push`, a move, or the flush of a move failed (see
    /// [`PushError::Unrecorded`]; [`entry`](RemoteMergeError::entry) names the entry `push` if
    /// it was written). `pr-merge` is not written. The next call fetches, finds the change on
    /// the hub, moves the local `main` to it if it is not there, and records the merge.
    Unrecorded {
        /// The receipt of the hub.
        receipt: Box<RecordEntry>,
        /// The merge `M` that `main` names on the hub.
        commit: CommitId,
        /// What failed.
        source: Box<PushError>,
    },
    /// The hub accepted the merge and the entry `push` was written if anything had to move, but
    /// `pr-merge` could not be written. The next call finds the change on the hub and records
    /// the merge.
    PullNotRecorded {
        /// The entry `push`, if it was written.
        pushed: Option<Box<Entry>>,
        /// Whether the local `main` followed (see [`RemoteMerge::main_followed`]).
        main_followed: bool,
        /// The receipt of the hub.
        receipt: Box<RecordEntry>,
        /// The merge `M` that `main` names on the hub.
        commit: CommitId,
        /// What failed.
        source: Box<PullError>,
    },
}

impl RemoteMergeError {
    /// The entry that moved or was to move the local `main` or a remote reference, if one was
    /// written: `push` after a receipt, `gate-merge` in a repair.
    pub fn entry(&self) -> Option<&Entry> {
        match self {
            Self::Unrecorded { source, .. } => source.entry(),
            Self::PullNotRecorded { pushed, .. } => pushed.as_deref(),
            Self::Gate(GateError::PullNotRecorded { moved, .. }) => Some(moved),
            Self::Gate(GateError::Update(error)) => error.entry(),
            _ => None,
        }
    }

    /// The error of the file system under the fetch or the update, if that is what failed (see
    /// the crate documentation). The errors of the gate and of the pull request carry their
    /// own sources.
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Fetch(error) => error.io_error(),
            Self::Push(error) => error.io_error(),
            Self::Unrecorded { source, .. } => source.io_error(),
            Self::Unknown(error) => error.io_error(),
            Self::Gate(_)
            | Self::Diverged { .. }
            | Self::PullNotRecorded { .. }
            | Self::Contended { .. } => None,
        }
    }
}

impl fmt::Display for RemoteMergeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Gate(error) => error.fmt(f),
            Self::Fetch(error) => write!(f, "the fetch before the merge failed: {error}"),
            Self::Push(error) => write!(f, "the merge did not land: {error}"),
            Self::Diverged { local, tip } => write!(
                f,
                "the local `main` at {local} holds commits that `main` on the hub at {tip} does \
                 not; nothing was sent"
            ),
            Self::Unknown(error) => {
                error.fmt_cause(f)?;
                f.write_str(
                    "; whether the hub accepted the merge is not known, and the next merge of the \
                     pull request fetches and records it",
                )
            }
            Self::Contended { number, attempts } => write!(
                f,
                "pull request {number} did not land after {attempts} attempts: `main` on the hub \
                 moved each time"
            ),
            Self::Unrecorded {
                receipt, source, ..
            } => write!(
                f,
                "the hub accepted the merge as record entry {}, and recording it in the clone \
                 failed: {source}; the next merge of the pull request records it",
                receipt.number()
            ),
            Self::PullNotRecorded {
                receipt, source, ..
            } => write!(
                f,
                "the hub accepted the merge as record entry {}, but its merge was not recorded: \
                 {source}; the next merge of the pull request records it",
                receipt.number()
            ),
        }
    }
}

impl Error for RemoteMergeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Gate(error) => error.source(),
            Self::Fetch(error) => error.source(),
            Self::Push(error) => error.source(),
            Self::Unknown(error) => error.source(),
            Self::Unrecorded { source, .. } => source.source(),
            Self::PullNotRecorded { source, .. } => source.source(),
            Self::Diverged { .. } | Self::Contended { .. } => None,
        }
    }
}
