//! The way a clone reaches the other side (ADR-022): the trait [`Transport`], and the transport
//! in memory that the tests use ([`MemoryTransport`]).
//!
//! A transport is pure I/O. It carries the bytes of a request to the other side and hands the
//! bytes of the answer back, and it says whether the answer is the message that was asked for or
//! an error in its place ([`Answer`]). It reads no message: the caller writes a request with the
//! writers of [`protocol`](crate::protocol) and reads an answer with its readers. So a rule of a
//! `fetch` or a `push` lives in the core once, whatever carries the bytes, and can be tested
//! without a socket.
//!
//! The trait has one method per request of the protocol (ADR-023). This version has two
//! requests, the list of references and the fetch; the slices that add a request add its
//! method, and the sign-in adds its step. The ref update and its receipt exist as messages
//! already ([`record`](crate::protocol::record), ADR-025); their request comes with the slice
//! of `push`, which builds the operation that answers it.
//!
//! The trait names nothing of a carrier: no address, no path, no status, no header, and no
//! compression. They belong to the implementation, as TLS and the token do (ADR-023). What a
//! carrier knows beyond the two kinds of answer, it says in the words of
//! [`TransportError::NoAnswer`].
//!
//! Every body is a stream. A request is read from a reader and an answer is read from one, so an
//! object stream never has to lie in memory as a whole. The bounds for the size of a message are
//! set by the side that reads it (ADR-023).

use std::error::Error;
use std::fmt;
use std::io::{self, BufRead, Cursor, Read, Write};

use crate::protocol::{ErrorMessage, FetchRequest, RefList};

/// The body of an answer, as a stream. It borrows the transport, so one request runs at a time.
///
/// A body that ends before the other side has sent all of it is reported by the carrier: its
/// reader returns an error, never an early end. No message carries a count or an end mark, so a
/// reader of the protocol cannot tell every early end: it refuses a cut inside a line or a frame
/// and a cut before a line that must be there, and reads a cut after a whole line or a whole
/// frame as a shorter message (ADR-023).
///
/// The caller may drop a body before its end, for example after a reader refused it. The next
/// request is allowed then, and the transport discards what was left.
pub type Body<'a> = Box<dyn BufRead + 'a>;

/// What the other side answered to a request. The transport tells the two cases apart, because
/// a small message has no line that names its type and an error begins with the same first line
/// as every message; the caller chooses the reader by the case.
pub enum Answer<'a> {
    /// The message that the request asks for.
    Message(Body<'a>),
    /// An error in its place, for the reader [`ErrorMessage::parse`].
    Error(Body<'a>),
}

impl fmt::Debug for Answer<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // A body is a stream that is not read yet, so only the case is shown.
        f.write_str(match self {
            Self::Message(_) => "Answer::Message(..)",
            Self::Error(_) => "Answer::Error(..)",
        })
    }
}

/// The requests of the protocol, as a clone sends them to the other side (ADR-022, ADR-023).
///
/// The trait is synchronous: a method returns when the answer begins, and the body is read
/// afterwards by the caller. An implementation does I/O and nothing else. It does not read, check
/// or change a message, and it decides nothing about a repository.
pub trait Transport {
    /// Asks for the list of references. The message of the answer is a [`RefList`].
    ///
    /// # Errors
    ///
    /// [`TransportError::Carrier`] if the carrier fails, and [`TransportError::NoAnswer`] if
    /// what comes back is no answer of the other side.
    fn refs(&mut self) -> Result<Answer<'_>, TransportError>;

    /// Sends a fetch request, which `request` holds as [`FetchRequest::encode`] writes it. The
    /// message of the answer is an object stream
    /// ([`ObjectStreamReader`](crate::protocol::ObjectStreamReader)).
    ///
    /// # Errors
    ///
    /// [`TransportError::Request`] if `request` cannot be read, [`TransportError::Carrier`] if
    /// the carrier fails, and [`TransportError::NoAnswer`] if what comes back is no answer of the
    /// other side.
    fn fetch(&mut self, request: &mut dyn Read) -> Result<Answer<'_>, TransportError>;
}

/// Why a request got no answer. An error that the other side answers is no failure of the
/// transport: it is [`Answer::Error`].
#[derive(Debug)]
#[non_exhaustive]
pub enum TransportError {
    /// The body of the request could not be read from its reader, which is on this side: for an
    /// upload it is the local store. Whether anything reached the other side is not known.
    Request(io::Error),
    /// The carrier failed: the other side was not reached, or the connection broke before the
    /// answer began.
    Carrier(io::Error),
    /// Something came back that is no answer of the other side in the protocol: the other side
    /// failed before it could answer, or something between the two sides answered in its place.
    NoAnswer {
        /// What came back, in the words of the carrier.
        detail: String,
    },
}

