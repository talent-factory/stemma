//! Property tests of the readers and encoders of trees, commits, indexes, actor lines, the entries
//! of the operation log in both encodings (a change; a stub and a purge), and the files of HEAD and
//! the branches. The journal of a purge keeps its type to its module, so its properties are tested
//! there, with the generator of this module.
//!
//! The unit tests change a golden encoding at one place at a time. These tests complement them
//! with generated input and check the two rules of ADR-005 on all of it:
//!
//! 1. **One object, one encoding.** For every value that the code can build, `parse(encode(x))`
//!    gives `x` back. For every input that a reader accepts, whatever its origin, encoding the
//!    result gives exactly the input. A reader that accepted a second spelling, or an encoder that
//!    wrote something that its reader rejects, breaks one of the two.
//! 2. **Total readers.** A reader never panics, and every rejection has a message, a cause chain
//!    that ends, and, where it names a line, a line inside the input. The commit reader reports a
//!    missing header at the line where it was expected, which is the line after the last one.
//!
//! The inputs come from a small generator with a fixed seed, so a failure is reproducible and the
//! test is not flaky. Inputs are built from valid encodings that are changed in several places at
//! once, spliced together, assembled from tokens of the format, and drawn as raw bytes. Set
//! `STEMMA_PROPERTY_SEED` to explore other inputs and `STEMMA_PROPERTY_CASES` to run more of them;
//! a failure prints both values.

use std::error::Error;
use std::io;

use crate::change::{sample_change, ChangeId};
use crate::index::{FileMode, Index, IndexParseError};
use crate::object::{
    Blob, BlobId, Commit, CommitId, CommitParseError, ObjectId, Tree, TreeEntry, TreeId,
    TreeParseError,
};
use crate::oplog::{
    Change, Entry, EntryParseError, Moved, OperationName, Purge, Record, MAX_NUMBER,
};
use crate::refs::{BranchName, Head, RefParseError};
use crate::store::{ObjectStore, StoreError};
use crate::testutil::{FaultyStore, TempDir, DISK_MESSAGE};
use crate::{Actor, ActorParseError};

/// The seed when `STEMMA_PROPERTY_SEED` is not set.
const DEFAULT_SEED: u64 = 0x5EED_5EED_5EED_5EED;

/// How many generated values or inputs each property checks when `STEMMA_PROPERTY_CASES` is not
/// set.
const DEFAULT_CASES: usize = 4_000;

pub(crate) fn seed() -> u64 {
    std::env::var("STEMMA_PROPERTY_SEED")
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(DEFAULT_SEED)
}

pub(crate) fn cases() -> usize {
    std::env::var("STEMMA_PROPERTY_CASES")
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(DEFAULT_CASES)
}

/// xorshift64*: small, fast, and good enough to vary inputs. Not for anything else.
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64, stream: u64) -> Self {
        // One step of splitmix64 over the seed and the stream, so that every seed and every
        // property starts from another state, and the state is never zero.
        const GOLDEN: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut z = seed
            .wrapping_add(stream.wrapping_mul(GOLDEN))
            .wrapping_add(GOLDEN);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        Self(if z == 0 { GOLDEN } else { z })
    }

    pub(crate) fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A number below `bound`, which must not be zero.
    pub(crate) fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }

    pub(crate) fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }

    /// True once in `n` on average.
    pub(crate) fn one_in(&mut self, n: usize) -> bool {
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
    "100775 ",
    "0120000 ",
    "+100644 ",
    "x-foo bar\n",
    "committer human a\n",
    "encoding utf-8\n",
    "\u{2028}",
    "100755 ",
    "120000 ",
    "040000 ",
    "40000 ",
    "tree ",
    "parent ",
    "author ",
    "change ",
    "stemma-oplog 1\n",
    "number ",
    "operation ",
    "working-copy ",
    "head ",
    "branch:",
    "commit:",
    "-",
    "ch:",
    "ch:00112233445566778899AABBCCDDEEFF",
    "time ",
    "human ",
    "agent ",
    " model=",
    " session=",
    " sponsor=",
    "stemma-index 1\n",
    "branch ",
    "commit ",
    "branch main\n",
    "stemma-index 2\n",
    "stemma-oplog 2\n",
    "stub\n",
    "operation purge\n",
    "commits ",
    "stubs ",
    "3-5 7",
    "stemma-purge 1\n",
    "blob ",
    "last ",
    "rewrite ",
    "new ",
    "delete commit ",
    "delete tree ",
    ".stemma",
    "9223372036854775807",
    "9223372036854775808",
];

