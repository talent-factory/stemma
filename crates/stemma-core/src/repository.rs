//! A repository: the metadata directory `.stemma/` with its object store and references
//! (ADR-009).
//!
//! The layout of `.stemma/` is:
//!
//! ```text
//! .stemma/
//! ├── format          "stemma-repository 1" and a newline
//! ├── HEAD            "branch <name>" and a newline (the branch may not exist yet)
//! ├── objects/        loose objects (ADR-007)
//! ├── refs/heads/     branches (ADR-008)
//! └── tmp/            temporary files of the object store
//! ```

use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::object::METADATA_DIR;
use crate::refs::{BranchName, FsRefStore, Head};
use crate::store::FsObjectStore;
use crate::{Actor, Authorizer, Decision};

/// The version of the repository format that this code writes and reads.
pub const FORMAT_VERSION: u32 = 1;

/// The name that the file `format` starts with, followed by the version.
const FORMAT_PREFIX: &str = "stemma-repository ";

/// The content of the file `format`: the prefix, the version, and a newline.
fn format_content() -> String {
    format!("{FORMAT_PREFIX}{FORMAT_VERSION}\n")
}

/// Reads the file `format`. It accepts what [`format_content`] writes for any version and nothing
/// else: the prefix, decimal digits without a leading zero, and a newline. Returns `None` for any
/// other input.
fn parse_format(bytes: &[u8]) -> Option<u32> {
    let text = std::str::from_utf8(bytes).ok()?;
    let digits = text.strip_prefix(FORMAT_PREFIX)?.strip_suffix('\n')?;
    let canonical = !digits.is_empty()
        && digits.bytes().all(|byte| byte.is_ascii_digit())
        && (digits.len() == 1 || !digits.starts_with('0'));
    if canonical {
        digits.parse().ok()
    } else {
        None
    }
}

/// What [`Repository::init`] needs besides the directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitOptions {
    initial_branch: BranchName,
}

impl InitOptions {
    /// Options for a repository whose HEAD points to `initial_branch`. The branch itself does not
    /// exist until the first commit.
    pub fn new(initial_branch: BranchName) -> Self {
        Self { initial_branch }
    }

    /// The branch that HEAD points to in a new repository.
    pub fn initial_branch(&self) -> &BranchName {
        &self.initial_branch
    }
}

impl Default for InitOptions {
    /// The initial branch is `main`.
    fn default() -> Self {
        Self::new(BranchName::new("main").expect("`main` is a valid branch name"))
    }
}

/// An open repository: the directory that contains `.stemma/`, with its two stores.
#[derive(Debug, Clone)]
pub struct Repository {
    path: PathBuf,
    objects: FsObjectStore,
    refs: FsRefStore,
}

