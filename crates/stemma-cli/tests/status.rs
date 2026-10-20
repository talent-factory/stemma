//! The command `stemma status`, run as a process, as a person or an agent would run it.

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
            "stemma-cli-status-{}-{}",
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

fn commit(dir: &Path, message: &str) {
    let result = run(dir, &["commit", "-m", message, "--actor", "human daniel"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
}

fn status(dir: &Path) -> Run {
    run(dir, &["status", "--actor", "human alice"])
}

/// The entries of `op log`, newest first, one JSON object each.
fn operations(dir: &Path) -> Vec<String> {
    let result = run(dir, &["op", "log", "--json"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    result.stdout.lines().map(str::to_owned).collect()
}

#[test]
fn a_new_repository_is_clean_and_status_writes_no_entry() {
    let dir = repository();
    let entries = operations(dir.path()).len();

    let result = status(dir.path());

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(result.stdout, "Working copy is clean\n");
    assert_eq!(result.stderr, "");
    assert_eq!(operations(dir.path()).len(), entries);
}

#[test]
fn tracked_files_before_the_first_commit_are_added() {
    let dir = repository();
    write(dir.path(), "b.txt", "b");
    write(dir.path(), "d/a.txt", "a");
    track(dir.path(), "b.txt");
    track(dir.path(), "d");

    let result = status(dir.path());

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(result.stdout, "Working copy changes:\nA b.txt\nA d/a.txt\n");
}

#[test]
fn changes_and_untracked_files_are_listed_in_two_sorted_sections() {
    let dir = repository();
    write(dir.path(), "keep.txt", "one");
    write(dir.path(), "gone.txt", "gone");
    write(dir.path(), "d/inner.txt", "i");
    track(dir.path(), ".");
    commit(dir.path(), "base");
    write(dir.path(), "keep.txt", "two");
    fs::remove_file(dir.path().join("gone.txt")).unwrap();
    write(dir.path(), "new.txt", "new");
    track(dir.path(), "new.txt");
    write(dir.path(), "z.txt", "z");
    write(dir.path(), "notes/one.md", "1");

    let result = status(dir.path());

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(
        result.stdout,
        "Working copy changes:\nD gone.txt\nM keep.txt\nA new.txt\n\
         Untracked files:\n? notes/one.md\n? z.txt\n"
    );
}

#[test]
fn after_a_commit_the_working_copy_is_clean_and_only_untracked_files_remain() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    track(dir.path(), "a.txt");
    commit(dir.path(), "base");
    assert_eq!(status(dir.path()).stdout, "Working copy is clean\n");
    write(dir.path(), "other.txt", "o");

    let result = status(dir.path());

    assert_eq!(result.stdout, "Untracked files:\n? other.txt\n");
}

#[test]
fn what_the_exclusion_file_names_is_not_listed_and_the_file_itself_is() {
    let dir = repository();
    write(dir.path(), ".stemmaignore", "*.log\ntarget/\n");
    write(dir.path(), "a.txt", "a");
    write(dir.path(), "d/x.log", "log");
    write(dir.path(), "target/debug/z", "z");

    let result = status(dir.path());

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(
        result.stdout,
        "Untracked files:\n? .stemmaignore\n? a.txt\n"
    );
}

#[test]
fn a_name_is_written_escaped() {
    let dir = repository();
    write(dir.path(), "it's.txt", "x");

    let result = status(dir.path());

    assert_eq!(result.stdout, "Untracked files:\n? it\\'s.txt\n");
}

#[test]
fn a_change_is_saved_by_the_actor_of_the_call_and_only_once() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    track(dir.path(), "a.txt");
    write(dir.path(), "a.txt", "two");
    let entries = operations(dir.path()).len();

    let first = status(dir.path());
    let second = status(dir.path());

    assert_eq!((first.code, second.code), (0, 0), "{}", first.stderr);
    assert_eq!(first.stdout, second.stdout);
    let log = operations(dir.path());
    assert_eq!(log.len(), entries + 1, "{log:?}");
    assert!(
        log[0].contains("\"operation\":\"snapshot\"")
            && log[0].contains("\"actor\":\"human alice\""),
        "{log:?}"
    );
}

#[test]
fn the_directory_flag_and_the_actor_variable_are_used_and_the_flag_wins() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    track(dir.path(), "a.txt");
    write(dir.path(), "a.txt", "two");
    let elsewhere = Dir::new();

    let result = stemma(
        elsewhere.path(),
        &["status", "-C", dir.path().to_str().unwrap()],
        |command| {
            command.env("STEMMA_ACTOR", "human bob");
        },
    );
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(operations(dir.path())[0].contains("\"actor\":\"human bob\""));

    write(dir.path(), "a.txt", "three");
    let result = stemma(
        dir.path(),
        &["status", "--actor", "human carol"],
        |command| {
            command.env("STEMMA_ACTOR", "human bob");
        },
    );
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(operations(dir.path())[0].contains("\"actor\":\"human carol\""));
}

#[test]
fn without_an_actor_the_command_fails_and_saves_nothing() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    track(dir.path(), "a.txt");
    write(dir.path(), "a.txt", "two");
    let entries = operations(dir.path()).len();

    let result = run(dir.path(), &["status"]);

    assert_eq!(result.code, 1);
    assert_eq!(result.stdout, "");
    assert!(
        result.stderr.starts_with("stemma: no actor"),
        "{}",
        result.stderr
    );
    assert_eq!(operations(dir.path()).len(), entries);
}

