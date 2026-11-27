//! The commands of the review path: `pr`, `check`, and `lease` (ADR-004, ADR-019).
//!
//! Each command is a thin layer over one operation of the core. The core decides and writes the
//! entry; this module reads the arguments, loads the register of an enforcing repository, and
//! prints the result. `pr open`, `pr close`, `pr approve`, and `check record` hand a signer to
//! the core, which signs the statement of the command (ADR-025) and checks the signature against
//! the register before it writes anything. `lease override` signs the entry itself and writes its
//! proof (`prove`), after it tried the key (`probe_key`).
//!
//! Exit codes follow the rest of the program: 1 for a refusal, 5 for damage, 6 for a lock that is
//! held (try again), and 3 if an entry is written and the rest of the command could not finish: a
//! proof of `lease override` that is not written, a lease file that is not replaced, or a merge
//! whose move is not flushed. What to do after 3 depends on the command: `pr merge` repeated
//! finishes the move, unless the message says that the pull request is merged and a worktree did
//! not follow `main` (then `switch main` in that worktree brings it along), and a repeated
//! `lease` command decides again from the lease file. The key of `lease override` is tried
//! against the register before anything is written, so exit 3 after the proof step is left for
//! a failure that the probe cannot see.

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
use stemma_core::oplog::{CheckResult, Entry, OpLogError};
use stemma_core::policy::RiskLevel;
use stemma_core::pull::{NotCounted, PullState};
use stemma_core::refs::BranchName;
use stemma_core::register::{self, Identity, Mode, ProofFile, Register};
use stemma_core::repository::{
    Behind, CheckError, Followed, GateError, LeaseError, PullError, Repository, Revision,
};
use stemma_core::signature::{self, PayloadKind, SshKeygen};
use stemma_core::statements::SetError;
use stemma_core::store::{ObjectStore, StoreError};
use stemma_core::{Actor, AllowAll};

use super::network::parse_remote_name;
use super::statements::{self, resolve_pull, short_name, Destination, PrShowArgs, PrSubmitArgs};
use super::{
    actor, handled_text, mask_controls, open, parse_revision, saved_text, short_id, CliError,
    ACTOR_ENV,
};
use stemma_core::remote::RemoteName;

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

/// A log error that says the entry may be in the log although its flush is not confirmed.
fn not_flushed(error: &OpLogError) -> bool {
    matches!(error, OpLogError::NotFlushed { .. })
}

// The core classifies the errors of the storage layers, but not the errors of the review path that
// wrap them. These functions look through the wrapping, so that damage ends with 5 and a held lock
// with 6 wherever it surfaces.

fn pull_kind(error: &PullError) -> FailureKind {
    match error {
        PullError::Refs(inner) => inner.kind(),
        PullError::Log(inner) => inner.kind(),
        PullError::History(inner) => inner.kind(),
        PullError::Store(inner) => inner.kind(),
        PullError::Lease(inner) => lease_kind(inner),
        PullError::Statements(inner) => set_kind(inner),
        _ => FailureKind::Other,
    }
}

/// A file of the set that is no statement under the name of its hash is damage; a failure of
/// the file system is not.
fn set_kind(error: &SetError) -> FailureKind {
    match error {
        SetError::Malformed { .. }
        | SetError::WrongName { .. }
        | SetError::TooLarge { .. }
        | SetError::UnexpectedFile(_) => FailureKind::Damaged,
        _ => FailureKind::Other,
    }
}

fn check_kind(error: &CheckError) -> FailureKind {
    match error {
        CheckError::Refs(inner) => inner.kind(),
        CheckError::Log(inner) => inner.kind(),
        CheckError::Store(inner) => inner.kind(),
        CheckError::History(inner) => inner.kind(),
        CheckError::Statements(inner) => set_kind(inner),
        _ => FailureKind::Other,
    }
}

fn lease_kind(error: &LeaseError) -> FailureKind {
    match error {
        LeaseError::Refs(inner) => inner.kind(),
        LeaseError::Log(inner) => inner.kind(),
        LeaseError::Corrupt { .. } => FailureKind::Damaged,
        _ => FailureKind::Other,
    }
}

