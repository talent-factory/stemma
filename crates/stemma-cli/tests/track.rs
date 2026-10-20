//! The command `stemma track`, run as a process, as a person or an agent would run it.

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
            "stemma-cli-track-{}-{}",
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

fn track(dir: &Path, paths: &[&str]) -> Run {
    let mut args = vec!["track", "--actor", "human alice"];
    args.extend_from_slice(paths);
    run(dir, &args)
}

/// The lines of `op log` that name an operation, newest first.
fn operations(dir: &Path) -> Vec<String> {
    let result = run(dir, &["op", "log"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    result
        .stdout
        .lines()
        .filter(|line| line.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .map(str::to_owned)
        .collect()
}

#[test]
fn a_file_is_tracked_and_the_command_lists_it() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    write(dir.path(), "other.txt", "untracked");
    let result = track(dir.path(), &["a.txt"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(result.stdout, "tracked a.txt\n");
    assert_eq!(result.stderr, "");
    let log = operations(dir.path());
    assert_eq!(log.len(), 2, "{log:?}");
    assert!(
        log[0].contains("human alice") && log[0].contains("track"),
        "{log:?}"
    );
}

#[test]
fn a_file_that_is_tracked_already_changes_nothing_and_the_command_says_so() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    assert_eq!(track(dir.path(), &["a.txt"]).code, 0);
    let result = track(dir.path(), &["a.txt"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(result.stdout, "No new files to track\n");
    assert_eq!(operations(dir.path()).len(), 2);
}

#[test]
fn a_dot_tracks_the_whole_directory_and_a_path_may_start_with_a_dot() {
    let dir = repository();
    write(dir.path(), "a.txt", "a");
    write(dir.path(), "d/b.txt", "b");
    let result = track(dir.path(), &["."]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(result.stdout, "tracked a.txt\ntracked d/b.txt\n");
    write(dir.path(), "d/c.txt", "c");
    write(dir.path(), "e.txt", "e");
    let result = track(dir.path(), &["./d/", "./e.txt"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(result.stdout, "tracked d/c.txt\ntracked e.txt\n");
}

#[test]
fn the_repository_can_be_named_and_the_paths_are_relative_to_it() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    let elsewhere = Dir::new();
    let result = run(
        elsewhere.path(),
        &[
            "track",
            "-C",
            dir.path().to_str().unwrap(),
            "--actor",
            "human alice",
            "a.txt",
        ],
    );
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(result.stdout, "tracked a.txt\n");
}

#[test]
fn a_change_to_a_tracked_file_is_saved_by_the_next_track() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    assert_eq!(track(dir.path(), &["a.txt"]).code, 0);
    write(dir.path(), "a.txt", "changed");
    write(dir.path(), "b.txt", "two");
    assert_eq!(track(dir.path(), &["b.txt"]).code, 0);
    // The change to `a.txt` was saved with it, so a track of nothing new finds no change.
    let before = operations(dir.path()).len();
    assert_eq!(
        track(dir.path(), &["a.txt"]).stdout,
        "No new files to track\n"
    );
    assert_eq!(operations(dir.path()).len(), before);
}

#[test]
fn a_path_that_is_wrong_fails_with_code_1_and_changes_nothing() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    for (path, message) in [
        ("missing.txt", "nothing at `missing.txt`"),
        ("/etc/passwd", "cannot be tracked"),
        ("../a.txt", "cannot be tracked"),
        (".stemma/format", "cannot be tracked"),
    ] {
        let result = track(dir.path(), &["a.txt", path]);
        assert_eq!(result.code, 1, "{path}: {}", result.stderr);
        assert_eq!(result.stdout, "", "{path}");
        assert!(
            result.stderr.starts_with("stemma: ") && result.stderr.contains(message),
            "{path}: {}",
            result.stderr
        );
    }
    assert_eq!(operations(dir.path()).len(), 1);
}

#[test]
fn no_path_is_a_usage_error() {
    let dir = repository();
    let result = run(dir.path(), &["track", "--actor", "human alice"]);
    assert_eq!(result.code, 2, "{}", result.stderr);
    assert_eq!(result.stdout, "");
}

#[test]
fn the_actor_is_needed_and_comes_from_the_flag_or_the_variable() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    write(dir.path(), "b.txt", "two");
    let result = run(dir.path(), &["track", "a.txt"]);
    assert_eq!(result.code, 1);
    assert!(result.stderr.contains("no actor"), "{}", result.stderr);
    assert_eq!(operations(dir.path()).len(), 1);
    let result = stemma(dir.path(), &["track", "a.txt"], |command| {
        command.env("STEMMA_ACTOR", "human bob");
    });
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(operations(dir.path())[0].contains("human bob"));
    let result = stemma(
        dir.path(),
        &["track", "--actor", "human alice", "b.txt"],
        |command| {
            command.env("STEMMA_ACTOR", "human bob");
        },
    );
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(operations(dir.path())[0].contains("human alice"));
}

#[test]
fn a_directory_without_a_repository_fails_with_code_1() {
    let dir = Dir::new();
    write(dir.path(), "a.txt", "one");
    let result = track(dir.path(), &["a.txt"]);
    assert_eq!(result.code, 1, "{}", result.stderr);
    assert!(result.stderr.starts_with("stemma: "), "{}", result.stderr);
}

#[cfg(unix)]
#[test]
fn a_special_file_stops_the_command_and_nothing_is_tracked() {
    use std::os::unix::net::UnixListener;
    let dir = repository();
    write(dir.path(), "d/a.txt", "one");
    let _socket = UnixListener::bind(dir.path().join("d/sock")).unwrap();
    let result = track(dir.path(), &["d"]);
    assert_eq!(result.code, 1, "{}", result.stderr);
    assert!(result.stderr.contains("d/sock"), "{}", result.stderr);
    assert_eq!(operations(dir.path()).len(), 1);
}

#[cfg(unix)]
#[test]
fn a_result_that_cannot_be_written_ends_with_code_3_after_the_files_are_tracked() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    let result = stemma(
        dir.path(),
        &["track", "--actor", "human alice", "a.txt"],
        |command| {
            // A full device, where there is one, makes the write fail.
            if Path::new("/dev/full").exists() {
                command.stdout(
                    fs::OpenOptions::new()
                        .write(true)
                        .open("/dev/full")
                        .unwrap(),
                );
            }
        },
    );
    if !Path::new("/dev/full").exists() {
        eprintln!("skipped: there is no /dev/full here");
        return;
    }
    assert_eq!(result.code, 3, "{}", result.stderr);
    assert!(
        result
            .stderr
            .contains("tracked the files, but could not write the result"),
        "{}",
        result.stderr
    );
    assert_eq!(operations(dir.path()).len(), 2);
}
