//! The commands of the network: `remote`, `clone`, and `fetch` (ADR-022, ADR-023, ADR-026,
//! STEMMA-305).
//!
//! Each command wires the HTTP client of `stemma-remote` to one operation of the core. The core
//! reads the list of references, pins and checks the key of the hub, writes the entries, and
//! moves the remote references; this module reads the arguments, makes the transport from the
//! configuration of the remote, signs in where the remote asks for it, and prints the result.
//!
//! Exit codes follow the rest of the program (chapter 12, ADR-023): 1 for a refusal of the hub or
//! of the core and for a hub that gives no answer of the protocol, 3 if an entry is written or a
//! clone is made and the rest could not finish, 5 for damage, and 6 for a lock that is held and
//! for a hub that is busy (503) or limits the requests (429): a later call can succeed. A key of
//! the hub that changed, a redirect, and a repository that the hub does not serve are reported as
//! such.
//!
//! Which local branch follows which remote reference (ADR-008): `clone` makes the branch `main`
//! at `remotes/<remote>/heads/main` and checks it out, if the hub has `main`. `fetch` moves no
//! local branch; `merge` or `rebase` with the remote reference brings a branch along.

use std::ffi::OsString;
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Subcommand};

use stemma_core::failure::{Classify, FailureKind};
use stemma_core::oplog::{Entry, OpLogError};
use stemma_core::protocol::record::HubAddress;
use stemma_core::refs::{BranchName, RefKind};
use stemma_core::remote::{CaFile, HubKeyError, RemoteAddress, RemoteConfig, RemoteName};
use stemma_core::remote::{RemoteRef, RemoteRefName};
use stemma_core::repository::{
    read_ref_list, Changes, CloneError, FetchError, Fetched, InitOptions, PinError,
    RemoteConfigError, RemoteSetup, Repository, Revision, SwitchTarget,
};
use stemma_core::signature::{KeyFingerprint, PublicKey, SshKeygen};
use stemma_core::transport::{sign_in, SignInError, TransportError};
use stemma_core::AllowAll;
use stemma_remote::{HttpTransport, Status, Timeouts};

use super::{actor, open, short_id, CliError, ACTOR_ENV};
use crate::op_log::{quote, JSON_SCHEMA};

/// The name of the remote that `clone` and `fetch` take when none is given.
const ORIGIN: &str = "origin";

/// Why a command of the network fails: the message, and the exit code that it ends with.
#[derive(Debug)]
pub(super) struct NetworkError {
    message: String,
    code: u8,
}

impl NetworkError {
    pub(super) fn refused(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: 1,
        }
    }

    pub(super) fn of_kind(message: impl Into<String>, kind: FailureKind) -> Self {
        let code = match kind {
            FailureKind::Damaged => 5,
            FailureKind::Locked => 6,
            FailureKind::Other => 1,
        };
        Self {
            message: message.into(),
            code,
        }
    }

    pub(super) fn incomplete(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: 3,
        }
    }

    pub(super) fn exit_code(&self) -> ExitCode {
        ExitCode::from(self.code)
    }

    /// The same failure with another message and the same exit code, for a command that
    /// reports it as part of another one (STEMMA-312).
    pub(super) fn reworded(self, message: impl FnOnce(&str) -> String) -> Self {
        Self {
            message: message(&self.message),
            code: self.code,
        }
    }
}

impl fmt::Display for NetworkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<NetworkError> for CliError {
    fn from(error: NetworkError) -> Self {
        Self::Network(error)
    }
}

// The arguments.

/// The directory of the repository and the actor.
#[derive(Debug, Args)]
pub(super) struct Common {
    /// A directory of the repository: the one that contains `.stemma`, or one below it
    #[arg(
        short = 'C',
        long = "repository",
        value_name = "DIR",
        default_value = "."
    )]
    repository: PathBuf,

    /// Who runs the command: `human <id>` or
    /// `agent <id> model=<model> session=<session> sponsor=<sponsor>`; required unless the
    /// environment sets STEMMA_ACTOR (`remote list` ignores it)
    #[arg(long, env = ACTOR_ENV, value_name = "ACTOR")]
    actor: Option<OsString>,
}

/// The key of a sign-in.
#[derive(Debug, Args)]
pub(super) struct SignInKey {
    /// The key file of the actor for the sign-in of a remote that asks for one, for `ssh-keygen
    /// -Y sign`: the private key, whose public key is KEY_FILE.pub, or the public key if
    /// `ssh-agent` holds the private one
    #[arg(long, value_name = "KEY_FILE")]
    key: Option<PathBuf>,
}

/// What the first contact with a hub takes besides the address (ADR-026, ADR-028).
#[derive(Debug, Args)]
pub(super) struct FirstContact {
    /// The fingerprint that the key of the hub must have, as `ssh-keygen -l` shows it
    /// (`SHA256:...`), from a second channel; without it the first contact is taken on trust and
    /// the fingerprint is shown
    #[arg(long, value_name = "FINGERPRINT", value_parser = parse_fingerprint)]
    expect_key: Option<KeyFingerprint>,

