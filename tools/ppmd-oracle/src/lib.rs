//! Out-of-process oracle checks for ppmd-turbo.
//!
//! `7zz` and `unrar` are the references ppmd-turbo must be bit-exact with.
//! This crate wraps PPMd streams in a minimal `.7z` container ([`sevenz`]),
//! drives the binaries ([`binaries`]) and compares their results with a codec
//! ([`codec`]): ppmd-rust 1.5.0 to validate the oracle itself, ppmd-turbo for
//! the real check.

#![forbid(unsafe_code)]

pub mod binaries;
pub mod codec;
pub mod corpus;
pub mod sevenz;
