//! Actors: who performs an operation (ADR-002).
//!
//! An [`Actor`] is a human or an agent. It can only be built through [`Actor::human`] and
//! [`Actor::agent`], which check every field, so an actor that exists is valid. Neither the
//! authorization hook nor the commit encoding ever sees an invalid actor. The rules for the fields
//! are recorded in ADR-005.

use std::error::Error;
use std::fmt::{self, Write as _};
use std::str::FromStr;

/// Actor of an operation. Agents carry model version, session, and sponsor.
///
/// Every field is non-empty visible ASCII (U+0021 to U+007E, no space). The fields are
/// identifiers, not display names. Whitespace and control characters would make the line-based
/// commit encoding ambiguous; invisible characters and look-alike letters from other scripts
/// would let identities be spoofed. Look-alikes within ASCII, such as `l` and `I`, remain possible.
/// All non-ASCII text is rejected, including harmless letters such as `ä`; an identifier derived
/// from a person's name has to be transliterated (ADR-005).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Actor(Repr);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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

/// A field of an [`Actor`], in the order of the actor line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ActorField {
    /// The identifier of a human or an agent.
    Id,
    /// The model version of an agent.
    Model,
    /// The session of an agent.
    Session,
    /// The sponsor of an agent.
    Sponsor,
}

impl ActorField {
    /// The name of the field, as it is used in an actor line and in messages.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::Model => "model",
            Self::Session => "session",
            Self::Sponsor => "sponsor",
        }
    }
}

impl fmt::Display for ActorField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Why an actor is rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ActorError {
    /// A field is empty or contains a character outside visible ASCII (U+0021 to U+007E, no
    /// space). Carries the field. With several invalid fields, the first in the order of
    /// [`ActorField`] is reported.
    InvalidField(ActorField),
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

/// Why the actor line of a commit is rejected by [`Actor::from_str`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ActorParseError {
    /// The line does not have the form of an actor line. Carries what is wrong with it. The form is
    /// checked before the fields are; see the implementation of [`FromStr`] for [`Actor`].
    Malformed(&'static str),
    /// The line has the right form, but a field is rejected.
    Invalid(ActorError),
}

impl fmt::Display for ActorParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(reason) => write!(f, "malformed actor line: {reason}"),
            Self::Invalid(source) => write!(f, "{source}"),
        }
    }
}

impl Error for ActorParseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Invalid(source) => Some(source),
            Self::Malformed(_) => None,
        }
    }
}

/// Reads the actor line of a commit: `human <id>` or
/// `agent <id> model=<model> session=<session> sponsor=<sponsor>`, with single spaces between the
/// tokens and the keys in this order. A value may contain `=`; each `key=value` token is split at
/// its first `=`. Every field is checked again by [`Actor::human`] or [`Actor::agent`], so a line
/// read from storage is as strict as an actor that is built in code. The result encodes back to
/// exactly the line it was read from.
///
/// The form is checked before the fields: the first token, the number of tokens, and the three
/// keys, in this order. Only a line of the right form can fail with
/// [`ActorParseError::Invalid`], for the first field, in the order of the line, that is rejected.
impl FromStr for Actor {
    type Err = ActorParseError;

    fn from_str(line: &str) -> Result<Self, Self::Err> {
        let tokens: Vec<&str> = line.split(' ').collect();
        match tokens.as_slice() {
            ["human", id] => Actor::human(*id).map_err(ActorParseError::Invalid),
            ["agent", id, model, session, sponsor] => {
                let model = token_value(
                    model,
                    "model=",
                    "the third token of an agent line is `model=<model>`",
                )?;
                let session = token_value(
                    session,
                    "session=",
                    "the fourth token of an agent line is `session=<session>`",
                )?;
                let sponsor = token_value(
                    sponsor,
                    "sponsor=",
                    "the fifth token of an agent line is `sponsor=<sponsor>`",
                )?;
                Actor::agent(*id, model, session, sponsor).map_err(ActorParseError::Invalid)
            }
            ["human", ..] => Err(ActorParseError::Malformed(
                "a human line is `human <id>` with one space",
            )),
            ["agent", ..] => Err(ActorParseError::Malformed(
                "an agent line is `agent <id> model=<model> session=<session> sponsor=<sponsor>` \
                 with single spaces",
            )),
            _ => Err(ActorParseError::Malformed(
                "an actor line starts with `human` or `agent`",
            )),
        }
    }
}

