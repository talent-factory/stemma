//! The statements of a pull request on a hub (ADR-025, STEMMA-312): how a command names a pull
//! request for people, how its statements reach the hub, and how the lines of the hub are read.
//!
//! **The name of a pull request.** A pull request is named by its number in this clone, as
//! `pr list` shows it, or by a prefix of at least [`MIN_PREFIX`] hexadecimal digits of the hash
//! of its opening. The number is short and holds in this clone only; the hash holds in every
//! clone and on the hub, offline as well, so two people speak of the same pull request with it.
//! The running number of the hub is not a name: it exists only once the hub holds the opening,
//! and it differs between hubs. A text of decimal digits that is also a prefix of another pull
//! request is refused as ambiguous.
//!
//! **The submission.** A statement reaches a hub only if it names the repository of the hub.
//! `pr open --remote NAME` and `check record --remote NAME` make it so; the approvals and the
//! closing of a pull request name the repository that its opening names. Such a statement is
//! submitted at once, after its entry is written, unless `--offline` says otherwise; one that
//! could not be submitted counts in the clone all the same, and `pr submit` sends it later. A
//! submission sends every statement of the pull request again: the hub answers one that it
//! holds already with its lines, so the repetition is safe, and the statements that an offline
//! clone made since reach the hub in their order.
//!
//! **The reading.** `pr show --remote NAME` reads the statements that the hub holds and shows
//! those of the pull request with the lines of the hub: its running number, its time, the last
//! record entry, and the identity that the register names.
//!
//! Exit codes (chapter 12): after an entry was written, 1 if the hub refused a statement and 3
//! if the statement could not be submitted (the hub is unreachable, busy, or answers no message
//! of the protocol); `pr submit` and `pr show` write nothing and end with 1 for a refusal and a
//! failure of the hub, 5 for damage, and 6 for a hub that is busy or limits the requests.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::PathBuf;

use clap::Args;

use stemma_core::failure::{Classify, FailureKind};
use stemma_core::oplog::Entry;
use stemma_core::protocol::held::HeldStatement;
use stemma_core::protocol::statement::{Body, Statement, StatementHash, StatementKind};
use stemma_core::protocol::{ErrorMessage, RepositoryId};
use stemma_core::pull::{PullRequest, PullState};
use stemma_core::register::Register;
use stemma_core::remote::{RemoteConfig, RemoteName};
use stemma_core::repository::{Outcome, Repository, Submission, SubmitError};
use stemma_core::{Actor, AllowAll};

use super::network::{connect, parse_remote_name, with_status, NetworkError};
use super::review::{pull_error, register_of};
use super::{actor, mask_controls, open, short_id, CliError, ACTOR_ENV};
use crate::op_log::{quote, JSON_SCHEMA};

/// The fewest hexadecimal digits of the hash of an opening that name a pull request.
pub(super) const MIN_PREFIX: usize = 8;

/// The digits of the hash of an opening that `pr list` and the messages show.
const SHOWN_DIGITS: usize = 12;

/// The name of a pull request for people: the first digits of the hash of its opening.
pub(super) fn short_name(id: StatementHash) -> String {
    id.hex().chars().take(SHOWN_DIGITS).collect()
}

