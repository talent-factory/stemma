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

/// Whether a user is bound by permissions: root is not, and a test that needs a failure of the
/// file system through a mode must fail there and not pass without having tested anything.
#[cfg(unix)]
fn assert_permissions_bind(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let probe = dir.join("probe");
    fs::create_dir(&probe).unwrap();
    fs::set_permissions(&probe, fs::Permissions::from_mode(0o000)).unwrap();
    let binds = fs::read_dir(&probe).is_err();
    fs::set_permissions(&probe, fs::Permissions::from_mode(0o755)).unwrap();
    fs::remove_dir(&probe).unwrap();
    assert!(
        binds,
        "permissions do not bind this user; do not run the tests as root"
    );
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
fn status_removes_the_scratch_that_an_interrupted_command_left() {
    let dir = repository();
    let scratch = dir.path().join(".stemma").join("tmp").join("1-0-stale");
    fs::write(&scratch, "left behind").unwrap();
    let init = dir.path().join(".stemma-init-1-0-0123456789abcdef");
    fs::create_dir(&init).unwrap();

    let result = status(dir.path());

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(!scratch.exists());
    assert!(!init.exists());
}

#[test]
fn status_keeps_a_directory_of_the_working_copy_named_like_the_one_of_init() {
    let dir = repository();
    write(dir.path(), ".stemma-init-notes/plan.md", "mine");
    track(dir.path(), ".stemma-init-notes/plan.md");

    let result = status(dir.path());

    assert_eq!(result.code, 0, "{}", result.stderr);
    let plan = dir.path().join(".stemma-init-notes").join("plan.md");
    assert_eq!(fs::read_to_string(plan).unwrap(), "mine");
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
fn paths_are_sorted_by_bytes_and_not_by_directory() {
    let dir = repository();
    for path in ["B", "a-b", "a.b", "a/b", "a/c"] {
        write(dir.path(), path, "x");
    }

    let result = status(dir.path());

    assert_eq!(
        result.stdout,
        "Untracked files:\n? B\n? a-b\n? a.b\n? a/b\n? a/c\n"
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
fn a_head_that_names_a_commit_does_not_change_the_output() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    track(dir.path(), "a.txt");
    commit(dir.path(), "base");
    let head = fs::read_to_string(dir.path().join(".stemma/refs/heads/main")).unwrap();
    fs::write(
        dir.path().join(".stemma/HEAD"),
        format!("commit {}\n", head.trim()),
    )
    .unwrap();
    write(dir.path(), "a.txt", "two");

    let result = status(dir.path());

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(result.stdout, "Working copy changes:\nM a.txt\n");
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
fn a_quote_is_written_escaped() {
    let dir = repository();
    write(dir.path(), "it's.txt", "x");

    let result = status(dir.path());

    assert_eq!(result.stdout, "Untracked files:\n? it\\'s.txt\n");
}

#[test]
fn a_name_with_letters_of_other_scripts_is_written_as_it_is() {
    let dir = repository();
    write(dir.path(), "é日本 😀.txt", "x");

    let result = status(dir.path());

    assert_eq!(result.stdout, "Untracked files:\n? é日本 😀.txt\n");
}

#[test]
fn a_combining_mark_is_written_escaped_so_that_it_is_no_other_name_in_disguise() {
    let dir = repository();
    // `e` and a combining acute accent: the name that the person sees as `é` is not the name of
    // the single letter `é`.
    write(dir.path(), "e\u{301}.txt", "x");

    let result = status(dir.path());

    assert_eq!(result.stdout, "Untracked files:\n? e\\u{301}.txt\n");
}

#[cfg(unix)]
#[test]
fn a_backslash_is_written_escaped() {
    let dir = repository();
    write(dir.path(), "a\\b.txt", "x");

    let result = status(dir.path());

    assert_eq!(result.stdout, "Untracked files:\n? a\\\\b.txt\n");
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
fn the_directory_flag_and_the_actor_variable_are_used() {
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
    assert_eq!(result.stdout, "Working copy changes:\nA a.txt\n");
    assert!(operations(dir.path())[0].contains("\"actor\":\"human bob\""));
    assert!(
        !elsewhere.path().join(".stemma").exists(),
        "the command worked in the directory that it was given"
    );
}

#[test]
fn the_actor_flag_goes_before_the_variable() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    track(dir.path(), "a.txt");
    write(dir.path(), "a.txt", "two");

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
        result.stderr.starts_with("stemma: ") && result.stderr.contains("actor"),
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
    assert_eq!(result.stdout, "");
    assert!(
        result.stderr.contains("not in its parents") && result.stderr.contains("`status`"),
        "{}",
        result.stderr
    );
}

#[test]
fn an_argument_that_status_does_not_take_is_a_usage_error_and_saves_nothing() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    track(dir.path(), "a.txt");
    write(dir.path(), "a.txt", "two");
    let entries = operations(dir.path()).len();

    let result = run(dir.path(), &["status", "--actor", "human alice", "a.txt"]);

    assert_eq!(result.code, 2, "{}", result.stderr);
    assert_eq!(result.stdout, "");
    assert_eq!(operations(dir.path()).len(), entries);
}

#[test]
fn there_is_no_json_form() {
    let dir = repository();

    let result = run(dir.path(), &["status", "--actor", "human alice", "--json"]);

    assert_eq!(result.code, 2, "{}", result.stderr);
    assert_eq!(result.stdout, "");
}

#[test]
fn the_help_names_the_command_and_its_flags() {
    let dir = repository();

    let result = run(dir.path(), &["status", "--help"]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(
        // The binary is `stemma.exe` on Windows, so the name is matched without its suffix.
        result.stdout.contains("Usage: stemma")
            && result.stdout.contains(" status")
            && result.stdout.contains("--repository")
            && result.stdout.contains("--actor"),
        "{}",
        result.stdout
    );
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
        result.stderr.starts_with("stemma: .stemmaignore: line 2: ")
            && result.stderr.ends_with("; nothing was written\n"),
        "{}",
        result.stderr
    );
    assert_eq!(operations(dir.path()).len(), entries);
}

#[test]
fn a_name_that_a_tree_refuses_stops_the_command_before_anything_is_saved() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    track(dir.path(), "a.txt");
    write(dir.path(), "a.txt", "two");
    write(dir.path(), "d/a\u{200B}b.txt", "x");
    let entries = operations(dir.path()).len();

    let result = status(dir.path());

    assert_eq!(result.code, 1, "{}", result.stdout);
    assert_eq!(result.stdout, "");
    assert!(
        result.stderr.ends_with("; nothing was written\n"),
        "{}",
        result.stderr
    );
    assert_eq!(operations(dir.path()).len(), entries);
}

#[test]
fn a_name_that_a_tree_refuses_is_left_out_when_a_pattern_excludes_it() {
    let dir = repository();
    write(dir.path(), "d/a\u{200B}b.txt", "x");
    write(dir.path(), ".stemmaignore", "*.txt\n");

    let result = status(dir.path());

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(result.stdout, "Untracked files:\n? .stemmaignore\n");
}

#[test]
fn a_file_that_is_corrupt_in_the_repository_fails_without_a_trace() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    fs::write(dir.path().join(".stemma/working-copy"), "damaged\n").unwrap();
    let entries = operations(dir.path()).len();

    let result = status(dir.path());

    assert_eq!(result.code, 1, "{}", result.stdout);
    assert_eq!(result.stdout, "");
    assert!(result.stderr.starts_with("stemma: "), "{}", result.stderr);
    assert_eq!(operations(dir.path()).len(), entries);
}

#[test]
fn a_failure_of_reading_after_the_snapshot_is_code_1_with_the_entry_named_and_no_second_snapshot() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    track(dir.path(), "a.txt");
    commit(dir.path(), "base");
    write(dir.path(), "a.txt", "two");
    // The parent of the working-copy commit is gone: the snapshot is made, and reading fails.
    let head = fs::read_to_string(dir.path().join(".stemma/refs/heads/main")).unwrap();
    let hex = head.trim().strip_prefix("b3:").unwrap().to_owned();
    fs::remove_file(
        dir.path()
            .join(".stemma/objects/b3")
            .join(&hex[..2])
            .join(&hex[2..]),
    )
    .unwrap();
    let entries = operations(dir.path()).len();

    let first = status(dir.path());

    assert_eq!(first.code, 1, "{}", first.stdout);
    assert_eq!(first.stdout, "");
    let log = operations(dir.path());
    assert_eq!(log.len(), entries + 1, "{log:?}");
    assert!(log[0].contains("\"operation\":\"snapshot\""), "{log:?}");
    assert!(
        first.stderr.contains("saved before as entry"),
        "{}",
        first.stderr
    );
    let again = status(dir.path());
    assert_eq!(again.code, 1);
    assert_eq!(
        operations(dir.path()).len(),
        entries + 1,
        "the second call has nothing left to save"
    );
}

