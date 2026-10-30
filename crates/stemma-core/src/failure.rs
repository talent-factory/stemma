//! What a caller can do about a failure (ADR-018).
//!
//! The errors of the crate wrap each other, and `Display` prints the wrapped message, so
//! [`std::error::Error::source`] leaves the wrapped error out. A caller that wants to know whether
//! a lock was held cannot walk that chain. [`Classify::kind`] looks through the wrappers and
//! names the one cause that decides what the caller does next.
//!
//! Every `match` here lists all variants and has no wildcard arm. A new variant of an error does
//! not compile until it is classified, so a lock or damage cannot fall back to
//! [`FailureKind::Other`] unnoticed.

use crate::history::HistoryError;
use crate::index::WriteTreeError;
use crate::oplog::OpLogError;
use crate::refs::RefError;
use crate::repository::{
    ApplyPurgeError, CheckoutError, CommitError, DiffError, InitError, JournalError, OpenError,
    PurgeError, PurgeFailure, RestoreError, SnapshotError, StatusError, TrackError, UpdateError,
};
use crate::store::StoreError;
use crate::verify::VerifyError;

/// The kind of a failure, as far as a caller acts on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// Another writer holds a lock, or won every race the writer tried. A later call can
    /// succeed. The lock error itself writes nothing, but the call may have left an entry for a move
    /// that did not happen, or the entry of the snapshot that came first.
    Locked,
    /// The repository is damaged: the log has a gap or a wrong number, the chain does not hold, or
    /// a file or object is not in canonical form. A retry does not help.
    Damaged,
    /// Every other failure.
    Other,
}

/// An error that can say what kind of failure it is.
pub trait Classify {
    /// The kind of the failure, looking through the errors that wrap the cause.
    fn kind(&self) -> FailureKind;
}

impl Classify for StoreError {
    fn kind(&self) -> FailureKind {
        match self {
            Self::Corrupt { .. } => FailureKind::Damaged,
            // Like `RefError::MissingCommit`, the variant also stands for an object that the caller
            // named with the wrong kind, which is an argument error.
            Self::Io { .. } | Self::KindMismatch { .. } => FailureKind::Other,
        }
    }
}

impl Classify for OpenError {
    fn kind(&self) -> FailureKind {
        match self {
            // The file is there but not in canonical form.
            Self::MalformedFormat { .. } => FailureKind::Damaged,
            // A missing `format`, another version, and a `.stemma` that is not a directory are
            // not a form the code wrote, but the caller cannot tell them from a wrong directory or
            // a newer program, so they stay with the other failures.
            Self::NotARepository { .. }
            | Self::NoRepositoryAbove { .. }
            | Self::Missing { .. }
            | Self::NotADirectoryToStartIn { .. }
            | Self::NotUtf8 { .. }
            | Self::OtherOwner { .. }
            | Self::AtVolumeRoot { .. }
            | Self::NotADirectory { .. }
            | Self::MissingFormat { .. }
            | Self::UnsupportedFormat { .. }
            | Self::Io { .. } => FailureKind::Other,
        }
    }
}

impl Classify for InitError {
    fn kind(&self) -> FailureKind {
        match self {
            Self::Log(error) => error.kind(),
            Self::Store(error) => error.kind(),
            Self::Leftover { error, .. } => error.kind(),
            Self::Denied { .. }
            | Self::AlreadyExists { .. }
            | Self::Io { .. }
            | Self::NotDurable { .. }
            | Self::ChangeId(_) => FailureKind::Other,
        }
    }
}

impl<L> Classify for VerifyError<L> {
    fn kind(&self) -> FailureKind {
        match self {
            // An object that a tree, commit or index names is not in the store.
            Self::Missing { .. } => FailureKind::Damaged,
            Self::Store { source, .. } => source.kind(),
        }
    }
}

