//! The command `stemma op log`, run as a process, as a person or an agent would run it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

/// A directory below the system temp directory that is removed on drop.
struct Dir(PathBuf);

impl Dir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "stemma-cli-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// What a process did.
struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Runs `stemma` in `dir` without `STEMMA_ACTOR`; `extra` can adjust the command before it runs.
fn stemma(dir: &Path, args: &[&str], extra: impl FnOnce(&mut Command)) -> Run {
    let mut command = Command::new(env!("CARGO_BIN_EXE_stemma"));
    command
        .current_dir(dir)
        .env_remove("STEMMA_ACTOR")
        .args(args);
    extra(&mut command);
    let output = command.output().unwrap();
    Run {
        code: output.status.code().expect("the process ended by a signal"),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn run(dir: &Path, args: &[&str]) -> Run {
    stemma(dir, args, |_| {})
}

const A: &str = "b3:1111111111111111111111111111111111111111111111111111111111111111";
const C: &str = "b3:3333333333333333333333333333333333333333333333333333333333333333";
const B: &str = "b3:2222222222222222222222222222222222222222222222222222222222222222";

/// A repository made by `init`, with the actor `human daniel`.
fn repository() -> Dir {
    let dir = Dir::new();
    let result = run(dir.path(), &["init", "--actor", "human daniel"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    dir
}

/// Writes an entry file as the log would, so that a test needs no command that makes one.
fn write_entry(dir: &Path, number: u64, body: &str) {
    let text = format!("stemma-oplog 1\nnumber {number}\n{body}");
    fs::write(
        dir.join(".stemma/oplog").join(format!("{number:012}")),
        text,
    )
    .unwrap();
}

fn snapshot(dir: &Path, number: u64, old: &str, new: &str) {
    write_entry(
        dir,
        number,
        &format!(
            "time 1700000000\nactor human daniel\noperation snapshot\nworking-copy {old} {new}\n\
             branch main {old} {new}\n"
        ),
    );
}

#[test]
fn a_new_repository_shows_the_entry_of_init() {
    let dir = repository();
    let result = run(dir.path(), &["op", "log"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(result.stderr, "");
    let mut lines = result.stdout.lines();
    let first = lines.next().unwrap();
    assert!(
        first.starts_with("1  ") && first.ends_with("  human daniel  init"),
        "{first}"
    );
    assert_eq!(lines.next(), Some("    head  - -> branch:main"));
    assert_eq!(lines.next(), None);
}

#[test]
fn the_newest_entry_comes_first_and_the_limit_cuts_the_old_ones() {
    let dir = repository();
    snapshot(dir.path(), 2, "-", A);
    snapshot(dir.path(), 3, A, B);
    let all = run(dir.path(), &["op", "log"]);
    assert_eq!(all.code, 0, "{}", all.stderr);
    let numbers: Vec<_> = all
        .stdout
        .lines()
        .filter(|line| !line.starts_with(' '))
        .map(|line| line.split("  ").next().unwrap())
        .collect();
    assert_eq!(numbers, ["3", "2", "1"]);
    assert!(all
        .stdout
        .contains(&format!("    working-copy  {A} -> {B}\n")));
    assert!(all.stdout.contains(&format!("    branch main  - -> {A}\n")));

    for limit in [["-n", "2"], ["--limit", "2"]] {
        let result = run(dir.path(), &["op", "log", limit[0], limit[1]]);
        assert_eq!(result.code, 0, "{}", result.stderr);
        assert!(result.stdout.starts_with("3  "), "{}", result.stdout);
        assert!(!result.stdout.contains("\n1  "), "{}", result.stdout);
    }
}

#[test]
fn the_limit_spares_an_entry_it_does_not_show() {
    // The old entry is damaged, but the limit does not read it.
    let dir = repository();
    snapshot(dir.path(), 2, "-", A);
    fs::write(dir.path().join(".stemma/oplog/000000000001"), "damaged").unwrap();
    let result = run(dir.path(), &["op", "log", "-n", "1"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(result.stdout.starts_with("2  "), "{}", result.stdout);
}

#[test]
fn the_json_form_is_one_object_per_line() {
    let dir = repository();
    snapshot(dir.path(), 2, "-", A);
    let result = run(dir.path(), &["op", "log", "--json"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    let lines: Vec<_> = result.stdout.lines().collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(
        lines[0],
        format!(
            "{{\"number\":2,\"time\":1700000000,\"actor\":\"human daniel\",\
             \"operation\":\"snapshot\",\"head\":null,\
             \"working_copy\":{{\"old\":null,\"new\":\"{A}\"}},\
             \"branches\":[{{\"name\":\"main\",\"old\":null,\"new\":\"{A}\"}}],\
             \"took_effect\":false}}"
        )
    );
    assert!(
        lines[1].starts_with("{\"number\":1,")
            && lines[1].contains("\"head\":{\"old\":null,\"new\":\"branch:main\"}")
            && lines[1].ends_with("\"branches\":[],\"took_effect\":true}"),
        "{}",
        lines[1]
    );
}

#[test]
fn the_repository_can_be_named() {
    let dir = repository();
    let elsewhere = Dir::new();
    let path = dir.path().to_str().unwrap();
    let result = run(elsewhere.path(), &["op", "log", path]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(result.stdout.starts_with("1  "), "{}", result.stdout);
}

#[test]
fn a_gap_is_reported_and_nothing_is_listed() {
    let dir = repository();
    snapshot(dir.path(), 3, A, B);
    let result = run(dir.path(), &["op", "log"]);
    assert_eq!(result.code, 1);
    assert!(
        result
            .stderr
            .starts_with("stemma: the operation log has no entry 2"),
        "{}",
        result.stderr
    );
    assert_eq!(result.stdout, "");
}

#[test]
fn a_damaged_entry_is_reported_and_nothing_is_listed() {
    let dir = repository();
    snapshot(dir.path(), 2, "-", A);
    write_entry(dir.path(), 3, "time 1\nactor human daniel\n");
    let result = run(dir.path(), &["op", "log"]);
    assert_eq!(result.code, 1);
    assert!(
        result.stderr.starts_with("stemma: the entry `") && result.stderr.contains("000000000003"),
        "{}",
        result.stderr
    );
    assert_eq!(result.stdout, "");
}

#[test]
fn outside_of_a_repository_the_command_fails() {
    let dir = Dir::new();
    let result = run(dir.path(), &["op", "log"]);
    assert_eq!(result.code, 1);
    assert!(result.stderr.starts_with("stemma: "), "{}", result.stderr);
    assert_eq!(result.stdout, "");
}

#[test]
fn a_limit_of_zero_is_a_usage_error() {
    let dir = repository();
    let result = run(dir.path(), &["op", "log", "-n", "0"]);
    assert_eq!(result.code, 2);
    assert!(result.stderr.contains("--limit"), "{}", result.stderr);
}

#[test]
fn the_log_needs_no_actor() {
    let dir = repository();
    let result = run(dir.path(), &["op", "log"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
}

/// The numbers of the entries that a text listing shows, in order.
fn numbers_of(stdout: &str) -> Vec<u64> {
    stdout
        .lines()
        .filter(|line| !line.starts_with(' '))
        .map(|line| line.split("  ").next().unwrap().parse().unwrap())
        .collect()
}

#[test]
fn head_and_branch_moves_are_shown_in_both_directions() {
    let dir = repository();
    write_entry(
        dir.path(),
        2,
        &format!(
            "time 1700000000\nactor human daniel\noperation checkout\n\
             head branch:main commit:{A}\nbranch main {A} {B}\n"
        ),
    );
    write_entry(
        dir.path(),
        3,
        &format!(
            "time 1700000000\nactor human daniel\noperation delete\n\
             head commit:{A} -\nbranch main {B} -\n"
        ),
    );
    let text = run(dir.path(), &["op", "log"]);
    assert_eq!(text.code, 0, "{}", text.stderr);
    for line in [
        format!("    head  branch:main -> commit:{A}\n"),
        format!("    head  commit:{A} -> -\n"),
        format!("    branch main  {A} -> {B}\n"),
        format!("    branch main  {B} -> -\n"),
    ] {
        assert!(text.stdout.contains(&line), "{line}: {}", text.stdout);
    }
    let json = run(dir.path(), &["op", "log", "--json"]);
    let lines: Vec<_> = json.stdout.lines().collect();
    assert!(
        lines[0].contains(&format!("\"head\":{{\"old\":\"commit:{A}\",\"new\":null}}"))
            && lines[0].contains(&format!(
                "\"branches\":[{{\"name\":\"main\",\"old\":\"{B}\",\"new\":null}}]"
            )),
        "{}",
        lines[0]
    );
    assert!(
        lines[1].contains(&format!(
            "\"head\":{{\"old\":\"branch:main\",\"new\":\"commit:{A}\"}}"
        )) && lines[1].contains(&format!("\"old\":\"{A}\",\"new\":\"{B}\"")),
        "{}",
        lines[1]
    );
}

#[test]
fn without_a_limit_the_twenty_newest_entries_are_shown() {
    let dir = repository();
    for number in 2..=21 {
        let (old, new) = if number % 2 == 0 { (A, B) } else { (B, A) };
        snapshot(dir.path(), number, if number == 2 { "-" } else { old }, new);
    }
    let shown = run(dir.path(), &["op", "log"]);
    assert_eq!(shown.code, 0, "{}", shown.stderr);
    assert_eq!(
        numbers_of(&shown.stdout),
        (2..=21).rev().collect::<Vec<_>>()
    );
    let more = run(dir.path(), &["op", "log", "-n", "100"]);
    assert_eq!(numbers_of(&more.stdout).len(), 21);
}

/// An entry that moves only the branch `main`, so that its outcome can be decided.
fn branch_move(dir: &Path, number: u64, old: &str, new: &str) {
    write_entry(
        dir,
        number,
        &format!(
            "time 1700000000\nactor human daniel\noperation commit\nbranch main {old} {new}\n"
        ),
    );
}

fn set_branch(dir: &Path, commit: &str) {
    fs::write(dir.join(".stemma/refs/heads/main"), format!("{commit}\n")).unwrap();
}

#[test]
fn an_entry_whose_move_took_effect_has_no_marker() {
    let dir = repository();
    branch_move(dir.path(), 2, "-", A);
    set_branch(dir.path(), A);
    let text = run(dir.path(), &["op", "log"]);
    assert_eq!(text.code, 0, "{}", text.stderr);
    assert!(
        !text.stdout.contains('!') && !text.stdout.contains('?'),
        "{}",
        text.stdout
    );
    let json = run(dir.path(), &["op", "log", "--json"]);
    assert!(
        json.stdout
            .lines()
            .all(|line| line.ends_with("\"took_effect\":true}")),
        "{}",
        json.stdout
    );
}

#[test]
fn an_entry_whose_move_never_happened_is_marked_in_both_forms() {
    // The entry 2 was written, but the branch did not move: entry 3 starts from the old value.
    let dir = repository();
    branch_move(dir.path(), 2, "-", A);
    branch_move(dir.path(), 3, "-", B);
    set_branch(dir.path(), B);
    let text = run(dir.path(), &["op", "log"]);
    assert_eq!(text.code, 0, "{}", text.stderr);
    let marker = "    ! the move did not take effect\n";
    let lines: Vec<_> = text.stdout.split_inclusive('\n').collect();
    let position = lines.iter().position(|l| l.starts_with("2  ")).unwrap();
    assert_eq!(lines[position + 2], marker, "{}", text.stdout);
    assert_eq!(text.stdout.matches(marker).count(), 1, "{}", text.stdout);
    let json = run(dir.path(), &["op", "log", "--json"]);
    let flags: Vec<_> = json
        .stdout
        .lines()
        .map(|line| line.rsplit(":").next().unwrap().trim_end_matches('}'))
        .collect();
    assert_eq!(flags, ["true", "false", "true"]);
}

#[test]
fn an_outcome_that_is_not_known_is_null_and_marked_with_a_question_mark() {
    // The branch has a value that is neither the old nor the new one of the last entry.
    let dir = repository();
    branch_move(dir.path(), 2, "-", A);
    set_branch(dir.path(), B);
    let text = run(dir.path(), &["op", "log"]);
    assert!(
        text.stdout
            .contains("    ? it is not known whether the move took effect\n"),
        "{}",
        text.stdout
    );
    let json = run(dir.path(), &["op", "log", "--json"]);
    assert!(json
        .stdout
        .lines()
        .next()
        .unwrap()
        .ends_with("\"took_effect\":null}"));
}

/// The value of `took_effect` of each line of the JSON listing, newest first.
fn flags(dir: &Path, extra: &[&str]) -> Vec<String> {
    let mut args = vec!["op", "log", "--json"];
    args.extend_from_slice(extra);
    let result = run(dir, &args);
    assert_eq!(result.code, 0, "{}", result.stderr);
    result
        .stdout
        .lines()
        .map(|line| {
            let (_, flag) = line.rsplit_once("\"took_effect\":").unwrap();
            flag.trim_end_matches('}').to_owned()
        })
        .collect()
}

#[test]
fn a_branch_that_was_deleted_took_effect_if_it_is_gone() {
    let dir = repository();
    branch_move(dir.path(), 2, "-", A);
    branch_move(dir.path(), 3, A, "-");
    // No `refs/heads/main`: the deletion happened.
    assert_eq!(flags(dir.path(), &[]), ["true", "true", "true"]);
    set_branch(dir.path(), A);
    assert_eq!(flags(dir.path(), &[]), ["false", "true", "true"]);
}

#[test]
fn a_move_that_started_from_a_third_value_is_not_called_a_failure() {
    // Something outside the log changed `main` between the two entries.
    let dir = repository();
    branch_move(dir.path(), 2, "-", A);
    branch_move(dir.path(), 3, B, A);
    set_branch(dir.path(), A);
    assert_eq!(flags(dir.path(), &[]), ["true", "null", "true"]);
}

#[test]
fn while_a_writer_holds_the_lock_an_old_value_is_not_called_a_failure() {
    let dir = repository();
    branch_move(dir.path(), 2, "-", A);
    assert_eq!(flags(dir.path(), &[]), ["false", "true"]);
    fs::write(dir.path().join(".stemma/refs.lock"), "writer").unwrap();
    assert_eq!(flags(dir.path(), &[]), ["null", "true"]);
}

#[test]
fn the_limit_does_not_change_the_outcome_of_a_shown_entry() {
    let dir = repository();
    branch_move(dir.path(), 2, "-", A);
    branch_move(dir.path(), 3, A, B);
    branch_move(dir.path(), 4, A, C);
    set_branch(dir.path(), C);
    assert_eq!(flags(dir.path(), &[]), ["true", "false", "true", "true"]);
    assert_eq!(flags(dir.path(), &["-n", "2"]), ["true", "false"]);
}

#[test]
fn references_that_cannot_be_read_give_a_warning_and_the_outcome_of_the_chain() {
    let dir = repository();
    branch_move(dir.path(), 2, "-", A);
    branch_move(dir.path(), 3, A, B);
    // A file of a branch that is not in canonical form.
    fs::write(dir.path().join(".stemma/refs/heads/main"), "damaged").unwrap();
    let result = run(dir.path(), &["op", "log", "--json"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(
        result
            .stderr
            .starts_with("stemma: warning: could not read the references"),
        "{}",
        result.stderr
    );
    assert!(result.stderr.contains("main"), "{}", result.stderr);
    let flags: Vec<_> = result
        .stdout
        .lines()
        .map(|line| {
            line.rsplit_once(':')
                .unwrap()
                .1
                .trim_end_matches('}')
                .to_owned()
        })
        .collect();
    // Entry 2 is confirmed by entry 3; entry 3 and the entry of `init` need the references.
    assert_eq!(flags, ["null", "true", "null"]);
    // The text form has the question mark on the same entries.
    let text = run(dir.path(), &["op", "log"]);
    assert_eq!(text.code, 0, "{}", text.stderr);
    assert_eq!(text.stdout.matches("    ? it is not known").count(), 2);
}

#[test]
fn a_head_that_is_missing_is_a_warning_too() {
    let dir = repository();
    fs::remove_file(dir.path().join(".stemma/HEAD")).unwrap();
    let result = run(dir.path(), &["op", "log"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(
        result.stderr.starts_with("stemma: warning: "),
        "{}",
        result.stderr
    );
    assert!(result.stdout.starts_with("1  "), "{}", result.stdout);
}
