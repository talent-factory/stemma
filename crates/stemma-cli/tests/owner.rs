//! The owner of the working copy (ADR-010): a command of another actor that would take a snapshot
//! is refused, and `--take-over` makes that actor the owner, with an entry of its own.

mod common;

use std::fs;
use std::path::Path;

use common::{operations, repository, run, track_ok, write, ACTOR};

/// Another actor than [`ACTOR`], which owns the repositories of [`repository`].
const OTHER: &str = "human bob";

fn owner_line(dir: &Path) -> String {
    let file = fs::read_to_string(dir.join(".stemma/working-copy")).unwrap();
    file.lines().nth(1).unwrap_or_default().to_owned()
}

#[test]
fn init_makes_its_actor_the_owner_in_format_5() {
    let dir = repository();
    assert_eq!(owner_line(dir.path()), format!("owner {ACTOR}"));
    assert_eq!(
        fs::read_to_string(dir.path().join(".stemma/format")).unwrap(),
        "stemma-repository 5\n"
    );
}

#[test]
fn every_command_that_takes_a_snapshot_refuses_another_actor_and_writes_nothing() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    track_ok(dir.path(), "a.txt");
    write(dir.path(), "a.txt", "two");
    let entries = operations(dir.path());
    let commands: [&[&str]; 5] = [
        &["track", "a.txt"],
        &["commit", "-m", "two"],
        &["status"],
        &["diff"],
        &["op", "restore", "2"],
    ];
    for command in commands {
        let mut args = command.to_vec();
        args.extend(["--actor", OTHER]);

        let result = run(dir.path(), &args);

        assert_eq!(result.code, 1, "{command:?}: {}", result.stderr);
        assert_eq!(result.stdout, "", "{command:?}");
        // The refusal is the same in every command, without a prefix of its own.
        let refusal = format!(
            "stemma: the working copy belongs to `{ACTOR}`, so `{OTHER}` may not save it; run \
             the command again with `--take-over` to make `{OTHER}` its owner\n"
        );
        assert_eq!(result.stderr, refusal, "{command:?}");
        assert_eq!(operations(dir.path()), entries, "{command:?}");
    }
    assert_eq!(owner_line(dir.path()), format!("owner {ACTOR}"));
}

