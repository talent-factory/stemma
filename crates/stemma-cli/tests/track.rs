//! The command `stemma track`, run as a process, as a person or an agent would run it.

mod common;

use std::fs;
use std::path::Path;

use common::{operations, repository, run, stemma, write, Dir, Run, ACTOR};

fn track(dir: &Path, paths: &[&str]) -> Run {
    let mut args = vec!["track", "--actor", "human alice"];
    args.extend_from_slice(paths);
    run(dir, &args)
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
        log[0].contains("\"actor\":\"human alice\"") && log[0].contains("\"operation\":\"track\""),
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
    use stemma_core::object::Blob;
    use stemma_core::repository::Repository;
    use stemma_core::store::ObjectStore;
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    assert_eq!(track(dir.path(), &["a.txt"]).code, 0);
    write(dir.path(), "a.txt", "changed");
    write(dir.path(), "b.txt", "two");
    let changed = Blob::new(b"changed".to_vec()).id().object_id();
    let repository = Repository::open(dir.path()).unwrap();
    assert!(!repository.objects().contains(&changed).unwrap());
    assert_eq!(track(dir.path(), &["b.txt"]).code, 0);
    // The content of the tracked file was saved by a track of another file.
    assert!(repository.objects().contains(&changed).unwrap());
    let before = operations(dir.path()).len();
    assert_eq!(
        track(dir.path(), &["a.txt"]).stdout,
        "No new files to track\n"
    );
    assert_eq!(operations(dir.path()).len(), before);
}

#[test]
fn an_empty_argument_does_not_track_the_directory() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    for path in ["", "//"] {
        let result = track(dir.path(), &[path]);
        assert_eq!(result.code, 1, "{path:?}: {}", result.stderr);
        assert!(result.stderr.starts_with("stemma: "), "{}", result.stderr);
    }
    assert_eq!(operations(dir.path()).len(), 1);
}