/// Numbers, modes, and keywords, spelled in ways that a strict reader has to reject: signs,
/// leading zeros, other digits, values at and beyond the limit of `i64`, and near-miss modes.
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
    // Change IDs with a near miss: another case, another length, a bad digit, a wrong prefix, and
    // multi-byte characters that make 32 bytes of 16 characters.
    "ch:00112233445566778899AABBCCDDEEFF",
    "ch:00112233445566778899aabbccddee",
    "ch:00112233445566778899aabbccddeeff0",
    "ch:00112233445566778899aabbccddeefg",
    "CH:00112233445566778899aabbccddeeff",
    "00112233445566778899aabbccddeeff",
    "ch:\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}",
    "ch:",
    // Modes and keywords with a near miss: the token that has to be replaced is a whole word.
    "100775",
    "0120000",
    "100664",
    "160000",
    "+100644",
    "Human",
    "HUMAN",
    "agent",
    "human",
    "tree",
    "parent",
    "author",
    "time",
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

pub(crate) fn random_blob(rng: &mut Rng) -> BlobId {
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

const BRANCH_NAMES: &[&str] = &[
    "main",
    "dev",
    "a",
    "b",
    "feat/x",
    "release/1.0",
    "x-y_z",
    "Z",
    "a.b",
    "m/n/o",
];

const OPERATION_NAMES: &[&str] = &["init", "snapshot", "commit", "op-9", "a", "checkout"];

pub(crate) fn random_branch(rng: &mut Rng) -> BranchName {
    BranchName::new(*rng.pick(BRANCH_NAMES)).expect("valid branch name")
}

fn random_commit_value(rng: &mut Rng) -> Option<CommitId> {
    if rng.one_in(4) {
        None
    } else {
        Some(CommitId::new(ObjectId::from(random_blob(rng))))
    }
}

fn random_head_value(rng: &mut Rng) -> Option<Head> {
    match rng.below(3) {
        0 => None,
        1 => Some(Head::Branch(random_branch(rng))),
        _ => Some(Head::Detached(CommitId::new(ObjectId::from(random_blob(
            rng,
        ))))),
    }
}

/// Two values that differ, as an entry holds for a pointer that changed.
fn random_moved<T: PartialEq>(
    rng: &mut Rng,
    mut value: impl FnMut(&mut Rng) -> Option<T>,
) -> Moved<T> {
    loop {
        let (old, new) = (value(rng), value(rng));
        if old != new {
            return Moved::from_to(old, new);
        }
    }
}

/// An entry of either encoding: the entry of a change (encoding 1), or a stub or the entry of a
/// purge (encoding 2).
fn random_entry(rng: &mut Rng) -> Entry {
    match rng.below(4) {
        0 => random_change_entry(rng)
            .to_stub()
            .expect("the entry of a change can become a stub"),
        1 => random_purge_entry(rng),
        _ => random_change_entry(rng),
    }
}

fn random_number(rng: &mut Rng) -> u64 {
    *rng.pick(&[1, 2, 41, 999, MAX_NUMBER - 1, MAX_NUMBER])
}

fn random_time(rng: &mut Rng) -> i64 {
    let random_time = rng.next() as i64;
    *rng.pick(&[0, 1, -1, 1_700_000_000, i64::MAX, i64::MIN, random_time])
}

fn random_change_entry(rng: &mut Rng) -> Entry {
    let number = random_number(rng);
    let time = random_time(rng);
    let head = rng.one_in(2).then(|| random_moved(rng, random_head_value));
    let working_copy = rng
        .one_in(2)
        .then(|| random_moved(rng, random_commit_value));
    let mut branches: Vec<(BranchName, Moved<CommitId>)> = Vec::new();
    for _ in 0..rng.below(4) {
        let name = random_branch(rng);
        if !branches.iter().any(|(existing, _)| *existing == name) {
            branches.push((name, random_moved(rng, random_commit_value)));
        }
    }
    if head.is_none() && working_copy.is_none() && branches.is_empty() {
        branches.push((random_branch(rng), random_moved(rng, random_commit_value)));
    }
    let operation = OperationName::new(*rng.pick(OPERATION_NAMES)).expect("valid operation");
    let change = Change::new(
        time,
        random_actor(rng),
        operation,
        head,
        working_copy,
        branches,
    )
    .expect("a random change is valid");
    Entry::new(number, change).expect("a random number is valid")
}

/// The entry of a purge. The stubs come in any order, with repeats, and only those below the
/// number of the entry are kept, so ranges of one number, ranges of several, and no stub at all
/// all occur.
fn random_purge_entry(rng: &mut Rng) -> Entry {
    let number = random_number(rng);
    let time = random_time(rng);
    let commits = *rng.pick(&[0, 1, 7, MAX_NUMBER]);
    let stubs: Vec<u64> = (0..rng.below(8))
        .map(|_| match rng.below(4) {
            0 => *rng.pick(&[1, 2, 3, 5, 6, 40]),
            1 => number.saturating_sub(1 + rng.below(3) as u64),
            _ => 1 + rng.next() % number,
        })
        .filter(|stub| (1..number).contains(stub))
        .collect();
    let purge = Purge::new(time, random_actor(rng), commits, &stubs).expect("a valid purge");
    Entry::of(number, Record::Purge(purge)).expect("the stubs are before the purge")
}

fn random_change(rng: &mut Rng) -> ChangeId {
    let mut bytes = [0u8; 16];
    for chunk in bytes.chunks_mut(8) {
        chunk.copy_from_slice(&rng.next().to_le_bytes()[..chunk.len()]);
    }
    ChangeId::from_bytes(bytes)
}

fn random_commit(rng: &mut Rng) -> Commit {
    let tree: TreeId = random_tree(rng).id();
    let mut parents: Vec<CommitId> = Vec::new();
    for n in 0..rng.below(4) {
        let parent = Commit::new(
            tree,
            vec![],
            sample_change(),
            random_actor(rng),
            n as i64,
            format!("p{n}"),
        )
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
    Commit::new(
        tree,
        parents,
        random_change(rng),
        random_actor(rng),
        time,
        message,
    )
    .expect("distinct parents")
}

/// A branch name made of a few parts, some of them invalid; the names that the rule rejects are
/// dropped.
fn random_branch_name(rng: &mut Rng) -> BranchName {
    const PARTS: &[&str] = &[
        "main", "feat", "TF-792", "a.b", "x_y", "v1.2", "con", "-x", ".x", "a.", "\u{e9}", "A", "z",
    ];
    loop {
        let depth = 1 + rng.below(3);
        let name = (0..depth)
            .map(|_| *rng.pick(PARTS))
            .collect::<Vec<_>>()
            .join("/");
        if let Ok(name) = BranchName::new(name) {
            return name;
        }
    }
}

fn random_head(rng: &mut Rng) -> Head {
    if rng.one_in(2) {
        Head::Branch(random_branch_name(rng))
    } else {
        Head::Detached(random_commit(rng).id())
    }
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
        match rng.below(15) {
            14 => edit_change_value(rng, &mut out),
            8 | 9 => replace_token(rng, &mut out),
            10 => edit_line_end(rng, &mut out),
            11 => extra_separator(rng, &mut out),
            12 => swap_tokens(rng, &mut out),
            13 => insert_line(rng, &mut out),
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

/// Changes the value of a `change` header by one character: another case, a bad digit, a missing
/// or an extra digit, or a doubled prefix. This reaches the spellings of a change ID that a strict
/// reader has to reject, which a change of a random byte seldom produces. It does nothing if the
/// input has no such header.
fn edit_change_value(rng: &mut Rng, bytes: &mut Vec<u8>) {
    const HEADER: &[u8] = b"change ";
    let Some(found) = bytes
        .windows(HEADER.len())
        .position(|window| window == HEADER)
        .filter(|&at| at == 0 || bytes[at - 1] == b'\n')
    else {
        return;
    };
    let start = found + HEADER.len();
    let end = bytes[start..]
        .iter()
        .position(|&byte| byte == b'\n')
        .map_or(bytes.len(), |offset| start + offset);
    if start >= end {
        return;
    }
    let at = start + rng.below(end - start);
    match rng.below(5) {
        0 => bytes[at] = bytes[at].to_ascii_uppercase(),
        1 => bytes[at] = b'g',
        2 => {
            bytes.remove(end - 1);
        }
        3 => bytes.insert(end, b'0'),
        _ => bytes.splice(start..start, "ch:".bytes()).for_each(drop),
    }
}

/// Adds a space, a tab, or a carriage return before a line end, or at the end of the input.
fn edit_line_end(rng: &mut Rng, bytes: &mut Vec<u8>) {
    let ends: Vec<usize> = (0..bytes.len()).filter(|&at| bytes[at] == b'\n').collect();
    let at = if ends.is_empty() {
        bytes.len()
    } else {
        *rng.pick(&ends)
    };
    let piece = *rng.pick(&[" ", "\r", " \r", "\t"]);
    bytes.splice(at..at, piece.bytes());
}

/// Doubles a separator, or puts a tab next to one.
fn extra_separator(rng: &mut Rng, bytes: &mut Vec<u8>) {
    let spaces: Vec<usize> = (0..bytes.len()).filter(|&at| bytes[at] == b' ').collect();
    if spaces.is_empty() {
        return;
    }
    let at = *rng.pick(&spaces) + usize::from(rng.one_in(2));
    let piece = *rng.pick(&[" ", "\t"]);
    bytes.splice(at..at, piece.bytes());
}

/// Swaps two space-separated tokens of one line, for example the keys of an actor line.
fn swap_tokens(rng: &mut Rng, bytes: &mut Vec<u8>) {
    let mut lines: Vec<Vec<u8>> = bytes
        .split_inclusive(|&byte| byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect();
    if lines.is_empty() {
        return;
    }
    let at = rng.below(lines.len());
    let ended = lines[at].last() == Some(&b'\n');
    let content = if ended {
        &lines[at][..lines[at].len() - 1]
    } else {
        &lines[at][..]
    };
    let mut tokens: Vec<Vec<u8>> = content
        .split(|&byte| byte == b' ')
        .map(<[u8]>::to_vec)
        .collect();
    let (a, b) = (rng.below(tokens.len()), rng.below(tokens.len()));
    tokens.swap(a, b);
    let mut line = tokens.join(&b' ');
    if ended {
        line.push(b'\n');
    }
    lines[at] = line;
    *bytes = lines.concat();
}

/// Inserts a whole line, an unknown header or a blank one, at the start of a line.
fn insert_line(rng: &mut Rng, bytes: &mut Vec<u8>) {
    let starts: Vec<usize> = (0..=bytes.len())
        .filter(|&at| at == 0 || bytes[at - 1] == b'\n')
        .collect();
    let at = *rng.pick(&starts);
    let line = *rng.pick(&[
        "x-foo bar\n",
        "committer human a\n",
        "encoding utf-8\n",
        "\n",
        " \n",
        "parent \n",
        "time 0\n",
        "tree \n",
        "change ch:00112233445566778899aabbccddeeff\n",
        "change \n",
    ]);
    bytes.splice(at..at, line.bytes());
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
pub(crate) fn arbitrary_input(rng: &mut Rng, valid: &[Vec<u8>]) -> Vec<u8> {
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

/// Walks the chain of causes and checks that it ends, that no link is empty, and that no link
/// repeats in its message the message of the next one: a message is complete and `source` goes on
/// with the first cause that the message does not print (crate documentation).
fn assert_no_message_twice(error: &dyn Error, context: &str) {
    let mut depth = 0;
    let mut link = error;
    while let Some(next) = link.source() {
        depth += 1;
        assert!(depth < 8, "{context}: the cause chain does not end");
        let (outer, inner) = (link.to_string(), next.to_string());
        assert!(!inner.is_empty(), "{context}: empty cause");
        assert!(
            !outer.contains(&inner),
            "{context}: {outer:?} has the message of its source {inner:?}"
        );
        link = next;
    }
}

/// Checks the rules for every rejection: the message is not empty, the chain of causes ends, and
/// the line, if there is one, is inside the input.
fn check_rejection(
    error: &dyn Error,
    line: Option<usize>,
    last_line: usize,
    input: &[u8],
    context: &str,
) {
    assert!(
        !error.to_string().is_empty(),
        "{context}: empty message for {input:?}"
    );
    assert_no_message_twice(error, context);
    if let Some(line) = line {
        assert!(
            (1..=last_line).contains(&line),
            "{context}: line {line} of {last_line} for {:?}",
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

fn entry_line(error: &EntryParseError) -> Option<usize> {
    match error {
        EntryParseError::NotUtf8 { .. }
        | EntryParseError::UnsupportedVersion(_)
        | EntryParseError::Entry(_) => None,
        EntryParseError::Malformed { line, .. }
        | EntryParseError::InvalidNumber { line, .. }
        | EntryParseError::InvalidTime { line, .. }
        | EntryParseError::InvalidActor { line, .. }
        | EntryParseError::InvalidOperation { line, .. }
        | EntryParseError::InvalidObjectId { line, .. }
        | EntryParseError::InvalidBranchName { line, .. }
        | EntryParseError::Unchanged { line }
        | EntryParseError::OutOfOrder { line, .. } => Some(*line),
    }
}

fn commit_line(error: &CommitParseError) -> Option<usize> {
    match error {
        CommitParseError::NotUtf8 { .. } | CommitParseError::Commit(_) => None,
        CommitParseError::Malformed { line, .. }
        | CommitParseError::InvalidObjectId { line, .. }
        | CommitParseError::InvalidChangeId { line, .. }
        | CommitParseError::InvalidTime { line, .. }
        | CommitParseError::InvalidAuthor { line, .. }
        | CommitParseError::DuplicateParent { line, .. } => Some(*line),
    }
}

/// The line of a rejected file of references: there is none, the files have one line.
fn ref_line(_error: &RefParseError) -> Option<usize> {
    None
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
pub(crate) struct Tally {
    pub(crate) accepted: usize,
    pub(crate) rejected: usize,
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
                check_rejection(
                    &error,
                    tree_line(&error),
                    line_count(&input),
                    &input,
                    &context,
                );
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
                // A missing header is reported at the line where it was expected: the one after the end.
                check_rejection(
                    &error,
                    commit_line(&error),
                    line_count(&input) + 1,
                    &input,
                    &context,
                );
            }
        }
    }
    assert!(tally.accepted > 0 && tally.rejected > 0, "vacuous run");
}

#[test]
fn every_entry_the_code_can_build_is_read_back() {
    let (seed, mut rng) = (seed(), Rng::new(seed(), 20));
    let mut kinds = std::collections::HashSet::new();
    for case in 0..cases() {
        let context = format!("seed {seed}, case {case}");
        let entry = random_entry(&mut rng);
        kinds.insert(std::mem::discriminant(entry.record()));
        let bytes = entry.encode();
        let again = Entry::parse(&bytes).unwrap_or_else(|e| panic!("{context}: {e} for {entry:?}"));
        assert_eq!(again, entry, "{context}");
        assert_eq!(again.encode(), bytes, "{context}");
    }
    // Both encodings: the entry of a change, a stub, and the entry of a purge.
    assert_eq!(kinds.len(), 3, "the run did not build every kind of entry");
}

#[test]
fn the_entry_reader_accepts_only_what_the_encoder_writes_and_never_panics() {
    let (seed, mut rng) = (seed(), Rng::new(seed(), 21));
    let valid: Vec<Vec<u8>> = (0..64).map(|_| random_entry(&mut rng).encode()).collect();
    let mut tally = Tally::default();
    let mut variants = std::collections::HashSet::new();
    for case in 0..cases() * 5 {
        let context = format!("seed {seed}, case {case}");
        let input = arbitrary_input(&mut rng, &valid);
        match Entry::parse(&input) {
            Ok(entry) => {
                tally.accepted += 1;
                assert_eq!(
                    entry.encode(),
                    input,
                    "{context}: a second spelling was accepted: {:?}",
                    String::from_utf8_lossy(&input)
                );
            }
            Err(error) => {
                tally.rejected += 1;
                variants.insert(std::mem::discriminant(&error));
                check_rejection(
                    &error,
                    entry_line(&error),
                    line_count(&input) + 1,
                    &input,
                    &context,
                );
            }
        }
    }
    assert!(tally.accepted > 0 && tally.rejected > 0, "vacuous run");
    // A run that reaches few kinds of rejection would not notice a reader that reports the wrong
    // one, so it has to reach most of them (there are twelve, and one cannot occur).
    // The default run reaches ten or eleven; a short run, such as the one of a fast check, only
    // a few, so the threshold depends on how many cases there were.
    let needed = if cases() >= 2_000 { 9 } else { 3 };
    assert!(
        variants.len() >= needed,
        "the run reached only {} kinds of rejection, and {needed} were needed",
        variants.len()
    );
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
                check_rejection(
                    &error,
                    index_line(&error),
                    line_count(&input),
                    &input,
                    &context,
                );
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
                check_rejection(error, None, 1, input.as_bytes(), &context);
            }
        }
    }
    assert!(tally.accepted > 0 && tally.rejected > 0, "vacuous run");
}

#[test]
fn every_seed_and_every_property_starts_from_another_state() {
    let mut firsts = std::collections::HashSet::new();
    // Every stream that a property of this crate uses, here and in `repository/purge/journal.rs`.
    let streams = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 20, 21, 22, 23];
    for seed in 0..64 {
        for stream in streams {
            let mut rng = Rng::new(seed, stream);
            assert_ne!(rng.0, 0, "seed {seed}, stream {stream}");
            firsts.insert(rng.next());
        }
    }
    // Consecutive seeds do not repeat each other, and the properties do not share a sequence.
    assert_eq!(firsts.len(), 64 * streams.len());
}

/// The round trip cannot see a rule that the encoder and the reader share: if a name rule were
/// loosened, both would agree and the property would still hold. This list is written down apart
/// from the rules (ADR-005), and every entry point has to reject all of it.
#[test]
fn names_and_fields_that_the_rules_forbid_are_rejected_everywhere() {
    let id = Blob::new("x").id();
    for name in [
        "",
        ".",
        "..",
        " ",
        "...",
        "a\0b",
        "a\u{7f}",
        "a\r",
        "a\t",
        "a\u{85}",
        "a\u{2028}",
        "a\u{200b}",
        "a\u{202e}",
        "a\u{feff}",
        "a/b",
        "a\nb",
        ".stemma",
    ] {
        assert!(
            TreeEntry::file(name, FileMode::File, id).is_err(),
            "TreeEntry::file accepts {name:?}"
        );
        let body = format!("100644 {name}\n{id}\n");
        assert!(
            Tree::parse(body.as_bytes()).is_err(),
            "Tree::parse accepts {name:?}"
        );
        // A path may contain `/`, and a path with a line feed is cut into lines.
        if name != "a/b" {
            let text = format!("stemma-index 1\n100644 {id} {name}\n");
            assert!(
                Index::parse(text.as_bytes()).is_err(),
                "Index::parse accepts {name:?}"
            );
            assert!(
                Index::new().insert(name, FileMode::File, id).is_err(),
                "Index::insert accepts {name:?}"
            );
        }
    }
    for field in [
        "",
        "a b",
        "a\tb",
        "\u{e9}",
        "a\0",
        "a\u{7f}",
        "a\r",
        "a\n",
        "\u{2028}",
        "a\u{200b}",
        "\u{fe0f}",
    ] {
        assert!(Actor::human(field).is_err(), "human accepts {field:?}");
        assert!(
            Actor::agent("i", field, "s", "p").is_err(),
            "agent accepts model {field:?}"
        );
        assert!(
            Actor::agent("i", "m", field, "p").is_err(),
            "agent accepts session {field:?}"
        );
        assert!(
            Actor::agent("i", "m", "s", field).is_err(),
            "agent accepts sponsor {field:?}"
        );
        for line in [
            format!("human {field}"),
            format!("agent {field} model=m session=s sponsor=p"),
            format!("agent i model={field} session=s sponsor=p"),
            format!("agent i model=m session={field} sponsor=p"),
            format!("agent i model=m session=s sponsor={field}"),
        ] {
            assert!(line.parse::<Actor>().is_err(), "{line:?} is accepted");
        }
    }
}

/// Every error that wraps another one, built from a failure of the file system that has a cause
/// of its own (the same on every system, see `FaultyStore`). The message of each ends in the
/// message of the I/O error, once, and the chain of causes goes on with the one link below that,
/// which the message does not print, and nothing after it.
#[test]
fn every_error_that_wraps_another_prints_the_cause_once_and_goes_on_below_it() {
    let dir = TempDir::new();
    let working = dir.store();
    let blob = working.put_blob(&Blob::new("hello")).unwrap();
    let id = ObjectId::from(blob);
    let empty = working.put_tree(&Tree::new(vec![]).unwrap()).unwrap();
    let mut index = Index::new();
    index.insert("a", FileMode::File, blob).unwrap();
    let tree = Tree::new(vec![TreeEntry::file("a", FileMode::File, blob).unwrap()]).unwrap();
    let commit = Commit::new(
        empty,
        vec![],
        sample_change(),
        Actor::human("daniel").unwrap(),
        0,
        "m",
    )
    .unwrap();

    let reading = FaultyStore::new(dir.store(), true, false);
    let writing = FaultyStore::new(dir.store(), false, true);
    let errors: Vec<(&str, Box<dyn Error>)> = vec![
        ("get", Box::new(reading.get(&id).unwrap_err())),
        ("get_tree", Box::new(reading.get_tree(&empty).unwrap_err())),
        (
            "get_commit",
            Box::new(reading.get_commit(&commit.id()).unwrap_err()),
        ),
        (
            "index verify",
            Box::new(index.verify(&reading).unwrap_err()),
        ),
        (
            "write_tree, check",
            Box::new(index.write_tree(&reading).unwrap_err()),
        ),
        (
            "write_tree, write",
            Box::new(index.write_tree(&writing).unwrap_err()),
        ),
        ("tree verify", Box::new(tree.verify(&reading).unwrap_err())),
        (
            "commit verify",
            Box::new(commit.verify(&reading).unwrap_err()),
        ),
    ];
    for (name, error) in &errors {
        let text = error.to_string();
        assert!(text.ends_with(DISK_MESSAGE), "{name}: {text}");
        assert_eq!(text.matches(DISK_MESSAGE).count(), 1, "{name}: {text}");
        assert!(!text.contains("deepest"), "{name}: {text}");
        // One link: the cause below the I/O error, whose message the text does not print.
        let first = error
            .source()
            .unwrap_or_else(|| panic!("{name}: no source"));
        assert_eq!(first.to_string(), "the deepest cause", "{name}");
        assert!(first.source().is_none(), "{name}");
        assert_no_message_twice(error.as_ref(), name);
    }
}

/// The typed error of the file system stays reachable through the wrappers, by the accessors.
#[test]
fn the_error_of_the_file_system_is_reached_through_every_wrapper() {
    let dir = TempDir::new();
    let working = dir.store();
    let blob = working.put_blob(&Blob::new("hello")).unwrap();
    let empty = working.put_tree(&Tree::new(vec![]).unwrap()).unwrap();
    let mut index = Index::new();
    index.insert("a", FileMode::File, blob).unwrap();
    let tree = Tree::new(vec![TreeEntry::file("a", FileMode::File, blob).unwrap()]).unwrap();
    let commit = Commit::new(
        empty,
        vec![],
        sample_change(),
        Actor::human("daniel").unwrap(),
        0,
        "m",
    )
    .unwrap();
    let reading = FaultyStore::new(dir.store(), true, false);
    let writing = FaultyStore::new(dir.store(), false, true);

    let kind = |store_error: Option<&StoreError>| {
        store_error
            .and_then(StoreError::io_error)
            .map(io::Error::kind)
    };
    let kinds = [
        reading
            .get(&ObjectId::from(blob))
            .unwrap_err()
            .io_error()
            .map(io::Error::kind),
        kind(reading.get_tree(&empty).unwrap_err().store_error()),
        kind(index.verify(&reading).unwrap_err().store_error()),
        kind(index.write_tree(&reading).unwrap_err().store_error()),
        kind(index.write_tree(&writing).unwrap_err().store_error()),
        kind(tree.verify(&reading).unwrap_err().store_error()),
        kind(commit.verify(&reading).unwrap_err().store_error()),
    ];
    assert!(
        kinds.iter().all(|kind| *kind == Some(io::ErrorKind::Other)),
        "{kinds:?}"
    );

    // A rejection that is not an error of the store has none.
    let mut missing = Index::new();
    missing
        .insert("a", FileMode::File, Blob::new("absent").id())
        .unwrap();
    assert!(missing
        .verify(&dir.store())
        .unwrap_err()
        .store_error()
        .is_none());
    let store = dir.store();
    let body = store
        .put_raw(crate::object::Kind::Tree, b"100644 a\nnot-an-id\n")
        .unwrap();
    let error = store.get_tree(&TreeId::new(body)).unwrap_err();
    assert!(error.store_error().is_none(), "{error}");
}

/// A chain in which a wrapper returns its cause although the message has it is the defect that
/// the rule excludes. The check has to see it.
#[test]
fn a_chain_that_repeats_a_message_is_found() {
    #[derive(Debug)]
    struct Inner;
    impl std::fmt::Display for Inner {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("inner")
        }
    }
    impl Error for Inner {}
    #[derive(Debug)]
    struct Outer(Inner);
    impl std::fmt::Display for Outer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "outer: {}", self.0)
        }
    }
    impl Error for Outer {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            Some(&self.0)
        }
    }
    let caught = std::panic::catch_unwind(|| assert_no_message_twice(&Outer(Inner), "test"));
    assert!(caught.is_err());
}