fn gate_kind(error: &GateError) -> FailureKind {
    match error {
        GateError::Refs(inner) => inner.kind(),
        GateError::Log(inner) => inner.kind(),
        GateError::Store(inner) => inner.kind(),
        GateError::History(inner) => inner.kind(),
        GateError::Merge(inner) => inner.kind(),
        GateError::Update(inner) => inner.kind(),
        GateError::Pull(inner) => pull_kind(inner),
        GateError::Check(inner) => check_kind(inner),
        _ => FailureKind::Other,
    }
}

pub(super) fn pull_error(error: PullError) -> ReviewError {
    if matches!(&error, PullError::Log(inner) if not_flushed(inner)) {
        return ReviewError::incomplete(error.to_string());
    }
    // The statement is signed and checked against the register before the first write.
    if matches!(
        error,
        PullError::SignatureRequired
            | PullError::KeyNotActive
            | PullError::Sign(_)
            | PullError::Proof(_)
    ) {
        return ReviewError::refused(format!("{error}; nothing is written"));
    }
    ReviewError::of_kind(error.to_string(), pull_kind(&error))
}

fn gate_error(error: GateError) -> ReviewError {
    // A lock that is held, or a `main` that moves while the gate works: try again.
    let contended = matches!(&error, GateError::Contended { .. });
    if contended {
        return ReviewError::of_kind(error.to_string(), FailureKind::Locked);
    }
    // The repair path records the merge itself, so its flush error arrives as `Pull`.
    let unflushed = match &error {
        GateError::Log(inner) => not_flushed(inner),
        GateError::Pull(PullError::Log(inner)) => not_flushed(inner),
        _ => false,
    };
    if unflushed
        || matches!(
            error,
            GateError::MoveFailed { .. }
                | GateError::PullNotRecorded { .. }
                | GateError::NotDurable { .. }
        )
    {
        return ReviewError::incomplete(error.to_string());
    }
    ReviewError::of_kind(error.to_string(), gate_kind(&error))
}

fn check_error(error: CheckError) -> ReviewError {
    if matches!(&error, CheckError::Log(inner) if not_flushed(inner)) {
        return ReviewError::incomplete(format!(
            "{error}; look at `check list`, and repeat the command if the entry is not there"
        ));
    }
    // The statement is signed and checked against the register before the first write.
    if matches!(
        error,
        CheckError::SignatureRequired
            | CheckError::KeyNotActive
            | CheckError::Sign(_)
            | CheckError::Proof(_)
    ) {
        return ReviewError::refused(format!("{error}; nothing is written"));
    }
    ReviewError::of_kind(error.to_string(), check_kind(&error))
}

fn lease_error(error: LeaseError) -> ReviewError {
    // Only a failed write can follow an entry; a failed read wrote nothing.
    let wrote = match &error {
        LeaseError::Io { operation, .. } => !operation.starts_with("read"),
        LeaseError::Log(inner) => not_flushed(inner),
        _ => false,
    };
    if wrote {
        return ReviewError::incomplete(format!(
            "{error}; an entry may be in the log without the lease file following it: repeat \
             the command, which decides again from the lease file"
        ));
    }
    ReviewError::of_kind(error.to_string(), lease_kind(&error))
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
    /// environment sets STEMMA_ACTOR (the commands that only read ignore it)
    #[arg(long, env = ACTOR_ENV, value_name = "ACTOR")]
    actor: Option<OsString>,
}

/// The key that signs, for a command that makes a signed statement or writes an entry whose rule
/// depends on who acts.
#[derive(Debug, Args)]
pub(super) struct Signing {
    /// The key file of the actor, for `ssh-keygen -Y sign`: the private key, or the public key if
    /// `ssh-agent` holds it. Required in an enforcing repository, where the command signs; an
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
    /// Submit the statements of a pull request to its hub
    Submit(PrSubmitArgs),
    /// Show a pull request and its statements, with the lines of the hub
    Show(PrShowArgs),
}

