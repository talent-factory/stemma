//! Actors: who performs an operation (ADR-002).
//!
//! An [`Actor`] is a human or an agent. It can only be built through [`Actor::human`] and
//! [`Actor::agent`], which check every field, so an actor that exists is valid. The authorization
//! hook and the commit encoding never see a value that a commit would reject. The rules for the
//! fields are recorded in ADR-005.

use std::error::Error;
use std::fmt;

/// Actor of an operation. Agents carry model version, session, and sponsor.
///
/// Every field is non-empty visible ASCII (U+0021 to U+007E, no space). The fields are
/// identifiers, not display names. Whitespace and control characters would make the line-based
/// commit encoding ambiguous; invisible characters and look-alike letters from other scripts
/// would let identities be spoofed. Look-alikes within ASCII, such as `l` and `I`, remain possible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor(Repr);

#[derive(Debug, Clone, PartialEq, Eq)]
enum Repr {
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

/// A view of the fields of an [`Actor`], for matching on the kind of actor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorRef<'a> {
    /// A human.
    Human {
        /// The identifier of the human.
        id: &'a str,
    },
    /// An agent that acts on behalf of a sponsor.
    Agent {
        /// The identifier of the agent.
        id: &'a str,
        /// The model version.
        model: &'a str,
        /// The session of the agent.
        session: &'a str,
        /// The human who is accountable for the agent (ADR-002).
        sponsor: &'a str,
    },
}

/// Why an actor is rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ActorError {
    /// A field is empty or contains a character outside visible ASCII (U+0021 to U+007E, no
    /// space). Carries the name of the field: `id`, `model`, `session`, or `sponsor`. With several
    /// invalid fields, the first in this order is reported.
    InvalidField(&'static str),
}

impl fmt::Display for ActorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidField(field) => write!(
                f,
                "actor field `{field}` must be non-empty visible ASCII (U+0021 to U+007E)"
            ),
        }
    }
}

impl Error for ActorError {}

impl Actor {
    /// Creates a human.
    ///
    /// # Errors
    ///
    /// Returns [`ActorError::InvalidField`] if the identifier is empty or not visible ASCII.
    pub fn human(id: impl Into<String>) -> Result<Self, ActorError> {
        Self::checked(Repr::Human { id: id.into() })
    }

    /// Creates an agent.
    ///
    /// # Errors
    ///
    /// Returns [`ActorError::InvalidField`] for the first field, in the order of the parameters,
    /// that is empty or not visible ASCII.
    pub fn agent(
        id: impl Into<String>,
        model: impl Into<String>,
        session: impl Into<String>,
        sponsor: impl Into<String>,
    ) -> Result<Self, ActorError> {
        Self::checked(Repr::Agent {
            id: id.into(),
            model: model.into(),
            session: session.into(),
            sponsor: sponsor.into(),
        })
    }

    fn checked(repr: Repr) -> Result<Self, ActorError> {
        if let Some(field) = invalid_field(&repr) {
            return Err(ActorError::InvalidField(field));
        }
        Ok(Self(repr))
    }

    /// Returns the fields, for matching on the kind of actor.
    pub fn view(&self) -> ActorRef<'_> {
        match &self.0 {
            Repr::Human { id } => ActorRef::Human { id },
            Repr::Agent {
                id,
                model,
                session,
                sponsor,
            } => ActorRef::Agent {
                id,
                model,
                session,
                sponsor,
            },
        }
    }

    /// Returns the label and the fields of the actor, the identifier first. This is the single
    /// list from which both the check of the fields and the commit encoding work, so a new field
    /// is always checked and always encoded.
    pub(crate) fn fields(&self) -> (&'static str, Vec<(&'static str, &str)>) {
        repr_fields(&self.0)
    }
}

fn repr_fields(repr: &Repr) -> (&'static str, Vec<(&'static str, &str)>) {
    match repr {
        Repr::Human { id } => ("human", vec![("id", id)]),
        Repr::Agent {
            id,
            model,
            session,
            sponsor,
        } => (
            "agent",
            vec![
                ("id", id),
                ("model", model),
                ("session", session),
                ("sponsor", sponsor),
            ],
        ),
    }
}

