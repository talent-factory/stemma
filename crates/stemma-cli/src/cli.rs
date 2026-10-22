//! Arguments, the actor, and the commands.
//!
//! Exit codes: 0 for success, 1 for a command that fails, 2 for arguments that are wrong (the code
//! of the argument parser), and 3 for a command that has done its work but could not finish: the
//! repository or the change exists, but flushing it to disk failed, or (for `track`, `commit`, and
//! `op restore`) writing the result failed after the work was made, or a `commit` moved one of
//! its two pointers and not the other. A script must not remove or repeat what such a command
//! made; only the next `commit` after a half-made one finishes it, and the message of that call is
//! not used. The exception is an `op restore` that moved some pointers and not all: it is code 3,
//! and the same `op restore` run again moves the rest. Running `status` again is safe, but it
//! finds nothing to save and so does not make an unflushed snapshot durable.
//!
//! A `purge` that stopped after its journal was written is code 3 as well, and so is a
//! `purge --apply` that finds the journal of another purge and an `--abort` that is refused after
//! the entry of the purge: until `purge --resume` (or `--abort`) has run, no other command
//! writes. A `purge --apply`, `--resume`, or `--abort` that has done its work but could not flush
//! the removal of the journal, or whose result cannot be written, is code 3 too; a closed pipe is
//! no failure there, as the log names the entry of a purge, an abort leaves the repository as it
//! was before the purge, and nothing can undo either.
//!
//! A command that only reads (`op log`, `status`, `op restore --dry-run`, and `purge` without
//! `--apply`, `--resume`, or `--abort`) has made nothing by its output, so a failed write of its
//! output is code 1; some of the output may already be written, and a script discards it. A
//! closed pipe is not a failure, and it does not hide an error of flushing that comes after the
//! output. The one exception is a closed pipe after an `op restore` that was made: the output is
//! the only place that names its entry, so it is code 3.
//! An `op restore` writes its result before an error that follows a restore that was made, as
//! `commit` does.
//!
//! Code 1 can leave two traces. A write that fails after its entry is in the operation log
//! (`track`, `commit`, `op restore`, `UpdateError::MoveFailed`) leaves an entry for a move that
//! did not happen, which `op log` marks as such. For `op restore` the files may be written then,
//! and no pointer moved: the same call run again completes it. And a command that saves the
//! tracked files first leaves that entry (`snapshot`) when it fails afterwards: `commit` when it
//! is refused for nothing to commit or for a branch that moved, `status` when it cannot read the
//! repository or write its output, `op restore` when a later step fails, and `purge --apply`
//! when the purge cannot be planned or started after it. The message of such a failure names the
//! entry, and repeating the command is safe.
//!
//! A snapshot that moved the pointer and could not flush it is code 3: for `status` after the
//! status is written, for `commit` without a commit (it can be repeated), for `op restore` after
//! the restore is made, for `purge --apply` before the purge, and for `track` as the other flush
//! errors.
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
use std::str::FromStr;

use clap::{Args, Parser, Subcommand};

use crate::op_log;
use stemma_core::object::{ObjectId, METADATA_DIR};
use stemma_core::oplog::{self, EntryHash, OpLogError};
use stemma_core::refs::{BranchName, RefError};
use stemma_core::repository::{
    normalize_path, ApplyPurgeError, CommitError, Committed, InitError, InitOptions, OpenError,
    PurgeError, PurgePlan, Purged, Repository, RestoreError, RestorePlan, SnapshotError, Status,
    StatusError, TrackError, Tracking, UpdateError,
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
    /// Remove a secret from the history and the log; without --apply, only show what it would do
    Purge(PurgeArgs),
}

#[derive(Debug, Args)]
struct PurgeArgs {
    /// The ID of the blob that holds the secret, `b3:` and 64 hexadecimal digits
    #[arg(
        value_name = "BLOB",
        required_unless_present_any = ["resume", "abort"],
        conflicts_with_all = ["resume", "abort"]
    )]
    blob: Option<ObjectId>,

    /// Purge: rewrite the history, replace the entries by stubs, and delete the objects; a purge
    /// cannot be undone
    #[arg(long, conflicts_with_all = ["resume", "abort"])]
    apply: bool,

    /// Finish a purge that stopped, from its journal
    #[arg(long, conflicts_with = "abort")]
    resume: bool,

    /// Undo a purge that stopped before its entry was written
    #[arg(long)]
    abort: bool,

    /// The directory of the repository, the one that contains `.stemma`
    #[arg(
        short = 'C',
        long = "repository",
        value_name = "DIR",
        default_value = "."
    )]
    repository: PathBuf,

    /// Who runs the command: `human <id>`; an agent may not purge. Required unless the
    /// environment sets STEMMA_ACTOR
    #[arg(long, env = ACTOR_ENV, value_name = "ACTOR")]
    actor: Option<String>,
}

#[derive(Debug, Subcommand)]
enum OpCommand {
    /// List the entries of the operation log, newest first
    Log(OpLogArgs),
    /// Put back the state before an entry: HEAD, the working copy, and the branches it moved
    Restore(OpRestoreArgs),
    /// Check the hash chain of the whole log, and print the anchor of its last entry if that is in
    /// the chain
    Verify(OpVerifyArgs),
}

#[derive(Debug, Args)]
struct OpVerifyArgs {
    /// The directory of the repository, the one that contains `.stemma`
    #[arg(
        short = 'C',
        long = "repository",
        value_name = "DIR",
        default_value = "."
    )]
    repository: PathBuf,

    /// An anchor that an earlier `op verify` printed, `<number>:<hash>`, kept outside the
    /// repository: the entry with the number must still have the hash
    #[arg(long, value_name = "NUMBER:HASH")]
    anchor: Option<Anchor>,
}

/// An entry and the hash that it had when `op verify` printed it (ADR-011). Its text is
/// `<number>:<hash>`, which `op verify` prints and `--anchor` reads.
#[derive(Debug, Clone, Copy)]
struct Anchor {
    number: u64,
    hash: EntryHash,
}

impl fmt::Display for Anchor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.number, self.hash)
    }
}

impl FromStr for Anchor {
    type Err = String;

    /// Reads the canonical form only, as `Display` writes it.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (number, hash) = text
            .split_once(':')
            .ok_or_else(|| "expected `<number>:<hash>`, such as `12:b3:…`".to_owned())?;
        let number = number
            .parse::<u64>()
            .ok()
            // The canonical form only: no sign, no leading zero, and not 0.
            .filter(|parsed| *parsed >= 1 && parsed.to_string() == number)
            .ok_or_else(|| format!("`{number}` is not the number of an entry"))?;
        let hash = EntryHash::from_str(hash).map_err(|error| format!("invalid hash: {error}"))?;
        Ok(Self { number, hash })
    }
}

