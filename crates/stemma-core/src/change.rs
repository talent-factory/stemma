//! The change ID: the identity of a change, which stays the same when the change is rewritten
//! (ADR-010).
//!
//! A commit ID names one state, and every rewrite, such as a snapshot of the working copy, an
//! amend, or a rebase, gives a new commit a new ID. The change ID names "the same change" through
//! all of them, so that a review, a pull request, and a rebase can follow it. It is therefore
//! random and not derived from the content of the commit. A new one is made by
//! [`ChangeId::generate`]. A rewrite of one change takes over the ID of the commit it replaces. A
//! merge commit is not a rewrite of either parent: it joins two changes and continues neither, so
//! it gets a new ID (ADR-010). `Commit::new` does not decide which case applies; the caller passes
//! the ID, and a reader gets it back from the commit.
//!
//! The textual form is `ch:` followed by 32 lowercase hexadecimal digits (128 bits). The prefix
//! sets it apart from an object ID (`b3:`, ADR-001), so that the one is never read as the other,
//! and it leaves room for another form. Uppercase digits are rejected, so every change ID has
//! exactly one textual form, as in ADR-005.
//!
//! Every value of 128 bits is a change ID, including all zeros, because the reader cannot know
//! whether a value was drawn at random. [`ChangeId::generate`] never returns all zeros: that value
//! can only come from a broken source, and it would give every new change the same ID.

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
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChangeId([u8; BYTES]);

impl ChangeId {
    /// Makes a new change ID from 128 bits of the random source of the operating system. Two calls
    /// give the same value with a probability of 2^-128.
    ///
    /// # Errors
    ///
    /// Returns [`GenerateError`] if the operating system gives no random bytes, or gives only
    /// zeros. The call never falls back to a weaker source: a change ID that two changes share
    /// would join them.
    pub fn generate() -> Result<Self, GenerateError> {
        Self::generate_with(getrandom::fill)
    }

    /// Makes a change ID from the bytes that `fill` writes, which stands for the random source.
    /// It exists so that a test can make the source fail or return zeros.
    pub(crate) fn generate_with(
        fill: impl FnOnce(&mut [u8]) -> Result<(), getrandom::Error>,
    ) -> Result<Self, GenerateError> {
        let mut bytes = [0u8; BYTES];
        fill(&mut bytes).map_err(|error| GenerateError {
            source: io::Error::from(error),
        })?;
        // A source that reports success and writes nothing leaves zeros. A real draw is all zeros
        // with a probability of 2^-128, so this can only be a broken source.
        if bytes == [0; BYTES] {
            return Err(GenerateError {
                source: io::Error::other("the random source gave only zero bytes"),
            });
        }
        Ok(Self(bytes))
    }

    /// Builds a change ID from its 16 bytes, whatever they are. Only the tests of this crate call
    /// it: production code gets a change ID from [`generate`](Self::generate) or from reading
    /// one, so that nobody can make up an ID, such as all zeros, for a change that is not the one
    /// it names.
    #[cfg(test)]
    pub(crate) fn from_bytes(bytes: [u8; BYTES]) -> Self {
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
    /// After the prefix are not exactly 32 bytes. Carries the number of bytes: a character that is
    /// not ASCII takes more than one, so this can differ from the number of characters.
    WrongLength(usize),
    /// A character is not a lowercase hexadecimal digit. Carries its position, counted from 0 in
    /// the bytes after the prefix.
    NotHex(usize),
}

impl fmt::Display for ChangeIdParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingPrefix => write!(f, "a change ID starts with `{PREFIX}`"),
            Self::WrongLength(length) => write!(
                f,
                "a change ID has {HEX_LEN} hexadecimal digits after the prefix, not {length} \
                 bytes"
            ),
            Self::NotHex(position) => write!(
                f,
                "byte {position} after the prefix of a change ID (counting from 0) is not a \
                 lowercase hexadecimal digit"
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

/// The operating system could not give random bytes for a change ID, or gave only zeros. The
/// message of this error ends with the message of the cause; use [`GenerateError::io_error`] to
/// reach the cause itself.
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
        // The message ends with that of the I/O error, so the source is the cause of that error,
        // the first link that the message does not print yet.
        self.source.source()
    }
}

