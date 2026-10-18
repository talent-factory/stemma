//! Stemma core: actors and the authorization hook (ADR-002), content-addressed objects, the
//! index, and object storage.

mod actor;
pub mod index;
pub mod object;
pub mod store;

pub use actor::{Actor, ActorError, ActorRef};

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
