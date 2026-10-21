//! The command `stemma op restore`, run as a process, as a person or an agent would run it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use stemma_core::repository::Repository;

/// A directory below the system temp directory that is removed on drop.
struct Dir(PathBuf);

impl Dir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "stemma-cli-restore-{}-{}",
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

/// Runs `stemma` in `dir` without `STEMMA_ACTOR`.
fn run(dir: &Path, args: &[&str]) -> Run {
    let output = Command::new(env!("CARGO_BIN_EXE_stemma"))
        .current_dir(dir)
        .env_remove("STEMMA_ACTOR")
        .args(args)
        .output()
        .unwrap();
    Run {
        code: output.status.code().expect("the process ended by a signal"),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

const ACTOR: &str = "human daniel";

fn ok(dir: &Path, args: &[&str]) -> Run {
    let result = run(dir, args);
    assert_eq!(result.code, 0, "{args:?}: {}", result.stderr);
    result
}

fn write(dir: &Path, path: &str, content: &str) {
    fs::write(dir.join(path), content).unwrap();
}

fn read(dir: &Path, path: &str) -> String {
    fs::read_to_string(dir.join(path)).unwrap()
}

/// The numbers of the entries of the log.
fn numbers(dir: &Path) -> Vec<u64> {
    let repository = Repository::open(dir).unwrap();
    repository
        .oplog()
        .entries()
        .unwrap()
        .iter()
        .map(stemma_core::oplog::Entry::number)
        .collect()
}

/// A repository with `a.txt` committed as "one", then changed to "two" and committed. Returns the
/// number of the entry of the second snapshot, which moved the working copy.
fn two_commits() -> (Dir, u64) {
    let dir = Dir::new();
    ok(dir.path(), &["init", "--actor", ACTOR]);
    write(dir.path(), "a.txt", "one");
    ok(dir.path(), &["track", "a.txt", "--actor", ACTOR]);
    ok(dir.path(), &["commit", "-m", "first", "--actor", ACTOR]);
    write(dir.path(), "a.txt", "two");
    let before = numbers(dir.path()).len();
    ok(dir.path(), &["commit", "-m", "second", "--actor", ACTOR]);
    let snapshot = numbers(dir.path())[before];
    (dir, snapshot)
}

#[test]
fn the_old_state_comes_back_and_the_output_says_how_to_undo_it() {
    let (dir, snapshot) = two_commits();

    let result = ok(
        dir.path(),
        &["op", "restore", &snapshot.to_string(), "--actor", ACTOR],
    );

    assert_eq!(read(dir.path(), "a.txt"), "one");
    let restored = *numbers(dir.path()).last().unwrap();
    assert_eq!(
        result.stdout,
        format!(
            "Restored the state before entry {snapshot} as entry {restored}\n\
             To undo it: stemma op restore {restored}\n"
        )
    );
    assert_eq!(result.stderr, "");
    let undone = ok(
        dir.path(),
        &["op", "restore", &restored.to_string(), "--actor", ACTOR],
    );
    assert_eq!(read(dir.path(), "a.txt"), "two", "{}", undone.stdout);
}

#[test]
fn an_unsaved_change_is_saved_first_and_named() {
    let (dir, snapshot) = two_commits();
    write(dir.path(), "a.txt", "scratch");

    let result = ok(
        dir.path(),
        &["op", "restore", &snapshot.to_string(), "--actor", ACTOR],
    );

    let all = numbers(dir.path());
    let (saved, restored) = (all[all.len() - 2], all[all.len() - 1]);
    assert_eq!(
        result.stdout,
        format!(
            "Saved the working copy first as entry {saved}\n\
             Restored the state before entry {snapshot} as entry {restored}\n\
             To undo it: stemma op restore {restored}\n"
        )
    );
    assert_eq!(read(dir.path(), "a.txt"), "one");
}

#[test]
fn a_dry_run_shows_the_plan_and_writes_nothing() {
    let (dir, snapshot) = two_commits();
    write(dir.path(), "a.txt", "scratch");
    let before = numbers(dir.path());

    let result = ok(
        dir.path(),
        &[
            "op",
            "restore",
            &snapshot.to_string(),
            "--dry-run",
            "--actor",
            ACTOR,
        ],
    );

    assert!(
        result.stdout.starts_with(&format!(
            "Would restore the state before entry {snapshot}; nothing is written\n"
        )),
        "{}",
        result.stdout
    );
    assert!(
        result.stdout.contains("    working-copy  b3:"),
        "{}",
        result.stdout
    );
    assert!(
        result.stdout.contains("    write a.txt\n"),
        "{}",
        result.stdout
    );
    assert!(
        result
            .stdout
            .contains("A real run saves unsaved changes first"),
        "{}",
        result.stdout
    );
    assert_eq!(numbers(dir.path()), before, "not even a snapshot");
    assert_eq!(read(dir.path(), "a.txt"), "scratch");
}

#[test]
fn restoring_again_finds_nothing_to_move() {
    let (dir, snapshot) = two_commits();
    ok(
        dir.path(),
        &["op", "restore", &snapshot.to_string(), "--actor", ACTOR],
    );
    let before = numbers(dir.path());

    let result = ok(
        dir.path(),
        &["op", "restore", &snapshot.to_string(), "--actor", ACTOR],
    );

    assert_eq!(
        result.stdout,
        format!(
            "Nothing to restore: every pointer of entry {snapshot} has its old value already\n"
        )
    );
    assert_eq!(numbers(dir.path()), before);
    let dry = ok(
        dir.path(),
        &[
            "op",
            "restore",
            &snapshot.to_string(),
            "--dry-run",
            "--actor",
            ACTOR,
        ],
    );
    assert_eq!(dry.stdout, result.stdout);
}

#[test]
fn the_directory_comes_from_dash_c() {
    let (dir, snapshot) = two_commits();
    let elsewhere = Dir::new();
    let target = dir.path().to_str().unwrap();

    ok(
        elsewhere.path(),
        &[
            "op",
            "restore",
            &snapshot.to_string(),
            "-C",
            target,
            "--actor",
            ACTOR,
        ],
    );

    assert_eq!(read(dir.path(), "a.txt"), "one");
}

#[test]
fn an_actor_is_required() {
    let (dir, snapshot) = two_commits();
    let before = numbers(dir.path());

    let result = run(dir.path(), &["op", "restore", &snapshot.to_string()]);

    assert_eq!(result.code, 1);
    assert!(
        result.stderr.starts_with("stemma: no actor"),
        "{}",
        result.stderr
    );
    assert_eq!(numbers(dir.path()), before);
}

#[test]
fn an_entry_that_is_not_in_the_log_is_an_error() {
    let (dir, _) = two_commits();
    let before = numbers(dir.path());

    let result = run(dir.path(), &["op", "restore", "999", "--actor", ACTOR]);

    assert_eq!(result.code, 1);
    assert_eq!(
        result.stderr,
        "stemma: the operation log has no entry 999\n"
    );
    assert_eq!(numbers(dir.path()), before);
}

#[test]
fn the_number_must_be_a_number() {
    let (dir, _) = two_commits();

    let result = run(dir.path(), &["op", "restore", "last", "--actor", ACTOR]);

    assert_eq!(result.code, 2);
}

#[test]
fn the_first_entry_cannot_be_restored() {
    let (dir, _) = two_commits();

    let result = run(dir.path(), &["op", "restore", "1", "--actor", ACTOR]);

    assert_eq!(result.code, 1);
    assert!(
        result.stderr.contains("created HEAD") || result.stderr.contains("cannot be restored"),
        "{}",
        result.stderr
    );
}

#[test]
fn a_file_in_the_way_stops_the_restore_before_anything_is_written() {
    let dir = Dir::new();
    ok(dir.path(), &["init", "--actor", ACTOR]);
    write(dir.path(), "a.txt", "one");
    ok(dir.path(), &["track", "a.txt", "--actor", ACTOR]);
    ok(dir.path(), &["commit", "-m", "first", "--actor", ACTOR]);
    fs::remove_file(dir.path().join("a.txt")).unwrap();
    let before = numbers(dir.path()).len();
    ok(dir.path(), &["status", "--actor", ACTOR]);
    let dropped = numbers(dir.path())[before];
    fs::create_dir(dir.path().join("a.txt")).unwrap();
    write(dir.path(), "a.txt/mine", "mine");
    let entries = numbers(dir.path());

    let result = run(
        dir.path(),
        &["op", "restore", &dropped.to_string(), "--actor", ACTOR],
    );

    assert_eq!(result.code, 1);
    assert!(
        result.stderr.contains("nothing of it was written")
            && result.stderr.contains("is in the way"),
        "{}",
        result.stderr
    );
    assert_eq!(numbers(dir.path()), entries);
    assert_eq!(read(dir.path(), "a.txt/mine"), "mine");
}
