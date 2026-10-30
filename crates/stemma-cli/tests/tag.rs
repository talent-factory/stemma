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

/// The number of entries in the log.
fn entry_count(dir: &Dir) -> usize {
    ok(dir.path(), &["op", "log", "--json", "--limit", "1000"])
        .stdout
        .lines()
        .count()
}

#[test]
fn a_tag_never_moves_and_a_refusal_writes_no_entry() {
    let dir = repository();
    let first = commit(&dir, "first");
    ok(dir.path(), &["tag", "v1", "--actor", ACTOR]);
    let second = commit(&dir, "second");
    let before = entry_count(&dir);

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
    assert_eq!(entry_count(&dir), before, "a refusal wrote an entry");
    let log = ok(dir.path(), &["op", "log", "--json", "--limit", "1000"]).stdout;
    assert_eq!(log.matches("\"operation\":\"tag\"").count(), 1, "{log}");
}

#[test]
fn a_commit_that_is_not_in_the_store_is_refused_without_an_entry() {
    let dir = repository();
    commit(&dir, "first");
    let before = ok(dir.path(), &["op", "log", "--json"]).stdout;
    let missing = format!("b3:{}", "0".repeat(64));
    let result = run(dir.path(), &["tag", "v1", &missing, "--actor", ACTOR]);
    assert_eq!(result.code, 1, "{}", result.stderr);
    assert!(result.stderr.contains(&missing), "{}", result.stderr);
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
fn creating_a_tag_needs_an_actor_and_writes_nothing_without_one() {
    let dir = repository();
    commit(&dir, "first");
    let before = entry_count(&dir);
    let result = run(dir.path(), &["tag", "v1"]);
    assert_eq!(result.code, 1, "{}", result.stderr);
    assert!(result.stderr.contains("actor"), "{}", result.stderr);
    assert!(!dir.path().join(".stemma/refs/tags/v1").exists());
    assert_eq!(entry_count(&dir), before);
}

#[test]
fn a_commit_needs_a_name_before_it() {
    // `COMMIT` is the second operand, so a lone ID is read as the name and is not a valid one.
    let dir = repository();
    let result = run(
        dir.path(),
        &["tag", "--actor", ACTOR, &format!("b3:{}", "0".repeat(64))],
    );
    assert_ne!(result.code, 0);
    assert!(result.code < 100, "the command panicked: {}", result.stderr);
    assert!(!dir.path().join(".stemma/refs/tags").exists());
}

#[test]
fn a_tag_and_a_branch_may_share_a_name_and_the_log_keeps_them_apart() {
    let dir = repository();
    let first = commit(&dir, "first");
    ok(dir.path(), &["tag", "main", "--actor", ACTOR]);
    let log = ok(dir.path(), &["op", "log", "--limit", "1"]).stdout;
    assert!(log.contains(&format!("tag main  - -> {first}")), "{log}");
    assert!(!log.contains("branch main"), "{log}");
    assert!(!log.contains("! ") && !log.contains("? "), "{log}");
}

#[test]
fn several_tags_are_listed_sorted_in_the_json_of_their_entries() {
    let dir = repository();
    let first = commit(&dir, "first");
    for name in ["b", "a"] {
        ok(dir.path(), &["tag", name, "--actor", ACTOR]);
    }
    let json = ok(dir.path(), &["op", "log", "--json", "--limit", "2"]).stdout;
    // Newest first; every entry has the one tag that it made.
    let lines: Vec<&str> = json.lines().collect();
    assert!(lines[0].contains(&format!(
        "\"tags\":[{{\"name\":\"a\",\"old\":null,\"new\":\"{first}\"}}]"
    )));
    assert!(lines[1].contains(&format!(
        "\"tags\":[{{\"name\":\"b\",\"old\":null,\"new\":\"{first}\"}}]"
    )));
}

#[test]
fn a_changed_tag_line_is_found_by_verify() {
    let dir = repository();
    let first = commit(&dir, "first");
    let second = commit(&dir, "second");
    ok(dir.path(), &["tag", "v1", &first, "--actor", ACTOR]);
    ok(dir.path(), &["op", "verify"]);
    let number = ok(dir.path(), &["op", "log", "--json", "--limit", "1"])
        .stdout
        .split("\"number\":")
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .and_then(|number| number.parse::<u64>().ok())
        .unwrap();
    let file = entry_file(dir.path(), number);
    let text = fs::read_to_string(&file).unwrap();
    assert!(text.contains(&first));
    fs::write(&file, text.replace(&first, &second)).unwrap();
    let result = run(dir.path(), &["op", "verify"]);
    assert_eq!(result.code, 5, "{}{}", result.stdout, result.stderr);
}

#[test]
fn restoring_an_entry_that_only_made_a_tag_is_refused_and_the_tag_stays() {
    let dir = repository();
    let first = commit(&dir, "first");
    ok(dir.path(), &["tag", "v1", "--actor", ACTOR]);
    let number = ok(dir.path(), &["op", "log", "--json", "--limit", "1"])
        .stdout
        .split("\"number\":")
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .unwrap()
        .to_owned();
    let before = entry_count(&dir);
    let result = run(dir.path(), &["op", "restore", &number, "--actor", ACTOR]);
    assert_eq!(result.code, 1, "{}{}", result.stdout, result.stderr);
    assert!(
        result.stderr.contains("only made tags"),
        "{}",
        result.stderr
    );
    assert_eq!(
        fs::read_to_string(dir.path().join(".stemma/refs/tags/v1")).unwrap(),
        format!("{first}\n")
    );
    assert_eq!(entry_count(&dir), before);
}
