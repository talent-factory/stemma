//! The journal of a purge: the file `.stemma/purge-pending` (ADR-014).
//!
//! The journal exists from the plan to the last step of a purge. It holds what `--resume` needs to
//! finish the purge and `--abort` to undo it, so that no step has to plan again from a store that
//! a step changed. Its form is strict text, as every file of the repository (ADR-005), in this
//! order:
//!
//! ```text
//! stemma-purge 1
//! blob <id>                       the blob that is purged
//! last <n>                        the number of the last entry of the log when the plan was made
//! rewrite <old> <new>             each rewritten commit, parents first
//! head <old> <new>                HEAD, if it moves (a commit; HEAD that names a branch stays)
//! working-copy <old> <new>        the working-copy commit, if it moves
//! branch <name> <old> <new>       each branch that moves, sorted by name
//! stubs <ranges>                  the entries that become stubs, as in the log (`4-5 7`, or `-`)
//! new <id>                        each object that the purge writes and the store did not have
//! delete commit <id>              each commit that is deleted, sorted
//! delete tree <id>                each tree that is deleted, sorted
//! ```
//!
//! The lines `new` name what `--abort` removes; an object that the store had before the purge
//! stays, even if the purge writes it too. The blob is deleted as well, after the trees. The IDs
//! are those of the removed objects, so the file exists only until the purge has finished.

use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use super::PurgePlan;
use crate::object::{BlobId, CommitId, ObjectId, TreeId};
use crate::oplog::{parse_ranges, ranges_text, Moved};
use crate::refs::{BranchName, Head, PURGE_JOURNAL};
use crate::store::{create_temp, sync_directory, unique_temp_name, StoreError};

/// The first line of the journal, with the version of its form.
const HEADER: &str = "stemma-purge 1";

/// The longest journal that is read: a journal names every rewritten commit and every deleted
/// object, so it is bounded only by the history. A larger file is damage.
const MAX_LEN: u64 = 1 << 30;

/// The most entries that a journal lists as stubs. The ranges of the line `stubs` are short, so a
/// few bytes can name up to [`MAX_NUMBER`](crate::oplog::MAX_NUMBER) entries, which the reader
/// would hold one by one. A plan that replaces more entries is refused
/// ([`PurgeError::TooManyEntries`](super::PurgeError::TooManyEntries)), so no purge writes a
/// journal that lists more, and a journal that does is damage.
pub(super) const MAX_STUBS: u64 = 1 << 24;

/// What the journal holds: the plan that the purge carries out, the last entry of the log before
/// it, and the objects that it writes and the store did not have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Journal {
    pub(super) plan: PurgePlan,
    pub(super) last: u64,
    pub(super) new: Vec<ObjectId>,
}

impl Journal {
    /// The canonical form (see the module documentation).
    pub(super) fn encode(&self) -> Vec<u8> {
        let plan = &self.plan;
        let mut out = format!("{HEADER}\nblob {}\nlast {}\n", plan.blob, self.last);
        for (old, new) in &plan.rewrites {
            out.push_str(&format!("rewrite {old} {new}\n"));
        }
        if let Some(head) = &plan.head {
            let (Some(Head::Detached(old)), Some(Head::Detached(new))) =
                (head.before(), head.after())
            else {
                unreachable!("a purge moves HEAD only from a commit to a commit");
            };
            out.push_str(&format!("head {old} {new}\n"));
        }
        if let Some((old, new)) = plan.working_copy.as_ref().and_then(values) {
            out.push_str(&format!("working-copy {old} {new}\n"));
        }
        for (name, moved) in &plan.branches {
            if let Some((old, new)) = values(moved) {
                out.push_str(&format!("branch {name} {old} {new}\n"));
            }
        }
        let stubs = ranges(&plan.entries);
        let stubs = if stubs.is_empty() {
            "-".to_owned()
        } else {
            ranges_text(&stubs)
        };
        out.push_str(&format!("stubs {stubs}\n"));
        for id in &self.new {
            out.push_str(&format!("new {id}\n"));
        }
        for commit in &plan.commits {
            out.push_str(&format!("delete commit {commit}\n"));
        }
        for tree in &plan.trees {
            out.push_str(&format!("delete tree {tree}\n"));
        }
        out.into_bytes()
    }

