//! The command `stemma diff`, run as a process, as a person or an agent would run it.

mod common;

use std::fs;
use std::path::Path;

use common::{operations, repository, run, track_ok, write, Dir, Run, ACTOR};

fn commit(dir: &Path, message: &str) {
    let result = run(dir, &["commit", "-m", message, "--actor", ACTOR]);
    assert_eq!(result.code, 0, "{}", result.stderr);
}

fn diff(dir: &Path, operands: &[&str]) -> Run {
    let mut args = vec!["diff", "--actor", "human alice"];
    args.extend_from_slice(operands);
    run(dir, &args)
}

/// The identifier of the commit that the first line of a diff names, `b3:…`.
fn old_side(output: &str) -> String {
    output
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("--- "))
        .unwrap()
        .to_owned()
}

/// A repository with one commit of `a.txt` and an edit of it in the working copy.
fn edited_repository() -> Dir {
    let dir = repository();
    write(dir.path(), "a.txt", "one\n");
    track_ok(dir.path(), "a.txt");
    commit(dir.path(), "first");
    write(dir.path(), "a.txt", "two\n");
    dir
}

/// A repository with one commit of `file` with the content `before`, and the working copy
/// with the content `after`.
fn repository_with_edit(file: &str, before: &[u8], after: &[u8]) -> Dir {
    let dir = repository();
    fs::create_dir_all(dir.path().join(file).parent().unwrap()).unwrap();
    fs::write(dir.path().join(file), before).unwrap();
    track_ok(dir.path(), file);
    commit(dir.path(), "first");
    fs::write(dir.path().join(file), after).unwrap();
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
    let old = old_side(&diff(dir.path(), &[]).stdout);

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

#[test]
fn one_changed_line_is_one_hunk_with_its_header_and_lines() {
    let dir = edited_repository();

    let result = diff(dir.path(), &[]);

    let lines: Vec<&str> = result.stdout.lines().skip(2).collect();
    assert_eq!(
        lines,
        ["diff a.txt", "@@ -1,1 +1,1 @@", "-one", "+two"],
        "{}",
        result.stdout
    );
}

#[test]
fn an_unchanged_working_copy_shows_the_header_only() {
    let dir = edited_repository();
    write(dir.path(), "a.txt", "one\n");

    let result = diff(dir.path(), &[]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(result.stdout.lines().count(), 2, "{}", result.stdout);
}

#[test]
fn a_change_of_the_line_ends_is_shown() {
    let dir = repository_with_edit("a.txt", b"a\nb\n", b"a\r\nb\r\n");

    let result = diff(dir.path(), &[]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(
        result.stdout.contains("@@ -1,2 +1,2 @@"),
        "{}",
        result.stdout
    );
    assert!(result.stdout.contains("-a\n"), "{}", result.stdout);
    assert!(result.stdout.contains("+a\r\n"), "{}", result.stdout);
}

#[test]
fn a_last_line_without_a_line_end_is_marked_as_such() {
    let dir = repository_with_edit("a.txt", b"one\n", b"one");

    let result = diff(dir.path(), &[]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(
        result
            .stdout
            .contains("+one\n\\ No newline at end of file\n"),
        "{}",
        result.stdout
    );
}

#[test]
fn a_change_of_bytes_that_are_not_utf8_is_shown() {
    let dir = repository_with_edit("a.txt", b"\xE9\n", b"\xE8\n");

    let result = diff(dir.path(), &[]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(
        result.stdout.contains("@@ -1,1 +1,1 @@"),
        "{}",
        result.stdout
    );
}

#[test]
fn a_binary_file_is_named_without_its_lines() {
    let dir = repository_with_edit("bin.dat", b"a\0b", b"a\0c");

    let result = diff(dir.path(), &[]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(
        result
            .stdout
            .contains("diff bin.dat\nBinary files differ\n"),
        "{}",
        result.stdout
    );
    assert!(!result.stdout.contains("@@"), "{}", result.stdout);
}

#[test]
fn a_file_in_a_directory_is_named_by_its_path() {
    let dir = repository_with_edit("dir/a.txt", b"one\n", b"two\n");

    let result = diff(dir.path(), &[]);

    assert!(
        result.stdout.contains("diff dir/a.txt\n"),
        "{}",
        result.stdout
    );
}

#[test]
fn an_added_file_has_an_empty_old_range() {
    let dir = edited_repository();
    write(dir.path(), "new.txt", "fresh\n");
    track_ok(dir.path(), "new.txt");

    let result = diff(dir.path(), &[]);

    assert!(
        result
            .stdout
            .contains("diff new.txt\n@@ -0,0 +1,1 @@\n+fresh\n"),
        "{}",
        result.stdout
    );
}

#[test]
fn a_deleted_file_has_an_empty_new_range() {
    let dir = repository_with_edit("gone.txt", b"bye\n", b"bye\n");
    fs::remove_file(dir.path().join("gone.txt")).unwrap();

    let result = diff(dir.path(), &[]);

    assert!(
        result
            .stdout
            .contains("diff gone.txt\n@@ -1,1 +0,0 @@\n-bye\n"),
        "{}",
        result.stdout
    );
}

#[test]
fn the_first_commit_has_no_old_side() {
    let dir = repository();
    write(dir.path(), "a.txt", "x\n");
    track_ok(dir.path(), "a.txt");

    let result = diff(dir.path(), &[]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(
        result.stdout.starts_with("--- -\n+++ working-copy b3:"),
        "{}",
        result.stdout
    );
    assert!(result.stdout.contains("+x\n"), "{}", result.stdout);
}

#[test]
fn two_commits_are_compared_without_a_snapshot_of_the_working_copy() {
    let dir = edited_repository();
    let first = old_side(&diff(dir.path(), &[]).stdout);
    commit(dir.path(), "second");
    let second = old_side(&diff(dir.path(), &[]).stdout);
    write(dir.path(), "a.txt", "three\n");
    let entries = operations(dir.path()).len();

    let result = diff(dir.path(), &[first.as_str(), second.as_str()]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(
        result
            .stdout
            .starts_with(&format!("--- {first}\n+++ {second}\n")),
        "{}",
        result.stdout
    );
    assert_eq!(
        operations(dir.path()).len(),
        entries,
        "no snapshot was taken"
    );
}

#[test]
fn an_unknown_operand_is_an_error_that_names_it() {
    let dir = edited_repository();
    let unknown = format!("b3:{}", "0".repeat(64));

    let result = diff(dir.path(), &[unknown.as_str()]);

    assert_eq!(result.code, 1, "{}", result.stdout);
    assert!(
        result
            .stderr
            .contains(&format!("`{unknown}` is not the identifier of a commit")),
        "{}",
        result.stderr
    );
}

#[test]
fn three_operands_are_a_usage_error() {
    let dir = edited_repository();

    let result = diff(dir.path(), &["b3:a", "b3:b", "b3:c"]);

    assert_eq!(result.code, 2, "{}", result.stderr);
}