/// Where the statement of a command goes besides this clone (STEMMA-312).
#[derive(Debug, Args)]
pub(super) struct Submitting {
    /// Make the statement for the hub of this remote and submit it at once. `pr approve` and
    /// `pr close` take the remote whose repository the opening names without it
    #[arg(long, value_name = "NAME", value_parser = parse_remote_name)]
    remote: Option<RemoteName>,

    /// Make the statement for the hub, but do not submit it now; `pr submit` sends it later
    #[arg(long)]
    offline: bool,
}

/// The pull request that a command acts on.
#[derive(Debug, Args)]
pub(super) struct PullArg {
    /// The pull request: its number in this clone, as `pr list` shows it, or at least 8
    /// hexadecimal digits of the hash of its opening
    #[arg(value_name = "PULL")]
    pull: String,
}

#[derive(Debug, Args)]
pub(super) struct PrOpenArgs {
    #[command(flatten)]
    common: Common,

    #[command(flatten)]
    signing: Signing,

    /// The branch to merge; its tip at this moment is the head of the pull request
    #[arg(value_name = "BRANCH")]
    branch: String,

    /// A reviewer, an actor line of the register; repeat the flag for more than one
    #[arg(long = "reviewer", value_name = "ACTOR")]
    reviewers: Vec<String>,

    /// A commit that this pull request repairs (a revision: a commit ID, a branch, a tag, or a
    /// remote reference `remotes/REMOTE/heads/NAME` or `remotes/REMOTE/tags/NAME`)
    #[arg(long, value_name = "COMMIT", value_parser = parse_revision)]
    repairs: Option<Revision>,

    #[command(flatten)]
    submitting: Submitting,
}

#[derive(Debug, Args)]
pub(super) struct PrNumberArgs {
    #[command(flatten)]
    common: Common,

    #[command(flatten)]
    signing: Signing,

    #[command(flatten)]
    pull: PullArg,

    #[command(flatten)]
    submitting: Submitting,
}

#[derive(Debug, Args)]
pub(super) struct PrApproveArgs {
    #[command(flatten)]
    common: Common,

    #[command(flatten)]
    signing: Signing,

    #[command(flatten)]
    pull: PullArg,

    #[command(flatten)]
    submitting: Submitting,
}

#[derive(Debug, Args)]
pub(super) struct PrMergeArgs {
    #[command(flatten)]
    common: Common,

    #[command(flatten)]
    pull: PullArg,

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

    /// The commit that was tested (a revision: a commit ID, a branch, a tag, or a remote
    /// reference `remotes/REMOTE/heads/NAME` or `remotes/REMOTE/tags/NAME`)
    #[arg(value_name = "COMMIT", value_parser = parse_revision)]
    commit: Revision,

    /// The result of the run
    #[arg(long, value_name = "RESULT", value_parser = ["pass", "fail"])]
    result: String,

    /// The tip of `main` that the commit was merged onto for the run; without it, the commit was
    /// tested as it is
    #[arg(long, value_name = "COMMIT", value_parser = parse_revision)]
    base: Option<Revision>,

