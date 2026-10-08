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
    CorruptStream,
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
            Self::CorruptStream => f.write_str("corrupt PPMd stream"),
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
