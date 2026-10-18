# Contributing

Issues and pull requests are welcome. Before a pull request:

```sh
samples/fetch.sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p stream3dm --all-targets --no-default-features -- -D warnings
cargo test --workspace --release
cargo test -p stream3dm --release --no-default-features
RUSTDOCFLAGS="-D warnings" cargo doc -p stream3dm --no-deps
node --test js/test/stream3dm.test.mjs  # after cargo build --release -p stream3dm-wasm --target wasm32-unknown-unknown
```

- Every public item carries documentation (`#![warn(missing_docs)]`); comments say what the code
  does and how, briefly.
- Reader changes come with a test on a sample file. A change to a report needs its golden file
  updated (`UPDATE_GOLDEN=1 cargo test`) and the diff explained in the pull request.
- The reader must never panic on malformed input: it returns an `InspectError` or a diagnostic.
- Sample files stay out of the repository. Public ones are added to `samples/manifest.tsv` with
  their upstream commit and SHA-256; private models are never committed, nor named in issues,
  tests or commit messages, and figures measured on them carry nothing that identifies them. Synthetic fixtures come from `tools/fixtures/`.
- Commit messages: a subject line and a body saying what changed and why it is right.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
the work by you, as defined in the Apache-2.0 license, shall be dual licensed as in the README,
without any additional terms or conditions.
