//! The policy of the base commit, for the gate and the lease (STEMMA-246, STEMMA-230).
//!
//! The rules that decide whether a change may land are read from the policy file of the *base
//! commit*, the tip of `main`, and never from the working directory or the branch of the change:
//! whoever changes a policy is subject to the policy that stands before the change.
//!
//! - **Open mode** (no register): a repository without a policy file has the built-in default
//!   ([`Policy::default`]): every path has level 2, except the files that the floor of the core
//!   keeps at level 3 (the policy file, the register), and the approvals are not enforced, as
//!   the gate asks for none (ADR-011).
//! - **Enforcing mode** (a register): the policy is required. A missing, unreadable, or invalid
//!   file, one with an unknown version among them, is an error that names the reason, and the
//!   caller refuses the merge. Nothing falls back to the default there.
//!
//! Only a *missing* file (or a missing `main`) depends on the mode. A policy file that exists but
//! is not valid, is not a file, or cannot be read is an error in the open mode as well: a
//! repository that wrote a policy meant it, and a rule that is silently not applied is worse
//! than a refusal.

use std::error::Error;
use std::fmt;

use super::difference::{self, DiffError};
use super::{Claimed, LeaseError, Repository};
use crate::object::{CommitId, EntryMode, Kind, ObjectId, TreeId};
use crate::policy::{self, PathChange, PathError, Policy, PolicyParseError, RiskLevel};
use crate::pull::TARGET;
use crate::refs::{BranchName, RefError};
use crate::register::Mode;
use crate::store::{ObjectStore, ReadError};
use crate::{Actor, Authorizer};

/// Why the policy of the base commit cannot be used. A caller that gets one refuses the action.
#[derive(Debug)]
#[non_exhaustive]
pub enum PolicyError {
    /// The repository is enforcing and its base commit has no policy file.
    Missing,
    /// The repository is enforcing and has no base commit: `main` does not exist.
    NoBase,
    /// The path of the policy file is taken by something that is not a blob, such as a
    /// directory or a symbolic link.
    NotAFile,
    /// The file is not a valid policy: a problem of its form, or a version that this reader does
    /// not know.
    Invalid(PolicyParseError),
    /// The commit, the tree, or the file could not be read.
    Unreadable(String),
    /// The reference of `main` could not be read.
    Refs(RefError),
}

impl fmt::Display for PolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => write!(
                f,
                "the repository is enforcing, and the base commit has no {}; no change can be \
                 merged without a policy",
                policy::FILE_NAME
            ),
            Self::NoBase => write!(
                f,
                "the repository is enforcing and has no branch `{TARGET}`, so there is no policy \
                 to read"
            ),
            Self::NotAFile => write!(f, "{} in the base commit is not a file", policy::FILE_NAME),
            Self::Invalid(error) => {
                write!(
                    f,
                    "{} in the base commit is not valid: {error}",
                    policy::FILE_NAME
                )
            }
            Self::Unreadable(reason) => {
                write!(
                    f,
                    "cannot read {} of the base commit: {reason}",
                    policy::FILE_NAME
                )
            }
            Self::Refs(error) => write!(f, "cannot read `{TARGET}`: {error}"),
        }
    }
}

impl Error for PolicyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Invalid(error) => Some(error),
            Self::Refs(error) => Some(error),
            _ => None,
        }
    }
}

fn unreadable(error: impl fmt::Display) -> PolicyError {
    PolicyError::Unreadable(error.to_string())
}

fn not_in_store(what: &str, id: impl fmt::Display) -> PolicyError {
    unreadable(format!("{what} {id} is not in the store"))
}

