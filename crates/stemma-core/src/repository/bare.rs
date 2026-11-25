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

/// The operations that the reference store of a bare repository names when it refuses a read or
/// a write of `HEAD` or of the working-copy commit itself.
const STORE_OPERATIONS: [&str; 3] = ["read HEAD", "set HEAD", "read the working-copy commit"];

/// Checks that `$result` is the error `$pattern`, which binds `$error` to a [`RefError::Bare`]
/// that names `$lacks`, and that the failure is not classified as a lock or as damage.
macro_rules! assert_refused {
    ($lacks:expr, $result:expr, $error:ident in $pattern:pat) => {
        assert_refused!(false, $lacks, $result, $error in $pattern)
    };
    ($from_store:expr, $lacks:expr, $result:expr, $error:ident in $pattern:pat) => {{
        let result = $result.map(|_| ());
        let message = result.as_ref().err().map(ToString::to_string);
        match result {
            Err($pattern) => {
                assert_eq!($error.kind(), FailureKind::Other, "{}", $error);
                // The message is the refusal alone: no prefix says that a read or a write
                // failed, as nothing was read or written.
                assert_eq!(message, Some($error.to_string()));
                match $error {
                    RefError::Bare {
                        path,
                        operation,
                        lacks,
                    } => {
                        assert_eq!(lacks, $lacks, "{operation}");
                        assert!(path.ends_with("hub"), "{}", path.display());
                        // The check of `Repository` refused, not the reference store behind it,
                        // which names the file that it was asked for.
                        assert_eq!(
                            STORE_OPERATIONS.contains(&operation),
                            $from_store,
                            "{operation}"
                        );
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
    // The temporary directory of `init_bare` is gone: nothing else lies beside the repository.
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
    // Nothing is left of a refused call, its temporary directory included.
    assert_eq!(listing(dir.path()), before);
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
    // The reader has the rule of the writer: `init_bare` writes no version before the marker
    // existed, so the marker beside such a version, written by hand, is damage.
    for version in ["2", "13", "14"] {
        fs::write(
            repository.path().join("format"),
            format!("stemma-repository {version}\n"),
        )
        .unwrap();
        let error = Repository::open_bare(repository.path()).unwrap_err();
        assert!(
            matches!(&error, OpenError::BareFormat { found, .. } if found == version),
            "{error}"
        );
        assert_eq!(error.kind(), FailureKind::Damaged);
    }
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

    assert_refused!(true, lacks, repository.refs().head(), error in error);
    assert_refused!(true, lacks, repository.refs().resolve_head(), error in error);
    assert_refused!(true, lacks, repository.refs().working_copy(), error in error);
    assert_refused!(true, lacks, repository.refs().working_copy_owner(), error in error);
    // The store refuses the writes as well, so no code of the crate that is not behind a check
    // of `Repository` makes one of the two files.
    let allowed = Allowed::ask(&AllowAll, &daniel(), &operation("switch"), "HEAD").unwrap();
    let head = Head::Branch(name("main"));
    let (refs, objects) = (repository.refs_store(), repository.objects());
    assert_refused!(true, lacks, refs.set_head(&head, objects, &allowed), error in error);
    assert_refused!(
        true,
        lacks,
        refs.replace_head(&head, &head, objects, &allowed),
        error in error
    );
    // The store that reads the pointers of all worktrees stays the store of a bare repository.
    assert_refused!(true, lacks, refs.all_pointers(), error in error);
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
    // Nothing took the lock, whose guard would stay, or made a log.
    assert!(!repository.refs().is_locked().unwrap());
    assert!(!repository.path().join("refs.lock").exists());
    assert!(!repository.path().join("refs.lock.guard").exists());
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

/// Records what it is asked and allows it.
#[derive(Default)]
struct Recording(std::sync::Mutex<Vec<(String, String)>>);

impl Authorizer for Recording {
    fn authorize(&self, _actor: &Actor, operation: &OperationName, target: &str) -> Decision {
        self.0
            .lock()
            .unwrap()
            .push((operation.to_string(), target.to_owned()));
        Decision::Allow
    }
}

#[test]
fn init_bare_asks_for_init_with_the_directory_as_the_target() {
    let dir = TempDir::new();
    let root = dir.path().join("hub");
    let asked = Recording::default();

    Repository::init_bare(&root, &daniel(), &asked).unwrap();

    assert_eq!(
        *asked.0.lock().unwrap(),
        [("init".to_owned(), root.display().to_string())]
    );
}

#[test]
fn a_refused_operation_does_not_ask_the_authorizer() {
    let dir = TempDir::new();
    let repository = bare(&dir);
    let commit = commit_of(&repository, &[("a.txt", "one\n")], vec![]);
    let asked = Recording::default();

    assert_refused!(
        BareLacks::WorkingCopy,
        repository.snapshot(&daniel(), &asked),
        error in SnapshotError::Refs(error)
    );
    assert_refused!(
        BareLacks::OperationLog,
        repository.update_branch(&operation("branch"), &name("main"), None, commit, &daniel(), &asked),
        error in UpdateError::Refs(error)
    );

    assert!(asked.0.lock().unwrap().is_empty());
}

#[test]
fn the_temporary_directory_of_init_bare_is_none_that_a_writer_removes_as_stale() {
    // A writer of a repository removes the directories of `init` next to its `.stemma`
    // (ADR-009). A bare repository may be made in such a directory, so its name differs.
    let made = format!("{BARE_STAGING_PREFIX}{}", unique_temp_name());
    assert!(!crate::store::is_init_staging_name(&made), "{made}");

    let dir = TempDir::new();
    let work = Repository::init(dir.path(), &InitOptions::default(), &daniel(), &AllowAll).unwrap();
    let staging = dir.path().join(&made);
    fs::create_dir(&staging).unwrap();
    // A directory of `init` beside it shows that the cleanup ran.
    let stale = dir.path().join(".stemma-init-1-0-0123456789abcdef");
    fs::create_dir(&stale).unwrap();
    // A writer takes the lock and cleans up.
    work.snapshot(&daniel(), &AllowAll).unwrap();
    fs::write(dir.path().join("a.txt"), "one\n").unwrap();
    work.track(&["a.txt".to_owned()], &daniel(), &AllowAll)
        .unwrap();
    assert!(!stale.exists());
    assert!(staging.is_dir());
}

#[test]
fn init_bare_builds_in_a_directory_of_its_own_name_beside_the_target() {
    // The name that `init_bare` really uses: the directory above refuses a new name, so the
    // error names the temporary directory.
    let dir = TempDir::new();
    let denied = crate::fault::deny_writes_in(dir.path());
    let error = Repository::init_bare(dir.path().join("hub"), &daniel(), &AllowAll).unwrap_err();
    drop(denied);
    let InitError::Io { path, .. } = &error else {
        panic!("{error:?}");
    };
    let made = path.file_name().unwrap().to_str().unwrap();
    assert!(made.starts_with(BARE_STAGING_PREFIX), "{made}");
    assert!(!crate::store::is_init_staging_name(made), "{made}");
    assert!(fs::read_dir(dir.path()).unwrap().next().is_none());
}

#[test]
fn init_bare_reports_a_final_flush_that_failed_with_the_directory_above() {
    let dir = TempDir::new();
    let root = dir.path().join("hub");
    let failure = crate::store::fail_flush_of(dir.path());
    let error = Repository::init_bare(&root, &daniel(), &AllowAll).unwrap_err();
    drop(failure);
    assert!(
        matches!(&error, InitError::NotDurable { path, .. } if path == dir.path()),
        "{error:?}"
    );
    // The repository is complete; only its name in the directory above may be lost.
    assert!(Repository::open_bare(&root).unwrap().is_bare());
}

#[test]
fn a_bare_repository_that_cannot_be_installed_leaves_no_temporary_directory() {
    // The content cannot be written.
    let dir = TempDir::new();
    let staging = dir.path().join("staging");
    let target = dir.path().join("hub");
    fs::create_dir(&staging).unwrap();
    let denied = crate::fault::deny_writes_in(&staging);
    let error = install_with(&staging, &target, &Allowed::granted(), fill_bare).unwrap_err();
    drop(denied);
    assert!(matches!(error, InitError::Io { .. }), "{error:?}");
    assert!(!staging.exists());
    assert!(!target.exists());

    // Something appeared at the target since the check.
    fs::create_dir(&staging).unwrap();
    fs::create_dir(&target).unwrap();
    fs::write(target.join("other"), "x").unwrap();
    let error = install_with(&staging, &target, &Allowed::granted(), fill_bare).unwrap_err();
    assert!(
        matches!(&error, InitError::AlreadyExists { path } if path == &target),
        "{error:?}"
    );
    assert!(!staging.exists());
    assert_eq!(fs::read(target.join("other")).unwrap(), b"x");
}

#[test]
fn a_bare_repository_has_no_remotes_and_no_remote_references() {
    let dir = TempDir::new();
    let repository = bare(&dir);
    let before = listing(repository.path());

    assert!(repository.remotes().unwrap().is_empty());
    assert!(repository.refs().remote_refs().unwrap().is_empty());
    let origin = crate::remote::RemoteName::try_new("origin").unwrap();
    assert!(repository.remote(&origin).unwrap().is_none());

    assert_eq!(listing(repository.path()), before);
}

#[test]
fn discover_finds_no_repository_in_a_bare_one() {
    // The directory is the metadata directory itself; `discover` looks for a `.stemma`.
    let dir = TempDir::new();
    let repository = bare(&dir);
    for start in [
        repository.path().to_path_buf(),
        repository.path().join("objects"),
    ] {
        let error = Repository::discover(&start).unwrap_err();
        assert!(
            matches!(error, OpenError::NoRepositoryAbove { .. }),
            "{error}"
        );
    }
}

#[test]
fn the_step_to_the_enforcing_mode_is_refused_and_writes_nothing() {
    let dir = TempDir::new();
    let repository = bare(&dir);
    let before = listing(repository.path());
    let register = Register::parse(register_text().as_bytes()).unwrap();

    let error = register::enter_enforcing(&repository, &register).unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    let inner = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<RefError>());
    assert!(
        matches!(
            inner,
            Some(RefError::Bare {
                lacks: BareLacks::WorkingCopy,
                ..
            })
        ),
        "{error}"
    );
    assert_eq!(listing(repository.path()), before);
    assert!(!register::is_enforcing(repository.path()).unwrap());
}

#[test]
fn the_checks_run_while_a_writer_holds_the_lock_of_the_references() {
    let dir = TempDir::new();
    let repository = bare(&dir);
    let base = commit_of(&repository, &[("a.txt", "one\n")], vec![]);
    let ours = commit_of(&repository, &[("a.txt", "ours\n")], vec![base]);
    let theirs = commit_of(
        &repository,
        &[("a.txt", "one\n"), ("b.txt", "two\n")],
        vec![base],
    );
    set_branch(&repository, "main", &base);
    let allowed = Allowed::ask(&AllowAll, &daniel(), &operation("push"), "main").unwrap();
    let lock = repository.refs_store().lock_writer(&allowed).unwrap();
    assert!(repository.refs().is_locked().unwrap());

    // None of them waits for the lock or fails on it.
    let stored = commit_of(&repository, &[("c.txt", "three\n")], vec![base]);
    assert!(repository.objects().get_commit(&stored).unwrap().is_some());
    assert!(merge_commits(repository.objects(), ours, theirs)
        .unwrap()
        .is_resolved());
    let policy = repository.policy_of(&base, &Mode::Open).unwrap();
    let tree = tree_of(&repository, &[("a.txt", "ours\n")]);
    assert_eq!(
        repository.level_of_change(&policy, &base, tree).unwrap(),
        RiskLevel::Two
    );
    assert_eq!(
        Register::at_commit(repository.objects(), &base).unwrap(),
        None
    );
    assert!(repository.verify_refs().unwrap().is_empty());
    assert_eq!(
        repository.refs().branches().unwrap(),
        [(name("main"), base)]
    );

    drop(lock);
    assert!(!repository.refs().is_locked().unwrap());
}

#[test]
fn a_linked_worktree_whose_shared_directory_has_the_marker_is_refused() {
    let dir = TempDir::new();
    let main = dir.path().join("main");
    let work = Repository::init(&main, &InitOptions::default(), &daniel(), &AllowAll).unwrap();
    fs::write(main.join("a.txt"), "one\n").unwrap();
    work.track(&["a.txt".to_owned()], &daniel(), &AllowAll)
        .unwrap();
    let _ = work.commit("first", &daniel(), &AllowAll).unwrap();
    let linked = dir.path().join("linked");
    let _ = work
        .add_worktree(
            &linked,
            &WorktreeBranch::Create(name("feature")),
            None,
            &daniel(),
            &AllowAll,
        )
        .unwrap();
    Repository::open(&linked).unwrap();
    // A state that no call makes: the marker in the directory that the worktrees share.
    fs::write(main.join(METADATA_DIR).join("bare"), "stemma-bare\n").unwrap();

    for path in [&linked, &main] {
        let error = Repository::open(path).unwrap_err();
        assert!(matches!(error, OpenError::Bare { .. }), "{error}");
    }
}

#[cfg(unix)]
#[test]
fn a_link_is_no_marker_and_no_free_place_for_a_bare_repository() {
    use std::os::unix::fs::symlink;
    let dir = TempDir::new();
    let repository = bare(&dir);
    // A link to a file with the content of the marker is not followed: it is damage.
    let marker = repository.path().join("bare");
    let elsewhere = dir.path().join("marker");
    fs::rename(&marker, &elsewhere).unwrap();
    symlink(&elsewhere, &marker).unwrap();
    let error = Repository::open_bare(repository.path()).unwrap_err();
    assert!(
        matches!(error, OpenError::MalformedBareMarker { .. }),
        "{error}"
    );
    // A link in the place of a new bare repository is in the way, whether it leads somewhere or
    // not, and `open_bare` names a link that leads nowhere.
    let dangling = dir.path().join("dangling");
    symlink(dir.path().join("nowhere"), &dangling).unwrap();
    let to_directory = dir.path().join("to-directory");
    symlink(dir.path(), &to_directory).unwrap();
    for path in [&dangling, &to_directory] {
        let error = Repository::init_bare(path, &daniel(), &AllowAll).unwrap_err();
        assert!(matches!(error, InitError::AlreadyExists { .. }), "{error}");
    }
    let error = Repository::open_bare(&dangling).unwrap_err();
    assert!(matches!(error, OpenError::NotADirectory { .. }), "{error}");
    // A path through a file names nothing.
    let error = Repository::open_bare(elsewhere.join("hub")).unwrap_err();
    assert!(matches!(error, OpenError::Missing { .. }), "{error}");
}

/// The public methods of `Repository` that run on a bare repository, or that make or open a
/// repository. Every other public method starts with the check of the bare repository.
const WITHOUT_CHECK: [&str; 25] = [
    "init",
    "init_bare",
    "open",
    "open_bare",
    "discover",
    "path",
    "metadata_dir",
    "worktree_name",
    "is_bare",
    "objects",
    "refs",
    "verify_refs",
    "oplog",
    "taking_over",
    "taken_over",
    "policy_of",
    "level_of_change",
    "remote",
    "remotes",
    "receive_objects",
    "check_connectivity",
    "remove_receive_leftovers",
    "answer_refs",
    "answer_fetch",
    "accept_ref_update",
];

/// The public methods of the `impl Repository` blocks in `source`, each with the first statement
/// of its body, joined into one line. The reading relies on the layout of rustfmt.
fn public_methods(source: &str) -> Vec<(String, String)> {
    let lines: Vec<&str> = source.lines().collect();
    let mut found = Vec::new();
    let mut inside = false;
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if line == "impl Repository {" {
            inside = true;
        } else if line == "}" {
            inside = false;
        } else if inside {
            let modifiers = ["const ", "async ", "unsafe "];
            let rest = line.strip_prefix("    pub ").map(|rest| {
                modifiers
                    .iter()
                    .fold(rest, |rest, word| rest.strip_prefix(word).unwrap_or(rest))
            });
            if let Some(rest) = rest.and_then(|rest| rest.strip_prefix("fn ")) {
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                while !lines[index].ends_with('{') {
                    index += 1;
                }
                let mut first = String::new();
                while !first.ends_with(';') && !first.ends_with('}') {
                    index += 1;
                    first.push_str(lines[index].trim());
                }
                found.push((name, first));
            }
        }
        index += 1;
    }
    found
}

#[test]
fn every_public_operation_starts_with_the_check_of_the_bare_repository() {
    // A method that is added without the check would run on a bare repository until it meets a
    // missing file; this test names it instead.
    // Every source of the crate: a block in a new module or directory is read too.
    let mut sources = Vec::new();
    let mut directories = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("src")];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                directories.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                sources.push(path);
            }
        }
    }
    let mut checked = 0;
    let mut seen = BTreeSet::new();
    for path in sources {
        for (name, first) in public_methods(&fs::read_to_string(&path).unwrap()) {
            seen.insert(name.clone());
            if WITHOUT_CHECK.contains(&name.as_str()) {
                continue;
            }
            // The statement ends with `?`: a check whose result is dropped refuses nothing.
            assert!(
                (first.starts_with("self.require_working_copy(\"")
                    || first.starts_with("self.require_oplog(\""))
                    && first.ends_with("?;"),
                "`{name}` in {} does not start with the check: `{first}`",
                path.display()
            );
            checked += 1;
        }
    }
    // The reading found the methods: a change of the layout must not empty the test.
    assert!(checked >= 50, "{checked}");
    for name in WITHOUT_CHECK {
        assert!(seen.contains(name), "{name}");
    }
}

#[test]
fn an_entry_of_the_log_is_refused_even_past_the_checks_of_the_operations() {
    let dir = TempDir::new();
    let repository = bare(&dir);
    // A directory that someone gave a log by hand: without the refusal an entry would be
    // written into it.
    fs::create_dir(repository.path().join("oplog")).unwrap();
    let allowed = Allowed::ask(&AllowAll, &daniel(), &operation("push"), "main").unwrap();
    let lock = repository.refs_store().lock_writer(&allowed).unwrap();
    let commit = commit_of(&repository, &[("a.txt", "one\n")], vec![]);
    let change = Change::try_new(
        1,
        daniel(),
        operation("push"),
        None,
        None,
        vec![(name("main"), Moved::from_to(None, Some(commit)))],
    )
    .unwrap();

    let lease = LeaseEntry::release(1, daniel(), name("main"));
    let pull = PullEntry::close(1, daniel(), 1).unwrap();
    let check = CheckEntry::new(1, daniel(), commit, None, CheckResult::Pass);

    let errors = [
        repository.append(&lock, &change).unwrap_err(),
        repository.append_lease(&lock, &lease).unwrap_err(),
        repository.append_pull(&lock, &pull).unwrap_err(),
        repository.append_check(&lock, &check).unwrap_err(),
    ];

    for error in &errors {
        let OpLogError::Io { source, .. } = error else {
            panic!("{error:?}");
        };
        assert_eq!(source.kind(), io::ErrorKind::Unsupported);
        assert!(matches!(
            source
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<RefError>()),
            Some(RefError::Bare {
                lacks: BareLacks::OperationLog,
                ..
            })
        ));
    }
    drop(lock);
    // No entry and no tip were written.
    assert!(fs::read_dir(repository.path().join("oplog"))
        .unwrap()
        .next()
        .is_none());
    assert!(!repository.path().join("oplog-tip").exists());
}

#[test]
fn the_message_of_an_unsupported_version_names_the_versions_that_are_read() {
    let text = supported_versions();
    for version in READ_VERSIONS {
        let named = text.split(" and ").any(|run| match run.split_once(" to ") {
            Some((first, last)) => {
                (first.parse::<u32>().unwrap()..=last.parse().unwrap()).contains(&version)
            }
            None => run.parse::<u32>().unwrap() == version,
        });
        assert!(named, "{version} in `{text}`");
    }
    // A version between two that are read is not named as supported: the text has one run for
    // each run of versions that follow each other.
    let breaks = READ_VERSIONS
        .windows(2)
        .filter(|pair| pair[1] != pair[0] + 1)
        .count();
    assert_eq!(text.matches(" and ").count(), breaks, "{text}");
}
