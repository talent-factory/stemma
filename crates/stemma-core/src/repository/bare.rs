//! Tests of the bare repository (ADR-009, STEMMA-284): what `init_bare` makes, what `open` and
//! `open_bare` accept, every operation that is refused, and every check that works without a
//! working copy and without the operation log.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use super::*;
use crate::change::ChangeId;
use crate::failure::{Classify, FailureKind};
use crate::index::Index;
use crate::merge::merge_commits;
use crate::object::{Blob, FileMode, TreeId};
use crate::oplog::CheckResult;
use crate::policy::{self, RiskLevel};
use crate::refs::encode_branch;
use crate::register::{self, Mode, ModeError, Register, RegisterAuthorizer};
use crate::testutil::TempDir;
use crate::verify::VerifyError;
use crate::weakened::FileChange;
use crate::{AllowAll, Decision};

/// Refuses everything. A refused operation that reports the bare repository and not a denial did
/// not ask the authorizer.
struct DenyAll;

impl Authorizer for DenyAll {
    fn authorize(&self, _actor: &Actor, _operation: &OperationName, _target: &str) -> Decision {
        Decision::Deny
    }
}

fn daniel() -> Actor {
    Actor::human("daniel").unwrap()
}

fn name(text: &str) -> BranchName {
    BranchName::try_new(text).unwrap()
}

fn operation(text: &str) -> OperationName {
    OperationName::try_new(text).unwrap()
}

/// A bare repository in the directory `hub` below `dir`.
fn bare(dir: &TempDir) -> Repository {
    Repository::init_bare(dir.path().join("hub"), &daniel(), &AllowAll).unwrap()
}

/// Every file and directory below `root`, relative to it, with the content of the files.
fn listing(root: &Path) -> BTreeSet<(PathBuf, Option<Vec<u8>>)> {
    fn walk(root: &Path, dir: &Path, found: &mut BTreeSet<(PathBuf, Option<Vec<u8>>)>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            if path.is_dir() {
                found.insert((relative, None));
                walk(root, &path, found);
            } else {
                found.insert((relative, Some(fs::read(&path).unwrap())));
            }
        }
    }
    let mut found = BTreeSet::new();
    walk(root, root, &mut found);
    found
}

/// Stores a commit with these files and parents in the object store of `repository`.
fn commit_of(repository: &Repository, files: &[(&str, &str)], parents: Vec<CommitId>) -> CommitId {
    let commit = Commit::try_new(
        tree_of(repository, files),
        parents,
        ChangeId::generate().unwrap(),
        daniel(),
        1,
        "a commit",
    )
    .unwrap();
    repository.objects().put_commit(&commit).unwrap()
}

fn tree_of(repository: &Repository, files: &[(&str, &str)]) -> TreeId {
    let mut index = Index::new();
    for (path, text) in files {
        let blob = repository.objects().put_blob(&Blob::new(*text)).unwrap();
        index.insert(*path, FileMode::File, blob).unwrap();
    }
    index.write_tree(repository.objects()).unwrap()
}

/// Writes a branch file by hand. A reference of a bare repository moves through the accepting
/// operation of a later slice (ADR-025); until then nothing in the crate moves one.
fn set_branch(repository: &Repository, branch: &str, commit: &CommitId) {
    let path = repository.metadata_dir().join("refs/heads").join(branch);
    fs::write(path, encode_branch(commit)).unwrap();
}

/// A register with one owner.
fn register_text() -> String {
    let key = ssh_key::PrivateKey::from(ssh_key::private::Ed25519Keypair::from_seed(&[1; 32]));
    format!(
        "stemma-register 1\nentry human daniel owner\nkey {} active\n",
        key.public_key().to_openssh().unwrap()
    )
}

