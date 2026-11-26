//! The status of an HTTP answer as the coarse class of ADR-023 ("Mapping to HTTP").

use std::fmt;
use std::time::Duration;

/// The status of the last answer, as the coarse class that ADR-023 gives it. The typed error
/// stands in the body ([`ErrorMessage`](stemma_core::protocol::ErrorMessage)); the class is what
/// a command needs beyond it, such as the exit code of a busy hub or that a sign-in is needed.
///
/// [`HttpTransport::last_status`](crate::HttpTransport::last_status) returns it for every answer
/// with an HTTP status: a message, an error message, or no answer of the protocol alike.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Status {
    /// 200 or 204. Carries the code.
    Success(u16),
    /// 400: a message that cannot be read, or an unknown protocol version.
    Malformed,
    /// 401 with `WWW-Authenticate: Bearer`: a token that is missing, expired, or invalid where
    /// one is needed. A 401 without that header is [`Other`](Self::Other).
    SignInNeeded,
    /// 403: an action that is refused, or a count of a namespace exceeded.
    Forbidden,
    /// 404: a repository that is missing or that the sender may not read.
    NotFound,
    /// 409: a ref update that is stale.
    Stale,
    /// 413: an object, a request, or a quota exceeded.
    TooLarge,
    /// 415: an unknown content encoding.
    UnsupportedEncoding,
    /// 422: a ref update whose objects are missing or not connected.
    Incomplete,
    /// 429: the limit on requests without a valid token. Carries the wait of `Retry-After` in
    /// seconds, or `None` if the header is missing or is no number of seconds.
    RateLimited(Option<Duration>),
    /// 503: the hub is busy. Carries the wait of `Retry-After` as for
    /// [`RateLimited`](Self::RateLimited).
    Busy(Option<Duration>),
    /// 3xx: a redirect, which the client does not follow. Carries the code and the new address
    /// of `Location` if the header is there, with a sequence that is no UTF-8 replaced.
    Redirect(u16, Option<String>),
    /// Any other code, which has no meaning in the protocol.
    Other(u16),
}

impl Status {
    /// The class of a code. `bearer` says whether the answer carries
    /// `WWW-Authenticate: Bearer`, `retry_after` and `location` are the values of those headers.
    pub(crate) fn classify(
        code: u16,
        bearer: bool,
        retry_after: Option<&str>,
        location: Option<&str>,
    ) -> Self {
        let wait = || retry_after.and_then(seconds);
        match code {
            200 | 204 => Self::Success(code),
            300..=399 => Self::Redirect(code, location.map(str::to_owned)),
            400 => Self::Malformed,
            401 if bearer => Self::SignInNeeded,
            403 => Self::Forbidden,
            404 => Self::NotFound,
            409 => Self::Stale,
            413 => Self::TooLarge,
            415 => Self::UnsupportedEncoding,
            422 => Self::Incomplete,
            429 => Self::RateLimited(wait()),
            503 => Self::Busy(wait()),
            _ => Self::Other(code),
        }
    }

    /// The code of the status.
    pub fn code(&self) -> u16 {
        match self {
            Self::Success(code) | Self::Redirect(code, _) | Self::Other(code) => *code,
            Self::Malformed => 400,
            Self::SignInNeeded => 401,
            Self::Forbidden => 403,
            Self::NotFound => 404,
            Self::Stale => 409,
            Self::TooLarge => 413,
            Self::UnsupportedEncoding => 415,
            Self::Incomplete => 422,
            Self::RateLimited(_) => 429,
            Self::Busy(_) => 503,
        }
    }

    /// Whether the code is one of an error, 4xx or 5xx, whose body may be an error message.
    pub(crate) fn is_error(&self) -> bool {
        (400..=599).contains(&self.code())
    }
}

/// Whether the values of `WWW-Authenticate` name the scheme `Bearer` in one of their challenges.
/// A value may hold several challenges, each a scheme and its parameters, separated by commas;
/// an element that begins with a word without `=` begins a challenge, and that word is its scheme.
/// A comma inside a quoted string separates nothing.
pub(crate) fn names_bearer<'a>(values: impl IntoIterator<Item = &'a str>) -> bool {
    values.into_iter().any(|value| {
        elements(value).into_iter().any(|element| {
            let word = element.trim_start().split([' ', '\t']).next().unwrap_or("");
            !word.contains('=') && word.eq_ignore_ascii_case("bearer")
        })
    })
}

