//! The sign-in at a hub (ADR-024): the challenge of the hub, the signed challenge that the client
//! answers, and the token that the hub issues for it.
//!
//! The three messages are line-oriented text, as the other small messages are:
//!
//! ```text
//! stemma-protocol 1                         the challenge
//! hub <address of the hub>
//! nonce <32 to 128 lowercase hexadecimal digits>
//!
//! stemma-protocol 1                         the signed challenge
//! hub <address of the hub>
//! nonce <the nonce of the challenge>
//! key ssh-ed25519 <base64>
//! signature <SSHSIG signature on one line>
//!
//! stemma-protocol 1                         the token answer
//! token <token>
//! lifetime <seconds>
//! ```
//!
//! The lines of a signed challenge between the first line and the `signature` line are the
//! payload that the key signs, a proof of the type [`PayloadKind::Challenge`]. The type tag is one
//! of its own, so the signature never counts as a log entry, an approval, a ref update, or any
//! other proof, and no other proof counts as a sign-in. The payload names the address of the hub
//! as the client calls it, so another hub cannot pass a challenge on, the nonce, so the answer
//! counts for one challenge, and the key.
//!
//! The client signs through [`Challenge::answer`], which refuses a challenge that names another
//! address than the one the client calls, before the signer is asked. The hub checks a signed
//! challenge with [`SignedChallenge::check`], and against a register with
//! [`SignedChallenge::actor`]. The nonce is the hub's: it derives it from a secret of its own and
//! accepts it for seconds (ADR-024), so whether a nonce is one of this hub and still counts is
//! decided by the hub and not here.
//!
//! The token answer holds the [`Token`], the one secret of the protocol. A token says which key
//! speaks and nothing else; it is no proof. It lives in the memory of the transport only: it is
//! written to no file and to no log, it has no `Display` form, and its `Debug` form does not show
//! it. No error of this module quotes a token. The message of the token answer is read by the
//! transport, which keeps the token and hands the caller the lifetime alone
//! ([`Transport::answer_challenge`](crate::transport::Transport::answer_challenge)).
//!
//! Not part of this module: the kind of error that answers a refused sign-in, and the carriage
//! of the token with the requests that follow (the `Authorization` header, ADR-023). They come
//! with the slices that build the hub and the HTTP transport.

use std::error::Error;
use std::fmt;
use std::num::NonZeroU64;
use std::str::FromStr;
use std::time::Duration;

use super::record::{HubAddress, SealError};
use super::{
    first_line, malformed, parse_key, parse_positive, Lines, Malformed, ParseError, VERSION,
};
use crate::register::{signature_from_line, signature_to_line, Identity, KeyState, Register};
use crate::signature::{
    self, PayloadKind, PublicKey, Signature, Signer, VerifyError, FORMAT_VERSION,
};

/// The shortest nonce, in bytes: 128 bits.
const MIN_NONCE_LEN: usize = 16;

/// The longest nonce, in bytes. A hub that derives its nonce from a time, a random value and a
/// keyed hash has room for all three.
const MAX_NONCE_LEN: usize = 64;

/// What is wrong with a message of the sign-in beyond the shape of its lines. The parts refuse a
/// value by it ([`Nonce::try_new`], [`Token::try_new`]), and the readers refuse a line by it
/// ([`ParseError::SignIn`]), so both sides name a broken rule with the same word. An address
/// that is refused is [`RecordProblem::Address`](super::record::RecordProblem::Address), the
/// word of [`HubAddress`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SignInProblem {
    /// The nonce is not 16 to 64 bytes, written as 32 to 128 lowercase hexadecimal digits.
    Nonce,
    /// The signature is not an SSHSIG signature of an Ed25519 key for the namespace of the
    /// proofs, written on one line.
    SignatureForm,
    /// The signature names another key than the `key` line: another key signed, or the `key`
    /// line was changed.
    Key,
    /// The signature names the key of the message and does not verify for it and the payload:
    /// the address or the nonce was changed, or the signature is not one of a challenge.
    Signature,
    /// The token is empty or holds another character than a letter, a digit, `-`, `.`, `_`,
    /// `~`, `+`, `/`, and `=` at its end.
    Token,
}

impl fmt::Display for SignInProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Nonce => "a nonce is 32 to 128 lowercase hexadecimal digits, two for each byte",
            Self::SignatureForm => {
                "the signature is not an SSHSIG signature of an Ed25519 key on one line"
            }
            Self::Key => "the signature was made by another key than the key of the sign-in",
            Self::Signature => {
                "the signature does not verify for the key, the address and the nonce of the \
                 challenge"
            }
            Self::Token => {
                "a token is letters, digits, `-`, `.`, `_`, `~`, `+` and `/`, with `=` at its \
                 end only"
            }
        })
    }
}

