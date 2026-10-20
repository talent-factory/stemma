//! Arguments, the actor, and the commands.
//!
//! Exit codes: 0 for success, 1 for a command that fails and has changed nothing, 2 for arguments
//! that are wrong (the code of the argument parser), and 3 for a command that has done its work
//! but could not finish: the repository or the change exists, but flushing it to disk or writing
//! the result failed, or a `commit` moved one of its two pointers and not the other. A script must
//! not remove or repeat what such a command made; only the next `commit` after a half-made one
//! finishes it, and the message of that call is not used. A command that only reads (`op log`)
//! has made nothing, so a failed write of its output is code 1; some of the output may already
//! be written, and a script discards it. `status` only reads as well, but it saves the tracked
//! files first (see the traces below); a snapshot that moved the pointer and could not flush it
//! is code 3 after the status is written, and so is a failure of reading after a snapshot that
//! was made.
//!
//! Code 1 can leave two traces. A write that fails after its entry is in the operation log
//! (`track`, `commit`, `UpdateError::MoveFailed`) leaves an entry for a move that did not happen,
//! which `op log` marks as such. And `commit` saves the tracked files first, as an entry of its
//! own (`snapshot`) that stays when the commit is then refused for nothing to commit or for a
//! branch that moved; repeating the command is safe. `status` makes the same entry if a tracked
//! file changed, also when it then fails to write its output. A snapshot that moved the pointer and could
//! not flush it is code 3, as for `track`; the commit was not made and the command can be
//! repeated.
//!
//! A failing command prints `stemma: <message>`, a complete message (see the crate documentation
//! of `stemma-core`). The argument parser prints its own errors.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::io::{self, Write};
use std::num::NonZeroUsize;
use std::path::{self, Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

use crate::op_log;
use stemma_core::object::METADATA_DIR;
use stemma_core::oplog::{self, OpLogError};
use stemma_core::refs::{BranchName, RefError};
use stemma_core::repository::{
    normalize_path, CommitError, Committed, InitError, InitOptions, OpenError, Repository,
    SnapshotError, Status, StatusError, TrackError, Tracking, UpdateError,
};
use stemma_core::{Actor, ActorParseError, AllowAll};

/// The name of the environment variable that holds the actor.
const ACTOR_ENV: &str = "STEMMA_ACTOR";

#[derive(Debug, Parser)]
#[command(
    name = "stemma",
    version,
    about = "A version control system for humans and agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create an empty repository
    Init(InitArgs),
    /// Track files: from now on a snapshot saves them
    Track(TrackArgs),
    /// Name the working-copy commit and start a new one
    Commit(CommitArgs),
    /// Show what the working copy changes and which files are not tracked
    Status(StatusArgs),
    /// Show the operation log
    Op {
        #[command(subcommand)]
        command: OpCommand,
    },
}

#[derive(Debug, Subcommand)]
enum OpCommand {
    /// List the entries of the operation log, newest first
    Log(OpLogArgs),
}

#[derive(Debug, Args)]
struct OpLogArgs {
    /// The directory of the repository, the one that contains `.stemma`
    #[arg(default_value = ".")]
    path: PathBuf,

    /// The number of entries to show; only their files are read, but all file names are listed
    #[arg(short = 'n', long, value_name = "N", default_value = "20")]
    limit: NonZeroUsize,

    /// Write one JSON object per line instead of text
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct TrackArgs {
    /// The files and directories to track, relative to the repository; a directory stands for
    /// the files below it, and `.` for the whole directory of the repository
    #[arg(required = true, value_name = "PATH")]
    paths: Vec<String>,

    /// The directory of the repository, the one that contains `.stemma`; the paths are relative
    /// to it
    #[arg(
        short = 'C',
        long = "repository",
        value_name = "DIR",
        default_value = "."
    )]
    repository: PathBuf,

    /// Who runs the command: `human <id>` or
    /// `agent <id> model=<model> session=<session> sponsor=<sponsor>`
    #[arg(long, env = ACTOR_ENV, value_name = "ACTOR")]
    actor: Option<String>,
}

#[derive(Debug, Args)]
struct CommitArgs {
    /// The message of the commit; it must have a character that is not white space
    #[arg(short = 'm', long, value_name = "MESSAGE")]
    message: String,

    /// The directory of the repository, the one that contains `.stemma`
    #[arg(
        short = 'C',
        long = "repository",
        value_name = "DIR",
        default_value = "."
    )]
    repository: PathBuf,

    /// Who runs the command: `human <id>` or
    /// `agent <id> model=<model> session=<session> sponsor=<sponsor>`
    #[arg(long, env = ACTOR_ENV, value_name = "ACTOR")]
    actor: Option<String>,
}

#[derive(Debug, Args)]
struct StatusArgs {
    /// The directory of the repository, the one that contains `.stemma`
    #[arg(
        short = 'C',
        long = "repository",
        value_name = "DIR",
        default_value = "."
    )]
    repository: PathBuf,

    /// Who runs the command: `human <id>` or
    /// `agent <id> model=<model> session=<session> sponsor=<sponsor>`; the snapshot that the
    /// command takes is made by this actor
    #[arg(long, env = ACTOR_ENV, value_name = "ACTOR")]
    actor: Option<String>,
}

#[derive(Debug, Args)]
struct InitArgs {
    /// The directory of the repository; it is created if it does not exist
    #[arg(default_value = ".")]
    path: PathBuf,

    /// The branch that HEAD points to; the branch exists after the first commit
    #[arg(long, value_name = "NAME", default_value = "main")]
    initial_branch: BranchName,

    /// Who runs the command: `human <id>` or
    /// `agent <id> model=<model> session=<session> sponsor=<sponsor>`
    #[arg(long, env = ACTOR_ENV, value_name = "ACTOR")]
    actor: Option<String>,
}

