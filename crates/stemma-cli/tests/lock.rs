//! A held lock ends every writer with code 6, and nothing is changed (ADR-017). `commit` and
//! `status` have their own tests with `--json`; this file covers the other commands.

mod common;

use common::{hold_the_lock, operations, repository, run, track_ok, write, ACTOR};

#[test]
fn a_held_lock_ends_the_other_writers_with_code_6_and_changes_nothing() {
    for args in [
        vec!["track", "b.txt"],
        vec!["op", "restore", "2"],
        vec!["diff"],
    ] {
        let dir = repository();
        write(dir.path(), "a.txt", "one");
        track_ok(dir.path(), "a.txt");
        write(dir.path(), "b.txt", "two");
        let entries = operations(dir.path()).len();
        hold_the_lock(dir.path());

        let mut args = args;
        args.extend(["--actor", ACTOR]);
        let result = run(dir.path(), &args);

        assert_eq!(result.code, 6, "{args:?}: {}", result.stderr);
        assert_eq!(result.stdout, "", "{args:?}");
        assert!(result.stderr.starts_with("stemma: "), "{}", result.stderr);
        assert_eq!(operations(dir.path()).len(), entries, "{args:?}");
    }
}

/// Reading takes no lock: a held lock does not stop `op log` and `op verify`.
#[test]
fn a_held_lock_does_not_stop_the_readers() {
    let dir = repository();
    hold_the_lock(dir.path());

    for args in [vec!["op", "log"], vec!["op", "verify"]] {
        let result = run(dir.path(), &args);

        assert_eq!(result.code, 0, "{args:?}: {}", result.stderr);
    }
}