#[cfg(unix)]
#[test]
fn a_special_file_is_listed_as_untracked_and_track_refuses_it() {
    let dir = repository();
    let socket = std::os::unix::net::UnixListener::bind(dir.path().join("sock")).unwrap();

    let listed = status(dir.path());
    let refused = run(dir.path(), &["track", "--actor", "human daniel", "sock"]);

    assert_eq!(listed.code, 0, "{}", listed.stderr);
    assert_eq!(listed.stdout, "Untracked files:\n? sock\n");
    assert_eq!(refused.code, 1, "{}", refused.stdout);
    drop(socket);
}

#[cfg(unix)]
#[test]
fn a_directory_that_cannot_be_listed_stops_the_command_before_anything_is_saved() {
    use std::os::unix::fs::PermissionsExt;
    let dir = repository();
    assert_permissions_bind(dir.path());
    write(dir.path(), "a.txt", "one");
    track(dir.path(), "a.txt");
    write(dir.path(), "a.txt", "two");
    write(dir.path(), "closed/x.txt", "x");
    let closed = dir.path().join("closed");
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).unwrap();
    let entries = operations(dir.path()).len();

    let result = status(dir.path());
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(result.code, 1, "{}", result.stdout);
    assert_eq!(result.stdout, "");
    assert!(
        result.stderr.contains("could not list")
            && result.stderr.ends_with("; nothing was written\n"),
        "{}",
        result.stderr
    );
    assert_eq!(operations(dir.path()).len(), entries);
}

