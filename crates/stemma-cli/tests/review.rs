//! The commands of the review path, run as a process: `pr`, `check`, and `lease`.

mod common;

use std::fs;
use std::path::Path;

use common::{ok, run, track_ok, write, Dir, ACTOR};

const OTHER: &str = "human other";

/// An open repository with the commit `base` on `main`, and the branch `feat` with one more commit.
/// The working copy is on `feat`.
fn with_feature_branch() -> Dir {
    let dir = Dir::new();
    ok(dir.path(), &["init", "--actor", ACTOR]);
    write(dir.path(), "a.txt", "a\n");
    track_ok(dir.path(), "a.txt");
    ok(dir.path(), &["commit", "-m", "base", "--actor", ACTOR]);
    feature_commit(dir.path());
    dir
}

fn feature_commit(dir: &Path) {
    ok(dir, &["branch", "feat", "--actor", ACTOR]);
    ok(dir, &["switch", "feat", "--actor", ACTOR]);
    write(dir, "b.txt", "b\n");
    track_ok(dir, "b.txt");
    ok(dir, &["commit", "-m", "feature", "--actor", ACTOR]);
}

/// Opens a pull request for `feat` and returns its number.
fn open_pull(dir: &Path) -> String {
    let result = ok(dir, &["pr", "open", "feat", "--actor", ACTOR]);
    result
        .stdout
        .split_whitespace()
        .find(|word| word.chars().all(|c| c.is_ascii_digit()))
        .expect("the output names the number")
        .to_owned()
}

#[test]
fn a_pull_request_goes_from_open_to_merged() {
    let dir = with_feature_branch();
    let number = open_pull(dir.path());
    let listed = ok(dir.path(), &["pr", "list"]);
    assert!(
        listed.stdout.contains(&format!("{number} open feat")),
        "{}",
        listed.stdout
    );
    assert!(listed.stdout.contains("approvals 0"), "{}", listed.stdout);

    ok(dir.path(), &["pr", "approve", &number, "--actor", OTHER]);
    let listed = ok(dir.path(), &["pr", "list"]);
    assert!(listed.stdout.contains("approvals 1"), "{}", listed.stdout);

    ok(
        dir.path(),
        &[
            "check", "record", "feat", "--base", "main", "--result", "pass", "--actor", OTHER,
        ],
    );
    let checks = ok(dir.path(), &["check", "list"]);
    assert!(checks.stdout.contains(" pass "), "{}", checks.stdout);
    assert!(checks.stdout.contains("on "), "{}", checks.stdout);

    let merged = ok(
        dir.path(),
        &["pr", "merge", &number, "--level", "2", "--actor", ACTOR],
    );
    assert!(
        merged.stdout.contains("Merged pull request"),
        "{}",
        merged.stdout
    );
    let listed = ok(dir.path(), &["pr", "list"]);
    assert!(
        listed.stdout.contains(&format!("{number} merged feat")),
        "{}",
        listed.stdout
    );
    assert!(
        fs::read_dir(dir.path().join(".stemma")).is_ok(),
        "the repository stays"
    );
}

#[test]
fn a_merge_without_a_check_is_refused_and_leaves_the_pull_request_open() {
    let dir = with_feature_branch();
    let number = open_pull(dir.path());
    let result = run(dir.path(), &["pr", "merge", &number, "--actor", ACTOR]);
    assert_eq!(result.code, 1, "{}", result.stdout);
    assert!(
        result.stderr.contains("no result of a check counts"),
        "{}",
        result.stderr
    );
    let listed = ok(dir.path(), &["pr", "list"]);
    assert!(listed.stdout.contains("open"), "{}", listed.stdout);
}

#[test]
fn a_failed_check_does_not_count() {
    let dir = with_feature_branch();
    let number = open_pull(dir.path());
    ok(
        dir.path(),
        &[
            "check", "record", "feat", "--base", "main", "--result", "fail", "--actor", OTHER,
        ],
    );
    ok(dir.path(), &["pr", "approve", &number, "--actor", OTHER]);
    let result = run(
        dir.path(),
        &["pr", "merge", &number, "--level", "2", "--actor", ACTOR],
    );
    assert_eq!(result.code, 1, "{}", result.stdout);
    assert!(
        result.stderr.contains("no result of a check counts"),
        "{}",
        result.stderr
    );
    let checks = ok(dir.path(), &["check", "list"]);
    assert!(checks.stdout.contains(" fail "), "{}", checks.stdout);
}

