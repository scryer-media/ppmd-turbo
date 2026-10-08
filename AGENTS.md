# Agent and contributor rules for ppmd-turbo

These rules apply to every automated agent and every human contributor.

## What this repository is

A standalone implementation of PPMd variant H (7-Zip's "Ppmd7"): the context
model, the sub-allocator, secondary escape estimation, two range coders (RAR's
carry-less coder and 7-Zip's LZMA-style coder) and two framings (RAR's block
API and 7z's stream API), for decoding and encoding. It is not a fork of any
crate and is not kept in step with one. The goals, in order: output that is
bit-exact with RARLAB unrar and 7-Zip, and speed.

## Attribution

Attribution is mandatory. Any algorithm, data layout or technique taken from a
named person or project is credited by name in three places:

1. at the use site, in a code comment naming the person or project and, where
   there is one, the reference function or file;
2. in `docs/algorithms.md`, in the section describing it;
3. in `ATTRIBUTION.md`.

PPMd variant H is Dmitry Shkarin's design. The 7-Zip `Ppmd7` implementation
and the 7z range coder are Igor Pavlov's. Others are added as they are taken.

## Correctness rules

- **Bit-exactness.** 7z output is byte-identical to 7-Zip's for the same order
  and memory size. RAR streams decode to exactly what unrar produces. Either
  difference is a bug, whatever the reason.
- **No panics on any input.** Malformed, truncated or hostile input returns an
  `Error`; it never panics, never reads or writes out of bounds and never does
  unbounded work.
- **`unsafe` is allowed, under rules.** Every `unsafe` block carries a
  `// SAFETY:` comment proving it sound: the invariant it relies on and the
  check that established it. Code with `unsafe` is covered by Miri wherever
  Miri can run it; a test that Miri cannot run is ignored under `cfg(miri)`,
  never the lane.
- **Fuzzing.** Every public decoder and encoder entry point has a fuzz target
  under `fuzz/`, added in the same change as the entry point.

## Tests

- A test gives the same result on a slow, loaded or shared runner as on an
  idle workstation. No fixed sleeps, no deadlines, no timeouts and no
  elapsed-time assertions.
- Fixtures use invented names only. No real media titles, release names or
  file names in fixtures, tests or examples.

## Benchmarks

- The benchmark harness is a Go program under `bench/ppmd-turbo-bench`.
- Ratios are written reference/ours, so a ratio above 1 means ppmd-turbo is
  faster (or smaller).
- A claimed speed-up shows the median of interleaved rounds against the
  previous release, with outputs compared before timing.

## Versions and pull requests

- One pull request per version. Work for a version is folded into that
  version's single branch, not stacked as per-feature pull requests.
- The version in a pull request is the latest version published on crates.io
  plus one increment, never more. An unpublished version heading is not a
  baseline; collapse it into the next one rather than bumping again.
- Any code change bumps the crate version, `Cargo.lock` and `CHANGELOG.md` in
  the same change. `CHANGELOG.md` headings are versions only.

## Repository hygiene

- Commits are SSH-signed. Never pass `--no-gpg-sign` and never set
  `commit.gpgsign=false`. If signing fails, leave the change uncommitted.
- No `Co-Authored-By` or other attribution trailers in commit messages.
- Never run destructive git working-tree operations (`checkout --`,
  `restore`, `reset --hard`, `clean`, `stash`) in a shared checkout.
- Branch names use gitflow prefixes: `feature/…`, `bugfix/…`, `hotfix/…`.
  Worktrees go under `.worktrees/`.
- Agents do not push unless told to. The maintainer pushes.
- Do not create, edit or delete Markdown files outside the task at hand.
- `cargo fmt --all`, `cargo clippy --locked --workspace --all-targets
  --all-features -- -D warnings` and `cargo test --locked --all-features` must
  pass before a commit is proposed.