    /// A file of PEM certificates whose roots replace the compiled-in roots for this remote; an
    /// absolute path
    #[arg(long, value_name = "FILE", value_parser = parse_ca_file)]
    ca_file: Option<CaFile>,

    /// The hub asks for a sign-in: every command signs in with --key first
    #[arg(long)]
    sign_in: bool,
}

impl FirstContact {
    fn setup(&self, address: RemoteAddress) -> RemoteSetup {
        let mut setup = RemoteSetup::new(address);
        setup.ca_file = self.ca_file.clone();
        setup.sign_in = self.sign_in;
        setup.expected = self.expect_key;
        setup
    }
}

#[derive(Debug, Subcommand)]
pub(super) enum RemoteCommand {
    /// Add a remote: read the list of references of the hub, pin the key of the hub, and write
    /// the configuration; moves no remote reference
    Add(RemoteAddArgs),
    /// List the remotes with their addresses and the fingerprints of their pinned keys
    List(RemoteListArgs),
    /// Remove a remote: its configuration and its remote references
    Remove(RemoteRemoveArgs),
    /// Pin a new key of the hub of a remote, after comparing its fingerprint on a second channel
    Pin(RemotePinArgs),
}

#[derive(Debug, Args)]
pub(super) struct RemoteAddArgs {
    #[command(flatten)]
    common: Common,

    #[command(flatten)]
    contact: FirstContact,

    #[command(flatten)]
    key: SignInKey,

    /// Write the result as JSON (ADR-018)
    #[arg(long)]
    json: bool,

    /// The name of the remote, one component by the rules of a branch name
    #[arg(value_name = "NAME", value_parser = parse_remote_name)]
    name: RemoteName,

    /// The address of the repository on the hub: `https://<hub>/<namespace>/<repository>`
    #[arg(value_name = "ADDRESS", value_parser = parse_address)]
    address: RemoteAddress,
}

#[derive(Debug, Args)]
pub(super) struct RemoteListArgs {
    /// A directory of the repository: the one that contains `.stemma`, or one below it
    #[arg(
        short = 'C',
        long = "repository",
        value_name = "DIR",
        default_value = "."
    )]
    repository: PathBuf,

    /// Write one JSON object per remote (ADR-018)
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub(super) struct RemoteRemoveArgs {
    #[command(flatten)]
    common: Common,

    /// Write the result as JSON (ADR-018)
    #[arg(long)]
    json: bool,

    /// The name of the remote
    #[arg(value_name = "NAME", value_parser = parse_remote_name)]
    name: RemoteName,
}

#[derive(Debug, Args)]
pub(super) struct RemotePinArgs {
    #[command(flatten)]
    common: Common,

    #[command(flatten)]
    key: SignInKey,

    /// The fingerprint of the new key of the hub, from a second channel (`SHA256:...`)
    #[arg(long, value_name = "FINGERPRINT", value_parser = parse_fingerprint)]
    expect_key: KeyFingerprint,

    /// Write the result as JSON (ADR-018)
    #[arg(long)]
    json: bool,

    /// The name of the remote
    #[arg(value_name = "NAME", value_parser = parse_remote_name)]
    name: RemoteName,
}

#[derive(Debug, Args)]
pub(super) struct CloneArgs {
    /// Who runs the command: `human <id>` or
    /// `agent <id> model=<model> session=<session> sponsor=<sponsor>`; required unless the
    /// environment sets STEMMA_ACTOR
    #[arg(long, env = ACTOR_ENV, value_name = "ACTOR")]
    actor: Option<OsString>,

    #[command(flatten)]
    contact: FirstContact,

    #[command(flatten)]
    key: SignInKey,

    /// The name of the remote in the new repository
    #[arg(long, value_name = "NAME", default_value = ORIGIN, value_parser = parse_remote_name)]
    remote: RemoteName,

    /// Write the result as JSON (ADR-018)
    #[arg(long)]
    json: bool,

    /// The address of the repository on the hub: `https://<hub>/<namespace>/<repository>`
    #[arg(value_name = "ADDRESS", value_parser = parse_address)]
    address: RemoteAddress,

