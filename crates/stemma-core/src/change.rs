//! The change ID: the identity of a change, which stays the same when the change is rewritten
//! (ADR-010).
//!
//! A commit ID names one state, and every rewrite, such as a snapshot of the working copy, an
//! amend, or a rebase, gives a new commit a new ID. The change ID names "the same change" through
//! all of them, so that a review, a pull request, and a rebase can follow it. It is therefore
//! random and not derived from the content of the commit. A new one is made by
//! [`ChangeId::generate`]; every rewrite takes over the one of the commit it replaces, and a
//! reader gets it back from the commit.
//!
//! The textual form is `ch:` followed by 32 lowercase hexadecimal digits (128 bits). The prefix
//! sets it apart from an object ID (`b3:`, ADR-001), so that the one is never read as the other,
//! and it leaves room for another form. Uppercase digits are rejected, so every change ID has
//! exactly one textual form, as in ADR-005.
//!
//! Every value of 128 bits is a change ID, including all zeros. The reader cannot know whether a
//! value was drawn at random.

use std::error::Error;
use std::fmt;
use std::io;
use std::str::FromStr;

/// The prefix of the textual form.
const PREFIX: &str = "ch:";

/// Number of bytes of a change ID.
const BYTES: usize = 16;

/// Number of hexadecimal characters of a change ID.
const HEX_LEN: usize = 2 * BYTES;

/// The identity of a change. See the module documentation.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChangeId([u8; BYTES]);

impl ChangeId {
    /// Makes a new change ID from 128 bits of the random source of the operating system. Two calls
    /// give the same value with a probability of 2^-128.
    ///
    /// # Errors
    ///
    /// Returns [`GenerateError`] if the operating system gives no random bytes. The call never
    /// falls back to a weaker source: a change ID that two changes share would join them.
    pub fn generate() -> Result<Self, GenerateError> {
        let mut bytes = [0u8; BYTES];
        getrandom::fill(&mut bytes).map_err(|error| GenerateError {
            source: io::Error::from(error),
        })?;
        Ok(Self(bytes))
    }

    /// Builds a change ID from its 16 bytes. This is for tests and for a caller that owns the
    /// source of randomness; [`generate`](Self::generate) is the way to make a new change.
    pub fn from_bytes(bytes: [u8; BYTES]) -> Self {
        Self(bytes)
    }

    /// Returns the 16 bytes.
    pub fn as_bytes(&self) -> &[u8; BYTES] {
        &self.0
    }
}

impl fmt::Display for ChangeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(PREFIX)?;
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for ChangeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ChangeId({self})")
    }
}

/// Why a string is not a change ID. The `Display` form is the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ChangeIdParseError {
    /// The text does not start with `ch:`.
    MissingPrefix,
    /// After the prefix are not exactly 32 characters. Carries the number of characters, counted
    /// in bytes.
    WrongLength(usize),
    /// A character is not a lowercase hexadecimal digit. Carries its position in the digits.
    NotHex(usize),
}

impl fmt::Display for ChangeIdParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingPrefix => write!(f, "a change ID starts with `{PREFIX}`"),
            Self::WrongLength(length) => write!(
                f,
                "a change ID has {HEX_LEN} hexadecimal digits after the prefix, not {length}"
            ),
            Self::NotHex(position) => write!(
                f,
                "character {position} of a change ID is not a lowercase hexadecimal digit"
            ),
        }
    }
}

impl Error for ChangeIdParseError {}

impl FromStr for ChangeId {
    type Err = ChangeIdParseError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let digits = text
            .strip_prefix(PREFIX)
            .ok_or(ChangeIdParseError::MissingPrefix)?;
        if digits.len() != HEX_LEN {
            return Err(ChangeIdParseError::WrongLength(digits.len()));
        }
        let digit = |position: usize| match digits.as_bytes()[position] {
            byte @ b'0'..=b'9' => Ok(byte - b'0'),
            byte @ b'a'..=b'f' => Ok(byte - b'a' + 10),
            _ => Err(ChangeIdParseError::NotHex(position)),
        };
        let mut bytes = [0u8; BYTES];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = (digit(2 * index)? << 4) | digit(2 * index + 1)?;
        }
        Ok(Self(bytes))
    }
}

/// The operating system could not give random bytes for a change ID. The cause is the field
/// `source`, and its message ends the message of this error; use
/// [`GenerateError::io_error`] to reach it.
#[derive(Debug)]
pub struct GenerateError {
    source: io::Error,
}

impl GenerateError {
    /// Returns the error of the operating system. The chain of [`Error::source`] does not lead to
    /// it (see the crate documentation).
    pub fn io_error(&self) -> &io::Error {
        &self.source
    }
}

impl fmt::Display for GenerateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "could not get random bytes for a change ID: {}",
            self.source
        )
    }
}

impl Error for GenerateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        // The message includes that of the I/O error, so the source is its own cause.
        self.source.source()
    }
}

