//! The HTTP client: [`HttpTransport`], its [`Timeouts`], and the errors of its construction.

use std::error::Error;
use std::fmt;
use std::io::{self, BufReader, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use stemma_core::protocol::sign_in::{Token, TokenAnswer};
use stemma_core::remote::RemoteConfig;
use stemma_core::transport::{
    Answer, Body, SignedIn, Transport, TransportError, MAX_SIGN_IN_MESSAGE,
};
use ureq::config::AutoHeaderValue;
use ureq::http::{header, HeaderMap, Response, Version};
use ureq::{Agent, Proxy, ProxyProtocol, SendBody};

use crate::address::{Base, Endpoint, Scheme};
use crate::encoding::{self, ContentEncoding, ZSTD};
use crate::status::{names_bearer, Status};
use crate::tls;

/// The media type of a message of the protocol, other than an error, in both directions: the
/// type of the body of a request, and the type that an answer with the status 200 must carry to
/// be a message. An answer with the status 204 is a message without a body and needs no type.
pub const MESSAGE_MEDIA_TYPE: &str = "application/vnd.stemma.message";

/// The media type of an error message of the protocol ([`ErrorMessage`]), which a 4xx or 5xx
/// answer must carry to be handed over as [`Answer::Error`].
///
/// [`ErrorMessage`]: stemma_core::protocol::ErrorMessage
pub const ERROR_MEDIA_TYPE: &str = "application/vnd.stemma.error";

/// The value of `User-Agent`: the name and the version of the program.
const USER_AGENT: &str = concat!("stemma/", env!("CARGO_PKG_VERSION"));

/// The timeouts of a transport (ADR-028). There is none for a whole request, so a long transfer
/// runs as long as bytes flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Timeouts {
    /// The longest time to resolve the name of the host, and again the longest time to establish
    /// the connection, including the handshake of TLS. Through a proxy, the name of the host is
    /// not resolved here; the name of the proxy and the `CONNECT` exchange count to the
    /// connection.
    pub connect: Duration,
    /// The longest time that one read or one write on the connection may wait for the other
    /// side: for the answer to begin, and for every further part of a body in either direction.
    /// It begins again with every read and every write. It covers the reads and writes of the
    /// handshake of TLS and of a `CONNECT` as well, so a connection is set up within the shorter
    /// of the two times as long as the other side stays silent.
    pub idle: Duration,
}

impl Timeouts {
    /// Thirty seconds to resolve the name, thirty seconds to connect, and sixty seconds of idle
    /// time.
    pub const DEFAULT: Self = Self {
        connect: Duration::from_secs(30),
        idle: Duration::from_secs(60),
    };

    /// These timeouts.
    pub fn new(connect: Duration, idle: Duration) -> Self {
        Self { connect, idle }
    }
}

impl Default for Timeouts {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Why a transport cannot be made for a remote.
#[derive(Debug)]
#[non_exhaustive]
pub enum ClientError {
    /// The address uses `http://` towards a host that is not a loopback address (ADR-028).
    /// Carries the address.
    PlainHttp(String),
    /// The CA file of the remote cannot be read.
    CaFileRead {
        /// The path of the file.
        path: PathBuf,
        /// The error of the file system.
        error: io::Error,
    },
    /// The CA file of the remote holds no certificate in PEM, a PEM section that cannot be read,
    /// or a certificate that rustls does not take as a root.
    CaFileInvalid {
        /// The path of the file.
        path: PathBuf,
        /// What is wrong with the file.
        detail: String,
    },
}

impl ClientError {
    /// The error of the file system, if that is what failed.
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::CaFileRead { error, .. } => Some(error),
            Self::PlainHttp(_) | Self::CaFileInvalid { .. } => None,
        }
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PlainHttp(address) => write!(
                f,
                "the address `{}` uses `http://` towards a host that is not a loopback address; \
                 use `https://`, or `http://` towards 127.0.0.1, ::1, or localhost",
                address.escape_debug()
            ),
            Self::CaFileRead { path, error } => {
                write!(f, "the CA file {} cannot be read: {error}", path.display())
            }
            Self::CaFileInvalid { path, detail } => {
                write!(f, "the CA file {} cannot be used: {detail}", path.display())
            }
        }
    }
}

impl Error for ClientError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            // The message includes the message of the cause, so the source is the cause of the
            // cause (see the crate documentation).
            Self::CaFileRead { error, .. } => error.source(),
            Self::PlainHttp(_) | Self::CaFileInvalid { .. } => None,
        }
    }
}

