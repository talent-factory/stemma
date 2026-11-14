//! Stemma core: actors and the authorization hook (ADR-002), content-addressed objects, the
//! index, object storage, references (branches, tags, and HEAD, ADR-008), the repository (ADR-009),
//! the operation log (ADR-011), the exclusion file `.stemmaignore` (ADR-012), and the best common
//! ancestors of two commits ([`merge_base`]), conflicts as sums of trees ([`conflict`]), and the
//! merge of trees ([`merge`]), and the SSHSIG signatures of proofs ([`signature`], ADR-020).
//!
//! # Errors
//!
//! The message of an error is complete: it includes the message of its cause, so `to_string()` is
//! all that a log, a command line, or an agent needs. [`Error::source`](std::error::Error::source)
//! therefore returns the cause of the cause, the first link that the message does not print yet,
//! and never the cause itself. A program that walks the chain and prints every link shows each
//! message once. The cause itself stays available as a field of the variant, typed, for a caller
//! that wants to match on it. An error that wraps another one without adding anything to its
//! message, such as [`store::ReadError::Store`], is transparent in the same way.
//!
//! The usual split is another one: each error prints only its own text, and a program prints the
//! chain of causes. It was not chosen, because a complete `to_string()` is what tests, logs, and
//! agents read, and because no code has to know how to print a chain. The price is that a walk
//! of [`Error::source`](std::error::Error::source) does not lead to the typed cause, so
//! `downcast_ref::<std::io::Error>()` on a source finds nothing, and neither does the idiom
//! `root_cause()` of an error library. A program that wants the error of the file system, for
//! example to map [`std::io::ErrorKind`] to an exit code, asks the error:
//! [`store::StoreError::io_error`] for the store, and `store_error()` on [`store::ReadError`],
//! [`index::WriteTreeError`], [`refs::RefError`], [`history::HistoryError`], and
//! [`verify::VerifyError`] (the error of a check of an index, a tree, a commit, or a reference)
//! for the error of the store inside. [`repository::InitError`], [`repository::OpenError`], and
//! [`change::GenerateError`] have an `io_error()` for the error of the file system or of the
//! operating system, and so do [`oplog::OpLogError`] and [`repository::UpdateError`]. A program
//! that only reports an error prints it with `{}` and does not walk the chain.

mod actor;
mod allowed;
pub mod change;
pub mod conflict;
pub mod diff;
pub mod failure;
// Public only with the feature `fault-injection`, so that the tests of other crates can arm a
// guard; hidden from the documentation.
#[cfg(feature = "fault-injection")]
#[doc(hidden)]
pub mod fault;
#[cfg(not(feature = "fault-injection"))]
mod fault;
#[cfg(any(fuzzing, test))]
#[doc(hidden)]
pub mod fuzz;
pub mod history;
pub mod ignore;
pub mod index;
pub mod merge;
pub mod merge_base;
pub mod moved;
pub mod object;
pub mod oplog;
#[cfg(test)]
mod properties;
pub mod refs;
pub mod repository;
pub mod signature;
pub mod store;
#[cfg(test)]
mod testutil;
pub mod verify;

use oplog::OperationName;

/// The attribute of a file that is a reparse point of any kind.
#[cfg(any(windows, test))]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;

/// Whether a handle that was opened as a reparse point itself must be opened again in the
/// ordinary way: it is a file (`is_file` of its metadata, which is false for a symbolic link or a
/// junction) that carries the reparse attribute, so it is a reparse point that is no link.
#[cfg(any(windows, test))]
fn needs_ordinary_open(is_file: bool, attributes: u32) -> bool {
    is_file && attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// Opens a file for reading. On Windows the last part of the path is opened itself when it is a
/// reparse point, and what it names is not opened. A symbolic link or a junction is refused
/// either because the open fails (a junction is a directory) or because the handle describes the
/// link, which is not a file for `Metadata::is_file`, so that a reader that checks the handle
/// refuses a link that has taken the place of a file. Another reparse point is no link and holds
/// data that a filter driver supplies (a placeholder of a cloud service, a deduplicated file):
/// opened itself it would give the stub, so it is opened again in the ordinary way. A swap of the
/// file for a link between the first open and the second is not seen. A part above the last is
/// followed on every platform. Elsewhere this is `File::open`.
///
/// A file that is gone at the second open is not reported as missing: the first open found it, so
/// the caller must not take it for a file that was never there.
pub(crate) fn open_without_following(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        let mut options = std::fs::OpenOptions::new();
        options
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        let file = fault::open_with(&options, path)?;
        let opened = file.metadata().map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!("cannot inspect the opened file: {error}"),
            )
        })?;
        if needs_ordinary_open(opened.is_file(), opened.file_attributes()) {
            return fault::open(path).map_err(|error| {
                let kind = match error.kind() {
                    std::io::ErrorKind::NotFound => std::io::ErrorKind::Other,
                    kind => kind,
                };
                std::io::Error::new(
                    kind,
                    format!("cannot open the reparse point in the ordinary way: {error}"),
                )
            });
        }
        Ok(file)
    }
    #[cfg(not(windows))]
    {
        fault::open(path)
    }
}

