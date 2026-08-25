# Code Review Assistant

A local assistant for reviewing code changes.

## Installation

Install the latest binary release for your platform:

### Linux x86_64

```sh
tmp="$(mktemp)" && curl -fsSL https://github.com/ericfourfour/code-review-assistant/releases/latest/download/code-review-assistant-linux-x86_64 -o "$tmp" && sudo mkdir -p /usr/local/bin && sudo install -m 0755 "$tmp" /usr/local/bin/code-review-assistant && rm -f "$tmp"
```

### macOS Apple Silicon

```sh
tmp="$(mktemp)" && curl -fsSL https://github.com/ericfourfour/code-review-assistant/releases/latest/download/code-review-assistant-macos-arm64 -o "$tmp" && sudo mkdir -p /usr/local/bin && sudo install -m 0755 "$tmp" /usr/local/bin/code-review-assistant && rm -f "$tmp"
```

### Windows x86_64 (PowerShell)

```powershell
$ErrorActionPreference = 'Stop'; $dir = Join-Path $env:LOCALAPPDATA 'Programs\code-review-assistant'; New-Item -ItemType Directory -Force $dir | Out-Null; Invoke-WebRequest 'https://github.com/ericfourfour/code-review-assistant/releases/latest/download/code-review-assistant-windows-x86_64.exe' -OutFile (Join-Path $dir 'code-review-assistant.exe'); $userPath = [Environment]::GetEnvironmentVariable('Path', 'User'); if (($userPath -split ';') -notcontains $dir) { [Environment]::SetEnvironmentVariable('Path', ("$userPath;$dir").Trim(';'), 'User') }; if (($env:Path -split ';') -notcontains $dir) { $env:Path = "$env:Path;$dir" }
```

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
