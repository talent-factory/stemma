//! Stemma core: actors and the authorization hook (ADR-002), content-addressed objects, the
//! index, and object storage.

pub mod index;
pub mod object;
pub mod store;

/// Actor of an operation. Agents carry model version, session, and sponsor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Actor {
    Human {
        id: String,
    },
    Agent {
        id: String,
        model: String,
        session: String,
        sponsor: String,
    },
}

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
        let human = Actor::Human {
            id: "daniel".into(),
        };
        let agent = Actor::Agent {
            id: "agent-1".into(),
            model: "claude-haiku-5-5".into(),
            session: "s1".into(),
            sponsor: "daniel".into(),
        };
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
