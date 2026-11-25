//! The checks of the fuzz targets in `fuzz/` (STEMMA-20), one for each of the strict readers of
//! a tree, a commit, an actor line, the index, an entry of the log, the journal of a purge, and the
//! policy file (whose check is a fixed point, see [`policy`]),
//! one for the readers of the protocol messages together ([`protocol`]), which include the ref
//! update, the record entry, and every signed statement,
//! and one for the merge of the lines of a file ([`merge_lines`]), which is no reader.
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

use crate::conflict::Merge;
use crate::diff::is_binary;
use crate::index::Index;
use crate::merge;
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

/// A policy file (`Policy::parse`). Comments and empty lines are not kept, so the check is not that
/// the encoding is the input but that it is a fixed point: the canonical form of an accepted file
/// is accepted, is an equal policy, gives the same form again, and has the same settings and the
/// same levels for the probed paths. A panic in the reader or in the evaluation is a finding as
/// well.
pub fn policy(input: &[u8]) {
    use crate::oplog::OperationName;
    use crate::policy::{EntryKind, Policy, RiskLevel, OPERATION_FLOORS};
    match Policy::parse(input) {
        Ok(policy) => {
            let canonical = policy.encode();
            let again = Policy::parse(canonical.as_bytes()).expect("the canonical form is read");
            assert_eq!(again, policy, "the canonical form is another policy");
            assert_eq!(again.encode(), canonical, "the canonical form changed");
            assert_eq!(again.lease(), policy.lease(), "the lease changed");
            // The settings do not change, for the floors, the levels, and the names in the file.
            for level in (0..4).filter_map(RiskLevel::from_number) {
                assert_eq!(again.agent_approvals(level), policy.agent_approvals(level));
            }
            let lines = std::str::from_utf8(input).expect("an accepted policy file is UTF-8");
            let names = lines
                .lines()
                .filter_map(|line| line.strip_prefix("operation "))
                .filter_map(|rest| rest.split(' ').next())
                .chain(OPERATION_FLOORS.map(|(name, _)| name))
                .filter_map(|name| OperationName::try_new(name).ok());
            for name in names {
                assert_eq!(again.operation_level(&name), policy.operation_level(&name));
            }
            // The evaluation never panics and does not depend on the spelling of the file. The
            // probes are the patterns of the file as paths (wildcards as letters, no slash at
            // the ends) and the names that a policy treats apart.
            let probes = lines
                .lines()
                .filter_map(|line| line.strip_prefix("level "))
                .filter_map(|rest| rest.split_once(' '))
                .map(|(_, pattern)| pattern.replace(['*', '?'], "x"))
                .map(|pattern| pattern.trim_matches('/').to_owned())
                .chain([".stemma-policy", ".stemma-actors/a", "a", "a/b"].map(str::to_owned));
            // The floor holds for every policy.
            for path in [".stemma-policy", ".stemma-actors", ".stemma-actors/a"] {
                assert_eq!(
                    policy.level_of_path(path, EntryKind::File),
                    Ok(RiskLevel::Three),
                    "{path} is below level 3"
                );
            }
            for path in probes {
                for kind in [EntryKind::File, EntryKind::Directory] {
                    assert_eq!(
                        again.level_of_path(&path, kind),
                        policy.level_of_path(&path, kind),
                        "the level of {path:?} changed"
                    );
                }
            }
        }
        Err(error) => check_rejection(&error),
    }
}

/// The longest body of an object that the check of [`protocol`] lets the reader of a stream
/// accept, so that the fuzzer reaches the refusal of a larger one.
const PROTOCOL_MAX_OBJECT_LEN: u64 = 1 << 16;