/// Why a command fails.
#[derive(Debug)]
enum CliError {
    /// There is no actor, neither as a flag nor in the environment.
    MissingActor,
    /// The flag or the variable is set, but empty. An empty flag does not fall back to the
    /// variable.
    EmptyActor,
    /// The text of the actor is not a valid actor line.
    Actor(ActorParseError),
    /// The core refused or could not create the repository, or created it and could not make it
    /// durable ([`InitError::NotDurable`]).
    Init(InitError),
    /// The repository cannot be opened.
    Open(OpenError),
    /// The core refused the paths or could not track them. Nothing is tracked, except for
    /// [`TrackError::NotDurable`]; a failed move leaves an entry in the log that names a move that
    /// did not happen (`UpdateError::MoveFailed`).
    Track(TrackError),
    /// The files are tracked, but the result could not be written to the output.
    TrackOutput(io::Error),
    /// The core refused the commit or could not make it. Nothing is committed, except for what
    /// [`CommitError::pointer_moved`] says; the snapshot before it may have made an entry.
    Commit(CommitError),
    /// The commit is made, but a directory could not be flushed (`Committed::late`).
    CommitNotDurable {
        /// The commit and the branch, as the output would have named them.
        committed: String,
        /// The error of the flush.
        source: Box<RefError>,
        /// The error of the output, if the result could not be written either.
        output: Option<io::Error>,
    },
    /// The commit is made, but the result could not be written to the output.
    CommitOutput {
        /// The commit and the branch, as the output would have named them.
        committed: String,
        /// The error of the output.
        source: io::Error,
    },
    /// The core could not report the status. If the tracked files were saved first, the error
    /// carries the entry, and the saving stays (`StatusError::pointer_moved`).
    Status(StatusError),
    /// The status was reported, but the snapshot that came first could not be flushed to disk.
    StatusNotDurable {
        /// The error of the flush.
        source: Box<RefError>,
        /// The error of the output, if the status could not be written either.
        output: Option<io::Error>,
    },
    /// The status could not be written to the output.
    StatusOutput(io::Error),
    /// The operation log cannot be read.
    OpLog(OpLogError),
    /// The operation log has no entry at all. `init` writes entry 1, so entries were lost or the
    /// repository was tampered with.
    EmptyLog,
    /// The entries were read, but the output could not be written.
    Write {
        /// The number of the entry that was being written.
        entry: u64,
        /// The error of the output.
        source: io::Error,
    },
    /// The command has done its work, but the result could not be written to the output.
    Output {
        /// What the command made.
        created: PathBuf,
        /// The error of the output.
        source: io::Error,
    },
}

impl CliError {
    /// The exit code of the error (see the module documentation).
    fn exit_code(&self) -> ExitCode {
        match self {
            Self::Init(InitError::NotDurable { .. })
            | Self::Track(TrackError::NotDurable { .. })
            | Self::Output { .. }
            | Self::TrackOutput(_)
            | Self::CommitNotDurable { .. }
            | Self::CommitOutput { .. }
            | Self::StatusNotDurable { .. } => ExitCode::from(3),
            Self::Commit(error) if pointer_moved(error) => ExitCode::from(3),
            Self::Status(error) if error.pointer_moved() => ExitCode::from(3),
            Self::MissingActor
            | Self::EmptyActor
            | Self::Actor(_)
            | Self::Init(_)
            | Self::Open(_)
            | Self::Track(_)
            | Self::Commit(_)
            | Self::Status(_)
            | Self::StatusOutput(_)
            | Self::OpLog(_)
            | Self::EmptyLog
            | Self::Write { .. } => ExitCode::FAILURE,
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingActor => write!(
                f,
                "no actor: pass `--actor \"human <id>\"` or set {ACTOR_ENV}; an agent is \
                 `agent <id> model=<model> session=<session> sponsor=<sponsor>`"
            ),
            Self::EmptyActor => write!(
                f,
                "the actor is empty: `--actor` or {ACTOR_ENV} is set to an empty value; pass \
                 `human <id>` or `agent <id> model=<model> session=<session> sponsor=<sponsor>`"
            ),
            Self::Actor(error) => write!(f, "invalid actor: {error}"),
            Self::Init(error) => error.fmt(f),
            Self::Open(error @ OpenError::NotARepository { .. }) => write!(
                f,
                "{error}; stemma looks only in the directory it is given, not in its parents \
                 (see `-C` of `track`, `commit`, and `status`, and the argument of `op log`)"
            ),
            Self::Open(error) => error.fmt(f),
            Self::Track(error) => error.fmt(f),
            Self::TrackOutput(source) => write!(
                f,
                "tracked the files, but could not write the result: {source}"
            ),
            Self::Commit(CommitError::Snapshot(SnapshotError::Update(
                UpdateError::NotDurable { entry, source },
            ))) => write!(
                f,
                "saved the working copy as entry {} of the operation log, but flushing failed: \
                 {source}; the commit was not made, and `stemma commit` can be run again",
                entry.number()
            ),
            Self::Commit(error @ CommitError::Update(UpdateError::PartlyMoved { .. })) => write!(
                f,
                "{error}; `stemma commit` run again finishes the commit, and the message of that \
                 call is not used"
            ),
            Self::Commit(error) => error.fmt(f),
            Self::CommitNotDurable {
                committed,
                source,
                output,
            } => {
                write!(
                    f,
                    "committed {committed}, but flushing failed: {source}; the commit may not \
                     survive a crash, and it must not be repeated"
                )?;
                match output {
                    Some(error) => write!(f, "; the result could not be written either: {error}"),
                    None => Ok(()),
                }
            }
            Self::CommitOutput { committed, source } => write!(
                f,
                "committed {committed}, but could not write the result: {source}"
            ),
            Self::Status(error) => error.fmt(f),
            Self::StatusNotDurable { source, output } => {
                write!(
                    f,
                    "the tracked files were saved, but flushing failed: {source}; the saved \
                     state may not survive a crash"
                )?;
                match output {
                    Some(error) => write!(f, "; the status could not be written either: {error}"),
                    None => Ok(()),
                }
            }
            Self::StatusOutput(source) => write!(f, "could not write the status: {source}"),
            Self::OpLog(error) => error.fmt(f),
            Self::EmptyLog => write!(
                f,
                "the operation log is empty: `init` always writes entry 1, so entries were lost \
                 or the repository was tampered with"
            ),
            Self::Write { entry, source } => write!(
                f,
                "could not write entry {entry} to standard output: {source}"
            ),
            Self::Output { created, source } => write!(
                f,
                "created `{}`, but could not write the result: {source}",
                created.display()
            ),
        }
    }
}

