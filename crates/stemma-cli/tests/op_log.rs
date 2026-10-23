//! The command `stemma op log`, run as a process, as a person or an agent would run it.

mod common;

use std::fs;
use std::path::Path;

use common::{entry_file, previous, repository, run, write_stub_entry, Dir};

const A: &str = "b3:1111111111111111111111111111111111111111111111111111111111111111";
const C: &str = "b3:3333333333333333333333333333333333333333333333333333333333333333";
const B: &str = "b3:2222222222222222222222222222222222222222222222222222222222222222";

/// Writes an entry file as the log would, in the hash chain, so that a test needs no command that
/// makes one. `body` starts with the lines `time` and `actor`.
fn write_entry(dir: &Path, number: u64, body: &str) {
    let mut lines = body.splitn(3, '\n');
    let (time, actor, rest) = (
        lines.next().unwrap(),
        lines.next().unwrap(),
        lines.next().unwrap(),
    );
    let text = format!(
        "stemma-oplog 3\nnumber {number}\n{time}\n{actor}\nprevious {}\n{rest}",
        previous(dir, number)
    );
    fs::write(entry_file(dir, number), text).unwrap();
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
    let working_copy = lines.next().unwrap();
    assert!(
        working_copy.starts_with("    working-copy  - -> b3:"),
        "{working_copy}"
    );
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
    // The entry starts from the working-copy commit that `init` made.
    let first = fs::read_to_string(dir.path().join(".stemma/working-copy"))
        .unwrap()
        .trim()
        .to_owned();
    write_entry(
        dir.path(),
        2,
        &format!(
            "time 1700000000\nactor human daniel\noperation snapshot\n\
             working-copy {first} {A}\nbranch main - {A}\n"
        ),
    );
    let result = run(dir.path(), &["op", "log", "--json"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    let lines: Vec<_> = result.stdout.lines().collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(
        lines[0],
        format!(
            "{{\"number\":2,\"kind\":\"change\",\"time\":1700000000,\"actor\":\"human daniel\",\
             \"operation\":\"snapshot\",\"head\":null,\
             \"working_copy\":{{\"old\":\"{first}\",\"new\":\"{A}\"}},\
             \"branches\":[{{\"name\":\"main\",\"old\":null,\"new\":\"{A}\"}}],\
             \"purge_pending\":false,\"took_effect\":false}}"
        )
    );
    assert!(
        lines[1].starts_with("{\"number\":1,")
            && lines[1].contains("\"head\":{\"old\":null,\"new\":\"branch:main\"}")
            && lines[1].ends_with("\"branches\":[],\"purge_pending\":false,\"took_effect\":true}"),
        "{}",
        lines[1]
    );
}

#[test]
fn the_repository_can_be_named() {
    let dir = repository();
    let elsewhere = Dir::new();
    let path = dir.path().to_str().unwrap();
    let result = run(elsewhere.path(), &["op", "log", "-C", path]);
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
fn an_empty_log_is_reported_and_nothing_is_listed() {
    let dir = repository();
    fs::remove_file(dir.path().join(".stemma/oplog/000000000001")).unwrap();
    // A log from before the tip file; with the tip the loss would be noticed (next test).
    fs::remove_file(dir.path().join(".stemma/oplog-tip")).unwrap();
    for args in [&["op", "log"][..], &["op", "log", "--json"][..]] {
        let result = run(dir.path(), args);
        assert_eq!(result.code, 1);
        assert!(
            result
                .stderr
                .starts_with("stemma: the operation log is empty"),
            "{}",
            result.stderr
        );
        assert_eq!(result.stdout, "");
    }
}

#[test]
fn a_lost_last_entry_is_reported_by_the_tip_file_and_nothing_is_listed() {
    let dir = repository();
    fs::remove_file(dir.path().join(".stemma/oplog/000000000001")).unwrap();
    for args in [&["op", "log"][..], &["op", "log", "--json"][..]] {
        let result = run(dir.path(), args);
        assert_eq!(result.code, 1);
        assert!(
            result.stderr.starts_with(
                "stemma: the operation log ends at entry 0, but the tip file names entry 1"
            ),
            "{}",
            result.stderr
        );
        assert_eq!(result.stdout, "");
    }
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

#[test]
fn a_new_repository_has_no_marker_and_a_missing_working_copy_file_leaves_it_unknown() {
    let dir = repository();
    let result = run(dir.path(), &["op", "log"]);
    assert!(
        !result.stdout.contains("! ") && !result.stdout.contains("? "),
        "{}",
        result.stdout
    );
    // The entry of `init` moved the working-copy commit. Without the file the log cannot say
    // whether the move took effect, so the entry is unknown and a warning says why; it is not
    // marked as a move that did not take effect.
    fs::remove_file(dir.path().join(".stemma/working-copy")).unwrap();
    let result = run(dir.path(), &["op", "log"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(
        result.stderr.contains("no working-copy commit"),
        "{}",
        result.stderr
    );
    assert!(
        !result.stdout.contains("! the move did not take effect"),
        "{}",
        result.stdout
    );
    assert!(
        result
            .stdout
            .contains("    ? it is not known whether the move took effect\n"),
        "{}",
        result.stdout
    );
}

#[test]
fn a_stub_and_a_purge_are_shown_without_ids() {
    let dir = repository();
    let first = fs::read_to_string(dir.path().join(".stemma/working-copy"))
        .unwrap()
        .trim()
        .to_owned();
    snapshot(dir.path(), 2, &first, A);
    snapshot(dir.path(), 3, A, B);
    write_stub_entry(dir.path(), 2, "stub\n");
    write_stub_entry(dir.path(), 3, "stub\n");
    write_stub_entry(dir.path(), 4, "operation purge\ncommits 2\nstubs 2-3\n");

    let result = run(dir.path(), &["op", "log", "-n", "3"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(
        result.stdout,
        "4  2023-11-14T22:13:20Z  human daniel  purge\n    commits 2  stubs 2-3\n\
         3  2023-11-14T22:13:20Z  human daniel  (purged)\n\
         2  2023-11-14T22:13:20Z  human daniel  (purged)\n"
    );
    let result = run(dir.path(), &["op", "log", "-n", "3", "--json"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    let lines: Vec<&str> = result.stdout.lines().collect();
    assert_eq!(
        lines[0],
        "{\"number\":4,\"kind\":\"purge\",\"time\":1700000000,\"actor\":\"human daniel\",\
         \"operation\":\"purge\",\"commits\":2,\"stubs\":[[2,3]]}"
    );
    assert_eq!(
        lines[1],
        "{\"number\":3,\"kind\":\"stub\",\"time\":1700000000,\"actor\":\"human daniel\"}"
    );
}

#[test]
fn a_purge_with_several_ranges_lists_each() {
    let dir = repository();
    let first = fs::read_to_string(dir.path().join(".stemma/working-copy"))
        .unwrap()
        .trim()
        .to_owned();
    snapshot(dir.path(), 2, &first, A);
    snapshot(dir.path(), 3, A, B);
    snapshot(dir.path(), 4, B, A);
    snapshot(dir.path(), 5, A, B);
    for number in [2, 3, 5] {
        write_stub_entry(dir.path(), number, "stub\n");
    }
    write_stub_entry(dir.path(), 6, "operation purge\ncommits 3\nstubs 2-3 5\n");

    let result = run(dir.path(), &["op", "log", "-n", "1"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(
        result.stdout,
        "6  2023-11-14T22:13:20Z  human daniel  purge\n    commits 3  stubs 2-3, 5\n"
    );
    let result = run(dir.path(), &["op", "log", "-n", "1", "--json"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(
        result.stdout,
        "{\"number\":6,\"kind\":\"purge\",\"time\":1700000000,\"actor\":\"human daniel\",\
         \"operation\":\"purge\",\"commits\":3,\"stubs\":[[2,3],[5,5]]}\n"
    );
}

#[test]
fn a_stub_that_no_purge_lists_is_reported() {
    let dir = repository();
    let first = fs::read_to_string(dir.path().join(".stemma/working-copy"))
        .unwrap()
        .trim()
        .to_owned();
    snapshot(dir.path(), 2, &first, A);
    write_stub_entry(dir.path(), 2, "stub\n");
    let result = run(dir.path(), &["op", "log"]);
    assert_eq!(result.code, 1);
    assert!(
        result
            .stderr
            .contains("entry 2 is a stub, but no purge entry lists it"),
        "{}",
        result.stderr
    );
}

#[test]
fn a_window_that_leaves_out_the_stubs_shows_the_purge_alone_and_a_pending_entry_is_marked() {
    let dir = repository();
    let first = fs::read_to_string(dir.path().join(".stemma/working-copy"))
        .unwrap()
        .trim()
        .to_owned();
    snapshot(dir.path(), 2, &first, A);
    snapshot(dir.path(), 3, A, B);
    write_stub_entry(dir.path(), 2, "stub\n");
    write_stub_entry(dir.path(), 4, "operation purge\ncommits 2\nstubs 2-3\n");

    let result = run(dir.path(), &["op", "log", "-n", "1"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(result.stdout.starts_with("4  "), "{}", result.stdout);

    // Entry 3 is listed but not a stub yet: the purge stopped before it.
    let result = run(dir.path(), &["op", "log", "-n", "2"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(
        result
            .stdout
            .ends_with("    ! a purge lists this entry and has not finished\n"),
        "{}",
        result.stdout
    );
    let result = run(dir.path(), &["op", "log", "-n", "2", "--json"]);
    assert!(
        result
            .stdout
            .lines()
            .nth(1)
            .unwrap()
            .contains("\"purge_pending\":true"),
        "{}",
        result.stdout
    );
}

#[test]
fn every_entry_before_a_stub_is_unknown_in_a_hash_chained_log() {
    // Entry 3 became a stub, so every entry before it is unknown (ADR-014): entry 2 although
    // entry 5, which starts from A, would confirm it, and entry 1 of `init` although HEAD and the
    // working-copy commit would.
    // Entry 3 moved `main` to C and entry 5 starts from A; what happened between them does not
    // matter here. Entry 5, after the purge, keeps its own outcome.
    let dir = repository();
    branch_move(dir.path(), 2, "-", A);
    branch_move(dir.path(), 3, A, C);
    write_stub_entry(dir.path(), 3, "stub\n");
    write_stub_entry(dir.path(), 4, "operation purge\ncommits 1\nstubs 3\n");
    branch_move(dir.path(), 5, A, B);
    set_branch(dir.path(), B);

    let result = run(dir.path(), &["op", "log", "--json"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    let lines: Vec<&str> = result.stdout.lines().collect();
    assert!(lines[0].starts_with("{\"number\":5,"), "{}", result.stdout);
    assert!(
        lines[0].ends_with("\"took_effect\":true}"),
        "{}",
        result.stdout
    );
    for (line, number) in [(3, 2), (4, 1)] {
        assert!(
            lines[line].starts_with(&format!("{{\"number\":{number},")),
            "{}",
            result.stdout
        );
        assert!(
            lines[line].ends_with("\"took_effect\":null}"),
            "{}",
            result.stdout
        );
    }
}