#[derive(Debug, Args)]
struct OpRestoreArgs {
    /// The number of the entry whose old values are put back; `op log` lists the numbers
    #[arg(value_name = "NUMBER")]
    number: u64,

    /// The directory of the repository, the one that contains `.stemma`
    #[arg(
        short = 'C',
        long = "repository",
        value_name = "DIR",
        default_value = "."
    )]
    repository: PathBuf,

    /// Restore although the log and the references do not show whether the entry took effect
    #[arg(long)]
    force: bool,

    /// Show what would be restored and write nothing, not even a snapshot of the working copy
    #[arg(long)]
    dry_run: bool,

    /// Who runs the command: `human <id>` or
    /// `agent <id> model=<model> session=<session> sponsor=<sponsor>`; required unless the
    /// environment sets STEMMA_ACTOR. The snapshot and the restore are made by this actor
    #[arg(long, env = ACTOR_ENV, value_name = "ACTOR")]
    actor: Option<String>,
}

#[derive(Debug, Args)]
struct OpLogArgs {
    /// The directory of the repository, the one that contains `.stemma`
    #[arg(
        short = 'C',
        long = "repository",
        value_name = "DIR",
        default_value = "."
    )]
    repository: PathBuf,

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
    /// `agent <id> model=<model> session=<session> sponsor=<sponsor>`; required unless the
    /// environment sets STEMMA_ACTOR
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
    /// `agent <id> model=<model> session=<session> sponsor=<sponsor>`; required unless the
    /// environment sets STEMMA_ACTOR
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
    /// `agent <id> model=<model> session=<session> sponsor=<sponsor>`; required unless the
    /// environment sets STEMMA_ACTOR. The snapshot that the command takes is made by this actor
    #[arg(long, env = ACTOR_ENV, value_name = "ACTOR")]
    actor: Option<String>,
}

#[derive(Debug, Args)]
struct InitArgs {
    /// The directory of the repository; it is created if it does not exist
    #[arg(
        short = 'C',
        long = "repository",
        value_name = "DIR",
        default_value = "."
    )]
    repository: PathBuf,

    /// The branch that HEAD points to; the branch exists after the first commit
    #[arg(long, value_name = "NAME", default_value = "main")]
    initial_branch: BranchName,

    /// Who runs the command: `human <id>` or
    /// `agent <id> model=<model> session=<session> sponsor=<sponsor>`; required unless the
    /// environment sets STEMMA_ACTOR
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
    /// The core could not report the status. If it saved the tracked files before it failed, the
    /// message names the entry of that snapshot ([`StatusError::saved`]), and a failure of
    /// flushing it is part of the message and of the exit code.
    Status(StatusError),
    /// The status was reported, but the snapshot that came first could not be flushed to disk.
    StatusNotDurable {
        /// The number of the entry of the snapshot.
        entry: u64,
        /// The error of the flush.
        source: Box<RefError>,
        /// The error of the output, if the status could not be written either. A closed pipe is
        /// not recorded here.
        output: Option<io::Error>,
    },
    /// The status could not be written to the output.
    StatusOutput {
        /// The error of the output.
        source: io::Error,
        /// The number of the entry of the snapshot that the call made, if one.
        entry: Option<u64>,
    },
    /// The restore was refused or stopped. [`RestoreError::pointer_moved`] says whether a pointer
    /// moved; the error says what is in the log and what state the files are in.
    Restore(RestoreError),
    /// The restore was made or planned, but the result could not be written to the output.
    RestoreOutput {
        /// The entry of the restore, if one was made.
        restored: Option<u64>,
        /// The entry of the snapshot that came first, if one was made.
        snapshot: Option<u64>,
        /// The error of the output.
        source: io::Error,
    },
    /// The purge was refused, or it could not be planned. Nothing was written.
    Purge(PurgeError),
    /// The plan of a purge could not be written to the output. Nothing was written to the
    /// repository.
    PurgeOutput(io::Error),
    /// The purge, its resume, or its abort was refused or stopped.
    /// [`ApplyPurgeError::pending`] says whether its journal stays.
    ApplyPurge(ApplyPurgeError),
    /// The purge, its resume, or its abort was made, but the result could not be written.
    PurgedOutput {
        /// What was made, as the first line of the output says it.
        made: String,
        /// The error of the output.
        source: io::Error,
    },
    /// The operation log cannot be read.
    OpLog(OpLogError),
    /// The entry that an anchor names is not in the log.
    AnchorMissing {
        /// The number of the entry.
        number: u64,
    },
    /// The entry that an anchor names was written before the chain, so it has no hash.
    AnchorUnchained {
        /// The number of the entry.
        number: u64,
    },
    /// The entry that an anchor names has another hash now.
    AnchorMismatch {
        /// The anchor.
        anchor: Anchor,
        /// The hash that the entry has now.
        found: EntryHash,
    },
    /// The log was checked, but the result could not be written.
    VerifyOutput(io::Error),
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
            | Self::StatusNotDurable { .. }
            | Self::PurgedOutput { .. }
            | Self::RestoreOutput {
                restored: Some(_), ..
            }
            | Self::RestoreOutput {
                snapshot: Some(_), ..
            } => ExitCode::from(3),
            Self::Restore(error) if error.pointer_moved() => ExitCode::from(3),
            Self::Commit(error) if pointer_moved(error) => ExitCode::from(3),
            Self::Status(error) if error.not_durable() => ExitCode::from(3),
            Self::ApplyPurge(error) if error.made() => ExitCode::from(3),
            Self::ApplyPurge(ApplyPurgeError::Snapshot(SnapshotError::Update(error)))
                if error.pointer_moved() =>
            {
                ExitCode::from(3)
            }
            Self::MissingActor
            | Self::EmptyActor
            | Self::Actor(_)
            | Self::Init(_)
            | Self::Open(_)
            | Self::Track(_)
            | Self::Commit(_)
            | Self::Status(_)
            | Self::StatusOutput { .. }
            | Self::Restore(_)
            | Self::RestoreOutput { .. }
            | Self::Purge(_)
            | Self::PurgeOutput(_)
            | Self::ApplyPurge(_)
            | Self::OpLog(_)
            | Self::AnchorMissing { .. }
            | Self::AnchorUnchained { .. }
            | Self::AnchorMismatch { .. }
            | Self::VerifyOutput(_)
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
                 (see `-C` of `track`, `commit`, `status`, `op restore`, and `purge`, and the \
                 argument of `op log`)"
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
            Self::StatusNotDurable {
                entry,
                source,
                output,
            } => {
                write!(
                    f,
                    "the tracked files were saved as entry {entry} of the operation log, but \
                     flushing failed: {source}; the saved state may not survive a crash, and \
                     running the command again does not make it durable"
                )?;
                match output {
                    Some(error) => write!(f, "; the status could not be written either: {error}"),
                    None => Ok(()),
                }
            }
            Self::StatusOutput { source, entry } => {
                write!(f, "could not write the status: {source}")?;
                match entry {
                    Some(entry) => write!(
                        f,
                        "; the tracked files were saved first as entry {entry} of the operation \
                         log"
                    ),
                    None => Ok(()),
                }
            }
            Self::Restore(error) => error.fmt(f),
            Self::RestoreOutput {
                restored,
                snapshot,
                source,
            } => {
                match (restored, snapshot) {
                    (Some(entry), _) => {
                        write!(f, "restored as entry {entry} of the operation log")?
                    }
                    (None, Some(_)) => write!(f, "nothing was restored")?,
                    (None, None) => write!(f, "nothing was restored or written")?,
                }
                if let Some(saved) = snapshot {
                    write!(f, " (the working copy was saved first as entry {saved})")?;
                }
                write!(f, ", but the result could not be written: {source}")?;
                match restored {
                    Some(entry) => write!(f, "; to undo it run `stemma op restore {entry}`"),
                    None => Ok(()),
                }
            }
            Self::Purge(error) => error.fmt(f),
            Self::PurgeOutput(source) => write!(
                f,
                "could not write the plan of the purge, and nothing was written: {source}"
            ),
            Self::ApplyPurge(error) => error.fmt(f),
            Self::PurgedOutput { made, source } => {
                write!(f, "{made}, but the result could not be written: {source}")
            }
            Self::OpLog(error) => error.fmt(f),
            Self::AnchorMissing { number } => write!(
                f,
                "the anchor names entry {number}, but the log has no such entry: the entry was \
                 lost, or the anchor belongs to another repository"
            ),
            Self::AnchorUnchained { number } => write!(
                f,
                "the anchor names entry {number}, but that entry was written before the hash \
                 chain and has no hash: the entry was replaced, or the anchor belongs to another \
                 repository"
            ),
            Self::AnchorMismatch { anchor, found } => write!(
                f,
                "entry {} has the hash {found}, not the hash {} of the anchor: the log was \
                 changed and its chain computed again since the anchor was taken",
                anchor.number, anchor.hash
            ),
            Self::VerifyOutput(source) => write!(
                f,
                "checked the operation log, but could not write the result: {source}"
            ),
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
        Command::Op {
            command: OpCommand::Restore(args),
        } => restore(args, out),
        Command::Op {
            command: OpCommand::Verify(args),
        } => verify_op_log(args, out),
        Command::Purge(args) => purge(args, out),
    }
}