    /// The directory of the new repository; by default the last component of the address, in
    /// the current directory
    #[arg(value_name = "DIRECTORY")]
    directory: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub(super) struct FetchArgs {
    #[command(flatten)]
    common: Common,

    #[command(flatten)]
    key: SignInKey,

    /// Write the result as JSON (ADR-018)
    #[arg(long)]
    json: bool,

    /// The name of the remote
    #[arg(value_name = "REMOTE", default_value = ORIGIN, value_parser = parse_remote_name)]
    remote: RemoteName,
}

pub(super) fn parse_remote_name(text: &str) -> Result<RemoteName, String> {
    RemoteName::try_new(text).map_err(|error| error.to_string())
}

fn parse_fingerprint(text: &str) -> Result<KeyFingerprint, String> {
    text.parse().map_err(|_| {
        "expected `SHA256:` and the Base64 text of the digest, as `ssh-keygen -l` shows it"
            .to_owned()
    })
}

fn parse_ca_file(text: &str) -> Result<CaFile, String> {
    CaFile::try_new(text).map_err(|error| error.to_string())
}

/// An address of a repository on a hub: a valid address of a remote with the hub and at least
/// two components after it, the namespace and the repository (ADR-027).
fn parse_address(text: &str) -> Result<RemoteAddress, String> {
    let address = RemoteAddress::try_new(text).map_err(|error| error.to_string())?;
    // A namespace or repository `.` or `..` names no repository, and the default directory of a
    // clone would be the current one or its parent.
    let dots = text
        .rsplit('/')
        .take(2)
        .any(|part| matches!(part, "." | ".."));
    if hub_of(&address).is_none() || dots {
        return Err(format!(
            "the address `{}` names no namespace and repository: expected \
             `https://<hub>/<namespace>/<repository>`",
            text.escape_debug()
        ));
    }
    Ok(address)
}

/// The address of the hub that `address` lies on: the address without its last two components,
/// the namespace and the repository (ADR-026, ADR-027). A sign-in names it.
fn hub_of(address: &RemoteAddress) -> Option<String> {
    let text = address.as_str();
    let rest = ["https://", "http://"]
        .iter()
        .find_map(|scheme| text.strip_prefix(scheme))?;
    let mut parts = rest.rsplitn(3, '/');
    let (_repository, _namespace, host) = (parts.next()?, parts.next()?, parts.next()?);
    if host.is_empty() {
        return None;
    }
    Some(text[..text.len() - rest.len() + host.len()].to_owned())
}

// The connection.

/// A transport to the repository of `config`, signed in if the configuration asks for it.
pub(super) fn connect(
    config: &RemoteConfig,
    key: Option<&Path>,
) -> Result<HttpTransport, NetworkError> {
    connect_noting(config, key, "; nothing was written").map_err(|unconnected| unconnected.error)
}

/// Why [`connect_noting`] did not connect: the error, and whether the hub could not be reached
/// for the sign-in, or its answer broke off, so that a later call can succeed.
pub(super) struct Unconnected {
    pub(super) error: NetworkError,
    pub(super) unreachable: bool,
}

impl From<NetworkError> for Unconnected {
    fn from(error: NetworkError) -> Self {
        Self {
            error,
            unreachable: false,
        }
    }
}

/// [`connect`] with `note` after the message of a failure that the remote or the hub causes:
/// what the failure leaves behind. A command that wrote an entry before it connects says so
/// itself (STEMMA-312).
pub(super) fn connect_noting(
    config: &RemoteConfig,
    key: Option<&Path>,
    note: &str,
) -> Result<HttpTransport, Unconnected> {
    let mut transport = HttpTransport::new(config, Timeouts::DEFAULT)
        .map_err(|error| NetworkError::refused(format!("{error}{note}")))?;
    if config.sign_in {
        let key = key.ok_or_else(|| {
            NetworkError::refused(format!(
                "the remote asks for a sign-in: pass `--key KEY_FILE`{note}"
            ))
        })?;
        let public = read_public_key(key)?;
        let hub = hub_of(&config.address).ok_or_else(|| {
            NetworkError::refused(format!(
                "the address `{}` of the remote names no namespace and repository, so the hub \
                 to sign in to is not known{note}",
                config.address
            ))
        })?;
        let called = HubAddress::try_new(hub.clone()).map_err(|error| {
            NetworkError::refused(format!("the address of the hub `{hub}`: {error}"))
        })?;
        if let Err(error) = sign_in(&mut transport, &called, &public, &SshKeygen::new(key)) {
            let unreachable = matches!(
                error,
                SignInError::Transport(TransportError::Carrier(_)) | SignInError::Body(_)
            );
            return Err(Unconnected {
                error: with_status(
                    format!("the sign-in failed: {error}{note}"),
                    FailureKind::Other,
                    &transport,
                ),
                unreachable,
            });
        }
    }
    Ok(transport)
}

/// The public key of a key file: the file itself if it holds a public key, and otherwise the
/// file with `.pub` appended.
fn read_public_key(key: &Path) -> Result<PublicKey, NetworkError> {
    let read = |path: &Path| {
        std::fs::read_to_string(path).map_err(|error| {
            NetworkError::refused(format!(
                "cannot read the key file {}: {error}",
                path.display()
            ))
        })
    };
    let text = read(key)?;
    let first = match PublicKey::from_openssh(text.trim()) {
        Ok(public) => return Ok(public),
        Err(error) => error,
    };
    // The file holds no public key, so it is the private key, and its public key lies beside it.
    let mut public = key.as_os_str().to_owned();
    public.push(".pub");
    let public = PathBuf::from(public);
    let text = std::fs::read_to_string(&public).map_err(|error| {
        NetworkError::refused(format!(
            "the key file {} holds no public key ({first}), and {} cannot be read: {error}",
            key.display(),
            public.display()
        ))
    })?;
    PublicKey::from_openssh(text.trim()).map_err(|error| {
        NetworkError::refused(format!(
            "the key file {} holds no public key ({first}), and {} is none either: {error}",
            key.display(),
            public.display()
        ))
    })
}

/// The error of a command that spoke to a hub, with what the status of the last answer adds: a
/// busy hub and a limit on requests are code 6, a redirect is named with its new address, and a
/// repository that the hub does not serve and a missing sign-in are said as such (ADR-023).
pub(super) fn with_status(
    message: String,
    kind: FailureKind,
    transport: &HttpTransport,
) -> NetworkError {
    // The message of the transport names the status, the wait, and the new address already.
    match transport.last_status() {
        Some(Status::Busy(_)) | Some(Status::RateLimited(_)) if kind == FailureKind::Other => {
            NetworkError::of_kind(
                format!("{message}; a later call can succeed"),
                FailureKind::Locked,
            )
        }
        Some(Status::Redirect(..)) => {
            NetworkError::refused(format!("{message}; check the address of the remote"))
        }
        Some(Status::NotFound) => NetworkError::refused(format!(
            "{message}; the repository does not exist, may not be read with this sign-in, or was \
             renamed"
        )),
        Some(Status::SignInNeeded) => NetworkError::refused(format!(
            "{message}; the hub asks for a sign-in: add the remote with `--sign-in` and pass \
             `--key KEY_FILE`"
        )),
        _ => NetworkError::of_kind(message, kind),
    }
}

/// The command that pins the new key of a hub that changed its key.
pub(super) fn key_hint(error: &HubKeyError, remote: &RemoteName) -> String {
    match error {
        HubKeyError::Changed { offered, .. } => {
            format!("; then run `stemma remote pin {remote} --expect-key {offered}`")
        }
        HubKeyError::NotPinned => format!(
            "; compare the fingerprint of the key of the hub on a second channel and run \
             `stemma remote pin {remote} --expect-key FINGERPRINT`"
        ),
        _ => String::new(),
    }
}

/// `1 remote reference`, `2 remote references`.
fn count(number: usize, noun: &str) -> String {
    let plural = if number == 1 { "" } else { "s" };
    format!("{number} {noun}{plural}")
}

fn not_flushed(error: &OpLogError) -> bool {
    matches!(error, OpLogError::NotFlushed { .. })
}

/// Writes the result of a command. A closed pipe is no failure, also after an entry: `op log`
/// names the entry of every one of these commands.
fn write_out(out: &mut dyn Write, text: &str, made: Option<u64>) -> Result<(), CliError> {
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(match made {
            Some(entry) => NetworkError::incomplete(format!(
                "entry {entry} is written, but the result could not be written: {error}"
            )),
            None => NetworkError::refused(format!("cannot write the result: {error}")),
        }
        .into()),
        Ok(()) => Ok(()),
    }
}