/// Writes the actor line of a commit: `human <id>` or
/// `agent <id> model=<model> session=<session> sponsor=<sponsor>`. This is the form that
/// [`Actor::from_str`] reads back.
///
/// The fields are not escaped. An [`Actor`] guarantees that no field is empty or contains a space,
/// so the line splits unambiguously at spaces. A value may still contain `=`; the reader splits
/// each `key=value` token at its first `=`.
impl fmt::Display for Actor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (label, fields) = self.0.fields();
        let mut line = String::from(label);
        for (field, value) in fields {
            match field {
                ActorField::Id => write!(line, " {value}")?,
                _ => write!(line, " {field}={value}")?,
            }
        }
        f.write_str(&line)
    }
}

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
        match repr.invalid_field() {
            Some(field) => Err(ActorError::InvalidField(field)),
            None => Ok(Self(repr)),
        }
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
}

/// Returns the part of the token after `key`, or [`ActorParseError::Malformed`] with the reason if
/// the token does not start with `key`. The keys contain no `=`, so a value may contain one: the
/// token is split at its first `=`.
fn token_value<'a>(
    token: &'a str,
    key: &str,
    reason: &'static str,
) -> Result<&'a str, ActorParseError> {
    token
        .strip_prefix(key)
        .ok_or(ActorParseError::Malformed(reason))
}

impl Repr {
    /// Returns the label and the fields, the identifier first. This is the single list from which
    /// both the check of the fields and the actor line work, so a new field is always checked and
    /// always written.
    fn fields(&self) -> (&'static str, Vec<(ActorField, &str)>) {
        match self {
            Self::Human { id } => ("human", vec![(ActorField::Id, id)]),
            Self::Agent {
                id,
                model,
                session,
                sponsor,
            } => (
                "agent",
                vec![
                    (ActorField::Id, id),
                    (ActorField::Model, model),
                    (ActorField::Session, session),
                    (ActorField::Sponsor, sponsor),
                ],
            ),
        }
    }