    #[command(flatten)]
    submitting: Submitting,
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
pub(super) fn register_of(repository: &Repository) -> Result<Option<Register>, ReviewError> {
    match Mode::of(repository) {
        Ok(Mode::Open) => Ok(None),
        Ok(Mode::Enforcing(register)) => Ok(Some(register)),
        Err(error) => Err(ReviewError::of_kind(
            format!("the actor register cannot be read: {error}"),
            FailureKind::Damaged,
        )),
    }
}

/// The commit that a revision names: a commit ID that the store holds, the tip of a branch or
/// a tag, or the commit of a remote reference. A bare name that is both a branch and a tag is
/// refused; `heads/` or `tags/` decide.
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
        Revision::Remote(name) => refs
            .remote_ref(name)
            .map_err(read)?
            .map(|reference| reference.commit),
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

/// Tries the key before an entry is written: a key that cannot sign, or that the register does not
/// accept as an active key of `actor`, is a refusal that leaves the log and the lease file alone,
/// not an entry without a proof. The probe is signed in the namespace of an approval, because the
/// register checks a new approval against the keys of the actor without an entry number.
fn probe_key(
    register: Option<&Register>,
    signing: &Signing,
    actor: &Actor,
) -> Result<(), ReviewError> {
    let Some(register) = register else {
        return Ok(());
    };
    let key = signing.key.as_deref().ok_or_else(missing_key)?;
    let refuse = |why: String| ReviewError::refused(format!("{why}; nothing is written"));
    let probe = b"probe";
    let signature = signature::sign(&SshKeygen::new(key), PayloadKind::Approval, probe)
        .map_err(|error| refuse(format!("the key cannot sign: {error}")))?;
    let verified = register
        .verify_approval(actor, probe, &signature, None)
        .map_err(|error| refuse(format!("the key does not prove {actor}: {error}")))?;
    if !register.is_active_key(&Identity::of(actor), verified.key()) {
        return Err(refuse(format!("the key of {actor} is not active")));
    }
    Ok(())
}

/// Signs the hash of the entry with the key of `actor`, checks the signature and the key against
/// the register, and only then writes the proof file, as the core asks the caller of an entry whose
/// rule depends on who acts. Every failure is exit 3, because the entry is written already.
/// Nothing is done in an open repository.
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
    // The reader refuses a proof that the register does not accept, and so does the writer: a
    // wrong key must not leave a file that `op verify` reports as damage.
    let register = register.expect("an enforcing repository has a register");
    let verified = register
        .verify_log_entry(
            actor,
            entry.number(),
            hash.to_string().as_bytes(),
            &signature,
        )
        .map_err(|error| {
            ReviewError::incomplete(format!(
                "entry {} is written, but the key does not prove it for {actor}: {error}; no \
                 proof is written, and the entry does not count",
                entry.number()
            ))
        })?;
    if !register.is_active_key(&Identity::of(actor), verified.key()) {
        return Err(ReviewError::incomplete(format!(
            "entry {} is written, but the key of {actor} is not active; no proof is written, \
             and the entry does not count",
            entry.number()
        )));
    }
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
        "this repository is enforcing, so the command signs: pass `--key KEY_FILE`; nothing is \
         written",
    )
}

/// The signer of a command that makes a signed statement: the key of `--key` in an enforcing
/// repository, where it is required, and none in an open one, which does not read the key.
fn signer_of(
    register: Option<&Register>,
    signing: &Signing,
) -> Result<Option<SshKeygen>, ReviewError> {
    match (register, signing.key.as_deref()) {
        (None, _) => Ok(None),
        (Some(_), Some(key)) => Ok(Some(SshKeygen::new(key))),
        (Some(_), None) => Err(missing_key()),
    }
}

fn as_signer(signer: &Option<SshKeygen>) -> Option<&dyn signature::Signer> {
    signer
        .as_ref()
        .map(|signer| signer as &dyn signature::Signer)
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
        PrCommand::Submit(args) => statements::pr_submit(args, out),
        PrCommand::Show(args) => statements::pr_show(args, out),
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
    let signer = signer_of(register.as_ref(), &args.signing)?;
    let repairs = args
        .repairs
        .as_ref()
        .map(|revision| commit_of(&repository, revision))
        .transpose()?;
    let destination = match &args.submitting.remote {
        Some(name) => Some(Destination::named(
            &repository,
            name,
            args.submitting.offline,
        )?),
        None if args.submitting.offline => return Err(offline_alone()),
        None => None,
    };
    let entry = match &destination {
        Some(destination) => repository.open_pull_for(
            destination.repository(),
            &branch,
            &who,
            reviewers,
            repairs,
            register.as_ref(),
            as_signer(&signer),
            &AllowAll,
        ),
        None => repository.open_pull(
            &branch,
            &who,
            reviewers,
            repairs,
            register.as_ref(),
            as_signer(&signer),
            &AllowAll,
        ),
    }
    .map_err(pull_error)?;
    let id = entry
        .statement()
        .map_or_else(String::new, |hash| format!(" ({})", short_name(hash)));
    let text = format!(
        "Opened pull request {}{id} for branch {branch}.\n",
        entry.number()
    );
    let hashes: Vec<_> = entry.statement().into_iter().collect();
    let later = entry.number().to_string();
    let submitted = destination.map(|destination| {
        destination.submit_after(
            &repository,
            &entry,
            &hashes,
            &later,
            args.signing.key.as_deref(),
            &who,
        )
    });
    finish(out, text, &entry, submitted)
}

