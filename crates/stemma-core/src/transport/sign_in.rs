//! The sign-in of a client at a hub (ADR-024), over any [`Transport`]: the one place that holds
//! the order of its two requests and the rule that a challenge for another address is not
//! signed.

use std::error::Error;
use std::fmt;
use std::io::{self, Read};
use std::time::Duration;

use super::{Answer, Body, SignedIn, Transport, TransportError};
use crate::protocol::record::HubAddress;
use crate::protocol::sign_in::{AnswerError, Challenge};
use crate::protocol::{ErrorMessage, ParseError};
use crate::signature::{PublicKey, Signer};

/// The longest challenge and the longest error message that [`sign_in`] reads, in bytes. It is
/// the bound of the client for the answers of a sign-in (ADR-023); both messages are a few lines.
pub const MAX_SIGN_IN_MESSAGE: u64 = 64 * 1024;

/// Signs in at a hub: asks for a challenge, signs it with the key of the sender, and sends the
/// answer. Afterwards the transport holds a token for the requests that follow; the caller gets
/// its lifetime, counted from the answer of the hub, and never the token.
///
/// `called` is the address under which the client calls the hub, the part of the address of the
/// remote before the namespace, and `key` is the public key that `signer` signs with. The signer
/// is asked once, and only for a challenge that names `called` ([`Challenge::answer`]).
///
/// A sign-in writes nothing: no file and no entry of a log. It can be repeated, and a command
/// that outlives its token signs in again.
///
/// # Errors
///
/// [`SignInError::Transport`] if a request gets no answer, [`SignInError::Refused`] if the other
/// side answers one of the two requests with an error, [`SignInError::Body`] and
/// [`SignInError::TooLarge`] if an answer cannot be read to its end or is longer than
/// [`MAX_SIGN_IN_MESSAGE`], [`SignInError::Challenge`] and [`SignInError::Error`] if the reader
/// of the challenge or of the error refuses it, and [`SignInError::Answer`] if the challenge
/// names another address than `called` or cannot be signed. The transport is signed in only if
/// the result is `Ok`; after an error it holds the token that it had, or none.
pub fn sign_in(
    transport: &mut dyn Transport,
    called: &HubAddress,
    key: &PublicKey,
    signer: &dyn Signer,
) -> Result<Duration, SignInError> {
    let challenge = match transport.challenge().map_err(SignInError::Transport)? {
        Answer::Message(body) => Challenge::parse(&read(body)?).map_err(SignInError::Challenge)?,
        Answer::Error(body) => return Err(refused(body)),
    };
    let signed = challenge
        .answer(called, key, signer)
        .map_err(SignInError::Answer)?;
    let mut request = &signed.encode()[..];
    match transport
        .answer_challenge(&mut request)
        .map_err(SignInError::Transport)?
    {
        SignedIn::Accepted(lifetime) => Ok(lifetime),
        SignedIn::Error(body) => Err(refused(body)),
    }
}

/// Reads a small answer to its end, up to the bound of this side.
fn read(body: Body<'_>) -> Result<Vec<u8>, SignInError> {
    let mut bytes = Vec::new();
    body.take(MAX_SIGN_IN_MESSAGE + 1)
        .read_to_end(&mut bytes)
        .map_err(SignInError::Body)?;
    if bytes.len() as u64 > MAX_SIGN_IN_MESSAGE {
        return Err(SignInError::TooLarge);
    }
    Ok(bytes)
}

/// The error that the other side answered, or why it cannot be read.
fn refused(body: Body<'_>) -> SignInError {
    match read(body) {
        Ok(bytes) => match ErrorMessage::parse(&bytes) {
            Ok(message) => SignInError::Refused(message),
            Err(error) => SignInError::Error(error),
        },
        Err(error) => error,
    }
}

/// Why a sign-in did not yield a token.
#[derive(Debug)]
#[non_exhaustive]
pub enum SignInError {
    /// One of the two requests got no answer.
    Transport(TransportError),
    /// The body of an answer could not be read to its end.
    Body(io::Error),
    /// An answer is longer than [`MAX_SIGN_IN_MESSAGE`].
    TooLarge,
    /// The reader of the challenge refused what the other side sent as one.
    Challenge(ParseError),
    /// The challenge was not answered: it names another address than the one that was called,
    /// or the signer failed.
    Answer(AnswerError),
    /// The other side answered with an error in place of the challenge or of the token answer.
    Refused(ErrorMessage),
    /// The other side answered with an error, and the reader of the error refused it.
    Error(ParseError),
}

impl SignInError {
    /// The error of the carrier, of the body of an answer, or of the signer, if that is what
    /// failed.
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Transport(error) => error.io_error(),
            Self::Body(error) => Some(error),
            Self::Answer(error) => error.io_error(),
            Self::TooLarge | Self::Challenge(_) | Self::Refused(_) | Self::Error(_) => None,
        }
    }
}

impl fmt::Display for SignInError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "the sign-in got no answer: {error}"),
            Self::Body(error) => write!(f, "cannot read an answer of the sign-in: {error}"),
            Self::TooLarge => write!(
                f,
                "an answer of the sign-in is longer than {MAX_SIGN_IN_MESSAGE} bytes"
            ),
            Self::Challenge(error) => write!(f, "cannot read the challenge: {error}"),
            Self::Answer(error) => write!(f, "the challenge was not answered: {error}"),
            Self::Refused(message) => write!(f, "the sign-in was refused: {message}"),
            Self::Error(error) => {
                write!(f, "cannot read the error that answers the sign-in: {error}")
            }
        }
    }
}

impl Error for SignInError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            // Each message includes the message of its cause, so the source is the cause of the
            // cause (see the crate documentation).
            Self::Transport(error) => error.source(),
            Self::Body(error) => error.source(),
            Self::Challenge(error) | Self::Error(error) => error.source(),
            Self::Answer(error) => error.source(),
            Self::Refused(message) => message.source(),
            Self::TooLarge => None,
        }
    }
}

#[cfg(test)]
mod tests;
