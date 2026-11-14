//! Signatures of proofs: SSHSIG with Ed25519 (ADR-003, ADR-020).
//!
//! A proof signs a log entry or an approval. The signed bytes are not the payload alone: a header
//! with the type and the format version of the payload comes first, so a
//! signature of one type or version never verifies as another (domain separation). The SSHSIG
//! namespace is [`NAMESPACE`] for all of them.
//!
//! Signing goes through the trait [`Signer`]. [`SshKeygen`] implements it with
//! `ssh-keygen -Y sign`, so Stemma never reads a private key. Verification runs in process with
//! the crate `ssh-key` ([`verify`]) and needs no file and no child process. Only Ed25519 keys and
//! the hash algorithm SHA-512 are accepted.
//!
//! Whether a key may sign for an actor is not decided here: that is the task of the actor
//! register, which hands a [`PublicKey`] to [`verify`].

use std::error::Error;
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;

use ssh_key::{Algorithm, HashAlg, LineEnding, SshSig};

/// The SSHSIG namespace of every proof of Stemma. The type and the version of the payload are
/// part of the signed bytes, not of the namespace.
pub const NAMESPACE: &str = "stemma-proof";

/// The version of the format of the signed bytes, and of the payloads that [`sign`] signs.
pub const FORMAT_VERSION: u32 = 1;

/// The first line of the signed bytes. It has the value of the namespace on purpose, but it is a
/// constant of its own: the header is part of the format, the namespace is part of SSHSIG.
const MAGIC: &str = "stemma-proof";

/// What a proof signs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PayloadKind {
    /// An entry of the operation log whose rule depends on who acts, such as an owner override
    /// (ADR-004).
    LogEntry,
    /// An approval of a commit, bound to the head that was reviewed (ADR-004).
    Approval,
}

impl PayloadKind {
    /// The name of the type, as it appears in the signed bytes. It is part of the format and
    /// never changes for a version.
    pub const fn name(self) -> &'static str {
        match self {
            Self::LogEntry => "log-entry",
            Self::Approval => "approval",
        }
    }
}

/// The bytes that a proof signs: the header, then the payload.
///
/// ```text
/// stemma-proof\n<type>\n<version>\n<payload>
/// ```
///
/// The magic, the name of the type, and the decimal version contain no newline, so the payload
/// begins after the third newline and the split is unambiguous.
///
/// [`sign`] is the way to sign a payload; a [`Signer`] that is given bytes without this header
/// would break the domain separation.
pub(crate) fn signed_bytes(kind: PayloadKind, version: u32, payload: &[u8]) -> Vec<u8> {
    let mut bytes = format!("{MAGIC}\n{}\n{version}\n", kind.name()).into_bytes();
    bytes.extend_from_slice(payload);
    bytes
}

/// An Ed25519 public key in the OpenSSH format, the form of a `.pub` file or of a line of
/// `authorized_keys` without options. The comment of the line is not kept.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PublicKey(ssh_key::PublicKey);

impl PublicKey {
    /// Reads a public key from its OpenSSH text, such as `ssh-ed25519 AAAA... comment`. A key of
    /// another type is refused.
    ///
    /// # Errors
    ///
    /// [`KeyError::Malformed`] if the text is no OpenSSH public key, and
    /// [`KeyError::NotEd25519`] if it is one of another type.
    pub fn from_openssh(text: &str) -> Result<Self, KeyError> {
        let mut key =
            ssh_key::PublicKey::from_openssh(text.trim()).map_err(|e| KeyError::Malformed {
                reason: e.to_string(),
            })?;
        match key.algorithm() {
            Algorithm::Ed25519 => {
                // The comment is no part of the key: two lines with the same key are equal.
                key.set_comment("");
                Ok(Self(key))
            }
            other => Err(KeyError::NotEd25519 {
                algorithm: other.to_string(),
            }),
        }
    }

    /// The key as a line of OpenSSH text, without a comment. A comment of the text that the key
    /// was read from is dropped.
    pub fn to_openssh(&self) -> String {
        // The encoding of an Ed25519 key cannot fail.
        self.0
            .to_openssh()
            .expect("an Ed25519 public key can be encoded")
    }
}

impl fmt::Display for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_openssh())
    }
}

impl FromStr for PublicKey {
    type Err = KeyError;