/// A fixed change ID for tests, `ch:00112233445566778899aabbccddeeff`.
#[cfg(test)]
pub(crate) fn sample_change() -> ChangeId {
    ChangeId::from_bytes([
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff,
    ])
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    const TEXT: &str = "ch:00112233445566778899aabbccddeeff";

    fn bytes() -> [u8; BYTES] {
        [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ]
    }

    #[test]
    fn the_textual_form_is_the_prefix_and_32_lowercase_digits() {
        let id = ChangeId::from_bytes(bytes());
        assert_eq!(id.to_string(), TEXT);
        assert_eq!(TEXT.parse::<ChangeId>().unwrap(), id);
        assert_eq!(id.as_bytes(), &bytes());
        assert_eq!(format!("{id:?}"), format!("ChangeId({TEXT})"));
    }

    #[test]
    fn leading_zeros_are_kept() {
        let zero = ChangeId::from_bytes([0; BYTES]);
        assert_eq!(zero.to_string(), "ch:00000000000000000000000000000000");
        assert_eq!(zero.to_string().parse::<ChangeId>().unwrap(), zero);
    }

    #[test]
    fn the_reader_rejects_everything_the_writer_would_not_write() {
        let cases: [(&str, ChangeIdParseError); 11] = [
            ("", ChangeIdParseError::MissingPrefix),
            (
                "00112233445566778899aabbccddeeff",
                ChangeIdParseError::MissingPrefix,
            ),
            (
                "CH:00112233445566778899aabbccddeeff",
                ChangeIdParseError::MissingPrefix,
            ),
            (
                "b3:00112233445566778899aabbccddeeff",
                ChangeIdParseError::MissingPrefix,
            ),
            ("ch:", ChangeIdParseError::WrongLength(0)),
            (
                "ch:00112233445566778899aabbccddee",
                ChangeIdParseError::WrongLength(30),
            ),
            (
                "ch:00112233445566778899aabbccddeeff0",
                ChangeIdParseError::WrongLength(33),
            ),
            (
                "ch:00112233445566778899AABBCCDDEEFF",
                ChangeIdParseError::NotHex(20),
            ),
            (
                "ch:0011223344556677 899aabbccddeeff",
                ChangeIdParseError::NotHex(16),
            ),
            (
                "ch:00112233445566778899aabbccddeefg",
                ChangeIdParseError::NotHex(31),
            ),
            (
                "ch: 0112233445566778899aabbccddeeff",
                ChangeIdParseError::NotHex(0),
            ),
        ];
        for (text, expected) in cases {
            assert_eq!(text.parse::<ChangeId>(), Err(expected), "{text:?}");
        }
    }

    #[test]
    fn a_non_ascii_character_is_reported_by_its_position_and_does_not_panic() {
        // Two bytes long: the length is counted in bytes, so 31 digits and one `é` are 32 bytes.
        let text = format!("ch:{}é", "a".repeat(30));
        assert_eq!(
            text.parse::<ChangeId>(),
            Err(ChangeIdParseError::NotHex(30))
        );
        let text = format!("ch:é{}", "a".repeat(30));
        assert_eq!(text.parse::<ChangeId>(), Err(ChangeIdParseError::NotHex(0)));
    }

    #[test]
    fn an_object_id_is_not_a_change_id() {
        let object = "b3:9db8039d1b509628ad5b125ea89780b69fcde5c272215d690a2e4dc69eac75ad";
        assert_eq!(
            object.parse::<ChangeId>(),
            Err(ChangeIdParseError::MissingPrefix)
        );
    }

    #[test]
    fn generated_ids_differ_and_round_trip() {
        let mut seen = HashSet::new();
        for _ in 0..1000 {
            let id = ChangeId::generate().unwrap();
            assert_eq!(id.to_string().parse::<ChangeId>().unwrap(), id);
            assert!(seen.insert(id), "a change ID was drawn twice");
        }
    }

    #[test]
    fn a_generated_id_is_not_the_zero_value_and_uses_the_whole_width() {
        // 1000 draws of 16 bytes: a byte position that is zero every time has a chance of
        // 256^-1000, so this cannot fail unless the bytes are not filled.
        let ids: Vec<ChangeId> = (0..1000).map(|_| ChangeId::generate().unwrap()).collect();
        for position in 0..BYTES {
            assert!(
                ids.iter().any(|id| id.as_bytes()[position] != 0),
                "{position}"
            );
        }
    }

    #[test]
    fn error_messages_are_complete_and_say_what_is_wrong() {
        assert!(ChangeIdParseError::MissingPrefix
            .to_string()
            .contains("`ch:`"));
        assert!(ChangeIdParseError::WrongLength(30)
            .to_string()
            .contains("32"));
        assert!(ChangeIdParseError::WrongLength(30)
            .to_string()
            .contains("30"));
        assert!(ChangeIdParseError::NotHex(4)
            .to_string()
            .contains("character 4"));
    }

    #[derive(Debug)]
    struct Outer;

    impl fmt::Display for Outer {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("outer")
        }
    }

    impl Error for Outer {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            Some(&Deepest)
        }
    }

    #[derive(Debug)]
    struct Deepest;

    impl fmt::Display for Deepest {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("deepest")
        }
    }

    impl Error for Deepest {}

    #[test]
    fn a_failed_generation_follows_the_convention_for_messages_and_causes() {
        let error = GenerateError {
            source: io::Error::other(Outer),
        };
        assert_eq!(
            error.to_string(),
            "could not get random bytes for a change ID: outer"
        );
        assert_eq!(error.source().unwrap().to_string(), "deepest");
        assert_eq!(error.io_error().to_string(), "outer");
    }
}