/// The messages of the protocol (ADR-023): the list of references, the fetch request, the error,
/// the object stream (`stemma_core::protocol`), the ref update and the record entry
/// (`stemma_core::protocol::record`, see [`protocol_record`]), and the signed statements of
/// every kind (`stemma_core::protocol::statement`, see [`protocol_statement`]). The input is
/// given to each of the
/// readers, because a message does not name its type and each reader must refuse the messages of
/// the others. A reader that accepts the input writes it back unchanged, and for a small message
/// the value it read is one that the constructors build. A refusal is answered with an error
/// message that is read back, where there is an answer, and a reader of a stream that refused
/// stays failed.
pub fn protocol(input: &[u8]) {
    use crate::protocol::{
        ErrorMessage, FetchRequest, FormerKey, ObjectStreamReader, ObjectStreamWriter, ParseError,
        Reason, RefList, Spoken, StreamError,
    };
    // A refusal is answered with a message that the reader of the error accepts.
    let check_refusal = |error: &ParseError| {
        check_rejection(error);
        let answer = ErrorMessage::from(error);
        assert_eq!(
            ErrorMessage::parse(&answer.encode()).as_ref(),
            Ok(&answer),
            "the answer to a refusal is not read"
        );
    };
    // An accepted message is also what the constructors accept: the reader builds its values
    // itself, so the round trip alone would not see a reader that takes an order, a range, or a
    // reason that the writer refuses.
    match RefList::parse(input) {
        Ok(list) => {
            assert_eq!(list.encode(), input, "a second spelling was accepted");
            let former = list
                .former()
                .iter()
                .map(|key| FormerKey::try_new(key.first(), key.last(), key.key().clone()))
                .collect::<Result<Vec<_>, _>>()
                .expect("the reader took a range that the writer refuses");
            let rebuilt = RefList::try_new(
                list.repository(),
                list.key().clone(),
                former,
                list.references().to_vec(),
            );
            assert_eq!(
                rebuilt.as_ref(),
                Ok(&list),
                "the reader and the writer disagree"
            );
        }
        Err(error) => check_refusal(&error),
    }
    match FetchRequest::parse(input) {
        Ok(request) => {
            assert_eq!(request.encode(), input, "a second spelling was accepted");
            let rebuilt = FetchRequest::try_new(
                request.want().iter().copied(),
                request.have().iter().copied(),
            );
            assert_eq!(
                rebuilt.as_ref(),
                Ok(&request),
                "the reader and the writer disagree"
            );
        }
        Err(error) => check_refusal(&error),
    }
    match ErrorMessage::parse(input) {
        Ok(message) => {
            assert_eq!(message.encode(), input, "a second spelling was accepted");
            let rebuilt = match &message {
                ErrorMessage::UnsupportedVersion(spoken) => {
                    Spoken::try_new(spoken.versions().iter().copied())
                        .map(ErrorMessage::UnsupportedVersion)
                }
                ErrorMessage::Malformed(reason) => {
                    Reason::try_new(reason.as_str()).map(ErrorMessage::Malformed)
                }
            };
            assert_eq!(
                rebuilt.as_ref(),
                Ok(&message),
                "the reader and the writer disagree"
            );
        }
        Err(error) => check_refusal(&error),
    }
    protocol_record(input, &check_refusal);
    protocol_statement(input, &check_refusal);
    let stream = ObjectStreamReader::new(input, PROTOCOL_MAX_OBJECT_LEN).and_then(|mut reader| {
        let mut writer = ObjectStreamWriter::new(Vec::new()).expect("a vector takes every byte");
        loop {
            match reader.next_object() {
                Ok(Some(object)) => {
                    assert!(object.body.len() as u64 <= PROTOCOL_MAX_OBJECT_LEN);
                    writer
                        .write(object.kind, &object.body)
                        .expect("a vector takes every byte");
                }
                Ok(None) => return Ok(writer.finish()),
                Err(error) => {
                    assert!(
                        matches!(reader.next_object(), Err(StreamError::Failed)),
                        "the reader went on after a refusal"
                    );
                    return Err(error);
                }
            }
        }
    });
    match stream {
        Ok(written) => assert_eq!(written, input, "a second spelling was accepted"),
        Err(error) => {
            assert!(error.io_error().is_none(), "a slice does not fail");
            check_rejection(&error);
            if let Some(answer) = error.answer() {
                assert_eq!(
                    ErrorMessage::parse(&answer.encode()).as_ref(),
                    Ok(&answer),
                    "the answer to a refusal is not read"
                );
            }
        }
    }
}