#[test]
fn the_opener_cannot_approve_the_own_pull_request() {
    let dir = with_feature_branch();
    let number = open_pull(dir.path());
    let result = run(dir.path(), &["pr", "approve", &number, "--actor", ACTOR]);
    assert_eq!(result.code, 1, "{}", result.stdout);
    let listed = ok(dir.path(), &["pr", "list"]);
    assert!(listed.stdout.contains("approvals 0"), "{}", listed.stdout);
}

#[test]
fn a_second_pull_request_for_the_same_branch_is_refused() {
    let dir = with_feature_branch();
    let _ = open_pull(dir.path());
    let result = run(dir.path(), &["pr", "open", "feat", "--actor", ACTOR]);
    assert_eq!(result.code, 1, "{}", result.stdout);
}

#[test]
fn a_pull_request_for_main_or_for_a_missing_branch_is_refused() {
    let dir = with_feature_branch();
    for branch in ["main", "nothing"] {
        let result = run(dir.path(), &["pr", "open", branch, "--actor", ACTOR]);
        assert_eq!(result.code, 1, "{branch}: {}", result.stdout);
    }
}

#[test]
fn closing_ends_the_pull_request_and_a_second_close_is_refused() {
    let dir = with_feature_branch();
    let number = open_pull(dir.path());
    ok(dir.path(), &["pr", "close", &number, "--actor", ACTOR]);
    let listed = ok(dir.path(), &["pr", "list"]);
    assert!(
        listed.stdout.contains(&format!("{number} closed feat")),
        "{}",
        listed.stdout
    );
    let result = run(dir.path(), &["pr", "close", &number, "--actor", ACTOR]);
    assert_eq!(result.code, 1, "{}", result.stdout);
    // An ended pull request is final: the same opening is not accepted twice (ADR-025).
    let again = run(dir.path(), &["pr", "open", "feat", "--actor", ACTOR]);
    assert_eq!(again.code, 1, "{}", again.stdout);
    assert!(
        again.stderr.contains("needs a new head"),
        "{}",
        again.stderr
    );
    // A new commit on the branch is a new head, and so a new pull request.
    write(dir.path(), "b.txt", "b, again\n");
    ok(dir.path(), &["commit", "-m", "more", "--actor", ACTOR]);
    let next = open_pull(dir.path());
    assert_ne!(next, number);
}

#[test]
fn op_verify_checks_the_statements_and_the_entries_that_name_them() {
    let dir = with_feature_branch();
    let number = open_pull(dir.path());
    ok(
        dir.path(),
        &[
            "check", "record", "feat", "--base", "main", "--result", "pass", "--actor", OTHER,
        ],
    );
    ok(dir.path(), &["pr", "approve", &number, "--actor", OTHER]);
    let verified = ok(dir.path(), &["op", "verify"]);
    assert!(
        verified.stdout.contains(
            "Checked 3 signed statements: each is valid, and every entry that names one agrees \
             with it.\n"
        ),
        "{}",
        verified.stdout
    );
    // One file per statement, named by its hash; the open mode signs nothing.
    let statements = dir.path().join(".stemma/statements");
    let mut files: Vec<_> = fs::read_dir(&statements)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    files.sort();
    assert_eq!(files.len(), 3);
    for file in &files {
        let text = fs::read_to_string(file).unwrap();
        assert!(text.starts_with("stemma-protocol 1\nstatement "), "{text}");
        assert!(text.ends_with("key -\nsignature -\n"), "{text}");
        assert_eq!(file.file_name().unwrap().len(), 64);
    }
    // A statement that an entry names and that is gone is damage; so is a changed one.
    let kept = fs::read(&files[0]).unwrap();
    fs::remove_file(&files[0]).unwrap();
    let missing = run(dir.path(), &["op", "verify"]);
    assert_eq!(missing.code, 5, "{}", missing.stdout);
    assert!(
        missing
            .stderr
            .contains("which the repository does not hold"),
        "{}",
        missing.stderr
    );
    let changed = String::from_utf8(kept.clone()).unwrap().replace(
        "repository -",
        "repository 00112233445566778899aabbccddeeff",
    );
    fs::write(&files[0], changed).unwrap();
    let damaged = run(dir.path(), &["op", "verify"]);
    assert_eq!(damaged.code, 5, "{}", damaged.stdout);
    assert!(
        damaged.stderr.contains("holds the statement"),
        "{}",
        damaged.stderr
    );
    fs::write(&files[0], kept).unwrap();
    ok(dir.path(), &["op", "verify"]);
}

