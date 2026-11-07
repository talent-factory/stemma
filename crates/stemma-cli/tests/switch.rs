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

/// `main` holds `a.txt` as "one\ntwo\nthree\n"; `feature` changes its first line to "ONE". The
/// repository is on `main`.
fn two_branches() -> Dir {
    let dir = Dir::new();
    ok(dir.path(), &["init", "--actor", ACTOR]);
    write(dir.path(), "a.txt", "one\ntwo\nthree\n");
    ok(dir.path(), &["track", "a.txt", "--actor", ACTOR]);
    ok(dir.path(), &["commit", "-m", "first", "--actor", ACTOR]);
    ok(dir.path(), &["switch", "-c", "feature", "--actor", ACTOR]);
    write(dir.path(), "a.txt", "ONE\ntwo\nthree\n");
    ok(dir.path(), &["commit", "-m", "edit", "--actor", ACTOR]);
    ok(dir.path(), &["switch", "main", "--actor", ACTOR]);
    dir
}

#[test]
fn the_changes_of_the_working_copy_are_carried_and_the_output_says_so() {
    let dir = two_branches();
    write(dir.path(), "a.txt", "one\ntwo\nTHREE\n");
    let before = newest(dir.path());

    let result = ok(dir.path(), &["switch", "feature", "--actor", ACTOR]);

    let entry = newest(dir.path());
    assert_eq!(entry, before + 2, "the snapshot and the switch");
    assert_eq!(
        result.stdout,
        format!(
            "Saved the working copy first as entry {}\n\
             Switched to branch feature as entry {entry}\n\
             Carried the changes of the working copy to the new base\n\
             To undo it: stemma op restore {entry}\n",
            before + 1
        )
    );
    assert_eq!(read(dir.path(), "a.txt"), "ONE\ntwo\nTHREE\n");
}

#[test]
fn a_conflict_of_the_carry_is_listed_and_is_no_failure() {
    let dir = two_branches();
    write(dir.path(), "a.txt", "uno\ntwo\nthree\n");

    let result = ok(dir.path(), &["switch", "feature", "--actor", ACTOR]);

    assert!(
        result.stdout.contains("Carried the changes"),
        "{}",
        result.stdout
    );
    assert!(
        result.stdout.contains("C conflict a.txt\n"),
        "{}",
        result.stdout
    );
    assert!(
        read(dir.path(), "a.txt").starts_with("<<<<<<< conflict\n"),
        "{}",
        read(dir.path(), "a.txt")
    );
    let status = ok(dir.path(), &["status", "--actor", ACTOR]);
    assert!(
        status.stdout.contains("C a.txt"),
        "status shows the conflict: {}",
        status.stdout
    );
}

#[test]
fn discard_brings_the_directory_to_the_target_and_keeps_the_changes_in_the_log() {
    let dir = two_branches();
    write(dir.path(), "a.txt", "scratch\n");
    let before = newest(dir.path());

    let result = ok(
        dir.path(),
        &["switch", "feature", "--discard", "--actor", ACTOR],
    );

    let entry = newest(dir.path());
    assert_eq!(entry, before + 2);
    assert!(
        result.stdout.contains(
            "Left the changes behind; the log keeps them, and `op restore` brings them back\n"
        ),
        "{}",
        result.stdout
    );
    assert_eq!(read(dir.path(), "a.txt"), "ONE\ntwo\nthree\n");
}

#[test]
fn a_dry_run_writes_nothing_and_names_the_plan() {
    let dir = two_branches();
    write(dir.path(), "a.txt", "uno\ntwo\nthree\n");
    ok(dir.path(), &["status", "--actor", ACTOR]);
    let before = newest(dir.path());

    let result = ok(
        dir.path(),
        &["switch", "feature", "--dry-run", "--actor", ACTOR],
    );

    assert_eq!(newest(dir.path()), before, "no entry");
    assert_eq!(read(dir.path(), "a.txt"), "uno\ntwo\nthree\n");
    assert!(
        result
            .stdout
            .starts_with("Would switch to branch feature; nothing is written\n"),
        "{}",
        result.stdout
    );
    assert!(
        result
            .stdout
            .contains("    carry the changes of the working copy to the new base\n"),
        "{}",
        result.stdout
    );
    assert!(
        result.stdout.contains("    conflict a.txt\n"),
        "{}",
        result.stdout
    );
    assert!(
        result.stdout.contains("    write a.txt\n"),
        "{}",
        result.stdout
    );
    assert_eq!(result.stderr, "");
}