/// The pull request that `text` names: its number in this clone, or a prefix of at least
/// [`MIN_PREFIX`] hexadecimal digits of the hash of its opening, with or without `b3:`.
pub(super) fn resolve_pull(
    repository: &Repository,
    register: Option<&Register>,
    text: &str,
) -> Result<PullRequest, CliError> {
    let pulls = repository.pull_requests(register).map_err(pull_error)?;
    let digits = text.strip_prefix("b3:").unwrap_or(text);
    let decimal = !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    let hex = digits.len() >= MIN_PREFIX
        && digits
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if !decimal && !hex {
        return Err(NetworkError::refused(format!(
            "`{}` names no pull request: give its number in this clone, as `pr list` shows \
             it, or at least {MIN_PREFIX} lowercase hexadecimal digits of the hash of its \
             opening",
            mask_controls(text)
        ))
        .into());
    }
    let number: Option<u64> = if decimal { text.parse().ok() } else { None };
    let found: Vec<&PullRequest> = pulls
        .iter()
        .filter(|pull| {
            Some(pull.number()) == number || (hex && pull.id().hex().starts_with(digits))
        })
        .collect();
    match found.as_slice() {
        [pull] => Ok((*pull).clone()),
        [] => Err(NetworkError::refused(format!(
            "there is no pull request `{}`; `pr list` shows them",
            mask_controls(text)
        ))
        .into()),
        several => {
            let names: Vec<String> = several
                .iter()
                .map(|pull| format!("{} ({})", pull.number(), short_name(pull.id())))
                .collect();
            Err(NetworkError::refused(format!(
                "`{}` names more than one pull request: {}; give more digits of the hash",
                mask_controls(text),
                names.join(", ")
            ))
            .into())
        }
    }
}

/// The configuration of the remote `name`.
pub(super) fn named_remote(
    repository: &Repository,
    name: &RemoteName,
) -> Result<RemoteConfig, NetworkError> {
    repository
        .remote(name)
        .map_err(|error| NetworkError::of_kind(error.to_string(), error.kind()))?
        .ok_or_else(|| {
            NetworkError::refused(format!(
                "there is no remote `{name}`; add it with `remote add`"
            ))
        })
}

/// The remote whose hub holds the repository `id`: the one that `wanted` names, which must
/// have that repository, or the only one that has it.
fn remote_for(
    repository: &Repository,
    id: RepositoryId,
    wanted: Option<&RemoteName>,
) -> Result<(RemoteName, RemoteConfig), NetworkError> {
    if let Some(name) = wanted {
        let config = named_remote(repository, name)?;
        if config.repository != id {
            return Err(NetworkError::refused(format!(
                "the remote `{name}` has the repository {}, and the pull request is for {id}",
                config.repository
            )));
        }
        return Ok((name.clone(), config));
    }
    let mut found: Vec<(RemoteName, RemoteConfig)> = repository
        .remotes()
        .map_err(|error| NetworkError::of_kind(error.to_string(), error.kind()))?
        .into_iter()
        .filter(|(_, config)| config.repository == id)
        .collect();
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => Err(NetworkError::refused(format!(
            "no remote has the repository {id}; add it with `remote add`"
        ))),
        _ => Err(NetworkError::refused(format!(
            "more than one remote has the repository {id}; pass `--remote NAME`"
        ))),
    }
}

/// The word and the facts of a refusal, as the `refused` line of the wire carries them.
fn refusal_line(outcome: &Outcome) -> Option<(String, String)> {
    let Outcome::Refused(reason) = outcome else {
        return None;
    };
    let encoded = ErrorMessage::StatementRefused(reason.clone()).encode();
    let text = String::from_utf8_lossy(&encoded).into_owned();
    let line = text
        .lines()
        .find_map(|line| line.strip_prefix("refused "))
        .unwrap_or("")
        .to_owned();
    let (word, facts) = line.split_once(' ').unwrap_or((line.as_str(), "-"));
    Some((word.to_owned(), facts.to_owned()))
}

fn kind_name(kind: StatementKind) -> String {
    kind.to_string()
}

/// One line for each answer of the hub.
fn submissions_text(submitted: &[Submission]) -> String {
    let mut text = String::new();
    for submission in submitted {
        let name = short_name(submission.hash);
        let kind = kind_name(submission.kind);
        match &submission.outcome {
            Outcome::Held(held) => {
                let _ = writeln!(
                    text,
                    "  {kind} {name}: held as {} (record {}, {})",
                    held.number(),
                    held.record(),
                    mask_controls(&held.actor().to_string())
                );
            }
            Outcome::Refused(reason) => {
                let word = refusal_line(&submission.outcome).map_or_else(String::new, |(w, _)| w);
                let _ = writeln!(text, "  {kind} {name}: refused ({word}): {reason}");
            }
        }
    }
    text
}

