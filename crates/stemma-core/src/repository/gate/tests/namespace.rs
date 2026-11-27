//! The local gate evaluates the namespace policy that the last fetch of each remote kept, beside
//! the policy of `T`, as the gate on a hub does (ADR-027, STEMMA-311).

use super::*;
use crate::namespace::KeptNamespace;
use crate::policy::Policy;
use crate::remote::{self, RemoteAddress, RemoteConfig, RemoteName};

fn origin() -> RemoteName {
    RemoteName::try_new("origin").unwrap()
}

/// Configures `remote` and keeps `policy` as its namespace policy, as a fetch does.
fn keep(f: &Fixture, remote: &RemoteName, policy: &str) {
    let config = RemoteConfig::new(
        RemoteAddress::try_new("https://hub.example/course/work").unwrap(),
        "00112233445566778899aabbccddeeff".parse().unwrap(),
    );
    let metadata = f.repository.metadata_dir();
    remote::write_config(&metadata, &metadata.join("tmp"), remote, &config).unwrap();
    keep_only(f, remote, policy);
}

/// Keeps `policy` for `remote` without configuring it.
fn keep_only(f: &Fixture, remote: &RemoteName, policy: &str) {
    let kept = KeptNamespace::new(f.base, Policy::parse(policy.as_bytes()).unwrap());
    f.repository.keep_namespace(remote, Some(&kept)).unwrap();
}

#[test]
fn the_kept_namespace_policy_raises_the_level_of_the_local_gate() {
    let f = fixture("zero\n");
    f.land_other();
    land_policy(&f, "stemma-policy 1\nlevel 0 a.txt\n");
    // A result for an older tip counts at level 0 only.
    f.check(f.base);
    keep(&f, &origin(), "stemma-policy 2\nlevel 2 a.txt\n");
    let tip = f.main();

    let error = gate_by_policy(&f, &Mode::Open).unwrap_err();

    assert!(
        matches!(
            error,
            GateError::NotChecked {
                level: RiskLevel::Two,
                ..
            }
        ),
        "{error}"
    );
    assert_eq!(f.main(), tip);

    // A fetch whose list names no namespace policy removes it, and the change lands at level 0.
    f.repository.keep_namespace(&origin(), None).unwrap();
    let merged = gate_by_policy(&f, &Mode::Open).unwrap();
    assert_eq!(f.main(), merged.commit);
}

#[test]
fn a_kept_namespace_policy_never_lowers_the_policy_of_the_tip() {
    let f = fixture("zero\n");
    f.land_other();
    land_policy(&f, "stemma-policy 1\nlevel 2 a.txt\n");
    f.check(f.base);
    keep(&f, &origin(), "stemma-policy 2\nlevel 0 a.txt\n");

    let error = gate_by_policy(&f, &Mode::Open).unwrap_err();

    assert!(
        matches!(
            error,
            GateError::NotChecked {
                level: RiskLevel::Two,
                ..
            }
        ),
        "{error}"
    );
}

#[test]
fn the_namespace_policy_of_a_removed_remote_counts_no_more() {
    let f = fixture("zero\n");
    f.land_other();
    land_policy(&f, "stemma-policy 1\nlevel 0 a.txt\n");
    f.check(f.base);
    // The file stays when the configuration goes; without the configuration it is not read.
    keep_only(&f, &origin(), "stemma-policy 2\nlevel 3 a.txt\n");

    let merged = gate_by_policy(&f, &Mode::Open).unwrap();
    assert_eq!(f.main(), merged.commit);
}

#[test]
fn a_damaged_kept_namespace_policy_refuses_the_merge() {
    let f = fixture("zero\n");
    land_policy(&f, "stemma-policy 1\nlevel 0 a.txt\n");
    f.check(f.main());
    keep(&f, &origin(), "stemma-policy 2\n");
    let path = f
        .repository
        .metadata_dir()
        .join("namespaces")
        .join("origin");
    fs::write(&path, "stemma-namespace 1\n").unwrap();
    let tip = f.main();

    let error = gate_by_policy(&f, &Mode::Open).unwrap_err();

    assert!(matches!(error, GateError::Namespace(_)), "{error}");
    assert_eq!(f.main(), tip);
}

#[test]
fn the_kept_namespace_policy_is_read_again_when_main_moves_between_the_attempts() {
    let f = fixture("zero\n");
    let policy = "stemma-policy 1\nlevel 0 a.txt\nlevel 0 b.txt\n";
    let tip = land_policy(&f, policy);
    // A result for the tip: at level 0 it also counts after `main` moves on.
    f.check(tip);
    keep(
        &f,
        &origin(),
        "stemma-policy 2\nlevel 0 a.txt\nlevel 0 b.txt\n",
    );
    let other = commit_of(
        &f.repository,
        tip,
        &[
            ("b.txt", "b\n"),
            ("c.txt", "zero\n"),
            (crate::policy::FILE_NAME, policy),
        ],
        "zoe",
    );
    let path = f.path();
    // Between the first attempt and its lock, `main` moves and a fetch brings a namespace policy
    // that raises `a.txt`: the second attempt must judge by it.
    pause::queue(move |_| {
        let repository = Repository::open(&path).unwrap();
        set_branch(&repository, "main", Some(tip), other);
        let kept = KeptNamespace::new(
            other,
            Policy::parse(b"stemma-policy 2\nlevel 2 a.txt\nlevel 0 b.txt\n").unwrap(),
        );
        repository.keep_namespace(&origin(), Some(&kept)).unwrap();
    });

    let error = gate_by_policy(&f, &Mode::Open).unwrap_err();

    assert!(
        matches!(
            error,
            GateError::NotChecked {
                level: RiskLevel::Two,
                ..
            }
        ),
        "{error}"
    );
    assert_eq!(f.main(), other);
}
