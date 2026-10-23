//! The command `stemma op verify`, run as a process, as a person or an agent would run it.

mod common;

use std::fs;
use std::path::Path;

use common::{entry_file, run, Dir, ACTOR};

/// A repository made by `init`, with two files tracked and committed, so that its log holds
/// several entries.
fn repository() -> Dir {
    let dir = Dir::new();
    for args in [
        &["init", "--actor", ACTOR][..],
        &["track", ".", "--actor", ACTOR][..],
    ] {
        let result = run(dir.path(), args);
        assert_eq!(result.code, 0, "{}", result.stderr);
    }
    fs::write(dir.path().join("a.txt"), "one").unwrap();
    for args in [
        &["track", "a.txt", "--actor", ACTOR][..],
        &["commit", "-m", "first", "--actor", ACTOR][..],
    ] {
        let result = run(dir.path(), args);
        assert_eq!(result.code, 0, "{}", result.stderr);
    }
    dir
}

fn last(dir: &Path) -> u64 {
    fs::read_dir(dir.join(".stemma/oplog")).unwrap().count() as u64
}

/// The anchor that `op verify` prints on its last line.
fn anchor(dir: &Path) -> String {
    let result = run(dir, &["op", "verify"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    let line = result.stdout.lines().last().unwrap();
    line.strip_prefix("anchor ").unwrap().to_owned()
}

#[test]
fn a_log_written_by_stemma_holds_and_the_last_entry_is_its_anchor() {
    let dir = repository();
    let result = run(dir.path(), &["op", "verify"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(result.stderr, "");
    let last = last(dir.path());
    let lines: Vec<&str> = result.stdout.lines().collect();
    assert_eq!(
        lines[0],
        format!("Checked {last} entries of the operation log: the hash chain holds.")
    );
    assert!(
        lines[1].starts_with(&format!("anchor {last}:b3:")),
        "{}",
        result.stdout
    );
    assert_eq!(lines.len(), 2);
}

#[test]
fn an_anchor_that_still_holds_is_confirmed() {
    let dir = repository();
    let kept = anchor(dir.path());
    // Later entries do not change the hash of the anchor.
    fs::write(dir.path().join("a.txt"), "two").unwrap();
    let result = run(dir.path(), &["commit", "-m", "second", "--actor", ACTOR]);
    assert_eq!(result.code, 0, "{}", result.stderr);

    let result = run(dir.path(), &["op", "verify", "--anchor", &kept]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    let number = kept.split(':').next().unwrap();
    assert!(
        result.stdout.contains(&format!(
            "Entry {number} still has the hash of the anchor.\n"
        )),
        "{}",
        result.stdout
    );
}

#[test]
fn a_changed_entry_breaks_the_chain_and_is_named() {
    let dir = repository();
    let text = fs::read_to_string(entry_file(dir.path(), 2)).unwrap();
    let time = text.lines().nth(2).unwrap();
    let changed = text.replacen(time, "time 1", 1);
    fs::write(entry_file(dir.path(), 2), changed).unwrap();

    let result = run(dir.path(), &["op", "verify"]);
    assert_eq!(result.code, 1);
    assert_eq!(result.stdout, "");
    assert!(
        result
            .stderr
            .starts_with("stemma: entry 3 does not name the hash of the entry before it"),
        "{}",
        result.stderr
    );
}

#[test]
fn a_log_whose_chain_was_computed_again_is_found_only_by_the_anchor() {
    let dir = repository();
    let kept = anchor(dir.path());
    // Rewrite the log as an attacker who can write `.stemma` would: change the last entry and
    // write the tip again. The chain holds, the anchor does not.
    let number = kept.split(':').next().unwrap().parse::<u64>().unwrap();
    let text = fs::read_to_string(entry_file(dir.path(), number)).unwrap();
    let time = text.lines().nth(2).unwrap();
    let changed = text.replacen(time, "time 1", 1);
    fs::write(entry_file(dir.path(), number), &changed).unwrap();
    let hash = stemma_core::oplog::Entry::parse(changed.as_bytes())
        .unwrap()
        .hash()
        .unwrap();
    fs::write(
        dir.path().join(".stemma/oplog-tip"),
        format!("stemma-oplog-tip 2\nnumber {number}\nhash {hash}\n"),
    )
    .unwrap();
    let result = run(dir.path(), &["op", "verify"]);
    assert_eq!(result.code, 0, "{}", result.stderr);

    let result = run(dir.path(), &["op", "verify", "--anchor", &kept]);
    assert_eq!(result.code, 1);
    assert!(
        result
            .stderr
            .contains(&format!("entry {number} has the hash"))
            && result.stderr.contains("not the hash"),
        "{}",
        result.stderr
    );
}

#[test]
fn an_anchor_beyond_the_log_or_in_the_wrong_form_is_refused() {
    let dir = repository();
    let kept = anchor(dir.path());
    let hash = kept.split_once(':').unwrap().1;
    let result = run(
        dir.path(),
        &["op", "verify", "--anchor", &format!("99:{hash}")],
    );
    assert_eq!(result.code, 1);
    assert!(
        result
            .stderr
            .contains("names entry 99, but the log has no such entry"),
        "{}",
        result.stderr
    );
    for bad in [
        "",
        "3",
        "03:b3:00",
        "0:b3:11",
        "x:b3:11",
        &format!("3:{}", hash.to_uppercase()),
    ] {
        let result = run(dir.path(), &["op", "verify", "--anchor", bad]);
        assert_eq!(result.code, 2, "{bad:?}: {}", result.stderr);
    }
}

#[test]
fn verify_outside_a_repository_fails() {
    let dir = Dir::new();
    let result = run(dir.path(), &["op", "verify"]);
    assert_eq!(result.code, 1);
    assert!(result.stderr.starts_with("stemma: "), "{}", result.stderr);
}

/// Writes the log of the repository again in the form from before the hash chain, with a tip of
/// version 1, at format 3, as a program before the chain left it.
fn to_the_form_before_the_chain(dir: &Path) {
    let last = last(dir);
    for number in 1..=last {
        let text = fs::read_to_string(entry_file(dir, number)).unwrap();
        let old: String = text
            .replacen("stemma-oplog 3\n", "stemma-oplog 1\n", 1)
            .lines()
            .filter(|line| !line.starts_with("previous "))
            .map(|line| format!("{line}\n"))
            .collect();
        fs::write(entry_file(dir, number), old).unwrap();
    }
    fs::write(
        dir.join(".stemma/oplog-tip"),
        format!("stemma-oplog-tip 1\nnumber {last}\n"),
    )
    .unwrap();
    fs::write(dir.join(".stemma/format"), "stemma-repository 3\n").unwrap();
}

#[test]
fn a_log_from_before_the_chain_has_no_anchor_until_op_restore_starts_the_chain() {
    let dir = repository();
    let kept = anchor(dir.path());
    to_the_form_before_the_chain(dir.path());
    let old = last(dir.path());

    let result = run(dir.path(), &["op", "verify"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(
        result.stdout,
        format!(
            "Checked {old} entries of the operation log: no entry is in the hash chain yet, so \
             there is no anchor; the next entry starts it.\n"
        )
    );
    // The entry of an anchor from before is now from before the chain, with no hash.
    let result = run(dir.path(), &["op", "verify", "--anchor", &kept]);
    assert_eq!(result.code, 1);
    assert!(
        result.stderr.contains("written before the hash chain"),
        "{}",
        result.stderr
    );

    // `op restore` writes the first entry of the chain and moves the repository to format 4.
    let result = run(
        dir.path(),
        &["op", "restore", &old.to_string(), "--actor", ACTOR],
    );
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(
        fs::read_to_string(dir.path().join(".stemma/format")).unwrap(),
        "stemma-repository 4\n"
    );
    let result = run(dir.path(), &["op", "verify"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    let start = old + 1;
    assert!(
        result.stdout.starts_with(&format!(
            "Checked {} entries of the operation log: the hash chain holds from entry {start}; \
             entries 1 to {old} were written before it and are not covered.\n",
            last(dir.path())
        )),
        "{}",
        result.stdout
    );
    assert!(result.stdout.contains("\nanchor "), "{}", result.stdout);
}

#[test]
fn a_tip_file_more_than_one_entry_behind_is_reported() {
    let dir = repository();
    let last = last(dir.path());
    let text = fs::read_to_string(entry_file(dir.path(), last - 2)).unwrap();
    let hash = stemma_core::oplog::Entry::parse(text.as_bytes())
        .unwrap()
        .hash()
        .unwrap();
    fs::write(
        dir.path().join(".stemma/oplog-tip"),
        format!("stemma-oplog-tip 2\nnumber {}\nhash {hash}\n", last - 2),
    )
    .unwrap();
    let result = run(dir.path(), &["op", "verify"]);
    assert_eq!(result.code, 1);
    assert_eq!(result.stdout, "");
    assert!(
        result.stderr.contains(&format!(
            "names entry {}, but the log ends at entry {last}",
            last - 2
        )),
        "{}",
        result.stderr
    );
}

#[test]
fn a_missing_tip_file_is_reported() {
    let dir = repository();
    fs::remove_file(dir.path().join(".stemma/oplog-tip")).unwrap();
    let result = run(dir.path(), &["op", "verify"]);
    assert_eq!(result.code, 1);
    assert_eq!(result.stdout, "");
    assert!(result.stderr.contains("no tip file"), "{}", result.stderr);
    // `op log` reads the log all the same.
    let result = run(dir.path(), &["op", "log"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
}

#[test]
fn op_log_warns_when_it_cannot_check_the_link_of_its_oldest_entry() {
    let dir = repository();
    let last = last(dir.path());
    fs::write(entry_file(dir.path(), last - 1), "damaged").unwrap();
    let result = run(dir.path(), &["op", "log", "-n", "1"]);
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(
        result.stderr.starts_with(&format!(
            "stemma: warning: the link of the oldest shown entry to entry {} was not checked",
            last - 1
        )),
        "{}",
        result.stderr
    );
    let result = run(dir.path(), &["op", "verify"]);
    assert_eq!(result.code, 1);
}