    /// Reads a journal as [`encode`](Self::encode) writes it, and nothing else: the input has to
    /// be exactly the encoding of what it holds, and the lists that are sorted have to be sorted.
    /// The stubs are entries that existed when the plan was made, so none is after `last`, and
    /// there are at most [`MAX_STUBS`]. Returns `None` for any other input.
    pub(super) fn parse(bytes: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(bytes).ok()?;
        let mut lines = text.strip_suffix('\n')?.split('\n');
        if lines.next()? != HEADER {
            return None;
        }
        let blob = BlobId::new(id(lines.next()?.strip_prefix("blob ")?)?);
        let last = u64::from_str(lines.next()?.strip_prefix("last ")?).ok()?;
        let mut plan = PurgePlan {
            blob,
            rewrites: Vec::new(),
            head: None,
            working_copy: None,
            branches: Vec::new(),
            entries: Vec::new(),
            commits: Vec::new(),
            trees: Vec::new(),
        };
        let mut new = Vec::new();
        for line in lines {
            let (key, rest) = line.split_once(' ')?;
            match key {
                "rewrite" => {
                    let (old, new) = pair(rest)?;
                    plan.rewrites.push((CommitId::new(old), CommitId::new(new)));
                }
                "head" => {
                    let (old, new) = pair(rest)?;
                    plan.head = Some(Moved::from_to(
                        Some(Head::Detached(CommitId::new(old))),
                        Some(Head::Detached(CommitId::new(new))),
                    ));
                }
                "working-copy" => {
                    let (old, new) = pair(rest)?;
                    plan.working_copy = Some(moved(old, new));
                }
                "branch" => {
                    let (name, rest) = rest.split_once(' ')?;
                    let (old, new) = pair(rest)?;
                    plan.branches
                        .push((BranchName::new(name).ok()?, moved(old, new)));
                }
                "stubs" if rest == "-" => {}
                "stubs" => add_stubs(&mut plan.entries, rest, last)?,
                "new" => new.push(id(rest)?),
                "delete" => match rest.split_once(' ')? {
                    ("commit", id_text) => plan.commits.push(CommitId::new(id(id_text)?)),
                    ("tree", id_text) => plan.trees.push(TreeId::new(id(id_text)?)),
                    _ => return None,
                },
                _ => return None,
            }
        }
        let sorted = plan.branches.is_sorted_by(|a, b| a.0 < b.0)
            && plan.commits.is_sorted_by(|a, b| a < b)
            && plan.trees.is_sorted_by(|a, b| a < b)
            && new.is_sorted_by(|a, b| a < b);
        let journal = Self { plan, last, new };
        (sorted && journal.encode() == bytes).then_some(journal)
    }

    /// The moves of the pointers, from the value before the purge to the one after it.
    pub(super) fn moves(&self) -> crate::refs::PurgeMoves {
        let plan = &self.plan;
        crate::refs::PurgeMoves {
            head: plan
                .head
                .as_ref()
                .and_then(|moved| match (moved.before()?, moved.after()?) {
                    (Head::Detached(old), Head::Detached(new)) => Some((*old, *new)),
                    _ => None,
                }),
            working_copy: plan.working_copy.as_ref().and_then(values),
            branches: plan
                .branches
                .iter()
                .filter_map(|(name, moved)| {
                    values(moved).map(|(old, new)| (name.clone(), old, new))
                })
                .collect(),
        }
    }
}

/// The two values of a move of a commit pointer, if it has both.
fn values(moved: &Moved<CommitId>) -> Option<(CommitId, CommitId)> {
    Some((*moved.before()?, *moved.after()?))
}

fn moved(old: ObjectId, new: ObjectId) -> Moved<CommitId> {
    Moved::from_to(Some(CommitId::new(old)), Some(CommitId::new(new)))
}

fn id(text: &str) -> Option<ObjectId> {
    ObjectId::from_str(text).ok()
}

fn pair(text: &str) -> Option<(ObjectId, ObjectId)> {
    let (old, new) = text.split_once(' ')?;
    Some((id(old)?, id(new)?))
}

/// Adds the numbers of a line `stubs` to those of the lines before. The ranges are checked before
/// they are expanded, so that a short line cannot make the reader hold more numbers than a purge
/// can list; the count includes the lines before, so several lines cannot add up past the limit
/// either. A second line is not canonical, but that is known only after all are read.
fn add_stubs(entries: &mut Vec<u64>, text: &str, last: u64) -> Option<()> {
    let ranges = parse_ranges(text)?;
    let count: u64 = ranges.iter().map(|(start, end)| end - start + 1).sum();
    let after_last = ranges.last().is_some_and(|(_, end)| *end > last);
    if after_last || entries.len() as u64 + count > MAX_STUBS {
        return None;
    }
    for (start, end) in ranges {
        entries.extend(start..=end);
    }
    Some(())
}

