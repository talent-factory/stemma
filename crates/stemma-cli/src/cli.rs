//! Arguments, the actor, and the commands.
//!
//! Exit codes: 0 for success, 1 for a command that fails and has changed nothing, 2 for arguments
//! that are wrong (the code of the argument parser), and 3 for a command that has done its work
//! but could not finish: the repository exists, but flushing it to disk or writing the result
//! failed. A script must not remove or repeat what such a command made. A command that only reads
//! (`op log`) has made nothing, so a failed write of its output is code 1; some of the output may
//! already be written, and a script discards it. A failing command prints
//! `stemma: <message>`, a complete message (see the crate documentation of `stemma-core`). The
//! argument parser prints its own errors.

use std::error::Error;
use std::fmt;
use std::io::{self, Write};
use std::num::NonZeroUsize;
use std::path::{self, Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

use crate::op_log;
use stemma_core::object::METADATA_DIR;
use stemma_core::oplog::OpLogError;
use stemma_core::refs::BranchName;
use stemma_core::repository::{InitError, InitOptions, OpenError, Repository};
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
    /// The operation log cannot be read.
    OpLog(OpLogError),
    /// The entries were read, but the output could not be written.
    Write(io::Error),
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
            Self::Init(InitError::NotDurable { .. }) | Self::Output { .. } => ExitCode::from(3),
            Self::MissingActor
            | Self::EmptyActor
            | Self::Actor(_)
            | Self::Init(_)
            | Self::Open(_)
            | Self::OpLog(_)
            | Self::Write(_) => ExitCode::FAILURE,
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
            Self::Open(error) => error.fmt(f),
            Self::OpLog(error) => error.fmt(f),
            Self::Write(source) => write!(f, "could not write the result: {source}"),
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
        Command::Op {
            command: OpCommand::Log(args),
        } => show_op_log(args, out),
    }
}

/// Lists the newest entries. The names of all files are listed, which finds a gap or a foreign
/// file; only the files of the entries that are shown are read, so damage in an older entry goes
/// unnoticed. Everything is read before anything is
/// written, so an entry that is damaged gives an error and no partial list.
fn show_op_log(args: OpLogArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let repository = Repository::open(&args.path).map_err(CliError::Open)?;
    let log = repository.oplog();
    let numbers = log.numbers().map_err(CliError::OpLog)?;
    let entries = numbers
        .iter()
        .rev()
        .take(args.limit.get())
        .map(|number| log.read(*number))
        .collect::<Result<Vec<_>, _>>()
        .map_err(CliError::OpLog)?;
    for entry in &entries {
        let written = if args.json {
            writeln!(out, "{}", op_log::json(entry))
        } else {
            write!(out, "{}", op_log::text(entry))
        };
        match written {
            // The reader of a pipe, such as `head`, has had enough: that is not a failure.
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => break,
            Err(error) => return Err(CliError::Write(error)),
            Ok(()) => {}
        }
    }
    Ok(())
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
            CliError::Write(io_error()),
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
        assert!(matches!(error, CliError::Write(_)), "{error}");
        assert!(error.to_string().contains("disk full"), "{error}");
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
