//! A set of signed statements on disk (ADR-025, ADR-009).
//!
//! A clone keeps the statements that it made and, later, those that a fetch brought; a hub keeps
//! one set for every repository. Both are a [`StatementSet`]: a directory with one file for each
//! statement, named by the 64 hexadecimal digits of its hash, with the message of the statement
//! as its content ([`Statement::encode`]). In a clone the directory is `.stemma/statements/`
//! ([`STATEMENT_DIR`]); a hub passes its own.
//!
//! The set is no log: it has no order, no number, and no time. The order of the statements is
//! the one of whoever holds the set, the entries of the operation log in a clone (ADR-011) and
//! the lines of the hub on a hub ([`crate::evidence::Placed`]). A statement that nothing places
//! is in the set and counts for nothing.
//!
//! A statement is never replaced and never removed here. Writing one that the set holds already
//! changes nothing, so a writer that stopped between the statement and its entry can run again.

use std::error::Error;
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::fault;
use crate::protocol::statement::{Statement, StatementHash};
use crate::protocol::ParseError;
use crate::store::{create_temp, flush_directory, unique_temp_name};

/// The name of the directory of the statements in the metadata directory of a clone.
pub const STATEMENT_DIR: &str = "statements";

/// The longest message of a statement that the set holds, in bytes. The reader refuses a longer
/// file, so the writer refuses the statement before it writes anything.
pub const MAX_STATEMENT_LEN: usize = 64 * 1024;

/// A set of statements in a directory.
#[derive(Debug, Clone)]
pub struct StatementSet {
    dir: PathBuf,
    scratch: PathBuf,
}

impl StatementSet {
    /// The set in `dir`. A statement is written to a temporary file in `scratch` first and then
    /// moved into the set, so `scratch` is a directory on the same file system that the caller
    /// may write to while it writes the set: in a clone `.stemma/tmp/`, whose writers hold the
    /// lock of the references (ADR-011). Neither directory has to exist.
    pub fn new(dir: impl Into<PathBuf>, scratch: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            scratch: scratch.into(),
        }
    }

    /// The directory of the set.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The file of the statement with this hash.
    pub fn path_of(&self, hash: &StatementHash) -> PathBuf {
        self.dir.join(hash.hex())
    }

    /// Puts a statement into the set and returns its hash. A statement that the set holds
    /// already is left as it is. The file is flushed before it gets its name, and the directory
    /// after it, also where the set held the statement already.
    ///
    /// # Errors
    ///
    /// [`SetError::TooLarge`] for a message above [`MAX_STATEMENT_LEN`], before anything is
    /// written, [`SetError::Malformed`] and [`SetError::WrongName`] if the set holds another
    /// content under the name of the statement, and [`SetError::Io`] for a failure of the file
    /// system.
    pub fn put(&self, statement: &Statement) -> Result<StatementHash, SetError> {
        let hash = statement.hash();
        let bytes = statement.encode();
        if bytes.len() > MAX_STATEMENT_LEN {
            return Err(SetError::TooLarge { hash });
        }
        let io = |operation: &'static str, path: &Path| {
            let path = path.to_path_buf();
            move |source| SetError::Io {
                operation,
                path,
                source,
            }
        };
        if self.get(&hash)?.is_some() {
            // An earlier call may have stopped between the rename and the flush, and an entry
            // is about to name the statement, so the directory is flushed here as well.
            flush_directory(&self.dir).map_err(io("flush the directory", &self.dir))?;
            return Ok(hash);
        }
        for dir in [&self.dir, &self.scratch] {
            std::fs::create_dir_all(dir).map_err(io("create the directory", dir))?;
        }
        let (mut file, mut temp) =
            create_temp(&self.scratch, &mut unique_temp_name).map_err(|error| SetError::Io {
                operation: "create a temporary file for a statement in",
                path: self.scratch.clone(),
                source: io::Error::other(error.to_string()),
            })?;
        file.write_all(&bytes)
            .map_err(io("write the statement", &temp.path))?;
        file.sync_all()
            .map_err(io("flush the statement", &temp.path))?;
        drop(file);
        let path = self.path_of(&hash);
        fault::rename(&temp.path, &path).map_err(io("put the statement into the set", &path))?;
        temp.disarm();
        flush_directory(&self.dir).map_err(io("flush the directory", &self.dir))?;
        Ok(hash)
    }

    /// The statement with this hash, or `None` if the set does not hold it.
    ///
    /// # Errors
    ///
    /// [`SetError::Io`] if the file cannot be read, [`SetError::TooLarge`] for a file above
    /// [`MAX_STATEMENT_LEN`], [`SetError::Malformed`] for a file that the strict reader of a
    /// statement refuses, a signature that does not verify included, and
    /// [`SetError::WrongName`] for a statement with another hash than the name of its file.
    pub fn get(&self, hash: &StatementHash) -> Result<Option<Statement>, SetError> {
        let path = self.path_of(hash);
        let bytes = match fault::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(SetError::Io {
                    operation: "read the statement",
                    path,
                    source,
                })
            }
        };
        if bytes.len() > MAX_STATEMENT_LEN {
            return Err(SetError::TooLarge { hash: *hash });
        }
        let statement = Statement::parse(&bytes).map_err(|source| SetError::Malformed {
            hash: *hash,
            source,
        })?;
        let found = statement.hash();
        if found != *hash {
            return Err(SetError::WrongName { name: *hash, found });
        }
        Ok(Some(statement))
    }

    /// The hashes of the statements of the set, in ascending order. A set whose directory does
    /// not exist is empty. Names that start with a dot are skipped, as for the references,
    /// because file managers and editors leave such files.
    ///
    /// # Errors
    ///
    /// [`SetError::Io`] if the directory cannot be read, and [`SetError::UnexpectedFile`] for
    /// any other name that is not the 64 lowercase hexadecimal digits of a hash.
    pub fn hashes(&self) -> Result<Vec<StatementHash>, SetError> {
        let io = |source| SetError::Io {
            operation: "read the directory",
            path: self.dir.clone(),
            source,
        };
        let read = match fault::read_dir(&self.dir) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(io(error)),
        };
        let mut hashes = Vec::new();
        for item in read {
            let name = item.map_err(io)?.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            match format!("b3:{name}").parse::<StatementHash>() {
                Ok(hash) if hash.hex() == name => hashes.push(hash),
                _ => return Err(SetError::UnexpectedFile(name)),
            }
        }
        hashes.sort_unstable();
        Ok(hashes)
    }
}

