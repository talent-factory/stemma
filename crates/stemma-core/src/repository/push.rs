//! The push of a clone (ADR-008, ADR-023, ADR-025, ADR-026, STEMMA-309): the client side of a
//! ref update, through a [`Transport`].
//!
//! A push is an upload, then a small ref update (ADR-023):
//!
//! 1. The authorizer is asked for `push` with the target `remote <name>`, and the configuration
//!    of the remote must hold a pinned key of the hub (ADR-026).
//! 2. Each reference that the caller names gets its move: `previous` and `old` come from the
//!    remote reference of the same name, the last state that a fetch or a push saw, and `new`
//!    and the evidence from the caller. A reference whose remote reference has its new value
//!    already moves nothing and is left out; a push that moves nothing sends nothing.
//! 3. The upload: everything that the new commits reach and the commits of the remote references
//!    of the remote do not, in the order of the format, cut into pieces of about
//!    [`PIECE_BYTES`] at commit boundaries ([`Repository::upload_of`]). A piece whose request
//!    fails on the carrier is sent again, in at most [`PIECE_ATTEMPTS`] attempts in all, so an
//!    interrupted upload goes on at the first piece that was not confirmed; within one pass a
//!    confirmed piece is not sent again. Any other failure of the request ends the push at
//!    once. A push without new objects sends no upload.
//! 4. The payload is signed by the actor and sent. The receipt must hold the update as it was
//!    sent and be signed by the pinned key of the hub ([`RecordEntry::check_receipt`]). A
//!    refusal because the objects of a new commit do not hold together
//!    ([`Refused::wants_upload`], status 422) uploads once more, this time everything that the
//!    new commits reach, and sends the same update again, once. A stale update
//!    ([`ErrorMessage::Stale`]) moves nothing and is reported with the state of each stale
//!    reference on the hub; the push does not repeat on its own, as only
//!    a fetch moves a remote reference to a state that this side has not seen (ADR-008).
//! 5. Under the lock of the references, after the receipt, one entry `push` is written with the
//!    remote references that move to the state of the receipt (the commit and the hash of the
//!    receipt as their record entry) and the number and the hash of the receipt, and then they
//!    move. The configuration of the remote is read once more under the lock, as a fetch does.
//! 6. The anchor of the new tip of the log is sent (ADR-026) and its confirmation kept. A failure
//!    there leaves the push as it is: the result names it, and the command warns.
//!
//! Every failure before the receipt leaves the clone as it was. A refusal of the hub leaves the
//! hub with unnamed objects at most, which are never served. An update that got no answer, or an
//! answer of success whose receipt cannot be read or checked, may have moved the hub
//! ([`PushError::outcome_unknown`]); a fetch says it. A failure after the receipt carries it
//! ([`PushError::Unrecorded`]): the hub moved, and a fetch brings the remote references along.
//!
//! The merge of a pull request in a repository whose `main` follows a remote uses steps 3 to 5
//! for its move of `main` ([`merge_gate_remote`](Repository::merge_gate_remote)).
//!
//! The level of this code is 3 by ADR-004 (the merge gate and the signatures); the command asks
//! the authorizer for `push` at run time.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::io;

use super::anchor::LogAnchorError;
use super::fetch::{self, FetchError};
use super::{now, Repository, UpdateError};
use crate::object::CommitId;
use crate::oplog::{
    largest_entry_len, Change, Entry, Moved, OpLogError, OperationName, Receipt, MAX_ENTRY_LEN,
    PUSH,
};
use crate::protocol::anchor::HeldAnchor;
use crate::protocol::record::{
    HubAddress, Move, ReceiptError, RecordEntry, RecordProblem, RefUpdate, RefUpdatePayload,
    SealError, Stale, StatementHash,
};
use crate::protocol::{ErrorMessage, ParseError, QualifiedName, RecordHash};
use crate::refs::{BranchName, RefError, RestoreTargets, WriteLock};
use crate::remote::{HubKeyError, RemoteConfig, RemoteError, RemoteName, RemoteRef, RemoteRefName};
use crate::serve::{AnswerError, Sent};
use crate::signature::{PublicKey, Signer};
use crate::transport::{Answer, Transport, TransportError};
use crate::{Actor, Allowed, Authorizer};

mod merge;

pub use merge::{RemoteMerge, RemoteMergeError};

/// The bytes of the bodies after which an upload begins a new piece: 64 MiB. A piece ends at a
/// commit boundary, so it may be larger by the objects of one commit.
pub const PIECE_BYTES: u64 = 64 << 20;