/// Purges a blob (ADR-014), or shows what that would do. The core refuses an agent before it
/// reads the history. Without `--apply` the command writes nothing, not even a snapshot, and a
/// closed pipe is not a failure. With `--apply` the purge is made: it takes a snapshot, rewrites
/// the history, replaces the entries by stubs, and deletes the objects; `--resume` finishes a
/// purge that stopped, and `--abort` undoes one that stopped before its entry was written.
fn purge(args: PurgeArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let actor = actor(args.actor)?;
    let repository = Repository::open(&args.repository).map_err(CliError::Open)?;
    let (made, text) = if args.resume {
        let purged = repository
            .resume_purge(&actor, &AllowAll)
            .map_err(CliError::ApplyPurge)?;
        warn_scratch_left(&purged);
        purged_text("Finished the purge of", &purged)
    } else if args.abort {
        let aborted = repository
            .abort_purge(&actor, &AllowAll)
            .map_err(CliError::ApplyPurge)?;
        let made = format!(
            "Undid the purge of blob {}; removed {} new objects",
            aborted.plan().blob(),
            aborted.removed().len()
        );
        (made.clone(), format!("{made}\n"))
    } else {
        let blob = args
            .blob
            .expect("the parser requires the blob without --resume or --abort");
        if !args.apply {
            let plan = repository
                .plan_purge(blob, &actor, &AllowAll)
                .map_err(CliError::Purge)?;
            return match out.write_all(purge_text(&plan).as_bytes()) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
                Err(error) => Err(CliError::PurgeOutput(error)),
            };
        }
        let purged = repository
            .apply_purge(blob, &actor, &AllowAll)
            .map_err(CliError::ApplyPurge)?;
        warn_scratch_left(&purged);
        purged_text("Purged", &purged)
    };
    match out.write_all(text.as_bytes()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(source) => Err(CliError::PurgedOutput { made, source }),
    }
}

/// Warns about each scratch file in `tmp/` that the purge could not remove: it may hold the secret
/// after a failed write (ADR-014). The purge itself succeeded, so this is no error.
fn warn_scratch_left(purged: &Purged) {
    for path in purged.scratch_left() {
        // A warning that cannot be written is dropped on purpose, as in `main`.
        let _ = writeln!(
            io::stderr(),
            "stemma: warning: could not remove the scratch file `{}`; it may hold the secret, \
             remove it by hand",
            path.display()
        );
    }
}

/// What a purge would do: a first line, the lines of [`plan_lines`], and a last line that says how
/// to make it.
fn purge_text(plan: &PurgePlan) -> String {
    format!(
        "Would purge blob {}; nothing is written\n{}{}",
        plan.blob(),
        plan_lines(plan),
        "Run it with --apply; a purge cannot be undone\n"
    )
}

/// What a purge did: the snapshot that came first, a line with the blob and the `purge` entries,
/// and the lines of [`plan_lines`]. `done` says what was done: the purge, or its end. Returns the
/// line with the entries as well, for an error of the output.
fn purged_text(done: &str, purged: &Purged) -> (String, String) {
    let numbers: Vec<String> = purged
        .entries()
        .iter()
        .map(|entry| entry.number().to_string())
        .collect();
    let entries = match numbers.as_slice() {
        [one] => format!("entry {one}"),
        more => format!("entries {}", more.join(", ")),
    };
    let made = format!(
        "{done} blob {} as {entries} of the operation log",
        purged.plan().blob()
    );
    let text = format!(
        "{}{made}\n{}",
        saved_text(purged.snapshot()),
        plan_lines(purged.plan())
    );
    (made, text)
}

