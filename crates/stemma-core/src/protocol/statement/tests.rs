//! The tests of the signed statements: the texts of the formats, the rules of the constructors,
//! the refusals of the reader one rule at a time, the signatures, and the hash.

use super::*;
use crate::fuzz::check_rejection;
use crate::protocol::record::tests::Seeded;
use crate::signature::{SignError, KEY_PROBE};

const REPOSITORY: &str = "00112233445566778899aabbccddeeff";

/// The text of an ID whose bytes are all `byte`.
fn id_text(byte: u8) -> String {
    format!("b3:{}", format!("{byte:02x}").repeat(32))
}

fn commit(byte: u8) -> CommitId {
    CommitId::new(id_text(byte).parse().unwrap())
}

fn repository() -> Option<RepositoryId> {
    Some(REPOSITORY.parse().unwrap())
}

fn branch(name: &str) -> BranchName {
    BranchName::try_new(name).unwrap()
}

fn identity(text: &str) -> Identity {
    text.parse().unwrap()
}

/// The key of the opener and owner of these tests.
fn daniel() -> Seeded {
    Seeded::new(1)
}

/// The key of the reviewer of these tests.
fn mia() -> Seeded {
    Seeded::new(3)
}

/// The key of the runner of the checks of these tests.
fn runner() -> Seeded {
    Seeded::new(4)
}

fn opening() -> Opening {
    Opening::try_new(
        repository(),
        branch("feat/login"),
        commit(0x22),
        vec![identity("human mia"), identity("agent reviewer")],
        None,
    )
    .unwrap()
}

/// What the checked-in example of a kind says, and the key that signs it.
fn example_body(kind: StatementKind) -> (Body, Seeded) {
    let pull = || example(StatementKind::Opening).hash();
    match kind {
        StatementKind::Opening => (Body::Opening(opening()), daniel()),
        StatementKind::Approval => (
            Body::Approval(Approval::new(repository(), pull(), commit(0x22))),
            mia(),
        ),
        StatementKind::Check => (
            Body::Check(
                Checked::try_new(
                    repository(),
                    commit(0x22),
                    Some(commit(0x11)),
                    CheckResult::Pass,
                )
                .unwrap(),
            ),
            runner(),
        ),
        StatementKind::Closing => (Body::Closing(Closing::new(repository(), pull())), daniel()),
        StatementKind::Override => (
            Body::Override(
                OwnerOverride::try_new(
                    repository(),
                    branch("feat/login"),
                    identity("human mia"),
                    Some(commit(0x22)),
                    "the holder is away, and the release waits",
                )
                .unwrap(),
            ),
            daniel(),
        ),
    }
}

/// The statement of the checked-in example `statement-<kind>`. Its key comes from a fixed seed,
/// so its signature is reproducible; nobody may trust it.
pub(crate) fn example(kind: StatementKind) -> Statement {
    let (body, signer) = example_body(kind);
    Statement::sign(body, &signer).unwrap()
}

/// The statement of the checked-in example `statement-unsigned`: an opening of the open mode,
/// which names no repository of a hub, no key, and no signature, and declares a repair.
pub(crate) fn example_unsigned() -> Statement {
    Statement::unsigned(Body::Opening(
        Opening::try_new(
            None,
            branch("fix/red-tip"),
            commit(0x33),
            Vec::new(),
            Some(commit(0x11)),
        )
        .unwrap(),
    ))
}

/// The message of a signed statement with these lines of the kind, signed by `signer` over the
/// lines as they are, without any check of a constructor: only a rule of the reader can refuse
/// it.
fn forged(kind: StatementKind, lines: &str, signer: &Seeded) -> Vec<u8> {
    let payload = format!("repository {REPOSITORY}\n{lines}key {}\n", signer.public());
    let signature = signature::sign(signer, kind.payload_kind(), payload.as_bytes()).unwrap();
    format!(
        "stemma-protocol 1\nstatement {kind}\n{payload}signature {}\n",
        signature_to_line(&signature)
    )
    .into_bytes()
}

fn refusal(bytes: &[u8]) -> ParseError {
    let error = Statement::parse(bytes).expect_err("the reader accepted the message");
    check_rejection(&error);
    error
}

