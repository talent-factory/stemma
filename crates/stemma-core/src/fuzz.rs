//! The checks of the fuzz targets in `fuzz/` (STEMMA-20), one for each of the strict readers of
//! a tree, a commit, an actor line, the index, an entry of the log, and the journal of a purge.
//! The readers of HEAD and of a branch file are a single line and have only the property tests.
//! The module exists only when the crate is built with `--cfg fuzzing`, as `cargo fuzz` does, or
//! for its tests, so it is not part of the interface. The tests run the checks on the seeds in
//! `fuzz/seeds/` with stable, so that `just ci` notices a seed that a reader no longer accepts.
//!
//! Each check holds the two rules of ADR-005 for one input, as `properties.rs` does for generated
//! input: the reader either accepts the input, and then encoding what it read gives exactly the
//! input, or it rejects it with a message and a chain of causes that ends. The reader of the
//! journal returns `None`, so only its round trip is checked. A panic, a second
//! spelling, or a reader that does not stop is a finding. The property tests run on every
//! `just ci`; the fuzzer follows the coverage of the readers and finds the inputs that the
//! generator of the property tests does not make.

use std::error::Error;

use crate::index::Index;
use crate::object::{Commit, Commit2, Tree};
use crate::oplog::Entry;
use crate::Actor;

/// A tree (`Tree::parse`).
pub fn tree(input: &[u8]) {
    match Tree::parse(input) {
        Ok(tree) => assert_eq!(tree.encode(), input, "a second spelling was accepted"),
        Err(error) => check_rejection(&error),
    }
}

/// A commit (`Commit::parse`).
pub fn commit(input: &[u8]) {
    match Commit::parse(input) {
        Ok(commit) => assert_eq!(commit.encode(), input, "a second spelling was accepted"),
        Err(error) => check_rejection(&error),
    }
}

/// A commit with a list of trees (`Commit2::parse`).
pub fn commit2(input: &[u8]) {
    match Commit2::parse(input) {
        Ok(commit) => assert_eq!(commit.encode(), input, "a second spelling was accepted"),
        Err(error) => check_rejection(&error),
    }
}

/// An actor line (`Actor::from_str`). The reader takes text, so input that is not UTF-8 is
/// skipped; the readers of commits and entries reject it before they read the actor.
pub fn actor(input: &[u8]) {
    let Ok(line) = std::str::from_utf8(input) else {
        return;
    };
    match line.parse::<Actor>() {
        Ok(actor) => assert_eq!(actor.to_string(), line, "a second spelling was accepted"),
        Err(error) => check_rejection(&error),
    }
}

/// An index (`Index::parse`).
pub fn index(input: &[u8]) {
    match Index::parse(input) {
        Ok(index) => assert_eq!(index.encode(), input, "a second spelling was accepted"),
        Err(error) => check_rejection(&error),
    }
}

/// An entry of the operation log in either encoding (`Entry::parse`).
pub fn entry(input: &[u8]) {
    match Entry::parse(input) {
        Ok(entry) => assert_eq!(entry.encode(), input, "a second spelling was accepted"),
        Err(error) => check_rejection(&error),
    }
}

/// The journal of a purge. Its reader returns `None` for a rejection, so only the round trip is
/// checked.
pub fn journal(input: &[u8]) {
    crate::repository::fuzz_journal(input);
}

/// A rejection has a message, and its chain of causes ends without an empty link and without a
/// link that repeats the message of the next one (crate documentation).
fn check_rejection(error: &dyn Error) {
    assert!(!error.to_string().is_empty(), "empty message");
    let mut link = error;
    for _ in 0..8 {
        let Some(next) = link.source() else {
            return;
        };
        let (outer, inner) = (link.to_string(), next.to_string());
        assert!(!inner.is_empty(), "empty cause");
        assert!(
            !outer.contains(&inner),
            "{outer:?} has the message of its source {inner:?}"
        );
        link = next;
    }
    panic!("the cause chain does not end");
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fs;
    use std::path::PathBuf;

    /// The seeds of a target, by name, from `fuzz/seeds/TARGET`.
    pub(crate) fn seeds(target: &str) -> Vec<(String, Vec<u8>)> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fuzz/seeds")
            .join(target);
        let mut seeds: Vec<(String, Vec<u8>)> = fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
            .map(|file| {
                let path = file.unwrap().path();
                (path.display().to_string(), fs::read(&path).unwrap())
            })
            .collect();
        seeds.sort();
        assert!(!seeds.is_empty(), "no seeds in {}", dir.display());
        seeds
    }

    /// Every seed is accepted, so that the fuzzer starts from valid encodings, and the check of
    /// its target holds for it and for each of its prefixes, which the readers mostly reject.
    #[test]
    fn every_seed_is_read_and_the_checks_hold_for_it_and_its_prefixes() {
        type Check = fn(&[u8]);
        type Accepts = fn(&[u8]) -> bool;
        let targets: [(&str, Check, Accepts); 6] = [
            ("tree", super::tree, |b| {
                crate::object::Tree::parse(b).is_ok()
            }),
            ("commit", super::commit, |b| {
                crate::object::Commit::parse(b).is_ok()
            }),
            ("commit2", super::commit2, |b| {
                crate::object::Commit2::parse(b).is_ok()
            }),
            ("actor", super::actor, |b| {
                std::str::from_utf8(b).is_ok_and(|s| s.parse::<crate::Actor>().is_ok())
            }),
            ("index", super::index, |b| {
                crate::index::Index::parse(b).is_ok()
            }),
            ("entry", super::entry, |b| {
                crate::oplog::Entry::parse(b).is_ok()
            }),
        ];
        for (target, check, accepts) in targets {
            for (name, seed) in seeds(target) {
                assert!(accepts(&seed), "{name} is not accepted");
                for end in 0..=seed.len() {
                    check(&seed[..end]);
                }
            }
        }
    }
}