/// Checks that `$result` is the error `$pattern`, which binds `$error` to a [`RefError::Bare`]
/// that names `$lacks`, and that the failure is not classified as a lock or as damage.
macro_rules! assert_refused {
    ($lacks:expr, $result:expr, $error:ident in $pattern:pat) => {{
        match $result.map(|_| ()) {
            Err($pattern) => {
                assert_eq!($error.kind(), FailureKind::Other, "{}", $error);
                match $error {
                    RefError::Bare {
                        path,
                        operation,
                        lacks,
                    } => {
                        assert_eq!(lacks, $lacks, "{operation}");
                        assert!(path.ends_with("hub"), "{}", path.display());
                        assert!(!operation.is_empty());
                    }
                    other => panic!("not a refusal of a bare repository: {other:?}"),
                }
            }
            other => panic!("not refused: {other:?}"),
        }
    }};
}

#[test]
fn init_bare_makes_the_layout_without_head_working_copy_and_log() {
    let dir = TempDir::new();
    let repository = bare(&dir);
    let root = dir.path().join("hub");

    assert!(repository.is_bare());
    assert_eq!(repository.path(), root);
    assert_eq!(repository.metadata_dir(), root);
    assert_eq!(repository.worktree_name(), None);
    let found: Vec<_> = listing(&root)
        .into_iter()
        .map(|(path, _)| path.to_string_lossy().replace('\\', "/"))
        .collect();
    assert_eq!(
        found,
        ["bare", "format", "objects", "refs", "refs/heads", "tmp"]
    );
    assert_eq!(
        fs::read(root.join("format")).unwrap(),
        b"stemma-repository 15\n"
    );
    assert_eq!(fs::read(root.join("bare")).unwrap(), b"stemma-bare\n");
    // The temporary directory of `init_bare` is gone: only the repository is beside nothing.
    let beside: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(beside, ["hub"]);
}

#[test]
fn init_bare_creates_missing_directories_above_and_the_result_opens() {
    let dir = TempDir::new();
    let root = dir.path().join("data/repositories/one");
    Repository::init_bare(&root, &daniel(), &AllowAll).unwrap();

    let opened = Repository::open_bare(&root).unwrap();
    assert!(opened.is_bare());
    assert_eq!(opened.path(), root);
    assert!(opened.refs().branches().unwrap().is_empty());
    assert!(opened.refs().tags().unwrap().is_empty());
}

#[test]
fn init_bare_refuses_a_path_that_exists_and_changes_nothing() {
    let dir = TempDir::new();
    let empty = dir.path().join("empty");
    fs::create_dir(&empty).unwrap();
    let file = dir.path().join("file");
    fs::write(&file, "x").unwrap();
    let before = listing(dir.path());

    for path in [&empty, &file] {
        let error = Repository::init_bare(path, &daniel(), &AllowAll).unwrap_err();
        assert!(
            matches!(&error, InitError::AlreadyExists { path: found } if found == path),
            "{error}"
        );
    }
    // A second bare repository in the place of the first one is refused as well.
    let first = bare(&dir);
    let after_first = listing(dir.path());
    let error = Repository::init_bare(first.path(), &daniel(), &AllowAll).unwrap_err();
    assert!(matches!(error, InitError::AlreadyExists { .. }), "{error}");
    assert_eq!(listing(dir.path()), after_first);
    assert!(before.is_subset(&after_first));
}

#[test]
fn init_bare_asks_the_authorizer_before_it_creates_anything() {
    let dir = TempDir::new();
    let root = dir.path().join("above/hub");

    let error = Repository::init_bare(&root, &daniel(), &DenyAll).unwrap_err();

    assert!(
        matches!(&error, InitError::Denied { path } if path == &root),
        "{error}"
    );
    assert!(listing(dir.path()).is_empty());
}