/// One line for each commit, pointer, and object of a purge, and one line for the stubs.
fn plan_lines(plan: &PurgePlan) -> String {
    use crate::op_log::{value, HeadValue};
    let mut text = String::new();
    for (old, new) in plan.rewrites() {
        text.push_str(&format!("    rewrite {old} -> {new}\n"));
    }
    if let Some(head) = plan.head() {
        text.push_str(&format!(
            "    head  {} -> {}\n",
            value(head.before().map(HeadValue).as_ref()),
            value(head.after().map(HeadValue).as_ref())
        ));
    }
    if let Some(moved) = plan.working_copy() {
        text.push_str(&format!(
            "    working-copy  {} -> {}\n",
            value(moved.before()),
            value(moved.after())
        ));
    }
    for (name, moved) in plan.branches() {
        text.push_str(&format!(
            "    branch {}  {} -> {}\n",
            name.to_string().escape_debug(),
            value(moved.before()),
            value(moved.after())
        ));
    }
    if !plan.entries().is_empty() {
        text.push_str(&format!("    stub entries {}\n", ranges(plan.entries())));
    }
    for commit in plan.deleted_commits() {
        text.push_str(&format!("    delete commit {commit}\n"));
    }
    for tree in plan.deleted_trees() {
        text.push_str(&format!("    delete tree {tree}\n"));
    }
    text.push_str(&format!("    delete blob {}\n", plan.blob()));
    text
}

/// Numbers in ascending order as ranges: `3, 5-7`.
fn ranges(numbers: &[u64]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut index = 0;
    while index < numbers.len() {
        let start = numbers[index];
        let mut end = start;
        while index + 1 < numbers.len() && numbers[index + 1] == end + 1 {
            index += 1;
            end = numbers[index];
        }
        parts.push(if start == end {
            start.to_string()
        } else {
            format!("{start}-{end}")
        });
        index += 1;
    }
    parts.join(", ")
}

/// Puts back the old values of an entry, or shows what that would do. The core plans first, so a
/// missing commit or a file in the way is an error before anything is written, and it saves the
/// working directory in a snapshot before it changes it.
///
/// The result is written before an error that follows a restore that was made (exit code 3), as
/// `commit` does, so that a script finds the entry that undoes it. A closed pipe is no failure for
/// a plan or for nothing to restore; after a restore it is, because the output is the only place
/// that names the entry.
fn restore(args: OpRestoreArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let actor = actor(args.actor)?;
    let repository = Repository::open(&args.repository).map_err(CliError::Open)?;
    if args.dry_run {
        let plan = repository
            .plan_restore(args.number, args.force, &actor, &AllowAll)
            .map_err(CliError::Restore)?;
        return write_restore(out, &plan_text(args.number, plan.as_ref()), None, None);
    }
    match repository.restore(args.number, args.force, &actor, &AllowAll) {
        Ok(result) => write_restore(
            out,
            &restored_text(args.number, result.snapshot(), result.entry()),
            result.entry().map(oplog::Entry::number),
            result.snapshot().map(oplog::Entry::number),
        ),
        Err(error) => {
            if error.pointer_moved() {
                // The message of the error names the entries too, so a failure to write is not
                // reported on top of it.
                let _ = out.write_all(made_text(args.number, &error).as_bytes());
            }
            Err(CliError::Restore(error))
        }
    }
}

/// Writes the result of a restore or a plan. `restored` and `snapshot` are the entries that the
/// call made.
fn write_restore(
    out: &mut dyn Write,
    text: &str,
    restored: Option<u64>,
    snapshot: Option<u64>,
) -> Result<(), CliError> {
    match out.write_all(text.as_bytes()) {
        Ok(()) => Ok(()),
        // The reader of a pipe has had enough, and nothing was made that only this text names.
        Err(error)
            if error.kind() == io::ErrorKind::BrokenPipe
                && restored.is_none()
                && snapshot.is_none() =>
        {
            Ok(())
        }
        Err(source) => Err(CliError::RestoreOutput {
            restored,
            snapshot,
            source,
        }),
    }
}

/// The line that says there is nothing to move.
fn nothing_to_restore(number: u64) -> String {
    format!("Nothing to restore: every pointer of entry {number} has its old value already\n")
}

/// What a restore would do: the pointers, the files, and a note about unsaved changes.
fn plan_text(number: u64, plan: Option<&RestorePlan>) -> String {
    use crate::op_log::{value, HeadValue};
    let Some(plan) = plan else {
        return nothing_to_restore(number);
    };
    let mut text = format!("Would restore the state before entry {number}; nothing is written\n");
    if let Some(head) = plan.head() {
        text.push_str(&format!(
            "    head  {} -> {}\n",
            value(head.before().map(HeadValue).as_ref()),
            value(head.after().map(HeadValue).as_ref())
        ));
    }
    if let Some(moved) = plan.working_copy() {
        text.push_str(&format!(
            "    working-copy  {} -> {}\n",
            value(moved.before()),
            value(moved.after())
        ));
    }
    for (name, moved) in plan.branches() {
        text.push_str(&format!(
            "    branch {}  {} -> {}\n",
            name.to_string().escape_debug(),
            value(moved.before()),
            value(moved.after())
        ));
    }
    if let Some(checkout) = plan.checkout().filter(|checkout| !checkout.is_empty()) {
        for write in checkout.writes() {
            text.push_str(&format!("    write {}\n", escape_path(write.path())));
        }
        for path in checkout.removals() {
            text.push_str(&format!("    remove {}\n", escape_path(path)));
        }
        text.push_str(
            "A real run saves unsaved changes first; the files above are replaced all the same\n",
        );
    }
    text
}

/// The line that names the snapshot that came first.
fn saved_text(snapshot: Option<&oplog::Entry>) -> String {
    snapshot.map_or_else(String::new, |entry| {
        format!("Saved the working copy first as entry {}\n", entry.number())
    })
}

/// What a restore did: the snapshot, the entry of the restore, and how to undo it.
fn restored_text(
    number: u64,
    snapshot: Option<&oplog::Entry>,
    entry: Option<&oplog::Entry>,
) -> String {
    let saved = saved_text(snapshot);
    match entry {
        Some(entry) => format!(
            "{saved}Restored the state before entry {number} as entry {}\nTo undo it: stemma op \
             restore {}\n",
            entry.number(),
            entry.number()
        ),
        None => format!("{saved}{}", nothing_to_restore(number)),
    }
}

