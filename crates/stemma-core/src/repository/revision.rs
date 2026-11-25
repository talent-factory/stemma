//! Naming a commit: the rule for a name that is a branch and a tag (ADR-008, STEMMA-186).
//!
//! The rule lives in this layer, the one that resolves the text of a command to a commit. The ref
//! store (`RefStore`, `FsRefStore`) stores and does not decide: it keeps its name rules, and a
//! branch or a tag called `heads/x` is a valid name for it.
//!
//! The refusal of a new name that starts with `heads/`, `tags/`, or `remotes/`
//! ([`require_unreserved`]) is
//! applied by the entry points of the core that make a name (`create_branch` and `update_branch` for a new branch, `create_tag`,
//! `rename_branch`, `switch`, and `add_worktree`), so a caller need not call it.
//!
//! [`Revision::from_str`](std::str::FromStr) reads the text in this order:
//!
//! 1. `b3:` and 64 hexadecimal digits are a commit ID.
//! 2. `heads/<name>` is the branch and `tags/<name>` is the tag. The qualified forms are never
//!    looked up as anything else: a branch or a tag that does not exist is an unknown name.
//!    `remotes/<remote>/heads/<name>` and `remotes/<remote>/tags/<name>` are the remote
//!    references (STEMMA-285); text that starts with `remotes/` and has another shape is refused.
//! 3. Any other text is a [`Revision::Name`]: it is looked up as a branch and as a tag. If exactly
//!    one exists, it is the answer; if both exist, the lookup fails with
//!    [`NameError::Ambiguous`]; if none exists, with [`NameError::Unknown`].

use std::error::Error;
use std::fmt;
use std::str::FromStr;

use super::Repository;
use crate::object::{CommitId, ObjectId, ObjectIdParseError};
use crate::refs::{BranchName, BranchNameError, RefError};
use crate::remote::{RemoteNameError, RemoteRefName, RemoteRefNameError};

/// The prefix of the qualified form of a branch.
pub const BRANCH_PREFIX: &str = "heads/";

/// The prefix of the qualified form of a tag.
pub const TAG_PREFIX: &str = "tags/";

/// The prefix of the qualified form of a remote reference, `remotes/<remote>/heads/<name>` or
/// `remotes/<remote>/tags/<name>`.
pub const REMOTE_PREFIX: &str = "remotes/";

/// A name for a commit in the arguments of a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Revision {
    /// The commit at the tip of the branch, and only a branch: the qualified form `heads/<name>`.
    Branch(BranchName),
    /// The commit of the tag, and only a tag (`tags/<name>`). A tag name follows the rules of a
    /// branch name (ADR-008), so it is a [`BranchName`].
    Tag(BranchName),
    /// The commit of a remote reference: a branch or a tag of a hub as the clone last saw it
    /// (`remotes/<remote>/heads/<name>`, `remotes/<remote>/tags/<name>`).
    Remote(RemoteRefName),
    /// A name that is looked up as a branch and as a tag; a name that is both is an error.
    Name(BranchName),
    /// A commit. The ID is a claim until the store confirms it.
    Commit(ObjectId),
}

impl fmt::Display for Revision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Branch(name) | Self::Name(name) => name.fmt(f),
            Self::Tag(name) => write!(f, "{TAG_PREFIX}{name}"),
            Self::Remote(name) => name.fmt(f),
            Self::Commit(id) => id.fmt(f),
        }
    }
}

/// Why the text of a revision cannot be read.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RevisionParseError {
    /// The text starts with `b3:` and is no commit ID.
    CommitId(ObjectIdParseError),
    /// The text, or the part behind a prefix, is no valid name.
    Name(BranchNameError),
    /// The remote in `remotes/<remote>/...` has no valid name.
    Remote(RemoteNameError),
    /// The text starts with `remotes/` and is neither `remotes/<remote>/heads/<name>` nor
    /// `remotes/<remote>/tags/<name>`. Carries the text.
    RemoteShape(String),
}

impl fmt::Display for RevisionParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CommitId(error) => write!(f, "invalid commit ID: {error}"),
            Self::Name(error) => error.fmt(f),
            Self::Remote(error) => error.fmt(f),
            Self::RemoteShape(text) => write!(
                f,
                "`{}` names no remote reference: write `{REMOTE_PREFIX}<remote>/heads/<name>` \
                 or `{REMOTE_PREFIX}<remote>/tags/<name>`",
                text.escape_debug()
            ),
        }
    }
}

impl Error for RevisionParseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CommitId(error) => Some(error),
            Self::Name(error) => Some(error),
            Self::Remote(error) => Some(error),
            Self::RemoteShape(_) => None,
        }
    }
}

