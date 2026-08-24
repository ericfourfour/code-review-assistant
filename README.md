# Code Review Assistant

A local assistant for reviewing code changes.

## Installation

Download the archive for your system from the [latest release](https://github.com/ericfourfour/code-review-assistant/releases/latest):

| System | Release asset |
| --- | --- |
| Linux x86_64 | `code-review-assistant-TAG-linux-x86_64.tar.gz` |
| Windows x86_64 | `code-review-assistant-TAG-windows-x86_64.zip` |
| macOS Apple Silicon | `code-review-assistant-TAG-macos-arm64.tar.gz` |

Extract the archive, then run `code-review-assistant.exe` on Windows or `code-review-assistant` on Linux and macOS. On Linux and macOS, you can run it from a terminal with:

```console
./code-review-assistant
```

The macOS executable is not code-signed, so its first launch may require approval in **System Settings → Privacy & Security**.

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