fn json_or_null(value: Option<String>) -> String {
    value.map_or_else(|| "null".to_owned(), |text| quote(&text))
}

fn fingerprint_of(config: &RemoteConfig) -> Option<KeyFingerprint> {
    config.hub_key.as_ref().map(PublicKey::fingerprint)
}

// `remote`.

pub(super) fn remote(command: RemoteCommand, out: &mut dyn Write) -> Result<(), CliError> {
    match command {
        RemoteCommand::Add(args) => remote_add(args, out),
        RemoteCommand::List(args) => remote_list(args, out),
        RemoteCommand::Remove(args) => remote_remove(args, out),
        RemoteCommand::Pin(args) => remote_pin(args, out),
    }
}

fn remote_config_error(error: RemoteConfigError, remote: &RemoteName) -> NetworkError {
    match &error {
        RemoteConfigError::Written { .. } => NetworkError::incomplete(format!(
            "{error}; look at `remote list`, and run the command again if it does not show the \
             change, which writes a second entry"
        )),
        RemoteConfigError::Log(inner) if not_flushed(inner) => NetworkError::incomplete(format!(
            "{error}; look at `op log`, and run the command again if the remote did not change"
        )),
        RemoteConfigError::Update(inner) if inner.entry().is_some() => {
            NetworkError::incomplete(format!("{error}; run `remote remove {remote}` again"))
        }
        RemoteConfigError::HubKey(inner) => {
            let hint = key_hint(inner, remote);
            NetworkError::refused(format!("{error}{hint}"))
        }
        _ => NetworkError::of_kind(error.to_string(), error.kind()),
    }
}

