use super::*;
use ssh_key::private::Ed25519Keypair;
use ssh_key::PrivateKey;

fn key(seed: u8) -> PublicKey {
    let private = PrivateKey::from(Ed25519Keypair::from_seed(&[seed; 32]));
    PublicKey::from_openssh(&private.public_key().to_openssh().unwrap()).unwrap()
}

fn register(text: &str) -> Register {
    Register::parse(text.as_bytes()).unwrap_or_else(|e| panic!("{text}: {e}"))
}

fn commit(byte: u8) -> CommitId {
    let text = format!("b3:{}", format!("{byte:02x}").repeat(32));
    CommitId::new(text.parse().unwrap())
}

fn policy(text: &str) -> Policy {
    Policy::parse(text.as_bytes()).unwrap()
}

/// The register of a namespace repository: the teacher and an assistant own it, a student is a
/// member, and the assistant has a retired key besides the active one.
fn namespace_register() -> Register {
    register(&format!(
        "stemma-register 1\n\
         entry human assistant owner\nkey {} active\nkey {} retired 3\n\
         entry human student member\nkey {} active\n\
         entry human teacher owner\nkey {} active\n",
        key(2),
        key(9),
        key(3),
        key(1),
    ))
}

const DEMANDING: &str = "stemma-policy 2\ndemand-namespace-owners\n";

#[test]
fn the_demanded_owners_are_the_human_owners_with_their_active_keys() {
    let namespace = Namespace::new(commit(1), policy(DEMANDING), &namespace_register()).unwrap();
    let owners: Vec<(String, Vec<PublicKey>)> = namespace
        .owners()
        .iter()
        .map(|owner| (owner.identity().to_string(), owner.keys().to_vec()))
        .collect();
    assert_eq!(
        owners,
        [
            ("human assistant".to_owned(), vec![key(2)]),
            ("human teacher".to_owned(), vec![key(1)]),
        ]
    );
    assert_eq!(namespace.commit(), commit(1));
}

#[test]
fn a_namespace_policy_without_the_line_demands_no_owner() {
    let namespace = Namespace::new(
        commit(1),
        policy("stemma-policy 2\n"),
        &namespace_register(),
    )
    .unwrap();
    assert!(namespace.owners().is_empty());
    let empty = register(&format!(
        "stemma-register 1\nentry human pupil owner\nkey {} active\n",
        key(7)
    ));
    assert!(namespace.missing_owner(&empty).is_none());
}

#[test]
fn a_register_without_a_demanded_owner_is_refused() {
    let namespace = Namespace::new(commit(1), policy(DEMANDING), &namespace_register()).unwrap();
    let holds_both = register(&format!(
        "stemma-register 1\n\
         entry human assistant owner\nkey {} active\n\
         entry human pupil owner\nkey {} active\n\
         entry human teacher owner\nkey {} active\n",
        key(2),
        key(7),
        key(1),
    ));
    assert!(namespace.missing_owner(&holds_both).is_none());

    let teacher = Identity::human("teacher").unwrap();
    let missing = |text: String| {
        namespace
            .missing_owner(&register(&text))
            .map(|owner| owner.identity().clone())
    };
    // Without the entry of the teacher.
    assert_eq!(
        missing(format!(
            "stemma-register 1\nentry human assistant owner\nkey {} active\n\
             entry human pupil owner\nkey {} active\n",
            key(2),
            key(7)
        )),
        Some(teacher.clone())
    );
    // The teacher as a member only.
    assert_eq!(
        missing(format!(
            "stemma-register 1\nentry human assistant owner\nkey {} active\n\
             entry human pupil owner\nkey {} active\nentry human teacher member\nkey {} active\n",
            key(2),
            key(7),
            key(1)
        )),
        Some(teacher.clone())
    );
    // The teacher as an owner with another key: the name alone is no owner of the namespace.
    assert_eq!(
        missing(format!(
            "stemma-register 1\nentry human assistant owner\nkey {} active\n\
             entry human teacher owner\nkey {} active\n",
            key(2),
            key(7)
        )),
        Some(teacher.clone())
    );
    // The teacher with the key retired.
    assert_eq!(
        missing(format!(
            "stemma-register 1\nentry human assistant owner\nkey {} active\n\
             entry human pupil owner\nkey {} active\n\
             entry human teacher owner\nkey {} retired 4\n",
            key(2),
            key(7),
            key(1)
        )),
        Some(teacher)
    );
}

#[test]
fn a_kept_namespace_policy_reads_back_and_refuses_every_other_form() {
    let kept = KeptNamespace::new(
        commit(5),
        policy("stemma-policy 2\ndemand-namespace-owners\nlevel 3 src/\n"),
    );
    let text = kept.encode();
    assert_eq!(
        text,
        format!(
            "stemma-namespace 1\ncommit {}\nstemma-policy 2\ndemand-namespace-owners\nlevel 3 src/\n",
            commit(5)
        )
    );
    assert_eq!(KeptNamespace::parse(text.as_bytes()).unwrap(), kept);
    for bad in [
        String::new(),
        text.replace("stemma-namespace 1", "stemma-namespace 2"),
        text.replace("commit ", "commit  "),
        text.replace("level 3 src/\n", "# comment\nlevel 3 src/\n"),
        text.replace("level 3 src/\n", "level 3 src/"),
        format!("{text}\n"),
    ] {
        assert!(KeptNamespace::parse(bad.as_bytes()).is_err(), "{bad:?}");
    }
    assert!(matches!(
        KeptNamespace::parse(text.replace("level 3", "level 9").as_bytes()),
        Err(KeptNamespaceError::Policy(_))
    ));
}

#[test]
fn a_valid_register_always_gives_a_demand_an_owner() {
    // The reader of a register refuses one without an owner with an active key, so the
    // refusal of a demand for nothing is a check that no valid register reaches.
    let only_retired = format!(
        "stemma-register 1\nentry human assistant owner\nkey {} retired 3\n",
        key(9)
    );
    assert!(Register::parse(only_retired.as_bytes()).is_err());
    assert!(Namespace::new(commit(1), policy(DEMANDING), &namespace_register()).is_ok());
}
