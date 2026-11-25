//! The tests of the pinning of the key of a hub in the configuration of a remote (ADR-026).

use super::*;
use crate::protocol::record::tests::{example_receipt, example_update, signed_former, Seeded};
use crate::protocol::record::ReceiptError;
use crate::protocol::{FormerKey, FormerKeys, ParseError};

const ADDRESS: &str = "https://hub.example/talent-factory/stemma";
const ID: &str = "0123456789abcdef0123456789abcdef";

/// The key of the hub that signed the example receipt (entry 98).
fn old_hub() -> Seeded {
    Seeded::new(2)
}

/// The key of the hub after it rotated.
fn new_hub() -> Seeded {
    Seeded::new(0x50)
}

fn config() -> RemoteConfig {
    RemoteConfig::new(
        RemoteAddress::try_new(ADDRESS).unwrap(),
        ID.parse().unwrap(),
    )
}

/// A list of references of the hub with the key of `current` and the former keys that it signed.
fn list(current: &Seeded, former: Vec<FormerKey>) -> RefList {
    let former = if former.is_empty() {
        FormerKeys::none()
    } else {
        signed_former(current, former)
    };
    let list = RefList::try_new(ID.parse().unwrap(), current.public(), former, Vec::new()).unwrap();
    // What the clone pins is what it read, so every list goes through the reader.
    RefList::parse(&list.encode()).unwrap()
}

#[test]
fn the_first_contact_pins_the_key_and_reports_its_fingerprint() {
    let mut config = config();
    assert_eq!(config.pinned_key(), Err(HubKeyError::NotPinned));
    assert_eq!(
        config.hub_keys(&list(&old_hub(), Vec::new())).unwrap_err(),
        HubKeyError::NotPinned
    );
    let fingerprint = config
        .pin_first_contact(&list(&old_hub(), Vec::new()), None)
        .unwrap();
    assert_eq!(fingerprint, old_hub().public().fingerprint());
    assert!(fingerprint.to_string().starts_with("SHA256:"));
    assert_eq!(config.pinned_key(), Ok(&old_hub().public()));
    // The pinned key is written into the file and read back.
    assert_eq!(RemoteConfig::parse(&config.encode()).unwrap(), config);
    // The same key again is no change.
    assert_eq!(
        config.pin_first_contact(&list(&old_hub(), Vec::new()), None),
        Ok(fingerprint)
    );
    assert_eq!(config.pinned_key(), Ok(&old_hub().public()));
}

#[test]
fn the_first_contact_with_an_expected_fingerprint_pins_only_that_key() {
    let expected = old_hub().public().fingerprint();
    let mut config = config();
    // Another hub answers under the address: nothing is pinned.
    assert_eq!(
        config.pin_first_contact(&list(&new_hub(), Vec::new()), Some(&expected)),
        Err(HubKeyError::Unexpected {
            expected,
            offered: new_hub().public().fingerprint(),
        })
    );
    assert_eq!(config.hub_key, None);
    assert_eq!(
        config.pin_first_contact(&list(&old_hub(), Vec::new()), Some(&expected)),
        Ok(expected)
    );
    assert_eq!(config.hub_key, Some(old_hub().public()));
}

