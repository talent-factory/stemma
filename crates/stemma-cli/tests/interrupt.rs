//! An interrupted command: Ctrl-C removes the lock of the references that the command holds
//! (ADR-014), and a command that is killed leaves a lock that the next command takes over
//! (ADR-008), so that the next command runs in both cases.
#![cfg(unix)]

mod common;

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::{Dir, ACTOR};

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
