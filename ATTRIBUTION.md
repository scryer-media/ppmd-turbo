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
decoder and encoder). Public domain. This crate's model is translated from
it (through ppmd-rust, below), and its 7z streams are bit-exact with it.

## unrar-rs (scryer-media/rarpar): the seed

The PPMd module of unrar-rs, in github.com/scryer-media/rarpar, is the seed
from which this crate's RAR framing (`src/rar.rs`) was extracted. unrar-rs is
a Rust crate by the same owner as this crate, not RARLAB's C++ unrar; RARLAB's
unrar was a read-only behavioural reference. No RARLAB source text is copied
into this crate. The context model is not seeded from it: an earlier model
was, and it has been replaced by the translation from ppmd-rust and 7-Zip.

## ppmd-rust: the model's source and the test oracle

The ppmd-rust crate (github.com/hasenbanck/ppmd-rust, version 1.5.0, by the
ppmd-rust authors), a Rust port of 7-Zip's `Ppmd7`. Dual-licensed CC0-1.0 OR
MIT-0. This crate's context model, sub-allocator and SEE (`src/model.rs`,
`src/model/encode.rs`, `src/alloc.rs`, `src/see.rs`) are a direct translation
of its `internal/ppmd7` module, and so of 7-Zip's `Ppmd7.c`, `Ppmd7Dec.c` and
`Ppmd7Enc.c`.

ppmd-rust is also the in-process reference in the differential tests, the
fuzz targets and the corpus generator, and the contender in the benchmarks.
It is a dev-dependency of the crate and a dependency of the fuzz harness and
tools, never a dependency of the library itself.