#[test]
fn take_over_makes_the_actor_the_owner_with_an_entry_of_its_own() {
    let dir = repository();
    write(dir.path(), "a.txt", "one");
    track_ok(dir.path(), "a.txt");
    write(dir.path(), "a.txt", "two");
    let before = operations(dir.path()).len();

    let result = run(dir.path(), &["status", "--actor", OTHER, "--take-over"]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(result.stdout, "Working copy changes:\nA a.txt\n");
    let number = before + 1;
    assert_eq!(
        result.stderr,
        format!("stemma: took the working copy over as entry {number}\n")
    );
    let log = operations(dir.path());
    assert_eq!(log.len(), before + 2, "{log:?}");
    // Newest first: the snapshot of the new owner, then the change of the owner.
    assert!(log[0].contains("\"operation\":\"snapshot\""), "{}", log[0]);
    assert!(
        log[0].contains(&format!("\"actor\":\"{OTHER}\"")),
        "{}",
        log[0]
    );
    assert!(
        log[1].contains(&format!("\"number\":{number},")),
        "{}",
        log[1]
    );
    assert!(log[1].contains("\"operation\":\"take-over\""), "{}", log[1]);
    assert!(
        log[1].contains(&format!("\"actor\":\"{OTHER}\"")),
        "{}",
        log[1]
    );
    assert!(
        log[1].contains("\"head\":null,\"working_copy\":null,\"branches\":[]"),
        "{}",
        log[1]
    );
    assert_eq!(owner_line(dir.path()), format!("owner {OTHER}"));
    let text = run(dir.path(), &["op", "log", "-n", "2"]).stdout;
    assert!(text.contains(&format!("  {OTHER}  take-over\n")), "{text}");

    // The old owner is refused now, and the chain of the log holds.
    let result = run(dir.path(), &["status", "--actor", ACTOR]);
    assert_eq!(result.code, 1, "{}", result.stderr);
    assert!(result.stderr.contains(&format!("belongs to `{OTHER}`")));
    let verify = run(dir.path(), &["op", "verify"]);
    assert_eq!(verify.code, 0, "{}", verify.stderr);
}

#[test]
fn take_over_by_the_owner_writes_no_entry_and_no_note() {
    let dir = repository();
    let before = operations(dir.path());

    let result = run(dir.path(), &["status", "--actor", ACTOR, "--take-over"]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(result.stderr, "");
    assert_eq!(operations(dir.path()), before);
}

#[test]
fn take_over_cannot_go_with_a_dry_run_of_a_restore() {
    let dir = repository();
    let before = operations(dir.path());

    let result = run(
        dir.path(),
        &[
            "op",
            "restore",
            "1",
            "--dry-run",
            "--take-over",
            "--actor",
            OTHER,
        ],
    );

    assert_eq!(result.code, 2, "{}", result.stderr);
    assert_eq!(operations(dir.path()), before);
    assert_eq!(owner_line(dir.path()), format!("owner {ACTOR}"));
}

#[test]
fn a_repository_of_format_4_rises_to_5_and_the_actor_of_its_next_entry_owns_it() {
    let dir = repository();
    let working_copy = dir.path().join(".stemma/working-copy");
    let file = fs::read_to_string(&working_copy).unwrap();
    let commit = file.lines().next().unwrap();
    fs::write(&working_copy, format!("{commit}\n")).unwrap();
    fs::write(dir.path().join(".stemma/format"), "stemma-repository 4\n").unwrap();
    write(dir.path(), "a.txt", "one");

    let result = run(dir.path(), &["track", "a.txt", "--actor", OTHER]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(operations(dir.path())[0].contains(&format!("\"actor\":\"{OTHER}\"")));
    assert_eq!(owner_line(dir.path()), format!("owner {OTHER}"));
    assert_eq!(
        fs::read_to_string(dir.path().join(".stemma/format")).unwrap(),
        "stemma-repository 5\n"
    );
}

#[test]
fn an_agent_may_not_take_over_and_nothing_is_written() {
    let dir = repository();
    let before = operations(dir.path());
    let agent = "agent claude model=opus-5-5 session=s1 sponsor=daniel";

    let result = run(dir.path(), &["status", "--actor", agent, "--take-over"]);

    assert_eq!(result.code, 1, "{}", result.stderr);
    assert_eq!(result.stdout, "");
    assert!(
        result
            .stderr
            .starts_with("stemma: an agent may not take the working copy over"),
        "{}",
        result.stderr
    );
    assert_eq!(operations(dir.path()), before);
    assert_eq!(owner_line(dir.path()), format!("owner {ACTOR}"));
}

#[test]
fn a_command_that_fails_its_own_checks_does_not_take_over() {
    let dir = repository();
    let before = operations(dir.path());
    let commands: [&[&str]; 3] = [
        &["op", "restore", "999"],
        &["track", "missing.txt"],
        // An empty message; "nothing to commit" is found only after the snapshot.
        &["commit", "-m", " "],
    ];
    for command in commands {
        let mut args = command.to_vec();
        args.extend(["--actor", OTHER, "--take-over"]);

        let result = run(dir.path(), &args);

        assert_eq!(result.code, 1, "{command:?}: {}", result.stderr);
        assert!(
            !result.stderr.contains("took the working copy over"),
            "{command:?}"
        );
        assert_eq!(operations(dir.path()), before, "{command:?}");
        assert_eq!(
            owner_line(dir.path()),
            format!("owner {ACTOR}"),
            "{command:?}"
        );
    }
}

#[test]
fn status_refuses_another_actor_before_it_reads_the_exclusion_file() {
    let dir = repository();
    write(dir.path(), ".stemmaignore", "!broken\n");

    let result = run(dir.path(), &["status", "--actor", OTHER]);

    assert_eq!(result.code, 1, "{}", result.stderr);
    assert!(
        result
            .stderr
            .starts_with(&format!("stemma: the working copy belongs to `{ACTOR}`")),
        "{}",
        result.stderr
    );
}
