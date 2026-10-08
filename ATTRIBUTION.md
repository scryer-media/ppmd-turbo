# Attribution

The algorithms this crate uses that conventionally carry their author's name
are listed here, with what was taken and on what terms, followed by the code
this crate was seeded from and the crate it is tested against. The same
credit also appears in a short comment where the algorithm is used.

## Dmitry Shkarin: PPMd variant H

The PPMd variant H design: the context model and its update rules, binary
contexts, the sub-allocator, secondary escape estimation (SEE) and information
inheritance, and the variant H encoder that the encode path mirrors. Shkarin
released PPMd and its reference source into the public domain.

## Dmitry Subbotin: carry-less range coder

The carry-less range coder that Shkarin's reference and RAR's PPMd blocks use:
the decoder behind the RAR path and the encoder behind the raw carry-less
streams the crate writes for its own round-trip tests.

## Igor Pavlov: 7-Zip Ppmd7

7-Zip's implementation of variant H, `Ppmd7.c`, `Ppmd7Dec.c` and `Ppmd7Enc.c`
(the encode path follows `Ppmd7z_EncodeSymbol`), and the LZMA-style range
coder with carry propagation that the 7z `PPMD` method uses (`Ppmd7z`
decoder and encoder). Public domain. This crate's 7z streams are bit-exact
with it.

## unrar-rs (scryer-media/rarpar): the seed

The PPMd module of unrar-rs, in github.com/scryer-media/rarpar, is the seed
from which this crate's model, sub-allocator and SEE were ported. unrar-rs is
a Rust crate by the same owner as this crate, not RARLAB's C++ unrar. Its
PPMd module was written against the public variant H algorithm and 7-Zip's
`Ppmd7`, with RARLAB's unrar as a read-only behavioural reference; the
comments that cite unrar's `model.cpp` point at that reference. No RARLAB
source text is copied into this crate.

## ppmd-rust: test oracle

ppmd-rust 1.5.0 (github.com/hasenbanck/ppmd-rust, CC0-1.0 or MIT-0) is the
in-process reference in the differential tests, the fuzz targets and the
corpus generator, and the contender in the benchmarks. It is a
dev-dependency of the crate and a dependency of the fuzz harness and tools,
never a dependency of the library itself.
