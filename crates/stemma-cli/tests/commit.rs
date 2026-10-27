//! The command `stemma commit`, run as a process, as a person or an agent would run it.

mod common;

use std::fs;
use std::path::Path;
#[cfg(target_os = "linux")]
use std::process::Command;

use stemma_core::object::{Commit, CommitId, Kind};
use stemma_core::refs::BranchName;
use stemma_core::repository::Repository;
use stemma_core::store::ObjectStore;

use common::{operations, repository, run, stemma, track_ok, write, Dir, Run};

fn commit(dir: &Path, message: &str) -> Run {
    run(dir, &["commit", "-m", message, "--actor", "human alice"])
}

fn main_branch() -> BranchName {
    BranchName::new("main").unwrap()
}

fn read(repository: &Repository, id: CommitId) -> Commit {
    let raw = repository
        .objects()
        .get_expecting(&id.object_id(), Kind::Commit)
        .unwrap()
        .unwrap();
    Commit::parse(raw.body()).unwrap()
}

/// The ID that a successful `commit` printed.
fn printed_id(result: &Run) -> String {
    let line = result.stdout.lines().next().unwrap();
    line.split(' ').nth(1).unwrap().to_owned()
}

/// The refusals leave the commit unmade: no branch, and the log as it was.
fn assert_nothing_committed(dir: &Path, entries: usize) {
    let repository = Repository::open(dir).unwrap();
    assert_eq!(repository.refs().branch(&main_branch()).unwrap(), None);
    assert_eq!(operations(dir).len(), entries);
}

#[test]
fn a_commit_names_the_working_copy_and_starts_a_new_one() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    track_ok(dir.path(), "a.txt");

    let result = commit(dir.path(), "Add a");

    assert_eq!(result.code, 0, "{}", result.stderr);
    let id = printed_id(&result);
    assert_eq!(result.stdout, format!("Committed {id} on main\n"));
    assert_eq!(result.stderr, "");
    let repository = Repository::open(dir.path()).unwrap();
    let branch = repository.refs().branch(&main_branch()).unwrap().unwrap();
    assert_eq!(branch.to_string(), id);
    let named = read(&repository, branch);
    assert_eq!(named.message(), "Add a");
    assert_eq!(named.author().to_string(), "human alice");
    let working = read(&repository, repository.refs().working_copy().unwrap());
    assert_eq!(working.parents(), [branch]);
    assert_eq!(working.tree(), named.tree());
    assert_eq!(working.message(), "");
    assert_ne!(working.change(), named.change());
    let log = operations(dir.path());
    assert_eq!(log.len(), 3, "{log:?}");
    assert!(
        log[0].contains("\"actor\":\"human alice\"") && log[0].contains("\"operation\":\"commit\""),
        "{log:?}"
    );
    assert!(log[0].contains("\"took_effect\":true"), "{log:?}");
}

#[test]
fn the_long_form_of_the_message_flag_works() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    track_ok(dir.path(), "a.txt");

    let result = run(
        dir.path(),
        &["commit", "--message", "Add a", "--actor", "human alice"],
    );

    assert_eq!(result.code, 0, "{}", result.stderr);
}

#[test]
fn a_change_since_the_last_command_is_committed_after_its_own_snapshot_entry() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    track_ok(dir.path(), "a.txt");
    let first = commit(dir.path(), "one");
    write(dir.path(), "a.txt", "two");

    let second = commit(dir.path(), "two");

    assert_eq!((first.code, second.code), (0, 0), "{}", second.stderr);
    let repository = Repository::open(dir.path()).unwrap();
    let tip = repository.refs().branch(&main_branch()).unwrap().unwrap();
    assert_eq!(tip.to_string(), printed_id(&second));
    let parents = read(&repository, tip).parents().to_vec();
    assert_eq!(parents.len(), 1);
    assert_eq!(parents[0].to_string(), printed_id(&first));
    let log = operations(dir.path());
    // init, track, commit, then the snapshot and the commit.
    assert_eq!(log.len(), 5, "{log:?}");
    assert!(log[0].contains("\"operation\":\"commit\""), "{log:?}");
    assert!(log[1].contains("\"operation\":\"snapshot\""), "{log:?}");
    assert!(log[1].contains("\"actor\":\"human alice\""), "{log:?}");
}

#[test]
fn deleting_the_tracked_file_is_a_change_to_commit() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    track_ok(dir.path(), "a.txt");
    assert_eq!(commit(dir.path(), "one").code, 0);
    fs::remove_file(dir.path().join("a.txt")).unwrap();

    let result = commit(dir.path(), "remove a");

    assert_eq!(result.code, 0, "{}", result.stderr);
}

