# Contributing

Thanks for looking. A few things before you open a pull request:

- Read [AGENTS.md](../AGENTS.md); the rules there apply to humans too.
- Credit algorithms that carry their author's name (PPMd, the carry-less
  range coder, the 7-Zip `Ppmd7` implementation) in
  [ATTRIBUTION.md](../ATTRIBUTION.md) and where they are used; general
  techniques need no credit.
- A change to a decoder or encoder keeps output bit-exact with 7-Zip and
  unrar, and a new entry point comes with a fuzz target.
- Run `cargo fmt --all`, `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings`
  and `cargo test --locked --all-features` locally.
