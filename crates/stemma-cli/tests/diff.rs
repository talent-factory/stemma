//! The command `stemma diff`, run as a process, as a person or an agent would run it.

mod common;

use std::fs;
#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::io::{Read, Write};
use std::path::Path;
#[cfg(unix)]
use std::process::{Child, Command, Stdio};
#[cfg(unix)]
use std::time::{Duration, Instant};

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
    let mut args = vec!["diff", "--actor", "human daniel"];
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
fn a_missing_object_after_the_snapshot_is_damage_and_names_its_entry() {
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

    assert_eq!(result.code, 5, "{}", result.stdout);
    assert!(
        result.stderr.contains("saved first as entry"),
        "{}",
        result.stderr
    );
}

#[test]
fn a_diff_written_to_a_pipe_has_no_colour_and_no_pager() {
    let dir = edited_repository();
    let result = stemma(
        dir.path(),
        &["diff", "--actor", "human daniel"],
        |command| {
            command.env("PAGER", "echo PAGER-RAN");
        },
    );

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(!result.stdout.contains('\x1b'), "{:?}", result.stdout);
    assert!(!result.stdout.contains("PAGER-RAN"), "{}", result.stdout);
    assert!(result.stdout.contains("-one\n+two\n"), "{}", result.stdout);
}

/// How long a test waits for the terminal before it fails, so that a command that does not do what
/// the test expects makes the test fail and does not hang it.
#[cfg(unix)]
const TERMINAL_WAIT: Duration = Duration::from_secs(20);

/// A command that runs on a terminal. It is killed and waited for when the test ends, also when an
/// assertion fails first, so that no command is left running.
#[cfg(unix)]
struct OnTerminal(Child);

#[cfg(unix)]
impl std::ops::Deref for OnTerminal {
    type Target = Child;

    fn deref(&self) -> &Child {
        &self.0
    }
}

#[cfg(unix)]
impl std::ops::DerefMut for OnTerminal {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

#[cfg(unix)]
impl Drop for OnTerminal {
    fn drop(&mut self) {
        // The command may have ended already, which is the usual case.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Waits until the terminal has something to read, and tells whether it has before `deadline`. A
/// terminal that the command has left counts as readable: the read then says so.
#[cfg(unix)]
fn wait_readable(master: &File, deadline: Instant) -> bool {
    use nix::errno::Errno;
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use std::os::fd::AsFd;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let timeout = PollTimeout::try_from(left).unwrap_or(PollTimeout::MAX);
        let mut fds = [PollFd::new(master.as_fd(), PollFlags::POLLIN)];
        match poll(&mut fds, timeout) {
            Ok(ready) => return ready > 0,
            Err(Errno::EINTR) => {}
            Err(error) => panic!("poll on the terminal: {error}"),
        }
    }
}

/// Starts `diff` with its output and its errors on a terminal, as a person sees it. `setup` sets
/// the environment, and `LESS` is not inherited. The terminal is returned with the child.
#[cfg(unix)]
fn spawn_on_terminal(dir: &Path, setup: impl FnOnce(&mut Command)) -> (OnTerminal, File) {
    // A terminal of 80 by 24, as a person has one: with no size, the interface would draw nothing.
    let size = nix::pty::Winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = nix::pty::openpty(Some(&size), None).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_stemma"));
    command
        .current_dir(dir)
        .env_remove("STEMMA_ACTOR")
        .env_remove("LESS")
        .args(["diff", "--actor", "human daniel"])
        // The input is the terminal too, so that the interface of `--tui` reads its keys there and
        // not from the controlling terminal of the test run, which may not exist.
        .stdin(Stdio::from(pty.slave.try_clone().unwrap()))
        .stdout(Stdio::from(pty.slave.try_clone().unwrap()))
        .stderr(Stdio::from(pty.slave));
    setup(&mut command);
    let child = command.spawn().unwrap();
    // The command holds the terminal until it is dropped, and the read ends when it is gone.
    drop(command);
    (OnTerminal(child), File::from(pty.master))
}

/// Reads what the terminal got until `marker` has appeared.
#[cfg(unix)]
fn read_until(master: &mut File, marker: &str) -> String {
    let deadline = Instant::now() + TERMINAL_WAIT;
    let mut output = String::new();
    let mut buffer = [0; 1024];
    while !output.contains(marker) {
        assert!(
            wait_readable(master, deadline),
            "no {marker} on the terminal within {TERMINAL_WAIT:?}: {output:?}"
        );
        match master.read(&mut buffer) {
            Ok(read) if read > 0 => output.push_str(&String::from_utf8_lossy(&buffer[..read])),
            other => panic!("the terminal closed before {marker} ({other:?}): {output:?}"),
        }
    }
    output
}

/// Reads what the terminal got until the command has gone.
#[cfg(unix)]
fn read_terminal(mut master: File) -> String {
    let deadline = Instant::now() + TERMINAL_WAIT;
    let mut output = String::new();
    let mut buffer = [0; 1024];
    loop {
        assert!(
            wait_readable(&master, deadline),
            "the command did not end within {TERMINAL_WAIT:?}: {output:?}"
        );
        match master.read(&mut buffer) {
            Ok(read) if read > 0 => output.push_str(&String::from_utf8_lossy(&buffer[..read])),
            _ => break,
        }
    }
    // A terminal ends a line with CR LF.
    output.replace("\r\n", "\n")
}

/// Runs `diff` on a terminal with the environment of `setup`, and returns what the terminal got.
/// The command must end with 0.
#[cfg(unix)]
fn diff_on_terminal(dir: &Path, setup: impl FnOnce(&mut Command)) -> String {
    let (mut child, master) = spawn_on_terminal(dir, setup);
    let output = read_terminal(master);
    assert!(child.wait().unwrap().success(), "{output:?}");
    output
}

/// The text without its colour escape sequences.
#[cfg(unix)]
fn without_colour(text: &str) -> String {
    let mut plain = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // An escape sequence ends with a letter.
            chars.by_ref().find(char::is_ascii_alphabetic);
        } else {
            plain.push(c);
        }
    }
    plain
}

