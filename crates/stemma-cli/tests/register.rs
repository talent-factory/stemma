//! The actor register, run as a process: `init --owner-key`, `register`, and `op verify`.

mod common;

use std::fs;
use std::path::Path;

use common::{ok, run, track_ok, Dir, ACTOR};

const OWNER_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMt6BisKCOOMdkWowXYqXUrS1zKF4aE6QenzZ1SSB/Md";
const OTHER_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAII7AEa9Zc0DvSxekMomLCSklfUdMJnm9WBbvZbqwYow/";
const REGISTER: &str = ".stemma-actors/register";

/// Writes a key file into a directory of its own, outside the repository, and returns its path.
fn key_file(keys: &Dir, name: &str, key: &str) -> String {
    let path = keys.path().join(name);
    fs::write(&path, format!("{key} comment\n")).unwrap();
    path.to_str().unwrap().to_owned()
}

/// An enforcing repository whose owner is the actor of the tests, with the register committed.
fn enforcing() -> (Dir, Dir) {
    let keys = Dir::new();
    let owner = key_file(&keys, "owner.pub", OWNER_KEY);
    let dir = Dir::new();
    ok(
        dir.path(),
        &["init", "--actor", ACTOR, "--owner-key", &owner],
    );
    (dir, keys)
}

fn commit_register(dir: &Path) {
    track_ok(dir, REGISTER);
    ok(dir, &["commit", "-m", "register", "--actor", ACTOR]);
}

#[test]
fn init_with_an_owner_key_writes_the_register_and_the_marker() {
    let (dir, _keys) = enforcing();
    assert_eq!(
        fs::read_to_string(dir.path().join(REGISTER)).unwrap(),
        format!("stemma-register 1\nentry human daniel owner\nkey {OWNER_KEY} active\n")
    );
    assert!(dir.path().join(".stemma/enforcing").is_file());
}

#[test]
fn init_with_an_owner_key_says_that_there_is_no_way_back() {
    let keys = Dir::new();
    let owner = key_file(&keys, "owner.pub", OWNER_KEY);
    let dir = Dir::new();
    let result = ok(
        dir.path(),
        &["init", "--actor", ACTOR, "--owner-key", &owner],
    );
    assert!(
        result.stdout.contains("Enforcing mode"),
        "{}",
        result.stdout
    );
    assert!(result.stdout.contains("no way back"), "{}", result.stdout);
}

#[test]
fn an_agent_cannot_own_the_register_and_nothing_is_made() {
    let keys = Dir::new();
    let owner = key_file(&keys, "owner.pub", OWNER_KEY);
    let dir = Dir::new();
    let agent = "agent bot model=m session=s sponsor=daniel";
    let result = run(
        dir.path(),
        &["init", "--actor", agent, "--owner-key", &owner],
    );
    assert_eq!(result.code, 1, "{}", result.stdout);
    assert!(
        result.stderr.contains("an agent cannot be an owner"),
        "{}",
        result.stderr
    );
    assert!(!dir.path().join(".stemma").exists());
}

#[test]
fn a_key_file_that_is_no_ed25519_key_leaves_no_repository() {
    let keys = Dir::new();
    let path = keys.path().join("bad.pub");
    fs::write(&path, "ssh-rsa AAAA nonsense\n").unwrap();
    let dir = Dir::new();
    let result = run(
        dir.path(),
        &[
            "init",
            "--actor",
            ACTOR,
            "--owner-key",
            path.to_str().unwrap(),
        ],
    );
    assert_eq!(result.code, 1, "{}", result.stdout);
    assert!(!dir.path().join(".stemma").exists());
    let missing = keys.path().join("missing.pub");
    let result = run(
        dir.path(),
        &[
            "init",
            "--actor",
            ACTOR,
            "--owner-key",
            missing.to_str().unwrap(),
        ],
    );
    assert_eq!(result.code, 1);
    assert!(!dir.path().join(".stemma").exists());
}

#[test]
fn op_verify_checks_the_register_of_an_enforcing_repository() {
    let (dir, _keys) = enforcing();
    commit_register(dir.path());
    let result = ok(dir.path(), &["op", "verify"]);
    assert!(
        result
            .stdout
            .contains("The actor register is valid, and 0 proofs of log entries hold against it."),
        "{}",
        result.stdout
    );
}

#[test]
fn op_verify_in_an_open_repository_says_nothing_about_a_register() {
    let dir = common::repository();
    let result = ok(dir.path(), &["op", "verify"]);
    assert!(!result.stdout.contains("register"), "{}", result.stdout);
}

