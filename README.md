# Code Review Assistant

A local assistant for reviewing code changes.

## Development

Install the [Rust toolchain](https://www.rust-lang.org/tools/install), then run the application from the repository root:

```console
cargo run
```

Run the same checks used during development with:

```console
cargo fmt --all --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
```