/// Adds a remote (ADR-026): the first contact reads the list of references, pins the key of the
/// hub against the expected fingerprint, and writes the entry `remote-add` and the
/// configuration. No remote reference moves; `fetch` brings them.
fn remote_add(args: RemoteAddArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let who = actor(args.common.actor)?;
    let repository = open(&args.common.repository)?;
    // A name that is taken is refused before the hub is contacted or signed in to; the core
    // checks it again under the lock.
    let taken = repository
        .remote(&args.name)
        .map_err(|error| NetworkError::of_kind(error.to_string(), error.kind()))?;
    if taken.is_some() {
        let error = RemoteConfigError::Exists(args.name.clone());
        return Err(NetworkError::refused(format!("{error}; nothing was written")).into());
    }
    let setup = args.contact.setup(args.address);
    let mut transport = connect(&setup.provisional(), args.key.key.as_deref())?;
    let added = match repository.add_remote(&args.name, &setup, &mut transport, &who, &AllowAll) {
        Ok(added) => added,
        Err(RemoteConfigError::List(error)) => {
            let kind = error.kind();
            return Err(
                with_status(format!("{error}; nothing was written"), kind, &transport).into(),
            );
        }
        Err(error) => return Err(remote_config_error(error, &args.name).into()),
    };
    let text = if args.json {
        format!(
            "{{\"schema\":{JSON_SCHEMA},\"remote\":{},\"address\":{},\"repository\":{},\
             \"fingerprint\":{},\"expected\":{},\"entry\":{}}}\n",
            quote(args.name.as_str()),
            quote(added.config.address.as_str()),
            quote(&added.config.repository.to_string()),
            quote(&added.fingerprint.to_string()),
            setup.expected.is_some(),
            added.entry.number()
        )
    } else {
        format!(
            "Added the remote `{}` at {}.\n{}",
            args.name,
            added.config.address,
            fingerprint_text(&added.fingerprint, setup.expected.is_some())
        )
    };
    write_out(out, &text, Some(added.entry.number()))
}

/// The line that names the pinned key: confirmed by the expected fingerprint, or to be compared.
fn fingerprint_text(fingerprint: &KeyFingerprint, expected: bool) -> String {
    if expected {
        format!("The key of the hub has the expected fingerprint {fingerprint} and is pinned.\n")
    } else {
        format!(
            "Pinned the key of the hub with the fingerprint {fingerprint}; compare it on a second \
             channel.\n"
        )
    }
}

/// Lists the remotes, sorted by name: the name, the address, and the fingerprint of the pinned
/// key (or `-`), and the CA file and the sign-in where they are set.
fn remote_list(args: RemoteListArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let repository = open(&args.repository)?;
    let remotes = repository
        .remotes()
        .map_err(|error| NetworkError::of_kind(error.to_string(), error.kind()))?;
    for (name, config) in remotes {
        let fingerprint = fingerprint_of(&config).map(|key| key.to_string());
        let text = if args.json {
            format!(
                "{{\"schema\":{JSON_SCHEMA},\"remote\":{},\"address\":{},\"repository\":{},\
                 \"fingerprint\":{},\"ca_file\":{},\"sign_in\":{}}}\n",
                quote(name.as_str()),
                quote(config.address.as_str()),
                quote(&config.repository.to_string()),
                json_or_null(fingerprint),
                json_or_null(config.ca_file.as_ref().map(ToString::to_string)),
                config.sign_in
            )
        } else {
            let mut line = format!(
                "{name} {} {}",
                config.address,
                fingerprint.as_deref().unwrap_or("-")
            );
            if let Some(file) = &config.ca_file {
                line.push_str(&format!(" ca-file={file}"));
            }
            if config.sign_in {
                line.push_str(" sign-in");
            }
            line.push('\n');
            line
        };
        match out.write_all(text.as_bytes()) {
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => break,
            Err(error) => {
                return Err(NetworkError::refused(format!("cannot write the list: {error}")).into())
            }
            Ok(()) => {}
        }
    }
    Ok(())
}

/// Removes a remote: the entry `remote-remove` names the configuration and the remote references,
/// which are deleted after it. The hub is not asked.
fn remote_remove(args: RemoteRemoveArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let who = actor(args.common.actor)?;
    let repository = open(&args.common.repository)?;
    let removed = repository
        .remove_remote(&args.name, &who, &AllowAll)
        .map_err(|error| remote_config_error(error, &args.name))?;
    let text = if args.json {
        format!(
            "{{\"schema\":{JSON_SCHEMA},\"remote\":{},\"references\":{},\"entry\":{}}}\n",
            quote(args.name.as_str()),
            removed.refs.len(),
            removed.entry.number()
        )
    } else {
        format!(
            "Removed the remote `{}` and its {}.\n",
            args.name,
            count(removed.refs.len(), "remote reference")
        )
    };
    write_out(out, &text, Some(removed.entry.number()))
}

