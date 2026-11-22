//! The bounds of the merge gate on a repository (ADR-019, STEMMA-242).
//!
//! [`crate::bounds`] holds the rules as pure functions over the log. This module reads the log, the
//! history of `main`, and the pull requests, applies them for one merge, and names the reason if
//! the merge has to wait ([`Pause`]). Three pauses can hold a merge back, and two of them are
//! bounds with values in the policy:
//!
//! - **A red tip.** While the latest verified tip is red, changes of level 2 and 3 wait; levels 0
//!   and 1 continue.
//! - **The unverified window.** At `max_unverified` commits after the latest verified tip, every
//!   merge waits.
//! - **A reservation.** A change of level 2 or 3 that lost `N` runs in a row to moves of `main`
//!   holds a reservation: every other merge waits until it ends.
//!
//! A *declared repair* is exempt from all three: the pull request names a red commit that lies
//! between the last green and the first red tip, and its change meets the paths of that commit.
//! The checks of the repair follow the rule table like those of any change, and the gate asks for
//! them before it asks for the bounds.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use super::difference;
use super::{now, CheckError, GateError, LevelError, PullError, Repository};
use crate::bounds::{self, Verification};
use crate::check::{self, Check};
use crate::history::{self, Parents};
use crate::merge::merge_commits;
use crate::object::{CommitId, EntryMode, ObjectId, TreeId};
use crate::policy::{Policy, RiskLevel, DEFAULT_MAX_UNVERIFIED};
use crate::pull::{PullRequest, TARGET};
use crate::refs::BranchName;
use crate::register::Register;

/// Why the gate does not merge a change yet. No entry, no merge commit, and no reference change
/// was written (the blobs and trees of the merge may stay in the store, ADR-014); the call can be
/// repeated when the reason has gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Pause {
    /// The latest verified tip of `main` is red and the change has level 2 or 3. Only a repair of
    /// the red commit goes ahead; levels 0 and 1 are not paused.
    RedTip {
        /// The first red tip: the oldest commit after the last green tip that a failing run
        /// covered.
        red: CommitId,
    },
    /// `max_unverified` commits or more lie after the latest verified tip. Every merge waits,
    /// whatever its level, except a repair, until a run covers a newer commit of `main`.
    Unverified {
        /// The commits after the latest verified tip (all of `main` if none is verified).
        count: usize,
        /// The bound of the policy.
        max: u32,
    },
    /// Another change holds the reservation against starving. Every other merge waits, whatever
    /// its level, except a repair.
    Reserved {
        /// The number of the pull request that holds it.
        holder: u64,
        /// When the reservation ends, in seconds since the Unix epoch (UTC).
        until: i64,
    },
}

impl fmt::Display for Pause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RedTip { red } => write!(
                f,
                "the latest verified tip of `{TARGET}` is red since {red}; only a repair of it \
                 lands at level 2 or 3"
            ),
            Self::Unverified { count, max } => write!(
                f,
                "{count} commits of `{TARGET}` lie after the latest verified tip (the bound is \
                 {max}); only a repair lands until a run covers a newer commit"
            ),
            Self::Reserved { holder, until } => write!(
                f,
                "pull request {holder} holds the reservation until {until} (seconds since the \
                 Unix epoch); only a repair of a red tip lands before"
            ),
        }
    }
}

/// What the gate knows of the change that it is about to merge.
pub(super) struct Subject<'a> {
    /// The pull request.
    pub(super) pull: &'a PullRequest,
    /// The tip of `main` that the merge was made on.
    pub(super) tip: CommitId,
    /// The tree of the merge.
    pub(super) tree: TreeId,
    /// The level of the change.
    pub(super) level: RiskLevel,
}