fn statement_problem(bytes: &[u8]) -> (usize, StatementProblem) {
    match refusal(bytes) {
        ParseError::Statement { line, reason } => (line, reason),
        other => panic!("refused for another reason: {other}"),
    }
}

fn shape_problem(bytes: &[u8]) -> (usize, Malformed) {
    match refusal(bytes) {
        ParseError::Malformed { line, reason } => (line, reason),
        other => panic!("refused for another reason: {other}"),
    }
}

#[test]
fn the_five_statements_have_these_texts() {
    let key = |signer: &Seeded| signer.public().to_string();
    let pull = example(StatementKind::Opening).hash();
    let expected = [
        (
            StatementKind::Opening,
            format!(
                "branch feat/login\ncommit {}\nreviewer agent reviewer\nreviewer human mia\n\
                 repairs -\n",
                id_text(0x22)
            ),
            key(&daniel()),
        ),
        (
            StatementKind::Approval,
            format!("pull {pull}\ncommit {}\n", id_text(0x22)),
            key(&mia()),
        ),
        (
            StatementKind::Check,
            format!(
                "commit {}\nbase {}\nresult pass\n",
                id_text(0x22),
                id_text(0x11)
            ),
            key(&runner()),
        ),
        (
            StatementKind::Closing,
            format!("pull {pull}\n"),
            key(&daniel()),
        ),
        (
            StatementKind::Override,
            format!(
                "branch feat/login\nholder human mia\ncommit {}\n\
                 reason the holder is away, and the release waits\n",
                id_text(0x22)
            ),
            key(&daniel()),
        ),
    ];
    for (kind, lines, key) in expected {
        let statement = example(kind);
        let payload = format!("repository {REPOSITORY}\n{lines}key {key}\n");
        assert_eq!(
            String::from_utf8(statement.payload()).unwrap(),
            payload,
            "{kind}"
        );
        let message = format!(
            "stemma-protocol 1\nstatement {kind}\n{payload}signature {}\n",
            signature_to_line(statement.signature().unwrap())
        );
        assert_eq!(String::from_utf8(statement.encode()).unwrap(), message);
        let read = Statement::parse(message.as_bytes()).unwrap();
        assert_eq!(read, statement);
        assert_eq!(read.kind(), kind);
        assert_eq!(read.body().repository(), repository());
        assert!(read.is_signed());
        assert_eq!(read.key().unwrap().to_string(), key);
    }
}

#[test]
fn the_kinds_have_these_names_and_a_type_tag_of_their_own() {
    let names: Vec<&str> = StatementKind::ALL.iter().map(|kind| kind.name()).collect();
    assert_eq!(
        names,
        [
            "pr-opening",
            "pr-approval",
            "check-result",
            "pr-closing",
            "owner-override"
        ]
    );
    // No tag of a statement is the tag of a proof from before the statements, so a signature
    // of one never stands for the other.
    for old in [
        PayloadKind::LogEntry,
        PayloadKind::Approval,
        PayloadKind::Sponsorship,
        PayloadKind::RefUpdate,
        PayloadKind::RecordEntry,
    ] {
        assert!(!names.contains(&old.name()), "{}", old.name());
    }
    for kind in StatementKind::ALL {
        assert_eq!(StatementKind::from_name(kind.name()), Some(kind));
        assert_eq!(kind.to_string(), kind.name());
    }
    assert_eq!(StatementKind::from_name("approval"), None);
}

#[test]
fn an_unsigned_statement_names_no_key_and_no_signature() {
    let statement = example_unsigned();
    let expected = format!(
        "stemma-protocol 1\nstatement pr-opening\nrepository -\nbranch fix/red-tip\n\
         commit {}\nrepairs {}\nkey -\nsignature -\n",
        id_text(0x33),
        id_text(0x11)
    );
    assert_eq!(String::from_utf8(statement.encode()).unwrap(), expected);
    let read = Statement::parse(expected.as_bytes()).unwrap();
    assert_eq!(read, statement);
    assert!(!read.is_signed());
    assert_eq!(read.key(), None);
    assert_eq!(read.signature(), None);
    assert_eq!(read.body().repository(), None);
    let Body::Opening(opening) = read.body() else {
        panic!("{read:?}")
    };
    assert_eq!(opening.branch(), &branch("fix/red-tip"));
    assert_eq!(opening.head(), commit(0x33));
    assert!(opening.reviewers().is_empty());
    assert_eq!(opening.repairs(), Some(commit(0x11)));
}

