//! Signatures of proofs: SSHSIG with Ed25519 (ADR-003, ADR-020).
//!
//! A proof signs a log entry or an approval. The signed bytes are not the payload alone: a header
//! with the type and the format version of the payload comes first ([`signed_bytes`]), so a
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

use ssh_key::{Algorithm, HashAlg, LineEnding, SshSig};

/// The SSHSIG namespace of every proof of Stemma. The type and the version of the payload are
/// part of the signed bytes, not of the namespace.
pub const NAMESPACE: &str = "stemma-proof";

/// The version of the format of the signed bytes, and of the payloads that [`sign`] signs.
pub const FORMAT_VERSION: u32 = 1;

/// The first line of the signed bytes.
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
/// The first three lines contain no newline themselves, so the payload begins after the third
/// one and the split is unambiguous.
pub fn signed_bytes(kind: PayloadKind, version: u32, payload: &[u8]) -> Vec<u8> {
    let mut bytes = format!("{MAGIC}\n{}\n{version}\n", kind.name()).into_bytes();
    bytes.extend_from_slice(payload);
    bytes
}

/// An Ed25519 public key in the OpenSSH format, the form of a line of `authorized_keys` and of
/// a `.pub` file.
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// A signature in the armored SSHSIG form (`-----BEGIN SSH SIGNATURE-----`), as
/// `ssh-keygen -Y sign` writes it. It is Ed25519 and SHA-512 by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature(SshSig);

impl Signature {
    /// Reads an armored SSHSIG signature.
    ///
    /// # Errors
    ///
    /// [`SignatureError::Malformed`] if the text is no SSHSIG signature,
    /// [`SignatureError::Namespace`] if it was made for another namespace than [`NAMESPACE`],
    /// [`SignatureError::Algorithm`] if the key is no Ed25519 key, and
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
            return Err(SignatureError::Algorithm {
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
        self.0
            .to_pem(LineEnding::LF)
            .expect("an SSHSIG signature can be encoded")
    }

    /// The public key that made the signature, as the signature names it. It is a claim until the
    /// actor register binds it to an identity: [`verify`] checks against a key that the caller
    /// chooses, and ignores this one.
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
    /// signature that [`verify`] accepts.
    fn sign(&self, message: &[u8]) -> Result<Signature, SignError>;
}

/// Signs a payload with a [`Signer`]: the header of [`signed_bytes`] with [`FORMAT_VERSION`]
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
/// another namespace does not verify. The key that the signature names is not consulted.
///
/// # Errors
///
/// [`VerifyError::UnsupportedVersion`] if `version` is not [`FORMAT_VERSION`], and
/// [`VerifyError::Invalid`] if the signature does not match.
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
/// The key file can be the private key or, when `ssh-agent` holds the key, the public key.
/// `ssh-keygen` asks for a passphrase on its own terminal when it needs one.
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
        let mut stdin = child.stdin.take().expect("the standard input is piped");
        // `ssh-keygen` reads the whole message before it writes, so a write here cannot block on a
        // full output pipe. A failed write shows in the exit status below.
        let written = stdin.write_all(message);
        drop(stdin);
        let output = child
            .wait_with_output()
            .map_err(|source| SignError::Spawn {
                program: self.program.clone(),
                source,
            })?;
        if !output.status.success() {
            return Err(SignError::Failed {
                status: output.status.to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        written.map_err(|source| SignError::Spawn {
            program: self.program.clone(),
            source,
        })?;
        let text = String::from_utf8(output.stdout).map_err(|_| SignError::Unreadable {
            reason: "the output is no text".to_owned(),
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
    Algorithm {
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
            Self::Algorithm { found } => {
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
    /// The program could not be started, or its standard input could not be written.
    Spawn {
        /// The program.
        program: PathBuf,
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
    /// The program answered with something that [`verify`] would not accept.
    Unreadable {
        /// Why.
        reason: String,
    },
}

impl SignError {
    /// The error of the operating system, if there is one.
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Spawn { source, .. } => Some(source),
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

// The message of `Spawn` prints its cause, so the source is the cause of the cause, which an
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
    fn a_wrong_payload_or_key_fails() {
        let signer = FakeSigner::new(1);
        let other = FakeSigner::new(2);
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
        assert_eq!(
            verify(
                &other.public_key(),
                PayloadKind::Approval,
                FORMAT_VERSION,
                b"p",
                &sig
            ),
            Err(VerifyError::Invalid)
        );
    }

    #[test]
    fn a_payload_cannot_pose_as_a_header() {
        // The payload of a log entry that looks like the rest of an approval header.
        let a = signed_bytes(PayloadKind::LogEntry, 1, b"approval\n1\nx");
        let b = signed_bytes(PayloadKind::Approval, 1, b"x");
        assert_ne!(a, b);
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
        if !ssh_keygen_available() {
            return;
        }
        let dir = scratch_dir("rsa");
        let key_file = dir.join("key");
        let status = Command::new("ssh-keygen")
            .args(["-q", "-t", "rsa", "-b", "2048", "-N", ""])
            .arg("-f")
            .arg(&key_file)
            .status()
            .unwrap();
        assert!(status.success());
        let text = std::fs::read_to_string(dir.join("key.pub")).unwrap();
        assert!(PublicKey::from_openssh(&text).is_err());
        let _ = std::fs::remove_dir_all(&dir);
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
        Command::new("ssh-keygen")
            .arg("-?")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
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
        assert!(matches!(err, SignError::Failed { .. }), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