#[cfg(unix)]
#[test]
fn a_diff_on_a_terminal_is_coloured_and_shown_by_the_pager() {
    let dir = edited_repository();

    // The pager is `cat` followed by a marker, so that the test sees both colour and pager.
    let output = diff_on_terminal(dir.path(), |command| {
        command.env("PAGER", "cat && echo PAGER-DONE");
    });

    assert!(output.contains("\x1b[31m-one\x1b[m"), "{output:?}");
    assert!(output.contains("\x1b[32m+two\x1b[m"), "{output:?}");
    assert!(output.contains("\x1b[36m@@ -1 +1 @@\x1b[m"), "{output:?}");
    assert!(output.contains("\x1b[1m--- "), "{output:?}");
    // The pager ran after the text.
    assert!(output.find("-one").unwrap() < output.find("PAGER-DONE").unwrap());
}

#[cfg(unix)]
#[test]
fn the_colour_only_adds_escape_sequences_to_the_plain_text() {
    let dir = edited_repository();
    let plain = diff(dir.path(), &[]).stdout;

    let output = diff_on_terminal(dir.path(), |command| {
        command.env("PAGER", "cat");
    });

    assert_eq!(without_colour(&output), plain);
}

#[cfg(unix)]
#[test]
fn a_pager_that_cannot_run_leaves_the_diff_on_the_terminal() {
    let dir = edited_repository();

    let output = diff_on_terminal(dir.path(), |command| {
        command.env("PAGER", "/nonexistent/pager");
    });

    assert!(output.contains("-one"), "{output:?}");
    assert!(output.contains("+two"), "{output:?}");
    assert!(output.contains("could not be run"), "{output:?}");
}

/// A directory with an executable `less` that prints a marker after the text, put in front of
/// `PATH`, so that the test sees whether the default pager was called.
#[cfg(unix)]
fn fake_less(dir: &Path) -> String {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("bin");
    fs::create_dir(&bin).unwrap();
    let less = bin.join("less");
    fs::write(&less, "#!/bin/sh\ncat\necho \"LESS-RAN $LESS\"\n").unwrap();
    fs::set_permissions(&less, fs::Permissions::from_mode(0o755)).unwrap();
    format!("{}:/usr/bin:/bin", bin.display())
}

#[cfg(unix)]
#[test]
fn without_a_pager_the_default_is_less_with_the_options_frx() {
    for pager in [None, Some("")] {
        let dir = edited_repository();
        let path = fake_less(dir.path());

        let output = diff_on_terminal(dir.path(), |command| {
            command.env("PATH", &path).env_remove("PAGER");
            if let Some(pager) = pager {
                command.env("PAGER", pager);
            }
        });

        assert!(output.contains("LESS-RAN FRX"), "{pager:?}: {output:?}");
    }
}