#[test]
fn a_statement_with_a_key_and_no_signature_or_the_other_way_round_is_refused() {
    let signed = String::from_utf8(example(StatementKind::Closing).encode()).unwrap();
    let signature_line = signed.lines().last().unwrap();
    let without_signature = signed.replace(signature_line, "signature -");
    assert_eq!(
        statement_problem(without_signature.as_bytes()),
        (6, StatementProblem::HalfSigned)
    );
    let key_line = signed.lines().nth(4).unwrap();
    assert!(key_line.starts_with("key ssh-ed25519 "));
    let without_key = signed.replace(key_line, "key -");
    assert_eq!(
        statement_problem(without_key.as_bytes()),
        (6, StatementProblem::HalfSigned)
    );
}

#[test]
fn the_hash_covers_the_whole_message_and_names_the_pull_request() {
    let opening = example(StatementKind::Opening);
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"stemma-statement-hash 1\n");
    hasher.update(&opening.encode());
    assert_eq!(
        opening.hash().to_string(),
        format!("b3:{}", hasher.finalize().to_hex())
    );
    // The approval and the closing of the examples name the pull request by that hash.
    for kind in [StatementKind::Approval, StatementKind::Closing] {
        let named = match example(kind).body() {
            Body::Approval(approval) => approval.pull(),
            Body::Closing(closing) => closing.pull(),
            other => panic!("{other:?}"),
        };
        assert_eq!(named, opening.hash());
    }
    // The same opening of another key, and the same opening without a signature, is another
    // pull request: the hash covers the key and the signature.
    let other_key = Statement::sign(Body::Opening(super::tests::opening()), &mia()).unwrap();
    let unsigned = Statement::unsigned(Body::Opening(super::tests::opening()));
    assert_ne!(other_key.hash(), opening.hash());
    assert_ne!(unsigned.hash(), opening.hash());
    assert_ne!(unsigned.hash(), other_key.hash());
    // A signature of Ed25519 is deterministic, so signing again gives the same statement.
    assert_eq!(example(StatementKind::Opening).hash(), opening.hash());
    let hashes: std::collections::HashSet<StatementHash> = StatementKind::ALL
        .iter()
        .map(|kind| example(*kind).hash())
        .collect();
    assert_eq!(hashes.len(), StatementKind::ALL.len());
}

#[test]
fn every_statement_names_its_commits() {
    let commits = |kind| example(kind).body().commits();
    assert_eq!(commits(StatementKind::Opening), [commit(0x22)]);
    assert_eq!(commits(StatementKind::Approval), [commit(0x22)]);
    assert_eq!(commits(StatementKind::Check), [commit(0x22), commit(0x11)]);
    assert!(commits(StatementKind::Closing).is_empty());
    assert_eq!(commits(StatementKind::Override), [commit(0x22)]);
    assert_eq!(
        example_unsigned().body().commits(),
        [commit(0x33), commit(0x11)]
    );
}

#[test]
fn the_constructors_hold_the_rules_of_the_kinds() {
    let open = |name: &str, reviewers: Vec<Identity>| {
        Opening::try_new(None, branch(name), commit(1), reviewers, None)
    };
    assert_eq!(
        open("main", Vec::new()).unwrap_err(),
        StatementProblem::TargetBranch
    );
    // The reviewers are sorted by their text, and a repeat stands once.
    let sorted = open(
        "f",
        vec![
            identity("human mia"),
            identity("agent bot"),
            identity("human ana"),
            identity("human mia"),
        ],
    )
    .unwrap();
    assert_eq!(
        sorted.reviewers(),
        [
            identity("agent bot"),
            identity("human ana"),
            identity("human mia")
        ]
    );
    let many = |count: usize| -> Vec<Identity> {
        (0..count)
            .map(|index| identity(&format!("human r{index:02}")))
            .collect()
    };
    assert!(open("f", many(MAX_REVIEWERS)).is_ok());
    assert_eq!(
        open("f", many(MAX_REVIEWERS + 1)).unwrap_err(),
        StatementProblem::TooManyReviewers
    );
    assert_eq!(
        Checked::try_new(None, commit(1), Some(commit(1)), CheckResult::Pass).unwrap_err(),
        StatementProblem::BaseIsCommit
    );
    let run = Checked::try_new(None, commit(1), None, CheckResult::Fail).unwrap();
    assert_eq!(
        (run.commit(), run.base(), run.result()),
        (commit(1), None, CheckResult::Fail)
    );
    let over = |reason: &str| {
        OwnerOverride::try_new(None, branch("f"), identity("human mia"), None, reason)
    };
    let longest = "x".repeat(MAX_REASON_LEN);
    for reason in [
        "",
        " x",
        "x ",
        "a\nb",
        "a\tb",
        "a\u{2028}b",
        &format!("{longest}x"),
    ] {
        assert_eq!(
            over(reason).unwrap_err(),
            StatementProblem::Reason,
            "{reason:?}"
        );
    }
    let taken = over(&longest).unwrap();
    assert_eq!(taken.reason(), longest);
    assert_eq!(taken.holder(), &identity("human mia"));
    assert_eq!(taken.branch(), &branch("f"));
    assert_eq!(taken.commit(), None);
    assert!(over("für Zoë, bis Montag").is_ok());
}

