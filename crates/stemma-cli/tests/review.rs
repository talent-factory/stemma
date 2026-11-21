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
    // The register has no command that adds a second human, so the file is written as it is read.
    let register = dir.path().join(REGISTER);
    let other_pub = fs::read_to_string(format!("{other_key}.pub")).unwrap();
    let mut text = fs::read_to_string(&register).unwrap();
    let key = other_pub
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    text.push_str(&format!("entry human other member\nkey {key} active\n"));
    fs::write(&register, text).unwrap();
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
    let number = open_pull_as(dir.path(), ACTOR);

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
    assert!(verified.stdout.contains("proofs"), "{}", verified.stdout);
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

fn open_pull_as(dir: &Path, actor: &str) -> String {
    let result = ok(dir, &["pr", "open", "feat", "--actor", actor]);
    result
        .stdout
        .split_whitespace()
        .find(|word| word.chars().all(|c| c.is_ascii_digit()))
        .expect("the output names the number")
        .to_owned()
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
    let number = open_pull_as(dir.path(), ACTOR);
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
