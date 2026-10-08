//! Errors returned by every decoder and encoder in the crate.

use core::fmt;

/// The result type used throughout the crate.
pub type Result<T> = core::result::Result<T, Error>;

/// Everything that can go wrong while coding a PPMd stream.
///
/// No input, however malformed, makes the crate panic; it returns one of
/// these instead.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The stream is not a valid PPMd stream for the given parameters.
    CorruptStream {
        /// What the decoder found wrong, for diagnostics.
        detail: &'static str,
    },
    /// The model order or memory size is outside the range variant H accepts,
    /// or a framing header carries values the format does not allow.
    InvalidParameters,
    /// The underlying reader or writer failed.
    Io(std::io::Error),
    /// The input ended before the stream did.
    Truncated,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CorruptStream { detail } => write!(f, "corrupt PPMd stream: {detail}"),
            Self::InvalidParameters => f.write_str("invalid PPMd parameters"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Truncated => f.write_str("truncated PPMd stream"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// The `std::io::Read` and `std::io::Write` impls report errors this way:
/// an I/O error is passed through as it came, anything else is wrapped with
/// the matching [`std::io::ErrorKind`] (`InvalidData` for a corrupt stream,
/// `UnexpectedEof` for a truncated one, `InvalidInput` for bad parameters)
/// and can be recovered with `get_ref()` and `downcast_ref::<Error>()`.
impl From<Error> for std::io::Error {
    fn from(e: Error) -> Self {
        use std::io::ErrorKind;
        let kind = match e {
            Error::Io(inner) => return inner,
            Error::CorruptStream { .. } => ErrorKind::InvalidData,
            Error::InvalidParameters => ErrorKind::InvalidInput,
            Error::Truncated => ErrorKind::UnexpectedEof,
        };
        std::io::Error::new(kind, e)
    }
}