#[test]
fn a_pull_request_that_does_not_exist_is_refused() {
    let dir = with_feature_branch();
    let result = run(dir.path(), &["pr", "close", "99", "--actor", ACTOR]);
    assert_eq!(result.code, 1, "{}", result.stdout);
    assert!(result.stderr.contains("99"), "{}", result.stderr);
}

#[test]
fn a_level_above_three_is_a_usage_error() {
    let dir = with_feature_branch();
    let result = run(dir.path(), &["pr", "merge", "1", "--level", "4"]);
    assert_eq!(result.code, 2, "{}", result.stdout);
}

#[test]
fn a_check_for_a_commit_that_does_not_exist_is_refused() {
    let dir = with_feature_branch();
    let result = run(
        dir.path(),
        &[
            "check", "record", "nothing", "--result", "pass", "--actor", OTHER,
        ],
    );
    assert_eq!(result.code, 1, "{}", result.stdout);
    assert_eq!(ok(dir.path(), &["check", "list"]).stdout, "");
}

#[test]
fn a_command_without_an_actor_fails_before_it_writes() {
    let dir = with_feature_branch();
    let result = run(dir.path(), &["pr", "open", "feat"]);
    assert_eq!(result.code, 1, "{}", result.stdout);
    assert!(result.stderr.contains("no actor"), "{}", result.stderr);
}

#[test]
fn an_open_repository_has_no_lease() {
    let dir = with_feature_branch();
    let shown = ok(dir.path(), &["lease", "show", "feat"]);
    assert!(shown.stdout.contains("has no lease"), "{}", shown.stdout);
    for args in [
        &["lease", "take", "feat", "--actor", ACTOR][..],
        &["lease", "release", "feat", "--actor", ACTOR][..],
    ] {
        let result = run(dir.path(), args);
        assert_eq!(result.code, 1, "{args:?}: {}", result.stdout);
        assert!(
            result.stderr.contains("no lease applies"),
            "{}",
            result.stderr
        );
    }
}

// The enforcing repository: signatures and leases need `ssh-keygen`.

const REGISTER: &str = ".stemma-actors/register";

/// Makes an Ed25519 key pair in `dir` with `ssh-keygen` and returns the path of the private key.
/// `None` if `ssh-keygen` is not on the path (the test is skipped, unless
/// STEMMA_REQUIRE_SSH_KEYGEN is set).
fn generate_key(dir: &Path, name: &str) -> Option<String> {
    let file = dir.join(name);
    let made = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&file)
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !made {
        assert!(
            std::env::var_os("STEMMA_REQUIRE_SSH_KEYGEN").is_none(),
            "STEMMA_REQUIRE_SSH_KEYGEN is set, but `ssh-keygen` does not work"
        );
        eprintln!("SKIPPED: `ssh-keygen` is not on the path");
        return None;
    }
    Some(file.to_str().unwrap().to_owned())
}

/// An enforcing repository with two identities, `human daniel` (owner) and `human other`, and the
/// branch `feat` at the commit that holds the register.
fn enforcing(owner_key: &str, other_key: &str) -> Dir {
    let dir = Dir::new();
    ok(
        dir.path(),
        &[
            "init",
            "--actor",
            ACTOR,
            "--owner-key",
            &format!("{owner_key}.pub"),
        ],
    );
    ok(
        dir.path(),
        &[
            "register",
            "add-human",
            &format!("{other_key}.pub"),
            "--human",
            "other",
            "--actor",
            ACTOR,
        ],
    );
    track_ok(dir.path(), REGISTER);
    ok(dir.path(), &["commit", "-m", "register", "--actor", ACTOR]);
    ok(dir.path(), &["branch", "feat", "--actor", ACTOR]);
    dir
}