    fn from_str(text: &str) -> Result<Self, KeyError> {
        Self::from_openssh(text)
    }
}

/// A signature in the armored SSHSIG form (`-----BEGIN SSH SIGNATURE-----`), as
/// `ssh-keygen -Y sign` writes it. [`Signature::from_armored`], the only public constructor, accepts
/// Ed25519 keys, SHA-512, and the namespace [`NAMESPACE`] only.
#[derive(Debug, Clone, PartialEq, Eq)]
///
/// A signature is not unique: another byte string can verify for the same key and message, so a
/// signature must not serve as an identity, for example to find duplicates. Compare the signed
/// payload and the key instead.
pub struct Signature(SshSig);

impl Signature {
    /// Reads an armored SSHSIG signature.
    ///
    /// # Errors
    ///
    /// [`SignatureError::Malformed`] if the text is no SSHSIG signature,
    /// [`SignatureError::Namespace`] if it was made for another namespace than [`NAMESPACE`],
    /// [`SignatureError::NotEd25519`] if the key is no Ed25519 key, and
    /// [`SignatureError::HashAlgorithm`] if the hash is not SHA-512.
    pub fn from_armored(text: &str) -> Result<Self, SignatureError> {
        let sig = SshSig::from_pem(text.trim()).map_err(|e| SignatureError::Malformed {
            reason: e.to_string(),
        })?;
        if sig.namespace() != NAMESPACE {
            return Err(SignatureError::Namespace {
                found: sig.namespace().to_owned(),
            });
        }
        if sig.public_key().algorithm() != Algorithm::Ed25519 {
            return Err(SignatureError::NotEd25519 {
                found: sig.public_key().algorithm().to_string(),
            });
        }
        if sig.hash_alg() != HashAlg::Sha512 {
            return Err(SignatureError::HashAlgorithm {
                found: sig.hash_alg().to_string(),
            });
        }
        Ok(Self(sig))
    }

    /// The signature in the armored form, with a line feed after each line.
    pub fn to_armored(&self) -> String {
        // The value was read from text or made by a signer, so it can be encoded again.
        self.0
            .to_pem(LineEnding::LF)
            .expect("an SSHSIG signature can be encoded")
    }

    /// The public key that made the signature, as the signature names it. It is a claim until the
    /// actor register binds it to an identity: [`verify`] takes the key from its caller, and a
    /// signature that names another key does not verify.
    pub fn signing_key(&self) -> PublicKey {
        PublicKey(ssh_key::PublicKey::from(self.0.public_key().clone()))
    }
}

/// Something that signs bytes with a private key that Stemma does not hold.
pub trait Signer {
    /// Signs `message` in the SSHSIG form, for the namespace [`NAMESPACE`] and with SHA-512.
    ///
    /// # Errors
    ///
    /// [`SignError`] if the key is not available, the process fails, or the answer is not a
    /// signature that [`Signature::from_armored`] accepts.
    fn sign(&self, message: &[u8]) -> Result<Signature, SignError>;
}

/// Signs a payload with a [`Signer`]: the header of the signed bytes (type and [`FORMAT_VERSION`])
/// comes first.
///
/// # Errors
///
/// The error of the signer.
pub fn sign(
    signer: &dyn Signer,
    kind: PayloadKind,
    payload: &[u8],
) -> Result<Signature, SignError> {
    signer.sign(&signed_bytes(kind, FORMAT_VERSION, payload))
}

/// Checks that `signature` is a signature of `key` over the payload of the given type and
/// version.
///
/// The type and the version are what the caller expects, usually read from the record that holds
/// the payload. A signature made for another type or version, another payload, another key, or
/// another namespace does not verify. The key that the signature names is no source of trust: it
/// must be `key`, and a signature that names another key is refused.
///
/// # Errors
///
/// [`VerifyError::UnsupportedVersion`] if `version` is not [`FORMAT_VERSION`],
/// [`VerifyError::WrongKey`] if the signature names another key than `key`, and
/// [`VerifyError::Invalid`] if the signature does not match the type, the version, and the
/// payload.
pub fn verify(
    key: &PublicKey,
    kind: PayloadKind,
    version: u32,
    payload: &[u8],
    signature: &Signature,
) -> Result<(), VerifyError> {
    if version != FORMAT_VERSION {
        return Err(VerifyError::UnsupportedVersion { version });
    }
    if signature.0.public_key() != key.0.key_data() {
        return Err(VerifyError::WrongKey);
    }
    key.0
        .verify(
            NAMESPACE,
            &signed_bytes(kind, version, payload),
            &signature.0,
        )
        .map_err(|_| VerifyError::Invalid)
}

