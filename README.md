# Code Review Assistant

A local, dense, keyboard-first GUI (Rust + [egui](https://github.com/emilk/egui)) that helps a
human review **AI-generated code comments** one at a time. Models like Claude Opus 5 tend to
generate comments that restate the code or bury the point; this tool walks you through every
comment a branch/PR introduced and asks three reviewer models whether each one should be
**kept, rewritten, or deleted** — then leaves the final call (and the final wording) to you.

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

## Flow

```
Pick Repository  →  Pick Branch / PR (or working tree)  →  Start or Pick File  →  Review
```

1. **Pick repository** — recent repos are remembered; add any path.
2. **Pick branch / PR** — local branches (via `git`) or open PRs (via your authenticated `gh`
   CLI). Selecting one checks it out and diffs it against its base (`base...HEAD`). You can
   also review uncommitted working-tree changes.
3. **Start or pick file** — every file with reviewable comment hunks is listed with counts;
   start from the top or jump to a file.
4. **Review** — one comment at a time:
   - the surrounding hunk is shown with the comment highlighted, so you know what it describes;
   - three candidates (default: `claude`, `codex`, `agy`/Antigravity — fully customizable)
     each return **keep / rewrite / delete** plus a one-sentence justification;
   - pick a candidate (or keep/delete yourself), then edit the final text freely in the text box;
   - **Save and Continue** (`Ctrl+S`) writes the working tree; **Commit and Continue**
     (`Ctrl+Enter`) also commits that file with provenance metadata;
   - progress bars track the current file and the whole branch.

## Model CLIs

Reviewer models run through the CLIs you already have installed and authenticated. Command
templates are configured in Settings (`Ctrl+,`), tokenized on whitespace — `{prompt}` is
replaced with the prompt; without it the prompt is piped to stdin. No shell is involved.

Each CLI is started **in the repository under review**, with the flags that let it read that
repository and nothing else:

| slot | shipped template |
| --- | --- |
| claude | `claude -p --session-id {session} --tools Read,Grep,Glob --allowed-tools Read,Grep,Glob` |
| codex | `codex exec --skip-git-repo-check --json --sandbox read-only` |
| agy | `agy --gemini_dir={cli_home} -p {prompt} --output-format json --mode plan --add-dir {repo}` |

The three CLIs spell "read, don't write" differently. claude takes a tool allowlist and codex a
sandbox mode, both applying to the directory the process starts in. agy scopes access to a
*workspace* rather than a working directory, and reading inside that workspace needs no
approval — so it is handed the repository with `--add-dir {repo}`, which is what makes its own
read and search tools work at all. Plan mode keeps it off the working tree, because a workspace
is writable by default and applying edits is this app's job.

agy takes its permissions from a settings file rather than from flags, and there is no way to
pass them per invocation. Writing yours would re-govern every agy session on the machine, so it
is given a home of its own instead — `--gemini_dir={cli_home}`, pointed at a directory beside
this app's database and containing only these rules:

```json
{
  "enableTerminalSandbox": true,
  "permissions": {
    "allow": ["command(*)", "read_file(<the repository under review>)"],
    "deny":  ["unsandboxed(*)", "write_file(*)", "read_url(*)", "execute_url(*)"]
  }
}
```

Read this repository, run commands only inside the sandbox, write nothing, reach no URLs. Your
own `~/.gemini` is never touched, and authentication still works because agy reads it from the
OS keyring. The read grant names one path, so the file is rewritten when you review a different
repository.

Templates are yours to edit. `{prompt}`, `{session}`, `{repo}` and `{cli_home}` are substituted;
the last two are replaced inside their argument, so a path with spaces survives.

### What each one can actually reach

All three read files they were not pointed at, and find definitions they were not given a path
to. They differ at the edge:

| | reads | searches | shell / git history | when a tool is refused |
| --- | --- | --- | --- | --- |
| claude | `Read` | `Grep`, `Glob` | no shell; reads `.git/` directly | tool is absent, it works around it |
| codex | shell | shell (`rg`) | full read-only shell | writes denied, run continues |
| agy | native | native | **unavailable** | **the run fails** with no verdict |

agy's shell stays closed, but it now fails softly. Two measured facts shape the config above.
Rule targets match literally — `command(git)`, `command(git log)`, `command(git log*)` and
anchored regexes are all refused, and only `command(*)` grants anything — so containment is
meant to come from the sandbox, not from enumerating commands. And the sandbox does not engage
in print mode: the `--sandbox` flag asks for an admin escalation nobody can answer and the run
dies with "context canceled", while `enableTerminalSandbox` leaves commands classified
unsandboxed. So `unsandboxed(*)` denies every command in practice, which is the intended
reading either way and upgrades by itself if that ever changes.

What this buys is the failure mode. A command refused by a *rule* is something the model routes
around — it answers from files instead, exactly as claude does — where an unanswered permission
*prompt* ends the run with no verdict at all. The git-history question that used to kill the
slot now comes back answered.

The prompt itself stays short — the file, the surrounding code with the comment marked, a note
that the repository is readable and the path is relative to its root, and a request for a JSON
verdict. The models are assumed to already know what a good comment looks like; what they
cannot know from the hunk is the code around it, so they are given the means to go and look.
Browsing costs time: the model timeout defaults to 300s.

## Commit provenance

Commits made by **Commit and Continue** carry metadata about the app and where the final
text came from:

```
review(comments): rewrite comment in src/lib.rs:42

<model justification, when a candidate was picked>

Reviewed-with: code-review-assistant
Comment-provenance: claude | claude+human-edited | human-authored
Co-authored-by: Claude <noreply@anthropic.com>     (when a co-author identity is configured)
```

If you edit a picked suggestion, provenance becomes `<model>+human-edited` (co-author kept);
if you write the comment yourself, it's `human-authored` with no co-author trailer.

## Storage

All activity — sessions, every model suggestion (with latency and errors), every human
decision, every commit — plus settings live in a local SQLite database at
`~/.local/share/code-review-assistant/cra.db` (platform data dir; override with `CRA_DB`).

## Hotkeys

Every action has one; the bottom bar always shows what's live. Highlights:

| Context | Keys |
|---|---|
| Everywhere | `Ctrl+,` settings · `Ctrl+Q` quit · `Esc` back |
| Pickers | `↑/↓` select · `Enter` open · `Tab` branches⇄PRs · `W` working tree · `R` refresh |
| Files | `Enter` start at file · `S` start full review |
| Review | `1/2/3` pick candidate · `K` keep · `D` delete · `E` edit · `R` re-run models · `P` prev · `N` skip |
| Continue | `Ctrl+S` save + continue · `Ctrl+Enter` commit + continue |

## Build & run

```sh
cargo run --release
```

Requires `git` on PATH; `gh` (authenticated) for the PR picker; and whichever model CLIs you
configure. `cargo test` runs the unit tests (diff parsing, comment extraction, provenance,
edit application, model-output parsing).

Run the same checks used in pull requests with:

```console
cargo fmt --all --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
```

## Scope notes (v1)

- Reviewable units are runs of **whole-line** comments (line or block style) that the diff
  added or touched; trailing comments sharing a line with code are not yet extracted.
- Language coverage is extension-based (Rust, C/C++, JS/TS, Python, Go, Java/Kotlin, shell,
  SQL, HTML/XML, and more — see `src/comments.rs`).
- Edits apply to the working tree of the checked-out branch; the app verifies the on-disk
  lines still match the diff before touching a file.