#[test]
fn in_a_directory_without_a_repository_the_command_fails_and_names_the_flag() {
    let elsewhere = Dir::new();

    let result = status(elsewhere.path());

    assert_eq!(result.code, 1);
    assert!(
        result.stderr.contains("not in its parents") && result.stderr.contains("`status`"),
        "{}",
        result.stderr
    );
}

#[test]
fn an_argument_that_status_does_not_take_is_a_usage_error() {
    let dir = repository();

    let result = run(dir.path(), &["status", "--actor", "human alice", "a.txt"]);

    assert_eq!(result.code, 2, "{}", result.stderr);
    assert_eq!(result.stdout, "");
}

#[test]
fn an_invalid_exclusion_file_stops_the_command_before_anything_is_saved() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    track(dir.path(), "a.txt");
    write(dir.path(), "a.txt", "two");
    write(dir.path(), ".stemmaignore", "*.log\n!keep.log\n");
    let entries = operations(dir.path()).len();

    let result = status(dir.path());

    assert_eq!(result.code, 1, "{}", result.stdout);
    assert_eq!(result.stdout, "");
    assert!(
        result
            .stderr
            .starts_with("stemma: .stemmaignore: line 2: negation")
            && result.stderr.ends_with("; nothing was written\n"),
        "{}",
        result.stderr
    );
    assert_eq!(operations(dir.path()).len(), entries);
}

#[test]
fn a_name_that_a_tree_refuses_stops_the_command_unless_a_pattern_excludes_it() {
    let dir = repository();
    write(dir.path(), "d/a\u{200B}b.txt", "x");

    let stopped = status(dir.path());

    assert_eq!(stopped.code, 1, "{}", stopped.stdout);
    assert!(
        stopped.stderr.ends_with("; nothing was written\n"),
        "{}",
        stopped.stderr
    );
    write(dir.path(), ".stemmaignore", "*.txt\n");
    let listed = status(dir.path());
    assert_eq!(listed.code, 0, "{}", listed.stderr);
    assert_eq!(listed.stdout, "Untracked files:\n? .stemmaignore\n");
}

/// `/dev/full` fails every write: a status that cannot be written is code 1.
#[cfg(target_os = "linux")]
#[test]
fn a_status_that_cannot_be_written_ends_with_code_1() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    let full = fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_stemma"))
        .current_dir(dir.path())
        .env_remove("STEMMA_ACTOR")
        .args(["status", "--actor", "human alice"])
        .stdout(full)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with("stemma: could not write the status"),
        "{stderr}"
    );
}

/// A snapshot whose pointer moves and whose directory cannot be flushed: the status is written
/// first, then the error, and the code is 3.
#[cfg(unix)]
#[test]
fn a_snapshot_that_cannot_be_flushed_gives_the_status_and_code_3() {
    use std::os::unix::fs::PermissionsExt;
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    track(dir.path(), "a.txt");
    commit(dir.path(), "base");
    write(dir.path(), "a.txt", "two");
    let metadata = dir.path().join(".stemma");
    fs::set_permissions(&metadata, fs::Permissions::from_mode(0o300)).unwrap();
    let works = fs::read_dir(&metadata).is_ok();
    let result = status(dir.path());
    fs::set_permissions(&metadata, fs::Permissions::from_mode(0o755)).unwrap();
    if works {
        return;
    }

    assert_eq!(result.code, 3, "{}", result.stderr);
    assert_eq!(result.stdout, "Working copy changes:\nM a.txt\n");
    assert!(
        result
            .stderr
            .contains("the tracked files were saved, but flushing failed"),
        "{}",
        result.stderr
    );
    // The pointer moved: the next call finds nothing to save.
    let entries = operations(dir.path()).len();
    assert_eq!(
        status(dir.path()).stdout,
        "Working copy changes:\nM a.txt\n"
    );
    assert_eq!(operations(dir.path()).len(), entries);
}