#[cfg(test)]
mod open_tests {
    use super::*;

    #[test]
    fn only_a_file_with_the_reparse_attribute_is_opened_again() {
        // A cloud placeholder or a deduplicated file: a file, and a reparse point.
        assert!(needs_ordinary_open(true, FILE_ATTRIBUTE_REPARSE_POINT));
        assert!(needs_ordinary_open(
            true,
            FILE_ATTRIBUTE_REPARSE_POINT | 0x20
        ));
        // A plain file has nothing to open again.
        assert!(!needs_ordinary_open(true, 0x20));
        // A symbolic link or a junction is no file for `is_file`: it must never be opened again,
        // or the read would lead through it.
        assert!(!needs_ordinary_open(false, FILE_ATTRIBUTE_REPARSE_POINT));
        assert!(!needs_ordinary_open(false, 0));
    }

    #[test]
    fn a_missing_file_is_not_found() {
        let dir = crate::testutil::TempDir::new();
        let error = open_without_following(&dir.path().join("none")).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn a_plain_file_is_opened() {
        use std::io::Read;
        let dir = crate::testutil::TempDir::new();
        std::fs::write(dir.path().join("a"), "data").unwrap();
        let mut content = String::new();
        open_without_following(&dir.path().join("a"))
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert_eq!(content, "data");
    }

    #[cfg(windows)]
    #[test]
    fn a_symbolic_link_is_opened_as_a_link_and_not_as_a_file() {
        let dir = crate::testutil::TempDir::new();
        std::fs::write(dir.path().join("target"), "data").unwrap();
        let link = dir.path().join("link");
        if !crate::testutil::symbolic_link(&dir.path().join("target"), &link) {
            return;
        }
        let file = open_without_following(&link).unwrap();
        assert!(!file.metadata().unwrap().is_file());
    }
}

pub use actor::{Actor, ActorError, ActorField, ActorMalformed, ActorParseError, ActorRef};
pub use allowed::Allowed;

/// Result of an authorization check. Dropping a decision unread is a bug, so `#[must_use]`
/// makes it a warning.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Decision {
    Allow,
    Deny,
}

/// Authorization hook (ADR-002). Every operation of [`repository::Repository`] that changes the
/// repository, or plans a change, calls it; the commands that only read the log have no actor
/// and do not. Objects stored directly through `Repository::objects()` are not authorized.
///
/// `operation` is the name that the operation log records (`init`, `snapshot`, `commit`,
/// `purge`, ...). `target` is what the operation acts on: a branch name, `HEAD`, `working-copy`,
/// the directory for `init`, the blob for `purge`, or `entry N` for `restore`.
pub trait Authorizer {
    fn authorize(&self, actor: &Actor, operation: &OperationName, target: &str) -> Decision;
}

/// Phase 1: everything is allowed. The call is mandatory nonetheless.
pub struct AllowAll;

impl Authorizer for AllowAll {
    fn authorize(&self, _actor: &Actor, _operation: &OperationName, _target: &str) -> Decision {
        Decision::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_all_allows_human_and_agent() {
        let human = Actor::human("daniel").unwrap();
        let agent = Actor::agent("agent-1", "claude-haiku-5-5", "s1", "daniel").unwrap();
        assert_eq!(
            AllowAll.authorize(&human, &OperationName::try_new("commit").unwrap(), "main"),
            Decision::Allow
        );
        assert_eq!(
            AllowAll.authorize(
                &agent,
                &OperationName::try_new("commit").unwrap(),
                "feature/x"
            ),
            Decision::Allow
        );
    }
}
