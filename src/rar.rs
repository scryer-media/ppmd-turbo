//! The RAR framing.
//!
//! Will hold PPMd as RAR 2.9/3.x carries it inside its block stream: the
//! block header flags (reset, order, memory size in MiB, escape character),
//! continuation of the previous model across blocks, and the decoder over the
//! carry-less range coder. Decoded output is identical to RARLAB unrar's.
