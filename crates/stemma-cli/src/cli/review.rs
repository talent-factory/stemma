//! The commands of the review path: `pr`, `check`, and `lease` (ADR-004, ADR-019).
//!
//! Each command is a thin layer over one operation of the core. The core decides and writes the
//! entry; this module reads the arguments, loads the register of an enforcing repository, signs
//! where the rule asks for a signature, and prints the result.
//!
//! Exit codes follow the rest of the program: 1 for a refusal, 5 for damage, 6 for a lock that is
//! held (try again), and 3 if an entry is written and the rest of the command could not finish: a
//! proof that is not written, a lease file that is not replaced, or a merge whose move is not
//! flushed. A script must not repeat a command that ends with 3.

use std::ffi::OsString;
use std::fmt;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Args, Subcommand};

use stemma_core::failure::{Classify, FailureKind};
use stemma_core::lease::DEFAULT_MINUTES;
use stemma_core::object::CommitId;
use stemma_core::oplog::{CheckResult, Entry};
use stemma_core::policy::RiskLevel;
use stemma_core::pull::{NotCounted, PullState};
use stemma_core::refs::BranchName;
use stemma_core::register::{self, Mode, ProofFile, Register};
use stemma_core::repository::{CheckError, GateError, LeaseError, PullError, Repository, Revision};
use stemma_core::signature::{self, PayloadKind, SshKeygen};
use stemma_core::store::{ObjectStore, StoreError};
use stemma_core::{Actor, AllowAll};

use super::{actor, mask_controls, open, parse_revision, short_id, CliError, ACTOR_ENV};

/// Why a command of the review path fails: the message, and the exit code that it ends with.
#[derive(Debug)]
pub(super) struct ReviewError {
    message: String,
    code: u8,
}

impl ReviewError {
    fn refused(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: 1,
        }
    }

    fn of_kind(message: impl Into<String>, kind: FailureKind) -> Self {
        let code = match kind {
            FailureKind::Damaged => 5,
            FailureKind::Locked => 6,
            _ => 1,
        };
        Self {
            message: message.into(),
            code,
        }
    }

    fn incomplete(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: 3,
        }
    }

    pub(super) fn exit_code(&self) -> ExitCode {
        ExitCode::from(self.code)
    }
}

impl fmt::Display for ReviewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<ReviewError> for CliError {
    fn from(error: ReviewError) -> Self {
        Self::Review(error)
    }
}

fn pull_error(error: PullError) -> ReviewError {
    let kind = match &error {
        PullError::Refs(inner) => inner.kind(),
        PullError::Log(inner) => inner.kind(),
        _ => FailureKind::Other,
    };
    if matches!(error, PullError::ProofNotWritten { .. }) {
        return ReviewError::incomplete(error.to_string());
    }
    ReviewError::of_kind(error.to_string(), kind)
}

fn gate_error(error: GateError) -> ReviewError {
    let kind = match &error {
        GateError::Refs(inner) => inner.kind(),
        GateError::Log(inner) => inner.kind(),
        _ => FailureKind::Other,
    };
    if matches!(
        error,
        GateError::MoveFailed { .. }
            | GateError::PullNotRecorded { .. }
            | GateError::NotDurable { .. }
    ) {
        return ReviewError::incomplete(error.to_string());
    }
    ReviewError::of_kind(error.to_string(), kind)
}

fn check_error(error: CheckError) -> ReviewError {
    let kind = match &error {
        CheckError::Refs(inner) => inner.kind(),
        CheckError::Log(inner) => inner.kind(),
        _ => FailureKind::Other,
    };
    ReviewError::of_kind(error.to_string(), kind)
}

fn lease_error(error: LeaseError) -> ReviewError {
    let kind = match &error {
        LeaseError::Refs(inner) => inner.kind(),
        LeaseError::Log(inner) => inner.kind(),
        _ => FailureKind::Other,
    };
    if matches!(error, LeaseError::Io { .. }) {
        return ReviewError::incomplete(error.to_string());
    }
    ReviewError::of_kind(error.to_string(), kind)
}

