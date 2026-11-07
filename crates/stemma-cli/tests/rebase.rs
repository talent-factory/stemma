//! The command `stemma rebase`, run as a process, as a person or an agent would run it.

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

fn commit_file(dir: &Path, path: &str, content: &str, message: &str) {
    write(dir, path, content);
    ok(dir, &["track", path, "--actor", ACTOR]);
    ok(dir, &["commit", "-m", message, "--actor", ACTOR]);
}

fn edit_file(dir: &Path, path: &str, content: &str, message: &str) {
    write(dir, path, content);
    ok(dir, &["commit", "-m", message, "--actor", ACTOR]);
}

/// `main` holds `a.txt` and then `c.txt`; `feature` branches off the first commit, changes the
/// first line of `a.txt` and adds `d.txt`. The repository is on `feature`.
fn diverged() -> Dir {
    let dir = Dir::new();
    let path = dir.path();
    ok(path, &["init", "--actor", ACTOR]);
    commit_file(path, "a.txt", "one\ntwo\nthree\n", "first");
    ok(path, &["switch", "-c", "feature", "--actor", ACTOR]);
    edit_file(path, "a.txt", "ONE\ntwo\nthree\n", "feature edit");
    commit_file(path, "d.txt", "d\n", "feature adds d");
    ok(path, &["switch", "main", "--actor", ACTOR]);
    commit_file(path, "c.txt", "c\n", "main adds c");
    ok(path, &["switch", "feature", "--actor", ACTOR]);
    dir
}

#[test]
fn a_rebase_names_the_entry_the_replayed_commits_and_how_to_undo_it() {
    let dir = diverged();

    let result = ok(dir.path(), &["rebase", "main", "--actor", ACTOR]);

    let entry = newest(dir.path());
    assert_eq!(
        result.stdout,
        format!(
            "Rebased feature onto main as entry {entry}: 2 commits replayed\n\
             To undo it: stemma op restore {entry}\n"
        )
    );
    assert_eq!(result.stderr, "");
    assert_eq!(read(dir.path(), "a.txt"), "ONE\ntwo\nthree\n");
    assert_eq!(read(dir.path(), "c.txt"), "c\n");
    assert_eq!(read(dir.path(), "d.txt"), "d\n");
    let status = ok(dir.path(), &["status", "--actor", ACTOR]);
    assert!(
        status.stdout.contains("Working copy is clean"),
        "{}",
        status.stdout
    );
}

#[test]
fn the_entry_of_a_rebase_is_undone_by_op_restore() {
    let dir = diverged();
    ok(dir.path(), &["rebase", "main", "--actor", ACTOR]);
    let entry = newest(dir.path()).to_string();

    ok(dir.path(), &["op", "restore", &entry, "--actor", ACTOR]);

    assert!(!dir.path().join("c.txt").exists());
    assert_eq!(read(dir.path(), "a.txt"), "ONE\ntwo\nthree\n");
    assert_eq!(read(dir.path(), "d.txt"), "d\n");
}

#[test]
fn a_branch_that_has_the_target_is_up_to_date_with_code_0_and_no_entry() {
    let dir = diverged();
    ok(dir.path(), &["rebase", "main", "--actor", ACTOR]);
    let before = newest(dir.path());

    let result = ok(dir.path(), &["rebase", "main", "--actor", ACTOR]);

    assert_eq!(result.stdout, "Already up to date with main\n");
    assert_eq!(newest(dir.path()), before);
}

#[test]
fn unsaved_changes_are_saved_first_and_the_line_says_so() {
    let dir = diverged();
    write(dir.path(), "d.txt", "d changed\n");
    let before = newest(dir.path());

    let result = ok(dir.path(), &["rebase", "main", "--actor", ACTOR]);

    assert!(
        result.stdout.starts_with(&format!(
            "Saved the working copy first as entry {}\n",
            before + 1
        )),
        "{}",
        result.stdout
    );
    assert_eq!(read(dir.path(), "d.txt"), "d changed\n");
    assert_eq!(read(dir.path(), "c.txt"), "c\n");
}

#[test]
fn a_conflict_does_not_stop_the_rebase_and_the_output_names_it() {
    let dir = Dir::new();
    let path = dir.path();
    ok(path, &["init", "--actor", ACTOR]);
    commit_file(path, "a.txt", "one\ntwo\nthree\n", "first");
    ok(path, &["switch", "-c", "feature", "--actor", ACTOR]);
    edit_file(path, "a.txt", "ONE\ntwo\nthree\n", "feature edit");
    ok(path, &["switch", "main", "--actor", ACTOR]);
    edit_file(path, "a.txt", "uno\ntwo\nthree\n", "main edit");
    ok(path, &["switch", "feature", "--actor", ACTOR]);

    let result = ok(path, &["rebase", "main", "--actor", ACTOR]);

    assert!(
        result
            .stdout
            .contains("has an open conflict in a.txt (2 sides)\n"),
        "{}",
        result.stdout
    );
    assert!(
        result
            .stdout
            .contains("The working copy has an open conflict in a.txt\n"),
        "{}",
        result.stdout
    );
    assert!(read(path, "a.txt").starts_with("<<<<<<< conflict\n"));
    // The conflict is data: a second snapshot finds nothing new, and `status` names it.
    let status = ok(path, &["status", "--actor", ACTOR]);
    assert!(status.stdout.contains("a.txt"), "{}", status.stdout);
}

