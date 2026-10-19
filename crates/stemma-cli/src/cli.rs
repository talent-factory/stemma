//! Arguments, the actor, and the commands.
//!
//! Exit codes: 0 for success, 1 for a command that fails, and 2 for arguments that are wrong
//! (the code of the argument parser). An error is printed with `{}` as `stemma: <message>`; its
//! message is complete (see the crate documentation of `stemma-core`).

use std::error::Error;
use std::fmt;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use stemma_core::refs::BranchName;
use stemma_core::repository::{InitError, InitOptions, Repository};
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
}

#[derive(Debug, Args)]
struct InitArgs {
    /// The directory of the repository; it is created if it does not exist
    #[arg(default_value = ".")]
    path: PathBuf,

    /// The branch that HEAD points to; the branch exists after the first commit
    #[arg(long, value_name = "NAME", default_value = "main", value_parser = parse_branch)]
    initial_branch: BranchName,

    /// Who runs the command: `human <id>` or
    /// `agent <id> model=<model> session=<session> sponsor=<sponsor>`
    #[arg(long, env = ACTOR_ENV, value_name = "ACTOR")]
    actor: Option<String>,
}

fn parse_branch(name: &str) -> Result<BranchName, String> {
    name.parse::<BranchName>()
        .map_err(|error| error.to_string())
}

/// Why a command fails.
#[derive(Debug)]
enum CliError {
    /// There is no actor, neither as a flag nor in the environment.
    MissingActor,
    /// The text of the actor is not a valid actor line.
    Actor(ActorParseError),
    /// The core refused or could not create the repository.
    Init(InitError),
    /// The result could not be written to the output.
    Output(io::Error),
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingActor => write!(
                f,
                "no actor: pass `--actor \"human <id>\"` or set {ACTOR_ENV}; an agent is \
                 `agent <id> model=<model> session=<session> sponsor=<sponsor>`"
            ),
            Self::Actor(error) => write!(f, "invalid actor: {error}"),
            Self::Init(error) => error.fmt(f),
            Self::Output(error) => write!(f, "could not write the result: {error}"),
        }
    }
}

impl Error for CliError {}

/// Runs the command and writes its result to `out`.
fn run(cli: Cli, out: &mut dyn Write) -> Result<(), CliError> {
    match cli.command {
        Command::Init(args) => {
            let actor = actor(args.actor)?;
            let options = InitOptions::new(args.initial_branch);
            let repository = Repository::init(&args.path, &options, &actor, &AllowAll)
                .map_err(CliError::Init)?;
            writeln!(
                out,
                "Initialized empty Stemma repository in {}",
                repository.metadata_dir().display()
            )
            .map_err(CliError::Output)
        }
    }
}

fn actor(text: Option<String>) -> Result<Actor, CliError> {
    match text {
        // An empty variable counts as not set: a script that exports it empty has no actor.
        Some(text) if !text.is_empty() => text.parse().map_err(CliError::Actor),
        _ => Err(CliError::MissingActor),
    }
}

pub fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        // Help and version go to the output with code 0, a usage error to stderr with code 2.
        Err(error) => error.exit(),
    };
    match run(cli, &mut io::stdout().lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("stemma: {error}");
            ExitCode::FAILURE
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
    fn an_empty_actor_is_missing() {
        assert!(matches!(actor(None), Err(CliError::MissingActor)));
        assert!(matches!(
            actor(Some(String::new())),
            Err(CliError::MissingActor)
        ));
    }

    #[test]
    fn an_actor_line_is_parsed_like_in_a_commit() {
        assert_eq!(
            actor(Some("human daniel".into())).unwrap().to_string(),
            "human daniel"
        );
        let line = "agent claude model=opus-5-5 session=s1 sponsor=daniel";
        assert_eq!(actor(Some(line.into())).unwrap().to_string(), line);
        for bad in ["daniel", "human", "human a b", "agent a", "human dänu"] {
            assert!(
                matches!(actor(Some(bad.into())), Err(CliError::Actor(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn error_messages_are_complete() {
        let missing = CliError::MissingActor.to_string();
        assert!(
            missing.contains("--actor") && missing.contains(ACTOR_ENV),
            "{missing}"
        );
        let invalid = actor(Some("human a b".into())).unwrap_err().to_string();
        assert!(invalid.starts_with("invalid actor: "), "{invalid}");
        assert!(invalid.len() > "invalid actor: ".len());
    }
}
