//! The command `stemma switch`, run as a process, as a person or an agent would run it.

mod common;

use std::fs;
use std::path::Path;

use common::{ok, run, write, Dir, ACTOR};

fn read(dir: &Path, path: &str) -> String {
    fs::read_to_string(dir.join(path)).unwrap()
}

/// A repository with `a.txt` committed as "one" on `main`.
fn committed() -> Dir {
    let dir = Dir::new();
    ok(dir.path(), &["init", "--actor", ACTOR]);
    write(dir.path(), "a.txt", "one");
    ok(dir.path(), &["track", "a.txt", "--actor", ACTOR]);
    ok(dir.path(), &["commit", "-m", "first", "--actor", ACTOR]);
    dir
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

#[test]
fn a_new_branch_and_back_name_the_entry_and_how_to_undo_it() {
    let dir = committed();

    let created = ok(dir.path(), &["switch", "-c", "feature", "--actor", ACTOR]);

    let entry = newest(dir.path());
    assert_eq!(
        created.stdout,
        format!(
            "Switched to a new branch feature as entry {entry}\n\
             To undo it: stemma op restore {entry}\n"
        )
    );
    assert_eq!(created.stderr, "");
    write(dir.path(), "a.txt", "two");
    ok(dir.path(), &["commit", "-m", "second", "--actor", ACTOR]);

    let back = ok(dir.path(), &["switch", "main", "--actor", ACTOR]);

    let entry = newest(dir.path());
    assert_eq!(
        back.stdout,
        format!(
            "Switched to branch main as entry {entry}\n\
             To undo it: stemma op restore {entry}\n"
        )
    );
    assert_eq!(read(dir.path(), "a.txt"), "one");
    ok(dir.path(), &["switch", "feature", "--actor", ACTOR]);
    assert_eq!(read(dir.path(), "a.txt"), "two");
}

#[test]
fn a_switch_to_the_current_branch_is_code_0_without_an_entry() {
    let dir = committed();
    let before = newest(dir.path());

    let result = ok(dir.path(), &["switch", "main", "--actor", ACTOR]);

    assert_eq!(result.stdout, "Already on main\n");
    assert_eq!(newest(dir.path()), before);
}

#[test]
fn a_working_copy_with_changes_is_refused_with_code_1_after_it_was_saved() {
    let dir = committed();
    ok(dir.path(), &["switch", "-c", "feature", "--actor", ACTOR]);
    write(dir.path(), "a.txt", "scratch");
    let before = newest(dir.path());

    let result = run(dir.path(), &["switch", "main", "--actor", ACTOR]);

    assert_eq!(result.code, 1, "{}", result.stdout);
    assert_eq!(result.stdout, "");
    assert!(
        result.stderr.contains("commit them first"),
        "{}",
        result.stderr
    );
    assert!(
        result
            .stderr
            .contains(&format!("saved first as entry {}", before + 1)),
        "{}",
        result.stderr
    );
    assert_eq!(read(dir.path(), "a.txt"), "scratch");
}

#[test]
fn an_unknown_branch_is_code_1_and_points_to_the_option_that_makes_one() {
    let dir = committed();

    let result = run(dir.path(), &["switch", "nope", "--actor", ACTOR]);

    assert_eq!(result.code, 1);
    assert!(
        result.stderr.contains("switch -c nope"),
        "{}",
        result.stderr
    );
}

#[test]
fn a_detached_head_refuses_a_commit_and_the_message_points_to_switch() {
    let dir = committed();
    let detached = ok(
        dir.path(),
        &["switch", "--detach", "main", "--actor", ACTOR],
    );
    assert!(
        detached
            .stdout
            .starts_with("Detached HEAD at main as entry "),
        "{}",
        detached.stdout
    );
    write(dir.path(), "a.txt", "two");

    let result = run(dir.path(), &["commit", "-m", "second", "--actor", ACTOR]);

    assert_eq!(result.code, 1);
    assert!(
        result.stderr.contains("switch -c <name>"),
        "{}",
        result.stderr
    );
}

#[test]
fn a_commit_is_no_branch_name_without_detach_and_the_parser_says_so() {
    let dir = committed();
    let id = format!("b3:{}", "0".repeat(64));

    let result = run(dir.path(), &["switch", &id, "--actor", ACTOR]);

    assert_eq!(result.code, 2);
    assert!(result.stderr.contains("--detach"), "{}", result.stderr);
    let start = run(dir.path(), &["switch", "feature", "main", "--actor", ACTOR]);
    assert_eq!(start.code, 2, "a start needs -c");
}

#[test]
fn a_new_branch_can_start_at_a_commit_by_its_id() {
    let dir = committed();
    let log = ok(dir.path(), &["log", "--json"]);
    let id = log
        .stdout
        .split('"')
        .find(|part| part.starts_with("b3:"))
        .unwrap()
        .to_owned();
    write(dir.path(), "a.txt", "two");
    ok(dir.path(), &["commit", "-m", "second", "--actor", ACTOR]);

    ok(dir.path(), &["switch", "-c", "old", &id, "--actor", ACTOR]);

    assert_eq!(read(dir.path(), "a.txt"), "one");
    let result = run(dir.path(), &["switch", "main", "--actor", ACTOR]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(read(dir.path(), "a.txt"), "two");
}

#[test]
fn a_detached_switch_takes_a_commit_id_and_an_unknown_id_is_refused() {
    let dir = committed();
    let log = ok(dir.path(), &["log", "--json"]);
    let id = log
        .stdout
        .split('"')
        .find(|part| part.starts_with("b3:"))
        .unwrap()
        .to_owned();
    write(dir.path(), "a.txt", "two");
    ok(dir.path(), &["commit", "-m", "second", "--actor", ACTOR]);
    let before = newest(dir.path());

    let unknown = run(
        dir.path(),
        &[
            "switch",
            "--detach",
            &format!("b3:{}", "0".repeat(64)),
            "--actor",
            ACTOR,
        ],
    );
    assert_eq!(unknown.code, 1, "{}", unknown.stderr);
    assert!(unknown.stderr.contains("no commit"), "{}", unknown.stderr);
    assert_eq!(newest(dir.path()), before, "nothing was written");

    let detached = ok(dir.path(), &["switch", "--detach", &id, "--actor", ACTOR]);
    assert!(
        detached.stdout.starts_with("Detached HEAD at "),
        "{}",
        detached.stdout
    );
    assert_eq!(read(dir.path(), "a.txt"), "one");
    let back = ok(dir.path(), &["switch", "main", "--actor", ACTOR]);
    assert!(
        back.stdout.starts_with("Switched to branch main"),
        "{}",
        back.stdout
    );
    assert_eq!(read(dir.path(), "a.txt"), "two");
}