/// The part of [`protocol`] for the ref update and the record entry (ADR-025). A fuzzer makes no
/// signature, so the form is read without the check of the signatures: what that reader accepts
/// is written back unchanged and is a value that the constructors build. The reader that checks
/// the signatures accepts the same value or refuses for a signature alone, and it refuses
/// everything else for the reason of the form. A record, which is entries one after the other,
/// is written back unchanged as well.
fn protocol_record(input: &[u8], check_refusal: &dyn Fn(&crate::protocol::ParseError)) {
    use crate::protocol::record::{
        read_record, HubAddress, Move, RecordEntry, RecordProblem, RefUpdate, RefUpdatePayload,
    };
    use crate::protocol::ParseError;
    let rebuilt = |update: &RefUpdate| {
        let payload = update.payload();
        let moves = payload
            .moves()
            .iter()
            .map(|moved| {
                Move::try_new(
                    moved.name().clone(),
                    moved.previous(),
                    moved.old_commit(),
                    moved.new_commit(),
                    moved.evidence(),
                )
            })
            .collect::<Result<Vec<_>, _>>()
            .expect("the reader took a move that the writer refuses");
        let hub = HubAddress::try_new(payload.hub().as_str())
            .expect("the reader took an address that the writer refuses");
        let rebuilt =
            RefUpdatePayload::try_new(hub, payload.repository(), moves, payload.key().clone());
        assert_eq!(
            rebuilt.as_ref(),
            Ok(payload),
            "the reader and the writer disagree"
        );
    };
    let only_a_signature = |error: &ParseError, reasons: &[RecordProblem]| {
        assert!(
            matches!(error, ParseError::Record { reason, .. } if reasons.contains(reason)),
            "a message whose form is right was refused for its form: {error}"
        );
    };
    match RefUpdate::parse_unverified(input) {
        Ok(update) => {
            assert_eq!(update.encode(), input, "a second spelling was accepted");
            rebuilt(&update);
            match RefUpdate::parse(input) {
                Ok(verified) => assert_eq!(verified, update),
                Err(error) => {
                    only_a_signature(
                        &error,
                        &[RecordProblem::ActorKey, RecordProblem::ActorSignature],
                    );
                    check_refusal(&error);
                }
            }
        }
        Err(error) => {
            check_refusal(&error);
            assert_eq!(RefUpdate::parse(input).as_ref(), Err(&error));
        }
    }
    match RecordEntry::parse_unverified(input) {
        Ok(entry) => {
            assert_eq!(entry.encode(), input, "a second spelling was accepted");
            rebuilt(entry.update());
            match RecordEntry::parse(input) {
                Ok(verified) => assert_eq!(verified, entry),
                Err(error) => {
                    only_a_signature(
                        &error,
                        &[
                            RecordProblem::ActorKey,
                            RecordProblem::ActorSignature,
                            RecordProblem::HubSignature,
                        ],
                    );
                    check_refusal(&error);
                }
            }
        }
        Err(error) => {
            check_refusal(&error);
            assert_eq!(RecordEntry::parse(input).as_ref(), Err(&error));
        }
    }
    match read_record(input) {
        Ok(entries) => {
            let written: Vec<u8> = entries.iter().flat_map(RecordEntry::encode).collect();
            assert_eq!(written, input, "a second spelling was accepted");
        }
        Err(error) => {
            check_rejection(&error);
            check_refusal(&error.error);
        }
    }
}

/// The part of [`protocol`] for the signed statements (ADR-025): the opening of a pull request,
/// an approval, the result of a check, a closing, and an owner override, which one reader reads
/// by the line that names the kind. A fuzzer makes no signature, so the form is read without
/// the check of the signature: what that reader accepts is written back unchanged and is a
/// value that the constructor of its kind builds, and it has a hash. The reader that checks the
/// signature accepts the same value or refuses for the signature alone, which it never does for
/// an unsigned statement, and it refuses everything else for the reason of the form.
fn protocol_statement(input: &[u8], check_refusal: &dyn Fn(&crate::protocol::ParseError)) {
    use crate::protocol::statement::{
        Approval, Body, Checked, Closing, Opening, OwnerOverride, Statement, StatementProblem,
    };
    use crate::protocol::ParseError;
    let statement = match Statement::parse_unverified(input) {
        Ok(statement) => statement,
        Err(error) => {
            check_refusal(&error);
            assert_eq!(Statement::parse(input).as_ref(), Err(&error));
            return;
        }
    };
    assert_eq!(statement.encode(), input, "a second spelling was accepted");
    let repository = statement.body().repository();
    let rebuilt = match statement.body() {
        Body::Opening(body) => Opening::try_new(
            repository,
            body.branch().clone(),
            body.head(),
            body.reviewers().to_vec(),
            body.repairs(),
        )
        .map(Body::Opening),
        Body::Approval(body) => Ok(Body::Approval(Approval::new(
            repository,
            body.pull(),
            body.head(),
        ))),
        Body::Check(body) => {
            Checked::try_new(repository, body.commit(), body.base(), body.result()).map(Body::Check)
        }
        Body::Closing(body) => Ok(Body::Closing(Closing::new(repository, body.pull()))),
        Body::Override(body) => OwnerOverride::try_new(
            repository,
            body.branch().clone(),
            body.holder().clone(),
            body.commit(),
            body.reason(),
        )
        .map(Body::Override),
    };
    assert_eq!(
        rebuilt.as_ref(),
        Ok(statement.body()),
        "the reader and the writer disagree"
    );
    assert_eq!(statement.is_signed(), statement.key().is_some());
    assert_eq!(statement.is_signed(), statement.signature().is_some());
    // The hash is a function of the message.
    assert_eq!(statement.hash(), statement.clone().hash());
    match Statement::parse(input) {
        Ok(verified) => assert_eq!(verified, statement),
        Err(error) => {
            assert!(
                statement.is_signed(),
                "an unsigned statement was refused for a signature"
            );
            assert!(
                matches!(
                    error,
                    ParseError::Statement {
                        reason: StatementProblem::Key | StatementProblem::Signature,
                        ..
                    }
                ),
                "a message whose form is right was refused for its form: {error}"
            );
            check_refusal(&error);
        }
    }
}

