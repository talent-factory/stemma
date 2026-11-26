//! The operation log (ADR-011): what every operation did to the pointers of a repository.
//!
//! The log is local. It is not an object and is not exchanged with the history (ADR-005). It is
//! the directory `.stemma/oplog/`, and each entry is a file of its own, named by its number: twelve
//! decimal digits, zero-padded, starting with 1. An entry holds the actor, the time, the name of
//! the operation, and for each pointer that the operation changes (HEAD, the working-copy commit,
//! branches, tags) its old and its new value. The old values are the commits that stop being reachable,
//! so the log keeps them (ADR-010).
//!
//! An entry is written, and flushed, *before* the pointer moves. It is therefore an intent: the
//! move can still fail, because the file system fails between the entry and the move. Nothing is
//! written after the move, so nothing can be half written; [`outcomes`] reads from the chain of
//! the entries of a pointer whether a move happened (ADR-011).
//!
//! # The encoding
//!
//! Version 1 of the entry encoding, from before the hash chain, is strict text. A reader accepts
//! what [`Entry::encode`] writes and nothing else, so one entry has exactly one encoding
//! (ADR-005):
//!
//! ```text
//! stemma-oplog 1
//! number 42
//! time 1700000000
//! actor human daniel
//! operation snapshot
//! head branch:main commit:b3:0eec…6eb6
//! working-copy b3:1d71…eed9 b3:4feb…b32c
//! branch main b3:8cb4…b3e8 b3:b5f2…e662
//! ```
//!
//! An entry of the operation `tag` has a line `tag v1 - b3:4feb…b32c` instead, and one of the
//! operation `untag` a line `tag v1 b3:4feb…b32c -` (see below).
//!
//! The `…` stands for digits that are left out here; a real ID has all 64. The first five lines come
//! in this order, each once. Then come the lines of the pointers that changed, at least one (but
//! see the change of the owner below): `head`, then `working-copy`, then one `branch <name>` for
//! each branch, sorted by name in UTF-8 byte order and each name once, then one `tag <name>` for
//! each tag, sorted and unique in the same way. A tag never moves (ADR-008), so a tag line made by
//! the writer has the old value `-` (a tag is made) or the new value `-` (a tag is deleted); the
//! reader does not enforce that. Each line has the old and
//! the new value, which differ. `-` means that the pointer did not exist, or does not exist any more.
//! A branch and the working-copy commit have a commit ID or `-`; HEAD has `branch:<name>`,
//! `commit:<id>`, or `-`. The file ends with a newline and has nothing after the last pointer.
//!
//! After the tags come the remote references (ADR-008), which the entry `fetch` and the restore
//! of one name, sorted in the order of [`RemoteRefName`] (the remote, the branches before the
//! tags, the name) and each once. A value has two parts, the commit and the hash of the record
//! entry of that name on the hub, and `- -` stands for a reference that does not exist; a value
//! with one part `-` is refused:
//!
//! ```text
//! remote remotes/origin/heads/main b3:8cb4… b3:a1a1… b3:b5f2… b3:a1a1…
//! remote remotes/origin/tags/v1 - - b3:4feb… b3:a1a1…
//! ```
//!
//! The line came with version 17 of the repository format (STEMMA-295, ADR-009): a program that
//! knows only version 16 refuses the repository instead of the line.
//!
//! # Stubs and purges (version 2)
//!
//! A purge (ADR-014) replaces every entry that names a commit it rewrote by a *stub* with the same
//! number, and writes an entry of its own. Both have version 2 of the encoding, and nothing else
//! has, so every entry still has one encoding:
//!
//! ```text
//! stemma-oplog 2              stemma-oplog 2
//! number 5                    number 9
//! time 1700000000             time 1700000500
//! actor human daniel          actor human daniel
//! stub                        operation purge
//!                             commits 3
//!                             stubs 4-5 7
//! ```
//!
//! A stub keeps the number, the time, and the actor, and drops the operation and every ID. The
//! `purge` entry names no pointer and no ID: the count of rewritten commits, and the numbers of the
//! stubs as ranges in ascending order that neither overlap nor touch, all below its own number, or
//! `-` for none. A purge that moved tags (STEMMA-185) has the line `tags <n>` between `commits`
//! and `stubs`, with the number of tags and no name; it is left out for none, so the entries from
//! before keep their encoding, and a `tags 0` is not read. A list that does not fit into one
//! entry is split over several, and the first has the counts. A stub that no `purge` entry lists
//! is an error of the log ([`check_stubs`]); a listed entry that is not a stub yet is not, as
//! the purge writes its entry first. A change cannot have the operation `purge`.
//!
//! # The hash chain (version 3)
//!
//! Every new entry that the writer makes names the hash of the entry before it (ADR-011,
//! STEMMA-41); only the stub of an entry from before the chain keeps version 2 and names none.
//! A stub that names what it replaced has version 4 (`stub <hash> approval` or
//! `stub <hash> other`, STEMMA-256); the stub in the example below is older, version 3.
//! A change, a stub, and a `purge` entry of version 3 have the line `previous` after the actor,
//! with the hash, or `-` for the entry that starts the chain:
//!
//! ```text
//! stemma-oplog 3         stemma-oplog 3         stemma-oplog 3
//! number 42              number 5               number 9
//! time 1700000000        time 1700000000        time 1700000500
//! actor human daniel     actor human daniel     actor human daniel
//! previous b3:7f0c…91aa  previous b3:20d4…6c03  previous b3:e5b1…0f7d
//! operation snapshot     stub b3:9a33…d215      operation purge
//! working-copy … …                              commits 3
//!                                               stubs 4-5 7
//! ```
//!
//! The hash of an entry ([`Entry::hash`]) is BLAKE3 over its envelope: the line
//! `stemma-oplog-hash 1`, then `number`, `time`, `actor`, `previous`, and `body` with the hash of
//! the body. The hash of the body ([`Entry::body_hash`]) is BLAKE3 over `stemma-oplog-body 1` and
//! the lines after `previous`: the operation and the pointers, or the operation, the count, and
//! the stubs. A stub keeps the hash of the body that it replaced, so it has the hash of the entry,
//! and a purge does not break the chain; time and actor stay covered.
//!
//! The reader checks the chain ([`check_chain`]): a change, a removal, an insertion, or a swap of
//! entries breaks it at that place, and an entry of version 1 or 2 after the chain is refused. A
//! log from before the chain keeps its old entries in front, and the first new entry starts the
//! chain. The tip file names the hash of the last entry as well if that is in the chain, so a
//! replaced last entry is found.
//! The chain shows that the entries agree with one another, not who wrote them: whoever can write
//! `.stemma` can compute every later hash and the tip again. Only an anchor kept outside the
//! repository, a hash that `stemma op verify` printed, shows that (ADR-011).
//!
//! # The change of the owner
//!
//! The entry of the operation `take-over` makes its actor the owner of the working copy
//! (ADR-010). The owner is not a pointer, so the entry has no line after the operation; it is
//! the only change that may have none, and it may have no other. Only a repository of format 5
//! has it, so a program that cannot read it refuses the repository before it reads the log.
//!
//! # Numbers and writers
//!
//! A writer takes the next number that is free and publishes the file with a hard link, which
//! fails if the number is taken, so the entry is whole from the first moment it can be seen. If
//! the number is taken, another writer was faster; the writer then looks for the end of the log
//! again and tries the next free number. [`Repository`](crate::repository::Repository) writes
//! under the lock of the references, so its writers do not meet here; the retry is for any other
//! writer.
//!
//! The numbers have no gap: a gap means that an entry is lost, and the reader reports it. A
//! directory listing is not a snapshot while a writer adds files, so the reader lists the directory
//! again before it reports a gap, and a writer that only needs the end of the log looks at the
//! largest number and at the tip file (below). Names that start with a dot are skipped, as in the
//! references, because file managers and editors leave such files; any other name is reported.
//!
//! Only the repository writes. `FsOpLog::append`, `append_purge`, `replace_with_stub`,
//! `Entry::linked`, `Entry::to_stub`, and `Purge::try_new` are visible inside this crate only, so no
//! caller can add an entry past the authorizer or invent one, a stub, or a purge that was never
//! in the log. `replace_with_stub` is the only writer that changes an entry that exists: a purge
//! replaces it by a stub (ADR-014).
//!
//! A gap does not stop a writer: the reader reports it, and a refusal would block the move that
//! repairs the log. A lost *last* entry leaves no gap, so the file `oplog-tip` next to `oplog/`
//! holds the number of the last entry. The writer replaces it after the entry is published. A tip
//! beyond the largest number in the directory means that the last entries are lost: the writer
//! refuses ([`OpLogError::TipAhead`]) and the reader reports it, as the number would go to another
//! move otherwise. A tip that is behind or a missing file is no fault for the readers and the
//! writers; [`FsOpLog::verify`] reports a missing tip, or one more than one behind, if the last
//! entry is in the chain. The tip file of
//! version 2 names the hash of the last entry too (`stemma-oplog-tip 2`, `number <n>`,
//! `hash <hash>`), and the writer compares the entry that the tip names with it before the first
//! attempt to publish, and refuses to link a new entry behind a replaced one
//! ([`OpLogError::TipMismatch`]); an entry that another writer published in between was compared
//! by that writer. The tip guards against accidents and crashes, not against an attack: whoever
//! can write the entry can write the tip as well (ADR-011).
//!
//! `tmp/` holds scratch files. A temporary name is removed after the entry is published, on a best
//! effort basis: a name that stays is harmless and costs only space until the next writer that
//! takes the lock of the references removes it (ADR-011, STEMMA-31).
//!
//! An entry is at most 64 KiB. The writer refuses a larger one before it writes anything, so the
//! reader never meets a file that the writer made and cannot read; the actor and the names in it
//! have no limit of their own.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::fault;
use crate::object::{parse_seconds, CommitId, ObjectId, ObjectIdParseError, TimeError};
use crate::protocol::statement::StatementHash;
use crate::protocol::RecordHash;
use crate::refs::{BranchName, BranchNameError, Head, MAIN_WORKTREE};
use crate::register::Identity;
use crate::remote::{RemoteName, RemoteRef, RemoteRefName, RemoteRefNameError};
use crate::signature::PublicKey;
use crate::store::{create_temp, sync_directory, unique_temp_name, StoreError};
use crate::{Actor, ActorParseError, ActorRef};

/// The first line of an entry of a change from before the hash chain, with the version of the
/// encoding. It is still read; a writer no longer writes it.
const HEADER: &str = "stemma-oplog 1";

/// The first line of a stub and of a `purge` entry from before the hash chain (ADR-014). Version 2
/// was written only for these two, so that every entry has exactly one encoding; a writer still
/// writes it for the stub of an entry from before the chain.
const HEADER_2: &str = "stemma-oplog 2";

/// The first line of every entry in the hash chain: a change, a stub, or a `purge` entry that
/// names the hash of the entry before it (ADR-011, STEMMA-41). A writer writes this version for
/// every new entry but a stub, and keeps it for a stub that was written before the kind (see
/// [`HEADER_4`]).
const HEADER_3: &str = "stemma-oplog 3";

/// The first line of the stub that says what it replaced (ADR-011, ADR-014, STEMMA-256): version 3
/// plus the kind of the entry after the hash of the body. A writer writes it for the stub of an
/// entry in the chain; a stub of version 3 is older and has no kind.
const HEADER_4: &str = "stemma-oplog 4";

/// The first line of the text that the hash of an entry in the chain is computed over.
const HASH_HEADER: &str = "stemma-oplog-hash 1";

/// The first line of the text that the hash of the body of an entry is computed over.
const BODY_HEADER: &str = "stemma-oplog-body 1";

/// The name of the operation of a purge. A change cannot have it: a purge entry names no pointer
/// (ADR-014).
pub const PURGE: &str = "purge";

/// The name of the operation that makes the actor of the entry the owner of the working copy
/// (ADR-010). Its entry names no pointer, as the owner is not one, and only its entry may name
/// none.
pub const TAKE_OVER: &str = "take-over";

/// The operation of the entry that a holder writes when it acquires the lease of a branch
/// (ADR-004, ADR-011). The four lease operations belong to the entry of a lease and to no change.
pub const LEASE_ACQUIRE: &str = "lease-acquire";

/// The operation of the entry that a holder writes when it gives its lease back.
pub const LEASE_RELEASE: &str = "lease-release";

/// The operation of the entry that an actor writes when it takes a lease that has expired.
pub const LEASE_TAKEOVER: &str = "lease-takeover";

/// The operation of the entry that an owner writes when it ends the lease of another actor.
pub const LEASE_OVERRIDE: &str = "lease-override";

/// The operation of the entry that the write owner of a branch writes when it opens a pull
/// request (ADR-004, ADR-011). The pull request operations belong to the entry of a pull request
/// and to no change.
pub const PR_OPEN: &str = "pr-open";

/// The operation of the entry that ends a pull request without a merge.
pub const PR_CLOSE: &str = "pr-close";

/// The operation of the entry that records the merge of a pull request.
pub const PR_MERGE: &str = "pr-merge";

/// The operation of the entry that records the result of a check run: the tested commit, the tip
/// of `main` it was merged onto for the test, and whether it passed (ADR-019, STEMMA-240). It
/// belongs to the entry of a check and to no change.
pub const CHECK: &str = "check";

/// The first version of the body of a `check` entry, the line `version 1`: the entry of the
/// repository formats 11 to 15, whose proof was a file that signed the hash of the entry. The
/// entry holds a type and a version, as the signed payload does (ADR-003), so a later form is
/// told from this one. A reader accepts this version and [`CHECK_STATEMENT_VERSION`]; the writer
/// of the repository writes the later one only.
pub const CHECK_VERSION: u32 = 1;

/// The version of the body of a `check` entry that names its signed statement, the line
/// `version 2` (ADR-025, repository format 16): the lines of version 1 and the line
/// `statement <hash>` after them.
pub const CHECK_STATEMENT_VERSION: u32 = 2;

/// The operation of the entry that approves the head of a pull request (ADR-004 rule 4). The
/// approver signs the statement that the entry names (ADR-025); in the repository formats 10
/// to 15 the signature was a proof file.
pub const PR_APPROVE: &str = "pr-approve";

/// The operation of the entry that pins the key of the hub of a remote again (ADR-026,
/// STEMMA-297): a human replaces the pinned key by a new one, with a fingerprint from a second
/// channel. It belongs to the entry of a pinning and to no change (repository format 18).
pub const REMOTE_PIN: &str = "remote-pin";

/// The most reviewers that the entry of an opening names.
pub const MAX_REVIEWERS: usize = 32;

/// The reason that the entry of a takeover names: the lease of the earlier holder had expired.
pub const EXPIRED: &str = "expired";

/// The longest reason of an owner override, in bytes.
pub const MAX_REASON_LEN: usize = 1024;

/// The start of the first line of every version of the encoding.
const HEADER_PREFIX: &str = "stemma-oplog ";

/// The number of digits of a file name.
const NUMBER_WIDTH: usize = 12;

/// The largest number of an entry: the largest value of twelve decimal digits.
pub const MAX_NUMBER: u64 = 999_999_999_999;

/// The largest size of an entry that is read, in bytes. An entry is a few hundred bytes; a file much
/// larger is not one.
const MAX_ENTRY_LEN: u64 = 64 * 1024;

/// How many numbers a writer tries before it gives up. A writer tries the next number only because
/// another writer took the one before, which means that the other one made progress, so the limit is
/// reached only by a writer that is overtaken again and again. Any other failure returns at once.
const ATTEMPTS: usize = 64;

/// The longest name of an operation, in characters.
const MAX_OPERATION_LEN: usize = 32;

/// The name of an operation, such as `init`, `snapshot`, or `commit`.
///
/// It is a lowercase ASCII letter followed by lowercase letters, digits, and `-`, at most 32
/// characters. The rule keeps the name one token of a line, and one spelling of it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OperationName(String);

/// Why a string is not the name of an operation. The `Display` form is the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OperationNameError {
    /// The name is empty.
    Empty,
    /// The name is longer than 32 bytes, which is 32 characters for a name that is valid. Carries
    /// its length in bytes. The length is checked before the characters, so a long name with a
    /// character that is not allowed is reported as too long.
    TooLong(usize),
    /// The name does not start with a lowercase letter.
    BadStart,
    /// A character is not a lowercase letter, a digit, or `-`. Carries its position in bytes.
    BadCharacter(usize),
}

impl fmt::Display for OperationNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("the name of an operation is empty"),
            Self::TooLong(length) => write!(
                f,
                "the name of an operation has at most {MAX_OPERATION_LEN} bytes, which is {MAX_OPERATION_LEN} \
                 characters of a valid name, not {length} bytes"
            ),
            Self::BadStart => {
                f.write_str("the name of an operation starts with a lowercase letter")
            }
            Self::BadCharacter(position) => write!(
                f,
                "byte {position} of the name of an operation is not a lowercase letter, a digit, \
                 or `-`"
            ),
        }
    }
}

impl Error for OperationNameError {}

impl OperationName {
    /// Checks the name.
    ///
    /// # Errors
    ///
    /// Returns [`OperationNameError`] for a name that is empty, longer than 32 characters, does
    /// not start with a lowercase letter, or has another character than a lowercase letter, a
    /// digit, or `-`.
    pub fn try_new(name: impl Into<String>) -> Result<Self, OperationNameError> {
        let name = name.into();
        if name.is_empty() {
            return Err(OperationNameError::Empty);
        }
        if name.len() > MAX_OPERATION_LEN {
            return Err(OperationNameError::TooLong(name.len()));
        }
        if !name.as_bytes()[0].is_ascii_lowercase() {
            return Err(OperationNameError::BadStart);
        }
        if let Some(position) = name
            .bytes()
            .position(|b| !(b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
        {
            return Err(OperationNameError::BadCharacter(position));
        }
        Ok(Self(name))
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OperationName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for OperationName {
    type Err = OperationNameError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::try_new(name)
    }
}

/// The value of one pointer before and after an operation. `None` means that the pointer did not
/// exist, or does not exist any more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moved<T> {
    old: Option<T>,
    new: Option<T>,
}

impl<T> Moved<T> {
    /// A pointer that goes from `old` to `new`. [`Change::try_new`] rejects a pointer where the two
    /// are equal.
    pub fn from_to(old: Option<T>, new: Option<T>) -> Self {
        Self { old, new }
    }

    /// The value before the operation.
    pub fn before(&self) -> Option<&T> {
        self.old.as_ref()
    }

    /// The value after the operation.
    pub fn after(&self) -> Option<&T> {
        self.new.as_ref()
    }
}

/// The pointer of a change, for a message: HEAD, the working-copy commit, a branch, or a tag.
/// A tag never moves, so an entry creates one (the operation `tag`) or deletes one (the
/// operation `untag`), and none moves one (ADR-011); `verify` names a tag too.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Pointer {
    /// HEAD.
    Head,
    /// The working-copy commit.
    WorkingCopy,
    /// A branch.
    Branch(BranchName),
    /// A tag (ADR-008).
    Tag(BranchName),
    /// A remote reference (ADR-008), as `verify` names it; the entries of `fetch` and `push`
    /// name it too.
    Remote(RemoteRefName),
}

impl fmt::Display for Pointer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Head => f.write_str("HEAD"),
            Self::WorkingCopy => f.write_str("the working-copy commit"),
            Self::Branch(name) => write!(f, "the branch `{name}`"),
            Self::Tag(name) => write!(f, "the tag `{name}`"),
            Self::Remote(name) => write!(f, "the remote reference `{name}`"),
        }
    }
}

/// Why a change or an entry is rejected. The `Display` form is the message.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EntryError {
    /// The operation changes no pointer. Only a change writes an entry (ADR-011); the operation
    /// `take-over` is the one that changes the owner instead (ADR-010).
    NoChange,
    /// A change with the operation `take-over` names a pointer. It changes the owner of the
    /// working copy, and its entry names no pointer.
    TakeOverWithPointer,
    /// The old and the new value of a pointer are equal, so it did not change.
    Unchanged(Pointer),
    /// A branch is listed twice.
    DuplicateBranch(BranchName),
    /// A tag is listed twice.
    DuplicateTag(BranchName),
    /// A remote reference is listed twice.
    DuplicateRemote(RemoteRefName),
    /// The number is 0 or larger than [`MAX_NUMBER`].
    NumberOutOfRange(u64),
    /// A change has the operation `purge`, which only the entry of a purge has, or one of the four
    /// lease operations, which only the entry of a lease has.
    ReservedOperation,
    /// A `purge` entry lists a stub whose number is not below its own: the purge entry comes after
    /// the entries that it replaces (ADR-014).
    StubNotBefore {
        /// The largest number that the purge lists.
        stub: u64,
        /// The number of the purge entry.
        purge: u64,
    },
    /// The count of commits of a purge is larger than [`MAX_NUMBER`].
    TooManyCommits(u64),
    /// The count of tags that a purge moved is larger than [`MAX_NUMBER`].
    TooManyTags(u64),
    /// A purge lists a number that is 0 or larger than [`MAX_NUMBER`] as a stub.
    StubOutOfRange(u64),
    /// The entry of a purge cannot become a stub. Carries its number.
    PurgeNotReplaceable(u64),
    /// A stub written before the chain has the hash of a body, which only a stub in the chain
    /// keeps. Carries its number.
    StubBodyOutsideChain(u64),
    /// A stub in the chain has no hash of a body, so its hash cannot be computed. Carries its
    /// number.
    StubWithoutBody(u64),
    /// Entry 1 names the hash of an entry before it, but there is none.
    FirstAfter,
    /// The name of a worktree is not one that a worktree has: ASCII letters, digits, `-`, `_`, and
    /// `.` (not first), at most 255 bytes, and not `main`, which the entry leaves out.
    BadWorktree(String),
    /// The directory of a worktree holds a control character, or the entry names no worktree.
    BadLocation(String),
    /// An entry names the former owner of a working copy but no directory of a worktree.
    FormerOwnerWithoutRemoval,
    /// A lease entry was written before the hash chain. Carries its number. The four lease
    /// entries came with the chain, so none has an older encoding.
    LeaseOutsideChain(u64),
    /// The entry of a lease cannot become a stub. Carries its number. A purge never lists it, as
    /// it names no commit.
    LeaseNotReplaceable(u64),
    /// A lease ends at or before the time of the entry that grants it.
    LeaseExpiry,
    /// A takeover or an override names the actor who acts as the earlier holder.
    LeaseSameActor,
    /// The reason of an override is empty, longer than [`MAX_REASON_LEN`] bytes, has a control
    /// character, or starts or ends with a space.
    BadReason,
    /// A pull request entry was written before the hash chain. Carries its number. The three
    /// pull request entries came with the chain, so none has an older encoding.
    PullOutsideChain(u64),
    /// The reviewers of an opening are not in strictly ascending order, name the opener, or are
    /// more than [`MAX_REVIEWERS`].
    BadReviewers,
    /// A pull request entry has fields that its kind does not have, or lacks one that it has.
    BadPullShape,
    /// A check entry was written before the hash chain. Carries its number. The entry came with
    /// the chain, so it has no older encoding.
    CheckOutsideChain(u64),
    /// A check entry names the commit it tested as its base: a run merged onto itself says
    /// nothing. Carries its number.
    CheckBaseIsCommit(u64),
    /// The entry of a pinning was written before the hash chain. Carries its number. The entry
    /// came with the chain, so it has no older encoding.
    RemotePinOutsideChain(u64),
    /// The entry of a pinning names the same key as the old and the new key: nothing changed.
    RemotePinUnchanged,
    /// The entry of a pinning names an agent as its actor. Only a human pins the key of a hub.
    RemotePinByAgent,
    /// The entry of a pinning names no commit, so a purge never replaces it with a stub.
    /// Carries its number.
    RemotePinNotReplaceable(u64),
}

impl fmt::Display for EntryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoChange => f.write_str("the operation changes no pointer"),
            Self::TakeOverWithPointer => write!(
                f,
                "a change with the operation `{TAKE_OVER}` changes the owner and names no pointer"
            ),
            Self::Unchanged(pointer) => write!(f, "{pointer} has the same old and new value"),
            Self::DuplicateBranch(name) => write!(f, "the branch `{name}` is listed twice"),
            Self::DuplicateTag(name) => write!(f, "the tag `{name}` is listed twice"),
            Self::DuplicateRemote(name) => {
                write!(f, "the remote reference `{name}` is listed twice")
            }
            Self::NumberOutOfRange(number) => {
                write!(f, "entry number {number} is not between 1 and {MAX_NUMBER}")
            }
            Self::ReservedOperation => write!(
                f,
                "a change cannot have the operation `{PURGE}`, which only the entry of a purge has, \
                 or an operation `{LEASE_ACQUIRE}`, `{LEASE_RELEASE}`, `{LEASE_TAKEOVER}`, or \
                 `{LEASE_OVERRIDE}`, which only the entry of a lease has, or `{PR_OPEN}`, `{PR_CLOSE}`, \
                 `{PR_MERGE}`, or `{PR_APPROVE}`, which only the entry of a pull request has, or \
                 `{CHECK}`, which only the entry of a check has"
            ),
            Self::StubNotBefore { stub, purge } => write!(
                f,
                "the purge entry {purge} lists entry {stub} as a stub, but a purge entry comes \
                 after the entries that it lists"
            ),
            Self::TooManyCommits(count) => write!(
                f,
                "a purge of {count} commits cannot be recorded; the count is at most {MAX_NUMBER}"
            ),
            Self::TooManyTags(count) => write!(
                f,
                "a purge that moved {count} tags cannot be recorded; the count is at most \
                 {MAX_NUMBER}"
            ),
            Self::StubOutOfRange(number) => write!(
                f,
                "a purge cannot list entry {number} as a stub, as it is not between 1 and \
                 {MAX_NUMBER}"
            ),
            Self::PurgeNotReplaceable(number) => {
                write!(
                    f,
                    "entry {number} is the entry of a purge and cannot become a stub"
                )
            }
            Self::StubBodyOutsideChain(number) => write!(
                f,
                "the stub {number} was written before the hash chain, but has the hash of a body or a kind"
            ),
            Self::StubWithoutBody(number) => write!(
                f,
                "the stub {number} is in the hash chain, but has no hash of a body"
            ),
            Self::FirstAfter => f.write_str(
                "entry 1 names the hash of an entry before it, but it is the first entry",
            ),
            Self::BadWorktree(name) => write!(
                f,
                "`{}` is not the name of a linked worktree",
                name.escape_debug()
            ),
            Self::BadLocation(path) => write!(
                f,
                "`{}` cannot be the directory of a worktree in an entry",
                path.escape_debug()
            ),
            Self::FormerOwnerWithoutRemoval => write!(
                f,
                "only an entry that removes a worktree can name the former owner of its working copy"
            ),
            Self::LeaseOutsideChain(number) => write!(
                f,
                "the lease entry {number} was written before the hash chain, which no lease entry \
                 was"
            ),
            Self::LeaseNotReplaceable(number) => write!(
                f,
                "entry {number} is the entry of a lease and cannot become a stub"
            ),
            Self::LeaseExpiry => f.write_str("a lease must end after the time of its entry"),
            Self::LeaseSameActor => f.write_str(
                "a takeover or an override names another actor as the earlier holder; the earlier \
                 holder acquires the lease again",
            ),
            Self::PullOutsideChain(number) => write!(
                f,
                "the pull request entry {number} was written before the hash chain, which no \
                 pull request entry was"
            ),
            Self::BadReviewers => write!(
                f,
                "the reviewers of a pull request are at most {MAX_REVIEWERS}, in ascending order, \
                 each identity once, and none is the opener"
            ),
            Self::BadPullShape => {
                f.write_str("a pull request entry has the fields of its kind and no others")
            }
            Self::CheckOutsideChain(number) => write!(
                f,
                "the check entry {number} cannot be older than the hash chain"
            ),
            Self::CheckBaseIsCommit(number) => write!(
                f,
                "the base of the check entry {number} is the commit it tested"
            ),
            Self::RemotePinOutsideChain(number) => write!(
                f,
                "the entry {number} that pins the key of a hub cannot be older than the hash chain"
            ),
            Self::RemotePinNotReplaceable(number) => write!(
                f,
                "the entry {number} that pins the key of a hub names no commit and is never a stub"
            ),
            Self::RemotePinByAgent => {
                f.write_str("the entry of a pinning names an agent; only a human pins a key")
            }
            Self::RemotePinUnchanged => {
                f.write_str("the entry of a pinning names the same key as its old and its new key")
            }
            Self::BadReason => write!(
                f,
                "the reason of an override is one line of 1 to {MAX_REASON_LEN} bytes, without a \
                 control character and without a space at its ends"
            ),
        }
    }
}

impl Error for EntryError {}

/// What an operation did to the pointers, without the number that the log gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    time: i64,
    actor: Actor,
    operation: OperationName,
    head: Option<Moved<Head>>,
    working_copy: Option<Moved<CommitId>>,
    branches: Vec<(BranchName, Moved<CommitId>)>,
    tags: Vec<(BranchName, Moved<CommitId>)>,
    /// The remote references that a `fetch` moved or removed, or that a restore of one puts
    /// back (ADR-008), sorted by name.
    remotes: Vec<(RemoteRefName, Moved<RemoteRef>)>,
    /// The linked worktree of the entry, boxed so that the common entry stays small.
    worktree: Option<Box<WorktreeTag>>,
}

/// The pointers of a [`Change`], for [`Change::build`]. A pointer that is `None`, or a list that is
/// empty, is not part of the change.
#[derive(Debug, Default)]
pub(crate) struct Pointers {
    pub(crate) head: Option<Moved<Head>>,
    pub(crate) working_copy: Option<Moved<CommitId>>,
    pub(crate) branches: Vec<(BranchName, Moved<CommitId>)>,
    pub(crate) tags: Vec<(BranchName, Moved<CommitId>)>,
    pub(crate) remotes: Vec<(RemoteRefName, Moved<RemoteRef>)>,
}

/// The worktree that an entry names and the directory of it, if the entry makes or removes one.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WorktreeTag {
    name: String,
    location: Option<String>,
    /// The owner of the working copy that a human removed with `worktree remove --take-over`
    /// while its directory was gone (ADR-010); only an entry of `worktree-remove` has it.
    former_owner: Option<Actor>,
}

impl Change {
    /// Builds a change. Branches are kept sorted by name, so the order of the input does not
    /// matter. The time is the one that the writer claims, in seconds since the Unix epoch (UTC),
    /// as in a commit (ADR-005). The change names no tag; [`try_new_with_tags`](Self::try_new_with_tags)
    /// does.
    ///
    /// # Errors
    ///
    /// Returns [`EntryError::ReservedOperation`] for the operation `purge`, which has an entry of
    /// its own kind, [`EntryError::NoChange`] if no pointer is given, unless the operation is
    /// `take-over`, [`EntryError::TakeOverWithPointer`] if it is and a pointer is given,
    /// [`EntryError::Unchanged`]
    /// for a pointer whose old and new value are equal, and [`EntryError::DuplicateBranch`] for a
    /// branch that is listed twice.
    pub fn try_new(
        time: i64,
        actor: Actor,
        operation: OperationName,
        head: Option<Moved<Head>>,
        working_copy: Option<Moved<CommitId>>,
        branches: Vec<(BranchName, Moved<CommitId>)>,
    ) -> Result<Self, EntryError> {
        Self::try_new_with_tags(
            time,
            actor,
            operation,
            head,
            working_copy,
            branches,
            Vec::new(),
        )
    }

    /// Builds a change that also names tags (ADR-008, ADR-011), which are kept sorted by name like
    /// the branches. A tag is a pointer of its own namespace, so a tag and a branch may have the
    /// same name.
    ///
    /// # Errors
    ///
    /// As [`try_new`](Self::try_new), and [`EntryError::DuplicateTag`] for a tag that is listed
    /// twice. A tag counts as a pointer for [`EntryError::NoChange`].
    pub fn try_new_with_tags(
        time: i64,
        actor: Actor,
        operation: OperationName,
        head: Option<Moved<Head>>,
        working_copy: Option<Moved<CommitId>>,
        branches: Vec<(BranchName, Moved<CommitId>)>,
        tags: Vec<(BranchName, Moved<CommitId>)>,
    ) -> Result<Self, EntryError> {
        Self::build(
            time,
            actor,
            operation,
            Pointers {
                head,
                working_copy,
                branches,
                tags,
                remotes: Vec::new(),
            },
        )
    }

    /// Builds a change that names remote references only (ADR-008, ADR-011): the entry `fetch`,
    /// and the restore of one. The references are kept sorted by name, in the order of
    /// [`RemoteRefName`]. A remote reference that the change creates has no old value, and one
    /// that it removes has no new value.
    ///
    /// # Errors
    ///
    /// As [`try_new`](Self::try_new), and [`EntryError::DuplicateRemote`] for a remote reference
    /// that is listed twice.
    pub fn try_new_with_remotes(
        time: i64,
        actor: Actor,
        operation: OperationName,
        remotes: Vec<(RemoteRefName, Moved<RemoteRef>)>,
    ) -> Result<Self, EntryError> {
        Self::build(
            time,
            actor,
            operation,
            Pointers {
                remotes,
                ..Pointers::default()
            },
        )
    }

    /// Builds a change with any of the pointers: the one place that checks the rules of a change.
    pub(crate) fn build(
        time: i64,
        actor: Actor,
        operation: OperationName,
        pointers: Pointers,
    ) -> Result<Self, EntryError> {
        let Pointers {
            head,
            working_copy,
            mut branches,
            mut tags,
            mut remotes,
        } = pointers;
        if operation.as_str() == PURGE
            || LeaseKind::from_operation(operation.as_str()).is_some()
            || PullKind::from_operation(operation.as_str()).is_some()
            || operation.as_str() == CHECK
            || operation.as_str() == REMOTE_PIN
        {
            return Err(EntryError::ReservedOperation);
        }
        let no_pointer = head.is_none()
            && working_copy.is_none()
            && branches.is_empty()
            && tags.is_empty()
            && remotes.is_empty();
        match (operation.as_str() == TAKE_OVER, no_pointer) {
            (false, true) => return Err(EntryError::NoChange),
            (true, false) => return Err(EntryError::TakeOverWithPointer),
            _ => {}
        }
        if head.as_ref().is_some_and(|m| m.old == m.new) {
            return Err(EntryError::Unchanged(Pointer::Head));
        }
        if working_copy.as_ref().is_some_and(|m| m.old == m.new) {
            return Err(EntryError::Unchanged(Pointer::WorkingCopy));
        }
        branches.sort_by(|a, b| a.0.cmp(&b.0));
        for pair in branches.windows(2) {
            if pair[0].0 == pair[1].0 {
                return Err(EntryError::DuplicateBranch(pair[0].0.clone()));
            }
        }
        if let Some((name, _)) = branches.iter().find(|(_, m)| m.old == m.new) {
            return Err(EntryError::Unchanged(Pointer::Branch(name.clone())));
        }
        tags.sort_by(|a, b| a.0.cmp(&b.0));
        for pair in tags.windows(2) {
            if pair[0].0 == pair[1].0 {
                return Err(EntryError::DuplicateTag(pair[0].0.clone()));
            }
        }
        if let Some((name, _)) = tags.iter().find(|(_, m)| m.old == m.new) {
            return Err(EntryError::Unchanged(Pointer::Tag(name.clone())));
        }
        remotes.sort_by(|a, b| a.0.cmp(&b.0));
        for pair in remotes.windows(2) {
            if pair[0].0 == pair[1].0 {
                return Err(EntryError::DuplicateRemote(pair[0].0.clone()));
            }
        }
        if let Some((name, _)) = remotes.iter().find(|(_, m)| m.old == m.new) {
            return Err(EntryError::Unchanged(Pointer::Remote(name.clone())));
        }
        Ok(Self {
            time,
            actor,
            operation,
            head,
            working_copy,
            branches,
            tags,
            remotes,
            worktree: None,
        })
    }

    /// The time that the writer claims, in seconds since the Unix epoch (UTC). It proves nothing;
    /// the order of the entries is the order of their numbers.
    pub fn time(&self) -> i64 {
        self.time
    }

    /// Who ran the operation.
    pub fn actor(&self) -> &Actor {
        &self.actor
    }

    /// The name of the operation.
    pub fn operation(&self) -> &OperationName {
        &self.operation
    }

    /// The change of HEAD, if the operation changed it.
    pub fn head(&self) -> Option<&Moved<Head>> {
        self.head.as_ref()
    }

    /// The change of the working-copy commit, if the operation changed it.
    pub fn working_copy(&self) -> Option<&Moved<CommitId>> {
        self.working_copy.as_ref()
    }

    /// The branches that the operation changed, sorted by name.
    pub fn branches(&self) -> &[(BranchName, Moved<CommitId>)] {
        &self.branches
    }

    /// The tags that the operation made or deleted, sorted by name. A tag never moves (ADR-008), so
    /// each is made (the old value is `None`) or deleted (the new value is `None`).
    pub fn tags(&self) -> &[(BranchName, Moved<CommitId>)] {
        &self.tags
    }

    /// The remote references that the operation moved, created, or removed, sorted by name
    /// (ADR-008). Only the entry `fetch` and the restore of one name them.
    pub fn remotes(&self) -> &[(RemoteRefName, Moved<RemoteRef>)] {
        &self.remotes
    }

    /// The linked worktree that the entry belongs to, or `None` for the main worktree, whose name
    /// the entry leaves out (ADR-011). `HEAD` and the working-copy commit of the entry are those of
    /// this worktree.
    pub fn worktree(&self) -> Option<&str> {
        self.worktree.as_deref().map(|tag| tag.name.as_str())
    }

    /// The name of the worktree as a message shows it: the name of the linked worktree, or `main`.
    pub fn worktree_name(&self) -> &str {
        self.worktree().unwrap_or(MAIN_WORKTREE)
    }

    /// The directory of the worktree, in the entries that make or remove one (`worktree-add`,
    /// `worktree-remove`); other entries leave it out.
    pub fn location(&self) -> Option<&str> {
        self.worktree.as_deref()?.location.as_deref()
    }

    /// The owner of the working copy that the actor of the entry removed without being that owner
    /// (`worktree remove --take-over` of a worktree whose directory is gone, ADR-010), or `None`.
    pub fn former_owner(&self) -> Option<&Actor> {
        self.worktree.as_deref()?.former_owner.as_ref()
    }

    /// The change that also names the owner `owner` of the working copy it removes without being
    /// that owner. The entry must be a `worktree-remove` that names a worktree and its directory.
    ///
    /// # Errors
    ///
    /// Returns [`EntryError::FormerOwnerWithoutRemoval`] for another operation, for an entry that
    /// names no directory, for an actor that is an agent (only a human takes over, ADR-004), and
    /// for the actor of the entry as its own former owner.
    pub(crate) fn with_former_owner(mut self, owner: Actor) -> Result<Self, EntryError> {
        let removal = self.operation.as_str() == "worktree-remove"
            && !matches!(self.actor.view(), ActorRef::Agent { .. })
            && owner != self.actor;
        match self.worktree.as_deref_mut() {
            Some(tag) if removal && tag.location.is_some() => {
                tag.former_owner = Some(owner);
                Ok(self)
            }
            _ => Err(EntryError::FormerOwnerWithoutRemoval),
        }
    }

    /// The change made for the worktree `worktree` (`None` for the main one), with the directory
    /// `location` if the operation makes or removes the worktree. An entry that names a worktree
    /// already keeps it.
    ///
    /// # Errors
    ///
    /// Returns [`EntryError::BadWorktree`] for a name that no worktree has and
    /// [`EntryError::BadLocation`] for a directory that holds a control character, or one without a
    /// worktree.
    pub(crate) fn in_worktree(
        mut self,
        worktree: Option<String>,
        location: Option<String>,
    ) -> Result<Self, EntryError> {
        if self.worktree.is_some() {
            return Ok(self);
        }
        if let Some(name) = &worktree {
            if !valid_worktree_name(name) {
                return Err(EntryError::BadWorktree(name.clone()));
            }
        }
        if let Some(path) = &location {
            if worktree.is_none() || !valid_location(path) {
                return Err(EntryError::BadLocation(path.clone()));
            }
        }
        self.worktree = worktree.map(|name| {
            Box::new(WorktreeTag {
                name,
                location,
                former_owner: None,
            })
        });
        Ok(self)
    }

    /// Every commit that the change names, old and new: the roots that the garbage collection
    /// needs from this entry (ADR-007, ADR-011). A commit that is named twice is listed twice.
    pub fn commits(&self) -> Vec<CommitId> {
        let mut commits = Vec::new();
        if let Some(head) = &self.head {
            for value in [&head.old, &head.new] {
                if let Some(Head::Detached(commit)) = value {
                    commits.push(*commit);
                }
            }
        }
        let moves = self
            .working_copy
            .iter()
            .chain(self.branches.iter().map(|(_, moved)| moved))
            .chain(self.tags.iter().map(|(_, moved)| moved));
        for moved in moves {
            commits.extend(moved.old.iter().chain(moved.new.iter()).copied());
        }
        for (_, moved) in &self.remotes {
            commits.extend(
                moved
                    .old
                    .iter()
                    .chain(moved.new.iter())
                    .map(|reference| reference.commit),
            );
        }
        commits
    }
}

/// Whether `name` can be the name of a linked worktree in an entry: the name of its state
/// directory, which is reduced to letters, digits, `-`, `_`, and `.` (not first), and is never
/// `main`.
pub(crate) fn valid_worktree_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name != MAIN_WORKTREE
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Whether `path` fits on one line of an entry: it is not empty and holds no control character.
fn valid_location(path: &str) -> bool {
    !path.is_empty() && !path.chars().any(char::is_control)
}

/// Hashes `text` after the line `header`, so that a hash of one kind of text cannot pass for one
/// of another kind.
fn digest(header: &str, text: &str) -> ObjectId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(header.as_bytes());
    hasher.update(b"\n");
    hasher.update(text.as_bytes());
    ObjectId::from_digest(hasher.finalize())
}

/// The hash of an entry in the chain, which the next entry names (ADR-011, STEMMA-41). Its text is
/// that of an object ID, `b3:` and 64 lowercase hexadecimal digits, but it names an entry, not an
/// object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EntryHash(ObjectId);

/// The hash of the body of an entry in the chain, which the hash of the entry covers and a stub
/// keeps. A type of its own, so that it cannot be named where the hash of an entry belongs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BodyHash(ObjectId);

impl EntryHash {
    fn of(text: &str) -> Self {
        Self(digest(HASH_HEADER, text))
    }
}

impl BodyHash {
    fn of(text: &str) -> Self {
        Self(digest(BODY_HEADER, text))
    }
}

impl fmt::Display for EntryHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Display for BodyHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for EntryHash {
    type Err = ObjectIdParseError;

    /// Reads the canonical form only, as [`ObjectId`] does.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        ObjectId::from_str(text).map(Self)
    }
}

impl FromStr for BodyHash {
    type Err = ObjectIdParseError;

    /// Reads the canonical form only, as [`ObjectId`] does.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        ObjectId::from_str(text).map(Self)
    }
}

/// How an entry belongs to the hash chain of the log (ADR-011, STEMMA-41).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Chain {
    /// The entry has version 1 or 2: it was written before the chain and names no hash. Such
    /// entries can only come before the first entry of the chain.
    Unchained,
    /// The entry starts the chain (`previous -`): it is entry 1, or the first entry after the
    /// entries written before the chain.
    Start,
    /// The entry names the hash of the entry before it.
    After(EntryHash),
}

/// What remains of an entry that a purge replaced (ADR-014): its time and its actor; the
/// [`Entry`] keeps its number. The operation and every ID are gone, so it cannot be restored. A
/// stub of an entry in the chain keeps the hash of the body it replaced, so that the hash of the
/// entry, which the next one names, stays the same (ADR-014, STEMMA-41).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stub {
    time: i64,
    actor: Actor,
    body: Option<BodyHash>,
    replaced: Option<StubKind>,
}

/// What a stub replaced, as far as the proof of the entry needs to know (STEMMA-256). The proof of
/// an approval signs the pull request and the head, which a purge removes, so the stub of an
/// approval cannot check it; the proof of any other entry signs the hash of the entry, which the
/// stub keeps. The kind is not covered by the hash of the entry,
/// so it is a convenience for an honest log and no boundary against one that is rewritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StubKind {
    /// An approval of a pull request.
    Approval,
    /// Any other entry.
    Other,
}

impl StubKind {
    fn name(self) -> &'static str {
        match self {
            Self::Approval => "approval",
            Self::Other => "other",
        }
    }
}

impl Stub {
    /// The time of the entry that the stub replaced.
    pub fn time(&self) -> i64 {
        self.time
    }

    /// The actor of the entry that the stub replaced.
    pub fn actor(&self) -> &Actor {
        &self.actor
    }

    /// The hash of the body of the entry that the stub replaced, or `None` for a stub of an entry
    /// written before the chain.
    pub fn body(&self) -> Option<BodyHash> {
        self.body
    }

    /// What the stub replaced, or `None` for a stub written before the kind was kept.
    pub fn replaced(&self) -> Option<StubKind> {
        self.replaced
    }
}

/// The entry of a purge (ADR-014): who purged, when, how many commits were rewritten, and the
/// numbers of the entries that became stubs. It names no pointer and no ID, as the audit must not
/// keep what the purge removes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Purge {
    time: i64,
    actor: Actor,
    commits: u64,
    tags: u64,
    stubs: Vec<(u64, u64)>,
}

impl Purge {
    /// Builds the record of a purge. `stubs` are the numbers of the entries that become stubs, in
    /// any order; they are kept as ranges.
    ///
    /// # Errors
    ///
    /// Returns [`EntryError::StubOutOfRange`] for a stub that is 0 or larger than [`MAX_NUMBER`],
    /// and [`EntryError::TooManyCommits`] for a count larger than [`MAX_NUMBER`], which the reader
    /// would reject.
    pub(crate) fn try_new(
        time: i64,
        actor: Actor,
        commits: u64,
        stubs: &[u64],
    ) -> Result<Self, EntryError> {
        if commits > MAX_NUMBER {
            return Err(EntryError::TooManyCommits(commits));
        }
        let mut numbers = stubs.to_vec();
        numbers.sort_unstable();
        numbers.dedup();
        if let Some(number) = numbers
            .iter()
            .find(|number| !(1..=MAX_NUMBER).contains(*number))
        {
            return Err(EntryError::StubOutOfRange(*number));
        }
        let mut ranges: Vec<(u64, u64)> = Vec::new();
        for number in numbers {
            match ranges.last_mut() {
                Some((_, end)) if *end + 1 == number => *end = number,
                _ => ranges.push((number, number)),
            }
        }
        Ok(Self {
            time,
            actor,
            commits,
            tags: 0,
            stubs: ranges,
        })
    }

    /// Records that the purge moved `tags` tags to the new versions of their commits (ADR-014).
    /// Only a count: the entry names no pointer and no ID.
    ///
    /// # Errors
    ///
    /// Returns [`EntryError::TooManyTags`] if the count is larger than [`MAX_NUMBER`], which
    /// the reader would reject.
    pub(crate) fn with_tags(mut self, tags: u64) -> Result<Self, EntryError> {
        if tags > MAX_NUMBER {
            return Err(EntryError::TooManyTags(tags));
        }
        self.tags = tags;
        Ok(self)
    }

    /// The time that the writer claims.
    pub fn time(&self) -> i64 {
        self.time
    }

    /// Who purged.
    pub fn actor(&self) -> &Actor {
        &self.actor
    }

    /// How many commits the purge rewrote. A purge whose list of stubs did not fit into one entry
    /// writes several; the first has the count and the others 0.
    pub fn commits(&self) -> u64 {
        self.commits
    }

    /// How many tags the purge moved to the new versions of their commits (0 if it moved none;
    /// the entry has no line `tags` then). Like the count of commits, it is in the first entry
    /// of a purge that needs several.
    pub fn tags(&self) -> u64 {
        self.tags
    }

    /// The numbers of the entries that became stubs, as inclusive ranges in ascending order. Two
    /// ranges neither overlap nor touch.
    pub fn stubs(&self) -> &[(u64, u64)] {
        &self.stubs
    }

    /// Whether the purge lists the entry as a stub.
    pub fn lists(&self, number: u64) -> bool {
        self.stubs
            .binary_search_by(|(start, end)| {
                if *end < number {
                    std::cmp::Ordering::Less
                } else if *start > number {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .is_ok()
    }

    /// The largest number that it lists, or 0 if it lists none.
    fn last_stub(&self) -> u64 {
        self.stubs.last().map_or(0, |(_, end)| *end)
    }

    /// Splits the record into records whose entries fit [`MAX_ENTRY_LEN`], with the count of
    /// commits in the first. The size is computed for the largest number an entry can have.
    fn split(self) -> Vec<Self> {
        let mut parts = Vec::new();
        let mut current = Self {
            stubs: Vec::new(),
            ..self.clone()
        };
        for range in self.stubs {
            current.stubs.push(range);
            // The size of an entry in the chain, whose line `previous` is the longest.
            let size = Entry {
                number: MAX_NUMBER,
                chain: Chain::After(EntryHash::of("")),
                record: Record::Purge(current.clone()),
            }
            .encode()
            .len() as u64;
            if size > MAX_ENTRY_LEN && current.stubs.len() > 1 {
                let range = current.stubs.pop().expect("one was pushed above");
                let next = Self {
                    commits: 0,
                    tags: 0,
                    stubs: vec![range],
                    ..current.clone()
                };
                parts.push(std::mem::replace(&mut current, next));
            }
        }
        parts.push(current);
        parts
    }
}

/// What a lease entry records (ADR-004, STEMMA-225).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LeaseKind {
    /// The first writing operation on a branch without a valid lease took it.
    Acquire,
    /// The holder gave the lease back.
    Release,
    /// An actor took a lease that had expired.
    Takeover,
    /// An owner ended the lease of another actor.
    Override,
}

impl LeaseKind {
    /// The name of the operation of this kind of entry.
    pub fn operation(self) -> &'static str {
        match self {
            Self::Acquire => LEASE_ACQUIRE,
            Self::Release => LEASE_RELEASE,
            Self::Takeover => LEASE_TAKEOVER,
            Self::Override => LEASE_OVERRIDE,
        }
    }

    /// The kind that an operation name stands for, or `None` for any other name.
    pub fn from_operation(name: &str) -> Option<Self> {
        match name {
            LEASE_ACQUIRE => Some(Self::Acquire),
            LEASE_RELEASE => Some(Self::Release),
            LEASE_TAKEOVER => Some(Self::Takeover),
            LEASE_OVERRIDE => Some(Self::Override),
            _ => None,
        }
    }
}

/// The entry of a lease: it names a branch and the actors, and no commit, so that a purge never
/// touches it (ADR-004, ADR-011). The actor of the entry is the one who acts: the new holder for
/// an acquisition, a takeover, and an override, and the holder for a release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseEntry {
    time: i64,
    actor: Actor,
    kind: LeaseKind,
    branch: BranchName,
    from: Option<Actor>,
    expires: Option<i64>,
    reason: Option<String>,
}

impl LeaseEntry {
    /// The entry of an acquisition: `actor` holds the lease of `branch` until `expires`, in
    /// seconds since the Unix epoch.
    ///
    /// # Errors
    ///
    /// Returns [`EntryError::LeaseExpiry`] if `expires` is not after `time`.
    pub fn acquire(
        time: i64,
        actor: Actor,
        branch: BranchName,
        expires: i64,
    ) -> Result<Self, EntryError> {
        Self::build(
            time,
            actor,
            LeaseKind::Acquire,
            branch,
            None,
            Some(expires),
            None,
        )
    }

    /// The entry of a release: `actor` gives the lease of `branch` back.
    pub fn release(time: i64, actor: Actor, branch: BranchName) -> Self {
        Self {
            time,
            actor,
            kind: LeaseKind::Release,
            branch,
            from: None,
            expires: None,
            reason: None,
        }
    }

    /// The entry of a takeover: `actor` takes the lease of `branch` that `from` held and that has
    /// expired, and holds it until `expires`. The reason is [`EXPIRED`].
    ///
    /// # Errors
    ///
    /// Returns [`EntryError::LeaseSameActor`] if `from` is `actor` (the earlier holder acquires
    /// again), and [`EntryError::LeaseExpiry`] if `expires` is not after `time`.
    pub fn takeover(
        time: i64,
        actor: Actor,
        branch: BranchName,
        from: Actor,
        expires: i64,
    ) -> Result<Self, EntryError> {
        Self::build(
            time,
            actor,
            LeaseKind::Takeover,
            branch,
            Some(from),
            Some(expires),
            Some(EXPIRED.to_owned()),
        )
    }

    /// The entry of an owner override: `owner` ends the lease of `from` on `branch` and holds it
    /// until `expires`, for a `reason` of at most [`MAX_REASON_LEN`] bytes on one line.
    ///
    /// # Errors
    ///
    /// Returns [`EntryError::LeaseSameActor`] if `from` is `owner`, [`EntryError::LeaseExpiry`] if
    /// `expires` is not after `time`, and [`EntryError::BadReason`] for a reason that is empty,
    /// too long, has a control character, or starts or ends with a space.
    pub fn owner_override(
        time: i64,
        owner: Actor,
        branch: BranchName,
        from: Actor,
        expires: i64,
        reason: impl Into<String>,
    ) -> Result<Self, EntryError> {
        Self::build(
            time,
            owner,
            LeaseKind::Override,
            branch,
            Some(from),
            Some(expires),
            Some(reason.into()),
        )
    }

    /// The one place of the rules of a lease entry, for the constructors and the reader.
    fn build(
        time: i64,
        actor: Actor,
        kind: LeaseKind,
        branch: BranchName,
        from: Option<Actor>,
        expires: Option<i64>,
        reason: Option<String>,
    ) -> Result<Self, EntryError> {
        if from.as_ref() == Some(&actor) {
            return Err(EntryError::LeaseSameActor);
        }
        if expires.is_some_and(|expires| expires <= time) {
            return Err(EntryError::LeaseExpiry);
        }
        if let Some(reason) = reason.as_deref() {
            let plain = !reason.is_empty()
                && reason.len() <= MAX_REASON_LEN
                && !reason.chars().any(char::is_control)
                && reason.trim() == reason;
            if !plain {
                return Err(EntryError::BadReason);
            }
        }
        Ok(Self {
            time,
            actor,
            kind,
            branch,
            from,
            expires,
            reason,
        })
    }

    /// The time that the writer claims, in seconds since the Unix epoch (UTC).
    pub fn time(&self) -> i64 {
        self.time
    }

    /// The actor who acts: the new holder, or the holder who releases.
    pub fn actor(&self) -> &Actor {
        &self.actor
    }

    /// What the entry records.
    pub fn kind(&self) -> LeaseKind {
        self.kind
    }

    /// The branch whose lease the entry is about.
    pub fn branch(&self) -> &BranchName {
        &self.branch
    }

    /// The earlier holder, for a takeover and an override.
    pub fn from(&self) -> Option<&Actor> {
        self.from.as_ref()
    }

    /// The end of the lease that the entry grants, in seconds since the Unix epoch, for every kind
    /// but a release.
    pub fn expires(&self) -> Option<i64> {
        self.expires
    }

    /// The reason: [`EXPIRED`] for a takeover, the text of the owner for an override.
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    /// The lines of the body, which the hash of the body covers.
    fn body_text(&self) -> String {
        let mut out = format!(
            "operation {}\nbranch {}\n",
            self.kind.operation(),
            self.branch
        );
        if let Some(from) = &self.from {
            out.push_str(&format!("from {from}\n"));
        }
        if let Some(expires) = self.expires {
            out.push_str(&format!("expires {expires}\n"));
        }
        if let Some(reason) = &self.reason {
            out.push_str(&format!("reason {reason}\n"));
        }
        out
    }
}

/// What a pull request entry records (ADR-004, STEMMA-238).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PullKind {
    /// The write owner of a branch opened a pull request.
    Open,
    /// The pull request ended without a merge.
    Close,
    /// The pull request was merged.
    Merge,
    /// An actor approved the head of the pull request.
    Approve,
}

impl PullKind {
    /// The name of the operation of this kind of entry.
    pub fn operation(self) -> &'static str {
        match self {
            Self::Open => PR_OPEN,
            Self::Close => PR_CLOSE,
            Self::Merge => PR_MERGE,
            Self::Approve => PR_APPROVE,
        }
    }

    /// The kind that an operation name stands for, or `None` for any other name.
    pub fn from_operation(name: &str) -> Option<Self> {
        match name {
            PR_OPEN => Some(Self::Open),
            PR_CLOSE => Some(Self::Close),
            PR_MERGE => Some(Self::Merge),
            PR_APPROVE => Some(Self::Approve),
            _ => None,
        }
    }
}

/// The entry of a pull request. A pull request has no object of its own: it is the sequence of
/// these entries, named by the number of its `pr-open` entry (ADR-004, ADR-011).
///
/// An opening names the branch, the commit at its tip, the reviewers, and, if the opener declares
/// a repair, the red commit (`repairs <commit>`, ADR-019). A closing names the pull request and no
/// commit. A merge names the pull request and the commit that it merged. The entries that name a
/// commit can become stubs in a purge; the purge refuses to replace the opening of a pull request
/// that is still open. An approval names the pull request and the head that the approver reviewed
/// (`pull <number>`, `commit <id>`); it counts for that head only (ADR-004 rule 4).
///
/// Since the repository format 16 an opening, a closing, and an approval name the signed
/// statement that they made, as their last line `statement <hash>` (ADR-025, ADR-011,
/// [`with_statement`](Self::with_statement)). The statement is the evidence, and the entry
/// records the act: its number is the name of the pull request in this log, and the hash of the
/// opening is its name everywhere. An entry without the line is one of the formats 9 to 15; it
/// is read as an entry of the chain and is no evidence. A merge names no statement, as "merged"
/// is none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullEntry {
    time: i64,
    actor: Actor,
    kind: PullKind,
    branch: Option<BranchName>,
    pull: Option<u64>,
    commit: Option<CommitId>,
    reviewers: Vec<Actor>,
    repairs: Option<CommitId>,
    statement: Option<StatementHash>,
}

impl PullEntry {
    /// The entry of an opening by `actor`: `branch` has `commit` at its tip, and the actors in
    /// `reviewers` review it. The reviewers are sorted; a repeat, the opener among them, or more
    /// than [`MAX_REVIEWERS`] is refused. `repairs` is the red commit that the pull request
    /// declares to repair.
    ///
    /// # Errors
    ///
    /// Returns [`EntryError::BadReviewers`] for the reviewers above.
    pub fn open(
        time: i64,
        actor: Actor,
        branch: BranchName,
        commit: CommitId,
        mut reviewers: Vec<Actor>,
        repairs: Option<CommitId>,
    ) -> Result<Self, EntryError> {
        reviewers.sort_by_cached_key(ToString::to_string);
        Self::build(
            time,
            actor,
            PullKind::Open,
            Some(branch),
            None,
            Some(commit),
            reviewers,
            repairs,
        )
    }

    /// The entry of a closing: `actor` ends the pull request `pull` without a merge.
    ///
    /// # Errors
    ///
    /// Returns [`EntryError::NumberOutOfRange`] if `pull` is 0 or larger than [`MAX_NUMBER`].
    pub fn close(time: i64, actor: Actor, pull: u64) -> Result<Self, EntryError> {
        Self::build(
            time,
            actor,
            PullKind::Close,
            None,
            Some(pull),
            None,
            Vec::new(),
            None,
        )
    }

    /// The entry of a merge: the pull request `pull` was merged with the head `commit`.
    ///
    /// # Errors
    ///
    /// Returns [`EntryError::NumberOutOfRange`] if `pull` is 0 or larger than [`MAX_NUMBER`].
    pub fn merge(time: i64, actor: Actor, pull: u64, commit: CommitId) -> Result<Self, EntryError> {
        Self::build(
            time,
            actor,
            PullKind::Merge,
            None,
            Some(pull),
            Some(commit),
            Vec::new(),
            None,
        )
    }

    /// The entry of an approval: `actor` approves the head `commit` of the pull request `pull`.
    /// Whether the actor may, and whether the approval counts, is decided by the writer and by the
    /// recount, not by the entry (ADR-004 rule 4).
    ///
    /// # Errors
    ///
    /// Returns [`EntryError::NumberOutOfRange`] if `pull` is 0 or larger than [`MAX_NUMBER`].
    pub fn approve(
        time: i64,
        actor: Actor,
        pull: u64,
        commit: CommitId,
    ) -> Result<Self, EntryError> {
        Self::build(
            time,
            actor,
            PullKind::Approve,
            None,
            Some(pull),
            Some(commit),
            Vec::new(),
            None,
        )
    }

    /// The one place of the rules of a pull request entry, for the constructors and the reader.
    #[allow(clippy::too_many_arguments)]
    fn build(
        time: i64,
        actor: Actor,
        kind: PullKind,
        branch: Option<BranchName>,
        pull: Option<u64>,
        commit: Option<CommitId>,
        reviewers: Vec<Actor>,
        repairs: Option<CommitId>,
    ) -> Result<Self, EntryError> {
        if let Some(number) = pull {
            if number == 0 || number > MAX_NUMBER {
                return Err(EntryError::NumberOutOfRange(number));
            }
        }
        // Which fields a kind has: the one place that couples the kind to its options.
        let shaped = match kind {
            PullKind::Open => branch.is_some() && pull.is_none() && commit.is_some(),
            PullKind::Close => branch.is_none() && pull.is_some() && commit.is_none(),
            PullKind::Merge | PullKind::Approve => {
                branch.is_none() && pull.is_some() && commit.is_some()
            }
        };
        if !shaped || (kind != PullKind::Open && (!reviewers.is_empty() || repairs.is_some())) {
            return Err(EntryError::BadPullShape);
        }
        // The register knows an identity, not an actor: an agent with another model or session
        // is the same reviewer, and the opener is none of them.
        let ascending = reviewers
            .windows(2)
            .all(|pair| pair[0].to_string() < pair[1].to_string());
        let identities: std::collections::HashSet<Identity> =
            reviewers.iter().map(Identity::of).collect();
        let is_opener = identities.contains(&Identity::of(&actor));
        if reviewers.len() > MAX_REVIEWERS
            || !ascending
            || identities.len() != reviewers.len()
            || is_opener
        {
            return Err(EntryError::BadReviewers);
        }
        Ok(Self {
            time,
            actor,
            kind,
            branch,
            pull,
            commit,
            reviewers,
            repairs,
            statement: None,
        })
    }

    /// The entry with the hash of the signed statement that it made (ADR-025): the opening, the
    /// closing, or the approval.
    ///
    /// # Errors
    ///
    /// Returns [`EntryError::BadPullShape`] for a merge, which names no statement.
    pub fn with_statement(mut self, statement: StatementHash) -> Result<Self, EntryError> {
        if self.kind == PullKind::Merge {
            return Err(EntryError::BadPullShape);
        }
        self.statement = Some(statement);
        Ok(self)
    }

    /// The hash of the signed statement that the entry made, or `None` for a merge and for an
    /// entry from before the repository format 16.
    pub fn statement(&self) -> Option<StatementHash> {
        self.statement
    }

    /// The time that the writer claims, in seconds since the Unix epoch (UTC).
    pub fn time(&self) -> i64 {
        self.time
    }

    /// The actor who acts: the opener, or whoever closes or merges.
    pub fn actor(&self) -> &Actor {
        &self.actor
    }

    /// What the entry records.
    pub fn kind(&self) -> PullKind {
        self.kind
    }

    /// The branch of an opening, or `None` for a closing and a merge.
    pub fn branch(&self) -> Option<&BranchName> {
        self.branch.as_ref()
    }

    /// The number of the pull request that a closing or a merge is about, or `None` for an
    /// opening, which the number of its own entry names.
    pub fn pull(&self) -> Option<u64> {
        self.pull
    }

    /// The commit at the tip of the branch (an opening) or the head that was merged (a merge).
    pub fn commit(&self) -> Option<&CommitId> {
        self.commit.as_ref()
    }

    /// The reviewers of an opening, in ascending order.
    pub fn reviewers(&self) -> &[Actor] {
        &self.reviewers
    }

    /// The red commit that an opening declares to repair.
    pub fn repairs(&self) -> Option<&CommitId> {
        self.repairs.as_ref()
    }

    /// Every commit that the entry names, which a purge may rewrite.
    pub fn commits(&self) -> Vec<CommitId> {
        self.commit
            .iter()
            .chain(self.repairs.iter())
            .copied()
            .collect()
    }

    /// The lines of the body, which the hash of the body covers.
    fn body_text(&self) -> String {
        let mut out = format!("operation {}\n", self.kind.operation());
        if let Some(branch) = &self.branch {
            out.push_str(&format!("branch {branch}\n"));
        }
        if let Some(pull) = self.pull {
            out.push_str(&format!("pull {pull}\n"));
        }
        if let Some(commit) = &self.commit {
            out.push_str(&format!("commit {commit}\n"));
        }
        for reviewer in &self.reviewers {
            out.push_str(&format!("reviewer {reviewer}\n"));
        }
        if let Some(repairs) = &self.repairs {
            out.push_str(&format!("repairs {repairs}\n"));
        }
        if let Some(statement) = &self.statement {
            out.push_str(&format!("statement {statement}\n"));
        }
        out
    }
}

/// The result of a check run (ADR-019).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CheckResult {
    /// The tree of the tested commit passed the set of tests.
    Pass,
    /// It failed.
    Fail,
}

impl CheckResult {
    /// The word that the entry has for the result.
    pub fn name(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        match name {
            "pass" => Some(Self::Pass),
            "fail" => Some(Self::Fail),
            _ => None,
        }
    }
}

/// The entry of a check: the result of a run, as evidence about a pair of commits (ADR-019,
/// STEMMA-240).
///
/// It names the commit that was tested and the tip of `main` it was merged onto for the test, its
/// *base*; a run on the tip of `main` itself, after a merge, has no base. One entry holds one
/// base, so a run against another tip is another entry. The entry says nothing about who may
/// count it: that follows from the writer, the register, and the risk level when the evidence is
/// read ([`crate::check`]).
///
/// Since the repository format 16 the entry names the signed statement that it made, as its
/// last line `statement <hash>`, and has the line `version 2` (ADR-025, ADR-011,
/// [`with_statement`](Self::with_statement)). The statement is the evidence, and in an enforcing
/// repository its signature is the proof of the writer. An entry of version 1 is one of the
/// formats 11 to 15, whose proof was a file that signed the hash of the entry; it is read as an
/// entry of the chain and is no evidence.
///
/// It names commits, so a purge replaces it with a stub when it rewrites one of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckEntry {
    time: i64,
    actor: Actor,
    commit: CommitId,
    base: Option<CommitId>,
    result: CheckResult,
    statement: Option<StatementHash>,
}

impl CheckEntry {
    /// The entry of a run by `actor`: `commit` was tested, merged onto `base` (or run as it is,
    /// for `None`, the run on the tip of `main`), with `result`.
    pub fn new(
        time: i64,
        actor: Actor,
        commit: CommitId,
        base: Option<CommitId>,
        result: CheckResult,
    ) -> Self {
        Self {
            time,
            actor,
            commit,
            base,
            result,
            statement: None,
        }
    }

    /// The entry with the hash of the signed statement that it made (ADR-025), which makes it
    /// an entry of version 2.
    #[must_use]
    pub fn with_statement(mut self, statement: StatementHash) -> Self {
        self.statement = Some(statement);
        self
    }

    /// The hash of the signed statement that the entry made, or `None` for an entry of
    /// version 1.
    pub fn statement(&self) -> Option<StatementHash> {
        self.statement
    }

    /// The time that the writer claims, in seconds since the Unix epoch (UTC).
    pub fn time(&self) -> i64 {
        self.time
    }

    /// The actor who ran the check and wrote the entry.
    pub fn actor(&self) -> &Actor {
        &self.actor
    }

    /// The commit that was tested.
    pub fn commit(&self) -> &CommitId {
        &self.commit
    }

    /// The tip of `main` that the commit was merged onto for the test, or `None` for a run on the
    /// tip itself.
    pub fn base(&self) -> Option<&CommitId> {
        self.base.as_ref()
    }

    /// The result of the run.
    pub fn result(&self) -> CheckResult {
        self.result
    }

    /// Every commit that the entry names, which a purge may rewrite.
    pub fn commits(&self) -> Vec<CommitId> {
        std::iter::once(self.commit).chain(self.base).collect()
    }

    /// The lines of the body, which the hash of the body covers.
    fn body_text(&self) -> String {
        let base = self
            .base
            .as_ref()
            .map_or_else(|| "-".to_owned(), ToString::to_string);
        let (version, statement) = match &self.statement {
            None => (CHECK_VERSION, String::new()),
            Some(statement) => (CHECK_STATEMENT_VERSION, format!("statement {statement}\n")),
        };
        format!(
            "operation {CHECK}\nversion {version}\ncommit {}\nbase {base}\nresult {}\n{statement}",
            self.commit,
            self.result.name()
        )
    }
}

// The entry that pins the key of a hub again (ADR-026, STEMMA-297).

/// The entry of a pinning: a human replaced the key of the hub that the configuration of a
/// remote pinned (ADR-026). It names the remote, the key that was pinned before (none if the
/// configuration had none), and the new key, and no commit, so that a purge never touches it.
///
/// ```text
/// operation remote-pin
/// remote origin
/// old ssh-ed25519 <base64>        or   old -
/// new ssh-ed25519 <base64>
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePinEntry {
    time: i64,
    actor: Actor,
    remote: RemoteName,
    old: Option<PublicKey>,
    new: PublicKey,
}

impl RemotePinEntry {
    /// The entry of `actor`, who pins `new` for the hub of `remote` in place of `old`.
    ///
    /// # Errors
    ///
    /// Returns [`EntryError::RemotePinByAgent`] if `actor` is an agent, as only a human pins the
    /// key of a hub (ADR-026), and [`EntryError::RemotePinUnchanged`] if `old` is `new`.
    pub fn try_new(
        time: i64,
        actor: Actor,
        remote: RemoteName,
        old: Option<PublicKey>,
        new: PublicKey,
    ) -> Result<Self, EntryError> {
        if matches!(actor.view(), ActorRef::Agent { .. }) {
            return Err(EntryError::RemotePinByAgent);
        }
        if old.as_ref() == Some(&new) {
            return Err(EntryError::RemotePinUnchanged);
        }
        Ok(Self {
            time,
            actor,
            remote,
            old,
            new,
        })
    }

    /// The time that the writer claims, in seconds since the Unix epoch (UTC).
    pub fn time(&self) -> i64 {
        self.time
    }

    /// The human who pinned the key.
    pub fn actor(&self) -> &Actor {
        &self.actor
    }

    /// The remote whose configuration holds the key.
    pub fn remote(&self) -> &RemoteName {
        &self.remote
    }

    /// The key that was pinned before, or `None` if the configuration held none.
    pub fn old(&self) -> Option<&PublicKey> {
        self.old.as_ref()
    }

    /// The key that is pinned now.
    pub fn new_key(&self) -> &PublicKey {
        &self.new
    }

    /// The lines of the body, which the hash of the body covers.
    fn body_text(&self) -> String {
        let old = self
            .old
            .as_ref()
            .map_or_else(|| "-".to_owned(), PublicKey::to_openssh);
        format!(
            "operation {REMOTE_PIN}\nremote {}\nold {old}\nnew {}\n",
            self.remote, self.new
        )
    }
}

/// What an entry holds. The enum is exhaustive on purpose: a new kind of entry has to be handled
/// by every reader, which the compiler then shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    /// An operation that moved pointers.
    Change(Change),
    /// An entry that a purge replaced.
    Stub(Stub),
    /// A purge.
    Purge(Purge),
    /// A lease of a branch: acquired, released, taken over, or overridden (ADR-004).
    Lease(LeaseEntry),
    /// A pull request: opened, closed, or merged (ADR-004).
    Pull(PullEntry),
    /// The result of a check run (ADR-019).
    Check(CheckEntry),
    /// A key of a hub pinned again (ADR-026).
    RemotePin(RemotePinEntry),
}

/// An entry of the log: a number, its place in the hash chain, and a record, which is a change,
/// a stub, or a purge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    number: u64,
    chain: Chain,
    record: Record,
}

impl Entry {
    /// Builds an entry of a change. Only the log gives a number, so only this crate builds one.
    ///
    /// # Errors
    ///
    /// Returns [`EntryError::NumberOutOfRange`] if the number is 0 or larger than [`MAX_NUMBER`].
    #[cfg(test)]
    pub(crate) fn try_new(number: u64, change: Change) -> Result<Self, EntryError> {
        Self::of(number, Record::Change(change))
    }

    /// Builds an entry of any record, written before the chain (version 1 or 2).
    ///
    /// # Errors
    ///
    /// See [`linked`](Self::linked).
    #[cfg(test)]
    pub(crate) fn of(number: u64, record: Record) -> Result<Self, EntryError> {
        Self::linked(number, Chain::Unchained, record)
    }

    /// Builds an entry of any record with its place in the chain.
    ///
    /// # Errors
    ///
    /// Returns [`EntryError::NumberOutOfRange`] if the number is 0 or larger than [`MAX_NUMBER`],
    /// [`EntryError::FirstAfter`] for entry 1 that names an entry before it,
    /// [`EntryError::StubNotBefore`] if a purge lists its own number or a later one,
    /// [`EntryError::StubBodyOutsideChain`] for a stub outside the chain that has the hash of a
    /// body or a kind, and [`EntryError::StubWithoutBody`] for a stub in the chain that has none. A
    /// [`Purge`] checks its own bounds when it is built, so every entry that is built here reads
    /// back equal.
    pub(crate) fn linked(number: u64, chain: Chain, record: Record) -> Result<Self, EntryError> {
        if !(1..=MAX_NUMBER).contains(&number) {
            return Err(EntryError::NumberOutOfRange(number));
        }
        if number == 1 && matches!(chain, Chain::After(_)) {
            return Err(EntryError::FirstAfter);
        }
        match (&record, chain) {
            (Record::Purge(purge), _) if purge.last_stub() >= number => {
                return Err(EntryError::StubNotBefore {
                    stub: purge.last_stub(),
                    purge: number,
                });
            }
            (Record::Stub(stub), Chain::Unchained)
                if stub.replaced.is_some() || stub.body.is_some() =>
            {
                return Err(EntryError::StubBodyOutsideChain(number));
            }
            (Record::Stub(stub), Chain::Start | Chain::After(_)) if stub.body.is_none() => {
                return Err(EntryError::StubWithoutBody(number));
            }
            (Record::Pull(_), Chain::Unchained) => {
                return Err(EntryError::PullOutsideChain(number));
            }
            (Record::Check(_), Chain::Unchained) => {
                return Err(EntryError::CheckOutsideChain(number));
            }
            (Record::Check(check), _) if check.base() == Some(check.commit()) => {
                return Err(EntryError::CheckBaseIsCommit(number));
            }
            (Record::Lease(_), Chain::Unchained) => {
                return Err(EntryError::LeaseOutsideChain(number));
            }
            (Record::RemotePin(_), Chain::Unchained) => {
                return Err(EntryError::RemotePinOutsideChain(number));
            }
            _ => {}
        }
        Ok(Self {
            number,
            chain,
            record,
        })
    }

    /// The number of the entry, which is also the name of its file.
    pub fn number(&self) -> u64 {
        self.number
    }

    /// What the entry holds.
    pub fn record(&self) -> &Record {
        &self.record
    }

    /// The place of the entry in the hash chain.
    pub fn chain(&self) -> Chain {
        self.chain
    }

    /// The hash of the entry, which the next entry names, or `None` for an entry written before
    /// the chain. It covers the number, the time, the actor, the hash of the entry before, and
    /// the hash of the body: the operation and the pointers of a change, and the count and the
    /// stubs of a purge. A stub keeps the hash of the body it replaced, so its hash is the hash of
    /// the entry it replaced (ADR-014).
    pub fn hash(&self) -> Option<EntryHash> {
        let previous = match self.chain {
            Chain::Unchained => return None,
            Chain::Start => "-".to_owned(),
            Chain::After(hash) => hash.to_string(),
        };
        let body = self
            .body_hash()
            .expect("an entry in the chain has the hash of a body, which `linked` checks");
        Some(EntryHash::of(&format!(
            "number {}\ntime {}\nactor {}\nprevious {previous}\nbody {body}\n",
            self.number,
            self.time(),
            self.actor()
        )))
    }

    /// The hash of the body of the entry, which its hash covers: of the operation and the pointers
    /// of a change, or of the count and the stubs of a purge. A stub has the one of the entry it
    /// replaced. `None` for an entry written before the chain.
    pub fn body_hash(&self) -> Option<BodyHash> {
        if self.chain == Chain::Unchained {
            return None;
        }
        match &self.record {
            Record::Stub(stub) => stub.body,
            _ => self.body_text().map(|text| BodyHash::of(&text)),
        }
    }

    /// The lines after the header lines of a change or a purge, which the hash of the body covers:
    /// the operation and the pointers, or the operation, the count, and the stubs. `None` for a
    /// stub, which has no body left.
    fn body_text(&self) -> Option<String> {
        match &self.record {
            Record::Stub(_) => None,
            Record::Purge(purge) => {
                let stubs = if purge.stubs.is_empty() {
                    "-".to_owned()
                } else {
                    ranges_text(&purge.stubs)
                };
                // The line is left out for no tags, so that an entry of a purge that moved none
                // keeps its encoding and its hash.
                let tags = if purge.tags == 0 {
                    String::new()
                } else {
                    format!("tags {}\n", purge.tags)
                };
                Some(format!(
                    "operation {PURGE}\ncommits {}\n{tags}stubs {stubs}\n",
                    purge.commits
                ))
            }
            Record::Lease(lease) => Some(lease.body_text()),
            Record::Pull(pull) => Some(pull.body_text()),
            Record::Check(check) => Some(check.body_text()),
            Record::RemotePin(pin) => Some(pin.body_text()),
            Record::Change(change) => {
                let mut out = format!("operation {}\n", change.operation);
                if let Some(tag) = change.worktree.as_deref() {
                    out.push_str(&format!("worktree {}\n", tag.name));
                    if let Some(location) = &tag.location {
                        out.push_str(&format!("location {location}\n"));
                    }
                    if let Some(owner) = &tag.former_owner {
                        out.push_str(&format!("former-owner {owner}\n"));
                    }
                }
                if let Some(head) = &change.head {
                    out.push_str(&format!(
                        "head {} {}\n",
                        head_value(head.old.as_ref()),
                        head_value(head.new.as_ref())
                    ));
                }
                if let Some(moved) = &change.working_copy {
                    out.push_str(&format!(
                        "working-copy {} {}\n",
                        commit_value(moved.old.as_ref()),
                        commit_value(moved.new.as_ref())
                    ));
                }
                for (name, moved) in &change.branches {
                    out.push_str(&format!(
                        "branch {name} {} {}\n",
                        commit_value(moved.old.as_ref()),
                        commit_value(moved.new.as_ref())
                    ));
                }
                for (name, moved) in &change.tags {
                    out.push_str(&format!(
                        "tag {name} {} {}\n",
                        commit_value(moved.old.as_ref()),
                        commit_value(moved.new.as_ref())
                    ));
                }
                for (name, moved) in &change.remotes {
                    out.push_str(&format!(
                        "remote {name} {} {}\n",
                        remote_value(moved.old.as_ref()),
                        remote_value(moved.new.as_ref())
                    ));
                }
                Some(out)
            }
        }
    }

    /// What the operation did, or `None` for a stub and a purge.
    pub fn change(&self) -> Option<&Change> {
        match &self.record {
            Record::Change(change) => Some(change),
            Record::Stub(_)
            | Record::Purge(_)
            | Record::Lease(_)
            | Record::Pull(_)
            | Record::Check(_)
            | Record::RemotePin(_) => None,
        }
    }

    /// The lease that the entry records, or `None` for any other entry.
    pub fn lease(&self) -> Option<&LeaseEntry> {
        match &self.record {
            Record::Lease(lease) => Some(lease),
            Record::Change(_)
            | Record::Stub(_)
            | Record::Purge(_)
            | Record::Pull(_)
            | Record::Check(_)
            | Record::RemotePin(_) => None,
        }
    }

    /// The pull request entry that the entry records, or `None` for any other entry.
    pub fn pull(&self) -> Option<&PullEntry> {
        match &self.record {
            Record::Pull(pull) => Some(pull),
            Record::Change(_)
            | Record::Stub(_)
            | Record::Purge(_)
            | Record::Lease(_)
            | Record::Check(_)
            | Record::RemotePin(_) => None,
        }
    }

    /// The check that the entry records, or `None` for any other entry.
    pub fn check(&self) -> Option<&CheckEntry> {
        match &self.record {
            Record::Check(check) => Some(check),
            Record::Change(_)
            | Record::Stub(_)
            | Record::Purge(_)
            | Record::Lease(_)
            | Record::Pull(_)
            | Record::RemotePin(_) => None,
        }
    }

    /// The pinning of the key of a hub that the entry records, or `None` for any other entry.
    pub fn remote_pin(&self) -> Option<&RemotePinEntry> {
        match &self.record {
            Record::RemotePin(pin) => Some(pin),
            Record::Change(_)
            | Record::Stub(_)
            | Record::Purge(_)
            | Record::Lease(_)
            | Record::Pull(_)
            | Record::Check(_) => None,
        }
    }

    /// The hash of the signed statement that the entry made (ADR-025): an opening, a closing, an
    /// approval, or the result of a check, since the repository format 16. `None` for every
    /// other entry, for a stub, and for an entry of an older format.
    pub fn statement(&self) -> Option<StatementHash> {
        match &self.record {
            Record::Pull(pull) => pull.statement,
            Record::Check(check) => check.statement,
            Record::Change(_)
            | Record::Stub(_)
            | Record::Purge(_)
            | Record::Lease(_)
            | Record::RemotePin(_) => None,
        }
    }

    /// The time that the writer of the entry claims. For a stub it is the time of the entry that
    /// it replaced, not of the purge.
    pub fn time(&self) -> i64 {
        match &self.record {
            Record::Change(change) => change.time,
            Record::Stub(stub) => stub.time,
            Record::Purge(purge) => purge.time,
            Record::Lease(lease) => lease.time,
            Record::Pull(pull) => pull.time,
            Record::Check(check) => check.time,
            Record::RemotePin(pin) => pin.time,
        }
    }

    /// Who wrote the entry. For a stub it is the actor of the entry that it replaced, not the one
    /// who purged.
    pub fn actor(&self) -> &Actor {
        match &self.record {
            Record::Change(change) => &change.actor,
            Record::Stub(stub) => &stub.actor,
            Record::Purge(purge) => &purge.actor,
            Record::Lease(lease) => &lease.actor,
            Record::Pull(pull) => &pull.actor,
            Record::Check(check) => &check.actor,
            Record::RemotePin(pin) => &pin.actor,
        }
    }

    /// Every commit that the entry names (see [`Change::commits`]); none for a stub and a purge.
    pub fn commits(&self) -> Vec<CommitId> {
        match &self.record {
            Record::Change(change) => change.commits(),
            Record::Pull(pull) => pull.commits(),
            Record::Check(check) => check.commits(),
            Record::Stub(_) | Record::Purge(_) | Record::Lease(_) | Record::RemotePin(_) => {
                Vec::new()
            }
        }
    }

    /// The stub that replaces this entry: the same number, time, actor, and place in the chain.
    /// The stub of an entry in the chain keeps the hash of its body, so it has the hash of the
    /// entry (ADR-014). A stub is its own stub.
    ///
    /// # Errors
    ///
    /// Returns [`EntryError::PurgeNotReplaceable`] for the entry of a purge, which a later purge
    /// keeps as the record of the first, and [`EntryError::LeaseNotReplaceable`] for the entry of
    /// a lease, which names no commit and so is never listed.
    pub(crate) fn to_stub(&self) -> Result<Self, EntryError> {
        if matches!(self.record, Record::Purge(_)) {
            return Err(EntryError::PurgeNotReplaceable(self.number));
        }
        if matches!(self.record, Record::Lease(_)) {
            return Err(EntryError::LeaseNotReplaceable(self.number));
        }
        if matches!(self.record, Record::RemotePin(_)) {
            return Err(EntryError::RemotePinNotReplaceable(self.number));
        }
        let body = self.body_hash();
        let replaced = match (&self.record, &self.chain) {
            (Record::Stub(stub), _) => stub.replaced,
            (_, Chain::Unchained) => None,
            (_, _) if self.pull().is_some_and(|p| p.kind() == PullKind::Approve) => {
                Some(StubKind::Approval)
            }
            (_, _) => Some(StubKind::Other),
        };
        let stub = Stub {
            time: self.time(),
            actor: self.actor().clone(),
            body,
            replaced,
        };
        Self::linked(self.number, self.chain, Record::Stub(stub))
    }

    /// Returns the canonical encoding (see the module documentation).
    pub fn encode(&self) -> Vec<u8> {
        let envelope = |header: &str| {
            format!(
                "{header}\nnumber {}\ntime {}\nactor {}\n",
                self.number,
                self.time(),
                self.actor()
            )
        };
        let previous = match self.chain {
            Chain::Unchained => None,
            Chain::Start => Some("-".to_owned()),
            Chain::After(hash) => Some(hash.to_string()),
        };
        let mut out = match (&previous, &self.record) {
            (None, Record::Change(_)) => envelope(HEADER),
            (None, _) => envelope(HEADER_2),
            (
                Some(previous),
                Record::Stub(Stub {
                    replaced: Some(_), ..
                }),
            ) => envelope(HEADER_4) + &format!("previous {previous}\n"),
            (Some(previous), _) => envelope(HEADER_3) + &format!("previous {previous}\n"),
        };
        match (&self.record, self.body_text()) {
            (Record::Stub(stub), _) => match (stub.body, stub.replaced) {
                (Some(body), Some(kind)) => {
                    out.push_str(&format!("stub {body} {}\n", kind.name()));
                }
                (Some(body), None) => out.push_str(&format!("stub {body}\n")),
                (None, _) => out.push_str("stub\n"),
            },
            (_, Some(body)) => out.push_str(&body),
            (_, None) => {}
        }
        out.into_bytes()
    }

    /// Reads an entry, as [`encode`](Self::encode) writes it. The reader is strict, by the rule of
    /// ADR-005: it rejects everything that `encode` would not have written, so an entry has
    /// exactly one encoding, and `parse(bytes).encode() == bytes` for every accepted input.
    ///
    /// # Errors
    ///
    /// Returns [`EntryParseError`] for input that is not UTF-8, a last line without a newline, a
    /// header that is missing, repeated, unknown, or out of order, a version other than 1 to 4, a
    /// number, time, actor, operation, commit ID, or branch name that is not in canonical form, a
    /// pointer whose old and new value are equal, branches that are not strictly ascending, a
    /// version-1 entry without a pointer or with the operation `purge`, a version-2 entry that is
    /// neither a stub nor a `purge` entry, a count or ranges that are not canonical, a purge that
    /// lists its own number or a later one, and a line after the last one; for version 3, a
    /// missing line `previous`, a hash that is not canonical, a stub without the hash of its body
    /// or a version-2 stub with one, and entry 1 that names an entry before it.
    /// For version 4: an entry that is not a stub, and a stub without a known kind
    /// (`approval` or `other`) after its hash.
    pub fn parse(bytes: &[u8]) -> Result<Self, EntryParseError> {
        let text = std::str::from_utf8(bytes).map_err(|error| EntryParseError::NotUtf8 {
            valid_up_to: error.valid_up_to(),
        })?;
        let malformed = |line, reason| EntryParseError::Malformed { line, reason };
        if !text.is_empty() && !text.ends_with('\n') {
            return Err(malformed(
                text.matches('\n').count() + 1,
                EntryMalformed::CutOff,
            ));
        }
        let lines: Vec<&str> = text.split_terminator('\n').collect();
        let line_at = |index: usize, missing| {
            lines
                .get(index)
                .copied()
                .ok_or_else(|| malformed(index + 1, missing))
        };

        // The header and its version.
        let header = line_at(0, EntryMalformed::ExpectedHeader)?;
        let version = match header.strip_prefix(HEADER_PREFIX) {
            Some("1") => 1,
            Some("2") => 2,
            Some("3") => 3,
            Some("4") => 4,
            Some(version) if !version.is_empty() && version.bytes().all(|b| b.is_ascii_digit()) => {
                return Err(EntryParseError::UnsupportedVersion(version.to_owned()));
            }
            _ => return Err(malformed(1, EntryMalformed::ExpectedHeader)),
        };

        let value = |index: usize, key: &str, reason| -> Result<&str, EntryParseError> {
            line_at(index, reason)?
                .strip_prefix(key)
                .ok_or_else(|| malformed(index + 1, reason))
        };
        let number = value(1, "number ", EntryMalformed::ExpectedNumber)?;
        let number = parse_number(number)
            .map_err(|reason| EntryParseError::InvalidNumber { line: 2, reason })?;
        let time = value(2, "time ", EntryMalformed::ExpectedTime)?;
        let time = parse_seconds(time)
            .map_err(|reason| EntryParseError::InvalidTime { line: 3, reason })?;
        let actor = value(3, "actor ", EntryMalformed::ExpectedActor)?;
        let actor = Actor::from_str(actor)
            .map_err(|source| EntryParseError::InvalidActor { line: 4, source })?;
        // An entry in the chain names the entry before it; its body starts one line later.
        let (chain, body) = if version >= 3 {
            let previous = value(4, "previous ", EntryMalformed::ExpectedPrevious)?;
            let chain = if previous == "-" {
                Chain::Start
            } else {
                Chain::After(
                    EntryHash::from_str(previous)
                        .map_err(|source| EntryParseError::InvalidHash { line: 5, source })?,
                )
            };
            (chain, 5)
        } else {
            (Chain::Unchained, 4)
        };
        let is_stub = lines
            .get(body)
            .is_some_and(|line| *line == "stub" || line.starts_with("stub "));
        let is_purge = lines.get(body).copied() == Some(&format!("operation {PURGE}"));
        let lease_kind = lines
            .get(body)
            .and_then(|line| line.strip_prefix("operation "))
            .and_then(LeaseKind::from_operation);
        let pull_kind = lines
            .get(body)
            .and_then(|line| line.strip_prefix("operation "))
            .and_then(PullKind::from_operation);
        if version == 3 && lines.get(body).copied() == Some(&format!("operation {CHECK}")) {
            return parse_check(&lines, body, number, chain, time, actor);
        }
        if version == 3 && lines.get(body).copied() == Some(&format!("operation {REMOTE_PIN}")) {
            return parse_remote_pin(&lines, body, number, chain, time, actor);
        }
        if let (3, Some(kind)) = (version, pull_kind) {
            return parse_pull(&lines, body, number, chain, time, actor, kind);
        }
        if let (3, Some(kind)) = (version, lease_kind) {
            return parse_lease(&lines, body, number, chain, time, actor, kind);
        }
        if version == 2 || (version == 3 && (is_stub || is_purge)) || version == 4 {
            return parse_stub_or_purge(&lines, body, number, chain, time, actor, version == 4);
        }
        let operation = value(body, "operation ", EntryMalformed::ExpectedOperation)?;
        let operation = OperationName::try_new(operation).map_err(|source| {
            EntryParseError::InvalidOperation {
                line: body + 1,
                source,
            }
        })?;

        // The worktree and its directory come before the pointers, if the entry has them.
        let mut first = body + 1;
        let mut worktree = None;
        let mut location = None;
        let mut former_owner = None;
        if let Some(name) = lines
            .get(first)
            .and_then(|line| line.strip_prefix("worktree "))
        {
            if !valid_worktree_name(name) {
                return Err(malformed(first + 1, EntryMalformed::BadWorktree));
            }
            worktree = Some(name.to_owned());
            first += 1;
            if let Some(path) = lines
                .get(first)
                .and_then(|line| line.strip_prefix("location "))
            {
                if !valid_location(path) {
                    return Err(malformed(first + 1, EntryMalformed::BadLocation));
                }
                location = Some(path.to_owned());
                first += 1;
                if let Some(text) = lines
                    .get(first)
                    .and_then(|line| line.strip_prefix("former-owner "))
                {
                    let owner = Actor::from_str(text)
                        .map_err(|_| malformed(first + 1, EntryMalformed::BadFormerOwner))?;
                    former_owner = Some(owner);
                    first += 1;
                }
            }
        }
        // The pointers, in the fixed order: head, working-copy, branches, tags.
        let mut head = None;
        let mut working_copy = None;
        let mut branches: Vec<(BranchName, Moved<CommitId>)> = Vec::new();
        let mut tags: Vec<(BranchName, Moved<CommitId>)> = Vec::new();
        let mut remotes: Vec<(RemoteRefName, Moved<RemoteRef>)> = Vec::new();
        for (index, line) in lines.iter().enumerate().skip(first) {
            let number = index + 1;
            let tokens: Vec<&str> = line.split(' ').collect();
            let after_branches = !branches.is_empty() || !tags.is_empty() || !remotes.is_empty();
            let after_tags = !remotes.is_empty();
            match tokens.as_slice() {
                ["head", old, new] => {
                    if head.is_some() || working_copy.is_some() || after_branches {
                        return Err(malformed(number, EntryMalformed::PointerOutOfOrder));
                    }
                    let moved = Moved {
                        old: parse_head_value(old, number)?,
                        new: parse_head_value(new, number)?,
                    };
                    if moved.old == moved.new {
                        return Err(EntryParseError::Unchanged { line: number });
                    }
                    head = Some(moved);
                }
                ["working-copy", old, new] => {
                    if working_copy.is_some() || after_branches {
                        return Err(malformed(number, EntryMalformed::PointerOutOfOrder));
                    }
                    let moved = Moved {
                        old: parse_commit_value(old, number)?,
                        new: parse_commit_value(new, number)?,
                    };
                    if moved.old == moved.new {
                        return Err(EntryParseError::Unchanged { line: number });
                    }
                    working_copy = Some(moved);
                }
                ["branch", name, old, new] => {
                    if !tags.is_empty() || after_tags {
                        return Err(malformed(number, EntryMalformed::PointerOutOfOrder));
                    }
                    let name = BranchName::try_new(*name).map_err(|source| {
                        EntryParseError::InvalidBranchName {
                            line: number,
                            source,
                        }
                    })?;
                    let moved = Moved {
                        old: parse_commit_value(old, number)?,
                        new: parse_commit_value(new, number)?,
                    };
                    if moved.old == moved.new {
                        return Err(EntryParseError::Unchanged { line: number });
                    }
                    if let Some((previous, _)) = branches.last() {
                        if previous.as_str().as_bytes() >= name.as_str().as_bytes() {
                            return Err(EntryParseError::OutOfOrder {
                                line: number,
                                name: name.to_string(),
                            });
                        }
                    }
                    branches.push((name, moved));
                }
                ["tag", name, old, new] => {
                    if after_tags {
                        return Err(malformed(number, EntryMalformed::PointerOutOfOrder));
                    }
                    let name = BranchName::try_new(*name).map_err(|source| {
                        EntryParseError::InvalidBranchName {
                            line: number,
                            source,
                        }
                    })?;
                    let moved = Moved {
                        old: parse_commit_value(old, number)?,
                        new: parse_commit_value(new, number)?,
                    };
                    if moved.old == moved.new {
                        return Err(EntryParseError::Unchanged { line: number });
                    }
                    if let Some((previous, _)) = tags.last() {
                        if previous.as_str().as_bytes() >= name.as_str().as_bytes() {
                            return Err(EntryParseError::OutOfOrder {
                                line: number,
                                name: name.to_string(),
                            });
                        }
                    }
                    tags.push((name, moved));
                }
                ["remote", name, old_commit, old_entry, new_commit, new_entry] => {
                    let name = RemoteRefName::from_str(name).map_err(|source| {
                        EntryParseError::InvalidRemoteName {
                            line: number,
                            source,
                        }
                    })?;
                    let moved = Moved {
                        old: parse_remote_value(old_commit, old_entry, number)?,
                        new: parse_remote_value(new_commit, new_entry, number)?,
                    };
                    if moved.old == moved.new {
                        return Err(EntryParseError::Unchanged { line: number });
                    }
                    if remotes
                        .last()
                        .is_some_and(|(previous, _)| *previous >= name)
                    {
                        return Err(EntryParseError::OutOfOrder {
                            line: number,
                            name: name.to_string(),
                        });
                    }
                    remotes.push((name, moved));
                }
                ["head" | "working-copy" | "branch" | "tag" | "remote", ..] => {
                    return Err(malformed(number, EntryMalformed::WrongValueCount));
                }
                _ => return Err(malformed(number, EntryMalformed::UnknownPointer)),
            }
        }
        if head.is_none()
            && working_copy.is_none()
            && branches.is_empty()
            && tags.is_empty()
            && remotes.is_empty()
            && operation.as_str() != TAKE_OVER
        {
            return Err(malformed(lines.len() + 1, EntryMalformed::ExpectedPointer));
        }
        // The rules of a change live in one place. The lines have been checked one by one above, to
        // report the line; this builds the value through the constructor, so that a reader cannot
        // return a change that the constructor would reject.
        let pointers = Pointers {
            head,
            working_copy,
            branches,
            tags,
            remotes,
        };
        Change::build(time, actor, operation, pointers)
            .and_then(|change| change.in_worktree(worktree, location))
            .and_then(|change| match former_owner {
                Some(owner) => change.with_former_owner(owner),
                None => Ok(change),
            })
            .and_then(|change| Entry::linked(number, chain, Record::Change(change)))
            .map_err(EntryParseError::Entry)
    }
}

/// Reads the body of a lease entry, which starts at the line `operation lease-…` with the index
/// `body`: `branch <name>`, `from <actor>` (a takeover and an override), `expires <seconds>` (all
/// but a release), and `reason <text>` (a takeover and an override), in this order.
fn parse_lease(
    lines: &[&str],
    body: usize,
    number: u64,
    chain: Chain,
    time: i64,
    actor: Actor,
    kind: LeaseKind,
) -> Result<Entry, EntryParseError> {
    let malformed = |line, reason| EntryParseError::Malformed { line, reason };
    let line_at = |index: usize, key: &str, reason| -> Result<&str, EntryParseError> {
        lines
            .get(index)
            .and_then(|line| line.strip_prefix(key))
            .ok_or_else(|| malformed(index + 1, reason))
    };
    let mut next = body + 1;
    let branch = line_at(next, "branch ", EntryMalformed::ExpectedBranch)?;
    let branch =
        BranchName::try_new(branch).map_err(|source| EntryParseError::InvalidBranchName {
            line: next + 1,
            source,
        })?;
    next += 1;
    let with_from = matches!(kind, LeaseKind::Takeover | LeaseKind::Override);
    let from = if with_from {
        let text = line_at(next, "from ", EntryMalformed::ExpectedFrom)?;
        let from = Actor::from_str(text).map_err(|source| EntryParseError::InvalidActor {
            line: next + 1,
            source,
        })?;
        next += 1;
        Some(from)
    } else {
        None
    };
    let expires = if kind == LeaseKind::Release {
        None
    } else {
        let text = line_at(next, "expires ", EntryMalformed::ExpectedExpires)?;
        let expires = parse_seconds(text).map_err(|reason| EntryParseError::InvalidTime {
            line: next + 1,
            reason,
        })?;
        next += 1;
        Some(expires)
    };
    let reason = if with_from {
        let text = line_at(next, "reason ", EntryMalformed::ExpectedReason)?;
        if kind == LeaseKind::Takeover && text != EXPIRED {
            return Err(malformed(next + 1, EntryMalformed::ExpectedReason));
        }
        next += 1;
        Some(text.to_owned())
    } else {
        None
    };
    if next < lines.len() {
        return Err(malformed(next + 1, EntryMalformed::TrailingLine));
    }
    LeaseEntry::build(time, actor, kind, branch, from, expires, reason)
        .and_then(|lease| Entry::linked(number, chain, Record::Lease(lease)))
        .map_err(EntryParseError::Entry)
}

/// Reads the body of a check entry, which starts with the line `operation check` at the index
/// `body`: `version 1` or `version 2`, `commit <id>`, `base <id>` or `base -`, `result pass` or
/// `result fail`, and for version 2 `statement <hash>`.
fn parse_check(
    lines: &[&str],
    body: usize,
    number: u64,
    chain: Chain,
    time: i64,
    actor: Actor,
) -> Result<Entry, EntryParseError> {
    let malformed = |line, reason| EntryParseError::Malformed { line, reason };
    let line_at = |index: usize, key: &str, reason| -> Result<&str, EntryParseError> {
        lines
            .get(index)
            .and_then(|line| line.strip_prefix(key))
            .ok_or_else(|| malformed(index + 1, reason))
    };
    let commit_of = |index: usize, text: &str| -> Result<CommitId, EntryParseError> {
        ObjectId::from_str(text)
            .map(CommitId::new)
            .map_err(|source| EntryParseError::InvalidObjectId {
                line: index + 1,
                source,
            })
    };
    let version = line_at(body + 1, "version ", EntryMalformed::ExpectedVersion)?;
    let names_statement = if version == CHECK_VERSION.to_string() {
        false
    } else if version == CHECK_STATEMENT_VERSION.to_string() {
        true
    } else {
        return Err(malformed(body + 2, EntryMalformed::ExpectedVersion));
    };
    let commit = commit_of(
        body + 2,
        line_at(body + 2, "commit ", EntryMalformed::ExpectedCommit)?,
    )?;
    let base = match line_at(body + 3, "base ", EntryMalformed::ExpectedBase)? {
        "-" => None,
        text => Some(commit_of(body + 3, text)?),
    };
    let result = CheckResult::from_name(line_at(
        body + 4,
        "result ",
        EntryMalformed::ExpectedResult,
    )?)
    .ok_or_else(|| malformed(body + 5, EntryMalformed::ExpectedResult))?;
    let mut check = CheckEntry::new(time, actor, commit, base, result);
    let mut next = body + 5;
    if names_statement {
        check = check.with_statement(statement_at(lines, next)?);
        next += 1;
    }
    if next < lines.len() {
        return Err(malformed(next + 1, EntryMalformed::TrailingLine));
    }
    Entry::linked(number, chain, Record::Check(check)).map_err(EntryParseError::Entry)
}

/// Reads the body of the entry of a pinning, which starts with the line `operation remote-pin` at
/// the index `body`: `remote <name>`, `old <key>` or `old -`, and `new <key>`. A key is in its
/// canonical form, without a comment.
fn parse_remote_pin(
    lines: &[&str],
    body: usize,
    number: u64,
    chain: Chain,
    time: i64,
    actor: Actor,
) -> Result<Entry, EntryParseError> {
    let malformed = |line, reason| EntryParseError::Malformed { line, reason };
    let line_at = |index: usize, key: &str, reason| -> Result<&str, EntryParseError> {
        lines
            .get(index)
            .and_then(|line| line.strip_prefix(key))
            .ok_or_else(|| malformed(index + 1, reason))
    };
    let key_at = |index: usize, text: &str, reason| -> Result<PublicKey, EntryParseError> {
        PublicKey::from_openssh(text)
            .ok()
            .filter(|key| key.to_openssh() == text)
            .ok_or_else(|| malformed(index + 1, reason))
    };
    let remote = line_at(body + 1, "remote ", EntryMalformed::ExpectedRemote)?;
    let remote = RemoteName::try_new(remote)
        .map_err(|_| malformed(body + 2, EntryMalformed::ExpectedRemote))?;
    let old = match line_at(body + 2, "old ", EntryMalformed::ExpectedOldKey)? {
        "-" => None,
        text => Some(key_at(body + 2, text, EntryMalformed::ExpectedOldKey)?),
    };
    let new = line_at(body + 3, "new ", EntryMalformed::ExpectedNewKey)?;
    let new = key_at(body + 3, new, EntryMalformed::ExpectedNewKey)?;
    if body + 4 < lines.len() {
        return Err(malformed(body + 5, EntryMalformed::TrailingLine));
    }
    RemotePinEntry::try_new(time, actor, remote, old, new)
        .and_then(|pin| Entry::linked(number, chain, Record::RemotePin(pin)))
        .map_err(EntryParseError::Entry)
}

/// Reads the line `statement <hash>` at the index `index`: the hash of a signed statement in
/// canonical form.
fn statement_at(lines: &[&str], index: usize) -> Result<StatementHash, EntryParseError> {
    let text = lines
        .get(index)
        .and_then(|line| line.strip_prefix("statement "))
        .ok_or(EntryParseError::Malformed {
            line: index + 1,
            reason: EntryMalformed::ExpectedStatement,
        })?;
    text.parse()
        .map_err(|source| EntryParseError::InvalidObjectId {
            line: index + 1,
            source,
        })
}

/// Reads the body of a pull request entry, which starts with the line `operation pr-…` at the
/// index `body`: `branch <name>` (an opening) or `pull <number>` (a closing, a merge, and an
/// approval), then `commit <id>` (an opening, a merge, and an approval), and for an opening
/// `reviewer <actor>` in ascending order and `repairs <id>`. An opening, a closing, and an
/// approval may end with `statement <hash>`.
fn parse_pull(
    lines: &[&str],
    body: usize,
    number: u64,
    chain: Chain,
    time: i64,
    actor: Actor,
    kind: PullKind,
) -> Result<Entry, EntryParseError> {
    let malformed = |line, reason| EntryParseError::Malformed { line, reason };
    let line_at = |index: usize, key: &str, reason| -> Result<&str, EntryParseError> {
        lines
            .get(index)
            .and_then(|line| line.strip_prefix(key))
            .ok_or_else(|| malformed(index + 1, reason))
    };
    let commit_at = |index: usize, key: &str| -> Result<CommitId, EntryParseError> {
        let text = line_at(index, key, EntryMalformed::ExpectedCommit)?;
        ObjectId::from_str(text)
            .map(CommitId::new)
            .map_err(|source| EntryParseError::InvalidObjectId {
                line: index + 1,
                source,
            })
    };
    let mut next = body + 1;
    let mut branch = None;
    let mut pull = None;
    if kind == PullKind::Open {
        let text = line_at(next, "branch ", EntryMalformed::ExpectedBranch)?;
        branch = Some(BranchName::try_new(text).map_err(|source| {
            EntryParseError::InvalidBranchName {
                line: next + 1,
                source,
            }
        })?);
    } else {
        let text = line_at(next, "pull ", EntryMalformed::ExpectedPull)?;
        pull = Some(
            parse_number(text).map_err(|reason| EntryParseError::InvalidNumber {
                line: next + 1,
                reason,
            })?,
        );
    }
    next += 1;
    let commit = if kind == PullKind::Close {
        None
    } else {
        next += 1;
        Some(commit_at(next - 1, "commit ")?)
    };
    let mut reviewers = Vec::new();
    let mut repairs = None;
    if kind == PullKind::Open {
        while let Some(text) = lines
            .get(next)
            .and_then(|line| line.strip_prefix("reviewer "))
        {
            reviewers.push(Actor::from_str(text).map_err(|source| {
                EntryParseError::InvalidActor {
                    line: next + 1,
                    source,
                }
            })?);
            next += 1;
        }
        if lines
            .get(next)
            .is_some_and(|line| line.starts_with("repairs "))
        {
            repairs = Some(commit_at(next, "repairs ")?);
            next += 1;
        }
    }
    // An entry of the repository format 16 names its statement; one of an older format has no
    // such line. A merge never has it.
    let statement = if kind != PullKind::Merge
        && lines
            .get(next)
            .is_some_and(|line| line.starts_with("statement "))
    {
        next += 1;
        Some(statement_at(lines, next - 1)?)
    } else {
        None
    };
    if next < lines.len() {
        return Err(malformed(next + 1, EntryMalformed::TrailingLine));
    }
    PullEntry::build(time, actor, kind, branch, pull, commit, reviewers, repairs)
        .and_then(|entry| match statement {
            Some(statement) => entry.with_statement(statement),
            None => Ok(entry),
        })
        .and_then(|entry| Entry::linked(number, chain, Record::Pull(entry)))
        .map_err(EntryParseError::Entry)
}

/// Reads the body of a stub or a `purge` entry, which starts at the line with the index `body`:
/// `stub` alone (version 2) or `stub <hash of the body>` (version 3), or `operation purge`,
/// `commits <n>`, and `stubs <ranges>`.
/// Version 4 adds the kind after the hash (`stub <hash> approval` or `stub <hash> other`).
fn parse_stub_or_purge(
    lines: &[&str],
    body: usize,
    number: u64,
    chain: Chain,
    time: i64,
    actor: Actor,
    with_kind: bool,
) -> Result<Entry, EntryParseError> {
    let malformed = |line, reason| EntryParseError::Malformed { line, reason };
    let first = lines.get(body).copied();
    let record = match first {
        Some(line) if line == "stub" || line.starts_with("stub ") => {
            // Version 4 ends the line with the kind; no other version has one.
            let (text, replaced) = match (line.strip_prefix("stub "), with_kind) {
                (Some(rest), true) => match rest.rsplit_once(' ') {
                    Some((hash, "approval")) => (Some(hash), Some(StubKind::Approval)),
                    Some((hash, "other")) => (Some(hash), Some(StubKind::Other)),
                    _ => return Err(malformed(body + 1, EntryMalformed::ExpectedStubOrPurge)),
                },
                (_, true) => return Err(malformed(body + 1, EntryMalformed::ExpectedStubOrPurge)),
                (rest, false) => (rest, None),
            };
            let hash = match (text, chain) {
                (None, Chain::Unchained) => None,
                (Some(hash), Chain::Start | Chain::After(_)) => {
                    Some(BodyHash::from_str(hash).map_err(|source| {
                        EntryParseError::InvalidHash {
                            line: body + 1,
                            source,
                        }
                    })?)
                }
                _ => return Err(malformed(body + 1, EntryMalformed::ExpectedStubOrPurge)),
            };
            if lines.len() > body + 1 {
                return Err(malformed(body + 2, EntryMalformed::TrailingLine));
            }
            Record::Stub(Stub {
                time,
                actor,
                body: hash,
                replaced,
            })
        }
        Some(line) if line == format!("operation {PURGE}") && !with_kind => {
            let commits = lines
                .get(body + 1)
                .and_then(|line| line.strip_prefix("commits "))
                .ok_or_else(|| malformed(body + 2, EntryMalformed::ExpectedCommits))?;
            let commits = parse_count(commits)
                .ok_or_else(|| malformed(body + 2, EntryMalformed::ExpectedCommits))?;
            // The line `tags <n>` is optional, and `0` is not written: one encoding per entry.
            let (tags, next) = match lines
                .get(body + 2)
                .and_then(|line| line.strip_prefix("tags "))
            {
                Some(count) => {
                    let tags = parse_count(count)
                        .filter(|tags| *tags > 0)
                        .ok_or_else(|| malformed(body + 3, EntryMalformed::BadTags))?;
                    (tags, body + 3)
                }
                None => (0, body + 2),
            };
            let stubs = lines
                .get(next)
                .and_then(|line| line.strip_prefix("stubs "))
                .ok_or_else(|| malformed(next + 1, EntryMalformed::ExpectedStubs))?;
            let stubs =
                parse_ranges(stubs).ok_or_else(|| malformed(next + 1, EntryMalformed::BadStubs))?;
            if lines.len() > next + 1 {
                return Err(malformed(next + 2, EntryMalformed::TrailingLine));
            }
            Record::Purge(Purge {
                time,
                actor,
                commits,
                tags,
                stubs,
            })
        }
        _ => return Err(malformed(body + 1, EntryMalformed::ExpectedStubOrPurge)),
    };
    Entry::linked(number, chain, record).map_err(EntryParseError::Entry)
}

/// Writes ranges as the log and the command line do: `4-5 7`.
pub(crate) fn ranges_text(ranges: &[(u64, u64)]) -> String {
    ranges
        .iter()
        .map(|(start, end)| {
            if start == end {
                start.to_string()
            } else {
                format!("{start}-{end}")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Reads a count: decimal digits without a leading zero, or `0`.
fn parse_count(text: &str) -> Option<u64> {
    if text == "0" {
        return Some(0);
    }
    parse_number(text).ok()
}

/// Reads the ranges of a `purge` entry: `-` for none, or ranges in ascending order that neither
/// overlap nor touch, each a number or `<start>-<end>` with the start below the end.
pub(crate) fn parse_ranges(text: &str) -> Option<Vec<(u64, u64)>> {
    if text == "-" {
        return Some(Vec::new());
    }
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    for token in text.split(' ') {
        let (start, end) = match token.split_once('-') {
            Some((start, end)) => {
                let (start, end) = (parse_number(start).ok()?, parse_number(end).ok()?);
                if start >= end {
                    return None;
                }
                (start, end)
            }
            None => {
                let number = parse_number(token).ok()?;
                (number, number)
            }
        };
        if let Some((_, previous)) = ranges.last() {
            if start <= previous + 1 {
                return None;
            }
        }
        ranges.push((start, end));
    }
    Some(ranges)
}

/// The text of a commit value: `-` or the ID.
fn commit_value(value: Option<&CommitId>) -> String {
    value.map_or_else(|| "-".to_owned(), ToString::to_string)
}

/// The text of the value of a remote reference in an entry: the commit and the hash of the record
/// entry, or `- -` for a reference that does not exist.
fn remote_value(value: Option<&RemoteRef>) -> String {
    value.map_or_else(
        || "- -".to_owned(),
        |reference| format!("{} {}", reference.commit, reference.entry),
    )
}

/// Reads the value of a remote reference from its two tokens; see [`remote_value`].
fn parse_remote_value(
    commit: &str,
    entry: &str,
    line: usize,
) -> Result<Option<RemoteRef>, EntryParseError> {
    match (commit, entry) {
        ("-", "-") => Ok(None),
        ("-", _) | (_, "-") => Err(EntryParseError::Malformed {
            line,
            reason: EntryMalformed::HalfRemoteValue,
        }),
        (commit, entry) => {
            let commit = ObjectId::from_str(commit)
                .map(CommitId::new)
                .map_err(|source| EntryParseError::InvalidObjectId { line, source })?;
            let entry = RecordHash::from_str(entry)
                .map_err(|source| EntryParseError::InvalidHash { line, source })?;
            Ok(Some(RemoteRef { commit, entry }))
        }
    }
}

/// The text of a HEAD value: `-`, `branch:<name>`, or `commit:<id>`.
fn head_value(value: Option<&Head>) -> String {
    match value {
        None => "-".to_owned(),
        Some(Head::Branch(name)) => format!("branch:{name}"),
        Some(Head::Detached(commit)) => format!("commit:{commit}"),
    }
}

fn parse_commit_value(text: &str, line: usize) -> Result<Option<CommitId>, EntryParseError> {
    if text == "-" {
        return Ok(None);
    }
    ObjectId::from_str(text)
        .map(|id| Some(CommitId::new(id)))
        .map_err(|source| EntryParseError::InvalidObjectId { line, source })
}

fn parse_head_value(text: &str, line: usize) -> Result<Option<Head>, EntryParseError> {
    if text == "-" {
        return Ok(None);
    }
    match text.split_once(':') {
        Some(("branch", name)) => BranchName::try_new(name)
            .map(|name| Some(Head::Branch(name)))
            .map_err(|source| EntryParseError::InvalidBranchName { line, source }),
        Some(("commit", id)) => ObjectId::from_str(id)
            .map(|id| Some(Head::Detached(CommitId::new(id))))
            .map_err(|source| EntryParseError::InvalidObjectId { line, source }),
        _ => Err(EntryParseError::Malformed {
            line,
            reason: EntryMalformed::UnknownHeadValue,
        }),
    }
}

/// Reads the number of an entry: decimal digits without a leading zero, from 1 to
/// [`MAX_NUMBER`].
fn parse_number(text: &str) -> Result<u64, NumberError> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) || (text.starts_with('0')) {
        return Err(NumberError::NotCanonical);
    }
    match text.parse::<u64>() {
        Ok(number) if number <= MAX_NUMBER => Ok(number),
        _ => Err(NumberError::OutOfRange),
    }
}

/// Why the number of an entry is rejected. The `Display` form is the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NumberError {
    /// The text is empty, has a character that is not a digit, or starts with `0`.
    NotCanonical,
    /// The number is larger than [`MAX_NUMBER`], or does not fit a 64-bit integer.
    OutOfRange,
}

impl fmt::Display for NumberError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotCanonical => {
                f.write_str("the number is not a decimal integer from 1, without a leading zero")
            }
            Self::OutOfRange => write!(f, "the number is larger than {MAX_NUMBER}"),
        }
    }
}

impl Error for NumberError {}

/// What is wrong with the shape of the lines of an entry. The `Display` form is the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EntryMalformed {
    /// The last line has no terminating newline: the entry is cut off.
    CutOff,
    /// The first line is not `stemma-oplog <version>`.
    ExpectedHeader,
    /// The second line is not `number <n>`.
    ExpectedNumber,
    /// The third line is not `time <seconds>`.
    ExpectedTime,
    /// The fourth line is not `actor <actor line>`.
    ExpectedActor,
    /// The fifth line is not `operation <name>`.
    ExpectedOperation,
    /// The entry ends without a line for a pointer.
    ExpectedPointer,
    /// A line is not `head`, `working-copy`, or `branch`.
    UnknownPointer,
    /// A pointer line has too few or too many values.
    WrongValueCount,
    /// `head` or `working-copy` comes after a line that has to come later, or twice.
    PointerOutOfOrder,
    /// A value of `head` is not `-`, `branch:<name>`, or `commit:<id>`.
    UnknownHeadValue,
    /// The file is larger than an entry can be.
    TooLong,
    /// The fifth line of a version-2 entry is neither `stub` nor `operation purge`.
    ExpectedStubOrPurge,
    /// The sixth line of a `purge` entry is not `commits <n>`.
    ExpectedCommits,
    /// The line `tags <n>` of a `purge` entry does not hold a count above 0.
    BadTags,
    /// The line after `commits <n>` (and `tags <n>`) of a `purge` entry is not `stubs <ranges>`.
    ExpectedStubs,
    /// The ranges of the stubs are not `-` or ascending ranges that neither overlap nor touch.
    BadStubs,
    /// A stub or a `purge` entry has a line after its last one.
    TrailingLine,
    /// The fifth line of a version-3 entry is not `previous <hash>` or `previous -`.
    ExpectedPrevious,
    /// The line `worktree` names `main` or a name that no linked worktree has.
    BadWorktree,
    /// The line `location` is empty or holds a control character.
    BadLocation,
    /// The line `former-owner` is not an actor.
    BadFormerOwner,
    /// The line after `operation lease-…` of a lease entry is not `branch <name>`.
    ExpectedBranch,
    /// A takeover or an override has no line `from <actor>`, or another entry has one.
    ExpectedFrom,
    /// An acquisition, a takeover, or an override has no line `expires <seconds>`, or a release
    /// has one.
    ExpectedExpires,
    /// A takeover or an override has no line `reason <text>`, a takeover has another reason than
    /// `expired`, or another entry has the line.
    ExpectedReason,
    /// An opening, a merge, an approval, or a pull request entry has no line `commit <id>` or `repairs <id>`
    /// where it belongs.
    ExpectedCommit,
    /// A closing, a merge, or an approval has no line `pull <number>`.
    ExpectedPull,
    /// A check has no line `version 1` or `version 2` after the operation.
    ExpectedVersion,
    /// A check has no line `base <id>` or `base -` after the commit.
    ExpectedBase,
    /// A check has no line `result pass` or `result fail` after the base.
    ExpectedResult,
    /// A check of version 2 has no line `statement <hash>` after the result.
    ExpectedStatement,
    /// The entry of a pinning lacks the line `remote <name>`, or the name is no name of a remote.
    ExpectedRemote,
    /// The entry of a pinning lacks the line `old <key>` or `old -`, or the key is not in its
    /// canonical form.
    ExpectedOldKey,
    /// The entry of a pinning lacks the line `new <key>`, or the key is not in its canonical
    /// form.
    ExpectedNewKey,
    /// A value of a line `remote` has a commit without the hash of its record entry, or the
    /// other way round: both are `-`, or neither is.
    HalfRemoteValue,
}

impl fmt::Display for EntryMalformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::CutOff => "the last line has no terminating newline",
            Self::ExpectedHeader => "expected the header `stemma-oplog 1`, `2`, or `3` first",
            Self::ExpectedNumber => "expected the header `number <n>`",
            Self::ExpectedTime => "expected the header `time <seconds>`",
            Self::ExpectedActor => "expected the header `actor <actor>`",
            Self::ExpectedOperation => "expected the header `operation <name>`",
            Self::ExpectedPointer => "expected the line of a pointer that changed",
            Self::UnknownPointer => "expected `head`, `working-copy`, or `branch <name>`",
            Self::WrongValueCount => "expected the old and the new value of the pointer",
            Self::PointerOutOfOrder => {
                "the pointers come in the order head, working-copy, branches, each once"
            }
            Self::UnknownHeadValue => "expected `-`, `branch:<name>`, or `commit:<id>`",
            Self::TooLong => "the file is too large to be an entry",
            Self::ExpectedStubOrPurge => {
                "expected `stub` (version 2), `stub <hash>` (version 3), `stub <hash> <kind>` (version 4), or `operation purge`"
            }
            Self::ExpectedCommits => "expected `commits <n>`",
            Self::BadTags => "expected `tags <n>` with a count above 0, or no line `tags`",
            Self::ExpectedStubs => "expected `stubs <ranges>`",
            Self::BadStubs => {
                "expected `-` or ranges such as `4-5 7`, ascending, that neither overlap nor touch"
            }
            Self::TrailingLine => "the entry has a line after its last one",
            Self::ExpectedPrevious => "expected the header `previous <hash>` or `previous -`",
            Self::BadWorktree => {
                "the name of the worktree is empty, is `main`, or has a character that a worktree \
                 name does not have"
            }
            Self::BadLocation => {
                "the directory of the worktree is empty or has a control character"
            }
            Self::BadFormerOwner => {
                "the line `former-owner` is not an actor"
            }
            Self::ExpectedBranch => "expected the line `branch <name>`",
            Self::ExpectedFrom => {
                "expected the line `from <actor>` for a takeover and an override, and none for \
                 another lease entry"
            }
            Self::ExpectedExpires => {
                "expected the line `expires <seconds>` for every lease entry but a release"
            }
            Self::ExpectedReason => {
                "expected the line `reason expired` for a takeover, `reason <text>` for an \
                 override, and none for another lease entry"
            }
            Self::ExpectedCommit => "expected the line `commit <id>` or `repairs <id>`",
            Self::ExpectedPull => "expected the line `pull <number>`",
            Self::ExpectedVersion => "expected the line `version 1` or `version 2`",
            Self::ExpectedBase => "expected the line `base <id>` or `base -`",
            Self::ExpectedResult => "expected the line `result pass` or `result fail`",
            Self::ExpectedStatement => "expected the line `statement <hash>`",
            Self::ExpectedRemote => "expected the line `remote <name>` with the name of a remote",
            Self::ExpectedOldKey => {
                "expected the line `old <key>` with an Ed25519 key without a comment, or `old -`"
            }
            Self::ExpectedNewKey => {
                "expected the line `new <key>` with an Ed25519 key without a comment"
            }
            Self::HalfRemoteValue => {
                "expected the commit and the record entry of the remote reference, or `- -`"
            }
        })
    }
}

/// Why the body of an entry is rejected by [`Entry::parse`]. Line numbers count from 1.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EntryParseError {
    /// The body is not valid UTF-8.
    NotUtf8 {
        /// Length of the longest valid prefix, in bytes.
        valid_up_to: usize,
    },
    /// A line is missing, repeated, unknown, or out of order, or the last line is cut off.
    Malformed {
        /// Number of the line.
        line: usize,
        /// What is wrong with it.
        reason: EntryMalformed,
    },
    /// The header names another version than 1, 2, or 3, in decimal digits.
    UnsupportedVersion(String),
    /// The number is not canonical.
    InvalidNumber {
        /// Number of the line.
        line: usize,
        /// Why the number is invalid.
        reason: NumberError,
    },
    /// The time is not a canonical decimal integer of seconds, or does not fit `i64`.
    InvalidTime {
        /// Number of the line.
        line: usize,
        /// Why the time is invalid.
        reason: TimeError,
    },
    /// The actor line is rejected.
    InvalidActor {
        /// Number of the line.
        line: usize,
        /// Why the actor is invalid.
        source: ActorParseError,
    },
    /// The name of the operation is rejected.
    InvalidOperation {
        /// Number of the line.
        line: usize,
        /// Why the name is invalid.
        source: OperationNameError,
    },
    /// A commit ID is not in canonical form.
    InvalidObjectId {
        /// Number of the line.
        line: usize,
        /// Why the ID is invalid.
        source: ObjectIdParseError,
    },
    /// A hash of the chain is not in canonical form.
    InvalidHash {
        /// Number of the line.
        line: usize,
        /// Why the hash is invalid.
        source: ObjectIdParseError,
    },
    /// A branch name is rejected.
    InvalidBranchName {
        /// Number of the line.
        line: usize,
        /// Why the name is invalid.
        source: BranchNameError,
    },
    /// The name of a remote reference is rejected.
    InvalidRemoteName {
        /// Number of the line.
        line: usize,
        /// Why the name is invalid.
        source: RemoteRefNameError,
    },
    /// The old and the new value of a pointer are equal.
    Unchanged {
        /// Number of the line.
        line: usize,
    },
    /// A branch does not sort after the one before it, or is listed twice.
    OutOfOrder {
        /// Number of the line.
        line: usize,
        /// The name of the branch.
        name: String,
    },
    /// The lines are well formed, but the entry is rejected as a whole: a change of the operation
    /// `purge`, a `purge` entry that lists its own number or a later one, or entry 1 that names
    /// an entry before it. The other rules of [`Change::try_new`] and `Entry::linked` are checked
    /// while the lines are read.
    Entry(EntryError),
}

impl fmt::Display for EntryParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotUtf8 { valid_up_to } => write!(
                f,
                "the entry is not valid UTF-8 (valid up to byte {valid_up_to})"
            ),
            Self::Malformed { line, reason } => write!(f, "line {line}: {reason}"),
            Self::UnsupportedVersion(version) => write!(
                f,
                "the entry has version {version}, but this code reads versions 1 to 4"
            ),
            Self::InvalidNumber { line, reason } => write!(f, "line {line}: {reason}"),
            Self::InvalidTime { line, reason } => write!(f, "line {line}: {reason}"),
            Self::InvalidActor { line, source } => write!(f, "line {line}: {source}"),
            Self::InvalidOperation { line, source } => write!(f, "line {line}: {source}"),
            Self::InvalidObjectId { line, source } => {
                write!(f, "line {line}: invalid commit ID: {source}")
            }
            Self::InvalidHash { line, source } => {
                write!(f, "line {line}: invalid hash: {source}")
            }
            Self::InvalidBranchName { line, source } => write!(f, "line {line}: {source}"),
            Self::InvalidRemoteName { line, source } => write!(f, "line {line}: {source}"),
            Self::Unchanged { line } => {
                write!(f, "line {line}: the old and the new value are equal")
            }
            Self::OutOfOrder { line, name } => write!(
                f,
                "line {line}: the branch `{name}` does not sort after the one before it; \
                 branches must be strictly ascending"
            ),
            Self::Entry(source) => source.fmt(f),
        }
    }
}

impl Error for EntryParseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        // The message of each variant includes the message of its cause, so the source is the
        // cause of the cause (see the crate documentation).
        match self {
            Self::InvalidNumber { reason, .. } => reason.source(),
            Self::InvalidTime { reason, .. } => reason.source(),
            Self::InvalidActor { source, .. } => source.source(),
            Self::InvalidOperation { source, .. } => source.source(),
            Self::InvalidObjectId { source, .. } | Self::InvalidHash { source, .. } => {
                source.source()
            }
            Self::InvalidBranchName { source, .. } => source.source(),
            Self::InvalidRemoteName { source, .. } => source.source(),
            _ => None,
        }
    }
}

/// Why an operation on the log fails.
#[derive(Debug)]
#[non_exhaustive]
pub enum OpLogError {
    /// The file system failed. The cause is the field `source`, and its message ends the message
    /// of this error. Use [`OpLogError::io_error`] to reach it.
    Io {
        /// What was being done, for example "write the entry".
        operation: &'static str,
        /// The file or directory involved.
        path: PathBuf,
        /// The error of the file system.
        source: io::Error,
    },
    /// An entry is not in canonical form.
    Corrupt {
        /// The file.
        path: PathBuf,
        /// What is wrong with it.
        source: EntryParseError,
    },
    /// The number inside an entry is not the number in the name of its file.
    WrongNumber {
        /// The file.
        path: PathBuf,
        /// The number in the name.
        expected: u64,
        /// The number inside.
        found: u64,
    },
    /// The numbers of the entries have a gap: an entry is lost.
    Gap {
        /// The first number that is missing.
        missing: u64,
    },
    /// The tip file names an entry that the directory does not hold: the last entries are lost.
    /// The writer refuses to write, as the next number would be given a second time to another
    /// move.
    TipAhead {
        /// The tip file.
        path: PathBuf,
        /// The number of the last entry that the tip file names.
        tip: u64,
        /// The largest number in the directory, 0 if it holds no entry.
        last: u64,
    },
    /// The tip file is not in canonical form.
    BadTip {
        /// The file.
        path: PathBuf,
    },
    /// The tip file names the hash of an entry, but the entry with its number has another hash:
    /// the entry was replaced, or the tip file was.
    TipMismatch {
        /// The tip file.
        path: PathBuf,
        /// The number that the tip file names.
        number: u64,
        /// The hash that the tip file names.
        expected: EntryHash,
        /// The hash that the entry has, or `None` if the entry was written before the chain and
        /// has none.
        found: Option<EntryHash>,
    },
    /// The last entry is in the hash chain, but there is no tip file to compare it with. Only
    /// [`FsOpLog::verify`] reports it: a stop between the first entry of the chain and its tip
    /// leaves the log so as well, and the next entry writes the tip.
    TipMissing {
        /// The tip file.
        path: PathBuf,
        /// The number of the last entry.
        last: u64,
    },
    /// The tip file names an entry more than one before the last: writing the tip failed more
    /// than once, so the writers compared nothing with it. Only [`FsOpLog::verify`] reports it.
    TipBehind {
        /// The tip file.
        path: PathBuf,
        /// The number that the tip file names.
        tip: u64,
        /// The number of the last entry.
        last: u64,
    },
    /// An entry does not name the hash of the entry before it, or starts the chain again: an entry
    /// at this place or before it was changed, removed, or inserted (ADR-011, STEMMA-41). Only
    /// [`check_chain`] builds it, so `expected` and `found` always describe a link that fails.
    ChainBroken {
        /// The number of the entry whose link does not hold.
        number: u64,
        /// The hash of the entry before it, or `None` if that one was written before the chain.
        expected: Option<EntryHash>,
        /// What the entry names.
        found: Chain,
    },
    /// An entry written before the chain (version 1 or 2) follows an entry in the chain: it was
    /// written by a program that does not know the chain, or put in its place.
    UnchainedAfterChain {
        /// The number of the entry.
        number: u64,
    },
    /// The writer cannot read the entry before the one it writes, so it cannot name its hash.
    /// Nothing was written.
    Predecessor {
        /// The number of the entry before.
        number: u64,
        /// Why it cannot be read.
        source: Box<OpLogError>,
    },
    /// The directory of the log holds something that is not an entry.
    UnexpectedFile {
        /// The file or directory.
        path: PathBuf,
    },
    /// There is no entry with this number.
    Missing {
        /// The number.
        number: u64,
    },
    /// The log is full: the next number is larger than [`MAX_NUMBER`].
    Full,
    /// Every number that the writer tried was taken, so many times that it gave up. Another writer
    /// made progress each time, so trying again is reasonable.
    Contended {
        /// How many numbers were tried.
        attempts: usize,
    },
    /// The entry was written and is in the log, but its directory could not be flushed to disk, so
    /// it may not survive a crash. A caller that orders "entry, then move" must not move: the
    /// entry is not known to be safe. The entry stays; a second attempt writes another one.
    NotFlushed {
        /// The entry that is in the log.
        entry: Box<Entry>,
        /// The directory that could not be flushed.
        path: PathBuf,
        /// The error of the file system.
        source: io::Error,
    },
    /// The entry would be larger than an entry can be. Nothing was written. The actor or a name in
    /// it is too long.
    EntryTooLarge {
        /// The size of the entry in bytes.
        size: usize,
    },
    /// The change is rejected.
    Entry(EntryError),
    /// An entry is a stub, but no `purge` entry lists it: an entry was replaced outside a purge, or
    /// the `purge` entry is lost (ADR-014).
    UnlistedStub {
        /// The number of the stub.
        number: u64,
    },
    /// An entry was to become a stub, but no `purge` entry lists it. Nothing was written.
    NotListed {
        /// The number of the entry.
        number: u64,
    },
    /// A purge was to list an entry that is not a change: a stub, or the entry of a purge, which
    /// cannot become a stub. Nothing was written.
    NotAChange {
        /// The number of the entry.
        number: u64,
    },
    /// The entries of a purge were written only in part. The written ones are in the log; the
    /// stubs that they list may be written, and the count of commits must not be written again.
    PurgeIncomplete {
        /// The entries that are in the log.
        written: Vec<Entry>,
        /// The numbers of the written entries that are not known to be flushed.
        not_flushed: Vec<u64>,
        /// What failed.
        source: Box<OpLogError>,
    },
    /// Every entry of a purge is in the log, but some are not known to be flushed, so they may not
    /// survive a crash. As after [`OpLogError::NotFlushed`], no stub must be written yet.
    PurgeNotFlushed {
        /// The entries that are in the log.
        written: Vec<Entry>,
        /// The numbers of the entries that are not known to be flushed.
        not_flushed: Vec<u64>,
        /// The directory that could not be flushed.
        path: PathBuf,
        /// The first error of the file system.
        source: io::Error,
    },
}

impl OpLogError {
    fn io<'a>(
        operation: &'static str,
        path: &'a Path,
    ) -> impl FnOnce(io::Error) -> OpLogError + 'a {
        move |source| OpLogError::Io {
            operation,
            path: path.to_path_buf(),
            source,
        }
    }

    /// The error of a helper of the object store. They fail only with a failure of the file system;
    /// any other error is kept as text in an I/O error, so that no cause is dropped.
    pub(crate) fn from_store(error: StoreError) -> Self {
        match error {
            StoreError::Io {
                operation,
                path,
                source,
            } => Self::Io {
                operation,
                path,
                source,
            },
            other => Self::Io {
                operation: "use the object store",
                path: PathBuf::new(),
                source: io::Error::other(other.to_string()),
            },
        }
    }

    /// Returns the error of the file system for [`OpLogError::Io`] and
    /// [`OpLogError::NotFlushed`]. The chain of [`Error::source`] does not lead to it (see the
    /// crate documentation).
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Io { source, .. }
            | Self::NotFlushed { source, .. }
            | Self::PurgeNotFlushed { source, .. } => Some(source),
            Self::PurgeIncomplete { source, .. } | Self::Predecessor { source, .. } => {
                source.io_error()
            }
            _ => None,
        }
    }
}

/// Numbers as a message lists them: `9, 10`.
fn list(numbers: &[u64]) -> String {
    numbers
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

// A path comes from the file system, and a file name in `oplog/` can be any bytes, so a path is
// written escaped like Rust debug output: an error must not write terminal escape sequences.
impl fmt::Display for OpLogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(
                f,
                "could not {operation} `{}`: {source}",
                path.display().to_string().escape_debug()
            ),
            Self::Corrupt { path, source } => write!(
                f,
                "the entry `{}` is not in canonical form: {source}",
                path.display().to_string().escape_debug()
            ),
            Self::WrongNumber {
                path,
                expected,
                found,
            } => write!(
                f,
                "the entry `{}` has number {found} inside, but its name says {expected}",
                path.display().to_string().escape_debug()
            ),
            Self::Gap { missing } => write!(
                f,
                "the operation log has no entry {missing}, but later ones: an entry is lost"
            ),
            Self::TipAhead { path, tip, last } => write!(
                f,
                "the operation log ends at entry {last}, but the tip file names entry {tip}: \
                 the last entries are lost; restore them, or, once the loss is understood, \
                 remove `{}`, which the next entry writes again",
                path.display().to_string().escape_debug()
            ),
            Self::BadTip { path } => write!(
                f,
                "the tip file `{}` of the operation log is not in canonical form; once the \
                 damage is understood, remove it, which the next entry writes again",
                path.display().to_string().escape_debug()
            ),
            Self::TipMismatch {
                path,
                number,
                expected,
                found,
            } => {
                let found = found
                    .map_or("none, as it is from before the chain".to_owned(), |hash| {
                        hash.to_string()
                    });
                write!(
                    f,
                    "the tip file `{}` names the hash {expected} for entry {number}, but the entry \
                     has {found}: the entry or the tip file was replaced; restore it from a backup \
                     of `.stemma`, or, once the change is understood, remove the tip file",
                    path.display().to_string().escape_debug()
                )
            }
            Self::TipMissing { path, last } => write!(
                f,
                "the last entry {last} is in the hash chain, but there is no tip file `{}` to \
                 compare it with: the tip file was removed, or the program stopped right after \
                 the entry; the next entry writes the tip again",
                path.display().to_string().escape_debug()
            ),
            Self::TipBehind { path, tip, last } => write!(
                f,
                "the tip file `{}` names entry {tip}, but the log ends at entry {last}: writing \
                 the tip failed, so nothing was compared with it; the next entry writes it again",
                path.display().to_string().escape_debug()
            ),
            Self::ChainBroken {
                number,
                expected,
                found,
            } => {
                let expected = expected.map_or(
                    "`previous -`, as the entry before is from before \
                     the chain"
                        .to_owned(),
                    |hash| format!("`previous {hash}`"),
                );
                let found = match found {
                    Chain::Unchained => "no hash".to_owned(),
                    Chain::Start => "`previous -`".to_owned(),
                    Chain::After(hash) => format!("`previous {hash}`"),
                };
                write!(
                    f,
                    "entry {number} does not name the hash of the entry before it: expected \
                     {expected}, found {found}; an entry up to {number} was changed, removed, or \
                     inserted; restore the log from a backup of `.stemma`, as no command repairs \
                     the log yet"
                )
            }
            Self::UnchainedAfterChain { number } => write!(
                f,
                "entry {number} is written in the form from before the hash chain, but follows an \
                 entry in the chain; it was written by an older program or put in its place"
            ),
            Self::Predecessor { number, source } => write!(
                f,
                "the new entry cannot name the hash of entry {number}, and nothing was written: \
                 {source}"
            ),
            Self::UnexpectedFile { path } => write!(
                f,
                "`{}` is in the operation log, but is not an entry",
                path.display().to_string().escape_debug()
            ),
            Self::Missing { number } => write!(f, "the operation log has no entry {number}"),
            Self::Full => write!(
                f,
                "the operation log is full: it holds {MAX_NUMBER} entries"
            ),
            Self::Contended { attempts } => write!(
                f,
                "the operation log is busy: every one of {attempts} numbers that were tried was \
                 taken by another writer"
            ),
            Self::NotFlushed {
                entry,
                path,
                source,
            } => write!(
                f,
                "entry {} was written, but flushing `{}` failed: {source}; it may not survive a \
                 crash",
                entry.number,
                path.display().to_string().escape_debug()
            ),
            Self::EntryTooLarge { size } => write!(
                f,
                "the entry would be {size} bytes, but an entry holds at most {MAX_ENTRY_LEN}; the \
                 actor or a name in it is too long, and nothing was written"
            ),
            Self::Entry(error) => error.fmt(f),
            Self::UnlistedStub { number } => write!(
                f,
                "entry {number} is a stub, but no purge entry lists it: it was replaced outside a \
                 purge, or the purge entry is lost; restore the purge entry from a backup of \
                 `.stemma`, as no command repairs the log yet"
            ),
            Self::NotListed { number } => write!(
                f,
                "entry {number} cannot become a stub, as no purge entry lists it, and nothing was \
                 written"
            ),
            Self::NotAChange { number } => write!(
                f,
                "a purge cannot list entry {number}, as it is not a change, and nothing was \
                 written"
            ),
            Self::PurgeIncomplete {
                written,
                not_flushed,
                source,
            } => {
                let numbers: Vec<u64> = written.iter().map(|entry| entry.number).collect();
                write!(
                    f,
                    "the purge wrote only part of its entries, {}",
                    list(&numbers)
                )?;
                if !not_flushed.is_empty() {
                    write!(f, " ({} not known to be flushed)", list(not_flushed))?;
                }
                write!(f, ": {source}")
            }
            Self::PurgeNotFlushed {
                written,
                not_flushed,
                path,
                source,
            } => {
                let numbers: Vec<u64> = written.iter().map(|entry| entry.number).collect();
                write!(
                    f,
                    "the purge wrote all of its entries, {}, but flushing `{}` failed for {}: \
                     {source}; they may not survive a crash",
                    list(&numbers),
                    path.display().to_string().escape_debug(),
                    list(not_flushed)
                )
            }
        }
    }
}

impl Error for OpLogError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            // The message includes that of the I/O error, so the source is its own cause.
            Self::Io { source, .. }
            | Self::NotFlushed { source, .. }
            | Self::PurgeNotFlushed { source, .. } => source.source(),
            Self::Corrupt { source, .. } => source.source(),
            Self::Entry(error) => error.source(),
            Self::PurgeIncomplete { source, .. } | Self::Predecessor { source, .. } => {
                source.source()
            }
            _ => None,
        }
    }
}

/// The name of the tip file, next to `oplog/` in the metadata directory. It holds the number of the
/// last entry, so that a lost last entry is noticed (ADR-011). A log from before the file has none
/// until the next entry writes it.
const TIP_FILE: &str = "oplog-tip";

/// The first line of the tip file that names only the number of the last entry, as it was
/// written before the hash chain. It is still read.
const TIP_HEADER: &str = "stemma-oplog-tip 1\n";

/// The first line of the tip file that names the number and the hash of the last entry.
const TIP_HEADER_2: &str = "stemma-oplog-tip 2\n";

/// What the tip file names: the number of the last entry and, from version 2 on, its hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Tip {
    number: u64,
    hash: Option<EntryHash>,
}

/// The last entries of the log, as [`FsOpLog::window`] reads them.
#[derive(Debug)]
pub struct Window {
    entries: Vec<Entry>,
    unchecked: Option<(u64, OpLogError)>,
}

impl Window {
    /// The entries, in the order of the numbers.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The entries, in the order of the numbers.
    pub fn into_entries(self) -> Vec<Entry> {
        self.entries
    }

    /// The number of the entry before the window and why it could not be read, if it could not:
    /// then the link of the first entry to it was not checked.
    pub fn unchecked(&self) -> Option<(u64, &OpLogError)> {
        self.unchecked
            .as_ref()
            .map(|(number, error)| (*number, error))
    }
}

/// The operation log of a repository, as files below its metadata directory.
#[derive(Debug, Clone)]
pub struct FsOpLog {
    root: PathBuf,
}

impl FsOpLog {
    /// Creates a log that keeps its files below `root`, the repository's metadata directory. The
    /// directory is not checked here; every operation checks it.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn dir(&self) -> PathBuf {
        self.root.join("oplog")
    }

    fn path_of(&self, number: u64) -> PathBuf {
        self.dir().join(format!("{number:0NUMBER_WIDTH$}"))
    }

    /// Lists the names of the directory as numbers, in no order. A name that starts with a dot is
    /// skipped; any other name that is not an entry number is reported.
    fn scan(&self) -> Result<Vec<u64>, OpLogError> {
        let dir = self.dir();
        let mut numbers = Vec::new();
        for item in fault::read_dir(&dir).map_err(OpLogError::io("read the directory", &dir))? {
            let item = item.map_err(OpLogError::io("read the directory", &dir))?;
            let name = item.file_name();
            // Names with a dot: file managers and editors leave them. The first byte is looked at,
            // so a name that is not UTF-8 is skipped as well.
            if name.as_encoded_bytes().first() == Some(&b'.') {
                continue;
            }
            let number = name
                .to_str()
                .and_then(parse_file_name)
                .ok_or_else(|| OpLogError::UnexpectedFile { path: item.path() })?;
            numbers.push(number);
        }
        Ok(numbers)
    }

    /// Returns the first number that is missing from 1, 2, 3, … in the sorted numbers, if any.
    fn first_gap(sorted: &[u64]) -> Option<u64> {
        sorted
            .iter()
            .enumerate()
            .find(|(index, number)| **number != *index as u64 + 1)
            .map(|(index, _)| index as u64 + 1)
    }

    /// Lists the numbers of the entries in ascending order. They have to be 1, 2, 3, … without a
    /// gap, and the last one has to reach the tip file: a tip beyond the last entry means that
    /// the last entries are lost. A name that starts with a dot is skipped, and any other name
    /// that is not an entry number is reported.
    ///
    /// A listing is not a snapshot while a writer adds files, so a gap is reported only if a second
    /// listing shows it too. The tip is read before the first listing (see `check_tip`).
    ///
    /// # Errors
    ///
    /// Returns [`OpLogError::UnexpectedFile`] for a name that is not an entry number,
    /// [`OpLogError::Gap`] if a number is missing, [`OpLogError::TipAhead`] if the tip names an
    /// entry that the directory lacks, [`OpLogError::BadTip`] if the tip file is not in canonical
    /// form, and [`OpLogError::Io`] if the directory or the tip file cannot be read.
    pub fn numbers(&self) -> Result<Vec<u64>, OpLogError> {
        self.listing().map(|(numbers, _)| numbers)
    }

    /// The numbers, as [`numbers`](Self::numbers) lists them, and the tip that was read before.
    fn listing(&self) -> Result<(Vec<u64>, Option<Tip>), OpLogError> {
        let tip = self.read_tip()?;
        let mut numbers = self.scan()?;
        numbers.sort_unstable();
        if Self::first_gap(&numbers).is_some() {
            numbers = self.scan()?;
            numbers.sort_unstable();
            if let Some(missing) = Self::first_gap(&numbers) {
                return Err(OpLogError::Gap { missing });
            }
        }
        self.check_tip(tip, numbers.last().copied().unwrap_or(0))?;
        Ok((numbers, tip))
    }

    fn tip_path(&self) -> PathBuf {
        self.root.join(TIP_FILE)
    }

    /// Reads the number of the last entry, and its hash from version 2 on, from the tip file. A
    /// missing file is `None`: a log without a tip yet (a repository made before the tip file, or a
    /// log that was never written).
    ///
    /// # Errors
    ///
    /// Returns [`OpLogError::BadTip`] if the file is not in canonical form, and
    /// [`OpLogError::Io`] if it cannot be read.
    fn read_tip(&self) -> Result<Option<Tip>, OpLogError> {
        let path = self.tip_path();
        let bytes = match fault::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(OpLogError::io("read the tip file", &path)(error)),
        };
        let parse = |text: &str| -> Option<Tip> {
            let (rest, chained) = match text.strip_prefix(TIP_HEADER) {
                Some(rest) => (rest, false),
                None => (text.strip_prefix(TIP_HEADER_2)?, true),
            };
            let (digits, rest) = rest.strip_prefix("number ")?.split_once('\n')?;
            let canonical = !digits.is_empty()
                && digits.bytes().all(|b| b.is_ascii_digit())
                && !(digits.len() > 1 && digits.starts_with('0'));
            let number = digits
                .parse::<u64>()
                .ok()
                .filter(|number| canonical && (1..=MAX_NUMBER).contains(number))?;
            let hash = if chained {
                let hash = rest.strip_prefix("hash ")?.strip_suffix('\n')?;
                Some(EntryHash::from_str(hash).ok()?)
            } else if rest.is_empty() {
                None
            } else {
                return None;
            };
            Some(Tip { number, hash })
        };
        let tip = std::str::from_utf8(&bytes).ok().and_then(parse);
        tip.map(Some).ok_or(OpLogError::BadTip { path })
    }

    /// Replaces the tip file with the number and the hash of the entry: a temporary file that is
    /// flushed, then a rename. An entry written before the chain gets a tip of version 1, which
    /// names no hash. The directory is flushed on a best effort basis, because a tip that is not on
    /// disk after a crash is one behind or missing, which the writer accepts; the next entry writes
    /// it again.
    fn write_tip(&self, entry: &Entry) -> Result<(), OpLogError> {
        let temp_dir = self.root.join("tmp");
        let (mut file, mut temp) =
            create_temp(&temp_dir, &mut unique_temp_name).map_err(OpLogError::from_store)?;
        let content = match entry.hash() {
            Some(hash) => format!("{TIP_HEADER_2}number {}\nhash {hash}\n", entry.number),
            None => format!("{TIP_HEADER}number {}\n", entry.number),
        };
        file.write_all(content.as_bytes())
            .map_err(OpLogError::io("write the temporary file", &temp.path))?;
        file.sync_all()
            .map_err(OpLogError::io("flush the temporary file", &temp.path))?;
        drop(file);
        let path = self.tip_path();
        fault::rename(&temp.path, &path).map_err(OpLogError::io("replace the tip file", &path))?;
        // The name is gone after the rename.
        temp.disarm();
        let _ = sync_directory(&self.root);
        Ok(())
    }

    /// Fails with [`OpLogError::TipMismatch`] if the tip names a hash and `entry`, the entry with
    /// its number, has another one. A tip of version 1 names no hash; it is the tip of an entry
    /// written before the chain, and naming an entry in the chain it is [`OpLogError::BadTip`],
    /// as a writer of the chain writes version 2.
    fn check_tip_hash(&self, tip: Tip, entry: &Entry) -> Result<(), OpLogError> {
        match (tip.hash, entry.hash()) {
            (Some(expected), found) if found != Some(expected) => Err(OpLogError::TipMismatch {
                path: self.tip_path(),
                number: tip.number,
                expected,
                found,
            }),
            (None, Some(_)) => Err(OpLogError::BadTip {
                path: self.tip_path(),
            }),
            _ => Ok(()),
        }
    }

    /// Fails with [`OpLogError::TipAhead`] if the tip names an entry beyond the largest number in
    /// the directory. A tip one behind is no fault: a stop between the entry and the tip.
    ///
    /// The caller reads the tip *before* it lists the directory. A writer links entry *n* before
    /// it writes tip *n*, so a tip that was read first is covered by the listing that follows,
    /// unless the entry is really lost. In the other order a reader could see the listing without
    /// entry *n* and the tip with it, and report a loss that did not happen.
    fn check_tip(&self, tip: Option<Tip>, last: u64) -> Result<(), OpLogError> {
        match tip {
            Some(tip) if tip.number > last => Err(OpLogError::TipAhead {
                path: self.tip_path(),
                tip: tip.number,
                last,
            }),
            _ => Ok(()),
        }
    }

    /// The number after the largest one in the directory, or 1. It does not look at the tip or at
    /// gaps; it serves the retry of a writer that [`append`](Self::append) has checked already.
    fn next_number(&self) -> Result<u64, OpLogError> {
        Ok(self.scan()?.into_iter().max().map_or(1, |last| last + 1))
    }

    /// Reads the entry with the number.
    ///
    /// # Errors
    ///
    /// Returns [`OpLogError::Missing`] if there is none, [`OpLogError::Corrupt`] if it is not in
    /// canonical form or larger than an entry can be, [`OpLogError::WrongNumber`] if the number
    /// inside is not the one of the name, and [`OpLogError::Io`] if the storage fails.
    pub fn read(&self, number: u64) -> Result<Entry, OpLogError> {
        let path = self.path_of(number);
        let file = match fault::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(OpLogError::Missing { number });
            }
            Err(error) => return Err(OpLogError::io("open the entry", &path)(error)),
        };
        // An entry is small. A file that is much larger is not one, and is not read in full.
        let mut bytes = Vec::new();
        file.take(MAX_ENTRY_LEN + 1)
            .read_to_end(&mut bytes)
            .map_err(OpLogError::io("read the entry", &path))?;
        if bytes.len() as u64 > MAX_ENTRY_LEN {
            return Err(OpLogError::Corrupt {
                path,
                source: EntryParseError::Malformed {
                    line: 1,
                    reason: EntryMalformed::TooLong,
                },
            });
        }
        let entry = Entry::parse(&bytes).map_err(|source| OpLogError::Corrupt {
            path: path.clone(),
            source,
        })?;
        if entry.number != number {
            return Err(OpLogError::WrongNumber {
                path,
                expected: number,
                found: entry.number,
            });
        }
        Ok(entry)
    }

    /// Reads every entry, in the order of the numbers, and checks the whole hash chain (see
    /// [`check_chain`]) and the hash that the tip file names.
    ///
    /// # Errors
    ///
    /// See [`numbers`](Self::numbers) and [`read`](Self::read), [`OpLogError::UnlistedStub`] for
    /// a stub that no `purge` entry lists, what [`check_chain`] returns, and
    /// [`OpLogError::TipMismatch`] if the tip names another hash than its entry has.
    pub fn entries(&self) -> Result<Vec<Entry>, OpLogError> {
        self.tail(usize::MAX)
    }

    /// The entries of [`window`](Self::window), for the callers in this crate that need not warn
    /// about the link before it.
    ///
    /// # Errors
    ///
    /// See [`entries`](Self::entries).
    pub(crate) fn tail(&self, count: usize) -> Result<Vec<Entry>, OpLogError> {
        self.window(count).map(Window::into_entries)
    }

    /// Reads the last `count` entries, in the order of the numbers, and checks their stubs (see
    /// [`check_stubs`]). The entry of a purge comes after the stubs that it lists, so a window at
    /// the end of the log holds it.
    ///
    /// It checks the hash chain of the window and the link of its first entry to the one before
    /// (see [`check_chain`]), which it reads too, and the hash that the tip file names if the tip
    /// is in the window or just before it. If the entry before cannot be read, the window is
    /// returned all the same, with the error ([`Window::unchecked`]), so that a caller can warn:
    /// then neither that link nor a tip that names that entry is checked. A change before the
    /// window is found only by reading the whole log ([`entries`](Self::entries)).
    ///
    /// A reader takes no lock, and a purge writes its entry first and the stubs after it. A reader
    /// that listed the numbers before the purge entry and read an entry after it became a stub
    /// sees a stub that nothing lists. So a stub that nothing lists is looked at once more: the
    /// numbers are listed again and the entries read again, and only a second finding is an error,
    /// as for a gap (see [`numbers`](Self::numbers)). The second reading covers one purge: if
    /// another purge writes its entry and a stub while it runs, it too can see a stub that nothing
    /// lists, and then the error is reported falsely. Running the command again reads the log
    /// whole.
    ///
    /// # Errors
    ///
    /// See [`entries`](Self::entries).
    pub fn window(&self, count: usize) -> Result<Window, OpLogError> {
        // Reads the window and the entry before it, and returns them with the number of entries
        // in front of the window, 1 or 0, and why the entry before could not be read.
        type Read = (Vec<Entry>, usize, Option<(u64, OpLogError)>);
        let read = || -> Result<Read, OpLogError> {
            let (numbers, tip) = self.listing()?;
            let window = numbers.len().saturating_sub(count);
            let mut entries = numbers[window..]
                .iter()
                .map(|number| self.read(*number))
                .collect::<Result<Vec<_>, _>>()?;
            // The entry before the window is read for the link only: if it cannot be read, the
            // window is shown all the same, as a limit spares the entries it does not show.
            let mut unchecked = None;
            let mut from = 0;
            if let Some(index) = window.checked_sub(1) {
                match self.read(numbers[index]) {
                    Ok(before) => {
                        entries.insert(0, before);
                        from = 1;
                    }
                    Err(error) => unchecked = Some((numbers[index], error)),
                }
            }
            check_chain(&entries)?;
            // A stub keeps the hash of its entry, so the tip holds while a purge runs.
            if let Some(tip) = tip {
                if let Some(entry) = entries.iter().find(|entry| entry.number == tip.number) {
                    self.check_tip_hash(tip, entry)?;
                }
            }
            Ok((entries, from, unchecked))
        };
        let (mut entries, mut before, mut unchecked) = read()?;
        if let Err(OpLogError::UnlistedStub { .. }) = check_stubs(&entries) {
            (entries, before, unchecked) = read()?;
            check_stubs(&entries)?;
        }
        entries.drain(..before);
        Ok(Window { entries, unchecked })
    }

    /// Reads every entry as [`entries`](Self::entries) does, and checks the tip file strictly:
    /// if the last entry is in the chain, the tip file has to name it or the one before it. The
    /// readers and the writers accept a tip that is missing or further behind, as a stop or a
    /// failure to write it leaves it so; a check of the whole log reports it, as the writers did
    /// not compare the last entries with it.
    ///
    /// # Errors
    ///
    /// See [`entries`](Self::entries), [`OpLogError::TipMissing`] if there is no tip file, and
    /// [`OpLogError::TipBehind`] if it names an entry more than one before the last.
    pub fn verify(&self) -> Result<Vec<Entry>, OpLogError> {
        let entries = self.entries()?;
        // The tip is read after the entries: a writer only moves it forward, so it is not behind
        // because a writer added entries in between.
        let tip = self.read_tip()?;
        if let Some(last) = entries.last().filter(|entry| entry.hash().is_some()) {
            match tip {
                None => {
                    return Err(OpLogError::TipMissing {
                        path: self.tip_path(),
                        last: last.number,
                    })
                }
                Some(tip) if tip.number + 1 < last.number => {
                    return Err(OpLogError::TipBehind {
                        path: self.tip_path(),
                        tip: tip.number,
                        last: last.number,
                    })
                }
                Some(_) => {}
            }
        }
        Ok(entries)
    }

    /// Writes a new entry for the change under the next free number, flushes it, and returns it.
    /// The entry is whole when it can first be seen: it is written to a temporary file, flushed,
    /// and published with a hard link, which fails if the number is taken. If it is taken, another
    /// writer was faster, and the end of the log is looked up again, and the entry is built again
    /// with the hash of the new entry before it.
    ///
    /// `Repository::update_branch` and `set_head` call this in the order of ADR-011, under the lock
    /// of the references: the entry first, then the move of the pointer. `init` calls it for the
    /// first entry, in a directory that no other writer can see yet.
    ///
    /// Before it takes a number, the writer reads the tip file and refuses if the tip is beyond the
    /// largest number in the directory ([`OpLogError::TipAhead`]), because the number of a lost
    /// last entry would go to another move. A gap in the middle does not stop it: a refusal would
    /// block the move that repairs the log. After the entry is published, the writer replaces the
    /// tip file. This is best effort: a tip that stays behind is accepted, and the next entry
    /// writes it again. After [`OpLogError::NotFlushed`] the tip is not advanced, as the entry may
    /// not survive a crash and a tip that did would then refuse every later writer.
    ///
    /// # Errors
    ///
    /// Returns [`OpLogError::TipAhead`], [`OpLogError::BadTip`], and [`OpLogError::TipMismatch`]
    /// (nothing is written), [`OpLogError::Predecessor`] if the entry before the new one or the
    /// entry that the tip names next to the end cannot be read, or if the entry before is out of
    /// place (nothing is written), [`OpLogError::EntryTooLarge`] if the entry would be larger than
    /// an entry can be (nothing is written), [`OpLogError::Full`] if the log is full,
    /// [`OpLogError::Contended`] if every number that was tried was taken,
    /// [`OpLogError::NotFlushed`] if the entry is in place but not known to be flushed,
    /// [`OpLogError::UnexpectedFile`] for a name in the directory that is not an entry, and
    /// [`OpLogError::Io`] if the storage fails, among others if the file system has no hard links,
    /// or if the directory of the log does not exist.
    pub(crate) fn append(&self, change: &Change) -> Result<Entry, OpLogError> {
        self.append_record(&Record::Change(change.clone()))
    }

    /// Writes the entry of a lease, as [`append`](Self::append) writes the entry of a change. The
    /// lease entry names no pointer, so the caller writes it before it replaces the lease file
    /// and holds the lock of the references (ADR-004, ADR-011).
    ///
    /// # Errors
    ///
    /// See [`append`](Self::append).
    pub(crate) fn append_lease(&self, lease: &LeaseEntry) -> Result<Entry, OpLogError> {
        self.append_record(&Record::Lease(lease.clone()))
    }

    /// Writes the entry of a pull request, as [`append`](Self::append) writes the entry of a
    /// change. The entry names no pointer, so the caller holds the lock of the references while it
    /// checks the state of the pull request and writes the entry (ADR-004, ADR-011).
    ///
    /// # Errors
    ///
    /// See [`append`](Self::append).
    pub(crate) fn append_pull(&self, pull: &PullEntry) -> Result<Entry, OpLogError> {
        self.append_record(&Record::Pull(pull.clone()))
    }

    /// Writes the entry of a check, as [`append`](Self::append) writes the entry of a change. The
    /// entry names no pointer, so the caller holds the lock of the references (ADR-019).
    ///
    /// # Errors
    ///
    /// See [`append`](Self::append).
    pub(crate) fn append_check(&self, check: &CheckEntry) -> Result<Entry, OpLogError> {
        self.append_record(&Record::Check(check.clone()))
    }

    /// Writes the entry that pins the key of a hub again, as [`append`](Self::append) writes the
    /// entry of a change. The entry names no pointer, so the caller holds the lock of the
    /// references while it reads and replaces the configuration of the remote (ADR-026).
    ///
    /// # Errors
    ///
    /// See [`append`](Self::append).
    pub(crate) fn append_remote_pin(&self, pin: &RemotePinEntry) -> Result<Entry, OpLogError> {
        self.append_record(&Record::RemotePin(pin.clone()))
    }

    /// Writes the entries of a purge, as [`append`](Self::append) writes an entry, and returns
    /// them. A list of stubs that does not fit into one entry is split over several, and the first
    /// has the count of commits (ADR-014). The entries go after the ones that they list. Before
    /// anything is written, every listed entry is read, and one that is not a change is refused:
    /// a purge that listed it could never finish, as the entry of a purge cannot become a stub. A
    /// part that is in place but not known to be flushed does not stop the others: every part is
    /// written, so that no stub misses its listing.
    ///
    /// # Errors
    ///
    /// Returns what [`read`](Self::read) returns for a listed entry and
    /// [`OpLogError::NotAChange`] for one that is not a change; nothing is written. See
    /// [`append`](Self::append) for a failure of the first part, which leaves nothing. A later
    /// failure is [`OpLogError::PurgeIncomplete`], which carries the entries that are in the log;
    /// a retry must not write the count again. If every part is in place but some are not known to
    /// be flushed, the error is [`OpLogError::NotFlushed`] for a single part and
    /// [`OpLogError::PurgeNotFlushed`] for several.
    pub(crate) fn append_purge(&self, purge: Purge) -> Result<Vec<Entry>, OpLogError> {
        for (start, end) in purge.stubs() {
            for number in *start..=*end {
                let entry = self.read(number)?;
                if entry.change().is_none() && entry.pull().is_none() && entry.check().is_none() {
                    return Err(OpLogError::NotAChange { number });
                }
            }
        }
        let mut written = Vec::new();
        let mut not_flushed = Vec::new();
        // The first failure to flush: the directory and the error of the file system.
        let mut flush_error = None;
        for part in purge.split() {
            match self.append_record(&Record::Purge(part)) {
                Ok(entry) => written.push(entry),
                Err(OpLogError::NotFlushed {
                    entry,
                    path,
                    source,
                }) => {
                    not_flushed.push(entry.number);
                    written.push(*entry);
                    flush_error.get_or_insert((path, source));
                }
                Err(error) if written.is_empty() => return Err(error),
                Err(error) => {
                    return Err(OpLogError::PurgeIncomplete {
                        written,
                        not_flushed,
                        source: Box::new(error),
                    })
                }
            }
        }
        match flush_error {
            None => Ok(written),
            Some((path, source)) if written.len() == 1 => Err(OpLogError::NotFlushed {
                entry: Box::new(written.remove(0)),
                path,
                source,
            }),
            Some((path, source)) => Err(OpLogError::PurgeNotFlushed {
                written,
                not_flushed,
                path,
                source,
            }),
        }
    }

    /// [`replace_with_stub_listed`](Self::replace_with_stub_listed) with the `purge` entries that
    /// follow the entry, read from the log. Each call lists the log and reads the later entries,
    /// so a purge passes the entries it holds instead.
    ///
    /// # Errors
    ///
    /// Returns what [`numbers`](Self::numbers) and [`read`](Self::read) return for a later entry,
    /// and what [`replace_with_stub_listed`](Self::replace_with_stub_listed) returns.
    #[cfg(test)]
    pub(crate) fn replace_with_stub(&self, number: u64) -> Result<Entry, OpLogError> {
        // A later entry that cannot be read is an error of its own: it may be the purge entry.
        let mut purges = Vec::new();
        for later in self.numbers()?.into_iter().filter(|later| *later > number) {
            let entry = self.read(later)?;
            if matches!(entry.record(), Record::Purge(_)) {
                purges.push(entry);
            }
        }
        self.replace_with_stub_listed(number, &purges)
    }

    /// Replaces the entry by its stub (ADR-014): the stub is written to a temporary file in `tmp/`,
    /// flushed, and renamed over the entry, so a reader sees the entry or the stub and never a
    /// mix; then the directory is flushed. A stub is replaced by itself, so the call can be
    /// repeated. It is called after the `purge` entry that lists the number is written; a stub
    /// that no `purge` entry lists is an error of the log.
    ///
    /// `purges` are entries of this log, read from it or written to it under the lock that the
    /// caller still holds; one of them has to be a `purge` entry after the entry that lists it.
    /// The log is not listed again, so a purge with many stubs reads each entry once.
    ///
    /// # Errors
    ///
    /// Returns what [`read`](Self::read) returns for the entry, [`OpLogError::NotListed`] if no
    /// `purge` entry of `purges` lists the number (nothing is written, as the stub would make the
    /// log unreadable), [`OpLogError::Entry`] with [`EntryError::PurgeNotReplaceable`] for the
    /// entry of a purge, [`OpLogError::Io`] if the storage fails before the stub is in place, and
    /// [`OpLogError::NotFlushed`] with the stub if it is in place but the directory could not be
    /// flushed; a repeated call is harmless.
    pub(crate) fn replace_with_stub_listed(
        &self,
        number: u64,
        purges: &[Entry],
    ) -> Result<Entry, OpLogError> {
        let stub = self.read(number)?.to_stub().map_err(OpLogError::Entry)?;
        let listed = purges.iter().any(|entry| {
            entry.number() > number
                && matches!(entry.record(), Record::Purge(purge) if purge.lists(number))
        });
        if !listed {
            return Err(OpLogError::NotListed { number });
        }
        let temp_dir = self.root.join("tmp");
        let (mut file, mut temp) =
            create_temp(&temp_dir, &mut unique_temp_name).map_err(OpLogError::from_store)?;
        file.write_all(&stub.encode())
            .map_err(OpLogError::io("write the temporary file", &temp.path))?;
        file.sync_all()
            .map_err(OpLogError::io("flush the temporary file", &temp.path))?;
        drop(file);
        let path = self.path_of(number);
        fault::rename(&temp.path, &path).map_err(OpLogError::io("replace the entry", &path))?;
        temp.disarm();
        let dir = self.dir();
        match sync_directory(&dir) {
            Ok(()) => Ok(stub),
            Err(error) => Err(OpLogError::NotFlushed {
                entry: Box::new(stub),
                path: dir,
                source: match error {
                    StoreError::Io { source, .. } => source,
                    other => io::Error::other(other.to_string()),
                },
            }),
        }
    }

    /// Writes an entry of any record; see [`append`](Self::append).
    fn append_record(&self, record: &Record) -> Result<Entry, OpLogError> {
        // The tip is read first, as in `numbers`.
        let tip = self.read_tip()?;
        let last = self.scan()?.into_iter().max().unwrap_or(0);
        // A lost last entry must not hand its number to another move.
        self.check_tip(tip, last)?;
        // A replaced last entry must not be named by the next one, which would take it into the
        // chain. A tip that is behind is compared with the entry it names.
        if let Some(tip) = tip {
            match self.read(tip.number) {
                Ok(entry) => self.check_tip_hash(tip, &entry)?,
                // A damaged or missing entry in the middle does not stop a writer, as a gap does
                // not; the reader reports it. A failure of the storage does.
                Err(ref error)
                    if tip.number + 1 < last && !matches!(error, OpLogError::Io { .. }) => {}
                Err(source) => {
                    return Err(OpLogError::Predecessor {
                        number: tip.number,
                        source: Box::new(source),
                    })
                }
            }
        }
        let result = self.publish(
            record,
            last + 1,
            &mut |_| {},
            &|from, to| fault::hard_link(from, to),
            &|dir| match sync_directory(dir) {
                Ok(()) => Ok(()),
                Err(StoreError::Io { source, .. }) => Err(source),
                Err(other) => Err(io::Error::other(other.to_string())),
            },
        );
        // Only a flushed entry advances the tip. A failure to write it is no error of the move: the
        // entry is in place, and the tip is one behind, which the next writer accepts.
        if let Ok(entry) = &result {
            let _ = self.write_tip(entry);
        }
        result
    }

    /// The place in the chain of a new entry with the number: the start for entry 1 and after an
    /// entry written before the chain, and otherwise after the hash of the entry before it.
    ///
    /// # Errors
    ///
    /// Returns [`OpLogError::Predecessor`] if the entry before cannot be read, or if it was written
    /// before the chain and the entry before it, if it can be read, is in the chain, so that the
    /// new entry would start the chain again behind an entry that does not belong there
    /// ([`OpLogError::UnchainedAfterChain`]).
    fn link_to(&self, number: u64) -> Result<Chain, OpLogError> {
        if number <= 1 {
            return Ok(Chain::Start);
        }
        let predecessor = |number: u64| {
            move |source| OpLogError::Predecessor {
                number,
                source: Box::new(source),
            }
        };
        let before = self.read(number - 1).map_err(predecessor(number - 1))?;
        if let Some(hash) = before.hash() {
            return Ok(Chain::After(hash));
        }
        // The entry before that is read for this check only: if it cannot be read, the reader
        // reports it, and the writer does not stop at a damage away from the end.
        let chained = |number| {
            self.read(number)
                .ok()
                .and_then(|entry| entry.hash())
                .is_some()
        };
        if number > 2 && chained(number - 2) {
            return Err(predecessor(number - 1)(OpLogError::UnchainedAfterChain {
                number: number - 1,
            }));
        }
        Ok(Chain::Start)
    }

    /// The loop of [`append`](Self::append), with the steps that a test has to be able to change:
    /// `before_link` is called with the number that is about to be tried, after the temporary file
    /// is ready and before it is linked; `link` publishes the temporary file under the name of the
    /// entry and has to fail with [`io::ErrorKind::AlreadyExists`] if the name is taken; `sync`
    /// flushes a directory.
    fn publish(
        &self,
        record: &Record,
        start: u64,
        before_link: &mut dyn FnMut(u64),
        link: &dyn Fn(&Path, &Path) -> io::Result<()>,
        sync: &dyn Fn(&Path) -> io::Result<()>,
    ) -> Result<Entry, OpLogError> {
        let dir = self.dir();
        let temp_dir = self.root.join("tmp");
        let mut number = start;
        for _ in 0..ATTEMPTS {
            if number > MAX_NUMBER {
                return Err(OpLogError::Full);
            }
            // The entry names the hash of the one before it, which is new after a lost race.
            let chain = self.link_to(number)?;
            let entry = match Entry::linked(number, chain, record.clone()) {
                Ok(entry) => entry,
                Err(EntryError::NumberOutOfRange(_)) => return Err(OpLogError::Full),
                Err(other) => return Err(OpLogError::Entry(other)),
            };
            let encoded = entry.encode();
            // The reader refuses a file larger than this, so the writer must not make one.
            if encoded.len() as u64 > MAX_ENTRY_LEN {
                return Err(OpLogError::EntryTooLarge {
                    size: encoded.len(),
                });
            }
            let (mut file, temp) =
                create_temp(&temp_dir, &mut unique_temp_name).map_err(OpLogError::from_store)?;
            file.write_all(&encoded)
                .map_err(OpLogError::io("write the temporary file", &temp.path))?;
            file.sync_all()
                .map_err(OpLogError::io("flush the temporary file", &temp.path))?;
            drop(file);
            before_link(number);
            let path = self.path_of(number);
            match link(&temp.path, &path) {
                Ok(()) => {
                    // The entry is in place under its own name. The temporary name is removed when
                    // `temp` is dropped, on a best effort basis: a name that stays in `tmp/` is
                    // scratch and harmless, and it must not stop a move whose entry is whole.
                    drop(temp);
                    return match sync(&dir) {
                        Ok(()) => Ok(entry),
                        Err(source) => Err(OpLogError::NotFlushed {
                            entry: Box::new(entry),
                            path: dir,
                            source,
                        }),
                    };
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    // Another writer was faster. Look again for the end of the log, which can be
                    // several entries further on, instead of stepping by one.
                    drop(temp);
                    number = self.next_number()?.max(number + 1);
                }
                Err(source) => {
                    return Err(OpLogError::Io {
                        operation: "publish the entry with a hard link",
                        path,
                        source,
                    });
                }
            }
        }
        Err(OpLogError::Contended { attempts: ATTEMPTS })
    }
}

/// Checks that a `purge` entry lists every stub (ADR-014). A listed number that is not a stub yet
/// is no error: the purge writes its entry before the stubs. `entries` must be the end of the log,
/// from some entry to the last: a purge entry comes after the stubs that it lists, so it is among
/// them.
///
/// # Errors
///
/// Returns [`OpLogError::UnlistedStub`] for the first stub that no `purge` entry lists.
pub fn check_stubs(entries: &[Entry]) -> Result<(), OpLogError> {
    let purges: Vec<&Purge> = entries
        .iter()
        .filter_map(|entry| match entry.record() {
            Record::Purge(purge) => Some(purge),
            _ => None,
        })
        .collect();
    match entries.iter().find(|entry| {
        matches!(entry.record(), Record::Stub(_))
            && !purges.iter().any(|purge| purge.lists(entry.number()))
    }) {
        Some(entry) => Err(OpLogError::UnlistedStub {
            number: entry.number(),
        }),
        None => Ok(()),
    }
}

/// Checks the hash chain of `entries`, which must ascend without a gap (ADR-011, STEMMA-41).
///
/// Each entry is linked to the one before it in `entries`: an entry in the chain names its hash, or
/// starts the chain if the one before was written before the chain. Entry 1 starts the chain or was
/// written before it, which [`Entry::parse`] checks. The first entry of `entries` is not linked to
/// anything else, so a caller that reads a window passes the entry before it as well. A stub has
/// the hash of the entry it replaced, so a purge does not break the chain; that a `purge` entry
/// lists the stub is checked by [`check_stubs`].
///
/// The chain shows that the entries agree with one another, not who wrote them: whoever can
/// write `.stemma` can compute every later hash and the tip again. The entries before the
/// first entry of the chain are not covered.
///
/// # Errors
///
/// Returns [`OpLogError::ChainBroken`] for the first entry that does not name the hash of the
/// one before it or starts the chain again, and [`OpLogError::UnchainedAfterChain`] for an entry
/// written before the chain that follows an entry in the chain.
pub fn check_chain(entries: &[Entry]) -> Result<(), OpLogError> {
    debug_assert!(
        entries
            .windows(2)
            .all(|pair| pair[0].number + 1 == pair[1].number),
        "the entries must ascend without a gap"
    );
    for pair in entries.windows(2) {
        let (before, entry) = (&pair[0], &pair[1]);
        let expected = before.hash();
        let holds = match (entry.chain, expected) {
            (Chain::Unchained, Some(_)) => {
                return Err(OpLogError::UnchainedAfterChain {
                    number: entry.number,
                })
            }
            (Chain::Unchained, None) | (Chain::Start, None) => true,
            (Chain::Start, Some(_)) | (Chain::After(_), None) => false,
            (Chain::After(hash), Some(expected)) => hash == expected,
        };
        if !holds {
            return Err(OpLogError::ChainBroken {
                number: entry.number,
                expected,
                found: entry.chain,
            });
        }
    }
    Ok(())
}

/// The number of the first entry in the hash chain, or `None` if no entry is in it yet. The
/// entries before it were written before the chain and are not covered by it. `entries` is the
/// whole log, from entry 1, as [`FsOpLog::entries`] reads it: in a window, the first entry in the
/// chain is not where it starts.
pub fn chain_start(entries: &[Entry]) -> Option<u64> {
    debug_assert!(
        entries.first().is_none_or(|entry| entry.number == 1),
        "the entries must begin at entry 1"
    );
    entries
        .iter()
        .find(|entry| entry.chain != Chain::Unchained)
        .map(Entry::number)
}

/// The numbers of the changes that a `purge` entry lists: a purge stopped before it replaced them
/// by stubs, after a crash for instance (ADR-014). Such an entry still names what the purge
/// removes, and it is about to become a stub.
pub fn pending_stubs(entries: &[Entry]) -> Vec<u64> {
    let purges: Vec<&Purge> = entries
        .iter()
        .filter_map(|entry| match entry.record() {
            Record::Purge(purge) => Some(purge),
            _ => None,
        })
        .collect();
    entries
        .iter()
        .filter(|entry| matches!(entry.record(), Record::Change(_)))
        .filter(|entry| purges.iter().any(|purge| purge.lists(entry.number())))
        .map(Entry::number)
        .collect()
}

/// Reads the name of a file of the log: exactly [`NUMBER_WIDTH`] decimal digits, from 1.
fn parse_file_name(name: &str) -> Option<u64> {
    if name.len() != NUMBER_WIDTH || !name.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    name.parse().ok().filter(|number| *number >= 1)
}

/// Whether the move of an entry took effect. An entry is written before the pointer moves, so
/// an entry can name a move that never happened (ADR-011).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Every pointer of the entry moved to its new value, as far as the log and the references
    /// show.
    Took,
    /// At least one pointer of the entry did not move to its new value.
    NotTaken,
    /// The log and the references do not decide it.
    Unknown,
}

/// The values that the pointers have now, for [`outcomes`]. The working-copy commit is part of it
/// only if [`Current::with_working_copy`] gave it; without it the outcome of its last move is
/// unknown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Current {
    head: Head,
    working_copy: WorkingCopy,
    branches: BTreeMap<BranchName, CommitId>,
    /// The tags that exist: `None` until [`Current::with_tags`] gave them, so that the outcome of
    /// an entry that made or deleted a tag stays unknown; `Some` of an empty map means that no tag
    /// exists.
    tags: Option<BTreeMap<BranchName, CommitId>>,
    /// The remote references that exist: `None` until [`Current::with_remotes`] gave them, so
    /// that the outcome of an entry that moved one stays unknown.
    remotes: Option<BTreeMap<RemoteRefName, RemoteRef>>,
    /// The owner of the working copy: `None` if [`Current::with_owner`] was not called, and
    /// `Some(None)` for a working copy without an owner.
    owner: Option<Option<Actor>>,
    /// The worktree whose HEAD, working-copy commit, and owner these are.
    worktree: String,
    settled: bool,
}

/// What [`Current`] knows about the working-copy commit.
#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkingCopy {
    /// [`Current::with_working_copy`] was not called.
    Unknown,
    /// The working-copy commit does not exist.
    Absent,
    /// The working-copy commit is at the commit.
    At(CommitId),
}

impl Current {
    /// The value of HEAD, and the branches that exist among those that the entries name: a
    /// branch that is not given does not exist. `settled` is false if a writer may be between its
    /// entry and its move (it holds the lock of the references), so that a pointer which still has
    /// its old value is not a move that failed.
    pub fn new(
        head: Head,
        branches: impl IntoIterator<Item = (BranchName, CommitId)>,
        settled: bool,
    ) -> Self {
        Self {
            head,
            working_copy: WorkingCopy::Unknown,
            branches: branches.into_iter().collect(),
            tags: None,
            remotes: None,
            owner: None,
            worktree: MAIN_WORKTREE.to_owned(),
            settled,
        }
    }

    /// Names the worktree that HEAD, the working-copy commit, and the owner belong to; the default
    /// is `main`. An entry of another worktree is judged by the entry of the same
    /// worktree that follows it: that entry confirms or refutes its HEAD and working-copy moves.
    /// The newest entry of another worktree has no later entry and no known current value here,
    /// so those moves are unknown. Branches and tags are shared and judged as before (ADR-011).
    #[must_use]
    pub fn in_worktree(mut self, worktree: impl Into<String>) -> Self {
        self.worktree = worktree.into();
        self
    }

    /// Adds the value of the working-copy commit, `None` if it does not exist. Without it, the
    /// last move of the working-copy commit in the log stays unknown.
    #[must_use]
    pub fn with_working_copy(mut self, working_copy: Option<CommitId>) -> Self {
        self.working_copy = working_copy.map_or(WorkingCopy::Absent, WorkingCopy::At);
        self
    }

    /// Adds the tags that exist. Without them, the outcome of an entry that made or deleted a tag
    /// stays unknown.
    #[must_use]
    pub fn with_tags(mut self, tags: impl IntoIterator<Item = (BranchName, CommitId)>) -> Self {
        self.tags = Some(tags.into_iter().collect());
        self
    }

    /// Adds the remote references that exist (ADR-008). Without them, the outcome of an entry
    /// that moved a remote reference stays unknown.
    #[must_use]
    pub fn with_remotes(
        mut self,
        remotes: impl IntoIterator<Item = (RemoteRefName, RemoteRef)>,
    ) -> Self {
        self.remotes = Some(remotes.into_iter().collect());
        self
    }

    /// Adds the owner of the working copy, `None` for a working copy without one (ADR-010).
    /// Without it, the newest entry of `take-over` stays unknown.
    #[must_use]
    pub fn with_owner(mut self, owner: Option<Actor>) -> Self {
        self.owner = Some(owner);
        self
    }
}

/// What is known about a pointer now, for [`judge`].
#[derive(Clone, Copy)]
enum Now<'a, T> {
    /// The value cannot be read, or `current` does not hold the working-copy commit.
    Unknown,
    /// The pointer does not exist.
    Absent,
    /// The pointer has this value.
    Is(&'a T),
}

/// The outcome of one pointer. `next` is the move of the same pointer in the next entry that
/// changes it, if there is one; `now` is the value the pointer has now; `settled` is false if a
/// writer may be between its entry and its move.
///
/// "Did not take effect" is the strongest statement of the log, so it is made only on clear
/// evidence: the next move starts from the old value of this one, or the pointer has its old value
/// now and nobody is writing. Any other value is a change outside the log, and the answer is
/// unknown.
fn judge<T: PartialEq>(
    moved: &Moved<T>,
    next: Option<&Moved<T>>,
    now: Now<'_, T>,
    settled: bool,
) -> Outcome {
    if let Some(next) = next {
        return if next.old == moved.new {
            Outcome::Took
        } else if next.old == moved.old {
            Outcome::NotTaken
        } else {
            Outcome::Unknown
        };
    }
    let now = match now {
        Now::Unknown => return Outcome::Unknown,
        Now::Absent => None,
        Now::Is(value) => Some(value),
    };
    if now == moved.new.as_ref() {
        Outcome::Took
    } else if now == moved.old.as_ref() && settled {
        Outcome::NotTaken
    } else {
        Outcome::Unknown
    }
}

/// Reads from the log whether the move of each entry took effect, in the order of `entries`.
///
/// An entry is an intent (ADR-011). A pointer that an entry moves is confirmed by the next entry
/// that changes the same pointer if its old value is the new value of this one, and refuted if it
/// is the old value of this one; any other old value is a change outside the log and leaves it
/// unknown. The last entry of a pointer is compared with the value the pointer has now: equal to
/// the new value confirms; equal to the old value refutes, but only if `current` is settled;
/// anything else leaves it unknown, as does a working-copy commit that `current` does not hold
/// ([`Current::with_working_copy`]), and a `current` that is `None` because the references could
/// not be read. An entry took effect
/// if all its pointers did, it did not if one did not, and otherwise it is unknown.
///
/// The entry of `take-over` names no pointer; it is judged by the owner of the working copy
/// (ADR-010). The newest one took effect if its actor owns the working copy now, and it did not
/// if another actor does and `current` is settled; a `current` without the owner
/// ([`Current::with_owner`]) leaves it unknown. An older one is unknown: only another take-over
/// changes an owner, so the owner now says nothing about it.
///
/// `entries` must ascend and end with the newest entry of the log, because an entry that is the
/// last of its pointer is compared with `current`; `current` must be read after the entries and
/// must hold every branch that they name. A writer that completes an entry and its move between
/// the two reads can make the newest entry unknown, or wrong if it moves a pointer back to its
/// old value; a caller that lists the log again after it read the references can detect that and
/// pass a `current` that is not settled.
///
/// A pointer that returns to an earlier value inside the log is decided correctly as long as
/// nothing outside the log moves it, because each entry reads its old value under the lock. A
/// change outside the log to one of the two values of an entry cannot be told from a move of the
/// log, so the reading has two limits (ADR-011): a pointer set back to the old value of an entry
/// that took effect looks like a failed move, and the entry is said not to have taken effect; a
/// pointer set to the new value of an entry that failed looks like a move that happened, and the
/// entry is said to have taken effect. For the newest entry of a pointer the first limit applies
/// only while no writer is at work; otherwise its outcome is unknown.
///
/// A stub and a purge entry are unknown. A stub is a barrier: the change that it replaced moved
/// pointers that are no longer known, so every entry before the newest stub is unknown too.
pub fn outcomes(entries: &[Entry], current: Option<&Current>) -> Vec<Outcome> {
    debug_assert!(
        entries
            .windows(2)
            .all(|pair| pair[0].number < pair[1].number),
        "the entries must ascend"
    );
    let settled = current.is_some_and(|current| current.settled);
    // HEAD and the working-copy commit belong to a worktree, so the next move of each is looked
    // up by the worktree of the entry; branches and tags are shared.
    let mut next_head: HashMap<&str, &Moved<Head>> = HashMap::new();
    let mut next_working_copy: HashMap<&str, &Moved<CommitId>> = HashMap::new();
    let mut next_branch: HashMap<&BranchName, &Moved<CommitId>> = HashMap::new();
    let mut next_tag: HashMap<&BranchName, &Moved<CommitId>> = HashMap::new();
    let mut next_remote: HashMap<&RemoteRefName, &Moved<RemoteRef>> = HashMap::new();
    let mut result = vec![Outcome::Unknown; entries.len()];
    // A stub replaced a change that moved pointers which can no longer be seen, so nothing after
    // it is evidence for an entry before it: such an entry is unknown (ADR-014). A purge entry
    // moves nothing and is no barrier.
    let mut behind_stub = false;
    // A take-over after this entry in the same worktree: the owner now is the work of that one.
    let mut newer_take_over: HashSet<&str> = HashSet::new();
    let here = current.map_or(MAIN_WORKTREE, |current| current.worktree.as_str());
    for (index, entry) in entries.iter().enumerate().rev() {
        let change = match entry.record() {
            Record::Change(change) if !behind_stub => change,
            Record::Change(_)
            | Record::Purge(_)
            | Record::Lease(_)
            | Record::Pull(_)
            | Record::Check(_)
            | Record::RemotePin(_) => continue,
            Record::Stub(_) => {
                behind_stub = true;
                continue;
            }
        };
        let worktree = change.worktree_name();
        let local = worktree == here;
        if change.operation().as_str() == TAKE_OVER {
            result[index] = if newer_take_over.contains(worktree) || !local {
                Outcome::Unknown
            } else {
                match current.and_then(|current| current.owner.as_ref()) {
                    Some(Some(owner)) if owner == change.actor() => Outcome::Took,
                    Some(_) if settled => Outcome::NotTaken,
                    _ => Outcome::Unknown,
                }
            };
            newer_take_over.insert(worktree);
            continue;
        }
        let mut judged = Vec::new();
        if let Some(moved) = change.head() {
            let now = match current {
                Some(current) if local => Now::Is(&current.head),
                _ => Now::Unknown,
            };
            judged.push(judge(moved, next_head.get(worktree).copied(), now, settled));
        }
        if let Some(moved) = change.working_copy() {
            let now = match current
                .filter(|_| local)
                .map(|current| &current.working_copy)
            {
                Some(WorkingCopy::Absent) => Now::Absent,
                Some(WorkingCopy::At(commit)) => Now::Is(commit),
                Some(WorkingCopy::Unknown) | None => Now::Unknown,
            };
            judged.push(judge(
                moved,
                next_working_copy.get(worktree).copied(),
                now,
                settled,
            ));
        }
        for (name, moved) in change.branches() {
            let now = current.map_or(Now::Unknown, |current| {
                current.branches.get(name).map_or(Now::Absent, Now::Is)
            });
            judged.push(judge(moved, next_branch.get(name).copied(), now, settled));
        }
        for (name, moved) in change.tags() {
            let now = current
                .and_then(|current| current.tags.as_ref())
                .map_or(Now::Unknown, |tags| {
                    tags.get(name).map_or(Now::Absent, Now::Is)
                });
            judged.push(judge(moved, next_tag.get(name).copied(), now, settled));
        }
        for (name, moved) in change.remotes() {
            let now = current
                .and_then(|current| current.remotes.as_ref())
                .map_or(Now::Unknown, |remotes| {
                    remotes.get(name).map_or(Now::Absent, Now::Is)
                });
            judged.push(judge(moved, next_remote.get(name).copied(), now, settled));
        }
        result[index] = if judged.contains(&Outcome::NotTaken) {
            Outcome::NotTaken
        } else if judged.iter().all(|outcome| *outcome == Outcome::Took) {
            Outcome::Took
        } else {
            Outcome::Unknown
        };
        // This entry is the next one for the entries before it.
        if let Some(moved) = change.head() {
            next_head.insert(worktree, moved);
        }
        if let Some(moved) = change.working_copy() {
            next_working_copy.insert(worktree, moved);
        }
        for (name, moved) in change.branches() {
            next_branch.insert(name, moved);
        }
        for (name, moved) in change.tags() {
            next_tag.insert(name, moved);
        }
        for (name, moved) in change.remotes() {
            next_remote.insert(name, moved);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    #[test]
    fn a_path_with_control_characters_is_escaped_in_the_message() {
        let error = OpLogError::UnexpectedFile {
            path: PathBuf::from("oplog/a\x1b[2Jb\n"),
        };
        let message = error.to_string();
        assert!(
            !message.contains('\x1b') && !message.contains('\n'),
            "{message:?}"
        );
        assert!(message.contains("a\\u{1b}[2Jb\\n"), "{message}");
    }

    use std::fs;
    use std::sync::Barrier;
    use std::thread;

    use super::*;
    use crate::testutil::TempDir;

    const A: &str = "b3:1d719c9e1d926eb0f33467406bf50911eca1d59f976105abd3eb5ccfaabdeed9";
    const B: &str = "b3:4feb2d73a2c706e33c2a71f6f03cbfb48f0a20b48567936fca293159e7e3b32c";
    const C: &str = "b3:8cb48a697fe25a78eeb950e011e7dc9897e8216d13e21f3210db70ce2d9db3e8";
    const TIME: i64 = 1_700_000_000;

    fn commit(text: &str) -> CommitId {
        CommitId::new(text.parse().expect("test ID must be valid"))
    }

    fn branch(name: &str) -> BranchName {
        BranchName::try_new(name).expect("valid branch name")
    }

    fn human() -> Actor {
        Actor::human("daniel").expect("valid actor")
    }

    fn operation(name: &str) -> OperationName {
        OperationName::try_new(name).expect("valid operation name")
    }

    fn moved(old: Option<&str>, new: Option<&str>) -> Moved<CommitId> {
        Moved::from_to(old.map(commit), new.map(commit))
    }

    fn entry(
        number: u64,
        head: Option<Moved<Head>>,
        branches: Vec<(BranchName, Moved<CommitId>)>,
    ) -> Entry {
        let change = Change::try_new(TIME, human(), operation("snapshot"), head, None, branches)
            .expect("valid change");
        Entry::try_new(number, change).expect("valid entry")
    }

    fn moves(name: &str, old: Option<&str>, new: Option<&str>) -> (BranchName, Moved<CommitId>) {
        (branch(name), moved(old, new))
    }

    fn current(branches: &[(&str, &str)], settled: bool) -> Current {
        Current::new(
            Head::Branch(branch("main")),
            branches.iter().map(|(n, c)| (branch(n), commit(c))),
            settled,
        )
    }

    /// The references now, with no writer at work.
    fn now(branches: &[(&str, &str)]) -> Option<Current> {
        Some(current(branches, true))
    }

    fn head_move(old: Option<Head>, new: Option<Head>) -> Option<Moved<Head>> {
        Some(Moved::from_to(old, new))
    }

    fn on(name: &str) -> Head {
        Head::Branch(branch(name))
    }

    fn working_copy(
        number: u64,
        old: Option<&str>,
        new: Option<&str>,
        branches: Vec<(BranchName, Moved<CommitId>)>,
    ) -> Entry {
        let change = Change::try_new(
            TIME,
            human(),
            operation("snapshot"),
            None,
            Some(moved(old, new)),
            branches,
        )
        .expect("valid change");
        Entry::try_new(number, change).expect("valid entry")
    }

    #[test]
    fn the_last_entry_of_a_pointer_is_compared_with_its_value_now() {
        let entries = [entry(1, None, vec![moves("main", None, Some(A))])];
        let outcome = |branches: &[(&str, &str)]| outcomes(&entries, now(branches).as_ref());
        assert_eq!(outcome(&[("main", A)]), [Outcome::Took]);
        assert_eq!(outcome(&[]), [Outcome::NotTaken]);
        assert_eq!(outcome(&[("main", B)]), [Outcome::Unknown]);
    }

    #[test]
    fn a_pointer_that_still_has_its_old_value_is_unknown_while_a_writer_is_at_work() {
        let entries = [
            entry(1, None, vec![moves("main", None, Some(A))]),
            entry(2, None, vec![moves("other", None, Some(B))]),
        ];
        // Entry 2 may be between its entry and its move; entry 1 is unaffected by the lock.
        let busy = current(&[("main", A)], false);
        assert_eq!(
            outcomes(&entries, Some(&busy)),
            [Outcome::Took, Outcome::Unknown]
        );
        let done = current(&[("main", A), ("other", B)], false);
        assert_eq!(
            outcomes(&entries, Some(&done)),
            [Outcome::Took, Outcome::Took]
        );
    }

    #[test]
    fn without_the_references_only_the_chain_decides() {
        let entries = [
            entry(1, None, vec![moves("main", None, Some(A))]),
            entry(2, None, vec![moves("main", Some(A), Some(B))]),
        ];
        assert_eq!(outcomes(&entries, None), [Outcome::Took, Outcome::Unknown]);
    }

    #[test]
    fn a_later_entry_confirms_or_refutes_the_one_before() {
        let entries = [
            entry(1, None, vec![moves("main", None, Some(A))]),
            entry(2, None, vec![moves("main", Some(A), Some(B))]),
            // Entry 2 did not move: entry 3 starts from A again.
            entry(3, None, vec![moves("main", Some(A), Some(C))]),
        ];
        assert_eq!(
            outcomes(&entries, now(&[("main", C)]).as_ref()),
            [Outcome::Took, Outcome::NotTaken, Outcome::Took]
        );
    }

    #[test]
    fn a_later_entry_that_starts_from_a_third_value_leaves_the_one_before_unknown() {
        // Something outside the log moved `main` between entry 1 and entry 2.
        let entries = [
            entry(1, None, vec![moves("main", Some(A), Some(B))]),
            entry(2, None, vec![moves("main", Some(C), Some(A))]),
        ];
        assert_eq!(
            outcomes(&entries, now(&[("main", A)]).as_ref()),
            [Outcome::Unknown, Outcome::Took]
        );
    }

    #[test]
    fn an_entry_took_effect_only_if_all_pointers_did() {
        let entries = [entry(
            1,
            None,
            vec![moves("a", None, Some(A)), moves("b", None, Some(B))],
        )];
        let outcome = |branches: &[(&str, &str)]| outcomes(&entries, now(branches).as_ref());
        assert_eq!(outcome(&[("a", A), ("b", B)]), [Outcome::Took]);
        // Took and unknown is unknown.
        assert_eq!(outcome(&[("a", A), ("b", C)]), [Outcome::Unknown]);
        // Not taken and unknown is not taken; not taken and took is not taken.
        assert_eq!(outcome(&[("a", C)]), [Outcome::NotTaken]);
        assert_eq!(outcome(&[("a", A)]), [Outcome::NotTaken]);
    }

    #[test]
    fn a_deleted_branch_took_effect_if_it_is_gone() {
        let entries = [entry(1, None, vec![moves("main", Some(A), None)])];
        let outcome = |branches: &[(&str, &str)]| outcomes(&entries, now(branches).as_ref());
        assert_eq!(outcome(&[]), [Outcome::Took]);
        assert_eq!(outcome(&[("main", A)]), [Outcome::NotTaken]);
        assert_eq!(outcome(&[("main", B)]), [Outcome::Unknown]);
    }

    #[test]
    fn an_entry_of_another_pointer_does_not_hide_the_chain_of_a_pointer() {
        let entries = [
            entry(1, None, vec![moves("main", None, Some(A))]),
            entry(2, None, vec![moves("other", None, Some(B))]),
            entry(3, None, vec![moves("main", Some(A), Some(C))]),
            entry(4, head_move(Some(on("main")), Some(on("other"))), vec![]),
            entry(5, None, vec![moves("other", Some(B), None)]),
            entry(6, head_move(Some(on("other")), Some(on("x"))), vec![]),
        ];
        let current = Current::new(on("x"), [(branch("main"), commit(C))], true);
        assert_eq!(
            outcomes(&entries, Some(&current)),
            [
                Outcome::Took,
                Outcome::Took,
                Outcome::Took,
                Outcome::Took,
                Outcome::Took,
                Outcome::Took
            ]
        );
    }

    #[test]
    fn head_is_judged_by_its_own_chain_and_by_its_value_now() {
        let entries = [
            entry(1, head_move(None, Some(on("main"))), vec![]),
            entry(
                2,
                head_move(Some(on("main")), Some(Head::Detached(commit(A)))),
                vec![],
            ),
        ];
        let at = |head: Head| outcomes(&entries, Some(&Current::new(head, [], true)));
        // HEAD is `main` now: entry 2 is refuted, entry 1 is confirmed by entry 2.
        assert_eq!(at(on("main")), [Outcome::Took, Outcome::NotTaken]);
        assert_eq!(
            at(Head::Detached(commit(A))),
            [Outcome::Took, Outcome::Took]
        );
        assert_eq!(at(on("other")), [Outcome::Took, Outcome::Unknown]);
    }

    #[test]
    fn a_working_copy_is_confirmed_by_the_chain_and_unknown_at_the_end() {
        let entries = [
            working_copy(1, None, Some(A), vec![]),
            working_copy(2, Some(A), Some(B), vec![moves("main", None, Some(B))]),
        ];
        let outcome = |branches: &[(&str, &str)]| outcomes(&entries, now(branches).as_ref());
        // The branch is confirmed, the working copy has no place in the references.
        assert_eq!(outcome(&[("main", B)]), [Outcome::Took, Outcome::Unknown]);
        // A refuted branch fails the entry whatever the working copy is.
        assert_eq!(outcome(&[]), [Outcome::Took, Outcome::NotTaken]);
        assert_eq!(outcomes(&[], now(&[]).as_ref()), []);
    }

    #[test]
    fn a_failed_entry_in_the_middle_is_refuted_by_its_retry() {
        // Entry 2 failed (`MoveFailed`); the retry writes entry 3 from the same old value.
        let entries = [
            entry(1, None, vec![moves("main", None, Some(A))]),
            entry(2, None, vec![moves("main", Some(A), Some(B))]),
            entry(3, None, vec![moves("main", Some(A), Some(B))]),
        ];
        assert_eq!(
            outcomes(&entries, now(&[("main", B)]).as_ref()),
            [Outcome::Took, Outcome::NotTaken, Outcome::Took]
        );
        // The chain alone decides the first two; the last one needs the value now.
        assert_eq!(
            outcomes(&entries, None),
            [Outcome::Took, Outcome::NotTaken, Outcome::Unknown]
        );
    }

    #[test]
    fn the_first_entry_is_judged_by_its_successor_and_without_one_by_the_value_now() {
        let first = entry(1, None, vec![moves("main", None, Some(A))]);
        let confirmed = [
            first.clone(),
            entry(2, None, vec![moves("main", Some(A), Some(B))]),
        ];
        let refuted = [
            first.clone(),
            entry(2, None, vec![moves("main", None, Some(B))]),
        ];
        // The value now does not overrule the chain for an entry that has a successor.
        for value in [A, B, C] {
            let current = now(&[("main", value)]);
            assert_eq!(outcomes(&confirmed, current.as_ref())[0], Outcome::Took);
            assert_eq!(outcomes(&refuted, current.as_ref())[0], Outcome::NotTaken);
        }
        // Alone, the first entry is also the last one.
        let alone = [first];
        assert_eq!(
            outcomes(&alone, now(&[("main", A)]).as_ref()),
            [Outcome::Took]
        );
        assert_eq!(outcomes(&alone, None), [Outcome::Unknown]);
    }

    #[test]
    fn a_pointer_that_returns_to_an_earlier_value_inside_the_log_is_decided() {
        let entries = [
            entry(1, None, vec![moves("main", None, Some(A))]),
            entry(2, None, vec![moves("main", Some(A), Some(B))]),
            entry(3, None, vec![moves("main", Some(B), Some(A))]),
        ];
        assert_eq!(
            outcomes(&entries, now(&[("main", A)]).as_ref()),
            [Outcome::Took; 3]
        );
        // Entry 3 failed: `main` is still B.
        assert_eq!(
            outcomes(&entries, now(&[("main", B)]).as_ref()),
            [Outcome::Took, Outcome::Took, Outcome::NotTaken]
        );
    }

    #[test]
    fn a_pointer_that_returns_to_its_old_value_outside_the_log_is_wrongly_refuted() {
        // The first documented limit (ADR-011): entry 2 moved `main` from A to B, and then
        // something outside the log set it back to A. The log sees what a failed move leaves, so
        // it says that the move did not take effect. The test pins the limit; it does not describe
        // the wanted behaviour. The same through a next entry that starts from A is
        // `a_later_entry_confirms_or_refutes_the_one_before`.
        let entries = [
            entry(1, None, vec![moves("main", None, Some(A))]),
            entry(2, None, vec![moves("main", Some(A), Some(B))]),
        ];
        assert_eq!(
            outcomes(&entries, now(&[("main", A)]).as_ref()),
            [Outcome::Took, Outcome::NotTaken]
        );
        // While a writer is at work, the old value is no evidence, and the error does not appear.
        assert_eq!(
            outcomes(&entries, Some(&current(&[("main", A)], false))),
            [Outcome::Took, Outcome::Unknown]
        );
    }

    #[test]
    fn a_pointer_set_to_the_new_value_of_a_failed_entry_outside_the_log_is_wrongly_confirmed() {
        // The second documented limit (ADR-011): entry 2 failed and left `main` at A, and then
        // something outside the log set it to B. The log sees what a move leaves, so it says that
        // the move took effect, through the value now and through a next entry that starts from B.
        // The test pins the limit; it does not describe the wanted behaviour.
        let entries = [
            entry(1, None, vec![moves("main", None, Some(A))]),
            entry(2, None, vec![moves("main", Some(A), Some(B))]),
        ];
        assert_eq!(
            outcomes(&entries, Some(&current(&[("main", B)], false))),
            [Outcome::Took, Outcome::Took]
        );
        let next = [
            entries[0].clone(),
            entries[1].clone(),
            entry(3, None, vec![moves("main", Some(B), Some(C))]),
        ];
        assert_eq!(
            outcomes(&next, now(&[("main", C)]).as_ref()),
            [Outcome::Took; 3]
        );
    }

    #[test]
    #[should_panic(expected = "the entries must ascend")]
    fn entries_that_do_not_ascend_are_a_mistake_of_the_caller() {
        let entries = [
            entry(2, None, vec![moves("main", None, Some(A))]),
            entry(1, None, vec![moves("main", None, Some(B))]),
        ];
        let _ = outcomes(&entries, None);
    }

    /// The change of the golden vector below.
    fn sample_change() -> Change {
        Change::try_new(
            TIME,
            human(),
            operation("snapshot"),
            Some(Moved::from_to(
                Some(Head::Branch(branch("main"))),
                Some(Head::Detached(commit(A))),
            )),
            Some(moved(Some(A), Some(B))),
            vec![
                (branch("main"), moved(Some(C), Some(A))),
                (branch("feat/x"), moved(None, Some(B))),
            ],
        )
        .expect("valid change")
    }

    fn sample_entry() -> Entry {
        Entry::try_new(42, sample_change()).expect("valid entry")
    }

    fn golden() -> String {
        format!(
            "stemma-oplog 1\nnumber 42\ntime 1700000000\nactor human daniel\noperation snapshot\n\
             head branch:main commit:{A}\nworking-copy {A} {B}\nbranch feat/x - {B}\n\
             branch main {C} {A}\n"
        )
    }

    // The operation name

    #[test]
    fn an_operation_name_is_a_lowercase_word() {
        for good in ["init", "snapshot", "a", "op-2", "x9-y", &"a".repeat(32)] {
            assert_eq!(OperationName::try_new(good).unwrap().as_str(), good);
            assert_eq!(good.parse::<OperationName>().unwrap().to_string(), good);
        }
        let cases = [
            ("", OperationNameError::Empty),
            ("1a", OperationNameError::BadStart),
            ("-a", OperationNameError::BadStart),
            ("Init", OperationNameError::BadStart),
            ("\u{e9}a", OperationNameError::BadStart),
            ("aB", OperationNameError::BadCharacter(1)),
            ("a b", OperationNameError::BadCharacter(1)),
            ("a_b", OperationNameError::BadCharacter(1)),
            ("a\n", OperationNameError::BadCharacter(1)),
            ("a\u{e9}", OperationNameError::BadCharacter(1)),
        ];
        for (bad, expected) in cases {
            assert_eq!(OperationName::try_new(bad), Err(expected), "{bad:?}");
        }
        assert_eq!(
            OperationName::try_new("a".repeat(33)),
            Err(OperationNameError::TooLong(33))
        );
    }

    // Changes

    #[test]
    fn a_change_needs_a_pointer_that_differs() {
        let build = |head, working_copy, branches| {
            Change::try_new(TIME, human(), operation("x"), head, working_copy, branches)
        };
        assert_eq!(build(None, None, vec![]), Err(EntryError::NoChange));
        assert_eq!(
            build(Some(Moved::from_to(None, None)), None, vec![]),
            Err(EntryError::Unchanged(Pointer::Head))
        );
        assert_eq!(
            build(None, Some(moved(Some(A), Some(A))), vec![]),
            Err(EntryError::Unchanged(Pointer::WorkingCopy))
        );
        assert_eq!(
            build(None, None, vec![(branch("b"), moved(Some(A), Some(A)))]),
            Err(EntryError::Unchanged(Pointer::Branch(branch("b"))))
        );
        assert_eq!(
            build(
                None,
                None,
                vec![
                    (branch("b"), moved(None, Some(A))),
                    (branch("b"), moved(None, Some(B)))
                ]
            ),
            Err(EntryError::DuplicateBranch(branch("b")))
        );
        // Duplicates are found whatever the order of the input, not only next to each other.
        assert_eq!(
            build(
                None,
                None,
                vec![
                    (branch("b"), moved(None, Some(A))),
                    (branch("a"), moved(None, Some(A))),
                    (branch("b"), moved(None, Some(B)))
                ]
            ),
            Err(EntryError::DuplicateBranch(branch("b")))
        );
    }

    #[test]
    fn the_branches_of_a_change_are_sorted_whatever_the_input_order() {
        let change = sample_change();
        let names: Vec<&str> = change.branches().iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["feat/x", "main"]);
    }

    #[test]
    fn the_number_of_an_entry_is_from_1_to_the_maximum() {
        assert_eq!(
            Entry::try_new(0, sample_change()),
            Err(EntryError::NumberOutOfRange(0))
        );
        assert_eq!(
            Entry::try_new(MAX_NUMBER + 1, sample_change()),
            Err(EntryError::NumberOutOfRange(MAX_NUMBER + 1))
        );
        assert!(Entry::try_new(1, sample_change()).is_ok());
        assert!(Entry::try_new(MAX_NUMBER, sample_change()).is_ok());
    }

    #[test]
    fn a_change_names_every_commit_it_made_unreachable_or_reachable() {
        let commits = sample_change().commits();
        for id in [A, B, C] {
            assert!(commits.contains(&commit(id)), "{id}");
        }
        let only_names = Change::try_new(
            TIME,
            human(),
            operation("x"),
            Some(Moved::from_to(None, Some(Head::Branch(branch("main"))))),
            None,
            vec![],
        )
        .unwrap();
        assert!(only_names.commits().is_empty());
    }

    // The encoding

    #[test]
    fn an_entry_is_written_as_the_golden_encoding() {
        assert_eq!(sample_entry().encode(), golden().into_bytes());
    }

    #[test]
    fn an_entry_of_a_linked_worktree_is_the_golden_text_with_two_lines_more() {
        let change = sample_change()
            .in_worktree(Some("linked".to_owned()), Some("/work/linked".to_owned()))
            .unwrap();
        let entry = Entry::try_new(42, change).unwrap();
        let text = golden().replace(
            "operation snapshot\n",
            "operation snapshot\nworktree linked\nlocation /work/linked\n",
        );
        assert_eq!(entry.encode(), text.clone().into_bytes());
        let read = Entry::parse(text.as_bytes()).unwrap();
        assert_eq!(read, entry);
        assert_eq!(read.change().unwrap().worktree(), Some("linked"));
        assert_eq!(read.change().unwrap().location(), Some("/work/linked"));
        // The entry of the main worktree has not one line more than before worktrees.
        assert!(!golden().contains("worktree"));
    }

    #[test]
    fn an_entry_is_read_back_from_its_encoding() {
        let entry = sample_entry();
        let read = Entry::parse(&entry.encode()).unwrap();
        assert_eq!(read, entry);
        assert_eq!(read.number(), 42);
        assert_eq!(read.change().unwrap().operation().as_str(), "snapshot");
        assert_eq!(read.change().unwrap().time(), TIME);
        assert_eq!(read.change().unwrap().actor(), &human());
        let head = read.change().unwrap().head().unwrap();
        assert_eq!(head.before(), Some(&Head::Branch(branch("main"))));
        assert_eq!(head.after(), Some(&Head::Detached(commit(A))));
        assert_eq!(
            read.change().unwrap().working_copy().unwrap().after(),
            Some(&commit(B))
        );
    }

    fn tag_change(
        operation: &str,
        branches: Vec<(BranchName, Moved<CommitId>)>,
        tags: Vec<(BranchName, Moved<CommitId>)>,
    ) -> Result<Change, EntryError> {
        Change::try_new_with_tags(
            TIME,
            human(),
            self::operation(operation),
            None,
            None,
            branches,
            tags,
        )
    }

    #[test]
    fn a_tag_is_written_below_the_branches_and_read_back() {
        let change = tag_change(
            "tag",
            vec![(branch("main"), moved(Some(A), Some(B)))],
            vec![
                (branch("v2"), moved(None, Some(B))),
                (branch("v1"), moved(None, Some(A))),
            ],
        )
        .unwrap();
        // The tags are sorted by name, as the branches are.
        let names: Vec<&str> = change.tags().iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["v1", "v2"]);
        let entry = Entry::try_new(7, change).unwrap();
        let text = String::from_utf8(entry.encode()).unwrap();
        assert!(
            text.ends_with(&format!(
                "branch main {A} {B}\ntag v1 - {A}\ntag v2 - {B}\n"
            )),
            "{text}"
        );
        assert_eq!(Entry::parse(text.as_bytes()).unwrap(), entry);
    }

    #[test]
    fn a_tag_alone_is_a_change_and_its_commit_is_one_that_the_entry_keeps_alive() {
        let change = tag_change("tag", vec![], vec![(branch("v1"), moved(None, Some(A)))]).unwrap();
        assert_eq!(change.commits(), vec![commit(A)]);
        let entry = Entry::try_new(2, change).unwrap();
        assert_eq!(Entry::parse(&entry.encode()).unwrap(), entry);
    }

    #[test]
    fn a_tag_and_a_branch_may_have_the_same_name() {
        let change = tag_change(
            "tag",
            vec![(branch("v1"), moved(None, Some(A)))],
            vec![(branch("v1"), moved(None, Some(A)))],
        );
        assert!(change.is_ok());
    }

    #[test]
    fn a_tag_that_is_listed_twice_or_unchanged_is_rejected() {
        let twice = tag_change(
            "tag",
            vec![],
            vec![
                (branch("v1"), moved(None, Some(A))),
                (branch("v1"), moved(None, Some(B))),
            ],
        );
        assert_eq!(twice, Err(EntryError::DuplicateTag(branch("v1"))));
        let unchanged = tag_change("tag", vec![], vec![(branch("v1"), moved(Some(A), Some(A)))]);
        assert_eq!(
            unchanged,
            Err(EntryError::Unchanged(Pointer::Tag(branch("v1"))))
        );
        assert_eq!(tag_change("tag", vec![], vec![]), Err(EntryError::NoChange));
    }

    #[test]
    fn the_lines_of_tags_must_be_sorted_unique_and_after_the_branches() {
        let head =
            format!("stemma-oplog 1\nnumber 1\ntime {TIME}\nactor human daniel\noperation tag\n");
        let tags_out_of_order = format!("{head}tag b - {A}\ntag a - {A}\n");
        assert!(matches!(
            Entry::parse(tags_out_of_order.as_bytes()),
            Err(EntryParseError::OutOfOrder { line: 7, .. })
        ));
        let branch_after_tag = format!("{head}tag a - {A}\nbranch b - {A}\n");
        assert!(matches!(
            Entry::parse(branch_after_tag.as_bytes()),
            Err(EntryParseError::Malformed {
                reason: EntryMalformed::PointerOutOfOrder,
                ..
            })
        ));
        let head_after_tag = format!("{head}tag a - {A}\nhead - branch:main\n");
        assert!(matches!(
            Entry::parse(head_after_tag.as_bytes()),
            Err(EntryParseError::Malformed {
                reason: EntryMalformed::PointerOutOfOrder,
                ..
            })
        ));
        let unchanged = format!("{head}tag a {A} {A}\n");
        assert!(matches!(
            Entry::parse(unchanged.as_bytes()),
            Err(EntryParseError::Unchanged { line: 6 })
        ));
    }

    #[test]
    fn the_outcome_of_a_tag_is_judged_from_the_tags_that_exist() {
        let change = tag_change("tag", vec![], vec![(branch("v1"), moved(None, Some(A)))]).unwrap();
        let entries = [Entry::try_new(1, change).unwrap()];
        let head = Head::Branch(branch("main"));
        let with = |tags: Vec<(BranchName, CommitId)>, settled| {
            let current = Current::new(head.clone(), [], settled).with_tags(tags);
            outcomes(&entries, Some(&current))
        };
        assert_eq!(with(vec![(branch("v1"), commit(A))], true), [Outcome::Took]);
        assert_eq!(with(vec![], true), [Outcome::NotTaken]);
        // A writer may be between its entry and its move.
        assert_eq!(with(vec![], false), [Outcome::Unknown]);
        // Without the tags the outcome is not known.
        let current = Current::new(head, [], true);
        assert_eq!(outcomes(&entries, Some(&current)), [Outcome::Unknown]);
    }

    #[test]
    fn the_deletion_of_a_tag_is_a_line_with_the_commit_and_a_dash_and_is_judged_by_the_tags() {
        let change =
            tag_change("untag", vec![], vec![(branch("v1"), moved(Some(A), None))]).unwrap();
        let entry = Entry::try_new(2, change).unwrap();
        assert!(String::from_utf8(entry.encode())
            .unwrap()
            .ends_with(&format!("operation untag\ntag v1 {A} -\n")));
        assert_eq!(Entry::parse(&entry.encode()).unwrap(), entry);
        let set = Entry::try_new(
            1,
            tag_change("tag", vec![], vec![(branch("v1"), moved(None, Some(A)))]).unwrap(),
        )
        .unwrap();
        let entries = [set, entry];
        let head = Head::Branch(branch("main"));
        let with = |tags: Vec<(BranchName, CommitId)>, settled| {
            let current = Current::new(head.clone(), [], settled).with_tags(tags);
            outcomes(&entries, Some(&current))
        };
        // The tag is gone: the deletion took effect, and so did the entry that set the tag, which
        // the deletion followed.
        assert_eq!(with(vec![], true), [Outcome::Took, Outcome::Took]);
        // The tag is still there: the deletion did not take effect.
        assert_eq!(
            with(vec![(branch("v1"), commit(A))], true),
            [Outcome::Took, Outcome::NotTaken]
        );
        // A writer may be between its entry and its deletion.
        assert_eq!(
            with(vec![(branch("v1"), commit(A))], false),
            [Outcome::Took, Outcome::Unknown]
        );
        // A tag at a commit that neither entry names was changed outside the log.
        assert_eq!(
            with(vec![(branch("v1"), commit(B))], true),
            [Outcome::Took, Outcome::Unknown]
        );
        // Without the tags the deletion is not known; the setting is, because the deletion that
        // follows it starts from the commit that it set.
        let current = Current::new(head, [], true);
        assert_eq!(
            outcomes(&entries, Some(&current)),
            [Outcome::Took, Outcome::Unknown]
        );
    }

    #[test]
    fn a_tag_that_is_deleted_and_set_again_is_judged_by_the_entries_that_follow() {
        let entry = |number: u64, old: Option<&str>, new: Option<&str>| {
            let operation = if new.is_some() { "tag" } else { "untag" };
            let change =
                tag_change(operation, vec![], vec![(branch("v1"), moved(old, new))]).unwrap();
            Entry::try_new(number, change).unwrap()
        };
        let head = Head::Branch(branch("main"));
        for again in [A, B] {
            // set at A, deleted, set again (at the same commit or at another)
            let entries = [
                entry(1, None, Some(A)),
                entry(2, Some(A), None),
                entry(3, None, Some(again)),
            ];
            let current =
                Current::new(head.clone(), [], true).with_tags([(branch("v1"), commit(again))]);
            assert_eq!(
                outcomes(&entries, Some(&current)),
                [Outcome::Took, Outcome::Took, Outcome::Took],
                "set again at {again}"
            );
        }
    }

    #[test]
    fn the_smallest_and_the_largest_entries_are_read_back() {
        let smallest = Entry::try_new(
            1,
            Change::try_new(
                i64::MIN,
                human(),
                operation("init"),
                Some(Moved::from_to(None, Some(Head::Branch(branch("main"))))),
                None,
                vec![],
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            smallest.encode(),
            b"stemma-oplog 1\nnumber 1\ntime -9223372036854775808\nactor human daniel\n\
              operation init\nhead - branch:main\n"
        );
        assert_eq!(Entry::parse(&smallest.encode()).unwrap(), smallest);
        let agent = Actor::agent("a1", "m=1", "s", "daniel").unwrap();
        let largest = Entry::try_new(
            MAX_NUMBER,
            Change::try_new(
                i64::MAX,
                agent,
                operation("op-9"),
                None,
                Some(moved(Some(A), None)),
                vec![(branch("z"), moved(Some(B), None))],
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(Entry::parse(&largest.encode()).unwrap(), largest);
    }

    fn with(entry: &str, from: &str, to: &str) -> Vec<u8> {
        assert!(entry.contains(from), "{from:?} is not in the entry");
        entry.replacen(from, to, 1).into_bytes()
    }

    fn parse_error(bytes: impl AsRef<[u8]>) -> EntryParseError {
        Entry::parse(bytes.as_ref()).unwrap_err()
    }

    fn malformed(line: usize, reason: EntryMalformed) -> EntryParseError {
        EntryParseError::Malformed { line, reason }
    }

    #[test]
    fn the_reader_reports_the_shape_of_a_bad_entry_at_its_line() {
        let text = golden();
        assert_eq!(
            parse_error(b""),
            malformed(1, EntryMalformed::ExpectedHeader)
        );
        assert_eq!(
            parse_error(with(&text, "stemma-oplog 1\n", "stemma-oplog\n")),
            malformed(1, EntryMalformed::ExpectedHeader)
        );
        assert_eq!(
            parse_error(with(&text, "stemma-oplog 1", "Stemma-oplog 1")),
            malformed(1, EntryMalformed::ExpectedHeader)
        );
        assert_eq!(
            parse_error(with(&text, "stemma-oplog 1", "stemma-oplog 1x")),
            malformed(1, EntryMalformed::ExpectedHeader)
        );
        assert_eq!(
            parse_error(with(&text, "number 42\n", "")),
            malformed(2, EntryMalformed::ExpectedNumber)
        );
        assert_eq!(
            parse_error(with(&text, "time 1700000000\n", "")),
            malformed(3, EntryMalformed::ExpectedTime)
        );
        assert_eq!(
            parse_error(with(&text, "actor human daniel\n", "")),
            malformed(4, EntryMalformed::ExpectedActor)
        );
        assert_eq!(
            parse_error(with(&text, "operation snapshot\n", "")),
            malformed(5, EntryMalformed::ExpectedOperation)
        );
        // The header lines out of order, repeated, or with another spacing.
        assert_eq!(
            parse_error(with(
                &text,
                "number 42\ntime 1700000000\n",
                "time 1700000000\nnumber 42\n"
            )),
            malformed(2, EntryMalformed::ExpectedNumber)
        );
        assert_eq!(
            parse_error(with(&text, "number 42\n", "number 42\nnumber 42\n")),
            malformed(3, EntryMalformed::ExpectedTime)
        );
        assert_eq!(
            parse_error(with(&text, "number 42", "number  42")),
            EntryParseError::InvalidNumber {
                line: 2,
                reason: NumberError::NotCanonical
            }
        );
    }

    #[test]
    fn a_cut_off_entry_is_reported_as_cut_off_at_the_line_it_ends() {
        let text = golden();
        assert_eq!(
            parse_error(text.trim_end()),
            malformed(9, EntryMalformed::CutOff)
        );
        assert_eq!(
            parse_error("stemma-oplog 1"),
            malformed(1, EntryMalformed::CutOff)
        );
        assert_eq!(
            parse_error("stemma-oplog 1\nnumber 4"),
            malformed(2, EntryMalformed::CutOff)
        );
        // An entry that ends after its header lines has no pointer, which is another error.
        assert_eq!(
            parse_error("stemma-oplog 1\nnumber 1\ntime 1\nactor human d\noperation x\n"),
            malformed(6, EntryMalformed::ExpectedPointer)
        );
        // Text after the last pointer is another pointer line, or unknown.
        assert_eq!(
            parse_error(format!("{text}\n")),
            malformed(10, EntryMalformed::UnknownPointer)
        );
        assert_eq!(
            parse_error(format!("{text}extra\n")),
            malformed(10, EntryMalformed::UnknownPointer)
        );
    }

    #[test]
    fn a_version_other_than_1_to_4_is_unsupported_and_not_malformed() {
        let text = golden();
        for version in ["0", "5", "10", "99999999999999999999"] {
            assert_eq!(
                parse_error(with(
                    &text,
                    "stemma-oplog 1",
                    &format!("stemma-oplog {version}")
                )),
                EntryParseError::UnsupportedVersion(version.to_owned()),
                "{version}"
            );
        }
        let message = parse_error(with(&text, "stemma-oplog 1", "stemma-oplog 5")).to_string();
        assert!(
            message.contains("version 5") && message.contains("versions 1 to 4"),
            "{message}"
        );
    }

    #[test]
    fn the_reader_rejects_a_number_that_is_not_canonical() {
        let text = golden();
        for (number, reason) in [
            ("", NumberError::NotCanonical),
            ("0", NumberError::NotCanonical),
            ("00", NumberError::NotCanonical),
            ("042", NumberError::NotCanonical),
            ("+42", NumberError::NotCanonical),
            ("-1", NumberError::NotCanonical),
            ("4 2", NumberError::NotCanonical),
            ("\u{664}", NumberError::NotCanonical),
            ("1000000000000", NumberError::OutOfRange),
            ("18446744073709551616", NumberError::OutOfRange),
        ] {
            assert_eq!(
                parse_error(with(&text, "number 42", &format!("number {number}"))),
                EntryParseError::InvalidNumber { line: 2, reason },
                "{number:?}"
            );
        }
        for good in ["1", "999999999999"] {
            assert!(Entry::parse(&with(&text, "number 42", &format!("number {good}"))).is_ok());
        }
    }

    #[test]
    fn the_reader_rejects_a_time_an_actor_and_an_operation_that_are_not_canonical() {
        let text = golden();
        for time in ["", "+1", "-0", "01", "1.5", " 1", "9223372036854775808"] {
            assert!(
                matches!(
                    parse_error(with(&text, "time 1700000000", &format!("time {time}"))),
                    EntryParseError::InvalidTime { line: 3, .. }
                ),
                "{time:?}"
            );
        }
        for actor in [
            "",
            "human",
            "human a b",
            "robot x",
            "agent a",
            "human d\u{e4}nu",
        ] {
            assert!(
                matches!(
                    parse_error(with(&text, "actor human daniel", &format!("actor {actor}"))),
                    EntryParseError::InvalidActor { line: 4, .. }
                ),
                "{actor:?}"
            );
        }
        for operation in ["", "Snapshot", "1x", "a_b", "a b"] {
            assert!(
                matches!(
                    parse_error(with(
                        &text,
                        "operation snapshot",
                        &format!("operation {operation}")
                    )),
                    EntryParseError::InvalidOperation { line: 5, .. }
                ),
                "{operation:?}"
            );
        }
    }

    #[test]
    fn the_pointers_come_in_a_fixed_order_and_each_once() {
        let h = "head - branch:main\n".to_string();
        let w = format!("working-copy {A} {B}\n");
        let b1 = format!("branch a {A} {B}\n");
        let b2 = format!("branch b {A} {B}\n");
        let prefix = "stemma-oplog 1\nnumber 1\ntime 1\nactor human d\noperation x\n";
        let at = |body: String| parse_error(format!("{prefix}{body}"));
        assert!(Entry::parse(format!("{prefix}{h}{w}{b1}{b2}").as_bytes()).is_ok());
        assert!(Entry::parse(format!("{prefix}{w}{b1}").as_bytes()).is_ok());
        assert!(Entry::parse(format!("{prefix}{b1}").as_bytes()).is_ok());
        // Out of order, and twice.
        assert_eq!(
            at(format!("{w}{h}")),
            malformed(7, EntryMalformed::PointerOutOfOrder)
        );
        assert_eq!(
            at(format!("{b1}{h}")),
            malformed(7, EntryMalformed::PointerOutOfOrder)
        );
        assert_eq!(
            at(format!("{b1}{w}")),
            malformed(7, EntryMalformed::PointerOutOfOrder)
        );
        assert_eq!(
            at(format!("{h}{h}")),
            malformed(7, EntryMalformed::PointerOutOfOrder)
        );
        assert_eq!(
            at(format!("{w}{w}")),
            malformed(7, EntryMalformed::PointerOutOfOrder)
        );
        // Branches are strictly ascending by the bytes of the name.
        assert_eq!(
            at(format!("{b2}{b1}")),
            EntryParseError::OutOfOrder {
                line: 7,
                name: "a".into()
            }
        );
        assert_eq!(
            at(format!("{b1}{b1}")),
            EntryParseError::OutOfOrder {
                line: 7,
                name: "a".into()
            }
        );
        // The wrong number of values, and unknown lines.
        assert_eq!(
            at(format!("working-copy {A}\n")),
            malformed(6, EntryMalformed::WrongValueCount)
        );
        assert_eq!(
            at(format!("branch a {A}\n")),
            malformed(6, EntryMalformed::WrongValueCount)
        );
        assert_eq!(
            at(format!("branch a {A} {B} {C}\n")),
            malformed(6, EntryMalformed::WrongValueCount)
        );
        assert_eq!(
            at("head - -  \n".to_string()),
            malformed(6, EntryMalformed::WrongValueCount)
        );
        assert_eq!(
            at("note a - -\n".to_owned()),
            malformed(6, EntryMalformed::UnknownPointer)
        );
        assert_eq!(
            at("\n".to_owned()),
            malformed(6, EntryMalformed::UnknownPointer)
        );
        assert_eq!(
            at(format!("Branch a {A} {B}\n")),
            malformed(6, EntryMalformed::UnknownPointer)
        );
    }

    #[test]
    fn the_reader_rejects_values_that_are_not_canonical() {
        let prefix = "stemma-oplog 1\nnumber 1\ntime 1\nactor human d\noperation x\n";
        let at = |body: String| parse_error(format!("{prefix}{body}"));
        // A commit ID, in every pointer line.
        for body in [
            "working-copy b3:00 -\n".to_owned(),
            "working-copy - B3:00\n".to_owned(),
            format!("working-copy - {}\n", A.to_uppercase()),
            format!("branch a - {}\n", &A[..A.len() - 1]),
            "branch a b3:zz -\n".to_owned(),
        ] {
            assert!(
                matches!(
                    at(body.clone()),
                    EntryParseError::InvalidObjectId { line: 6, .. }
                ),
                "{body:?}"
            );
        }
        // A branch name: the error is exactly the one of `BranchName`, or the shape is wrong
        // because the name has a space. The line is the one of the pointer.
        for name in ["", "/a", "a/", ".a", "A\u{e9}", "a//b"] {
            assert_eq!(
                at(format!("branch {name} - {A}\n")),
                EntryParseError::InvalidBranchName {
                    line: 6,
                    source: BranchName::try_new(name).unwrap_err()
                },
                "{name:?}"
            );
        }
        assert_eq!(
            at(format!("branch a b - {A}\n")),
            malformed(6, EntryMalformed::WrongValueCount)
        );
        // A value of HEAD, in the old and in the new column.
        let branch_error = |name: &str| BranchName::try_new(name).unwrap_err();
        let id_error = ObjectId::from_str("b3:00").unwrap_err();
        let head_cases: [(&str, EntryParseError); 9] = [
            ("main", malformed(6, EntryMalformed::UnknownHeadValue)),
            ("branch", malformed(6, EntryMalformed::UnknownHeadValue)),
            ("commit", malformed(6, EntryMalformed::UnknownHeadValue)),
            (
                "Branch:main",
                malformed(6, EntryMalformed::UnknownHeadValue),
            ),
            ("tag:x", malformed(6, EntryMalformed::UnknownHeadValue)),
            (":", malformed(6, EntryMalformed::UnknownHeadValue)),
            (
                "branch:",
                EntryParseError::InvalidBranchName {
                    line: 6,
                    source: branch_error(""),
                },
            ),
            (
                "commit:b3:00",
                EntryParseError::InvalidObjectId {
                    line: 6,
                    source: id_error,
                },
            ),
            ("branch:a b", malformed(6, EntryMalformed::WrongValueCount)),
        ];
        for (value, expected) in head_cases {
            assert_eq!(at(format!("head {value} -\n")), expected, "old: {value}");
            assert_eq!(at(format!("head - {value}\n")), expected, "new: {value}");
        }
        // The line is the line of the pointer, also on the second and the third pointer line.
        let h = "head - branch:main\n";
        let w = format!("working-copy {A} {B}\n");
        assert_eq!(
            at(format!("{h}working-copy b3:00 -\n")),
            EntryParseError::InvalidObjectId {
                line: 7,
                source: ObjectId::from_str("b3:00").unwrap_err()
            }
        );
        assert_eq!(
            at(format!("{h}{w}branch a - b3:zz\n")),
            EntryParseError::InvalidObjectId {
                line: 8,
                source: ObjectId::from_str("b3:zz").unwrap_err()
            }
        );
        assert_eq!(
            at(format!("{h}{w}branch a {A} {A}\n")),
            EntryParseError::Unchanged { line: 8 }
        );
        assert_eq!(
            at(format!("{h}{w}branch /a - {A}\n")),
            EntryParseError::InvalidBranchName {
                line: 8,
                source: BranchName::try_new("/a").unwrap_err()
            }
        );
        // Old and new are equal.
        for body in [
            format!("working-copy {A} {A}\n"),
            "working-copy - -\n".to_owned(),
            format!("branch a {B} {B}\n"),
            "head - -\n".to_owned(),
            "head branch:a branch:a\n".to_owned(),
        ] {
            assert_eq!(
                at(body.clone()),
                EntryParseError::Unchanged { line: 6 },
                "{body:?}"
            );
        }
        // Not UTF-8: reported before anything else.
        assert_eq!(
            parse_error(b"stemma-oplog 1\n\xff"),
            EntryParseError::NotUtf8 { valid_up_to: 15 }
        );
    }

    #[test]
    fn the_reader_accepts_no_second_spelling() {
        // Every single change of one byte that the reader still accepts has to encode back to the
        // changed bytes, or the entry has two encodings.
        let golden = golden().into_bytes();
        let mut accepted = 0;
        for at in 0..golden.len() {
            for &replacement in b"0aA \n-+:" {
                let mut bytes = golden.clone();
                bytes[at] = replacement;
                if let Ok(entry) = Entry::parse(&bytes) {
                    accepted += 1;
                    assert_eq!(entry.encode(), bytes, "byte {at} to {replacement:?}");
                }
            }
        }
        assert!(accepted > 0, "the check reached no valid entry");
    }

    #[test]
    fn error_messages_are_complete_and_name_the_line() {
        let text = golden();
        let message = parse_error(with(&text, "time 1700000000", "time 01")).to_string();
        assert_eq!(
            message,
            "line 3: the time is not a canonical decimal integer of seconds"
        );
        assert!(Error::source(&parse_error(with(&text, "time 1700000000", "time 01"))).is_none());
        let message = parse_error(with(&text, "number 42", "number 0")).to_string();
        assert!(message.starts_with("line 2: the number"), "{message}");
        let message = parse_error(format!("{text}branch zz {A}\n")).to_string();
        assert_eq!(
            message,
            "line 10: expected the old and the new value of the pointer"
        );
        assert_eq!(
            EntryError::Unchanged(Pointer::Branch(branch("main"))).to_string(),
            "the branch `main` has the same old and new value"
        );
        assert_eq!(
            EntryError::NoChange.to_string(),
            "the operation changes no pointer"
        );
    }

    // The log on disk

    fn log_in(dir: &TempDir) -> FsOpLog {
        fs::create_dir(dir.path().join("oplog")).unwrap();
        fs::create_dir(dir.path().join("tmp")).unwrap();
        FsOpLog::new(dir.path())
    }

    fn change(branch_name: &str, old: Option<&str>, new: &str) -> Change {
        Change::try_new(
            TIME,
            human(),
            operation("commit"),
            None,
            None,
            vec![(branch(branch_name), moved(old, Some(new)))],
        )
        .unwrap()
    }

    fn files(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn entries_get_the_next_numbers_and_are_read_back() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        assert_eq!(log.numbers().unwrap(), Vec::<u64>::new());
        let first = log.append(&change("main", None, A)).unwrap();
        let second = log.append(&change("main", Some(A), B)).unwrap();
        assert_eq!((first.number(), second.number()), (1, 2));
        assert_eq!(log.numbers().unwrap(), [1, 2]);
        assert_eq!(log.read(2).unwrap(), second);
        assert_eq!(log.entries().unwrap(), [first, second]);
        assert_eq!(
            files(&dir.path().join("oplog")),
            ["000000000001", "000000000002"]
        );
        // The temporary files are gone, and the content is the canonical encoding.
        assert_eq!(files(&dir.path().join("tmp")), Vec::<String>::new());
        assert_eq!(
            fs::read(dir.path().join("oplog").join("000000000002")).unwrap(),
            log.read(2).unwrap().encode()
        );
    }

    fn four_entries(dir: &TempDir) -> FsOpLog {
        let log = log_in(dir);
        for (old, new) in [(None, A), (Some(A), B), (Some(B), A), (Some(A), B)] {
            log.append(&change("main", old, new)).unwrap();
        }
        log
    }

    /// The number that the tip file names.
    fn tip_number(log: &FsOpLog) -> Option<u64> {
        log.read_tip().unwrap().map(|tip| tip.number)
    }

    /// Sets the tip by hand, as the writer would: in version 2 with the hash of the entry if it
    /// is in the chain, and otherwise in version 1, which names no hash.
    fn set_tip(dir: &TempDir, number: u64) {
        let hash = fs::read(entry_file(dir, number))
            .ok()
            .and_then(|bytes| Entry::parse(&bytes).ok())
            .and_then(|entry| entry.hash());
        let content = match hash {
            Some(hash) => format!("stemma-oplog-tip 2\nnumber {number}\nhash {hash}\n"),
            None => format!("stemma-oplog-tip 1\nnumber {number}\n"),
        };
        fs::write(dir.path().join("oplog-tip"), content).unwrap();
    }

    fn entry_file(dir: &TempDir, number: u64) -> PathBuf {
        dir.path().join("oplog").join(format!("{number:012}"))
    }

    #[test]
    fn the_tip_file_names_the_last_entry() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        assert!(!dir.path().join("oplog-tip").exists());

        let first = log.append(&change("main", None, A)).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("oplog-tip")).unwrap(),
            format!(
                "stemma-oplog-tip 2\nnumber 1\nhash {}\n",
                first.hash().unwrap()
            )
        );
        log.append(&change("main", Some(A), B)).unwrap();

        assert_eq!(tip_number(&log), Some(2));
        assert_eq!(files(&dir.path().join("tmp")), Vec::<String>::new());
    }

    #[test]
    fn a_lost_last_entry_is_noticed_and_the_writer_refuses_its_number() {
        let dir = TempDir::new();
        let log = four_entries(&dir);
        fs::remove_file(entry_file(&dir, 4)).unwrap();

        // Without the tip file the directory looks like a log of three entries.
        assert!(matches!(
            log.numbers(),
            Err(OpLogError::TipAhead {
                tip: 4,
                last: 3,
                ..
            })
        ));
        let error = log.append(&change("main", Some(B), A)).unwrap_err();

        assert!(matches!(
            error,
            OpLogError::TipAhead {
                tip: 4,
                last: 3,
                ..
            }
        ));
        assert_eq!(files(&dir.path().join("oplog")).len(), 3);
        assert_eq!(tip_number(&log), Some(4));
    }

    #[test]
    fn all_entries_lost_are_noticed_too() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        log.append(&change("main", None, A)).unwrap();
        fs::remove_file(entry_file(&dir, 1)).unwrap();

        let error = log.append(&change("main", None, B)).unwrap_err();

        assert!(matches!(
            error,
            OpLogError::TipAhead {
                tip: 1,
                last: 0,
                ..
            }
        ));
    }

    #[test]
    fn a_lost_first_entry_is_a_gap_and_a_lost_middle_one_too() {
        for lost in [1, 2] {
            let dir = TempDir::new();
            let log = four_entries(&dir);
            fs::remove_file(entry_file(&dir, lost)).unwrap();

            assert!(
                matches!(log.numbers(), Err(OpLogError::Gap { missing }) if missing == lost),
                "{lost}"
            );
            // The writer does not refuse a gap: that would stop the move that repairs the log.
            let appended = log.append(&change("main", Some(B), A)).unwrap();
            assert_eq!(appended.number(), 5, "{lost}");
            assert_eq!(tip_number(&log), Some(5), "{lost}");
        }
    }

    #[test]
    fn a_tip_one_behind_is_a_stop_between_the_entry_and_the_tip_and_is_repaired() {
        let dir = TempDir::new();
        let log = four_entries(&dir);
        // The stop: entry 4 is in place, the tip still names 3.
        set_tip(&dir, 3);

        assert_eq!(log.numbers().unwrap(), [1, 2, 3, 4]);
        let appended = log.append(&change("main", Some(B), A)).unwrap();

        assert_eq!(appended.number(), 5);
        assert_eq!(tip_number(&log), Some(5));
    }

    #[test]
    fn a_log_without_a_tip_file_gets_one_with_the_next_entry() {
        let dir = TempDir::new();
        let log = four_entries(&dir);
        fs::remove_file(dir.path().join("oplog-tip")).unwrap();

        assert_eq!(log.numbers().unwrap(), [1, 2, 3, 4]);
        log.append(&change("main", Some(B), A)).unwrap();

        assert_eq!(tip_number(&log), Some(5));
    }

    #[test]
    fn a_tip_file_that_is_not_canonical_is_an_error_and_nothing_is_written() {
        for content in [
            &b""[..],
            b"stemma-oplog-tip 1\n",
            b"stemma-oplog-tip 1\nnumber 04\n",
            b"stemma-oplog-tip 1\nnumber 4",
            b"stemma-oplog-tip 1\nnumber -4\n",
            b"stemma-oplog-tip 2\nnumber 4\n",
            b"stemma-oplog-tip 1\nnumber 4\nmore\n",
            b"stemma-oplog-tip 1\nnumber 1000000000000\n",
            b"stemma-oplog-tip 1\nnumber 99999999999999\n",
        ] {
            let dir = TempDir::new();
            let log = four_entries(&dir);
            fs::write(dir.path().join("oplog-tip"), content).unwrap();

            let error = log.append(&change("main", Some(B), A)).unwrap_err();

            assert!(matches!(error, OpLogError::BadTip { .. }), "{content:?}");
            assert!(matches!(log.numbers(), Err(OpLogError::BadTip { .. })));
            assert_eq!(files(&dir.path().join("oplog")).len(), 4, "{content:?}");
        }
    }

    #[test]
    fn the_largest_number_is_a_valid_tip() {
        let dir = TempDir::new();
        let log = four_entries(&dir);
        set_tip(&dir, MAX_NUMBER);

        assert_eq!(tip_number(&log), Some(MAX_NUMBER));
        // It is a tip far beyond the entries, so the log reports the loss.
        assert!(matches!(log.numbers(), Err(OpLogError::TipAhead { .. })));
    }

    #[test]
    fn after_a_refusal_the_writer_works_again_once_the_tip_is_set_back() {
        let dir = TempDir::new();
        let log = four_entries(&dir);
        fs::remove_file(entry_file(&dir, 4)).unwrap();
        assert!(log.append(&change("main", Some(B), A)).is_err());

        set_tip(&dir, 3);
        let appended = log.append(&change("main", Some(B), A)).unwrap();

        assert_eq!(appended.number(), 4);
        assert_eq!(log.numbers().unwrap(), [1, 2, 3, 4]);
    }

    #[test]
    fn a_gap_is_reported_before_a_tip_that_is_ahead() {
        let dir = TempDir::new();
        let log = four_entries(&dir);
        fs::remove_file(entry_file(&dir, 2)).unwrap();
        fs::remove_file(entry_file(&dir, 4)).unwrap();

        assert!(matches!(log.numbers(), Err(OpLogError::Gap { missing: 2 })));
        // The writer ignores the gap, but not the tip: the last entry is lost.
        assert!(matches!(
            log.append(&change("main", Some(B), A)),
            Err(OpLogError::TipAhead { .. })
        ));
    }

    #[test]
    fn the_message_of_a_tip_that_is_ahead_names_the_file_and_what_to_do() {
        let dir = TempDir::new();
        let log = four_entries(&dir);
        fs::remove_file(entry_file(&dir, 4)).unwrap();

        let message = log.numbers().unwrap_err().to_string();

        assert!(
            message
                .starts_with("the operation log ends at entry 3, but the tip file names entry 4: "),
            "{message}"
        );
        assert!(message.contains("oplog-tip"), "{message}");
        assert!(message.contains("remove"), "{message}");
    }

    #[test]
    fn a_tip_file_that_cannot_be_read_blocks_the_writer_and_the_reader() {
        // A directory in place of the file is an error of the file system, not a missing tip.
        let dir = TempDir::new();
        let log = four_entries(&dir);
        fs::remove_file(dir.path().join("oplog-tip")).unwrap();
        fs::create_dir(dir.path().join("oplog-tip")).unwrap();

        assert!(matches!(
            log.append(&change("main", Some(B), A)),
            Err(OpLogError::Io { .. })
        ));
        assert!(matches!(log.numbers(), Err(OpLogError::Io { .. })));
        assert_eq!(files(&dir.path().join("oplog")).len(), 4);
    }

    #[test]
    fn a_failing_write_of_the_tip_is_no_error_of_the_entry() {
        let dir = TempDir::new();
        let log = four_entries(&dir);
        // The tip is renamed into the root, which may be entered but not written. The entry goes
        // to `oplog/`, which stays writable, and the temporary file to `tmp/`.
        let denied = fault::deny_writes_in(dir.path());
        let result = log.append(&change("main", Some(B), A));
        drop(denied);

        assert_eq!(result.unwrap().number(), 5);
        // The tip is one behind, which the reader and the next writer accept.
        assert_eq!(tip_number(&log), Some(4));
        assert_eq!(log.numbers().unwrap(), [1, 2, 3, 4, 5]);
        assert_eq!(files(&dir.path().join("tmp")), Vec::<String>::new());
        assert_eq!(log.append(&change("main", Some(A), B)).unwrap().number(), 6);
        assert_eq!(tip_number(&log), Some(6));
    }

    #[test]
    fn a_reader_sees_no_loss_while_a_writer_appends_as_a_smoke_test() {
        // The writer links an entry before it writes the tip, so the reader reads the tip before
        // it lists the directory (see `check_tip`). This is a smoke test only: the window of the
        // wrong order is narrow, and the test did not fail when that order was built in. The
        // argument is in `check_tip`.
        let dir = TempDir::new();
        let log = log_in(&dir);
        let reader = FsOpLog::new(dir.path());
        let stop = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            let handle = scope.spawn(|| {
                let mut reads = 0_u32;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    reader.numbers().expect("no loss while a writer appends");
                    reads += 1;
                }
                reads
            });
            for i in 0..150 {
                let (old, new) = if i % 2 == 0 { (None, A) } else { (Some(A), B) };
                log.append(&change("main", old, new)).unwrap();
            }
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            assert!(handle.join().unwrap() > 0);
        });
        assert_eq!(log.numbers().unwrap().len(), 150);
    }

    #[test]
    fn a_missing_entry_is_reported_by_its_number() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        assert!(matches!(
            log.read(1),
            Err(OpLogError::Missing { number: 1 })
        ));
        let message = log.read(7).unwrap_err().to_string();
        assert!(message.contains('7'), "{message}");
    }

    #[test]
    fn a_gap_in_the_numbers_is_reported_and_not_skipped() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        for n in 1..=3 {
            log.append(&change("main", None, if n % 2 == 0 { A } else { B }))
                .unwrap();
        }
        fs::remove_file(dir.path().join("oplog").join("000000000002")).unwrap();
        assert!(matches!(log.numbers(), Err(OpLogError::Gap { missing: 2 })));
        assert!(matches!(log.entries(), Err(OpLogError::Gap { missing: 2 })));
        // A writer needs only the end of the log: it takes the number after the largest, and the
        // reader still reports the gap.
        assert_eq!(log.append(&change("main", None, A)).unwrap().number(), 4);
        assert!(matches!(log.numbers(), Err(OpLogError::Gap { missing: 2 })));
        let message = log.numbers().unwrap_err().to_string();
        assert!(
            message.contains("entry 2") && message.contains("lost"),
            "{message}"
        );
    }

    #[test]
    fn the_log_holds_nothing_but_entries_and_skips_names_with_a_dot() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        log.append(&change("main", None, A)).unwrap();
        let oplog = dir.path().join("oplog");
        // File managers and editors leave names with a dot, as for the references.
        for name in [".DS_Store", ".hidden", "._000000000001"] {
            fs::write(oplog.join(name), b"x").unwrap();
        }
        assert_eq!(log.numbers().unwrap(), [1]);
        assert_eq!(log.append(&change("main", Some(A), B)).unwrap().number(), 2);
        // Any other name is reported with its path, also on the way to a write.
        for name in [
            "000000000003.tmp",
            "1",
            "00000000000a",
            "0000000000000",
            "000000000000",
            "entry",
        ] {
            let path = oplog.join(name);
            fs::write(&path, b"x").unwrap();
            for error in [
                log.numbers().unwrap_err(),
                log.append(&change("main", None, A)).unwrap_err(),
            ] {
                assert!(
                    matches!(&error, OpLogError::UnexpectedFile { path: found } if found == &path),
                    "{name}: {error}"
                );
            }
            fs::remove_file(path).unwrap();
        }
        assert_eq!(log.numbers().unwrap(), [1, 2]);
    }

    #[test]
    fn a_directory_in_the_place_of_an_entry_cannot_be_read_as_one() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        fs::create_dir(dir.path().join("oplog").join("000000000001")).unwrap();
        // The name is that of an entry, so the listing accepts it; reading it fails.
        assert_eq!(log.numbers().unwrap(), [1]);
        let error = log.read(1).unwrap_err();
        assert!(matches!(error, OpLogError::Io { .. }), "{error}");
        assert!(error.io_error().is_some());
    }

    #[test]
    fn a_file_that_is_larger_than_an_entry_is_not_read_in_full() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        fs::write(
            dir.path().join("oplog").join("000000000001"),
            vec![b'x'; 70_000],
        )
        .unwrap();
        let error = log.read(1).unwrap_err();
        assert!(
            matches!(
                &error,
                OpLogError::Corrupt {
                    source: EntryParseError::Malformed {
                        reason: EntryMalformed::TooLong,
                        ..
                    },
                    ..
                }
            ),
            "{error}"
        );
        assert!(error.to_string().contains("too large"), "{error}");
    }

    #[test]
    fn a_damaged_entry_is_reported_with_its_file_and_does_not_touch_the_others() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        log.append(&change("main", None, A)).unwrap();
        log.append(&change("main", Some(A), B)).unwrap();
        let path = dir.path().join("oplog").join("000000000002");
        let mut bytes = fs::read(&path).unwrap();
        bytes.pop();
        fs::remove_file(&path).unwrap();
        fs::write(&path, &bytes).unwrap();
        let error = log.read(2).unwrap_err();
        assert!(
            matches!(
                &error,
                OpLogError::Corrupt {
                    source: EntryParseError::Malformed {
                        reason: EntryMalformed::CutOff,
                        ..
                    },
                    ..
                }
            ),
            "{error}"
        );
        assert!(error.to_string().contains("000000000002"), "{error}");
        assert!(log.read(1).is_ok());
    }

    #[test]
    fn a_number_inside_that_differs_from_the_name_is_reported() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        log.append(&change("main", None, A)).unwrap();
        let text = fs::read_to_string(dir.path().join("oplog").join("000000000001")).unwrap();
        let moved_to = dir.path().join("oplog").join("000000000001");
        fs::remove_file(&moved_to).unwrap();
        fs::write(&moved_to, text.replace("number 1", "number 5")).unwrap();
        assert!(matches!(
            log.read(1),
            Err(OpLogError::WrongNumber {
                expected: 1,
                found: 5,
                ..
            })
        ));
    }

    #[test]
    fn an_append_without_a_directory_fails_and_leaves_nothing() {
        let dir = TempDir::new();
        fs::create_dir(dir.path().join("tmp")).unwrap();
        let error = FsOpLog::new(dir.path())
            .append(&change("main", None, A))
            .unwrap_err();
        assert!(matches!(error, OpLogError::Io { .. }), "{error}");
        assert!(error.io_error().is_some());
        assert_eq!(files(&dir.path().join("tmp")), Vec::<String>::new());
        // And without the directory for temporary files.
        let dir = TempDir::new();
        fs::create_dir(dir.path().join("oplog")).unwrap();
        let error = FsOpLog::new(dir.path())
            .append(&change("main", None, A))
            .unwrap_err();
        assert!(error.io_error().is_some(), "{error}");
        assert_eq!(files(&dir.path().join("oplog")), Vec::<String>::new());
    }

    #[test]
    fn writers_at_the_same_moment_get_different_numbers_without_a_gap() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        let barrier = Barrier::new(8);
        let numbers: Vec<Vec<u64>> = thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|writer| {
                    let (log, barrier) = (&log, &barrier);
                    scope.spawn(move || {
                        barrier.wait();
                        (0..5)
                            .map(|round| {
                                let name = format!("w{writer}-{round}");
                                log.append(&change(&name, None, A)).unwrap().number()
                            })
                            .collect()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let mut all: Vec<u64> = numbers.into_iter().flatten().collect();
        all.sort_unstable();
        assert_eq!(all, (1..=40).collect::<Vec<u64>>());
        assert_eq!(log.numbers().unwrap(), (1..=40).collect::<Vec<u64>>());
        // Every entry is whole and holds the writer that made it.
        let mut names: Vec<String> = log
            .entries()
            .unwrap()
            .iter()
            .map(|e| e.change().unwrap().branches()[0].0.to_string())
            .collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 40);
        assert_eq!(files(&dir.path().join("tmp")), Vec::<String>::new());
    }

    fn link(from: &Path, to: &Path) -> io::Result<()> {
        fs::hard_link(from, to)
    }

    fn sync(dir: &Path) -> io::Result<()> {
        match sync_directory(dir) {
            Ok(()) => Ok(()),
            Err(StoreError::Io { source, .. }) => Err(source),
            Err(other) => Err(io::Error::other(other.to_string())),
        }
    }

    #[test]
    fn a_writer_that_loses_a_number_looks_for_the_end_again_and_cleans_up() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        log.append(&change("main", None, A)).unwrap();
        // The writer has chosen 2. Before it links, other writers take 2 and 3 and 4.
        let mut taken = Vec::new();
        let mut before_link = |number: u64| {
            taken.push(number);
            if number == 2 {
                for n in 0..3 {
                    log.append(&change(&format!("w{n}"), None, B)).unwrap();
                }
            }
        };
        let entry = log
            .publish(
                &Record::Change(change("late", None, C)),
                2,
                &mut before_link,
                &link,
                &sync,
            )
            .unwrap();
        // The second try is not 3: the writer looked at the end of the log, which was 4, and took 5.
        assert_eq!(taken, [2, 5]);
        assert_eq!(entry.number(), 5);
        assert_eq!(log.numbers().unwrap(), [1, 2, 3, 4, 5]);
        assert_eq!(
            log.read(5).unwrap().change().unwrap().branches()[0].0,
            branch("late")
        );
        // The second try named the hash of the entry that the last of the others wrote.
        assert_eq!(
            log.read(5).unwrap().chain(),
            Chain::After(log.read(4).unwrap().hash().unwrap())
        );
        assert!(log.entries().is_ok());
        // The three entries of the others are whole, and nothing is left in `tmp/`.
        for n in 2..=4 {
            assert_eq!(
                log.read(n).unwrap().change().unwrap().operation().as_str(),
                "commit"
            );
        }
        assert_eq!(files(&dir.path().join("tmp")), Vec::<String>::new());
    }

    #[test]
    fn a_writer_that_is_overtaken_every_time_gives_up_as_contended_and_leaves_nothing() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        // Another writer takes each number first: the name exists when this writer links it. The
        // entries of the other writer are those of this one, so the chain holds.
        let always_taken = |from: &Path, to: &Path| {
            fs::hard_link(from, to)?;
            Err(io::ErrorKind::AlreadyExists.into())
        };
        let error = log
            .publish(
                &Record::Change(change("main", None, A)),
                1,
                &mut |_| {},
                &always_taken,
                &sync,
            )
            .unwrap_err();
        assert!(
            matches!(error, OpLogError::Contended { attempts } if attempts == ATTEMPTS),
            "{error}"
        );
        assert!(error.to_string().contains("busy"), "{error}");
        assert_eq!(files(&dir.path().join("tmp")), Vec::<String>::new());
        // Each attempt named the hash of the entry the other writer had written before it.
        assert_eq!(log.entries().unwrap().len(), ATTEMPTS);
    }

    #[test]
    fn the_largest_number_is_written_and_the_next_one_is_a_full_log() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        // The entry before, written before the chain, so that the next one starts it.
        let before = Entry::of(MAX_NUMBER - 1, Record::Change(change("z", None, C))).unwrap();
        fs::write(log.path_of(MAX_NUMBER - 1), before.encode()).unwrap();
        let entry = log
            .publish(
                &Record::Change(change("a", None, A)),
                MAX_NUMBER,
                &mut |_| {},
                &link,
                &sync,
            )
            .unwrap();
        assert_eq!(entry.number(), MAX_NUMBER);
        assert_eq!(entry.chain(), Chain::Start);
        assert_eq!(
            files(&dir.path().join("oplog")),
            ["999999999998", "999999999999"]
        );
        // The file is there, and a reader reports the gap before it, as for any other loss.
        assert!(matches!(log.numbers(), Err(OpLogError::Gap { missing: 1 })));
        assert_eq!(log.read(MAX_NUMBER).unwrap(), entry);
        // A writer that is at the end finds that there is no number after it.
        let error = log
            .append(&change("b", None, B))
            .expect_err("the next number is out of range");
        assert!(matches!(error, OpLogError::Full), "{error}");
        assert!(error.to_string().contains("999999999999"), "{error}");
        assert_eq!(files(&dir.path().join("tmp")), Vec::<String>::new());
        let error = log
            .publish(
                &Record::Change(change("c", None, C)),
                MAX_NUMBER + 1,
                &mut |_| {},
                &link,
                &sync,
            )
            .unwrap_err();
        assert!(matches!(error, OpLogError::Full), "{error}");
    }

    #[test]
    fn a_failing_flush_leaves_the_entry_and_carries_it() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        log.append(&change("main", None, A)).unwrap();
        let failing = |_: &Path| Err(io::Error::other("disk"));
        let error = log
            .publish(
                &Record::Change(change("main", Some(A), B)),
                2,
                &mut |_| {},
                &link,
                &failing,
            )
            .unwrap_err();
        let OpLogError::NotFlushed { entry, .. } = &error else {
            panic!("not a failed flush: {error}");
        };
        // The error carries the whole entry, not only its number.
        assert_eq!(entry.number(), 2);
        assert_eq!(entry.change().unwrap().branches()[0].0, branch("main"));
        let message = error.to_string();
        assert!(
            message.starts_with("entry 2 was written") && message.contains("disk"),
            "{message}"
        );
        assert!(error.io_error().is_some());
        // The entry is in the log, whole, and the temporary name is gone.
        assert_eq!(log.numbers().unwrap(), [1, 2]);
        assert_eq!(&log.read(2).unwrap(), entry.as_ref());
        assert_eq!(files(&dir.path().join("tmp")), Vec::<String>::new());
    }

    #[test]
    fn a_failing_link_is_an_error_that_names_the_entry_and_leaves_nothing() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        let refused = |_: &Path, _: &Path| Err(io::Error::from(io::ErrorKind::PermissionDenied));
        let error = log
            .publish(
                &Record::Change(change("main", None, A)),
                1,
                &mut |_| {},
                &refused,
                &sync,
            )
            .unwrap_err();
        assert!(
            matches!(&error, OpLogError::Io { operation, path, .. }
                if operation.contains("hard link") && path.ends_with("000000000001")),
            "{error}"
        );
        assert!(error.io_error().is_some());
        assert_eq!(files(&dir.path().join("tmp")), Vec::<String>::new());
        assert_eq!(files(&dir.path().join("oplog")), Vec::<String>::new());
    }

    #[test]
    fn an_oplog_directory_that_cannot_be_written_fails_at_the_link_and_leaves_nothing() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        // The link is refused the way a directory without write permission refuses it.
        let refuse = |_: &Path, _: &Path| Err(io::Error::from(io::ErrorKind::PermissionDenied));
        let error = log
            .publish(
                &Record::Change(change("main", None, A)),
                1,
                &mut |_| {},
                &refuse,
                &sync,
            )
            .unwrap_err();
        assert!(
            matches!(&error, OpLogError::Io { operation, .. } if operation.contains("hard link")),
            "{error}"
        );
        assert_eq!(files(&dir.path().join("tmp")), Vec::<String>::new());
        assert_eq!(files(&dir.path().join("oplog")), Vec::<String>::new());
    }

    #[test]
    fn a_temporary_name_that_cannot_be_removed_does_not_stop_a_whole_entry() {
        // `tmp/` holds scratch files. If the name cannot be removed after the link, the entry is
        // still whole and flushed, and the write succeeds: a cleanup must not stop a move.
        let dir = TempDir::new();
        let log = log_in(&dir);
        let tmp = dir.path().join("tmp");
        let mut denied = None;
        let mut make_tmp_read_only = |_: u64| {
            denied = Some(fault::deny_writes_in(&tmp));
        };
        let result = log.publish(
            &Record::Change(change("main", None, A)),
            1,
            &mut make_tmp_read_only,
            &link,
            &sync,
        );
        let left = files(&tmp);
        drop(denied);
        let entry = result.expect("the entry is whole, so the write succeeds");
        assert_eq!(entry.number(), 1);
        assert_eq!(log.numbers().unwrap(), [1]);
        assert_eq!(log.read(1).unwrap(), entry);
        // The name stays in `tmp/` and is scratch; the entry has its own name.
        assert_eq!(left.len(), 1);
        assert!(!left[0].starts_with('0'), "{left:?}");
    }

    #[test]
    fn an_entry_that_is_larger_than_an_entry_can_be_is_refused_and_nothing_is_written() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        let size_of = |id_len: usize| {
            let actor = Actor::human("a".repeat(id_len)).unwrap();
            let change = Change::try_new(
                TIME,
                actor,
                operation("commit"),
                None,
                None,
                vec![(branch("main"), moved(None, Some(A)))],
            )
            .unwrap();
            (
                change.clone(),
                Entry::linked(1, Chain::Start, Record::Change(change))
                    .unwrap()
                    .encode()
                    .len(),
            )
        };
        // Find the id length that gives exactly the largest entry.
        let (_, base) = size_of(1);
        let fit = MAX_ENTRY_LEN as usize - base + 1;
        let (change_fit, size) = size_of(fit);
        assert_eq!(size as u64, MAX_ENTRY_LEN);
        // The largest entry that the writer makes can be read back.
        let entry = log.append(&change_fit).unwrap();
        assert_eq!(log.read(1).unwrap(), entry);
        // The next entry names the hash of the first, so a change of the same size is too large.
        let size = Entry::linked(
            2,
            Chain::After(entry.hash().unwrap()),
            Record::Change(change_fit),
        )
        .unwrap()
        .encode()
        .len();
        assert!(size as u64 > MAX_ENTRY_LEN);
        let (change_over, _) = size_of(fit);
        let error = log.append(&change_over).unwrap_err();
        assert!(
            matches!(error, OpLogError::EntryTooLarge { size: refused } if refused == size),
            "{error}"
        );
        assert!(error.to_string().contains("nothing was written"), "{error}");
        assert_eq!(log.numbers().unwrap(), [1]);
        assert_eq!(files(&dir.path().join("tmp")), Vec::<String>::new());
    }

    #[test]
    fn errors_of_the_log_follow_the_convention_for_messages_and_causes() {
        #[derive(Debug)]
        struct Outer;
        impl fmt::Display for Outer {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("outer")
            }
        }
        impl Error for Outer {
            fn source(&self) -> Option<&(dyn Error + 'static)> {
                Some(&Deepest)
            }
        }
        #[derive(Debug)]
        struct Deepest;
        impl fmt::Display for Deepest {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("deepest")
            }
        }
        impl Error for Deepest {}
        let path = PathBuf::from("/x/.stemma/oplog/000000000001");
        let io_error = OpLogError::Io {
            operation: "read the entry",
            path: path.clone(),
            source: io::Error::other(Outer),
        };
        assert_eq!(
            io_error.to_string(),
            "could not read the entry `/x/.stemma/oplog/000000000001`: outer"
        );
        assert_eq!(io_error.source().unwrap().to_string(), "deepest");
        assert_eq!(io_error.io_error().unwrap().to_string(), "outer");
        let not_flushed = OpLogError::NotFlushed {
            entry: Box::new(Entry::try_new(7, change("main", None, A)).unwrap()),
            path: PathBuf::from("/x/oplog"),
            source: io::Error::other(Outer),
        };
        let message = not_flushed.to_string();
        assert!(
            message.contains("entry 7 was written")
                && message.contains("flushing `/x/oplog` failed: outer"),
            "{message}"
        );
        assert!(message.contains("may not survive"), "{message}");
        assert!(not_flushed.io_error().is_some());
        for (error, text) in [
            (OpLogError::Missing { number: 3 }, "no entry 3"),
            (OpLogError::Full, "full"),
            (OpLogError::Contended { attempts: 64 }, "64 numbers"),
            (OpLogError::EntryTooLarge { size: 70_000 }, "70000 bytes"),
            (OpLogError::Gap { missing: 2 }, "lost"),
            (
                OpLogError::UnexpectedFile { path: path.clone() },
                "is not an entry",
            ),
            (
                OpLogError::WrongNumber {
                    path: path.clone(),
                    expected: 1,
                    found: 2,
                },
                "number 2 inside",
            ),
            (OpLogError::Entry(EntryError::NoChange), "no pointer"),
        ] {
            let message = error.to_string();
            assert!(message.contains(text), "{message}");
            assert!(error.source().is_none());
            assert!(error.io_error().is_none());
        }
        let corrupt = OpLogError::Corrupt {
            path,
            source: EntryParseError::Unchanged { line: 6 },
        };
        assert!(corrupt.to_string().contains("line 6"), "{corrupt}");
    }

    // Stubs and purges (ADR-014)

    /// A log with three changes, then the entry of a purge that lists 2 and 3, with entries 2
    /// and 3 replaced by stubs.
    fn purged_log(dir: &TempDir) -> FsOpLog {
        let log = log_in(dir);
        log.append(&change("a", None, A)).unwrap();
        log.append(&change("b", None, B)).unwrap();
        log.append(&change("c", None, C)).unwrap();
        let written = log
            .append_purge(Purge::try_new(TIME + 9, human(), 2, &[3, 2]).unwrap())
            .unwrap();
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].number(), 4);
        log.replace_with_stub(2).unwrap();
        log.replace_with_stub(3).unwrap();
        log
    }

    #[test]
    fn a_stub_and_a_purge_have_one_encoding_each() {
        let stub = Entry::try_new(5, change("a", None, A))
            .unwrap()
            .to_stub()
            .unwrap();
        let text = "stemma-oplog 2\nnumber 5\ntime 1700000000\nactor human daniel\nstub\n";
        assert_eq!(String::from_utf8(stub.encode()).unwrap(), text);
        assert_eq!(Entry::parse(text.as_bytes()).unwrap(), stub);

        let purge = Entry::of(
            9,
            Record::Purge(Purge::try_new(TIME, human(), 3, &[7, 4, 5, 1]).unwrap()),
        )
        .unwrap();
        let text = "stemma-oplog 2\nnumber 9\ntime 1700000000\nactor human daniel\n\
                    operation purge\ncommits 3\nstubs 1 4-5 7\n";
        assert_eq!(String::from_utf8(purge.encode()).unwrap(), text);
        assert_eq!(Entry::parse(text.as_bytes()).unwrap(), purge);

        let none = Entry::of(
            2,
            Record::Purge(Purge::try_new(TIME, human(), 0, &[]).unwrap()),
        )
        .unwrap();
        let text = "stemma-oplog 2\nnumber 2\ntime 1700000000\nactor human daniel\n\
                    operation purge\ncommits 0\nstubs -\n";
        assert_eq!(String::from_utf8(none.encode()).unwrap(), text);
        assert_eq!(Entry::parse(text.as_bytes()).unwrap(), none);
    }

    #[test]
    fn a_purge_that_moved_tags_counts_them_in_a_line_of_its_own() {
        let purge = Entry::of(
            9,
            Record::Purge(
                Purge::try_new(TIME, human(), 3, &[4, 5, 7])
                    .unwrap()
                    .with_tags(2)
                    .unwrap(),
            ),
        )
        .unwrap();
        let text = "stemma-oplog 2\nnumber 9\ntime 1700000000\nactor human daniel\n\
                    operation purge\ncommits 3\ntags 2\nstubs 4-5 7\n";
        assert_eq!(String::from_utf8(purge.encode()).unwrap(), text);
        assert_eq!(Entry::parse(text.as_bytes()).unwrap(), purge);
        match purge.record() {
            Record::Purge(purge) => assert_eq!(purge.tags(), 2),
            other => panic!("{other:?}"),
        }
        // No tags: no line, so that the entries from before the line keep their encoding.
        let none = Entry::of(
            9,
            Record::Purge(Purge::try_new(TIME, human(), 3, &[4, 5, 7]).unwrap()),
        )
        .unwrap();
        assert!(!String::from_utf8(none.encode()).unwrap().contains("tags"));
        // The hash of the body covers the count, so a change of it breaks the chain.
        let chained = |entry: &Entry| {
            Entry::linked(9, Chain::Start, entry.record().clone())
                .unwrap()
                .body_hash()
        };
        assert_ne!(chained(&none), chained(&purge));
        // `tags 0`, a second line, and a line out of place are not canonical.
        let start = "stemma-oplog 2\nnumber 9\ntime 1700000000\nactor human daniel\n";
        let malformed = |rest: &str| match Entry::parse(format!("{start}{rest}").as_bytes()) {
            Err(EntryParseError::Malformed { line, reason }) => (line, reason),
            other => panic!("{rest:?}: {other:?}"),
        };
        assert_eq!(
            malformed("operation purge\ncommits 3\ntags 0\nstubs 4\n"),
            (7, EntryMalformed::BadTags)
        );
        assert_eq!(
            malformed("operation purge\ncommits 3\ntags x\nstubs 4\n"),
            (7, EntryMalformed::BadTags)
        );
        assert_eq!(
            malformed("operation purge\ncommits 3\ntags 1\ntags 1\nstubs 4\n"),
            (8, EntryMalformed::ExpectedStubs)
        );
        assert_eq!(
            malformed("operation purge\ncommits 3\nstubs 4\ntags 1\n"),
            (8, EntryMalformed::TrailingLine)
        );
    }

    #[test]
    fn the_reader_rejects_every_other_form_of_a_stub_or_a_purge() {
        let start = "stemma-oplog 2\nnumber 9\ntime 1700000000\nactor human daniel\n";
        for rest in [
            "",
            "stub\nextra\n",
            "stub \n",
            "operation snapshot\n",
            "operation purge\n",
            "operation purge\ncommits 01\nstubs 1\n",
            "operation purge\ncommits 1\n",
            "operation purge\ncommits 1\nstubs \n",
            "operation purge\ncommits 1\nstubs 2 1\n",
            "operation purge\ncommits 1\nstubs 1 2\n",
            "operation purge\ncommits 1\nstubs 1-2 2\n",
            "operation purge\ncommits 1\nstubs 3-3\n",
            "operation purge\ncommits 1\nstubs 1,2\n",
            "operation purge\ncommits 1\nstubs 0\n",
            "operation purge\ncommits 1\nstubs 9\n",
            "operation purge\ncommits 1\nstubs 1\nmore\n",
            "operation purge\ncommits 1\nstubs 2-1\n",
            "operation purge\ncommits 1\nstubs 1-\n",
            "operation purge\ncommits 1\nstubs -1\n",
            "operation purge\ncommits 1\nstubs - 1\n",
            "operation purge\ncommits 1\nstubs 1  3\n",
            "operation purge\ncommits 1\nstubs 1 \n",
            "operation purge\ncommits 1\nstubs 01\n",
            "operation purge\ncommits 1\nstubs 1-2 3\n",
            "operation purge\ncommits \nstubs 1\n",
            "operation purge\ncommits +1\nstubs 1\n",
        ] {
            let text = format!("{start}{rest}");
            assert!(Entry::parse(text.as_bytes()).is_err(), "{text:?}");
        }
        let malformed = |rest: &str| match Entry::parse(format!("{start}{rest}").as_bytes()) {
            Err(EntryParseError::Malformed { line, reason }) => (line, reason),
            other => panic!("{rest:?}: {other:?}"),
        };
        assert_eq!(malformed(""), (5, EntryMalformed::ExpectedStubOrPurge));
        assert_eq!(malformed("stub\nx\n"), (6, EntryMalformed::TrailingLine));
        assert_eq!(
            malformed("operation purge\n"),
            (6, EntryMalformed::ExpectedCommits)
        );
        assert_eq!(
            malformed("operation purge\ncommits 1\n"),
            (7, EntryMalformed::ExpectedStubs)
        );
        assert_eq!(
            malformed("operation purge\ncommits 1\nstubs 2 1\n"),
            (7, EntryMalformed::BadStubs)
        );
        assert_eq!(
            malformed("operation purge\ncommits 1\nstubs 1\nx\n"),
            (8, EntryMalformed::TrailingLine)
        );
        // Version 1 never holds a stub.
        assert!(Entry::parse(
            b"stemma-oplog 1\nnumber 9\ntime 1700000000\nactor human daniel\nstub\n"
        )
        .is_err());
        // A change cannot take the name of a purge, in either version.
        let text = "stemma-oplog 1\nnumber 3\ntime 1700000000\nactor human daniel\n\
                    operation purge\nbranch main - "
            .to_owned()
            + A
            + "\n";
        assert!(matches!(
            Entry::parse(text.as_bytes()),
            Err(EntryParseError::Entry(EntryError::ReservedOperation))
        ));
        assert!(matches!(
            Entry::parse(b"stemma-oplog 5\n"),
            Err(EntryParseError::UnsupportedVersion(version)) if version == "5"
        ));
    }

    #[test]
    fn a_stub_in_the_chain_names_what_it_replaced_in_version_4() {
        let body = BodyHash::of("");
        let stub = |replaced| {
            let stub = Stub {
                time: TIME,
                actor: human(),
                body: Some(body),
                replaced,
            };
            Entry::linked(2, Chain::After(EntryHash::of("x")), Record::Stub(stub)).unwrap()
        };
        for (kind, name) in [(StubKind::Approval, "approval"), (StubKind::Other, "other")] {
            let entry = stub(Some(kind));
            let text = String::from_utf8(entry.encode()).unwrap();
            assert!(text.starts_with("stemma-oplog 4\n"), "{text}");
            assert!(text.ends_with(&format!("stub {body} {name}\n")), "{text}");
            assert_eq!(Entry::parse(text.as_bytes()).unwrap(), entry);
        }
        // A stub without a kind is the older one: version 3, and the hash does not change.
        let old = stub(None);
        // A kind has no place outside the chain.
        let outside = Stub {
            time: TIME,
            actor: human(),
            body: None,
            replaced: Some(StubKind::Other),
        };
        assert_eq!(
            Entry::linked(2, Chain::Unchained, Record::Stub(outside)),
            Err(EntryError::StubBodyOutsideChain(2))
        );
        assert!(old.encode().starts_with(b"stemma-oplog 3\n"));
        assert_eq!(old.hash(), stub(Some(StubKind::Other)).hash());
        // Version 4 has the kind, and nothing else.
        let text = String::from_utf8(stub(Some(StubKind::Other)).encode()).unwrap();
        for bad in [
            text.replace(" other\n", "\n"),
            text.replace(" other\n", " approve\n"),
            text.replace("stemma-oplog 4", "stemma-oplog 3"),
        ] {
            assert!(Entry::parse(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_purge_replaces_entries_by_stubs_that_keep_number_time_and_actor() {
        let dir = TempDir::new();
        let log = purged_log(&dir);
        let entries = log.entries().unwrap();
        let numbers: Vec<u64> = entries.iter().map(Entry::number).collect();
        assert_eq!(numbers, [1, 2, 3, 4], "no gap");
        assert!(entries[0].change().is_some());
        for stub in &entries[1..3] {
            assert!(matches!(stub.record(), Record::Stub(_)));
            assert_eq!(stub.time(), TIME);
            assert_eq!(stub.actor(), &human());
            assert!(stub.commits().is_empty());
        }
        match entries[3].record() {
            Record::Purge(purge) => {
                assert_eq!(purge.stubs(), [(2, 3)]);
                assert_eq!(purge.commits(), 2);
                assert!(purge.lists(2) && purge.lists(3) && !purge.lists(1));
            }
            other => panic!("expected a purge, got {other:?}"),
        }
        let text = fs::read_to_string(dir.path().join("oplog/000000000002")).unwrap();
        for id in [A, B, C] {
            assert!(!text.contains(id), "a stub names no ID: {text}");
        }
        assert_eq!(files(&dir.path().join("tmp")), Vec::<String>::new());
    }

    #[test]
    fn replacing_a_stub_again_changes_nothing_and_a_purge_cannot_be_replaced() {
        let dir = TempDir::new();
        let log = purged_log(&dir);
        let before = fs::read(dir.path().join("oplog/000000000002")).unwrap();
        log.replace_with_stub(2).unwrap();
        assert_eq!(
            fs::read(dir.path().join("oplog/000000000002")).unwrap(),
            before
        );
        assert!(matches!(
            log.replace_with_stub(4),
            Err(OpLogError::Entry(EntryError::PurgeNotReplaceable(4)))
        ));
        assert!(matches!(
            log.replace_with_stub(9),
            Err(OpLogError::Missing { .. })
        ));
    }

    #[test]
    fn a_stub_needs_a_given_purge_entry_after_it_that_lists_it() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        log.append(&change("a", None, A)).unwrap();
        log.append(&change("b", None, B)).unwrap();
        let written = log
            .append_purge(Purge::try_new(TIME + 9, human(), 1, &[2]).unwrap())
            .unwrap();
        let before = fs::read(dir.path().join("oplog/000000000002")).unwrap();
        let others = log.entries().unwrap()[..2].to_vec();
        for purges in [&[][..], &others[..]] {
            assert!(matches!(
                log.replace_with_stub_listed(2, purges),
                Err(OpLogError::NotListed { number: 2 })
            ));
        }
        assert!(matches!(
            log.replace_with_stub_listed(1, &written),
            Err(OpLogError::NotListed { number: 1 })
        ));
        assert_eq!(
            fs::read(dir.path().join("oplog/000000000002")).unwrap(),
            before,
            "nothing is written"
        );
        let stub = log.replace_with_stub_listed(2, &written).unwrap();
        assert_eq!(log.read(2).unwrap(), stub);
        assert_eq!(log.entries().unwrap().len(), 3);
    }

    #[test]
    fn a_stub_that_no_purge_lists_is_an_error_of_the_log() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        log.append(&change("a", None, A)).unwrap();
        log.append(&change("b", None, B)).unwrap();
        // The writer refuses to make such a stub, so it is written by hand, as an accident would.
        assert!(matches!(
            log.replace_with_stub(1),
            Err(OpLogError::NotListed { number: 1 })
        ));
        let stub = log.read(1).unwrap().to_stub().unwrap();
        let path = dir.path().join("oplog/000000000001");
        fs::write(&path, stub.encode()).unwrap();
        let error = log.entries().unwrap_err();
        assert!(
            matches!(error, OpLogError::UnlistedStub { number: 1 }),
            "{error:?}"
        );
        assert!(error.to_string().contains("no purge entry lists it"));
    }

    #[test]
    fn a_listed_entry_that_is_not_a_stub_yet_is_no_error() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        log.append(&change("a", None, A)).unwrap();
        log.append_purge(Purge::try_new(TIME, human(), 1, &[1]).unwrap())
            .unwrap();
        assert_eq!(
            log.entries().unwrap().len(),
            2,
            "a purge stopped before its stubs"
        );
    }

    #[test]
    fn a_purge_must_come_after_the_entries_it_lists() {
        let purge = Record::Purge(Purge::try_new(TIME, human(), 1, &[2, 5]).unwrap());
        assert_eq!(
            Entry::of(5, purge.clone()),
            Err(EntryError::StubNotBefore { stub: 5, purge: 5 })
        );
        Entry::of(6, purge).unwrap();
    }

    #[test]
    fn a_long_list_of_stubs_is_split_over_entries_that_fit() {
        // Every second number, so that no two merge into a range.
        let numbers: Vec<u64> = (1..=20_000).map(|n| n * 2 + 100_000_000_000).collect();
        let parts = Purge::try_new(TIME, human(), 7, &numbers).unwrap().split();
        assert!(parts.len() > 1, "{} parts", parts.len());
        assert_eq!(parts[0].commits(), 7);
        assert!(parts[1..].iter().all(|part| part.commits() == 0));
        let listed: usize = parts.iter().map(|part| part.stubs().len()).sum();
        assert_eq!(listed, numbers.len());
        for part in &parts {
            let size = Entry::of(MAX_NUMBER, Record::Purge(part.clone()))
                .unwrap()
                .encode()
                .len() as u64;
            assert!(size <= MAX_ENTRY_LEN, "{size}");
        }
    }

    #[test]
    fn a_stub_makes_every_entry_before_it_unknown_and_a_purge_does_not() {
        let first = entry(1, None, vec![moves("main", None, Some(A))]);
        let stub = Entry {
            number: 2,
            ..first.to_stub().unwrap()
        };
        let purge = Entry::of(
            3,
            Record::Purge(Purge::try_new(TIME, human(), 1, &[2]).unwrap()),
        )
        .unwrap();
        let after = entry(4, None, vec![moves("other", None, Some(B))]);
        let result = outcomes(
            &[first.clone(), stub, purge.clone(), after],
            now(&[("main", A), ("other", B)]).as_ref(),
        );
        assert_eq!(
            result,
            [
                Outcome::Unknown,
                Outcome::Unknown,
                Outcome::Unknown,
                Outcome::Took
            ]
        );
        // Without the stub, the purge entry is no barrier.
        let purge = Entry { number: 2, ..purge };
        let alone = Entry::of(
            2,
            Record::Purge(Purge::try_new(TIME, human(), 0, &[]).unwrap()),
        )
        .unwrap();
        assert_eq!(purge.number(), alone.number());
        let result = outcomes(&[first, alone], now(&[("main", A)]).as_ref());
        assert_eq!(result, [Outcome::Took, Outcome::Unknown]);
    }

    #[test]
    fn a_stub_that_moved_a_pointer_back_does_not_turn_into_a_false_verdict() {
        // 1 moves main A -> B; 2 (the secret, B -> C) and 3 (a restore, C -> A) became stubs; 4
        // moves main A -> B again. Without the barrier, 4 would refute 1, as it starts at A.
        let first = entry(1, None, vec![moves("main", Some(A), Some(B))]);
        let second = entry(2, None, vec![moves("main", Some(B), Some(C))]);
        let third = entry(3, None, vec![moves("main", Some(C), Some(A))]);
        let fourth = entry(4, None, vec![moves("main", Some(A), Some(B))]);
        let stub = |entry: &Entry| entry.to_stub().unwrap();
        let purge = Entry::of(
            5,
            Record::Purge(Purge::try_new(TIME, human(), 2, &[2, 3]).unwrap()),
        )
        .unwrap();
        let result = outcomes(
            &[first, stub(&second), stub(&third), fourth, purge],
            now(&[("main", B)]).as_ref(),
        );
        assert_eq!(result[0], Outcome::Unknown);
        assert_eq!(result[3], Outcome::Took);
    }

    #[test]
    fn every_purge_that_the_writer_accepts_reads_back_equal() {
        let cases: Vec<(u64, Vec<u64>)> = vec![
            (0, vec![]),
            (1, vec![1]),
            (2, vec![1, 2]),
            (3, vec![1, 3, 4, 4, 6]),
            (MAX_NUMBER, vec![MAX_NUMBER - 1]),
            (5, vec![MAX_NUMBER - 2, MAX_NUMBER - 1, 7]),
        ];
        for (commits, stubs) in cases {
            let purge = Purge::try_new(TIME, human(), commits, &stubs).unwrap();
            let entry = Entry::of(MAX_NUMBER, Record::Purge(purge)).unwrap();
            assert_eq!(Entry::parse(&entry.encode()).unwrap(), entry, "{stubs:?}");
        }
        assert_eq!(
            Purge::try_new(TIME, human(), 1, &[0, 1]),
            Err(EntryError::StubOutOfRange(0))
        );
        assert_eq!(
            Purge::try_new(TIME, human(), 1, &[1, MAX_NUMBER + 1]),
            Err(EntryError::StubOutOfRange(MAX_NUMBER + 1))
        );
        assert_eq!(
            Purge::try_new(TIME, human(), MAX_NUMBER + 1, &[1]),
            Err(EntryError::TooManyCommits(MAX_NUMBER + 1))
        );
        assert!(EntryError::StubOutOfRange(0)
            .to_string()
            .contains("cannot list entry 0 as a stub"));
    }

    /// Writes a change under each number by hand, as `append` only writes at the end.
    fn changes_at(dir: &TempDir, numbers: &[u64]) {
        for number in numbers {
            fs::write(
                dir.path().join("oplog").join(format!("{number:012}")),
                Entry::try_new(*number, change("a", None, A))
                    .unwrap()
                    .encode(),
            )
            .unwrap();
        }
    }

    /// Numbers wide enough and far enough apart that a purge of them does not fit one entry, and
    /// below `below`.
    fn wide_numbers(below: u64) -> Vec<u64> {
        (1..=6_000).map(|n| below - 2 * n).collect()
    }

    #[test]
    fn a_purge_that_does_not_fit_is_written_in_parts_at_the_following_numbers() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        let mut numbers = wide_numbers(100_000_100_000);
        numbers.sort_unstable();
        changes_at(&dir, &numbers);
        // The last entry before the purge, which comes after the stubs.
        changes_at(&dir, &[100_000_100_000]);
        let written = log
            .append_purge(Purge::try_new(TIME, human(), 7, &numbers).unwrap())
            .unwrap();
        assert!(written.len() > 1);
        for (index, entry) in written.iter().enumerate() {
            assert_eq!(entry.number(), 100_000_100_001 + index as u64);
            assert_eq!(log.read(entry.number()).unwrap(), *entry);
            match entry.record() {
                Record::Purge(purge) => {
                    assert_eq!(purge.commits(), if index == 0 { 7 } else { 0 });
                }
                other => panic!("{other:?}"),
            }
        }
        let last = numbers[numbers.len() - 1];
        let second = match written[1].record() {
            Record::Purge(purge) => purge.stubs()[0].0,
            other => panic!("{other:?}"),
        };
        assert!(written[..1]
            .iter()
            .all(|entry| !matches!(entry.record(), Record::Purge(p) if p.lists(second))));
        assert!(matches!(written.last().unwrap().record(), Record::Purge(p) if p.lists(last)));
        assert_eq!(tip_number(&log), Some(written.last().unwrap().number()));
    }

    #[test]
    fn the_split_keeps_the_order_and_fills_each_part() {
        let numbers: Vec<u64> = (1..=20_000).map(|n| n * 2 + 100_000_000_000).collect();
        let whole = Purge::try_new(TIME, human(), 7, &numbers).unwrap();
        let parts = whole.clone().split();
        let joined: Vec<(u64, u64)> = parts
            .iter()
            .flat_map(|part| part.stubs().to_vec())
            .collect();
        assert_eq!(joined, whole.stubs());
        let previous = Chain::After(EntryHash::of(""));
        let size = |purge: Purge| {
            Entry::linked(MAX_NUMBER, previous, Record::Purge(purge))
                .unwrap()
                .encode()
                .len() as u64
        };
        for pair in parts.windows(2) {
            let mut fuller = pair[0].clone();
            fuller.stubs.push(pair[1].stubs[0]);
            assert!(
                size(fuller) > MAX_ENTRY_LEN,
                "a part could hold one more range"
            );
        }
    }

    #[test]
    fn a_purge_that_lists_a_missing_entry_is_refused_and_writes_nothing() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        log.append(&change("a", None, A)).unwrap();
        let error = log
            .append_purge(Purge::try_new(TIME, human(), 1, &[1, 2]).unwrap())
            .unwrap_err();
        assert!(
            matches!(error, OpLogError::Missing { number: 2 }),
            "{error:?}"
        );
        assert_eq!(log.numbers().unwrap(), [1]);
        assert_eq!(tip_number(&log), Some(1));
    }

    #[test]
    fn a_purge_that_lists_a_stub_or_a_purge_entry_is_refused_and_writes_nothing() {
        let dir = TempDir::new();
        let log = purged_log(&dir);
        for listed in [2, 4] {
            let error = log
                .append_purge(Purge::try_new(TIME, human(), 1, &[1, listed]).unwrap())
                .unwrap_err();
            assert!(
                matches!(error, OpLogError::NotAChange { number } if number == listed),
                "{error:?}"
            );
            assert!(error.to_string().contains("not a change"), "{error}");
        }
        assert_eq!(log.numbers().unwrap(), [1, 2, 3, 4]);
    }

    #[test]
    fn a_purge_that_stops_after_a_part_reports_the_parts_in_the_log() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        let mut numbers = wide_numbers(MAX_NUMBER - 1);
        numbers.sort_unstable();
        changes_at(&dir, &numbers);
        changes_at(&dir, &[MAX_NUMBER - 1]);
        let error = log
            .append_purge(Purge::try_new(TIME, human(), 7, &numbers).unwrap())
            .unwrap_err();
        let OpLogError::PurgeIncomplete {
            written,
            not_flushed,
            source,
        } = &error
        else {
            panic!("{error:?}");
        };
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].number(), MAX_NUMBER);
        assert_eq!(log.read(MAX_NUMBER).unwrap(), written[0]);
        assert!(matches!(written[0].record(), Record::Purge(p) if p.commits() == 7));
        assert!(not_flushed.is_empty());
        assert!(matches!(**source, OpLogError::Full));
        let message = error.to_string();
        assert!(
            message.contains(&format!("only part of its entries, {MAX_NUMBER}: ")),
            "{message}"
        );
        assert!(message.contains("is full"), "{message}");
    }

    #[test]
    fn the_messages_of_a_purge_name_the_entries_not_known_to_be_flushed() {
        let part = |number| {
            Entry::of(
                number,
                Record::Purge(Purge::try_new(TIME, human(), 0, &[1]).unwrap()),
            )
            .unwrap()
        };
        let incomplete = OpLogError::PurgeIncomplete {
            written: vec![part(9)],
            not_flushed: vec![9],
            source: Box::new(OpLogError::Full),
        };
        assert!(incomplete
            .to_string()
            .contains("only part of its entries, 9 (9 not known to be flushed): "));
        let not_flushed = OpLogError::PurgeNotFlushed {
            written: vec![part(9), part(10)],
            not_flushed: vec![10],
            path: PathBuf::from("oplog"),
            source: io::Error::other("disk"),
        };
        let message = not_flushed.to_string();
        assert!(
            message.contains("wrote all of its entries, 9, 10, but flushing `oplog` failed for 10"),
            "{message}"
        );
        assert_eq!(not_flushed.io_error().unwrap().to_string(), "disk");
    }

    #[test]
    fn an_unreadable_later_entry_is_reported_instead_of_a_missing_listing() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        log.append(&change("a", None, A)).unwrap();
        log.append_purge(Purge::try_new(TIME, human(), 1, &[1]).unwrap())
            .unwrap();
        fs::write(dir.path().join("oplog/000000000002"), "junk").unwrap();
        assert!(matches!(
            log.replace_with_stub(1),
            Err(OpLogError::Corrupt { .. })
        ));
        assert!(log.read(1).unwrap().change().is_some(), "nothing written");
    }

    #[test]
    fn a_stub_that_only_a_later_part_lists_is_listed() {
        let first = entry(1, None, vec![moves("main", None, Some(A))]);
        let second = entry(2, None, vec![moves("other", None, Some(B))]);
        let part = |number, commits, stubs: &[u64]| {
            Entry::of(
                number,
                Record::Purge(Purge::try_new(TIME, human(), commits, stubs).unwrap()),
            )
            .unwrap()
        };
        let parts = [part(3, 1, &[]), part(4, 0, &[2])];
        let stubbed = [first.clone(), second.to_stub().unwrap()];
        let log: Vec<Entry> = stubbed.iter().chain(&parts).cloned().collect();
        check_stubs(&log).unwrap();
        assert!(pending_stubs(&log).is_empty());
        let log: Vec<Entry> = [first, second].iter().chain(&parts).cloned().collect();
        check_stubs(&log).unwrap();
        assert_eq!(pending_stubs(&log), [2]);
    }

    #[test]
    fn a_damaged_entry_cannot_become_a_stub() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        log.append(&change("a", None, A)).unwrap();
        log.append_purge(Purge::try_new(TIME, human(), 1, &[1]).unwrap())
            .unwrap();
        fs::write(dir.path().join("oplog/000000000001"), "junk").unwrap();
        assert!(matches!(
            log.replace_with_stub(1),
            Err(OpLogError::Corrupt { .. })
        ));
    }

    #[test]
    fn the_changes_that_an_unfinished_purge_lists_are_pending() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        log.append(&change("a", None, A)).unwrap();
        log.append(&change("b", None, B)).unwrap();
        log.append_purge(Purge::try_new(TIME, human(), 2, &[1, 2]).unwrap())
            .unwrap();
        log.replace_with_stub(1).unwrap();
        assert_eq!(pending_stubs(&log.entries().unwrap()), [2]);
        log.replace_with_stub(2).unwrap();
        assert!(pending_stubs(&log.entries().unwrap()).is_empty());
    }

    #[test]
    fn the_tail_holds_the_last_entries() {
        let dir = TempDir::new();
        let log = purged_log(&dir);
        let numbers: Vec<u64> = log.tail(1).unwrap().iter().map(Entry::number).collect();
        assert_eq!(numbers, [4]);
        let numbers: Vec<u64> = log.tail(2).unwrap().iter().map(Entry::number).collect();
        assert_eq!(numbers, [3, 4]);
        assert_eq!(log.tail(9).unwrap().len(), 4);
    }

    // The hash chain (STEMMA-41)

    /// A log of four entries in the chain, written by the writer.
    fn chained(dir: &TempDir) -> (FsOpLog, Vec<Entry>) {
        let log = log_in(dir);
        let entries = vec![
            log.append(&change("main", None, A)).unwrap(),
            log.append(&change("main", Some(A), B)).unwrap(),
            log.append(&change("main", Some(B), C)).unwrap(),
            log.append(&change("main", Some(C), A)).unwrap(),
        ];
        (log, entries)
    }

    #[test]
    fn an_entry_in_the_chain_has_one_encoding_and_names_its_predecessor() {
        let first = Entry::linked(1, Chain::Start, Record::Change(sample_change())).unwrap();
        let text = golden()
            .replacen("stemma-oplog 1", "stemma-oplog 3", 1)
            .replacen("number 42", "number 1", 1)
            .replacen(
                "actor human daniel\n",
                "actor human daniel\nprevious -\n",
                1,
            );
        assert_eq!(String::from_utf8(first.encode()).unwrap(), text);
        assert_eq!(Entry::parse(text.as_bytes()).unwrap(), first);

        let hash = first.hash().unwrap();
        let second = Entry::linked(2, Chain::After(hash), Record::Change(sample_change())).unwrap();
        let encoded = String::from_utf8(second.encode()).unwrap();
        assert!(
            encoded.contains(&format!("\nprevious {hash}\n")),
            "{encoded}"
        );
        assert_eq!(Entry::parse(encoded.as_bytes()).unwrap(), second);
        // An entry written before the chain has no hash.
        assert_eq!(sample_entry().hash(), None);
        assert_eq!(sample_entry().chain(), Chain::Unchained);
    }

    #[test]
    fn a_take_over_names_no_pointer_and_only_it_may() {
        let take_over = |working_copy| {
            Change::try_new(
                TIME,
                human(),
                operation(TAKE_OVER),
                None,
                working_copy,
                vec![],
            )
        };
        let change = take_over(None).unwrap();
        assert_eq!(
            take_over(Some(moved(Some(A), Some(B)))),
            Err(EntryError::TakeOverWithPointer)
        );

        let entry = Entry::linked(1, Chain::Start, Record::Change(change)).unwrap();
        let text = format!(
            "stemma-oplog 3\nnumber 1\ntime {TIME}\nactor human daniel\nprevious -\n\
             operation take-over\n"
        );
        assert_eq!(String::from_utf8(entry.encode()).unwrap(), text);
        assert_eq!(Entry::parse(text.as_bytes()).unwrap(), entry);

        // Any other operation needs a pointer, in the reader as in the constructor.
        let other = text.replace("operation take-over", "operation snapshot");
        assert!(Entry::parse(other.as_bytes()).is_err());
        let with_pointer = format!("{text}working-copy {A} {B}\n");
        assert_eq!(
            Entry::parse(with_pointer.as_bytes()),
            Err(EntryParseError::Entry(EntryError::TakeOverWithPointer))
        );
    }

    #[test]
    fn the_hash_of_an_entry_is_computed_over_its_envelope_and_its_body() {
        // The rule, written out: BLAKE3 of the envelope text, which names the BLAKE3 of the body
        // text. A change of the rule changes the hashes of every log, so this pins it.
        let entry =
            Entry::linked(1, Chain::Start, Record::Change(change("main", None, A))).unwrap();
        let body = format!("stemma-oplog-body 1\noperation commit\nbranch main - {A}\n");
        let body = format!("b3:{}", blake3::hash(body.as_bytes()).to_hex());
        let envelope = format!(
            "stemma-oplog-hash 1\nnumber 1\ntime {TIME}\nactor human daniel\nprevious -\n\
             body {body}\n"
        );
        let expected = format!("b3:{}", blake3::hash(envelope.as_bytes()).to_hex());
        assert_eq!(entry.body_hash().unwrap().to_string(), body);
        assert_eq!(entry.hash().unwrap().to_string(), expected);
    }

    #[test]
    fn the_reader_rejects_every_other_form_of_an_entry_in_the_chain() {
        let start = "stemma-oplog 3\nnumber 2\ntime 1700000000\nactor human daniel\n";
        let hash = format!("b3:{}", "a".repeat(64));
        for rest in [
            "operation snapshot\nbranch main - b3:".to_owned() + &"1".repeat(64) + "\n",
            "previous\noperation snapshot\n".to_owned(),
            "previous b3:00\noperation snapshot\n".to_owned(),
            format!("previous {}\noperation snapshot\n", hash.to_uppercase()),
            format!("previous {hash}\n"),
            format!("previous {hash}\nstub\n"),
            format!("previous {hash}\nstub b3:11\n"),
            format!("previous {hash}\nstub {hash}\nmore\n"),
            format!("previous {hash}\noperation snapshot\n"),
        ] {
            let text = format!("{start}{rest}");
            assert!(Entry::parse(text.as_bytes()).is_err(), "{text:?}");
        }
        let error = Entry::parse(format!("{start}operation snapshot\n").as_bytes()).unwrap_err();
        assert_eq!(error, malformed(5, EntryMalformed::ExpectedPrevious));
        let error = Entry::parse(format!("{start}previous b3:00\n").as_bytes()).unwrap_err();
        assert!(
            matches!(error, EntryParseError::InvalidHash { line: 5, .. }),
            "{error}"
        );
        // A stub of version 2 keeps no hash, and one of version 3 has to.
        let error = Entry::parse(format!("{start}previous {hash}\nstub\n").as_bytes()).unwrap_err();
        assert_eq!(error, malformed(6, EntryMalformed::ExpectedStubOrPurge));
        let old = "stemma-oplog 2\nnumber 2\ntime 1700000000\nactor human daniel\n";
        assert!(Entry::parse(format!("{old}stub {hash}\n").as_bytes()).is_err());
        // The stub and the purge of version 3 read back.
        for rest in [
            format!("previous {hash}\nstub {hash}\n"),
            "previous -\noperation purge\ncommits 1\nstubs 1\n".to_owned(),
        ] {
            let text = format!("{start}{rest}");
            let entry = Entry::parse(text.as_bytes()).unwrap();
            assert_eq!(String::from_utf8(entry.encode()).unwrap(), text);
        }
    }

    #[test]
    fn the_writer_links_every_entry_and_the_tip_names_the_last_hash() {
        let dir = TempDir::new();
        let (log, entries) = chained(&dir);
        assert_eq!(entries[0].chain(), Chain::Start);
        for pair in entries.windows(2) {
            assert_eq!(pair[1].chain(), Chain::After(pair[0].hash().unwrap()));
        }
        assert_eq!(log.entries().unwrap(), entries);
        let tip = log.read_tip().unwrap().unwrap();
        assert_eq!((tip.number, tip.hash), (4, entries[3].hash()));
    }

    #[test]
    fn a_changed_entry_in_the_middle_breaks_the_chain_at_the_next_one() {
        let dir = TempDir::new();
        let (log, _) = chained(&dir);
        // The entry is still canonical, only its time differs.
        let text = fs::read_to_string(entry_file(&dir, 2)).unwrap();
        fs::write(
            entry_file(&dir, 2),
            text.replace("time 1700000000", "time 1700000001"),
        )
        .unwrap();
        assert!(matches!(
            log.entries(),
            Err(OpLogError::ChainBroken { number: 3, .. })
        ));
        // `op log` with a limit sees the break if the window reaches the entry after it.
        assert!(matches!(
            log.tail(2),
            Err(OpLogError::ChainBroken { number: 3, .. })
        ));
        assert!(log.tail(1).is_ok());
        let message = log.entries().unwrap_err().to_string();
        assert!(
            message.contains("entry 3 does not name the hash"),
            "{message}"
        );
    }

    #[test]
    fn a_removed_inserted_or_swapped_entry_breaks_the_chain() {
        let renumber = |dir: &TempDir, from: u64, to: u64| {
            let text = fs::read_to_string(entry_file(dir, from)).unwrap();
            fs::write(
                entry_file(dir, to),
                text.replacen(&format!("number {from}\n"), &format!("number {to}\n"), 1),
            )
            .unwrap();
        };
        // Removed: entry 2 is gone, and 3 and 4 move down, so that no gap shows.
        let dir = TempDir::new();
        let (log, _) = chained(&dir);
        renumber(&dir, 3, 2);
        renumber(&dir, 4, 3);
        fs::remove_file(entry_file(&dir, 4)).unwrap();
        set_tip(&dir, 3);
        assert!(matches!(
            log.entries(),
            Err(OpLogError::ChainBroken { number: 2, .. })
        ));

        // Inserted: a copy of entry 2 goes in as entry 3, and the later ones move up.
        let dir = TempDir::new();
        let (log, _) = chained(&dir);
        renumber(&dir, 4, 5);
        renumber(&dir, 3, 4);
        renumber(&dir, 2, 3);
        set_tip(&dir, 5);
        assert!(matches!(
            log.entries(),
            Err(OpLogError::ChainBroken { number: 3, .. })
        ));

        // Swapped: entries 2 and 3 change places.
        let dir = TempDir::new();
        let (log, _) = chained(&dir);
        let two = fs::read_to_string(entry_file(&dir, 2)).unwrap();
        renumber(&dir, 3, 2);
        fs::write(
            entry_file(&dir, 3),
            two.replacen("number 2\n", "number 3\n", 1),
        )
        .unwrap();
        assert!(matches!(
            log.entries(),
            Err(OpLogError::ChainBroken { number: 2, .. })
        ));
    }

    #[test]
    fn a_replaced_last_entry_is_found_by_the_hash_of_the_tip() {
        let dir = TempDir::new();
        let (log, entries) = chained(&dir);
        // A new last entry with the right link: only the tip tells.
        let forged = Entry::linked(
            4,
            Chain::After(entries[2].hash().unwrap()),
            Record::Change(change("main", Some(C), B)),
        )
        .unwrap();
        fs::write(entry_file(&dir, 4), forged.encode()).unwrap();
        assert!(matches!(
            log.entries(),
            Err(OpLogError::TipMismatch { number: 4, .. })
        ));
        assert!(matches!(
            log.tail(1),
            Err(OpLogError::TipMismatch { number: 4, .. })
        ));
        // The writer refuses to take a replaced entry into the chain.
        let error = log.append(&change("main", Some(B), A)).unwrap_err();
        assert!(matches!(error, OpLogError::TipMismatch { .. }), "{error}");
        assert_eq!(files(&dir.path().join("oplog")).len(), 4);
        assert!(error.to_string().contains("oplog-tip"), "{error}");
    }

    #[test]
    fn a_tip_one_behind_is_compared_with_the_entry_it_names() {
        let dir = TempDir::new();
        let (log, entries) = chained(&dir);
        // The stop between entry 4 and its tip: the tip of entry 3.
        log.write_tip(&entries[2]).unwrap();
        assert!(log.entries().is_ok());
        let appended = log.append(&change("main", Some(A), B)).unwrap();
        assert_eq!(appended.chain(), Chain::After(entries[3].hash().unwrap()));
        assert_eq!(tip_number(&log), Some(5));
    }

    #[test]
    fn a_tip_of_version_2_is_strict() {
        let dir = TempDir::new();
        let (log, entries) = chained(&dir);
        let hash = entries[3].hash().unwrap();
        for content in [
            "stemma-oplog-tip 2\nnumber 4\n".to_owned(),
            format!("stemma-oplog-tip 2\nnumber 4\nhash {hash}"),
            format!(
                "stemma-oplog-tip 2\nnumber 4\nhash {}\n",
                hash.to_string().to_uppercase()
            ),
            format!("stemma-oplog-tip 2\nnumber 4\nhash {hash}\nmore\n"),
            format!("stemma-oplog-tip 1\nnumber 4\nhash {hash}\n"),
        ] {
            fs::write(dir.path().join("oplog-tip"), &content).unwrap();
            assert!(
                matches!(log.numbers(), Err(OpLogError::BadTip { .. })),
                "{content:?}"
            );
        }
    }

    #[test]
    fn a_stub_keeps_the_hash_of_its_entry_and_a_changed_stub_breaks_the_chain() {
        let dir = TempDir::new();
        let log = purged_log(&dir);
        let entries = log.entries().unwrap();
        for stub in &entries[1..3] {
            let Record::Stub(record) = stub.record() else {
                panic!("a stub");
            };
            assert!(record.body().is_some());
            assert!(matches!(stub.chain(), Chain::After(_)));
        }
        // The stub has the hash of the entry it replaced, so the next entry still links to it.
        check_chain(&entries).unwrap();
        // A purge changes the entries before the last one only, so the tip still holds.
        assert_eq!(log.read_tip().unwrap().unwrap().hash, entries[3].hash());
        // A stub whose time differs from that of its entry breaks the chain.
        let text = fs::read_to_string(entry_file(&dir, 2)).unwrap();
        fs::write(
            entry_file(&dir, 2),
            text.replace("time 1700000000", "time 1700000001"),
        )
        .unwrap();
        assert!(matches!(
            log.entries(),
            Err(OpLogError::ChainBroken { number: 3, .. })
        ));
    }

    #[test]
    fn entries_from_before_the_chain_come_first_and_the_chain_starts_after_them() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        for number in 1..=2 {
            let entry = Entry::of(number, Record::Change(change("main", None, A))).unwrap();
            fs::write(entry_file(&dir, number), entry.encode()).unwrap();
        }
        set_tip(&dir, 2);
        let third = log.append(&change("main", Some(A), B)).unwrap();
        assert_eq!(third.chain(), Chain::Start);
        let fourth = log.append(&change("main", Some(B), C)).unwrap();
        assert_eq!(fourth.chain(), Chain::After(third.hash().unwrap()));
        assert_eq!(log.entries().unwrap().len(), 4);

        // An entry from before the chain after it is refused.
        let old = Entry::of(5, Record::Change(change("main", Some(C), A))).unwrap();
        fs::write(entry_file(&dir, 5), old.encode()).unwrap();
        assert!(matches!(
            log.entries(),
            Err(OpLogError::UnchainedAfterChain { number: 5 })
        ));
        // And so is a second start of the chain.
        let again =
            Entry::linked(5, Chain::Start, Record::Change(change("main", Some(C), A))).unwrap();
        fs::write(entry_file(&dir, 5), again.encode()).unwrap();
        assert!(matches!(
            log.entries(),
            Err(OpLogError::ChainBroken { number: 5, .. })
        ));
    }

    #[test]
    fn a_first_entry_that_names_a_predecessor_is_refused() {
        assert_eq!(
            Entry::linked(
                1,
                Chain::After(EntryHash::of("")),
                Record::Change(change("main", None, A)),
            ),
            Err(EntryError::FirstAfter)
        );
        let dir = TempDir::new();
        let log = log_in(&dir);
        let first = Entry::linked(1, Chain::Start, Record::Change(change("main", None, A)))
            .unwrap()
            .encode();
        let text = String::from_utf8(first)
            .unwrap()
            .replace("previous -", &format!("previous {}", EntryHash::of("")));
        fs::write(entry_file(&dir, 1), text).unwrap();
        assert!(matches!(
            log.entries(),
            Err(OpLogError::Corrupt {
                source: EntryParseError::Entry(EntryError::FirstAfter),
                ..
            })
        ));
    }

    #[test]
    fn a_writer_that_cannot_read_the_entry_before_refuses_and_writes_nothing() {
        let dir = TempDir::new();
        let (log, _) = chained(&dir);
        fs::remove_file(dir.path().join("oplog-tip")).unwrap();
        fs::write(entry_file(&dir, 4), "damaged").unwrap();
        let error = log.append(&change("main", Some(A), B)).unwrap_err();
        assert!(
            matches!(&error, OpLogError::Predecessor { number: 4, .. }),
            "{error}"
        );
        assert!(error.to_string().contains("entry 4"), "{error}");
        assert_eq!(files(&dir.path().join("oplog")).len(), 4);
        assert_eq!(files(&dir.path().join("tmp")), Vec::<String>::new());
    }

    #[test]
    fn whoever_writes_the_log_can_compute_the_chain_again() {
        // The limit of the chain (ADR-011): an entry changed and every later hash and the tip
        // computed again is not found. Only an anchor outside `.stemma` would find it.
        let dir = TempDir::new();
        let (log, entries) = chained(&dir);
        let mut previous = Chain::Start;
        let mut last = None;
        for entry in &entries {
            let record = if entry.number() == 2 {
                Record::Change(change("main", Some(A), C))
            } else {
                entry.record().clone()
            };
            let forged = Entry::linked(entry.number(), previous, record).unwrap();
            fs::write(entry_file(&dir, forged.number()), forged.encode()).unwrap();
            previous = Chain::After(forged.hash().unwrap());
            last = Some(forged);
        }
        log.write_tip(&last.unwrap()).unwrap();
        let forged = log.verify().unwrap();
        // The hash of the changed entry is another one, so an anchor kept from before finds it.
        assert_ne!(forged[1].hash(), entries[1].hash());
    }

    #[test]
    fn a_broken_link_names_the_hash_it_expected_and_the_one_it_found() {
        let dir = TempDir::new();
        let (log, entries) = chained(&dir);
        let other = Entry::linked(
            3,
            Chain::After(entries[0].hash().unwrap()),
            entries[2].record().clone(),
        )
        .unwrap();
        fs::write(entry_file(&dir, 3), other.encode()).unwrap();
        let error = log.entries().unwrap_err();
        let OpLogError::ChainBroken {
            number,
            expected,
            found,
        } = &error
        else {
            panic!("{error}");
        };
        assert_eq!(*number, 3);
        assert_eq!(*expected, entries[1].hash());
        assert_eq!(*found, Chain::After(entries[0].hash().unwrap()));
        let message = error.to_string();
        assert!(
            message.contains(&entries[1].hash().unwrap().to_string())
                && message.contains(&entries[0].hash().unwrap().to_string()),
            "{message}"
        );
    }

    #[test]
    fn a_stub_has_the_hash_of_a_body_exactly_when_it_is_in_the_chain() {
        let stub = |body| {
            Record::Stub(Stub {
                time: TIME,
                actor: human(),
                body,
                replaced: None,
            })
        };
        let body = Some(BodyHash::of(""));
        assert_eq!(
            Entry::linked(2, Chain::Unchained, stub(body)),
            Err(EntryError::StubBodyOutsideChain(2))
        );
        assert_eq!(
            Entry::linked(2, Chain::Start, stub(None)),
            Err(EntryError::StubWithoutBody(2))
        );
        assert!(Entry::linked(2, Chain::Unchained, stub(None)).is_ok());
        assert!(Entry::linked(2, Chain::Start, stub(body)).is_ok());
    }

    #[test]
    fn a_tip_one_behind_whose_entry_was_replaced_stops_the_writer() {
        let dir = TempDir::new();
        let (log, entries) = chained(&dir);
        // The stop between entry 4 and its tip, and then entry 3 replaced.
        log.write_tip(&entries[2]).unwrap();
        let forged = Entry::linked(
            3,
            Chain::After(entries[1].hash().unwrap()),
            Record::Change(change("main", Some(B), A)),
        )
        .unwrap();
        fs::write(entry_file(&dir, 3), forged.encode()).unwrap();
        let error = log.append(&change("main", Some(A), B)).unwrap_err();
        assert!(
            matches!(&error, OpLogError::TipMismatch { number: 3, .. }),
            "{error}"
        );
        assert_eq!(files(&dir.path().join("oplog")).len(), 4);
        assert_eq!(files(&dir.path().join("tmp")), Vec::<String>::new());
    }

    #[test]
    fn a_tip_far_behind_is_still_compared_with_the_entry_it_names() {
        let dir = TempDir::new();
        let (log, entries) = chained(&dir);
        log.write_tip(&entries[0]).unwrap();
        let forged =
            Entry::linked(1, Chain::Start, Record::Change(change("main", None, B))).unwrap();
        fs::write(entry_file(&dir, 1), forged.encode()).unwrap();
        let error = log.append(&change("main", Some(A), B)).unwrap_err();
        assert!(
            matches!(&error, OpLogError::TipMismatch { number: 1, .. }),
            "{error}"
        );
        assert_eq!(files(&dir.path().join("oplog")).len(), 4);
    }

    #[test]
    fn a_tip_of_version_1_or_with_number_0_is_refused_beside_a_chain() {
        let dir = TempDir::new();
        let (log, _) = chained(&dir);
        for content in [
            "stemma-oplog-tip 1\nnumber 4\n",
            "stemma-oplog-tip 1\nnumber 0\n",
        ] {
            fs::write(dir.path().join("oplog-tip"), content).unwrap();
            assert!(
                matches!(log.entries(), Err(OpLogError::BadTip { .. })),
                "{content:?}"
            );
            let error = log.append(&change("main", Some(A), B)).unwrap_err();
            assert!(matches!(error, OpLogError::BadTip { .. }), "{error}");
        }
        assert_eq!(files(&dir.path().join("oplog")).len(), 4);
    }

    #[test]
    fn verify_reports_a_tip_that_is_missing_or_far_behind() {
        let dir = TempDir::new();
        let (log, entries) = chained(&dir);
        assert_eq!(log.verify().unwrap(), entries);

        // One behind is the stop between an entry and its tip.
        log.write_tip(&entries[2]).unwrap();
        assert!(log.verify().is_ok());

        log.write_tip(&entries[1]).unwrap();
        assert!(log.entries().is_ok());
        assert!(matches!(
            log.verify(),
            Err(OpLogError::TipBehind {
                tip: 2,
                last: 4,
                ..
            })
        ));

        fs::remove_file(dir.path().join("oplog-tip")).unwrap();
        assert!(log.entries().is_ok());
        let error = log.verify().unwrap_err();
        assert!(
            matches!(error, OpLogError::TipMissing { last: 4, .. }),
            "{error}"
        );
        // The next entry writes the tip again.
        log.append(&change("main", Some(A), B)).unwrap();
        assert_eq!(log.verify().unwrap().len(), 5);
    }

    #[test]
    fn a_writer_compares_the_tip_only_with_an_entry_next_to_the_end() {
        let dir = TempDir::new();
        let (log, entries) = chained(&dir);
        // The tip names entry 2 of 4, and entry 2 is damaged. A damage away from the end does not
        // stop a writer (as a gap does not), and `verify` reports the tip and the damage.
        log.write_tip(&entries[1]).unwrap();
        fs::write(entry_file(&dir, 2), b"damaged").unwrap();
        log.append(&change("main", Some(A), B)).unwrap();
        assert!(log.verify().is_err());

        // The tip names the entry before the last, and that entry is damaged: the writer cannot
        // compare the tip with the entry it names, so it stops.
        let dir = TempDir::new();
        let (log, entries) = chained(&dir);
        log.write_tip(&entries[2]).unwrap();
        fs::write(entry_file(&dir, 3), b"damaged").unwrap();
        let error = log.append(&change("main", Some(A), B)).unwrap_err();
        assert!(
            matches!(error, OpLogError::Predecessor { number: 3, .. }),
            "{error}"
        );
    }

    #[test]
    fn verify_accepts_a_log_from_before_the_chain_without_a_tip() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        let entry = Entry::of(1, Record::Change(change("main", None, A))).unwrap();
        fs::write(entry_file(&dir, 1), entry.encode()).unwrap();
        let entries = log.verify().unwrap();
        assert_eq!(entries, std::slice::from_ref(&entry));
        assert_eq!(chain_start(&entries), None);
    }

    #[test]
    fn a_window_whose_entry_before_cannot_be_read_says_so() {
        let dir = TempDir::new();
        let (log, entries) = chained(&dir);
        let window = log.window(2).unwrap();
        assert_eq!(window.entries(), &entries[2..]);
        assert!(window.unchecked().is_none());

        fs::write(entry_file(&dir, 2), "damaged").unwrap();
        let window = log.window(2).unwrap();
        assert_eq!(window.entries(), &entries[2..]);
        let (number, error) = window.unchecked().unwrap();
        assert_eq!(number, 2);
        assert!(matches!(error, OpLogError::Corrupt { .. }), "{error}");
        assert_eq!(log.tail(2).unwrap(), &entries[2..]);
        // The whole log does not pass.
        assert!(log.entries().is_err());
    }

    #[test]
    fn a_broken_link_at_the_edge_of_a_window_fails_the_window() {
        let dir = TempDir::new();
        let (log, entries) = chained(&dir);
        // Entry 2 replaced, readable, and no longer the entry that entry 3 names.
        let other = Entry::linked(
            2,
            Chain::After(entries[0].hash().unwrap()),
            Record::Change(change("main", Some(A), C)),
        )
        .unwrap();
        fs::write(entry_file(&dir, 2), other.encode()).unwrap();
        assert!(matches!(
            log.window(2),
            Err(OpLogError::ChainBroken { number: 3, .. })
        ));
        // A window that does not reach the link is not checked against it.
        assert!(log.window(1).is_ok());
    }

    #[test]
    fn a_writer_that_cannot_read_the_entry_two_before_starts_no_hidden_chain() {
        let dir = TempDir::new();
        let (log, entries) = chained(&dir);
        // Entry 4 in the form from before the chain, entry 3 damaged, and the tip removed: the
        // writer cannot tell that entry 4 follows the chain, so it starts the chain again.
        let old = Entry::of(4, entries[3].record().clone()).unwrap();
        fs::write(entry_file(&dir, 4), old.encode()).unwrap();
        fs::write(entry_file(&dir, 3), "damaged").unwrap();
        fs::remove_file(dir.path().join("oplog-tip")).unwrap();
        let appended = log.append(&change("main", Some(A), B)).unwrap();
        assert_eq!(appended.chain(), Chain::Start);
        // The damage is not hidden: the whole log still fails at entry 3.
        assert!(matches!(
            log.entries(),
            Err(OpLogError::Corrupt { path, .. }) if path == entry_file(&dir, 3)
        ));
        // And once entry 3 is restored, the old entry after the chain is found.
        fs::write(entry_file(&dir, 3), entries[2].encode()).unwrap();
        assert!(matches!(
            log.entries(),
            Err(OpLogError::UnchainedAfterChain { number: 4 })
        ));
    }

    #[test]
    fn a_writer_refuses_to_follow_an_old_entry_that_follows_the_chain() {
        let dir = TempDir::new();
        let (log, entries) = chained(&dir);
        // Entry 4 put in its place in the form from before the chain, and the tip removed.
        let old = Entry::of(4, entries[3].record().clone()).unwrap();
        fs::write(entry_file(&dir, 4), old.encode()).unwrap();
        fs::remove_file(dir.path().join("oplog-tip")).unwrap();
        let error = log.append(&change("main", Some(A), B)).unwrap_err();
        assert!(
            matches!(
                &error,
                OpLogError::Predecessor { number: 4, source }
                    if matches!(**source, OpLogError::UnchainedAfterChain { number: 4 })
            ),
            "{error}"
        );
        assert_eq!(files(&dir.path().join("oplog")).len(), 4);
    }

    #[test]
    fn an_old_entry_is_purged_to_an_old_stub_and_the_chain_after_it_holds() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        for number in 1..=2 {
            let entry = Entry::of(number, Record::Change(change("main", None, A))).unwrap();
            fs::write(entry_file(&dir, number), entry.encode()).unwrap();
        }
        set_tip(&dir, 2);
        log.append(&change("main", Some(A), B)).unwrap();
        log.append_purge(Purge::try_new(TIME, human(), 1, &[2]).unwrap())
            .unwrap();
        let stub = log.replace_with_stub(2).unwrap();
        assert_eq!(stub.chain(), Chain::Unchained);
        assert!(String::from_utf8(stub.encode())
            .unwrap()
            .starts_with("stemma-oplog 2\n"));
        let entries = log.verify().unwrap();
        assert_eq!(chain_start(&entries), Some(3));
        // The chain starts at entry 3 after the stub of entry 2, which is still from before it.
        log.append(&change("main", Some(B), C)).unwrap();
        assert_eq!(log.verify().unwrap().len(), 5);
    }

    #[test]
    fn a_log_with_old_stubs_and_an_old_purge_entry_starts_the_chain_after_them() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        let old = [
            Entry::of(1, Record::Change(change("main", None, A))).unwrap(),
            Entry::of(2, Record::Change(change("main", Some(A), B)))
                .unwrap()
                .to_stub()
                .unwrap(),
            Entry::of(
                3,
                Record::Purge(Purge::try_new(TIME, human(), 1, &[2]).unwrap()),
            )
            .unwrap(),
        ];
        for entry in &old {
            fs::write(entry_file(&dir, entry.number()), entry.encode()).unwrap();
        }
        set_tip(&dir, 3);
        let fourth = log.append(&change("main", Some(B), C)).unwrap();
        assert_eq!(fourth.chain(), Chain::Start);
        let entries = log.verify().unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(chain_start(&entries), Some(4));
    }

    // ---- The worktree of an entry (ADR-011)

    fn in_worktree(change: Change, name: &str) -> Change {
        change.in_worktree(Some(name.to_owned()), None).unwrap()
    }

    #[test]
    fn an_entry_of_a_linked_worktree_names_it_and_the_main_one_leaves_it_out() {
        let main = Entry::linked(1, Chain::Start, Record::Change(change("main", None, A))).unwrap();
        assert!(!String::from_utf8(main.encode())
            .unwrap()
            .contains("worktree"));
        assert_eq!(main.change().unwrap().worktree(), None);
        assert_eq!(main.change().unwrap().worktree_name(), "main");

        let linked = in_worktree(change("main", None, A), "feature-2");
        let entry = Entry::linked(1, Chain::Start, Record::Change(linked)).unwrap();
        let text = String::from_utf8(entry.encode()).unwrap();
        assert!(
            text.contains("\noperation commit\nworktree feature-2\nbranch main - "),
            "{text}"
        );
        assert_eq!(Entry::parse(text.as_bytes()).unwrap(), entry);
        // The name is covered by the hash of the body.
        assert_ne!(entry.body_hash(), main.body_hash());
    }

    /// A change of `operation` in the worktree `w` with the directory `/tmp/w`.
    fn change_of(operation_name: &str, location: Option<&str>) -> Change {
        Change::try_new(
            TIME,
            human(),
            operation(operation_name),
            None,
            None,
            vec![(branch("main"), moved(None, Some(A)))],
        )
        .unwrap()
        .in_worktree(Some("w".to_owned()), location.map(str::to_owned))
        .unwrap()
    }

    fn entry_of(change: Change) -> Entry {
        Entry::linked(1, Chain::Start, Record::Change(change)).unwrap()
    }

    #[test]
    fn the_former_owner_follows_the_directory_of_a_removal() {
        let owner = Actor::human("anna").unwrap();
        let removal = change_of("worktree-remove", Some("/tmp/w"));
        let plain = entry_of(removal.clone());
        let entry = entry_of(removal.with_former_owner(owner.clone()).unwrap());
        let text = String::from_utf8(entry.encode()).unwrap();
        assert!(
            text.contains("\nworktree w\nlocation /tmp/w\nformer-owner human anna\nbranch main - "),
            "{text}"
        );
        let read = Entry::parse(text.as_bytes()).unwrap();
        assert_eq!(read, entry);
        assert_eq!(read.change().unwrap().former_owner(), Some(&owner));
        // The line is part of the hash of the body.
        assert_ne!(entry.body_hash(), plain.body_hash());
        assert_eq!(plain.change().unwrap().former_owner(), None);
    }

    #[test]
    fn an_agent_can_be_the_former_owner() {
        let owner = Actor::agent("bot", "m", "s", "daniel").unwrap();
        let change = change_of("worktree-remove", Some("/tmp/w"))
            .with_former_owner(owner.clone())
            .unwrap();
        let entry = entry_of(change);
        let read = Entry::parse(&entry.encode()).unwrap();
        assert_eq!(read.change().unwrap().former_owner(), Some(&owner));
        assert_eq!(read, entry);
    }

    #[test]
    fn only_the_removal_of_a_worktree_with_a_directory_can_name_a_former_owner() {
        let owner = Actor::human("anna").unwrap();
        let main = Change::try_new(
            TIME,
            human(),
            operation("worktree-remove"),
            None,
            None,
            vec![(branch("main"), moved(None, Some(A)))],
        )
        .unwrap();
        for change in [
            main,
            change_of("worktree-remove", None),
            // The entry that makes a worktree has a directory too, and no former owner.
            change_of("worktree-add", Some("/tmp/w")),
            change_of("commit", Some("/tmp/w")),
        ] {
            assert_eq!(
                change.with_former_owner(owner.clone()).unwrap_err(),
                EntryError::FormerOwnerWithoutRemoval
            );
        }
        // Only a human takes over (ADR-004), and nobody is the former owner of their own copy.
        let removal = change_of("worktree-remove", Some("/tmp/w"));
        assert_eq!(
            removal.with_former_owner(human()).unwrap_err(),
            EntryError::FormerOwnerWithoutRemoval
        );
        let mut by_agent = change_of("worktree-remove", Some("/tmp/w"));
        by_agent.actor = Actor::agent("bot", "m", "s", "daniel").unwrap();
        assert_eq!(
            by_agent.with_former_owner(owner).unwrap_err(),
            EntryError::FormerOwnerWithoutRemoval
        );
    }

    #[test]
    fn the_reader_refuses_a_line_former_owner_that_is_wrong() {
        let owner = Actor::human("anna").unwrap();
        let good = entry_of(
            change_of("worktree-remove", Some("/tmp/w"))
                .with_former_owner(owner)
                .unwrap(),
        );
        let text = String::from_utf8(good.encode()).unwrap();
        let line = "former-owner human anna\n";
        // Not an actor, or nothing after the key.
        for value in ["former-owner nobody\n", "former-owner \n"] {
            let bad = text.replace(line, value);
            assert!(
                matches!(
                    Entry::parse(bad.as_bytes()),
                    Err(EntryParseError::Malformed {
                        reason: EntryMalformed::BadFormerOwner,
                        line: 9
                    })
                ),
                "{value:?}: {:?}",
                Entry::parse(bad.as_bytes())
            );
        }
        // Twice, before `location`, after a pointer, and on an entry that is no removal.
        let without = text.replace(line, "");
        let before = without.replace("location /tmp/w\n", &format!("{line}location /tmp/w\n"));
        let twice = text.replace(line, &format!("{line}{line}"));
        let after = format!("{without}{line}");
        let other = text.replace("operation worktree-remove", "operation worktree-add");
        for bad in [before, twice, after, other] {
            assert!(Entry::parse(bad.as_bytes()).is_err(), "{bad}");
        }
        // Without a directory the line has nothing to follow.
        let no_location = text.replace("location /tmp/w\n", "");
        assert!(Entry::parse(no_location.as_bytes()).is_err());
    }

    #[test]
    fn the_directory_of_a_worktree_follows_its_name() {
        let change = change("main", None, A)
            .in_worktree(Some("w".to_owned()), Some("/tmp/w".to_owned()))
            .unwrap();
        let entry = Entry::linked(1, Chain::Start, Record::Change(change)).unwrap();
        let text = String::from_utf8(entry.encode()).unwrap();
        assert!(
            text.contains("\nworktree w\nlocation /tmp/w\nbranch main - "),
            "{text}"
        );
        assert_eq!(Entry::parse(text.as_bytes()).unwrap(), entry);
        assert_eq!(entry.change().unwrap().location(), Some("/tmp/w"));
    }

    #[test]
    fn a_name_or_a_directory_that_no_worktree_has_is_refused() {
        for name in ["", "main", ".hidden", "a b", "a/b", "ä"] {
            assert_eq!(
                change("main", None, A).in_worktree(Some(name.to_owned()), None),
                Err(EntryError::BadWorktree(name.to_owned())),
                "{name:?}"
            );
        }
        assert!(matches!(
            change("main", None, A).in_worktree(None, Some("/tmp/w".to_owned())),
            Err(EntryError::BadLocation(_))
        ));
        assert!(matches!(
            change("main", None, A).in_worktree(Some("w".to_owned()), Some("a\nb".to_owned())),
            Err(EntryError::BadLocation(_))
        ));
        let good = Entry::linked(
            1,
            Chain::Start,
            Record::Change(in_worktree(change("main", None, A), "w")),
        )
        .unwrap();
        let text = String::from_utf8(good.encode()).unwrap();
        for bad in ["worktree main", "worktree ", "worktree a b"] {
            let other = text.replace("worktree w", bad);
            assert!(Entry::parse(other.as_bytes()).is_err(), "{bad}");
        }
        // The line comes right after the operation, before the pointers.
        let late = text.replace("worktree w\n", "") + "worktree w\n";
        assert!(Entry::parse(late.as_bytes()).is_err());
    }

    #[test]
    fn the_outcome_of_a_pointer_is_judged_inside_its_worktree() {
        let moved_wc = |from, to| {
            Change::try_new(
                TIME,
                human(),
                operation("snapshot"),
                None,
                Some(moved(from, to)),
                vec![],
            )
            .unwrap()
        };
        let entry = |number, change| Entry::of(number, Record::Change(change)).unwrap();
        // The linked worktree went from A to B and on to C; the main worktree from A to B.
        let entries = [
            entry(1, in_worktree(moved_wc(Some(A), Some(B)), "w")),
            entry(2, moved_wc(Some(A), Some(B))),
            entry(3, in_worktree(moved_wc(Some(B), Some(C)), "w")),
        ];
        let main =
            Current::new(Head::Branch(branch("main")), [], true).with_working_copy(Some(commit(B)));
        let linked = Current::new(Head::Branch(branch("w")), [], true)
            .in_worktree("w")
            .with_working_copy(Some(commit(C)));

        // Seen from the main worktree, the entry before another one of the same worktree is
        // confirmed by it; the newest entry of the linked one is not known.
        assert_eq!(
            outcomes(&entries, Some(&main)),
            [Outcome::Took, Outcome::Took, Outcome::Unknown]
        );
        assert_eq!(
            outcomes(&entries, Some(&linked)),
            [Outcome::Took, Outcome::Unknown, Outcome::Took]
        );
    }

    #[test]
    fn a_take_over_is_judged_in_its_own_worktree_only() {
        let take_over = Change::try_new(TIME, human(), operation(TAKE_OVER), None, None, vec![])
            .unwrap()
            .in_worktree(Some("w".to_owned()), None)
            .unwrap();
        let entries = [Entry::of(1, Record::Change(take_over)).unwrap()];
        let main = Current::new(Head::Branch(branch("main")), [], true)
            .with_working_copy(Some(commit(A)))
            .with_owner(Some(human()));
        assert_eq!(outcomes(&entries, Some(&main)), [Outcome::Unknown]);
        assert_eq!(
            outcomes(&entries, Some(&main.clone().in_worktree("w"))),
            [Outcome::Took]
        );
    }

    fn mia() -> Actor {
        Actor::human("mia").expect("valid actor")
    }

    fn lease_entry(number: u64, lease: LeaseEntry) -> Entry {
        Entry::linked(number, Chain::Start, Record::Lease(lease)).expect("valid entry")
    }

    fn lease_entries() -> Vec<LeaseEntry> {
        vec![
            LeaseEntry::acquire(TIME, human(), branch("feature/x"), TIME + 3600).unwrap(),
            LeaseEntry::release(TIME, human(), branch("feature/x")),
            LeaseEntry::takeover(TIME, mia(), branch("feature/x"), human(), TIME + 3600).unwrap(),
            LeaseEntry::owner_override(
                TIME,
                human(),
                branch("feature/x"),
                mia(),
                TIME + 60,
                "the agent is stuck",
            )
            .unwrap(),
        ]
    }

    #[test]
    fn a_lease_entry_is_written_as_it_is_read() {
        for lease in lease_entries() {
            let entry = lease_entry(1, lease);
            let bytes = entry.encode();
            assert!(bytes.starts_with(b"stemma-oplog 3\n"));
            let again = Entry::parse(&bytes).expect("reads back");
            assert_eq!(again, entry);
            assert_eq!(again.encode(), bytes);
            assert!(again.hash().is_some(), "a lease entry is in the chain");
            assert!(again.commits().is_empty());
            assert!(again.change().is_none());
        }
    }

    #[test]
    fn the_four_encodings_are_fixed() {
        let text = |lease| String::from_utf8(lease_entry(7, lease).encode()).unwrap();
        let entries = lease_entries();
        assert_eq!(
            text(entries[0].clone()),
            format!(
                "stemma-oplog 3\nnumber 7\ntime {TIME}\nactor human daniel\nprevious -\n\
                 operation lease-acquire\nbranch feature/x\nexpires {}\n",
                TIME + 3600
            )
        );
        assert!(text(entries[1].clone()).ends_with("operation lease-release\nbranch feature/x\n"));
        assert!(text(entries[2].clone()).ends_with(&format!(
            "operation lease-takeover\nbranch feature/x\nfrom human daniel\nexpires {}\n\
             reason expired\n",
            TIME + 3600
        )));
        assert!(text(entries[3].clone()).ends_with(&format!(
            "operation lease-override\nbranch feature/x\nfrom human mia\nexpires {}\n\
             reason the agent is stuck\n",
            TIME + 60
        )));
    }

    #[test]
    fn the_reader_refuses_what_the_writer_does_not_write() {
        let good = |lease| String::from_utf8(lease_entry(7, lease).encode()).unwrap();
        let entries = lease_entries();
        let acquire = good(entries[0].clone());
        let takeover = good(entries[2].clone());
        let override_ = good(entries[3].clone());
        let release = good(entries[1].clone());
        let cases = [
            // A lease entry names the chain: no older encoding has it.
            acquire.replace("stemma-oplog 3", "stemma-oplog 1"),
            acquire.replace("stemma-oplog 3", "stemma-oplog 2"),
            // Lines that are missing, repeated, or in the wrong place.
            acquire.replace("branch feature/x\n", ""),
            acquire.replace("expires", "expires 1\nexpires"),
            acquire + "reason x\n",
            release.clone() + "expires 5\n",
            takeover.replace("from human daniel\n", ""),
            takeover.replace("reason expired", "reason something else"),
            takeover.replace("reason expired\n", ""),
            override_.replace("reason the agent is stuck\n", ""),
            override_.replace("reason the agent is stuck", "reason "),
            override_.replace("reason the agent is stuck", "reason  padded"),
            // The earlier holder is the actor who acts.
            takeover.replace("from human daniel", "from human mia"),
            // Expires before the entry.
            release.replace("branch feature/x\n", "branch feature/x\nexpires 1\n"),
        ];
        for bad in cases {
            assert!(Entry::parse(bad.as_bytes()).is_err(), "{bad}");
        }
        assert!(Entry::parse(
            good(entries[3].clone())
                .replace("expires", "expires -")
                .as_bytes()
        )
        .is_err());
    }

    #[test]
    fn a_change_cannot_take_a_lease_operation() {
        for kind in [LEASE_ACQUIRE, LEASE_RELEASE, LEASE_TAKEOVER, LEASE_OVERRIDE] {
            let result = Change::try_new(
                TIME,
                human(),
                operation(kind),
                None,
                None,
                vec![moves("main", None, Some(A))],
            );
            assert_eq!(result, Err(EntryError::ReservedOperation), "{kind}");
        }
    }

    #[test]
    fn the_rules_of_a_lease_entry_are_checked_once() {
        let b = || branch("main");
        assert_eq!(
            LeaseEntry::acquire(TIME, human(), b(), TIME),
            Err(EntryError::LeaseExpiry)
        );
        assert_eq!(
            LeaseEntry::takeover(TIME, human(), b(), human(), TIME + 1),
            Err(EntryError::LeaseSameActor)
        );
        for reason in ["", " x", "x ", "a\nb", &"x".repeat(MAX_REASON_LEN + 1)] {
            assert_eq!(
                LeaseEntry::owner_override(TIME, human(), b(), mia(), TIME + 1, reason),
                Err(EntryError::BadReason),
                "{reason:?}"
            );
        }
        let longest = "x".repeat(MAX_REASON_LEN);
        assert!(LeaseEntry::owner_override(TIME, human(), b(), mia(), TIME + 1, longest).is_ok());
        // A lease entry outside the chain cannot be built.
        assert_eq!(
            Entry::of(3, Record::Lease(LeaseEntry::release(TIME, human(), b()))),
            Err(EntryError::LeaseOutsideChain(3))
        );
    }

    #[test]
    fn a_lease_entry_is_covered_by_the_chain_and_never_becomes_a_stub() {
        let first = lease_entry(1, lease_entries().remove(0));
        let second = Entry::linked(
            2,
            Chain::After(first.hash().unwrap()),
            Record::Lease(lease_entries().remove(1)),
        )
        .unwrap();
        check_chain(&[first.clone(), second.clone()]).expect("the chain holds");
        // Another branch in the entry is another hash, so a swap breaks the chain.
        let changed = Entry::linked(
            1,
            Chain::Start,
            Record::Lease(
                LeaseEntry::acquire(TIME, human(), branch("feature/y"), TIME + 3600).unwrap(),
            ),
        )
        .unwrap();
        assert!(check_chain(&[changed, second]).is_err());
        assert_eq!(first.to_stub(), Err(EntryError::LeaseNotReplaceable(1)));
    }

    #[test]
    fn a_lease_entry_moves_no_pointer_and_blocks_no_outcome() {
        let before = entry(1, None, vec![moves("main", None, Some(A))]);
        let lease = lease_entry(2, lease_entries().remove(0));
        let after = entry(3, None, vec![moves("main", Some(A), Some(B))]);
        let entries = [before, lease, after];
        assert_eq!(
            outcomes(&entries, now(&[("main", B)]).as_ref()),
            [Outcome::Took, Outcome::Unknown, Outcome::Took]
        );
    }

    #[test]
    fn the_log_appends_and_reads_a_lease_entry() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        let first = log
            .append(
                &Change::try_new(
                    TIME,
                    human(),
                    operation("snapshot"),
                    None,
                    None,
                    vec![moves("main", None, Some(A))],
                )
                .unwrap(),
            )
            .unwrap();
        let lease = LeaseEntry::acquire(TIME, human(), branch("main"), TIME + 60).unwrap();
        let written = log.append_lease(&lease).unwrap();
        assert_eq!(written.number(), 2);
        assert_eq!(written.chain(), Chain::After(first.hash().unwrap()));
        let read = log.read(2).unwrap();
        assert_eq!(read.lease(), Some(&lease));
        log.verify().expect("the whole log is in the chain");
        // The purge lists commits and never a lease entry.
        let purge = Purge::try_new(TIME, human(), 0, &[2]).unwrap();
        assert!(matches!(
            log.append_purge(purge),
            Err(OpLogError::NotAChange { number: 2 })
        ));
    }

    fn pull_entry(number: u64, pull: PullEntry) -> Entry {
        Entry::linked(number, Chain::Start, Record::Pull(pull)).expect("valid entry")
    }

    fn pull_entries() -> Vec<PullEntry> {
        vec![
            PullEntry::open(
                TIME,
                human(),
                branch("feature/x"),
                commit(A),
                vec![
                    mia(),
                    Actor::agent("claude", "opus-5-5", "s1", "daniel").unwrap(),
                ],
                Some(commit(B)),
            )
            .unwrap(),
            PullEntry::close(TIME, human(), 3).unwrap(),
            PullEntry::merge(TIME, mia(), 3, commit(B)).unwrap(),
            PullEntry::approve(TIME, mia(), 3, commit(A)).unwrap(),
        ]
    }

    #[test]
    fn a_pull_entry_is_written_as_it_is_read() {
        for pull in pull_entries() {
            let entry = pull_entry(4, pull);
            let bytes = entry.encode();
            assert!(bytes.starts_with(b"stemma-oplog 3\n"));
            let again = Entry::parse(&bytes).expect("reads back");
            assert_eq!(again, entry);
            assert_eq!(again.encode(), bytes);
            assert!(again.hash().is_some(), "a pull entry is in the chain");
            assert!(again.change().is_none());
            assert_eq!(again.pull(), entry.pull());
        }
    }

    #[test]
    fn the_three_encodings_are_fixed() {
        let text = |pull| String::from_utf8(pull_entry(7, pull).encode()).unwrap();
        let entries = pull_entries();
        let open = text(entries[0].clone());
        assert!(
            open.starts_with("stemma-oplog 3\nnumber 7\n")
                && open.contains("\noperation pr-open\n"),
            "{open}"
        );
        let body = open.split("operation pr-open\n").nth(1).unwrap();
        assert_eq!(
            body,
            format!(
                "branch feature/x\ncommit {}\nreviewer agent claude model=opus-5-5 session=s1 \
                 sponsor=daniel\nreviewer human mia\nrepairs {}\n",
                commit(A),
                commit(B)
            )
        );
        assert!(text(entries[1].clone()).ends_with("operation pr-close\npull 3\n"));
        assert!(text(entries[2].clone()).ends_with(&format!(
            "operation pr-merge\npull 3\ncommit {}\n",
            commit(B)
        )));
        assert!(text(entries[3].clone()).ends_with(&format!(
            "operation pr-approve\npull 3\ncommit {}\n",
            commit(A)
        )));
    }

    #[test]
    fn the_reader_refuses_what_the_pull_writer_does_not_write() {
        let good = |pull| String::from_utf8(pull_entry(7, pull).encode()).unwrap();
        let entries = pull_entries();
        let open = good(entries[0].clone());
        let close = good(entries[1].clone());
        let merge = good(entries[2].clone());
        let approve = good(entries[3].clone());
        let cases = [
            // A pull request entry names the chain: no older encoding has it.
            open.replace("stemma-oplog 3", "stemma-oplog 1"),
            open.replace("stemma-oplog 3", "stemma-oplog 2"),
            // Lines missing, repeated, out of order, or added.
            open.replace("branch feature/x\n", ""),
            open.replace(&format!("commit {}\n", commit(A)), ""),
            open.replace("reviewer human mia\n", "reviewer human mia\nreviewer human mia\n"),
            open.clone() + "reviewer human zoe\n",
            open.clone() + "repairs x\n",
            close.clone() + "commit x\n",
            close.replace("pull 3\n", ""),
            close.replace("pull 3", "pull 0"),
            close.replace("pull 3", "pull 03"),
            merge.replace(&format!("commit {}\n", commit(B)), ""),
            merge.replace("pull 3\n", "pull 3\nbranch main\n"),
            // An approval names the pull request and the head, and nothing else.
            approve.replace(&format!("commit {}\n", commit(A)), ""),
            approve.replace("pull 3\n", ""),
            approve.clone() + "reviewer human zoe\n",
            approve.clone() + "repairs x\n",
            // The reviewers are not in ascending order, or name the opener.
            open.replace(
                "reviewer agent claude model=opus-5-5 session=s1 sponsor=daniel\nreviewer human mia\n",
                "reviewer human mia\nreviewer agent claude model=opus-5-5 session=s1 sponsor=daniel\n",
            ),
            open.replace("reviewer human mia", "reviewer human daniel"),
        ];
        for bad in cases {
            assert!(Entry::parse(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_change_cannot_take_a_pull_operation() {
        for kind in [PR_OPEN, PR_CLOSE, PR_MERGE, PR_APPROVE] {
            let result = Change::try_new(
                TIME,
                human(),
                operation(kind),
                None,
                None,
                vec![moves("main", None, Some(A))],
            );
            assert_eq!(result, Err(EntryError::ReservedOperation), "{kind}");
        }
    }

    #[test]
    fn the_rules_of_a_pull_entry_are_checked_once() {
        let open =
            |reviewers| PullEntry::open(TIME, human(), branch("f"), commit(A), reviewers, None);
        assert_eq!(
            open(vec![human()]),
            Err(EntryError::BadReviewers),
            "the opener"
        );
        assert_eq!(open(vec![mia(), mia()]), Err(EntryError::BadReviewers));
        let many: Vec<Actor> = (0..=MAX_REVIEWERS)
            .map(|index| Actor::human(format!("r{index}")).unwrap())
            .collect();
        assert_eq!(open(many.clone()), Err(EntryError::BadReviewers));
        assert!(open(many[..MAX_REVIEWERS].to_vec()).is_ok());
        // The constructor sorts, so any order of the caller gives the one encoding.
        let sorted = open(vec![mia(), Actor::human("ana").unwrap()]).unwrap();
        assert_eq!(sorted.reviewers()[0], Actor::human("ana").unwrap());
        for number in [0, MAX_NUMBER + 1] {
            assert_eq!(
                PullEntry::close(TIME, human(), number),
                Err(EntryError::NumberOutOfRange(number))
            );
        }
        assert_eq!(
            Entry::of(3, Record::Pull(pull_entries().remove(1))),
            Err(EntryError::PullOutsideChain(3))
        );
    }

    #[test]
    fn a_reviewer_is_an_identity_so_the_same_agent_in_another_session_is_the_opener() {
        let agent = |session: &str| Actor::agent("claude", "opus-5-5", session, "daniel").unwrap();
        let open = |actor: Actor, reviewers: Vec<Actor>| {
            PullEntry::open(TIME, actor, branch("f"), commit(A), reviewers, None)
        };
        assert_eq!(
            open(agent("s1"), vec![agent("s2")]),
            Err(EntryError::BadReviewers),
            "the opener under another session"
        );
        assert_eq!(
            open(human(), vec![agent("s1"), agent("s2")]),
            Err(EntryError::BadReviewers),
            "one agent twice"
        );
        assert!(open(human(), vec![agent("s1")]).is_ok());
    }

    #[test]
    fn a_pull_entry_names_its_commits_and_becomes_a_stub() {
        let entries = pull_entries();
        let open = pull_entry(1, entries[0].clone());
        assert_eq!(open.commits(), vec![commit(A), commit(B)]);
        assert!(pull_entry(2, entries[1].clone()).commits().is_empty());
        assert_eq!(pull_entry(3, entries[2].clone()).commits(), vec![commit(B)]);
        assert_eq!(pull_entry(4, entries[3].clone()).commits(), vec![commit(A)]);
        let stub = open.to_stub().expect("an opening can become a stub");
        assert!(stub.pull().is_none() && stub.commits().is_empty());
        assert_eq!(stub.hash(), open.hash());
        let next = Entry::linked(
            2,
            Chain::After(open.hash().unwrap()),
            Record::Pull(entries[1].clone()),
        )
        .unwrap();
        check_chain(&[open, next.clone()]).expect("the chain holds");
        check_chain(&[stub, next]).expect("the chain holds over the stub");
    }

    #[test]
    fn the_log_appends_reads_and_lets_a_purge_list_a_pull_entry() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        let open = log.append_pull(&pull_entries().remove(0)).unwrap();
        assert_eq!(open.number(), 1);
        assert_eq!(log.read(1).unwrap().pull(), open.pull());
        log.verify().expect("the whole log is in the chain");
        let purge = Purge::try_new(TIME, human(), 2, &[1]).unwrap();
        let written = log.append_purge(purge).unwrap();
        assert_eq!(written.len(), 1);
        assert!(matches!(written[0].record(), Record::Purge(_)));
        log.verify().expect("the whole log is in the chain");
    }

    // ----- the entries that name a signed statement (ADR-025, STEMMA-290) -----

    fn statement_hash() -> StatementHash {
        C.parse().unwrap()
    }

    #[test]
    fn an_entry_of_a_pull_request_names_its_statement_in_its_last_line() {
        for pull in pull_entries() {
            let kind = pull.kind();
            let old = pull_entry(4, pull.clone());
            assert_eq!(
                old.statement(),
                None,
                "an entry of an older format names none"
            );
            let Ok(named) = pull.with_statement(statement_hash()) else {
                assert_eq!(kind, PullKind::Merge, "only a merge names no statement");
                continue;
            };
            assert_ne!(kind, PullKind::Merge);
            let entry = pull_entry(4, named);
            assert_eq!(entry.statement(), Some(statement_hash()));
            assert_eq!(entry.pull().unwrap().statement(), Some(statement_hash()));
            let bytes = entry.encode();
            let text = String::from_utf8(bytes.clone()).unwrap();
            // The line comes after the lines of the older form, which stay as they were.
            let old_text = String::from_utf8(old.encode()).unwrap();
            assert_eq!(text, format!("{old_text}statement {C}\n"));
            let again = Entry::parse(&bytes).expect("reads back");
            assert_eq!(again, entry);
            assert_eq!(again.encode(), bytes);
            // The hash of the body covers the line, so the chain covers the name.
            assert_ne!(entry.body_hash(), old.body_hash());
            assert_ne!(entry.hash(), old.hash());
            // The entry names the same commits, which a purge looks for.
            assert_eq!(entry.commits(), old.commits());
            // The older form is still read as what it is.
            assert_eq!(Entry::parse(&old.encode()).unwrap(), old);
        }
    }

    #[test]
    fn the_reader_refuses_a_statement_line_that_the_writer_does_not_write() {
        let entries = pull_entries();
        let text =
            |pull: &PullEntry| String::from_utf8(pull_entry(4, pull.clone()).encode()).unwrap();
        let refused = |text: String| Entry::parse(text.as_bytes()).unwrap_err();
        // A merge names no statement.
        let merge = format!("{}statement {C}\n", text(&entries[2]));
        assert!(
            matches!(
                refused(merge),
                EntryParseError::Malformed {
                    reason: EntryMalformed::TrailingLine,
                    ..
                }
            ),
            "a merge with a statement was read"
        );
        let close = text(&entries[1]);
        // A hash in another form, a second line, and a line that does not come last.
        let cases = [
            format!("{close}statement {}\n", C.to_uppercase()),
            format!("{close}statement -\n"),
            format!("{close}statement {C}\nstatement {C}\n"),
            close.replace("pull 3\n", &format!("statement {C}\npull 3\n")),
        ];
        for case in cases {
            let error = refused(case.clone());
            assert!(
                matches!(
                    error,
                    EntryParseError::InvalidObjectId { .. } | EntryParseError::Malformed { .. }
                ),
                "{case}: {error:?}"
            );
        }
    }

    #[test]
    fn a_check_that_names_its_statement_has_version_2() {
        for check in check_entries() {
            let old = check_entry(4, check.clone());
            assert_eq!(old.statement(), None);
            let entry = check_entry(4, check.with_statement(statement_hash()));
            assert_eq!(entry.statement(), Some(statement_hash()));
            assert_eq!(entry.check().unwrap().statement(), Some(statement_hash()));
            let text = String::from_utf8(entry.encode()).unwrap();
            let old_text = String::from_utf8(old.encode()).unwrap();
            assert_eq!(
                text,
                format!(
                    "{}statement {C}\n",
                    old_text.replace("\nversion 1\n", "\nversion 2\n")
                )
            );
            let again = Entry::parse(text.as_bytes()).expect("reads back");
            assert_eq!(again, entry);
            assert_ne!(entry.hash(), old.hash());
            assert_eq!(entry.commits(), old.commits());
            // The version and the line go together: neither form takes the other's.
            let without = text.replace(&format!("statement {C}\n"), "");
            assert!(
                matches!(
                    Entry::parse(without.as_bytes()),
                    Err(EntryParseError::Malformed {
                        reason: EntryMalformed::ExpectedStatement,
                        ..
                    })
                ),
                "version 2 without a statement was read"
            );
            let with = format!("{old_text}statement {C}\n");
            assert!(
                matches!(
                    Entry::parse(with.as_bytes()),
                    Err(EntryParseError::Malformed {
                        reason: EntryMalformed::TrailingLine,
                        ..
                    })
                ),
                "version 1 with a statement was read"
            );
            let later = text.replace("\nversion 2\n", "\nversion 3\n");
            assert!(
                matches!(
                    Entry::parse(later.as_bytes()),
                    Err(EntryParseError::Malformed {
                        reason: EntryMalformed::ExpectedVersion,
                        ..
                    })
                ),
                "an unknown version was read"
            );
        }
    }

    fn check_entry(number: u64, check: CheckEntry) -> Entry {
        Entry::linked(number, Chain::Start, Record::Check(check)).expect("valid entry")
    }

    fn check_entries() -> Vec<CheckEntry> {
        vec![
            CheckEntry::new(TIME, mia(), commit(A), Some(commit(B)), CheckResult::Pass),
            CheckEntry::new(TIME, human(), commit(B), None, CheckResult::Fail),
        ]
    }

    #[test]
    fn a_check_entry_is_written_as_it_is_read() {
        for check in check_entries() {
            let entry = check_entry(4, check);
            let bytes = entry.encode();
            assert!(bytes.starts_with(b"stemma-oplog 3\n"));
            let again = Entry::parse(&bytes).expect("reads back");
            assert_eq!(again, entry);
            assert_eq!(again.encode(), bytes);
            assert!(again.hash().is_some(), "a check entry is in the chain");
            assert!(again.change().is_none() && again.pull().is_none());
            assert_eq!(again.check(), entry.check());
        }
    }

    #[test]
    fn the_two_encodings_of_a_check_are_fixed() {
        let text = |check| String::from_utf8(check_entry(7, check).encode()).unwrap();
        let entries = check_entries();
        let with_base = text(entries[0].clone());
        assert!(
            with_base.starts_with("stemma-oplog 3\nnumber 7\n")
                && with_base.contains("\noperation check\n"),
            "{with_base}"
        );
        assert!(with_base.ends_with(&format!(
            "operation check\nversion 1\ncommit {}\nbase {}\nresult pass\n",
            commit(A),
            commit(B)
        )));
        assert!(text(entries[1].clone()).ends_with(&format!(
            "operation check\nversion 1\ncommit {}\nbase -\nresult fail\n",
            commit(B)
        )));
    }

    #[test]
    fn the_reader_refuses_what_the_check_writer_does_not_write() {
        let good = String::from_utf8(check_entry(7, check_entries().remove(0)).encode()).unwrap();
        let cases = [
            // A check names the chain: no older encoding has it.
            good.replace("stemma-oplog 3", "stemma-oplog 1"),
            good.replace("stemma-oplog 3", "stemma-oplog 2"),
            // Another version, or none.
            good.replace("version 1", "version 2"),
            good.replace("version 1\n", ""),
            // Lines missing, out of order, repeated, or added.
            good.replace(&format!("commit {}\n", commit(A)), ""),
            good.replace(&format!("base {}\n", commit(B)), ""),
            good.replace("result pass\n", ""),
            good.clone() + "result pass\n",
            good.clone() + "base -\n",
            good.replace("result pass", "result green"),
            good.replace("result pass", "result Pass"),
            good.replace(&format!("base {}", commit(B)), "base x"),
            good.replace(&format!("base {}", commit(B)), "base"),
            // A run merged onto itself.
            good.replace(
                &format!("base {}", commit(B)),
                &format!("base {}", commit(A)),
            ),
        ];
        for bad in cases {
            assert!(Entry::parse(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_hash_of_a_check_covers_each_field_of_its_body() {
        let good = String::from_utf8(check_entry(7, check_entries().remove(0)).encode()).unwrap();
        let hash = |text: &str| Entry::parse(text.as_bytes()).unwrap().hash();
        let original = hash(&good);
        assert!(original.is_some());
        for changed in [
            good.replace("result pass", "result fail"),
            good.replace(&format!("base {}", commit(B)), "base -"),
            good.replace(
                &format!("commit {}", commit(A)),
                &format!("commit {}", commit(C)),
            ),
        ] {
            assert_ne!(hash(&changed), original, "{changed}");
        }
        // The writer, the time, and the number are part of the hash too: the proof of a check
        // signs it, so a proof of one writer is no proof of another.
        let first = check_entries().remove(0);
        let other = |number: u64, time: i64, actor: Actor| {
            let check = CheckEntry::new(
                time,
                actor,
                *first.commit(),
                first.base().copied(),
                first.result(),
            );
            check_entry(number, check).hash()
        };
        let same = other(7, TIME, mia());
        assert_eq!(same, original);
        assert_ne!(other(7, TIME, human()), same);
        assert_ne!(other(7, TIME + 1, mia()), same);
        assert_ne!(other(8, TIME, mia()), same);
    }

    #[test]
    fn a_check_cannot_name_its_own_commit_as_its_base() {
        let own = CheckEntry::new(TIME, mia(), commit(A), Some(commit(A)), CheckResult::Pass);
        assert_eq!(
            Entry::linked(5, Chain::Start, Record::Check(own)),
            Err(EntryError::CheckBaseIsCommit(5))
        );
    }

    #[test]
    fn a_change_cannot_take_the_check_operation() {
        let result = Change::try_new(
            TIME,
            human(),
            operation(CHECK),
            None,
            None,
            vec![moves("main", None, Some(A))],
        );
        assert_eq!(result, Err(EntryError::ReservedOperation));
        assert_eq!(
            Entry::of(3, Record::Check(check_entries().remove(0))),
            Err(EntryError::CheckOutsideChain(3))
        );
    }

    #[test]
    fn a_check_entry_names_its_commits_and_becomes_a_stub() {
        let entries = check_entries();
        let with_base = check_entry(1, entries[0].clone());
        assert_eq!(with_base.commits(), vec![commit(A), commit(B)]);
        assert_eq!(
            check_entry(2, entries[1].clone()).commits(),
            vec![commit(B)]
        );
        let stub = with_base.to_stub().expect("a check can become a stub");
        assert!(stub.check().is_none() && stub.commits().is_empty());
        assert_eq!(stub.hash(), with_base.hash());
        let next = Entry::linked(
            2,
            Chain::After(with_base.hash().unwrap()),
            Record::Check(entries[1].clone()),
        )
        .unwrap();
        check_chain(&[with_base, next.clone()]).expect("the chain holds");
        check_chain(&[stub, next]).expect("the chain holds over the stub");
    }

    #[test]
    fn the_log_appends_reads_and_lets_a_purge_list_a_check_entry() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        let written = log.append_check(&check_entries().remove(0)).unwrap();
        assert_eq!(written.number(), 1);
        assert_eq!(log.read(1).unwrap().check(), written.check());
        log.verify().expect("the whole log is in the chain");
        let purge = Purge::try_new(TIME, human(), 2, &[1]).unwrap();
        log.append_purge(purge).unwrap();
        log.verify().expect("the whole log is in the chain");
    }

    // The entry that pins the key of a hub again (STEMMA-297).

    fn hub_key(seed: u8) -> PublicKey {
        crate::protocol::record::tests::Seeded::new(seed).public()
    }

    fn remote_pin(old: Option<u8>) -> RemotePinEntry {
        RemotePinEntry::try_new(
            TIME,
            human(),
            RemoteName::try_new("origin").unwrap(),
            old.map(hub_key),
            hub_key(0x71),
        )
        .unwrap()
    }

    fn pin_entry(pin: RemotePinEntry) -> Entry {
        Entry::linked(7, Chain::Start, Record::RemotePin(pin)).unwrap()
    }

    #[test]
    fn a_remote_pin_entry_is_written_as_it_is_read() {
        for pin in [remote_pin(Some(0x70)), remote_pin(None)] {
            let entry = pin_entry(pin.clone());
            let text = String::from_utf8(entry.encode()).unwrap();
            let old = pin
                .old()
                .map_or_else(|| "-".to_owned(), PublicKey::to_openssh);
            assert!(
                text.starts_with("stemma-oplog 3\nnumber 7\n")
                    && text.ends_with(&format!(
                        "previous -\noperation remote-pin\nremote origin\nold {old}\nnew {}\n",
                        hub_key(0x71)
                    )),
                "{text}"
            );
            let read = Entry::parse(text.as_bytes()).unwrap();
            assert_eq!(read, entry);
            assert_eq!(read.remote_pin(), Some(&pin));
            assert_eq!(read.time(), TIME);
            assert_eq!(read.actor(), &human());
            assert!(read.commits().is_empty());
            assert_eq!(read.change(), None);
            assert_eq!(read.statement(), None);
        }
    }

    #[test]
    fn the_reader_refuses_what_the_remote_pin_writer_does_not_write() {
        let good = String::from_utf8(pin_entry(remote_pin(Some(0x70))).encode()).unwrap();
        let old = hub_key(0x70).to_string();
        let new = hub_key(0x71).to_string();
        let cases = [
            // The entry names the chain: no older encoding has it.
            good.replace("stemma-oplog 3", "stemma-oplog 2"),
            good.replace("stemma-oplog 3", "stemma-oplog 4"),
            // Lines missing, out of order, repeated, or added.
            good.replace("remote origin\n", ""),
            good.replace(&format!("old {old}\n"), ""),
            good.replace(&format!("new {new}\n"), ""),
            good.clone() + &format!("new {new}\n"),
            good.replace(
                &format!("old {old}\nnew {new}\n"),
                &format!("new {new}\nold {old}\n"),
            ),
            // A name that no remote has, a key that is not canonical, the same key twice.
            good.replace("remote origin", "remote a/b"),
            good.replace("remote origin", "remote "),
            good.replace(&format!("old {old}"), "old ssh-ed25519 AAAA"),
            good.replace(&format!("old {old}"), &format!("old {old} comment")),
            good.replace(&format!("new {new}"), "new -"),
            good.replace(&format!("old {old}"), &format!("old {new}")),
            // An agent does not pin the key of a hub.
            good.replace(
                "actor human daniel",
                "actor agent a1 model=m session=s1 sponsor=daniel",
            ),
        ];
        for bad in cases {
            assert!(Entry::parse(bad.as_bytes()).is_err(), "{bad}");
        }
        // A program that does not know the entry reads it as a change without a pointer, which
        // is damage: the reason for the repository format 18.
        let change = good.replace("operation remote-pin", "operation remote-pinned");
        assert!(Entry::parse(change.as_bytes()).is_err());
    }

    #[test]
    fn a_remote_pin_entry_is_refused_for_the_same_key_outside_the_chain_and_as_a_stub() {
        assert_eq!(
            RemotePinEntry::try_new(
                TIME,
                human(),
                RemoteName::try_new("origin").unwrap(),
                Some(hub_key(0x71)),
                hub_key(0x71),
            ),
            Err(EntryError::RemotePinUnchanged)
        );
        assert_eq!(
            Entry::linked(3, Chain::Unchained, Record::RemotePin(remote_pin(None))),
            Err(EntryError::RemotePinOutsideChain(3))
        );
        assert_eq!(
            pin_entry(remote_pin(None)).to_stub(),
            Err(EntryError::RemotePinNotReplaceable(7))
        );
        assert_eq!(
            RemotePinEntry::try_new(
                TIME,
                Actor::agent("a1", "m", "s1", "daniel").unwrap(),
                RemoteName::try_new("origin").unwrap(),
                None,
                hub_key(0x71),
            ),
            Err(EntryError::RemotePinByAgent)
        );
        for error in [
            EntryError::RemotePinByAgent,
            EntryError::RemotePinUnchanged,
            EntryError::RemotePinOutsideChain(3),
            EntryError::RemotePinNotReplaceable(7),
        ] {
            assert!(!error.to_string().is_empty());
        }
        // A change cannot take the name of the operation.
        assert_eq!(
            Change::try_new(
                TIME,
                human(),
                OperationName::try_new(REMOTE_PIN).unwrap(),
                None,
                None,
                vec![(
                    BranchName::try_new("main").unwrap(),
                    Moved::from_to(None, Some(commit(A)))
                )],
            ),
            Err(EntryError::ReservedOperation)
        );
    }

    #[test]
    fn the_log_appends_and_reads_a_remote_pin_entry() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        let written = log.append_remote_pin(&remote_pin(Some(0x70))).unwrap();
        assert_eq!(log.read(1).unwrap().remote_pin(), written.remote_pin());
        log.verify().expect("the whole log is in the chain");
    }
}
