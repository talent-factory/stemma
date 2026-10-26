//! The command `stemma diff`, run as a process, as a person or an agent would run it.

mod common;

use std::path::Path;

use common::{repository, run, track_ok, write, Run, ACTOR};

fn commit(dir: &Path, message: &str) {
    let result = run(dir, &["commit", "-m", message, "--actor", ACTOR]);
    assert_eq!(result.code, 0, "{}", result.stderr);
}

fn diff(dir: &Path, operands: &[&str]) -> Run {
    let mut args = vec!["diff", "--actor", "human alice"];
    args.extend_from_slice(operands);
    run(dir, &args)
}

/// A repository with one commit of `a.txt` and an edit of it in the working copy.
fn edited_repository() -> common::Dir {
    let dir = repository();
    write(dir.path(), "a.txt", "one\n");
    track_ok(dir.path(), "a.txt");
    commit(dir.path(), "first");
    write(dir.path(), "a.txt", "two\n");
    dir
}

#[test]
fn the_working_copy_is_named_by_its_snapshot_identifier_and_no_time() {
    let dir = edited_repository();

    let result = diff(dir.path(), &[]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(result.stderr, "");
    let mut lines = result.stdout.lines();
    let old = lines.next().unwrap();
    let new = lines.next().unwrap();
    assert!(old.starts_with("--- b3:"), "{old}");
    let working_copy = new
        .strip_prefix("+++ working-copy ")
        .unwrap_or_else(|| panic!("the new side names the working copy: {new}"));
    assert!(working_copy.starts_with("b3:"), "{new}");
    assert_eq!(
        working_copy.len(),
        "b3:".len() + 64,
        "{new}: no time after the id"
    );
    assert!(result.stdout.contains("\n-one\n"), "{}", result.stdout);
    assert!(result.stdout.contains("\n+two\n"), "{}", result.stdout);
}

#[test]
fn a_commit_is_compared_with_the_working_copy_by_its_identifier() {
    let dir = edited_repository();
    let first = diff(dir.path(), &[]);
    let old = first
        .stdout
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("--- "))
        .unwrap()
        .to_owned();

    let result = diff(dir.path(), &[old.as_str()]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    let mut lines = result.stdout.lines();
    assert_eq!(lines.next(), Some(format!("--- {old}").as_str()));
    assert!(
        lines
            .next()
            .is_some_and(|line| line.starts_with("+++ working-copy b3:")),
        "{}",
        result.stdout
    );
}
