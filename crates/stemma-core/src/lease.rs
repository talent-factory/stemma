//! The lease of a branch (ADR-004, STEMMA-225).
//!
//! The lease is state, not an event: one file per branch in the directory `leases/` of the shared
//! metadata directory, so every worktree sees it. The file names the holder, the time of the
//! acquisition, and the expiry, and it is replaced as a whole by a rename. The log records the
//! changes of a lease in the entries `lease-acquire`, `lease-release`, `lease-takeover`, and
//! `lease-override` ([`LeaseEntry`]); a renewal writes no entry.
//!
//! This module holds the file and the rule that decides what an actor has to do to write a
//! branch: a pure function of the file, the actor, and the time. The operations that take the
//! lock of the references and write the entry are methods of
//! [`Repository`](crate::repository::Repository).
//!
//! # The file
//!
//! ```text
//! stemma-lease 1
//! branch feature/login
//! holder agent claude model=opus-5-5 session=s1 sponsor=daniel
//! acquired 1763280000
//! expires 1763283600
//! ```
//!
//! The reader is as strict as the one of the log: one encoding for one lease. The file is UTF-8,
//! every line ends with a single `\n`, the lines come in this order, and the file is at most
//! [`MAX_BYTES`] long.
//!
//! # The rule
//!
//! After the expiry the lease counts as free, so no clock stops a writer for good. The lease is
//! valid while the time is before `expires`; at `expires` it has ended.
//!
//! | The file | The actor | What the actor does |
//! |---|---|---|
//! | none | any | acquires (entry `lease-acquire`) |
//! | valid | the holder | renews (the file only, no entry) |
//! | valid | another | is refused, with holder and expiry |
//! | expired | the earlier holder | acquires again (entry `lease-acquire`) |
//! | expired | another | takes it over (entry `lease-takeover`, reason `expired`) |
//!
//! A lease exists only where the repository has a register with more than one identity and the
//! branch is not protected ([`applies`]); in open mode nothing writes a file.

use std::error::Error;
use std::fmt;
use std::str::FromStr;

use crate::oplog::LeaseEntry;
use crate::refs::BranchName;
use crate::register::Register;
use crate::Actor;

/// The directory in the metadata directory that holds the files of the leases.
pub const DIR: &str = "leases";

/// The first line of a lease file.
const HEADER: &str = "stemma-lease 1";

/// The longest lease file that is read. A branch name is at most a few hundred bytes, so a longer
/// file is not in canonical form anyway.
pub const MAX_BYTES: u64 = 4096;

/// The default duration of a lease, in minutes (ADR-004). The policy file may change it; the same
/// number lives in [`crate::policy`].
pub const DEFAULT_MINUTES: u32 = crate::policy::DEFAULT_LEASE_MINUTES;

/// The lease of one branch: who holds it, since when, and until when. Times are seconds since the
/// Unix epoch, as in the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    branch: BranchName,
    holder: Actor,
    acquired: i64,
    expires: i64,
}

impl Lease {
    /// A lease of `branch` that `holder` took at `acquired` and that ends at `expires`.
    ///
    /// # Errors
    ///
    /// Returns [`LeaseFileError::Span`] if `expires` is not after `acquired`.
    pub fn new(
        branch: BranchName,
        holder: Actor,
        acquired: i64,
        expires: i64,
    ) -> Result<Self, LeaseFileError> {
        if expires <= acquired {
            return Err(LeaseFileError::Span);
        }
        Ok(Self {
            branch,
            holder,
            acquired,
            expires,
        })
    }

    /// The lease that an entry of an acquisition, a takeover, or an override grants, or `None` for
    /// a release, which grants none.
    pub fn of_entry(entry: &LeaseEntry) -> Option<Self> {
        Some(Self {
            branch: entry.branch().clone(),
            holder: entry.actor().clone(),
            acquired: entry.time(),
            expires: entry.expires()?,
        })
    }

    /// The branch.
    pub fn branch(&self) -> &BranchName {
        &self.branch
    }

    /// The actor who holds the lease: the only one who may write the branch while it is valid.
    pub fn holder(&self) -> &Actor {
        &self.holder
    }