#[test]
fn a_lease_is_taken_shown_refused_to_another_and_released() {
    let keys = Dir::new();
    let (Some(owner), Some(other)) = (
        generate_key(keys.path(), "owner"),
        generate_key(keys.path(), "other"),
    ) else {
        return;
    };
    let dir = enforcing(&owner, &other);
    let taken = ok(dir.path(), &["lease", "take", "feat", "--actor", ACTOR]);
    assert!(taken.stdout.contains("Took the lease"), "{}", taken.stdout);
    let renewed = ok(dir.path(), &["lease", "take", "feat", "--actor", ACTOR]);
    assert!(renewed.stdout.contains("Renewed"), "{}", renewed.stdout);
    let shown = ok(dir.path(), &["lease", "show", "feat"]);
    assert!(
        shown.stdout.contains("held by human daniel"),
        "{}",
        shown.stdout
    );
    let refused = run(dir.path(), &["lease", "take", "feat", "--actor", OTHER]);
    assert_eq!(refused.code, 1, "{}", refused.stdout);
    ok(dir.path(), &["lease", "release", "feat", "--actor", ACTOR]);
    let shown = ok(dir.path(), &["lease", "show", "feat"]);
    assert!(shown.stdout.contains("no lease"), "{}", shown.stdout);
}

#[test]
fn an_owner_overrides_a_lease_with_a_signed_entry() {
    let keys = Dir::new();
    let (Some(owner), Some(other)) = (
        generate_key(keys.path(), "owner"),
        generate_key(keys.path(), "other"),
    ) else {
        return;
    };
    let dir = enforcing(&owner, &other);
    ok(dir.path(), &["lease", "take", "feat", "--actor", OTHER]);
    let unsigned = run(
        dir.path(),
        &[
            "lease", "override", "feat", "--reason", "stuck", "--actor", ACTOR,
        ],
    );
    assert_eq!(unsigned.code, 1, "{}", unsigned.stdout);
    let shown = ok(dir.path(), &["lease", "show", "feat"]);
    assert!(
        shown.stdout.contains("held by human other"),
        "{}",
        shown.stdout
    );
    ok(
        dir.path(),
        &[
            "lease", "override", "feat", "--reason", "stuck", "--key", &owner, "--actor", ACTOR,
        ],
    );
    let shown = ok(dir.path(), &["lease", "show", "feat"]);
    assert!(
        shown.stdout.contains("held by human daniel"),
        "{}",
        shown.stdout
    );
    let verified = ok(dir.path(), &["op", "verify"]);
    assert!(verified.stdout.contains("proofs"), "{}", verified.stdout);
}

#[test]
fn an_enforcing_repository_signs_checks_and_approvals() {
    let keys = Dir::new();
    let (Some(owner), Some(other)) = (
        generate_key(keys.path(), "owner"),
        generate_key(keys.path(), "other"),
    ) else {
        return;
    };
    let dir = enforcing(&owner, &other);
    ok(dir.path(), &["lease", "take", "feat", "--actor", ACTOR]);
    ok(dir.path(), &["switch", "feat", "--actor", ACTOR]);
    write(dir.path(), "b.txt", "b\n");
    track_ok(dir.path(), "b.txt");
    ok(dir.path(), &["commit", "-m", "feature", "--actor", ACTOR]);
    // The opening is a signed statement: no key, no entry.
    let unsigned = run(dir.path(), &["pr", "open", "feat", "--actor", ACTOR]);
    assert_eq!(unsigned.code, 1, "{}", unsigned.stdout);
    assert!(
        unsigned.stderr.contains("--key KEY_FILE"),
        "{}",
        unsigned.stderr
    );
    let number = open_pull_as(dir.path(), ACTOR, &owner);

    // No key, no entry.
    let unsigned = run(dir.path(), &["pr", "approve", &number, "--actor", OTHER]);
    assert_eq!(unsigned.code, 1, "{}", unsigned.stdout);
    ok(
        dir.path(),
        &["pr", "approve", &number, "--key", &other, "--actor", OTHER],
    );
    let listed = ok(dir.path(), &["pr", "list"]);
    assert!(listed.stdout.contains("approvals 1"), "{}", listed.stdout);

    let unsigned = run(
        dir.path(),
        &[
            "check", "record", "feat", "--result", "pass", "--actor", OTHER,
        ],
    );
    assert_eq!(unsigned.code, 1, "{}", unsigned.stdout);
    ok(
        dir.path(),
        &[
            "check", "record", "feat", "--base", "main", "--result", "pass", "--key", &other,
            "--actor", OTHER,
        ],
    );
    let verified = ok(dir.path(), &["op", "verify"]);
    // The signatures are part of the statements, so there is no proof file to count.
    assert!(
        verified
            .stdout
            .contains("and 0 proofs of log entries hold against it."),
        "{}",
        verified.stdout
    );
    assert!(
        verified.stdout.contains(
            "Checked 3 signed statements: each is valid, and every entry that names one agrees \
             with it, and the signatures hold against the actor register.\n"
        ),
        "{}",
        verified.stdout
    );
    assert!(!dir.path().join(".stemma/proofs").exists());
    let merged = ok(
        dir.path(),
        &["pr", "merge", &number, "--level", "2", "--actor", ACTOR],
    );
    assert!(
        merged.stdout.contains("Merged pull request"),
        "{}",
        merged.stdout
    );
}