#[cfg(unix)]
#[test]
fn the_less_options_of_the_user_are_kept() {
    let dir = edited_repository();

    let output = diff_on_terminal(dir.path(), |command| {
        command.env("PAGER", "echo LESS=$LESS").env("LESS", "-i");
    });

    assert!(output.contains("LESS=-i"), "{output:?}");
}

#[cfg(unix)]
#[test]
fn a_pager_that_quits_early_or_fails_is_not_an_error() {
    let dir = edited_repository();

    // `true` does not read the text, and the exit code of `exit 3` is not the one of the diff.
    for pager in ["true", "cat >/dev/null; exit 3"] {
        let output = diff_on_terminal(dir.path(), |command| {
            command.env("PAGER", pager);
        });

        assert!(!output.contains("could not be run"), "{pager}: {output:?}");
    }
}

#[cfg(unix)]
#[test]
fn an_interrupt_waits_for_the_pager_and_does_not_end_the_diff() {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    let dir = edited_repository();
    let (mut child, mut master) = spawn_on_terminal(dir.path(), |command| {
        command.env("PAGER", "echo PAGER-READY; sleep 1; cat");
    });
    // The signal arrives when the pager has started; the process ends only when the pager does.
    let mut output = read_until(&mut master, "PAGER-READY");
    let process = Pid::from_raw(i32::try_from(child.id()).unwrap());
    kill(process, Signal::SIGINT).unwrap();
    output.push_str(&read_terminal(master));

    let status = child.wait().unwrap();
    assert!(status.success(), "{status:?} {output:?}");
    assert!(output.contains("-one"), "{output:?}");
}

#[cfg(unix)]
#[test]
fn a_termination_signal_ends_the_pager_and_the_diff_at_once() {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    for (signal, code) in [(Signal::SIGTERM, 143), (Signal::SIGHUP, 129)] {
        let dir = edited_repository();
        let marker = dir.path().join("pager-pid");
        let (mut child, mut master) = spawn_on_terminal(dir.path(), |command| {
            command.env(
                "PAGER",
                format!(
                    "echo $$ > '{}'; echo PAGER-READY; exec sleep 30",
                    marker.display()
                ),
            );
        });
        read_until(&mut master, "PAGER-READY");
        let pager = Pid::from_raw(fs::read_to_string(&marker).unwrap().trim().parse().unwrap());
        let process = Pid::from_raw(i32::try_from(child.id()).unwrap());
        let start = std::time::Instant::now();
        kill(process, signal).unwrap();

        let status = child.wait().unwrap();
        assert_eq!(status.code(), Some(code), "{signal:?} {status:?}");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "{signal:?}"
        );
        // The pager is gone too. Its process may take a moment to end, and an orphan that
        // nobody has reaped yet (a zombie) counts as gone.
        let gone = (0..100).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(50));
            let state = Command::new("ps")
                .args(["-o", "stat=", "-p", &pager.to_string()])
                .output()
                .expect("the test needs the `ps` command");
            let state = String::from_utf8_lossy(&state.stdout);
            state.trim().is_empty() || state.trim().starts_with('Z')
        });
        assert!(gone, "{signal:?}: the pager is still running");
    }
}

#[cfg(unix)]
#[test]
fn a_pager_that_is_not_executable_leaves_the_diff_on_the_terminal() {
    use std::os::unix::fs::PermissionsExt;
    let dir = edited_repository();
    let pager = dir.path().join("pager");
    fs::write(&pager, "#!/bin/sh\ncat\n").unwrap();
    fs::set_permissions(&pager, fs::Permissions::from_mode(0o644)).unwrap();

    let output = diff_on_terminal(dir.path(), |command| {
        command.env("PAGER", &pager);
    });

    assert!(output.contains("+two"), "{output:?}");
    assert!(output.contains("could not be run"), "{output:?}");
}

