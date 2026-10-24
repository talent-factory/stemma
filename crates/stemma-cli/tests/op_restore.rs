//! The command `stemma op restore`, run as a process, as a person or an agent would run it.

mod common;

use std::fs;
use std::path::Path;
use std::process::Command;

use stemma_core::repository::Repository;

use common::{ok, run, write, write_stub_entry, Dir, ACTOR};

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
fn a_commit_after_a_restore_before_a_commit_is_refused_and_finishes_nothing() {
    // The restored working-copy commit is an older version of the one that the second commit
    // named; the log says that commit took effect, so it did not stop between its moves.
    let (dir, snapshot) = two_commits();
    ok(
        dir.path(),
        &["op", "restore", &snapshot.to_string(), "--actor", ACTOR],
    );
    let entries = numbers(dir.path()).len();
    let pointers = ["working-copy", "refs/heads/main"]
        .map(|name| read(dir.path(), &format!(".stemma/{name}")));

    let result = run(dir.path(), &["commit", "-m", "third", "--actor", ACTOR]);

    assert_eq!(result.code, 1, "{}", result.stdout);
    assert_eq!(result.stdout, "");
    assert!(
        result.stderr.contains("the branch `main` is at"),
        "{}",
        result.stderr
    );
    assert!(!result.stderr.contains("warning"), "{}", result.stderr);
    assert_eq!(numbers(dir.path()).len(), entries);
    assert_eq!(
        ["working-copy", "refs/heads/main"]
            .map(|name| read(dir.path(), &format!(".stemma/{name}"))),
        pointers
    );
}