impl Classify for RefError {
    fn kind(&self) -> FailureKind {
        match self {
            Self::Locked { .. } => FailureKind::Locked,
            Self::Corrupt { .. } => FailureKind::Damaged,
            Self::Store(error) => error.kind(),
            // A branch, HEAD, or the working-copy commit that moved is the caller's stale view, not
            // a held lock: the caller reads again and decides (ADR-018 keeps "a moved branch" at
            // 1). `StaleHead` and `StaleWorkingCopy` are the same case for HEAD and the
            // working-copy commit.
            // A tag that was deleted and set again since the caller looked is the same stale view.
            Self::Stale { .. }
            | Self::StaleTag { .. }
            | Self::StaleHead { .. }
            | Self::StaleWorkingCopy { .. } => FailureKind::Other,
            // A tag that exists is a name that is taken; it never moves, so there is nothing to
            // read again and retry.
            Self::TagExists { .. } => FailureKind::Other,
            // A pointer that a purge moves is at neither of its journal values. A person has to
            // look at it, but the variant also stands for a plain race after a lock removed by
            // hand, so it is not called damage here; `purge --resume` says what to do.
            Self::PurgeStale { .. } => FailureKind::Other,
            // The variant cannot tell a commit that the caller named (an argument error) from one
            // that the log or a pointer names (damage), so it stays 1 in both.
            Self::MissingCommit(_) | Self::NotACommit { .. } => FailureKind::Other,
            Self::PurgePending { .. }
            | Self::Io { .. }
            | Self::Conflict { .. }
            | Self::NotFlushed { .. }
            | Self::NoHead { .. }
            | Self::NoWorkingCopy { .. } => FailureKind::Other,
        }
    }
}

impl Classify for OpLogError {
    fn kind(&self) -> FailureKind {
        match self {
            Self::Contended { .. } => FailureKind::Locked,
            Self::Corrupt { .. }
            | Self::WrongNumber { .. }
            | Self::Gap { .. }
            | Self::TipAhead { .. }
            | Self::BadTip { .. }
            | Self::TipMismatch { .. }
            | Self::TipMissing { .. }
            | Self::TipBehind { .. }
            | Self::ChainBroken { .. }
            | Self::UnchainedAfterChain { .. }
            | Self::UnexpectedFile { .. }
            | Self::UnlistedStub { .. } => FailureKind::Damaged,
            Self::Predecessor { source, .. } | Self::PurgeIncomplete { source, .. } => {
                source.kind()
            }
            Self::Io { .. }
            | Self::Missing { .. }
            | Self::Full
            | Self::NotFlushed { .. }
            | Self::EntryTooLarge { .. }
            | Self::Entry(_)
            | Self::NotListed { .. }
            | Self::NotAChange { .. }
            | Self::PurgeNotFlushed { .. } => FailureKind::Other,
        }
    }
}

impl Classify for UpdateError {
    fn kind(&self) -> FailureKind {
        match self {
            Self::Refs(error) => error.kind(),
            Self::Log(error) => error.kind(),
            Self::MoveFailed { source, .. }
            | Self::PartlyMoved { source, .. }
            | Self::NotDurable { source, .. } => source.kind(),
            Self::Denied { .. } => FailureKind::Other,
        }
    }
}

impl Classify for WriteTreeError {
    fn kind(&self) -> FailureKind {
        match self {
            Self::Store(error) => error.kind(),
            Self::Verify(error) => error.kind(),
            Self::Tree(_) => FailureKind::Other,
        }
    }
}

impl Classify for SnapshotError {
    fn kind(&self) -> FailureKind {
        match self {
            Self::Update(error) => error.kind(),
            Self::TakeOver(error) => error.kind(),
            Self::Refs(error) => error.kind(),
            Self::Store(error) => error.kind(),
            Self::WriteTree(error) => error.kind(),
            Self::NotOwner { .. } | Self::AgentTakeOver => FailureKind::Other,
            // The working-copy commit names an object that is not there or does not parse.
            Self::Missing { .. } | Self::Commit { .. } | Self::Tree { .. } => FailureKind::Damaged,
            Self::Unsupported { .. } | Self::Gone { .. } | Self::Index(_) | Self::Io { .. } => {
                FailureKind::Other
            }
        }
    }
}

impl Classify for CommitError {
    fn kind(&self) -> FailureKind {
        match self {
            Self::Snapshot(error) | Self::Tip(error) => error.kind(),
            Self::Store(error) => error.kind(),
            Self::Refs(error) => error.kind(),
            Self::Update(error) => error.kind(),
            Self::Log(error) => error.kind(),
            Self::AfterSnapshot { source, .. } => source.kind(),
            Self::EmptyMessage
            | Self::DetachedHead(_)
            | Self::NothingToCommit
            | Self::BranchMoved { .. }
            | Self::ChangeId(_) => FailureKind::Other,
        }
    }
}

impl Classify for StatusError {
    fn kind(&self) -> FailureKind {
        match self {
            Self::Update(error) => error.kind(),
            Self::Walk(error) => error.kind(),
            Self::Snapshot(error) => error.kind(),
            Self::Refs { source, .. } => source.kind(),
            Self::Read { source, .. } => source.kind(),
            Self::Ignore(_) => FailureKind::Other,
        }
    }
}