/// The [`Transport`] over HTTP/1.1 to one repository on a hub (ADR-022, ADR-023, ADR-028).
///
/// It is made from the configuration of a remote: its address and its CA file. Making it opens
/// no connection; every request opens one or reuses one that an earlier request left open after
/// its answer was read to the end. A request returns when the head of the answer has arrived,
/// and the body is read by the caller; a body that ends before its length, or before the last
/// chunk, gives an error of its reader. An answer whose body has neither a length nor chunks is no
/// answer, because its end could not be told from a cut.
///
/// A request body is sent to its end before the answer is read. A hub that answers early, such
/// as with 413 for an upload that is too large, and closes the connection, leaves a failure of
/// the carrier: its error message is lost.
///
/// The token of a sign-in is kept in memory and sent with every request that follows, except
/// the two requests of a sign-in. The `Debug` form shows the address and whether a token is held,
/// not the token.
pub struct HttpTransport {
    agent: Agent,
    base: Base,
    token: Option<Token>,
    last_status: Option<Status>,
    /// Whether the answer to the last request was a message or an error message of the protocol
    /// and named zstd in `Accept-Encoding`, so that the next upload is compressed (ADR-023).
    /// Every request sets it back first; another answer, such as the page of a proxy, or a
    /// request without an answer leaves it unset.
    hub_accepts_zstd: bool,
}

impl fmt::Debug for HttpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpTransport")
            .field("base", &self.base)
            .field("holds_token", &self.token.is_some())
            .field("last_status", &self.last_status)
            .field("hub_accepts_zstd", &self.hub_accepts_zstd)
            .finish_non_exhaustive()
    }
}

impl HttpTransport {
    /// A transport to the repository of `remote`, with these timeouts, not signed in.
    ///
    /// The address must use `https://`, or `http://` towards `127.0.0.1`, `::1`, or `localhost`.
    /// Without a CA file the roots of trust are the Mozilla roots that are compiled in; with one
    /// they are the certificates of the file alone. A connection with `https://` uses an HTTP or
    /// HTTPS proxy that the environment names, through `CONNECT`, so the proxy sees no token: the
    /// first of `ALL_PROXY`, `HTTPS_PROXY`, and `HTTP_PROXY` (each also in lower case) that holds
    /// a valid proxy address, unless `NO_PROXY` names the host; this holds for an `https://`
    /// address towards a loopback host as well. The variables are read here, once. If that first
    /// variable names a SOCKS proxy, no proxy is used, and the later variables are not tried. An
    /// HTTPS proxy is checked against the same roots as the hub, so against the CA file if the
    /// remote names one. A connection with `http://` uses no proxy.
    ///
    /// # Errors
    ///
    /// [`ClientError::PlainHttp`] for an address with `http://` towards another host, and
    /// [`ClientError::CaFileRead`] or [`ClientError::CaFileInvalid`] for a CA file that cannot be
    /// read, holds no certificate, holds a PEM section that cannot be read, or holds a
    /// certificate that rustls does not take as a root. The file is read here, once.
    pub fn new(remote: &RemoteConfig, timeouts: Timeouts) -> Result<Self, ClientError> {
        let base = Base::new(&remote.address)
            .ok_or_else(|| ClientError::PlainHttp(remote.address.as_str().to_owned()))?;
        let tls = tls::config(remote.ca_file.as_ref().map(|file| file.as_path()))?;
        let builder = Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .https_only(base.scheme() == Scheme::Tls)
            .tls_config(tls)
            .user_agent(USER_AGENT)
            .accept(format!("{MESSAGE_MEDIA_TYPE}, {ERROR_MEDIA_TYPE}"))
            // The decoder of zstd is the client's own (ADR-029); `ureq` passes the body through.
            .accept_encoding(AutoHeaderValue::Provided(Arc::new(ZSTD.to_owned())))
            .timeout_resolve(Some(timeouts.connect))
            .timeout_connect(Some(timeouts.connect))
            .timeout_per_read(Some(timeouts.idle))
            .timeout_per_write(Some(timeouts.idle));
        let proxy = match base.scheme() {
            // `ureq` would take a SOCKS proxy and, without its feature `socks-proxy`, connect
            // past it or fail.
            Scheme::Tls => Proxy::try_from_env().filter(|proxy| {
                matches!(proxy.protocol(), ProxyProtocol::Http | ProxyProtocol::Https)
            }),
            Scheme::PlainLoopback => None,
        };
        let builder = builder.proxy(proxy);
        Ok(Self {
            agent: builder.build().new_agent(),
            base,
            token: None,
            last_status: None,
            hub_accepts_zstd: false,
        })
    }