fn open_pull_as(dir: &Path, actor: &str, key: &str) -> String {
    let result = ok(dir, &["pr", "open", "feat", "--key", key, "--actor", actor]);
    result
        .stdout
        .split_whitespace()
        .find(|word| word.chars().all(|c| c.is_ascii_digit()))
        .expect("the output names the number")
        .to_owned()
}

#[test]
fn a_pull_request_opened_in_the_open_mode_is_opened_again_after_enforce() {
    let keys = Dir::new();
    let Some(owner) = generate_key(keys.path(), "owner") else {
        return;
    };
    let dir = with_feature_branch();
    let number = open_pull(dir.path());
    ok(
        dir.path(),
        &[
            "enforce",
            "--actor",
            ACTOR,
            "--owner-key",
            &format!("{owner}.pub"),
        ],
    );
    track_ok(dir.path(), REGISTER);
    ok(dir.path(), &["commit", "-m", "register", "--actor", ACTOR]);
    // The opening carries no signature, so the enforcing repository does not count it.
    let listed = ok(dir.path(), &["pr", "list"]);
    assert!(
        !listed.stdout.contains(&format!("{number} open")),
        "{}",
        listed.stdout
    );
    // The branch has a new head since the commit of the register, and a signed opening counts.
    let again = open_pull_as(dir.path(), ACTOR, &owner);
    assert_ne!(again, number);
    let listed = ok(dir.path(), &["pr", "list"]);
    assert!(
        listed.stdout.contains(&format!("{again} open feat")),
        "{}",
        listed.stdout
    );
    let verified = ok(dir.path(), &["op", "verify"]);
    assert!(
        verified
            .stdout
            .contains("1 entries name a statement without a signature"),
        "{}",
        verified.stdout
    );
}

#[test]
fn a_key_that_cannot_sign_is_refused_before_the_entry_is_written() {
    let keys = Dir::new();
    let (Some(owner), Some(other)) = (
        generate_key(keys.path(), "owner"),
        generate_key(keys.path(), "other"),
    ) else {
        return;
    };
    let dir = enforcing(&owner, &other);
    let missing = keys.path().join("missing");
    let before = ok(dir.path(), &["op", "log"]).stdout;
    let result = run(
        dir.path(),
        &[
            "check",
            "record",
            "feat",
            "--result",
            "pass",
            "--key",
            missing.to_str().unwrap(),
            "--actor",
            OTHER,
        ],
    );
    assert_eq!(result.code, 1, "{}", result.stdout);
    assert!(
        result.stderr.contains("nothing is written"),
        "{}",
        result.stderr
    );
    assert_eq!(ok(dir.path(), &["op", "log"]).stdout, before);
}

#[test]
fn a_member_cannot_override_a_lease_and_nothing_changes() {
    let keys = Dir::new();
    let (Some(owner), Some(other)) = (
        generate_key(keys.path(), "owner"),
        generate_key(keys.path(), "other"),
    ) else {
        return;
    };
    let dir = enforcing(&owner, &other);
    ok(dir.path(), &["lease", "take", "feat", "--actor", ACTOR]);
    let result = run(
        dir.path(),
        &[
            "lease", "override", "feat", "--reason", "x", "--key", &other, "--actor", OTHER,
        ],
    );
    assert_eq!(result.code, 1, "{}", result.stdout);
    let shown = ok(dir.path(), &["lease", "show", "feat"]);
    assert!(
        shown.stdout.contains("held by human daniel"),
        "{}",
        shown.stdout
    );
}