/// `--offline` says only where a statement for a hub goes, so it needs the hub.
fn offline_alone() -> CliError {
    ReviewError::refused(
        "`--offline` makes the statement for a hub without submitting it: pass `--remote NAME` \
         as well; nothing is written",
    )
    .into()
}

/// Writes the line of a command that wrote `entry`, with the lines of its submission, and ends
/// with the error of the submission if it did not finish.
fn finish(
    out: &mut dyn Write,
    mut text: String,
    entry: &Entry,
    submitted: Option<(String, Option<CliError>)>,
) -> Result<(), CliError> {
    let error = submitted.and_then(|(lines, error)| {
        text.push_str(&lines);
        error
    });
    match error {
        Some(error) => {
            let _ = out.write_all(text.as_bytes());
            Err(error)
        }
        None => write_out(out, &text, Some(entry.number())),
    }
}

/// Submits the statements of `pull` after a command wrote `entry`, if its opening names a hub
/// and `--offline` does not keep them back.
fn submit_pull_after(
    repository: &Repository,
    register: Option<&Register>,
    pull: &stemma_core::pull::PullRequest,
    submitting: &Submitting,
    signing: &Signing,
    who: &Actor,
    entry: &Entry,
) -> Result<Option<(String, Option<CliError>)>, CliError> {
    let Some(destination) = Destination::of_pull(
        repository,
        pull,
        submitting.remote.as_ref(),
        submitting.offline,
    )?
    else {
        return Ok(None);
    };
    let hashes = statements::hashes_of_pull(repository, register, pull)?;
    Ok(Some(destination.submit_after(
        repository,
        entry,
        &hashes,
        &pull.number().to_string(),
        signing.key.as_deref(),
        who,
    )))
}

fn pr_close(args: PrNumberArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let who = actor(args.common.actor)?;
    let repository = open(&args.common.repository)?;
    let register = register_of(&repository)?;
    let signer = signer_of(register.as_ref(), &args.signing)?;
    let pull = resolve_pull(&repository, register.as_ref(), &args.pull.pull)?;
    // The destination is known before the entry, so a remote that is missing refuses first.
    Destination::of_pull(&repository, &pull, args.submitting.remote.as_ref(), true)?;
    let entry = repository
        .close_pull(
            pull.number(),
            &who,
            register.as_ref(),
            as_signer(&signer),
            &AllowAll,
        )
        .map_err(pull_error)?;
    let text = format!(
        "Closed pull request {} ({}).\n",
        pull.number(),
        short_name(pull.id())
    );
    let submitted = submit_pull_after(
        &repository,
        register.as_ref(),
        &pull,
        &args.submitting,
        &args.signing,
        &who,
        &entry,
    );
    finish(
        out,
        text,
        &entry,
        submitted.unwrap_or_else(|error| Some((String::new(), Some(error)))),
    )
}

fn pr_approve(args: PrApproveArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let who = actor(args.common.actor)?;
    let repository = open(&args.common.repository)?;
    let register = register_of(&repository)?;
    let signer = signer_of(register.as_ref(), &args.signing)?;
    let pull = resolve_pull(&repository, register.as_ref(), &args.pull.pull)?;
    Destination::of_pull(&repository, &pull, args.submitting.remote.as_ref(), true)?;
    let entry = repository
        .approve_pull(
            pull.number(),
            &who,
            register.as_ref(),
            as_signer(&signer),
            &AllowAll,
        )
        .map_err(pull_error)?;
    let text = format!(
        "Approved the head of pull request {} ({}).\n",
        pull.number(),
        short_name(pull.id())
    );
    let submitted = submit_pull_after(
        &repository,
        register.as_ref(),
        &pull,
        &args.submitting,
        &args.signing,
        &who,
        &entry,
    );
    finish(
        out,
        text,
        &entry,
        submitted.unwrap_or_else(|error| Some((String::new(), Some(error)))),
    )
}