/// Runs `diff --tui` on a terminal, ends it with `end` once the interface shows the files, and
/// returns what the terminal got together with the exit code. `end` is a key, or a signal.
#[cfg(unix)]
fn tui_on_terminal(dir: &Path, extra: &[&str], end: End) -> (String, Option<i32>) {
    let (mut child, mut master) = spawn_on_terminal(dir, |command| {
        command.args(extra);
    });
    // The interface has turned on raw mode when it has drawn its first frame.
    let mut output = read_until(&mut master, "Files");
    match end {
        End::Key(key) => master.write_all(key).unwrap(),
        End::Signal(signal) => {
            let pid = nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap());
            nix::sys::signal::kill(pid, signal).unwrap();
        }
    }
    output.push_str(&read_terminal(master));
    (output, child.wait().unwrap().code())
}

/// How a test ends the interface.
#[cfg(unix)]
enum End {
    Key(&'static [u8]),
    Signal(nix::sys::signal::Signal),
}

/// Asserts that the interface entered the alternate screen first and left it again, and that
/// nothing but the interface was written before, and the cursor is back.
#[cfg(unix)]
fn assert_terminal_given_back(output: &str) {
    let entered = output.find("\x1b[?1049h");
    let left = output.find("\x1b[?1049l");
    assert_eq!(entered, Some(0), "{output:?}");
    assert!(left > entered, "{output:?}");
    // The cursor, which the interface hides while it draws, is shown again.
    assert!(output.contains("\x1b[?25h"), "{output:?}");
}

#[cfg(unix)]
#[test]
fn tui_shows_the_diff_and_ends_with_q() {
    let dir = edited_repository();
    let (output, code) = tui_on_terminal(dir.path(), &["--tui"], End::Key(b"q"));
    assert_eq!(code, Some(0), "{output:?}");
    // The file list and the lines of the file reached the terminal.
    assert!(output.contains("a.txt"), "{output:?}");
    assert!(output.contains("-one"), "{output:?}");
    assert!(output.contains("+two"), "{output:?}");
    assert_terminal_given_back(&output);
}

#[cfg(unix)]
#[test]
fn tui_ends_with_ctrl_c_as_a_key_and_not_as_a_signal() {
    let dir = edited_repository();
    // Raw mode delivers the typed Ctrl-C to the interface: an exit code of 130 would mean that a
    // signal ended the command.
    let (output, code) = tui_on_terminal(dir.path(), &["--tui"], End::Key(b"\x03"));
    assert_eq!(code, Some(0), "{output:?}");
    assert_terminal_given_back(&output);
}

#[cfg(unix)]
#[test]
fn tui_gives_the_terminal_back_when_a_signal_ends_the_command() {
    use nix::sys::signal::Signal;
    for (signal, code) in [
        (Signal::SIGTERM, 143),
        (Signal::SIGHUP, 129),
        (Signal::SIGINT, 130),
    ] {
        let dir = edited_repository();
        let (output, exit) = tui_on_terminal(dir.path(), &["--tui"], End::Signal(signal));
        assert_eq!(exit, Some(code), "{signal:?}: {output:?}");
        assert_terminal_given_back(&output);
    }
}

#[cfg(unix)]
#[test]
fn tui_with_exit_code_ends_with_4_when_a_file_differs() {
    let dir = edited_repository();
    let (output, code) = tui_on_terminal(dir.path(), &["--tui", "--exit-code"], End::Key(b"q"));
    assert_eq!(code, Some(4), "{output:?}");
}

#[cfg(unix)]
#[test]
fn tui_does_not_start_when_no_file_differs() {
    let dir = repository();
    write(dir.path(), "a.txt", "one\n");
    track_ok(dir.path(), "a.txt");
    commit(dir.path(), "first");

    // No key is pressed: a command that started the interface would wait for one.
    let (mut child, master) = spawn_on_terminal(dir.path(), |command| {
        command.args(["--tui", "--exit-code"]);
    });
    let output = read_terminal(master);
    assert_eq!(child.wait().unwrap().code(), Some(0), "{output:?}");
    assert!(!output.contains("\x1b[?1049h"), "{output:?}");
}

#[test]
fn tui_is_refused_without_a_terminal_and_saves_nothing() {
    let dir = edited_repository();
    let before = operations(dir.path());
    let result = diff(dir.path(), &["--tui"]);
    assert_eq!(result.code, 1, "{}", result.stderr);
    assert!(result.stderr.contains("--tui"), "{}", result.stderr);
    assert_eq!(result.stdout, "");
    assert_eq!(operations(dir.path()), before);
}

/// A repository with `a.txt`, `dir/b.txt` and `dir/sub/c.txt` committed, and all three edited.
fn three_edited_files() -> Dir {
    let dir = repository();
    for file in ["a.txt", "dir/b.txt", "dir/sub/c.txt"] {
        write(dir.path(), file, "one\n");
        track_ok(dir.path(), file);
    }
    commit(dir.path(), "first");
    for file in ["a.txt", "dir/b.txt", "dir/sub/c.txt"] {
        write(dir.path(), file, "two\n");
    }
    dir
}

/// The paths of the files that a diff shows, in its order.
fn shown(output: &str) -> Vec<&str> {
    output
        .lines()
        .filter_map(|line| line.strip_prefix("diff "))
        .collect()
}

#[test]
fn a_file_limits_the_diff_to_that_file() {
    let dir = three_edited_files();

    let result = diff(dir.path(), &["dir/b.txt"]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(shown(&result.stdout), ["dir/b.txt"], "{}", result.stdout);
}

#[test]
fn a_directory_shows_everything_below_it() {
    let dir = three_edited_files();

    for operand in ["dir", "dir/", "./dir"] {
        let result = diff(dir.path(), &[operand]);

        assert_eq!(result.code, 0, "{operand}: {}", result.stderr);
        assert_eq!(
            shown(&result.stdout),
            ["dir/b.txt", "dir/sub/c.txt"],
            "{operand}"
        );
    }
}

#[test]
fn a_name_that_only_begins_like_a_directory_does_not_match_it() {
    let dir = repository();
    write(dir.path(), "dir/a.txt", "one\n");
    write(dir.path(), "dir2/b.txt", "one\n");
    track_ok(dir.path(), "dir");
    track_ok(dir.path(), "dir2");
    commit(dir.path(), "first");
    write(dir.path(), "dir/a.txt", "two\n");
    write(dir.path(), "dir2/b.txt", "two\n");

    let result = diff(dir.path(), &["dir"]);

    assert_eq!(shown(&result.stdout), ["dir/a.txt"], "{}", result.stdout);
}

#[test]
fn several_paths_show_the_files_of_each_in_the_order_of_the_diff() {
    let dir = three_edited_files();

    let result = diff(dir.path(), &["dir/sub", "a.txt"]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(
        shown(&result.stdout),
        ["a.txt", "dir/sub/c.txt"],
        "{}",
        result.stdout
    );
}

#[test]
fn the_dot_selects_every_file() {
    let dir = three_edited_files();

    let result = diff(dir.path(), &["."]);

    assert_eq!(
        shown(&result.stdout),
        ["a.txt", "dir/b.txt", "dir/sub/c.txt"],
        "{}",
        result.stdout
    );
}

#[test]
fn the_dot_from_a_subdirectory_selects_that_directory_only() {
    let dir = three_edited_files();

    let result = diff(&dir.path().join("dir"), &["."]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(
        shown(&result.stdout),
        ["dir/b.txt", "dir/sub/c.txt"],
        "{}",
        result.stdout
    );
}

#[test]
fn a_path_above_the_root_from_a_subdirectory_is_refused() {
    let dir = three_edited_files();

    let result = diff(&dir.path().join("dir"), &["../../outside"]);

    assert_eq!(result.code, 1, "{}", result.stdout);
    assert!(result.stderr.contains("cannot limit"), "{}", result.stderr);
}

#[test]
fn two_commits_and_a_path_compare_the_commits_and_ignore_the_working_copy() {
    let dir = repository();
    for file in ["a.txt", "b.txt"] {
        write(dir.path(), file, "one\n");
        track_ok(dir.path(), file);
    }
    let first = commit(dir.path(), "first");
    write(dir.path(), "a.txt", "two\n");
    write(dir.path(), "b.txt", "two\n");
    let second = commit(dir.path(), "second");
    write(dir.path(), "a.txt", "three\n");

    let result = diff(dir.path(), &[first.as_str(), second.as_str(), "b.txt"]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(shown(&result.stdout), ["b.txt"], "{}", result.stdout);
    assert!(result.stdout.contains("\n+two\n"), "{}", result.stdout);
    assert!(!result.stdout.contains("three"), "{}", result.stdout);
}

#[test]
fn a_commit_that_is_not_the_parent_limits_the_comparison_with_the_working_copy() {
    let dir = repository();
    for file in ["a.txt", "b.txt"] {
        write(dir.path(), file, "one\n");
        track_ok(dir.path(), file);
    }
    let first = commit(dir.path(), "first");
    write(dir.path(), "a.txt", "two\n");
    commit(dir.path(), "second");
    write(dir.path(), "b.txt", "two\n");

    // Against the first commit `a.txt` differs too; against the parent only `b.txt` does.
    let all = diff(dir.path(), &[first.as_str()]);
    let one = diff(dir.path(), &[first.as_str(), "a.txt"]);

    assert_eq!(shown(&all.stdout), ["a.txt", "b.txt"], "{}", all.stdout);
    assert_eq!(shown(&one.stdout), ["a.txt"], "{}", one.stdout);
}

#[test]
fn a_directory_that_is_gone_is_selected_by_its_path() {
    let dir = three_edited_files();
    fs::remove_dir_all(dir.path().join("dir")).unwrap();

    let result = diff(dir.path(), &["dir"]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(
        shown(&result.stdout),
        ["dir/b.txt", "dir/sub/c.txt"],
        "{}",
        result.stdout
    );
}

#[test]
fn a_missing_or_invalid_path_ends_with_1_also_with_the_exit_code_option() {
    let dir = three_edited_files();

    for operand in ["typo.txt", "../outside"] {
        let result = diff(dir.path(), &["--exit-code", operand]);

        assert_eq!(result.code, 1, "{operand}: {}", result.stdout);
        assert_eq!(result.stdout, "", "{operand}");
    }
}

#[test]
fn a_mistyped_identifier_is_named_as_a_commit_and_not_as_a_path() {
    let dir = three_edited_files();

    let result = diff(dir.path(), &["B3:abc"]);

    assert_eq!(result.code, 1, "{}", result.stdout);
    assert!(
        result.stderr.contains("not the identifier of a commit"),
        "{}",
        result.stderr
    );
}

#[test]
fn paths_follow_a_commit_with_or_without_the_separator() {
    let dir = three_edited_files();
    let old = old_side(&diff(dir.path(), &[]).stdout);

    for operands in [
        vec![old.as_str(), "a.txt"],
        vec![old.as_str(), "--", "a.txt"],
        vec!["--", "a.txt"],
    ] {
        let result = diff(dir.path(), &operands);

        assert_eq!(result.code, 0, "{operands:?}: {}", result.stderr);
        assert_eq!(shown(&result.stdout), ["a.txt"], "{operands:?}");
    }
}

// A colon is not allowed in the name of a file on Windows.
#[cfg(unix)]
#[test]
fn a_path_that_looks_like_an_identifier_needs_the_separator() {
    let dir = three_edited_files();
    let name = format!("b3:{}", "0".repeat(64));
    write(dir.path(), &name, "one\n");
    track_ok(dir.path(), &format!("./{name}"));
    commit(dir.path(), "named like an identifier");
    write(dir.path(), &name, "two\n");

    let without = diff(dir.path(), &[name.as_str()]);
    let with = diff(dir.path(), &["--", name.as_str()]);

    // Without `--` it is a commit, and no commit has this identifier.
    assert_eq!(without.code, 1, "{}", without.stdout);
    assert!(without.stderr.contains("not the identifier of a commit"));
    assert_eq!(with.code, 0, "{}", with.stderr);
    assert_eq!(shown(&with.stdout), [name.as_str()], "{}", with.stdout);
}

#[test]
fn an_added_and_a_deleted_file_are_selected_by_their_path() {
    let dir = repository();
    write(dir.path(), "gone.txt", "one\n");
    track_ok(dir.path(), "gone.txt");
    commit(dir.path(), "first");
    fs::remove_file(dir.path().join("gone.txt")).unwrap();
    write(dir.path(), "new.txt", "one\n");
    track_ok(dir.path(), "new.txt");

    let deleted = diff(dir.path(), &["gone.txt"]);
    let added = diff(dir.path(), &["new.txt"]);

    assert_eq!(deleted.code, 0, "{}", deleted.stderr);
    assert_eq!(shown(&deleted.stdout), ["gone.txt"]);
    assert_eq!(added.code, 0, "{}", added.stderr);
    assert_eq!(shown(&added.stdout), ["new.txt"]);
}

#[test]
fn a_path_on_neither_side_is_an_error_and_shows_nothing() {
    let dir = three_edited_files();

    let result = diff(dir.path(), &["a.txt", "typo.txt"]);

    assert_eq!(result.code, 1, "{}", result.stdout);
    assert_eq!(result.stdout, "");
    assert!(result.stderr.contains("`typo.txt`"), "{}", result.stderr);
}

#[test]
fn a_file_that_is_not_tracked_is_on_neither_side() {
    let dir = three_edited_files();
    write(dir.path(), "loose.txt", "x\n");

    let result = diff(dir.path(), &["loose.txt"]);

    assert_eq!(result.code, 1, "{}", result.stdout);
    assert!(result.stderr.contains("not tracked"), "{}", result.stderr);
}

#[test]
fn a_path_that_leaves_the_repository_is_refused_before_anything_is_saved() {
    let dir = three_edited_files();
    let before = operations(dir.path());

    for operand in ["../outside", "a/../b", "/etc/passwd", ""] {
        let result = diff(dir.path(), &["--", operand]);

        assert_eq!(result.code, 1, "{operand:?}: {}", result.stdout);
        assert!(
            result.stderr.contains("cannot limit"),
            "{operand:?}: {}",
            result.stderr
        );
    }
    assert_eq!(operations(dir.path()), before);
}

#[test]
fn a_path_is_relative_to_the_directory_of_the_command() {
    let dir = three_edited_files();
    let sub = dir.path().join("dir");

    let result = diff(&sub, &["sub", "../a.txt"]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(
        shown(&result.stdout),
        ["a.txt", "dir/sub/c.txt"],
        "{}",
        result.stdout
    );
}

#[test]
fn the_exit_code_4_looks_only_at_the_selected_files() {
    let dir = repository();
    write(dir.path(), "a.txt", "one\n");
    write(dir.path(), "b.txt", "one\n");
    track_ok(dir.path(), "a.txt");
    track_ok(dir.path(), "b.txt");
    commit(dir.path(), "first");
    write(dir.path(), "a.txt", "two\n");

    let differs = diff(dir.path(), &["--exit-code", "a.txt"]);
    let equal = diff(dir.path(), &["--exit-code", "b.txt"]);

    assert_eq!(differs.code, 4, "{}", differs.stderr);
    assert_eq!(equal.code, 0, "{}", equal.stderr);
    assert_eq!(equal.stdout, "");
}

#[test]
fn more_than_two_commits_before_the_separator_are_a_usage_error() {
    let dir = three_edited_files();
    let id = format!("b3:{}", "0".repeat(64));

    let result = diff(dir.path(), &[&id, &id, &id, "--", "a.txt"]);

    assert_eq!(result.code, 2, "{}", result.stderr);
}

#[test]
fn the_usage_error_for_too_many_commits_comes_before_the_actor_and_the_repository() {
    // No actor and no repository: the usage error is still the first.
    let dir = Dir::new();
    let id = format!("b3:{}", "0".repeat(64));

    let result = run(dir.path(), &["diff", &id, &id, &id]);

    assert_eq!(result.code, 2, "{}", result.stderr);
    assert!(
        result.stderr.contains("at most two commits"),
        "{}",
        result.stderr
    );
}

#[test]
fn a_group_of_inserted_lines_takes_the_place_the_indent_heuristic_chooses() {
    let dir = repository_with_edit("a.txt", b"{\n\n", b"{\nx\n\n{\n\n{\n");

    let result = diff(dir.path(), &[]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    let body: Vec<&str> = result
        .stdout
        .lines()
        .skip_while(|l| !l.starts_with("@@"))
        .collect();
    assert_eq!(
        body[1..],
        ["+{", "+x", "+", " {", " ", "+{"],
        "{}",
        result.stdout
    );
}

#[test]
fn the_deletions_of_a_group_come_before_its_insertions() {
    let dir = repository_with_edit("a.txt", b"a\nx\nb\n", b"a\ny\nz\nb\n");

    let result = diff(dir.path(), &[]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    let body: Vec<&str> = result
        .stdout
        .lines()
        .skip_while(|l| !l.starts_with("@@"))
        .collect();
    assert_eq!(
        body[1..],
        [" a", "-x", "+y", "+z", " b"],
        "{}",
        result.stdout
    );
}