    /// The time of the acquisition, which a renewal does not change.
    pub fn acquired(&self) -> i64 {
        self.acquired
    }

    /// The end of the lease.
    pub fn expires(&self) -> i64 {
        self.expires
    }

    /// Whether the lease has ended at `now`.
    pub fn is_expired(&self, now: i64) -> bool {
        now >= self.expires
    }

    /// The lease after a renewal at `now` for `minutes`: the same holder and acquisition, and an
    /// expiry that is not earlier than before. Never shortens a lease.
    pub fn renewed(&self, now: i64, minutes: u32) -> Self {
        Self {
            expires: self.expires.max(expiry(now, minutes)),
            ..self.clone()
        }
    }

    /// The canonical encoding of the file.
    pub fn encode(&self) -> String {
        format!(
            "{HEADER}\nbranch {}\nholder {}\nacquired {}\nexpires {}\n",
            self.branch, self.holder, self.acquired, self.expires
        )
    }

    /// Reads a lease file, as [`encode`](Self::encode) writes it and nothing else.
    ///
    /// # Errors
    ///
    /// Returns [`LeaseFileError`] for input that is not UTF-8, lines that are not the five of the
    /// format in order, a branch name, an actor, or a time that is not in canonical form, and an
    /// expiry that is not after the acquisition.
    pub fn parse(bytes: &[u8]) -> Result<Self, LeaseFileError> {
        let text = std::str::from_utf8(bytes).map_err(|_| LeaseFileError::NotUtf8)?;
        let mut lines = text.split_inclusive('\n');
        let mut line = |key: &str| -> Result<&str, LeaseFileError> {
            lines
                .next()
                .and_then(|line| line.strip_suffix('\n'))
                .and_then(|line| line.strip_prefix(key))
                .ok_or(LeaseFileError::Shape)
        };
        if line("")? != HEADER {
            return Err(LeaseFileError::Shape);
        }
        let branch = BranchName::try_new(line("branch ")?).map_err(|_| LeaseFileError::Shape)?;
        let holder = Actor::from_str(line("holder ")?).map_err(|_| LeaseFileError::Shape)?;
        let acquired = parse_time(line("acquired ")?)?;
        let expires = parse_time(line("expires ")?)?;
        if lines.next().is_some() {
            return Err(LeaseFileError::Shape);
        }
        Self::new(branch, holder, acquired, expires)
    }
}

/// The time at which a lease that is taken at `now` for `minutes` ends. Saturates instead of
/// overflowing.
pub fn expiry(now: i64, minutes: u32) -> i64 {
    now.saturating_add(i64::from(minutes) * 60)
}

fn parse_time(text: &str) -> Result<i64, LeaseFileError> {
    crate::object::parse_seconds(text).map_err(|_| LeaseFileError::Shape)
}

/// The name of the file of a branch's lease in [`DIR`]: the branch name with every byte that is not
/// a lowercase ASCII letter, a digit, `-`, or `_` written as `%` and two hex digits. `%` is no
/// character of a branch name, so two branches never share a file, and no name starts with a dot
/// or reaches another directory. Capitals are written as escapes too, so that `Feature/x` and
/// `feature/x` keep two files on a file system that does not tell the case apart.
pub fn file_name(branch: &BranchName) -> String {
    let mut name = String::new();
    for byte in branch.as_str().bytes() {
        if byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_' {
            name.push(char::from(byte));
        } else {
            name.push_str(&format!("%{byte:02X}"));
        }
    }
    if name.len() > MAX_FILE_NAME {
        // A long name, mostly of `/` and `.` that grow to three bytes each, would exceed the 255
        // bytes of a path component. The start of the name and a hash of the whole name stay; the
        // branch in the file tells the branches apart if the hashes ever met (`read_lease` checks).
        let hash = blake3::hash(branch.as_str().as_bytes()).to_hex();
        name.truncate(MAX_FILE_NAME - 17);
        // The cut is at a byte that is ASCII, but may split an escape; it is only a prefix.
        name.push('~');
        name.push_str(&hash[..16]);
    }
    name
}