impl Repository {
    /// The policy of a base commit: the file [`policy::FILE_NAME`] in its root.
    ///
    /// `enforcing` says whether the repository is in the enforcing mode. Without a file the open
    /// mode gets the built-in default, and the enforcing mode an error (see the module). The
    /// callers derive it from the [`Mode`] of the repository; the function is not public so that
    /// no caller can pass a flag that disagrees with the register.
    ///
    /// # Errors
    ///
    /// [`PolicyError::Missing`] for an enforcing repository without a file,
    /// [`PolicyError::NotAFile`], [`PolicyError::Invalid`] for a file that does not parse, and
    /// [`PolicyError::Unreadable`] if an object cannot be read.
    pub(crate) fn policy_at(
        &self,
        base: &CommitId,
        enforcing: bool,
    ) -> Result<Policy, PolicyError> {
        let store = self.objects();
        let commit = store
            .get_commit(base)
            .map_err(unreadable)?
            .ok_or_else(|| not_in_store("the base commit", base))?;
        let tree = store
            .get_tree(&commit.tree())
            .map_err(unreadable)?
            .ok_or_else(|| not_in_store("the tree of the base commit", commit.tree()))?;
        let Some(entry) = tree
            .entries()
            .iter()
            .find(|entry| entry.name() == policy::FILE_NAME)
        else {
            return if enforcing {
                Err(PolicyError::Missing)
            } else {
                Ok(Policy::default())
            };
        };
        let id: ObjectId = entry.id();
        let raw = store
            .get(&id)
            .map_err(|error| unreadable(ReadError::Store(Box::new(error))))?
            .ok_or_else(|| not_in_store("the policy file", id))?;
        if raw.kind() != Kind::Blob || entry.mode() == EntryMode::Symlink {
            return Err(PolicyError::NotAFile);
        }
        Policy::parse(raw.body()).map_err(PolicyError::Invalid)
    }

    /// The policy of the tip of `main`, the base commit of every merge and of the leases. See
    /// [`policy_at`](Self::policy_at). A repository without `main` has the default policy in the
    /// open mode and none in the enforcing mode.
    ///
    /// # Errors
    ///
    /// The errors of [`policy_at`](Self::policy_at), [`PolicyError::NoBase`], and
    /// [`PolicyError::Refs`].
    pub(crate) fn policy_of_main(&self, enforcing: bool) -> Result<Policy, PolicyError> {
        let main = BranchName::try_new(TARGET).expect("`main` is a branch name");
        match self.refs().branch(&main).map_err(PolicyError::Refs)? {
            Some(tip) => self.policy_at(&tip, enforcing),
            None if enforcing => Err(PolicyError::NoBase),
            None => Ok(Policy::default()),
        }
    }

    /// The level of the change that turns the tree of `tip` into `result`: the highest level of
    /// every file that differs, by the policy. A file that is added, removed, or changed counts
    /// with its path; a move shows as a removal and an addition, so it touches both paths.
    ///
    /// # Errors
    ///
    /// [`LevelError::Diff`] if a tree cannot be read, [`LevelError::Path`] for a path that the
    /// policy cannot judge (too deep, or a name that a tree refuses), and
    /// [`LevelError::NoChange`] if no file differs. None of them is a lower level.
    pub fn level_of_change(
        &self,
        policy: &Policy,
        tip: &CommitId,
        result: TreeId,
    ) -> Result<RiskLevel, LevelError> {
        let before = self.files_with_modes_of(*tip).map_err(LevelError::Diff)?;
        let after =
            difference::files_with_modes(self.objects(), result).map_err(LevelError::Diff)?;
        // The mode counts as well: a script made executable is a change of that path.
        let mut changes = Vec::new();
        for (path, entry) in &after {
            match before.get(path) {
                None => changes.push(PathChange::Added { path }),
                Some(old) if old != entry => changes.push(PathChange::Modified { path }),
                Some(_) => {}
            }
        }
        for path in before.keys().filter(|path| !after.contains_key(*path)) {
            changes.push(PathChange::Deleted { path });
        }
        // The trees differ, so a change that touches no path is a bug here, never level 0.
        if changes.is_empty() {
            return Err(LevelError::NoChange);
        }
        policy.level_of(&changes).map_err(LevelError::Path)
    }

    /// [`claim_lease`](Self::claim_lease) with the duration of the lease from the policy of the
    /// tip of `main` (the line `lease`, or 60 minutes).
    ///
    /// `mode` is the mode of the repository ([`Mode::of`]); the claim is only as strict as the
    /// mode the caller passes. A policy that cannot be used refuses the claim
    /// ([`LeaseError::Policy`]) and nothing is written: in either mode a file that is invalid,
    /// not a file, or unreadable, and in an enforcing repository also a missing file.
    ///
    /// # Errors
    ///
    /// [`LeaseError::Policy`], and the errors of [`claim_lease`](Self::claim_lease).
    pub fn claim_lease_by_policy(
        &self,
        branch: &BranchName,
        actor: &Actor,
        mode: &Mode,
        authorizer: &dyn Authorizer,
    ) -> Result<Claimed, LeaseError> {
        let minutes = self
            .policy_of_main(matches!(mode, Mode::Enforcing(_)))
            .map_err(LeaseError::Policy)?
            .lease_minutes();
        self.claim_lease(branch, actor, minutes, authorizer)
    }
}

