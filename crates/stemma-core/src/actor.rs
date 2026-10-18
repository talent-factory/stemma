//! Actors: who performs an operation (ADR-002).
//!
//! An [`Actor`] is a human or an agent. It can only be built through [`Actor::human`] and
//! [`Actor::agent`], which check every field, so an actor that exists is valid. Neither the
//! authorization hook nor the commit encoding ever sees an invalid actor. The rules for the fields
//! are recorded in ADR-005.

use std::error::Error;
use std::fmt;

/// Actor of an operation. Agents carry model version, session, and sponsor.
///
/// Every field is non-empty visible ASCII (U+0021 to U+007E, no space). The fields are
/// identifiers, not display names. Whitespace and control characters would make the line-based
/// commit encoding ambiguous; invisible characters and look-alike letters from other scripts
/// would let identities be spoofed. Look-alikes within ASCII, such as `l` and `I`, remain possible.
/// All non-ASCII text is rejected, including harmless letters such as `ä`; an identifier derived
/// from a person's name has to be transliterated (ADR-005).
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
///
/// The fields of this type are public, so a value of it is not known to be valid. Only the result
/// of [`Actor::view`] is: do not take an `ActorRef` as proof that an actor was checked.
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
    /// Returns [`ActorError::InvalidField`] if the identifier is empty or contains a character
    /// outside U+0021 to U+007E (see [`Actor`]).
    pub fn human(id: impl Into<String>) -> Result<Self, ActorError> {
        Self::checked(Repr::Human { id: id.into() })
    }

    /// Creates an agent.
    ///
    /// # Errors
    ///
    /// Returns [`ActorError::InvalidField`] for the first field, in the order of the parameters,
    /// that is empty or contains a character outside U+0021 to U+007E (see [`Actor`]).
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

    fn rejected_field(actor: Result<Actor, ActorError>) -> Option<&'static str> {
        match actor {
            Err(ActorError::InvalidField(field)) => Some(field),
            Ok(_) => None,
        }
    }

    /// Asserts that the human is rejected because of the given field.
    #[track_caller]
    fn assert_human_rejected(id: &str, field: &str) {
        assert_eq!(
            rejected_field(Actor::human(id)),
            Some(field),
            "human id {id:?}"
        );
    }

    /// Asserts that the agent is rejected because of the given field.
    #[track_caller]
    fn assert_agent_rejected(id: &str, model: &str, session: &str, sponsor: &str, field: &str) {
        assert_eq!(
            rejected_field(Actor::agent(id, model, session, sponsor)),
            Some(field),
            "agent {id:?} {model:?} {session:?} {sponsor:?}"
        );
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
    fn a_human_id_that_is_not_visible_ascii_is_rejected() {
        for id in [
            "",
            "x\n",
            "x\n\nfake",
            "two words",
            "a\u{7f}",
            "a\u{85}",
            "a\u{a0}b",
            "a\u{2028}b",
            "a\u{2003}b",
            "dan\u{200b}iel",
            "dani\u{202e}el",
            "d\u{e4}niel",
            // Cyrillic small a looks like the Latin one.
            "d\u{430}niel",
        ] {
            assert_human_rejected(id, "id");
        }
    }

    #[test]
    fn an_agent_field_that_is_not_visible_ascii_is_rejected_with_its_name() {
        assert_agent_rejected("a\0", "m", "s", "p", "id");
        assert_agent_rejected("a", "", "s", "p", "model");
        assert_agent_rejected("a", "m session=s", "t", "p", "model");
        assert_agent_rejected("a", "m", "", "p", "session");
        assert_agent_rejected("a", "m", "s sponsor=q", "p", "session");
        assert_agent_rejected("a", "m", "s", "", "sponsor");
        assert_agent_rejected("a", "m", "s", "p\r", "sponsor");
        assert_agent_rejected("a", "m", "s", "p\u{1f600}", "sponsor");
    }

    #[test]
    fn with_several_invalid_fields_the_first_in_field_order_is_reported() {
        // Chains of three and four invalid fields.
        assert_agent_rejected("a b", "m c", "", "p\n", "id");
        assert_agent_rejected("a", "m c", "", "p\n", "model");
        // Each pair of neighbouring and distant fields, with only those two invalid.
        assert_agent_rejected("a b", "m c", "s", "p", "id");
        assert_agent_rejected("a b", "m", "s t", "p", "id");
        assert_agent_rejected("a b", "m", "s", "p\n", "id");
        assert_agent_rejected("a", "m c", "s t", "p", "model");
        assert_agent_rejected("a", "m c", "s", "p\n", "model");
        assert_agent_rejected("a", "m", "s t", "p\n", "session");
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
        assert!(Actor::human(all.clone()).is_ok());
        assert!(Actor::agent(all.clone(), "!", "~", all).is_ok());
    }

    #[test]
    fn actors_that_differ_in_one_field_are_not_equal() {
        let base = Actor::agent("a", "m", "s", "p").unwrap();
        assert_eq!(base, base.clone());
        for other in [
            Actor::agent("b", "m", "s", "p").unwrap(),
            Actor::agent("a", "n", "s", "p").unwrap(),
            Actor::agent("a", "m", "t", "p").unwrap(),
            Actor::agent("a", "m", "s", "q").unwrap(),
        ] {
            assert_ne!(base, other);
        }
    }
}
