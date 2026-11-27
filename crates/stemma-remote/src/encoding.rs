//! The content encoding of a body (ADR-023, ADR-029): zstd or none.
//!
//! The client wraps a body in the encoder and the decoder of zstd itself; `ureq` passes a body
//! with a content encoding that it does not know through as it is, because its feature `gzip` is
//! not built (ADR-028). The level, the window, and the bound of the window are fixed here, for
//! both directions, and a sender of the protocol keeps to the same bound (ADR-023), so no header
//! has to carry it.

use std::error::Error;
use std::fmt;
use std::io::{self, BufRead, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ureq::http::{header, HeaderMap};
use zstd::stream::raw::CParameter;
use zstd::zstd_safe::zstd_sys::ZSTD_ErrorCode;

/// The name of the content encoding of zstd in `Accept-Encoding` and `Content-Encoding`.
pub(crate) const ZSTD: &str = "zstd";

/// The level of the encoder. Measured on a Stemma store (ADR-029), level 3 with the long window
/// of [`WINDOW_LOG`] came within 35 % of level 19 with it, in a fortieth of its time.
const LEVEL: i32 = 3;

/// The base-2 logarithm of the window of the encoder, with long-distance matching: 2^27 bytes,
/// 128 MiB. It is the bound of the receiver as well ([`MAX_WINDOW`]).
const WINDOW_LOG: u32 = 27;

/// The largest window that the decoder accepts in a frame, in bytes: 2^27, 128 MiB (ADR-023).
/// A decoder holds the window in memory, so a frame that asks for more is refused with
/// [`WindowTooLarge`] before its window is allocated. It is the default bound of the C library
/// of zstd, so a sender that uses that library without settings keeps to it.
pub const MAX_WINDOW: u64 = 1 << WINDOW_LOG;

/// The error inside the [`io::Error`] that the body of an answer gives when a frame of zstd asks
/// for a window larger than [`MAX_WINDOW`]. The kind of the [`io::Error`] is
/// [`io::ErrorKind::InvalidData`], and [`io::Error::get_ref`] returns this error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct WindowTooLarge {
    /// The bound that the frame exceeds, in bytes: [`MAX_WINDOW`].
    pub bound: u64,
}

impl fmt::Display for WindowTooLarge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the body compressed with zstd asks for a window larger than {} bytes",
            self.bound
        )
    }
}

impl Error for WindowTooLarge {}

/// The content encoding of the body of an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContentEncoding {
    /// No `Content-Encoding`, or `identity` alone.
    Identity,
    /// `zstd`, once, beside any number of `identity`.
    Zstd,
}

/// The content encoding that `headers` name, read over every line and every value of
/// `Content-Encoding`, or the value that the client does not read: one that is neither
/// `identity` nor `zstd`, or a second `zstd`, which would be a body compressed twice.
pub(crate) fn content_encoding(headers: &HeaderMap) -> Result<ContentEncoding, String> {
    let mut found = ContentEncoding::Identity;
    for value in headers.get_all(header::CONTENT_ENCODING) {
        let value = String::from_utf8_lossy(value.as_bytes());
        for part in value.split(',').map(str::trim) {
            if part.eq_ignore_ascii_case("identity") {
                continue;
            }
            if part.eq_ignore_ascii_case(ZSTD) && found == ContentEncoding::Identity {
                found = ContentEncoding::Zstd;
                continue;
            }
            return Err(value.into_owned());
        }
    }
    Ok(found)
}

/// Whether `headers` name zstd in `Accept-Encoding`, with a weight that is not zero. That is how
/// the hub says that it takes an upload compressed with zstd (ADR-023).
pub(crate) fn accepts_zstd(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|part| {
            let mut pieces = part.split(';').map(str::trim);
            let named = pieces
                .next()
                .is_some_and(|name| name.eq_ignore_ascii_case(ZSTD));
            named && pieces.all(|piece| !is_zero_weight(piece))
        })
}

/// Whether a parameter of `Accept-Encoding` is the weight zero (`q=0`, `q=0.0`, up to `q=0.000`),
/// which names an encoding as not acceptable.
fn is_zero_weight(piece: &str) -> bool {
    let Some((name, weight)) = piece.split_once('=') else {
        return false;
    };
    name.trim().eq_ignore_ascii_case("q")
        && weight.trim().strip_prefix('0').is_some_and(|rest| {
            rest.is_empty() || rest.trim_start_matches('.').bytes().all(|b| b == b'0')
        })
}

/// The reader of a body to upload, compressed with zstd at [`LEVEL`] with the window of
/// [`WINDOW_LOG`] and long-distance matching. An error of `body` comes out of the reader as it
/// is.
pub(crate) fn encoder<'r, R: Read + 'r>(body: R) -> io::Result<Box<dyn Read + 'r>> {
    let mut encoder = zstd::stream::read::Encoder::new(body, LEVEL)?;
    encoder.set_parameter(CParameter::WindowLog(WINDOW_LOG))?;
    encoder.set_parameter(CParameter::EnableLongDistanceMatching(true))?;
    Ok(Box::new(encoder))
}

