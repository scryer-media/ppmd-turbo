# Contributing

Thanks for looking. A few things before you open a pull request:

- Read [AGENTS.md](../AGENTS.md); the rules there apply to humans too.
- Credit what you take. An algorithm or technique from a named person or
  project is credited at the use site, in `docs/algorithms.md` and in
  [ATTRIBUTION.md](../ATTRIBUTION.md).
- A change to a decoder or encoder keeps output bit-exact with 7-Zip and
  unrar, and a new entry point comes with a fuzz target.
- Run `cargo fmt --all`, `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings`
  and `cargo test --locked --all-features` locally.