/// The implementation of [`Signer`] that calls `ssh-keygen -Y sign`.
///
/// The key file can be the private key or, when `ssh-agent` holds the key (found through
/// `SSH_AUTH_SOCK`), the public key. A private key with a passphrase needs a terminal, where
/// `ssh-keygen` asks for it, and blocks the caller until it is answered. Without a terminal
/// the signing fails with [`SignError::Failed`]; a key in `ssh-agent` avoids the question.
#[derive(Debug, Clone)]
pub struct SshKeygen {
    program: PathBuf,
    key_file: PathBuf,
}

impl SshKeygen {
    /// A signer that runs `ssh-keygen` from the search path with the key in `key_file`.
    pub fn new(key_file: impl Into<PathBuf>) -> Self {
        Self {
            program: PathBuf::from("ssh-keygen"),
            key_file: key_file.into(),
        }
    }

    /// The same, with another program, such as an `ssh-keygen` at a known path.
    pub fn with_program(program: impl Into<PathBuf>, key_file: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            key_file: key_file.into(),
        }
    }

    /// The file of the key.
    pub fn key_file(&self) -> &Path {
        &self.key_file
    }
}

impl Signer for SshKeygen {
    fn sign(&self, message: &[u8]) -> Result<Signature, SignError> {
        let mut child = Command::new(&self.program)
            .args(["-Y", "sign", "-n", NAMESPACE, "-f"])
            .arg(&self.key_file)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| SignError::Spawn {
                program: self.program.clone(),
                source,
            })?;
        let mut stdin = child
            .stdin
            .take()
            .expect("the standard input was set to piped above");
        // This relies on `ssh-keygen -Y sign` reading all of its standard input before it writes,
        // so that a write here cannot block on a full output pipe. If it ends early, for example
        // because the key file is missing, the write fails with a broken pipe; the exit status
        // and the standard error below say why, and they are reported first.
        let written = stdin.write_all(message);
        drop(stdin);
        let output = child.wait_with_output().map_err(|source| SignError::Io {
            program: self.program.clone(),
            operation: "wait for",
            source,
        })?;
        if !output.status.success() {
            return Err(SignError::Failed {
                status: output.status.to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        written.map_err(|source| SignError::Io {
            program: self.program.clone(),
            operation: "write the message to",
            source,
        })?;
        let text = String::from_utf8(output.stdout).map_err(|e| SignError::Unreadable {
            reason: format!(
                "the output of {} is no text: {}",
                self.program.display(),
                e.utf8_error()
            ),
        })?;
        Signature::from_armored(&text).map_err(|e| SignError::Unreadable {
            reason: e.to_string(),
        })
    }
}

/// The error of reading a [`PublicKey`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeyError {
    /// The text is no OpenSSH public key.
    Malformed {
        /// What is wrong with it.
        reason: String,
    },
    /// The key is of another type than Ed25519.
    NotEd25519 {
        /// The name of the type, such as `ssh-rsa`.
        algorithm: String,
    },
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed { reason } => write!(f, "not an OpenSSH public key: {reason}"),
            Self::NotEd25519 { algorithm } => {
                write!(f, "only Ed25519 keys are accepted, this one is {algorithm}")
            }
        }
    }
}

impl Error for KeyError {}

/// The error of reading a [`Signature`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SignatureError {
    /// The text is no SSHSIG signature.
    Malformed {
        /// What is wrong with it.
        reason: String,
    },
    /// The signature was made for another namespace.
    Namespace {
        /// The namespace that it names.
        found: String,
    },
    /// The key of the signature is not an Ed25519 key.
    NotEd25519 {
        /// The name of the type.
        found: String,
    },
    /// The hash algorithm of the signature is not SHA-512.
    HashAlgorithm {
        /// The name of the algorithm.
        found: String,
    },
}