fn held_json(held: &HeldStatement) -> String {
    format!(
        "{{\"number\":{},\"time\":{},\"register\":{},\"record\":{},\"actor\":{}}}",
        held.number(),
        held.time(),
        quote(&held.register().to_string()),
        held.record(),
        quote(&held.actor().to_string())
    )
}

fn submission_json(submission: &Submission) -> String {
    let held = match &submission.outcome {
        Outcome::Held(held) => held_json(held),
        Outcome::Refused(_) => "null".to_owned(),
    };
    let refused = match (&submission.outcome, refusal_line(&submission.outcome)) {
        (Outcome::Refused(reason), Some((word, facts))) => format!(
            "{{\"reason\":{},\"facts\":{},\"message\":{}}}",
            quote(&word),
            quote(&facts),
            quote(&reason.to_string())
        ),
        _ => "null".to_owned(),
    };
    format!(
        "{{\"hash\":{},\"kind\":{},\"held\":{held},\"refused\":{refused}}}",
        quote(&submission.hash.to_string()),
        quote(&kind_name(submission.kind))
    )
}

/// The hashes of the statements of `pull` that this clone counts.
pub(super) fn hashes_of_pull(
    repository: &Repository,
    register: Option<&Register>,
    pull: &PullRequest,
) -> Result<Vec<StatementHash>, CliError> {
    let statements = repository
        .statements_of_pull(pull.number(), register)
        .map_err(|error| NetworkError::of_kind(error.to_string(), submit_kind(&error)))?;
    Ok(statements.iter().map(Statement::hash).collect())
}

/// Whether the hub refused one of the statements.
fn any_refused(submitted: &[Submission]) -> bool {
    submitted
        .iter()
        .any(|submission| matches!(submission.outcome, Outcome::Refused(_)))
}

/// Connects to the remote and submits the statements.
fn send(
    repository: &Repository,
    remote: &RemoteName,
    config: &RemoteConfig,
    hashes: &[StatementHash],
    key: Option<&std::path::Path>,
    who: &Actor,
) -> Result<Vec<Submission>, (Vec<Submission>, NetworkError, bool)> {
    let mut transport = connect(config, key).map_err(|error| (Vec::new(), error, true))?;
    repository
        .submit_statements(remote, hashes, &mut transport, who, &AllowAll)
        .map_err(|error| {
            let unreachable = error.is_unreachable();
            let submitted = error.submitted().to_vec();
            let kind = submit_kind(&error);
            (
                submitted,
                with_status(error.to_string(), kind, &transport),
                unreachable,
            )
        })
}

fn submit_kind(error: &SubmitError) -> FailureKind {
    match error {
        SubmitError::Refs(inner) => inner.kind(),
        SubmitError::Statements(_) | SubmitError::Missing(_) => FailureKind::Damaged,
        SubmitError::After { source, .. } => submit_kind(source),
        _ => FailureKind::Other,
    }
}

/// Where a statement that was just made goes: the remote of its hub, or none if `--offline`
/// keeps it in the clone for now.
pub(super) struct Destination {
    remote: RemoteName,
    config: RemoteConfig,
    offline: bool,
}

impl Destination {
    /// The remote `name`, for `pr open --remote` and `check record --remote`.
    pub(super) fn named(
        repository: &Repository,
        name: &RemoteName,
        offline: bool,
    ) -> Result<Self, CliError> {
        let config = named_remote(repository, name)?;
        Ok(Self {
            remote: name.clone(),
            config,
            offline,
        })
    }