#[test]
fn an_approval_signed_with_the_wrong_key_is_refused() {
    let keys = Dir::new();
    let (Some(owner), Some(other)) = (
        generate_key(keys.path(), "owner"),
        generate_key(keys.path(), "other"),
    ) else {
        return;
    };
    let dir = enforcing(&owner, &other);
    ok(dir.path(), &["lease", "take", "feat", "--actor", ACTOR]);
    ok(dir.path(), &["switch", "feat", "--actor", ACTOR]);
    write(dir.path(), "b.txt", "b\n");
    track_ok(dir.path(), "b.txt");
    ok(dir.path(), &["commit", "-m", "feature", "--actor", ACTOR]);
    let number = open_pull_as(dir.path(), ACTOR, &owner);
    let result = run(
        dir.path(),
        &["pr", "approve", &number, "--key", &owner, "--actor", OTHER],
    );
    assert_eq!(result.code, 1, "{}", result.stdout);
    let listed = ok(dir.path(), &["pr", "list"]);
    assert!(listed.stdout.contains("approvals 0"), "{}", listed.stdout);
}

#[test]
fn a_merge_moves_main_to_the_commit_that_it_names() {
    let dir = with_feature_branch();
    let number = open_pull(dir.path());
    ok(
        dir.path(),
        &[
            "check", "record", "feat", "--base", "main", "--result", "pass", "--actor", OTHER,
        ],
    );
    let merged = ok(
        dir.path(),
        &["pr", "merge", &number, "--level", "2", "--actor", ACTOR],
    );
    let branches = ok(dir.path(), &["branch"]);
    let main = branches
        .stdout
        .lines()
        .find(|line| line.trim_start_matches(['*', ' ']).starts_with("main "))
        .expect("main is listed");
    assert!(main.contains("Merge pull request"), "{main}");
    let short = main.split_whitespace().nth(1).unwrap();
    assert!(merged.stdout.contains(short), "{}", merged.stdout);
    let again = run(
        dir.path(),
        &["pr", "merge", &number, "--level", "2", "--actor", ACTOR],
    );
    assert_eq!(again.code, 1, "{}", again.stdout);
}

/// [`with_feature_branch`] with the working copy back on `main`, a pull request for `feat`, and a
/// passing check of another actor. Returns the number of the pull request.
fn ready_to_merge_on_main() -> (Dir, String) {
    let dir = with_feature_branch();
    ok(dir.path(), &["switch", "main", "--actor", ACTOR]);
    let number = open_pull(dir.path());
    ok(
        dir.path(),
        &[
            "check", "record", "feat", "--base", "main", "--result", "pass", "--actor", OTHER,
        ],
    );
    (dir, number)
}

#[test]
fn a_merge_brings_the_worktree_that_has_main_checked_out_to_the_new_tip() {
    let (dir, number) = ready_to_merge_on_main();
    assert!(!dir.path().join("b.txt").exists());

    let merged = ok(
        dir.path(),
        &["pr", "merge", &number, "--level", "2", "--actor", ACTOR],
    );

    assert!(
        merged.stdout.contains("Merged pull request")
            && merged.stdout.contains(
                "Brought the working copy of the worktree main to the tip of main as entry"
            )
            && merged.stdout.contains("To undo it: stemma op restore"),
        "{}",
        merged.stdout
    );
    assert_eq!(fs::read_to_string(dir.path().join("b.txt")).unwrap(), "b\n");
    // The working copy stands on the tip: nothing is left to bring along, and a commit works.
    let again = ok(dir.path(), &["switch", "main", "--actor", ACTOR]);
    assert_eq!(again.stdout, "Already on main\n");
    write(dir.path(), "c.txt", "c\n");
    track_ok(dir.path(), "c.txt");
    ok(dir.path(), &["commit", "-m", "next", "--actor", ACTOR]);
}

#[test]
fn a_merge_carries_unsaved_changes_of_the_worktree_onto_the_new_tip() {
    let (dir, number) = ready_to_merge_on_main();
    write(dir.path(), "a.txt", "a\nmine\n");

    let merged = ok(
        dir.path(),
        &["pr", "merge", &number, "--level", "2", "--actor", ACTOR],
    );

    assert!(
        merged
            .stdout
            .contains("Saved the working copy first as entry")
            && merged
                .stdout
                .contains("Carried the changes of the working copy to the new base"),
        "{}",
        merged.stdout
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "a\nmine\n"
    );
    assert_eq!(fs::read_to_string(dir.path().join("b.txt")).unwrap(), "b\n");
}

