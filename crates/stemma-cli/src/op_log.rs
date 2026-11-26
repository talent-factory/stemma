//! The output of `stemma op log`: one entry as text for a person and as one line of JSON for a
//! program (ADR-011).

use std::fmt::{self, Display, Write};

use stemma_core::oplog::{
    Change, CheckEntry, Entry, LeaseEntry, Moved, Outcome, PullEntry, Purge, Record, CHECK,
};
use stemma_core::refs::Head;
use stemma_core::remote::RemoteRef;

/// The `schema` of every JSON object the program writes (ADR-018). One number covers all of them:
/// removing, renaming, or retyping a field of any object raises it.
pub const JSON_SCHEMA: u32 = 1;

/// The value of a pointer as the log writes it: `-` if it does not exist.
pub(crate) fn value<T: Display>(value: Option<&T>) -> String {
    value.map_or_else(|| "-".to_owned(), ToString::to_string)
}

/// HEAD as the log writes it: `branch:<name>`, `commit:<id>`, or `-`.
pub(crate) struct HeadValue<'a>(pub(crate) &'a Head);

impl Display for HeadValue<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Head::Branch(name) => write!(f, "branch:{name}"),
            Head::Detached(commit) => write!(f, "commit:{commit}"),
        }
    }
}

/// Seconds since the Unix epoch as `YYYY-MM-DDThh:mm:ssZ` (UTC), by the civil-from-days
/// algorithm of Howard Hinnant.
pub(crate) fn utc(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    )
}

