//! The command `stemma init`, run as a process, as a person or an agent would run it.

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

/// The path that a successful `init` reports.
fn reported_path(result: &Run) -> String {
    result
        .stdout
        .strip_prefix("Initialized empty Stemma repository in ")
        .unwrap_or_else(|| panic!("{}", result.stdout))
        .trim_end()
        .to_string()
}

fn head(dir: &Path) -> String {
    String::from_utf8(fs::read(dir.join(".stemma").join("HEAD")).unwrap()).unwrap()
}

#[test]
fn init_creates_a_repository_and_says_where_with_an_absolute_path() {
    let dir = Dir::new();
    let result = run(dir.path(), &["init", "--actor", "human daniel"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    let path = reported_path(&result);
    assert!(Path::new(&path).is_absolute(), "{path}");
    assert!(path.ends_with(".stemma"), "{path}");
    assert_eq!(result.stderr, "");
    assert_eq!(head(dir.path()), "branch main\n");
    assert!(dir.path().join(".stemma/refs/heads").is_dir());
}

#[test]
fn init_takes_a_directory_and_creates_it() {
    let dir = Dir::new();
    let result = run(dir.path(), &["init", "a/b", "--actor", "human daniel"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(head(&dir.path().join("a").join("b")), "branch main\n");
    assert!(!dir.path().join(".stemma").exists());
    let reported = reported_path(&result);
    assert!(Path::new(&reported).is_absolute(), "{reported}");
    let components: Vec<_> = Path::new(&reported)
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
    assert!(
        components.ends_with(&["a".into(), "b".into(), ".stemma".into()]),
        "{reported}"
    );
}

#[test]
fn init_uses_the_initial_branch() {
    let dir = Dir::new();
    let result = run(
        dir.path(),
        &[
            "init",
            "--initial-branch",
            "trunk/main",
            "--actor",
            "human daniel",
        ],
    );
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(head(dir.path()), "branch trunk/main\n");
}

#[test]
fn an_invalid_branch_is_a_usage_error_and_creates_nothing() {
    let dir = Dir::new();
    for bad in ["", "a b", "/a", "a/"] {
        let result = run(
            dir.path(),
            &[
                "init",
                "target",
                "--initial-branch",
                bad,
                "--actor",
                "human daniel",
            ],
        );
        assert_eq!(result.code, 2, "{bad:?}");
        assert!(
            result.stderr.contains("--initial-branch"),
            "{bad:?}: {}",
            result.stderr
        );
        assert_eq!(result.stdout, "");
        assert!(!dir.path().join("target").exists(), "{bad:?}");
    }
}

#[test]
fn without_an_actor_the_command_fails_and_creates_nothing() {
    let dir = Dir::new();
    let result = run(dir.path(), &["init", "target"]);
    assert_eq!(result.code, 1);
    assert!(
        result.stderr.starts_with("stemma: no actor"),
        "{}",
        result.stderr
    );
    assert!(
        result.stderr.contains("--actor") && result.stderr.contains("STEMMA_ACTOR"),
        "{}",
        result.stderr
    );
    assert_eq!(result.stdout, "");
    assert!(!dir.path().join("target").exists());
}

#[test]
fn an_empty_actor_is_reported_as_empty_and_creates_nothing() {
    let dir = Dir::new();
    let from_variable = stemma(dir.path(), &["init", "target"], |command| {
        command.env("STEMMA_ACTOR", "");
    });
    let from_flag = run(dir.path(), &["init", "target", "--actor", ""]);
    // An empty flag does not fall back to a variable that is set.
    let flag_over_variable = stemma(dir.path(), &["init", "target", "--actor", ""], |command| {
        command.env("STEMMA_ACTOR", "human daniel");
    });
    for result in [from_variable, from_flag, flag_over_variable] {
        assert_eq!(result.code, 1);
        assert!(
            result.stderr.starts_with("stemma: the actor is empty"),
            "{}",
            result.stderr
        );
        assert_eq!(result.stdout, "");
    }
    assert!(!dir.path().join("target").exists());
}

#[test]
fn the_actor_can_come_from_the_environment() {
    let dir = Dir::new();
    let result = stemma(dir.path(), &["init"], |command| {
        command.env(
            "STEMMA_ACTOR",
            "agent claude model=opus-5-5 session=s1 sponsor=daniel",
        );
    });
    assert_eq!(result.code, 0, "{}", result.stderr);
}

#[test]
fn the_flag_goes_before_the_environment() {
    let dir = Dir::new();
    let result = stemma(
        dir.path(),
        &["init", "--actor", "human daniel"],
        |command| {
            command.env("STEMMA_ACTOR", "not an actor");
        },
    );
    assert_eq!(result.code, 0, "{}", result.stderr);
}

#[test]
fn an_invalid_actor_is_reported_with_the_reason() {
    let dir = Dir::new();
    for bad in [
        "daniel",
        "human",
        "human a b",
        "human dänu",
        "agent a",
        "  ",
    ] {
        let result = run(dir.path(), &["init", "target", "--actor", bad]);
        assert_eq!(result.code, 1, "{bad:?}");
        assert!(
            result.stderr.starts_with("stemma: invalid actor: "),
            "{bad:?}: {}",
            result.stderr
        );
        assert!(!dir.path().join("target").exists(), "{bad:?}");
    }
}

#[test]
fn a_second_init_fails_and_changes_nothing() {
    let dir = Dir::new();
    let first = run(dir.path(), &["init", "--actor", "human daniel"]);
    assert_eq!(first.code, 0);
    let marker = dir.path().join(".stemma").join("marker");
    fs::write(&marker, b"keep").unwrap();
    let second = run(dir.path(), &["init", "--actor", "human daniel"]);
    assert_eq!(second.code, 1);
    // The wording is that of the core; the contract of the command is the prefix and the code.
    assert!(second.stderr.starts_with("stemma: "), "{}", second.stderr);
    assert!(second.stderr.contains(".stemma"), "{}", second.stderr);
    assert_eq!(second.stdout, "");
    assert_eq!(fs::read(&marker).unwrap(), b"keep");
}

#[test]
fn a_path_that_is_a_file_fails_and_leaves_the_file() {
    let dir = Dir::new();
    let file = dir.path().join("README.md");
    fs::write(&file, b"keep").unwrap();
    let result = run(
        dir.path(),
        &["init", "README.md", "--actor", "human daniel"],
    );
    assert_eq!(result.code, 1);
    assert!(result.stderr.starts_with("stemma: "), "{}", result.stderr);
    assert_eq!(result.stdout, "");
    assert_eq!(fs::read(&file).unwrap(), b"keep");
}

#[test]
fn a_command_is_required() {
    let dir = Dir::new();
    let result = run(dir.path(), &[]);
    assert_eq!(result.code, 2);
    assert!(result.stderr.contains("Usage"), "{}", result.stderr);
}

#[test]
fn version_and_help_succeed() {
    let dir = Dir::new();
    let version = run(dir.path(), &["--version"]);
    assert_eq!(version.code, 0);
    assert_eq!(
        version.stdout.trim_end(),
        format!("stemma {}", env!("CARGO_PKG_VERSION"))
    );
    let help = run(dir.path(), &["init", "--help"]);
    assert_eq!(help.code, 0);
    for text in ["--initial-branch", "--actor", "STEMMA_ACTOR"] {
        assert!(help.stdout.contains(text), "{text}: {}", help.stdout);
    }
}
