//! Remotes: the configuration of a remote and the remote references (ADR-008, ADR-009).
//!
//! A remote is a repository on a hub that a clone knows by a name. Two kinds of files belong to
//! it, both below the metadata directory that every worktree shares:
//!
//! ```text
//! <root>/remotes/<remote>                      the configuration of the remote
//! <root>/refs/remotes/<remote>/heads/<name>    a branch of the hub as the clone last saw it
//! <root>/refs/remotes/<remote>/tags/<name>     a tag of the hub as the clone last saw it
//! ```
//!
//! The configuration has six lines in a fixed order, and `-` stands for a value that is not set:
//!
//! ```text
//! stemma-remote 1
//! address https://hub.example/talent-factory/stemma
//! repository 0123456789abcdef0123456789abcdef
//! hub-key ssh-ed25519 AAAA...        or   hub-key -
//! ca-file /etc/stemma/own-ca.pem     or   ca-file -
//! sign-in yes                        or   sign-in no
//! ```
//!
//! A remote reference has two lines, the commit and the hash of the record entry of that name on
//! the hub (ADR-025):
//!
//! ```text
//! b3:<64 hex digits>
//! entry b3:<64 hex digits>
//! ```
//!
//! Both follow the rule of ADR-005: one value, one encoding, and a reader that rejects everything
//! else. This module holds the types, the two encodings, and the reader of the configuration; the
//! remote references are read through the sealed [`RefStore`](crate::refs::RefStore). The module
//! opens no connection, and a command that writes one of the files writes its entry in the
//! operation log first (ADR-011).

use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::fault;
use crate::object::{CommitId, ObjectId};
use crate::protocol::{RecordHash, RepositoryId};
use crate::refs::{
    BranchName, BranchNameError, BranchNameProblem, RefKind, RefMalformed, RefParseError,
};
use crate::signature::PublicKey;

/// The directory below the metadata directory that holds one configuration file per remote.
pub(crate) const REMOTES_DIR: &str = "remotes";

/// The first line of the configuration of a remote.
const CONFIG_HEADER: &str = "stemma-remote 1";

/// The value of a line of the configuration that is not set.
const UNSET: &str = "-";

/// The rule that a rejected remote name violates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RemoteNameProblem {
    /// The name violates a rule of a branch name.
    Name(BranchNameProblem),
    /// The name has more than one component: it contains `/`.
    Separator,
}

impl fmt::Display for RemoteNameProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // The words of a branch name would offer `/`, which a remote name cannot hold.
            Self::Name(BranchNameProblem::InvalidCharacter(c)) => write!(
                f,
                "the character `{}` is not allowed; use letters, digits, `.`, `_`, and `-`",
                c.escape_debug()
            ),
            Self::Name(problem) => problem.fmt(f),
            Self::Separator => f.write_str("the name of a remote is one component, without `/`"),
        }
    }
}

/// Why the name of a remote is rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RemoteNameError {
    /// The rejected name.
    pub name: String,
    /// The rule it violates.
    pub problem: RemoteNameProblem,
}

impl fmt::Display for RemoteNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid remote name `{}`: {}",
            self.name.escape_debug(),
            self.problem
        )
    }
}

impl Error for RemoteNameError {}

/// The name of a remote, such as `origin`: one component by the rules of a branch name
/// (ADR-008). It is a directory below `refs/remotes` and a file below `remotes`, so the rule is
/// that of a name that is a file name on every platform.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RemoteName(String);

impl RemoteName {
    /// Checks the name.
    ///
    /// # Errors
    ///
    /// Returns [`RemoteNameError`] with the first rule that the name violates.
    pub fn try_new(name: impl Into<String>) -> Result<Self, RemoteNameError> {
        let name = name.into();
        let problem = match BranchName::try_new(name.as_str()) {
            Err(error) => RemoteNameProblem::Name(error.problem),
            Ok(_) if name.contains('/') => RemoteNameProblem::Separator,
            Ok(_) => return Ok(Self(name)),
        };
        Err(RemoteNameError { name, problem })
    }

    /// Returns the name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RemoteName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for RemoteName {
    type Err = RemoteNameError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::try_new(name)
    }
}

/// The name of a remote reference: the remote, whether it is a branch or a tag there, and the
/// name. Its text is the qualified form `remotes/<remote>/heads/<name>` or
/// `remotes/<remote>/tags/<name>` (ADR-008).
///
/// The order is that of the remote, then the branches before the tags, then the name.
/// [`FromStr`] reads the qualified form back, and nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RemoteRefName {
    /// The remote.
    pub remote: RemoteName,
    /// A branch or a tag of the remote.
    pub kind: RefKind,
    /// The name of the branch or the tag on the hub.
    pub name: BranchName,
}

impl fmt::Display for RemoteRefName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{REMOTES_DIR}/{}/{}/{}",
            self.remote,
            self.kind.directory(),
            self.name
        )
    }
}

/// Why text is no qualified form of a remote reference.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RemoteRefNameError {
    /// The text is neither `remotes/<remote>/heads/<name>` nor `remotes/<remote>/tags/<name>`.
    /// Carries the text.
    Shape(String),
    /// The remote has no valid name.
    Remote(RemoteNameError),
    /// The name of the branch or the tag is no valid name.
    Name(BranchNameError),
}