    /// Whether the transport holds a token, because a sign-in was answered with one. It says
    /// nothing about the lifetime: a later sign-in that does not succeed (refused, without an
    /// answer, or with a failure of the carrier) leaves the token of the earlier one where it is.
    pub fn holds_token(&self) -> bool {
        self.token.is_some()
    }

    /// The status of the answer to the last request, if that answer had an HTTP status: for an
    /// answer that is a message, an error message, or no answer of the protocol alike. It is
    /// `None` if no request was sent yet, if the carrier failed, if the body of the request could
    /// not be read, and if what came back was no HTTP answer with a status (another service
    /// behind the port, a head that is too large, a proxy that refused the tunnel).
    pub fn last_status(&self) -> Option<&Status> {
        self.last_status.as_ref()
    }

    /// Sends one request by `method`: a GET without a body, a POST with an empty body, or a POST
    /// with the body of a reader. The token goes with it unless `endpoint` is one of the two of
    /// the sign-in, or no token is held.
    fn send(
        &mut self,
        endpoint: Endpoint,
        method: Method<'_>,
    ) -> Result<Response<ureq::Body>, TransportError> {
        self.last_status = None;
        // Only this request reads the flag; an answer of the protocol to it sets it again.
        let compress = std::mem::take(&mut self.hub_accepts_zstd);
        let url = self.base.url(endpoint);
        let bearer = match (&self.token, endpoint) {
            (_, Endpoint::Challenge | Endpoint::Token) | (None, _) => None,
            (Some(token), _) => Some(format!("Bearer {}", token.expose())),
        };
        let result = match method {
            Method::Get => {
                let request = self.agent.get(&url);
                match &bearer {
                    Some(value) => request.header(header::AUTHORIZATION, value).call(),
                    None => request.call(),
                }
            }
            Method::PostEmpty => {
                let request = self.agent.post(&url);
                match &bearer {
                    Some(value) => request.header(header::AUTHORIZATION, value).send_empty(),
                    None => request.send_empty(),
                }
            }
            Method::Post(body) => {
                // The body is compressed only after the hub named zstd in `Accept-Encoding`; the
                // encoder reads from the body, so an error of the body still counts for this side.
                // A failure of the encoder, which is on this side as well, counts the same way.
                let mut encoded;
                let (body, request) = if compress {
                    encoded = encoding::encoder(body).map_err(TransportError::Request)?;
                    let request = self.agent.post(&url).header(header::CONTENT_ENCODING, ZSTD);
                    (&mut *encoded, request)
                } else {
                    (body, self.agent.post(&url))
                };
                let mut tracked = Tracked {
                    inner: body,
                    error: None,
                };
                let request = request.header(header::CONTENT_TYPE, MESSAGE_MEDIA_TYPE);
                let request = match &bearer {
                    Some(value) => request.header(header::AUTHORIZATION, value),
                    None => request,
                };
                let result = request.send(SendBody::from_reader(&mut tracked));
                if let Some(error) = tracked.error {
                    return Err(TransportError::Request(error));
                }
                result
            }
        };
        result.map_err(failure)
    }