/// The directory of the repository and the actor, the two arguments that every command has.
#[derive(Debug, Args)]
pub(super) struct Common {
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
}

/// The key that signs, for a command that writes an entry whose rule depends on who acts.
#[derive(Debug, Args)]
pub(super) struct Signing {
    /// The key file of the actor, for `ssh-keygen -Y sign`: the private key, or the public key if
    /// `ssh-agent` holds it. Required in an enforcing repository, where the entry is signed; an
    /// open repository has no register and does not read it
    #[arg(long, value_name = "KEY_FILE")]
    key: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub(super) enum PrCommand {
    /// Open a pull request that would merge a branch into `main`
    Open(PrOpenArgs),
    /// End an open pull request without a merge
    Close(PrNumberArgs),
    /// Approve the head of an open pull request
    Approve(PrApproveArgs),
    /// Merge an open pull request into `main` through the merge gate
    Merge(PrMergeArgs),
    /// List the pull requests, in the order of their openings
    List(PrListArgs),
}

#[derive(Debug, Args)]
pub(super) struct PrOpenArgs {
    #[command(flatten)]
    common: Common,

    /// The branch to merge; its tip at this moment is the head of the pull request
    #[arg(value_name = "BRANCH")]
    branch: String,

    /// A reviewer, an actor line of the register; repeat the flag for more than one
    #[arg(long = "reviewer", value_name = "ACTOR")]
    reviewers: Vec<String>,

    /// A commit that this pull request repairs (a revision: a commit ID, a branch, or a tag)
    #[arg(long, value_name = "COMMIT", value_parser = parse_revision)]
    repairs: Option<Revision>,
}

#[derive(Debug, Args)]
pub(super) struct PrNumberArgs {
    #[command(flatten)]
    common: Common,

    /// The number of the pull request, as `pr list` shows it
    #[arg(value_name = "NUMBER")]
    number: u64,
}

#[derive(Debug, Args)]
pub(super) struct PrApproveArgs {
    #[command(flatten)]
    common: Common,

    #[command(flatten)]
    signing: Signing,

    /// The number of the pull request, as `pr list` shows it
    #[arg(value_name = "NUMBER")]
    number: u64,
}

#[derive(Debug, Args)]
pub(super) struct PrMergeArgs {
    #[command(flatten)]
    common: Common,

    /// The number of the pull request, as `pr list` shows it
    #[arg(value_name = "NUMBER")]
    number: u64,

    /// The risk level of the change, from 0 to 3. The rule table of the gate asks for the checks
    /// that this level needs. The policy file does not derive the level yet, so the default is the
    /// strictest one
    #[arg(long, value_name = "LEVEL", default_value_t = 3, value_parser = clap::value_parser!(u32).range(0..=3))]
    level: u32,
}

#[derive(Debug, Args)]
pub(super) struct PrListArgs {
    #[command(flatten)]
    common: Common,
}

#[derive(Debug, Subcommand)]
pub(super) enum CheckCommand {
    /// Write the result of a check run for a commit
    Record(CheckRecordArgs),
    /// List the results of the checks, in the order of their entries
    List(CheckListArgs),
}

#[derive(Debug, Args)]
pub(super) struct CheckRecordArgs {
    #[command(flatten)]
    common: Common,

    #[command(flatten)]
    signing: Signing,

    /// The commit that was tested (a revision: a commit ID, a branch, or a tag)
    #[arg(value_name = "COMMIT", value_parser = parse_revision)]
    commit: Revision,

    /// The result of the run
    #[arg(long, value_name = "RESULT", value_parser = ["pass", "fail"])]
    result: String,

