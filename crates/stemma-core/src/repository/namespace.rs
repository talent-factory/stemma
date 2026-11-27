//! The namespace policy in a repository (ADR-027, STEMMA-311): a hub reads it from the
//! namespace repository ([`Repository::namespace`]), and a clone keeps the one that it fetched
//! last from each remote in `namespaces/<remote>` ([`Repository::kept_namespace`]), which the
//! local gate evaluates beside the policy of `main`.
//!
//! The kept file is replaced as a whole under the lock of the references by the fetch that read
//! the list, before the remote references move, and removed by a fetch whose list names no
//! namespace policy. It is the answer of the hub and no history: no entry of the log names it,
//! and `op restore` does not bring an older one back. The local gate reads the files of the
//! remotes that are configured, so the file of a removed remote counts no more.

use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::{raise_format, PolicyError, Repository, FORMAT_VERSION};
use crate::fault;
use crate::namespace::{KeptNamespace, KeptNamespaceError, Namespace, MAX_KEPT_LEN};
use crate::policy::{self, Policy};
use crate::pull::TARGET;
use crate::refs::{BranchName, RefError};
use crate::register::{ModeError, Register};
use crate::remote::{RemoteError, RemoteName};
use crate::store::{flush_directory, unique_temp_name, StoreError};

/// The directory below the metadata directory that holds one kept namespace policy per remote.
pub(crate) const NAMESPACES_DIR: &str = "namespaces";

/// The first version of the format that knows the directory [`NAMESPACES_DIR`].
pub(crate) const NAMESPACES_SINCE: u32 = 22;

const _: () = assert!(NAMESPACES_SINCE <= FORMAT_VERSION);

impl Repository {
    /// The namespace that this repository, a namespace repository, sets (ADR-027): the file
    /// [`NAMESPACE_FILE_NAME`](policy::NAMESPACE_FILE_NAME) at the tip of `main`, the commit of
    /// that tip, and the owners of the namespace that the policy demands, from the register of
    /// that tip ([`Namespace::new`]). A hub reads it for every ref update of a repository of the
    /// namespace and passes it in the context of the accepting operation. It reads the store and
    /// the references only, takes no lock, and works on a bare repository.
    ///
    /// Every error means that the rules of the namespace cannot be told. A hub then refuses the
    /// ref update and the list of references of the repositories of the namespace; it never
    /// treats them as outside a namespace, which would accept what the namespace policy forbids.
    ///
    /// # Errors
    ///
    /// [`NamespaceError::NoMain`] without `main`, [`NamespaceError::NoPolicy`] if its tip has no
    /// file of the namespace policy, [`NamespaceError::Policy`] if the file cannot be used,
    /// [`NamespaceError::NoRegister`] and [`NamespaceError::Register`] for the register,
    /// [`NamespaceError::NoOwner`] if the policy demands the owners and the register has none,
    /// and [`NamespaceError::Refs`] if `main` cannot be read.
    pub fn namespace(&self) -> Result<Namespace, NamespaceError> {
        let main = BranchName::try_new(TARGET).expect("`main` is a branch name");
        let tip = self
            .refs()
            .branch(&main)
            .map_err(NamespaceError::Refs)?
            .ok_or(NamespaceError::NoMain)?;
        let policy = self
            .policy_file_at(&tip, policy::NAMESPACE_FILE_NAME)
            .map_err(NamespaceError::Policy)?
            .ok_or(NamespaceError::NoPolicy)?;
        let register = Register::at_commit(self.objects(), &tip)
            .map_err(NamespaceError::Register)?
            .ok_or(NamespaceError::NoRegister)?;
        Namespace::new(tip, policy, &register).map_err(|_| NamespaceError::NoOwner)
    }

    /// The namespace policy that the last fetch from `remote` brought, or `None` if it named
    /// none or no fetch ran.
    ///
    /// # Errors
    ///
    /// [`KeptNamespaceReadError::Io`] if the file cannot be read, and
    /// [`KeptNamespaceReadError::Damaged`] if it is not in the form of a fetch.
    ///
    /// On a bare repository the call is refused before anything else, with the error
    /// [`Bare`](crate::refs::RefError::Bare) of the references; see
    /// [`is_bare`](crate::repository::Repository::is_bare).
    pub fn kept_namespace(
        &self,
        remote: &RemoteName,
    ) -> Result<Option<KeptNamespace>, KeptNamespaceReadError> {
        self.require_oplog("read the kept namespace policy")?;
        self.read_kept_namespace(remote)
    }