/// What a restore that was made or half made has done, for the output before its error.
fn made_text(number: u64, error: &RestoreError) -> String {
    match (error.half_made(), error.entry()) {
        (true, Some(entry)) => format!(
            "{}Half made: entry {} of the restore moved some of the pointers of entry {number}\n\
             To finish it: stemma op restore {number}\n",
            saved_text(error.snapshot()),
            entry.number()
        ),
        _ => restored_text(number, error.snapshot(), error.entry()),
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
    let repository = Repository::open(&args.repository).map_err(CliError::Open)?;
    let log = repository.oplog();
    let numbers = log.numbers().map_err(CliError::OpLog)?;
    if numbers.is_empty() {
        return Err(CliError::EmptyLog);
    }
    // The shown entries are the newest ones, so every later entry of a pointer is among them, and
    // only the last entry of a pointer needs its value now. The purge entry of a shown stub comes
    // after it, so it is shown too, and `tail` checks the stubs.
    let window = log.window(args.limit.get()).map_err(CliError::OpLog)?;
    if let Some((number, error)) = window.unchecked() {
        // A warning that cannot be written is dropped on purpose, as in `main`.
        let _ = writeln!(
            io::stderr(),
            "stemma: warning: the link of the oldest shown entry to entry {number} was not \
             checked, as that entry cannot be read: {error}; `stemma op verify` checks the whole \
             log"
        );
    }
    let entries = window.into_entries();
    let pending = oplog::pending_stubs(&entries);
    let names: BTreeSet<&BranchName> = entries
        .iter()
        .filter_map(oplog::Entry::change)
        .flat_map(|change| change.branches().iter().map(|(name, _)| name))
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
        let pending = pending.contains(&entry.number());
        let written = if args.json {
            writeln!(out, "{}", op_log::json(entry, outcome, pending))
        } else {
            write!(out, "{}", op_log::text(entry, outcome, pending))
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

/// Checks the hash chain of the whole log and the tip file (see `FsOpLog::verify`), and the
/// anchor if one is given, and prints the anchor of the last entry (ADR-011). The chain shows that
/// the entries agree with one another; whoever can write `.stemma` can compute it again, so only
/// an anchor kept outside the repository shows that the log was not rewritten. It only reads, and
/// a closed pipe is not a failure.
fn verify_op_log(args: OpVerifyArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let repository = Repository::open(&args.repository).map_err(CliError::Open)?;
    let entries = repository.oplog().verify().map_err(CliError::OpLog)?;
    let last = entries.last().ok_or(CliError::EmptyLog)?;
    if let Some(anchor) = args.anchor {
        let entry = entries
            .iter()
            .find(|entry| entry.number() == anchor.number)
            .ok_or(CliError::AnchorMissing {
                number: anchor.number,
            })?;
        let found = entry.hash().ok_or(CliError::AnchorUnchained {
            number: anchor.number,
        })?;
        if found != anchor.hash {
            return Err(CliError::AnchorMismatch { anchor, found });
        }
    }
    let start = oplog::chain_start(&entries);
    let mut text = format!("Checked {} entries of the operation log", entries.len());
    match start {
        Some(1) => text.push_str(": the hash chain holds.\n"),
        Some(start) => text.push_str(&format!(
            ": the hash chain holds from entry {start}; entries 1 to {} were written before it \
             and are not covered.\n",
            start - 1
        )),
        None => text.push_str(
            ": no entry is in the hash chain yet, so there is no anchor; the next entry starts \
             it.\n",
        ),
    }
    if let Some(anchor) = args.anchor {
        text.push_str(&format!(
            "Entry {} still has the hash of the anchor.\n",
            anchor.number
        ));
    }
    if let Some(hash) = last.hash() {
        let anchor = Anchor {
            number: last.number(),
            hash,
        };
        text.push_str(&format!("anchor {anchor}\n"));
    }
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(source) => Err(CliError::VerifyOutput(source)),
        Ok(()) => Ok(()),
    }
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
        .map(|path| format!("tracked {}\n", escape_path(path)))
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
/// error comes after it. A closed pipe ends the output quietly, as for `op log`, and the error of
/// flushing stays.
fn finish_status(status: Status, out: &mut dyn Write) -> Result<(), CliError> {
    let text = status_text(&status);
    let entry = status.snapshot.as_ref().map(|saved| saved.entry.number());
    let late = status.snapshot.and_then(|saved| saved.late);
    match out.write_all(text.as_bytes()) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {}
        Err(source) => {
            return Err(match (late, entry) {
                (Some(late), Some(entry)) => CliError::StatusNotDurable {
                    entry,
                    source: Box::new(late),
                    output: Some(source),
                },
                _ => CliError::StatusOutput { source, entry },
            });
        }
    }
    match (late, entry) {
        (Some(source), Some(entry)) => Err(CliError::StatusNotDurable {
            entry,
            source: Box::new(source),
            output: None,
        }),
        _ => Ok(()),
    }
}

/// The text of the status: the changes with their letters and the untracked files, each section
/// only if it has files, or one line if there are none. A path is written with [`escape_path`].
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
                escape_path(&change.path)
            ));
        }
    }
    if !status.untracked.is_empty() {
        text.push_str("Untracked files:\n");
        for path in &status.untracked {
            text.push_str(&format!("? {}\n", escape_path(path)));
        }
    }
    text
}