/// How often a push sends one piece of its upload before it gives up, when the carrier fails.
pub const PIECE_ATTEMPTS: u32 = 3;

/// The longest receipt that a push reads: 1 MiB. A receipt holds two lines per reference of
/// the update.
const MAX_RECEIPT: u64 = 1 << 20;

/// The longest confirmation of an anchor that a push reads: 64 KiB.
const MAX_HELD_ANCHOR: u64 = 64 << 10;

/// A reference that a push moves on the hub: its qualified name (`heads/<branch>` or
/// `tags/<tag>`), its new commit or `None` to delete it, and the statement that the move rests
/// on, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushTarget {
    /// The reference on the hub.
    pub name: QualifiedName,
    /// The commit that it names after the push, or `None` to delete it.
    pub new: Option<CommitId>,
    /// The hash of the statement that the move rests on, or `None`.
    pub evidence: Option<StatementHash>,
}

impl PushTarget {
    /// The move of `name` to `new`, without evidence.
    pub fn new(name: QualifiedName, new: Option<CommitId>) -> Self {
        Self {
            name,
            new,
            evidence: None,
        }
    }
}

/// What the upload of a push carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct Uploaded {
    /// The pieces that the other side confirmed, over every pass.
    pub pieces: u32,
    /// The requests that failed on the carrier and were sent again.
    pub repeated: u32,
    /// The passes over the upload that sent at least one request: one per ref update, and
    /// one more after a refusal for want of objects. A merge with a remote counts them over
    /// every attempt.
    pub passes: u32,
    /// What the confirmed pieces carried, over every pass.
    pub sent: Sent,
}

/// What a push did.
#[derive(Debug)]
#[non_exhaustive]
pub struct Pushed {
    /// The entry `push`, or `None` if every remote reference had the state of the receipt
    /// already, for example because a fetch brought it in between.
    pub entry: Option<Entry>,
    /// The receipt of the hub.
    pub receipt: RecordEntry,
    /// What the upload carried.
    pub uploaded: Uploaded,
    /// The confirmation of the anchor that was sent after the entry, or why it is missing. The
    /// push has happened in either case (ADR-026).
    pub anchor: Result<HeldAnchor, AnchorFailure>,
}

/// A ref update that the hub accepted, before anything moved in the clone.
struct Accepted {
    receipt: RecordEntry,
    uploaded: Uploaded,
}

