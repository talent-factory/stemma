//! The command `stemma purge`, run as a process, as a person or an agent would run it (ADR-014):
//! the plan, which writes nothing, the purge with `--apply`, and `--resume` and `--abort` of a
//! purge that stopped.

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
        Some(&"Run it with --apply; a purge cannot be undone")
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

/// The path of an object of the store.
fn object_path(dir: &Path, id: &str) -> PathBuf {
    let hex = id.strip_prefix("b3:").unwrap();
    dir.join(".stemma/objects/b3")
        .join(&hex[..2])
        .join(&hex[2..])
}

#[test]
fn apply_purges_the_blob_and_moves_the_repository_to_format_3() {
    let dir = with_secret();
    let result = ok(
        dir.path(),
        &["purge", &secret(), "--apply", "--actor", ACTOR],
    );
    let lines: Vec<&str> = result.stdout.lines().collect();
    assert!(
        lines[0].starts_with(&format!("Purged blob {} as entry ", secret())),
        "{}",
        result.stdout
    );
    assert_eq!(
        lines.last(),
        Some(&format!("    delete blob {}", secret()).as_str())
    );
    assert!(lines.iter().any(|line| line.starts_with("    rewrite ")));
    assert!(
        !object_path(dir.path(), &secret()).exists(),
        "the blob is gone"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join(".stemma/format")).unwrap(),
        "stemma-repository 3\n"
    );
    assert!(!dir.path().join(".stemma/purge-pending").exists());

    // The log shows the stubs and the purge, and a restore of them is refused.
    let log = ok(dir.path(), &["op", "log"]);
    assert!(log.stdout.contains("purge"), "{}", log.stdout);
    assert!(log.stdout.contains("stub"), "{}", log.stdout);
    // Entry 4 saved the secret with `track`; it names a rewritten commit, so it is a stub now.
    let restore = run(
        dir.path(),
        &["op", "restore", "4", "--force", "--actor", ACTOR],
    );
    assert_eq!(restore.code, 1, "{}", restore.stderr);
    assert!(restore.stderr.contains("stub"), "{}", restore.stderr);

    // A second plan finds no blob, and the next commands work.
    let again = run(dir.path(), &["purge", &secret(), "--actor", ACTOR]);
    assert_eq!(again.code, 1);
    ok(dir.path(), &["status", "--actor", ACTOR]);
}

#[test]
fn an_agent_may_not_apply_resume_or_abort() {
    let dir = with_secret();
    let before = files(dir.path());
    for args in [
        vec!["purge", &secret(), "--apply"],
        vec!["purge", "--resume"],
        vec!["purge", "--abort"],
    ] {
        let mut args = args.clone();
        args.extend(["--actor", AGENT]);
        let result = run(dir.path(), &args);
        assert_eq!(result.code, 1, "{args:?}");
        assert!(
            result.stderr.contains("an agent may not purge"),
            "{}",
            result.stderr
        );
    }
    assert_eq!(files(dir.path()), before);
}

#[test]
fn resume_and_abort_without_a_purge_are_refused() {
    let dir = with_secret();
    for flag in ["--resume", "--abort"] {
        let result = run(dir.path(), &["purge", flag, "--actor", ACTOR]);
        assert_eq!(result.code, 1, "{flag}: {}", result.stderr);
        assert!(
            result.stderr.contains("no purge is pending"),
            "{}",
            result.stderr
        );
    }
}

#[test]
fn the_modes_exclude_each_other_and_the_blob_is_required_otherwise() {
    let dir = with_secret();
    for args in [
        vec!["purge", "--apply"],
        vec!["purge", "--resume", "--abort"],
        vec!["purge", &secret(), "--resume"],
        vec!["purge", &secret(), "--apply", "--abort"],
    ] {
        let result = run(dir.path(), &args);
        assert_eq!(result.code, 2, "{args:?}: {}", result.stderr);
    }
}

#[test]
fn a_writer_refuses_while_a_journal_exists_and_names_resume() {
    let dir = with_secret();
    fs::write(dir.path().join(".stemma/purge-pending"), "damaged").unwrap();
    fs::write(dir.path().join("a.txt"), "two").unwrap();
    for args in [
        vec!["track", "a.txt"],
        vec!["commit", "-m", "more"],
        vec!["status"],
        vec!["op", "restore", "2"],
        vec!["purge", &secret(), "--apply"],
    ] {
        let mut args = args.clone();
        args.extend(["--actor", ACTOR]);
        let result = run(dir.path(), &args);
        assert_ne!(result.code, 0, "{args:?}");
        assert!(
            result.stderr.contains("stemma purge --resume"),
            "{args:?}: {}",
            result.stderr
        );
    }
    // A journal that cannot be read stops the resume, and the journal stays.
    let result = run(dir.path(), &["purge", "--resume", "--actor", ACTOR]);
    assert_eq!(result.code, 3, "{}", result.stderr);
    assert!(
        result.stderr.contains("not the journal of a purge"),
        "{}",
        result.stderr
    );
}

/// Makes the directory of the log read-only, so that the purge stops at its entry after the new
/// objects are written, and returns the stopped run.
#[cfg(unix)]
fn stop_at_the_entry(dir: &Path) -> Run {
    use std::os::unix::fs::PermissionsExt;
    let oplog = dir.join(".stemma/oplog");
    fs::set_permissions(&oplog, fs::Permissions::from_mode(0o555)).unwrap();
    let result = run(dir, &["purge", &secret(), "--apply", "--actor", ACTOR]);
    fs::set_permissions(&oplog, fs::Permissions::from_mode(0o755)).unwrap();
    result
}

#[cfg(unix)]
#[test]
fn a_purge_that_stopped_is_finished_by_resume() {
    let dir = with_secret();
    let stopped = stop_at_the_entry(dir.path());
    assert_eq!(stopped.code, 3, "{}", stopped.stderr);
    assert!(
        stopped.stderr.contains("the purge stopped at"),
        "{}",
        stopped.stderr
    );
    assert!(dir.path().join(".stemma/purge-pending").exists());
    assert!(
        object_path(dir.path(), &secret()).exists(),
        "nothing is lost yet"
    );

    let result = ok(dir.path(), &["purge", "--resume", "--actor", ACTOR]);
    assert!(
        result
            .stdout
            .starts_with(&format!("Finished the purge of blob {}", secret())),
        "{}",
        result.stdout
    );
    assert!(!object_path(dir.path(), &secret()).exists());
    assert!(!dir.path().join(".stemma/purge-pending").exists());
}

#[cfg(unix)]
#[test]
fn a_purge_that_stopped_before_its_entry_is_undone_by_abort() {
    let dir = with_secret();
    let before: Vec<(PathBuf, Vec<u8>)> = files(dir.path())
        .into_iter()
        .filter(|(path, _)| !path.ends_with("format"))
        .collect();
    let stopped = stop_at_the_entry(dir.path());
    assert_eq!(stopped.code, 3, "{}", stopped.stderr);
    let result = ok(dir.path(), &["purge", "--abort", "--actor", ACTOR]);
    assert!(
        result
            .stdout
            .starts_with(&format!("Undid the purge of blob {}", secret())),
        "{}",
        result.stdout
    );
    let after: Vec<(PathBuf, Vec<u8>)> = files(dir.path())
        .into_iter()
        .filter(|(path, _)| !path.ends_with("format"))
        .collect();
    assert_eq!(
        after, before,
        "the repository is as it was, but for its format"
    );
}