/// A path as it is written in the output, so that two different paths are never written alike:
/// every character is escaped as `char::escape_debug` does it (a quote, a backslash, a control
/// character, and a combining mark in every position, where `str::escape_debug` leaves one after
/// the first character as it is), and a space at the start or the end of a name is `\u{20}`.
fn escape_path(path: &str) -> String {
    path.split('/')
        .map(|name| {
            let last = name.chars().count().saturating_sub(1);
            name.chars()
                .enumerate()
                .map(|(index, c)| match c {
                    ' ' if index == 0 || index == last => "\\u{20}".to_owned(),
                    other => other.escape_debug().to_string(),
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/")
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
    let created = absolute(&args.repository.join(METADATA_DIR));
    // A repository that is complete but not durable is announced like a success, so that a
    // script can find it, and the command ends with the error afterwards.
    let late = match Repository::init(&args.repository, &options, &actor, &AllowAll) {
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
    // A command that writes holds the lock of the references at least as long as its snapshot takes
    // (ADR-014). Ctrl-C (or a termination signal) removes it before the process ends, so that the
    // next command does not find it; the process ends with 130, as a shell reports an interrupt.
    // Without the handler (it could not be installed) an interrupt leaves the lock behind.
    let _ = ctrlc::set_handler(|| {
        stemma_core::refs::release_locks_for_exit();
        std::process::exit(INTERRUPTED);
    });
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

/// The exit code of a command that was interrupted: 128 and the number of `SIGINT`.
const INTERRUPTED: i32 = 130;

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

    #[test]
    fn a_purge_that_leaves_its_journal_ends_with_code_3_and_one_that_did_not_start_with_code_1() {
        use stemma_core::repository::{PurgeFailure, PurgeStep};
        let path = PathBuf::from(".stemma/purge-pending");
        let code = |error: ApplyPurgeError| CliError::ApplyPurge(error).exit_code();
        assert_eq!(
            code(ApplyPurgeError::Stopped {
                step: PurgeStep::Entry,
                entries: Vec::new(),
                source: Box::new(PurgeFailure::Refs(RefError::Locked { path: path.clone() })),
            }),
            ExitCode::from(3)
        );
        assert_eq!(
            code(ApplyPurgeError::Pending { path: path.clone() }),
            ExitCode::from(3),
            "no other command may write until the purge is resumed or undone"
        );
        assert_eq!(
            code(ApplyPurgeError::PastNoReturn { entries: vec![7] }),
            ExitCode::from(3)
        );
        assert_eq!(
            code(ApplyPurgeError::NotDurable {
                entries: vec![7],
                source: Box::new(stemma_core::repository::JournalError::Malformed {
                    path: path.clone(),
                }),
            }),
            ExitCode::from(3),
            "the purge is made, although the journal that is gone may come back"
        );
        assert_eq!(
            CliError::PurgedOutput {
                made: "Purged blob".to_owned(),
                source: io::Error::other("disk full"),
            }
            .exit_code(),
            ExitCode::from(3)
        );
        assert_eq!(
            code(ApplyPurgeError::NotPending { path: path.clone() }),
            ExitCode::FAILURE
        );
        assert_eq!(
            code(ApplyPurgeError::Plan(PurgeError::Agent)),
            ExitCode::FAILURE
        );
        assert_eq!(
            code(ApplyPurgeError::Start(PurgeFailure::Refs(
                RefError::Locked { path }
            ))),
            ExitCode::FAILURE
        );
    }

    #[test]
    fn a_restore_that_moved_a_pointer_ends_with_code_3_and_any_other_failure_with_code_1() {
        let (dir, entry) = repository_with_entry("restore-codes");
        let entry = Box::new(entry);
        let locked = || RefError::Locked { path: dir.clone() };
        let code = |error: RestoreError| CliError::Restore(error).exit_code();
        assert_eq!(
            code(RestoreError::PartlyMoved {
                entry: entry.clone(),
                source: locked()
            }),
            ExitCode::from(3),
            "some pointers moved: the same restore run again moves the rest"
        );
        assert_eq!(
            code(RestoreError::NotDurable {
                entry: entry.clone(),
                source: locked()
            }),
            ExitCode::from(3)
        );
        assert_eq!(
            code(RestoreError::AfterSnapshot {
                snapshot: entry.clone(),
                late: None,
                source: Box::new(RestoreError::PartlyMoved {
                    entry: entry.clone(),
                    source: locked()
                })
            }),
            ExitCode::from(3)
        );
        for error in [
            RestoreError::MoveFailed {
                entry: entry.clone(),
                source: locked(),
            },
            RestoreError::NoSuchEntry { number: 9 },
            RestoreError::Refs(locked()),
        ] {
            assert_eq!(code(error), ExitCode::FAILURE);
        }
        assert_eq!(
            code(RestoreError::SnapshotNotDurable {
                snapshot: entry.clone(),
                restored: None,
                source: locked()
            }),
            ExitCode::from(3)
        );
        assert_eq!(
            code(RestoreError::AfterSnapshot {
                snapshot: entry.clone(),
                late: None,
                source: Box::new(RestoreError::CheckoutFailed {
                    entry: entry.clone(),
                    source: stemma_core::repository::CheckoutError::InTheWay {
                        path: "a".to_owned()
                    }
                })
            }),
            ExitCode::FAILURE,
            "the files are written and no pointer moved: run the same restore again"
        );
        let output = |restored, snapshot| {
            CliError::RestoreOutput {
                restored,
                snapshot,
                source: io::Error::other("x"),
            }
            .exit_code()
        };
        assert_eq!(output(Some(3), None), ExitCode::from(3));
        assert_eq!(output(None, Some(2)), ExitCode::from(3));
        assert_eq!(output(None, None), ExitCode::FAILURE);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A restore that is made, with the entry of the first commit as the one to restore.
    fn restore_args(dir: &Path, dry_run: bool) -> (OpRestoreArgs, u64) {
        let repository = Repository::open(dir).unwrap();
        let commit = repository
            .oplog()
            .entries()
            .unwrap()
            .iter()
            .rev()
            .find(|entry| entry.change().unwrap().operation().as_str() == "commit")
            .unwrap()
            .number();
        let args = OpRestoreArgs {
            number: commit,
            repository: dir.to_path_buf(),
            force: false,
            dry_run,
            actor: Some("human daniel".to_owned()),
        };
        (args, commit)
    }

    #[test]
    fn a_failed_write_after_a_restore_is_code_3_and_names_the_entry_to_undo_it() {
        let (dir, _, _) = committed("restore-write");
        let (args, _) = restore_args(&dir, false);
        let error = restore(args, &mut Failing(io::ErrorKind::Other)).unwrap_err();
        assert_eq!(error.exit_code(), ExitCode::from(3));
        let text = error.to_string();
        assert!(
            text.starts_with("restored as entry ")
                && text.contains("to undo it run `stemma op restore "),
            "{text}"
        );
        let repository = Repository::open(&dir).unwrap();
        let last = repository.oplog().entries().unwrap().pop().unwrap();
        assert_eq!(last.change().unwrap().operation().as_str(), "restore");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_closed_pipe_after_a_restore_is_code_3_but_after_a_plan_it_is_nothing() {
        let (dir, _, _) = committed("restore-pipe");
        let (plan, _) = restore_args(&dir, true);
        assert!(restore(plan, &mut Failing(io::ErrorKind::BrokenPipe)).is_ok());
        let (args, _) = restore_args(&dir, false);
        let error = restore(args, &mut Failing(io::ErrorKind::BrokenPipe)).unwrap_err();
        assert_eq!(error.exit_code(), ExitCode::from(3));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_failed_write_of_a_plan_is_code_1_and_says_nothing_was_restored() {
        let (dir, _, _) = committed("restore-plan-write");
        let (args, _) = restore_args(&dir, true);
        let before = Repository::open(&dir)
            .unwrap()
            .oplog()
            .entries()
            .unwrap()
            .len();
        let error = restore(args, &mut Failing(io::ErrorKind::Other)).unwrap_err();
        assert_eq!(error.exit_code(), ExitCode::FAILURE);
        assert!(
            error
                .to_string()
                .starts_with("nothing was restored or written"),
            "{error}"
        );
        let after = Repository::open(&dir)
            .unwrap()
            .oplog()
            .entries()
            .unwrap()
            .len();
        assert_eq!(after, before, "a plan writes no entry");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_text_of_a_restore_names_the_snapshot_and_the_way_back() {
        let (dir, _, entry) = committed("restore-text");
        assert_eq!(
            restored_text(7, None, None),
            "Nothing to restore: every pointer of entry 7 has its old value already\n"
        );
        assert_eq!(
            restored_text(7, Some(&entry), Some(&entry)),
            "Saved the working copy first as entry 1\n\
             Restored the state before entry 7 as entry 1\nTo undo it: stemma op restore 1\n"
        );
        assert_eq!(
            restored_text(7, Some(&entry), None),
            "Saved the working copy first as entry 1\n\
             Nothing to restore: every pointer of entry 7 has its old value already\n"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_half_made_restore_writes_the_way_to_finish_it_before_its_error() {
        let (dir, entry) = repository_with_entry("restore-half");
        let entry = Box::new(entry);
        let half = RestoreError::AfterSnapshot {
            snapshot: entry.clone(),
            late: None,
            source: Box::new(RestoreError::PartlyMoved {
                entry: entry.clone(),
                source: RefError::Locked { path: dir.clone() },
            }),
        };
        assert!(half.half_made());
        assert_eq!(
            made_text(5, &half),
            "Saved the working copy first as entry 1\n\
             Half made: entry 1 of the restore moved some of the pointers of entry 5\n\
             To finish it: stemma op restore 5\n"
        );
        let not_durable = RestoreError::NotDurable {
            entry: entry.clone(),
            source: RefError::Locked { path: dir.clone() },
        };
        assert!(!not_durable.half_made());
        assert_eq!(
            made_text(5, &not_durable),
            "Restored the state before entry 5 as entry 1\nTo undo it: stemma op restore 1\n"
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
    /// may be written and entered, but not read. It fails where the permissions have no effect
    /// (the tests must not run as root), and a test that skipped itself there would pass without
    /// having tested anything.
    #[cfg(unix)]
    fn a_status_not_durable(name: &str) -> (PathBuf, Status) {
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
        let binds = std::fs::read_dir(&metadata).is_err();
        let result = repository.status(&actor, &AllowAll);
        std::fs::set_permissions(&metadata, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            binds,
            "permissions do not bind this user; do not run the tests as root"
        );
        (dir, result.unwrap())
    }

    /// A status of a repository with a tracked file that changed after the last `track`, so that
    /// the call saved it in a snapshot of its own.
    fn a_status_with_a_snapshot(name: &str) -> (PathBuf, Status) {
        let (dir, _) = repository_with_entry(name);
        let repository = Repository::open(&dir).unwrap();
        let actor: Actor = "human daniel".parse().unwrap();
        std::fs::write(dir.join("a.txt"), "one").unwrap();
        repository
            .track(&["a.txt".to_owned()], &actor, &AllowAll)
            .unwrap();
        std::fs::write(dir.join("a.txt"), "two").unwrap();
        let status = repository.status(&actor, &AllowAll).unwrap();
        assert!(status.snapshot.is_some());
        (dir, status)
    }

    #[cfg(unix)]
    #[test]
    fn a_status_is_written_and_a_snapshot_that_is_not_durable_follows_with_code_3() {
        let (dir, status) = a_status_not_durable("status-late");
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
        let message = error.to_string();
        assert!(
            message.contains("saved as entry") && message.contains("flushing failed"),
            "{message}"
        );
        assert!(
            message.contains("does not make it durable"),
            "running again is no remedy: {message}"
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
        let (dir, late) = a_status_not_durable("status-pipe-late");
        let error = finish_status(late, &mut Failing(io::ErrorKind::BrokenPipe)).unwrap_err();
        assert!(
            matches!(&error, CliError::StatusNotDurable { output: None, .. }),
            "{error}"
        );
        assert_eq!(error.exit_code(), ExitCode::from(3));
        let _ = std::fs::remove_dir_all(dir);
        let (dir, late) = a_status_not_durable("status-output-late");
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
    fn a_status_that_cannot_be_written_is_code_1_and_names_the_entry_of_its_snapshot() {
        let (dir, status) = a_status("status-output");
        assert!(status.snapshot.is_none());
        let mut full = Failing(io::ErrorKind::StorageFull);
        let error = finish_status(status, &mut full).unwrap_err();
        assert!(
            matches!(&error, CliError::StatusOutput { entry: None, .. }),
            "{error}"
        );
        assert_eq!(error.exit_code(), ExitCode::FAILURE);
        assert_eq!(
            error.to_string().split(':').next(),
            Some("could not write the status")
        );
        assert!(!error.to_string().contains("saved first"), "{error}");
        let _ = std::fs::remove_dir_all(dir);

        let (dir, saved) = a_status_with_a_snapshot("status-output-saved");
        let number = saved.snapshot.as_ref().unwrap().entry.number();
        let error = finish_status(saved, &mut full).unwrap_err();
        assert!(
            matches!(&error, CliError::StatusOutput { entry: Some(n), .. } if *n == number),
            "{error}"
        );
        assert_eq!(error.exit_code(), ExitCode::FAILURE);
        assert!(
            error
                .to_string()
                .contains(&format!("saved first as entry {number}")),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_failure_of_reading_after_a_snapshot_is_code_1_and_names_its_entry() {
        let (dir, _) = repository_with_entry("status-read");
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
        let hex = parent.strip_prefix("b3:").unwrap();
        let file = dir
            .join(".stemma/objects/b3")
            .join(&hex[..2])
            .join(&hex[2..]);
        std::fs::remove_file(file).unwrap();

        let error = CliError::Status(repository.status(&actor, &AllowAll).unwrap_err());

        assert_eq!(error.exit_code(), ExitCode::FAILURE);
        assert!(
            error.to_string().contains("saved before as entry"),
            "{error}"
        );
        assert!(!error.to_string().contains("flushing failed"), "{error}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_failure_of_reading_after_a_snapshot_that_was_not_flushed_is_code_3() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, _) = repository_with_entry("status-read-late");
        let repository = Repository::open(&dir).unwrap();
        let actor: Actor = "human daniel".parse().unwrap();
        std::fs::write(dir.join("a.txt"), "one").unwrap();
        repository
            .track(&["a.txt".to_owned()], &actor, &AllowAll)
            .unwrap();
        let _ = repository.commit("base", &actor, &AllowAll).unwrap();
        std::fs::write(dir.join("a.txt"), "two").unwrap();
        let parent = repository
            .refs()
            .resolve_head()
            .unwrap()
            .unwrap()
            .to_string();
        let hex = parent.strip_prefix("b3:").unwrap();
        std::fs::remove_file(
            dir.join(".stemma/objects/b3")
                .join(&hex[..2])
                .join(&hex[2..]),
        )
        .unwrap();
        let metadata = dir.join(".stemma");
        std::fs::set_permissions(&metadata, std::fs::Permissions::from_mode(0o300)).unwrap();
        let binds = std::fs::read_dir(&metadata).is_err();
        let result = repository.status(&actor, &AllowAll);
        std::fs::set_permissions(&metadata, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            binds,
            "permissions do not bind this user; do not run the tests as root"
        );

        let error = CliError::Status(result.unwrap_err());

        assert_eq!(error.exit_code(), ExitCode::from(3));
        assert!(error.to_string().contains("flushing failed"), "{error}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_other_failures_of_a_status_are_code_1() {
        let (dir, _) = repository_with_entry("status-codes");
        for error in [
            StatusError::Update(UpdateError::Denied {
                operation: "status".into(),
                target: "working-copy".into(),
            }),
            StatusError::Ignore(stemma_core::ignore::IgnoreError::Changed { path: dir.clone() }),
            StatusError::Walk(TrackError::InvalidName { path: "x".into() }),
            StatusError::Refs {
                source: RefError::Locked { path: dir.clone() },
                saved: None,
            },
        ] {
            let error = CliError::Status(error);
            assert_eq!(error.exit_code(), ExitCode::FAILURE, "{error}");
        }
        let walk = CliError::Status(StatusError::Walk(TrackError::InvalidName {
            path: "x".into(),
        }));
        assert!(
            walk.to_string().ends_with("; nothing was written"),
            "{walk}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_numbers_of_the_stubs_are_written_as_ranges() {
        assert_eq!(ranges(&[]), "");
        assert_eq!(ranges(&[4]), "4");
        assert_eq!(ranges(&[3, 5, 6, 7, 9, 10]), "3, 5-7, 9-10");
    }

    #[test]
    fn a_path_is_written_so_that_two_different_paths_never_look_alike() {
        // NFC and NFD of the same letter are two names, and both are written differently.
        assert_eq!(escape_path("\u{e9}.txt"), "é.txt");
        assert_eq!(escape_path("e\u{301}.txt"), "e\\u{301}.txt");
        assert_eq!(escape_path("dir/e\u{301}"), "dir/e\\u{301}");
        // A space at the edge of a name is visible, a space inside is not changed.
        assert_eq!(escape_path(" a"), "\\u{20}a");
        assert_eq!(escape_path("a "), "a\\u{20}");
        assert_eq!(escape_path("a b"), "a b");
        assert_eq!(escape_path("d / e"), "d\\u{20}/\\u{20}e");
        // A quote, a backslash, and a control character are escaped, and a letter is not.
        assert_eq!(escape_path("it's"), "it\\'s");
        assert_eq!(escape_path("a\\b"), "a\\\\b");
        assert_eq!(escape_path("a\nb\t"), "a\\nb\\t");
        assert_eq!(escape_path("日本 😀.txt"), "日本 😀.txt");
        assert_eq!(escape_path("d/a.txt"), "d/a.txt");
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
            parse(&["init", "-C", path, "--actor", "human daniel"]),
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
            parse(&["init", "-C", path, "--actor", "human daniel"]),
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
            parse(&["init", "-C", path, "--actor", "human daniel"]),
            &mut Vec::new(),
        )
        .unwrap();
        let closed = run(parse(&["op", "log", "-C", path]), &mut Closed);
        let full = run(parse(&["op", "log", "-C", path]), &mut Full);
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
            parse(&["init", "-C", path, "--actor", "human daniel"]),
            &mut Vec::new(),
        )
        .unwrap();
        for number in 2..=3 {
            let oplog = dir.join(".stemma/oplog");
            let before = std::fs::read(oplog.join(format!("{:012}", number - 1))).unwrap();
            let previous = oplog::Entry::parse(&before).unwrap().hash().unwrap();
            std::fs::write(
                oplog.join(format!("{number:012}")),
                format!(
                    "stemma-oplog 3\nnumber {number}\ntime 1700000000\nactor human daniel\n\
                     previous {previous}\noperation snapshot\nworking-copy - b3:{}\n",
                    "1".repeat(64)
                ),
            )
            .unwrap();
        }
        // The newest entry is written first, so a failing first write names entry 3, with or
        // without a limit; a failure of the second write names the entry that follows it.
        let first = run(parse(&["op", "log", "-C", path]), &mut Full).unwrap_err();
        let limited = run(parse(&["op", "log", "-n", "2", "-C", path]), &mut Full).unwrap_err();
        let second = run(parse(&["op", "log", "-C", path]), &mut FullAfterOne(false)).unwrap_err();
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
        // `/a/b` has no drive on Windows and would be completed, so the path comes from the system.
        let given = std::env::temp_dir().join("a").join("b");
        assert_eq!(absolute(&given), given);
    }

    #[test]
    fn an_anchor_reads_back_what_it_writes_and_nothing_else() {
        let hash = format!("b3:{}", "0a".repeat(32));
        let text = format!("12:{hash}");
        let anchor = Anchor::from_str(&text).unwrap();
        assert_eq!(anchor.to_string(), text);
        for other in [
            format!("012:{hash}"),
            format!("0:{hash}"),
            format!("+12:{hash}"),
            format!("12:{}", hash.to_uppercase()),
            format!("12 {hash}"),
            "12:".to_owned(),
        ] {
            assert!(Anchor::from_str(&other).is_err(), "{other}");
        }
    }
}