impl Repository {
    /// Creates a repository in `path`, which is created if it does not exist.
    ///
    /// The call asks `authorizer` first (ADR-002); the operation is `init` and the target is the
    /// directory. The metadata directory is built under a temporary name next to its final place
    /// and renamed at the end, so that a failure leaves no half-made `.stemma/`. A temporary
    /// directory can stay behind if the process is killed; it is named `.stemma-init-<pid>-<n>`
    /// and can be removed by hand. HEAD points to the initial branch of `options`, which does not
    /// exist yet.
    ///
    /// The check for an existing `.stemma/` and the rename are two steps. Two processes that
    /// initialise the same directory at the same moment end with one success and one error, but
    /// the error can be [`InitError::Io`] instead of [`InitError::AlreadyExists`].
    ///
    /// # Errors
    ///
    /// Returns [`InitError::Denied`] if the authorizer refuses, [`InitError::AlreadyExists`] if
    /// `.stemma` exists in `path` (nothing is changed), and [`InitError::Io`] if the storage
    /// fails.
    pub fn init(
        path: impl AsRef<Path>,
        options: &InitOptions,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Self, InitError> {
        let path = path.as_ref();
        let target = path.join(METADATA_DIR);
        if authorizer.authorize(actor, "init", &path.display().to_string()) == Decision::Deny {
            return Err(InitError::Denied {
                path: path.to_path_buf(),
            });
        }
        fs::create_dir_all(path).map_err(InitError::io("create the directory", path))?;
        if exists(&target).map_err(InitError::io("inspect", &target))? {
            return Err(InitError::AlreadyExists { path: target });
        }

        static NEXT: AtomicU64 = AtomicU64::new(0);
        let staging = path.join(format!(
            "{METADATA_DIR}-init-{}-{}",
            process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let built = build(&staging, options).and_then(|()| {
            fs::rename(&staging, &target)
                .map_err(InitError::io("rename the new directory", &target))
        });
        if let Err(error) = built {
            let _ = fs::remove_dir_all(&staging);
            // The rename of a directory onto a non-empty one fails; report what is in the way.
            return Err(match exists(&target) {
                Ok(true) => InitError::AlreadyExists { path: target },
                _ => error,
            });
        }
        sync_directory(path).map_err(InitError::io("flush the directory", path))?;
        Ok(Self::at(path))
    }

    /// Opens the repository in `path`, the directory that contains `.stemma/`. It checks the file
    /// `format` before it returns, which the stores do not do themselves (ADR-007).
    ///
    /// # Errors
    ///
    /// Returns [`OpenError::NotARepository`] if there is no `.stemma` directory,
    /// [`OpenError::MissingFormat`], [`OpenError::MalformedFormat`], or
    /// [`OpenError::UnsupportedFormat`] if the file `format` is missing, not in canonical form, or
    /// of another version, and [`OpenError::Io`] if the storage fails.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, OpenError> {
        let path = path.as_ref();
        let metadata_dir = path.join(METADATA_DIR);
        match fs::metadata(&metadata_dir) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(OpenError::NotARepository {
                    path: path.to_path_buf(),
                })
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(OpenError::NotARepository {
                    path: path.to_path_buf(),
                });
            }
            Err(error) => return Err(OpenError::io("inspect", &metadata_dir)(error)),
        }
        let format_path = metadata_dir.join("format");
        let bytes = match fs::read(&format_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(OpenError::MissingFormat { path: format_path });
            }
            Err(error) => return Err(OpenError::io("read the format file", &format_path)(error)),
        };
        match parse_format(&bytes) {
            Some(FORMAT_VERSION) => Ok(Self::at(path)),
            Some(found) => Err(OpenError::UnsupportedFormat {
                path: format_path,
                found,
            }),
            None => Err(OpenError::MalformedFormat { path: format_path }),
        }
    }

    fn at(path: &Path) -> Self {
        let metadata_dir = path.join(METADATA_DIR);
        Self {
            path: path.to_path_buf(),
            objects: FsObjectStore::new(&metadata_dir),
            refs: FsRefStore::new(metadata_dir),
        }
    }

    /// The directory that contains `.stemma/`.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The metadata directory `.stemma/`.
    pub fn metadata_dir(&self) -> PathBuf {
        self.path.join(METADATA_DIR)
    }

    /// The object store of this repository.
    pub fn objects(&self) -> &FsObjectStore {
        &self.objects
    }

    /// The references of this repository.
    pub fn refs(&self) -> &FsRefStore {
        &self.refs
    }
}

/// Builds the content of `.stemma/` in `dir`, which must not exist.
fn build(dir: &Path, options: &InitOptions) -> Result<(), InitError> {
    fs::create_dir(dir).map_err(InitError::io("create the new directory", dir))?;
    write_new(&dir.join("format"), format_content().as_bytes())?;
    for name in ["objects", "tmp"] {
        fs::create_dir(dir.join(name)).map_err(InitError::io("create the directory", dir))?;
    }
    let heads = dir.join("refs").join("heads");
    fs::create_dir_all(&heads).map_err(InitError::io("create the directory", &heads))?;
    let head = Head::Branch(options.initial_branch.clone());
    write_new(&dir.join("HEAD"), &head.encode())?;
    for flushed in [heads.parent().unwrap_or(dir), &heads, dir] {
        sync_directory(flushed).map_err(InitError::io("flush the directory", flushed))?;
    }
    Ok(())
}

/// Writes a file that must not exist yet, and flushes it to disk.
fn write_new(path: &Path, content: &[u8]) -> Result<(), InitError> {
    let write = || -> io::Result<()> {
        let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
        file.write_all(content)?;
        file.sync_all()
    };
    write().map_err(InitError::io("write the file", path))
}