fn pin_error(error: PinError) -> NetworkError {
    let kind = match &error {
        PinError::Refs(inner) => inner.kind(),
        PinError::Remote(inner) => inner.kind(),
        PinError::Log(inner) => inner.kind(),
        _ => FailureKind::Other,
    };
    match &error {
        PinError::Log(inner) if not_flushed(inner) => NetworkError::incomplete(format!(
            "{error}; the entry is in the log: pin the key again"
        )),
        PinError::Io { .. } => NetworkError::incomplete(format!(
            "{error}; the entry is in the log and the old key may still be pinned: pin the key \
             again"
        )),
        _ => NetworkError::of_kind(error.to_string(), kind),
    }
}

/// Pins the key that the hub names now, after a human compared its fingerprint on a second
/// channel (ADR-026): the entry `remote-pin` with the old and the new key. An agent is refused.
fn remote_pin(args: RemotePinArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let who = actor(args.common.actor)?;
    let repository = open(&args.common.repository)?;
    let config = repository
        .remote(&args.name)
        .map_err(|error| NetworkError::of_kind(error.to_string(), error.kind()))?
        .ok_or_else(|| NetworkError::refused(format!("there is no remote `{}`", args.name)))?;
    let mut transport = connect(&config, args.key.key.as_deref())?;
    let list = read_ref_list(&mut transport).map_err(|error| {
        let kind = error.kind();
        with_status(format!("{error}; nothing was pinned"), kind, &transport)
    })?;
    let pinned = repository
        .pin_hub_key_again(&args.name, &list, &args.expect_key, &who, &AllowAll)
        .map_err(pin_error)?;
    let number = pinned.as_ref().map(Entry::number);
    let text = if args.json {
        format!(
            "{{\"schema\":{JSON_SCHEMA},\"remote\":{},\"fingerprint\":{},\"entry\":{}}}\n",
            quote(args.name.as_str()),
            quote(&args.expect_key.to_string()),
            number.map_or_else(|| "null".to_owned(), |number| number.to_string())
        )
    } else if pinned.is_some() {
        format!(
            "Pinned the key {} for the hub of the remote `{}`.\n",
            args.expect_key, args.name
        )
    } else {
        format!(
            "The key {} is pinned for the remote `{}` already.\n",
            args.expect_key, args.name
        )
    };
    write_out(out, &text, number)
}

// `fetch`.

fn fetch_error(error: FetchError, remote: &RemoteName, transport: &HttpTransport) -> NetworkError {
    let kind = error.kind();
    match &error {
        FetchError::JournalLeft { .. } => {
            NetworkError::incomplete(format!("{error}; the next fetch reads it"))
        }
        FetchError::Update(inner) if inner.entry().is_some() => {
            NetworkError::incomplete(format!("{error}; fetch again"))
        }
        FetchError::HubKey(inner) => {
            let hint = key_hint(inner, remote);
            NetworkError::refused(format!("{error}{hint}"))
        }
        _ => with_status(error.to_string(), kind, transport),
    }
}