impl Repository {
    /// Pushes `targets` to the remote `remote` through `transport` (ADR-023, ADR-025): uploads
    /// what the new commits reach and the remote references do not, signs the ref update with
    /// `signer` for the hub `hub`, sends it, checks the receipt against the pinned key, and then
    /// moves the remote references with one entry `push` and sends the anchor of the log
    /// (ADR-023, ADR-025, ADR-026). The upload goes in pieces of [`PIECE_BYTES`] at commit
    /// boundaries, a piece is sent again after a failure of the carrier, and a refusal for want
    /// of objects uploads once more. A remote that asks for a sign-in is signed in on
    /// `transport` by the caller first. A target whose remote reference has its new value
    /// already is left out, and a push of nothing sends nothing and returns `None`; a remote
    /// reference that is behind the hub is found by the hub as stale.
    ///
    /// # Errors
    ///
    /// Before anything is sent: [`PushError::Refs`] with [`RefError::Bare`] in a bare repository,
    /// [`PushError::Update`] with [`UpdateError::Denied`] for a refusal of the authorizer,
    /// [`PushError::Remote`], [`PushError::NoSuchRemote`], [`PushError::HubKey`] for a remote
    /// without a pinned key, [`PushError::Move`] for a target that breaks a rule of a move,
    /// [`PushError::Duplicate`] for a reference that is named twice, and
    /// [`PushError::TooLarge`] for more moves than one entry `push` holds.
    ///
    /// Before the receipt, with nothing moved in the clone: [`PushError::Objects`] if the store
    /// cannot give an object of the upload, [`PushError::Upload`] if a piece failed on the
    /// carrier in [`PIECE_ATTEMPTS`] attempts or its request failed otherwise,
    /// [`PushError::Sign`], [`PushError::Answer`] for an error of the hub that cannot be read,
    /// [`PushError::Refused`] for an error of the hub, and [`PushError::Stale`] for a stale
    /// update.
    ///
    /// With the outcome on the hub not known ([`PushError::outcome_unknown`]), nothing moved in
    /// the clone, and a fetch says whether the hub moved: [`PushError::Transport`] if the ref
    /// update got no answer, [`PushError::ReceiptLost`] if the answer of success could not be
    /// read, [`PushError::Unreadable`] if the reader refused the receipt, and
    /// [`PushError::Receipt`] for a receipt that is not the one of the update or not signed by
    /// the pinned key.
    ///
    /// After the receipt: [`PushError::Unrecorded`] with the receipt, if the configuration of
    /// the remote changed during the push or the entry or a move failed.
    #[allow(clippy::too_many_arguments)]
    pub fn push(
        &self,
        remote: &RemoteName,
        hub: &HubAddress,
        targets: &[PushTarget],
        transport: &mut dyn Transport,
        signer: &dyn Signer,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Option<Pushed>, PushError> {
        self.require_oplog("push").map_err(PushError::Refs)?;
        self.push_with(
            remote,
            hub,
            targets,
            transport,
            signer,
            actor,
            authorizer,
            PIECE_BYTES,
        )
    }

    /// [`push`](Self::push) with the size of a piece of the upload, which the tests make small.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn push_with(
        &self,
        remote: &RemoteName,
        hub: &HubAddress,
        targets: &[PushTarget],
        transport: &mut dyn Transport,
        signer: &dyn Signer,
        actor: &Actor,
        authorizer: &dyn Authorizer,
        piece: u64,
    ) -> Result<Option<Pushed>, PushError> {
        let allowed = authorize_push(remote, actor, authorizer)?;
        let config = self.pinned_remote(remote)?;
        let mut moves = Vec::new();
        let mut names = BTreeSet::new();
        for target in targets {
            if !names.insert(target.name.clone()) {
                return Err(PushError::Duplicate(target.name.clone()));
            }
            let state = self
                .refs()
                .remote_ref(&remote_ref_name(remote, &target.name))
                .map_err(PushError::Refs)?;
            if state.map(|state| state.commit) == target.new {
                continue;
            }
            let moved = Move::try_new(
                target.name.clone(),
                state.map(|state| state.entry),
                state.map(|state| state.commit),
                target.new,
                target.evidence,
            )
            .map_err(|problem| PushError::Move {
                name: target.name.clone(),
                problem,
            })?;
            moves.push(moved);
        }
        if moves.is_empty() {
            return Ok(None);
        }
        self.require_entry_fits(remote, &moves, actor)?;
        let mut uploaded = Uploaded::default();
        let receipt = self.send_update(
            remote,
            &config,
            hub,
            moves,
            transport,
            signer,
            piece,
            &mut uploaded,
        )?;
        let accepted = Accepted { receipt, uploaded };
        let unrecorded = |source: PushError| PushError::Unrecorded {
            receipt: Box::new(accepted.receipt.clone()),
            source: Box::new(source),
        };
        let lock = self
            .refs_store()
            .lock_writer(&allowed)
            .map_err(|error| unrecorded(PushError::Refs(error)))?;
        let (entry, _) = self
            .record_push(&lock, remote, &config, &accepted.receipt, None, actor)
            .map_err(unrecorded)?;
        drop(lock);
        let anchor = self.send_anchor(remote, hub, transport, signer);
        Ok(Some(Pushed {
            entry,
            receipt: accepted.receipt,
            uploaded: accepted.uploaded,
            anchor,
        }))
    }

    /// The configuration of `remote`, which must hold a pinned key of the hub.
    pub(super) fn pinned_remote(&self, remote: &RemoteName) -> Result<RemoteConfig, PushError> {
        let config = self
            .remote(remote)
            .map_err(PushError::Remote)?
            .ok_or_else(|| PushError::NoSuchRemote(remote.clone()))?;
        config.pinned_key().map_err(PushError::HubKey)?;
        Ok(config)
    }