/// Why a set of statements cannot be read or written.
#[derive(Debug)]
#[non_exhaustive]
pub enum SetError {
    /// The file system failed.
    Io {
        /// What was tried.
        operation: &'static str,
        /// Where.
        path: PathBuf,
        /// What failed.
        source: io::Error,
    },
    /// The message of the statement is longer than [`MAX_STATEMENT_LEN`].
    TooLarge {
        /// The hash that names it.
        hash: StatementHash,
    },
    /// The file is no statement that the strict reader accepts.
    Malformed {
        /// The hash that the name of the file stands for.
        hash: StatementHash,
        /// Why the reader refuses it.
        source: ParseError,
    },
    /// The statement in the file has another hash than the name of the file.
    WrongName {
        /// The hash that the name of the file stands for.
        name: StatementHash,
        /// The hash of the statement in it.
        found: StatementHash,
    },
    /// A name in the directory is not the hash of a statement.
    UnexpectedFile(String),
}

impl SetError {
    /// The error of the operating system, if the file system failed.
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl fmt::Display for SetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(f, "cannot {operation} {}: {source}", path.display()),
            Self::TooLarge { hash } => write!(
                f,
                "the statement {hash} is longer than {MAX_STATEMENT_LEN} bytes"
            ),
            Self::Malformed { hash, source } => {
                write!(
                    f,
                    "the file of the statement {hash} is no statement: {source}"
                )
            }
            Self::WrongName { name, found } => write!(
                f,
                "the file of the statement {name} holds the statement {found}"
            ),
            Self::UnexpectedFile(name) => write!(
                f,
                "{name:?} in the directory of the statements is no hash of a statement"
            ),
        }
    }
}