/// The remote references that a fetch moved, created, or deleted, from its entry.
fn moved_refs(fetched: &Fetched) -> Vec<(RemoteRefName, Option<RemoteRef>, Option<RemoteRef>)> {
    fetched
        .entry
        .as_ref()
        .and_then(Entry::change)
        .map(|change| {
            change
                .remotes()
                .iter()
                .map(|(name, moved)| {
                    (
                        name.clone(),
                        moved.before().copied(),
                        moved.after().copied(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn fetched_text(remote: &RemoteName, fetched: &Fetched) -> String {
    let moved = moved_refs(fetched);
    if moved.is_empty() {
        return format!("Nothing new from `{remote}`.\n");
    }
    let mut text = format!(
        "Fetched from `{remote}`: {}, {} received.\n",
        count(moved.len(), "remote reference"),
        count(fetched.received.commits as usize, "commit")
    );
    for (name, old, new) in moved {
        let side = |value: Option<RemoteRef>| value.map(|value| short_id(value.commit));
        let line = match (side(old), side(new)) {
            (None, Some(new)) => format!("  {name}  new at {new}\n"),
            (Some(old), None) => format!("  {name}  deleted (was {old})\n"),
            (Some(old), Some(new)) if old == new => format!("  {name}  {new}, new record entry\n"),
            (Some(old), Some(new)) => format!("  {name}  {old} -> {new}\n"),
            (None, None) => continue,
        };
        text.push_str(&line);
    }
    text
}

fn fetched_json(remote: &RemoteName, fetched: &Fetched) -> String {
    let references: Vec<String> = moved_refs(fetched)
        .into_iter()
        .map(|(name, old, new)| {
            let side = |value: Option<RemoteRef>| json_or_null(value.map(|v| v.commit.to_string()));
            format!(
                "{{\"name\":{},\"old\":{},\"new\":{}}}",
                quote(&name.to_string()),
                side(old),
                side(new)
            )
        })
        .collect();
    format!(
        "{{\"schema\":{JSON_SCHEMA},\"remote\":{},\"entry\":{},\"commits\":{},\"objects\":{},\
         \"references\":[{}]}}\n",
        quote(remote.as_str()),
        fetched
            .entry
            .as_ref()
            .map_or_else(|| "null".to_owned(), |entry| entry.number().to_string()),
        fetched.received.commits,
        fetched.received.objects,
        references.join(",")
    )
}

/// Fetches from a remote (ADR-008, ADR-023): moves its remote references to the state of the
/// hub, with one entry `fetch`. No local branch moves.
pub(super) fn fetch(args: FetchArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let who = actor(args.common.actor)?;
    let repository = open(&args.common.repository)?;
    let config = repository
        .remote(&args.remote)
        .map_err(|error| NetworkError::of_kind(error.to_string(), error.kind()))?
        .ok_or_else(|| {
            NetworkError::refused(format!(
                "there is no remote `{}`; add it with `remote add`",
                args.remote
            ))
        })?;
    let mut transport = connect(&config, args.key.key.as_deref())?;
    let fetched = repository
        .fetch(&args.remote, &mut transport, &who, &AllowAll)
        .map_err(|error| fetch_error(error, &args.remote, &transport))?;
    let text = if args.json {
        fetched_json(&args.remote, &fetched)
    } else {
        fetched_text(&args.remote, &fetched)
    };
    write_out(out, &text, fetched.entry.as_ref().map(Entry::number))
}

// `clone`.

/// The directory that `clone` makes when none is given: the last component of the address,
/// which [`parse_address`] made sure is neither `.` nor `..`.
fn default_directory(address: &RemoteAddress) -> PathBuf {
    let last = address
        .as_str()
        .rsplit('/')
        .next()
        .expect("split yields one part at least");
    PathBuf::from(last)
}

/// Refuses a target of `clone` that exists and is not an empty directory: the clone would mix
/// with what is there, and the switch to `main` would take it into a snapshot.
fn refuse_used_directory(directory: &Path) -> Result<(), NetworkError> {
    let used = match std::fs::read_dir(directory) {
        Ok(mut entries) => entries.next().is_some(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) if error.kind() == io::ErrorKind::NotADirectory => true,
        Err(error) => {
            return Err(NetworkError::refused(format!(
                "cannot read the directory `{}`: {error}; nothing was made",
                directory.display()
            )))
        }
    };
    if used {
        return Err(NetworkError::refused(format!(
            "`{}` exists and is not an empty directory; nothing was made",
            directory.display()
        )));
    }
    Ok(())
}

fn clone_error(error: CloneError, transport: &HttpTransport) -> NetworkError {
    let kind = error.kind();
    match &error {
        CloneError::After { .. } => NetworkError::incomplete(error.to_string()),
        CloneError::Before(FetchError::HubKey(_)) => {
            NetworkError::refused(format!("{error}; nothing was made"))
        }
        CloneError::Before(_) => with_status(format!("{error}; nothing was made"), kind, transport),
        _ => NetworkError::of_kind(error.to_string(), kind),
    }
}

/// Clones a repository of a hub (ADR-023, ADR-026): reads the list, pins the key of the hub,
/// makes the repository with the remote, fetches, and makes the branch `main` at
/// `remotes/<remote>/heads/main` with its files, if the hub has `main`.
pub(super) fn clone(args: CloneArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let who = actor(args.actor)?;
    let directory = args
        .directory
        .unwrap_or_else(|| default_directory(&args.address));
    refuse_used_directory(&directory)?;
    let setup = args.contact.setup(args.address);
    let mut transport = connect(&setup.provisional(), args.key.key.as_deref())?;
    let (repository, fetched) = Repository::clone_remote(
        &directory,
        &InitOptions::default(),
        &args.remote,
        &setup,
        &mut transport,
        &who,
        &AllowAll,
    )
    .map_err(|error| clone_error(error, &transport))?;
    let made = |what: String| {
        NetworkError::incomplete(format!(
            "the clone in `{}` is made, but {what}",
            directory.display()
        ))
    };
    let config = repository
        .remote(&args.remote)
        .map_err(|error| made(format!("its remote cannot be read: {error}")))?
        .ok_or_else(|| made(format!("its remote `{}` is gone", args.remote)))?;
    let fingerprint = fingerprint_of(&config)
        .ok_or_else(|| made(format!("its remote `{}` holds no key", args.remote)))?;
    let main = BranchName::try_new("main").expect("`main` is a branch name");
    let upstream = RemoteRefName {
        remote: args.remote.clone(),
        kind: RefKind::Branch,
        name: main.clone(),
    };
    let has_main = repository
        .refs()
        .remote_ref(&upstream)
        .map_err(|error| {
            made(format!(
                "its remote reference `{upstream}` cannot be read: {error}; run `switch -c main \
                 {upstream}` there"
            ))
        })?
        .is_some();
    if has_main {
        let target = SwitchTarget::Create {
            name: main.clone(),
            start: Some(Revision::Remote(upstream.clone())),
        };
        // The directory was empty, so the new repository has no change to carry; what the switch
        // did is what the output says.
        let _switched = repository
            .switch(&target, Changes::Carry, &who, &AllowAll)
            .map_err(|error| {
                NetworkError::incomplete(format!(
                    "the clone in `{}` is made, but the branch `main` could not be made at \
                     `{upstream}`: {error}; run `switch -c main {upstream}` there",
                    directory.display()
                ))
            })?;
    }
    let text = if args.json {
        format!(
            "{{\"schema\":{JSON_SCHEMA},\"directory\":{},\"remote\":{},\"address\":{},\
             \"repository\":{},\"fingerprint\":{},\"expected\":{},\"references\":{},\
             \"commits\":{},\"branch\":{}}}\n",
            quote(&directory.display().to_string()),
            quote(args.remote.as_str()),
            quote(config.address.as_str()),
            quote(&config.repository.to_string()),
            quote(&fingerprint.to_string()),
            setup.expected.is_some(),
            moved_refs(&fetched).len(),
            fetched.received.commits,
            if has_main {
                quote(main.as_str())
            } else {
                "null".to_owned()
            }
        )
    } else {
        let mut text = format!(
            "Cloned {} into `{}`: {}, {}.\n",
            config.address,
            directory.display(),
            count(moved_refs(&fetched).len(), "remote reference"),
            count(fetched.received.commits as usize, "commit")
        );
        text.push_str(&fingerprint_text(&fingerprint, setup.expected.is_some()));
        if has_main {
            text.push_str(&format!("The branch `main` starts at `{upstream}`.\n"));
        } else {
            text.push_str("The hub has no `main`, so no local branch is made.\n");
        }
        text
    };
    // The clone is made: a result that cannot be written is code 3. A closed pipe is none, as
    // the directory names the clone.
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(made(format!("the result could not be written: {error}")).into()),
        Ok(()) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hub_of_an_address_is_the_address_without_namespace_and_repository() {
        let hub = |text: &str| hub_of(&RemoteAddress::try_new(text).unwrap());
        assert_eq!(
            hub("https://hub.example/talent-factory/stemma").as_deref(),
            Some("https://hub.example")
        );
        assert_eq!(
            hub("http://127.0.0.1:8080/base/ns/repo").as_deref(),
            Some("http://127.0.0.1:8080/base")
        );
        assert_eq!(hub("https://hub.example/stemma"), None);
        assert_eq!(hub("https://hub.example"), None);
    }

    #[test]
    fn the_public_key_of_a_key_file_is_the_file_or_the_file_with_pub() {
        const KEY: &str =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIjwWEZ4603dDnTO8ilPzaYzYG5PnbKb6TxOzauTQ9du";
        let dir = std::env::temp_dir().join(format!("stemma-key-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let public = dir.join("id.pub");
        std::fs::write(&public, format!("{KEY} daniel\n")).unwrap();
        let private = dir.join("id");
        std::fs::write(&private, "-----BEGIN OPENSSH PRIVATE KEY-----\n").unwrap();
        let expected = PublicKey::from_openssh(KEY).unwrap();
        assert_eq!(read_public_key(&public).unwrap(), expected);
        assert_eq!(read_public_key(&private).unwrap(), expected);
        // A missing file is named with the error of the file system.
        let missing = read_public_key(&dir.join("none")).unwrap_err().to_string();
        assert!(missing.contains("cannot read the key file"), "{missing}");
        std::fs::remove_file(&public).unwrap();
        let error = read_public_key(&private).unwrap_err().to_string();
        assert!(error.contains("id.pub"), "{error}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_default_directory_is_the_last_component_of_the_address() {
        let address = RemoteAddress::try_new("https://hub.example/ns/stemma").unwrap();
        assert_eq!(default_directory(&address), PathBuf::from("stemma"));
    }

    #[test]
    fn an_address_whose_namespace_or_repository_is_a_dot_is_refused() {
        for text in [
            "https://hub.example/ns/.",
            "https://hub.example/ns/..",
            "https://hub.example/../stemma",
        ] {
            assert!(parse_address(text).is_err(), "{text}");
        }
        assert!(parse_address("https://hub.example/ns/.stemma").is_ok());
    }
}
