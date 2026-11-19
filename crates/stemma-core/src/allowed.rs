//! The proof that the authorizer allowed an operation (ADR-002).
//!
//! A mutating operation of [`Repository`](crate::repository::Repository) starts with the question
//! to the [`Authorizer`]. [`Allowed`] is the answer in the type system: it can only be made from
//! [`Decision::Allow`], inside this module, and the functions that open a write (the locks of the
//! references, and the installation of a new repository) take it as an argument. A write path that
//! opens its write through them and never asks the authorizer has no [`Allowed`] to pass, so it
//! does not compile. What the type does not cover is listed in ADR-002.

use std::fmt;

use crate::oplog::OperationName;
use crate::register::Identity;
use crate::{Actor, Authorizer, Decision};

/// The proof that [`Authorizer::authorize`] returned [`Decision::Allow`].
///
/// The field is private to this module, so no other module of the crate can make a value; the
/// question to the authorizer is the only way. The type is public because the mutators of the
/// public trait [`RefStore`](crate::refs::RefStore) take it, and a caller outside the crate cannot
/// forge one:
///
/// ```compile_fail,E0423
/// let _forged = stemma_core::Allowed(());
/// ```
///
/// Without a proof, a write through the public trait does not compile either: the calls below
/// lack the argument (E0061).
///
/// ```compile_fail,E0061
/// use stemma_core::refs::{BranchName, FsRefStore, Head, RefStore};
/// use stemma_core::store::FsObjectStore;
///
/// let refs = FsRefStore::new("refs");
/// let objects = FsObjectStore::new("objects");
/// let head = Head::Branch(BranchName::try_new("main").unwrap());
/// refs.set_head(&head, &objects);
/// ```
///
/// ```compile_fail,E0061
/// use stemma_core::object::CommitId;
/// use stemma_core::refs::{BranchName, FsRefStore, RefStore};
/// use stemma_core::store::FsObjectStore;
///
/// let refs = FsRefStore::new("refs");
/// let objects = FsObjectStore::new("objects");
/// let main = BranchName::try_new("main").unwrap();
/// let commit: CommitId = "b3:4a0b6603381c7c38c557e9ceb3cba8876f0a7863146e7309801352e80b073b65"
///     .parse()
///     .unwrap();
/// refs.update_branch(&main, None, commit, &objects);
/// ```
///
/// The proof says that some operation was asked and allowed. It is bound to no operation, no target,
/// and no repository: the call that asks stays responsible for writing what it asked for.
#[derive(Debug)]
#[must_use = "a proof that is dropped unused means the question to the authorizer had no effect"]
pub struct Allowed(());

impl Allowed {
    /// Asks the authorizer, and returns the proof if it allows `operation` on `target`.
    #[must_use]
    pub(crate) fn ask(
        authorizer: &dyn Authorizer,
        actor: &Actor,
        operation: &OperationName,
        target: &str,
    ) -> Option<Self> {
        match authorizer.authorize(actor, operation, target) {
            Decision::Allow => Some(Self(())),
            Decision::Deny => None,
        }
    }

    /// A proof for a test that opens a write without an operation to ask for.
    #[cfg(test)]
    pub(crate) fn granted() -> Self {
        Self(())
    }
}

/// Why an actor may not approve a pull request: the approval would be an approval of oneself
/// (ADR-004 rule 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SelfApproval {
    /// The reviewer opened the pull request.
    Opener,
    /// The reviewer wrote a commit that the pull request brings.
    Author,
}

impl fmt::Display for SelfApproval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Opener => "the reviewer opened the pull request",
            Self::Author => "the reviewer wrote a commit of the pull request",
        })
    }
}

/// The ban on self-approval (ADR-004 rule 4): `reviewer` may not approve a pull request that
/// `opener` opened and whose commits `authors` wrote. The rule is by identity, as "independent"
/// is (ADR-004): the model and the session of an agent do not make another reviewer. It goes no
/// further: two agents of one sponsor count as two reviews, and the stricter rule for the writer
/// of a `check` is the one of ADR-019.
///
/// One function, two callers: the writer of an approval asks it before it writes the entry, and
/// the recount asks it again for every approval it reads, so that an entry written by hand does
/// not count.
pub fn self_approval(reviewer: &Actor, opener: &Actor, authors: &[Actor]) -> Option<SelfApproval> {
    let reviewer = Identity::of(reviewer);
    if reviewer == Identity::of(opener) {
        return Some(SelfApproval::Opener);
    }
    authors
        .iter()
        .any(|author| Identity::of(author) == reviewer)
        .then_some(SelfApproval::Author)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AllowAll;

    struct DenyAll;

    impl Authorizer for DenyAll {
        fn authorize(&self, _actor: &Actor, _operation: &OperationName, _target: &str) -> Decision {
            Decision::Deny
        }
    }

    fn human(id: &str) -> Actor {
        Actor::human(id).unwrap()
    }

    fn agent(id: &str, session: &str, sponsor: &str) -> Actor {
        Actor::agent(id, "opus-5-5", session, sponsor).unwrap()
    }

    #[test]
    fn nobody_approves_what_they_opened_or_wrote() {
        let (mia, ana) = (human("mia"), human("ana"));
        assert_eq!(self_approval(&mia, &mia, &[]), Some(SelfApproval::Opener));
        assert_eq!(
            self_approval(&ana, &mia, &[human("zoe"), ana.clone()]),
            Some(SelfApproval::Author)
        );
        assert_eq!(self_approval(&ana, &mia, &[human("zoe")]), None);
    }

    #[test]
    fn the_rule_is_by_identity_and_not_by_model_or_session() {
        let first = agent("claude", "s1", "daniel");
        let second = agent("claude", "s2", "daniel");
        assert_eq!(
            self_approval(&second, &first, &[]),
            Some(SelfApproval::Opener)
        );
        assert_eq!(
            self_approval(&second, &human("mia"), std::slice::from_ref(&first)),
            Some(SelfApproval::Author)
        );
    }

    #[test]
    fn a_sponsor_and_its_agents_are_other_identities() {
        let bot = agent("bot", "s", "daniel");
        let other = agent("other", "s", "daniel");
        let daniel = human("daniel");
        assert_eq!(
            self_approval(&bot, &daniel, std::slice::from_ref(&daniel)),
            None
        );
        assert_eq!(
            self_approval(&daniel, &bot, std::slice::from_ref(&bot)),
            None
        );
        assert_eq!(
            self_approval(&other, &bot, std::slice::from_ref(&bot)),
            None
        );
    }

    fn ask(authorizer: &dyn Authorizer) -> Option<Allowed> {
        let actor = Actor::human("daniel").unwrap();
        let operation = OperationName::try_new("commit").unwrap();
        Allowed::ask(authorizer, &actor, &operation, "main")
    }

    #[test]
    fn only_an_allow_makes_the_proof() {
        assert!(ask(&AllowAll).is_some());
        assert!(ask(&DenyAll).is_none());
    }
}