impl Repository {
    /// The reason that the change `subject` has to wait, or `None` (see the module).
    ///
    /// `policy` is the policy of the tip: it gives `N` (`reservation`) and `max_unverified`, and
    /// the levels of the other changes for the reservation. Without one (the caller of
    /// [`merge_gate`](Self::merge_gate) gave the level and no policy is read) the bounds have
    /// their defaults and no reservation is applied, as the levels of the other changes are the
    /// policy's to tell. `register` decides which results count, as in
    /// [`counted_checks`](Self::counted_checks).
    pub(super) fn pause_of(
        &self,
        subject: &Subject<'_>,
        policy: Option<&Policy>,
        register: Option<&Register>,
    ) -> Result<Option<Pause>, GateError> {
        let tip = subject.tip;
        // Only a run of a writer who is independent of the change counts for its bounds: the
        // opener, the authors, and their sponsors could clear a red tip for their own change.
        let proven = self.proven_checks(register)?;
        let checks = self.independent_checks(&proven, subject.pull, &tip, register, true)?;
        let main = self.main_history(&tip)?;
        let verification = bounds::verification(&checks, &main);
        // The exemption of a repair needs more: a red window that the change did not write itself.
        // A failing run of an involved writer still pauses (above), but it cannot open the door.
        let strict = self.independent_checks(&proven, subject.pull, &tip, register, false)?;
        let repair_window = bounds::verification(&strict, &main);

        if let Some(red) = subject.pull.repairs() {
            if self.is_repair(&repair_window, &main, red, subject)? {
                return Ok(None);
            }
        }
        if let (true, Some(red)) = (subject.level >= RiskLevel::Two, verification.first_red) {
            return Ok(Some(Pause::RedTip { red: main[red] }));
        }
        let count = bounds::unverified(&verification, main.len());
        let max = policy.map_or(DEFAULT_MAX_UNVERIFIED, Policy::max_unverified);
        if count >= usize::try_from(max).unwrap_or(usize::MAX) {
            return Ok(Some(Pause::Unverified { count, max }));
        }
        let Some(policy) = policy else {
            return Ok(None);
        };
        let n = policy.reservation_after();
        let mut candidates = Vec::new();
        for other in self.pull_requests()?.iter().filter(|pull| pull.is_open()) {
            let own = other.number() == subject.pull.number();
            let runs = if own { &checks } else { &proven };
            let held = |runs: &[Check]| {
                bounds::reservation(runs, other.head(), &tip, other.opened(), n)
                    .filter(|window| window.holds_at(now()))
            };
            // A cheap look first: without a window on all proven runs there is none on the
            // independent runs either (dropping passing runs only removes counted ones). The
            // time is no part of it: the window of the independent runs starts and ends at
            // other runs, so only they decide whether it holds now.
            if bounds::reservation(runs, other.head(), &tip, other.opened(), n).is_none() {
                continue;
            }
            let window = if own {
                held(&checks)
            } else {
                held(&self.independent_checks(&proven, other, &tip, register, true)?)
            };
            let Some(window) = window else {
                continue;
            };
            let level = if own {
                Some(subject.level)
            } else {
                self.level_of_other(policy, &tip, other)?
            };
            if level.is_some_and(|level| level >= RiskLevel::Two) {
                candidates.push((other.number(), other.opened(), window));
            }
        }
        Ok(bounds::holder(&candidates, now())
            .filter(|holder| holder.number != subject.pull.number())
            .map(|holder| Pause::Reserved {
                holder: holder.number,
                until: holder.window.until,
            }))
    }

    /// The first-parent history of the tip of `main`, the tip first.
    fn main_history(&self, tip: &CommitId) -> Result<Vec<CommitId>, GateError> {
        Ok(
            history::ancestors(self.objects(), *tip, Parents::First, None)?
                .into_iter()
                .map(|(id, _)| id)
                .collect(),
        )
    }

    /// Whether the change is a repair that the bounds do not hold back: the red commit is declared
    /// in the window, and the change meets its paths. A change that only declares a repair is
    /// none.
    fn is_repair(
        &self,
        verification: &Verification,
        main: &[CommitId],
        red: &CommitId,
        subject: &Subject<'_>,
    ) -> Result<bool, GateError> {
        if !bounds::is_declared_repair(verification, main, red) {
            return Ok(false);
        }
        let objects = self.objects();
        let after = self
            .files_with_modes_of(*red)
            .map_err(|error| GateError::Level(LevelError::Diff(error)))?;
        // The commit before the red one on `main`; the root has none and changed all its files.
        let before = match main
            .iter()
            .position(|commit| commit == red)
            .map(|at| at + 1)
        {
            Some(parent) if parent < main.len() => self
                .files_with_modes_of(main[parent])
                .map_err(|error| GateError::Level(LevelError::Diff(error)))?,
            _ => BTreeMap::new(),
        };
        let red_paths = changed_paths(&before, &after);
        let tip_files = self
            .files_with_modes_of(subject.tip)
            .map_err(|error| GateError::Level(LevelError::Diff(error)))?;
        let merged = difference::files_with_modes(objects, subject.tree)
            .map_err(|error| GateError::Level(LevelError::Diff(error)))?;
        Ok(bounds::touches_the_red_paths(
            &red_paths,
            &changed_paths(&tip_files, &merged),
        ))
    }