    /// The tip of `main` that the commit was merged onto for the run; without it, the commit was
    /// tested as it is
    #[arg(long, value_name = "COMMIT", value_parser = parse_revision)]
    base: Option<Revision>,
}

#[derive(Debug, Args)]
pub(super) struct CheckListArgs {
    #[command(flatten)]
    common: Common,
}

#[derive(Debug, Subcommand)]
pub(super) enum LeaseCommand {
    /// Show who holds the lease of a branch
    Show(LeaseBranchArgs),
    /// Take the lease of a branch: acquire it if it is free or expired, renew it if you hold it
    Take(LeaseTakeArgs),
    /// Give the lease back
    Release(LeaseBranchArgs),
    /// Take the lease from another actor (owner only), and say why
    Override(LeaseOverrideArgs),
}

#[derive(Debug, Args)]
pub(super) struct LeaseBranchArgs {
    #[command(flatten)]
    common: Common,

    /// The branch
    #[arg(value_name = "BRANCH")]
    branch: String,
}

#[derive(Debug, Args)]
pub(super) struct LeaseTakeArgs {
    #[command(flatten)]
    common: Common,

    /// The branch
    #[arg(value_name = "BRANCH")]
    branch: String,

    /// How long the lease lasts, in minutes
    #[arg(long, value_name = "MINUTES", default_value_t = DEFAULT_MINUTES, value_parser = clap::value_parser!(u32).range(1..))]
    minutes: u32,
}

#[derive(Debug, Args)]
pub(super) struct LeaseOverrideArgs {
    #[command(flatten)]
    common: Common,

    #[command(flatten)]
    signing: Signing,

    /// The branch
    #[arg(value_name = "BRANCH")]
    branch: String,

    /// Why the lease is taken: one line, at most 1 KiB
    #[arg(long, value_name = "TEXT")]
    reason: String,

