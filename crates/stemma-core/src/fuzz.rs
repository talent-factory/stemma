//! The checks of the fuzz targets in `fuzz/` (STEMMA-20), one for each of the strict readers of
//! a tree, a commit, an actor line, the index, an entry of the log, and the journal of a purge.
//! The readers of HEAD and of a branch file are a single line and have only the property tests.
//! The module exists only when the crate is built with `--cfg fuzzing`, as `cargo fuzz` does, so
//! it is not part of the interface.
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
use crate::object::{Commit, Tree};
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