/// Whether something (a file, a directory, or a link) is at `path`.
fn exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn sync_directory(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// No-op on platforms that cannot flush a directory: the rename is atomic but not made durable.
#[cfg(not(unix))]
fn sync_directory(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Reasons why [`Repository::init`] fails.
#[derive(Debug)]
#[non_exhaustive]
pub enum InitError {
    /// The authorizer refused the operation.
    Denied {
        /// The directory that was to become a repository.
        path: PathBuf,
    },
    /// The directory contains `.stemma` already. Nothing was changed.
    AlreadyExists {
        /// The existing `.stemma`.
        path: PathBuf,
    },
    /// The storage failed. The cause is the field `source`, and its message ends the message of
    /// this error. Use [`InitError::io_error`] to reach it.
    Io {
        /// What was being done, for example "write the file".
        operation: &'static str,
        /// The file or directory involved.
        path: PathBuf,
        /// The error of the file system.
        source: io::Error,
    },
}

impl InitError {
    fn io<'a>(operation: &'static str, path: &'a Path) -> impl FnOnce(io::Error) -> InitError + 'a {
        move |source| InitError::Io {
            operation,
            path: path.to_path_buf(),
            source,
        }
    }

    /// Returns the error of the file system if this is [`InitError::Io`]. The chain of
    /// [`Error::source`] does not lead to it (see the crate documentation).
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Denied { .. } | Self::AlreadyExists { .. } => None,
        }
    }
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Denied { path } => {
                write!(
                    f,
                    "not allowed to create a repository in `{}`",
                    path.display()
                )
            }
            Self::AlreadyExists { path } => {
                write!(
                    f,
                    "`{}` exists already; nothing was changed",
                    path.display()
                )
            }
            Self::Io {
                operation,
                path,
                source,
            } => write!(f, "could not {operation} `{}`: {source}", path.display()),
        }
    }
}

impl Error for InitError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            // The message includes that of the I/O error, so the source is its own cause.
            Self::Io { source, .. } => source.source(),
            Self::Denied { .. } | Self::AlreadyExists { .. } => None,
        }
    }
}

/// Reasons why [`Repository::open`] fails.
#[derive(Debug)]
#[non_exhaustive]
pub enum OpenError {
    /// The directory has no `.stemma` directory.
    NotARepository {
        /// The directory that was to be opened.
        path: PathBuf,
    },
    /// `.stemma` has no file `format`.
    MissingFormat {
        /// The file that is missing.
        path: PathBuf,
    },
    /// The file `format` is not in canonical form.
    MalformedFormat {
        /// The file.
        path: PathBuf,
    },
    /// The file `format` names another version than this code supports.
    UnsupportedFormat {
        /// The file.
        path: PathBuf,
        /// The version that the file names.
        found: u32,
    },
    /// The storage failed. The cause is the field `source`, and its message ends the message of
    /// this error. Use [`OpenError::io_error`] to reach it.
    Io {
        /// What was being done, for example "read the format file".
        operation: &'static str,
        /// The file or directory involved.
        path: PathBuf,
        /// The error of the file system.
        source: io::Error,
    },
}

impl OpenError {
    fn io<'a>(operation: &'static str, path: &'a Path) -> impl FnOnce(io::Error) -> OpenError + 'a {
        move |source| OpenError::Io {
            operation,
            path: path.to_path_buf(),
            source,
        }
    }

    /// Returns the error of the file system if this is [`OpenError::Io`]. The chain of
    /// [`Error::source`] does not lead to it (see the crate documentation).
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::NotARepository { .. }
            | Self::MissingFormat { .. }
            | Self::MalformedFormat { .. }
            | Self::UnsupportedFormat { .. } => None,
        }
    }
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotARepository { path } => {
                write!(
                    f,
                    "`{}` is not a repository: there is no `{METADATA_DIR}`",
                    path.display()
                )
            }
            Self::MissingFormat { path } => {
                write!(f, "the repository has no format file `{}`", path.display())
            }
            Self::MalformedFormat { path } => {
                write!(
                    f,
                    "the format file `{}` is not in canonical form",
                    path.display()
                )
            }
            Self::UnsupportedFormat { path, found } => write!(
                f,
                "the format file `{}` names version {found}, but this code supports version \
                 {FORMAT_VERSION}",
                path.display()
            ),
            Self::Io {
                operation,
                path,
                source,
            } => write!(f, "could not {operation} `{}`: {source}", path.display()),
        }
    }
}