impl Classify for TrackError {
    fn kind(&self) -> FailureKind {
        match self {
            Self::Snapshot(error) => error.kind(),
            Self::NotDurable { error, .. } => error.kind(),
            Self::InvalidPath { .. }
            | Self::InvalidName { .. }
            | Self::NotFound { .. }
            | Self::Spelling { .. }
            | Self::Excluded { .. }
            | Self::GitDirectory { .. }
            | Self::CheckoutLeftover { .. }
            | Self::Ignore(_) => FailureKind::Other,
        }
    }
}

impl Classify for DiffError {
    fn kind(&self) -> FailureKind {
        match self {
            Self::Snapshot(error) => error.kind(),
            Self::Refs(error) => error.kind(),
            Self::Store(error) => error.kind(),
            Self::AfterSnapshot { source, .. } => source.kind(),
            // An object that a commit of the repository names is not there or does not parse.
            Self::Missing { .. } | Self::Damaged { .. } => FailureKind::Damaged,
            Self::UnknownCommit { .. } | Self::InvalidPath { .. } | Self::NoSuchPath { .. } => {
                FailureKind::Other
            }
        }
    }
}

impl Classify for HistoryError {
    fn kind(&self) -> FailureKind {
        match self {
            // A commit that a pointer or a commit names is not there, or its body is not in
            // canonical form: the history is damaged. Nobody named these commits as an argument.
            Self::Missing { .. } => FailureKind::Damaged,
            Self::Read(error) => match error.store_error() {
                Some(StoreError::Io { .. }) => FailureKind::Other,
                // A parent of the wrong kind (`KindMismatch`) is damage here, unlike the case of
                // `StoreError::kind`, where the caller named the object. So is an object that is
                // corrupt, a body that is not canonical, and an ID that does not match (a defect of
                // the reader, which the caller cannot tell from damage).
                Some(StoreError::Corrupt { .. } | StoreError::KindMismatch { .. }) | None => {
                    FailureKind::Damaged
                }
            },
        }
    }
}

impl Classify for CheckoutError {
    fn kind(&self) -> FailureKind {
        match self {
            Self::Snapshot(error) => error.kind(),
            Self::TempLeft { source, .. } | Self::Interrupted { source, .. } => source.kind(),
            Self::Ignore(_)
            | Self::Reserved { .. }
            | Self::Name { .. }
            | Self::Excluded { .. }
            | Self::CaseCollision { .. }
            | Self::ThroughLink { .. }
            | Self::InTheWay { .. }
            | Self::ChangedSincePlan { .. }
            | Self::ForeignPlan { .. } => FailureKind::Other,
        }
    }
}

impl Classify for RestoreError {
    fn kind(&self) -> FailureKind {
        match self {
            Self::Update(error) => error.kind(),
            Self::Log(error) => error.kind(),
            Self::Refs(error) => error.kind(),
            Self::Snapshot(error) => error.kind(),
            Self::Checkout(error) | Self::CheckoutFailed { source: error, .. } => error.kind(),
            Self::MoveFailed { source, .. }
            | Self::PartlyMoved { source, .. }
            | Self::NotDurable { source, .. }
            | Self::SnapshotNotDurable { source, .. } => source.kind(),
            Self::AfterSnapshot { source, .. } => source.kind(),
            Self::NoSuchEntry { .. }
            | Self::Purged { .. }
            | Self::IsPurge { .. }
            | Self::PurgePending { .. }
            | Self::OutcomeUnknown { .. }
            | Self::NoOldValue { .. } => FailureKind::Other,
        }
    }
}

impl Classify for JournalError {
    fn kind(&self) -> FailureKind {
        match self {
            Self::Malformed { .. } => FailureKind::Damaged,
            Self::Io { .. } | Self::NotFlushed { .. } => FailureKind::Other,
        }
    }
}

impl Classify for PurgeError {
    fn kind(&self) -> FailureKind {
        match self {
            Self::Update(error) => error.kind(),
            Self::Refs(error) => error.kind(),
            Self::Log(error) => error.kind(),
            Self::Read(error) => error.kind(),
            Self::Store(error) | Self::BlobUnreadable { source: error, .. } => error.kind(),
            Self::Leftover { source, .. } => source.kind(),
            // No command writes a `commit2` yet, so one in the store is reported as damage, as an
            // unknown kind was before.
            Self::UnsupportedKind { .. } => FailureKind::Damaged,
            Self::Agent
            | Self::NoSuchBlob { .. }
            | Self::DuplicateParent { .. }
            | Self::TooManyEntries { .. }
            | Self::TaggedCommit { .. }
            | Self::Unsound { .. } => FailureKind::Other,
        }
    }
}