/// The longest file name of a lease, in bytes: well below the 255 of a path component.
const MAX_FILE_NAME: usize = 200;

/// Why a lease file is rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LeaseFileError {
    /// The file is not UTF-8.
    NotUtf8,
    /// The lines are not the five of the format, in order, in canonical form.
    Shape,
    /// The expiry is not after the acquisition.
    Span,
}

impl fmt::Display for LeaseFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotUtf8 => "the lease file is not UTF-8",
            Self::Shape => {
                "the lease file is not `stemma-lease 1`, `branch`, `holder`, `acquired`, and \
                 `expires` in canonical form"
            }
            Self::Span => "the lease ends at or before its acquisition",
        })
    }
}

impl Error for LeaseFileError {}

/// What an actor has to do to write a branch, as [`claim`] decides it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// The actor holds a valid lease: renew the file, write no entry.
    Renew,
    /// There is no lease, or the actor held the one that expired: acquire, with an entry.
    Acquire,
    /// Another actor held the lease and it expired: take it over, with an entry that names the
    /// earlier holder and the reason [`EXPIRED`](crate::oplog::EXPIRED).
    Takeover {
        /// The earlier holder.
        from: Actor,
    },
}

/// A valid lease of another actor stops the write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    /// The branch.
    pub branch: BranchName,
    /// The holder.
    pub holder: Actor,
    /// The end of the lease.
    pub expires: i64,
}

impl fmt::Display for Held {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the branch `{}` is leased to {} until {} (seconds since the Unix epoch); it can be \
             written after the expiry, or an owner can override the lease",
            self.branch, self.holder, self.expires
        )
    }
}

impl Error for Held {}

/// Decides what `actor` does to write the branch at `now`, given the lease file (`None` if there
/// is none). The rule is the table in the module documentation.
///
/// # Errors
///
/// Returns [`Held`] (boxed, as it is large) for a lease of another actor that is still valid.
pub fn claim(lease: Option<&Lease>, actor: &Actor, now: i64) -> Result<Claim, Box<Held>> {
    let Some(lease) = lease else {
        return Ok(Claim::Acquire);
    };
    match (lease.is_expired(now), lease.holder() == actor) {
        (false, true) => Ok(Claim::Renew),
        (false, false) => Err(Box::new(Held {
            branch: lease.branch().clone(),
            holder: lease.holder().clone(),
            expires: lease.expires(),
        })),
        (true, true) => Ok(Claim::Acquire),
        (true, false) => Ok(Claim::Takeover {
            from: lease.holder().clone(),
        }),
    }
}