#[test]
fn a_dry_run_cannot_be_combined_with_take_over() {
    let dir = committed();

    let result = run(
        dir.path(),
        &[
            "switch",
            "main",
            "--dry-run",
            "--take-over",
            "--actor",
            ACTOR,
        ],
    );

    assert_eq!(result.code, 2, "{}", result.stderr);
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

#[test]
fn discard_without_changes_says_nothing_about_them() {
    let dir = two_branches();

    let result = ok(
        dir.path(),
        &["switch", "feature", "--discard", "--actor", ACTOR],
    );

    assert!(
        !result.stdout.contains("Left the changes"),
        "{}",
        result.stdout
    );
    assert!(!result.stdout.contains("Carried"), "{}", result.stdout);
}

#[test]
fn a_dry_run_of_a_discard_names_the_discard() {
    let dir = two_branches();
    write(dir.path(), "a.txt", "scratch\n");
    ok(dir.path(), &["status", "--actor", ACTOR]);

    let result = ok(
        dir.path(),
        &[
            "switch",
            "feature",
            "--discard",
            "--dry-run",
            "--actor",
            ACTOR,
        ],
    );

    assert!(
        result
            .stdout
            .contains("    discard the changes of the working copy\n"),
        "{}",
        result.stdout
    );
}

/// The ID of the newest commit of `main`.
fn tip(dir: &Path) -> String {
    ok(dir, &["log", "--json"])
        .stdout
        .split('"')
        .find(|part| part.starts_with("b3:"))
        .unwrap()
        .to_owned()
}

/// `a.txt` is "one" at the tag `v1`, and "two" at the tip of `main`. The branch `later` and the
/// tag `main` exist as well: `v1` is only a tag, `later` only a branch, and `main` both.
fn branches_and_tags() -> Dir {
    let dir = committed();
    ok(dir.path(), &["tag", "v1", "--actor", ACTOR]);
    ok(dir.path(), &["tag", "main", "--actor", ACTOR]);
    write(dir.path(), "a.txt", "two");
    ok(dir.path(), &["commit", "-m", "second", "--actor", ACTOR]);
    ok(dir.path(), &["branch", "later", "--actor", ACTOR]);
    dir
}

#[test]
fn a_detached_switch_takes_a_tag_and_a_branch_by_the_plain_name() {
    let dir = branches_and_tags();

    let tag = ok(dir.path(), &["switch", "--detach", "v1", "--actor", ACTOR]);
    assert!(
        tag.stdout.starts_with("Detached HEAD at "),
        "{}",
        tag.stdout
    );
    assert_eq!(read(dir.path(), "a.txt"), "one");

    ok(
        dir.path(),
        &["switch", "--detach", "later", "--actor", ACTOR],
    );
    assert_eq!(read(dir.path(), "a.txt"), "two");
}

#[test]
fn a_name_that_is_a_branch_and_a_tag_is_refused_and_the_qualified_forms_pick_one() {
    let dir = branches_and_tags();
    let before = newest(dir.path());

    let result = run(
        dir.path(),
        &["switch", "--detach", "main", "--actor", ACTOR],
    );

    assert_eq!(result.code, 1, "{}", result.stderr);
    assert!(
        result.stderr.contains("heads/main") && result.stderr.contains("tags/main"),
        "{}",
        result.stderr
    );
    assert_eq!(newest(dir.path()), before, "nothing was written");
    assert_eq!(read(dir.path(), "a.txt"), "two");

    ok(
        dir.path(),
        &["switch", "--detach", "tags/main", "--actor", ACTOR],
    );
    assert_eq!(read(dir.path(), "a.txt"), "one");
    ok(
        dir.path(),
        &["switch", "--detach", "heads/main", "--actor", ACTOR],
    );
    assert_eq!(read(dir.path(), "a.txt"), "two");
}

#[test]
fn a_qualified_form_of_a_missing_name_is_unknown_and_never_falls_through() {
    let dir = branches_and_tags();
    let before = newest(dir.path());

    let no_branch = run(
        dir.path(),
        &["switch", "--detach", "heads/v1", "--actor", ACTOR],
    );
    assert_eq!(no_branch.code, 1, "{}", no_branch.stderr);
    assert!(
        no_branch.stderr.contains("no branch"),
        "{}",
        no_branch.stderr
    );

    let no_tag = run(
        dir.path(),
        &["switch", "--detach", "tags/later", "--actor", ACTOR],
    );
    assert_eq!(no_tag.code, 1, "{}", no_tag.stderr);
    assert!(no_tag.stderr.contains("no tag"), "{}", no_tag.stderr);

    let unknown = run(
        dir.path(),
        &["switch", "--detach", "nowhere", "--actor", ACTOR],
    );
    assert_eq!(unknown.code, 1, "{}", unknown.stderr);
    assert!(
        unknown.stderr.contains("neither a branch nor a tag"),
        "{}",
        unknown.stderr
    );
    assert_eq!(newest(dir.path()), before, "nothing was written");
}

#[test]
fn a_tag_is_no_branch_name_without_detach_and_the_parser_says_so() {
    let dir = branches_and_tags();

    let result = run(dir.path(), &["switch", "tags/v1", "--actor", ACTOR]);

    assert_eq!(result.code, 2, "{}", result.stderr);
    assert!(result.stderr.contains("--detach"), "{}", result.stderr);
    // The plain name of a branch that is also a tag still changes to the branch: `switch NAME`
    // reads no tags.
    ok(dir.path(), &["switch", "main", "--actor", ACTOR]);
    ok(dir.path(), &["switch", "heads/later", "--actor", ACTOR]);
}

#[test]
fn a_new_branch_or_tag_cannot_start_with_a_reserved_prefix() {
    let dir = branches_and_tags();
    let before = newest(dir.path());
    let tip = tip(dir.path());

    for args in [
        vec!["switch", "-c", "heads/x"],
        vec!["switch", "-c", "tags/x"],
        vec!["branch", "heads/x"],
        vec!["branch", "tags/x"],
        vec!["branch", "-m", "later", "heads/x"],
        vec!["tag", "heads/x"],
        vec!["tag", "tags/x", &tip],
    ] {
        let mut args = args;
        args.extend(["--actor", ACTOR]);
        let result = run(dir.path(), &args);
        assert_eq!(result.code, 1, "{args:?}: {}", result.stderr);
        assert!(
            result.stderr.contains("reserved"),
            "{args:?}: {}",
            result.stderr
        );
    }
    assert_eq!(newest(dir.path()), before, "nothing was written");
    // A plain name that is also a tag is a valid name for a new branch: `switch -c` reads no tags.
    ok(dir.path(), &["switch", "-c", "v1", "--actor", ACTOR]);
    ok(dir.path(), &["switch", "main", "--actor", ACTOR]);
    // A name that only contains the prefix further in is fine.
    ok(dir.path(), &["branch", "feat/heads/x", "--actor", ACTOR]);
    ok(dir.path(), &["tag", "feat/tags/x", "--actor", ACTOR]);
}

#[test]
fn the_start_of_a_new_branch_and_the_target_of_a_rebase_do_not_take_a_qualified_form_yet() {
    let dir = branches_and_tags();
    let before = newest(dir.path());

    for args in [
        vec!["switch", "-c", "x", "tags/v1"],
        vec!["switch", "-c", "x", "heads/main"],
        vec!["rebase", "tags/v1"],
        vec!["rebase", "heads/main"],
    ] {
        let mut args = args;
        args.extend(["--actor", ACTOR]);
        let result = run(dir.path(), &args);
        assert_eq!(result.code, 2, "{args:?}: {}", result.stderr);
        assert!(
            result.stderr.contains("qualified form"),
            "{args:?}: {}",
            result.stderr
        );
    }
    assert_eq!(newest(dir.path()), before, "nothing was written");
}

#[test]
fn the_message_of_a_detached_switch_keeps_the_prefix_of_a_qualified_form() {
    let dir = branches_and_tags();

    let branch = ok(
        dir.path(),
        &["switch", "--detach", "heads/main", "--actor", ACTOR],
    );
    assert!(
        branch
            .stdout
            .starts_with("Detached HEAD at heads/main as entry"),
        "{}",
        branch.stdout
    );
    let tag = ok(
        dir.path(),
        &["switch", "--detach", "tags/main", "--actor", ACTOR],
    );
    assert!(
        tag.stdout
            .starts_with("Detached HEAD at tags/main as entry"),
        "{}",
        tag.stdout
    );
}