impl fmt::Display for SignatureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed { reason } => write!(f, "not an SSHSIG signature: {reason}"),
            Self::Namespace { found } => {
                write!(
                    f,
                    "signature of the namespace {found:?}, expected {NAMESPACE:?}"
                )
            }
            Self::NotEd25519 { found } => {
                write!(
                    f,
                    "only Ed25519 signatures are accepted, this one is {found}"
                )
            }
            Self::HashAlgorithm { found } => {
                write!(
                    f,
                    "only SHA-512 signatures are accepted, this one is {found}"
                )
            }
        }
    }
}

impl Error for SignatureError {}

/// The error of signing.
#[derive(Debug)]
#[non_exhaustive]
pub enum SignError {
    /// The program could not be started.
    Spawn {
        /// The program.
        program: PathBuf,
        /// The error of the operating system.
        source: io::Error,
    },
    /// The program ran, but writing its standard input or waiting for it failed.
    Io {
        /// The program.
        program: PathBuf,
        /// What was done, as it completes "cannot ... the program": "write the message to" or
        /// "wait for".
        operation: &'static str,
        /// The error of the operating system.
        source: io::Error,
    },
    /// The program ended with a failure, for example because the key file is missing.
    Failed {
        /// The exit status.
        status: String,
        /// What the program wrote to its standard error.
        stderr: String,
    },
    /// The program answered with something that [`Signature::from_armored`] does not accept.
    Unreadable {
        /// Why.
        reason: String,
    },
}

impl SignError {
    /// The error of the operating system, if there is one.
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Spawn { source, .. } | Self::Io { source, .. } => Some(source),
            Self::Failed { .. } | Self::Unreadable { .. } => None,
        }
    }
}

impl fmt::Display for SignError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn { program, source } => {
                write!(f, "cannot run {}: {source}", program.display())
            }
            Self::Io {
                program,
                operation,
                source,
            } => write!(f, "cannot {operation} {}: {source}", program.display()),
            Self::Failed { status, stderr } if stderr.is_empty() => {
                write!(f, "signing failed: {status}")
            }
            Self::Failed { status, stderr } => write!(f, "signing failed: {status}: {stderr}"),
            Self::Unreadable { reason } => {
                write!(f, "the signer did not return a usable signature: {reason}")
            }
        }
    }
}

// The messages of `Spawn` and `Io` print their cause, so the source is the cause of the cause, which an
// `io::Error` of the operating system does not have.
impl Error for SignError {}

/// The error of [`verify`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum VerifyError {
    /// The version of the payload is not one that this program knows.
    UnsupportedVersion {
        /// The version.
        version: u32,
    },
    /// The signature names another key than the one that was asked for.
    WrongKey,
    /// The signature does not match the key, the type, the version, and the payload.
    Invalid,
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion { version } => write!(
                f,
                "payload format version {version} is not supported (expected {FORMAT_VERSION})"
            ),
            Self::WrongKey => f.write_str("the signature was made by another key"),
            Self::Invalid => {
                f.write_str("the signature does not match the key, the type, and the payload")
            }
        }
    }
}

impl Error for VerifyError {}

#[cfg(test)]
mod tests {
    use super::*;
    use ssh_key::private::Ed25519Keypair;
    use ssh_key::PrivateKey;

    /// A signature of an RSA key for the namespace `stemma-proof`, made with SHA-512 by
    /// `ssh-keygen`, for a key that was generated for this test only.
    const RSA_SIGNATURE: &str = "-----BEGIN SSH SIGNATURE-----\n\
U1NIU0lHAAAAAQAAAJcAAAAHc3NoLXJzYQAAAAMBAAEAAACBAJcF3Tk/HnATjScgg+VrGn\n\
w4UW1BRTELw5I71dzRAzgeYI6fQtaGEG7a8yD8YkqyircrjE2ufufnQiaxWH7Gx3bhHGUL\n\
sZKqU3mEo0/Xkm/0z7EEI6qNUXtnYuH0tITYEBblOZflbn/ebntZqWNlceoVRxE6QEhg1c\n\
YmgN3nnflLAAAADHN0ZW1tYS1wcm9vZgAAAAAAAAAGc2hhNTEyAAAAlAAAAAxyc2Etc2hh\n\
Mi01MTIAAACALpUDjEPyba1iSP6Ik1lliwqvUfWrvzic3MZPddMWyPFpkB9MreKoKdrX3L\n\
hNl5Y6D/vYCCljXYO2T97eDo07UyYzv4MdMiECaV7t1NN7T2/2q1fFDB8V5YgdXOYSQx2s\n\
AVIRoZlxo88ExLG0/iVqNKMORmBJ7dsKhnfS9dIz37M=\n\
-----END SSH SIGNATURE-----";