impl Error for CliError {}

/// Runs the command and writes its result to `out`.
fn run(cli: Cli, out: &mut dyn Write) -> Result<(), CliError> {
    match cli.command {
        Command::Init(args) => init(args, out),
        Command::Track(args) => track(args, out),
        Command::Commit(args) => commit(args, out),
        Command::Status(args) => status(args, out),
        Command::Op {
            command: OpCommand::Log(args),
        } => show_op_log(args, out),
    }
}

/// Lists the newest entries. The names of all files are listed, which finds a gap or a foreign
/// file; only the files of the entries that are shown are read, so damage in an older entry goes
/// unnoticed. Everything is read before anything is written, so an entry that is damaged gives an
/// error and no partial list.
///
/// The values of HEAD and of the branches that the entries name are read after the entries. With
/// the chain of the entries they decide whether each move took effect (see `oplog::outcomes`). If
/// the references cannot be read, the command warns and shows what the chain decides. A writer
/// may be between its entry and its move while the references are read: the lock is looked at
/// before and after, and the log is listed again, and if anything shows a writer, a pointer that
/// still has its old value is not called a failed move.
fn show_op_log(args: OpLogArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let repository = Repository::open(&args.path).map_err(CliError::Open)?;
    let log = repository.oplog();
    let numbers = log.numbers().map_err(CliError::OpLog)?;
    if numbers.is_empty() {
        return Err(CliError::EmptyLog);
    }
    // The shown entries are the newest ones, so every later entry of a pointer is among them, and
    // only the last entry of a pointer needs its value now.
    let shown = &numbers[numbers.len().saturating_sub(args.limit.get())..];
    let entries = shown
        .iter()
        .map(|number| log.read(*number))
        .collect::<Result<Vec<_>, _>>()
        .map_err(CliError::OpLog)?;
    let names: BTreeSet<&BranchName> = entries
        .iter()
        .flat_map(|entry| entry.change().branches().iter().map(|(name, _)| name))
        .collect();
    let read = || -> Result<_, RefError> {
        let refs = repository.refs();
        let locked_before = refs.is_locked()?;
        let head = refs.head()?;
        // A missing file is not a pointer that does not exist: `init` records one, so the log
        // cannot say what the file held and the outcome of the working-copy entries stays unknown.
        let working_copy = match refs.working_copy() {
            Ok(commit) => Some(commit),
            Err(RefError::NoWorkingCopy { .. }) => None,
            Err(error) => return Err(error),
        };
        let mut branches = Vec::new();
        for name in &names {
            if let Some(commit) = refs.branch(name)? {
                branches.push(((*name).clone(), commit));
            }
        }
        let locked_after = refs.is_locked()?;
        Ok((head, working_copy, branches, locked_before || locked_after))
    };
    let current = match read() {
        Ok((head, working_copy, branches, locked)) => {
            let unchanged = log.numbers().map_err(CliError::OpLog)? == numbers;
            let current = oplog::Current::new(head, branches, !locked && unchanged);
            Some(match working_copy {
                Some(commit) => current.with_working_copy(Some(commit)),
                None => {
                    // A warning that cannot be written is dropped on purpose, as in `main`.
                    let _ = writeln!(
                        io::stderr(),
                        "stemma: warning: there is no working-copy commit, so the outcome of its \
                         moves is not known"
                    );
                    current
                }
            })
        }
        Err(error) => {
            // A warning that cannot be written is dropped on purpose, as in `main`.
            let _ = writeln!(
                io::stderr(),
                "stemma: warning: could not read the references, so the outcome of the newest \
                 entries is not known: {error}"
            );
            None
        }
    };
    let outcomes = oplog::outcomes(&entries, current.as_ref());
    for (entry, outcome) in entries.iter().zip(outcomes).rev() {
        let written = if args.json {
            writeln!(out, "{}", op_log::json(entry, outcome))
        } else {
            write!(out, "{}", op_log::text(entry, outcome))
        };
        match written {
            // The reader of a pipe, such as `head`, has had enough: that is not a failure.
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => break,
            Err(source) => {
                return Err(CliError::Write {
                    entry: entry.number(),
                    source,
                });
            }
            Ok(()) => {}
        }
    }
    Ok(())
}

/// Tracks the files and lists the ones that are new. `track` saves the tracked files as well, so
/// it is the snapshot that ADR-010 asks for before a command that changes something; `op log`
/// only reads and takes none.
fn track(args: TrackArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let actor = actor(args.actor)?;
    let repository = Repository::open(&args.repository).map_err(CliError::Open)?;
    let paths: Vec<String> = args.paths.iter().map(|path| normalize_path(path)).collect();
    finish_track(repository.track(&paths, &actor, &AllowAll), out)
}

/// Writes what `track` did. If the files are tracked but a directory could not be flushed, the
/// list is written first, like the announcement of `init`, so that a script finds what exists
/// before it reads the error. A closed pipe ends the output quietly, as it does for `op log`.
fn finish_track(result: Result<Tracking, TrackError>, out: &mut dyn Write) -> Result<(), CliError> {
    let (added, late) = match result {
        Ok(tracking) => (tracking.added, None),
        Err(TrackError::NotDurable { added, error }) => (
            added,
            Some(CliError::Track(TrackError::NotDurable {
                added: Vec::new(),
                error,
            })),
        ),
        Err(error) => return Err(CliError::Track(error)),
    };
    match out.write_all(track_text(&added).as_bytes()) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {}
        Err(source) => {
            return Err(late.unwrap_or(CliError::TrackOutput(source)));
        }
    }
    late.map_or(Ok(()), Err)
}

