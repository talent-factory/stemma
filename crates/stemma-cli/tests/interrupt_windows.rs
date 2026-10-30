//! An interrupted command on Windows: `CTRL_BREAK_EVENT` removes the lock of the references that
//! the command holds (ADR-014) and ends the command with 130, the code of Ctrl-C. The counterpart
//! of `interrupt.rs`, which sends signals and so runs on Unix only.
//!
//! Windows has no way to send Ctrl-C to one process: `GenerateConsoleCtrlEvent` reaches a process
//! group. The command therefore starts in a group of its own (`CREATE_NEW_PROCESS_GROUP`), whose
//! ID is its process ID, and the event goes to that group alone. A process in such a group ignores
//! `CTRL_C_EVENT`, so the test sends `CTRL_BREAK_EVENT`, which the handler of `ctrlc` reports the
//! same way.
#![cfg(windows)]

mod common;

use std::fs;
use std::io;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::{Dir, ACTOR};

const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const CTRL_BREAK_EVENT: u32 = 1;

extern "system" {
    /// `kernel32`, which every Windows program links.
    fn GenerateConsoleCtrlEvent(event: u32, process_group_id: u32) -> i32;
}

/// An unstarted `stemma` in `dir` without `STEMMA_ACTOR` and with its output discarded, in a
/// process group of its own.
fn stemma(dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_stemma"));
    command
        .current_dir(dir)
        .env_remove("STEMMA_ACTOR")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NEW_PROCESS_GROUP);
    command
}

#[test]
fn a_break_during_a_snapshot_removes_the_lock_and_ends_with_130() {
    // An attempt that misses the lock is tried again, so that the test does not pass without
    // having interrupted a snapshot.
    let mut missed = Vec::new();
    let interrupted = (0..3).any(|_| match interrupt_a_snapshot() {
        Ok(()) => true,
        Err(reason) => {
            missed.push(reason);
            false
        }
    });
    assert!(
        interrupted,
        "no attempt interrupted the snapshot while it held the lock: {missed:?}"
    );
}

/// Interrupts a snapshot while it holds the lock, and checks what it leaves. Returns why it could
/// not interrupt the snapshot there, which the caller tries again.
fn interrupt_a_snapshot() -> Result<(), &'static str> {
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
        return Err("the command ended before its lock was seen");
    }
    // SAFETY: the call reads two integers and touches no memory of this program.
    let sent = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, child.id()) };
    if sent == 0 {
        // Not a miss to try again: the event needs the console of this process, and without
        // one the test cannot show anything.
        let error = io::Error::last_os_error();
        let _ = child.kill();
        let _ = child.wait();
        panic!("GenerateConsoleCtrlEvent failed: {error}");
    }
    let status = child.wait().unwrap();
    if status.success() {
        // The command ended between the check of the lock and the event.
        return Err("the command ended before the event");
    }
    assert_eq!(status.code(), Some(130), "{status:?}");
    assert!(!lock.exists(), "the interrupted command removed its lock");
    let status = stemma(dir.path(), &["status", "--actor", ACTOR])
        .status()
        .unwrap();
    assert!(status.success(), "the next command runs");
    Ok(())
}
