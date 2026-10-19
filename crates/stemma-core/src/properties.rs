//! Property tests of the readers and encoders of trees, commits, indexes, and actor lines.
//!
//! The unit tests change a golden encoding at one place at a time. These tests complement them
//! with generated input and check the two rules of ADR-005 on all of it:
//!
//! 1. **One object, one encoding.** For every value that the code can build, `parse(encode(x))`
//!    gives `x` back. For every input that a reader accepts, whatever its origin, encoding the
//!    result gives exactly the input. A reader that accepted a second spelling, or an encoder that
//!    wrote something that its reader rejects, breaks one of the two.
//! 2. **Total readers.** A reader never panics, and every rejection has a message, a cause chain
//!    that ends, and a line number inside the input.
//!
//! The inputs come from a small generator with a fixed seed, so a failure is reproducible and the
//! test is not flaky. Inputs are built from valid encodings that are changed in several places at
//! once, spliced together, assembled from tokens of the format, and drawn as raw bytes. Set
//! `STEMMA_PROPERTY_SEED` to explore other inputs and `STEMMA_PROPERTY_CASES` to run more of them;
//! a failure prints both values.

use std::error::Error;

use crate::index::{FileMode, Index, IndexParseError};
use crate::object::{
    Blob, BlobId, Commit, CommitId, CommitParseError, Tree, TreeEntry, TreeId, TreeParseError,
};
use crate::{Actor, ActorParseError};

/// The seed when `STEMMA_PROPERTY_SEED` is not set.
const DEFAULT_SEED: u64 = 0x5EED_5EED_5EED_5EED;

/// How many generated values or inputs each property checks when `STEMMA_PROPERTY_CASES` is not
/// set.
const DEFAULT_CASES: usize = 4_000;

fn seed() -> u64 {
    std::env::var("STEMMA_PROPERTY_SEED")
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(DEFAULT_SEED)
}

fn cases() -> usize {
    std::env::var("STEMMA_PROPERTY_CASES")
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(DEFAULT_CASES)
}

/// xorshift64*: small, fast, and good enough to vary inputs. Not for anything else.
struct Rng(u64);