#[test]
fn open_and_discover_refuse_a_bare_repository_and_open_bare_refuses_one_with_a_working_copy() {
    let dir = TempDir::new();
    // A bare repository in the place of a `.stemma`: the commands with a working copy name it
    // and do not report the missing `HEAD` as damage.
    let planted = dir.path().join("planted");
    fs::create_dir(&planted).unwrap();
    Repository::init_bare(planted.join(METADATA_DIR), &daniel(), &AllowAll).unwrap();
    let error = Repository::open(&planted).unwrap_err();
    assert!(
        matches!(&error, OpenError::Bare { path } if path == &planted.join(METADATA_DIR)),
        "{error}"
    );
    assert_eq!(error.kind(), FailureKind::Other);
    assert!(error.to_string().contains("bare repository"), "{error}");
    let error = Repository::discover_with(&planted, &|_| Ok(())).unwrap_err();
    assert!(matches!(error, OpenError::Bare { .. }), "{error}");
    // The other way round: the `.stemma` of a repository with a working copy has no marker, and
    // its root has no `format`.
    let work = dir.path().join("work");
    Repository::init(&work, &InitOptions::default(), &daniel(), &AllowAll).unwrap();
    let error = Repository::open_bare(work.join(METADATA_DIR)).unwrap_err();
    assert!(
        matches!(&error, OpenError::NotBare { path } if path == &work.join(METADATA_DIR)),
        "{error}"
    );
    assert_eq!(error.kind(), FailureKind::Other);
    let error = Repository::open_bare(&work).unwrap_err();
    assert!(matches!(error, OpenError::MissingFormat { .. }), "{error}");
    // The repository with a working copy still opens.
    assert!(!Repository::open(&work).unwrap().is_bare());
}

#[test]
fn open_bare_names_a_missing_directory_a_file_and_an_unsupported_format() {
    let dir = TempDir::new();
    let missing = dir.path().join("missing");
    let error = Repository::open_bare(&missing).unwrap_err();
    assert!(
        matches!(&error, OpenError::Missing { path } if path == &missing),
        "{error}"
    );
    let file = dir.path().join("file");
    fs::write(&file, "x").unwrap();
    let error = Repository::open_bare(&file).unwrap_err();
    assert!(matches!(error, OpenError::NotADirectory { .. }), "{error}");

    let repository = bare(&dir);
    fs::write(repository.path().join("format"), "stemma-repository 99\n").unwrap();
    let error = Repository::open_bare(repository.path()).unwrap_err();
    assert!(
        matches!(&error, OpenError::UnsupportedFormat { found, .. } if found == "99"),
        "{error}"
    );
    // A program that knows only the versions before the bare repository refuses it by its
    // version: `init_bare` never writes one of them.
    fs::remove_file(repository.path().join("format")).unwrap();
    let error = Repository::open_bare(repository.path()).unwrap_err();
    assert!(matches!(error, OpenError::MissingFormat { .. }), "{error}");
}

#[test]
fn a_marker_that_init_bare_did_not_write_is_damage_for_open_and_for_open_bare() {
    for damage in ["content", "long", "empty", "directory"] {
        let dir = TempDir::new();
        let planted = dir.path().join("planted");
        fs::create_dir(&planted).unwrap();
        let root = planted.join(METADATA_DIR);
        Repository::init_bare(&root, &daniel(), &AllowAll).unwrap();
        let marker = root.join("bare");
        match damage {
            "content" => fs::write(&marker, "stemma-bare 2\n").unwrap(),
            "long" => fs::write(&marker, "stemma-bare\n\n").unwrap(),
            "empty" => fs::write(&marker, "").unwrap(),
            _ => {
                fs::remove_file(&marker).unwrap();
                fs::create_dir(&marker).unwrap();
            }
        }

        for error in [
            Repository::open_bare(&root).unwrap_err(),
            Repository::open(&planted).unwrap_err(),
        ] {
            assert!(
                matches!(&error, OpenError::MalformedBareMarker { path } if path == &marker),
                "{damage}: {error}"
            );
            assert_eq!(error.kind(), FailureKind::Damaged, "{damage}");
            assert!(error.to_string().contains("stemma-bare"), "{error}");
        }
    }
}