    /// Steps 3 and 4 of a push: uploads what the new commits of `moves` reach, signs and sends
    /// the update, uploads once more after a refusal for want of objects, and checks the
    /// receipt. Nothing moves in the clone. What the uploads carried is added to `uploaded`,
    /// also when the call fails.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn send_update(
        &self,
        remote: &RemoteName,
        config: &RemoteConfig,
        hub: &HubAddress,
        moves: Vec<Move>,
        transport: &mut dyn Transport,
        signer: &dyn Signer,
        piece: u64,
        uploaded: &mut Uploaded,
    ) -> Result<RecordEntry, PushError> {
        let pinned = config.pinned_key().map_err(PushError::HubKey)?.clone();
        let tips: BTreeSet<CommitId> = moves.iter().filter_map(Move::new_commit).collect();
        let had = self.remote_commits(remote)?;
        self.upload(&tips, &had, transport, piece, uploaded)?;
        let key = signer
            .public_key()
            .map_err(|error| PushError::Sign(SealError::Sign(error)))?;
        let update = RefUpdatePayload::try_new(hub.clone(), config.repository, moves, key)
            .expect("the moves are not empty and name every reference once")
            .sign(signer)
            .map_err(PushError::Sign)?;
        let mut again = true;
        loop {
            match send(transport, &update, &pinned) {
                Ok(receipt) => return Ok(receipt),
                Err(PushError::Refused(ErrorMessage::Refused(refused)))
                    if again && refused.wants_upload() =>
                {
                    // The hub misses objects of a new commit (ADR-023): a purge on either side,
                    // or an upload that the hub lost. The second pass names no `have`, so it
                    // also brings what the hub lost below a remote reference; the same update
                    // is sent once more.
                    again = false;
                    self.upload(&tips, &BTreeSet::new(), transport, piece, uploaded)?;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// The commits of the remote references of `remote`: what the hub holds by what this side
    /// has seen.
    fn remote_commits(&self, remote: &RemoteName) -> Result<BTreeSet<CommitId>, PushError> {
        Ok(self
            .refs()
            .remote_refs()
            .map_err(PushError::Refs)?
            .into_iter()
            .filter(|(name, _)| name.remote == *remote)
            .map(|(_, reference)| reference.commit)
            .collect())
    }

    /// One pass over the upload: every piece in its order, each sent again on a failure of the
    /// carrier, in at most [`PIECE_ATTEMPTS`] attempts in all. A failure that no repeat mends
    /// (a request that cannot be read, a request that the transport does not carry, no answer
    /// of the protocol) ends the pass at once.
    fn upload(
        &self,
        tips: &BTreeSet<CommitId>,
        had: &BTreeSet<CommitId>,
        transport: &mut dyn Transport,
        piece: u64,
        uploaded: &mut Uploaded,
    ) -> Result<(), PushError> {
        let mut upload = self.upload_of(tips, had).map_err(boxed_objects)?;
        if upload.remaining() == 0 {
            return Ok(());
        }
        uploaded.passes += 1;
        let before = uploaded.pieces;
        while let Some(next) = upload.next_piece(piece).map_err(boxed_objects)? {
            let mut attempt = 1;
            loop {
                match transport.upload(&mut next.bytes.as_slice()) {
                    Ok(Answer::Message(_)) => break,
                    Ok(Answer::Error(body)) => {
                        return Err(match fetch::refused(body) {
                            FetchError::Refused(message) => PushError::Refused(message),
                            other => PushError::Answer(Box::new(other)),
                        })
                    }
                    Err(source) if attempt >= PIECE_ATTEMPTS || !transient(&source) => {
                        return Err(PushError::Upload {
                            confirmed: uploaded.pieces - before,
                            source,
                        })
                    }
                    Err(_) => {
                        attempt += 1;
                        uploaded.repeated += 1;
                    }
                }
            }
            uploaded.pieces += 1;
            uploaded.sent.objects += next.sent.objects;
            uploaded.sent.commits += next.sent.commits;
            uploaded.sent.bytes += next.sent.bytes;
        }
        Ok(())
    }

    /// Refuses moves whose entry `push` could not be written ([`MAX_ENTRY_LEN`]), before
    /// anything is sent: the hub would accept the update, and the clone could not record it.
    fn require_entry_fits(
        &self,
        remote: &RemoteName,
        moves: &[Move],
        actor: &Actor,
    ) -> Result<(), PushError> {
        // The largest values: a receipt with the most digits, and a record hash in its one form.
        let hash: RecordHash = format!("b3:{}", "0".repeat(64))
            .parse()
            .expect("a record hash in its one form");
        let remotes = moves
            .iter()
            .map(|moved| {
                let value = |commit: Option<CommitId>, entry| {
                    commit.map(|commit| RemoteRef { commit, entry })
                };
                (
                    remote_ref_name(remote, moved.name()),
                    Moved::from_to(
                        value(moved.old_commit(), moved.previous().unwrap_or(hash)),
                        value(moved.new_commit(), hash),
                    ),
                )
            })
            .collect();
        let receipt = Receipt {
            number: u64::MAX,
            hash,
        };
        let change = Change::try_new_push(now(), actor.clone(), Vec::new(), remotes, receipt)
            .and_then(|change| change.in_worktree(self.worktree.clone(), None))
            .map_err(|error| PushError::Update(UpdateError::Log(OpLogError::Entry(error))))?;
        let size = largest_entry_len(change);
        if size as u64 > MAX_ENTRY_LEN {
            return Err(PushError::TooLarge {
                moves: moves.len(),
                size,
            });
        }
        Ok(())
    }

    /// Step 5 of a push, under the lock: the configuration is read again, the remote references
    /// of the receipt move to its state, and `branch`, the local `main` of a merge with the
    /// value it had when the attempt began, moves to its new commit, after one entry `push`. A
    /// remote reference that is no longer in the state that the update was signed against is
    /// left as it is: a fetch moved it to a later state of the hub. A branch that no longer has
    /// its value is left as well: another writer moved it. Returns the entry, `None` if nothing
    /// had to move, and whether `branch` moved.
    pub(super) fn record_push(
        &self,
        lock: &WriteLock,
        remote: &RemoteName,
        config: &RemoteConfig,
        receipt: &RecordEntry,
        branch: Option<(BranchName, Option<CommitId>, CommitId)>,
        actor: &Actor,
    ) -> Result<(Option<Entry>, bool), PushError> {
        if self.remote(remote).map_err(PushError::Remote)?.as_ref() != Some(config) {
            return Err(PushError::RemoteChanged(remote.clone()));
        }
        let hash = receipt.hash();
        // A remote reference moves from the state that the update was signed against. One that
        // a fetch moved in between holds a later state of the hub, which it keeps.
        let mut remotes: Vec<(RemoteRefName, Option<RemoteRef>)> = Vec::new();
        for moved in receipt.update().payload().moves() {
            let name = remote_ref_name(remote, moved.name());
            let signed = moved
                .old_commit()
                .zip(moved.previous())
                .map(|(commit, entry)| RemoteRef { commit, entry });
            if self.refs().remote_ref(&name).map_err(PushError::Refs)? != signed {
                continue;
            }
            let value = moved.new_commit().map(|commit| RemoteRef {
                commit,
                entry: hash,
            });
            remotes.push((name, value));
        }
        let mut branches: Vec<(BranchName, Option<CommitId>)> = Vec::new();
        if let Some((name, expected, new)) = branch {
            if self.refs().branch(&name).map_err(PushError::Refs)? == expected {
                branches.push((name, Some(new)));
            }
        }
        let targets = RestoreTargets {
            head: None,
            working_copy: None,
            branches: &branches,
            remotes: &remotes,
        };
        let number = receipt.number();
        let logged = self
            .refs_store()
            .push_after(lock, &targets, self.objects(), |moves| {
                let branches = moves
                    .branches
                    .iter()
                    .map(|(name, old, new)| (name.clone(), Moved::from_to(*old, *new)))
                    .collect();
                let remotes = moves
                    .remotes
                    .iter()
                    .map(|(name, old, new)| (name.clone(), Moved::from_to(*old, *new)))
                    .collect();
                let receipt = Receipt { number, hash };
                let change = Change::try_new_push(now(), actor.clone(), branches, remotes, receipt)
                    .map_err(OpLogError::Entry)?;
                self.append(lock, &change)
            })
            .map_err(PushError::Refs)?;
        let entry = logged.into_result().map_err(PushError::Update)?;
        Ok((entry, !branches.is_empty()))
    }

    /// Step 6 of a push: the anchor of the tip of the log, sent and its confirmation kept.
    pub(super) fn send_anchor(
        &self,
        remote: &RemoteName,
        hub: &HubAddress,
        transport: &mut dyn Transport,
        signer: &dyn Signer,
    ) -> Result<HeldAnchor, AnchorFailure> {
        let anchor = self
            .anchor(remote, hub.clone(), signer)
            .map_err(|error| AnchorFailure::Log(Box::new(error)))?;
        let answer = transport
            .anchor(&mut anchor.encode().as_slice())
            .map_err(AnchorFailure::Transport)?;
        let held = match answer {
            Answer::Message(body) => {
                let bytes = fetch::read_bounded(body, MAX_HELD_ANCHOR)
                    .map_err(|error| AnchorFailure::Answer(Box::new(error)))?;
                HeldAnchor::parse(&bytes).map_err(AnchorFailure::Unreadable)?
            }
            Answer::Error(body) => {
                return Err(match fetch::refused(body) {
                    FetchError::Refused(message) => AnchorFailure::Refused(message),
                    other => AnchorFailure::Answer(Box::new(other)),
                })
            }
        };
        self.keep_confirmation(remote, &held)
            .map_err(|error| AnchorFailure::NotKept(Box::new(error)))?;
        Ok(held)
    }
}

/// Whether a failure of a request may pass when the request is sent again: the carrier failed,
/// and not because it does not carry the request at all.
fn transient(error: &TransportError) -> bool {
    matches!(error, TransportError::Carrier(source) if source.kind() != io::ErrorKind::Unsupported)
}

/// Sends a signed update and reads the receipt, checked against the pinned key `hub`.
fn send(
    transport: &mut dyn Transport,
    update: &RefUpdate,
    hub: &PublicKey,
) -> Result<RecordEntry, PushError> {
    let answer = transport
        .ref_update(&mut update.encode().as_slice())
        .map_err(PushError::Transport)?;
    match answer {
        Answer::Message(body) => {
            let bytes = fetch::read_bounded(body, MAX_RECEIPT)
                .map_err(|error| PushError::ReceiptLost(Box::new(error)))?;
            let receipt = RecordEntry::parse(&bytes).map_err(PushError::Unreadable)?;
            receipt
                .check_receipt(update, hub)
                .map_err(PushError::Receipt)?;
            Ok(receipt)
        }
        Answer::Error(body) => Err(match fetch::refused(body) {
            FetchError::Refused(ErrorMessage::Stale(stale)) => PushError::Stale(stale),
            FetchError::Refused(message) => PushError::Refused(message),
            other => PushError::Answer(Box::new(other)),
        }),
    }
}

/// The remote reference of `remote` for a qualified name on the hub.
pub(super) fn remote_ref_name(remote: &RemoteName, name: &QualifiedName) -> RemoteRefName {
    RemoteRefName {
        remote: remote.clone(),
        kind: name.kind(),
        name: name.name().clone(),
    }
}

/// Asks the authorizer for `push` to the remote `remote`, with the target `remote <name>`.
pub(super) fn authorize_push(
    remote: &RemoteName,
    actor: &Actor,
    authorizer: &dyn Authorizer,
) -> Result<Allowed, PushError> {
    let operation = OperationName::try_new(PUSH).expect("`push` is a valid operation name");
    let target = format!("remote {remote}");
    Allowed::ask(authorizer, actor, &operation, &target).ok_or_else(|| {
        PushError::Update(UpdateError::Denied {
            operation: operation.to_string(),
            target,
        })
    })
}

fn boxed_objects(error: AnswerError) -> PushError {
    PushError::Objects(Box::new(error))
}

/// Why the anchor after a push is missing (ADR-026). The push has happened all the same.
#[derive(Debug)]
#[non_exhaustive]
pub enum AnchorFailure {
    /// The anchor could not be built.
    Log(Box<LogAnchorError>),
    /// The hub confirmed the anchor, and the clone could not keep the confirmation.
    NotKept(Box<LogAnchorError>),
    /// The request got no answer.
    Transport(TransportError),
    /// The answer could not be read.
    Answer(Box<FetchError>),
    /// The hub refused the anchor.
    Refused(ErrorMessage),
    /// The reader refused the confirmation.
    Unreadable(ParseError),
}

impl fmt::Display for AnchorFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Log(error) => write!(f, "the anchor of the log failed: {error}"),
            Self::NotKept(error) => write!(
                f,
                "the hub confirmed the anchor, but its confirmation was not kept: {error}"
            ),
            Self::Transport(error) => write!(f, "the anchor was not sent: {error}"),
            Self::Answer(error) => write!(f, "the answer to the anchor: {error}"),
            Self::Refused(message) => write!(f, "the hub refused the anchor: {message}"),
            Self::Unreadable(error) => {
                write!(f, "the confirmation of the anchor cannot be read: {error}")
            }
        }
    }
}