/// Folds ascending numbers into inclusive ranges that neither overlap nor touch.
fn ranges(numbers: &[u64]) -> Vec<(u64, u64)> {
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    for number in numbers {
        match ranges.last_mut() {
            Some((_, end)) if *end + 1 == *number => *end = *number,
            _ => ranges.push((*number, *number)),
        }
    }
    ranges
}

/// The check of the fuzz target `journal` (see `crate::fuzz`): an input that the reader accepts is
/// the encoding of what it read.
#[cfg(any(fuzzing, test))]
pub(crate) fn fuzz(input: &[u8]) {
    if let Some(journal) = Journal::parse(input) {
        assert_eq!(journal.encode(), input, "a second spelling was accepted");
    }
}

/// The path of the journal in the metadata directory.
pub(super) fn path(dir: &Path) -> PathBuf {
    dir.join(PURGE_JOURNAL)
}

/// Writes the journal: a temporary file in `tmp/`, a flush, a rename, and a flush of the
/// directory (ADR-014). The caller holds the lock and has seen that no journal exists.
///
/// # Errors
///
/// Returns [`JournalError::Io`] if the journal could not be put in place, and
/// [`JournalError::NotFlushed`] if it is in place but the directory could not be flushed.
pub(super) fn write(dir: &Path, journal: &Journal) -> Result<(), JournalError> {
    debug_assert_eq!(
        Journal::parse(&journal.encode()).as_ref(),
        Some(journal),
        "a journal reads back as it is written"
    );
    let temp_dir = dir.join("tmp");
    let (mut file, mut temp) =
        create_temp(&temp_dir, &mut unique_temp_name).map_err(from_store(&temp_dir))?;
    file.write_all(&journal.encode())
        .map_err(io_error("write the temporary file", &temp.path))?;
    file.sync_all()
        .map_err(io_error("flush the temporary file", &temp.path))?;
    drop(file);
    let path = path(dir);
    fs::rename(&temp.path, &path).map_err(io_error("move the journal into place at", &path))?;
    temp.disarm();
    flush(dir)
}

/// Reads the journal, or returns `None` if there is none.
///
/// # Errors
///
/// Returns [`JournalError::Io`] if the file cannot be read, and [`JournalError::Malformed`] if it
/// is not a journal in canonical form.
pub(super) fn read(dir: &Path) -> Result<Option<Journal>, JournalError> {
    let path = path(dir);
    let bytes = match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() && metadata.len() <= MAX_LEN => {
            fs::read(&path).map_err(io_error("read the journal", &path))?
        }
        Ok(_) => return Err(JournalError::Malformed { path }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error("inspect the journal", &path)(error)),
    };
    Journal::parse(&bytes)
        .map(Some)
        .ok_or(JournalError::Malformed { path })
}

/// Removes the journal and flushes the directory: the last step of a purge, and of an abort.
///
/// # Errors
///
/// Returns [`JournalError::Io`] if the file cannot be removed, and [`JournalError::NotFlushed`]
/// if it is gone but the directory could not be flushed.
pub(super) fn remove(dir: &Path) -> Result<(), JournalError> {
    let path = path(dir);
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error("remove the journal", &path)(error)),
    }
    flush(dir)
}

fn flush(dir: &Path) -> Result<(), JournalError> {
    sync_directory(dir).map_err(|error| match error {
        StoreError::Io { source, .. } => JournalError::NotFlushed {
            path: dir.to_path_buf(),
            source,
        },
        other => JournalError::Io {
            operation: "flush the directory",
            path: dir.to_path_buf(),
            source: io::Error::other(other.to_string()),
        },
    })
}

/// Turns an error of the helpers that the object store shares into an error of the journal.
/// They fail only in the file system; another error keeps the directory that was used.
fn from_store(dir: &Path) -> impl FnOnce(StoreError) -> JournalError + '_ {
    move |error| match error {
        StoreError::Io {
            operation,
            path,
            source,
        } => JournalError::Io {
            operation,
            path,
            source,
        },
        other => JournalError::Io {
            operation: "create the temporary file in",
            path: dir.to_path_buf(),
            source: io::Error::other(other.to_string()),
        },
    }
}

fn io_error<'a>(
    operation: &'static str,
    path: &'a Path,
) -> impl FnOnce(io::Error) -> JournalError + 'a {
    move |source| JournalError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