/// The ranges of the stubs of a purge as the command line writes them: `4-5, 7`, or `-` for none.
pub(crate) fn stubs_text(stubs: &[(u64, u64)]) -> String {
    if stubs.is_empty() {
        return "-".to_owned();
    }
    stubs
        .iter()
        .map(|(start, end)| {
            if start == end {
                start.to_string()
            } else {
                format!("{start}-{end}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// An entry for a person: a line with the number, the time, the actor, and the operation, then
/// one indented line for each pointer that moved, and a last line if the move did not take effect
/// or if that is not known. A stub has only the first line, with `(purged)` for the operation; a
/// purge has a second line with the count of commits and the stubs (ADR-014). `pending` says that
/// a purge lists the entry and has not replaced it yet, which a last line says.
pub fn text(entry: &Entry, outcome: Outcome, pending: bool) -> String {
    let head = |operation: &dyn Display| {
        format!(
            "{}  {}  {}  {operation}\n",
            entry.number(),
            utc(entry.time()),
            entry.actor()
        )
    };
    let change = match entry.record() {
        Record::Change(change) => change,
        Record::Stub(_) => return head(&"(purged)"),
        Record::Lease(lease) => {
            let mut out = head(&lease.kind().operation());
            let _ = write!(out, "    branch {}", lease.branch());
            if let Some(from) = lease.from() {
                let _ = write!(out, "  from {from}");
            }
            if let Some(expires) = lease.expires() {
                let _ = write!(out, "  expires {}", utc(expires));
            }
            if let Some(reason) = lease.reason() {
                let _ = write!(out, "  reason {}", reason.escape_debug());
            }
            out.push('\n');
            return out;
        }
        Record::Pull(pull) => {
            let mut out = head(&pull.kind().operation());
            if let Some(branch) = pull.branch() {
                let _ = write!(out, "    branch {branch}");
            }
            if let Some(number) = pull.pull() {
                let _ = write!(out, "    pull {number}");
            }
            if let Some(commit) = pull.commit() {
                let _ = write!(out, "  commit {commit}");
            }
            for reviewer in pull.reviewers() {
                let _ = write!(out, "  reviewer {reviewer}");
            }
            if let Some(repairs) = pull.repairs() {
                let _ = write!(out, "  repairs {repairs}");
            }
            out.push('\n');
            return out;
        }
        Record::Check(check) => {
            let mut out = head(&CHECK);
            let _ = write!(out, "    commit {}", check.commit());
            match check.base() {
                Some(base) => {
                    let _ = write!(out, "  base {base}");
                }
                None => out.push_str("  base -"),
            }
            let _ = write!(out, "  result {}", check.result().name());
            out.push('\n');
            return out;
        }
        Record::Purge(purge) => {
            // The tags are named only when the purge moved some, so that an entry from before
            // them reads as before.
            let tags = match purge.tags() {
                0 => String::new(),
                count => format!("  tags {count}"),
            };
            return format!(
                "{}    commits {}{tags}  stubs {}\n",
                head(&"purge"),
                purge.commits(),
                stubs_text(purge.stubs())
            );
        }
    };
    let mut out = head(change.operation());
    // The main worktree is left out, so that a repository without linked worktrees reads as
    // before.
    if let Some(worktree) = change.worktree() {
        let _ = match change.location() {
            Some(location) => writeln!(
                out,
                "    worktree {}  {}",
                worktree.escape_debug(),
                location.escape_debug()
            ),
            None => writeln!(out, "    worktree {}", worktree.escape_debug()),
        };
        if let Some(owner) = change.former_owner() {
            let _ = writeln!(
                out,
                "    former owner  {}",
                owner.to_string().escape_debug()
            );
        }
    }
    let mut line = |name: String, old: String, new: String| {
        // Writing to a string does not fail.
        let _ = writeln!(out, "    {name}  {old} -> {new}");
    };
    if let Some(head) = change.head() {
        line(
            "head".into(),
            value(head.before().map(HeadValue).as_ref()),
            value(head.after().map(HeadValue).as_ref()),
        );
    }
    if let Some(moved) = change.working_copy() {
        line(
            "working-copy".into(),
            value(moved.before()),
            value(moved.after()),
        );
    }
    for (name, moved) in change.branches() {
        line(
            format!("branch {}", name.to_string().escape_debug()),
            value(moved.before()),
            value(moved.after()),
        );
    }
    for (name, moved) in change.tags() {
        line(
            format!("tag {}", name.to_string().escape_debug()),
            value(moved.before()),
            value(moved.after()),
        );
    }
    // A remote reference shows its commit; the record entry is in the entry itself.
    for (name, moved) in change.remotes() {
        line(
            format!("remote {}", name.to_string().escape_debug()),
            value(moved.before().map(|reference| &reference.commit)),
            value(moved.after().map(|reference| &reference.commit)),
        );
    }
    match outcome {
        Outcome::Took => {}
        Outcome::NotTaken => out.push_str("    ! the move did not take effect\n"),
        Outcome::Unknown => out.push_str("    ? it is not known whether the move took effect\n"),
    }
    if pending {
        out.push_str("    ! a purge lists this entry and has not finished\n");
    }
    out
}

/// A string as a JSON string literal.
pub fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A value for JSON: the string, or `null` if the pointer does not exist.
fn json_value<T: Display>(value: Option<&T>) -> String {
    value.map_or_else(|| "null".to_owned(), |v| quote(&v.to_string()))
}

fn moved_json<T: Display>(moved: &Moved<T>) -> String {
    format!(
        "\"old\":{},\"new\":{}",
        json_value(moved.before()),
        json_value(moved.after())
    )
}

/// An entry for a program: one line of JSON. `took_effect` is `true`, `false`, or `null` if it is
/// not known (see [`stemma_core::oplog::outcomes`]). `head` and `working_copy` are `null` if the
/// operation did not change them, and `branches` is a list, empty if no branch changed. A pointer
/// that did not exist or does not exist any more has `null` as its old or new value. `remotes`
/// lists the remote references of a `fetch` and of the restore of one, each with `name`, and
/// `old` and `new` as an object with `commit` and `entry`, or `null`.
///
/// Every line starts with `schema` (ADR-018), `number`, and `kind`: `change`, `stub`, `purge`,
/// `lease`, `pull`, or `check`. A check has `time`, `actor`, `operation` (`check`), `commit`,
/// `base` (`null` for a run on the tip of `main` itself), and `result` (`pass` or `fail`).
/// A pull request entry has `time`, `actor`, `operation` (`pr-open`,
/// `pr-close`, or `pr-merge`), `branch`, `pull`, `commit`, `reviewers`, a list that is empty
/// where the operation has none, and `repairs`; the other fields that the operation has not are
/// `null`. A lease has `time`, `actor`, `operation`
/// (`lease-acquire`, `lease-release`, `lease-takeover`, or `lease-override`), `branch`, `from`, `expires`, and `reason`; the last
/// three are `null` where the operation has none.
/// A stub has only `time` and `actor` besides; a purge has `time`, `actor`, `operation`,
/// `commits`, `tags`, and `stubs`, a list of ranges `[start, end]` (ADR-014). The fields above
/// are those of a change, which has `purge_pending` before `took_effect` as well: `true` if a
/// purge lists it and has not replaced it yet.
pub fn json(entry: &Entry, outcome: Outcome, pending: bool) -> String {
    match entry.record() {
        Record::Change(change) => change_json(entry, change, outcome, pending),
        Record::Stub(_) => format!(
            "{{\"schema\":{JSON_SCHEMA},\"number\":{},\"kind\":\"stub\",\"time\":{},\"actor\":{}}}",
            entry.number(),
            entry.time(),
            quote(&entry.actor().to_string())
        ),
        Record::Purge(purge) => purge_json(entry, purge),
        Record::Lease(lease) => lease_json(entry, lease),
        Record::Pull(pull) => pull_json(entry, pull),
        Record::Check(check) => check_json(entry, check),
    }
}

fn check_json(entry: &Entry, check: &CheckEntry) -> String {
    let base = check
        .base()
        .map_or_else(|| "null".to_owned(), |base| quote(&base.to_string()));
    format!(
        "{{\"schema\":{JSON_SCHEMA},\"number\":{},\"kind\":\"check\",\"time\":{},\"actor\":{},\
         \"operation\":{},\"commit\":{},\"base\":{base},\"result\":{}}}",
        entry.number(),
        entry.time(),
        quote(&entry.actor().to_string()),
        quote(CHECK),
        quote(&check.commit().to_string()),
        quote(check.result().name())
    )
}

fn pull_json(entry: &Entry, pull: &PullEntry) -> String {
    let text = |value: Option<String>| value.map_or_else(|| "null".to_owned(), |v| quote(&v));
    let reviewers: Vec<String> = pull
        .reviewers()
        .iter()
        .map(|reviewer| quote(&reviewer.to_string()))
        .collect();
    format!(
        "{{\"schema\":{JSON_SCHEMA},\"number\":{},\"kind\":\"pull\",\"time\":{},\"actor\":{},\"operation\":{},\
         \"branch\":{},\"pull\":{},\"commit\":{},\"reviewers\":[{}],\"repairs\":{}}}",
        entry.number(),
        entry.time(),
        quote(&entry.actor().to_string()),
        quote(pull.kind().operation()),
        text(pull.branch().map(ToString::to_string)),
        pull.pull()
            .map_or_else(|| "null".to_owned(), |number| number.to_string()),
        text(pull.commit().map(ToString::to_string)),
        reviewers.join(","),
        text(pull.repairs().map(ToString::to_string))
    )
}

fn lease_json(entry: &Entry, lease: &LeaseEntry) -> String {
    let text = |value: Option<String>| value.map_or_else(|| "null".to_owned(), |v| quote(&v));
    format!(
        "{{\"schema\":{JSON_SCHEMA},\"number\":{},\"kind\":\"lease\",\"time\":{},\"actor\":{},\"operation\":{},\
         \"branch\":{},\"from\":{},\"expires\":{},\"reason\":{}}}",
        entry.number(),
        entry.time(),
        quote(&entry.actor().to_string()),
        quote(lease.kind().operation()),
        quote(&lease.branch().to_string()),
        text(lease.from().map(ToString::to_string)),
        lease
            .expires()
            .map_or_else(|| "null".to_owned(), |expires| expires.to_string()),
        text(lease.reason().map(str::to_owned))
    )
}

fn purge_json(entry: &Entry, purge: &Purge) -> String {
    let stubs: Vec<String> = purge
        .stubs()
        .iter()
        .map(|(start, end)| format!("[{start},{end}]"))
        .collect();
    format!(
        "{{\"schema\":{JSON_SCHEMA},\"number\":{},\"kind\":\"purge\",\"time\":{},\"actor\":{},\"operation\":\"purge\",\
         \"commits\":{},\"tags\":{},\"stubs\":[{}]}}",
        entry.number(),
        entry.time(),
        quote(&entry.actor().to_string()),
        purge.commits(),
        purge.tags(),
        stubs.join(",")
    )
}

fn change_json(entry: &Entry, change: &Change, outcome: Outcome, pending: bool) -> String {
    let took_effect = match outcome {
        Outcome::Took => "true",
        Outcome::NotTaken => "false",
        Outcome::Unknown => "null",
    };
    let head = change.head().map_or_else(
        || "null".to_owned(),
        |moved| {
            format!(
                "{{\"old\":{},\"new\":{}}}",
                json_value(moved.before().map(HeadValue).as_ref()),
                json_value(moved.after().map(HeadValue).as_ref())
            )
        },
    );
    let working_copy = change.working_copy().map_or_else(
        || "null".to_owned(),
        |moved| format!("{{{}}}", moved_json(moved)),
    );
    let branches: Vec<String> = change
        .branches()
        .iter()
        .map(|(name, moved)| {
            format!(
                "{{\"name\":{},{}}}",
                quote(&name.to_string()),
                moved_json(moved)
            )
        })
        .collect();
    let tags: Vec<String> = change
        .tags()
        .iter()
        .map(|(name, moved)| {
            format!(
                "{{\"name\":{},{}}}",
                quote(&name.to_string()),
                moved_json(moved)
            )
        })
        .collect();
    // A remote reference has the commit and the record entry as its value (ADR-008).
    let remote = |value: Option<&RemoteRef>| {
        value.map_or_else(
            || "null".to_owned(),
            |reference| {
                format!(
                    "{{\"commit\":{},\"entry\":{}}}",
                    quote(&reference.commit.to_string()),
                    quote(&reference.entry.to_string())
                )
            },
        )
    };
    let remotes: Vec<String> = change
        .remotes()
        .iter()
        .map(|(name, moved)| {
            format!(
                "{{\"name\":{},\"old\":{},\"new\":{}}}",
                quote(&name.to_string()),
                remote(moved.before()),
                remote(moved.after())
            )
        })
        .collect();
    format!(
        "{{\"schema\":{JSON_SCHEMA},\"number\":{},\"kind\":\"change\",\"time\":{},\"actor\":{},\"operation\":{},\
         \"worktree\":{},\"location\":{},\"former_owner\":{},\
         \"head\":{head},\
         \"working_copy\":{working_copy},\"branches\":[{}],\"tags\":[{}],\"remotes\":[{}],\
         \"purge_pending\":{pending},\
         \"took_effect\":{took_effect}}}",
        entry.number(),
        change.time(),
        quote(&change.actor().to_string()),
        quote(change.operation().as_str()),
        quote(change.worktree_name()),
        change.location().map_or_else(|| "null".to_owned(), quote),
        change
            .former_owner()
            .map_or_else(|| "null".to_owned(), |owner| quote(&owner.to_string())),
        branches.join(","),
        tags.join(","),
        remotes.join(",")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_time_is_utc_and_handles_the_epoch_and_leap_days() {
        assert_eq!(utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(utc(-1), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn a_string_is_quoted_for_json() {
        assert_eq!(quote("a\"b\\c\nd"), "\"a\\\"b\\\\c\\u000ad\"");
        assert_eq!(quote("ä"), "\"ä\"");
    }

    fn lease_entry() -> Entry {
        let text = "stemma-oplog 3\nnumber 5\ntime 1700000000\nactor human mia\nprevious -\n\
                    operation lease-takeover\nbranch feature/x\nfrom human daniel\n\
                    expires 1700003600\nreason expired\n";
        Entry::parse(text.as_bytes()).unwrap()
    }

    #[test]
    fn a_lease_entry_shows_its_branch_actors_and_reason() {
        assert_eq!(
            text(&lease_entry(), Outcome::Unknown, false),
            "5  2023-11-14T22:13:20Z  human mia  lease-takeover\n    branch feature/x  \
             from human daniel  expires 2023-11-14T23:13:20Z  reason expired\n"
        );
    }

    #[test]
    fn a_lease_entry_has_its_own_kind_in_json() {
        assert_eq!(
            json(&lease_entry(), Outcome::Unknown, false),
            "{\"schema\":1,\"number\":5,\"kind\":\"lease\",\"time\":1700000000,\
             \"actor\":\"human mia\",\"operation\":\"lease-takeover\",\"branch\":\"feature/x\",\
             \"from\":\"human daniel\",\"expires\":1700003600,\"reason\":\"expired\"}"
        );
        let text = "stemma-oplog 3\nnumber 6\ntime 1700000000\nactor human mia\nprevious -\n\
                    operation lease-release\nbranch main\n";
        let release = Entry::parse(text.as_bytes()).unwrap();
        let line = json(&release, Outcome::Unknown, false);
        assert!(
            line.ends_with("\"branch\":\"main\",\"from\":null,\"expires\":null,\"reason\":null}"),
            "{line}"
        );
    }

    #[test]
    fn a_pull_entry_shows_its_branch_commit_and_reviewers() {
        let commit = "b3:1d719c9e1d926eb0f33467406bf50911eca1d59f976105abd3eb5ccfaabdeed9";
        let text = format!(
            "stemma-oplog 3\nnumber 7\ntime 1700000000\nactor human mia\nprevious -\n\
             operation pr-open\nbranch feature/x\ncommit {commit}\nreviewer human daniel\n"
        );
        let open = Entry::parse(text.as_bytes()).unwrap();
        assert_eq!(
            super::text(&open, Outcome::Unknown, false),
            format!(
                "7  2023-11-14T22:13:20Z  human mia  pr-open\n    branch feature/x  commit \
                 {commit}  reviewer human daniel\n"
            )
        );
        assert_eq!(
            json(&open, Outcome::Unknown, false),
            format!(
                "{{\"schema\":1,\"number\":7,\"kind\":\"pull\",\"time\":1700000000,\
                 \"actor\":\"human mia\",\"operation\":\"pr-open\",\"branch\":\"feature/x\",\
                 \"pull\":null,\"commit\":\"{commit}\",\"reviewers\":[\"human daniel\"],\
                 \"repairs\":null}}"
            )
        );
        let text = "stemma-oplog 3\nnumber 8\ntime 1700000000\nactor human mia\nprevious -\n\
                    operation pr-close\npull 7\n";
        let close = Entry::parse(text.as_bytes()).unwrap();
        assert_eq!(
            super::text(&close, Outcome::Unknown, false),
            "8  2023-11-14T22:13:20Z  human mia  pr-close\n    pull 7\n"
        );
        let line = json(&close, Outcome::Unknown, false);
        assert!(
            line.ends_with(
                "\"branch\":null,\"pull\":7,\"commit\":null,\"reviewers\":[],\"repairs\":null}"
            ),
            "{line}"
        );
    }

    #[test]
    fn a_check_entry_shows_its_commit_base_and_result() {
        let commit = "b3:1d719c9e1d926eb0f33467406bf50911eca1d59f976105abd3eb5ccfaabdeed9";
        let base = "b3:4feb2d73a2c706e33c2a71f6f03cbfb48f0a20b48567936fca293159e7e3b32c";
        let text = format!(
            "stemma-oplog 3\nnumber 9\ntime 1700000000\nactor human mia\nprevious -\n\
             operation check\nversion 1\ncommit {commit}\nbase {base}\nresult pass\n"
        );
        let run = Entry::parse(text.as_bytes()).unwrap();
        assert_eq!(
            super::text(&run, Outcome::Unknown, false),
            format!(
                "9  2023-11-14T22:13:20Z  human mia  check\n    commit {commit}  base {base}  \
                 result pass\n"
            )
        );
        assert_eq!(
            json(&run, Outcome::Unknown, false),
            format!(
                "{{\"schema\":1,\"number\":9,\"kind\":\"check\",\"time\":1700000000,\
                 \"actor\":\"human mia\",\"operation\":\"check\",\"commit\":\"{commit}\",\
                 \"base\":\"{base}\",\"result\":\"pass\"}}"
            )
        );
        let tip = text
            .replace(&format!("base {base}"), "base -")
            .replace("pass", "fail");
        let line = json(
            &Entry::parse(tip.as_bytes()).unwrap(),
            Outcome::Unknown,
            false,
        );
        assert!(
            line.ends_with("\"base\":null,\"result\":\"fail\"}"),
            "{line}"
        );
    }
}