    /// `checks` without the passing runs of a writer who is not independent of the change `pull`
    /// ([`check::is_independent`]: the opener, the authors of the commits `tip..head`, and their
    /// sponsors). With `keep_failures`, a failing run stays whoever wrote it: it only holds a merge
    /// back, so it must not vanish because its writer is involved in the change. Without it, only
    /// independent writers count, which is what the exemption of a repair needs: nobody opens the
    /// door for their own change by writing the failing run. A pull request that `tip` reaches has no authors of its own to exclude.
    fn independent_checks(
        &self,
        checks: &[Check],
        pull: &PullRequest,
        tip: &CommitId,
        register: Option<&Register>,
        keep_failures: bool,
    ) -> Result<Vec<Check>, GateError> {
        let authors = match self.change_authors(pull.head(), tip) {
            Ok(authors) => authors,
            Err(CheckError::TipReachesHead { .. }) => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        Ok(checks
            .iter()
            .filter(|check| {
                (keep_failures && !check.passed())
                    || check::is_independent(check.writer(), pull.opener(), &authors, register)
            })
            .cloned()
            .collect())
    }

    /// The level of another open pull request merged onto the tip, or `None` if the change cannot
    /// land as it is, so that it holds no reservation: its branch is gone or moved away from the
    /// opening, the merge conflicts, the merge changes nothing, or a path of it has no level.
    ///
    /// # Errors
    ///
    /// A failure to read the refs or the objects is an error: a reservation that
    /// vanishes with a flaky store would let others pass the change that waits.
    fn level_of_other(
        &self,
        policy: &Policy,
        tip: &CommitId,
        other: &PullRequest,
    ) -> Result<Option<RiskLevel>, GateError> {
        match self.branch_head(other.branch()) {
            Ok(head) if head == *other.head() => {}
            Ok(_) | Err(PullError::NoSuchBranch { .. }) => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let sum = merge_commits(self.objects(), *tip, *other.head())?.simplify();
        let [tree] = sum.terms() else {
            return Ok(None);
        };
        match self.level_of_change(policy, tip, *tree) {
            Ok(level) => Ok(Some(level)),
            // The change has a path that no level can be told for, or changes nothing: the gate
            // refuses it at its own merge, so it cannot land and holds no reservation. It must not
            // stop the merge of another change, and its author chooses the paths.
            Err(LevelError::NoChange | LevelError::Path(_)) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// The window between the last green and the first red tip, oldest commit first: the commits
    /// after the last green tip up to and including the first red tip (up to the root if no tip
    /// was ever green). The first red tip is the last of them. It is empty while the latest
    /// verified tip is green or `main` does not exist. To find the commit that broke `main`, run
    /// [`bounds::bisect`] over it with a test that runs the checks on one commit.
    ///
    /// `register` decides which runs count, as in [`counted_checks`](Self::counted_checks).
    ///
    /// # Errors
    ///
    /// [`CheckError::Log`], [`CheckError::Io`], [`CheckError::History`], or [`CheckError::Refs`].
    pub fn red_window(&self, register: Option<&Register>) -> Result<Vec<CommitId>, CheckError> {
        let main = BranchName::try_new(TARGET).expect("`main` is a branch name");
        let Some(tip) = self.refs().branch(&main)? else {
            return Ok(Vec::new());
        };
        let history: Vec<CommitId> = history::ancestors(self.objects(), tip, Parents::First, None)?
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let verification = bounds::verification(&self.proven_checks(register)?, &history);
        let Some(red) = verification.first_red else {
            return Ok(Vec::new());
        };
        let end = verification.last_green.unwrap_or(history.len());
        Ok(history[red..end].iter().rev().copied().collect())
    }
}

/// The paths that differ between two lists of files, by content or by mode.
fn changed_paths(
    before: &BTreeMap<String, (ObjectId, EntryMode)>,
    after: &BTreeMap<String, (ObjectId, EntryMode)>,
) -> BTreeSet<String> {
    let mut changed: BTreeSet<String> = after
        .iter()
        .filter(|(path, entry)| before.get(*path) != Some(entry))
        .map(|(path, _)| path.clone())
        .collect();
    changed.extend(
        before
            .keys()
            .filter(|path| !after.contains_key(*path))
            .cloned(),
    );
    changed
}
