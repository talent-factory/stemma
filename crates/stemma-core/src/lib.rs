//! Stemma core: actors and the authorization hook (ADR-002), content-addressed objects, the
//! index, and object storage.
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

mod actor;
pub mod index;
pub mod object;
#[cfg(test)]
mod properties;
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

/// Authorization hook. Every operation in the core calls it (ADR-002).
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