impl TransportError {
    /// The error of the reader of the request or of the carrier, if that is what failed.
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Request(error) | Self::Carrier(error) => Some(error),
            Self::NoAnswer { .. } => None,
        }
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Request(error) => write!(f, "the request could not be read: {error}"),
            Self::Carrier(error) => write!(f, "the transport failed: {error}"),
            Self::NoAnswer { detail } => {
                write!(f, "the other side gave no answer of the protocol: {detail}")
            }
        }
    }
}

impl Error for TransportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            // The message includes the message of the cause, so the source is the cause of the
            // cause (see the crate documentation).
            Self::Request(error) | Self::Carrier(error) => error.source(),
            Self::NoAnswer { .. } => None,
        }
    }
}

/// The other side of a [`MemoryTransport`]: what a repository answers to each request, through
/// the operations of the core.
///
/// The methods take the values of [`protocol`](crate::protocol): the transport reads the bytes of
/// a request before it asks. A small answer is returned as a value, which the transport encodes;
/// an object stream is written by this side itself, and the transport passes its bytes on as
/// they are. The answering operation of the core for `refs` and `fetch` is the implementation
/// [`Answering`](crate::serve::Answering), for the tests that fetch from a repository; the
/// accepting operation follows with the requests of a push. A test may implement the trait
/// itself to answer what a correct side never answers.
pub trait OtherSide {
    /// Answers the list of references.
    ///
    /// # Errors
    ///
    /// A [`Refusal`]: the error message that this side answers, or the failure that kept it from
    /// answering.
    fn refs(&self) -> Result<RefList, Refusal>;

    /// Answers a fetch request: writes the object stream, with its first line, to `stream`.
    ///
    /// # Errors
    ///
    /// A [`Refusal`]: the error message that this side answers, or the failure that kept it from
    /// answering. What was written to `stream` before is discarded.
    fn fetch(&self, request: &FetchRequest, stream: &mut dyn Write) -> Result<(), Refusal>;
}

/// Why an [`OtherSide`] did not answer with the message that was asked for.
#[derive(Debug)]
#[non_exhaustive]
pub enum Refusal {
    /// It answers this error, which travels as [`Answer::Error`].
    Error(ErrorMessage),
    /// It failed before it could answer, for example because its store could not be read. The
    /// text reaches the caller as [`TransportError::NoAnswer`].
    Failed(String),
}

impl From<ErrorMessage> for Refusal {
    fn from(message: ErrorMessage) -> Self {
        Self::Error(message)
    }
}

/// A [`Transport`] in memory, for tests: it joins the caller to an [`OtherSide`] in the same
/// process, with no socket and no second server (ADR-022).
///
/// Between the two sides travel bytes, as over a network. A request is read to its end and
/// parsed by the strict reader of the protocol, as a hub parses it. A list of references and an
/// error are encoded by their writers; an object stream is passed on as the other side wrote it,
/// unread. So the caller reads an answer with the reader that it uses for every transport. A
/// request that the reader refuses is answered with the error of the protocol
/// ([`ErrorMessage`]): `unsupported-version` for another protocol version, `malformed` for
/// everything else, and the other side is not asked.
///
/// The answer is complete before the method returns, and it is held in memory. This transport
/// compresses nothing and sets no bound for the size of a request.
#[derive(Debug)]
pub struct MemoryTransport<S> {
    side: S,
}

impl<S: OtherSide> MemoryTransport<S> {
    /// A transport to `side`.
    pub fn new(side: S) -> Self {
        Self { side }
    }

    /// The other side.
    pub fn side(&self) -> &S {
        &self.side
    }
}

/// The answer of the other side as it travels: the encoded message, the encoded error, or no
/// answer.
fn answer<'a>(result: Result<Vec<u8>, Refusal>) -> Result<Answer<'a>, TransportError> {
    match result {
        Ok(message) => Ok(Answer::Message(Box::new(Cursor::new(message)))),
        Err(Refusal::Error(error)) => Ok(Answer::Error(Box::new(Cursor::new(error.encode())))),
        Err(Refusal::Failed(detail)) => Err(TransportError::NoAnswer { detail }),
    }
}

impl<S: OtherSide> Transport for MemoryTransport<S> {
    fn refs(&mut self) -> Result<Answer<'_>, TransportError> {
        answer(self.side.refs().map(|list| list.encode()))
    }

    fn fetch(&mut self, request: &mut dyn Read) -> Result<Answer<'_>, TransportError> {
        let mut bytes = Vec::new();
        request
            .read_to_end(&mut bytes)
            .map_err(TransportError::Request)?;
        let request = match FetchRequest::parse(&bytes) {
            Ok(request) => request,
            Err(error) => return answer(Err(ErrorMessage::from(&error).into())),
        };
        let mut stream = Vec::new();
        answer(self.side.fetch(&request, &mut stream).map(|()| stream))
    }
}

#[cfg(test)]
mod tests;