#[test]
fn every_operation_that_needs_a_working_copy_is_refused_before_it_asks_or_writes() {
    let dir = TempDir::new();
    let repository = bare(&dir);
    let commit = commit_of(&repository, &[("a.txt", "one\n")], vec![]);
    set_branch(&repository, "main", &commit);
    let tree = tree_of(&repository, &[("a.txt", "one\n")]);
    // A plan of a repository with a working copy, for `apply_checkout`.
    let work = Repository::init(
        dir.path().join("work"),
        &InitOptions::default(),
        &daniel(),
        &AllowAll,
    )
    .unwrap();
    let empty = work
        .objects()
        .put_tree(&Tree::try_new(Vec::new()).unwrap())
        .unwrap();
    let plan = work.plan_checkout(None, empty).unwrap();
    let before = listing(repository.path());
    let lacks = BareLacks::WorkingCopy;
    let (actor, deny) = (daniel(), DenyAll);
    let main = name("main");

    assert_refused!(
        lacks,
        repository.snapshot(&actor, &deny),
        error in SnapshotError::Refs(error)
    );
    assert_refused!(
        lacks,
        repository.track(&["a.txt".to_owned()], &actor, &deny),
        error in TrackError::Snapshot(SnapshotError::Refs(error))
    );
    assert_refused!(
        lacks,
        repository.status(&actor, &deny),
        error in StatusError::Snapshot(SnapshotError::Refs(error))
    );
    assert_refused!(
        lacks,
        repository.commit("a message", &actor, &deny),
        error in CommitError::Refs(error)
    );
    assert_refused!(
        lacks,
        repository.diff(&actor, &deny, Comparison::WithParent),
        error in DiffError::Refs(error)
    );
    assert_refused!(
        lacks,
        repository.diff_paths(&actor, &deny, Comparison::WithParent, &[]),
        error in DiffError::Refs(error)
    );
    assert_refused!(
        lacks,
        repository.plan_checkout(None, tree),
        error in CheckoutError::Snapshot(SnapshotError::Refs(error))
    );
    assert_refused!(
        lacks,
        repository.apply_checkout(&plan),
        error in CheckoutError::Snapshot(SnapshotError::Refs(error))
    );
    let target = SwitchTarget::Existing(main.clone());
    assert_refused!(
        lacks,
        repository.switch(&target, Changes::Carry, &actor, &deny),
        error in SwitchError::Refs(error)
    );
    assert_refused!(
        lacks,
        repository.plan_switch(&target, Changes::Carry, &actor, &deny),
        error in SwitchError::Refs(error)
    );
    assert_refused!(
        lacks,
        repository.plan_restore(1, false, &actor, &deny),
        error in RestoreError::Refs(error)
    );
    assert_refused!(
        lacks,
        repository.restore(1, false, &actor, &deny),
        error in RestoreError::Refs(error)
    );
    assert_refused!(
        lacks,
        repository.merge(&main, None, &actor, &deny),
        error in MergeBranchError::Refs(error)
    );
    assert_refused!(
        lacks,
        repository.rebase(&Revision::Branch(main.clone()), &actor, &deny),
        error in RebaseError::Refs(error)
    );
    assert_refused!(
        lacks,
        repository.rewrite_working_copy(commit, &actor, &deny),
        error in RewriteError::Refs(error)
    );
    assert_refused!(
        lacks,
        repository.follow_main(&actor, &deny),
        error in WorktreeError::Refs(error)
    );
    assert_refused!(
        lacks,
        repository.add_worktree(
            &dir.path().join("linked"),
            &WorktreeBranch::Create(name("feature")),
            None,
            &actor,
            &deny
        ),
        error in WorktreeError::Refs(error)
    );
    assert_refused!(
        lacks,
        repository.remove_worktree("linked", false, false, &actor, &deny),
        error in WorktreeError::Refs(error)
    );
    assert_refused!(lacks, repository.worktrees(), error in WorktreeError::Refs(error));
    let head = Head::Branch(main.clone());
    assert_refused!(
        lacks,
        repository.set_head(&operation("switch"), &head, &head, &actor, &deny),
        error in UpdateError::Refs(error)
    );
    assert_refused!(
        lacks,
        repository.update_working_copy(&operation("snapshot"), commit, commit, &actor, &deny),
        error in UpdateError::Refs(error)
    );
    // The value that takes over is the same bare repository.
    assert_refused!(
        lacks,
        repository.taking_over().snapshot(&actor, &deny),
        error in SnapshotError::Refs(error)
    );

    assert_eq!(listing(repository.path()), before);
    assert!(!dir.path().join("linked").exists());
}