/// Why the journal of a purge cannot be written, read, or removed.
#[derive(Debug)]
#[non_exhaustive]
pub enum JournalError {
    /// The file system failed.
    Io {
        /// What was being done, for example "read the journal".
        operation: &'static str,
        /// The file or directory involved.
        path: PathBuf,
        /// The error of the file system. Its message ends the message of this error.
        source: io::Error,
    },
    /// The change of the journal was made, but the directory could not be flushed to disk, so it
    /// may not survive a crash.
    NotFlushed {
        /// The directory.
        path: PathBuf,
        /// The error of the file system.
        source: io::Error,
    },
    /// The file is not a journal in canonical form.
    Malformed {
        /// The file.
        path: PathBuf,
    },
}

impl JournalError {
    /// Whether the change of the journal was made: a journal that was written is in place, and
    /// one that was removed is gone.
    pub(super) fn made(&self) -> bool {
        matches!(self, Self::NotFlushed { .. })
    }
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(f, "could not {operation} `{}`: {source}", path.display()),
            Self::NotFlushed { path, source } => write!(
                f,
                "the journal was changed, but the directory `{}` could not be flushed to disk: \
                 {source}",
                path.display()
            ),
            Self::Malformed { path } => write!(
                f,
                "`{}` is not the journal of a purge in canonical form",
                path.display()
            ),
        }
    }
}