#[cfg(unix)]
#[test]
fn a_tracked_file_that_cannot_be_read_fails_and_saves_nothing() {
    use std::os::unix::fs::PermissionsExt;
    let dir = repository();
    assert_permissions_bind(dir.path());
    write(dir.path(), "a.txt", "one");
    track(dir.path(), "a.txt");
    let entries = operations(dir.path()).len();
    fs::set_permissions(dir.path().join("a.txt"), fs::Permissions::from_mode(0o000)).unwrap();

    let result = status(dir.path());
    fs::set_permissions(dir.path().join("a.txt"), fs::Permissions::from_mode(0o644)).unwrap();

    assert_eq!(result.code, 1, "{}", result.stdout);
    assert_eq!(result.stdout, "");
    assert!(result.stderr.contains("a.txt"), "{}", result.stderr);
    assert_eq!(operations(dir.path()).len(), entries);
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
    let binds = fs::read_dir(&metadata).is_err();
    let result = status(dir.path());
    fs::set_permissions(&metadata, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        binds,
        "permissions do not bind this user; do not run the tests as root"
    );

    assert_eq!(result.code, 3, "{}", result.stderr);
    assert_eq!(result.stdout, "Working copy changes:\nM a.txt\n");
    assert!(
        result
            .stderr
            .contains("the tracked files were saved as entry")
            && result.stderr.contains("flushing failed"),
        "{}",
        result.stderr
    );
    // The pointer moved: the next call finds nothing to save, and says so by saving nothing. It
    // does not make the entry durable, and the code says that it is fine.
    let entries = operations(dir.path()).len();
    let again = status(dir.path());
    assert_eq!(again.code, 0, "{}", again.stderr);
    assert_eq!(again.stdout, "Working copy changes:\nM a.txt\n");
    assert_eq!(operations(dir.path()).len(), entries);
}