/// The byte that separates the three texts of an input of [`merge_lines`].
pub const MERGE_SEPARATOR: u8 = 0x1e;

/// The merge of the lines of a file (`merge::merge_lines`). The input is the base, ours, and
/// theirs, separated by [`MERGE_SEPARATOR`] (further separators stay in theirs); a missing part is
/// empty.
///
/// The merge has no encoding to round-trip, so the check holds the rules of the merge for one
/// input. A binary text or a text that is not UTF-8 is never merged. For text, the sides can be
/// swapped without a change of the result. Two equal sides give that side, a side that equals the
/// base gives the other side, and a result is built only from lines that one of the three texts
/// has. A panic, or a result that depends on which side is ours, is a finding.
pub fn merge_lines(input: &[u8]) {
    let mut parts = input.splitn(3, |&byte| byte == MERGE_SEPARATOR);
    let mut next = || parts.next().unwrap_or_default();
    let (base, ours, theirs) = (next(), next(), next());
    let merged = |ours: &[u8], base: &[u8], theirs: &[u8]| {
        merge::merge_lines(&Merge::try_from_terms(vec![ours, base, theirs]).expect("three terms"))
    };

    let result = merged(ours, base, theirs);
    assert_eq!(
        result,
        merged(theirs, base, ours),
        "the result depends on the side"
    );
    let is_text = |text: &[u8]| !is_binary(text) && std::str::from_utf8(text).is_ok();
    if ![base, ours, theirs].into_iter().all(is_text) {
        assert_eq!(
            result, None,
            "a binary file or a file that is not UTF-8 was merged"
        );
        return;
    }
    assert_eq!(
        merged(ours, base, ours),
        Some(ours.to_vec()),
        "two equal sides"
    );
    assert_eq!(
        merged(base, base, theirs),
        Some(theirs.to_vec()),
        "only theirs changed"
    );
    assert_eq!(
        merged(ours, base, base),
        Some(ours.to_vec()),
        "only ours changed"
    );
    if let Some(text) = result {
        let known: Vec<&[u8]> = [base, ours, theirs]
            .into_iter()
            .flat_map(|text| text.split_inclusive(|&byte| byte == b'\n'))
            .collect();
        for line in text.split_inclusive(|&byte| byte == b'\n') {
            assert!(known.contains(&line), "a line that no text has");
        }
    }
}

/// A rejection has a message, and its chain of causes ends without an empty link and without a
/// link that repeats the message of the next one (crate documentation).
pub(crate) fn check_rejection(error: &dyn Error) {
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
        let targets: [(&str, Check, Accepts); 8] = [
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
            ("policy", super::policy, |b| {
                crate::policy::Policy::parse(b).is_ok()
            }),
            ("protocol", super::protocol, |b| {
                use crate::protocol::record::{RecordEntry, RefUpdate};
                use crate::protocol::statement::Statement;
                use crate::protocol::{ErrorMessage, FetchRequest, ObjectStreamReader, RefList};
                RefList::parse(b).is_ok()
                    || Statement::parse(b).is_ok()
                    || RefUpdate::parse(b).is_ok()
                    || RecordEntry::parse(b).is_ok()
                    || FetchRequest::parse(b).is_ok()
                    || ErrorMessage::parse(b).is_ok()
                    || ObjectStreamReader::new(b, u64::MAX).is_ok_and(|mut reader| loop {
                        match reader.next_object() {
                            Ok(Some(_)) => {}
                            last => break last.is_ok(),
                        }
                    })
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

    /// The seeds of the protocol messages are the checked-in example messages, so the fuzzer starts
    /// from every message that the examples show.
    #[test]
    fn the_seeds_of_the_protocol_are_the_example_messages() {
        let seeds = seeds("protocol");
        for example in crate::protocol::examples::ALL {
            assert!(
                seeds.iter().any(|(_, seed)| seed == example.bytes),
                "the example {} is no seed",
                example.name
            );
        }
        assert_eq!(seeds.len(), crate::protocol::examples::ALL.len());
    }

    /// Every seed of the line merge is three texts that merge, and the check holds for it and for
    /// each of its prefixes.
    #[test]
    fn every_seed_of_the_line_merge_merges_and_the_check_holds_for_its_prefixes() {
        for (name, seed) in seeds("merge_lines") {
            let mut parts = seed.splitn(3, |&byte| byte == super::MERGE_SEPARATOR);
            let mut next = || parts.next().unwrap_or_default();
            let (base, ours, theirs) = (next(), next(), next());
            let sum = crate::conflict::Merge::try_from_terms(vec![ours, base, theirs]).unwrap();
            assert!(
                crate::merge::merge_lines(&sum).is_some(),
                "{name} does not merge"
            );
            for end in 0..=seed.len() {
                super::merge_lines(&seed[..end]);
            }
        }
    }
}