#[test]
fn a_worktree_of_another_actor_stays_behind_and_its_owner_brings_it_along() {
    let (dir, number) = ready_to_merge_on_main();

    let merged = ok(
        dir.path(),
        &["pr", "merge", &number, "--level", "2", "--actor", OTHER],
    );

    assert!(
        merged.stdout.contains("Merged pull request")
            && merged.stdout.contains("The worktree main at ")
            && merged
                .stdout
                .contains(&format!("its working copy belongs to {ACTOR}"))
            && merged
                .stdout
                .contains("`stemma switch main` there brings it along"),
        "{}",
        merged.stdout
    );
    assert!(!dir.path().join("b.txt").exists());
    let listed = ok(dir.path(), &["pr", "list"]);
    assert!(listed.stdout.contains("merged feat"), "{}", listed.stdout);

    let planned = ok(
        dir.path(),
        &["switch", "main", "--dry-run", "--actor", ACTOR],
    );
    assert!(
        planned.stdout.starts_with(
            "Would bring the working copy to the tip of branch main; nothing is written\n"
        ),
        "{}",
        planned.stdout
    );
    assert!(!dir.path().join("b.txt").exists());
    let followed = ok(dir.path(), &["switch", "main", "--actor", ACTOR]);
    assert!(
        followed
            .stdout
            .starts_with("Brought the working copy to the tip of branch main as entry "),
        "{}",
        followed.stdout
    );
    assert_eq!(fs::read_to_string(dir.path().join("b.txt")).unwrap(), "b\n");
}

#[test]
fn a_file_in_the_way_leaves_the_worktree_behind_and_the_merge_stands() {
    let (dir, number) = ready_to_merge_on_main();
    // Not tracked, and where the change has a file of its own.
    write(dir.path(), "b.txt", "mine\n");
    let before = common::entry_count(dir.path());

    let merged = ok(
        dir.path(),
        &["pr", "merge", &number, "--level", "2", "--actor", ACTOR],
    );

    // The second step wrote nothing, so the command is complete: only the two entries of the gate.
    assert_eq!(common::entry_count(dir.path()), before + 2);
    assert!(
        merged.stdout.contains("Merged pull request")
            && merged
                .stdout
                .contains("has main checked out, but it did not follow:")
            && merged
                .stdout
                .contains("`stemma switch main` there brings it along"),
        "{}",
        merged.stdout
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("b.txt")).unwrap(),
        "mine\n"
    );
    let listed = ok(dir.path(), &["pr", "list"]);
    assert!(listed.stdout.contains("merged feat"), "{}", listed.stdout);
    // Once the file is out of the way, the switch brings the working copy along.
    fs::remove_file(dir.path().join("b.txt")).unwrap();
    ok(dir.path(), &["switch", "main", "--actor", ACTOR]);
    assert_eq!(fs::read_to_string(dir.path().join("b.txt")).unwrap(), "b\n");
}

#[test]
fn a_switch_that_stops_after_its_entry_ends_the_merge_with_3_and_the_merge_stands() {
    let (dir, number) = ready_to_merge_on_main();

    // Writing `b.txt` into the working directory fails, after the entry of the switch.
    let merged = common::run_failing(
        dir.path(),
        &["pr", "merge", &number, "--level", "2", "--actor", ACTOR],
        &[("write", dir.path())],
    );

    assert_eq!(merged.code, 3, "{}{}", merged.stdout, merged.stderr);
    assert!(
        merged.stdout.contains("Merged pull request"),
        "{}",
        merged.stdout
    );
    assert!(
        merged
            .stderr
            .contains(&format!("pull request {number} is merged"))
            && merged
                .stderr
                .contains("the worktree main did not follow `main`"),
        "{}",
        merged.stderr
    );
    let listed = ok(dir.path(), &["pr", "list"]);
    assert!(listed.stdout.contains("merged feat"), "{}", listed.stdout);
    // The same switch finishes what stopped.
    ok(dir.path(), &["switch", "main", "--actor", ACTOR]);
    assert_eq!(fs::read_to_string(dir.path().join("b.txt")).unwrap(), "b\n");
}