impl Error for AnchorFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Log(error) | Self::NotKept(error) => error.source(),
            Self::Transport(error) => error.source(),
            Self::Answer(error) => error.source(),
            Self::Unreadable(error) => error.source(),
            Self::Refused(_) => None,
        }
    }
}

/// Why [`Repository::push`] failed. Every variant but [`Unrecorded`](Self::Unrecorded) moved
/// nothing in the clone. On the hub nothing moved either, except after the receipt and where
/// the outcome is not known ([`outcome_unknown`](Self::outcome_unknown)).
#[derive(Debug)]
#[non_exhaustive]
pub enum PushError {
    /// The references could not be read, the lock could not be taken, or the repository is bare
    /// ([`RefError::Bare`]).
    Refs(RefError),
    /// The authorizer refused `push`, or the change of the references failed (see
    /// [`UpdateError`]).
    Update(UpdateError),
    /// The configuration of the remote could not be read.
    Remote(RemoteError),
    /// The repository has no remote of this name.
    NoSuchRemote(RemoteName),
    /// The configuration of the remote holds no pinned key of the hub, so no receipt can be
    /// checked.
    HubKey(HubKeyError),
    /// A target breaks a rule of a move, such as a tag that would move from one commit to
    /// another.
    Move {
        /// The reference.
        name: QualifiedName,
        /// The rule that it breaks.
        problem: RecordProblem,
    },
    /// A reference is named twice.
    Duplicate(QualifiedName),
    /// The entry `push` of these moves would be larger than an entry of the log may be (64 KiB),
    /// so the clone could not record the receipt. Nothing was sent; a push
    /// of fewer references at a time fits.
    TooLarge {
        /// The moves of the push.
        moves: usize,
        /// The size of the entry in bytes.
        size: usize,
    },
    /// An object of the upload could not be read from the store.
    Objects(Box<AnswerError>),
    /// A piece of the upload failed: on the carrier in [`PIECE_ATTEMPTS`] attempts, or at once
    /// for a failure that no repeat mends. The pieces before it are confirmed; the hub holds
    /// their objects unnamed.
    Upload {
        /// The pieces of this pass that the other side confirmed before the one that failed.
        confirmed: u32,
        /// The last failure.
        source: TransportError,
    },
    /// The update could not be signed.
    Sign(SealError),
    /// The ref update got no answer. Whether the hub accepted it is not known; a fetch says it.
    Transport(TransportError),
    /// An error of the hub could not be read: its body failed, it is too long, or its message
    /// is not readable.
    Answer(Box<FetchError>),
    /// The hub answered success, and the receipt could not be read: its body failed or it is
    /// too long. Whether the hub accepted the update is not known; a fetch says it.
    ReceiptLost(Box<FetchError>),
    /// The reader refused the receipt. Whether the hub accepted the update is not known; a
    /// fetch says it.
    Unreadable(ParseError),
    /// The receipt is not the one of the update, or another key than the pinned one signed it.
    /// Whether the hub accepted the update is not known; a fetch says it.
    Receipt(ReceiptError),
    /// The update is stale: a reference on the hub is no longer in the state that the remote
    /// reference names. Carries the state of each stale reference on the hub; a fetch brings
    /// it, and the actor decides what to push then.
    Stale(Stale),
    /// The hub answered an error: a refusal of the register, the policy, the write ownership,
    /// or the gate, a refusal for want of objects after the second upload, or an error of the
    /// protocol.
    Refused(ErrorMessage),
    /// The configuration of the remote was removed or changed during the push. It comes as the
    /// source of [`Unrecorded`](Self::Unrecorded), as the check runs after the receipt.
    RemoteChanged(RemoteName),
    /// The hub accepted the update, and recording it in the clone failed: the configuration of
    /// the remote changed, or the lock, the entry, a move, or the flush of a move failed
    /// ([`entry`](PushError::entry) names the entry if it was written). The hub moved; a fetch
    /// brings the remote references along.
    Unrecorded {
        /// The receipt of the hub.
        receipt: Box<RecordEntry>,
        /// What failed.
        source: Box<PushError>,
    },
}