impl Classify for PurgeFailure {
    fn kind(&self) -> FailureKind {
        match self {
            Self::Refs(error) => error.kind(),
            Self::Store(error) => error.kind(),
            Self::Log(error) => error.kind(),
            Self::Journal(error) => error.kind(),
            Self::Plan(error) => error.kind(),
            // The store no longer gives the objects that the journal names.
            Self::Mismatch { .. } | Self::Missing(_) => FailureKind::Damaged,
            Self::Entry(_) => FailureKind::Other,
        }
    }
}

impl Classify for ApplyPurgeError {
    fn kind(&self) -> FailureKind {
        match self {
            Self::Plan(error) => error.kind(),
            Self::Snapshot(error) => error.kind(),
            Self::Start(error) => error.kind(),
            Self::AfterSnapshot { source, .. } => source.kind(),
            Self::Unreadable(error) => error.kind(),
            Self::Stopped { source, .. } => source.kind(),
            Self::NotDurable { source, .. } => source.kind(),
            Self::Pending { .. } | Self::NotPending { .. } | Self::PastNoReturn { .. } => {
                FailureKind::Other
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::object::{Kind, ObjectId};
    use crate::refs::LockHolder;

    fn path() -> PathBuf {
        PathBuf::from(".stemma/x")
    }

    fn id() -> ObjectId {
        format!("b3:{}", "0".repeat(64)).parse().unwrap()
    }

    fn locked() -> RefError {
        RefError::Locked {
            path: path(),
            holder: LockHolder::Unknown,
        }
    }

    fn gap() -> OpLogError {
        OpLogError::Gap { missing: 2 }
    }

    fn missing_object() -> SnapshotError {
        SnapshotError::Missing {
            id: id(),
            kind: Kind::Commit,
        }
    }

    #[test]
    fn the_read_errors_of_the_history_are_damage_except_a_failing_disk() {
        use crate::object::CommitParseError;
        use crate::store::ReadError;

        let commit = crate::object::CommitId::new(id());
        let tree = crate::object::TreeId::new(id());
        let io = StoreError::Io {
            operation: "read the object file",
            path: path(),
            source: std::io::Error::other("the disk failed"),
        };
        let mismatch = StoreError::KindMismatch {
            id: id(),
            expected: Kind::Commit,
            found: Kind::Tree,
        };
        let history = |error: ReadError| HistoryError::Read(error).kind();
        assert_eq!(history(ReadError::Store(Box::new(io))), FailureKind::Other);
        // A parent that names a tree: nobody gave that object as an argument.
        assert_eq!(
            history(ReadError::Store(Box::new(mismatch))),
            FailureKind::Damaged
        );
        assert_eq!(
            history(ReadError::Commit {
                id: commit,
                source: CommitParseError::NotUtf8 { valid_up_to: 0 }
            }),
            FailureKind::Damaged
        );
        assert_eq!(
            history(ReadError::IdMismatch {
                requested: id(),
                parsed: tree.object_id()
            }),
            FailureKind::Damaged
        );
    }

    #[test]
    fn a_commit_of_the_history_that_is_missing_is_damage() {
        let commit = crate::object::CommitId::new(id());
        assert_eq!(
            HistoryError::Missing {
                id: commit,
                child: None
            }
            .kind(),
            FailureKind::Damaged
        );
    }

    #[test]
    fn a_format_file_that_is_not_canonical_is_damage_and_the_other_open_errors_are_not() {
        assert_eq!(
            OpenError::MalformedFormat { path: path() }.kind(),
            FailureKind::Damaged
        );
        for error in [
            OpenError::NotARepository { path: path() },
            OpenError::MissingFormat { path: path() },
            OpenError::UnsupportedFormat {
                path: path(),
                found: "99".to_owned(),
            },
        ] {
            assert_eq!(error.kind(), FailureKind::Other, "{error}");
        }
    }

    #[test]
    fn init_passes_the_cause_of_the_log_and_the_store_on() {
        assert_eq!(InitError::Log(gap()).kind(), FailureKind::Damaged);
        assert_eq!(
            InitError::Leftover {
                path: path(),
                error: Box::new(InitError::Log(gap())),
                source: std::io::Error::other("disk"),
            }
            .kind(),
            FailureKind::Damaged
        );
        assert_eq!(
            InitError::AlreadyExists { path: path() }.kind(),
            FailureKind::Other
        );
    }

    /// These stay with the other failures because the code cannot tell a wrong argument from
    /// damage (see the comments at the variants), and ADR-018 says so. A change has to be made on
    /// purpose, with the ADR and the book.
    #[test]
    fn the_variants_that_stand_for_an_argument_as_well_as_for_damage_stay_with_the_others() {
        assert_eq!(
            RefError::NotACommit {
                id: id(),
                found: Kind::Blob,
            }
            .kind(),
            FailureKind::Other
        );
        assert_eq!(
            StoreError::KindMismatch {
                id: id(),
                expected: Kind::Commit,
                found: Kind::Blob,
            }
            .kind(),
            FailureKind::Other
        );
    }

    #[test]
    fn an_object_file_that_is_corrupt_is_damage_wherever_it_is_wrapped() {
        let corrupt = || StoreError::Corrupt {
            id: id(),
            path: path(),
            reason: crate::store::CorruptReason::NotARegularFile,
        };
        assert_eq!(corrupt().kind(), FailureKind::Damaged);
        assert_eq!(
            RefError::Store(Box::new(corrupt())).kind(),
            FailureKind::Damaged
        );
        assert_eq!(
            SnapshotError::Store(Box::new(corrupt())).kind(),
            FailureKind::Damaged
        );
    }

    #[test]
    fn an_object_that_the_index_names_and_the_store_lacks_is_damage() {
        let missing: WriteTreeError = WriteTreeError::Verify(VerifyError::Missing {
            at: crate::verify::EntryPath("a.txt".to_owned()),
            id: id(),
        });
        assert_eq!(missing.kind(), FailureKind::Damaged);
        let io: VerifyError<()> = VerifyError::Store {
            at: (),
            source: Box::new(StoreError::Io {
                operation: "read the object file",
                path: path(),
                source: std::io::Error::other("disk"),
            }),
        };
        assert_eq!(io.kind(), FailureKind::Other);
    }

    #[test]
    fn a_lock_and_a_lost_number_race_are_retryable() {
        assert_eq!(locked().kind(), FailureKind::Locked);
        assert_eq!(
            OpLogError::Contended { attempts: 8 }.kind(),
            FailureKind::Locked
        );
    }

    #[test]
    fn every_damage_of_the_log_is_damage() {
        for error in [
            gap(),
            OpLogError::WrongNumber {
                path: path(),
                expected: 2,
                found: 3,
            },
            OpLogError::TipAhead {
                path: path(),
                tip: 5,
                last: 3,
            },
            OpLogError::BadTip { path: path() },
            OpLogError::TipMissing {
                path: path(),
                last: 3,
            },
            OpLogError::TipBehind {
                path: path(),
                tip: 1,
                last: 3,
            },
            OpLogError::UnexpectedFile { path: path() },
            OpLogError::UnlistedStub { number: 2 },
        ] {
            assert_eq!(error.kind(), FailureKind::Damaged, "{error}");
        }
    }

    #[test]
    fn a_failure_of_the_log_that_is_neither_stays_other() {
        for error in [
            OpLogError::Missing { number: 9 },
            OpLogError::Full,
            OpLogError::EntryTooLarge { size: 1 << 20 },
            OpLogError::NotListed { number: 2 },
        ] {
            assert_eq!(error.kind(), FailureKind::Other, "{error}");
        }
    }

    #[test]
    fn a_missing_head_is_not_a_lock() {
        assert_eq!(RefError::NoHead { path: path() }.kind(), FailureKind::Other);
        assert_eq!(
            RefError::PurgeStale {
                pointer: "HEAD".to_owned()
            }
            .kind(),
            FailureKind::Other
        );
    }

    #[test]
    fn the_predecessor_of_an_entry_gives_its_own_kind() {
        let damaged = OpLogError::Predecessor {
            number: 1,
            source: Box::new(gap()),
        };
        let other = OpLogError::Predecessor {
            number: 1,
            source: Box::new(OpLogError::Missing { number: 1 }),
        };
        assert_eq!(damaged.kind(), FailureKind::Damaged);
        assert_eq!(other.kind(), FailureKind::Other);
    }

    #[test]
    fn the_wrappers_of_an_update_pass_the_cause_on() {
        assert_eq!(UpdateError::Refs(locked()).kind(), FailureKind::Locked);
        assert_eq!(UpdateError::Log(gap()).kind(), FailureKind::Damaged);
        assert_eq!(
            UpdateError::Denied {
                operation: "commit".to_owned(),
                target: "main".to_owned()
            }
            .kind(),
            FailureKind::Other
        );
    }

    #[test]
    fn the_wrappers_of_a_snapshot_pass_the_cause_on() {
        assert_eq!(
            SnapshotError::Update(UpdateError::Refs(locked())).kind(),
            FailureKind::Locked
        );
        assert_eq!(SnapshotError::Refs(locked()).kind(), FailureKind::Locked);
        assert_eq!(
            SnapshotError::Update(UpdateError::Log(gap())).kind(),
            FailureKind::Damaged
        );
        assert_eq!(missing_object().kind(), FailureKind::Damaged);
    }

    #[test]
    fn the_wrappers_of_the_commands_pass_the_cause_on() {
        // Each pair is the kind of a wrapper around a lock and of one around damage.
        for (locked_kind, damaged_kind) in [
            (
                CommitError::Snapshot(SnapshotError::Refs(locked())).kind(),
                CommitError::Tip(missing_object()).kind(),
            ),
            (
                CommitError::Refs(locked()).kind(),
                CommitError::Log(gap()).kind(),
            ),
            (
                StatusError::Snapshot(SnapshotError::Refs(locked())).kind(),
                StatusError::Read {
                    source: missing_object(),
                    saved: None,
                }
                .kind(),
            ),
            (
                StatusError::Refs {
                    source: locked(),
                    saved: None,
                }
                .kind(),
                StatusError::Snapshot(missing_object()).kind(),
            ),
            (
                TrackError::Snapshot(SnapshotError::Refs(locked())).kind(),
                TrackError::Snapshot(missing_object()).kind(),
            ),
            (
                DiffError::Snapshot(SnapshotError::Refs(locked())).kind(),
                DiffError::Snapshot(missing_object()).kind(),
            ),
            (
                DiffError::Refs(locked()).kind(),
                DiffError::Damaged {
                    id: id(),
                    reason: "no".to_owned(),
                }
                .kind(),
            ),
            (
                RestoreError::Refs(locked()).kind(),
                RestoreError::Log(gap()).kind(),
            ),
            (
                RestoreError::Snapshot(SnapshotError::Refs(locked())).kind(),
                RestoreError::Snapshot(missing_object()).kind(),
            ),
            (
                RestoreError::Update(UpdateError::Refs(locked())).kind(),
                RestoreError::Update(UpdateError::Log(gap())).kind(),
            ),
        ] {
            assert_eq!(locked_kind, FailureKind::Locked);
            assert_eq!(damaged_kind, FailureKind::Damaged);
        }
    }

    #[test]
    fn a_diff_that_met_damage_after_its_snapshot_is_damage() {
        let after = DiffError::AfterSnapshot {
            entry: 4,
            source: Box::new(DiffError::Missing { id: id() }),
        };
        assert_eq!(after.kind(), FailureKind::Damaged);
        assert_eq!(
            DiffError::UnknownCommit {
                operand: "x".to_owned()
            }
            .kind(),
            FailureKind::Other
        );
    }

    #[test]
    fn the_wrappers_of_a_purge_pass_the_cause_on() {
        assert_eq!(
            ApplyPurgeError::Start(PurgeFailure::Refs(locked())).kind(),
            FailureKind::Locked
        );
        assert_eq!(
            ApplyPurgeError::Plan(PurgeError::Refs(locked())).kind(),
            FailureKind::Locked
        );
        assert_eq!(
            ApplyPurgeError::Snapshot(SnapshotError::Refs(locked())).kind(),
            FailureKind::Locked
        );
        assert_eq!(
            ApplyPurgeError::Stopped {
                step: crate::repository::PurgeStep::Entry,
                entries: Vec::new(),
                source: Box::new(PurgeFailure::Log(gap())),
            }
            .kind(),
            FailureKind::Damaged
        );
        assert_eq!(
            ApplyPurgeError::Unreadable(Box::new(JournalError::Malformed { path: path() })).kind(),
            FailureKind::Damaged
        );
        assert_eq!(
            ApplyPurgeError::Plan(PurgeError::Agent).kind(),
            FailureKind::Other
        );
        assert_eq!(
            ApplyPurgeError::NotPending { path: path() }.kind(),
            FailureKind::Other
        );
    }
}
