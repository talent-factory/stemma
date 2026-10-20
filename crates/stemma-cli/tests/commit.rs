//! The command `stemma commit`, run as a process, as a person or an agent would run it.

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
            "stemma-cli-commit-{}-{}",
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

/// A repository made by `init`, with the actor `human daniel`.
fn repository() -> Dir {
    let dir = Dir::new();
    let result = run(dir.path(), &["init", "--actor", "human daniel"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    dir
}

fn write(dir: &Path, path: &str, content: &str) {
    let full = dir.join(path);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    fs::write(full, content).unwrap();
}

fn track(dir: &Path, path: &str) {
    let result = run(dir, &["track", "--actor", "human daniel", path]);
    assert_eq!(result.code, 0, "{}", result.stderr);
}

fn commit(dir: &Path, message: &str) -> Run {
    run(dir, &["commit", "-m", message, "--actor", "human alice"])
}

/// The entries of `op log`, newest first, one JSON object each.
fn operations(dir: &Path) -> Vec<String> {
    let result = run(dir, &["op", "log", "--json"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    result.stdout.lines().map(str::to_owned).collect()
}

#[test]
fn a_commit_names_the_working_copy_and_says_which_commit_and_branch() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    track(dir.path(), "a.txt");

    let result = commit(dir.path(), "Add a");

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(
        result.stdout.starts_with("Committed b3:") && result.stdout.ends_with(" on main\n"),
        "{}",
        result.stdout
    );
    assert_eq!(result.stderr, "");
    let log = operations(dir.path());
    assert!(
        log[0].contains("\"actor\":\"human alice\"") && log[0].contains("\"operation\":\"commit\""),
        "{log:?}"
    );
    assert!(!log[0].contains("\"took_effect\":false"), "{log:?}");
}

#[test]
fn a_change_since_the_last_command_is_committed_and_two_commits_differ() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    track(dir.path(), "a.txt");
    let first = commit(dir.path(), "one");
    write(dir.path(), "a.txt", "two");

    let second = commit(dir.path(), "two");

    assert_eq!((first.code, second.code), (0, 0), "{}", second.stderr);
    assert_ne!(first.stdout, second.stdout);
    let log = operations(dir.path());
    // init, track, commit, then the snapshot and the commit.
    assert_eq!(log.len(), 5, "{log:?}");
    assert!(log[1].contains("\"operation\":\"snapshot\""), "{log:?}");
}

#[test]
fn an_empty_message_nothing_to_commit_and_no_message_fail_without_a_trace() {
    let dir = repository();
    let entries = operations(dir.path()).len();

    for message in ["", "   "] {
        let result = commit(dir.path(), message);
        assert_eq!(result.code, 1, "{}", result.stdout);
        assert!(
            result.stderr.contains("must not be empty"),
            "{}",
            result.stderr
        );
    }
    let nothing = commit(dir.path(), "nothing");
    assert_eq!(nothing.code, 1);
    assert!(
        nothing.stderr.contains("nothing to commit"),
        "{}",
        nothing.stderr
    );
    assert_eq!(nothing.stdout, "");
    assert_eq!(operations(dir.path()).len(), entries);

    let usage = run(dir.path(), &["commit", "--actor", "human alice"]);
    assert_eq!(usage.code, 2, "{}", usage.stderr);
}

#[test]
fn without_an_actor_or_a_repository_the_command_fails_with_code_1() {
    let dir = repository();
    let result = run(dir.path(), &["commit", "-m", "x"]);
    assert_eq!(result.code, 1);
    assert!(result.stderr.contains("no actor"), "{}", result.stderr);
    let elsewhere = Dir::new();
    let result = commit(elsewhere.path(), "x");
    assert_eq!(result.code, 1);
    assert!(
        result.stderr.contains("not in its parents"),
        "{}",
        result.stderr
    );
}

#[test]
fn the_directory_and_the_actor_come_from_the_flag_and_the_environment() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    track(dir.path(), "a.txt");
    let elsewhere = Dir::new();

    let result = stemma(
        elsewhere.path(),
        &["commit", "-C", dir.path().to_str().unwrap(), "-m", "Add a"],
        |command| {
            command.env("STEMMA_ACTOR", "human bob");
        },
    );

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(operations(dir.path())[0].contains("\"actor\":\"human bob\""));
}

#[test]
fn a_branch_that_moved_is_refused_and_the_log_marks_nothing() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    track(dir.path(), "a.txt");
    assert_eq!(commit(dir.path(), "one").code, 0);
    // Someone points main at the working-copy commit.
    let heads = dir.path().join(".stemma/refs/heads/main");
    fs::copy(dir.path().join(".stemma/working-copy"), &heads).unwrap();
    write(dir.path(), "a.txt", "two");

    let result = commit(dir.path(), "two");

    assert_eq!(result.code, 1, "{}", result.stdout);
    assert!(result.stderr.contains("branch `main`"), "{}", result.stderr);
}

#[test]
fn a_commit_that_stopped_between_its_moves_is_finished_by_the_next_one() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    track(dir.path(), "a.txt");
    assert_eq!(commit(dir.path(), "one").code, 0);
    let metadata = dir.path().join(".stemma");
    let before = fs::read(metadata.join("working-copy")).unwrap();
    write(dir.path(), "a.txt", "two");
    let second = commit(dir.path(), "two");
    assert_eq!(second.code, 0, "{}", second.stderr);
    // The state of a stop after the move of the branch: the working-copy pointer is the old one.
    fs::write(metadata.join("working-copy"), before).unwrap();
    let id = second.stdout.split(' ').nth(1).unwrap().to_owned();

    let result = commit(dir.path(), "ignored");

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(
        result.stdout,
        format!(
            "Finished the commit {id} on main that had stopped; the message of this call was not \
             used\n"
        )
    );
    // The repository works again.
    write(dir.path(), "a.txt", "three");
    assert_eq!(commit(dir.path(), "three").code, 0);
}

/// `/dev/full` fails every write: the commit is made, and the command says so with code 3.
#[cfg(target_os = "linux")]
#[test]
fn a_result_that_cannot_be_written_ends_with_code_3_after_the_commit() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    track(dir.path(), "a.txt");
    let full = fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_stemma"))
        .current_dir(dir.path())
        .env_remove("STEMMA_ACTOR")
        .args(["commit", "-m", "Add a", "--actor", "human alice"])
        .stdout(full)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("committed, but"));
    assert!(operations(dir.path())[0].contains("\"operation\":\"commit\""));
}