impl fmt::Display for RemoteRefNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Shape(text) => write!(
                f,
                "`{}` names no remote reference: write `{REMOTES_DIR}/<remote>/heads/<name>` \
                 or `{REMOTES_DIR}/<remote>/tags/<name>`",
                text.escape_debug()
            ),
            Self::Remote(error) => error.fmt(f),
            Self::Name(error) => error.fmt(f),
        }
    }
}

impl Error for RemoteRefNameError {}

impl FromStr for RemoteRefName {
    type Err = RemoteRefNameError;

    /// Reads the qualified form that [`Display`](fmt::Display) writes.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let shape = || RemoteRefNameError::Shape(text.to_owned());
        let mut parts = text.splitn(4, '/');
        let (Some(REMOTES_DIR), Some(remote), Some(kind), Some(name)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(shape());
        };
        let kind = [RefKind::Branch, RefKind::Tag]
            .into_iter()
            .find(|found| found.directory() == kind)
            .ok_or_else(shape)?;
        Ok(Self {
            remote: remote.parse().map_err(RemoteRefNameError::Remote)?,
            kind,
            name: name.parse().map_err(RemoteRefNameError::Name)?,
        })
    }
}

/// What a remote reference holds: the state of a branch or a tag on a hub as the clone last saw
/// it (ADR-008).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteRef {
    /// The commit.
    pub commit: CommitId,
    /// The hash of the last record entry of this name on the hub, which a push names as
    /// `previous` (ADR-025).
    pub entry: RecordHash,
}

impl RemoteRef {
    /// The content of the file of a remote reference: the ID of the commit, a newline, `entry `,
    /// the hash of the record entry, and a newline.
    pub fn encode(&self) -> Vec<u8> {
        format!("{}\nentry {}\n", self.commit, self.entry).into_bytes()
    }

    /// Reads the file of a remote reference. It accepts what [`encode`](Self::encode) writes and
    /// nothing else, so `parse(bytes).encode() == bytes` for every accepted input.
    ///
    /// # Errors
    ///
    /// Returns [`RefParseError`] for input that is not UTF-8, has no newline at its end, does not
    /// have exactly two lines of which the second starts with `entry `, or names a commit or a hash
    /// that is not in canonical form.
    pub fn parse(bytes: &[u8]) -> Result<Self, RefParseError> {
        let text = std::str::from_utf8(bytes).map_err(|error| RefParseError::NotUtf8 {
            valid_up_to: error.valid_up_to(),
        })?;
        let lines = text
            .strip_suffix('\n')
            .ok_or(RefParseError::Malformed(RefMalformed::CutOff))?;
        let shape = RefParseError::Malformed(RefMalformed::RemoteShape);
        let (commit, entry) = lines.split_once('\n').ok_or(shape.clone())?;
        let entry = entry.strip_prefix("entry ").ok_or(shape.clone())?;
        if entry.contains('\n') {
            return Err(shape);
        }
        let commit = ObjectId::from_str(commit).map_err(RefParseError::InvalidObjectId)?;
        let entry = RecordHash::from_str(entry).map_err(RefParseError::InvalidRecordHash)?;
        Ok(Self {
            commit: CommitId::new(commit),
            entry,
        })
    }
}

/// A line of the configuration of a remote, as an error names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RemoteField {
    /// The line `address`.
    Address,
    /// The line `repository`.
    RepositoryId,
    /// The line `hub-key`.
    HubKey,
    /// The line `ca-file`.
    CaFile,
    /// The line `sign-in`.
    SignIn,
}

impl RemoteField {
    /// The rule of the value, for a message.
    fn rule(self) -> &'static str {
        match self {
            Self::Address => {
                "an address starts with `https://` or `http://` and goes on with printable ASCII \
                 characters without a space; it has no `@`, `?`, or `#` and does not end with `/`"
            }
            Self::RepositoryId => "a repository ID is 32 lowercase hexadecimal digits",
            Self::HubKey => {
                "the key of a hub is an Ed25519 public key in the OpenSSH format without a \
                 comment, or `-`"
            }
            Self::CaFile => {
                "the path of a CA file is absolute, has no control character, and neither starts \
                 nor ends with a space; `-` alone stands for no file"
            }
            Self::SignIn => "the value is `yes` or `no`",
        }
    }
}

impl fmt::Display for RemoteField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Address => "address",
            Self::RepositoryId => "repository",
            Self::HubKey => "hub-key",
            Self::CaFile => "ca-file",
            Self::SignIn => "sign-in",
        })
    }
}

/// A value of the configuration of a remote that is rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RemoteValueError {
    /// The line that the value belongs to.
    pub field: RemoteField,
    /// The rejected value.
    pub text: String,
}

impl fmt::Display for RemoteValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid value `{}` for `{}`: {}",
            self.text.escape_debug(),
            self.field,
            self.field.rule()
        )
    }
}

impl Error for RemoteValueError {}

fn rejected(field: RemoteField, text: &str) -> RemoteValueError {
    RemoteValueError {
        field,
        text: text.to_owned(),
    }
}