/// Why the level of a change cannot be told. A caller that gets one refuses the change.
#[derive(Debug)]
#[non_exhaustive]
pub enum LevelError {
    /// A tree of the change could not be read.
    Diff(DiffError),
    /// A path that the policy cannot judge.
    Path(PathError),
    /// The result has no file that differs from the tip, so there is nothing to judge. The gate
    /// does not ask for a result that equals the tip (the repair path), so this is refused.
    NoChange,
}

impl fmt::Display for LevelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Diff(error) => write!(f, "cannot compare the change with the tip: {error}"),
            Self::Path(error) => write!(f, "cannot tell the level of the change: {error}"),
            Self::NoChange => f.write_str("the change has no file that differs from the tip"),
        }
    }
}

impl Error for LevelError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Diff(error) => Some(error),
            Self::Path(error) => Some(error),
            Self::NoChange => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::ChangeId;
    use crate::index::Index;
    use crate::object::{Blob, Commit, FileMode};
    use crate::oplog::OperationName;
    use crate::register::Register;
    use crate::repository::InitOptions;
    use crate::testutil::TempDir;
    use crate::AllowAll;

    fn human(id: &str) -> Actor {
        Actor::human(id).unwrap()
    }

    /// A register with one owner, for the enforcing mode.
    fn register() -> Register {
        let key = ssh_key::PrivateKey::from(ssh_key::private::Ed25519Keypair::from_seed(&[1; 32]));
        let text = format!(
            "stemma-register 1\nentry human daniel owner\nkey {} active\n",
            key.public_key().to_openssh().unwrap()
        );
        Register::parse(text.as_bytes()).unwrap()
    }

    fn name(text: &str) -> BranchName {
        BranchName::try_new(text).unwrap()
    }

    fn repository(dir: &TempDir) -> Repository {
        Repository::init(
            dir.path(),
            &InitOptions::default(),
            &human("daniel"),
            &AllowAll,
        )
        .unwrap()
    }

    /// Puts a commit with these files on `main`.
    fn land(repository: &Repository, files: &[(&str, &str)]) -> CommitId {
        let mut index = Index::new();
        for (path, text) in files {
            let blob = repository.objects().put_blob(&Blob::new(*text)).unwrap();
            index.insert(*path, FileMode::File, blob).unwrap();
        }
        let tree = index.write_tree(repository.objects()).unwrap();
        let main = name("main");
        let before = repository.refs().branch(&main).unwrap();
        let parent = before.unwrap_or_else(|| repository.refs().working_copy().unwrap());
        let commit = repository
            .objects()
            .put_commit(
                &Commit::try_new(
                    tree,
                    vec![parent],
                    ChangeId::generate().unwrap(),
                    human("daniel"),
                    1,
                    "policy".to_owned(),
                )
                .unwrap(),
            )
            .unwrap();
        repository
            .update_branch(
                &OperationName::try_new("branch").unwrap(),
                &main,
                before,
                commit,
                &human("daniel"),
                &AllowAll,
            )
            .unwrap();
        commit
    }

    #[test]
    fn a_repository_without_main_has_the_default_policy_in_the_open_mode_only() {
        let dir = TempDir::new();
        let repository = repository(&dir);

        assert_eq!(repository.policy_of_main(false).unwrap(), Policy::default());
        assert!(matches!(
            repository.policy_of_main(true),
            Err(PolicyError::NoBase)
        ));
    }

    #[test]
    fn the_policy_comes_from_the_commit_that_is_asked_for() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let plain = land(&repository, &[("x.txt", "x\n")]);
        let strict = land(
            &repository,
            &[
                ("x.txt", "x\n"),
                (".stemma-policy", "stemma-policy 1\nlease 30\n"),
            ],
        );

        assert_eq!(
            repository.policy_at(&plain, false).unwrap(),
            Policy::default()
        );
        assert!(matches!(
            repository.policy_at(&plain, true),
            Err(PolicyError::Missing)
        ));
        for enforcing in [false, true] {
            let policy = repository.policy_at(&strict, enforcing).unwrap();
            assert_eq!(policy.lease_minutes(), 30);
            assert_eq!(repository.policy_of_main(enforcing).unwrap(), policy);
        }
    }

    #[test]
    fn the_duration_of_the_lease_comes_from_the_policy_of_main() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        land(
            &repository,
            &[
                ("x.txt", "x\n"),
                (".stemma-policy", "stemma-policy 1\nlease 30\n"),
            ],
        );

        let claimed = repository
            .claim_lease_by_policy(
                &name("feature"),
                &human("mia"),
                &Mode::Enforcing(register()),
                &AllowAll,
            )
            .unwrap();

        assert_eq!(claimed.lease.expires() - claimed.lease.acquired(), 30 * 60);
    }

    #[test]
    fn without_a_policy_the_lease_lasts_an_hour_in_the_open_mode_and_is_refused_when_enforcing() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        land(&repository, &[("x.txt", "x\n")]);

        let refused = repository
            .claim_lease_by_policy(
                &name("feature"),
                &human("mia"),
                &Mode::Enforcing(register()),
                &AllowAll,
            )
            .unwrap_err();

        assert!(
            matches!(refused, LeaseError::Policy(PolicyError::Missing)),
            "{refused}"
        );
        assert_eq!(repository.lease(&name("feature")).unwrap(), None);
        let claimed = repository
            .claim_lease_by_policy(&name("feature"), &human("mia"), &Mode::Open, &AllowAll)
            .unwrap();
        assert_eq!(
            claimed.lease.expires() - claimed.lease.acquired(),
            i64::from(policy::DEFAULT_LEASE_MINUTES) * 60
        );
    }

    #[test]
    fn an_unusable_policy_refuses_the_lease_in_both_modes_and_writes_nothing() {
        for (files, reason) in [
            (
                vec![("x.txt", "x\n"), (".stemma-policy", "stemma-policy 2\n")],
                "unknown version",
            ),
            (
                vec![("x.txt", "x\n"), (".stemma-policy", "junk\n")],
                "first line has to be",
            ),
            (
                vec![("x.txt", "x\n"), (".stemma-policy/y", "1")],
                "not a file",
            ),
        ] {
            for mode in [Mode::Open, Mode::Enforcing(register())] {
                let dir = TempDir::new();
                let repository = repository(&dir);
                land(&repository, &files);

                let error = repository
                    .claim_lease_by_policy(&name("feature"), &human("mia"), &mode, &AllowAll)
                    .unwrap_err();

                assert!(matches!(error, LeaseError::Policy(_)), "{error}");
                assert!(error.to_string().contains(reason), "{error}");
                assert!(
                    error.to_string().starts_with("the lease is refused"),
                    "{error}"
                );
                assert_eq!(repository.lease(&name("feature")).unwrap(), None);
            }
        }
    }

    #[test]
    fn a_repository_without_main_refuses_the_lease_when_enforcing() {
        let dir = TempDir::new();
        let repository = repository(&dir);

        let error = repository
            .claim_lease_by_policy(
                &name("feature"),
                &human("mia"),
                &Mode::Enforcing(register()),
                &AllowAll,
            )
            .unwrap_err();

        assert!(
            matches!(error, LeaseError::Policy(PolicyError::NoBase)),
            "{error}"
        );
    }

    /// The level of the change from the tip to a commit with these files.
    fn level(
        repository: &Repository,
        policy: &str,
        tip_files: &[(&str, &str, FileMode)],
        result_files: &[(&str, &str, FileMode)],
    ) -> Result<RiskLevel, LevelError> {
        let tree = |files: &[(&str, &str, FileMode)]| {
            let mut index = Index::new();
            for (path, text, mode) in files {
                let blob = repository.objects().put_blob(&Blob::new(*text)).unwrap();
                index.insert(*path, *mode, blob).unwrap();
            }
            index.write_tree(repository.objects()).unwrap()
        };
        let tip = repository
            .objects()
            .put_commit(
                &Commit::try_new(
                    tree(tip_files),
                    vec![],
                    ChangeId::generate().unwrap(),
                    human("daniel"),
                    1,
                    "tip".to_owned(),
                )
                .unwrap(),
            )
            .unwrap();
        let policy = Policy::parse(policy.as_bytes()).unwrap();
        repository.level_of_change(&policy, &tip, tree(result_files))
    }

    #[test]
    fn the_level_of_a_change_is_the_highest_level_of_the_files_that_differ() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let policy = "stemma-policy 1\nlevel 0 docs.md\nlevel 1 mid.rs\nlevel 3 core.rs\n";
        let file = FileMode::File;
        let base = [
            ("docs.md", "d\n", file),
            ("mid.rs", "m\n", file),
            ("core.rs", "c\n", file),
        ];
        let case = |result: &[(&str, &str, FileMode)]| level(&repository, policy, &base, result);

        // Modified, added, and deleted files each count with their path.
        let modified = [
            ("docs.md", "d2\n", file),
            ("mid.rs", "m\n", file),
            ("core.rs", "c\n", file),
        ];
        assert_eq!(case(&modified).unwrap(), RiskLevel::Zero);
        let modified_mid = [
            ("docs.md", "d\n", file),
            ("mid.rs", "m2\n", file),
            ("core.rs", "c\n", file),
        ];
        assert_eq!(case(&modified_mid).unwrap(), RiskLevel::One);
        let deleted = [("docs.md", "d\n", file), ("mid.rs", "m\n", file)];
        assert_eq!(case(&deleted).unwrap(), RiskLevel::Three);
        let added = [
            ("docs.md", "d\n", file),
            ("mid.rs", "m\n", file),
            ("core.rs", "c\n", file),
            ("new.txt", "n\n", file),
        ];
        assert_eq!(case(&added).unwrap(), RiskLevel::Two);
        // A move touches the old and the new path.
        let moved = [
            ("docs.md", "d\n", file),
            ("mid.rs", "m\n", file),
            ("moved.rs", "c\n", file),
        ];
        assert_eq!(case(&moved).unwrap(), RiskLevel::Three);
    }

    #[test]
    fn a_change_of_the_mode_alone_has_the_level_of_its_path() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let policy = "stemma-policy 1\nlevel 3 core.sh\nlevel 0 docs.md\n";

        let executable = level(
            &repository,
            policy,
            &[
                ("core.sh", "run\n", FileMode::File),
                ("docs.md", "d\n", FileMode::File),
            ],
            &[
                ("core.sh", "run\n", FileMode::Executable),
                ("docs.md", "d\n", FileMode::File),
            ],
        );

        assert_eq!(executable.unwrap(), RiskLevel::Three);
    }

    #[test]
    fn a_result_that_equals_the_tip_has_no_level() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let same = [("a.txt", "a\n", FileMode::File)];

        let error = level(&repository, "stemma-policy 1\n", &same, &same).unwrap_err();

        assert!(matches!(error, LevelError::NoChange), "{error}");
    }

    #[test]
    fn a_path_that_is_too_deep_is_refused_and_not_judged() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let deep = vec!["d"; policy::MAX_PATH_PARTS + 1].join("/");

        let error = level(
            &repository,
            "stemma-policy 1\n",
            &[("a.txt", "a\n", FileMode::File)],
            &[
                ("a.txt", "a\n", FileMode::File),
                (deep.as_str(), "x\n", FileMode::File),
            ],
        )
        .unwrap_err();

        assert!(matches!(error, LevelError::Path(_)), "{error}");
    }

    #[test]
    fn a_policy_that_is_a_link_is_refused() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let mut index = Index::new();
        let blob = repository
            .objects()
            .put_blob(&Blob::new("stemma-policy 1\n"))
            .unwrap();
        index
            .insert(".stemma-policy", FileMode::Symlink, blob)
            .unwrap();
        let tree = index.write_tree(repository.objects()).unwrap();
        let commit = repository
            .objects()
            .put_commit(
                &Commit::try_new(
                    tree,
                    vec![],
                    ChangeId::generate().unwrap(),
                    human("daniel"),
                    1,
                    "link".to_owned(),
                )
                .unwrap(),
            )
            .unwrap();

        assert!(matches!(
            repository.policy_at(&commit, true),
            Err(PolicyError::NotAFile)
        ));
    }

    #[test]
    fn the_errors_name_the_policy_file_and_the_reason() {
        for (error, part) in [
            (PolicyError::Missing, "has no .stemma-policy"),
            (PolicyError::NoBase, "no branch `main`"),
            (PolicyError::NotAFile, "not a file"),
            (
                PolicyError::Invalid(Policy::parse(b"stemma-policy 9\n").unwrap_err()),
                "unknown version",
            ),
            (PolicyError::Unreadable("boom".to_owned()), "boom"),
        ] {
            assert!(error.to_string().contains(part), "{error}");
        }
    }
}
