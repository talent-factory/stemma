//! The command `stemma merge`, run as a process, as a person or an agent would run it.

mod common;

use std::fs;
use std::path::Path;

use common::{ok, run, write, Dir, ACTOR};

fn read(dir: &Path, path: &str) -> String {
    fs::read_to_string(dir.join(path)).unwrap()
}

/// The number of the newest entry of the log.
fn newest(dir: &Path) -> u64 {
    let repository = stemma_core::repository::Repository::open(dir).unwrap();
    repository
        .oplog()
        .entries()
        .unwrap()
        .last()
        .unwrap()
        .number()
}

/// A repository on `main` with `a.txt` committed as three lines. `feature` changes the first
/// line, `other` the last line, and `clash` the first line in another way.
fn history() -> Dir {
    let dir = Dir::new();
    let path = dir.path();
    ok(path, &["init", "--actor", ACTOR]);
    write(path, "a.txt", "one\ntwo\nthree\n");
    ok(path, &["track", "a.txt", "--actor", ACTOR]);
    ok(path, &["commit", "-m", "first", "--actor", ACTOR]);
    for (branch, text) in [
        ("feature", "ONE\ntwo\nthree\n"),
        ("other", "one\ntwo\nTHREE\n"),
        ("clash", "uno\ntwo\nthree\n"),
    ] {
        ok(path, &["switch", "-c", branch, "--actor", ACTOR]);
        write(path, "a.txt", text);
        ok(path, &["commit", "-m", branch, "--actor", ACTOR]);
        ok(path, &["switch", "main", "--actor", ACTOR]);
    }
    dir
}

fn tip(dir: &Path, branch: &str) -> String {
    let repository = stemma_core::repository::Repository::open(dir).unwrap();
    let name = branch.parse().unwrap();
    repository
        .refs()
        .branch(&name)
        .unwrap()
        .unwrap()
        .to_string()
}

#[test]
fn a_branch_that_is_ahead_is_fast_forwarded_and_the_entry_is_named() {
    let dir = history();
    let feature = tip(dir.path(), "feature");

    let merged = ok(dir.path(), &["merge", "feature", "--actor", ACTOR]);

    let entry = newest(dir.path());
    assert_eq!(
        merged.stdout,
        format!(
            "Fast-forwarded main to feature (commit {feature}) as entry {entry}\n\
             To undo it: stemma op restore {entry}\n"
        )
    );
    assert_eq!(merged.stderr, "");
    assert_eq!(tip(dir.path(), "main"), feature);
    assert_eq!(read(dir.path(), "a.txt"), "ONE\ntwo\nthree\n");
}

#[test]
fn diverged_branches_get_a_merge_commit_that_the_log_shows() {
    let dir = history();
    ok(dir.path(), &["switch", "feature", "--actor", ACTOR]);

    let merged = ok(
        dir.path(),
        &["merge", "other", "-m", "both edits", "--actor", ACTOR],
    );

    let entry = newest(dir.path());
    let commit = tip(dir.path(), "feature");
    assert_eq!(
        merged.stdout,
        format!(
            "Merged other into feature (commit {commit}) as entry {entry}\n\
             To undo it: stemma op restore {entry}\n"
        )
    );
    assert_eq!(read(dir.path(), "a.txt"), "ONE\ntwo\nTHREE\n");
    let log = ok(dir.path(), &["log"]);
    assert!(log.stdout.contains("both edits"), "{}", log.stdout);
    let status = ok(dir.path(), &["status", "--actor", ACTOR]);
    assert_eq!(status.stdout, "Working copy is clean\n");
}

#[test]
fn a_conflict_is_no_failure_and_its_paths_are_listed() {
    let dir = history();
    ok(dir.path(), &["switch", "feature", "--actor", ACTOR]);

    let merged = ok(dir.path(), &["merge", "clash", "--actor", ACTOR]);

    let entry = newest(dir.path());
    assert!(
        merged.stdout.contains("Conflicts:\nC a.txt\n"),
        "{}",
        merged.stdout
    );
    assert!(
        merged
            .stdout
            .ends_with(&format!("To undo it: stemma op restore {entry}\n")),
        "{}",
        merged.stdout
    );
    assert!(read(dir.path(), "a.txt").starts_with("<<<<<<< conflict\n"));
    let status = ok(dir.path(), &["status", "--actor", ACTOR]);
    assert!(
        status.stdout.contains("C a.txt (2 sides)"),
        "{}",
        status.stdout
    );

    // The entry undoes the merge, the branch included.
    ok(
        dir.path(),
        &["op", "restore", &entry.to_string(), "--actor", ACTOR],
    );
    assert_eq!(read(dir.path(), "a.txt"), "ONE\ntwo\nthree\n");
}

#[test]
fn a_branch_that_is_behind_is_up_to_date_without_an_entry() {
    let dir = history();
    ok(dir.path(), &["merge", "feature", "--actor", ACTOR]);
    let before = newest(dir.path());

    let result = ok(dir.path(), &["merge", "feature", "--actor", ACTOR]);

    assert_eq!(result.stdout, "Already up to date with feature\n");
    assert_eq!(newest(dir.path()), before);
}

#[test]
fn unsaved_edits_are_saved_first_and_the_entry_is_named() {
    let dir = history();
    write(dir.path(), "a.txt", "one\ntwo\nthree\nfour\n");

    let merged = ok(dir.path(), &["merge", "feature", "--actor", ACTOR]);

    assert!(
        merged
            .stdout
            .starts_with("Saved the working copy first as entry "),
        "{}",
        merged.stdout
    );
    assert_eq!(read(dir.path(), "a.txt"), "ONE\ntwo\nthree\nfour\n");
}

#[test]
fn a_missing_branch_an_empty_message_and_a_detached_head_end_with_code_1() {
    let dir = history();
    let before = newest(dir.path());

    let missing = run(dir.path(), &["merge", "absent", "--actor", ACTOR]);
    assert_eq!(missing.code, 1, "{}", missing.stderr);
    assert!(
        missing.stderr.contains("there is no branch `absent`"),
        "{}",
        missing.stderr
    );

    let empty = run(
        dir.path(),
        &["merge", "feature", "-m", " ", "--actor", ACTOR],
    );
    assert_eq!(empty.code, 1, "{}", empty.stderr);
    assert_eq!(newest(dir.path()), before);

    ok(
        dir.path(),
        &["switch", "--detach", "main", "--actor", ACTOR],
    );
    let detached = run(dir.path(), &["merge", "feature", "--actor", ACTOR]);
    assert_eq!(detached.code, 1, "{}", detached.stderr);
    assert!(
        detached.stderr.contains("not a branch"),
        "{}",
        detached.stderr
    );
}

#[test]
fn an_agent_is_refused_the_take_over_of_a_working_copy_that_it_does_not_own() {
    let dir = history();
    let before = newest(dir.path());

    let result = run(
        dir.path(),
        &[
            "merge",
            "feature",
            "--actor",
            "agent bot model=m session=s sponsor=daniel",
        ],
    );

    assert_eq!(result.code, 1, "{}", result.stderr);
    assert_eq!(newest(dir.path()), before);
}