    /// A `Signer` that signs in process with a key from a fixed seed, so tests need no key file
    /// and no `ssh-keygen`. It also records the messages it was asked to sign.
    struct FakeSigner {
        key: PrivateKey,
        seen: std::cell::RefCell<Vec<Vec<u8>>>,
    }

    impl FakeSigner {
        fn new(seed: u8) -> Self {
            let pair = Ed25519Keypair::from_seed(&[seed; 32]);
            Self {
                key: PrivateKey::from(pair),
                seen: Default::default(),
            }
        }

        fn public_key(&self) -> PublicKey {
            PublicKey::from_openssh(&self.key.public_key().to_openssh().unwrap()).unwrap()
        }
    }

    impl Signer for FakeSigner {
        fn sign(&self, message: &[u8]) -> Result<Signature, SignError> {
            self.seen.borrow_mut().push(message.to_vec());
            let sig = self
                .key
                .sign(NAMESPACE, HashAlg::Sha512, message)
                .expect("the fake key signs");
            Ok(Signature(sig))
        }
    }

    /// A signer that fails, for the error path of [`sign`].
    struct Broken;

    impl Signer for Broken {
        fn sign(&self, _message: &[u8]) -> Result<Signature, SignError> {
            Err(SignError::Unreadable {
                reason: "broken".into(),
            })
        }
    }

    #[test]
    fn a_signature_verifies_for_its_key_type_version_and_payload() {
        let signer = FakeSigner::new(1);
        let sig = sign(&signer, PayloadKind::Approval, b"commit abc").unwrap();
        assert_eq!(
            verify(
                &signer.public_key(),
                PayloadKind::Approval,
                FORMAT_VERSION,
                b"commit abc",
                &sig
            ),
            Ok(())
        );
    }

    #[test]
    fn the_signer_gets_the_header_and_the_payload() {
        let signer = FakeSigner::new(1);
        sign(&signer, PayloadKind::LogEntry, b"entry").unwrap();
        assert_eq!(
            signer.seen.borrow()[0],
            b"stemma-proof\nlog-entry\n1\nentry".to_vec()
        );
    }

    #[test]
    fn a_wrong_type_fails() {
        let signer = FakeSigner::new(1);
        let sig = sign(&signer, PayloadKind::Approval, b"p").unwrap();
        assert_eq!(
            verify(
                &signer.public_key(),
                PayloadKind::LogEntry,
                FORMAT_VERSION,
                b"p",
                &sig
            ),
            Err(VerifyError::Invalid)
        );
    }

    #[test]
    fn a_wrong_version_fails() {
        let signer = FakeSigner::new(1);
        // A signature over the bytes of a version 2, as a later program would make it.
        let sig = signer
            .sign(&signed_bytes(PayloadKind::Approval, 2, b"p"))
            .unwrap();
        // This program refuses the version before it looks at the signature ...
        assert_eq!(
            verify(&signer.public_key(), PayloadKind::Approval, 2, b"p", &sig),
            Err(VerifyError::UnsupportedVersion { version: 2 })
        );
        // ... and a signature of version 2 does not pass as version 1.
        assert_eq!(
            verify(
                &signer.public_key(),
                PayloadKind::Approval,
                FORMAT_VERSION,
                b"p",
                &sig
            ),
            Err(VerifyError::Invalid)
        );
    }

    #[test]
    fn a_wrong_payload_fails() {
        let signer = FakeSigner::new(1);
        let sig = sign(&signer, PayloadKind::Approval, b"p").unwrap();
        assert_eq!(
            verify(
                &signer.public_key(),
                PayloadKind::Approval,
                FORMAT_VERSION,
                b"q",
                &sig
            ),
            Err(VerifyError::Invalid)
        );
    }