/// A fixed change ID for tests, `ch:00112233445566778899aabbccddeeff`. Its two nibbles of each byte
/// are equal; a test that has to tell the nibbles apart uses [`DISTINCT`](tests::DISTINCT).
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

    /// A value whose two nibbles differ in every byte, so that a swap of the nibbles shows.
    pub(crate) const DISTINCT: &str = "ch:0123456789abcdeffedcba9876543210";

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
    fn the_nibbles_of_a_byte_keep_their_order() {
        let id: ChangeId = DISTINCT.parse().unwrap();
        assert_eq!(
            id.as_bytes(),
            &[
                0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54,
                0x32, 0x10
            ]
        );
        assert_eq!(id.to_string(), DISTINCT);
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
    fn the_length_is_counted_in_bytes_and_a_non_ascii_character_does_not_panic() {
        // 30 digits and one two-byte character are 32 bytes: the length is right, the digit is not.
        let text = format!("ch:{}\u{e9}", "a".repeat(30));
        assert_eq!(
            text.parse::<ChangeId>(),
            Err(ChangeIdParseError::NotHex(30))
        );
        let text = format!("ch:\u{e9}{}", "a".repeat(30));
        assert_eq!(text.parse::<ChangeId>(), Err(ChangeIdParseError::NotHex(0)));
        // 16 two-byte characters are 32 bytes, and 32 of them are 64 bytes.
        let text = format!("ch:{}", "\u{e9}".repeat(16));
        assert_eq!(text.parse::<ChangeId>(), Err(ChangeIdParseError::NotHex(0)));
        let text = format!("ch:{}", "\u{e9}".repeat(32));
        assert_eq!(
            text.parse::<ChangeId>(),
            Err(ChangeIdParseError::WrongLength(64))
        );
        // A four-byte character at the end of 28 digits makes 32 bytes.
        let text = format!("ch:{}\u{1f600}", "a".repeat(28));
        assert_eq!(
            text.parse::<ChangeId>(),
            Err(ChangeIdParseError::NotHex(28))
        );
        // The length is checked before the digits.
        let text = format!("ch:{}g", "a".repeat(32));
        assert_eq!(
            text.parse::<ChangeId>(),
            Err(ChangeIdParseError::WrongLength(33))
        );
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
    fn every_byte_of_a_generated_id_takes_many_values() {
        // 1000 draws of 16 bytes. A byte that is filled with a constant, or not filled at all, has
        // one value; a random byte has about 250 of them in 1000 draws, and fewer than 100 has a
        // probability far below 2^-100.
        let ids: Vec<ChangeId> = (0..1000).map(|_| ChangeId::generate().unwrap()).collect();
        for position in 0..BYTES {
            let values: HashSet<u8> = ids.iter().map(|id| id.as_bytes()[position]).collect();
            assert!(
                values.len() > 100,
                "byte {position} has {} values",
                values.len()
            );
        }
    }

    #[test]
    fn a_failing_source_is_an_error_with_its_cause_and_no_id() {
        let error = ChangeId::generate_with(|_| Err(getrandom::Error::UNSUPPORTED)).unwrap_err();
        let prefix = "could not get random bytes for a change ID: ";
        let message = error.to_string();
        assert!(
            message.starts_with(prefix) && message.len() > prefix.len(),
            "{message}"
        );
        assert_eq!(
            error.io_error().to_string(),
            io::Error::from(getrandom::Error::UNSUPPORTED).to_string()
        );
    }

    #[test]
    fn a_source_that_writes_nothing_is_an_error_and_not_the_zero_id() {
        let error = ChangeId::generate_with(|_| Ok(())).unwrap_err();
        assert!(error.to_string().contains("only zero bytes"), "{error}");
        let error = ChangeId::generate_with(|bytes| {
            bytes.fill(0);
            Ok(())
        })
        .unwrap_err();
        assert!(error.to_string().contains("only zero bytes"), "{error}");
    }

    #[test]
    fn a_source_that_fills_the_buffer_gives_exactly_those_bytes() {
        let id = ChangeId::generate_with(|bytes| {
            bytes.copy_from_slice(&[7; BYTES]);
            Ok(())
        })
        .unwrap();
        assert_eq!(id.as_bytes(), &[7; BYTES]);
        // A single non-zero byte is enough: only all zeros is refused.
        let id = ChangeId::generate_with(|bytes| {
            bytes[BYTES - 1] = 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(id.to_string(), "ch:00000000000000000000000000000001");
    }

    #[test]
    fn error_messages_are_complete_and_say_what_is_wrong() {
        let prefix = ChangeIdParseError::MissingPrefix.to_string();
        assert!(prefix.contains("`ch:`"), "{prefix}");
        let length = ChangeIdParseError::WrongLength(30).to_string();
        assert!(
            length.contains("32") && length.contains("30 bytes"),
            "{length}"
        );
        let digit = ChangeIdParseError::NotHex(4).to_string();
        assert!(
            digit.contains("byte 4") && digit.contains("counting from 0"),
            "{digit}"
        );
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