impl PushError {
    /// Whether the update was sent and it is not known whether the hub accepted it: it got no
    /// answer, or the answer of success could not be read or checked. Nothing moved in the
    /// clone; a fetch says whether the hub moved.
    pub fn outcome_unknown(&self) -> bool {
        matches!(
            self,
            Self::Transport(_) | Self::ReceiptLost(_) | Self::Unreadable(_) | Self::Receipt(_)
        )
    }

    /// The cause of an error whose outcome is not known, without the words on the outcome.
    pub(super) fn fmt_cause(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "{error}"),
            Self::ReceiptLost(error) => write!(f, "the receipt cannot be read: {error}"),
            Self::Unreadable(error) => write!(f, "the receipt cannot be read: {error}"),
            Self::Receipt(error) => write!(f, "{error}"),
            other => write!(f, "{other}"),
        }
    }

    /// The receipt, if the hub accepted the update.
    pub fn receipt(&self) -> Option<&RecordEntry> {
        match self {
            Self::Unrecorded { receipt, .. } => Some(receipt),
            _ => None,
        }
    }

    /// The entry `push`, if it was written.
    pub fn entry(&self) -> Option<&Entry> {
        match self {
            Self::Update(error) => error.entry(),
            Self::Unrecorded { source, .. } => source.entry(),
            _ => None,
        }
    }

    /// The error of the file system, if that is what failed (see the crate documentation).
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Refs(error) => error.io_error(),
            Self::Update(error) => error.io_error(),
            Self::Remote(error) => error.io_error(),
            Self::Objects(error) => error.io_error(),
            Self::Upload { source, .. } | Self::Transport(source) => source.io_error(),
            Self::Sign(error) => error.io_error(),
            Self::Answer(error) | Self::ReceiptLost(error) => error.io_error(),
            Self::Unrecorded { source, .. } => source.io_error(),
            Self::NoSuchRemote(_)
            | Self::HubKey(_)
            | Self::Move { .. }
            | Self::Duplicate(_)
            | Self::TooLarge { .. }
            | Self::Unreadable(_)
            | Self::Receipt(_)
            | Self::Stale(_)
            | Self::Refused(_)
            | Self::RemoteChanged(_) => None,
        }
    }
}