#[test]
fn a_signature_stands_for_its_key_its_kind_and_its_payload_only() {
    let closing = example(StatementKind::Closing);
    let signature = closing.signature().unwrap().clone();
    let key = closing.key().unwrap().clone();
    // The same body under another key.
    assert_eq!(
        Statement::try_new(closing.body().clone(), mia().public(), signature.clone()).unwrap_err(),
        VerifyError::WrongKey
    );
    // Another body under the same key.
    let other = Body::Closing(Closing::new(None, example(StatementKind::Opening).hash()));
    assert_eq!(
        Statement::try_new(other, key.clone(), signature).unwrap_err(),
        VerifyError::Invalid
    );
    // The lines of a closing, signed as a proof of another type: a statement of the open mode
    // of phase 4 or of another kind never passes for this one.
    for tag in [PayloadKind::Approval, PayloadKind::PullOpening] {
        let foreign = signature::sign(&daniel(), tag, &closing.payload()).unwrap();
        assert_eq!(
            Statement::try_new(closing.body().clone(), key.clone(), foreign).unwrap_err(),
            VerifyError::Invalid
        );
    }
}

/// A signer that names one key and signs with another.
struct OtherKey;

impl Signer for OtherKey {
    fn sign(&self, message: &[u8]) -> Result<Signature, SignError> {
        mia().sign(message)
    }

    fn public_key(&self) -> Result<PublicKey, SignError> {
        Ok(daniel().public())
    }
}

/// A signer that fails.
struct Broken;

impl Signer for Broken {
    fn sign(&self, _: &[u8]) -> Result<Signature, SignError> {
        Err(SignError::Unreadable {
            reason: "broken".into(),
        })
    }
}

#[test]
fn signing_asks_the_signer_for_its_key_and_checks_what_it_returns() {
    // The default way to the key of a signer is a signature of the probe, which is no proof.
    assert_eq!(daniel().public_key().unwrap(), daniel().public());
    assert!(!KEY_PROBE.starts_with(crate::signature::NAMESPACE.as_bytes()));
    let body = || example(StatementKind::Closing).body().clone();
    let signed = Statement::sign(body(), &daniel()).unwrap();
    assert_eq!(signed.key(), Some(&daniel().public()));
    assert!(matches!(
        Statement::sign(body(), &OtherKey),
        Err(SealError::Verify(VerifyError::WrongKey))
    ));
    assert!(matches!(
        Statement::sign(body(), &Broken),
        Err(SealError::Sign(SignError::Unreadable { .. }))
    ));
}

