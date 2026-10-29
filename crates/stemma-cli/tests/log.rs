//! The command `stemma log`, run as a process, as a person or an agent would run it.

mod common;

use std::fs;

use common::{ok, repository, run, track_ok, write, Dir, ACTOR};

/// Makes a commit with the message `message` that adds a file of its own.
fn commit(dir: &Dir, message: &str) {
    let file = format!("{}.txt", message.replace(' ', "-"));
    write(dir.path(), &file, message);
    track_ok(dir.path(), &file);
    ok(dir.path(), &["commit", "--actor", ACTOR, "-m", message]);
}

/// The values of the field `field` in the JSON lines of `log --json`, as they are written.
fn field<'a>(lines: &[&'a str], field: &str) -> Vec<&'a str> {
    let key = format!("\"{field}\":");
    lines
        .iter()
        .map(|line| {
            let start = line.find(&key).unwrap() + key.len();
            let rest = &line[start..];
            let end = rest.find([',', '}']).unwrap();
            &rest[..end]
        })
        .collect()
}

#[test]
fn a_new_repository_shows_the_working_copy_commit_alone() {
    let dir = repository();
    let result = run(dir.path(), &["log"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(result.stderr, "");
    assert!(result.stdout.starts_with("@ b3:"), "{}", result.stdout);
    assert!(result.stdout.contains("parents  -\n"), "{}", result.stdout);
    assert_eq!(result.stdout.matches("\n@ ").count() + 1, 1);
}

#[test]
fn the_commits_follow_from_the_newest_to_the_first_and_the_working_copy_is_marked() {
    let dir = repository();
    commit(&dir, "first");
    commit(&dir, "second");
    let result = run(dir.path(), &["log"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(result.stderr, "");
    let text = &result.stdout;
    let marks: Vec<&str> = text
        .lines()
        .filter(|line| line.starts_with(['@', 'o']))
        .map(|line| &line[..1])
        .collect();
    // The working-copy commit on top, then the two commits, then the first working-copy commit
    // that `init` made and that the first commit named.
    assert_eq!(marks[0], "@");
    assert!(marks[1..].iter().all(|mark| *mark == "o"), "{text}");
    let second = text.find("    second\n").unwrap();
    let first = text.find("    first\n").unwrap();
    assert!(second < first, "{text}");
}

#[test]
fn the_limit_shows_the_newest_commits_only() {
    let dir = repository();
    commit(&dir, "first");
    commit(&dir, "second");
    let all = run(dir.path(), &["log", "--json"]);
    let limited = run(dir.path(), &["log", "-n", "2", "--json"]);
    assert_eq!(limited.code, 0, "{}", limited.stderr);
    let all_lines: Vec<&str> = all.stdout.lines().collect();
    let limited_lines: Vec<&str> = limited.stdout.lines().collect();
    assert_eq!(limited_lines.len(), 2);
    assert!(all_lines.len() > 2);
    assert_eq!(limited_lines[..], all_lines[..2]);
}

#[test]
fn a_limit_of_zero_is_a_usage_error() {
    let dir = repository();
    let result = run(dir.path(), &["log", "-n", "0"]);
    assert_eq!(result.code, 2, "{}", result.stderr);
    assert_eq!(result.stdout, "");
}

#[test]
fn json_has_one_object_per_line_with_the_schema_and_the_fields() {
    let dir = repository();
    commit(&dir, "first");
    let result = run(dir.path(), &["log", "--json"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(result.stderr, "");
    let lines: Vec<&str> = result.stdout.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines.iter().all(|line| line.starts_with("{\"schema\":1,")));
    assert_eq!(field(&lines, "working_copy"), vec!["true", "false"]);
    let parents = field(&lines, "parents");
    // Each commit names the next line as its parent; the oldest has none.
    let ids = field(&lines, "commit");
    assert_eq!(parents[0], format!("[{}", ids[1]).as_str().to_owned() + "]");
    assert_eq!(parents[1], "[]");
    assert!(lines[1].contains("\"message\":\"first\""), "{}", lines[1]);
    assert!(
        lines[1].contains("\"author\":\"human daniel\""),
        "{}",
        lines[1]
    );
}

#[test]
fn json_escapes_a_message_with_quotes_and_newlines() {
    let dir = repository();
    let message = "say \"hi\"\nsecond line";
    write(dir.path(), "a.txt", "a");
    track_ok(dir.path(), "a.txt");
    ok(dir.path(), &["commit", "--actor", ACTOR, "-m", message]);
    let result = run(dir.path(), &["log", "--json"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(
        result
            .stdout
            .contains("\"message\":\"say \\\"hi\\\"\\u000asecond line\""),
        "{}",
        result.stdout
    );
}

#[test]
fn the_text_indents_every_line_of_the_message() {
    let dir = repository();
    write(dir.path(), "a.txt", "a");
    track_ok(dir.path(), "a.txt");
    ok(
        dir.path(),
        &["commit", "--actor", ACTOR, "-m", "title\n\nbody"],
    );
    let result = run(dir.path(), &["log"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(
        result.stdout.contains("    title\n    \n    body\n"),
        "{}",
        result.stdout
    );
}

#[test]
fn first_parent_on_a_linear_history_shows_the_same_commits() {
    let dir = repository();
    commit(&dir, "first");
    commit(&dir, "second");
    let all = run(dir.path(), &["log", "--json"]);
    let first = run(dir.path(), &["log", "--first-parent", "--json"]);
    assert_eq!(first.code, 0, "{}", first.stderr);
    assert_eq!(first.stdout, all.stdout);
}

#[test]
fn log_needs_no_actor_and_changes_nothing() {
    let dir = repository();
    commit(&dir, "first");
    let before = run(dir.path(), &["op", "log", "--json"]).stdout;
    let result = run(dir.path(), &["log"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(run(dir.path(), &["op", "log", "--json"]).stdout, before);
}

#[test]
fn log_works_from_a_directory_below_the_root() {
    let dir = repository();
    fs::create_dir(dir.path().join("sub")).unwrap();
    let below = dir.path().join("sub");
    let result = run(&below, &["log"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(result.stdout.starts_with("@ b3:"), "{}", result.stdout);
}

#[test]
fn outside_a_repository_log_fails_with_code_1() {
    let dir = Dir::new();
    let result = run(dir.path(), &["log"]);
    assert_eq!(result.code, 1, "{}", result.stderr);
    assert_eq!(result.stdout, "");
    assert!(result.stderr.starts_with("stemma: "), "{}", result.stderr);
}

#[test]
fn a_missing_parent_commit_is_damage_with_code_5_and_lists_nothing() {
    let dir = repository();
    commit(&dir, "first");
    commit(&dir, "second");
    // Remove the object of the oldest commit: the second line of the list names it as a parent.
    let json = run(dir.path(), &["log", "--json"]).stdout;
    let lines: Vec<&str> = json.lines().collect();
    let oldest = field(&lines, "commit")[2].trim_matches('"').to_owned();
    let digits = oldest.strip_prefix("b3:").unwrap();
    let object = dir
        .path()
        .join(".stemma/objects/b3")
        .join(&digits[..2])
        .join(&digits[2..]);
    fs::remove_file(object).unwrap();

    let result = run(dir.path(), &["log"]);
    assert_eq!(result.code, 5, "{}", result.stderr);
    assert_eq!(result.stdout, "");
    assert!(
        result.stderr.contains("not in the object store"),
        "{}",
        result.stderr
    );
}

#[test]
fn the_text_masks_control_characters_of_the_message_and_keeps_other_characters() {
    let dir = repository();
    write(dir.path(), "a.txt", "a");
    track_ok(dir.path(), "a.txt");
    let message = "red \u{1b}[31m text\rline \u{85} \u{2028} Bär";
    ok(dir.path(), &["commit", "--actor", ACTOR, "-m", message]);
    let result = run(dir.path(), &["log"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(!result
        .stdout
        .contains(['\u{1b}', '\r', '\u{85}', '\u{2028}']));
    assert!(
        result
            .stdout
            .contains("    red \\u{1b}[31m text\\u{d}line \\u{85} \\u{2028} Bär\n"),
        "{}",
        result.stdout
    );
}