#[test]
fn a_refused_commit_names_the_entry_of_the_snapshot_it_saved_first() {
    let (dir, snapshot) = two_commits();
    ok(
        dir.path(),
        &["op", "restore", &snapshot.to_string(), "--actor", ACTOR],
    );
    write(dir.path(), "a.txt", "scratch");

    let result = run(dir.path(), &["commit", "-m", "third", "--actor", ACTOR]);

    assert_eq!(result.code, 1, "{}", result.stdout);
    let saved = *numbers(dir.path()).last().unwrap();
    assert!(
        result.stderr.starts_with("stemma: the branch `main` is at"),
        "{}",
        result.stderr
    );
    assert!(
        result.stderr.ends_with(&format!(
            "; the working copy was saved first as entry {saved}\n"
        )),
        "{}",
        result.stderr
    );
    assert_eq!(read(dir.path(), "a.txt"), "scratch");
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
    assert_eq!(
        result.stderr,
        "stemma: entry 1 created HEAD, which cannot be absent, so it cannot be restored\n"
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

/// A line `    <name>  b3:<id> -> b3:<id>` or `... -> -`, checked without the IDs.
fn assert_pointer_line(line: &str, name: &str, deleted: bool) {
    let rest = line
        .strip_prefix(&format!("    {name}  b3:"))
        .unwrap_or_else(|| panic!("{line:?} is not a line of {name}"));
    let (old, new) = rest.split_once(" -> ").unwrap();
    assert_eq!(old.len(), 64, "{line:?}");
    if deleted {
        assert_eq!(new, "-", "{line:?}");
    } else {
        assert_eq!(new.strip_prefix("b3:").map(str::len), Some(64), "{line:?}");
    }
}

#[test]
fn a_dry_run_names_the_pointers_and_the_files_in_order() {
    let (dir, snapshot) = two_commits();

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

    let lines: Vec<&str> = result.stdout.lines().collect();
    assert_eq!(
        lines[0],
        format!("Would restore the state before entry {snapshot}; nothing is written")
    );
    assert_pointer_line(lines[1], "working-copy", false);
    assert_eq!(lines[2], "    write a.txt");
    assert_eq!(
        lines[3],
        "A real run saves unsaved changes first; the files above are replaced all the same"
    );
    assert_eq!(lines.len(), 4, "{}", result.stdout);
}

#[test]
fn a_dry_run_of_a_commit_shows_the_branch_and_no_file_note() {
    let (dir, snapshot) = two_commits();
    let commit = snapshot + 1;

    let result = ok(
        dir.path(),
        &[
            "op",
            "restore",
            &commit.to_string(),
            "--dry-run",
            "--actor",
            ACTOR,
        ],
    );

    // A commit changes the pointers and not the tree, so there is nothing to write.
    let lines: Vec<&str> = result.stdout.lines().collect();
    assert_pointer_line(lines[1], "working-copy", false);
    assert_pointer_line(lines[2], "branch main", false);
    assert_eq!(lines.len(), 3, "{}", result.stdout);
}

#[test]
fn a_dry_run_shows_a_deleted_branch_and_a_removed_file_and_the_restore_does_both() {
    let dir = Dir::new();
    ok(dir.path(), &["init", "--actor", ACTOR]);
    write(dir.path(), "a.txt", "one");
    ok(dir.path(), &["track", "a.txt", "--actor", ACTOR]);
    ok(dir.path(), &["commit", "-m", "first", "--actor", ACTOR]);
    let all = numbers(dir.path());
    let (track, commit) = (all[1].to_string(), all[2].to_string());

    let branch = ok(
        dir.path(),
        &["op", "restore", &commit, "--dry-run", "--actor", ACTOR],
    );
    let files = ok(
        dir.path(),
        &["op", "restore", &track, "--dry-run", "--actor", ACTOR],
    );

    let lines: Vec<&str> = branch.stdout.lines().collect();
    assert_pointer_line(lines[2], "branch main", true);
    let lines: Vec<&str> = files.stdout.lines().collect();
    assert_pointer_line(lines[1], "working-copy", false);
    assert_eq!(lines[2], "    remove a.txt", "{}", files.stdout);
    ok(dir.path(), &["op", "restore", &track, "--actor", ACTOR]);
    assert!(
        !dir.path().join("a.txt").exists(),
        "the tracked file is removed"
    );
}

/// Moves the working-copy commit to a commit that no entry names as one, so that the outcome of
/// the newest entry of the working copy is not known.
fn make_the_outcome_unknown(dir: &Path) {
    let repository = Repository::open(dir).unwrap();
    let tip = repository
        .refs()
        .branch(&"main".parse().unwrap())
        .unwrap()
        .unwrap();
    let parent = {
        let raw = stemma_core::store::ObjectStore::get(repository.objects(), &tip.object_id())
            .unwrap()
            .unwrap();
        stemma_core::object::Commit::parse(raw.body())
            .unwrap()
            .parents()[0]
    };
    fs::write(
        dir.join(".stemma").join("working-copy"),
        format!("{parent}\n"),
    )
    .unwrap();
}

#[test]
fn an_unknown_outcome_is_refused_and_force_restores_anyway() {
    let (dir, snapshot) = two_commits();
    let commit = (snapshot + 1).to_string();
    make_the_outcome_unknown(dir.path());
    let before = numbers(dir.path());

    let refused = run(dir.path(), &["op", "restore", &commit, "--actor", ACTOR]);
    let planned = run(
        dir.path(),
        &["op", "restore", &commit, "--dry-run", "--actor", ACTOR],
    );

    for result in [&refused, &planned] {
        assert_eq!(result.code, 1, "{}", result.stdout);
        assert!(
            result
                .stderr
                .contains(&format!("whether entry {commit} took effect"))
                && result.stderr.contains("use --force to restore anyway"),
            "{}",
            result.stderr
        );
    }
    assert_eq!(numbers(dir.path()), before);
    let dry = ok(
        dir.path(),
        &[
            "op",
            "restore",
            &commit,
            "--dry-run",
            "--force",
            "--actor",
            ACTOR,
        ],
    );
    assert!(dry.stdout.starts_with("Would restore"), "{}", dry.stdout);
    assert_eq!(
        numbers(dir.path()),
        before,
        "a dry run with --force writes nothing"
    );
    let forced = ok(
        dir.path(),
        &["op", "restore", &commit, "--force", "--actor", ACTOR],
    );
    assert!(
        forced.stdout.contains("Restored the state before entry"),
        "{}",
        forced.stdout
    );
}

#[test]
fn the_actor_comes_from_the_environment_and_the_flag_wins() {
    let (dir, snapshot) = two_commits();
    let number = snapshot.to_string();
    let output = Command::new(env!("CARGO_BIN_EXE_stemma"))
        .current_dir(dir.path())
        .env("STEMMA_ACTOR", "human erika")
        .args(["op", "restore", &number])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let repository = Repository::open(dir.path()).unwrap();
    let last = repository.oplog().entries().unwrap().pop().unwrap();
    assert_eq!(last.change().unwrap().actor().to_string(), "human erika");

    let again = Command::new(env!("CARGO_BIN_EXE_stemma"))
        .current_dir(dir.path())
        .env("STEMMA_ACTOR", "human erika")
        .args([
            "op",
            "restore",
            &last.number().to_string(),
            "--actor",
            "human max",
        ])
        .output()
        .unwrap();
    assert_eq!(again.status.code(), Some(0), "{again:?}");
    let last = Repository::open(dir.path())
        .unwrap()
        .oplog()
        .entries()
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(last.change().unwrap().actor().to_string(), "human max");
}

#[test]
fn wrong_arguments_are_code_2_and_write_nothing() {
    let (dir, snapshot) = two_commits();
    let before = numbers(dir.path());
    let number = snapshot.to_string();
    let cases: [&[&str]; 6] = [
        &["op", "restore"],
        &["op", "restore", "-3"],
        &["op", "restore", "99999999999999999999999"],
        &["op", "restore", &number, "--unknown"],
        &["op", "restore", &number, "extra"],
        &["op", "restore", &number, "--force", "--force"],
    ];
    for args in cases {
        let mut full = args.to_vec();
        full.extend(["--actor", ACTOR]);
        let result = run(dir.path(), &full);
        assert_eq!(result.code, 2, "{args:?}: {}", result.stderr);
        assert_eq!(result.stdout, "");
    }
    assert_eq!(numbers(dir.path()), before);
}

#[test]
fn a_directory_without_a_repository_is_an_error_that_names_the_flag() {
    let (_dir, snapshot) = two_commits();
    let elsewhere = Dir::new();

    let result = run(
        elsewhere.path(),
        &["op", "restore", &snapshot.to_string(), "--actor", ACTOR],
    );

    assert_eq!(result.code, 1);
    assert!(result.stderr.starts_with("stemma: "), "{}", result.stderr);
    assert!(result.stderr.contains("-C"), "{}", result.stderr);
}

#[cfg(unix)]
#[test]
fn a_restore_whose_pointers_cannot_move_names_the_way_on_and_the_same_call_finishes_it() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, snapshot) = two_commits();
    let commit = (snapshot + 1).to_string();
    let branch = |dir: &Path| {
        Repository::open(dir)
            .unwrap()
            .refs()
            .branch(&"main".parse().unwrap())
            .unwrap()
    };
    let tip = branch(dir.path());
    // The branch is written through a temporary file that is renamed into `refs/heads`, which
    // a directory that cannot be written stops. The entry comes before the move.
    let heads = dir.path().join(".stemma").join("refs").join("heads");
    fs::set_permissions(&heads, fs::Permissions::from_mode(0o555)).unwrap();
    // A process that runs as root ignores the permission; the test needs a real failure.
    if fs::File::create(heads.join("probe")).is_ok() {
        let _ = fs::remove_file(heads.join("probe"));
        fs::set_permissions(&heads, fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }

    let result = run(dir.path(), &["op", "restore", &commit, "--actor", ACTOR]);

    fs::set_permissions(&heads, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(result.code, 1, "{}", result.stdout);
    assert!(
        result
            .stderr
            .contains("the files are at the old state, but no pointer moved")
            && result.stderr.contains("run the same restore again"),
        "{}",
        result.stderr
    );
    assert_eq!(branch(dir.path()), tip, "no pointer moved");

    let finished = ok(dir.path(), &["op", "restore", &commit, "--actor", ACTOR]);

    assert!(
        finished.stdout.contains("Restored the state before entry"),
        "{}",
        finished.stdout
    );
    assert_ne!(
        branch(dir.path()),
        tip,
        "the branch is back at its old commit"
    );
}

#[test]
fn a_stub_and_a_purge_cannot_be_restored() {
    let (dir, snapshot) = two_commits();
    let last = *numbers(dir.path()).last().unwrap();
    let purge = last + 1;
    write_stub_entry(dir.path(), snapshot, "stub\n");
    write_stub_entry(
        dir.path(),
        purge,
        &format!("operation purge\ncommits 1\nstubs {snapshot}\n"),
    );

    let result = run(
        dir.path(),
        &["op", "restore", &snapshot.to_string(), "--actor", ACTOR],
    );
    assert_eq!(result.code, 1);
    assert!(
        result
            .stderr
            .contains("was replaced by a stub when a secret was purged"),
        "{}",
        result.stderr
    );
    let result = run(
        dir.path(),
        &["op", "restore", &purge.to_string(), "--actor", ACTOR],
    );
    assert_eq!(result.code, 1);
    assert!(
        result
            .stderr
            .contains("is a purge, which removed history for good"),
        "{}",
        result.stderr
    );
    assert_eq!(
        *numbers(dir.path()).last().unwrap(),
        purge,
        "nothing was written"
    );
}
