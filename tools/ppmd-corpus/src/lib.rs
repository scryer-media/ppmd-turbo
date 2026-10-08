//! The ppmd-turbo test and benchmark corpus: deterministic payloads, the
//! oracles that check every stream (ppmd-rust and 7-Zip's `7zz`), the
//! read-only walks that pull a raw PPMd stream out of a `.7z` or a RAR
//! 2.9/3.x/4.x archive, and `fixtures`, the one entry point that regenerates
//! every generated file the tests and fuzz targets read (none is committed). A repository tool: it is a workspace member so its
//! dependencies never reach the crate's own graph, and it is never published.

pub mod corpus;
pub mod fixtures;
pub mod oracle;
pub mod payload;
pub mod rar;
pub mod sevenz;
