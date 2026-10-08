# Attribution

Every algorithm, data layout or technique this crate takes from a named person
or project is listed here, with what was taken and on what terms. The same
credit appears at the use site in the code and in `docs/algorithms.md`.

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
method uses. Public domain. This crate's 7z framing is bit-exact with it.

## Eugene Roshal: RAR

RAR's integration of PPMd variant H (block headers, model continuation across
blocks, the escape character) and its use of the carry-less coder. Used as a
reference for behaviour only; no RAR or unrar code is copied.

## unrar-rs (scryer-media/rarpar)

The PPMd module of unrar-rs, in github.com/scryer-media/rarpar, is the seed
from which this crate's RAR decoding path is extracted. Same owner as this
crate.