/// The reader of a body compressed with zstd, which gives the decoded bytes.
///
/// A frame with a window larger than [`MAX_WINDOW`] gives [`WindowTooLarge`], every other error
/// of zstd an error of the kind [`io::ErrorKind::InvalidData`], and a body that ends inside a
/// frame, also an empty one, an error of the kind [`io::ErrorKind::UnexpectedEof`];
/// [`decoding_error`] tells these from the others. An error of `body` comes out as it is, with
/// its inner error, and so does a failure of the decoder on this side, such as one to allocate
/// its memory. An error stays: every read after the first one fails with the same kind and text.
pub(crate) fn decoder<R: BufRead + 'static>(body: R) -> io::Result<Decoded> {
    let failed = Arc::new(AtomicBool::new(false));
    let source = Source {
        inner: body,
        failed: Arc::clone(&failed),
    };
    let mut decoder = zstd::stream::read::Decoder::with_buffer(source)?;
    decoder.window_log_max(WINDOW_LOG)?;
    Ok(Decoded {
        decoder: Box::new(decoder),
        source_failed: failed,
        failed: None,
    })
}

/// The text of an error that the decoder gives for the bytes of the other side: a window above
/// the bound, a frame that cannot be decoded, or a body that ends inside a frame. `None` for an
/// error of the carrier and for a failure on this side.
pub(crate) fn decoding_error(error: &io::Error) -> Option<String> {
    let inner = error.get_ref()?;
    if inner.is::<WindowTooLarge>() || inner.is::<Undecodable>() {
        Some(inner.to_string())
    } else {
        None
    }
}

/// The error inside the [`io::Error`] of a body compressed with zstd that cannot be decoded,
/// other than [`WindowTooLarge`].
#[derive(Debug)]
struct Undecodable(String);

impl fmt::Display for Undecodable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for Undecodable {}

/// The body below the decoder, which notes that an error came from it, so that the decoder can
/// tell an error of the body from one of its own.
struct Source<R> {
    inner: R,
    failed: Arc<AtomicBool>,
}

impl<R> Source<R> {
    fn note<T>(&self, result: io::Result<T>) -> io::Result<T> {
        if let Err(error) = &result {
            if error.kind() != io::ErrorKind::Interrupted {
                self.failed.store(true, Ordering::Relaxed);
            }
        }
        result
    }
}

impl<R: BufRead> Read for Source<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let result = self.inner.read(buf);
        self.note(result)
    }
}

impl<R: BufRead> BufRead for Source<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        // The borrow of the buffer ends before the error is noted.
        if let Err(error) = self.inner.fill_buf() {
            return self.note(Err(error));
        }
        self.inner.fill_buf()
    }

    fn consume(&mut self, amount: usize) {
        self.inner.consume(amount);
    }
}

/// The decoded body of an answer; see [`decoder`].
pub(crate) struct Decoded {
    decoder: Box<dyn Read>,
    source_failed: Arc<AtomicBool>,
    failed: Option<(io::ErrorKind, String, Kind)>,
}

/// Which error a [`Decoded`] keeps, to give it again with its inner error where that is one of
/// this module.
#[derive(Clone, Copy)]
enum Kind {
    Window,
    Undecodable,
    Other,
}

impl Read for Decoded {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some((kind, text, which)) = &self.failed {
            return Err(match which {
                Kind::Window => io::Error::new(*kind, WindowTooLarge { bound: MAX_WINDOW }),
                Kind::Undecodable => io::Error::new(*kind, Undecodable(text.clone())),
                Kind::Other => io::Error::new(*kind, text.clone()),
            });
        }
        let error = match self.decoder.read(buf) {
            Ok(read) => return Ok(read),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => return Err(error),
            Err(error) => error,
        };
        let code = error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<zstd::Error>())
            .copied();
        let error = if self.source_failed.load(Ordering::Relaxed) {
            // An error of the body, which the decoder passed on.
            error
        } else if let Some(code) = code {
            if code.is(ZSTD_ErrorCode::ZSTD_error_frameParameter_windowTooLarge) {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    WindowTooLarge { bound: MAX_WINDOW },
                )
            } else {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    Undecodable(format!(
                        "the body compressed with zstd cannot be decoded: {code}"
                    )),
                )
            }
        } else if error.kind() == io::ErrorKind::UnexpectedEof {
            // The body ended without an error of its own, and the decoder says that a frame
            // is not complete.
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                Undecodable("the body compressed with zstd ends inside a frame".to_owned()),
            )
        } else {
            // A failure of the decoder on this side, such as one to allocate its memory.
            error
        };
        let which = match error.get_ref() {
            Some(inner) if inner.is::<WindowTooLarge>() => Kind::Window,
            Some(inner) if inner.is::<Undecodable>() => Kind::Undecodable,
            _ => Kind::Other,
        };
        self.failed = Some((error.kind(), error.to_string(), which));
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ureq::http::HeaderValue;

    fn accepts(values: &[&str]) -> bool {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(
                header::ACCEPT_ENCODING,
                HeaderValue::from_str(value).unwrap(),
            );
        }
        accepts_zstd(&headers)
    }

    #[test]
    fn only_a_weight_of_zero_names_zstd_as_not_acceptable() {
        for zero in ["q=0", "q=0.", "q=0.0", "q=0.000", " Q = 0 "] {
            assert!(is_zero_weight(zero), "{zero}");
        }
        for other in ["q=0.001", "q=0.5", "q=1", "q=1.0", "q=", "level=0", "q"] {
            assert!(!is_zero_weight(other), "{other}");
        }
    }

    #[test]
    fn zstd_is_accepted_on_any_line_with_a_weight_that_is_not_zero() {
        assert!(accepts(&["zstd"]));
        assert!(accepts(&["ZSTD;q=0.001"]));
        assert!(accepts(&["gzip", "br, zstd ; q=1"]));
        assert!(!accepts(&[]));
        assert!(!accepts(&["gzip"]));
        assert!(!accepts(&["zstd;q=0"]));
        assert!(!accepts(&["*"]));
        assert!(!accepts(&["zstdx"]));
    }
}
