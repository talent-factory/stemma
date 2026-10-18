//! Kern von Stemma: Akteure und Autorisierungs-Hook (ADR-002).

/// Akteur einer Operation. Agenten führen Modellversion, Session und Sponsor mit.
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

/// Ergebnis einer Autorisierungsprüfung.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
}

/// Autorisierungs-Hook. Jede Operation im Kern ruft ihn auf (ADR-002).
pub trait Authorizer {
    fn authorize(&self, actor: &Actor, operation: &str, target: &str) -> Decision;
}

/// Phase 1: alles ist erlaubt. Der Aufruf ist trotzdem Pflicht.
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
