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

/// Runs `stemma` in `dir` without an actor in the environment, and `extra` can change that. A
/// command that reads a configuration or the home directory has to point those at `dir` here.
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
             \"branches\":[{{\"name\":\"main\",\"old\":null,\"new\":\"{A}\"}}]}}"
        )
    );
    assert!(
        lines[1].starts_with("{\"number\":1,")
            && lines[1].contains("\"head\":{\"old\":null,\"new\":\"branch:main\"}")
            && lines[1].ends_with("\"branches\":[]}"),
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
