//! The address of a remote as the client uses it: the rule for `http://` (ADR-028) and the
//! endpoints below the address of the repository (ADR-023).

use stemma_core::remote::RemoteAddress;

/// The scheme of an address with TLS.
const HTTPS: &str = "https://";

/// The scheme of an address without TLS, which is accepted towards a loopback address alone.
const HTTP: &str = "http://";

/// The hosts that an address with `http://` may name (ADR-028). An IPv6 address stands in
/// brackets in an address, as `[::1]`.
const LOOPBACK: [&str; 3] = ["127.0.0.1", "[::1]", "localhost"];

/// The requests of this version, by the endpoint below the address of the repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Endpoint {
    /// `GET <repo>/refs`, and `POST <repo>/refs` for the ref update.
    Refs,
    /// `POST <repo>/fetch`.
    Fetch,
    /// `POST <repo>/challenge`.
    Challenge,
    /// `POST <repo>/token`.
    Token,
}

impl Endpoint {
    fn name(self) -> &'static str {
        match self {
            Self::Refs => "refs",
            Self::Fetch => "fetch",
            Self::Challenge => "challenge",
            Self::Token => "token",
        }
    }
}

/// How the client reaches the repository: with TLS, or without it towards a loopback address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scheme {
    /// `https://`.
    Tls,
    /// `http://` towards `127.0.0.1`, `::1`, or `localhost`.
    PlainLoopback,
}

/// The address of a repository that the client accepts.
#[derive(Debug, Clone)]
pub(crate) struct Base {
    address: String,
    scheme: Scheme,
}

impl Base {
    /// Checks the address against the rule of the client: `https://` towards every host, and
    /// `http://` only towards a loopback address. Returns `None` for an address with `http://`
    /// towards any other host.
    pub(crate) fn new(address: &RemoteAddress) -> Option<Self> {
        let text = address.as_str();
        let scheme = if text.starts_with(HTTPS) {
            Scheme::Tls
        } else if text
            .strip_prefix(HTTP)
            .is_some_and(|rest| is_loopback(host(rest)))
        {
            Scheme::PlainLoopback
        } else {
            return None;
        };
        Some(Self {
            address: text.to_owned(),
            scheme,
        })
    }

    /// How the client reaches the repository.
    pub(crate) fn scheme(&self) -> Scheme {
        self.scheme
    }

    /// The URL of an endpoint: the address of the repository, `/`, and the name of the endpoint.
    /// A [`RemoteAddress`] has no `/` at its end, no query, and no fragment, so the join is plain.
    pub(crate) fn url(&self, endpoint: Endpoint) -> String {
        format!("{}/{}", self.address, endpoint.name())
    }
}

/// The host of the part of an address after the scheme, with its brackets for IPv6 and without
/// its port. A [`RemoteAddress`] has no `@`, so there is no user part to skip. A port that is not
/// decimal digits, or anything else after the host, leaves the whole authority, which is no
/// loopback host.
fn host(rest: &str) -> &str {
    let authority = rest.split('/').next().unwrap_or(rest);
    let split = if authority.starts_with('[') {
        // `[::1]:8080`: the host ends with the bracket, and a port may follow.
        authority.find(']').map(|end| authority.split_at(end + 1))
    } else {
        authority.find(':').map(|colon| authority.split_at(colon))
    };
    match split {
        None => authority,
        Some((host, "")) => host,
        Some((host, port)) => match port.strip_prefix(':') {
            Some(digits) if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) => {
                host
            }
            _ => authority,
        },
    }
}

/// Whether the host is one of the loopback hosts of ADR-028. `localhost` is compared without
/// regard to the case of its letters, as a host name is.
fn is_loopback(host: &str) -> bool {
    LOOPBACK
        .iter()
        .any(|loopback| host.eq_ignore_ascii_case(loopback))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(address: &str) -> Option<Scheme> {
        Base::new(&RemoteAddress::try_new(address).unwrap()).map(|base| base.scheme())
    }

    #[test]
    fn https_is_accepted_towards_every_host() {
        for address in [
            "https://hub.example/ns/repo",
            "https://127.0.0.1/ns/repo",
            "https://[2001:db8::1]:8443/ns/repo",
        ] {
            assert_eq!(base(address), Some(Scheme::Tls), "{address}");
        }
    }

    #[test]
    fn http_is_accepted_towards_the_loopback_hosts_alone() {
        for address in [
            "http://127.0.0.1/ns/repo",
            "http://127.0.0.1:8080/ns/repo",
            "http://[::1]/ns/repo",
            "http://[::1]:8080/ns/repo",
            "http://localhost/ns/repo",
            "http://LocalHost:8080/ns/repo",
        ] {
            assert_eq!(base(address), Some(Scheme::PlainLoopback), "{address}");
        }
        for address in [
            "http://hub.example/ns/repo",
            "http://127.0.0.2/ns/repo",
            "http://127.0.0.1.hub.example/ns/repo",
            "http://localhost.hub.example/ns/repo",
            "http://[::2]/ns/repo",
            "http://[::1].hub.example/ns/repo",
            "http://0.0.0.0/ns/repo",
            "http://10.0.0.1:8080/ns/repo",
            "http://localhost:evil.example/ns/repo",
            "http://127.0.0.1:/ns/repo",
            "http://[::1]:x/ns/repo",
        ] {
            assert_eq!(base(address), None, "{address}");
        }
    }

    #[test]
    fn an_endpoint_lies_below_the_address_of_the_repository() {
        let base =
            Base::new(&RemoteAddress::try_new("https://hub.example/ns/repo").unwrap()).unwrap();
        assert_eq!(base.url(Endpoint::Refs), "https://hub.example/ns/repo/refs");
        assert_eq!(
            base.url(Endpoint::Fetch),
            "https://hub.example/ns/repo/fetch"
        );
        assert_eq!(
            base.url(Endpoint::Challenge),
            "https://hub.example/ns/repo/challenge"
        );
        assert_eq!(
            base.url(Endpoint::Token),
            "https://hub.example/ns/repo/token"
        );
    }
}
