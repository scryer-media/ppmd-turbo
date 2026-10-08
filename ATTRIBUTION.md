# Attribution

The algorithms this crate uses that conventionally carry their author's name
are listed here, with what was taken and on what terms. The same credit also
appears in a short comment where the algorithm is used.

## Dmitry Shkarin: PPMd variant H

The PPMd variant H design: the context model and its update rules, binary
contexts, the sub-allocator, secondary escape estimation (SEE) and information
inheritance. Shkarin released PPMd and its reference source into the public
domain.

## Dmitry Subbotin: carry-less range coder

The carry-less range coder that Shkarin's reference and RAR's PPMd blocks use.

## Igor Pavlov: 7-Zip Ppmd7

7-Zip's implementation of variant H, `Ppmd7.c`, `Ppmd7Dec.c` and `Ppmd7Enc.c`,
and the LZMA-style range coder with carry propagation that the 7z `PPMD`
method uses. Public domain. This crate's model is translated from it (through
ppmd-rust, below), and its 7z framing is bit-exact with it.

## ppmd-rust

The ppmd-rust crate (github.com/hasenbanck/ppmd-rust, version 1.5.0, by the
ppmd-rust authors), a Rust port of 7-Zip's `Ppmd7`. Dual-licensed CC0-1.0 OR
MIT-0. This crate's context model, sub-allocator and SEE (`src/model.rs`,
`src/model/encode.rs`, `src/alloc.rs`, `src/see.rs`) are a direct translation
of its `internal/ppmd7` module, and so of 7-Zip's `Ppmd7.c`, `Ppmd7Dec.c` and
`Ppmd7Enc.c`.

## Eugene Roshal: RAR

RAR's integration of PPMd variant H (block headers, model continuation across
blocks, the escape character) and its use of the carry-less coder. Used as a
reference for behaviour only; no RAR or unrar code is copied.

## unrar-rs (scryer-media/rarpar)

The PPMd module of unrar-rs, in github.com/scryer-media/rarpar, is the seed
from which this crate's RAR framing (`src/rar.rs`) was extracted. Same owner
as this crate. The context model is not seeded from it: an earlier model was,
and it has been replaced by the translation from ppmd-rust and 7-Zip above.