/// Returns the name of the first field that is empty or not visible ASCII (U+0021 to U+007E).
fn invalid_field(repr: &Repr) -> Option<&'static str> {
    repr_fields(repr)
        .1
        .into_iter()
        .find(|(_, value)| value.is_empty() || !value.chars().all(|c| c.is_ascii_graphic()))
        .map(|(field, _)| field)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invalid_agent(id: &str, model: &str, session: &str, sponsor: &str) -> Option<&'static str> {
        match Actor::agent(id, model, session, sponsor) {
            Err(ActorError::InvalidField(field)) => Some(field),
            Ok(_) => None,
        }
    }

    fn invalid_human(id: &str) -> Option<&'static str> {
        match Actor::human(id) {
            Err(ActorError::InvalidField(field)) => Some(field),
            Ok(_) => None,
        }
    }

    #[test]
    fn a_human_and_an_agent_keep_their_fields_in_the_view() {
        let human = Actor::human("daniel").unwrap();
        assert_eq!(human.view(), ActorRef::Human { id: "daniel" });
        let agent = Actor::agent("agent-1", "claude-haiku-5-5", "s1", "daniel").unwrap();
        assert_eq!(
            agent.view(),
            ActorRef::Agent {
                id: "agent-1",
                model: "claude-haiku-5-5",
                session: "s1",
                sponsor: "daniel",
            }
        );
    }

    #[test]
    fn a_human_and_an_agent_with_the_same_id_differ() {
        assert_ne!(
            Actor::human("x").unwrap(),
            Actor::agent("x", "m", "s", "p").unwrap()
        );
    }

    #[test]
    fn fields_that_are_not_visible_ascii_are_rejected_with_the_name_of_the_field() {
        let cases = [
            (invalid_human(""), "id"),
            (invalid_human("x\n"), "id"),
            (invalid_human("x\n\nfake"), "id"),
            (invalid_human("two words"), "id"),
            (invalid_human("a\u{7f}"), "id"),
            (invalid_human("a\u{85}"), "id"),
            (invalid_human("a\u{a0}b"), "id"),
            (invalid_human("a\u{2028}b"), "id"),
            (invalid_human("a\u{2003}b"), "id"),
            (invalid_human("dan\u{200b}iel"), "id"),
            (invalid_human("dani\u{202e}el"), "id"),
            (invalid_human("d\u{e4}niel"), "id"),
            // Cyrillic small a looks like the Latin one.
            (invalid_human("d\u{430}niel"), "id"),
            (invalid_agent("a", "m session=s", "t", "p"), "model"),
            (invalid_agent("a", "m", "s sponsor=q", "p"), "session"),
            (invalid_agent("a", "m", "s", "p\r"), "sponsor"),
            (invalid_agent("a", "m", "s", ""), "sponsor"),
            (invalid_agent("a\0", "m", "s", "p"), "id"),
            (invalid_agent("a", "", "s", "p"), "model"),
            (invalid_agent("a", "m", "", "p"), "session"),
            (invalid_agent("a", "m", "s", "p\u{1f600}"), "sponsor"),
            // With several invalid fields, the first in field order is reported.
            (invalid_agent("a b", "m c", "", "p\n"), "id"),
            (invalid_agent("a", "m c", "", "p\n"), "model"),
        ];
        for (found, expected) in cases {
            assert_eq!(found, Some(expected));
        }
    }

    #[test]
    fn the_error_names_the_field_and_the_rule() {
        let error = Actor::human("").unwrap_err();
        assert_eq!(error, ActorError::InvalidField("id"));
        assert_eq!(
            error.to_string(),
            "actor field `id` must be non-empty visible ASCII (U+0021 to U+007E)"
        );
    }

    #[test]
    fn fields_accept_the_whole_visible_ascii_range() {
        let all: String = ('\u{21}'..='\u{7e}').collect();
        assert!(Actor::agent(all.clone(), "!", "~", all).is_ok());
    }
}
