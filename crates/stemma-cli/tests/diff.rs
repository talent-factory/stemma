//! The command `stemma diff`, run as a process, as a person or an agent would run it.

mod common;

use std::fs::{self, File};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

use common::{operations, repository, run, stemma, track_ok, write, Dir, Run, ACTOR};

/// Commits and returns the identifier of the new commit, `b3:…`.
fn commit(dir: &Path, message: &str) -> String {
    let result = run(dir, &["commit", "-m", message, "--actor", ACTOR]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    result
        .stdout
        .split_whitespace()
        .nth(1)
        .expect("the commit names its identifier")
        .to_owned()
}

fn diff(dir: &Path, operands: &[&str]) -> Run {
    let mut args = vec!["diff", "--actor", "human alice"];
    args.extend_from_slice(operands);
    run(dir, &args)
}

/// The identifier of the commit that the old side of the first file names, `b3:…`.
fn old_side(output: &str) -> String {
    output
        .lines()
        .find_map(|line| line.strip_prefix("--- "))
        .and_then(|side| side.split_whitespace().next())
        .unwrap()
        .to_owned()
}

/// A repository with one commit of `a.txt` and an edit of it in the working copy.
fn edited_repository() -> Dir {
    let dir = repository();
    write(dir.path(), "a.txt", "one\n");
    track_ok(dir.path(), "a.txt");
    commit(dir.path(), "first");
    write(dir.path(), "a.txt", "two\n");
    dir
}

/// A repository with one commit of `file` with the content `before`, and the working copy
/// with the content `after`.
fn repository_with_edit(file: &str, before: &[u8], after: &[u8]) -> Dir {
    let dir = repository();
    fs::create_dir_all(dir.path().join(file).parent().unwrap()).unwrap();
    fs::write(dir.path().join(file), before).unwrap();
    track_ok(dir.path(), file);
    commit(dir.path(), "first");
    fs::write(dir.path().join(file), after).unwrap();
    dir
}

#[test]
fn the_working_copy_is_named_by_its_snapshot_identifier_and_no_time() {
    let dir = edited_repository();

    let result = diff(dir.path(), &[]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(result.stderr, "");
    let mut lines = result.stdout.lines();
    assert_eq!(lines.next(), Some("diff a.txt"), "{}", result.stdout);
    let old = lines.next().unwrap();
    let new = lines.next().unwrap();
    assert!(old.starts_with("--- b3:"), "{old}");
    let working_copy = new
        .strip_prefix("+++ working-copy ")
        .and_then(|side| side.split_whitespace().next())
        .unwrap_or_else(|| panic!("the new side names the working copy: {new}"));
    assert!(working_copy.starts_with("b3:"), "{new}");
    assert_eq!(
        working_copy.len(),
        "b3:".len() + 64,
        "{new}: no time after the id"
    );
    assert!(result.stdout.contains("\n-one\n"), "{}", result.stdout);
    assert!(result.stdout.contains("\n+two\n"), "{}", result.stdout);
}

#[test]
fn a_commit_is_compared_with_the_working_copy_by_its_identifier() {
    let dir = edited_repository();
    let old = old_side(&diff(dir.path(), &[]).stdout);

    let result = diff(dir.path(), &[old.as_str()]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    let mut lines = result.stdout.lines();
    assert_eq!(lines.next(), Some("diff a.txt"), "{}", result.stdout);
    assert_eq!(
        lines.next(),
        Some(format!("--- {old} a.txt").as_str()),
        "{}",
        result.stdout
    );
    assert!(
        lines
            .next()
            .is_some_and(|line| line.starts_with("+++ working-copy b3:")),
        "{}",
        result.stdout
    );
}

#[test]
fn one_changed_line_is_one_hunk_with_its_header_and_lines() {
    let dir = edited_repository();

    let result = diff(dir.path(), &[]);

    let lines: Vec<&str> = result.stdout.lines().collect();
    assert_eq!(lines[0], "diff a.txt", "{}", result.stdout);
    assert!(lines[1].starts_with("--- b3:"), "{}", result.stdout);
    assert!(
        lines[2].starts_with("+++ working-copy b3:"),
        "{}",
        result.stdout
    );
    assert_eq!(
        &lines[3..],
        ["@@ -1 +1 @@", "-one", "+two"],
        "{}",
        result.stdout
    );
}

#[test]
fn an_unchanged_working_copy_shows_the_header_only() {
    let dir = edited_repository();
    write(dir.path(), "a.txt", "one\n");

    let result = diff(dir.path(), &[]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(result.stdout, "", "{}", result.stdout);
}

#[test]
fn a_change_of_the_line_ends_is_shown() {
    let dir = repository_with_edit("a.txt", b"a\nb\n", b"a\r\nb\r\n");

    let result = diff(dir.path(), &[]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(
        result.stdout.contains("@@ -1,2 +1,2 @@"),
        "{}",
        result.stdout
    );
    assert!(result.stdout.contains("-a\n"), "{}", result.stdout);
    assert!(result.stdout.contains("+a\r\n"), "{}", result.stdout);
}

#[test]
fn a_last_line_without_a_line_end_is_marked_as_such() {
    let dir = repository_with_edit("a.txt", b"one\n", b"one");

    let result = diff(dir.path(), &[]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(
        result
            .stdout
            .contains("+one\n\\ No newline at end of file\n"),
        "{}",
        result.stdout
    );
}

#[test]
fn a_change_of_bytes_that_are_not_utf8_is_shown() {
    let dir = repository_with_edit("a.txt", b"\xE9\n", b"\xE8\n");

    let result = diff(dir.path(), &[]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(result.stdout.contains("@@ -1 +1 @@"), "{}", result.stdout);
}

#[test]
fn a_binary_file_is_named_without_its_lines() {
    let dir = repository_with_edit("bin.dat", b"a\0b", b"a\0c");

    let result = diff(dir.path(), &[]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(
        result.stdout.starts_with("diff bin.dat\nBinary files b3:"),
        "{}",
        result.stdout
    );
    assert!(
        result.stdout.contains(" bin.dat and working-copy b3:")
            && result.stdout.ends_with(" bin.dat differ\n"),
        "{}",
        result.stdout
    );
    assert!(!result.stdout.contains("@@"), "{}", result.stdout);
}

#[test]
fn a_file_in_a_directory_is_named_by_its_path() {
    let dir = repository_with_edit("dir/a.txt", b"one\n", b"two\n");

    let result = diff(dir.path(), &[]);

    assert!(
        result.stdout.contains("diff dir/a.txt\n"),
        "{}",
        result.stdout
    );
}

#[test]
fn an_added_file_has_an_empty_old_range() {
    let dir = edited_repository();
    write(dir.path(), "new.txt", "fresh\n");
    track_ok(dir.path(), "new.txt");

    let result = diff(dir.path(), &[]);

    assert!(
        result
            .stdout
            .contains("diff new.txt\n--- /dev/null\n+++ working-copy b3:"),
        "{}",
        result.stdout
    );
    assert!(
        result.stdout.contains("@@ -0,0 +1 @@\n+fresh\n"),
        "{}",
        result.stdout
    );
}

#[test]
fn a_deleted_file_has_an_empty_new_range() {
    let dir = repository_with_edit("gone.txt", b"bye\n", b"bye\n");
    fs::remove_file(dir.path().join("gone.txt")).unwrap();

    let result = diff(dir.path(), &[]);

    assert!(
        result.stdout.starts_with("diff gone.txt\n--- b3:"),
        "{}",
        result.stdout
    );
    assert!(
        result
            .stdout
            .contains(" gone.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-bye\n"),
        "{}",
        result.stdout
    );
}

#[test]
fn the_first_commit_has_no_old_side() {
    let dir = repository();
    write(dir.path(), "a.txt", "x\n");
    track_ok(dir.path(), "a.txt");

    let result = diff(dir.path(), &[]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(
        result
            .stdout
            .starts_with("diff a.txt\n--- /dev/null\n+++ working-copy b3:"),
        "{}",
        result.stdout
    );
    assert!(result.stdout.contains("+x\n"), "{}", result.stdout);
}

#[test]
fn two_commits_are_compared_without_a_snapshot_of_the_working_copy() {
    let dir = edited_repository();
    let first = old_side(&diff(dir.path(), &[]).stdout);
    let second = commit(dir.path(), "second");
    write(dir.path(), "a.txt", "three\n");
    let entries = operations(dir.path()).len();

    let result = diff(dir.path(), &[first.as_str(), second.as_str()]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(
        result.stdout.starts_with(&format!(
            "diff a.txt\n--- {first} a.txt\n+++ {second} a.txt\n"
        )),
        "{}",
        result.stdout
    );
    assert_eq!(
        operations(dir.path()).len(),
        entries,
        "no snapshot was taken"
    );
}

#[test]
fn an_unknown_operand_is_an_error_that_names_it() {
    let dir = edited_repository();
    let unknown = format!("b3:{}", "0".repeat(64));

    let result = diff(dir.path(), &[unknown.as_str()]);

    assert_eq!(result.code, 1, "{}", result.stdout);
    assert!(
        result
            .stderr
            .contains(&format!("`{unknown}` is not the identifier of a commit")),
        "{}",
        result.stderr
    );
}

/// The identifiers of all objects of the store, `b3:…`, in no particular order.
fn object_ids(dir: &Path) -> Vec<String> {
    let mut ids = Vec::new();
    let objects = dir.join(".stemma").join("objects");
    for algorithm in fs::read_dir(&objects).unwrap() {
        let algorithm = algorithm.unwrap();
        let name = algorithm.file_name().into_string().unwrap();
        for fan in fs::read_dir(algorithm.path()).unwrap() {
            let fan = fan.unwrap();
            let prefix = fan.file_name().into_string().unwrap();
            for file in fs::read_dir(fan.path()).unwrap() {
                let rest = file.unwrap().file_name().into_string().unwrap();
                ids.push(format!("{name}:{prefix}{rest}"));
            }
        }
    }
    ids
}

#[test]
fn an_object_that_is_not_a_commit_is_an_unknown_commit_whatever_its_kind() {
    let dir = edited_repository();
    let ids = object_ids(dir.path());
    let (mut commits, mut others) = (0, 0);

    for id in &ids {
        let result = diff(dir.path(), &[id.as_str()]);
        if result.code == 0 {
            commits += 1;
            continue;
        }
        assert_eq!(result.code, 1, "{id}: {}", result.stderr);
        assert!(
            result
                .stderr
                .contains(&format!("`{id}` is not the identifier of a commit")),
            "{id}: {}",
            result.stderr
        );
        others += 1;
    }

    assert!(commits >= 1, "the commit is accepted");
    assert!(others >= 2, "the blobs and the tree are refused: {ids:?}");
}

#[test]
fn three_operands_are_a_usage_error() {
    let dir = edited_repository();

    let result = diff(dir.path(), &["b3:a", "b3:b", "b3:c"]);

    assert_eq!(result.code, 2, "{}", result.stderr);
}

#[test]
fn exit_code_four_reports_a_difference_only_with_the_flag() {
    let dir = edited_repository();

    let plain = diff(dir.path(), &[]);
    assert_eq!(plain.code, 0, "{}", plain.stderr);
    let flagged = diff(dir.path(), &["--exit-code"]);
    assert_eq!(flagged.code, 4, "{}", flagged.stderr);
    assert_eq!(
        flagged.stdout, plain.stdout,
        "the flag changes only the code"
    );
}

#[test]
fn exit_code_zero_when_nothing_differs_even_with_the_flag() {
    let dir = edited_repository();
    write(dir.path(), "a.txt", "one\n");

    let result = diff(dir.path(), &["--exit-code"]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(result.stdout, "");
}

#[test]
fn an_empty_added_file_is_a_difference_with_a_header_and_no_hunk() {
    let dir = edited_repository();
    write(dir.path(), "empty.txt", "");
    track_ok(dir.path(), "empty.txt");

    let result = diff(dir.path(), &["--exit-code"]);

    assert_eq!(result.code, 4, "{}", result.stderr);
    let section = result
        .stdout
        .split("diff empty.txt\n")
        .nth(1)
        .unwrap_or_else(|| panic!("{}", result.stdout));
    assert!(
        section.starts_with("--- /dev/null\n+++ working-copy b3:"),
        "{}",
        result.stdout
    );
    assert!(!section.contains("@@"), "{}", result.stdout);
}

#[test]
fn a_binary_file_names_both_sides_as_the_summary_line_does() {
    let dir = repository_with_edit("bin.dat", b"a\0b", b"a\0c");

    let result = diff(dir.path(), &["--exit-code"]);

    assert_eq!(result.code, 4, "{}", result.stderr);
    let line = result
        .stdout
        .lines()
        .find(|line| line.starts_with("Binary files "))
        .unwrap_or_else(|| panic!("{}", result.stdout));
    assert!(
        line.starts_with("Binary files b3:") && line.ends_with(" bin.dat differ"),
        "{line}"
    );
    assert!(line.contains(" and working-copy b3:"), "{line}");
}

#[test]
fn a_failure_after_the_snapshot_names_its_entry() {
    let dir = repository();
    write(dir.path(), "a.txt", "one\n");
    track_ok(dir.path(), "a.txt");
    let first = commit(dir.path(), "first");
    write(dir.path(), "a.txt", "two\n");
    // The parent commit is read after the snapshot; without its object the diff fails there.
    let hex = first.strip_prefix("b3:").unwrap();
    let object = dir
        .path()
        .join(".stemma/objects/b3")
        .join(&hex[..2])
        .join(&hex[2..]);
    assert!(object.exists(), "{}", object.display());
    fs::remove_file(&object).unwrap();

    let result = diff(dir.path(), &[]);

    assert_eq!(result.code, 1, "{}", result.stdout);
    assert!(
        result.stderr.contains("saved first as entry"),
        "{}",
        result.stderr
    );
}

#[test]
fn a_diff_written_to_a_pipe_has_no_colour_and_no_pager() {
    let dir = edited_repository();
    let result = stemma(dir.path(), &["diff", "--actor", "human alice"], |command| {
        command.env("PAGER", "echo PAGER-RAN");
    });

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(!result.stdout.contains('\x1b'), "{:?}", result.stdout);
    assert!(!result.stdout.contains("PAGER-RAN"), "{}", result.stdout);
    assert!(result.stdout.contains("-one\n+two\n"), "{}", result.stdout);
}

/// Runs `diff` with its output on a terminal, as a person sees it, and returns what the terminal
/// got. The pager is `cat` followed by a marker, so that the test sees both the colour and the
/// pager.
#[cfg(unix)]
fn diff_on_terminal(dir: &Path) -> String {
    let pty = nix::pty::openpty(None, None).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_stemma"));
    command
        .current_dir(dir)
        .env_remove("STEMMA_ACTOR")
        .env("PAGER", "cat && echo PAGER-DONE")
        .args(["diff", "--actor", "human alice"])
        .stdout(Stdio::from(pty.slave.try_clone().unwrap()))
        .stderr(Stdio::from(pty.slave));
    let mut child = command.spawn().unwrap();
    // The command holds the terminal until it is dropped, and the read ends when it is gone.
    drop(command);
    let mut output = String::new();
    let mut master = File::from(pty.master);
    let mut buffer = [0; 1024];
    while let Ok(read) = master.read(&mut buffer) {
        if read == 0 {
            break;
        }
        output.push_str(&String::from_utf8_lossy(&buffer[..read]));
    }
    assert!(child.wait().unwrap().success());
    output
}

#[cfg(unix)]
#[test]
fn a_diff_on_a_terminal_is_coloured_and_shown_by_the_pager() {
    let dir = edited_repository();

    let output = diff_on_terminal(dir.path());

    assert!(output.contains("\x1b[31m-one\x1b[m"), "{output:?}");
    assert!(output.contains("\x1b[32m+two\x1b[m"), "{output:?}");
    assert!(output.contains("\x1b[36m@@ -1 +1 @@\x1b[m"), "{output:?}");
    assert!(output.contains("PAGER-DONE"), "{output:?}");
}
