//! The output of `stemma op log`: one entry as text for a person and as one line of JSON for a
//! program (ADR-011).

use std::fmt::{self, Display, Write};

use stemma_core::oplog::{Entry, Moved, Outcome};
use stemma_core::refs::Head;

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
fn utc(seconds: i64) -> String {
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

/// An entry for a person: a line with the number, the time, the actor, and the operation, then
/// one indented line for each pointer that moved, and a last line if the move did not take effect
/// or if that is not known.
pub fn text(entry: &Entry, outcome: Outcome) -> String {
    let change = entry.change();
    let mut out = format!(
        "{}  {}  {}  {}\n",
        entry.number(),
        utc(change.time()),
        change.actor(),
        change.operation()
    );
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
    match outcome {
        Outcome::Took => {}
        Outcome::NotTaken => out.push_str("    ! the move did not take effect\n"),
        Outcome::Unknown => out.push_str("    ? it is not known whether the move took effect\n"),
    }
    out
}

/// A string as a JSON string literal.
fn quote(text: &str) -> String {
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
/// that did not exist or does not exist any more has `null` as its old or new value.
pub fn json(entry: &Entry, outcome: Outcome) -> String {
    let took_effect = match outcome {
        Outcome::Took => "true",
        Outcome::NotTaken => "false",
        Outcome::Unknown => "null",
    };
    let change = entry.change();
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
    format!(
        "{{\"number\":{},\"time\":{},\"actor\":{},\"operation\":{},\"head\":{head},\
         \"working_copy\":{working_copy},\"branches\":[{}],\"took_effect\":{took_effect}}}",
        entry.number(),
        change.time(),
        quote(&change.actor().to_string()),
        quote(change.operation().as_str()),
        branches.join(",")
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
}