    /// The remote of the hub of `pull`, for `pr approve` and `pr close`, or `None` for a pull
    /// request of this clone only.
    pub(super) fn of_pull(
        repository: &Repository,
        pull: &PullRequest,
        wanted: Option<&RemoteName>,
        offline: bool,
    ) -> Result<Option<Self>, CliError> {
        let Some(id) = pull.repository() else {
            if let Some(name) = wanted {
                return Err(NetworkError::refused(format!(
                    "pull request {} counts in this clone only: its opening names no \
                     repository of a hub, so its statements cannot go to `{name}`",
                    pull.number()
                ))
                .into());
            }
            return Ok(None);
        };
        let (remote, config) = remote_for(repository, id, wanted)?;
        Ok(Some(Self {
            remote,
            config,
            offline,
        }))
    }

    /// The repository of the hub, which the statement names.
    pub(super) fn repository(&self) -> RepositoryId {
        self.config.repository
    }

    /// Submits `hashes` after `entry` was written, and returns the lines to print after the
    /// line of the command, and the error to end with if the submission did not finish. `pull`
    /// is the name that `pr submit` takes to send them later.
    pub(super) fn submit_after(
        &self,
        repository: &Repository,
        entry: &Entry,
        hashes: &[StatementHash],
        later: &str,
        key: Option<&std::path::Path>,
        who: &Actor,
    ) -> (String, Option<CliError>) {
        if self.offline {
            return (
                format!(
                    "The statement counts in this clone; `stemma pr submit {later}` sends it to \
                     `{}`.\n",
                    self.remote
                ),
                None,
            );
        }
        match send(repository, &self.remote, &self.config, hashes, key, who) {
            Ok(submitted) => {
                let mut text = format!("Submitted to `{}`:\n", self.remote);
                text.push_str(&submissions_text(&submitted));
                let error = any_refused(&submitted).then(|| {
                    NetworkError::refused(format!(
                        "the hub of `{}` refused a statement; entry {} is written and counts \
                         in this clone",
                        self.remote,
                        entry.number()
                    ))
                    .into()
                });
                (text, error)
            }
            Err((submitted, error, _)) => {
                let mut text = String::new();
                if !submitted.is_empty() {
                    text = format!("Submitted to `{}`:\n", self.remote);
                    text.push_str(&submissions_text(&submitted));
                }
                let message = error.message().replace("; nothing was written", "");
                let error = NetworkError::incomplete(format!(
                    "entry {} is written and counts in this clone, but the submission to `{}` \
                     did not finish: {message}; `stemma pr submit {later}` sends it later",
                    entry.number(),
                    self.remote
                ));
                (text, Some(error.into()))
            }
        }
    }
}

// The arguments of `pr submit` and `pr show`.

/// The arguments that `pr submit` and `pr show` share.
#[derive(Debug, Args)]
pub(super) struct HubArgs {
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
    /// environment sets STEMMA_ACTOR
    #[arg(long, env = ACTOR_ENV, value_name = "ACTOR")]
    actor: Option<OsString>,

    /// The key file for the sign-in, for a remote that asks for one
    #[arg(long, value_name = "KEY_FILE")]
    key: Option<PathBuf>,

    /// Print the result as one JSON object (ADR-018)
    #[arg(long)]
    json: bool,

    /// The pull request: its number in this clone, or at least 8 hexadecimal digits of the
    /// hash of its opening
    #[arg(value_name = "PULL")]
    pull: String,
}

#[derive(Debug, Args)]
pub(super) struct PrSubmitArgs {
    #[command(flatten)]
    hub: HubArgs,

    /// The remote to submit to; without it, the remote whose repository the opening names
    #[arg(long, value_name = "NAME", value_parser = parse_remote_name)]
    remote: Option<RemoteName>,
}

#[derive(Debug, Args)]
pub(super) struct PrShowArgs {
    #[command(flatten)]
    hub: HubArgs,

    /// Read the statements that the hub of this remote holds, and show their lines; without
    /// it, the command shows what this clone holds and makes no request
    #[arg(long, value_name = "NAME", value_parser = parse_remote_name)]
    remote: Option<RemoteName>,
}

fn write_out(out: &mut dyn Write, text: &str) -> Result<(), CliError> {
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => {
            Err(NetworkError::refused(format!("cannot write the result: {error}")).into())
        }
        Ok(()) => Ok(()),
    }
}