    /// How long the lease of the owner lasts, in minutes
    #[arg(long, value_name = "MINUTES", default_value_t = DEFAULT_MINUTES, value_parser = clap::value_parser!(u32).range(1..))]
    minutes: u32,
}

fn branch_name(text: &str) -> Result<BranchName, ReviewError> {
    BranchName::try_new(text)
        .map_err(|error| ReviewError::refused(format!("branch {text:?}: {error}")))
}

/// The mode of the repository: the register of an enforcing one, `None` for an open one.
fn register_of(repository: &Repository) -> Result<Option<Register>, ReviewError> {
    match Mode::of(repository) {
        Ok(Mode::Open) => Ok(None),
        Ok(Mode::Enforcing(register)) => Ok(Some(register)),
        Err(error) => Err(ReviewError::of_kind(
            format!("the actor register cannot be read: {error}"),
            FailureKind::Damaged,
        )),
    }
}

/// The commit that a revision names: a commit ID that the store holds, or the tip of a branch or
/// a tag.
fn commit_of(repository: &Repository, revision: &Revision) -> Result<CommitId, ReviewError> {
    let refs = repository.refs();
    let read =
        |error: stemma_core::refs::RefError| ReviewError::of_kind(error.to_string(), error.kind());
    let found = match revision {
        Revision::Commit(id) => repository
            .objects()
            .resolve_commit(id)
            .map_err(|error: StoreError| ReviewError::of_kind(error.to_string(), error.kind()))?,
        Revision::Branch(name) => refs.branch(name).map_err(read)?,
        Revision::Tag(name) => refs.tag(name).map_err(read)?,
        Revision::Name(name) => match (
            refs.branch(name).map_err(read)?,
            refs.tag(name).map_err(read)?,
        ) {
            (Some(_), Some(_)) => {
                return Err(ReviewError::refused(format!(
                    "{name} is a branch and a tag; write `heads/{name}` or `tags/{name}`"
                )))
            }
            (branch, tag) => branch.or(tag),
        },
    };
    found.ok_or_else(|| ReviewError::refused(format!("{revision} names no commit")))
}

/// Signs the hash of the entry with the key of `actor` and writes the proof file, as the core asks
/// the caller of an entry whose rule depends on who acts. Nothing is done in an open repository.
fn prove(
    repository: &Repository,
    register: Option<&Register>,
    signing: &Signing,
    actor: &Actor,
    entry: &Entry,
) -> Result<(), ReviewError> {
    if register.is_none() {
        return Ok(());
    }
    let key = signing.key.as_deref().ok_or_else(missing_key)?;
    let hash = entry.hash().ok_or_else(|| {
        ReviewError::incomplete(format!(
            "entry {} is written, but it has no hash to sign",
            entry.number()
        ))
    })?;
    let signature = signature::sign(
        &SshKeygen::new(key),
        PayloadKind::LogEntry,
        hash.to_string().as_bytes(),
    )
    .map_err(|error| {
        ReviewError::incomplete(format!(
            "entry {} is written, but cannot be signed: {error}; without a proof the entry does \
             not count",
            entry.number()
        ))
    })?;
    let proof = ProofFile {
        actor: actor.clone(),
        signature,
    };
    register::write_proof(&repository.metadata_dir(), entry.number(), &proof).map_err(|error| {
        ReviewError::incomplete(format!(
            "entry {} is written, but its proof is not: {error}",
            entry.number()
        ))
    })
}

fn missing_key() -> ReviewError {
    ReviewError::refused(
        "this repository is enforcing, so the entry is signed: pass `--key KEY_FILE`",
    )
}

fn write_out(out: &mut dyn Write, text: &str, made: Option<u64>) -> Result<(), CliError> {
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe && made.is_none() => Ok(()),
        Err(error) => Err(match made {
            Some(entry) => ReviewError::incomplete(format!(
                "entry {entry} is written, but the result could not be written: {error}"
            )),
            None => ReviewError::refused(format!("cannot write the result: {error}")),
        }
        .into()),
        Ok(()) => Ok(()),
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

pub(super) fn pr(command: PrCommand, out: &mut dyn Write) -> Result<(), CliError> {
    match command {
        PrCommand::Open(args) => pr_open(args, out),
        PrCommand::Close(args) => pr_close(args, out),
        PrCommand::Approve(args) => pr_approve(args, out),
        PrCommand::Merge(args) => pr_merge(args, out),
        PrCommand::List(args) => pr_list(args, out),
    }
}

fn pr_open(args: PrOpenArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let who = actor(args.common.actor)?;
    let branch = branch_name(&args.branch)?;
    let mut reviewers = Vec::new();
    for text in &args.reviewers {
        reviewers.push(
            text.parse::<Actor>()
                .map_err(|error| ReviewError::refused(format!("--reviewer {text:?}: {error}")))?,
        );
    }
    let repository = open(&args.common.repository)?;
    let register = register_of(&repository)?;
    let repairs = args
        .repairs
        .as_ref()
        .map(|revision| commit_of(&repository, revision))
        .transpose()?;
    let entry = repository
        .open_pull(
            &branch,
            &who,
            reviewers,
            repairs,
            register.as_ref(),
            &AllowAll,
        )
        .map_err(pull_error)?;
    let text = format!(
        "Opened pull request {} for branch {branch}.\n",
        entry.number()
    );
    write_out(out, &text, Some(entry.number()))
}

fn pr_close(args: PrNumberArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let who = actor(args.common.actor)?;
    let repository = open(&args.common.repository)?;
    let register = register_of(&repository)?;
    let entry = repository
        .close_pull(args.number, &who, register.as_ref(), &AllowAll)
        .map_err(pull_error)?;
    let text = format!("Closed pull request {}.\n", args.number);
    write_out(out, &text, Some(entry.number()))
}

fn pr_approve(args: PrApproveArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let who = actor(args.common.actor)?;
    let repository = open(&args.common.repository)?;
    let register = register_of(&repository)?;
    let signer = args.signing.key.as_deref().map(SshKeygen::new);
    if register.is_some() && signer.is_none() {
        return Err(missing_key().into());
    }
    let entry = repository
        .approve_pull(
            args.number,
            &who,
            register.as_ref(),
            signer
                .as_ref()
                .map(|signer| signer as &dyn signature::Signer),
            &AllowAll,
        )
        .map_err(pull_error)?;
    let text = format!("Approved the head of pull request {}.\n", args.number);
    write_out(out, &text, Some(entry.number()))
}

fn pr_merge(args: PrMergeArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let who = actor(args.common.actor)?;
    let level = RiskLevel::from_number(args.level).expect("the parser limits the level to 0..=3");
    let repository = open(&args.common.repository)?;
    let register = register_of(&repository)?;
    let merged = repository
        .merge_gate(args.number, level, register.as_ref(), &who, &AllowAll)
        .map_err(gate_error)?;
    let text = if merged.repaired {
        format!(
            "Recorded the merge of pull request {}: main held the change already, at {}.\n",
            args.number,
            short_id(merged.commit)
        )
    } else {
        format!(
            "Merged pull request {} into main: {} on top of {}.\n",
            args.number,
            short_id(merged.commit),
            short_id(merged.tip)
        )
    };
    write_out(out, &text, Some(merged.merged.number()))
}

fn pr_list(args: PrListArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let repository = open(&args.common.repository)?;
    let register = register_of(&repository)?;
    let pulls = repository.pull_requests().map_err(pull_error)?;
    let mut text = String::new();
    for pull in &pulls {
        let state = match pull.state() {
            PullState::Open => "open",
            PullState::Closed { .. } => "closed",
            PullState::Merged { .. } => "merged",
        };
        let approvals = if pull.is_open() {
            let recount = repository
                .recount_approvals(pull.number(), register.as_ref())
                .map_err(pull_error)?;
            let void = recount.void().len();
            let voided = recount
                .void()
                .iter()
                .filter(|(_, why)| matches!(why, NotCounted::OtherHead))
                .count();
            if void == 0 {
                format!(" approvals {}", recount.count())
            } else {
                format!(
                    " approvals {} ({void} void, {voided} for an older head)",
                    recount.count()
                )
            }
        } else {
            String::new()
        };
        text.push_str(&format!(
            "{} {state} {} {} by {}{approvals}\n",
            pull.number(),
            mask_controls(pull.branch().as_str()),
            short_id(*pull.head()),
            mask_controls(&pull.opener().to_string()),
        ));
    }
    write_out(out, &text, None)
}

pub(super) fn check(command: CheckCommand, out: &mut dyn Write) -> Result<(), CliError> {
    match command {
        CheckCommand::Record(args) => check_record(args, out),
        CheckCommand::List(args) => check_list(args, out),
    }
}

fn check_record(args: CheckRecordArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let who = actor(args.common.actor)?;
    let repository = open(&args.common.repository)?;
    let register = register_of(&repository)?;
    if register.is_some() && args.signing.key.is_none() {
        return Err(missing_key().into());
    }
    let commit = commit_of(&repository, &args.commit)?;
    let base = args
        .base
        .as_ref()
        .map(|revision| commit_of(&repository, revision))
        .transpose()?;
    let result = if args.result == "pass" {
        CheckResult::Pass
    } else {
        CheckResult::Fail
    };
    let entry = repository
        .record_check(commit, base, result, &who, &AllowAll)
        .map_err(check_error)?;
    prove(&repository, register.as_ref(), &args.signing, &who, &entry)?;
    let text = format!(
        "Recorded {} for {} in entry {}.\n",
        result.name(),
        short_id(commit),
        entry.number()
    );
    write_out(out, &text, Some(entry.number()))
}

fn check_list(args: CheckListArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let repository = open(&args.common.repository)?;
    let checks = repository.checks().map_err(check_error)?;
    let mut text = String::new();
    for check in &checks {
        let base = check.base().map_or_else(
            || "as it is".to_owned(),
            |base| format!("on {}", short_id(*base)),
        );
        text.push_str(&format!(
            "{} {} {} {base} by {}\n",
            check.number(),
            check.result().name(),
            short_id(*check.commit()),
            mask_controls(&check.writer().to_string()),
        ));
    }
    write_out(out, &text, None)
}

pub(super) fn lease(command: LeaseCommand, out: &mut dyn Write) -> Result<(), CliError> {
    match command {
        LeaseCommand::Show(args) => lease_show(args, out),
        LeaseCommand::Take(args) => lease_take(args, out),
        LeaseCommand::Release(args) => lease_release(args, out),
        LeaseCommand::Override(args) => lease_override(args, out),
    }
}

/// A repository with a lease needs a register that lists more than one identity: in an open
/// repository, and in one with a single identity, no lease applies and none is written.
fn require_leases(register: Option<&Register>) -> Result<&Register, ReviewError> {
    register
        .filter(|register| stemma_core::lease::applies(Some(register), false))
        .ok_or_else(|| {
            ReviewError::refused(
                "no lease applies: a lease exists only in an enforcing repository whose register \
                 lists more than one identity",
            )
        })
}

fn minutes_left(expires: i64) -> i64 {
    ((expires - now()).max(0) + 59) / 60
}

fn lease_show(args: LeaseBranchArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let branch = branch_name(&args.branch)?;
    let repository = open(&args.common.repository)?;
    let text = match repository.lease(&branch).map_err(lease_error)? {
        None => format!("{branch} has no lease.\n"),
        Some(lease) if lease.is_expired(now()) => format!(
            "{branch}: the lease of {} has expired; the next writer takes it over.\n",
            mask_controls(&lease.holder().to_string())
        ),
        Some(lease) => format!(
            "{branch} is held by {}, for {} more minutes.\n",
            mask_controls(&lease.holder().to_string()),
            minutes_left(lease.expires())
        ),
    };
    write_out(out, &text, None)
}

fn lease_take(args: LeaseTakeArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let who = actor(args.common.actor)?;
    let branch = branch_name(&args.branch)?;
    let repository = open(&args.common.repository)?;
    let register = register_of(&repository)?;
    require_leases(register.as_ref())?;
    let claimed = repository
        .claim_lease(&branch, &who, args.minutes, &AllowAll)
        .map_err(lease_error)?;
    let what = match &claimed.entry {
        None => "Renewed",
        Some(_) => "Took",
    };
    let text = format!(
        "{what} the lease of {branch} for {} minutes.\n",
        args.minutes
    );
    write_out(out, &text, claimed.entry.as_ref().map(Entry::number))
}

fn lease_release(args: LeaseBranchArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let who = actor(args.common.actor)?;
    let branch = branch_name(&args.branch)?;
    let repository = open(&args.common.repository)?;
    let register = register_of(&repository)?;
    require_leases(register.as_ref())?;
    let entry = repository
        .release_lease(&branch, &who, &AllowAll)
        .map_err(lease_error)?;
    let text = format!("Released the lease of {branch}.\n");
    write_out(out, &text, Some(entry.number()))
}

fn lease_override(args: LeaseOverrideArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let who = actor(args.common.actor)?;
    let branch = branch_name(&args.branch)?;
    let repository = open(&args.common.repository)?;
    let register = register_of(&repository)?;
    let register = require_leases(register.as_ref())?;
    if args.signing.key.is_none() {
        return Err(missing_key().into());
    }
    let claimed = repository
        .override_lease(
            &branch,
            &who,
            register,
            args.minutes,
            &args.reason,
            &AllowAll,
        )
        .map_err(lease_error)?;
    let entry = claimed.entry.as_ref().expect("an override writes an entry");
    prove(&repository, Some(register), &args.signing, &who, entry)?;
    let text = format!(
        "Took the lease of {branch} over for {} minutes.\n",
        args.minutes
    );
    write_out(out, &text, Some(entry.number()))
}