/// One line for each file that is tracked now and was not before, or one line that says there is
/// none. A name with a control character is written escaped, as in the other output.
fn track_text(added: &[String]) -> String {
    if added.is_empty() {
        return "No new files to track\n".to_owned();
    }
    added
        .iter()
        .map(|path| format!("tracked {}\n", path.escape_debug()))
        .collect()
}

/// Whether the failed `commit` moved a pointer: the error says so itself, or it is the snapshot
/// before the commit that moved the working-copy commit and could not flush it.
fn pointer_moved(error: &CommitError) -> bool {
    error.pointer_moved()
        || matches!(
            error,
            CommitError::Snapshot(SnapshotError::Update(update)) if update.pointer_moved()
        )
}

/// Names the working-copy commit and starts a new one. The core saves the tracked files first, so
/// a change since the last command is part of the commit.
fn commit(args: CommitArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let actor = actor(args.actor)?;
    let repository = Repository::open(&args.repository).map_err(CliError::Open)?;
    let result = repository
        .commit(&args.message, &actor, &AllowAll)
        .map_err(CliError::Commit)?;
    if result.resumed {
        // A warning that cannot be written is dropped on purpose, as in `main`.
        let _ = writeln!(
            io::stderr(),
            "stemma: warning: this call finished a commit that had stopped; its message was not \
             used, and what changed since the stop is not in that commit"
        );
    }
    finish_commit(result, out)
}

/// Writes what `commit` did. If a directory could not be flushed, the result is written first, so
/// that a script can read the commit ID from the output before it sees the error. A closed pipe
/// ends the output quietly.
fn finish_commit(result: Committed, out: &mut dyn Write) -> Result<(), CliError> {
    let text = commit_text(&result);
    let committed = format!("{} on {}", result.commit, result.branch);
    let late = result.late;
    match out.write_all(text.as_bytes()) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {}
        Err(source) => {
            return Err(match late {
                Some(late) => CliError::CommitNotDurable {
                    committed,
                    source: Box::new(late),
                    output: Some(source),
                },
                None => CliError::CommitOutput { committed, source },
            });
        }
    }
    match late {
        Some(source) => Err(CliError::CommitNotDurable {
            committed,
            source: Box::new(source),
            output: None,
        }),
        None => Ok(()),
    }
}

/// Reports what the working copy changes and which files are not tracked. The core saves the
/// tracked files first, so the status is of the working copy as it is now.
fn status(args: StatusArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let actor = actor(args.actor)?;
    let repository = Repository::open(&args.repository).map_err(CliError::Open)?;
    let result = repository
        .status(&actor, &AllowAll)
        .map_err(CliError::Status)?;
    finish_status(result, out)
}

/// Writes the status. If the snapshot could not be flushed, the status is written first and the
/// error comes after it. A closed pipe ends the output quietly, as for `op log`.
fn finish_status(status: Status, out: &mut dyn Write) -> Result<(), CliError> {
    let text = status_text(&status);
    let late = status.snapshot.and_then(|saved| saved.late);
    match out.write_all(text.as_bytes()) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {}
        Err(source) => {
            return Err(match late {
                Some(late) => CliError::StatusNotDurable {
                    source: Box::new(late),
                    output: Some(source),
                },
                None => CliError::StatusOutput(source),
            });
        }
    }
    match late {
        Some(source) => Err(CliError::StatusNotDurable {
            source: Box::new(source),
            output: None,
        }),
        None => Ok(()),
    }
}

/// The text of the status: the changes with their letters and the untracked files, each section
/// only if it has files, or one line if there are none. A name with a control character is written
/// escaped, as in the other output.
fn status_text(status: &Status) -> String {
    if status.changes.is_empty() && status.untracked.is_empty() {
        return "Working copy is clean\n".to_owned();
    }
    let mut text = String::new();
    if !status.changes.is_empty() {
        text.push_str("Working copy changes:\n");
        for change in &status.changes {
            text.push_str(&format!(
                "{} {}\n",
                change.kind.letter(),
                change.path.escape_debug()
            ));
        }
    }
    if !status.untracked.is_empty() {
        text.push_str("Untracked files:\n");
        for path in &status.untracked {
            text.push_str(&format!("? {}\n", path.escape_debug()));
        }
    }
    text
}

/// The one line that reports the commit: `Committed` for a new commit, `Finished the commit` if
/// the call finished an earlier commit that had stopped between its two moves.
fn commit_text(result: &Committed) -> String {
    if result.resumed {
        format!(
            "Finished the commit {} on {} that had stopped; the message of this call was not \
             used\n",
            result.commit, result.branch
        )
    } else {
        format!("Committed {} on {}\n", result.commit, result.branch)
    }
}

fn init(args: InitArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let actor = actor(args.actor)?;
    let options = InitOptions::new(args.initial_branch);
    let created = absolute(&args.path.join(METADATA_DIR));
    // A repository that is complete but not durable is announced like a success, so that a
    // script can find it, and the command ends with the error afterwards.
    let late = match Repository::init(&args.path, &options, &actor, &AllowAll) {
        Ok(_) => None,
        Err(error @ InitError::NotDurable { .. }) => Some(error),
        Err(error) => return Err(CliError::Init(error)),
    };
    writeln!(
        out,
        "Initialized empty Stemma repository in {}",
        created.display()
    )
    .map_err(|source| CliError::Output { created, source })?;
    late.map_or(Ok(()), |error| Err(CliError::Init(error)))
}