#[test]
fn a_dot_with_other_paths_and_in_an_empty_directory() {
    let dir = repository();
    let result = track(dir.path(), &["."]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(result.stdout, "No new files to track\n");
    assert_eq!(operations(dir.path()).len(), 1);
    write(dir.path(), "a.txt", "a");
    write(dir.path(), "d/b.txt", "b");
    let result = track(dir.path(), &["d", "a.txt", "."]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(result.stdout, "tracked a.txt\ntracked d/b.txt\n");
}

#[test]
fn a_relative_repository_is_the_base_of_the_paths() {
    let outer = Dir::new();
    let init = run(
        outer.path(),
        &["init", "-C", "proj", "--actor", "human daniel"],
    );
    assert_eq!(init.code, 0, "{}", init.stderr);
    write(&outer.path().join("proj"), "a.txt", "one");
    fs::create_dir(outer.path().join("elsewhere")).unwrap();
    let result = run(
        &outer.path().join("elsewhere"),
        &["track", "-C", "../proj", "--actor", "human alice", "."],
    );
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(result.stdout, "tracked a.txt\n");
}

#[test]
fn below_the_root_the_repository_is_found_and_the_paths_are_relative_to_the_directory() {
    let dir = repository();
    let sub = dir.path().join("sub");
    fs::create_dir_all(sub.join("deep")).unwrap();
    write(&sub, "a.txt", "one");
    write(&sub, "deep/b.txt", "two");
    write(dir.path(), "top.txt", "three");
    // `a.txt` is `sub/a.txt`, `..` leads up to the root, and `.` is the directory itself.
    let result = track(&sub, &["a.txt", "../top.txt"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(result.stdout, "tracked sub/a.txt\ntracked top.txt\n");
    let result = track(&sub.join("deep"), &["."]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(result.stdout, "tracked sub/deep/b.txt\n");
    // A path that leaves the repository is refused.
    let result = track(&sub, &["../../outside.txt"]);
    assert_eq!(result.code, 1, "{}", result.stderr);
    // Another command runs below the root as well.
    let result = run(&sub, &["status", "--actor", ACTOR]);
    assert_eq!(result.code, 0, "{}", result.stderr);
}

#[test]
fn a_repository_without_a_working_copy_commit_fails_with_code_1_and_changes_nothing() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    fs::remove_file(dir.path().join(".stemma/working-copy")).unwrap();
    let result = track(dir.path(), &["a.txt"]);
    assert_eq!(result.code, 1, "{}", result.stderr);
    assert!(
        result.stderr.contains("no working-copy commit"),
        "{}",
        result.stderr
    );
    assert_eq!(result.stdout, "");
}

#[cfg(unix)]
#[test]
fn a_name_with_a_newline_is_refused_and_named_escaped() {
    let dir = repository();
    write(dir.path(), "d/ok.txt", "x");
    write(dir.path(), "d/bad\nname", "x");
    let result = track(dir.path(), &["d"]);
    assert_eq!(result.code, 1, "{}", result.stderr);
    assert!(result.stderr.contains("bad\\nname"), "{}", result.stderr);
    assert_eq!(operations(dir.path()).len(), 1);
}

#[test]
fn there_is_no_json_form() {
    let dir = repository();
    let result = track(dir.path(), &["--json", "a.txt"]);
    assert_eq!(result.code, 2, "{}", result.stderr);
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
fn a_lost_last_entry_stops_track_and_changes_nothing() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    let first = track(dir.path(), &["a.txt"]);
    assert_eq!(first.code, 0, "{}", first.stderr);
    fs::remove_file(dir.path().join(".stemma/oplog/000000000002")).unwrap();
    write(dir.path(), "b.txt", "two");
    let before = fs::read(dir.path().join(".stemma/working-copy")).unwrap();

    let result = track(dir.path(), &["b.txt"]);

    assert_eq!(result.code, 1);
    assert!(
        result.stderr.contains("the tip file names entry 2"),
        "{}",
        result.stderr
    );
    assert_eq!(result.stdout, "");
    assert_eq!(
        fs::read(dir.path().join(".stemma/working-copy")).unwrap(),
        before
    );
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

#[test]
fn a_dot_leaves_out_what_the_exclusion_file_names_and_tracks_the_file_itself() {
    let dir = repository();
    write(dir.path(), ".stemmaignore", "*.log\ntarget/\n");
    write(dir.path(), "a.txt", "a");
    write(dir.path(), "d/x.log", "log");
    write(dir.path(), "target/debug/z", "z");

    let result = track(dir.path(), &["."]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(result.stdout, "tracked .stemmaignore\ntracked a.txt\n");
}

#[test]
fn a_path_that_is_named_and_excluded_fails_with_the_line_and_changes_nothing() {
    let dir = repository();
    write(dir.path(), ".stemmaignore", "# logs\n*.log\n");
    write(dir.path(), "d/x.log", "log");
    let entries = operations(dir.path()).len();

    let result = track(dir.path(), &["d/x.log"]);

    assert_eq!(result.code, 1, "{}", result.stdout);
    assert_eq!(result.stdout, "");
    assert!(
        result.stderr.starts_with("stemma: ")
            && result.stderr.contains(
                "`d/x.log` is excluded by line 2 of .stemmaignore, so `track` leaves it out"
            ),
        "{}",
        result.stderr
    );
    assert_eq!(operations(dir.path()).len(), entries);
}

#[test]
fn an_invalid_exclusion_file_stops_the_command_and_names_the_line() {
    let dir = repository();
    write(dir.path(), ".stemmaignore", "*.log\n!keep.log\n");
    write(dir.path(), "a.txt", "a");
    let entries = operations(dir.path()).len();

    let result = track(dir.path(), &["a.txt"]);

    assert_eq!(result.code, 1, "{}", result.stdout);
    assert!(
        result
            .stderr
            .contains(".stemmaignore: line 2: negation (`!`) is not supported"),
        "{}",
        result.stderr
    );
    assert_eq!(operations(dir.path()).len(), entries);
}

#[test]
fn an_invalid_exclusion_file_does_not_stop_a_commit() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    assert_eq!(track(dir.path(), &["a.txt"]).code, 0);
    write(dir.path(), ".stemmaignore", "!broken\n");
    write(dir.path(), "a.txt", "two");

    let result = run(
        dir.path(),
        &["commit", "-m", "Change a", "--actor", "human alice"],
    );

    assert_eq!(result.code, 0, "{}", result.stderr);
}

#[test]
fn an_exclusion_file_from_another_system_stops_the_command_and_says_why() {
    let cases: &[(&[u8], &str)] = &[
        (b"*.log\r\n", "line 1: carriage return"),
        (
            b"\xEF\xBB\xBF*.log\n",
            "the file starts with a byte order mark",
        ),
        (b"*.log\n\xFF\n", "the file is not UTF-8"),
    ];
    for (bytes, text) in cases {
        let dir = repository();
        fs::write(dir.path().join(".stemmaignore"), bytes).unwrap();
        write(dir.path(), "a.txt", "a");
        let entries = operations(dir.path()).len();

        let result = track(dir.path(), &["a.txt"]);

        assert_eq!(result.code, 1, "{bytes:?}: {}", result.stdout);
        assert_eq!(result.stdout, "");
        assert!(
            result.stderr.starts_with("stemma: .stemmaignore: ")
                && result.stderr.contains(text)
                && result.stderr.contains("nothing was tracked"),
            "{bytes:?}: {}",
            result.stderr
        );
        assert_eq!(operations(dir.path()).len(), entries);
    }
}

#[test]
fn a_directory_as_the_exclusion_file_stops_the_command() {
    let dir = repository();
    fs::create_dir(dir.path().join(".stemmaignore")).unwrap();
    write(dir.path(), "a.txt", "a");

    let result = track(dir.path(), &["a.txt"]);

    assert_eq!(result.code, 1, "{}", result.stdout);
    assert!(
        result.stderr.contains("is a directory, not a regular file"),
        "{}",
        result.stderr
    );
}