/// The address of a remote: the address of the repository on the hub,
/// `<hub>/<namespace>/<repository>` (ADR-023). It is kept as it was given and is not normalised.
/// Every endpoint of the repository is this address and a name (ADR-023), so it has no query, no
/// fragment, and no `/` at its end, and one repository has one spelling. It has no `@` either: a
/// password in the address would lie in the file and in every message that names the address.
/// That `http://` is accepted towards a loopback address alone is a rule of the client (ADR-028).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RemoteAddress(String);

impl RemoteAddress {
    /// Checks the address.
    ///
    /// # Errors
    ///
    /// Returns [`RemoteValueError`] unless the text starts with `https://` or `http://` and goes
    /// on with at least one printable ASCII character and no space, without `@`, `?`, or `#`
    /// and without `/` at its start or its end.
    pub fn try_new(address: impl Into<String>) -> Result<Self, RemoteValueError> {
        let address = address.into();
        let rest = ["https://", "http://"]
            .iter()
            .find_map(|scheme| address.strip_prefix(scheme));
        match rest {
            Some(rest)
                if !rest.is_empty()
                    && rest.chars().all(|c| c.is_ascii_graphic())
                    && !rest.contains(['@', '?', '#'])
                    && !rest.starts_with('/')
                    && !rest.ends_with('/') =>
            {
                Ok(Self(address))
            }
            _ => Err(rejected(RemoteField::Address, &address)),
        }
    }

    /// Returns the address.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RemoteAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The path of an additional CA file of a remote, for a hub with a certificate of an own CA
/// (ADR-028). It is text that fits a line; whether the file exists is the business of the client
/// that reads it. The path is absolute by the rule of the platform
/// ([`Path::is_absolute`]): the configuration is shared by every worktree, and a relative path
/// would name another file from another directory. The file belongs to one machine, so a path of
/// another platform is rejected like any other that is not absolute.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CaFile(String);

impl CaFile {
    /// Checks the path.
    ///
    /// # Errors
    ///
    /// Returns [`RemoteValueError`] for a path that is not absolute (so for an empty one and for
    /// `-`, which stands for no file), has a control character, or starts or ends with a space.
    pub fn try_new(path: impl Into<String>) -> Result<Self, RemoteValueError> {
        let path = path.into();
        if !Path::new(&path).is_absolute()
            || path.chars().any(char::is_control)
            || path.starts_with(' ')
            || path.ends_with(' ')
        {
            Err(rejected(RemoteField::CaFile, &path))
        } else {
            Ok(Self(path))
        }
    }

    /// Returns the path.
    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }
}

impl fmt::Display for CaFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The configuration of a remote (ADR-009): what a clone knows about a repository on a hub. The
/// name of the remote is the name of the file and no part of its content.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RemoteConfig {
    /// The address of the repository on the hub (ADR-023).
    pub address: RemoteAddress,
    /// The ID that the hub assigned to the repository (ADR-025, ADR-027).
    pub repository: RepositoryId,
    /// The public key of the hub that the clone pinned, or `None` before it pinned one (ADR-026).
    /// Whether a command ever leaves a remote without a key is the decision of the slice that
    /// writes the configuration; a `fetch` or a `push` must not trust a hub while it is `None`.
    pub hub_key: Option<PublicKey>,
    /// An additional CA file for the connection (ADR-028).
    pub ca_file: Option<CaFile>,
    /// Whether the hub asks for a sign-in, which the client learns once (ADR-023).
    pub sign_in: bool,
}

impl RemoteConfig {
    /// The configuration of a remote of which only the address and the repository are known: no
    /// pinned key, no CA file, and no sign-in.
    pub fn new(address: RemoteAddress, repository: RepositoryId) -> Self {
        Self {
            address,
            repository,
            hub_key: None,
            ca_file: None,
            sign_in: false,
        }
    }

    /// The content of the configuration file: the header and the five lines in their fixed order.
    pub fn encode(&self) -> Vec<u8> {
        let hub_key = self
            .hub_key
            .as_ref()
            .map_or_else(|| UNSET.to_owned(), PublicKey::to_openssh);
        let ca_file = self.ca_file.as_ref().map_or(UNSET, |file| file.0.as_str());
        let sign_in = if self.sign_in { "yes" } else { "no" };
        format!(
            "{CONFIG_HEADER}\naddress {}\nrepository {}\nhub-key {hub_key}\nca-file {ca_file}\n\
             sign-in {sign_in}\n",
            self.address, self.repository
        )
        .into_bytes()
    }