/// The elements of a list of a header, split at the commas outside a quoted string. A backslash
/// in a quoted string escapes the next character.
fn elements(value: &str) -> Vec<&str> {
    let mut elements = Vec::new();
    let (mut start, mut quoted, mut escaped) = (0, false, false);
    for (index, c) in value.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ',' if !quoted => {
                elements.push(&value[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    elements.push(&value[start..]);
    elements
}

/// A `Retry-After` of a number of seconds: decimal digits alone. The form of a date is not read.
fn seconds(value: &str) -> Option<Duration> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok().map(Duration::from_secs)
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let code = self.code();
        let wait = |f: &mut fmt::Formatter<'_>, wait: &Option<Duration>| match wait {
            Some(wait) => write!(f, ", retry after {} s", wait.as_secs()),
            None => Ok(()),
        };
        match self {
            Self::Success(_) => write!(f, "status {code} (success)"),
            Self::Malformed => write!(f, "status {code} (a message that cannot be read)"),
            Self::SignInNeeded => write!(f, "status {code} (a sign-in is needed)"),
            Self::Forbidden => write!(f, "status {code} (refused)"),
            Self::NotFound => write!(f, "status {code} (no such repository)"),
            Self::Stale => write!(f, "status {code} (stale)"),
            Self::TooLarge => write!(f, "status {code} (too large)"),
            Self::UnsupportedEncoding => write!(f, "status {code} (unknown content encoding)"),
            Self::Incomplete => write!(f, "status {code} (objects missing)"),
            Self::RateLimited(after) => {
                write!(f, "status {code} (too many requests")?;
                wait(f, after)?;
                f.write_str(")")
            }
            Self::Busy(after) => {
                write!(f, "status {code} (the hub is busy")?;
                wait(f, after)?;
                f.write_str(")")
            }
            Self::Redirect(_, Some(location)) => write!(
                f,
                "status {code} (a redirect to `{}`, which is not followed)",
                location.escape_debug()
            ),
            Self::Redirect(_, None) => {
                write!(f, "status {code} (a redirect, which is not followed)")
            }
            Self::Other(_) => write!(f, "status {code}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_of_the_table_has_its_class() {
        let table = [
            (200, Status::Success(200)),
            (204, Status::Success(204)),
            (400, Status::Malformed),
            (401, Status::SignInNeeded),
            (403, Status::Forbidden),
            (404, Status::NotFound),
            (409, Status::Stale),
            (413, Status::TooLarge),
            (415, Status::UnsupportedEncoding),
            (422, Status::Incomplete),
            (429, Status::RateLimited(None)),
            (503, Status::Busy(None)),
        ];
        for (code, class) in table {
            let found = Status::classify(code, true, None, None);
            assert_eq!(found, class, "{code}");
            assert_eq!(found.code(), code);
        }
    }

    #[test]
    fn bearer_is_found_in_every_challenge_of_every_value() {
        for values in [
            vec!["Bearer"],
            vec!["bearer"],
            vec![r#"Bearer realm="hub", error="invalid_token""#],
            vec![r#"Basic realm="x", Bearer realm="y""#],
            vec!["Basic realm=x", "Bearer"],
        ] {
            assert!(names_bearer(values.iter().copied()), "{values:?}");
        }
        for values in [
            vec![],
            vec!["Basic"],
            vec!["Bearerish"],
            vec![r#"Basic realm="Bearer""#],
            vec![r#"Basic realm=x, error=Bearer"#],
            vec![r#"Basic realm="a, Bearer b""#],
            vec![r#"Basic realm="a \", Bearer b""#],
        ] {
            assert!(!names_bearer(values.iter().copied()), "{values:?}");
        }
    }

    #[test]
    fn a_401_without_bearer_is_no_sign_in() {
        assert_eq!(Status::classify(401, false, None, None), Status::Other(401));
    }

    #[test]
    fn retry_after_is_read_as_seconds_alone() {
        assert_eq!(
            Status::classify(503, false, Some("30"), None),
            Status::Busy(Some(Duration::from_secs(30)))
        );
        assert_eq!(
            Status::classify(429, false, Some("7"), None),
            Status::RateLimited(Some(Duration::from_secs(7)))
        );
        for value in [
            "",
            "-1",
            "+1",
            "1.5",
            "Wed, 21 Oct 2026 07:28:00 GMT",
            "99999999999999999999",
        ] {
            assert_eq!(
                Status::classify(503, false, Some(value), None),
                Status::Busy(None),
                "{value}"
            );
        }
    }

    #[test]
    fn a_redirect_names_its_address() {
        let status = Status::classify(301, false, None, Some("https://elsewhere.example/x"));
        assert_eq!(
            status,
            Status::Redirect(301, Some("https://elsewhere.example/x".to_owned()))
        );
        assert!(status.to_string().contains("https://elsewhere.example/x"));
        assert!(!status.is_error());
    }

    #[test]
    fn other_codes_keep_their_number() {
        for code in [100, 201, 202, 402, 418, 500, 502, 504] {
            assert_eq!(
                Status::classify(code, false, None, None),
                Status::Other(code)
            );
        }
        assert!(Status::Other(502).is_error());
        assert!(!Status::Other(201).is_error());
    }
}