    /// [`kept_namespace`](Self::kept_namespace) without the check of the bare repository.
    fn read_kept_namespace(
        &self,
        remote: &RemoteName,
    ) -> Result<Option<KeptNamespace>, KeptNamespaceReadError> {
        let path = self.shared.join(NAMESPACES_DIR).join(remote.as_str());
        let file = match fault::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(KeptNamespaceReadError::Io { path, source }),
        };
        let mut bytes = Vec::new();
        if let Err(source) =
            io::Read::read_to_end(&mut io::Read::take(file, MAX_KEPT_LEN + 1), &mut bytes)
        {
            return Err(KeptNamespaceReadError::Io { path, source });
        }
        KeptNamespace::parse(&bytes)
            .map(Some)
            .map_err(|error| KeptNamespaceReadError::Damaged { path, error })
    }

    /// The namespace policies that the local gate evaluates beside the policy of `main`: the
    /// kept one of every configured remote, in the order of the names of the remotes.
    pub(crate) fn kept_namespace_policies(&self) -> Result<Vec<Policy>, KeptNamespaceReadError> {
        let mut policies = Vec::new();
        for (remote, _) in self.remotes().map_err(KeptNamespaceReadError::Remote)? {
            if let Some(kept) = self.read_kept_namespace(&remote)? {
                policies.push(kept.policy().clone());
            }
        }
        Ok(policies)
    }

    /// Replaces the kept namespace policy of `remote` with `kept`, or removes it for `None`. The
    /// caller holds the lock of the references. The repository rises to the format that knows
    /// the directory before the first file is written (ADR-009).
    pub(super) fn keep_namespace(
        &self,
        remote: &RemoteName,
        kept: Option<&KeptNamespace>,
    ) -> Result<(), KeepNamespaceError> {
        let dir = self.shared.join(NAMESPACES_DIR);
        let path = dir.join(remote.as_str());
        let Some(kept) = kept else {
            return match fault::remove_file(&path) {
                Ok(()) => flush_directory(&dir).map_err(|source| KeepNamespaceError::Io {
                    operation: "flush the directory",
                    path: dir,
                    source,
                }),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(source) => Err(KeepNamespaceError::Io {
                    operation: "remove the kept namespace policy",
                    path,
                    source,
                }),
            };
        };
        // A file that cannot be read is no equal file: it is written again, which repairs it.
        if self.read_kept_namespace(remote).ok().flatten().as_ref() == Some(kept) {
            return Ok(());
        }
        raise_format(&self.shared, FORMAT_VERSION).map_err(KeepNamespaceError::Format)?;
        write_replacing(
            &dir,
            &self.shared.join("tmp"),
            remote.as_str(),
            kept.encode().as_bytes(),
        )
    }
}

/// Writes `bytes` as the file `name` of `dir`: into a temporary file of `scratch`, flushed, and
/// renamed into place, and the directory is flushed, so a reader sees the old or the new file.
fn write_replacing(
    dir: &Path,
    scratch: &Path,
    name: &str,
    bytes: &[u8],
) -> Result<(), KeepNamespaceError> {
    let io_error = |operation: &'static str, path: &Path| {
        let path = path.to_path_buf();
        move |source| KeepNamespaceError::Io {
            operation,
            path,
            source,
        }
    };
    for made in [scratch, dir] {
        match fault::create_dir(made) {
            Ok(()) => {
                let parent = made.parent().unwrap_or(made);
                flush_directory(parent).map_err(io_error("flush the directory", parent))?;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(io_error("make the directory", made)(error)),
        }
    }
    let temp = scratch.join(unique_temp_name());
    let written = (|| {
        let mut file = fault::create_new(&temp)?;
        io::Write::write_all(&mut file, bytes)?;
        file.sync_all()
    })();
    if let Err(error) = written {
        let _ = fs::remove_file(&temp);
        return Err(io_error("write the temporary file", &temp)(error));
    }
    let path = dir.join(name);
    if let Err(error) = fault::rename(&temp, &path) {
        let _ = fs::remove_file(&temp);
        return Err(io_error("move the file into place at", &path)(error));
    }
    flush_directory(dir).map_err(io_error("flush the directory", dir))
}

