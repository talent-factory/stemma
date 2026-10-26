//! The command `stemma diff`, run as a process, as a person or an agent would run it.

mod common;

use std::fs;
use std::path::Path;

use common::{repository, run, track_ok, write, ACTOR};

/// Commits with the message and returns the identifier of the commit, `b3:<hex>`.
fn commit(dir: &Path, message: &str) -> String {
    let result = run(dir, &["commit", "-m", message, "--actor", ACTOR]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    result
        .stdout
        .split_whitespace()
        .nth(1)
        .expect("the commit names its identifier")
        .to_owned()
}

fn diff(dir: &Path, extra: &[&str]) -> common::Run {
    let mut args = vec!["diff", "--actor", ACTOR];
    args.extend_from_slice(extra);
    run(dir, &args)
}

#[test]
fn a_clean_working_copy_shows_nothing_and_exits_zero() {
    let dir = repository();
    write(dir.path(), "a.txt", "one\n");
    track_ok(dir.path(), "a.txt");
    commit(dir.path(), "first");

    let result = diff(dir.path(), &["--exit-code"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(result.stdout, "");
}

#[test]
fn an_edit_is_shown_as_a_hunk_and_exit_code_four_reports_it() {
    let dir = repository();
    write(dir.path(), "a.txt", "one\n");
    track_ok(dir.path(), "a.txt");
    let first = commit(dir.path(), "first");
    write(dir.path(), "a.txt", "two\n");

    let result = diff(dir.path(), &[]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(result
        .stdout
        .starts_with(&format!("--- {first} a.txt\n+++ working-copy b3:")));
    assert!(
        result.stdout.contains("@@ -1 +1 @@\n-one\n+two\n"),
        "{}",
        result.stdout
    );

    let with_flag = diff(dir.path(), &["--exit-code"]);
    assert_eq!(with_flag.code, 4, "{}", with_flag.stderr);
    assert_eq!(with_flag.stdout, result.stdout);
}

#[test]
fn an_added_and_a_deleted_file_name_dev_null_on_the_missing_side() {
    let dir = repository();
    write(dir.path(), "gone.txt", "bye\n");
    track_ok(dir.path(), "gone.txt");
    let first = commit(dir.path(), "first");
    fs::remove_file(dir.path().join("gone.txt")).unwrap();
    write(dir.path(), "new.txt", "hi\n");
    track_ok(dir.path(), "new.txt");

    let result = diff(dir.path(), &[]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(
        result
            .stdout
            .contains(&format!("--- {first} gone.txt\n+++ /dev/null\n")),
        "{}",
        result.stdout
    );
    assert!(
        result
            .stdout
            .contains("--- /dev/null\n+++ working-copy b3:"),
        "{}",
        result.stdout
    );
    assert!(
        result.stdout.contains("@@ -0,0 +1 @@\n+hi\n"),
        "{}",
        result.stdout
    );
}

#[test]
fn an_empty_added_file_counts_as_a_difference() {
    let dir = repository();
    write(dir.path(), "a.txt", "one\n");
    track_ok(dir.path(), "a.txt");
    commit(dir.path(), "first");
    write(dir.path(), "empty.txt", "");
    track_ok(dir.path(), "empty.txt");

    let result = diff(dir.path(), &["--exit-code"]);
    assert_eq!(result.code, 4, "{}", result.stderr);
    assert!(
        result
            .stdout
            .contains("--- /dev/null\n+++ working-copy b3:"),
        "{}",
        result.stdout
    );
    assert!(result.stdout.contains(" empty.txt\n"), "{}", result.stdout);
}

#[test]
fn two_commits_are_compared_with_each_other() {
    let dir = repository();
    write(dir.path(), "a.txt", "one\n");
    track_ok(dir.path(), "a.txt");
    let first = commit(dir.path(), "first");
    write(dir.path(), "a.txt", "two\n");
    track_ok(dir.path(), "a.txt");
    let second = commit(dir.path(), "second");

    let result = diff(dir.path(), &["--exit-code", &first, &second]);
    assert_eq!(result.code, 4, "{}", result.stderr);
    assert!(
        result
            .stdout
            .starts_with(&format!("--- {first} a.txt\n+++ {second} a.txt\n")),
        "{}",
        result.stdout
    );
    assert!(result.stdout.contains("-one\n+two\n"), "{}", result.stdout);
}

#[test]
fn a_binary_file_is_named_and_not_shown() {
    let dir = repository();
    fs::write(dir.path().join("img.bin"), b"\0first").unwrap();
    track_ok(dir.path(), "img.bin");
    let first = commit(dir.path(), "first");
    fs::write(dir.path().join("img.bin"), b"\0second").unwrap();

    let result = diff(dir.path(), &[]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(
        result.stdout.starts_with(&format!(
            "Binary files {first} img.bin and working-copy b3:"
        )) && result.stdout.ends_with(" img.bin differ\n"),
        "{}",
        result.stdout
    );
}

#[test]
fn an_unknown_commit_exits_one_and_names_it() {
    let dir = repository();
    write(dir.path(), "a.txt", "one\n");
    track_ok(dir.path(), "a.txt");
    commit(dir.path(), "first");
    let unknown = format!("b3:{}", "a".repeat(64));

    let result = diff(dir.path(), &[&unknown]);
    assert_eq!(result.code, 1);
    assert!(
        result.stderr.contains("could not read the commit"),
        "{}",
        result.stderr
    );
    assert!(result.stderr.contains(&unknown), "{}", result.stderr);
}

#[test]
fn an_operand_that_is_not_an_identifier_exits_two() {
    let dir = repository();
    write(dir.path(), "a.txt", "one\n");
    track_ok(dir.path(), "a.txt");
    commit(dir.path(), "first");

    let result = diff(dir.path(), &["xyz"]);
    assert_eq!(result.code, 2);
    assert!(
        result.stderr.contains("is not a commit identifier"),
        "{}",
        result.stderr
    );
}

#[test]
fn more_than_two_commits_are_a_usage_error() {
    let dir = repository();
    write(dir.path(), "a.txt", "one\n");
    track_ok(dir.path(), "a.txt");
    let first = commit(dir.path(), "first");

    let result = diff(dir.path(), &[&first, &first, &first]);
    assert_eq!(result.code, 2, "{}", result.stderr);
}