#[test]
fn every_operation_that_needs_the_operation_log_is_refused_before_it_asks_or_writes() {
    let dir = TempDir::new();
    let repository = bare(&dir);
    let commit = commit_of(&repository, &[("a.txt", "one\n")], vec![]);
    set_branch(&repository, "main", &commit);
    let blob = Blob::new("one\n").id().object_id();
    let register = Register::parse(register_text().as_bytes()).unwrap();
    let before = listing(repository.path());
    let lacks = BareLacks::OperationLog;
    let (actor, deny) = (daniel(), DenyAll);
    let (main, feature) = (name("main"), name("feature"));
    let branch = operation("branch");

    // The moves of a branch and a tag.
    assert_refused!(
        lacks,
        repository.update_branch(&branch, &main, Some(commit), commit, &actor, &deny),
        error in UpdateError::Refs(error)
    );
    assert_refused!(
        lacks,
        repository.create_branch(&branch, &feature, commit.object_id(), &actor, &deny),
        error in UpdateError::Refs(error)
    );
    assert_refused!(
        lacks,
        repository.delete_branch(&branch, &main, commit.object_id(), &actor, &deny),
        error in UpdateError::Refs(error)
    );
    assert_refused!(
        lacks,
        repository.rename_branch(&branch, &main, &feature, commit.object_id(), &actor, &deny),
        error in UpdateError::Refs(error)
    );
    assert_refused!(
        lacks,
        repository.create_tag(
            &operation("tag"),
            &feature,
            commit.object_id(),
            &actor,
            &deny
        ),
        error in UpdateError::Refs(error)
    );
    assert_refused!(
        lacks,
        repository.delete_tag(
            &operation("tag"),
            &feature,
            commit.object_id(),
            &actor,
            &deny
        ),
        error in UpdateError::Refs(error)
    );
    // The leases.
    assert_refused!(lacks, repository.lease(&main), error in LeaseError::Refs(error));
    assert_refused!(
        lacks,
        repository.claim_lease(&main, &actor, 60, &deny),
        error in LeaseError::Refs(error)
    );
    assert_refused!(
        lacks,
        repository.claim_lease_by_policy(&main, &actor, &Mode::Open, &deny),
        error in LeaseError::Refs(error)
    );
    assert_refused!(
        lacks,
        repository.release_lease(&main, &actor, &deny),
        error in LeaseError::Refs(error)
    );
    assert_refused!(
        lacks,
        repository.override_lease(&main, &actor, &register, 60, "a reason", &deny),
        error in LeaseError::Refs(error)
    );
    // The pull requests.
    assert_refused!(lacks, repository.pull_requests(), error in PullError::Refs(error));
    assert_refused!(
        lacks,
        repository.open_pull(&feature, &actor, vec![], None, None, &deny),
        error in PullError::Refs(error)
    );
    assert_refused!(
        lacks,
        repository.close_pull(1, &actor, None, &deny),
        error in PullError::Refs(error)
    );
    assert_refused!(
        lacks,
        repository.record_pull_merge(1, &actor, commit, &deny),
        error in PullError::Refs(error)
    );
    assert_refused!(
        lacks,
        repository.approve_pull(1, &actor, None, None, &deny),
        error in PullError::Refs(error)
    );
    assert_refused!(
        lacks,
        repository.recount_approvals(1, None),
        error in PullError::Refs(error)
    );
    // The checks and the bounds of the gate that read them.
    assert_refused!(
        lacks,
        repository.record_check(commit, None, CheckResult::Pass, &actor, &deny),
        error in CheckError::Refs(error)
    );
    assert_refused!(lacks, repository.checks(), error in CheckError::Refs(error));
    assert_refused!(
        lacks,
        repository.counted_checks(1, &commit, &commit, RiskLevel::Two, None),
        error in CheckError::Refs(error)
    );
    assert_refused!(
        lacks,
        repository.last_green_tip(&commit, None),
        error in CheckError::Refs(error)
    );
    assert_refused!(lacks, repository.red_window(None), error in CheckError::Refs(error));
    // The gate.
    assert_refused!(
        lacks,
        repository.merge_gate(1, RiskLevel::Two, None, &actor, &deny),
        error in GateError::Refs(error)
    );
    assert_refused!(
        lacks,
        repository.merge_gate_by_policy(1, &Mode::Open, &actor, &deny),
        error in GateError::Refs(error)
    );
    // The purge.
    assert_refused!(
        lacks,
        repository.plan_purge(blob, &actor, &deny),
        error in PurgeError::Refs(error)
    );
    assert_refused!(
        lacks,
        repository.plan_purge_with(blob, &actor, &deny, TagPolicy::Move),
        error in PurgeError::Refs(error)
    );
    assert_refused!(
        lacks,
        repository.apply_purge(blob, &actor, &deny),
        error in ApplyPurgeError::Snapshot(SnapshotError::Refs(error))
    );
    assert_refused!(
        lacks,
        repository.apply_purge_with(blob, &actor, &deny, TagPolicy::Move),
        error in ApplyPurgeError::Snapshot(SnapshotError::Refs(error))
    );
    assert_refused!(
        lacks,
        repository.resume_purge(&actor, &deny),
        error in ApplyPurgeError::Snapshot(SnapshotError::Refs(error))
    );
    assert_refused!(
        lacks,
        repository.abort_purge(&actor, &deny),
        error in ApplyPurgeError::Snapshot(SnapshotError::Refs(error))
    );

    assert_eq!(listing(repository.path()), before);
}

