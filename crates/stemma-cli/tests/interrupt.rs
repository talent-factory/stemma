//! An interrupted command: Ctrl-C removes the lock of the references that the command holds
//! (ADR-014), and a command that is killed leaves a lock that the next command takes over
//! (ADR-008), so that the next command runs in both cases. Ctrl-C also removes the temporary file
//! of a restore that is writing a file (ADR-013).
#![cfg(unix)]

mod common;

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use stemma_core::repository::Repository;

use common::{ok, Dir, ACTOR};

/// An unstarted `stemma` in `dir` without `STEMMA_ACTOR` and with its output discarded. Unlike
/// `common::stemma`, which runs the process to its end, it returns the `Command`, so that the test
/// can spawn the process and interrupt it.
fn stemma(dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_stemma"));
    command
        .current_dir(dir)
        .env_remove("STEMMA_ACTOR")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

#[test]
fn ctrl_c_during_a_snapshot_removes_the_lock_and_ends_with_130() {
    let dir = Dir::new();
    let status = stemma(dir.path(), &["init", "--actor", ACTOR])
        .status()
        .unwrap();
    assert!(status.success());
    // The first `track` flushes every object, so it holds the lock long enough to be interrupted.
    for number in 0..500 {
        fs::write(
            dir.path().join(format!("f{number}.txt")),
            number.to_string(),
        )
        .unwrap();
    }
    let lock = dir.path().join(".stemma").join("refs.lock");
    let mut child = stemma(dir.path(), &["track", "--actor", ACTOR, "."])
        .spawn()
        .unwrap();
    let start = Instant::now();
    while !lock.exists() && start.elapsed() < Duration::from_secs(30) {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    if !lock.exists() {
        let _ = child.kill();
        let _ = child.wait();
        eprintln!("skipped: the command ended before its lock was seen");
        return;
    }
    let killed = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let status = child.wait().unwrap();
    if status.success() {
        // The command ended between the check of the lock and the signal.
        eprintln!("skipped: the command ended before the signal");
        return;
    }
    assert_eq!(status.code(), Some(130), "{status:?}");
    assert!(!lock.exists(), "the interrupted command removed its lock");
    let status = stemma(dir.path(), &["status", "--actor", ACTOR])
        .status()
        .unwrap();
    assert!(status.success(), "the next command runs");
}

#[test]
fn a_command_that_is_killed_leaves_a_lock_that_the_next_command_takes_over() {
    let dir = Dir::new();
    let status = stemma(dir.path(), &["init", "--actor", ACTOR])
        .status()
        .unwrap();
    assert!(status.success());
    for number in 0..500 {
        fs::write(
            dir.path().join(format!("f{number}.txt")),
            number.to_string(),
        )
        .unwrap();
    }
    let lock = dir.path().join(".stemma").join("refs.lock");
    // The lock is made empty and its token written after it, so the test waits for the token.
    let written =
        || fs::read_to_string(&lock).is_ok_and(|text| text.starts_with("stemma-lock 1\n"));
    let mut child = stemma(dir.path(), &["track", "--actor", ACTOR, "."])
        .spawn()
        .unwrap();
    let start = Instant::now();
    while !written() && start.elapsed() < Duration::from_secs(30) {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    if !written() {
        let _ = child.kill();
        let _ = child.wait();
        eprintln!("skipped: the command ended before its lock was seen");
        return;
    }
    // SIGKILL: no handler runs, so the lock file stays, and the operating system releases the
    // guard.
    child.kill().unwrap();
    let status = child.wait().unwrap();
    // The command may have ended, or removed its lock and not ended yet, before the signal.
    if status.success() || !lock.exists() {
        eprintln!("skipped: the command released its lock before the signal");
        return;
    }
    let left = fs::read_to_string(&lock).unwrap();
    assert!(left.starts_with("stemma-lock 1\nhost "), "{left:?}");
    let status = stemma(dir.path(), &["status", "--actor", ACTOR])
        .status()
        .unwrap();
    assert!(status.success(), "the next command takes the lock over");
    assert!(!lock.exists());
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

/// The temporary files of a checkout below `dir`, outside `.stemma`.
fn temporary_files(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            // An entry that the restore renamed in the meantime is gone.
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if name != ".stemma" {
                    pending.push(entry.path());
                }
            } else if name.starts_with(".stemma-checkout-") {
                found.push(entry.path().display().to_string());
            }
        }
    }
    found
}

/// Interrupts a restore of many files while it writes one, and checks what it leaves. Returns
/// why it could not interrupt the restore while it was writing, which the caller tries again.
fn interrupt_a_restore() -> Result<(), &'static str> {
    let dir = Dir::new();
    ok(dir.path(), &["init", "--actor", ACTOR]);
    // A restore that brings back many files writes long enough to be interrupted, and it spends
    // much of that time in a temporary file, which it flushes.
    let files: Vec<String> = (0..500).map(|number| format!("f{number}.txt")).collect();
    for name in &files {
        fs::write(dir.path().join(name), name).unwrap();
    }
    ok(dir.path(), &["track", "--actor", ACTOR, "."]);
    ok(dir.path(), &["commit", "-m", "files", "--actor", ACTOR]);
    for name in &files {
        fs::remove_file(dir.path().join(name)).unwrap();
    }
    let before = numbers(dir.path()).len();
    ok(dir.path(), &["commit", "-m", "none", "--actor", ACTOR]);
    // The snapshot that saw the files removed.
    let removal = numbers(dir.path())[before].to_string();
    let lock = dir.path().join(".stemma").join("refs.lock");
    let mut child = stemma(dir.path(), &["op", "restore", &removal, "--actor", ACTOR])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The signal is sent once a temporary file was seen, so that it likely finds one.
    let writing = || !temporary_files(dir.path()).is_empty();
    let start = Instant::now();
    while !writing() {
        if child.try_wait().unwrap().is_some() {
            return Err("the restore ended before a temporary file was seen");
        }
        if start.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("no temporary file was seen in 30 seconds");
        }
    }
    // Directly, not through a `kill` process, which would take longer than one write.
    let process = Pid::from_raw(i32::try_from(child.id()).unwrap());
    signal::kill(process, Signal::SIGINT).unwrap();
    let output = child.wait_with_output().unwrap();
    let restored = files
        .iter()
        .filter(|name| dir.path().join(name).exists())
        .count();
    if output.status.success() || restored == files.len() {
        return Err("the restore had written every file before the signal");
    }
    assert_eq!(output.status.code(), Some(130), "{:?}", output.status);
    assert_eq!(temporary_files(dir.path()), Vec::<String>::new());
    assert!(!lock.exists(), "the interrupted restore removed its lock");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "",
        "nothing stayed, so nothing is named"
    );
    Ok(())
}

#[test]
fn ctrl_c_during_a_restore_removes_the_temporary_file_and_the_lock() {
    // An attempt that misses the writes is tried again, so that the test does not pass without
    // having interrupted a write.
    let mut missed = Vec::new();
    for _ in 0..3 {
        match interrupt_a_restore() {
            Ok(()) => return,
            Err(reason) => missed.push(reason),
        }
    }
    panic!("no attempt interrupted the restore while it wrote: {missed:?}");
}
