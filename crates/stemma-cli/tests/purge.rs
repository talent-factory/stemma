//! The command `stemma purge`, run as a process, as a person or an agent would run it. Only the
//! plan is built so far (ADR-014): the command shows it and writes nothing.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use stemma_core::object::Blob;

/// A directory below the system temp directory that is removed on drop.
struct Dir(PathBuf);

impl Dir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "stemma-cli-purge-{}-{}",
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
const AGENT: &str = "agent bot model=m session=s sponsor=daniel";
const SECRET: &str = "key=hunter2";

fn ok(dir: &Path, args: &[&str]) -> Run {
    let result = run(dir, args);
    assert_eq!(result.code, 0, "{args:?}: {}", result.stderr);
    result
}

fn secret() -> String {
    Blob::new(SECRET).id().to_string()
}

/// Every file below `.stemma` with its content, to show that nothing was written.
fn files(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut found = Vec::new();
    let mut pending = vec![dir.join(".stemma")];
    while let Some(path) = pending.pop() {
        for entry in fs::read_dir(&path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry.path());
            } else {
                found.push((entry.path(), fs::read(entry.path()).unwrap()));
            }
        }
    }
    found.sort();
    found
}

/// A repository with `a.txt` committed, then the secret in `key` committed.
fn with_secret() -> Dir {
    let dir = Dir::new();
    ok(dir.path(), &["init", "--actor", ACTOR]);
    fs::write(dir.path().join("a.txt"), "one").unwrap();
    ok(dir.path(), &["track", "a.txt", "--actor", ACTOR]);
    ok(dir.path(), &["commit", "-m", "first", "--actor", ACTOR]);
    fs::write(dir.path().join("key"), SECRET).unwrap();
    ok(dir.path(), &["track", "key", "--actor", ACTOR]);
    ok(dir.path(), &["commit", "-m", "second", "--actor", ACTOR]);
    dir
}

#[test]
fn the_plan_names_the_rewrites_the_moves_the_stubs_and_the_deletions_and_writes_nothing() {
    let dir = with_secret();
    let before = files(dir.path());

    let result = ok(dir.path(), &["purge", &secret(), "--actor", ACTOR]);

    let lines: Vec<&str> = result.stdout.lines().collect();
    assert_eq!(
        lines[0],
        format!("Would purge blob {}; nothing is written", secret())
    );
    let count = |prefix: &str| lines.iter().filter(|line| line.starts_with(prefix)).count();
    // The working copy that `track` saved with the secret, the commit that `commit` made of it,
    // and the new working copy after it. The counts follow how many commits `track` and `commit`
    // make; the deleted commits are the old sides of the rewrites.
    assert_eq!(count("    rewrite "), 3, "{}", result.stdout);
    assert_eq!(count("    working-copy  "), 1);
    assert_eq!(count("    branch main  "), 1);
    assert_eq!(count("    stub entries "), 1);
    assert_eq!(count("    delete commit "), 3);
    assert_eq!(count("    delete tree "), 1);
    assert_eq!(count("    delete blob "), 1);
    assert_eq!(
        lines.last(),
        Some(&"Only the plan is built so far; the purge itself is not")
    );
    assert_eq!(files(dir.path()), before, "nothing was written");
}

#[test]
fn an_agent_is_refused() {
    let dir = with_secret();
    let before = files(dir.path());
    let result = run(dir.path(), &["purge", &secret(), "--actor", AGENT]);
    assert_eq!(result.code, 1);
    assert!(
        result.stderr.contains("an agent may not purge"),
        "{}",
        result.stderr
    );
    assert_eq!(result.stdout, "");
    assert_eq!(files(dir.path()), before);
}

#[test]
fn an_unknown_blob_is_refused() {
    let dir = with_secret();
    let unknown = Blob::new("nothing").id().to_string();
    let result = run(dir.path(), &["purge", &unknown, "--actor", ACTOR]);
    assert_eq!(result.code, 1);
    assert_eq!(
        result.stderr,
        format!(
            "stemma: could not plan the purge, nothing was written: the object store has no \
             blob {unknown}\n"
        )
    );
}

#[test]
fn an_id_that_is_not_an_id_is_an_argument_error() {
    let dir = with_secret();
    let result = run(dir.path(), &["purge", "b3:short", "--actor", ACTOR]);
    assert_eq!(result.code, 2, "{}", result.stderr);
}

#[test]
fn the_actor_is_required() {
    let dir = with_secret();
    let result = run(dir.path(), &["purge", &secret()]);
    assert_eq!(result.code, 1);
    assert!(result.stderr.contains("no actor"), "{}", result.stderr);
}
