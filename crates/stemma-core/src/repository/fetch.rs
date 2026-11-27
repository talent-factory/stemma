//! The fetch and the clone of a clone (ADR-008, ADR-023, STEMMA-295): the client side of the
//! transfer, through a [`Transport`].
//!
//! A fetch is the list of references and one round with `want` and `have` (ADR-023):
//!
//! 1. The authorizer is asked for `fetch` with the target `remote <name>`.
//! 2. The list of references is read. Its repository ID must be the one that the configuration
//!    of the remote holds: another ID means that another repository lies under the same address,
//!    and nothing moves ([`FetchError::OtherRepository`]).
//! 3. The receiving side begins ([`Repository::begin_fetch`]): the journal `fetch-pending` is
//!    opened, and the commits that an interrupted fetch recorded in it count as `have` if they
//!    still hold.
//! 4. The request wants every listed commit that is no `have` commit, and has the `have`
//!    commits. A list whose commits are all known needs no request.
//! 5. The stream is stored and checked commit by commit, and then every listed commit is checked
//!    for connectivity. No remote reference moves before that check has passed.
//! 6. Under the lock of the references, one entry of the operation `fetch` is written with every
//!    remote reference of the remote that moves, is created, or is removed (old and new state,
//!    commit and record entry), and then they move. A remote reference that the list no longer
//!    names is removed, because the list is whole (ADR-008); `op restore` of the entry brings it
//!    back. A purge that wrote its entry since the fetch began may have removed an object that
//!    the check of step 5 saw, and so may the abort of a purge that had stopped before the fetch
//!    began, so the check is made once more under the lock in both cases. A purge that starts,
//!    stops, and is aborted while the fetch receives leaves no trace; its abort removes only
//!    objects that it wrote itself, which the stream would have to bring with the same content.
//! 7. The journal is removed.
//!
//! A fetch that stops before step 6 leaves the journal, and the next fetch resumes from it. A
//! fetch whose remote references have their values already writes no entry.
//!
//! A clone is the same fetch without `have` into a new repository: it reads the list, makes the
//! repository with [`Repository::init`], writes the configuration of the remote with the address
//! and the repository ID of the list, and fetches with the list that it read, so it costs the
//! same two requests. It pins the key of the hub that the list names before it makes anything
//! (ADR-026), a fetch refuses a list whose key is not the pinned one, and a
//! sign-in, where the remote needs one, is made by the caller on the transport before
//! ([`transport::sign_in`](crate::transport::sign_in)). A clone creates no local branch: which
//! local branch follows which remote reference belongs to the command surface (ADR-008).
//!
//! The level of this code is 2 by ADR-004 (network, references, and the operation log); the
//! command asks the authorizer for `fetch` at run time, as `restore` asks for `restore`.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use super::{now, InitError, InitOptions, Repository, UpdateError};
use crate::object::CommitId;
use crate::oplog::{Change, Entry, Moved, OpLogError, OperationName, Record};
use crate::protocol::{ErrorMessage, FetchRequest, ParseError, RefList, RepositoryId};
use crate::receive::{Bounds, ConnectivityError, FetchJournalError, ReceiveError, Received};
use crate::refs::{RefError, RefStore, PURGE_JOURNAL};
use crate::remote::{
    self, HubKeyError, RemoteConfig, RemoteError, RemoteName, RemoteRef, RemoteRefName,
};
// The setup of a remote on the first contact (STEMMA-305).
use super::remote_config::RemoteSetup;
use crate::transport::{Answer, Body, Transport, TransportError};
use crate::{Actor, Allowed, Authorizer};

/// The operation that the log records for a fetch, and that the authorizer is asked for.
pub const FETCH: &str = "fetch";

/// The longest list of references that a fetch reads: 64 MiB. A reference takes a line of about
/// 160 bytes, so the bound holds more references than a repository at the target scale has
/// (ADR-015); a longer answer is refused before it is parsed.
pub const MAX_REF_LIST: u64 = 64 << 20;

/// The longest error message that a fetch reads in place of an answer: 64 KiB.
const MAX_ERROR_MESSAGE: u64 = 64 << 10;

/// What a fetch did.
#[derive(Debug)]
#[non_exhaustive]
pub struct Fetched {
    /// The entry `fetch`, or `None` if every remote reference of the remote had its value
    /// already, so that nothing moved and no entry was written.
    pub entry: Option<Entry>,
    /// What the stream carried, or zeros if no request was needed.
    pub received: Received,
    /// The commits that an interrupted fetch had recorded in the journal and that this fetch
    /// found complete and sent as `have` (see [`Fetch::resumed`](crate::receive::Fetch::resumed)).
    pub resumed: Vec<CommitId>,
}