fn assert_refused_for_an_empty_message(message: &str) {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    track_ok(dir.path(), "a.txt");
    let entries = operations(dir.path()).len();

    let result = commit(dir.path(), message);

    assert_eq!(result.code, 1, "{}", result.stdout);
    assert_eq!(result.stdout, "");
    assert!(
        result.stderr.starts_with("stemma: ") && result.stderr.contains("must not be empty"),
        "{}",
        result.stderr
    );
    assert_nothing_committed(dir.path(), entries);
}

#[test]
fn an_empty_message_is_refused() {
    assert_refused_for_an_empty_message("");
}

#[test]
fn a_message_of_white_space_is_refused() {
    assert_refused_for_an_empty_message("  \n\t");
}

#[test]
fn nothing_to_commit_in_a_new_repository_is_refused() {
    let dir = repository();
    let entries = operations(dir.path()).len();

    let result = commit(dir.path(), "nothing");

    assert_eq!(result.code, 1);
    assert_eq!(result.stdout, "");
    assert!(
        result.stderr.starts_with("stemma: ") && result.stderr.contains("nothing to commit"),
        "{}",
        result.stderr
    );
    assert_nothing_committed(dir.path(), entries);
}

#[test]
fn nothing_to_commit_after_a_commit_is_refused_without_an_entry() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    track_ok(dir.path(), "a.txt");
    assert_eq!(commit(dir.path(), "one").code, 0);
    let entries = operations(dir.path()).len();

    let result = commit(dir.path(), "again");

    assert_eq!(result.code, 1);
    assert!(
        result.stderr.contains("nothing to commit"),
        "{}",
        result.stderr
    );
    assert_eq!(operations(dir.path()).len(), entries);
}

#[test]
fn a_file_that_is_not_tracked_is_not_committed() {
    let dir = repository();
    write(dir.path(), "untracked.txt", "x");
    let entries = operations(dir.path()).len();

    let result = commit(dir.path(), "nothing tracked");

    assert_eq!(result.code, 1);
    assert!(
        result.stderr.contains("nothing to commit"),
        "{}",
        result.stderr
    );
    assert_nothing_committed(dir.path(), entries);
}

#[test]
fn a_commit_that_is_refused_after_the_snapshot_leaves_the_snapshot_entry() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    track_ok(dir.path(), "a.txt");
    assert_eq!(commit(dir.path(), "one").code, 0);
    // Someone points main at the working-copy commit, past the parent that it is based on.
    let metadata = dir.path().join(".stemma");
    fs::copy(
        metadata.join("working-copy"),
        metadata.join("refs/heads/main"),
    )
    .unwrap();
    let entries = operations(dir.path()).len();
    write(dir.path(), "a.txt", "two");

    let result = commit(dir.path(), "two");

    assert_eq!(result.code, 1, "{}", result.stdout);
    assert_eq!(result.stdout, "");
    assert!(
        result.stderr.starts_with("stemma: ") && result.stderr.contains("branch `main`"),
        "{}",
        result.stderr
    );
    let log = operations(dir.path());
    assert_eq!(log.len(), entries + 1, "{log:?}");
    assert!(log[0].contains("\"operation\":\"snapshot\""), "{log:?}");
}

#[test]
fn a_head_that_names_a_commit_is_refused() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    track_ok(dir.path(), "a.txt");
    let first = commit(dir.path(), "one");
    let id = printed_id(&first);
    fs::write(dir.path().join(".stemma/HEAD"), format!("commit {id}\n")).unwrap();
    write(dir.path(), "a.txt", "two");
    let entries = operations(dir.path()).len();

    let result = commit(dir.path(), "two");

    assert_eq!(result.code, 1, "{}", result.stdout);
    assert_eq!(result.stdout, "");
    assert!(result.stderr.contains("not a branch"), "{}", result.stderr);
    assert_eq!(operations(dir.path()).len(), entries);
}

#[test]
fn without_a_message_the_command_is_a_usage_error() {
    let dir = repository();

    let result = run(dir.path(), &["commit", "--actor", "human alice"]);

    assert_eq!(result.code, 2, "{}", result.stderr);
    assert_eq!(result.stdout, "");
}

#[test]
fn without_an_actor_the_command_fails_and_commits_nothing() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    track_ok(dir.path(), "a.txt");
    let entries = operations(dir.path()).len();

    let result = run(dir.path(), &["commit", "-m", "x"]);

    assert_eq!(result.code, 1);
    assert!(result.stderr.contains("no actor"), "{}", result.stderr);
    assert_nothing_committed(dir.path(), entries);
}

#[test]
fn outside_a_repository_the_command_fails_and_names_the_flag() {
    let elsewhere = Dir::new();

    let result = commit(elsewhere.path(), "x");

    assert_eq!(result.code, 1);
    assert!(
        result.stderr.contains("is not in a repository") && result.stderr.contains("`-C`"),
        "{}",
        result.stderr
    );
}

