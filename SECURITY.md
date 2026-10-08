# Security policy

## Supported versions

Only the latest published release of `ppmd-turbo` receives fixes.

## Reporting a vulnerability

Please do not open a public issue for security problems. Report privately
through GitHub at <https://github.com/scryer-media/ppmd-turbo/security/advisories/new>
(the Security tab, "Report a vulnerability"). You will get an acknowledgement
within a few days, and a fix ships as a new release of the crate, credited to
you unless you ask otherwise.

## What counts

A decompressor reads hostile input by design. Reports of any of these are
security reports and are handled as such:

- an out-of-bounds read or write, or any other undefined behaviour, for any
  input stream or parameters;
- a panic, an infinite loop, or memory or work out of proportion to the
  declared memory size and the output produced, reachable from
  caller-supplied data or parameters;
- decoded output that differs from 7-Zip's or unrar's for the same stream, or
  7z encoder output that 7-Zip decodes differently.

Include the stream (or how it was generated), the framing (7z or RAR), the
order and memory size, the platform and the crate version.