impl Rng {
    fn new(seed: u64, stream: u64) -> Self {
        // Mix the stream in, so that each property sees other numbers; never start at zero.
        let mut rng = Self(seed ^ stream.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        rng.next();
        rng
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A number below `bound`, which must not be zero.
    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }

    /// True once in `n` on average.
    fn one_in(&mut self, n: usize) -> bool {
        self.below(n) == 0
    }
}

/// Pieces that change an encoding: separators, signs, digits, the keywords and modes of the
/// formats, and characters that the name rules treat specially.
const PIECES: &[&str] = &[
    " ",
    "  ",
    "\n",
    "\n\n",
    "\r",
    "\r\n",
    "\t",
    "\0",
    "+",
    "-",
    "-0",
    "=",
    ":",
    ".",
    "..",
    "/",
    "\\",
    "0",
    "00",
    "9",
    "a",
    "A",
    "f",
    "~",
    "\"",
    "\u{7f}",
    "\u{85}",
    "\u{a0}",
    "\u{e9}",
    "e\u{301}",
    "\u{200b}",
    "\u{200d}",
    "\u{202e}",
    "\u{fe0f}",
    "\u{feff}",
    "\u{1f600}",
    "b3:",
    "b3:0000000000000000000000000000000000000000000000000000000000000000",
    "100644 ",
    "100755 ",
    "120000 ",
    "040000 ",
    "40000 ",
    "tree ",
    "parent ",
    "author ",
    "time ",
    "human ",
    "agent ",
    " model=",
    " session=",
    " sponsor=",
    "stemma-index 1\n",
    "stemma-index 2\n",
    ".stemma",
    "9223372036854775807",
    "9223372036854775808",
];

/// Numbers and near-numbers, spelled in ways that a strict reader has to reject: signs, leading
/// zeros, other digits, and values at and beyond the limit of `i64`.
const NUMBERS: &[&str] = &[
    "0",
    "-0",
    "00",
    "01",
    "-01",
    "+1",
    "+0",
    "-",
    "1e3",
    "0x10",
    "1_000",
    "\u{ff11}",
    "\u{663}",
    "9223372036854775807",
    "9223372036854775808",
    "-9223372036854775808",
    "-9223372036854775809",
    "99999999999999999999",
    "",
];

/// Names, some of them invalid; the generators keep what the constructors accept.
const NAMES: &[&str] = &[
    "a",
    "b",
    "c",
    "A",
    "a-b",
    "a.b",
    "a b",
    "x.y.z",
    "...a",
    "a...",
    "~",
    ".hidden",
    "trail.",
    "a=b",
    "say\"hi",
    "back\\slash",
    "\u{e9}",
    "e\u{301}",
    "\u{65e5}\u{672c}",
    "\u{1f600}",
    "\u{fe0f}",
    "a\u{fe0f}",
    "\u{e9}\u{fe0f}",
    "..",
    ".",
    "",
    "a/b",
    "a\nb",
    "a\u{200b}b",
    ".stemma",
    "AUX",
];

fn random_blob(rng: &mut Rng) -> BlobId {
    let len = rng.below(6);
    let bytes: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
    Blob::new(bytes).id()
}

fn random_mode(rng: &mut Rng) -> FileMode {
    *rng.pick(&[FileMode::File, FileMode::Executable, FileMode::Symlink])
}

/// A name from the pool, or a few pool names joined, so that rare combinations occur.
fn random_name(rng: &mut Rng) -> String {
    let mut name = rng.pick(NAMES).to_string();
    if rng.one_in(4) {
        name.push_str(rng.pick(NAMES));
    }
    name
}

fn random_tree(rng: &mut Rng) -> Tree {
    let mut entries: Vec<TreeEntry> = Vec::new();
    for _ in 0..rng.below(8) {
        let name = random_name(rng);
        if entries.iter().any(|entry| entry.name() == name) {
            continue;
        }
        let entry = if rng.one_in(3) {
            let inner = Tree::new(vec![TreeEntry::file(
                "x",
                random_mode(rng),
                random_blob(rng),
            )
            .expect("valid name")])
            .expect("one entry");
            TreeEntry::directory(name, inner.id())
        } else {
            TreeEntry::file(name, random_mode(rng), random_blob(rng))
        };
        if let Ok(entry) = entry {
            entries.push(entry);
        }
    }
    Tree::new(entries).expect("names are distinct")
}

/// A visible-ASCII field, which may contain `=`.
fn random_field(rng: &mut Rng) -> String {
    (0..1 + rng.below(6))
        .map(|_| char::from(0x21 + rng.below(0x7e - 0x21 + 1) as u8))
        .collect()
}

fn random_actor(rng: &mut Rng) -> Actor {
    if rng.one_in(2) {
        Actor::human(random_field(rng)).expect("visible ASCII")
    } else {
        Actor::agent(
            random_field(rng),
            random_field(rng),
            random_field(rng),
            random_field(rng),
        )
        .expect("visible ASCII")
    }
}

fn random_commit(rng: &mut Rng) -> Commit {
    let tree: TreeId = random_tree(rng).id();
    let mut parents: Vec<CommitId> = Vec::new();
    for n in 0..rng.below(4) {
        let parent = Commit::new(tree, vec![], random_actor(rng), n as i64, format!("p{n}"))
            .expect("no parents")
            .id();
        if !parents.contains(&parent) {
            parents.push(parent);
        }
    }
    let random_time = rng.next() as i64;
    let time = *rng.pick(&[0, 1, -1, 1_700_000_000, i64::MAX, i64::MIN, random_time]);
    let message = {
        let mut message = String::new();
        for _ in 0..rng.below(4) {
            message.push_str(rng.pick(&["", "a", "\n", "\n\n", " ", "\u{e9}", "\r\n", "tree x\n"]));
        }
        message
    };
    Commit::new(tree, parents, random_actor(rng), time, message).expect("distinct parents")
}

fn random_index(rng: &mut Rng) -> Index {
    let mut index = Index::new();
    for _ in 0..rng.below(8) {
        let depth = 1 + rng.below(3);
        let path = (0..depth)
            .map(|_| random_name(rng))
            .collect::<Vec<_>>()
            .join("/");
        // A path can be rejected by the rules or conflict with another one; keep the rest.
        let _ = index.insert(path, random_mode(rng), random_blob(rng));
    }
    index
}

/// Changes `bytes` at several places at once.
fn mutate(rng: &mut Rng, bytes: &[u8]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    for _ in 0..1 + rng.below(4) {
        match rng.below(10) {
            8 | 9 => replace_token(rng, &mut out),
            0 => {
                let at = rng.below(out.len() + 1);
                let piece = rng.pick(PIECES).as_bytes().to_vec();
                out.splice(at..at, piece);
            }
            1 if !out.is_empty() => {
                let at = rng.below(out.len());
                let end = (at + 1 + rng.below(3)).min(out.len());
                out.drain(at..end);
            }
            2 if !out.is_empty() => {
                let at = rng.below(out.len());
                let piece = rng.pick(PIECES).as_bytes().to_vec();
                out.splice(at..at + 1, piece);
            }
            3 if !out.is_empty() => {
                let a = rng.below(out.len());
                let b = rng.below(out.len());
                out.swap(a, b);
            }
            4 => {
                let at = rng.below(out.len() + 1);
                out.truncate(at);
            }
            5..=7 => {
                let mut lines: Vec<Vec<u8>> = out
                    .split_inclusive(|&byte| byte == b'\n')
                    .map(<[u8]>::to_vec)
                    .collect();
                if lines.is_empty() {
                    continue;
                }
                let at = rng.below(lines.len());
                match rng.below(3) {
                    0 => {
                        lines.remove(at);
                    }
                    1 => lines.insert(at, lines[at].clone()),
                    _ => {
                        let other = rng.below(lines.len());
                        lines.swap(at, other);
                    }
                }
                out = lines.concat();
            }
            _ => {}
        }
    }
    out
}

/// Replaces one token (the bytes between separators) by a number or another piece. Changes at
/// random positions rarely hit a short token, so this reaches the values of the headers and the
/// fields of a line.
fn replace_token(rng: &mut Rng, bytes: &mut Vec<u8>) {
    let starts: Vec<usize> = (0..=bytes.len())
        .filter(|&at| at == 0 || matches!(bytes[at - 1], b' ' | b'\n'))
        .collect();
    let start = *rng.pick(&starts);
    let end = bytes[start..]
        .iter()
        .position(|&byte| matches!(byte, b' ' | b'\n'))
        .map_or(bytes.len(), |offset| start + offset);
    let replacement = if rng.one_in(2) {
        rng.pick(NUMBERS)
    } else {
        rng.pick(PIECES)
    };
    bytes.splice(start..end, replacement.bytes());
}

/// Joins the first part of one encoding to the last part of another.
fn splice(rng: &mut Rng, first: &[u8], second: &[u8]) -> Vec<u8> {
    let mut out = first[..rng.below(first.len() + 1)].to_vec();
    out.extend_from_slice(&second[rng.below(second.len() + 1)..]);
    out
}

/// Pieces of the format glued together at random, with no valid object as the base.
fn soup(rng: &mut Rng) -> Vec<u8> {
    let mut out = Vec::new();
    for _ in 0..rng.below(12) {
        out.extend_from_slice(rng.pick(PIECES).as_bytes());
        if rng.one_in(3) {
            out.extend_from_slice(rng.pick(NAMES).as_bytes());
        }
    }
    out
}

/// Raw bytes from a skewed alphabet, so that valid UTF-8, ASCII, and stray bytes all occur.
fn raw_bytes(rng: &mut Rng) -> Vec<u8> {
    (0..rng.below(40))
        .map(|_| match rng.below(4) {
            0 => b"0123456789abcdef"[rng.below(16)],
            1 => b" \n=:/.-+"[rng.below(8)],
            2 => rng.next() as u8,
            _ => b"abcdefghijklmnopqrstuvwxyz"[rng.below(26)],
        })
        .collect()
}

/// One input of the third kind of source, chosen at random: a changed valid encoding, a splice of
/// two, a soup, or raw bytes.
fn arbitrary_input(rng: &mut Rng, valid: &[Vec<u8>]) -> Vec<u8> {
    match rng.below(8) {
        0..=3 => {
            let base = rng.pick(valid).clone();
            mutate(rng, &base)
        }
        4 | 5 => {
            let first = rng.pick(valid).clone();
            let second = rng.pick(valid).clone();
            splice(rng, &first, &second)
        }
        6 => soup(rng),
        _ => raw_bytes(rng),
    }
}

/// The number of lines of the input, counting a last line without a newline, and at least one.
fn line_count(bytes: &[u8]) -> usize {
    let newlines = bytes.iter().filter(|&&byte| byte == b'\n').count();
    let cut_off = !bytes.is_empty() && bytes.last() != Some(&b'\n');
    (newlines + usize::from(cut_off)).max(1)
}

/// Checks the rules for every rejection: the message is not empty, the chain of causes ends, and
/// the line, if there is one, is inside the input.
fn check_rejection(error: &dyn Error, line: Option<usize>, input: &[u8], context: &str) {
    assert!(
        !error.to_string().is_empty(),
        "{context}: empty message for {input:?}"
    );
    let mut depth = 0;
    let mut cause = error.source();
    while let Some(next) = cause {
        depth += 1;
        assert!(depth < 8, "{context}: the cause chain does not end");
        assert!(!next.to_string().is_empty(), "{context}: empty cause");
        cause = next.source();
    }
    if let Some(line) = line {
        let lines = line_count(input);
        assert!(
            (1..=lines + 1).contains(&line),
            "{context}: line {line} of {lines} for {:?}",
            String::from_utf8_lossy(input)
        );
    }
}

fn tree_line(error: &TreeParseError) -> Option<usize> {
    match error {
        TreeParseError::NotUtf8 { .. } => None,
        TreeParseError::Malformed { line, .. }
        | TreeParseError::UnknownMode { line, .. }
        | TreeParseError::InvalidObjectId { line, .. }
        | TreeParseError::Entry { line, .. }
        | TreeParseError::OutOfOrder { line, .. } => Some(*line),
    }
}

fn commit_line(error: &CommitParseError) -> Option<usize> {
    match error {
        CommitParseError::NotUtf8 { .. } | CommitParseError::Commit(_) => None,
        CommitParseError::Malformed { line, .. }
        | CommitParseError::InvalidObjectId { line, .. }
        | CommitParseError::InvalidAuthor { line, .. }
        | CommitParseError::DuplicateParent { line, .. } => Some(*line),
    }
}

fn index_line(error: &IndexParseError) -> Option<usize> {
    match error {
        IndexParseError::NotUtf8 { .. }
        | IndexParseError::MissingHeader
        | IndexParseError::UnsupportedVersion(_) => None,
        IndexParseError::Malformed { line, .. }
        | IndexParseError::UnknownMode { line, .. }
        | IndexParseError::InvalidObjectId { line, .. }
        | IndexParseError::OutOfOrder { line, .. }
        | IndexParseError::Entry { line, .. } => Some(*line),
    }
}

/// What a property run saw, so that a test can check that it was not vacuous.
#[derive(Default)]
struct Tally {
    accepted: usize,
    rejected: usize,
}

#[test]
fn every_tree_the_code_can_build_is_read_back() {
    let (seed, mut rng) = (seed(), Rng::new(seed(), 1));
    for case in 0..cases() {
        let context = format!("seed {seed}, case {case}");
        let tree = random_tree(&mut rng);
        let bytes = tree.encode();
        let again = Tree::parse(&bytes).unwrap_or_else(|e| panic!("{context}: {e} for {tree:?}"));
        assert_eq!(again, tree, "{context}");
        assert_eq!(again.encode(), bytes, "{context}");
        assert_eq!(again.id(), tree.id(), "{context}");
    }
}

#[test]
fn every_commit_the_code_can_build_is_read_back() {
    let (seed, mut rng) = (seed(), Rng::new(seed(), 2));
    for case in 0..cases() {
        let context = format!("seed {seed}, case {case}");
        let commit = random_commit(&mut rng);
        let bytes = commit.encode();
        let again =
            Commit::parse(&bytes).unwrap_or_else(|e| panic!("{context}: {e} for {commit:?}"));
        assert_eq!(again, commit, "{context}");
        assert_eq!(again.encode(), bytes, "{context}");
        assert_eq!(again.id(), commit.id(), "{context}");
    }
}

#[test]
fn every_index_the_code_can_build_is_read_back() {
    let (seed, mut rng) = (seed(), Rng::new(seed(), 3));
    for case in 0..cases() {
        let context = format!("seed {seed}, case {case}");
        let index = random_index(&mut rng);
        let bytes = index.encode();
        let again = Index::parse(&bytes).unwrap_or_else(|e| panic!("{context}: {e} for {index:?}"));
        assert_eq!(again, index, "{context}");
        assert_eq!(again.encode(), bytes, "{context}");
    }
}

#[test]
fn every_actor_the_code_can_build_is_read_back() {
    let (seed, mut rng) = (seed(), Rng::new(seed(), 4));
    for case in 0..cases() {
        let context = format!("seed {seed}, case {case}");
        let actor = random_actor(&mut rng);
        let line = actor.to_string();
        let again: Actor = line
            .parse()
            .unwrap_or_else(|e| panic!("{context}: {e} for {actor:?}"));
        assert_eq!(again, actor, "{context}");
        assert_eq!(again.to_string(), line, "{context}");
    }
}

#[test]
fn the_tree_reader_accepts_only_what_the_encoder_writes_and_never_panics() {
    let (seed, mut rng) = (seed(), Rng::new(seed(), 5));
    let valid: Vec<Vec<u8>> = (0..64).map(|_| random_tree(&mut rng).encode()).collect();
    let mut tally = Tally::default();
    for case in 0..cases() * 5 {
        let context = format!("seed {seed}, case {case}");
        let input = arbitrary_input(&mut rng, &valid);
        match Tree::parse(&input) {
            Ok(tree) => {
                tally.accepted += 1;
                assert_eq!(
                    tree.encode(),
                    input,
                    "{context}: a second spelling was accepted: {:?}",
                    String::from_utf8_lossy(&input)
                );
            }
            Err(error) => {
                tally.rejected += 1;
                check_rejection(&error, tree_line(&error), &input, &context);
            }
        }
    }
    assert!(tally.accepted > 0 && tally.rejected > 0, "vacuous run");
}

#[test]
fn the_commit_reader_accepts_only_what_the_encoder_writes_and_never_panics() {
    let (seed, mut rng) = (seed(), Rng::new(seed(), 6));
    let valid: Vec<Vec<u8>> = (0..64).map(|_| random_commit(&mut rng).encode()).collect();
    let mut tally = Tally::default();
    for case in 0..cases() * 5 {
        let context = format!("seed {seed}, case {case}");
        let input = arbitrary_input(&mut rng, &valid);
        match Commit::parse(&input) {
            Ok(commit) => {
                tally.accepted += 1;
                assert_eq!(
                    commit.encode(),
                    input,
                    "{context}: a second spelling was accepted: {:?}",
                    String::from_utf8_lossy(&input)
                );
            }
            Err(error) => {
                tally.rejected += 1;
                check_rejection(&error, commit_line(&error), &input, &context);
            }
        }
    }
    assert!(tally.accepted > 0 && tally.rejected > 0, "vacuous run");
}

#[test]
fn the_index_reader_accepts_only_what_the_encoder_writes_and_never_panics() {
    let (seed, mut rng) = (seed(), Rng::new(seed(), 7));
    let valid: Vec<Vec<u8>> = (0..64).map(|_| random_index(&mut rng).encode()).collect();
    let mut tally = Tally::default();
    for case in 0..cases() * 5 {
        let context = format!("seed {seed}, case {case}");
        let input = arbitrary_input(&mut rng, &valid);
        match Index::parse(&input) {
            Ok(index) => {
                tally.accepted += 1;
                assert_eq!(
                    index.encode(),
                    input,
                    "{context}: a second spelling was accepted: {:?}",
                    String::from_utf8_lossy(&input)
                );
            }
            Err(error) => {
                tally.rejected += 1;
                check_rejection(&error, index_line(&error), &input, &context);
            }
        }
    }
    assert!(tally.accepted > 0 && tally.rejected > 0, "vacuous run");
}

#[test]
fn the_actor_reader_accepts_only_what_the_encoder_writes_and_never_panics() {
    let (seed, mut rng) = (seed(), Rng::new(seed(), 8));
    let valid: Vec<Vec<u8>> = (0..64)
        .map(|_| random_actor(&mut rng).to_string().into_bytes())
        .collect();
    let mut tally = Tally::default();
    for case in 0..cases() * 5 {
        let context = format!("seed {seed}, case {case}");
        let input = String::from_utf8_lossy(&arbitrary_input(&mut rng, &valid)).into_owned();
        match input.parse::<Actor>() {
            Ok(actor) => {
                tally.accepted += 1;
                assert_eq!(
                    actor.to_string(),
                    input,
                    "{context}: a second spelling was accepted"
                );
            }
            Err(error) => {
                tally.rejected += 1;
                let error: &ActorParseError = &error;
                check_rejection(error, None, input.as_bytes(), &context);
            }
        }
    }
    assert!(tally.accepted > 0 && tally.rejected > 0, "vacuous run");
}
