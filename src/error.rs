//! Errors, stream positions and the progress every step call reports.
//!
//! The crate performs no I/O, so nothing here wraps an I/O error: a call
//! fails because the stream is corrupt or truncated, because the parameters
//! are out of range, or because the model's memory could not be had.
//!
//! **Progress invariant.** Every step call returns a [`Progress`] that says
//! how many input bytes it took and how many output bytes it wrote. A call
//! that took nothing and wrote nothing always says why in its status: the
//! input left is too short to decode a symbol from and is not the last
//! (`NeedInput`), the output slice is empty (`OutputFull`), or the stream
//! has stopped (a size reached, an end marker, a RAR escape or model end).
//! A caller loop that feeds more input on `NeedInput`, drains output on
//! `OutputFull` and stops on the rest therefore always makes progress; it
//! never spins and never needs a timer to notice a stall. The fuzz target
//! `chunking_invariance` asserts this on every call.

use core::fmt;

/// Where in a stream an error was found.
///
/// Both counts run from the start of the stream: the codec's construction or
/// last `reset` for 7z and the raw carry-less streams, the last
/// `start_block` for RAR. The container adds its own context (block index,
/// packed offset).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Position {
    /// Input bytes consumed before the error.
    pub input: u64,
    /// Output bytes produced before the error.
    pub output: u64,
}

/// What went wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The stream is not a valid PPMd stream for its parameters. The text
    /// says what the decoder found, for diagnostics only.
    Corrupt(&'static str),
    /// The input ended before the stream did: the last input was given and a
    /// symbol needed a byte past it (7z), or more padding than the caller
    /// allows (RAR).
    Truncated,
    /// The model order or memory size is outside what variant H accepts, or
    /// a call came in an order the codec does not allow.
    InvalidParameters,
    /// The model's memory could not be allocated.
    AllocationFailed {
        /// Bytes asked of the allocator.
        bytes: u64,
    },
    /// The model would need more memory than the caller allowed.
    MemoryLimit {
        /// Bytes the stream's parameters need.
        required: u64,
        /// The caller's limit.
        limit: u64,
    },
}

/// An error from a decoder or encoder, with the stream position it was found
/// at.
///
/// Errors are `Copy`: after one, the codec repeats it on every later call
/// until it is reset, so a caller that retries sees exactly the same error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error {
    /// What went wrong.
    pub kind: ErrorKind,
    /// Where it was found.
    pub at: Position,
}

impl Error {
    /// An error of `kind` at the start of the stream.
    #[inline]
    pub const fn new(kind: ErrorKind) -> Self {
        Self {
            kind,
            at: Position {
                input: 0,
                output: 0,
            },
        }
    }

    /// What went wrong.
    #[inline]
    pub const fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Where it was found.
    #[inline]
    pub const fn position(&self) -> Position {
        self.at
    }

    /// Whether the bytes of the stream are at fault ([`ErrorKind::Corrupt`]
    /// or [`ErrorKind::Truncated`]), as opposed to the parameters or the
    /// memory available. A container that can repair or refetch data treats
    /// these as repairable.
    #[inline]
    pub const fn is_data_error(&self) -> bool {
        matches!(self.kind, ErrorKind::Corrupt(_) | ErrorKind::Truncated)
    }

    #[cold]
    #[inline(never)]
    pub(crate) const fn corrupt(detail: &'static str) -> Self {
        Self::new(ErrorKind::Corrupt(detail))
    }

    #[cold]
    #[inline(never)]
    pub(crate) const fn truncated() -> Self {
        Self::new(ErrorKind::Truncated)
    }

    #[cold]
    #[inline(never)]
    pub(crate) const fn invalid_parameters() -> Self {
        Self::new(ErrorKind::InvalidParameters)
    }

    /// The same error at `at`.
    #[inline]
    pub(crate) const fn at(self, at: Position) -> Self {
        Self {
            kind: self.kind,
            at,
        }
    }
}