#[test]
fn the_directory_flag_and_the_environment_variable_are_used() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    track_ok(dir.path(), "a.txt");
    let elsewhere = Dir::new();

    let result = stemma(
        elsewhere.path(),
        &["commit", "-C", dir.path().to_str().unwrap(), "-m", "Add a"],
        |command| {
            command.env("STEMMA_ACTOR", "human bob");
        },
    );

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(operations(dir.path())[0].contains("\"actor\":\"human bob\""));
}

#[test]
fn the_flag_goes_before_the_variable() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    track_ok(dir.path(), "a.txt");

    let result = stemma(
        dir.path(),
        &["commit", "-m", "Add a", "--actor", "human alice"],
        |command| {
            command.env("STEMMA_ACTOR", "human bob");
        },
    );

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(operations(dir.path())[0].contains("\"actor\":\"human alice\""));
}

#[test]
fn a_commit_that_stopped_between_its_moves_is_finished_by_the_next_one() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    track_ok(dir.path(), "a.txt");
    let repository = Repository::open(dir.path()).unwrap();
    let before = repository.refs().working_copy().unwrap();
    assert_eq!(commit(dir.path(), "stopped").code, 0);
    let named_id = repository.refs().branch(&main_branch()).unwrap().unwrap();
    let named = read(&repository, named_id);
    // The state of a stop after the move of the branch: the entry names both moves and the branch
    // has the named commit, but the working-copy commit is still the one it was made from. The
    // file is written directly, as a crash between the moves would leave it; it is no interface.
    // The log reads the entry as not taken because the pointer is at the old value of the entry.
    fs::write(
        dir.path().join(".stemma").join("working-copy"),
        format!("{before}\n"),
    )
    .unwrap();
    let entries = operations(dir.path()).len();

    let result = commit(dir.path(), "ignored");

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(
        result.stdout,
        format!(
            "Finished the commit {named_id} on main that had stopped; the message of this call \
             was not used\n"
        )
    );
    assert!(result.stderr.contains("warning"), "{}", result.stderr);
    assert_eq!(
        repository.refs().branch(&main_branch()).unwrap(),
        Some(named_id)
    );
    assert_eq!(read(&repository, named_id).message(), "stopped");
    let next = read(&repository, repository.refs().working_copy().unwrap());
    assert_eq!(next.parents(), [named_id]);
    assert_eq!(next.tree(), named.tree());
    assert_eq!(next.message(), "");
    assert_ne!(next.change(), named.change());
    let log = operations(dir.path());
    assert_eq!(log.len(), entries + 1, "{log:?}");
    // The repository works again.
    write(dir.path(), "a.txt", "three");
    assert_eq!(commit(dir.path(), "three").code, 0);
}

#[test]
fn a_stop_is_finished_also_after_a_change_since_the_stop() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    track_ok(dir.path(), "a.txt");
    let repository = Repository::open(dir.path()).unwrap();
    let before = repository.refs().working_copy().unwrap();
    assert_eq!(commit(dir.path(), "stopped").code, 0);
    let named_id = repository.refs().branch(&main_branch()).unwrap().unwrap();
    // A stop between the moves, written as in the test above, and then a change of a file.
    fs::write(
        dir.path().join(".stemma").join("working-copy"),
        format!("{before}\n"),
    )
    .unwrap();
    write(dir.path(), "a.txt", "two");

    let result = commit(dir.path(), "ignored");

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(
        result.stdout.starts_with(&format!(
            "Finished the commit {named_id} on main that had stopped"
        )),
        "{}",
        result.stdout
    );
    assert!(result.stderr.contains("warning"), "{}", result.stderr);
    let next = read(&repository, repository.refs().working_copy().unwrap());
    assert_eq!(next.parents(), [named_id]);
    assert_ne!(next.tree(), read(&repository, named_id).tree());
    // The change is kept: the next commit records it.
    assert_eq!(commit(dir.path(), "two").code, 0);
    let tip = repository.refs().branch(&main_branch()).unwrap().unwrap();
    assert_eq!(read(&repository, tip).tree(), next.tree());
}

/// `/dev/full` fails every write: the commit is made, and the command says so with code 3.
#[cfg(target_os = "linux")]
#[test]
fn a_result_that_cannot_be_written_ends_with_code_3_after_the_commit() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    track_ok(dir.path(), "a.txt");
    let full = fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_stemma"))
        .current_dir(dir.path())
        .env_remove("STEMMA_ACTOR")
        .args(["commit", "-m", "Add a", "--actor", "human alice"])
        .stdout(full)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with("stemma: committed b3:") && stderr.contains("could not write"),
        "{stderr}"
    );
    assert!(operations(dir.path())[0].contains("\"operation\":\"commit\""));
    let repository = Repository::open(dir.path()).unwrap();
    assert!(repository.refs().branch(&main_branch()).unwrap().is_some());
}
