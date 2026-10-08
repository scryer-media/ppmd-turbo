//! The 7z framing.
//!
//! Will hold the `PPMD` method as `.7z` carries it: the five-byte properties
//! (order, then little-endian memory size), and the stream decoder and encoder
//! over 7-Zip's range coder. Encoder output is byte-identical to 7-Zip's for
//! the same order and memory size.