/// Merges a pull request through the gate, and then brings the worktree that has `main` checked
/// out to the new tip (STEMMA-261). The gate moves the branch only; the second step is a switch
/// to `main` in that worktree, with an entry of its own. A worktree stays behind with a line that
/// says so, and the command still ends with 0, whenever the second step wrote nothing: the
/// working copy belongs to another actor, its directory cannot be opened, or the switch was
/// refused (a file in the way, a held lock, a command of the worktree that stopped). The merge is
/// complete then. If the switch wrote an entry and failed after it, or is made and not known to be
/// on disk, the result of the merge is written first and the command ends with 3; repeating it
/// does not help, as the pull request is merged. `switch main` in the worktree brings it along.
fn pr_merge(args: PrMergeArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let who = actor(args.common.actor)?;
    let level = RiskLevel::from_number(args.level).expect("the parser limits the level to 0..=3");
    let repository = open(&args.common.repository)?;
    let register = register_of(&repository)?;
    let number = resolve_pull(&repository, register.as_ref(), &args.pull.pull)?.number();
    let merged = repository
        .merge_gate(number, level, register.as_ref(), &who, &AllowAll)
        .map_err(gate_error)?;
    let mut text = if merged.repaired {
        format!(
            "Recorded the merge of pull request {}: main held the change already, at {}.\n",
            number,
            short_id(merged.commit)
        )
    } else {
        format!(
            "Merged pull request {} into main: {} on top of {}.\n",
            number,
            short_id(merged.commit),
            short_id(merged.tip)
        )
    };
    let stays = "it stays on the old tip, and `stemma switch main` there brings it along";
    let failed = |message: String| {
        ReviewError::incomplete(format!(
            "pull request {} is merged (entry {}), but {message}",
            number,
            merged.merged.number()
        ))
    };
    // `op restore` of the entry of the switch runs in the worktree that moved.
    let here = repository.worktree_name().unwrap_or("main").to_owned();
    let undo = |worktree: &str, path: &std::path::Path, entry: u64| {
        if worktree == here {
            format!("To undo it: stemma op restore {entry}\n")
        } else {
            format!(
                "To undo it: stemma op restore {entry} -C {}\n",
                mask_controls(&path.display().to_string())
            )
        }
    };
    let late = match repository.follow_main(&who, &AllowAll) {
        Ok(Followed::NoWorktree | Followed::UpToDate { .. }) => None,
        Ok(Followed::Done {
            worktree,
            path,
            snapshot,
            entry,
            handled,
        }) => {
            text.push_str(&format!(
                "{}Brought the working copy of the worktree {} to the tip of main as entry {}\n{}{}",
                saved_text(snapshot.as_deref()),
                mask_controls(&worktree),
                entry.number(),
                handled_text(&handled, true),
                undo(&worktree, &path, entry.number())
            ));
            None
        }
        // The switch wrote an entry and did not finish, or it is made and a flush is not known.
        Ok(Followed::Behind {
            worktree,
            path,
            reason: Behind::Failed(error),
        }) if error.entry().is_some() || error.snapshot().is_some() => {
            let name = mask_controls(&worktree);
            if !error.made() {
                Some(failed(format!(
                    "the worktree {name} did not follow `main`: {error}; `stemma switch main` \
                     there brings it along"
                )))
            } else {
                if let Some(entry) = error.entry() {
                    text.push_str(&format!(
                        "{}Brought the working copy of the worktree {name} to the tip of main \
                         as entry {}\n{}",
                        saved_text(error.snapshot()),
                        entry.number(),
                        undo(&worktree, &path, entry.number())
                    ));
                }
                Some(failed(format!(
                    "the worktree {name} followed `main`, and that is not known to be on disk: \
                     {error}"
                )))
            }
        }
        Ok(Followed::Behind {
            worktree,
            path,
            reason,
        }) => {
            // Nothing of the second step was written: the worktree stays as it was.
            let why = match &reason {
                Behind::Failed(error) => format!("but it did not follow: {error}"),
                other => format!("and {other}"),
            };
            text.push_str(&format!(
                "The worktree {} at {} has main checked out, {why}; {stays}.\n",
                mask_controls(&worktree),
                mask_controls(&path.display().to_string())
            ));
            None
        }
        Ok(Followed::Unknown { damaged }) => {
            text.push_str(&format!(
                "The state of these worktrees cannot be read: {}. One of them may have main \
                 checked out and stay on the old tip; `stemma worktree list` says what is \
                 damaged.\n",
                mask_controls(&damaged.join(", "))
            ));
            None
        }
        Err(error) => Some(failed(format!(
            "the worktree that has `main` checked out could not be looked for: {error}; \
             `stemma switch main` there brings it along"
        ))),
    };
    match late {
        // The message of the error names the entry of the merge too, so a failure to write the
        // result is not reported on top of it.
        Some(error) => {
            let _ = out.write_all(text.as_bytes());
            Err(error.into())
        }
        None => write_out(out, &text, Some(merged.merged.number())),
    }
}