/// The path as an absolute one, so that a script can use it after it changes its working
/// directory. If the working directory cannot be read, the path as given is still a correct
/// answer.
fn absolute(path: &Path) -> PathBuf {
    path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

fn actor(text: Option<String>) -> Result<Actor, CliError> {
    match text.as_deref() {
        None => Err(CliError::MissingActor),
        Some("") => Err(CliError::EmptyActor),
        Some(line) => line.parse().map_err(CliError::Actor),
    }
}

pub fn main() -> ExitCode {
    // Help and version go to the output with code 0, a usage error to stderr with code 2.
    let cli = Cli::parse();
    match run(cli, &mut io::stdout().lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // The one result that is dropped on purpose: if stderr is closed, there is no channel
            // left to report on, and a panic would replace the exit code with 101.
            let _ = writeln!(io::stderr(), "stemma: {error}");
            error.exit_code()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stemma_core::repository::{SnapshotError, UpdateError};

    /// A repository made by `init` below the temp directory, and its first entry.
    fn repository_with_entry(name: &str) -> (PathBuf, stemma_core::oplog::Entry) {
        let dir =
            std::env::temp_dir().join(format!("stemma-cli-unit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let repository = Repository::init(
            &dir,
            &InitOptions::default(),
            &"human daniel".parse().unwrap(),
            &AllowAll,
        )
        .unwrap();
        let entry = repository.oplog().read(1).unwrap();
        (dir, entry)
    }

    #[test]
    fn a_commit_that_moved_a_pointer_ends_with_code_3_and_any_other_failure_with_code_1() {
        let (dir, entry) = repository_with_entry("commit-codes");
        let partly = CommitError::Update(UpdateError::PartlyMoved {
            entry: Box::new(entry),
            source: RefError::Locked { path: dir.clone() },
        });
        assert_eq!(
            CliError::Commit(partly).exit_code(),
            ExitCode::from(3),
            "one pointer moved: the commit is not repeated"
        );
        assert_eq!(
            CliError::Commit(CommitError::NothingToCommit).exit_code(),
            ExitCode::FAILURE
        );
        assert_eq!(
            CliError::CommitNotDurable {
                committed: "b3:x on main".to_owned(),
                source: Box::new(RefError::Locked { path: dir.clone() }),
                output: None,
            }
            .exit_code(),
            ExitCode::from(3)
        );
        assert_eq!(
            CliError::CommitOutput {
                committed: "b3:x on main".to_owned(),
                source: io::Error::other("x"),
            }
            .exit_code(),
            ExitCode::from(3)
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A writer that fails every write with an error of this kind.
    struct Failing(io::ErrorKind);

    impl Write for Failing {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(self.0))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A commit made in a new repository, and the repository's first entry.
    fn committed(name: &str) -> (PathBuf, Committed, stemma_core::oplog::Entry) {
        let (dir, entry) = repository_with_entry(name);
        let repository = Repository::open(&dir).unwrap();
        let actor: Actor = "human daniel".parse().unwrap();
        std::fs::write(dir.join("a.txt"), "one").unwrap();
        repository
            .track(&["a.txt".to_owned()], &actor, &AllowAll)
            .unwrap();
        let result = repository.commit("Add a", &actor, &AllowAll).unwrap();
        (dir, result, entry)
    }

    fn not_durable() -> RefError {
        RefError::NotFlushed {
            path: PathBuf::from("/x"),
            source: io::Error::other("disk"),
        }
    }

    #[test]
    fn a_commit_is_announced_with_the_commit_and_the_branch() {
        let (dir, result, _) = committed("commit-text");
        let id = result.commit;
        let mut out = Vec::new();
        finish_commit(result, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("Committed {id} on main\n")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_resumed_commit_says_so_in_one_line() {
        let (dir, mut result, _) = committed("commit-resumed");
        result.resumed = true;
        let id = result.commit;
        let text = commit_text(&result);
        assert_eq!(
            text,
            format!(
                "Finished the commit {id} on main that had stopped; the message of this call was \
                 not used\n"
            )
        );
        assert_eq!(text.lines().count(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_commit_that_is_not_durable_writes_the_result_first_and_ends_with_code_3() {
        let (dir, mut result, _) = committed("commit-late");
        result.late = Some(not_durable());
        let id = result.commit;
        let mut out = Vec::new();
        let error = finish_commit(result, &mut out).unwrap_err();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("Committed {id} on main\n")
        );
        assert_eq!(error.exit_code(), ExitCode::from(3));
        let text = error.to_string();
        assert!(
            text.contains(&id.to_string()) && text.contains("must not be repeated"),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_closed_pipe_ends_a_commit_quietly_but_keeps_the_late_error() {
        let (dir, result, _) = committed("commit-pipe");
        let mut closed = Failing(io::ErrorKind::BrokenPipe);
        assert!(finish_commit(result, &mut closed).is_ok());
        let (_, mut late, _) = committed("commit-pipe-late");
        late.late = Some(not_durable());
        let error = finish_commit(late, &mut closed).unwrap_err();
        assert!(
            matches!(&error, CliError::CommitNotDurable { output: None, .. }),
            "{error}"
        );
        assert_eq!(error.exit_code(), ExitCode::from(3));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_result_that_cannot_be_written_names_the_commit_and_ends_with_code_3() {
        let (dir, result, _) = committed("commit-output");
        let id = result.commit;
        let mut full = Failing(io::ErrorKind::StorageFull);
        let error = finish_commit(result, &mut full).unwrap_err();
        assert!(matches!(&error, CliError::CommitOutput { .. }), "{error}");
        assert_eq!(error.exit_code(), ExitCode::from(3));
        assert!(error.to_string().contains(&id.to_string()), "{error}");
        let (_, mut late, _) = committed("commit-output-late");
        late.late = Some(not_durable());
        let both = finish_commit(late, &mut full).unwrap_err();
        assert!(
            matches!(
                &both,
                CliError::CommitNotDurable {
                    output: Some(_),
                    ..
                }
            ),
            "{both}"
        );
        assert!(both.to_string().contains("either"), "{both}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The status of a repository that has a tracked file that changed, a deleted one, and an
    /// untracked one.
    fn a_status(name: &str) -> (PathBuf, Status) {
        let (dir, _) = repository_with_entry(name);
        let repository = Repository::open(&dir).unwrap();
        let actor: Actor = "human daniel".parse().unwrap();
        std::fs::write(dir.join("keep.txt"), "one").unwrap();
        std::fs::write(dir.join("gone.txt"), "gone").unwrap();
        repository
            .track(&[".".to_owned()], &actor, &AllowAll)
            .unwrap();
        let _ = repository.commit("base", &actor, &AllowAll).unwrap();
        std::fs::write(dir.join("keep.txt"), "two").unwrap();
        std::fs::remove_file(dir.join("gone.txt")).unwrap();
        std::fs::write(dir.join("new.txt"), "new").unwrap();
        repository
            .track(&["new.txt".to_owned()], &actor, &AllowAll)
            .unwrap();
        std::fs::write(dir.join("it's.txt"), "x").unwrap();
        let status = repository.status(&actor, &AllowAll).unwrap();
        (dir, status)
    }

    #[test]
    fn the_status_lists_the_changes_with_their_letters_and_the_untracked_files_escaped() {
        let (dir, status) = a_status("status-text");
        assert_eq!(
            status_text(&status),
            "Working copy changes:\nD gone.txt\nM keep.txt\nA new.txt\n\
             Untracked files:\n? it\\'s.txt\n"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_section_without_files_is_left_out_and_no_files_at_all_is_one_line() {
        let (dir, mut status) = a_status("status-sections");
        status.untracked.clear();
        assert_eq!(
            status_text(&status),
            "Working copy changes:\nD gone.txt\nM keep.txt\nA new.txt\n"
        );
        status.changes.clear();
        assert_eq!(status_text(&status), "Working copy is clean\n");
        status.untracked.push("only.txt".to_owned());
        assert_eq!(status_text(&status), "Untracked files:\n? only.txt\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A status whose snapshot moved the pointer and could not be flushed: the metadata directory
    /// may be written and entered, but not read. `None` where the permissions have no effect.
    #[cfg(unix)]
    fn a_status_not_durable(name: &str) -> Option<(PathBuf, Status)> {
        use std::os::unix::fs::PermissionsExt;
        let (dir, _) = repository_with_entry(name);
        let repository = Repository::open(&dir).unwrap();
        let actor: Actor = "human daniel".parse().unwrap();
        std::fs::write(dir.join("a.txt"), "one").unwrap();
        repository
            .track(&["a.txt".to_owned()], &actor, &AllowAll)
            .unwrap();
        let _ = repository.commit("base", &actor, &AllowAll).unwrap();
        std::fs::write(dir.join("a.txt"), "two").unwrap();
        let metadata = dir.join(".stemma");
        std::fs::set_permissions(&metadata, std::fs::Permissions::from_mode(0o300)).unwrap();
        let works = std::fs::read_dir(&metadata).is_ok();
        let result = repository.status(&actor, &AllowAll);
        std::fs::set_permissions(&metadata, std::fs::Permissions::from_mode(0o755)).unwrap();
        if works {
            return None;
        }
        Some((dir, result.unwrap()))
    }

    #[cfg(unix)]
    #[test]
    fn a_status_is_written_and_a_snapshot_that_is_not_durable_follows_with_code_3() {
        let Some((dir, status)) = a_status_not_durable("status-late") else {
            return;
        };
        assert!(status.late().is_some());
        let text = status_text(&status);
        let mut out = Vec::new();

        let error = finish_status(status, &mut out).unwrap_err();

        assert_eq!(String::from_utf8(out).unwrap(), text);
        assert!(text.contains("M a.txt"), "{text}");
        assert!(
            matches!(&error, CliError::StatusNotDurable { output: None, .. }),
            "{error}"
        );
        assert_eq!(error.exit_code(), ExitCode::from(3));
        assert!(
            error.to_string().contains("saved, but flushing failed"),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_closed_pipe_ends_a_status_quietly() {
        let (dir, status) = a_status("status-pipe");
        let mut closed = Failing(io::ErrorKind::BrokenPipe);
        assert!(finish_status(status, &mut closed).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_closed_pipe_keeps_the_late_error_and_a_full_output_names_both() {
        let Some((dir, late)) = a_status_not_durable("status-pipe-late") else {
            return;
        };
        let error = finish_status(late, &mut Failing(io::ErrorKind::BrokenPipe)).unwrap_err();
        assert!(
            matches!(&error, CliError::StatusNotDurable { output: None, .. }),
            "{error}"
        );
        assert_eq!(error.exit_code(), ExitCode::from(3));
        let _ = std::fs::remove_dir_all(dir);
        let Some((dir, late)) = a_status_not_durable("status-output-late") else {
            return;
        };
        let both = finish_status(late, &mut Failing(io::ErrorKind::StorageFull)).unwrap_err();
        assert!(
            matches!(
                &both,
                CliError::StatusNotDurable {
                    output: Some(_),
                    ..
                }
            ),
            "{both}"
        );
        assert_eq!(both.exit_code(), ExitCode::from(3));
        assert!(
            both.to_string().contains("could not be written either"),
            "{both}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_status_that_cannot_be_written_is_code_1() {
        let (dir, status) = a_status("status-output");
        let mut full = Failing(io::ErrorKind::StorageFull);
        let error = finish_status(status, &mut full).unwrap_err();
        assert!(matches!(&error, CliError::StatusOutput(_)), "{error}");
        assert_eq!(error.exit_code(), ExitCode::FAILURE);
        assert!(
            error.to_string().starts_with("could not write the status"),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The first file below `dir` whose path ends with `suffix`.
    fn find(dir: &Path, suffix: &str) -> Option<PathBuf> {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                if let Some(found) = find(&path, suffix) {
                    return Some(found);
                }
            } else if path.to_string_lossy().ends_with(suffix) {
                return Some(path);
            }
        }
        None
    }

    #[test]
    fn the_failures_of_a_status_have_the_exit_code_that_says_what_was_done() {
        let (dir, _) = repository_with_entry("status-codes");
        let repository = Repository::open(&dir).unwrap();
        let actor: Actor = "human daniel".parse().unwrap();
        std::fs::write(dir.join("a.txt"), "one").unwrap();
        repository
            .track(&["a.txt".to_owned()], &actor, &AllowAll)
            .unwrap();
        let _ = repository.commit("base", &actor, &AllowAll).unwrap();
        std::fs::write(dir.join("a.txt"), "two").unwrap();
        // The parent of the working-copy commit is gone: the snapshot is made, and reading fails.
        let parent = repository
            .refs()
            .resolve_head()
            .unwrap()
            .unwrap()
            .to_string();
        let file = find(&dir.join(".stemma/objects"), &parent[parent.len() - 8..]).unwrap();
        std::fs::remove_file(file).unwrap();

        let error = CliError::Status(repository.status(&actor, &AllowAll).unwrap_err());

        assert_eq!(error.exit_code(), ExitCode::from(3));
        assert!(
            error.to_string().contains("saved before as entry"),
            "{error}"
        );
        // The inputs that stop the call before the snapshot leave nothing: code 1.
        let invalid = CliError::Status(StatusError::Walk(TrackError::InvalidName {
            path: "x".into(),
        }));
        assert_eq!(invalid.exit_code(), ExitCode::FAILURE);
        assert!(
            invalid.to_string().ends_with("; nothing was written"),
            "{invalid}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_failures_of_a_commit_have_the_exit_code_that_says_what_was_done() {
        let (dir, entry) = repository_with_entry("commit-table");
        let snapshot_late = || {
            CommitError::Snapshot(SnapshotError::Update(UpdateError::NotDurable {
                entry: Box::new(entry.clone()),
                source: not_durable(),
            }))
        };
        // Nothing was made, or only a snapshot: code 1.
        for error in [
            CommitError::EmptyMessage,
            CommitError::NothingToCommit,
            CommitError::BranchMoved {
                branch: BranchName::new("main").unwrap(),
                parents: Vec::new(),
                tip: None,
            },
            CommitError::Update(UpdateError::MoveFailed {
                entry: Box::new(entry.clone()),
                source: RefError::Locked { path: dir.clone() },
            }),
        ] {
            assert_eq!(CliError::Commit(error).exit_code(), ExitCode::FAILURE);
        }
        // A pointer moved: code 3, and the text says what to do.
        let partly = CliError::Commit(CommitError::Update(UpdateError::PartlyMoved {
            entry: Box::new(entry.clone()),
            source: RefError::Locked { path: dir.clone() },
        }));
        assert_eq!(partly.exit_code(), ExitCode::from(3));
        assert!(
            partly.to_string().contains("finishes the commit"),
            "{partly}"
        );
        let durable = CliError::Commit(CommitError::Update(UpdateError::NotDurable {
            entry: Box::new(entry.clone()),
            source: not_durable(),
        }));
        assert_eq!(durable.exit_code(), ExitCode::from(3));
        let snapshot = CliError::Commit(snapshot_late());
        assert_eq!(snapshot.exit_code(), ExitCode::from(3));
        let text = snapshot.to_string();
        assert!(
            text.contains("the commit was not made") && text.contains("can be run again"),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_output_of_track_lists_new_files_or_says_there_are_none() {
        assert_eq!(track_text(&[]), "No new files to track\n");
        let two = ["a.txt".to_owned(), "bad\nname".to_owned()];
        assert_eq!(track_text(&two), "tracked a.txt\ntracked bad\\nname\n");
    }

    #[test]
    fn a_track_that_is_not_durable_lists_the_files_first_and_ends_with_code_3() {
        let (dir, entry) = repository_with_entry("not-durable");
        let late = TrackError::NotDurable {
            added: vec!["a.txt".to_owned()],
            error: Box::new(UpdateError::NotDurable {
                entry: Box::new(entry),
                source: RefError::Io {
                    operation: "flush",
                    path: PathBuf::from("x"),
                    source: io::Error::other("x"),
                },
            }),
        };
        let mut out = Vec::new();
        let error = finish_track(Err(late), &mut out).unwrap_err();
        assert_eq!(String::from_utf8(out).unwrap(), "tracked a.txt\n");
        assert_eq!(error.exit_code(), ExitCode::from(3), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_move_is_code_1_and_a_refused_path_is_code_1() {
        let (dir, entry) = repository_with_entry("move-failed");
        let failed = TrackError::Snapshot(SnapshotError::Update(UpdateError::MoveFailed {
            entry: Box::new(entry),
            source: RefError::Io {
                operation: "write",
                path: PathBuf::from("x"),
                source: io::Error::other("x"),
            },
        }));
        let mut out = Vec::new();
        let error = finish_track(Err(failed), &mut out).unwrap_err();
        assert_eq!(error.exit_code(), ExitCode::FAILURE, "{error}");
        assert!(out.is_empty());
        let refused = TrackError::InvalidPath {
            path: String::new(),
        };
        let error = finish_track(Err(refused), &mut out).unwrap_err();
        assert_eq!(error.exit_code(), ExitCode::FAILURE, "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_track_whose_result_cannot_be_written_is_code_3_and_a_closed_pipe_is_quiet() {
        let dir =
            std::env::temp_dir().join(format!("stemma-cli-unit-track-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.to_str().unwrap();
        run(
            parse(&["init", path, "--actor", "human daniel"]),
            &mut Vec::new(),
        )
        .unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::write(dir.join("b.txt"), "b").unwrap();
        let track_args = |file: &'static str| {
            let mut args = vec!["track", "-C", path, "--actor", "human daniel"];
            args.push(file);
            args
        };
        let full = run(parse(&track_args("a.txt")), &mut Full).unwrap_err();
        assert!(matches!(full, CliError::TrackOutput(_)), "{full}");
        assert_eq!(full.exit_code(), ExitCode::from(3));
        // The file is tracked although the result was not written.
        let mut out = Vec::new();
        run(parse(&track_args("a.txt")), &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "No new files to track\n");
        // A reader that has had enough is not a failure; the file is tracked.
        run(parse(&track_args("b.txt")), &mut Closed).unwrap();
        let mut out = Vec::new();
        run(parse(&track_args("b.txt")), &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "No new files to track\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_directory_that_is_not_a_repository_says_where_stemma_looks() {
        let error = CliError::Open(OpenError::NotARepository {
            path: PathBuf::from("sub"),
        });
        let message = error.to_string();
        assert!(message.contains("not in its parents"), "{message}");
    }

    #[test]
    fn the_arguments_are_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn the_actor_is_missing_or_empty_or_parsed() {
        assert!(matches!(actor(None), Err(CliError::MissingActor)));
        assert!(matches!(
            actor(Some(String::new())),
            Err(CliError::EmptyActor)
        ));
        assert!(matches!(
            actor(Some("human".into())),
            Err(CliError::Actor(_))
        ));
        assert_eq!(
            actor(Some("human daniel".into())).unwrap().to_string(),
            "human daniel"
        );
    }

    /// An output that fails on every write, such as a closed pipe.
    struct Closed;

    /// An output that fails on every write with another error than a closed pipe.
    struct Full;

    impl Write for Full {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("disk full"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Write for Closed {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// An output that takes the first write and fails on the second one.
    struct FullAfterOne(bool);

    impl Write for FullAfterOne {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if std::mem::replace(&mut self.0, true) {
                Err(io::Error::other("disk full"))
            } else {
                Ok(buf.len())
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("stemma").chain(args.iter().copied())).unwrap()
    }

    #[test]
    fn a_result_that_cannot_be_written_names_the_repository_that_exists() {
        let dir = std::env::temp_dir().join(format!("stemma-cli-unit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.to_str().unwrap();
        let error = run(
            parse(&["init", path, "--actor", "human daniel"]),
            &mut Closed,
        )
        .unwrap_err();
        let exists = dir.join(".stemma").join("HEAD").is_file();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(exists, "the repository was not made");
        assert!(matches!(error, CliError::Output { .. }), "{error}");
        let message = error.to_string();
        assert!(message.starts_with("created `"), "{message}");
        assert!(message.contains(".stemma"), "{message}");
        assert!(message.contains("could not write the result"), "{message}");
    }

    #[test]
    fn the_exit_code_tells_a_script_whether_something_was_made() {
        let io_error = || io::Error::other("x");
        let failed = [
            CliError::MissingActor,
            CliError::EmptyActor,
            CliError::Actor("human".parse::<Actor>().unwrap_err()),
            CliError::Open(OpenError::NotARepository {
                path: PathBuf::from("."),
            }),
            CliError::OpLog(OpLogError::Gap { missing: 2 }),
            CliError::EmptyLog,
            CliError::Write {
                entry: 1,
                source: io_error(),
            },
            CliError::Init(InitError::AlreadyExists {
                path: PathBuf::from(".stemma"),
            }),
            CliError::Init(InitError::Io {
                operation: "write the file",
                path: PathBuf::from(".stemma"),
                source: io_error(),
            }),
        ];
        for error in failed {
            assert_eq!(error.exit_code(), ExitCode::FAILURE, "{error}");
        }
        let made = [
            CliError::Init(InitError::NotDurable {
                path: PathBuf::from("."),
                source: io_error(),
            }),
            CliError::Output {
                created: PathBuf::from(".stemma"),
                source: io_error(),
            },
        ];
        for error in made {
            assert_eq!(error.exit_code(), ExitCode::from(3), "{error}");
        }
    }

    #[test]
    fn a_closed_pipe_ends_the_list_quietly_and_another_write_error_fails() {
        let dir = std::env::temp_dir().join(format!("stemma-cli-unit-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.to_str().unwrap();
        run(
            parse(&["init", path, "--actor", "human daniel"]),
            &mut Vec::new(),
        )
        .unwrap();
        let closed = run(parse(&["op", "log", path]), &mut Closed);
        let full = run(parse(&["op", "log", path]), &mut Full);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(closed.is_ok(), "{closed:?}");
        let error = full.unwrap_err();
        assert!(matches!(error, CliError::Write { entry: 1, .. }), "{error}");
        assert_eq!(
            error.to_string(),
            "could not write entry 1 to standard output: disk full"
        );
    }

    #[test]
    fn a_write_error_names_the_entry_in_flight_and_not_the_first_or_the_oldest() {
        let dir = std::env::temp_dir().join(format!("stemma-cli-unit-num-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.to_str().unwrap();
        run(
            parse(&["init", path, "--actor", "human daniel"]),
            &mut Vec::new(),
        )
        .unwrap();
        for number in 2..=3 {
            std::fs::write(
                dir.join(".stemma/oplog").join(format!("{number:012}")),
                format!(
                    "stemma-oplog 1\nnumber {number}\ntime 1700000000\nactor human daniel\n\
                     operation snapshot\nworking-copy - b3:{}\n",
                    "1".repeat(64)
                ),
            )
            .unwrap();
        }
        // The newest entry is written first, so a failing first write names entry 3, with or
        // without a limit; a failure of the second write names the entry that follows it.
        let first = run(parse(&["op", "log", path]), &mut Full).unwrap_err();
        let limited = run(parse(&["op", "log", "-n", "2", path]), &mut Full).unwrap_err();
        let second = run(parse(&["op", "log", path]), &mut FullAfterOne(false)).unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(matches!(first, CliError::Write { entry: 3, .. }), "{first}");
        assert!(
            matches!(limited, CliError::Write { entry: 3, .. }),
            "{limited}"
        );
        assert!(
            matches!(second, CliError::Write { entry: 2, .. }),
            "{second}"
        );
    }

    #[test]
    fn the_message_of_a_repository_that_is_not_durable_says_that_it_exists() {
        let error = CliError::Init(InitError::NotDurable {
            path: PathBuf::from("/x"),
            source: io::Error::other("disk"),
        });
        let message = error.to_string();
        assert!(
            message.contains("was created") && message.contains("disk"),
            "{message}"
        );
    }

    #[test]
    fn the_path_in_the_result_is_absolute() {
        assert!(absolute(Path::new("a/b")).is_absolute());
        assert_eq!(absolute(Path::new("/a/b")), PathBuf::from("/a/b"));
    }
}