impl Error for SignInProblem {}

fn problem(line: usize, reason: SignInProblem) -> ParseError {
    ParseError::SignIn { line, reason }
}

/// The nonce of a challenge: 16 to 64 bytes that a hub chose, written as lowercase hexadecimal
/// digits. A client reads nothing from it and signs it as it is.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Nonce(Vec<u8>);

impl Nonce {
    /// Wraps the bytes of a nonce.
    ///
    /// # Errors
    ///
    /// [`SignInProblem::Nonce`] unless there are 16 to 64 bytes.
    pub fn try_new(bytes: impl Into<Vec<u8>>) -> Result<Self, SignInProblem> {
        let bytes = bytes.into();
        if (MIN_NONCE_LEN..=MAX_NONCE_LEN).contains(&bytes.len()) {
            Ok(Self(bytes))
        } else {
            Err(SignInProblem::Nonce)
        }
    }

    /// Returns the bytes of the nonce.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Display for Nonce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl fmt::Debug for Nonce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Nonce({self})")
    }
}

impl FromStr for Nonce {
    type Err = SignInProblem;

    /// Reads the canonical form only: uppercase digits and half a byte are refused, so that a
    /// nonce has one text.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let nibble = |digit: u8| match digit {
            b'0'..=b'9' => Ok(digit - b'0'),
            b'a'..=b'f' => Ok(digit - b'a' + 10),
            _ => Err(SignInProblem::Nonce),
        };
        let (pairs, half) = text.as_bytes().as_chunks::<2>();
        if !half.is_empty() {
            return Err(SignInProblem::Nonce);
        }
        let bytes = pairs
            .iter()
            .map(|[high, low]| Ok((nibble(*high)? << 4) | nibble(*low)?))
            .collect::<Result<Vec<u8>, SignInProblem>>()?;
        Self::try_new(bytes)
    }
}

/// The challenge of a hub (ADR-024): the address under which the client calls the hub, and a
/// nonce. It is the answer to the first request of a sign-in and holds no secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenge {
    hub: HubAddress,
    nonce: Nonce,
}

impl Challenge {
    /// A challenge of the hub at this address.
    pub fn new(hub: HubAddress, nonce: Nonce) -> Self {
        Self { hub, nonce }
    }

    /// The address that the hub names as its own.
    pub fn hub(&self) -> &HubAddress {
        &self.hub
    }

    /// The nonce.
    pub fn nonce(&self) -> &Nonce {
        &self.nonce
    }

    /// The lines `hub` and `nonce`.
    fn lines(&self) -> String {
        format!("hub {}\nnonce {}\n", self.hub, self.nonce)
    }

    /// Writes the message.
    pub fn encode(&self) -> Vec<u8> {
        (first_line(VERSION) + &self.lines()).into_bytes()
    }

    /// Reads the message, as [`encode`](Self::encode) writes it, and nothing else.
    ///
    /// # Errors
    ///
    /// Returns an error for input that is not UTF-8, a first line that is missing or malformed,
    /// another protocol version, a line that is missing, repeated, unknown, or out of order, a
    /// last line without a line feed, an address that [`HubAddress::try_new`] refuses, and a
    /// nonce that is not in canonical form. The first problem is reported, with its line.
    pub fn parse(bytes: &[u8]) -> Result<Self, ParseError> {
        let mut lines = Lines::open(bytes)?;
        let challenge = read_challenge(&mut lines)?;
        lines.end()?;
        Ok(challenge)
    }

    /// Signs the challenge with the key of the sender and returns the answer of the client.
    /// `called` is the address under which the client calls the hub, the part of the address of
    /// its remote before the namespace.
    ///
    /// The address is compared byte for byte, as [`HubAddress`] keeps it, and the comparison
    /// comes before the signature: nothing is signed for an address that the client did not
    /// call, and a key that asks for a touch is not asked.
    ///
    /// # Errors
    ///
    /// [`AnswerError::OtherAddress`] if the challenge names another address than `called`, and
    /// [`AnswerError::Seal`] if the signer fails or its signature does not verify for `key`, for
    /// example because the signer holds another key.
    pub fn answer(
        &self,
        called: &HubAddress,
        key: &PublicKey,
        signer: &dyn Signer,
    ) -> Result<SignedChallenge, AnswerError> {
        if &self.hub != called {
            return Err(AnswerError::OtherAddress {
                named: self.hub.clone(),
                called: called.clone(),
            });
        }
        let payload = payload(self, key);
        let signature = signature::sign(signer, PayloadKind::Challenge, &payload)
            .map_err(|error| AnswerError::Seal(SealError::Sign(error)))?;
        SignedChallenge::try_new(self.clone(), key.clone(), signature)
            .map_err(|error| AnswerError::Seal(SealError::Verify(error)))
    }
}