    /// Sorts an answer into a message, an error message, or no answer of the protocol, and keeps
    /// its status.
    fn classify(
        &mut self,
        response: Response<ureq::Body>,
    ) -> Result<Answer<'static>, TransportError> {
        let headers = response.headers();
        let location = headers.get(header::LOCATION).map(lossy);
        let status = Status::classify(
            response.status().as_u16(),
            names_bearer(
                headers
                    .get_all(header::WWW_AUTHENTICATE)
                    .iter()
                    .filter_map(|value| value.to_str().ok()),
            ),
            text(headers, header::RETRY_AFTER),
            location.as_deref(),
        );
        self.last_status = Some(status.clone());
        let accepts_zstd = encoding::accepts_zstd(headers);
        let no_answer = |detail: String| Err(TransportError::NoAnswer { detail });
        // Every value of every line is read; a value that is not text is no encoding the client
        // reads.
        let content_encoding = match encoding::content_encoding(headers) {
            Ok(content_encoding) => content_encoding,
            Err(encoding) => {
                return no_answer(format!(
                    "{status}, with the content encoding `{}`, which the client does not read",
                    encoding.escape_debug()
                ))
            }
        };
        // `ureq` reads chunks in HTTP/1.1 alone; an HTTP/1.0 answer with the header is read to the
        // close of the connection, so the header counts for nothing there.
        let http11 = response.version() == Version::HTTP_11;
        let mut chunked = false;
        for value in headers.get_all(header::TRANSFER_ENCODING).iter().map(lossy) {
            for part in value.split(',').map(str::trim) {
                if part.eq_ignore_ascii_case("chunked") {
                    chunked = http11;
                } else {
                    return no_answer(format!(
                        "{status}, with the transfer encoding `{}`",
                        part.escape_debug()
                    ));
                }
            }
        }
        if status == Status::Success(204) {
            self.hub_accepts_zstd = accepts_zstd;
            return Ok(Answer::Message(Box::new(io::empty())));
        }
        let media = headers.get(header::CONTENT_TYPE).map(|value| {
            let value = lossy(value);
            value
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        });
        let case: fn(Body<'static>) -> Answer<'static> = match (&status, media.as_deref()) {
            (Status::Success(200), Some(MESSAGE_MEDIA_TYPE)) => Answer::Message,
            (status, Some(ERROR_MEDIA_TYPE)) if status.is_error() => Answer::Error,
            (status, Some(media)) => {
                return no_answer(format!(
                    "{status}, with a body of the media type `{}`",
                    media.escape_debug()
                ))
            }
            (status, None) => {
                return no_answer(format!("{status}, with a body without a media type"))
            }
        };
        if !chunked && !headers.contains_key(header::CONTENT_LENGTH) {
            return no_answer(format!(
                "{status}, with a body without a length, whose end cannot be told from a cut"
            ));
        }
        let stream = BufReader::new(Stream {
            reader: response.into_body().into_reader(),
            failed: None,
        });
        let body: Body<'static> = match content_encoding {
            ContentEncoding::Identity => Box::new(stream),
            ContentEncoding::Zstd => Box::new(BufReader::new(
                // A decoder that cannot be made, for want of memory, fails on this side; the
                // answer is not read.
                encoding::decoder(stream).map_err(TransportError::Carrier)?,
            )),
        };
        self.hub_accepts_zstd = accepts_zstd;
        Ok(case(body))
    }

    /// Sends a request and sorts its answer.
    fn request(
        &mut self,
        endpoint: Endpoint,
        method: Method<'_>,
    ) -> Result<Answer<'static>, TransportError> {
        let response = self.send(endpoint, method)?;
        self.classify(response)
    }
}

/// The method and the body of a request.
enum Method<'r> {
    /// A GET without a body.
    Get,
    /// A POST with an empty body.
    PostEmpty,
    /// A POST with the body of the reader.
    Post(&'r mut dyn Read),
}

impl Transport for HttpTransport {
    fn refs(&mut self) -> Result<Answer<'_>, TransportError> {
        self.request(Endpoint::Refs, Method::Get)
    }

    fn fetch(&mut self, request: &mut dyn Read) -> Result<Answer<'_>, TransportError> {
        self.request(Endpoint::Fetch, Method::Post(request))
    }

    fn ref_update(&mut self, request: &mut dyn Read) -> Result<Answer<'_>, TransportError> {
        self.request(Endpoint::Refs, Method::Post(request))
    }

    // The sign-in (ADR-024).

    fn challenge(&mut self) -> Result<Answer<'_>, TransportError> {
        self.request(Endpoint::Challenge, Method::PostEmpty)
    }

    fn answer_challenge(&mut self, request: &mut dyn Read) -> Result<SignedIn<'_>, TransportError> {
        let body = match self.request(Endpoint::Token, Method::Post(request))? {
            Answer::Message(body) => body,
            Answer::Error(body) => return Ok(SignedIn::Error(body)),
        };
        let mut bytes = Vec::new();
        body.take(MAX_SIGN_IN_MESSAGE + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| {
                match protocol_error(&error).or_else(|| encoding::decoding_error(&error)) {
                    // Chunks that cannot be read, and a body of zstd that cannot be decoded, came
                    // back from the other side.
                    Some(detail) => TransportError::NoAnswer { detail },
                    None => TransportError::Carrier(error),
                }
            })?;
        if bytes.len() as u64 > MAX_SIGN_IN_MESSAGE {
            return Err(TransportError::NoAnswer {
                detail: format!(
                    "the token answer of the sign-in is longer than {MAX_SIGN_IN_MESSAGE} bytes"
                ),
            });
        }
        // The error of the reader holds no part of the token (`TokenAnswer::parse`).
        let answer = TokenAnswer::parse(&bytes).map_err(|error| TransportError::NoAnswer {
            detail: format!("the token answer of the sign-in cannot be read: {error}"),
        })?;
        let lifetime = answer.lifetime();
        self.token = Some(answer.into_token());
        Ok(SignedIn::Accepted(lifetime))
    }

    // The statements on a hub (ADR-025, STEMMA-326). The range of a reading request travels as
    // the body, as `Range::encode` writes it, so the transport reads no message here either.

    fn submit_statement(&mut self, request: &mut dyn Read) -> Result<Answer<'_>, TransportError> {
        self.request(Endpoint::Statements, Method::Post(request))
    }

    fn statements(&mut self, request: &mut dyn Read) -> Result<Answer<'_>, TransportError> {
        self.request(Endpoint::StatementRange, Method::Post(request))
    }

    fn record(&mut self, request: &mut dyn Read) -> Result<Answer<'_>, TransportError> {
        self.request(Endpoint::RecordRange, Method::Post(request))
    }
}