// Each message includes the message of its cause, so the source is the cause of the cause (see
// the crate documentation), and neither cause has one.
impl Error for SetError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::statement::tests::{example, example_unsigned};
    use crate::protocol::statement::StatementKind;
    use crate::testutil::TempDir;

    fn set(dir: &TempDir) -> StatementSet {
        StatementSet::new(dir.path().join(STATEMENT_DIR), dir.path().join("tmp"))
    }

    #[test]
    fn a_statement_is_a_file_named_by_its_hash_and_is_read_back() {
        let dir = TempDir::new();
        let set = set(&dir);
        assert!(
            set.hashes().unwrap().is_empty(),
            "no directory, no statement"
        );
        let mut written = Vec::new();
        for kind in StatementKind::ALL {
            let statement = example(kind);
            let hash = set.put(&statement).unwrap();
            assert_eq!(hash, statement.hash());
            assert_eq!(
                std::fs::read(set.dir().join(hash.hex())).unwrap(),
                statement.encode()
            );
            assert_eq!(set.get(&hash).unwrap(), Some(statement));
            written.push(hash);
        }
        let unsigned = example_unsigned();
        written.push(set.put(&unsigned).unwrap());
        written.sort_unstable();
        assert_eq!(set.hashes().unwrap(), written);
        assert!(
            std::fs::read_dir(dir.path().join("tmp"))
                .unwrap()
                .next()
                .is_none(),
            "the temporary file is moved, not copied"
        );
    }

    #[test]
    fn putting_a_statement_twice_changes_nothing_and_a_missing_one_is_none() {
        let dir = TempDir::new();
        let set = set(&dir);
        let statement = example(StatementKind::Opening);
        let hash = set.put(&statement).unwrap();
        let before = std::fs::metadata(set.path_of(&hash))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(set.put(&statement).unwrap(), hash);
        let after = std::fs::metadata(set.path_of(&hash))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(before, after);
        assert_eq!(set.hashes().unwrap(), [hash]);
        assert_eq!(
            set.get(&example(StatementKind::Closing).hash()).unwrap(),
            None
        );
    }

    #[test]
    fn the_reader_refuses_a_changed_file_a_wrong_name_and_a_foreign_name() {
        let dir = TempDir::new();
        let set = set(&dir);
        let opening = example(StatementKind::Opening);
        let closing = example(StatementKind::Closing);
        let hash = set.put(&opening).unwrap();
        // The statement of another hash under this name.
        std::fs::write(set.path_of(&hash), closing.encode()).unwrap();
        assert!(matches!(
            set.get(&hash),
            Err(SetError::WrongName { name, found }) if name == hash && found == closing.hash()
        ));
        // The writer does not write over what it cannot read.
        assert!(matches!(set.put(&opening), Err(SetError::WrongName { .. })));
        // A changed payload: the signature does not verify.
        let text = String::from_utf8(opening.encode()).unwrap();
        std::fs::write(set.path_of(&hash), text.replace("feat/login", "feat/logon")).unwrap();
        assert!(matches!(set.get(&hash), Err(SetError::Malformed { .. })));
        // A file above the limit is not parsed.
        std::fs::write(set.path_of(&hash), vec![b'x'; MAX_STATEMENT_LEN + 1]).unwrap();
        assert!(matches!(set.get(&hash), Err(SetError::TooLarge { .. })));
        // A name that is no hash, and a name that an editor leaves.
        std::fs::write(set.dir().join(".DS_Store"), "").unwrap();
        assert_eq!(set.hashes().unwrap(), [hash]);
        // No upper case here: a file system that folds the case would take it for the file of
        // the statement.
        let not_hex = hash.hex().replace(['0', '1', '2', '3'], "g");
        for name in ["notes", &not_hex, &hash.hex()[1..]] {
            let path = set.dir().join(name);
            std::fs::write(&path, "").unwrap();
            assert!(
                matches!(set.hashes(), Err(SetError::UnexpectedFile(found)) if found == *name),
                "{name}"
            );
            std::fs::remove_file(path).unwrap();
        }
    }
}