    #[test]
    fn a_payload_cannot_pose_as_a_header() {
        // A payload that continues the header of a log entry so that the bytes read like an
        // approval with the payload `x`: the bytes of the header differ already.
        let signer = FakeSigner::new(1);
        let forged = b"approval\n1\nx";
        let sig = sign(&signer, PayloadKind::LogEntry, forged).unwrap();
        assert_eq!(
            verify(
                &signer.public_key(),
                PayloadKind::Approval,
                FORMAT_VERSION,
                b"x",
                &sig
            ),
            Err(VerifyError::Invalid)
        );
        // The digits of a version cannot move into the payload either.
        assert_ne!(
            signed_bytes(PayloadKind::Approval, 1, b"0\nx"),
            signed_bytes(PayloadKind::Approval, 10, b"x")
        );
    }

    #[test]
    fn empty_binary_and_multiline_payloads_round_trip() {
        let signer = FakeSigner::new(1);
        for payload in [&b""[..], b"line 1\nline 2\n", &[0, 255, 10, 13, 0]] {
            let sig = sign(&signer, PayloadKind::LogEntry, payload).unwrap();
            let armored = Signature::from_armored(&sig.to_armored()).unwrap();
            assert_eq!(
                verify(
                    &signer.public_key(),
                    PayloadKind::LogEntry,
                    FORMAT_VERSION,
                    payload,
                    &armored
                ),
                Ok(())
            );
        }
    }

    #[test]
    fn a_signature_that_names_another_key_is_refused_as_wrong_key() {
        let signer = FakeSigner::new(1);
        let other = FakeSigner::new(2);
        let sig = sign(&signer, PayloadKind::Approval, b"p").unwrap();
        assert_eq!(
            verify(
                &other.public_key(),
                PayloadKind::Approval,
                FORMAT_VERSION,
                b"p",
                &sig
            ),
            Err(VerifyError::WrongKey)
        );
    }

    #[test]
    fn a_tampered_signature_never_verifies() {
        let signer = FakeSigner::new(1);
        let sig = sign(&signer, PayloadKind::Approval, b"p").unwrap();
        let armored = sig.to_armored();
        let lines: Vec<&str> = armored.lines().collect();
        // Flip one character in each line of the body, one at a time.
        for index in 1..lines.len() - 1 {
            let mut changed: Vec<String> = lines.iter().map(|l| (*l).to_owned()).collect();
            let flipped = if changed[index].starts_with('A') {
                'B'
            } else {
                'A'
            };
            changed[index].replace_range(0..1, &flipped.to_string());
            let text = changed.join("\n");
            if let Ok(parsed) = Signature::from_armored(&text) {
                assert_ne!(
                    verify(
                        &signer.public_key(),
                        PayloadKind::Approval,
                        FORMAT_VERSION,
                        b"p",
                        &parsed
                    ),
                    Ok(()),
                    "line {index} changed"
                );
            }
        }
    }

    #[test]
    fn a_truncated_or_broken_armor_is_malformed() {
        let signer = FakeSigner::new(1);
        let armored = sign(&signer, PayloadKind::Approval, b"p")
            .unwrap()
            .to_armored();
        let lines: Vec<&str> = armored.lines().collect();
        let cut_body = format!("{}\n{}\n{}", lines[0], lines[1], lines[lines.len() - 1]);
        let no_end = lines[..lines.len() - 1].join("\n");
        let no_begin = lines[1..].join("\n");
        for text in [cut_body.as_str(), no_end.as_str(), no_begin.as_str(), ""] {
            assert!(
                matches!(
                    Signature::from_armored(text),
                    Err(SignatureError::Malformed { .. })
                ),
                "{text:?}"
            );
        }
    }

    #[test]
    fn crlf_line_endings_and_surrounding_whitespace_are_accepted() {
        let signer = FakeSigner::new(1);
        let sig = sign(&signer, PayloadKind::Approval, b"p").unwrap();
        let text = format!("\n  {}  \n", sig.to_armored().replace('\n', "\r\n"));
        assert_eq!(Signature::from_armored(&text).unwrap(), sig);
    }

    #[test]
    fn the_armored_form_round_trips() {
        let signer = FakeSigner::new(3);
        let sig = sign(&signer, PayloadKind::Approval, b"p").unwrap();
        let armored = sig.to_armored();
        assert!(armored.starts_with("-----BEGIN SSH SIGNATURE-----\n"));
        let back = Signature::from_armored(&armored).unwrap();
        assert_eq!(back, sig);
        assert_eq!(back.signing_key(), signer.public_key());
        assert_eq!(
            verify(
                &signer.public_key(),
                PayloadKind::Approval,
                FORMAT_VERSION,
                b"p",
                &back
            ),
            Ok(())
        );
    }

