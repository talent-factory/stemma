//! The command `stemma tag`, run as a process, as a person or an agent would run it.

mod common;

use std::fs;

use common::{entry_file, ok, repository, run, track_ok, write, Dir, ACTOR};

/// Makes a commit with the message `message` that adds a file of its own, and returns its ID.
fn commit(dir: &Dir, message: &str) -> String {
    let file = format!("{}.txt", message.replace(' ', "-"));
    write(dir.path(), &file, message);
    track_ok(dir.path(), &file);
    let result = ok(dir.path(), &["commit", "--actor", ACTOR, "-m", message]);
    // "Committed b3:… on main"
    result
        .stdout
        .split_whitespace()
        .find(|word| word.starts_with("b3:"))
        .expect("the output names the commit")
        .to_owned()
}

#[test]
fn a_tag_names_the_tip_of_the_branch_by_default_and_the_log_says_who_and_when() {
    let dir = repository();
    let first = commit(&dir, "first");
    let result = run(dir.path(), &["tag", "v1", "--actor", ACTOR]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(result.stdout, format!("Tagged v1 at {first}\n"));
    assert_eq!(
        fs::read_to_string(dir.path().join(".stemma/refs/tags/v1")).unwrap(),
        format!("{first}\n")
    );

    let log = ok(dir.path(), &["op", "log"]);
    let newest = log.stdout.lines().next().unwrap();
    assert!(newest.contains("tag"), "{}", log.stdout);
    assert!(
        log.stdout.contains(&format!("tag v1  - -> {first}")),
        "{}",
        log.stdout
    );
    assert!(!log.stdout.contains("! "), "{}", log.stdout);
    assert!(!log.stdout.contains("? "), "{}", log.stdout);

    let json = ok(dir.path(), &["op", "log", "--json", "--limit", "1"]);
    assert!(
        json.stdout.contains("\"operation\":\"tag\""),
        "{}",
        json.stdout
    );
    assert!(
        json.stdout.contains(&format!(
            "\"tags\":[{{\"name\":\"v1\",\"old\":null,\"new\":\"{first}\"}}]"
        )),
        "{}",
        json.stdout
    );
    assert!(
        json.stdout.contains("\"took_effect\":true"),
        "{}",
        json.stdout
    );
}

#[test]
fn the_entry_of_a_tag_is_a_line_below_the_branches() {
    let dir = repository();
    let first = commit(&dir, "first");
    ok(dir.path(), &["tag", "v1", "--actor", ACTOR]);
    let number = ok(dir.path(), &["op", "log", "--json", "--limit", "1"])
        .stdout
        .split("\"number\":")
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .and_then(|number| number.parse::<u64>().ok())
        .unwrap();
    let text = fs::read_to_string(entry_file(dir.path(), number)).unwrap();
    assert!(text.contains("\noperation tag\n"), "{text}");
    assert!(text.ends_with(&format!("tag v1 - {first}\n")), "{text}");
    assert!(!text.contains("\nbranch "), "{text}");
    ok(dir.path(), &["op", "verify"]);
}

#[test]
fn a_commit_can_be_named() {
    let dir = repository();
    let first = commit(&dir, "first");
    commit(&dir, "second");
    ok(dir.path(), &["tag", "old", &first, "--actor", ACTOR]);
    let list = ok(dir.path(), &["tag"]);
    assert_eq!(list.stdout, format!("old {first}\n"));
}

#[test]
fn without_a_name_the_tags_are_listed_by_name_and_need_no_actor() {
    let dir = repository();
    let first = commit(&dir, "first");
    ok(dir.path(), &["tag", "b", "--actor", ACTOR]);
    ok(dir.path(), &["tag", "a", "--actor", ACTOR]);
    let list = ok(dir.path(), &["tag"]);
    assert_eq!(list.stdout, format!("a {first}\nb {first}\n"));
    let json = ok(dir.path(), &["tag", "--json"]);
    assert_eq!(
        json.stdout,
        format!(
            "{{\"schema\":1,\"tag\":\"a\",\"commit\":\"{first}\"}}\n\
             {{\"schema\":1,\"tag\":\"b\",\"commit\":\"{first}\"}}\n"
        )
    );
}

#[test]
fn a_tag_never_moves_and_a_refusal_writes_no_entry() {
    let dir = repository();
    let first = commit(&dir, "first");
    ok(dir.path(), &["tag", "v1", "--actor", ACTOR]);
    let entries = ok(dir.path(), &["op", "log", "--json"])
        .stdout
        .lines()
        .count();
    let second = commit(&dir, "second");

    // At another commit and at the same commit: both are refused.
    for target in [&second, &first] {
        let again = run(dir.path(), &["tag", "v1", target, "--actor", ACTOR]);
        assert_eq!(again.code, 1, "{}", again.stderr);
        assert!(again.stderr.contains("v1"), "{}", again.stderr);
        assert!(again.stderr.contains("never moves"), "{}", again.stderr);
    }
    assert_eq!(
        fs::read_to_string(dir.path().join(".stemma/refs/tags/v1")).unwrap(),
        format!("{first}\n")
    );
    // The commit made two entries (a snapshot may add more); the refusals added none.
    let after = ok(dir.path(), &["op", "log", "--json"]).stdout;
    assert!(
        !after.contains("\"operation\":\"tag\"")
            || after.matches("\"operation\":\"tag\"").count() == 1,
        "{after}"
    );
    assert!(after.lines().count() > entries);
}

#[test]
fn a_commit_that_is_not_in_the_store_is_refused_without_an_entry() {
    let dir = repository();
    commit(&dir, "first");
    let before = ok(dir.path(), &["op", "log", "--json"]).stdout;
    let missing = format!("b3:{}", "0".repeat(64));
    let result = run(dir.path(), &["tag", "v1", &missing, "--actor", ACTOR]);
    assert_eq!(result.code, 1, "{}", result.stderr);
    assert_eq!(ok(dir.path(), &["op", "log", "--json"]).stdout, before);
    assert!(!dir.path().join(".stemma/refs/tags/v1").exists());
}

#[test]
fn a_commit_is_needed_when_the_branch_has_none() {
    let dir = repository();
    let result = run(dir.path(), &["tag", "v1", "--actor", ACTOR]);
    assert_eq!(result.code, 1, "{}", result.stderr);
    assert!(
        result.stderr.contains("no commit to tag"),
        "{}",
        result.stderr
    );
}

#[test]
fn creating_a_tag_needs_an_actor() {
    let dir = repository();
    commit(&dir, "first");
    let result = run(dir.path(), &["tag", "v1"]);
    assert_eq!(result.code, 1, "{}", result.stderr);
    assert!(!dir.path().join(".stemma/refs/tags/v1").exists());
}

#[test]
fn a_commit_without_a_name_is_a_usage_error() {
    let dir = repository();
    let result = run(
        dir.path(),
        &["tag", "--actor", ACTOR, &format!("b3:{}", "0".repeat(64))],
    );
    // The first operand is the name, which is not a valid one; either way nothing is made.
    assert_ne!(result.code, 0);
    assert!(!dir.path().join(".stemma/refs/tags").exists());
}

#[test]
fn a_tag_is_deleted_and_set_again_and_the_log_says_who_and_when() {
    let dir = repository();
    let first = commit(&dir, "first");
    ok(dir.path(), &["tag", "v1", "--actor", ACTOR]);
    let second = commit(&dir, "second");

    let result = run(dir.path(), &["tag", "--delete", "v1", "--actor", ACTOR]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(result.stdout, format!("Deleted v1, which was at {first}\n"));
    assert!(!dir.path().join(".stemma/refs/tags/v1").exists());
    assert_eq!(ok(dir.path(), &["tag"]).stdout, "");

    let log = ok(dir.path(), &["op", "log"]);
    let newest = log.stdout.lines().next().unwrap();
    assert!(newest.contains("untag"), "{}", log.stdout);
    assert!(
        log.stdout.contains(&format!("tag v1  {first} -> -")),
        "{}",
        log.stdout
    );
    assert!(!log.stdout.contains("! "), "{}", log.stdout);
    assert!(!log.stdout.contains("? "), "{}", log.stdout);
    let json = ok(dir.path(), &["op", "log", "--json", "--limit", "1"]);
    assert!(
        json.stdout.contains("\"operation\":\"untag\"")
            && json.stdout.contains(&format!(
                "\"tags\":[{{\"name\":\"v1\",\"old\":\"{first}\",\"new\":null}}]"
            ))
            && json.stdout.contains("\"took_effect\":true"),
        "{}",
        json.stdout
    );
    // The newest line names the actor who deleted the tag.
    assert!(newest.contains("human daniel"), "{}", log.stdout);
    ok(dir.path(), &["op", "verify"]);

    // The name is free again.
    ok(dir.path(), &["tag", "v1", &second, "--actor", ACTOR]);
    assert_eq!(ok(dir.path(), &["tag"]).stdout, format!("v1 {second}\n"));
    // Setting, deleting, and setting again all took effect.
    let log = ok(dir.path(), &["op", "log", "--json"]).stdout;
    for line in log.lines().filter(|line| line.contains("\"tags\":[{")) {
        assert!(line.contains("\"took_effect\":true"), "{line}");
    }
    assert_eq!(log.matches("\"tags\":[{").count(), 3, "{log}");
}

#[test]
fn a_nested_tag_is_deleted_but_a_name_that_is_only_a_directory_is_not_a_tag() {
    let dir = repository();
    let first = commit(&dir, "first");
    ok(dir.path(), &["tag", "rel/v1", "--actor", ACTOR]);

    // `rel` is a directory with a tag below it, not a tag.
    let before = ok(dir.path(), &["op", "log", "--json"]).stdout;
    let result = run(dir.path(), &["tag", "--delete", "rel", "--actor", ACTOR]);
    assert_eq!(result.code, 1, "{}", result.stderr);
    assert!(
        result.stderr.contains("no tag `rel` to delete"),
        "{}",
        result.stderr
    );
    assert_eq!(ok(dir.path(), &["op", "log", "--json"]).stdout, before);

    let result = run(
        dir.path(),
        &["tag", "-d", "rel/v1", "--json", "--actor", ACTOR],
    );
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(
        result.stdout,
        format!("{{\"schema\":1,\"tag\":\"rel/v1\",\"commit\":\"{first}\",\"deleted\":true}}\n")
    );
    // The directory that the deletion emptied goes with it.
    assert!(!dir.path().join(".stemma/refs/tags/rel").exists());
}

#[test]
fn deleting_a_tag_can_write_json_and_short_d_does_the_same() {
    let dir = repository();
    let first = commit(&dir, "first");
    ok(dir.path(), &["tag", "v1", "--actor", ACTOR]);
    let result = run(dir.path(), &["tag", "-d", "v1", "--json", "--actor", ACTOR]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(
        result.stdout,
        format!("{{\"schema\":1,\"tag\":\"v1\",\"commit\":\"{first}\",\"deleted\":true}}\n")
    );
}

#[test]
fn deleting_a_tag_that_does_not_exist_is_refused_without_an_entry() {
    let dir = repository();
    commit(&dir, "first");
    let before = ok(dir.path(), &["op", "log", "--json"]).stdout;
    let result = run(dir.path(), &["tag", "--delete", "v1", "--actor", ACTOR]);
    assert_eq!(result.code, 1, "{}", result.stderr);
    assert!(
        result.stderr.contains("no tag `v1` to delete"),
        "{}",
        result.stderr
    );
    assert_eq!(ok(dir.path(), &["op", "log", "--json"]).stdout, before);
}

#[test]
fn deleting_a_tag_needs_an_actor_and_a_name_and_no_commit() {
    let dir = repository();
    let first = commit(&dir, "first");
    ok(dir.path(), &["tag", "v1", "--actor", ACTOR]);
    let before = ok(dir.path(), &["op", "log", "--json"]).stdout;

    let result = run(dir.path(), &["tag", "--delete", "v1"]);
    assert_eq!(result.code, 1, "{}", result.stderr);
    assert!(result.stderr.contains("actor"), "{}", result.stderr);
    // A usage error: no name, or a commit that a deletion does not take.
    let nameless = run(dir.path(), &["tag", "--delete", "--actor", ACTOR]);
    assert_eq!(nameless.code, 2, "{}", nameless.stderr);
    let with_commit = run(
        dir.path(),
        &["tag", "--delete", "v1", &first, "--actor", ACTOR],
    );
    assert_eq!(with_commit.code, 2, "{}", with_commit.stderr);

    assert!(dir.path().join(".stemma/refs/tags/v1").is_file());
    assert_eq!(ok(dir.path(), &["op", "log", "--json"]).stdout, before);
}