    /// Reads a configuration file. It accepts what [`encode`](Self::encode) writes and nothing
    /// else, so `parse(bytes).encode() == bytes` for every accepted input.
    ///
    /// # Errors
    ///
    /// Returns [`RemoteParseError`] for input that is not UTF-8, has no newline at its end, does
    /// not start with the header, has another line than the five in their order, or holds a value
    /// that its type rejects. A key with a comment is rejected, as it is not what is written.
    pub fn parse(bytes: &[u8]) -> Result<Self, RemoteParseError> {
        let text = std::str::from_utf8(bytes).map_err(|error| RemoteParseError::NotUtf8 {
            valid_up_to: error.valid_up_to(),
        })?;
        let text = text.strip_suffix('\n').ok_or(RemoteParseError::CutOff)?;
        let mut lines = text.split('\n');
        if lines.next() != Some(CONFIG_HEADER) {
            return Err(RemoteParseError::Header);
        }
        let mut value = |field: RemoteField| {
            lines
                .next()
                .and_then(|line| line.strip_prefix(&format!("{field} ")))
                .ok_or(RemoteParseError::Line { expected: field })
        };
        let address = RemoteAddress::try_new(value(RemoteField::Address)?)?;
        let repository = value(RemoteField::RepositoryId)?;
        let repository = repository
            .parse::<RepositoryId>()
            .map_err(|_| rejected(RemoteField::RepositoryId, repository))?;
        let hub_key = match value(RemoteField::HubKey)? {
            UNSET => None,
            text => Some(
                PublicKey::from_openssh(text)
                    .ok()
                    .filter(|key| key.to_openssh() == text)
                    .ok_or_else(|| rejected(RemoteField::HubKey, text))?,
            ),
        };
        let ca_file = match value(RemoteField::CaFile)? {
            UNSET => None,
            text => Some(CaFile::try_new(text)?),
        };
        let sign_in = match value(RemoteField::SignIn)? {
            "yes" => true,
            "no" => false,
            text => return Err(rejected(RemoteField::SignIn, text).into()),
        };
        if lines.next().is_some() {
            return Err(RemoteParseError::ExtraLines);
        }
        Ok(Self {
            address,
            repository,
            hub_key,
            ca_file,
            sign_in,
        })
    }
}

/// Why the content of the configuration of a remote is rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RemoteParseError {
    /// The content is not valid UTF-8.
    NotUtf8 {
        /// Length of the longest valid prefix, in bytes.
        valid_up_to: usize,
    },
    /// The file does not end with a newline, or it is empty: it is cut off.
    CutOff,
    /// The first line is not `stemma-remote 1`.
    Header,
    /// A line is missing or does not start with the name that has its place.
    Line {
        /// The line that was expected.
        expected: RemoteField,
    },
    /// The file goes on after the line `sign-in`.
    ExtraLines,
    /// A value is rejected.
    Value(RemoteValueError),
    /// The name of the file is no name of a remote.
    Name(RemoteNameError),
}

impl From<RemoteValueError> for RemoteParseError {
    fn from(error: RemoteValueError) -> Self {
        Self::Value(error)
    }
}

impl fmt::Display for RemoteParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotUtf8 { valid_up_to } => {
                write!(f, "not valid UTF-8 (valid up to byte {valid_up_to})")
            }
            Self::CutOff => f.write_str("the file is empty or has no terminating newline"),
            Self::Header => write!(f, "the first line is not `{CONFIG_HEADER}`"),
            Self::Line { expected } => write!(f, "expected the line `{expected} <value>`"),
            Self::ExtraLines => f.write_str("the file goes on after the line `sign-in`"),
            Self::Value(error) => error.fmt(f),
            Self::Name(error) => error.fmt(f),
        }
    }
}

impl Error for RemoteParseError {}

/// Why the configuration of the remotes cannot be read.
#[derive(Debug)]
#[non_exhaustive]
pub enum RemoteError {
    /// The file system failed.
    Io {
        /// What was being done, for example "read the configuration of the remote".
        operation: &'static str,
        /// The file or directory involved.
        path: PathBuf,
        /// The error of the file system. Its message ends the message of this error.
        source: io::Error,
    },
    /// A configuration file is not in canonical form, or an entry of the directory has a name
    /// that no remote can have.
    Corrupt {
        /// The file.
        path: PathBuf,
        /// What is wrong with it.
        source: RemoteParseError,
    },
}

impl RemoteError {
    /// Returns the error of the file system, if that is what failed. The chain of
    /// [`Error::source`] does not lead to it (see the crate documentation).
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Corrupt { .. } => None,
        }
    }
}

impl fmt::Display for RemoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(f, "could not {operation} `{}`: {source}", path.display()),
            Self::Corrupt { path, source } => write!(
                f,
                "`{}` is not a valid configuration of a remote: {source}",
                path.display()
            ),
        }
    }
}

impl Error for RemoteError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            // The message has the message of the cause, so the source is the cause of the cause.
            Self::Io { source, .. } => source.source(),
            Self::Corrupt { .. } => None,
        }
    }
}

fn io_error<'a>(
    operation: &'static str,
    path: &'a Path,
) -> impl FnOnce(io::Error) -> RemoteError + 'a {
    move |source| RemoteError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

/// The names of the remotes that have a configuration below the metadata directory `root`, each
/// with its file, sorted by name. A repository without the directory has no remote. An entry
/// whose name starts with a dot is skipped, as in the listing of the branches; any other entry
/// whose name is no name of a remote is damage, because a reader accepts only what the writer
/// writes (ADR-005).
fn configured(root: &Path) -> Result<Vec<(RemoteName, PathBuf)>, RemoteError> {
    let dir = root.join(REMOTES_DIR);
    let operation = "list the remotes in";
    let entries = match fault::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(io_error(operation, &dir)(error)),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io_error(operation, &dir))?;
        let file_name = entry.file_name().to_string_lossy().into_owned();
        if file_name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let name = RemoteName::try_new(file_name).map_err(|error| RemoteError::Corrupt {
            path: path.clone(),
            source: RemoteParseError::Name(error),
        })?;
        found.push((name, path));
    }
    found.sort();
    Ok(found)
}