impl Error for JournalError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            // The message has the message of the cause, so the source is the cause of the cause.
            Self::Io { source, .. } | Self::NotFlushed { source, .. } => source.source(),
            Self::Malformed { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::Blob;
    use crate::oplog::MAX_NUMBER;
    use crate::properties::{arbitrary_input, cases, random_blob, random_branch, seed, Rng, Tally};
    use crate::testutil::TempDir;

    fn commit(text: &str) -> CommitId {
        CommitId::new(Blob::new(text).id().object_id())
    }

    fn sample() -> Journal {
        let mut commits = vec![commit("a"), commit("b")];
        commits.sort();
        let mut trees = vec![TreeId::new(Blob::new("t").id().object_id())];
        trees.sort();
        let mut new = vec![commit("c").object_id(), commit("d").object_id()];
        new.sort();
        Journal {
            plan: PurgePlan {
                blob: Blob::new("key=hunter2").id(),
                rewrites: vec![(commit("a"), commit("c")), (commit("b"), commit("d"))],
                head: Some(Moved::from_to(
                    Some(Head::Detached(commit("a"))),
                    Some(Head::Detached(commit("c"))),
                )),
                working_copy: Some(Moved::from_to(Some(commit("b")), Some(commit("d")))),
                branches: vec![(
                    BranchName::new("main").unwrap(),
                    Moved::from_to(Some(commit("a")), Some(commit("c"))),
                )],
                entries: vec![3, 4, 5, 7],
                commits,
                trees,
            },
            last: 9,
            new,
        }
    }

    #[test]
    fn a_journal_reads_back_as_it_was_written() {
        let journal = sample();
        let text = String::from_utf8(journal.encode()).unwrap();
        assert!(text.starts_with("stemma-purge 1\nblob b3:"));
        assert!(text.contains("\nlast 9\n"));
        assert!(text.contains("\nstubs 3-5 7\n"));
        assert_eq!(Journal::parse(text.as_bytes()), Some(journal));
    }

    #[test]
    fn a_journal_without_moves_and_stubs_reads_back() {
        let mut journal = sample();
        journal.plan.rewrites.clear();
        journal.plan.head = None;
        journal.plan.working_copy = None;
        journal.plan.branches.clear();
        journal.plan.entries.clear();
        journal.plan.commits.clear();
        journal.plan.trees.clear();
        journal.new.clear();
        let bytes = journal.encode();
        assert!(String::from_utf8(bytes.clone())
            .unwrap()
            .ends_with("\nstubs -\n"));
        assert_eq!(Journal::parse(&bytes), Some(journal));
    }

    #[test]
    fn a_journal_that_is_not_canonical_is_refused() {
        let text = String::from_utf8(sample().encode()).unwrap();
        let cases = [
            text.replace("stemma-purge 1", "stemma-purge 2"),
            text.replace("last 9", "last 09"),
            text.trim_end().to_owned(),
            text.replace("stubs 3-5 7", "stubs 3 4-5 7"),
            format!("{text}unknown line\n"),
            // The groups in another order.
            text.replacen("stubs 3-5 7\n", "", 1).replacen(
                "delete commit",
                "stubs 3-5 7\ndelete commit",
                1,
            ),
        ];
        for case in cases {
            assert_eq!(Journal::parse(case.as_bytes()), None, "{case}");
        }
        // The deleted commits out of order.
        let mut journal = sample();
        journal.plan.commits.reverse();
        assert_eq!(Journal::parse(&journal.encode()), None);
    }

    /// A line of a few bytes names a range of up to a trillion entries. The reader has to refuse
    /// it before it expands the range, and not run out of memory (found by the property test).
    #[test]
    fn a_journal_with_too_many_stubs_or_stubs_after_the_last_entry_is_refused() {
        let text = String::from_utf8(sample().encode()).unwrap();
        let cases = [
            // After `last 9`.
            text.replace("stubs 3-5 7", "stubs 3-5 10"),
            text.replace("stubs 3-5 7", "stubs 1-999999999999"),
            text.replace("last 9", &format!("last {MAX_NUMBER}"))
                .replace("stubs 3-5 7", &format!("stubs 1-{MAX_NUMBER}")),
            text.replace("last 9", &format!("last {}", MAX_STUBS + 1))
                .replace("stubs 3-5 7", &format!("stubs 1-{}", MAX_STUBS + 1)),
        ];
        for case in cases {
            assert_eq!(Journal::parse(case.as_bytes()), None, "{case}");
        }
        // Two lines, each under the limit and together over it. The check of the canonical form
        // refuses the journal anyway, but only after the lines are read, so the second line has
        // to be refused before it is expanded.
        let mut entries = Vec::new();
        add_stubs(&mut entries, &format!("1-{}", MAX_STUBS - 1), 2 * MAX_STUBS).unwrap();
        let too_many = format!("{} {}", MAX_STUBS + 1, 2 * MAX_STUBS);
        assert_eq!(add_stubs(&mut entries, &too_many, 2 * MAX_STUBS), None);
        assert_eq!(
            entries.len() as u64,
            MAX_STUBS - 1,
            "nothing of the second line is added"
        );
        // A stub may be the last entry itself.
        let at_last = text.replace("stubs 3-5 7", "stubs 3-5 9");
        let read = Journal::parse(at_last.as_bytes()).expect("a stub at the last entry");
        assert_eq!(read.plan.entries, [3, 4, 5, 9]);
        let mut journal = sample();
        journal.last = MAX_STUBS + 9;
        journal.plan.entries = vec![2, 4];
        let mut text = String::from_utf8(journal.encode()).unwrap();
        text = text.replace("stubs 2 4", &format!("stubs 1-{MAX_STUBS}"));
        let read = Journal::parse(text.as_bytes()).expect("the most stubs that a journal can list");
        assert_eq!(read.plan.entries.len() as u64, MAX_STUBS);
    }

    #[test]
    fn a_journal_is_written_read_and_removed() {
        let dir = TempDir::new();
        fs::create_dir_all(dir.path().join("tmp")).unwrap();
        assert!(read(dir.path()).unwrap().is_none());
        write(dir.path(), &sample()).unwrap();
        assert_eq!(read(dir.path()).unwrap(), Some(sample()));
        remove(dir.path()).unwrap();
        assert!(read(dir.path()).unwrap().is_none());
        remove(dir.path()).unwrap();
        let tmp: Vec<_> = fs::read_dir(dir.path().join("tmp")).unwrap().collect();
        assert!(tmp.is_empty(), "no temporary file stays");
    }

    #[test]
    fn a_damaged_journal_is_an_error_that_names_it() {
        let dir = TempDir::new();
        fs::write(path(dir.path()), "junk").unwrap();
        let error = read(dir.path()).unwrap_err();
        assert!(matches!(error, JournalError::Malformed { .. }), "{error:?}");
        assert!(error.to_string().contains("purge-pending"));
    }

    fn random_id(rng: &mut Rng) -> ObjectId {
        ObjectId::from(random_blob(rng))
    }

    fn random_commit(rng: &mut Rng) -> CommitId {
        CommitId::new(random_id(rng))
    }

    /// Up to `max` values in ascending order and without repeats, as the journal keeps its sorted
    /// lists.
    fn sorted<T: Ord>(rng: &mut Rng, max: usize, mut value: impl FnMut(&mut Rng) -> T) -> Vec<T> {
        let mut values: Vec<T> = (0..rng.below(max + 1)).map(|_| value(rng)).collect();
        values.sort();
        values.dedup();
        values
    }

    /// A journal of any shape that a purge can write: any of the optional moves and lists, the
    /// numbers of the stubs as one range, several, or none, and numbers at the limit.
    fn random_journal(rng: &mut Rng) -> Journal {
        let rewrites = (0..rng.below(4))
            .map(|_| (random_commit(rng), random_commit(rng)))
            .collect();
        let head = rng.one_in(2).then(|| {
            Moved::from_to(
                Some(Head::Detached(random_commit(rng))),
                Some(Head::Detached(random_commit(rng))),
            )
        });
        let working_copy = rng
            .one_in(2)
            .then(|| Moved::from_to(Some(random_commit(rng)), Some(random_commit(rng))));
        let mut branches: Vec<(BranchName, Moved<CommitId>)> = Vec::new();
        for name in sorted(rng, 3, random_branch) {
            let moved = Moved::from_to(Some(random_commit(rng)), Some(random_commit(rng)));
            branches.push((name, moved));
        }
        let last = *rng.pick(&[0, 1, 9, 40, MAX_NUMBER - 1, MAX_NUMBER]);
        // The stubs are entries of the log up to `last`.
        let entries = sorted(rng, 6, |rng| match rng.below(3) {
            0 => *rng.pick(&[1, 2, 3, 5, 6, 40]),
            1 => last.saturating_sub(rng.below(3) as u64),
            _ => 1 + rng.next() % last.max(1),
        })
        .into_iter()
        .filter(|number| (1..=last).contains(number))
        .collect();
        let plan = PurgePlan {
            blob: random_blob(rng),
            rewrites,
            head,
            working_copy,
            branches,
            entries,
            commits: sorted(rng, 3, random_commit),
            trees: sorted(rng, 3, |rng| TreeId::new(random_id(rng))),
        };
        Journal {
            plan,
            last,
            new: sorted(rng, 3, random_id),
        }
    }

    /// The seeds of the fuzz target `journal` are journals, and the check holds for each prefix.
    #[test]
    fn every_seed_of_the_fuzz_target_is_a_journal() {
        for (name, seed) in crate::fuzz::tests::seeds("journal") {
            assert!(Journal::parse(&seed).is_some(), "{name} is not a journal");
            for end in 0..=seed.len() {
                fuzz(&seed[..end]);
            }
        }
    }

    #[test]
    fn every_journal_the_code_can_build_is_read_back() {
        let (seed, mut rng) = (seed(), Rng::new(seed(), 22));
        let mut shapes = [false; 5];
        for case in 0..cases() {
            let context = format!("seed {seed}, case {case}");
            let journal = random_journal(&mut rng);
            let entries = &journal.plan.entries;
            for (seen, now) in shapes.iter_mut().zip([
                journal.plan.head.is_some() && journal.plan.working_copy.is_some(),
                entries.is_empty(),
                entries.len() > 1,
                entries.last() == Some(&journal.last),
                journal.last == MAX_NUMBER,
            ]) {
                *seen |= now;
            }
            let bytes = journal.encode();
            assert_eq!(
                Journal::parse(&bytes).as_ref(),
                Some(&journal),
                "{context}: {:?}",
                String::from_utf8_lossy(&bytes)
            );
        }
        // Both moves, no stubs, several stubs, a stub at the last entry, and the largest number;
        // a short run, with few cases, may miss one.
        if cases() < 1000 {
            return;
        }
        assert_eq!(
            shapes, [true; 5],
            "the run did not build every shape of journal"
        );
    }

    /// The rule of ADR-005 on generated input (see `properties.rs`): every input that the reader
    /// accepts is the encoding of what it read, and no input makes it panic. A rejection has no
    /// message to check, as the reader returns `None` and the caller names the file.
    #[test]
    fn the_journal_reader_accepts_only_what_the_encoder_writes_and_never_panics() {
        let (seed, mut rng) = (seed(), Rng::new(seed(), 23));
        let valid: Vec<Vec<u8>> = (0..64).map(|_| random_journal(&mut rng).encode()).collect();
        let mut tally = Tally::default();
        for case in 0..cases() * 5 {
            let context = format!("seed {seed}, case {case}");
            let input = arbitrary_input(&mut rng, &valid);
            match Journal::parse(&input) {
                Some(journal) => {
                    tally.accepted += 1;
                    assert_eq!(
                        journal.encode(),
                        input,
                        "{context}: a second spelling was accepted: {:?}",
                        String::from_utf8_lossy(&input)
                    );
                }
                None => tally.rejected += 1,
            }
        }
        assert!(tally.accepted > 0 && tally.rejected > 0, "vacuous run");
    }
}