#[test]
fn the_reader_refuses_a_changed_payload_a_changed_kind_and_another_key() {
    let closing = String::from_utf8(example(StatementKind::Closing).encode()).unwrap();
    // A changed payload: the signature names the key of the statement and does not verify.
    let changed = closing.replace(REPOSITORY, "ffeeddccbbaa99887766554433221100");
    assert_eq!(
        statement_problem(changed.as_bytes()),
        (6, StatementProblem::Signature)
    );
    // Another key in the `key` line: the signature names another key.
    let own = daniel().public().to_string();
    let other = closing.replace(&own, &mia().public().to_string());
    assert_eq!(
        statement_problem(other.as_bytes()),
        (6, StatementProblem::Key)
    );
    // The line `statement` is not signed, and the type tag stands for it: an approval and a
    // closing differ in their lines, so a renamed statement is refused for its form first, and
    // one whose lines fit another kind for its signature.
    let renamed = closing.replace("statement pr-closing", "statement pr-approval");
    assert_eq!(
        shape_problem(renamed.as_bytes()),
        (5, Malformed::Expected("commit"))
    );
    let lines = format!("pull {}\n", id_text(0x44));
    let mut as_closing =
        String::from_utf8(forged(StatementKind::Closing, &lines, &daniel())).unwrap();
    assert!(Statement::parse(as_closing.as_bytes()).is_ok());
    as_closing = as_closing.replace("statement pr-closing", "statement pr-opening");
    assert_eq!(
        shape_problem(as_closing.as_bytes()),
        (4, Malformed::Expected("branch"))
    );
}

#[test]
fn the_reader_refuses_lines_that_the_writer_does_not_write() {
    let a = id_text(0x22);
    let b = id_text(0x11);
    let opening = |lines: &str| forged(StatementKind::Opening, lines, &daniel());
    let check = |lines: &str| forged(StatementKind::Check, lines, &daniel());
    let over = |lines: &str| forged(StatementKind::Override, lines, &daniel());
    let pull = |lines: &str| forged(StatementKind::Approval, lines, &daniel());
    // What the constructors refuse, the reader refuses, at the line of the value.
    assert_eq!(
        statement_problem(&opening(&format!("branch main\ncommit {a}\nrepairs -\n"))),
        (4, StatementProblem::TargetBranch)
    );
    assert_eq!(
        statement_problem(&opening(&format!(
            "branch .hidden\ncommit {a}\nrepairs -\n"
        ))),
        (4, StatementProblem::Branch)
    );
    let reviewers: String = (0..=MAX_REVIEWERS)
        .map(|index| format!("reviewer human r{index:02}\n"))
        .collect();
    assert_eq!(
        statement_problem(&opening(&format!(
            "branch f\ncommit {a}\n{reviewers}repairs -\n"
        ))),
        (6 + MAX_REVIEWERS + 1, StatementProblem::TooManyReviewers)
    );
    assert_eq!(
        statement_problem(&check(&format!("commit {a}\nbase {a}\nresult pass\n"))),
        (5, StatementProblem::BaseIsCommit)
    );
    assert_eq!(
        statement_problem(&check(&format!("commit {a}\nbase -\nresult green\n"))),
        (6, StatementProblem::Result)
    );
    for reason in ["", " x", "x ", "a\tb"] {
        assert_eq!(
            statement_problem(&over(&format!(
                "branch f\nholder human mia\ncommit -\nreason {reason}\n"
            ))),
            (7, StatementProblem::Reason),
            "{reason:?}"
        );
    }
    // The reviewers stand in ascending order, each once, in the one text of an identity.
    for reviewers in [
        "reviewer human mia\nreviewer human ana\n",
        "reviewer human mia\nreviewer human mia\n",
    ] {
        assert_eq!(
            shape_problem(&opening(&format!(
                "branch f\ncommit {a}\n{reviewers}repairs -\n"
            ))),
            (7, Malformed::Order)
        );
    }
    for reviewer in ["mia", "human", "robot mia", "human  mia", "human m\u{e4}"] {
        assert_eq!(
            statement_problem(&opening(&format!(
                "branch f\ncommit {a}\nreviewer {reviewer}\nrepairs -\n"
            ))),
            (6, StatementProblem::Identity),
            "{reviewer:?}"
        );
    }
    assert_eq!(
        statement_problem(&over("branch f\nholder mia\ncommit -\nreason x\n")),
        (5, StatementProblem::Identity)
    );
    // A hash and a commit in canonical form only.
    assert_eq!(
        statement_problem(&pull(&format!("pull {}\ncommit {a}\n", a.to_uppercase()))),
        (4, StatementProblem::Pull)
    );
    assert_eq!(
        statement_problem(&pull(&format!("pull -\ncommit {a}\n"))),
        (4, StatementProblem::Pull)
    );
    assert!(matches!(
        refusal(&pull(&format!("pull {a}\ncommit -\n"))),
        ParseError::InvalidObjectId { line: 5, .. }
    ));
    // A line that is missing, repeated, unknown, or out of order.
    let cases: [(Vec<u8>, usize, Malformed); 6] = [
        (
            opening(&format!("branch f\ncommit {a}\n")),
            6,
            Malformed::Expected("repairs"),
        ),
        (
            opening(&format!("commit {a}\nbranch f\nrepairs -\n")),
            4,
            Malformed::Expected("branch"),
        ),
        (
            opening(&format!(
                "branch f\ncommit {a}\nrepairs -\nreviewer human mia\n"
            )),
            7,
            Malformed::Expected("key"),
        ),
        (
            check(&format!("commit {a}\nresult pass\n")),
            5,
            Malformed::Expected("base"),
        ),
        (
            check(&format!("commit {a}\nbase {b}\nresult pass\nresult pass\n")),
            7,
            Malformed::Expected("key"),
        ),
        (
            over("branch f\nholder human mia\ncommit -\nreason\n"),
            7,
            Malformed::Expected("reason"),
        ),
    ];
    for (bytes, line, reason) in cases {
        assert_eq!(
            shape_problem(&bytes),
            (line, reason),
            "{}",
            String::from_utf8_lossy(&bytes)
        );
    }
}

