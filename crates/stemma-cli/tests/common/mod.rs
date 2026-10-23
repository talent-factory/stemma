//! Helpers that the process tests of the command line share: a temp directory, a run of the
//! program, a repository to run it in, and readers and writers of the operation log that it
//! writes.
//!
//! Every test file compiles this module into its own test binary and uses only a part of it. The
//! warning for the helpers that one binary leaves unused is therefore allowed; it also hides a
//! helper that no test uses any more, so remove such a helper by hand.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use stemma_core::oplog::Entry;

/// The actor that the tests run the commands as, unless a test names another.
pub const ACTOR: &str = "human daniel";

/// A directory below the system temp directory that is removed on drop. Its name holds three
/// parts, so that no two directories clash: the name of the test binary, which tells the
/// directories of the test files apart; the process, since several binaries and several runs of
/// one binary run concurrently; and a counter, since the tests of one binary run in parallel
/// threads.
pub struct Dir(PathBuf);

impl Dir {
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "stemma-cli-{}-{}-{}",
            env!("CARGO_CRATE_NAME"),
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// What a process did.
pub struct Run {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Runs `stemma` in `dir` without `STEMMA_ACTOR` in the environment; `extra` can adjust the
/// command before it runs, for example to set the actor through the environment.
pub fn stemma(dir: &Path, args: &[&str], extra: impl FnOnce(&mut Command)) -> Run {
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

/// Runs `stemma` in `dir` without `STEMMA_ACTOR`.
pub fn run(dir: &Path, args: &[&str]) -> Run {
    stemma(dir, args, |_| {})
}

/// Runs `stemma` like [`run`] and asserts that it succeeds.
pub fn ok(dir: &Path, args: &[&str]) -> Run {
    let result = run(dir, args);
    assert_eq!(result.code, 0, "{args:?}: {}", result.stderr);
    result
}

/// A repository made by `init`, with the actor [`ACTOR`].
pub fn repository() -> Dir {
    let dir = Dir::new();
    let result = run(dir.path(), &["init", "--actor", ACTOR]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    dir
}

/// Writes `content` to `path` below `dir`, with the directories above it.
pub fn write(dir: &Path, path: &str, content: &str) {
    let full = dir.join(path);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    fs::write(full, content).unwrap();
}

/// Tracks `path` with the actor [`ACTOR`] and asserts that it succeeds.
pub fn track_ok(dir: &Path, path: &str) {
    let result = run(dir, &["track", "--actor", ACTOR, path]);
    assert_eq!(result.code, 0, "{}", result.stderr);
}

/// The entries of `op log`, newest first, one JSON object each.
pub fn operations(dir: &Path) -> Vec<String> {
    let result = run(dir, &["op", "log", "--json"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    result.stdout.lines().map(str::to_owned).collect()
}

/// The file of entry `number` of the log.
pub fn entry_file(dir: &Path, number: u64) -> PathBuf {
    dir.join(".stemma/oplog").join(format!("{number:012}"))
}

/// The `previous` line that entry `number` of the log names: the hash of the entry before it, or
/// `-` at the start of the chain (ADR-011).
pub fn previous(dir: &Path, number: u64) -> String {
    if number == 1 {
        return "-".to_owned();
    }
    // An entry that is missing (a test of a gap) starts the chain again.
    fs::read(entry_file(dir, number - 1))
        .ok()
        .and_then(|bytes| Entry::parse(&bytes).ok())
        .and_then(|entry| entry.hash())
        .map_or_else(|| "-".to_owned(), |hash| hash.to_string())
}

/// The stub of entry `number`, as a purge writes it: the header lines of the entry, up to its line
/// `previous`, and the hash of its body.
pub fn stub_text(dir: &Path, number: u64) -> String {
    let text = fs::read_to_string(entry_file(dir, number)).unwrap();
    let entry = Entry::parse(text.as_bytes()).unwrap();
    let head: String = text.split_inclusive('\n').take(5).collect();
    format!("{head}stub {}\n", entry.body_hash().unwrap())
}

/// Writes a stub or a `purge` entry file as a purge would, in the hash chain (ADR-014). A body of
/// `stub` alone becomes the stub of the entry with the number.
pub fn write_stub_entry(dir: &Path, number: u64, body: &str) {
    let text = if body == "stub\n" {
        stub_text(dir, number)
    } else {
        format!(
            "stemma-oplog 3\nnumber {number}\ntime 1700000000\nactor {ACTOR}\n\
             previous {}\n{body}",
            previous(dir, number)
        )
    };
    fs::write(entry_file(dir, number), text).unwrap();
}
