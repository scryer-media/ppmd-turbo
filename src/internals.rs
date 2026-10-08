//! The crate's engine, for its own fuzz targets, benches and differential
//! tests. Not part of the public API: nothing here is stable, and none of it
//! is needed to decode or encode a stream.

pub use crate::model::Model;
pub use crate::params::{carryless_margin, sevenz_margin};
pub use crate::rc::{
    CarrylessEncoderRegs, CarrylessRangeDecoder, CarrylessRangeEncoder, Drain, IntoRangeInput,
    Pending, RangeCoderState, RangeDecoder, RangeEncoder, RangeInput, RangeOutput, RarRangeDecoder,
    SevenZipDecoderRegs, SevenZipEncoderRegs, SevenZipRangeDecoder, SevenZipRangeEncoder,
    SliceInput,
};