/// `pr submit`: sends every statement of the pull request to its hub, and prints the answer to
/// each. It writes nothing into the clone.
pub(super) fn pr_submit(args: PrSubmitArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let who = actor(args.hub.actor)?;
    let repository = open(&args.hub.repository)?;
    let register = register_of(&repository)?;
    let pull = resolve_pull(&repository, register.as_ref(), &args.hub.pull)?;
    let destination = Destination::of_pull(&repository, &pull, args.remote.as_ref(), false)?
        .ok_or_else(|| {
            NetworkError::refused(format!(
                "pull request {} counts in this clone only: its opening names no repository \
                 of a hub; open it again with `pr open --remote NAME` to bring it to a hub",
                pull.number()
            ))
        })?;
    let hashes = hashes_of_pull(&repository, register.as_ref(), &pull)?;
    let result = send(
        &repository,
        &destination.remote,
        &destination.config,
        &hashes,
        args.hub.key.as_deref(),
        &who,
    );
    let (submitted, failure) = match result {
        Ok(submitted) => (submitted, None),
        Err((submitted, error, _)) => (submitted, Some(error)),
    };
    let text = if args.hub.json {
        let items: Vec<String> = submitted.iter().map(submission_json).collect();
        format!(
            "{{\"schema\":{JSON_SCHEMA},\"pull\":{},\"id\":{},\"remote\":{},\
             \"complete\":{},\"statements\":[{}]}}\n",
            pull.number(),
            quote(&pull.id().to_string()),
            quote(destination.remote.as_str()),
            failure.is_none(),
            items.join(",")
        )
    } else {
        let mut text = format!(
            "Pull request {} ({}) to `{}`:\n",
            pull.number(),
            short_name(pull.id()),
            destination.remote
        );
        text.push_str(&submissions_text(&submitted));
        text
    };
    if let Some(error) = failure {
        let _ = out.write_all(text.as_bytes());
        return Err(error.into());
    }
    write_out(out, &text)?;
    if any_refused(&submitted) {
        return Err(NetworkError::refused(format!(
            "the hub of `{}` refused a statement",
            destination.remote
        ))
        .into());
    }
    Ok(())
}

/// Whether a held statement is one of `pull`: its opening, an approval or the closing that
/// names it, or a check of one of `heads`.
fn of_pull(
    held: &HeldStatement,
    pull: &PullRequest,
    heads: &[stemma_core::object::CommitId],
) -> bool {
    match held.statement().body() {
        Body::Opening(_) => held.hash() == pull.id(),
        Body::Approval(approval) => approval.pull() == pull.id(),
        Body::Closing(closing) => closing.pull() == pull.id(),
        Body::Check(check) => heads.contains(&check.commit()),
        _ => false,
    }
}

