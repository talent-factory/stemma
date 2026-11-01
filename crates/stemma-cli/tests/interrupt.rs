//! An interrupted command: Ctrl-C or a termination signal removes the lock of the references
//! that the command holds (ADR-014), and a command that is killed leaves a lock that the next
//! command takes over (ADR-008), so that the next command runs in both cases. An interrupt also
//! removes the temporary file of a restore that is writing a file (ADR-013).
#![cfg(unix)]

mod common;

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;

use common::{ok, prepare_restore, restore_files, temporary_files, Dir, ACTOR};

/// An unstarted `stemma` in `dir` without `STEMMA_ACTOR` and with its output discarded. Unlike
/// `common::stemma`, which runs the process to its end, it returns the `Command`, so that the test
/// can spawn the process and interrupt it.
fn stemma(dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_stemma"));
    command
        .current_dir(dir)
        .env_remove("STEMMA_ACTOR")
        .env_remove("STEMMA_FAULT")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// The signals that end a command, and the exit code that each one gives (128 plus its number).
const INTERRUPTS: [(Signal, i32); 3] = [
    (Signal::SIGINT, 130),
    (Signal::SIGTERM, 143),
    (Signal::SIGHUP, 129),
];

#[test]
fn an_interrupt_during_a_snapshot_removes_the_lock_and_ends_with_128_plus_the_signal() {
    for (signal, code) in INTERRUPTS {
        // An attempt that misses the lock is tried again, so that the test does not pass
        // without having interrupted a snapshot.
        let mut missed = Vec::new();
        let interrupted = (0..3).any(|_| match interrupt_a_snapshot(signal, code) {
            Ok(()) => true,
            Err(reason) => {
                missed.push(reason);
                false
            }
        });
        assert!(
            interrupted,
            "{signal:?}: no attempt interrupted the snapshot while it held the lock: {missed:?}"
        );
    }
}

/// Interrupts a snapshot while it holds the lock, and checks what it leaves. Returns why it could
/// not interrupt the snapshot there, which the caller tries again.
fn interrupt_a_snapshot(signal: Signal, code: i32) -> Result<(), &'static str> {
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
    signal::kill(Pid::from_raw(i32::try_from(child.id()).unwrap()), signal).unwrap();
    let status = child.wait().unwrap();
    if status.success() {
        // The command ended between the check of the lock and the signal.
        return Err("the command ended before the signal");
    }
    assert_eq!(status.code(), Some(code), "{signal:?} {status:?}");
    assert!(!lock.exists(), "the interrupted command removed its lock");
    let status = stemma(dir.path(), &["status", "--actor", ACTOR])
        .status()
        .unwrap();
    assert!(status.success(), "the next command runs");
    Ok(())
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

/// Interrupts a restore while it writes a file, and checks what it leaves. Returns why it could
/// not interrupt the restore there, which the caller tries again.
fn interrupt_a_restore(signal: Signal, code: i32) -> Result<(), &'static str> {
    let dir = Dir::new();
    let removal = prepare_restore(dir.path());
    let lock = dir.path().join(".stemma").join("refs.lock");
    let mut child = stemma(dir.path(), &["op", "restore", &removal, "--actor", ACTOR])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The signal is sent once a temporary file is seen, so it likely finds one to remove.
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
    // Directly, not through `kill` process, which would take longer than one write.
    let process = Pid::from_raw(i32::try_from(child.id()).unwrap());
    signal::kill(process, signal).unwrap();
    let output = child.wait_with_output().unwrap();
    // A file is there once the restore has renamed its temporary file onto it.
    let restored = restore_files()
        .iter()
        .filter(|name| dir.path().join(name).exists())
        .count();
    if output.status.success() || restored == restore_files().len() {
        return Err("the restore had written every file before the signal");
    }
    assert_eq!(
        output.status.code(),
        Some(code),
        "{signal:?} {:?}",
        output.status
    );
    assert_eq!(temporary_files(dir.path()), Vec::<String>::new());
    assert!(!lock.exists(), "the interrupted restore removed its lock");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "",
        "nothing stayed, so nothing named"
    );
    // The repository is usable: the next command runs and lists nothing left over.
    ok(dir.path(), &["status", "--actor", ACTOR]);
    Ok(())
}

#[test]
fn an_interrupt_during_a_restore_removes_the_temporary_file_and_the_lock() {
    for (signal, code) in INTERRUPTS {
        // An attempt that misses the writes is tried again, so that the test does not pass
        // without having interrupted a write.
        let mut missed = Vec::new();
        let interrupted = (0..3).any(|_| match interrupt_a_restore(signal, code) {
            Ok(()) => true,
            Err(reason) => {
                missed.push(reason);
                false
            }
        });
        assert!(
            interrupted,
            "{signal:?}: no attempt interrupted the restore while it wrote: {missed:?}"
        );
    }
}
