//! An interrupted command on Windows: `CTRL_BREAK_EVENT` removes the lock of the references that
//! the command holds (ADR-014) and ends the command with 130, the code of Ctrl-C. An interrupted
//! restore also removes the temporary file it writes (ADR-013). The counterpart of
//! `interrupt.rs`, which sends signals and so runs on Unix only.
//!
//! Windows has no way to send Ctrl-C to one process: `GenerateConsoleCtrlEvent` reaches a process
//! group. The command therefore starts in a group of its own (`CREATE_NEW_PROCESS_GROUP`), whose
//! ID is its process ID, and the event goes to that group alone. A process in such a group ignores
//! `CTRL_C_EVENT`, so the test sends `CTRL_BREAK_EVENT`, which the handler of `ctrlc` reports the
//! same way.
#![cfg(windows)]

mod common;

use std::fs;
use std::io::{self, Read};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use common::{ok, prepare_restore, restore_files, temporary_files, Dir, ACTOR};

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
        .env_remove("STEMMA_FAULT")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NEW_PROCESS_GROUP);
    command
}

/// Sends `CTRL_BREAK_EVENT` to the process group of `child`, waits for it to end, and returns its
/// status and what it wrote to stderr (piped by the caller). Returns why the event missed, which
/// the caller tries again.
fn break_and_wait(child: &mut Child) -> Result<(ExitStatus, String), &'static str> {
    // SAFETY: the call reads two integers and touches no memory of this program.
    let sent = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, child.id()) };
    if sent == 0 {
        let error = io::Error::last_os_error();
        // A command that ended on its own took its process group with it: a miss to try again.
        if child.try_wait().unwrap().is_some() {
            return Err("the command ended before the event");
        }
        // Otherwise the event needs the console of this process, and without one the test cannot
        // show anything.
        let _ = child.kill();
        let _ = child.wait();
        panic!("GenerateConsoleCtrlEvent failed: {error}");
    }
    // A handler that does not end the command must fail the test and not hang it.
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if start.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the command did not end 30 seconds after the event");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    if status.success() {
        // The command ended between the check of the caller and the event.
        return Err("the command ended before the event");
    }
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    Ok((status, stderr))
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
    // Stderr is kept to name the reason when the test fails, for example a handler that could not
    // be installed.
    let mut child = stemma(dir.path(), &["track", "--actor", ACTOR, "."])
        .stderr(Stdio::piped())
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
    let (status, stderr) = break_and_wait(&mut child)?;
    assert_eq!(status.code(), Some(130), "{status:?}, stderr: {stderr:?}");
    assert!(!lock.exists(), "the interrupted command removed its lock");
    let status = stemma(dir.path(), &["status", "--actor", ACTOR])
        .status()
        .unwrap();
    assert!(status.success(), "the next command runs");
    Ok(())
}

#[test]
fn a_break_during_a_restore_removes_the_temporary_file_and_the_lock() {
    // An attempt that misses the write is tried again, so that the test does not pass without
    // having interrupted a write.
    let mut missed = Vec::new();
    let interrupted = (0..3).any(|_| match interrupt_a_restore() {
        Ok(()) => true,
        Err(reason) => {
            missed.push(reason);
            false
        }
    });
    assert!(
        interrupted,
        "no attempt interrupted the restore while it wrote: {missed:?}"
    );
}

/// Interrupts a restore while it writes a file, and checks what it leaves. Returns why it could
/// not interrupt the restore there, which the caller tries again.
fn interrupt_a_restore() -> Result<(), &'static str> {
    let dir = Dir::new();
    let removal = prepare_restore(dir.path());
    let lock = dir.path().join(".stemma").join("refs.lock");
    let mut child = stemma(dir.path(), &["op", "restore", &removal, "--actor", ACTOR])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The event is sent once a temporary file is seen, so it likely finds one to remove.
    let start = Instant::now();
    while temporary_files(dir.path()).is_empty() {
        if child.try_wait().unwrap().is_some() {
            return Err("the restore ended before a temporary file was seen");
        }
        if start.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("no temporary file seen in 30 seconds");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let (status, stderr) = break_and_wait(&mut child)?;
    // A file is there once the restore has renamed its temporary file onto it.
    let restored = restore_files()
        .iter()
        .filter(|name| dir.path().join(name).exists())
        .count();
    if restored == restore_files().len() {
        return Err("the restore had written every file before the event");
    }
    assert_eq!(status.code(), Some(130), "{status:?}, stderr: {stderr:?}");
    assert_eq!(temporary_files(dir.path()), Vec::<String>::new());
    assert!(!lock.exists(), "the interrupted restore removed its lock");
    assert_eq!(stderr, "", "nothing stayed, so nothing named");
    // The repository is usable: the next command runs and lists nothing left over.
    ok(dir.path(), &["status", "--actor", ACTOR]);
    Ok(())
}