impl fmt::Display for PushError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refs(error) => error.fmt(f),
            Self::Update(error) => error.fmt(f),
            Self::Remote(error) => error.fmt(f),
            Self::NoSuchRemote(name) => write!(f, "there is no remote `{name}`"),
            Self::HubKey(error) => write!(f, "{error}; nothing was pushed"),
            Self::Move { name, problem } => write!(f, "the move of `{name}`: {problem}"),
            Self::Duplicate(name) => write!(f, "the reference `{name}` is named twice"),
            Self::TooLarge { moves, size } => write!(
                f,
                "the entry of a push of {moves} references would be {size} bytes, but an entry \
                 holds at most {MAX_ENTRY_LEN} bytes; nothing was sent, push fewer references at \
                 a time"
            ),
            Self::Objects(error) => write!(f, "the upload cannot be read from the store: {error}"),
            Self::Upload { confirmed, source } if transient(source) => write!(
                f,
                "the upload failed after {confirmed} confirmed pieces: {source}; nothing moved, \
                 and a push again sends the upload anew"
            ),
            Self::Upload { confirmed, source } => write!(
                f,
                "the upload failed after {confirmed} confirmed pieces: {source}; nothing moved"
            ),
            Self::Sign(error) => write!(f, "the ref update cannot be signed: {error}"),
            Self::Transport(_) | Self::ReceiptLost(_) | Self::Unreadable(_) | Self::Receipt(_) => {
                self.fmt_cause(f)?;
                f.write_str("; whether the hub accepted the push is not known, a fetch says it")
            }
            Self::Answer(error) => error.fmt(f),
            Self::Stale(stale) => write!(f, "{stale}; nothing moved, fetch first"),
            Self::Refused(message) => write!(f, "the hub refused: {message}"),
            Self::RemoteChanged(name) => write!(
                f,
                "the remote `{name}` was removed or changed during the push"
            ),
            Self::Unrecorded { receipt, source } => write!(
                f,
                "the hub accepted the push as record entry {}, and recording it in the clone \
                 failed: {source}; a fetch brings the remote references along",
                receipt.number()
            ),
        }
    }
}

impl Error for PushError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        // The message includes the message of the cause, so the source is the cause of the cause
        // (see the crate documentation).
        match self {
            Self::Refs(error) => error.source(),
            Self::Update(error) => error.source(),
            Self::Remote(error) => error.source(),
            Self::Objects(error) => error.source(),
            Self::Upload { source, .. } | Self::Transport(source) => source.source(),
            Self::Sign(error) => error.source(),
            Self::Answer(error) | Self::ReceiptLost(error) => error.source(),
            Self::Unreadable(error) => error.source(),
            Self::Unrecorded { source, .. } => source.source(),
            Self::NoSuchRemote(_)
            | Self::HubKey(_)
            | Self::Move { .. }
            | Self::Duplicate(_)
            | Self::TooLarge { .. }
            | Self::Receipt(_)
            | Self::Stale(_)
            | Self::Refused(_)
            | Self::RemoteChanged(_) => None,
        }
    }
}

#[cfg(test)]
mod tests;