impl FromStr for Revision {
    type Err = RevisionParseError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.starts_with("b3:") {
            return text
                .parse::<ObjectId>()
                .map(Self::Commit)
                .map_err(RevisionParseError::CommitId);
        }
        let name = |text: &str| text.parse().map_err(RevisionParseError::Name);
        if let Some(rest) = text.strip_prefix(BRANCH_PREFIX) {
            name(rest).map(Self::Branch)
        } else if let Some(rest) = text.strip_prefix(TAG_PREFIX) {
            name(rest).map(Self::Tag)
        } else if text.starts_with(REMOTE_PREFIX) {
            text.parse::<RemoteRefName>()
                .map(Self::Remote)
                .map_err(|error| match error {
                    RemoteRefNameError::Remote(error) => RevisionParseError::Remote(error),
                    RemoteRefNameError::Name(error) => RevisionParseError::Name(error),
                    _ => RevisionParseError::RemoteShape(text.to_owned()),
                })
        } else {
            name(text).map(Self::Name)
        }
    }
}

/// Why a name does not lead to a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum NameError {
    /// `tags/<name>`, and there is no such tag.
    NoSuchTag(BranchName),
    /// `remotes/<remote>/heads/<name>` or `remotes/<remote>/tags/<name>`, and the clone holds
    /// no such remote reference.
    NoSuchRemoteReference(RemoteRefName),
    /// A name that is neither a branch nor a tag.
    Unknown(BranchName),
    /// A name that is a branch and a tag. The command does not pick one.
    Ambiguous(BranchName),
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSuchTag(name) => write!(f, "there is no tag `{name}`"),
            Self::NoSuchRemoteReference(name) => {
                write!(f, "there is no remote reference `{name}`")
            }
            Self::Unknown(name) => write!(f, "there is neither a branch nor a tag `{name}`"),
            Self::Ambiguous(name) => write!(
                f,
                "`{name}` is a branch and a tag; write `{BRANCH_PREFIX}{name}` for the branch or \
                 `{TAG_PREFIX}{name}` for the tag"
            ),
        }
    }
}

impl Error for NameError {}

/// The name of a new branch or tag starts with a prefix that the qualified forms use.
///
/// Such a name would be unreachable by its plain name, and `heads/x` could mean two things. A
/// branch or a tag that has such a name already is not rewritten (ADR-008); it is reached by its
/// own qualified form, such as `heads/remotes/x`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservedPrefix {
    /// The refused name, as text: with the prefix of a qualified form it may be longer than a
    /// branch name can be.
    pub name: String,
    /// The prefix that it starts with.
    pub prefix: &'static str,
}

impl fmt::Display for ReservedPrefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the name `{}` starts with `{}`, which is reserved for the qualified forms \
             `{BRANCH_PREFIX}<name>`, `{TAG_PREFIX}<name>`, and \
             `{REMOTE_PREFIX}<remote>/...`",
            self.name, self.prefix
        )
    }
}

impl Error for ReservedPrefix {}

/// Refuses the name of a new branch or tag that starts with `heads/`, `tags/`, or `remotes/`.
///
/// # Errors
///
/// Returns [`ReservedPrefix`] for such a name.
pub fn require_unreserved(name: BranchName) -> Result<BranchName, ReservedPrefix> {
    match [BRANCH_PREFIX, TAG_PREFIX, REMOTE_PREFIX]
        .into_iter()
        .find(|prefix| name.as_str().starts_with(prefix))
    {
        Some(prefix) => Err(ReservedPrefix {
            name: name.as_str().to_owned(),
            prefix,
        }),
        None => Ok(name),
    }
}

impl Repository {
    /// The commit that a branch, a tag, a remote reference, or a plain name leads to. A [`Revision::Commit`] is the
    /// business of the caller, which needs the store: it comes back as [`Missing::Commit`].
    ///
    /// The inner `Err` is a name that does not lead to one commit. A missing branch is told apart
    /// from the rest, so that a caller keeps the error of its own for it.
    pub(super) fn named_commit(
        &self,
        revision: &Revision,
    ) -> Result<Result<CommitId, Missing>, RefError> {
        let refs = self.refs();
        Ok(match revision {
            Revision::Branch(name) => refs.branch(name)?.ok_or(Missing::Branch(name.clone())),
            Revision::Tag(name) => refs
                .tag(name)?
                .ok_or_else(|| Missing::Name(NameError::NoSuchTag(name.clone()))),
            Revision::Remote(name) => refs
                .remote_ref(name)?
                .map(|reference| reference.commit)
                .ok_or_else(|| Missing::Name(NameError::NoSuchRemoteReference(name.clone()))),
            Revision::Name(name) => match (refs.branch(name)?, refs.tag(name)?) {
                (Some(commit), None) | (None, Some(commit)) => Ok(commit),
                (None, None) => Err(Missing::Name(NameError::Unknown(name.clone()))),
                (Some(_), Some(_)) => Err(Missing::Name(NameError::Ambiguous(name.clone()))),
            },
            Revision::Commit(id) => Err(Missing::Commit(*id)),
        })
    }
}