    /// Returns the first field that is empty or not visible ASCII (U+0021 to U+007E).
    fn invalid_field(&self) -> Option<ActorField> {
        self.fields()
            .1
            .into_iter()
            .find(|(_, value)| value.is_empty() || !value.chars().all(|c| c.is_ascii_graphic()))
            .map(|(field, _)| field)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rejected_field(actor: Result<Actor, ActorError>) -> Option<&'static str> {
        match actor {
            Err(ActorError::InvalidField(field)) => Some(field.name()),
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
    fn the_field_names_are_those_of_the_actor_line() {
        let names: Vec<_> = [
            ActorField::Id,
            ActorField::Model,
            ActorField::Session,
            ActorField::Sponsor,
        ]
        .into_iter()
        .map(|field| (field.name(), field.to_string()))
        .collect();
        assert_eq!(
            names,
            [
                ("id", "id".to_owned()),
                ("model", "model".to_owned()),
                ("session", "session".to_owned()),
                ("sponsor", "sponsor".to_owned()),
            ]
        );
    }

    #[test]
    fn the_display_form_is_the_actor_line_and_reads_back() {
        let human = Actor::human("daniel").unwrap();
        assert_eq!(human.to_string(), "human daniel");
        let agent = Actor::agent("agent-1", "claude-haiku-5-5", "s=1", "daniel").unwrap();
        assert_eq!(
            agent.to_string(),
            "agent agent-1 model=claude-haiku-5-5 session=s=1 sponsor=daniel"
        );
        for actor in [human, agent] {
            assert_eq!(actor.to_string().parse::<Actor>().as_ref(), Ok(&actor));
        }
    }

    #[test]
    fn equal_actors_hash_alike_and_different_ones_are_kept_apart() {
        use std::collections::HashSet;
        let set: HashSet<Actor> = [
            Actor::human("x").unwrap(),
            Actor::human("x").unwrap(),
            Actor::agent("x", "m", "s", "p").unwrap(),
        ]
        .into_iter()
        .collect();
        assert_eq!(set.len(), 2);
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
        assert_eq!(error, ActorError::InvalidField(ActorField::Id));
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

    #[test]
    fn an_actor_line_with_the_wrong_form_is_malformed() {
        for line in [
            "",
            " ",
            "human",
            "human  x",
            "human a b",
            "Human x",
            "robot x",
            "agent",
            "agent a",
            "agent a model=m session=s",
            "agent a model=m session=s sponsor=p extra",
            "agent a  model=m session=s sponsor=p",
            "agent a session=s model=m sponsor=p",
            "agent a model m session=s sponsor=p",
            "agent a Model=m session=s sponsor=p",
            " human x",
            "human x ",
        ] {
            assert!(
                matches!(line.parse::<Actor>(), Err(ActorParseError::Malformed(_))),
                "line {line:?}"
            );
        }
    }

    #[test]
    fn an_actor_line_with_a_rejected_field_names_the_field() {
        for (line, field) in [
            ("human \u{e4}", ActorField::Id),
            ("human x\n", ActorField::Id),
            ("agent a model= session=s sponsor=p", ActorField::Model),
            ("agent a model=m session= sponsor=p", ActorField::Session),
            ("agent a model=m session=s sponsor=", ActorField::Sponsor),
            ("agent a model=m session=s sponsor=p\r", ActorField::Sponsor),
            (
                "agent a model=m session=s\u{a0} sponsor=p",
                ActorField::Session,
            ),
        ] {
            assert_eq!(
                line.parse::<Actor>(),
                Err(ActorParseError::Invalid(ActorError::InvalidField(field))),
                "line {line:?}"
            );
        }
    }

    #[test]
    fn a_value_keeps_everything_after_the_first_equals_sign() {
        let actor: Actor = "agent a=b model=m=1 session==s sponsor=p=".parse().unwrap();
        assert_eq!(
            actor.view(),
            ActorRef::Agent {
                id: "a=b",
                model: "m=1",
                session: "=s",
                sponsor: "p=",
            }
        );
    }

    #[test]
    fn an_actor_parse_error_names_the_form_or_the_field() {
        let error = "human".parse::<Actor>().unwrap_err();
        assert!(
            error.to_string().starts_with("malformed actor line:"),
            "{error}"
        );
        assert!(std::error::Error::source(&error).is_none());
        let error = "human ".parse::<Actor>().unwrap_err();
        assert_eq!(
            error.to_string(),
            "actor field `id` must be non-empty visible ASCII (U+0021 to U+007E)"
        );
        assert!(std::error::Error::source(&error).is_some());
    }

    #[test]
    fn the_reasons_of_a_malformed_actor_line_are_stable() {
        for (line, reason) in [
            ("human a b", "a human line is `human <id>` with one space"),
            (
                "agent a model=m session=s",
                "an agent line is `agent <id> model=<model> session=<session> sponsor=<sponsor>` \
                 with single spaces",
            ),
            ("robot x", "an actor line starts with `human` or `agent`"),
            (
                "agent a session=s model=m sponsor=p",
                "the third token of an agent line is `model=<model>`",
            ),
            (
                "agent a model=m sessio=s sponsor=p",
                "the fourth token of an agent line is `session=<session>`",
            ),
            (
                "agent a model=m session=s sponsor",
                "the fifth token of an agent line is `sponsor=<sponsor>`",
            ),
        ] {
            assert_eq!(
                line.parse::<Actor>().unwrap_err().to_string(),
                format!("malformed actor line: {reason}"),
                "line {line:?}"
            );
        }
    }

    #[test]
    fn the_form_of_an_actor_line_is_checked_before_its_fields() {
        // Each line has a field that is rejected and a form that is wrong; the form wins.
        for line in [
            "human \u{e4} extra",
            "agent \u{e4} session=s model=m sponsor=p",
            "agent a model=\u{e4} sessio=s sponsor=p",
            "agent \u{e4} model=m session=s",
        ] {
            assert!(
                matches!(line.parse::<Actor>(), Err(ActorParseError::Malformed(_))),
                "line {line:?}"
            );
        }
        // With the right form the field is rejected, also the identifier of an agent.
        assert_eq!(
            "agent \u{e4} model=m session=s sponsor=p".parse::<Actor>(),
            Err(ActorParseError::Invalid(ActorError::InvalidField(
                ActorField::Id
            )))
        );
    }
}