/// Reads one configuration file, which has to be a regular file; a link is not followed. A file
/// that is gone since the listing is `None`: a reader holds no lock, so a change may remove a
/// remote in between, and the remote then does not exist.
fn read_config(path: &Path) -> Result<Option<RemoteConfig>, RemoteError> {
    let operation = "read the configuration of the remote";
    let gone = |error: &io::Error| error.kind() == io::ErrorKind::NotFound;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if gone(&error) => return Ok(None),
        Err(error) => return Err(io_error(operation, path)(error)),
    };
    if !metadata.is_file() {
        return Err(RemoteError::Io {
            operation,
            path: path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"),
        });
    }
    let bytes = match fault::read(path) {
        Ok(bytes) => bytes,
        Err(error) if gone(&error) => return Ok(None),
        Err(error) => return Err(io_error(operation, path)(error)),
    };
    RemoteConfig::parse(&bytes)
        .map(Some)
        .map_err(|source| RemoteError::Corrupt {
            path: path.to_path_buf(),
            source,
        })
}

/// The configuration of every remote below the metadata directory `root`, sorted by name.
pub(crate) fn read_all(root: &Path) -> Result<Vec<(RemoteName, RemoteConfig)>, RemoteError> {
    let mut found = Vec::new();
    for (name, path) in configured(root)? {
        if let Some(config) = read_config(&path)? {
            found.push((name, config));
        }
    }
    Ok(found)
}