/// The text deliberately avoids the words "method", "unsupported",
/// "password" and "encrypted": archive tooling downstream classifies errors
/// by matching them, and a PPMd error is none of those.
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            ErrorKind::Corrupt(detail) => write!(f, "corrupt PPMd stream: {detail}")?,
            ErrorKind::Truncated => f.write_str("truncated PPMd stream")?,
            ErrorKind::InvalidParameters => f.write_str("invalid PPMd parameters")?,
            ErrorKind::AllocationFailed { bytes } => {
                write!(f, "could not allocate {bytes} bytes for the PPMd model")?;
            }
            ErrorKind::MemoryLimit { required, limit } => write!(
                f,
                "the PPMd model needs {required} bytes, more than the {limit}-byte limit"
            )?,
        }
        write!(
            f,
            " (at input byte {}, output byte {})",
            self.at.input, self.at.output
        )
    }
}

#[cfg(feature = "std")]
impl std::error::Error for Error {}

/// Errors as [`std::io::Error`]s, for code that drives a codec through
/// `Read` or `Write`: a corrupt stream is `InvalidData`, a truncated one
/// `UnexpectedEof`, bad parameters `InvalidInput`, and a memory refusal
/// `OutOfMemory`. The [`Error`] stays inside and can be recovered with
/// `get_ref()` and `downcast_ref::<Error>()`.
#[cfg(feature = "std")]
impl From<Error> for std::io::Error {
    fn from(e: Error) -> Self {
        use std::io::ErrorKind as Io;
        let kind = match e.kind {
            ErrorKind::Corrupt(_) => Io::InvalidData,
            ErrorKind::Truncated => Io::UnexpectedEof,
            ErrorKind::InvalidParameters => Io::InvalidInput,
            ErrorKind::AllocationFailed { .. } | ErrorKind::MemoryLimit { .. } => Io::OutOfMemory,
        };
        std::io::Error::new(kind, e)
    }
}

/// The result type used throughout the crate.
pub type Result<T> = core::result::Result<T, Error>;

/// What one step call did.
///
/// `consumed` and `produced` are exact: the codec never keeps caller bytes
/// between calls, so the bytes after `consumed` belong to the caller and
/// must be presented again (followed by more) on the next call. See the
/// module documentation for the progress invariant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress<S> {
    /// Input bytes taken.
    pub consumed: usize,
    /// Output bytes written.
    pub produced: usize,
    /// Why the call returned.
    pub status: S,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_avoids_the_words_downstream_classifiers_match() {
        let kinds = [
            ErrorKind::Corrupt("model pointer or frequency out of bounds"),
            ErrorKind::Corrupt("7z range coder: first byte is not zero"),
            ErrorKind::Truncated,
            ErrorKind::InvalidParameters,
            ErrorKind::AllocationFailed { bytes: 1 << 30 },
            ErrorKind::MemoryLimit {
                required: 1 << 30,
                limit: 1 << 20,
            },
        ];
        for kind in kinds {
            let text = Error::new(kind).to_string().to_ascii_lowercase();
            for word in ["method", "unsupported", "password", "encrypted"] {
                assert!(!text.contains(word), "{text}");
            }
        }
    }

    #[cfg(feature = "std")]
    #[test]
    fn io_conversion_keeps_the_kind_and_the_error() {
        use std::io::ErrorKind as Io;
        for (kind, io) in [
            (ErrorKind::Corrupt("x"), Io::InvalidData),
            (ErrorKind::Truncated, Io::UnexpectedEof),
            (ErrorKind::InvalidParameters, Io::InvalidInput),
            (ErrorKind::AllocationFailed { bytes: 1 }, Io::OutOfMemory),
        ] {
            let e = Error::new(kind).at(Position {
                input: 3,
                output: 4,
            });
            let converted = std::io::Error::from(e);
            assert_eq!(converted.kind(), io);
            let inner = converted.get_ref().unwrap().downcast_ref::<Error>();
            assert_eq!(inner, Some(&e));
        }
    }
}