/// Whether a branch has a lease: only in a repository whose register lists more than one identity
/// (`register` is `None` in open mode), and not on a protected branch, which changes only through
/// a pull request (ADR-004 rules 1 and 3).
pub fn applies(register: Option<&Register>, protected: bool) -> bool {
    !protected && register.is_some_and(|register| register.entries().len() > 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn branch(name: &str) -> BranchName {
        BranchName::try_new(name).unwrap()
    }

    fn human(id: &str) -> Actor {
        Actor::human(id).unwrap()
    }

    fn lease(holder: &str, acquired: i64, expires: i64) -> Lease {
        Lease::new(branch("feature/x"), human(holder), acquired, expires).unwrap()
    }

    #[test]
    fn the_file_reads_back_as_it_was_written() {
        let lease = lease("mia", 100, 3700);
        assert_eq!(
            lease.encode(),
            "stemma-lease 1\nbranch feature/x\nholder human mia\nacquired 100\nexpires 3700\n"
        );
        assert_eq!(Lease::parse(lease.encode().as_bytes()).unwrap(), lease);
    }

    #[test]
    fn the_reader_accepts_nothing_but_the_encoding() {
        let good = lease("mia", 100, 3700).encode();
        for bad in [
            good.replace("stemma-lease 1", "stemma-lease 2"),
            good.replace("expires 3700\n", "expires 3700"),
            good.replace("\n", "\r\n"),
            good.replace("expires 3700\n", "expires 3700\nextra\n"),
            good.replace("acquired 100", "acquired 0100"),
            good.replace(
                "holder human mia\nacquired 100\n",
                "acquired 100\nholder human mia\n",
            ),
            good.replace("expires 3700", "expires 100"),
            String::new(),
        ] {
            assert!(Lease::parse(bad.as_bytes()).is_err(), "{bad:?}");
        }
        assert_eq!(Lease::parse(&[0xff]), Err(LeaseFileError::NotUtf8));
    }

    #[test]
    fn a_branch_name_never_reaches_another_directory() {
        assert_eq!(file_name(&branch("main")), "main");
        assert_eq!(file_name(&branch("feature/x")), "feature%2Fx");
        assert_ne!(file_name(&branch("a/b")), file_name(&branch("a-b")));
        assert_ne!(
            file_name(&branch("Feature/x")).to_lowercase(),
            file_name(&branch("feature/x")).to_lowercase(),
            "no two branches share a file on a case-insensitive file system"
        );
        assert!(!file_name(&branch("a.b")).contains('.'));
    }

    #[test]
    fn a_long_branch_name_gets_a_short_file_name_of_its_own() {
        let long = |last: char| {
            let name = format!("{}{last}", "a.".repeat(99));
            branch(&name)
        };
        let (one, two) = (long('x'), long('y'));
        assert!(file_name(&one).len() <= MAX_FILE_NAME);
        assert_ne!(file_name(&one), file_name(&two));
        assert_eq!(file_name(&one), file_name(&one));
    }

    #[test]
    fn the_lease_ends_at_the_expiry() {
        let lease = lease("mia", 100, 200);
        assert!(!lease.is_expired(199));
        assert!(lease.is_expired(200));
    }

    #[test]
    fn a_renewal_moves_the_expiry_and_never_back() {
        let lease = lease("mia", 100, 3700);
        let later = lease.renewed(1000, 60);
        assert_eq!(later.expires(), 1000 + 3600);
        assert_eq!(later.acquired(), 100);
        let earlier = lease.renewed(0, 5);
        assert_eq!(earlier.expires(), 3700);
    }

    #[test]
    fn the_rule_follows_the_table() {
        let mia = human("mia");
        let dan = human("daniel");
        let held = lease("mia", 100, 200);
        assert_eq!(claim(None, &mia, 150), Ok(Claim::Acquire));
        assert_eq!(claim(Some(&held), &mia, 150), Ok(Claim::Renew));
        let refused = claim(Some(&held), &dan, 150).unwrap_err();
        assert_eq!(refused.holder, mia);
        assert_eq!(refused.expires, 200);
        assert_eq!(claim(Some(&held), &mia, 200), Ok(Claim::Acquire));
        assert_eq!(
            claim(Some(&held), &dan, 200),
            Ok(Claim::Takeover { from: mia })
        );
    }

    #[test]
    fn the_expiry_saturates() {
        assert_eq!(expiry(i64::MAX - 1, 60), i64::MAX);
    }

    #[test]
    fn a_lease_applies_only_with_two_identities_and_an_unprotected_branch() {
        use ssh_key::private::Ed25519Keypair;
        use ssh_key::PrivateKey;

        let key = |seed: u8| {
            let private = PrivateKey::from(Ed25519Keypair::from_seed(&[seed; 32]));
            crate::signature::PublicKey::from_openssh(&private.public_key().to_openssh().unwrap())
                .unwrap()
        };
        let one = Register::parse(
            format!(
                "stemma-register 1\nentry human daniel owner\nkey {} active\n",
                key(1)
            )
            .as_bytes(),
        )
        .unwrap();
        let two = Register::parse(
            format!(
                "stemma-register 1\nentry human daniel owner\nkey {} active\n\
                 entry human mia member\nkey {} active\n",
                key(1),
                key(2)
            )
            .as_bytes(),
        )
        .unwrap();
        assert!(!applies(None, false), "open mode has no lease");
        assert!(!applies(Some(&one), false), "one identity has no lease");
        assert!(applies(Some(&two), false));
        assert!(
            !applies(Some(&two), true),
            "a protected branch has no lease"
        );
    }
}