#[test]
fn op_verify_refuses_a_proof_that_is_not_a_proof() {
    let (dir, _keys) = enforcing();
    commit_register(dir.path());
    let proofs = dir.path().join(".stemma/proofs");
    fs::create_dir_all(&proofs).unwrap();
    fs::write(proofs.join("000000000001"), "not a proof\n").unwrap();
    let result = run(dir.path(), &["op", "verify"]);
    assert_eq!(result.code, 5, "{}", result.stdout);
    assert!(
        result.stderr.contains("proof of entry 1"),
        "{}",
        result.stderr
    );
}

#[test]
fn an_enforcing_repository_without_its_register_is_damaged_and_not_open() {
    let (dir, _keys) = enforcing();
    fs::remove_file(dir.path().join(REGISTER)).unwrap();
    let result = run(dir.path(), &["op", "verify"]);
    assert_eq!(result.code, 5, "{}", result.stdout);
    assert!(result.stderr.contains("no way back"), "{}", result.stderr);
}

#[test]
fn an_owner_rotates_a_key_and_retires_the_old_one() {
    let (dir, keys) = enforcing();
    commit_register(dir.path());
    let old = key_file(&keys, "owner.pub", OWNER_KEY);
    let new = key_file(&keys, "new.pub", OTHER_KEY);
    // The only owner key cannot go before another one is there.
    let result = run(dir.path(), &["register", "retire", &old, "--actor", ACTOR]);
    assert_eq!(result.code, 1, "{}", result.stdout);
    assert!(
        result.stderr.contains("no owner with an active key"),
        "{}",
        result.stderr
    );
    ok(dir.path(), &["register", "add-key", &new, "--actor", ACTOR]);
    let result = ok(dir.path(), &["register", "retire", &old, "--actor", ACTOR]);
    assert!(
        result.stdout.contains("from log entry"),
        "{}",
        result.stdout
    );
    let text = fs::read_to_string(dir.path().join(REGISTER)).unwrap();
    assert!(
        text.contains(&format!("key {OWNER_KEY} retired ")),
        "{text}"
    );
    assert!(text.contains(&format!("key {OTHER_KEY} active")), "{text}");
    // The changed file is a normal change: the register of the base commit stays valid for verify.
    ok(dir.path(), &["op", "verify"]);
}

#[test]
fn a_revoked_key_is_written_with_its_position_in_the_log() {
    let (dir, keys) = enforcing();
    commit_register(dir.path());
    let old = key_file(&keys, "owner.pub", OWNER_KEY);
    let new = key_file(&keys, "new.pub", OTHER_KEY);
    ok(dir.path(), &["register", "add-key", &new, "--actor", ACTOR]);
    let entries = fs::read_dir(dir.path().join(".stemma/oplog"))
        .unwrap()
        .count();
    ok(dir.path(), &["register", "revoke", &old, "--actor", ACTOR]);
    let text = fs::read_to_string(dir.path().join(REGISTER)).unwrap();
    assert!(
        text.contains(&format!("key {OWNER_KEY} revoked {}", entries + 1)),
        "{text}"
    );
    // A second revocation is refused.
    let result = run(dir.path(), &["register", "revoke", &old, "--actor", ACTOR]);
    assert_eq!(result.code, 1);
    assert!(
        result.stderr.contains("revoked already"),
        "{}",
        result.stderr
    );
}

#[test]
fn only_an_owner_changes_the_entry_of_another_identity() {
    let (dir, keys) = enforcing();
    let other = key_file(&keys, "other.pub", OTHER_KEY);
    let result = run(
        dir.path(),
        &[
            "register",
            "add-key",
            &other,
            "--actor",
            "human mia",
            "--identity",
            "human daniel",
        ],
    );
    assert_eq!(result.code, 1, "{}", result.stdout);
    assert!(
        result.stderr.contains("is not an owner"),
        "{}",
        result.stderr
    );
    // The register is as it was.
    let text = fs::read_to_string(dir.path().join(REGISTER)).unwrap();
    assert!(!text.contains(OTHER_KEY), "{text}");
    // Mia has no entry of her own.
    let result = run(
        dir.path(),
        &["register", "add-key", &other, "--actor", "human mia"],
    );
    assert_eq!(result.code, 1);
    assert!(
        result.stderr.contains("no entry for human mia"),
        "{}",
        result.stderr
    );
}

#[test]
fn an_open_repository_has_no_register_to_change() {
    let dir = common::repository();
    let keys = Dir::new();
    let key = key_file(&keys, "k.pub", OWNER_KEY);
    let result = run(dir.path(), &["register", "retire", &key, "--actor", ACTOR]);
    assert_eq!(result.code, 1, "{}", result.stdout);
    assert!(
        result.stderr.contains("init --owner-key"),
        "{}",
        result.stderr
    );
}
