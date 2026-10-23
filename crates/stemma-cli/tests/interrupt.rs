//! An interrupted command: Ctrl-C removes the lock of the references that the command holds
//! (ADR-014), so that the next command runs.
#![cfg(unix)]

mod common;

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::Dir;

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
    let status = stemma(dir.path(), &["init", "--actor", "human daniel"])
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
    let mut child = stemma(dir.path(), &["track", "--actor", "human daniel", "."])
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
    let status = stemma(dir.path(), &["status", "--actor", "human daniel"])
        .status()
        .unwrap();
    assert!(status.success(), "the next command runs");
}