/// `pr show`: the pull request and its statements, with the lines of the hub if `--remote`
/// names one.
pub(super) fn pr_show(args: PrShowArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let repository = open(&args.hub.repository)?;
    let register = register_of(&repository)?;
    let pull = resolve_pull(&repository, register.as_ref(), &args.hub.pull)?;
    let statements = repository
        .statements_of_pull(pull.number(), register.as_ref())
        .map_err(|error| NetworkError::of_kind(error.to_string(), submit_kind(&error)))?;
    let hashes: Vec<StatementHash> = statements.iter().map(Statement::hash).collect();
    let kinds: Vec<StatementKind> = statements.iter().map(Statement::kind).collect();
    let mut heads = vec![*pull.head()];
    for statement in &statements {
        if let Body::Approval(approval) = statement.body() {
            heads.push(approval.head());
        }
    }
    let held: Option<Vec<HeldStatement>> = match &args.remote {
        None => None,
        Some(name) => {
            let who = actor(args.hub.actor)?;
            let config = named_remote(&repository, name)?;
            if pull.repository() != Some(config.repository) {
                return Err(NetworkError::refused(format!(
                    "the remote `{name}` has the repository {}, and pull request {} is {}",
                    config.repository,
                    pull.number(),
                    pull.repository().map_or_else(
                        || "for this clone only".to_owned(),
                        |id| format!("for {id}")
                    )
                ))
                .into());
            }
            let mut transport = connect(&config, args.hub.key.as_deref())?;
            let all = repository
                .held_statements(name, &mut transport, &who, &AllowAll)
                .map_err(|error| with_status(error.to_string(), submit_kind(&error), &transport))?;
            Some(
                all.into_iter()
                    .filter(|h| of_pull(h, &pull, &heads))
                    .collect(),
            )
        }
    };
    let held_of = |hash: &StatementHash| {
        held.as_ref()
            .and_then(|held| held.iter().find(|held| held.hash() == *hash))
    };
    let only_on_hub: Vec<&HeldStatement> = held
        .iter()
        .flatten()
        .filter(|held| !hashes.contains(&held.hash()))
        .collect();
    let state = match pull.state() {
        PullState::Open => "open",
        PullState::Closed { .. } => "closed",
        PullState::Merged { .. } => "merged",
    };
    let text = if args.hub.json {
        let mut items: Vec<String> = Vec::new();
        for (hash, kind) in hashes.iter().zip(&kinds) {
            items.push(format!(
                "{{\"hash\":{},\"kind\":{},\"in_clone\":true,\"held\":{}}}",
                quote(&hash.to_string()),
                quote(&kind_name(*kind)),
                held_of(hash).map_or_else(|| "null".to_owned(), held_json)
            ));
        }
        for held in &only_on_hub {
            items.push(format!(
                "{{\"hash\":{},\"kind\":{},\"in_clone\":false,\"held\":{}}}",
                quote(&held.hash().to_string()),
                quote(&kind_name(held.statement().kind())),
                held_json(held)
            ));
        }
        format!(
            "{{\"schema\":{JSON_SCHEMA},\"pull\":{},\"id\":{},\"branch\":{},\"head\":{},\
             \"state\":\"{state}\",\"opener\":{},\"repository\":{},\"remote\":{},\
             \"statements\":[{}]}}\n",
            pull.number(),
            quote(&pull.id().to_string()),
            quote(pull.branch().as_str()),
            quote(&pull.head().to_string()),
            quote(&pull.opener().to_string()),
            pull.repository()
                .map_or_else(|| "null".to_owned(), |id| quote(&id.to_string())),
            args.remote
                .as_ref()
                .map_or_else(|| "null".to_owned(), |name| quote(name.as_str())),
            items.join(",")
        )
    } else {
        let mut text = format!(
            "Pull request {} ({}), {state}: {} at {} by {}\n",
            pull.number(),
            short_name(pull.id()),
            mask_controls(pull.branch().as_str()),
            short_id(*pull.head()),
            mask_controls(&pull.opener().to_string())
        );
        match pull.repository() {
            Some(id) => {
                let _ = writeln!(text, "For the repository {id} of a hub.");
            }
            None => text.push_str("For this clone only.\n"),
        }
        for (hash, kind) in hashes.iter().zip(&kinds) {
            let hub = match (&held, held_of(hash)) {
                (None, _) => String::new(),
                (Some(_), None) => ": not on the hub".to_owned(),
                (Some(_), Some(held)) => format!(
                    ": held as {} at {} (record {}, {})",
                    held.number(),
                    held.time(),
                    held.record(),
                    mask_controls(&held.actor().to_string())
                ),
            };
            let _ = writeln!(text, "  {} {}{hub}", kind_name(*kind), short_name(*hash));
        }
        for held in &only_on_hub {
            let _ = writeln!(
                text,
                "  {} {}: only on the hub, held as {} at {} (record {}, {})",
                kind_name(held.statement().kind()),
                short_name(held.hash()),
                held.number(),
                held.time(),
                held.record(),
                mask_controls(&held.actor().to_string())
            );
        }
        text
    };
    write_out(out, &text)
}