#[test]
fn the_references_have_no_head_and_no_working_copy_commit_to_read() {
    let dir = TempDir::new();
    let repository = bare(&dir);
    let lacks = BareLacks::WorkingCopy;

    assert_refused!(lacks, repository.refs().head(), error in error);
    assert_refused!(lacks, repository.refs().resolve_head(), error in error);
    assert_refused!(lacks, repository.refs().working_copy(), error in error);
    assert_refused!(lacks, repository.refs().working_copy_owner(), error in error);
    // The store refuses the writes as well, so no code of the crate that is not behind a check
    // of `Repository` makes one of the two files.
    let allowed = Allowed::ask(&AllowAll, &daniel(), &operation("switch"), "HEAD").unwrap();
    let head = Head::Branch(name("main"));
    let (refs, objects) = (repository.refs_store(), repository.objects());
    assert_refused!(lacks, refs.set_head(&head, objects, &allowed), error in error);
    assert_refused!(
        lacks,
        refs.replace_head(&head, &head, objects, &allowed),
        error in error
    );
    assert!(!repository.path().join("HEAD").exists());
    assert!(!repository.path().join("working-copy").exists());
    // The mode of a repository is read from the commit that HEAD names, so it has none here; a
    // caller reads the register of a commit instead.
    let error = Mode::of(&repository).unwrap_err();
    assert!(
        matches!(&error, ModeError::Head(text) if text.contains("bare repository")),
        "{error}"
    );
    // The message names the repository, what it lacks, and the operation.
    let error = repository.snapshot(&daniel(), &AllowAll).unwrap_err();
    let text = error.to_string();
    assert!(
        text.contains("hub` is a bare repository and has no working copy, which `snapshot` needs"),
        "{text}"
    );
}

