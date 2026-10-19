//! The command `stemma init`, run as a process, as a person or an agent would run it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
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

/// `stemma` in `dir` without an actor in the environment.
fn stemma(dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_stemma"));
    command.current_dir(dir).env_remove("STEMMA_ACTOR");
    command
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("the process ended by a signal")
}

fn head(dir: &Path) -> String {
    text(&fs::read(dir.join(".stemma").join("HEAD")).unwrap())
}

#[test]
fn init_creates_a_repository_and_says_where() {
    let dir = Dir::new();
    let output = stemma(dir.path())
        .args(["init", "--actor", "human daniel"])
        .output()
        .unwrap();
    assert_eq!(code(&output), 0, "{}", text(&output.stderr));
    let stdout = text(&output.stdout);
    assert!(
        stdout.starts_with("Initialized empty Stemma repository in "),
        "{stdout}"
    );
    assert!(stdout.trim_end().ends_with(".stemma"), "{stdout}");
    assert_eq!(text(&output.stderr), "");
    assert_eq!(head(dir.path()), "branch main\n");
    assert!(dir
        .path()
        .join(".stemma")
        .join("refs")
        .join("heads")
        .is_dir());
}

#[test]
fn init_takes_a_directory_and_creates_it() {
    let dir = Dir::new();
    let output = stemma(dir.path())
        .args(["init", "a/b", "--actor", "human daniel"])
        .output()
        .unwrap();
    assert_eq!(code(&output), 0, "{}", text(&output.stderr));
    assert_eq!(head(&dir.path().join("a").join("b")), "branch main\n");
    assert!(!dir.path().join(".stemma").exists());
}

#[test]
fn init_uses_the_initial_branch() {
    let dir = Dir::new();
    let output = stemma(dir.path())
        .args([
            "init",
            "--initial-branch",
            "trunk/main",
            "--actor",
            "human daniel",
        ])
        .output()
        .unwrap();
    assert_eq!(code(&output), 0, "{}", text(&output.stderr));
    assert_eq!(head(dir.path()), "branch trunk/main\n");
}

#[test]
fn an_invalid_branch_is_a_usage_error_and_creates_nothing() {
    let dir = Dir::new();
    let output = stemma(dir.path())
        .args([
            "init",
            "target",
            "--initial-branch",
            "a b",
            "--actor",
            "human daniel",
        ])
        .output()
        .unwrap();
    assert_eq!(code(&output), 2);
    let stderr = text(&output.stderr);
    assert!(stderr.contains("--initial-branch"), "{stderr}");
    assert!(!dir.path().join("target").exists());
}

#[test]
fn without_an_actor_the_command_fails_and_creates_nothing() {
    let dir = Dir::new();
    let output = stemma(dir.path())
        .args(["init", "target"])
        .output()
        .unwrap();
    assert_eq!(code(&output), 1);
    let stderr = text(&output.stderr);
    assert!(stderr.starts_with("stemma: no actor"), "{stderr}");
    assert!(
        stderr.contains("--actor") && stderr.contains("STEMMA_ACTOR"),
        "{stderr}"
    );
    assert_eq!(text(&output.stdout), "");
    assert!(!dir.path().join("target").exists());
}

#[test]
fn an_empty_actor_variable_is_no_actor() {
    let dir = Dir::new();
    let output = stemma(dir.path())
        .arg("init")
        .env("STEMMA_ACTOR", "")
        .output()
        .unwrap();
    assert_eq!(code(&output), 1);
    assert!(
        text(&output.stderr).contains("no actor"),
        "{}",
        text(&output.stderr)
    );
}

#[test]
fn the_actor_can_come_from_the_environment() {
    let dir = Dir::new();
    let output = stemma(dir.path())
        .arg("init")
        .env(
            "STEMMA_ACTOR",
            "agent claude model=opus-5-5 session=s1 sponsor=daniel",
        )
        .output()
        .unwrap();
    assert_eq!(code(&output), 0, "{}", text(&output.stderr));
}

#[test]
fn the_flag_goes_before_the_environment() {
    let dir = Dir::new();
    let output = stemma(dir.path())
        .args(["init", "--actor", "human daniel"])
        .env("STEMMA_ACTOR", "not an actor")
        .output()
        .unwrap();
    assert_eq!(code(&output), 0, "{}", text(&output.stderr));
}

#[test]
fn an_invalid_actor_is_reported_with_the_reason() {
    let dir = Dir::new();
    for bad in ["daniel", "human a b", "human dänu", "agent a"] {
        let output = stemma(dir.path())
            .args(["init", "target", "--actor", bad])
            .output()
            .unwrap();
        assert_eq!(code(&output), 1, "{bad}");
        let stderr = text(&output.stderr);
        assert!(
            stderr.starts_with("stemma: invalid actor: "),
            "{bad}: {stderr}"
        );
        assert!(!dir.path().join("target").exists(), "{bad}");
    }
}

#[test]
fn a_second_init_fails_and_changes_nothing() {
    let dir = Dir::new();
    let first = stemma(dir.path())
        .args(["init", "--actor", "human daniel"])
        .output()
        .unwrap();
    assert_eq!(code(&first), 0);
    let marker = dir.path().join(".stemma").join("marker");
    fs::write(&marker, b"keep").unwrap();
    let second = stemma(dir.path())
        .args(["init", "--actor", "human daniel"])
        .output()
        .unwrap();
    assert_eq!(code(&second), 1);
    let stderr = text(&second.stderr);
    assert!(stderr.starts_with("stemma: "), "{stderr}");
    assert!(
        stderr.contains("exists already") && stderr.contains(".stemma"),
        "{stderr}"
    );
    assert_eq!(text(&second.stdout), "");
    assert_eq!(fs::read(&marker).unwrap(), b"keep");
}

#[test]
fn a_command_is_required() {
    let dir = Dir::new();
    let output = stemma(dir.path()).output().unwrap();
    assert_eq!(code(&output), 2);
    assert!(
        text(&output.stderr).contains("Usage"),
        "{}",
        text(&output.stderr)
    );
}

#[test]
fn version_and_help_succeed() {
    let dir = Dir::new();
    let version = stemma(dir.path()).arg("--version").output().unwrap();
    assert_eq!(code(&version), 0);
    assert_eq!(
        text(&version.stdout).trim_end(),
        format!("stemma {}", env!("CARGO_PKG_VERSION"))
    );
    let help = stemma(dir.path())
        .args(["init", "--help"])
        .output()
        .unwrap();
    assert_eq!(code(&help), 0);
    let stdout = text(&help.stdout);
    assert!(
        stdout.contains("--initial-branch") && stdout.contains("--actor"),
        "{stdout}"
    );
    assert!(stdout.contains("STEMMA_ACTOR"), "{stdout}");
}