#[test]
fn an_unknown_target_a_detached_head_and_a_missing_actor_are_refused() {
    let dir = diverged();
    let before = newest(dir.path());

    let unknown = run(dir.path(), &["rebase", "nope", "--actor", ACTOR]);
    assert_eq!(unknown.code, 1);
    assert!(
        unknown.stderr.contains("neither a branch nor a tag `nope`"),
        "{}",
        unknown.stderr
    );
    let anonymous = run(dir.path(), &["rebase", "main"]);
    assert_eq!(anonymous.code, 1);
    assert_eq!(newest(dir.path()), before);

    ok(
        dir.path(),
        &["switch", "--detach", "feature", "--actor", ACTOR],
    );
    let detached = run(dir.path(), &["rebase", "main", "--actor", ACTOR]);
    assert_eq!(detached.code, 1);
    assert!(
        detached.stderr.contains("HEAD names a commit"),
        "{}",
        detached.stderr
    );
}

#[test]
fn the_target_is_required_and_a_commit_id_is_a_valid_target() {
    let dir = diverged();
    let missing = run(dir.path(), &["rebase", "--actor", ACTOR]);
    assert_eq!(missing.code, 2);

    let log = ok(dir.path(), &["log", "--json"]);
    let id = log
        .stdout
        .split('"')
        .rfind(|part| part.starts_with("b3:"))
        .unwrap()
        .to_owned();
    // The oldest commit lies below the branch, which has it already.
    let result = ok(dir.path(), &["rebase", &id, "--actor", ACTOR]);
    assert_eq!(result.stdout, format!("Already up to date with {id}\n"));
}

/// `diverged` with the tag `base` on `main` (the commit that `main` adds) and the tag `main` on
/// the first commit, so that the name `main` is a branch and a tag.
fn tagged() -> Dir {
    let dir = diverged();
    let path = dir.path();
    ok(path, &["switch", "--detach", "main", "--actor", ACTOR]);
    ok(path, &["tag", "base", "--actor", ACTOR]);
    ok(path, &["switch", "feature", "--actor", ACTOR]);
    ok(path, &["tag", "main", "--actor", ACTOR]);
    dir
}

#[test]
fn the_target_takes_a_tag_and_the_qualified_forms() {
    let dir = tagged();

    let result = ok(dir.path(), &["rebase", "base", "--actor", ACTOR]);

    assert!(
        result
            .stdout
            .starts_with("Rebased feature onto base as entry"),
        "{}",
        result.stdout
    );
    assert_eq!(read(dir.path(), "c.txt"), "c\n");

    let dir = tagged();
    let result = ok(dir.path(), &["rebase", "heads/main", "--actor", ACTOR]);
    assert!(
        result
            .stdout
            .starts_with("Rebased feature onto heads/main as entry"),
        "{}",
        result.stdout
    );
    assert_eq!(read(dir.path(), "c.txt"), "c\n");

    let dir = tagged();
    let result = ok(dir.path(), &["rebase", "tags/base", "--actor", ACTOR]);
    assert!(
        result
            .stdout
            .starts_with("Rebased feature onto tags/base as entry"),
        "{}",
        result.stdout
    );
}

#[test]
fn the_up_to_date_line_keeps_the_prefix_of_a_qualified_form() {
    let dir = tagged();

    let tag = ok(dir.path(), &["rebase", "tags/main", "--actor", ACTOR]);
    assert_eq!(tag.stdout, "Already up to date with tags/main\n");

    ok(dir.path(), &["rebase", "heads/main", "--actor", ACTOR]);
    let branch = ok(dir.path(), &["rebase", "heads/main", "--actor", ACTOR]);
    assert_eq!(branch.stdout, "Already up to date with heads/main\n");
}

#[test]
fn a_qualified_target_that_does_not_exist_is_refused() {
    let dir = tagged();
    let before = newest(dir.path());

    for target in ["heads/nowhere", "tags/nowhere"] {
        let result = run(dir.path(), &["rebase", target, "--actor", ACTOR]);
        assert_eq!(result.code, 1, "{target}: {}", result.stderr);
        assert_eq!(newest(dir.path()), before, "nothing was written");
    }
}

#[test]
fn a_target_that_is_a_branch_and_a_tag_is_refused() {
    let dir = tagged();
    let before = newest(dir.path());

    let result = run(dir.path(), &["rebase", "main", "--actor", ACTOR]);

    assert_eq!(result.code, 1, "{}", result.stderr);
    assert!(
        result.stderr.contains("heads/main") && result.stderr.contains("tags/main"),
        "{}",
        result.stderr
    );
    assert_eq!(newest(dir.path()), before, "nothing was written");
    assert!(!dir.path().join("c.txt").exists());
}