/// Reads the lines `hub` and `nonce`.
fn read_challenge(lines: &mut Lines<'_>) -> Result<Challenge, ParseError> {
    let (line, value) = lines.require("hub")?;
    let hub = HubAddress::try_new(value).map_err(|reason| ParseError::Record { line, reason })?;
    let (line, value) = lines.require("nonce")?;
    let nonce = value.parse().map_err(|reason| problem(line, reason))?;
    Ok(Challenge { hub, nonce })
}

/// The bytes that the key of a sign-in signs: the lines of the message from `hub` to `key`.
fn payload(challenge: &Challenge, key: &PublicKey) -> Vec<u8> {
    format!("{}key {key}\n", challenge.lines()).into_bytes()
}

/// Why a challenge was not answered.
#[derive(Debug)]
#[non_exhaustive]
pub enum AnswerError {
    /// The challenge names another address than the one the client calls: the hub is reached
    /// under an address that it does not know as its own, or something between the two sides
    /// passes on the challenge of another hub. Nothing was signed.
    OtherAddress {
        /// The address that the challenge names.
        named: HubAddress,
        /// The address that the client calls.
        called: HubAddress,
    },
    /// The challenge could not be signed.
    Seal(SealError),
}

impl AnswerError {
    /// The error of the operating system, if the signer failed with one.
    pub fn io_error(&self) -> Option<&std::io::Error> {
        match self {
            Self::Seal(error) => error.io_error(),
            Self::OtherAddress { .. } => None,
        }
    }
}

impl fmt::Display for AnswerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OtherAddress { named, called } => write!(
                f,
                "the challenge is one of the hub at {named}, and this side calls {called}; \
                 nothing was signed"
            ),
            Self::Seal(error) => error.fmt(f),
        }
    }
}

// The message of `Seal` is the message of its cause, which has no cause of its own (see the
// crate documentation).
impl Error for AnswerError {}

/// The answer of a client to a challenge (ADR-024): the challenge, the public key of the sender,
/// and the signature of that key over the three. Every value carries a signature that verifies,
/// whether it was built here or read from a message.
///
/// A value says that the holder of the key signed this nonce for this address. It does not say
/// that the address is that of the hub that reads it, that the nonce is one of that hub, or that
/// the key is known: see [`check`](Self::check) and [`actor`](Self::actor).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedChallenge {
    challenge: Challenge,
    key: PublicKey,
    signature: Signature,
}

impl SignedChallenge {
    /// Joins a challenge, a key, and the signature of the key.
    ///
    /// # Errors
    ///
    /// The error of [`signature::verify`] if the signature is not one of `key` over the address,
    /// the nonce and the key, as a proof of the type [`PayloadKind::Challenge`].
    pub fn try_new(
        challenge: Challenge,
        key: PublicKey,
        signature: Signature,
    ) -> Result<Self, VerifyError> {
        signature::verify(
            &key,
            PayloadKind::Challenge,
            FORMAT_VERSION,
            &payload(&challenge, &key),
            &signature,
        )?;
        Ok(Self {
            challenge,
            key,
            signature,
        })
    }

    /// The address that the key signed.
    pub fn hub(&self) -> &HubAddress {
        &self.challenge.hub
    }

    /// The nonce that the key signed. Whether it is a nonce of this hub that still counts is the
    /// question of the hub.
    pub fn nonce(&self) -> &Nonce {
        &self.challenge.nonce
    }

    /// The public key that signed. Which actor it speaks for is decided by a register
    /// ([`actor`](Self::actor)), not by the message.
    pub fn key(&self) -> &PublicKey {
        &self.key
    }

    /// The signature of the key.
    pub fn signature(&self) -> &Signature {
        &self.signature
    }

    /// The bytes that the key signed, as a proof of the type [`PayloadKind::Challenge`]: the
    /// lines of the message from `hub` to `key`.
    pub fn payload(&self) -> Vec<u8> {
        payload(&self.challenge, &self.key)
    }