/// What [`Repository::named_commit`] did not find.
#[derive(Debug)]
pub(super) enum Missing {
    /// A commit ID: it names no reference, and the store resolves it.
    Commit(ObjectId),
    /// The branch of `heads/<name>`: each command has an error of its own for it.
    Branch(BranchName),
    /// The tag, the remote reference, the name, or the clash of a branch and a tag.
    Name(NameError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refs::RefKind;

    fn name(text: &str) -> BranchName {
        text.parse().expect("a valid name")
    }

    fn remote(remote: &str, kind: RefKind, reference: &str) -> RemoteRefName {
        RemoteRefName {
            remote: remote.parse().expect("a valid remote name"),
            kind,
            name: name(reference),
        }
    }

    #[test]
    fn text_that_starts_with_remotes_is_a_remote_reference_or_refused() {
        // Never a plain name: the prefix does not fall through to the lookup.
        for text in [
            "remotes/",
            "remotes/origin",
            "remotes/origin/heads",
            "remotes/origin/main",
            "remotes/origin/branches/main",
            "remotes/origin/Heads/main",
        ] {
            assert_eq!(
                text.parse::<Revision>(),
                Err(RevisionParseError::RemoteShape(text.to_owned())),
                "{text}"
            );
        }
        assert!(matches!(
            "remotes/.x/heads/main".parse::<Revision>(),
            Err(RevisionParseError::Remote(_))
        ));
        assert!(matches!(
            "remotes//heads/main".parse::<Revision>(),
            Err(RevisionParseError::Remote(_))
        ));
        for text in ["remotes/origin/heads/", "remotes/origin/tags/a//b"] {
            assert!(
                matches!(text.parse::<Revision>(), Err(RevisionParseError::Name(_))),
                "{text}"
            );
        }
        let message = "remotes/origin"
            .parse::<Revision>()
            .unwrap_err()
            .to_string();
        assert!(
            message.contains("remotes/<remote>/heads/<name>")
                && message.contains("remotes/<remote>/tags/<name>"),
            "{message}"
        );
        // The text reads back.
        let text = "remotes/origin/tags/v1";
        assert_eq!(text.parse::<Revision>().unwrap().to_string(), text);
        assert_eq!(
            NameError::NoSuchRemoteReference(remote("origin", RefKind::Tag, "v1")).to_string(),
            "there is no remote reference `remotes/origin/tags/v1`"
        );
    }

    #[test]
    fn plain_text_is_a_name_and_the_prefixes_are_qualified() {
        assert_eq!(
            "main".parse::<Revision>().unwrap(),
            Revision::Name(name("main"))
        );
        assert_eq!(
            "heads/main".parse::<Revision>().unwrap(),
            Revision::Branch(name("main"))
        );
        assert_eq!(
            "tags/v1".parse::<Revision>().unwrap(),
            Revision::Tag(name("v1"))
        );
        assert_eq!(
            "remotes/origin/heads/feat/x".parse::<Revision>().unwrap(),
            Revision::Remote(remote("origin", RefKind::Branch, "feat/x"))
        );
        assert_eq!(
            "remotes/origin/tags/v1".parse::<Revision>().unwrap(),
            Revision::Remote(remote("origin", RefKind::Tag, "v1"))
        );
        // A branch or a tag from before the prefix `remotes/` was reserved keeps its way in.
        assert_eq!(
            "heads/remotes/x".parse::<Revision>().unwrap(),
            Revision::Branch(name("remotes/x"))
        );
        assert_eq!(
            "tags/remotes/x".parse::<Revision>().unwrap(),
            Revision::Tag(name("remotes/x"))
        );
        // A prefix is a prefix of the whole name, not of a component further in.
        assert_eq!(
            "feat/heads/x".parse::<Revision>().unwrap(),
            Revision::Name(name("feat/heads/x"))
        );
    }

    #[test]
    fn a_commit_id_is_not_a_name() {
        let id = format!("b3:{}", "0".repeat(64));
        assert!(matches!(id.parse::<Revision>(), Ok(Revision::Commit(_))));
        assert!(matches!(
            "b3:xyz".parse::<Revision>(),
            Err(RevisionParseError::CommitId(_))
        ));
    }

    #[test]
    fn a_qualified_form_needs_a_valid_name_behind_the_prefix() {
        assert!(matches!(
            "tags/".parse::<Revision>(),
            Err(RevisionParseError::Name(_))
        ));
        assert!(matches!(
            "heads/..".parse::<Revision>(),
            Err(RevisionParseError::Name(_))
        ));
    }

    #[test]
    fn a_qualified_tag_prints_with_its_prefix() {
        assert_eq!(Revision::Tag(name("v1")).to_string(), "tags/v1");
        assert_eq!(Revision::Name(name("v1")).to_string(), "v1");
    }

    #[test]
    fn new_names_with_a_reserved_prefix_are_refused() {
        for text in [
            "heads/x",
            "tags/x",
            "heads/a/b",
            "remotes/x",
            "remotes/origin/heads/x",
        ] {
            let error = require_unreserved(name(text)).unwrap_err();
            assert!(text.starts_with(error.prefix), "{error}");
        }
        for text in [
            "main",
            "feat/heads/x",
            "headsup",
            "tags",
            "remotes",
            "feat/remotes/x",
        ] {
            assert_eq!(require_unreserved(name(text)).unwrap(), name(text));
        }
    }

    #[test]
    fn the_message_of_a_clash_names_both_forms() {
        let message = NameError::Ambiguous(name("v1")).to_string();
        assert!(
            message.contains("heads/v1") && message.contains("tags/v1"),
            "{message}"
        );
    }
}
