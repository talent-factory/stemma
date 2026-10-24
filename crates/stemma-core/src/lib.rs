//! Stemma core: actors and the authorization hook (ADR-002), content-addressed objects, the
//! index, object storage, references (branches and HEAD, ADR-008), the repository (ADR-009), and
//! the operation log (ADR-011), and the exclusion file `.stemmaignore` (ADR-012).
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
//! [`index::WriteTreeError`], [`refs::RefError`], and [`verify::VerifyError`] (the error of a
//! check of an index, a tree, a commit, or a reference) for the error of the store
//! inside. [`repository::InitError`], [`repository::OpenError`], and [`change::GenerateError`]
//! have an `io_error()` for the error of the file system or of the operating system, and so do
//! [`oplog::OpLogError`] and [`repository::UpdateError`]. A program
//! that only reports an error prints it with `{}` and does not walk the chain.

mod actor;
pub mod change;
#[cfg(any(fuzzing, test))]
#[doc(hidden)]
pub mod fuzz;
pub mod ignore;
pub mod index;
pub mod object;
pub mod oplog;
#[cfg(test)]
mod properties;
pub mod refs;
pub mod repository;
pub mod store;
#[cfg(test)]
mod testutil;
pub mod verify;

pub use actor::{Actor, ActorError, ActorField, ActorMalformed, ActorParseError, ActorRef};

/// Result of an authorization check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
}

/// Authorization hook. Every operation of the core that writes calls it (ADR-002); one that only
/// reads has no actor and does not.
pub trait Authorizer {
    fn authorize(&self, actor: &Actor, operation: &str, target: &str) -> Decision;
}

/// Phase 1: everything is allowed. The call is mandatory nonetheless.
pub struct AllowAll;

impl Authorizer for AllowAll {
    fn authorize(&self, _actor: &Actor, _operation: &str, _target: &str) -> Decision {
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
            AllowAll.authorize(&human, "commit", "main"),
            Decision::Allow
        );
        assert_eq!(
            AllowAll.authorize(&agent, "commit", "feature/x"),
            Decision::Allow
        );
    }
}