impl Repository {
    /// Fetches from the remote `remote` through `transport` and moves its remote references to
    /// the state of the list of references (ADR-008, ADR-023). The steps: the authorizer is
    /// asked for `fetch` with the target `remote <name>`; the list is read, and its repository
    /// ID must be the one of the configuration; the journal `fetch-pending` is opened, and its
    /// commits that still hold count as `have`; every listed commit that is no `have` is wanted,
    /// and no request is sent if there is none; the stream is stored, and every listed commit is
    /// checked for connectivity; under the lock of the references, one entry `fetch` names every
    /// remote reference of the remote that moves, is created, or is removed, and then they move;
    /// the journal is removed. A remote reference that the list no longer names is removed, and
    /// `op restore` of the entry brings it back. A purge that wrote an entry since the fetch
    /// began, or a journal of a purge at its start, makes the check run once more under the
    /// lock. A remote whose configuration says `sign-in yes` is signed in on `transport` by the
    /// caller first.
    ///
    /// # Errors
    ///
    /// Before anything is written: [`FetchError::Refs`] with [`RefError::Bare`] in a bare
    /// repository, [`FetchError::Update`] with [`UpdateError::Denied`] for a refusal of the
    /// authorizer, [`FetchError::Remote`] if the configuration cannot be read and
    /// [`FetchError::NoSuchRemote`] if there is none, [`FetchError::Transport`],
    /// [`FetchError::Body`], [`FetchError::TooLarge`], [`FetchError::List`],
    /// [`FetchError::Refused`], and [`FetchError::Unreadable`] for the two requests and their
    /// answers, [`FetchError::OtherRepository`] for a list of another repository, and
    /// [`FetchError::HubKey`] if the configuration pinned no key of the hub or the list names
    /// another one. Under the lock, [`FetchError::RemoteChanged`] if the configuration was removed
    /// or changed during the transfer; what arrived stays for the next fetch.
    ///
    /// The receiving side: [`FetchError::Receive`] if the journal cannot be opened or the stream
    /// is refused, cut off, or not complete, and [`FetchError::Connectivity`] if a listed commit
    /// is not complete. What arrived whole stays in the store and in the journal, and the next
    /// fetch resumes.
    ///
    /// The moves: [`FetchError::Update`] as for any change of a pointer (a remote reference in
    /// the way, the lock, the entry, and a move that failed after it). [`FetchError::JournalLeft`]
    /// if the references moved and the journal could not be removed: the fetch has happened.
    pub fn fetch(
        &self,
        remote: &RemoteName,
        transport: &mut dyn Transport,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Fetched, FetchError> {
        self.require_oplog("fetch").map_err(FetchError::Refs)?;
        let allowed = authorize_fetch(remote, actor, authorizer)?;
        let config = self
            .remote(remote)
            .map_err(FetchError::Remote)?
            .ok_or_else(|| FetchError::NoSuchRemote(remote.clone()))?;
        let list = read_list(transport)?;
        if list.repository() != config.repository {
            return Err(FetchError::OtherRepository {
                remote: remote.clone(),
                expected: config.repository,
                found: list.repository(),
            });
        }
        // The pinned key of the hub must be the key that the list names (ADR-026, STEMMA-305):
        // a changed key moves nothing, and a configuration that pinned none is not trusted.
        config.hub_keys(&list).map_err(FetchError::HubKey)?;
        self.fetch_listed(remote, &config, &list, transport, actor, &allowed)
    }

    /// Makes a new repository at `path` as a clone of the repository at the address of `setup`,
    /// reached through `transport`, under the remote name `remote` (ADR-023): the list is read
    /// first, and the key of the hub that it names is pinned against the expected fingerprint of
    /// `setup` if there is one (ADR-026), so that a failure there makes nothing; then
    /// [`init`](Self::init) runs with `options`, the configuration of the remote is written with
    /// the repository ID of the list, the pinned key, and the CA file and the sign-in of
    /// `setup`, and the fetch runs with that list, so the clone costs the two requests of a
    /// fetch. It makes no local branch.
    ///
    /// The authorizer is asked for `fetch` first, before anything is made, and for `init` by
    /// `init`.
    ///
    /// # Errors
    ///
    /// [`CloneError::Before`] for a failure of the list or of the key of the hub
    /// ([`FetchError::HubKey`]), before anything was made;
    /// [`CloneError::Init`] if the repository cannot be made; and [`CloneError::After`] for a
    /// failure after the repository was made, which stays where it is with what it holds. Its
    /// `configured` says whether the configuration of the remote was written: then a
    /// [`fetch`](Self::fetch) of `remote` there goes on, and otherwise the directory holds an
    /// empty repository without the remote.
    pub fn clone_remote(
        path: impl AsRef<Path>,
        options: &InitOptions,
        remote: &RemoteName,
        setup: &RemoteSetup,
        transport: &mut dyn Transport,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<(Self, Fetched), CloneError> {
        let path = path.as_ref();
        // The authorizer is asked before anything is made, so a refusal leaves no directory.
        let allowed = authorize_fetch(remote, actor, authorizer).map_err(CloneError::Before)?;
        let list = read_list(transport).map_err(CloneError::Before)?;
        // The first contact pins the key of the hub (ADR-026, STEMMA-305).
        let config = setup
            .first_contact(&list)
            .map_err(|error| CloneError::Before(FetchError::HubKey(error)))?;
        let repository = Self::init(path, options, actor, authorizer).map_err(CloneError::Init)?;
        let after = |configured, source| CloneError::After {
            path: path.to_path_buf(),
            configured,
            source: Box::new(source),
        };
        {
            // The configuration is written under the lock, as every file of the shared metadata
            // directory that a writer changes.
            let _lock = repository
                .refs_store()
                .lock_writer(&allowed)
                .map_err(|error| after(false, FetchError::Update(UpdateError::Refs(error))))?;
            remote::write_config(
                &repository.metadata_dir(),
                &repository.metadata_dir().join("tmp"),
                remote,
                &config,
            )
            .map_err(|error| after(false, FetchError::Remote(error)))?;
        }
        let fetched = repository
            .fetch_listed(remote, &config, &list, transport, actor, &allowed)
            .map_err(|error| after(true, error))?;
        Ok((repository, fetched))
    }

    /// Steps 3 to 7 of a fetch, with the list that was read.
    fn fetch_listed(
        &self,
        remote: &RemoteName,
        config: &RemoteConfig,
        list: &RefList,
        transport: &mut dyn Transport,
        actor: &Actor,
        allowed: &Allowed,
    ) -> Result<Fetched, FetchError> {
        // The last entry before the transfer: a purge after it is seen under the lock. A purge
        // that stopped before its entry may be aborted meanwhile, which removes the objects
        // that it wrote and writes no entry, so its journal counts as well.
        let before = self.last_entry().map_err(log_error)?;
        let journal = self.metadata_dir().join(PURGE_JOURNAL);
        let purge_pending = match std::fs::symlink_metadata(&journal) {
            Ok(_) => true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(source) => {
                return Err(FetchError::Refs(RefError::Io {
                    operation: "inspect the journal of a purge",
                    path: journal,
                    source,
                }))
            }
        };
        let listed: BTreeSet<CommitId> = list
            .references()
            .iter()
            .map(|reference| reference.commit())
            .collect();
        let mut fetch = self.begin_fetch().map_err(FetchError::Receive)?;
        let resumed = fetch.resumed().to_vec();
        let want: BTreeSet<CommitId> = listed.difference(fetch.have()).copied().collect();
        let mut received = Received::default();
        if !want.is_empty() {
            let request = FetchRequest::try_new(want.iter().copied(), fetch.have().iter().copied())
                .expect("a request with a wanted commit can be built");
            let encoded = request.encode();
            let answer = transport
                .fetch(&mut encoded.as_slice())
                .map_err(FetchError::Transport)?;
            match answer {
                Answer::Message(body) => {
                    received = fetch
                        .receive(body, Bounds::default())
                        .map_err(FetchError::Receive)?;
                }
                Answer::Error(body) => return Err(refused(body)),
            }
        }
        fetch.check(&listed).map_err(FetchError::Connectivity)?;

        // The state that the list gives the remote references of this remote: every listed
        // reference, and no other.
        let mut targets: Vec<(RemoteRefName, Option<RemoteRef>)> = list
            .references()
            .iter()
            .map(|reference| {
                let name = RemoteRefName {
                    remote: remote.clone(),
                    kind: reference.name().kind(),
                    name: reference.name().name().clone(),
                };
                let value = RemoteRef {
                    commit: reference.commit(),
                    entry: reference.entry(),
                };
                (name, Some(value))
            })
            .collect();
        let operation = OperationName::try_new(FETCH).expect("`fetch` is a valid operation name");
        let lock = self
            .refs_store()
            .lock_writer(allowed)
            .map_err(|error| FetchError::Update(UpdateError::Refs(error)))?;
        // The configuration is read once more under the lock (STEMMA-305): a `remote remove` or
        // a pinning during the transfer must not be undone by remote references of the remote
        // as it was before.
        if self.remote(remote).map_err(FetchError::Remote)?.as_ref() != Some(config) {
            return Err(FetchError::RemoteChanged(remote.clone()));
        }
        // The references to remove are read under the lock, so that none that a writer made in
        // between stays beside the list.
        let kept: BTreeSet<RemoteRefName> = targets.iter().map(|(name, _)| name.clone()).collect();
        for (name, _) in self.refs_store().remote_refs().map_err(FetchError::Refs)? {
            if name.remote == *remote && !kept.contains(&name) {
                targets.push((name, None));
            }
        }
        if purge_pending || self.purged_since(before).map_err(log_error)? {
            let tips: Vec<CommitId> = listed.iter().copied().collect();
            self.check_connectivity(&tips)
                .map_err(FetchError::Connectivity)?;
        }
        let logged = self
            .refs_store()
            .move_remotes_after(&lock, &targets, self.objects(), |moves| {
                let remotes = moves
                    .iter()
                    .map(|(name, old, new)| (name.clone(), Moved::from_to(*old, *new)))
                    .collect();
                let change =
                    Change::try_new_with_remotes(now(), actor.clone(), operation.clone(), remotes)
                        .map_err(OpLogError::Entry)?;
                self.append(&lock, &change)
            })
            .map_err(|error| FetchError::Update(UpdateError::Refs(error)))?;
        let entry = logged.into_result().map_err(FetchError::Update)?;
        drop(lock);
        let fetched = Fetched {
            entry,
            received,
            resumed,
        };
        match fetch.finish() {
            Ok(()) => Ok(fetched),
            Err(source) => Err(FetchError::JournalLeft {
                fetched: Box::new(fetched),
                source,
            }),
        }
    }

    /// The number of the last entry of the log, or 0 for a log without one.
    fn last_entry(&self) -> Result<u64, OpLogError> {
        Ok(self
            .oplog()
            .window(1)?
            .entries()
            .last()
            .map_or(0, Entry::number))
    }

    /// Whether an entry after `before` is a purge or a stub: a purge ran since, and may have
    /// removed an object that a check without the lock saw (ADR-014).
    fn purged_since(&self, before: u64) -> Result<bool, OpLogError> {
        let last = self.last_entry()?;
        for number in before.saturating_add(1)..=last {
            if matches!(
                self.oplog().read(number)?.record(),
                Record::Purge(_) | Record::Stub(_)
            ) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Asks the authorizer for `fetch` from the remote `remote`, with the target `remote <name>`.
fn authorize_fetch(
    remote: &RemoteName,
    actor: &Actor,
    authorizer: &dyn Authorizer,
) -> Result<Allowed, FetchError> {
    let operation = OperationName::try_new(FETCH).expect("`fetch` is a valid operation name");
    let target = format!("remote {remote}");
    Allowed::ask(authorizer, actor, &operation, &target).ok_or_else(|| {
        FetchError::Update(UpdateError::Denied {
            operation: operation.to_string(),
            target,
        })
    })
}

/// An error of the log before any entry of the fetch was written.
fn log_error(error: OpLogError) -> FetchError {
    FetchError::Update(UpdateError::Log(error))
}

// The list of references for a command (STEMMA-305).

/// Asks the other side for its list of references and reads it, as the first step of a fetch
/// does. `remote pin` reads it so, to pin the key that the hub names now.
///
/// # Errors
///
/// [`FetchError::Transport`], [`FetchError::Body`], [`FetchError::TooLarge`],
/// [`FetchError::List`], [`FetchError::Refused`], and [`FetchError::Unreadable`], as for the
/// first request of a fetch.
pub fn read_ref_list(transport: &mut dyn Transport) -> Result<RefList, FetchError> {
    read_list(transport)
}

/// Asks for the list of references and reads it.
fn read_list(transport: &mut dyn Transport) -> Result<RefList, FetchError> {
    match transport.refs().map_err(FetchError::Transport)? {
        Answer::Message(body) => {
            let bytes = read_bounded(body, MAX_REF_LIST)?;
            RefList::parse(&bytes).map_err(FetchError::List)
        }
        Answer::Error(body) => Err(refused(body)),
    }
}

/// Reads a small answer to its end, up to `bound` bytes.
fn read_bounded(body: Body<'_>, bound: u64) -> Result<Vec<u8>, FetchError> {
    let mut bytes = Vec::new();
    body.take(bound + 1)
        .read_to_end(&mut bytes)
        .map_err(FetchError::Body)?;
    if bytes.len() as u64 > bound {
        return Err(FetchError::TooLarge);
    }
    Ok(bytes)
}

/// The error that the other side answered, or why it cannot be read.
fn refused(body: Body<'_>) -> FetchError {
    match read_bounded(body, MAX_ERROR_MESSAGE) {
        Ok(bytes) => match ErrorMessage::parse(&bytes) {
            Ok(message) => FetchError::Refused(message),
            Err(error) => FetchError::Unreadable(error),
        },
        Err(error) => error,
    }
}

/// Why [`Repository::fetch`] failed. Every variant but `Update` (after the entry) and
/// `JournalLeft` moved no remote reference.
#[derive(Debug)]
#[non_exhaustive]
pub enum FetchError {
    /// The references could not be read, or the repository is bare ([`RefError::Bare`]).
    Refs(RefError),
    /// The authorizer refused `fetch`, or the change of the remote references failed (see
    /// [`UpdateError`]: before the entry, nothing moved; with an entry, the error says what did).
    Update(UpdateError),
    /// The configuration of the remote could not be read or written.
    Remote(RemoteError),
    /// The repository has no remote of this name.
    NoSuchRemote(RemoteName),
    /// A request got no answer.
    Transport(TransportError),
    /// The body of an answer could not be read to its end.
    Body(io::Error),
    /// The list of references, or an error in its place, is longer than its bound
    /// ([`MAX_REF_LIST`], 64 KiB for an error).
    TooLarge,
    /// The reader refused the list of references.
    List(ParseError),
    /// The other side answered an error.
    Refused(ErrorMessage),
    /// The other side answered an error that the reader refused.
    Unreadable(ParseError),
    /// The list of references names another repository ID than the configuration of the remote:
    /// another repository lies under the address now (ADR-027). Nothing was received.
    OtherRepository {
        /// The remote.
        remote: RemoteName,
        /// The ID that the configuration holds.
        expected: RepositoryId,
        /// The ID that the list names.
        found: RepositoryId,
    },
    /// The receiving side failed: the journal, the stream, its bounds, the store, or the check
    /// of a commit of the stream. What arrived whole is kept for the next fetch.
    Receive(ReceiveError),
    /// A listed commit is not complete in the store, or the check could not be made.
    Connectivity(ConnectivityError),
    /// The fetch has happened (the entry is written and the remote references moved, or they had
    /// their values already), but the journal `fetch-pending` could not be removed. The next
    /// fetch reads the journal that stayed.
    JournalLeft {
        /// What the fetch did.
        fetched: Box<Fetched>,
        /// Why the journal stayed.
        source: FetchJournalError,
    },
    // The key of the hub (ADR-026, STEMMA-305).
    /// The hub names another key than the pinned one, the list is that of another repository,
    /// or a first contact found another key than the expected one. Nothing moved.
    HubKey(HubKeyError),
    /// The configuration of the remote was removed or changed while the fetch received, which a
    /// `remote remove` or a pinning does. What arrived stays in the store and in the journal;
    /// nothing moved.
    RemoteChanged(RemoteName),
}

impl FetchError {
    /// The entry `fetch`, if it was written.
    pub fn entry(&self) -> Option<&Entry> {
        match self {
            Self::Update(error) => error.entry(),
            Self::JournalLeft { fetched, .. } => fetched.entry.as_ref(),
            _ => None,
        }
    }

    /// The error of the file system, if that is what failed (see the crate documentation).
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Refs(error) => error.io_error(),
            Self::Update(error) => error.io_error(),
            Self::Remote(error) => error.io_error(),
            Self::Transport(error) => error.io_error(),
            Self::Body(error) => Some(error),
            Self::Receive(error) => error.io_error(),
            Self::Connectivity(error) => error.io_error(),
            Self::JournalLeft { source, .. } => source.io_error(),
            Self::NoSuchRemote(_)
            | Self::TooLarge
            | Self::List(_)
            | Self::Refused(_)
            | Self::Unreadable(_)
            | Self::OtherRepository { .. }
            | Self::HubKey(_)
            | Self::RemoteChanged(_) => None,
        }
    }
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refs(error) => error.fmt(f),
            Self::Update(error) => error.fmt(f),
            Self::Remote(error) => error.fmt(f),
            Self::NoSuchRemote(name) => write!(f, "there is no remote `{name}`"),
            Self::Transport(error) => error.fmt(f),
            Self::Body(error) => write!(f, "the answer could not be read: {error}"),
            Self::TooLarge => f.write_str("the answer is longer than this side reads"),
            Self::List(error) => write!(f, "the list of references is refused: {error}"),
            Self::Refused(message) => write!(f, "the other side refused: {message}"),
            Self::Unreadable(error) => {
                write!(
                    f,
                    "the other side answered an error that cannot be read: {error}"
                )
            }
            Self::OtherRepository {
                remote,
                expected,
                found,
            } => write!(
                f,
                "the remote `{remote}` names the repository {expected}, but its address now \
                 serves the repository {found}; nothing was fetched"
            ),
            Self::Receive(error) => error.fmt(f),
            Self::Connectivity(error) => error.fmt(f),
            Self::JournalLeft { source, .. } => write!(
                f,
                "the fetch has happened, but its journal stayed: {source}"
            ),
            Self::HubKey(error) => write!(f, "{error}; nothing was fetched"),
            Self::RemoteChanged(name) => write!(
                f,
                "the remote `{name}` was removed or changed while the fetch received; no remote \
                 reference moved, and a fetch of the remote as it is now goes on"
            ),
        }
    }
}

impl Error for FetchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        // The message includes the message of the cause, so the source is the cause of the cause
        // (see the crate documentation).
        match self {
            Self::Refs(error) => error.source(),
            Self::Update(error) => error.source(),
            Self::Remote(error) => error.source(),
            Self::Transport(error) => error.source(),
            Self::Body(error) => error.source(),
            Self::List(error) | Self::Unreadable(error) => error.source(),
            Self::Receive(error) => error.source(),
            Self::Connectivity(error) => error.source(),
            Self::JournalLeft { source, .. } => source.source(),
            Self::NoSuchRemote(_)
            | Self::TooLarge
            | Self::Refused(_)
            | Self::OtherRepository { .. }
            | Self::HubKey(_)
            | Self::RemoteChanged(_) => None,
        }
    }
}