    /// Writes the message.
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = first_line(VERSION).into_bytes();
        bytes.extend_from_slice(&self.payload());
        bytes.extend_from_slice(
            format!("signature {}\n", signature_to_line(&self.signature)).as_bytes(),
        );
        bytes
    }

    /// Reads the message, as [`encode`](Self::encode) writes it, and nothing else, and checks its
    /// signature.
    ///
    /// # Errors
    ///
    /// Returns an error for input that is not UTF-8, a first line that is missing or malformed,
    /// another protocol version, a line that is missing, repeated, unknown, or out of order, a
    /// last line without a line feed, an address, a nonce, or a key that is not in canonical
    /// form, and a signature that is not in its one-line form. The first problem is reported,
    /// with its line. A message whose form is right is refused last for its signature, at the
    /// `signature` line: as [`SignInProblem::Key`] if the signature names another key than the
    /// `key` line, and as [`SignInProblem::Signature`] if it names that key and does not verify.
    pub fn parse(bytes: &[u8]) -> Result<Self, ParseError> {
        let (unverified, line) = read_signed(bytes)?;
        Self::try_new(unverified.challenge, unverified.key, unverified.signature).map_err(|error| {
            let reason = match error {
                VerifyError::WrongKey => SignInProblem::Key,
                _ => SignInProblem::Signature,
            };
            problem(line, reason)
        })
    }

    /// Reads the form of the message without checking the signature, for the fuzz target: a
    /// fuzzer makes no signature, so the check of the signature would hide the reader from it.
    #[cfg(any(test, fuzzing))]
    pub(crate) fn parse_unverified(bytes: &[u8]) -> Result<Self, ParseError> {
        read_signed(bytes).map(|(unverified, _)| unverified)
    }

    /// The check of a hub that needs no register: the challenge was signed for this hub.
    /// `hub` is the public address of the hub that checks.
    ///
    /// The signature is checked already, by every way to a value of this type. The nonce is not
    /// checked here: the caller refuses one that it did not issue or that is too old before it
    /// issues a token.
    ///
    /// # Errors
    ///
    /// [`Refused::OtherAddress`] if the key signed another address, compared byte for byte.
    pub fn check(&self, hub: &HubAddress) -> Result<(), Refused> {
        if self.hub() == hub {
            Ok(())
        } else {
            Err(Refused::OtherAddress)
        }
    }

    /// The check of a hub against a register: the challenge was signed for this hub
    /// ([`check`](Self::check)), by a key that the register holds as an active key. Returns the
    /// identity that the key speaks for, a human or an agent.
    ///
    /// A retired key and a revoked key sign nothing new, so neither signs in, whatever the
    /// number from which it no longer counts. The nonce is not checked here, as in
    /// [`check`](Self::check).
    ///
    /// # Errors
    ///
    /// [`Refused::OtherAddress`] if the key signed another address, and [`Refused::NotActive`]
    /// if no entry of the register holds the key as an active key: the key is unknown, retired,
    /// or revoked. The address is judged first.
    pub fn actor<'a>(
        &self,
        hub: &HubAddress,
        register: &'a Register,
    ) -> Result<&'a Identity, Refused> {
        self.check(hub)?;
        register
            .entries()
            .iter()
            .find(|entry| {
                entry
                    .keys()
                    .iter()
                    .any(|held| held.key == self.key && held.state == KeyState::Active)
            })
            .map(|entry| entry.identity())
            .ok_or(Refused::NotActive)
    }
}

/// Reads the lines of a signed challenge and returns it with the number of its `signature` line.
/// The signature is not checked, so the value is not one that a caller may see.
fn read_signed(bytes: &[u8]) -> Result<(SignedChallenge, usize), ParseError> {
    let mut lines = Lines::open(bytes)?;
    let challenge = read_challenge(&mut lines)?;
    let (line, value) = lines.require("key")?;
    let key = parse_key(line, value)?;
    let (line, value) = lines.require("signature")?;
    let signature =
        signature_from_line(value).ok_or(problem(line, SignInProblem::SignatureForm))?;
    lines.end()?;
    let unverified = SignedChallenge {
        challenge,
        key,
        signature,
    };
    Ok((unverified, line))
}

