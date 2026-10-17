---
name: open-pr
description: Open, push to, or report on a pull request in ppmd-turbo. Runs the preflight a mergeable PR needs (one PR per version, the version bump, a dated changelog heading, README, fixtures, lockfile, fmt, clippy, tests, semver-checks, package) and writes the PR body. Use whenever a PR is to be opened, pushed, updated or called ready.
---

# Opening a pull request in ppmd-turbo

Write every PR as if it were already released. The moment it merges, `main`
must release by dispatching the `release` workflow with nothing else to edit:
the workflow releases the version in `Cargo.toml` and refuses anything below.
Each step here exists because skipping it has already failed a release.

## 1. One PR per version

- `gh pr list --repo scryer-media/ppmd-turbo --state open`. If a PR for the
  next version is open, this work goes onto its branch as more commits. Never
  open a second PR for the same version, and never open a PR whose base is
  another PR's branch.
- Branches use `feature/…`, `bugfix/…` or `hotfix/…`, cut from `main`, in a
  worktree under `.worktrees/`.
- Open the PR only when the maintainer asked for one. Agents do not push
  otherwise.

## 2. The version

- Published is what crates.io says, nothing else: not a changelog heading, not
  a tag, not a review bot.
  `curl -s https://crates.io/api/v1/crates/ppmd-turbo | jq -r .crate.max_version`
- The PR's version is published plus one step. If `main` already carries an
  unpublished version above published, use it; never bump twice before a
  release.
- Which step: run `cargo semver-checks -p ppmd-turbo` against the published
  baseline. A breaking change on 0.x is a minor bump (0.2.0 → 0.3.0);
  anything else is a patch bump.
- Any code change needs the bump. Set `version` in `Cargo.toml`, then
  `cargo update -p ppmd-turbo --offline` and confirm
  `cargo metadata --locked --format-version 1 >/dev/null` passes.

## 3. Changelog

- The section heading is `## <version> - <YYYY-MM-DD>`, dated the day the PR
  is opened. Headings are versions only: never `Unreleased`, which the
  release workflow refuses.
- `grep -n -i unreleased CHANGELOG.md` must print nothing.
- Every change in the PR has a bullet under that one heading. A later commit on
  the same PR adds bullets; it does not add a heading.

## 4. README and docs

- A dependency line added to the README names the current major.minor.
- A new or renamed entry point, feature, parameter or error kind is reflected
  in the README and in `docs/` in the same PR. A new public decoder or encoder
  entry point also gets its fuzz target under `fuzz/` in the same PR.

## 5. Fixtures

No binary file is committed. A changed fixture is a changed recipe
(`tests/fixtures/manifest.json`, `fuzz/src/seeds.rs`); regenerate and refresh
the digest list:

```sh
cargo run --locked --release -p ppmd-corpus -- fixtures --write-digests
```

## 6. Local gates

Run what CI runs, with `--locked`, before pushing:

```sh
cargo fmt --all --check
cargo fmt --all --check --manifest-path fuzz/Cargo.toml
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo clippy --locked --all-targets -- -D warnings
cargo clippy --locked --manifest-path fuzz/Cargo.toml --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --all-features
cargo run --locked --release -p ppmd-corpus -- fixtures
cargo nextest run --locked --workspace --all-features --no-fail-fast
cargo test --doc --locked --workspace --all-features --no-fail-fast
cargo nextest run --locked --no-fail-fast
cargo semver-checks -p ppmd-turbo
cargo package --locked -p ppmd-turbo
```

- A change to `unsafe` code runs its Miri and sanitizer selectors from
  `docs/unsafe-validation.md`, and that inventory is kept current.
- Keep the hooks on: `git config core.hooksPath .githooks`.

## 7. Commit

- Every commit is SSH-signed. If signing fails, stop and leave the change
  staged; never `--no-gpg-sign`.
- No `Co-Authored-By` or other attribution trailers.
- Conventional subject: `feat(scope): …`, `fix(scope): …`, `!` for breaking.

## 8. PR title and body

Title: the conventional subject plus the version, for example
`fix(rar): a decode with no block started does not poison the decoder (0.2.1)`.

Body sections, in this order, leaving out any that would be empty:

- **What**: one paragraph.
- **Why**: the problem it fixes or the consumer that needs it.
- **Changes**: grouped by area.
- **Breaking changes**: what a consumer must change, if anything.
- **Gates**: each command from step 6 and its result.
- **Follow-ups**: work left for later. Follow-ups live in the body; do not
  open GitHub issues without the maintainer's approval.

## 9. After every push

- Do not enable auto-merge unless the maintainer asks.
- Read the review comments:
  `gh api repos/scryer-media/ppmd-turbo/pulls/<n>/comments --paginate`.
  Fix each finding on this branch, or reply with the reason it is wrong, then
  resolve the thread.
- A code-scanning alert on the PR is fixed, or dismissed with a written reason
  when it is a false positive.
- The PR is ready only when every job, `ci-complete` included, is green on the
  head and every correctness, safety or version finding is fixed and replied
  to. Pure wording nits may be deferred with a reply.

## 10. After merge

`ci-complete` must be green on the merge commit on `main`. Then Actions →
`release` → Run workflow on `main`, `dry_run` on, then again with it off.
