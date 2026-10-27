//! What a caller can do about a failure (ADR-017).
//!
//! The errors of the crate wrap each other, and `Display` prints the wrapped message, so
//! [`std::error::Error::source`] leaves the wrapped error out. A caller that wants to know whether
//! a lock was held cannot walk that chain. [`Classify::kind`] looks through the wrappers and
//! names the one cause that decides what the caller does next.

use crate::oplog::OpLogError;
use crate::refs::RefError;
use crate::repository::{
    CommitError, DiffError, RestoreError, SnapshotError, StatusError, TrackError, UpdateError,
};

/// The kind of a failure, as far as a caller acts on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// Another writer holds a lock, or won every race the writer tried. Nothing was changed by
    /// this failure itself, and a later call can succeed.
    Locked,
    /// The repository is damaged: the log has a gap or a wrong number, the chain does not hold, or
    /// a file or object is not in canonical form. A retry does not help.
    Damaged,
    /// Every other failure.
    Other,
}

impl FailureKind {
    /// The kind of two causes that both applied: damage wins over a lock, because a person has to
    /// look at the repository, and a lock wins over the rest.
    #[must_use]
    pub fn worst(self, other: Self) -> Self {
        match (self, other) {
            (Self::Damaged, _) | (_, Self::Damaged) => Self::Damaged,
            (Self::Locked, _) | (_, Self::Locked) => Self::Locked,
            _ => Self::Other,
        }
    }
}

/// An error that can say what kind of failure it is.
pub trait Classify {
    /// The kind of the failure, looking through the errors that wrap the cause.
    fn kind(&self) -> FailureKind;
}

impl Classify for RefError {
    fn kind(&self) -> FailureKind {
        match self {
            Self::Locked { .. } => FailureKind::Locked,
            Self::Corrupt { .. } => FailureKind::Damaged,
            _ => FailureKind::Other,
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
            | Self::UnexpectedFile { .. } => FailureKind::Damaged,
            Self::Predecessor { source, .. } => source.kind(),
            _ => FailureKind::Other,
        }
    }
}

impl Classify for UpdateError {
    fn kind(&self) -> FailureKind {
        match self {
            Self::Refs(error) => error.kind(),
            Self::Log(error) => error.kind(),
            Self::MoveFailed { source, .. } | Self::PartlyMoved { source, .. } => source.kind(),
            _ => FailureKind::Other,
        }
    }
}

impl Classify for SnapshotError {
    fn kind(&self) -> FailureKind {
        match self {
            Self::Update(error) => error.kind(),
            Self::Refs(error) => error.kind(),
            _ => FailureKind::Other,
        }
    }
}

impl Classify for CommitError {
    fn kind(&self) -> FailureKind {
        match self {
            Self::Snapshot(error) | Self::Tip(error) => error.kind(),
            Self::Refs(error) => error.kind(),
            Self::Update(error) => error.kind(),
            Self::Log(error) => error.kind(),
            Self::AfterSnapshot { source, .. } => source.kind(),
            _ => FailureKind::Other,
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
            _ => FailureKind::Other,
        }
    }
}

impl Classify for TrackError {
    fn kind(&self) -> FailureKind {
        match self {
            Self::Snapshot(error) => error.kind(),
            _ => FailureKind::Other,
        }
    }
}

impl Classify for DiffError {
    fn kind(&self) -> FailureKind {
        match self {
            Self::Snapshot(error) => error.kind(),
            Self::Refs(error) => error.kind(),
            Self::Damaged { .. } => FailureKind::Damaged,
            Self::AfterSnapshot { source, .. } => source.kind(),
            _ => FailureKind::Other,
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
            Self::MoveFailed { source, .. } | Self::PartlyMoved { source, .. } => source.kind(),
            _ => FailureKind::Other,
        }
    }
}