/// The configuration of the remote with exactly this name, or `None` if there is none. The name
/// is compared with the listing of the directory, so on a file system that ignores case `origin`
/// does not open the file of `Origin`.
pub(crate) fn read_one(
    root: &Path,
    name: &RemoteName,
) -> Result<Option<RemoteConfig>, RemoteError> {
    match configured(root)?
        .into_iter()
        .find(|(found, _)| found == name)
    {
        Some((_, path)) => read_config(&path),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    const KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMt6BisKCOOMdkWowXYqXUrS1zKF4aE6QenzZ1SSB/Md";
    const ID: &str = "0123456789abcdef0123456789abcdef";
    const ADDRESS: &str = "https://hub.example/talent-factory/stemma";
    /// An absolute path with a space, by the rule of the platform.
    const CA_FILE: &str = if cfg!(windows) {
        r"C:\stemma\own ca.pem"
    } else {
        "/etc/stemma/own ca.pem"
    };

    fn id(digit: char) -> String {
        format!("b3:{}", digit.to_string().repeat(64))
    }

    fn config() -> RemoteConfig {
        RemoteConfig::new(
            RemoteAddress::try_new(ADDRESS).unwrap(),
            ID.parse().unwrap(),
        )
    }

    fn full() -> RemoteConfig {
        RemoteConfig {
            hub_key: Some(KEY.parse().unwrap()),
            ca_file: Some(CaFile::try_new(CA_FILE).unwrap()),
            sign_in: true,
            ..config()
        }
    }

    fn lines(lines: &[&str]) -> Vec<u8> {
        lines
            .iter()
            .flat_map(|line| [line, "\n"])
            .collect::<String>()
            .into_bytes()
    }

    #[test]
    fn the_name_of_a_remote_is_one_component_of_a_branch_name() {
        for name in ["origin", "hub-2", "Up.stream_1"] {
            assert_eq!(RemoteName::try_new(name).unwrap().as_str(), name);
        }
        assert_eq!(
            RemoteName::try_new("a/b").unwrap_err().problem,
            RemoteNameProblem::Separator
        );
        for (name, problem) in [
            ("", BranchNameProblem::Empty),
            (".hidden", BranchNameProblem::LeadingDotOrDash),
            ("a b", BranchNameProblem::InvalidCharacter(' ')),
            ("con", BranchNameProblem::ReservedName),
            ("/a", BranchNameProblem::EmptyComponent),
        ] {
            let error = RemoteName::try_new(name).unwrap_err();
            assert_eq!(error.problem, RemoteNameProblem::Name(problem), "{name}");
        }
        assert_eq!(
            RemoteName::try_new("a b").unwrap_err().to_string(),
            "invalid remote name `a b`: the character ` ` is not allowed; use letters, digits, \
             `.`, `_`, and `-`"
        );
        assert_eq!(
            "a/b".parse::<RemoteName>().unwrap_err().to_string(),
            "invalid remote name `a/b`: the name of a remote is one component, without `/`"
        );
    }

    #[test]
    fn a_remote_reference_prints_as_its_qualified_form_and_sorts_by_remote_kind_and_name() {
        let named = |remote: &str, kind, name: &str| RemoteRefName {
            remote: remote.parse().unwrap(),
            kind,
            name: name.parse().unwrap(),
        };
        let branch = named("origin", RefKind::Branch, "feat/x");
        let tag = named("origin", RefKind::Tag, "a");
        assert_eq!(branch.to_string(), "remotes/origin/heads/feat/x");
        assert_eq!(tag.to_string(), "remotes/origin/tags/a");
        // The text reads back, and nothing but the qualified form does.
        for name in [&branch, &tag] {
            assert_eq!(&name.to_string().parse::<RemoteRefName>().unwrap(), name);
        }
        for text in [
            "origin/heads/x",
            "remotes/origin/x",
            "remotes/origin/heads",
            "heads/x",
        ] {
            assert_eq!(
                text.parse::<RemoteRefName>(),
                Err(RemoteRefNameError::Shape(text.to_owned())),
                "{text}"
            );
        }
        assert!(matches!(
            "remotes/.o/heads/x".parse::<RemoteRefName>(),
            Err(RemoteRefNameError::Remote(_))
        ));
        assert!(matches!(
            "remotes/o/tags/x//y".parse::<RemoteRefName>(),
            Err(RemoteRefNameError::Name(_))
        ));
        let mut names = vec![tag.clone(), named("hub", RefKind::Tag, "z"), branch.clone()];
        names.sort();
        assert_eq!(names, [named("hub", RefKind::Tag, "z"), branch, tag]);
    }

    #[test]
    fn the_file_of_a_remote_reference_is_pinned_by_a_literal() {
        let reference = RemoteRef {
            commit: CommitId::new(id('1').parse().unwrap()),
            entry: id('2').parse().unwrap(),
        };
        let text = format!("{}\nentry {}\n", id('1'), id('2'));
        assert_eq!(reference.encode(), text.as_bytes());
        assert_eq!(RemoteRef::parse(text.as_bytes()).unwrap(), reference);
    }

    #[test]
    fn the_reader_of_a_remote_reference_rejects_what_the_writer_does_not_write() {
        let (commit, entry) = (id('1'), id('2'));
        let shape = RefParseError::Malformed(RefMalformed::RemoteShape);
        let cut_off = RefParseError::Malformed(RefMalformed::CutOff);
        for (text, expected) in [
            (String::new(), cut_off.clone()),
            (format!("{commit}\nentry {entry}"), cut_off),
            // The file of a branch: one line.
            (format!("{commit}\n"), shape.clone()),
            (format!("{commit}\n{entry}\n"), shape.clone()),
            (format!("{commit}\nentry  {entry}\n"), {
                RefParseError::InvalidRecordHash(
                    format!(" {entry}").parse::<ObjectId>().unwrap_err(),
                )
            }),
            (format!("{commit}\nEntry {entry}\n"), shape.clone()),
            (format!("{commit}\nentry {entry}\n\n"), shape.clone()),
            (
                format!("{commit}\nentry {entry}\nentry {entry}\n"),
                shape.clone(),
            ),
            (format!("{commit}\r\nentry {entry}\n"), {
                RefParseError::InvalidObjectId(
                    format!("{commit}\r").parse::<ObjectId>().unwrap_err(),
                )
            }),
            (format!("entry {entry}\n{commit}\n"), shape),
        ] {
            assert_eq!(RemoteRef::parse(text.as_bytes()), Err(expected), "{text:?}");
        }
        assert!(matches!(
            RemoteRef::parse(format!("{commit}\nentry b3:XYZ\n").as_bytes()),
            Err(RefParseError::InvalidRecordHash(_))
        ));
        assert!(matches!(
            RemoteRef::parse(format!("b3:12\nentry {entry}\n").as_bytes()),
            Err(RefParseError::InvalidObjectId(_))
        ));
        assert!(matches!(
            RemoteRef::parse(b"\xff\n"),
            Err(RefParseError::NotUtf8 { valid_up_to: 0 })
        ));
    }

    #[test]
    fn the_configuration_is_pinned_by_literals() {
        let minimal = lines(&[
            "stemma-remote 1",
            "address https://hub.example/talent-factory/stemma",
            "repository 0123456789abcdef0123456789abcdef",
            "hub-key -",
            "ca-file -",
            "sign-in no",
        ]);
        assert_eq!(config().encode(), minimal);
        assert_eq!(RemoteConfig::parse(&minimal).unwrap(), config());

        let all = lines(&[
            "stemma-remote 1",
            "address https://hub.example/talent-factory/stemma",
            "repository 0123456789abcdef0123456789abcdef",
            &format!("hub-key {KEY}"),
            &format!("ca-file {CA_FILE}"),
            "sign-in yes",
        ]);
        assert_eq!(full().encode(), all);
        assert_eq!(RemoteConfig::parse(&all).unwrap(), full());
        assert_eq!(full().ca_file.unwrap().as_path(), Path::new(CA_FILE));
    }

    /// The lines of the full configuration with one line replaced, removed, or added.
    fn changed(index: usize, with: &[&str]) -> Vec<u8> {
        let text = String::from_utf8(full().encode()).unwrap();
        let mut all: Vec<&str> = text.lines().collect();
        all.splice(index..=index, with.iter().copied());
        lines(&all)
    }

    #[test]
    fn the_reader_of_the_configuration_rejects_what_the_writer_does_not_write() {
        let line = |expected| RemoteParseError::Line { expected };
        let value = |field, text: &str| RemoteParseError::Value(rejected(field, text));
        let key_with_comment = format!("{KEY} hub");
        for (bytes, expected) in [
            (Vec::new(), RemoteParseError::CutOff),
            (full().encode()[..10].to_vec(), RemoteParseError::CutOff),
            (changed(0, &["stemma-remote 2"]), RemoteParseError::Header),
            (changed(0, &[]), RemoteParseError::Header),
            // A line that is missing, and two lines in the other order.
            (changed(1, &[]), line(RemoteField::Address)),
            (changed(4, &[]), line(RemoteField::CaFile)),
            (changed(5, &[]), line(RemoteField::SignIn)),
            (
                changed(1, &[&format!("repository {ID}")]),
                line(RemoteField::Address),
            ),
            (changed(2, &["repository"]), line(RemoteField::RepositoryId)),
            (changed(3, &["hub-key"]), line(RemoteField::HubKey)),
            (
                changed(5, &["sign-in yes", ""]),
                RemoteParseError::ExtraLines,
            ),
            (
                changed(5, &["sign-in yes", "sign-in yes"]),
                RemoteParseError::ExtraLines,
            ),
            // The values.
            (
                changed(1, &["address hub.example/a/b"]),
                value(RemoteField::Address, "hub.example/a/b"),
            ),
            (
                changed(1, &["address https://"]),
                value(RemoteField::Address, "https://"),
            ),
            (
                changed(1, &["address https://hub.example/a b"]),
                value(RemoteField::Address, "https://hub.example/a b"),
            ),
            (
                changed(1, &["address  https://hub.example/a"]),
                value(RemoteField::Address, " https://hub.example/a"),
            ),
            (
                changed(2, &["repository 0123456789ABCDEF0123456789abcdef"]),
                value(
                    RemoteField::RepositoryId,
                    "0123456789ABCDEF0123456789abcdef",
                ),
            ),
            (
                changed(2, &["repository 0123"]),
                value(RemoteField::RepositoryId, "0123"),
            ),
            (
                changed(2, &["repository -"]),
                value(RemoteField::RepositoryId, "-"),
            ),
            (
                changed(3, &[&format!("hub-key {key_with_comment}")]),
                value(RemoteField::HubKey, &key_with_comment),
            ),
            (
                changed(3, &["hub-key ssh-ed25519 AAAA"]),
                value(RemoteField::HubKey, "ssh-ed25519 AAAA"),
            ),
            (changed(3, &["hub-key "]), value(RemoteField::HubKey, "")),
            (changed(4, &["ca-file "]), value(RemoteField::CaFile, "")),
            (
                changed(4, &["ca-file own-ca.pem"]),
                value(RemoteField::CaFile, "own-ca.pem"),
            ),
            (
                changed(4, &["ca-file ./-"]),
                value(RemoteField::CaFile, "./-"),
            ),
            (
                changed(4, &["ca-file /a "]),
                value(RemoteField::CaFile, "/a "),
            ),
            (
                changed(4, &["ca-file /a\tb"]),
                value(RemoteField::CaFile, "/a\tb"),
            ),
            (
                changed(5, &["sign-in true"]),
                value(RemoteField::SignIn, "true"),
            ),
            (
                changed(5, &["sign-in Yes"]),
                value(RemoteField::SignIn, "Yes"),
            ),
            (
                changed(1, &["address https://hub.example/\u{e4}"]),
                value(RemoteField::Address, "https://hub.example/\u{e4}"),
            ),
            (
                changed(1, &["address https://hub.example/a\u{7f}"]),
                value(RemoteField::Address, "https://hub.example/a\u{7f}"),
            ),
            (
                changed(1, &["address HTTPS://hub.example/a"]),
                value(RemoteField::Address, "HTTPS://hub.example/a"),
            ),
            (
                changed(1, &["address ftp://hub.example/a"]),
                value(RemoteField::Address, "ftp://hub.example/a"),
            ),
            // One repository has one spelling, and no secret lies in the address.
            (
                changed(1, &["address https://hub.example/a/b/"]),
                value(RemoteField::Address, "https://hub.example/a/b/"),
            ),
            (
                changed(1, &["address https:///a/b"]),
                value(RemoteField::Address, "https:///a/b"),
            ),
            (
                changed(1, &["address https://hub.example/a/b?x=1"]),
                value(RemoteField::Address, "https://hub.example/a/b?x=1"),
            ),
            (
                changed(1, &["address https://hub.example/a/b#f"]),
                value(RemoteField::Address, "https://hub.example/a/b#f"),
            ),
            (
                changed(1, &["address https://me:secret@hub.example/a/b"]),
                value(RemoteField::Address, "https://me:secret@hub.example/a/b"),
            ),
            (
                changed(4, &["ca-file  /a"]),
                value(RemoteField::CaFile, " /a"),
            ),
        ] {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            assert_eq!(RemoteConfig::parse(&bytes), Err(expected), "{text:?}");
        }
        assert_eq!(
            RemoteConfig::parse(b"stemma-remote 1\n\xff\n"),
            Err(RemoteParseError::NotUtf8 { valid_up_to: 16 })
        );
        // A carriage return is no part of a line.
        let dos = String::from_utf8(full().encode())
            .unwrap()
            .replace('\n', "\r\n");
        assert_eq!(
            RemoteConfig::parse(dos.as_bytes()),
            Err(RemoteParseError::Header)
        );
    }

    #[test]
    fn every_accepted_configuration_reads_back_to_its_bytes() {
        // `http://` is a valid address; that only a loopback address may have it is a rule of
        // the client.
        let local = RemoteConfig {
            address: RemoteAddress::try_new("http://127.0.0.1:8080/a/b").unwrap(),
            ..config()
        };
        for config in [config(), full(), local] {
            let bytes = config.encode();
            assert_eq!(RemoteConfig::parse(&bytes).unwrap().encode(), bytes);
        }
    }

    #[test]
    fn the_messages_of_a_rejected_configuration_are_pinned() {
        assert_eq!(
            RemoteParseError::Line {
                expected: RemoteField::HubKey
            }
            .to_string(),
            "expected the line `hub-key <value>`"
        );
        assert_eq!(
            RemoteParseError::Header.to_string(),
            "the first line is not `stemma-remote 1`"
        );
        assert_eq!(
            rejected(RemoteField::RepositoryId, "x\n").to_string(),
            "invalid value `x\\n` for `repository`: a repository ID is 32 lowercase hexadecimal \
             digits"
        );
        assert_eq!(CaFile::try_new("-").unwrap_err().field, RemoteField::CaFile);
    }

    fn write(dir: &TempDir, name: &str, content: &[u8]) -> PathBuf {
        let remotes = dir.path().join(REMOTES_DIR);
        fs::create_dir_all(&remotes).unwrap();
        let path = remotes.join(name);
        fs::write(&path, content).unwrap();
        path
    }

    fn remote(name: &str) -> RemoteName {
        name.parse().unwrap()
    }

    #[test]
    fn without_the_directory_there_is_no_remote() {
        let dir = TempDir::new();
        assert!(read_all(dir.path()).unwrap().is_empty());
        assert_eq!(read_one(dir.path(), &remote("origin")).unwrap(), None);
    }

    #[test]
    fn the_remotes_are_read_sorted_by_name_and_one_by_its_exact_name() {
        let dir = TempDir::new();
        write(&dir, "origin", &config().encode());
        write(&dir, "backup", &full().encode());
        write(&dir, ".DS_Store", b"left by a file manager");
        assert_eq!(
            read_all(dir.path()).unwrap(),
            [(remote("backup"), full()), (remote("origin"), config())]
        );
        assert_eq!(
            read_one(dir.path(), &remote("origin")).unwrap(),
            Some(config())
        );
        assert_eq!(read_one(dir.path(), &remote("Origin")).unwrap(), None);
        assert_eq!(read_one(dir.path(), &remote("other")).unwrap(), None);
    }

    #[test]
    fn a_file_that_is_not_canonical_is_damage_with_its_path() {
        let dir = TempDir::new();
        let path = write(&dir, "origin", b"stemma-remote 1\n");
        for result in [
            read_all(dir.path()).map(drop),
            read_one(dir.path(), &remote("origin")).map(drop),
        ] {
            let error = result.unwrap_err();
            assert!(
                matches!(
                    &error,
                    RemoteError::Corrupt { path: found, source: RemoteParseError::Line { .. } }
                        if found == &path
                ),
                "{error}"
            );
            assert!(error.io_error().is_none());
            assert_eq!(
                error.to_string(),
                format!(
                    "`{}` is not a valid configuration of a remote: expected the line `address \
                     <value>`",
                    path.display()
                )
            );
        }
    }

    #[test]
    fn an_entry_with_a_name_that_no_remote_has_is_damage() {
        let dir = TempDir::new();
        write(&dir, "origin", &config().encode());
        let path = write(&dir, "origin~", &config().encode());
        let error = read_one(dir.path(), &remote("origin")).unwrap_err();
        assert!(
            matches!(
                &error,
                RemoteError::Corrupt { path: found, source: RemoteParseError::Name(_) }
                    if found == &path
            ),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_link_in_the_place_of_a_file_is_not_followed() {
        let dir = TempDir::new();
        let elsewhere = dir.path().join("elsewhere");
        fs::write(&elsewhere, config().encode()).unwrap();
        fs::create_dir_all(dir.path().join(REMOTES_DIR)).unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.path().join(REMOTES_DIR).join("origin"))
            .unwrap();
        for error in [
            read_all(dir.path()).unwrap_err(),
            read_one(dir.path(), &remote("origin")).unwrap_err(),
        ] {
            assert!(error.to_string().contains("not a regular file"), "{error}");
        }
    }

    #[test]
    fn a_file_that_is_gone_since_the_listing_is_no_remote() {
        let dir = TempDir::new();
        fs::create_dir_all(dir.path().join(REMOTES_DIR)).unwrap();
        let gone = dir.path().join(REMOTES_DIR).join("origin");
        assert_eq!(read_config(&gone).unwrap(), None);
    }

    #[test]
    fn a_directory_in_the_place_of_a_file_is_an_error_of_the_file_system() {
        let dir = TempDir::new();
        fs::create_dir_all(dir.path().join(REMOTES_DIR).join("origin")).unwrap();
        let error = read_all(dir.path()).unwrap_err();
        assert!(error.io_error().is_some(), "{error}");
        assert!(error.to_string().contains("not a regular file"), "{error}");

        // The directory of the remotes as a file is no "no remotes".
        let other = TempDir::new();
        fs::write(other.path().join(REMOTES_DIR), b"").unwrap();
        assert!(read_all(other.path()).unwrap_err().io_error().is_some());
    }
}