#[test]
fn a_changed_key_is_a_typed_error_and_changes_nothing() {
    let mut config = config();
    config
        .pin_first_contact(&list(&old_hub(), Vec::new()), None)
        .unwrap();
    let pinned = config.clone();
    // The hub rotated its key and names the old one as a former key: the clone does not follow.
    let rotated = list(
        &new_hub(),
        vec![FormerKey::try_new(1, 98, old_hub().public()).unwrap()],
    );
    let changed = HubKeyError::Changed {
        pinned: old_hub().public().fingerprint(),
        offered: new_hub().public().fingerprint(),
    };
    assert_eq!(config.hub_keys(&rotated).unwrap_err(), changed);
    assert_eq!(
        config.pin_first_contact(&rotated, None).unwrap_err(),
        changed
    );
    // Not even with the fingerprint of the new key: a first contact never replaces a key.
    let new = new_hub().public().fingerprint();
    assert_eq!(
        config.pin_first_contact(&rotated, Some(&new)).unwrap_err(),
        changed
    );
    assert_eq!(config, pinned);
    let message = changed.to_string();
    assert!(message.contains(&new.to_string()), "{message}");
    assert!(message.contains("pin the key again"), "{message}");
    crate::fuzz::check_rejection(&changed);
    for error in [
        HubKeyError::NotPinned,
        HubKeyError::Unexpected {
            expected: new,
            offered: new,
        },
    ] {
        assert!(!error.to_string().is_empty());
        crate::fuzz::check_rejection(&error);
    }
}

#[test]
fn an_old_record_entry_is_checked_through_the_list_of_former_keys() {
    // The receipt of entry 98 was signed by the old key; the clone pinned the new one.
    let receipt = example_receipt();
    assert_eq!(receipt.number(), 98);
    let mut config = config();
    config.hub_key = Some(new_hub().public());
    let rotated = list(
        &new_hub(),
        vec![FormerKey::try_new(1, 98, old_hub().public()).unwrap()],
    );
    let keys = config.hub_keys(&rotated).unwrap();
    assert_eq!(keys.current(), &new_hub().public());
    assert_eq!(keys.at(98), &old_hub().public());
    assert_eq!(keys.at(99), &new_hub().public());
    assert!(keys.signed(&receipt));
    // A list whose former keys do not cover the entry leaves it to the current key, which did
    // not sign it.
    let short = list(
        &new_hub(),
        vec![FormerKey::try_new(1, 97, old_hub().public()).unwrap()],
    );
    assert!(!config.hub_keys(&short).unwrap().signed(&receipt));
    // A receipt is new and is checked against the pinned key alone: the one of the old key is
    // refused.
    assert_eq!(
        receipt.check_receipt(&example_update(), config.pinned_key().unwrap()),
        Err(ReceiptError::OtherKey)
    );
    assert_eq!(
        receipt.check_receipt(&example_update(), &old_hub().public()),
        Ok(())
    );
}

#[test]
fn a_forged_list_of_former_keys_is_refused() {
    let mut config = config();
    config.hub_key = Some(new_hub().public());
    let genuine = list(
        &new_hub(),
        vec![FormerKey::try_new(1, 98, old_hub().public()).unwrap()],
    );
    let text = String::from_utf8(genuine.encode()).unwrap();
    // Someone between the clone and the hub widens the range of the former key, or puts a key
    // of their own in its place: the signature of the current key no longer fits.
    let refused = |forged: String| {
        assert_ne!(forged, text);
        RefList::parse(forged.as_bytes()).unwrap_err()
    };
    for forged in [
        text.replacen("former 1 98 ", "former 1 99 ", 1),
        text.replacen(
            &old_hub().public().to_string(),
            &Seeded::new(0x51).public().to_string(),
            1,
        ),
    ] {
        assert!(
            matches!(refused(forged), ParseError::FormerKeys { line: 5, .. }),
            "{text}"
        );
    }
    // Without its signature the list is cut and refused.
    let signature_line = text
        .lines()
        .find(|line| line.starts_with("former-signature "))
        .unwrap();
    assert!(matches!(
        refused(text.replacen(&format!("{signature_line}\n"), "", 1)),
        ParseError::Malformed { line: 5, .. }
    ));
    // A list that another key signed in whole is read, and refused by what the clone pinned.
    let foreign = list(
        &Seeded::new(0x51),
        vec![FormerKey::try_new(1, 98, Seeded::new(0x52).public()).unwrap()],
    );
    assert!(matches!(
        config.hub_keys(&foreign),
        Err(HubKeyError::Changed { .. })
    ));
}
