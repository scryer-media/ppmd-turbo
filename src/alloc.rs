//! The sub-allocator.
//!
//! Will hold variant H's unit allocator: one contiguous arena addressed by
//! 32-bit offsets, the indexed free lists over 12-byte units, the text area
//! that grows up from the base and the unit area that grows down, and the
//! glue/split passes. Its layout is part of the format: a model's behaviour
//! when the arena fills (restart) depends on it, so it is reproduced exactly.