#[test]
fn the_reader_refuses_a_message_that_is_not_one_statement() {
    let closing = example(StatementKind::Closing).encode();
    let text = String::from_utf8(closing.clone()).unwrap();
    // A line after the signature, a second statement, and a message that is cut off.
    let mut trailing = closing.clone();
    trailing.extend_from_slice(b"note x\n");
    assert_eq!(shape_problem(&trailing), (7, Malformed::Unexpected));
    let mut twice = closing.clone();
    twice.extend_from_slice(&closing);
    assert_eq!(shape_problem(&twice), (7, Malformed::Unexpected));
    assert_eq!(
        shape_problem(&closing[..closing.len() - 1]),
        (6, Malformed::CutOff)
    );
    // An unknown kind, a missing kind, and another protocol version.
    assert_eq!(
        statement_problem(text.replace("pr-closing", "pr-merge").as_bytes()),
        (2, StatementProblem::Kind)
    );
    let without_kind = text.replace("statement pr-closing\n", "");
    assert_eq!(
        shape_problem(without_kind.as_bytes()),
        (2, Malformed::Expected("statement"))
    );
    assert_eq!(
        refusal(
            text.replace("stemma-protocol 1", "stemma-protocol 2")
                .as_bytes()
        ),
        ParseError::UnsupportedVersion { found: 2 }
    );
    // A repository that is neither an ID nor `-`, and a signature in another form.
    assert!(matches!(
        refusal(text.replace(REPOSITORY, "none").as_bytes()),
        ParseError::InvalidRepositoryId { line: 3, .. }
    ));
    let signature = text.lines().last().unwrap().to_owned();
    for other in [
        "signature x".to_owned(),
        format!("{signature}="),
        "signature".to_owned(),
    ] {
        let changed = text.replace(&signature, &other);
        match refusal(changed.as_bytes()) {
            ParseError::Statement {
                line: 6,
                reason: StatementProblem::SignatureForm,
            }
            | ParseError::Malformed {
                line: 6,
                reason: Malformed::Expected("signature"),
            } => {}
            error => panic!("{other:?}: {error}"),
        }
    }
    // A key with a comment is not the one text of the key.
    let key = daniel().public().to_string();
    assert_eq!(
        shape_problem(
            text.replace(&key, &format!("{key} daniel@laptop"))
                .as_bytes()
        ),
        (5, Malformed::KeyForm)
    );
}

#[test]
fn every_example_statement_is_read_back_and_passes_the_check_of_the_fuzz_target() {
    for statement in StatementKind::ALL
        .into_iter()
        .map(example)
        .chain([example_unsigned()])
    {
        let bytes = statement.encode();
        assert_eq!(Statement::parse(&bytes).unwrap(), statement);
        assert_eq!(Statement::parse_unverified(&bytes).unwrap(), statement);
        for end in 0..=bytes.len() {
            crate::fuzz::protocol(&bytes[..end]);
        }
    }
}