#[test]
fn a_merge_from_another_worktree_names_the_worktree_that_followed_in_the_undo_hint() {
    let dir = with_feature_branch();
    let number = open_pull(dir.path());
    ok(
        dir.path(),
        &[
            "check", "record", "feat", "--base", "main", "--result", "pass", "--actor", OTHER,
        ],
    );
    // The main worktree stays on `feat`; a linked worktree has `main` checked out.
    let place = Dir::new();
    let side = place.path().join("side");
    ok(
        dir.path(),
        &[
            "worktree",
            "add",
            side.to_str().unwrap(),
            "main",
            "--actor",
            ACTOR,
        ],
    );

    let merged = ok(
        dir.path(),
        &["pr", "merge", &number, "--level", "2", "--actor", ACTOR],
    );

    assert!(
        merged
            .stdout
            .contains("Brought the working copy of the worktree side to the tip of main as entry"),
        "{}",
        merged.stdout
    );
    assert_eq!(fs::read_to_string(side.join("b.txt")).unwrap(), "b\n");
    // The hint is a command that works from where the merge ran.
    let hint = merged
        .stdout
        .lines()
        .find_map(|line| line.strip_prefix("To undo it: stemma "))
        .expect("the output names the undo");
    assert!(hint.contains(" -C "), "{hint}");
    let mut args: Vec<&str> = hint.split(' ').collect();
    args.extend(["--actor", ACTOR]);
    ok(dir.path(), &args);
    assert!(!side.join("b.txt").exists());
}

#[test]
fn bad_arguments_are_refused_before_anything_is_written() {
    let dir = with_feature_branch();
    let before = ok(dir.path(), &["op", "log"]).stdout;
    for args in [
        &[
            "pr",
            "open",
            "feat",
            "--reviewer",
            "garbage",
            "--actor",
            ACTOR,
        ][..],
        &[
            "pr",
            "open",
            "feat",
            "--repairs",
            "nothing",
            "--actor",
            ACTOR,
        ][..],
    ] {
        let result = run(dir.path(), args);
        assert_eq!(result.code, 1, "{args:?}: {}", result.stdout);
    }
    for args in [
        &[
            "check", "record", "feat", "--result", "bogus", "--actor", OTHER,
        ][..],
        &["lease", "take", "feat", "--minutes", "0", "--actor", ACTOR][..],
    ] {
        let result = run(dir.path(), args);
        assert_eq!(result.code, 2, "{args:?}: {}", result.stdout);
    }
    assert_eq!(ok(dir.path(), &["op", "log"]).stdout, before);
}

#[test]
fn a_check_signed_with_a_key_of_another_actor_is_refused_and_writes_nothing() {
    let keys = Dir::new();
    let (Some(owner), Some(other)) = (
        generate_key(keys.path(), "owner"),
        generate_key(keys.path(), "other"),
    ) else {
        return;
    };
    let dir = enforcing(&owner, &other);
    let before = ok(dir.path(), &["op", "log"]).stdout;
    let result = run(
        dir.path(),
        &[
            "check", "record", "feat", "--result", "pass", "--key", &owner, "--actor", OTHER,
        ],
    );
    assert_eq!(result.code, 1, "{}", result.stdout);
    assert!(
        result.stderr.contains("nothing is written"),
        "{}",
        result.stderr
    );
    assert_eq!(ok(dir.path(), &["op", "log"]).stdout, before);
    assert_eq!(ok(dir.path(), &["op", "verify"]).code, 0);
}
#[test]
fn an_override_with_the_key_of_another_actor_is_refused_and_the_lease_stays() {
    let keys = Dir::new();
    let (Some(owner), Some(other)) = (
        generate_key(keys.path(), "owner"),
        generate_key(keys.path(), "other"),
    ) else {
        return;
    };
    let dir = enforcing(&owner, &other);
    ok(dir.path(), &["lease", "take", "feat", "--actor", OTHER]);
    let before = ok(dir.path(), &["op", "log"]).stdout;
    let result = run(
        dir.path(),
        &[
            "lease", "override", "feat", "--reason", "stuck", "--key", &other, "--actor", ACTOR,
        ],
    );
    assert_eq!(result.code, 1, "{}", result.stdout);
    assert!(
        result.stderr.contains("nothing is written"),
        "{}",
        result.stderr
    );
    assert_eq!(ok(dir.path(), &["op", "log"]).stdout, before);
    let shown = ok(dir.path(), &["lease", "show", "feat"]);
    assert!(
        shown.stdout.contains("held by human other"),
        "{}",
        shown.stdout
    );
}