#[test]
fn the_object_store_takes_and_verifies_objects_without_the_lock() {
    let dir = TempDir::new();
    let repository = bare(&dir);

    // Objects that a computation made, here a commit with its tree and its blob, lie in the same
    // store as uploaded ones would.
    let first = commit_of(&repository, &[("a.txt", "one\n")], vec![]);
    let second = commit_of(
        &repository,
        &[("a.txt", "one\n"), ("b.txt", "two\n")],
        vec![first],
    );

    let stored = repository.objects().get_commit(&second).unwrap().unwrap();
    assert_eq!(stored.parents(), [first]);
    stored.verify(repository.objects()).unwrap();
    repository
        .objects()
        .get_tree(&stored.tree())
        .unwrap()
        .unwrap()
        .verify(repository.objects())
        .unwrap();
    // A commit whose parent the store lacks does not verify.
    let other = TempDir::new();
    let elsewhere = bare(&other);
    let foreign = commit_of(&elsewhere, &[("c.txt", "three\n")], vec![]);
    let orphan = Commit::try_new(
        stored.tree(),
        vec![foreign],
        ChangeId::generate().unwrap(),
        daniel(),
        1,
        "an orphan",
    )
    .unwrap();
    assert!(orphan.verify(repository.objects()).is_err());
    // Nothing took the lock or made a file outside `objects/`.
    assert!(!repository.refs().is_locked().unwrap());
    assert!(!repository.path().join("refs.lock").exists());
    assert!(!repository.path().join("oplog").exists());
}

#[test]
fn the_references_are_read_and_verified_without_head_and_working_copy() {
    let dir = TempDir::new();
    let repository = bare(&dir);
    let commit = commit_of(&repository, &[("a.txt", "one\n")], vec![]);
    set_branch(&repository, "main", &commit);
    fs::create_dir(repository.path().join("refs/tags")).unwrap();
    fs::write(
        repository.path().join("refs/tags/v1"),
        encode_branch(&commit),
    )
    .unwrap();

    assert_eq!(
        repository.refs().branch(&name("main")).unwrap(),
        Some(commit)
    );
    assert_eq!(
        repository.refs().branches().unwrap(),
        [(name("main"), commit)]
    );
    assert_eq!(repository.refs().tags().unwrap(), [(name("v1"), commit)]);
    assert!(repository.verify_refs().unwrap().is_empty());

    // A branch and a tag that name a commit that the store lacks are found, and a missing HEAD
    // is no finding and no error.
    let other = TempDir::new();
    let foreign = commit_of(&bare(&other), &[("c.txt", "three\n")], vec![]);
    set_branch(&repository, "broken", &foreign);
    fs::write(
        repository.path().join("refs/tags/v2"),
        encode_branch(&foreign),
    )
    .unwrap();
    let errors = repository.verify_refs().unwrap();
    assert_eq!(errors.len(), 2, "{errors:?}");
    assert!(
        errors
            .iter()
            .all(|error| matches!(error, VerifyError::Missing { .. })),
        "{errors:?}"
    );
    // A reopened repository reads the same.
    let reopened = Repository::open_bare(repository.path()).unwrap();
    assert_eq!(reopened.verify_refs().unwrap().len(), 2);
}