#[test]
fn every_head_and_branch_file_the_code_can_build_is_read_back() {
    let (seed, mut rng) = (seed(), Rng::new(seed(), 9));
    for case in 0..cases() {
        let context = format!("seed {seed}, case {case}");
        let head = random_head(&mut rng);
        let bytes = head.encode();
        let again = Head::parse(&bytes).unwrap_or_else(|e| panic!("{context}: {e} for {head:?}"));
        assert_eq!(again, head, "{context}");
        assert_eq!(again.encode(), bytes, "{context}");
        let commit = random_commit(&mut rng).id();
        let bytes = crate::refs::encode_branch(&commit);
        assert_eq!(crate::refs::parse_branch(&bytes), Ok(commit), "{context}");
    }
}

#[test]
fn the_head_reader_accepts_only_what_the_encoder_writes_and_never_panics() {
    let (seed, mut rng) = (seed(), Rng::new(seed(), 10));
    let valid: Vec<Vec<u8>> = (0..64).map(|_| random_head(&mut rng).encode()).collect();
    let mut tally = Tally::default();
    for case in 0..cases() * 5 {
        let context = format!("seed {seed}, case {case}");
        let input = arbitrary_input(&mut rng, &valid);
        match Head::parse(&input) {
            Ok(head) => {
                tally.accepted += 1;
                assert_eq!(
                    head.encode(),
                    input,
                    "{context}: a second spelling was accepted: {:?}",
                    String::from_utf8_lossy(&input)
                );
            }
            Err(error) => {
                tally.rejected += 1;
                check_rejection(&error, ref_line(&error), 1, &input, &context);
            }
        }
    }
    assert!(tally.accepted > 0 && tally.rejected > 0, "vacuous run");
}

#[test]
fn the_branch_file_reader_accepts_only_what_the_encoder_writes_and_never_panics() {
    let (seed, mut rng) = (seed(), Rng::new(seed(), 11));
    let valid: Vec<Vec<u8>> = (0..64)
        .map(|_| crate::refs::encode_branch(&random_commit(&mut rng).id()))
        .collect();
    let mut tally = Tally::default();
    for case in 0..cases() * 5 {
        let context = format!("seed {seed}, case {case}");
        let input = arbitrary_input(&mut rng, &valid);
        match crate::refs::parse_branch(&input) {
            Ok(commit) => {
                tally.accepted += 1;
                assert_eq!(
                    crate::refs::encode_branch(&commit),
                    input,
                    "{context}: a second spelling was accepted: {:?}",
                    String::from_utf8_lossy(&input)
                );
            }
            Err(error) => {
                tally.rejected += 1;
                check_rejection(&error, ref_line(&error), 1, &input, &context);
            }
        }
    }
    assert!(tally.accepted > 0 && tally.rejected > 0, "vacuous run");
}