    #[test]
    fn a_signature_of_another_namespace_is_refused_when_read() {
        let signer = FakeSigner::new(1);
        let sig = signer.key.sign("file", HashAlg::Sha512, b"p").unwrap();
        let armored = sig.to_pem(LineEnding::LF).unwrap();
        assert_eq!(
            Signature::from_armored(&armored),
            Err(SignatureError::Namespace {
                found: "file".into()
            })
        );
    }

    #[test]
    fn a_signature_with_sha256_is_refused_when_read() {
        let signer = FakeSigner::new(1);
        let sig = signer.key.sign(NAMESPACE, HashAlg::Sha256, b"p").unwrap();
        let armored = sig.to_pem(LineEnding::LF).unwrap();
        assert!(matches!(
            Signature::from_armored(&armored),
            Err(SignatureError::HashAlgorithm { .. })
        ));
    }

    #[test]
    fn garbage_is_no_signature() {
        assert!(matches!(
            Signature::from_armored("not a signature"),
            Err(SignatureError::Malformed { .. })
        ));
    }

    #[test]
    fn a_public_key_round_trips_and_only_ed25519_is_accepted() {
        let signer = FakeSigner::new(4);
        let key = signer.public_key();
        assert_eq!(PublicKey::from_openssh(&key.to_openssh()).unwrap(), key);
        assert_eq!(
            PublicKey::from_openssh(&format!("{}\n", key.to_openssh())).unwrap(),
            key
        );
        // The comment of a line is no part of the key.
        assert_eq!(
            PublicKey::from_openssh(&format!("{} agent@example", key.to_openssh())).unwrap(),
            key
        );
        assert!(matches!(
            PublicKey::from_openssh("nonsense"),
            Err(KeyError::Malformed { .. })
        ));
    }

    #[test]
    fn a_key_of_another_type_is_refused() {
        // Public keys of other types, generated for this test only.
        let rsa = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAAAgQCXBd05Px5wE40nIIPlaxp8OFFtQUUxC8OSO9Xc0QM4HmCOn0LWhhBu2vMg/GJKsoq3K4xNrn7n50ImsVh+xsd24RxlC7GSqlN5hKNP15Jv9M+xBCOqjVF7Z2Lh9LSE2BAW5TmX5W5/3m57WaljZXHqFUcROkBIYNXGJoDd5535Sw==";
        let ecdsa = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBOAXOcAhbzonDxZR8B4VHRhL3Nv4E20e4drPrBu7hru1IO/lXzYPQVL19ePLMu1Wnmqqy8ohyDl+q8dxuTQBLGk=";
        let error = PublicKey::from_openssh(rsa).unwrap_err();
        assert_eq!(
            error,
            KeyError::NotEd25519 {
                algorithm: "ssh-rsa".into()
            }
        );
        assert_eq!(
            error.to_string(),
            "only Ed25519 keys are accepted, this one is ssh-rsa"
        );
        // A type that the crate is not built to read is refused as well, as a malformed key.
        assert!(PublicKey::from_openssh(ecdsa).is_err());
    }