/// Why a hub issues no token for a signed challenge ([`SignedChallenge::check`],
/// [`SignedChallenge::actor`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Refused {
    /// The challenge was signed for another address than that of this hub: it was issued by
    /// another hub, or the client calls this hub under a name that it does not know as its own.
    OtherAddress,
    /// No entry of the register holds the key as an active key.
    NotActive,
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::OtherAddress => "the challenge was signed for another address than this hub's",
            Self::NotActive => "the register does not hold the key of the sign-in as an active key",
        })
    }
}

impl Error for Refused {}

/// The token of a hub (ADR-024): a short-lived credential that says which key speaks. It is the
/// one secret of the protocol, so it is kept apart from every other value: it has no `Display`
/// form, its `Debug` form does not show it, and it is compared without an early end.
///
/// The text is what an `Authorization: Bearer` header carries (ADR-023): at least one letter,
/// digit, `-`, `.`, `_`, `~`, `+`, or `/`, then any number of `=`. Its content is the hub's; a
/// client reads nothing from it.
#[derive(Clone)]
pub struct Token(String);

impl Token {
    /// Checks the text of a token.
    ///
    /// # Errors
    ///
    /// [`SignInProblem::Token`] if the text is empty, holds another character than the ones
    /// above, or has a `=` before its end.
    pub fn try_new(text: impl Into<String>) -> Result<Self, SignInProblem> {
        let text = text.into();
        let body = text.trim_end_matches('=');
        let allowed = |c: char| c.is_ascii_alphanumeric() || "-._~+/".contains(c);
        if !body.is_empty() && body.chars().all(allowed) {
            Ok(Self(text))
        } else {
            Err(SignInProblem::Token)
        }
    }

    /// The text of the token, for the one place that sends it: the transport, with a request to
    /// the hub that issued it. Do not print it, log it, or write it to a file.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(..)")
    }
}

impl PartialEq for Token {
    /// Compares every byte, so the time of a comparison does not tell where two tokens of the
    /// same length differ.
    fn eq(&self, other: &Self) -> bool {
        let (one, other) = (self.0.as_bytes(), other.0.as_bytes());
        one.len() == other.len()
            && one
                .iter()
                .zip(other)
                .fold(0, |differences, (a, b)| differences | (a ^ b))
                == 0
    }
}

impl Eq for Token {}

/// The answer of a hub to a signed challenge that it accepts (ADR-024): the token and its
/// lifetime in seconds, from 1. The lifetime is a setting of the hub and counts from the answer;
/// a token must be valid when a request begins, not until it ends (ADR-023).
///
/// This is the one message that holds a secret. A transport reads it and keeps the token; no
/// other code needs the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenAnswer {
    token: Token,
    seconds: NonZeroU64,
}

impl TokenAnswer {
    /// A token answer of this token for this number of seconds.
    pub fn new(token: Token, seconds: NonZeroU64) -> Self {
        Self { token, seconds }
    }

    /// The token.
    pub fn token(&self) -> &Token {
        &self.token
    }

    /// The token, for the transport that keeps it.
    pub fn into_token(self) -> Token {
        self.token
    }

    /// How long the token counts, from the answer of the hub.
    pub fn lifetime(&self) -> Duration {
        Duration::from_secs(self.seconds.get())
    }

    /// Writes the message. The bytes hold the token: they are for the carrier and for nothing
    /// that keeps or shows them.
    pub fn encode(&self) -> Vec<u8> {
        format!(
            "{}token {}\nlifetime {}\n",
            first_line(VERSION),
            self.token.0,
            self.seconds
        )
        .into_bytes()
    }

    /// Reads the message, as [`encode`](Self::encode) writes it, and nothing else.
    ///
    /// # Errors
    ///
    /// Returns an error for input that is not UTF-8, a first line that is missing or malformed,
    /// another protocol version, a line that is missing, repeated, unknown, or out of order, a
    /// last line without a line feed, a token that [`Token::try_new`] refuses, and a lifetime
    /// that is not a decimal integer from 1 without a sign or a leading zero that fits 64 bits.
    /// The first problem is reported, with its line; no error holds a part of the token.
    pub fn parse(bytes: &[u8]) -> Result<Self, ParseError> {
        let mut lines = Lines::open(bytes)?;
        let (line, value) = lines.require("token")?;
        let token = Token::try_new(value).map_err(|reason| problem(line, reason))?;
        let (line, value) = lines.require("lifetime")?;
        let seconds =
            parse_positive::<NonZeroU64>(value).ok_or(malformed(line, Malformed::Number))?;
        lines.end()?;
        Ok(Self { token, seconds })
    }
}

#[cfg(test)]
pub(crate) mod tests;