/// Why [`Repository::clone_remote`] failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum CloneError {
    /// The authorizer refused `fetch`, or the list of references could not be read. Nothing was
    /// made.
    Before(FetchError),
    /// The repository could not be made.
    Init(InitError),
    /// The repository was made at `path`, and a later step failed. It stays with what it holds.
    After {
        /// The directory of the new repository.
        path: PathBuf,
        /// Whether the configuration of the remote was written, so that the failure is one of the
        /// fetch and a fetch there goes on.
        configured: bool,
        /// What failed, boxed, as the unboxed error is too large for clippy's
        /// `result_large_err` lint.
        source: Box<FetchError>,
    },
}

impl CloneError {
    /// The entry `fetch` of the clone, if it was written.
    pub fn entry(&self) -> Option<&Entry> {
        match self {
            Self::After { source, .. } => source.entry(),
            Self::Before(_) | Self::Init(_) => None,
        }
    }

    /// The error of the file system, if that is what failed (see the crate documentation).
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Before(error) => error.io_error(),
            Self::After { source, .. } => source.io_error(),
            Self::Init(error) => error.io_error(),
        }
    }
}

impl fmt::Display for CloneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Before(error) => error.fmt(f),
            Self::Init(error) => error.fmt(f),
            Self::After {
                path,
                configured: false,
                source,
            } => write!(
                f,
                "the repository in `{}` was made, but its remote could not be configured: \
                 {source}; remove the directory and clone again",
                path.display()
            ),
            Self::After { path, source, .. }
                if matches!(**source, FetchError::JournalLeft { .. }) =>
            {
                write!(f, "the clone in `{}` is made, but {source}", path.display())
            }
            Self::After { path, source, .. } => write!(
                f,
                "the clone in `{}` was made, but its fetch did not finish: {source}; a fetch \
                 there goes on",
                path.display()
            ),
        }
    }
}

impl Error for CloneError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Before(error) => error.source(),
            Self::After { source, .. } => source.source(),
            Self::Init(error) => error.source(),
        }
    }
}

#[cfg(test)]
mod tests;
