//! The proof that the authorizer allowed an operation (ADR-002).
//!
//! A mutating operation of [`Repository`](crate::repository::Repository) starts with the question
//! to the [`Authorizer`]. [`Allowed`] is the answer in the type system: it can only be made from
//! [`Decision::Allow`], inside this module, and the functions that open a write (the locks of the
//! references, and the installation of a new repository) take it as an argument. A write path that
//! opens its write through them and never asks the authorizer has no [`Allowed`] to pass, so it
//! does not compile. What the type does not cover is listed in ADR-002.

use crate::oplog::OperationName;
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