/// Why a hub cannot read the namespace from a namespace repository.
#[derive(Debug)]
#[non_exhaustive]
pub enum NamespaceError {
    /// The namespace repository has no `main`: no first push reached it.
    NoMain,
    /// The tip of `main` has no file of the namespace policy.
    NoPolicy,
    /// The file of the namespace policy cannot be used: it is no file, not valid, or cannot be
    /// read from the store.
    Policy(PolicyError),
    /// The tip of `main` has no register.
    NoRegister,
    /// The register of the tip of `main` cannot be read or is not valid.
    Register(ModeError),
    /// The namespace policy demands the owners of the namespace, and the register holds no human
    /// owner with an active key, so the demand would ask for nothing.
    NoOwner,
    /// The reference `main` cannot be read.
    Refs(RefError),
}

impl fmt::Display for NamespaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let file = policy::NAMESPACE_FILE_NAME;
        match self {
            Self::NoMain => write!(f, "the namespace repository has no `{TARGET}`"),
            Self::NoPolicy => write!(
                f,
                "the tip of `{TARGET}` of the namespace repository has no {file}"
            ),
            Self::Policy(PolicyError::NotAFile) => {
                write!(f, "{file} of the namespace repository is not a file")
            }
            Self::Policy(PolicyError::Invalid(error)) => {
                write!(
                    f,
                    "{file} of the namespace repository is not valid: {error}"
                )
            }
            Self::Policy(PolicyError::Unreadable(reason)) => {
                write!(
                    f,
                    "cannot read {file} of the namespace repository: {reason}"
                )
            }
            Self::Policy(error) => error.fmt(f),
            Self::NoRegister => write!(
                f,
                "the tip of `{TARGET}` of the namespace repository has no register"
            ),
            Self::Register(error) => {
                write!(
                    f,
                    "the register of the namespace repository cannot be used: {error}"
                )
            }
            Self::NoOwner => crate::namespace::NoDemandedOwner.fmt(f),
            Self::Refs(error) => write!(f, "cannot read `{TARGET}`: {error}"),
        }
    }
}

impl Error for NamespaceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Policy(error) => Some(error),
            Self::Register(error) => Some(error),
            Self::Refs(error) => Some(error),
            Self::NoMain | Self::NoPolicy | Self::NoRegister | Self::NoOwner => None,
        }
    }
}

/// Why the kept namespace policy of a clone cannot be read.
#[derive(Debug)]
#[non_exhaustive]
pub enum KeptNamespaceReadError {
    /// The repository is bare, or the references cannot be read.
    Refs(RefError),
    /// The configuration of the remotes cannot be read.
    Remote(RemoteError),
    /// The file cannot be read.
    Io {
        /// The file.
        path: PathBuf,
        /// The error of the system.
        source: io::Error,
    },
    /// The file is not in the form of a fetch.
    Damaged {
        /// The file.
        path: PathBuf,
        /// What the reader found.
        error: KeptNamespaceError,
    },
}

impl From<RefError> for KeptNamespaceReadError {
    fn from(error: RefError) -> Self {
        Self::Refs(error)
    }
}

impl fmt::Display for KeptNamespaceReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refs(error) => error.fmt(f),
            Self::Remote(error) => error.fmt(f),
            Self::Io { path, source } => write!(
                f,
                "cannot read the kept namespace policy {}: {source}",
                path.display()
            ),
            Self::Damaged { path, error } => write!(f, "{}: {error}", path.display()),
        }
    }
}

impl Error for KeptNamespaceReadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Refs(error) => Some(error),
            Self::Remote(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            Self::Damaged { error, .. } => Some(error),
        }
    }
}

/// Why a fetch cannot keep the namespace policy of its list.
#[derive(Debug)]
#[non_exhaustive]
pub enum KeepNamespaceError {
    /// The repository cannot rise to the format that knows the file.
    Format(StoreError),
    /// A file or a directory cannot be written, renamed, removed, or flushed.
    Io {
        /// What was tried.
        operation: &'static str,
        /// The path.
        path: PathBuf,
        /// The error of the system.
        source: io::Error,
    },
}

impl fmt::Display for KeepNamespaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Format(error) => write!(f, "cannot raise the format of the repository: {error}"),
            Self::Io {
                operation,
                path,
                source,
            } => write!(f, "cannot {operation} {}: {source}", path.display()),
        }
    }
}

impl Error for KeepNamespaceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Format(error) => Some(error),
            Self::Io { source, .. } => Some(source),
        }
    }
}

#[cfg(test)]
mod tests;
