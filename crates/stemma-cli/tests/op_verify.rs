//! The command `stemma op verify`, run as a process, as a person or an agent would run it.

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

const ACTOR: &str = "human daniel";

/// A repository made by `init`, with two files tracked and committed, so that its log holds
/// several entries.
fn repository() -> Dir {
    let dir = Dir::new();
    for args in [
        &["init", "--actor", ACTOR][..],
        &["track", ".", "--actor", ACTOR][..],
    ] {
        let result = run(dir.path(), args);
        assert_eq!(result.code, 0, "{}", result.stderr);
    }
    fs::write(dir.path().join("a.txt"), "one").unwrap();
    for args in [
        &["track", "a.txt", "--actor", ACTOR][..],
        &["commit", "-m", "first", "--actor", ACTOR][..],
    ] {
        let result = run(dir.path(), args);
        assert_eq!(result.code, 0, "{}", result.stderr);
    }
    dir
}

fn entry_file(dir: &Path, number: u64) -> PathBuf {
    dir.join(".stemma/oplog").join(format!("{number:012}"))
}

fn last(dir: &Path) -> u64 {
    fs::read_dir(dir.join(".stemma/oplog")).unwrap().count() as u64
}

/// The anchor that `op verify` prints on its last line.
fn anchor(dir: &Path) -> String {
    let result = run(dir, &["op", "verify"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    let line = result.stdout.lines().last().unwrap();
    line.strip_prefix("anchor ").unwrap().to_owned()
}

#[test]
fn a_log_written_by_stemma_holds_and_the_last_entry_is_its_anchor() {
    let dir = repository();
    let result = run(dir.path(), &["op", "verify"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(result.stderr, "");
    let last = last(dir.path());
    let lines: Vec<&str> = result.stdout.lines().collect();
    assert_eq!(
        lines[0],
        format!("Checked {last} entries of the operation log: the hash chain holds.")
    );
    assert!(
        lines[1].starts_with(&format!("anchor {last}:b3:")),
        "{}",
        result.stdout
    );
    assert_eq!(lines.len(), 2);
}

#[test]
fn an_anchor_that_still_holds_is_confirmed() {
    let dir = repository();
    let kept = anchor(dir.path());
    // Later entries do not change the hash of the anchor.
    fs::write(dir.path().join("a.txt"), "two").unwrap();
    let result = run(dir.path(), &["commit", "-m", "second", "--actor", ACTOR]);
    assert_eq!(result.code, 0, "{}", result.stderr);

    let result = run(dir.path(), &["op", "verify", "--anchor", &kept]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    let number = kept.split(':').next().unwrap();
    assert!(
        result.stdout.contains(&format!(
            "Entry {number} still has the hash of the anchor.\n"
        )),
        "{}",
        result.stdout
    );
}

#[test]
fn a_changed_entry_breaks_the_chain_and_is_named() {
    let dir = repository();
    let text = fs::read_to_string(entry_file(dir.path(), 2)).unwrap();
    let time = text.lines().nth(2).unwrap();
    let changed = text.replacen(time, "time 1", 1);
    fs::write(entry_file(dir.path(), 2), changed).unwrap();

    let result = run(dir.path(), &["op", "verify"]);
    assert_eq!(result.code, 1);
    assert_eq!(result.stdout, "");
    assert!(
        result
            .stderr
            .starts_with("stemma: entry 3 does not name the hash of the entry before it"),
        "{}",
        result.stderr
    );
}

#[test]
fn a_log_whose_chain_was_computed_again_is_found_only_by_the_anchor() {
    let dir = repository();
    let kept = anchor(dir.path());
    // Rewrite the log as an attacker who can write `.stemma` would: change the last entry and
    // write the tip again. The chain holds, the anchor does not.
    let number = kept.split(':').next().unwrap().parse::<u64>().unwrap();
    let text = fs::read_to_string(entry_file(dir.path(), number)).unwrap();
    let time = text.lines().nth(2).unwrap();
    fs::write(
        entry_file(dir.path(), number),
        text.replacen(time, "time 1", 1),
    )
    .unwrap();
    fs::remove_file(dir.path().join(".stemma/oplog-tip")).unwrap();
    let result = run(dir.path(), &["op", "verify"]);
    assert_eq!(result.code, 0, "{}", result.stderr);

    let result = run(dir.path(), &["op", "verify", "--anchor", &kept]);
    assert_eq!(result.code, 1);
    assert!(
        result
            .stderr
            .contains(&format!("entry {number} has the hash"))
            && result.stderr.contains("not the hash"),
        "{}",
        result.stderr
    );
}

#[test]
fn an_anchor_beyond_the_log_or_in_the_wrong_form_is_refused() {
    let dir = repository();
    let kept = anchor(dir.path());
    let hash = kept.split_once(':').unwrap().1;
    let result = run(
        dir.path(),
        &["op", "verify", "--anchor", &format!("99:{hash}")],
    );
    assert_eq!(result.code, 1);
    assert!(
        result
            .stderr
            .contains("names entry 99, but the log has no such entry"),
        "{}",
        result.stderr
    );
    for bad in [
        "",
        "3",
        "03:b3:00",
        "0:b3:11",
        "x:b3:11",
        &format!("3:{}", hash.to_uppercase()),
    ] {
        let result = run(dir.path(), &["op", "verify", "--anchor", bad]);
        assert_eq!(result.code, 2, "{bad:?}: {}", result.stderr);
    }
}

#[test]
fn verify_outside_a_repository_fails() {
    let dir = Dir::new();
    let result = run(dir.path(), &["op", "verify"]);
    assert_eq!(result.code, 1);
    assert!(result.stderr.starts_with("stemma: "), "{}", result.stderr);
}