fn pr_list(args: PrListArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let repository = open(&args.common.repository)?;
    let register = register_of(&repository)?;
    let pulls = repository
        .pull_requests(register.as_ref())
        .map_err(pull_error)?;
    let mut text = String::new();
    for pull in &pulls {
        let state = match pull.state() {
            PullState::Open => "open",
            PullState::Closed { .. } => "closed",
            PullState::Merged { .. } => "merged",
        };
        let approvals = if pull.is_open() {
            match repository.recount_approvals(pull.number(), register.as_ref()) {
                Err(error) => format!(" approvals unknown ({error})"),
                Ok(recount) => {
                    let void = recount.void().len();
                    let older = recount
                        .void()
                        .iter()
                        .filter(|(_, why)| matches!(why, NotCounted::OtherHead))
                        .count();
                    if void == 0 {
                        format!(" approvals {}", recount.count())
                    } else {
                        format!(
                            " approvals {} ({void} void, {older} for an older head)",
                            recount.count()
                        )
                    }
                }
            }
        } else {
            String::new()
        };
        let hub = if pull.repository().is_some() {
            " hub"
        } else {
            ""
        };
        text.push_str(&format!(
            "{} {state} {} {} by {}{approvals} id {}{hub}\n",
            pull.number(),
            mask_controls(pull.branch().as_str()),
            short_id(*pull.head()),
            mask_controls(&pull.opener().to_string()),
            short_name(pull.id()),
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
    let signer = signer_of(register.as_ref(), &args.signing)?;
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
    let destination = match &args.submitting.remote {
        Some(name) => Some(Destination::named(
            &repository,
            name,
            args.submitting.offline,
        )?),
        None if args.submitting.offline => return Err(offline_alone()),
        None => None,
    };
    let entry = match &destination {
        Some(destination) => repository.record_check_for(
            destination.repository(),
            commit,
            base,
            result,
            &who,
            register.as_ref(),
            as_signer(&signer),
            &AllowAll,
        ),
        None => repository.record_check(
            commit,
            base,
            result,
            &who,
            register.as_ref(),
            as_signer(&signer),
            &AllowAll,
        ),
    }
    .map_err(check_error)?;
    let text = format!(
        "Recorded {} for {} in entry {}.\n",
        result.name(),
        short_id(commit),
        entry.number()
    );
    // A check belongs to no pull request, so a later submission names it by a pull request
    // of its commit: `pr submit` sends the checks of the heads of a pull request.
    let hashes: Vec<_> = entry.statement().into_iter().collect();
    let submitted = destination.map(|destination| {
        destination.submit_after(
            &repository,
            &entry,
            &hashes,
            "PULL",
            args.signing.key.as_deref(),
            &who,
        )
    });
    finish(out, text, &entry, submitted)
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
    probe_key(Some(register), &args.signing, &who)?;
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