impl Error for OpenError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            // The message includes that of the I/O error, so the source is its own cause.
            Self::Io { source, .. } => source.source(),
            Self::NotARepository { .. }
            | Self::MissingFormat { .. }
            | Self::MalformedFormat { .. }
            | Self::UnsupportedFormat { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refs::RefStore;
    use crate::testutil::TempDir;
    use crate::AllowAll;

    struct DenyAll;

    impl Authorizer for DenyAll {
        fn authorize(&self, _actor: &Actor, _operation: &str, _target: &str) -> Decision {
            Decision::Deny
        }
    }

    fn actor() -> Actor {
        Actor::human("daniel").unwrap()
    }

    fn init(path: &Path) -> Result<Repository, InitError> {
        Repository::init(path, &InitOptions::default(), &actor(), &AllowAll)
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn init_creates_the_layout() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let meta = dir.path().join(".stemma");
        assert_eq!(names(&meta), ["HEAD", "format", "objects", "refs", "tmp"]);
        assert_eq!(
            fs::read(meta.join("format")).unwrap(),
            b"stemma-repository 1\n"
        );
        assert_eq!(fs::read(meta.join("HEAD")).unwrap(), b"branch main\n");
        assert!(meta.join("refs").join("heads").is_dir());
        assert_eq!(repository.path(), dir.path());
        assert_eq!(repository.metadata_dir(), meta);
    }

    #[test]
    fn init_leaves_nothing_but_the_metadata_directory() {
        let dir = TempDir::new();
        init(dir.path()).unwrap();
        assert_eq!(names(dir.path()), [".stemma"]);
    }

    #[test]
    fn head_points_to_a_branch_that_does_not_exist_yet() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let main = BranchName::new("main").unwrap();
        assert_eq!(
            repository.refs().head().unwrap(),
            Head::Branch(main.clone())
        );
        assert_eq!(repository.refs().branch(&main).unwrap(), None);
        assert_eq!(repository.refs().resolve_head().unwrap(), None);
        assert!(repository.refs().branches().unwrap().is_empty());
    }

    #[test]
    fn init_uses_the_initial_branch() {
        let dir = TempDir::new();
        let options = InitOptions::new(BranchName::new("trunk/main").unwrap());
        let repository = Repository::init(dir.path(), &options, &actor(), &AllowAll).unwrap();
        assert_eq!(
            fs::read(dir.path().join(".stemma").join("HEAD")).unwrap(),
            b"branch trunk/main\n"
        );
        assert_eq!(
            repository.refs().head().unwrap(),
            Head::Branch(BranchName::new("trunk/main").unwrap())
        );
    }

    #[test]
    fn init_creates_a_missing_directory() {
        let dir = TempDir::new();
        let path = dir.path().join("a").join("b");
        init(&path).unwrap();
        assert!(path.join(".stemma").is_dir());
    }

    #[test]
    fn a_second_init_fails_and_changes_nothing() {
        let dir = TempDir::new();
        init(dir.path()).unwrap();
        let marker = dir.path().join(".stemma").join("marker");
        fs::write(&marker, b"keep").unwrap();
        let error = init(dir.path()).unwrap_err();
        assert!(matches!(error, InitError::AlreadyExists { .. }), "{error}");
        assert_eq!(fs::read(&marker).unwrap(), b"keep");
        assert_eq!(names(dir.path()), [".stemma"]);
    }

    #[test]
    fn init_refuses_an_existing_metadata_directory_even_if_empty() {
        let dir = TempDir::new();
        fs::create_dir(dir.path().join(".stemma")).unwrap();
        let error = init(dir.path()).unwrap_err();
        assert!(matches!(error, InitError::AlreadyExists { .. }), "{error}");
        assert_eq!(names(&dir.path().join(".stemma")), Vec::<String>::new());
    }

    #[test]
    fn init_fails_if_the_path_is_a_file() {
        let dir = TempDir::new();
        let file = dir.path().join("file");
        fs::write(&file, b"x").unwrap();
        let error = init(&file).unwrap_err();
        assert!(matches!(error, InitError::Io { .. }), "{error}");
        assert!(error.io_error().is_some());
        assert_eq!(fs::read(&file).unwrap(), b"x");
    }

    #[test]
    fn a_denied_init_creates_nothing() {
        let dir = TempDir::new();
        let path = dir.path().join("new");
        let error =
            Repository::init(&path, &InitOptions::default(), &actor(), &DenyAll).unwrap_err();
        assert!(matches!(error, InitError::Denied { .. }), "{error}");
        assert!(!path.exists());
    }

    #[test]
    fn init_asks_the_authorizer_with_the_operation_and_the_directory() {
        use std::cell::RefCell;
        struct Recorder(RefCell<Vec<(String, String)>>);
        impl Authorizer for Recorder {
            fn authorize(&self, _actor: &Actor, operation: &str, target: &str) -> Decision {
                self.0.borrow_mut().push((operation.into(), target.into()));
                Decision::Allow
            }
        }
        let dir = TempDir::new();
        let recorder = Recorder(RefCell::new(Vec::new()));
        Repository::init(dir.path(), &InitOptions::default(), &actor(), &recorder).unwrap();
        assert_eq!(
            *recorder.0.borrow(),
            [("init".to_string(), dir.path().display().to_string())]
        );
    }

    #[test]
    fn build_fails_if_its_directory_exists() {
        let dir = TempDir::new();
        let staging = dir.path().join("staging");
        fs::create_dir(&staging).unwrap();
        // `build` needs a directory that does not exist, so it fails at the first step.
        let error = build(&staging, &InitOptions::default()).unwrap_err();
        assert!(matches!(error, InitError::Io { .. }), "{error}");
    }

    #[test]
    fn open_returns_what_init_made() {
        let dir = TempDir::new();
        init(dir.path()).unwrap();
        let repository = Repository::open(dir.path()).unwrap();
        assert_eq!(repository.path(), dir.path());
        assert_eq!(repository.refs().resolve_head().unwrap(), None);
    }

    #[test]
    fn open_without_a_metadata_directory_fails() {
        let dir = TempDir::new();
        let error = Repository::open(dir.path()).unwrap_err();
        assert!(matches!(error, OpenError::NotARepository { .. }), "{error}");
        fs::write(dir.path().join(".stemma"), b"x").unwrap();
        let error = Repository::open(dir.path()).unwrap_err();
        assert!(matches!(error, OpenError::NotARepository { .. }), "{error}");
    }

    #[test]
    fn open_without_a_format_file_fails() {
        let dir = TempDir::new();
        fs::create_dir(dir.path().join(".stemma")).unwrap();
        let error = Repository::open(dir.path()).unwrap_err();
        assert!(matches!(error, OpenError::MissingFormat { .. }), "{error}");
    }

    #[test]
    fn open_rejects_another_version() {
        for version in [0, 2, 99, u32::MAX] {
            let dir = TempDir::new();
            init(dir.path()).unwrap();
            fs::write(
                dir.path().join(".stemma").join("format"),
                format!("stemma-repository {version}\n"),
            )
            .unwrap();
            let error = Repository::open(dir.path()).unwrap_err();
            assert!(
                matches!(error, OpenError::UnsupportedFormat { found, .. } if found == version),
                "{version}: {error}"
            );
        }
    }

    #[test]
    fn open_rejects_a_format_that_is_not_canonical() {
        let cases: [&[u8]; 11] = [
            b"",
            b"\n",
            b"stemma-repository 1",
            b"stemma-repository 1\n\n",
            b"stemma-repository 01\n",
            b"stemma-repository +1\n",
            b"stemma-repository  1\n",
            b"stemma-repository 1 \n",
            b"stemma-repository 4294967296\n",
            b"Stemma-repository 1\n",
            b"stemma-repository 1\r\n",
        ];
        for content in cases {
            assert_eq!(parse_format(content), None, "{content:?}");
            let dir = TempDir::new();
            init(dir.path()).unwrap();
            fs::write(dir.path().join(".stemma").join("format"), content).unwrap();
            let error = Repository::open(dir.path()).unwrap_err();
            assert!(
                matches!(error, OpenError::MalformedFormat { .. }),
                "{content:?}: {error}"
            );
        }
        assert_eq!(parse_format(&[0xff, 0xfe]), None);
    }

    #[test]
    fn the_format_that_init_writes_is_the_one_that_open_accepts() {
        assert_eq!(
            parse_format(format_content().as_bytes()),
            Some(FORMAT_VERSION)
        );
    }

    #[test]
    fn error_messages_are_complete_and_name_the_path() {
        let dir = TempDir::new();
        init(dir.path()).unwrap();
        let error = init(dir.path()).unwrap_err();
        assert!(error.to_string().contains(".stemma"), "{error}");
        assert!(error.source().is_none());

        let file = dir.path().join("file");
        fs::write(&file, b"x").unwrap();
        let error = init(&file).unwrap_err();
        let message = error.to_string();
        let cause = error.io_error().unwrap().to_string();
        assert!(message.ends_with(&cause), "{message}");
        // The chain shows the cause of the cause, never the cause itself.
        assert!(error
            .source()
            .is_none_or(|source| source.to_string() != cause));
    }
}
