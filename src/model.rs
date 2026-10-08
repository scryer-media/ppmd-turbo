//! The variant H context model.
//!
//! Will hold Dmitry Shkarin's PPMd var.H model as 7-Zip implements it in
//! `Ppmd7.c`: contexts and their symbol states, binary contexts with their
//! `BinSumm` adaptive probabilities, model update and restart, and the
//! symbol decode/encode loops (`Ppmd7Dec.c` / `Ppmd7Enc.c`), generic over the
//! range coders in [`crate::rc`].