    #[test]
    fn a_signature_of_a_key_of_another_type_is_refused_when_read() {
        // A signature of an RSA key for the namespace `stemma-proof`, made with SHA-512.
        let armored = RSA_SIGNATURE;
        let error = Signature::from_armored(armored).unwrap_err();
        assert!(
            matches!(error, SignatureError::NotEd25519 { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn the_error_of_the_signer_comes_back() {
        assert!(matches!(
            sign(&Broken, PayloadKind::Approval, b"p"),
            Err(SignError::Unreadable { .. })
        ));
    }

    #[test]
    fn the_messages_are_complete() {
        assert_eq!(
            VerifyError::UnsupportedVersion { version: 9 }.to_string(),
            "payload format version 9 is not supported (expected 1)"
        );
        let missing = SshKeygen::with_program("/nonexistent/ssh-keygen", "key")
            .sign(b"m")
            .unwrap_err();
        assert!(missing.to_string().contains("/nonexistent/ssh-keygen"));
        assert!(missing.io_error().is_some());
    }

    /// `ssh-keygen` is on the path of every machine that uses Stemma with signatures, but a test
    /// must not fail on a machine without it.
    fn ssh_keygen_available() -> bool {
        let available = Command::new("ssh-keygen")
            .arg("-?")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok();
        if !available {
            // A skipped test must not look like a passed one.
            assert!(
                std::env::var_os("STEMMA_REQUIRE_SSH_KEYGEN").is_none(),
                "STEMMA_REQUIRE_SSH_KEYGEN is set, but `ssh-keygen` is not on the path"
            );
            eprintln!("SKIPPED: `ssh-keygen` is not on the path");
        }
        available
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("stemma-sshsig-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn ssh_keygen_signs_and_the_signature_verifies_in_process() {
        if !ssh_keygen_available() {
            return;
        }
        let dir = scratch_dir("roundtrip");
        let key_file = dir.join("key");
        let status = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", ""])
            .arg("-f")
            .arg(&key_file)
            .status()
            .unwrap();
        assert!(status.success());
        let public =
            PublicKey::from_openssh(&std::fs::read_to_string(dir.join("key.pub")).unwrap())
                .unwrap();

        let signer = SshKeygen::new(&key_file);
        let sig = sign(&signer, PayloadKind::Approval, b"commit abc\n").unwrap();
        assert_eq!(sig.signing_key(), public);
        assert_eq!(
            verify(
                &public,
                PayloadKind::Approval,
                FORMAT_VERSION,
                b"commit abc\n",
                &sig
            ),
            Ok(())
        );
        assert_eq!(
            verify(
                &public,
                PayloadKind::LogEntry,
                FORMAT_VERSION,
                b"commit abc\n",
                &sig
            ),
            Err(VerifyError::Invalid)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ssh_keygen_with_a_missing_key_fails_with_its_message() {
        if !ssh_keygen_available() {
            return;
        }
        let dir = scratch_dir("missing");
        let err = SshKeygen::new(dir.join("absent")).sign(b"m").unwrap_err();
        match &err {
            SignError::Failed { stderr, .. } => assert!(!stderr.is_empty(), "{err}"),
            other => panic!("{other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A program that stands in for `ssh-keygen`: a shell script with the given body.
    #[cfg(unix)]
    fn fake_program(name: &str, body: &str) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch_dir(name);
        let path = dir.join("fake-ssh-keygen");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        (dir, path)
    }

    #[cfg(unix)]
    fn sign_with_fake(name: &str, body: &str, message: &[u8]) -> SignError {
        let (dir, path) = fake_program(name, body);
        let error = SshKeygen::with_program(&path, "key")
            .sign(message)
            .unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);
        error
    }

    #[cfg(unix)]
    #[test]
    fn a_program_without_output_gives_no_signature() {
        let error = sign_with_fake("empty", "cat > /dev/null", b"m");
        assert!(matches!(error, SignError::Unreadable { .. }), "{error:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_program_with_output_that_is_no_text_gives_no_signature() {
        let error = sign_with_fake("binary", "cat > /dev/null; printf '\\377\\376'", b"m");
        assert!(error.to_string().contains("is no text"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_program_that_answers_with_a_signature_of_the_wrong_key_type_gives_none() {
        let body = format!("cat > /dev/null; cat <<'EOF'\n{RSA_SIGNATURE}\nEOF");
        let error = sign_with_fake("rsa", &body, b"m");
        assert!(matches!(error, SignError::Unreadable { .. }), "{error:?}");
        assert!(error.to_string().contains("Ed25519"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_program_that_fails_without_a_message_is_reported_with_its_status() {
        let error = sign_with_fake("silent-failure", "cat > /dev/null; exit 3", b"m");
        assert_eq!(error.to_string(), "signing failed: exit status: 3");
    }

    #[cfg(unix)]
    #[test]
    fn a_program_that_does_not_read_its_input_is_an_io_error() {
        // More than a pipe holds, so that the write has to fail when the program is gone.
        let message = vec![b'x'; 4 * 1024 * 1024];
        let error = sign_with_fake("no-read", "exit 0", &message);
        assert!(matches!(error, SignError::Io { .. }), "{error:?}");
        assert!(
            error.to_string().starts_with("cannot write the message to"),
            "{error}"
        );
        assert!(error.io_error().is_some());
    }
}