/// The value of a header as text, if it is there once or more (the first one) and is text.
fn text(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// The text of a header value, with every sequence that is no UTF-8 replaced by U+FFFD, for a
/// message and a comparison.
fn lossy(value: &header::HeaderValue) -> String {
    String::from_utf8_lossy(value.as_bytes()).into_owned()
}

/// Why a request failed in `ureq`: no answer of the protocol when something answered that is no
/// HTTP answer of the other side (another service behind the port, a head that is too large, a
/// proxy that refused the tunnel, a body that stalled), and a failure of the carrier for every
/// other error, among them the errors of I/O and of TLS. A request of this crate cannot meet the
/// errors of `ureq` that a malformed request would give (a bad address or header), because its
/// address and its headers are checked before.
fn failure(error: ureq::Error) -> TransportError {
    match error {
        ureq::Error::Io(error) => TransportError::Carrier(error),
        ureq::Error::Timeout(_) => {
            TransportError::Carrier(io::Error::new(io::ErrorKind::TimedOut, error))
        }
        ureq::Error::Protocol(_)
        | ureq::Error::LargeResponseHeader(..)
        | ureq::Error::ConnectProxyFailed(_)
        | ureq::Error::BodyStalled => TransportError::NoAnswer {
            detail: error.to_string(),
        },
        other => TransportError::Carrier(io::Error::other(other)),
    }
}

/// The reader of the body of a request, which keeps the error of the reader, so that a failure
/// on this side is told from a failure of the carrier.
struct Tracked<'r> {
    inner: &'r mut dyn Read,
    error: Option<io::Error>,
}

impl Read for Tracked<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.inner.read(buf) {
                // `ureq` does not read again after `Interrupted`, so the reader is asked here.
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    let kind = error.kind();
                    self.error = Some(error);
                    return Err(io::Error::new(
                        kind,
                        "the body of the request could not be read",
                    ));
                }
                result => return result,
            }
        }
    }
}

/// The text of an error of `ureq` for a part of an answer that is no HTTP, such as chunks that
/// cannot be read, if the error of I/O is one.
fn protocol_error(error: &io::Error) -> Option<String> {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<ureq::Error>())
        .filter(|inner| matches!(inner, ureq::Error::Protocol(_)))
        .map(ToString::to_string)
}

/// The body of an answer. A timeout is an error of the kind `TimedOut`, and an error stays: every
/// read after the first error fails with the same kind and text, because `ureq` would answer it
/// with an end, which a reader could take for the end of the body.
struct Stream {
    reader: ureq::BodyReader<'static>,
    failed: Option<(io::ErrorKind, String)>,
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some((kind, text)) = &self.failed {
            return Err(io::Error::new(*kind, text.clone()));
        }
        self.reader.read(buf).map_err(|error| {
            let timed_out = error
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<ureq::Error>())
                .is_some_and(|inner| matches!(inner, ureq::Error::Timeout(_)));
            let error = if timed_out {
                io::Error::new(io::ErrorKind::TimedOut, error)
            } else {
                error
            };
            if error.kind() != io::ErrorKind::Interrupted {
                self.failed = Some((error.kind(), error.to_string()));
            }
            error
        })
    }
}