#[test]
fn the_policy_of_a_base_commit_and_the_level_of_a_change_are_read_from_the_store() {
    let dir = TempDir::new();
    let repository = bare(&dir);
    let plain = commit_of(&repository, &[("src/a.rs", "one\n")], vec![]);
    let text = "stemma-policy 1\nlevel 0 docs/\nlevel 3 src/core/\n";
    let base = commit_of(
        &repository,
        &[("src/a.rs", "one\n"), (policy::FILE_NAME, text)],
        vec![plain],
    );
    let enforcing = Mode::Enforcing(Register::parse(register_text().as_bytes()).unwrap());

    // Without a file the mode decides; with one it is the policy of the commit in both modes.
    assert_eq!(
        repository.policy_of(&plain, &Mode::Open).unwrap(),
        policy::Policy::default()
    );
    assert!(matches!(
        repository.policy_of(&plain, &enforcing),
        Err(PolicyError::Missing)
    ));
    let read = repository.policy_of(&base, &Mode::Open).unwrap();
    assert_eq!(read, policy::Policy::parse(text.as_bytes()).unwrap());
    assert_eq!(repository.policy_of(&base, &enforcing).unwrap(), read);

    let files = [("src/a.rs", "one\n"), (policy::FILE_NAME, text)];
    let level = |added: (&str, &str)| {
        let mut after = files.to_vec();
        after.push(added);
        repository.level_of_change(&read, &base, tree_of(&repository, &after))
    };
    assert_eq!(level(("docs/guide.md", "text\n")).unwrap(), RiskLevel::Zero);
    assert_eq!(level(("src/b.rs", "two\n")).unwrap(), RiskLevel::Two);
    assert_eq!(level(("src/core/c.rs", "x\n")).unwrap(), RiskLevel::Three);
    assert!(matches!(
        repository.level_of_change(&read, &base, tree_of(&repository, &files)),
        Err(LevelError::NoChange)
    ));
    // The escalation when tests are weakened reads the two contents and nothing else.
    let weakening = read.weakening(&[FileChange {
        path: "src/a.rs",
        old: Some(b"#[test]\nfn t() {\n    assert!(a());\n    assert!(b());\n}\n"),
        new: Some(b"#[test]\nfn t() {\n    assert!(a());\n}\n"),
    }]);
    assert_eq!(weakening.level, read.weakened_level());
    assert!(!weakening.reasons.is_empty());
    assert!(!repository.path().join("refs.lock").exists());
}

#[test]
fn the_authorizer_is_built_from_the_register_of_a_base_commit() {
    let dir = TempDir::new();
    let repository = bare(&dir);
    let open = commit_of(&repository, &[("a.txt", "one\n")], vec![]);
    let text = register_text();
    let base = commit_of(
        &repository,
        &[("a.txt", "one\n"), (register::FILE, &text)],
        vec![open],
    );
    let target = operation("push");

    // A commit without a register: the open mode, where the actor is a claim.
    assert_eq!(
        Register::at_commit(repository.objects(), &open).unwrap(),
        None
    );
    let mode = Mode::Open;
    let authorizer = RegisterAuthorizer::new(&mode, None);
    assert_eq!(
        authorizer.authorize(&daniel(), &target, "main"),
        Decision::Allow
    );
    // A commit with a register: the enforcing mode, which refuses an actor without a proof.
    let found = Register::at_commit(repository.objects(), &base)
        .unwrap()
        .unwrap();
    assert!(found.is_owner(&daniel()));
    let mode = Mode::Enforcing(found);
    let authorizer = RegisterAuthorizer::new(&mode, None);
    assert_eq!(
        authorizer.authorize(&daniel(), &target, "main"),
        Decision::Deny
    );
}

#[test]
fn the_tree_merge_runs_on_the_store_and_moves_no_reference() {
    let dir = TempDir::new();
    let repository = bare(&dir);
    let base = commit_of(
        &repository,
        &[("a.txt", "one\n"), ("b.txt", "two\n")],
        vec![],
    );
    let ours = commit_of(
        &repository,
        &[("a.txt", "ours\n"), ("b.txt", "two\n")],
        vec![base],
    );
    let theirs = commit_of(
        &repository,
        &[("a.txt", "one\n"), ("b.txt", "theirs\n")],
        vec![base],
    );
    let clash = commit_of(
        &repository,
        &[("a.txt", "clash\n"), ("b.txt", "two\n")],
        vec![base],
    );
    set_branch(&repository, "main", &ours);
    let before = repository.refs().branches().unwrap();

    let merged = merge_commits(repository.objects(), ours, theirs).unwrap();
    assert!(merged.is_resolved());
    let expected = tree_of(&repository, &[("a.txt", "ours\n"), ("b.txt", "theirs\n")]);
    assert_eq!(merged, crate::conflict::Merge::resolved(expected));
    // A conflict is data of the result, as in a repository with a working copy.
    let conflicted = merge_commits(repository.objects(), ours, clash).unwrap();
    assert!(!conflicted.is_resolved());

    assert_eq!(repository.refs().branches().unwrap(), before);
    assert!(!repository.path().join("refs.lock").exists());
    assert!(!repository.path().join("oplog").exists());
}
