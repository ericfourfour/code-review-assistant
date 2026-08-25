# Code Review Assistant

A local, dense, keyboard-first GUI (Rust + [egui](https://github.com/emilk/egui)) that helps a
human review **AI-generated changes** one unit at a time — the comments a branch introduced
*and* the code around them. Models like Claude Opus 5 tend to generate comments that restate
the code or bury the point, and code that is plausible without being right; this tool walks
you through every reviewable unit a branch/PR introduced, asks three reviewer models for a
verdict on each, and leaves the final call (and the final text) to you.

Two kinds of unit share one walk:

- **Comment units** — runs of whole-line comments the diff touched. Verdicts:
  **keep / rewrite / delete**.
- **Code units** — clusters of changed code lines. Verdicts: **approve / revise / flag /
  delete**, where *revise* comes with replacement lines and *flag* is a concern the unit's
  own lines cannot fix (the justification is the payload; nothing is edited).

Either kind can be switched off in Settings; both are on by default. A comment run sitting
*between* changed code lines is judged with that code rather than separately — units in a
file stay disjoint and ordered so sequential edits land exactly where they should.

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
3. **Start or pick file** — every file with reviewable units is listed with comment and code
   counts; start from the top or jump to a file.
4. **Review** — one unit at a time:
   - the excerpt shows the unit in context: `>` marks the lines under review, `+` other lines
     the branch added, `-` lines it removed, interleaved where they were removed;
   - three candidates (default: `claude`, `codex`, `agy`/Antigravity — fully customizable)
     each return a verdict plus a one-sentence justification and the **evidence** they read;
   - pick a candidate (or decide yourself), then edit the final text freely in the text box;
   - **Save and Continue** (`Ctrl+S`) writes the working tree (and runs your validation
     command, if configured); **Commit and Continue** (`Ctrl+Enter`) also commits that file
     with provenance metadata;
   - progress bars track the current file and the whole branch.

## Semantic units and hunk units

A code unit's editable region is the tight cluster of changed lines, but its *context* is as
wide as can be justified. When the language has a bundled grammar, the excerpt is the
**whole enclosing function or class** — a change judged against the definition it actually
lives in, with the scope named in the unit badge (`code · fn main()`). Everywhere else —
unknown extensions, config files, languages without a grammar, scopes over ~240 lines —
the excerpt falls back to the surrounding **hunk**, which needs nothing but the diff. Both
kinds are reviewable; semantic is simply preferred when possible so the context is relevant
rather than merely nearby.

Semantic structure comes from **real parsers**: tree-sitter grammars (bundled for Rust,
Python, JS/TS/TSX, Go, Java, C, C++, C#, PHP) answer both "which definition encloses this
line" and "where do definitions begin" — a fixture string containing `"fn main() {"` is a
string to the grammar, not a function. A language without a bundled grammar (Swift, Kotlin,
Scala, …) degrades to hunk context and blank-line windows — never to a wrong edit, because
every write re-verifies the on-disk lines first. The grammars compile as C code at build
time, so building needs a C compiler alongside the Rust toolchain.

## Seeing what the models read

The models are told to browse the repository before judging, and their JSON verdict includes
an `evidence` list: the files and line ranges they actually read, with a note on why each
mattered. Every candidate card shows its evidence as clickable chips — clicking one opens the
**real file at that spot** (with margin around the named range), not the model's paraphrase of
it. A model that misreports a path is caught by the same viewer: the failure to open it is
shown, which is itself worth knowing when weighing the verdict. Evidence is stored alongside
every suggestion in the database.

## Triage: riskiest first

The walk visits units riskiest-first by default (Settings → walk order turns it back to diff
order). Risk is a **local, deterministic heuristic** — deliberately not a model call, which
would stall plan building and add a failure mode to the one step that must always work. The
score (0–100) adds up inspectable signals: code outranks comments, size, removed lines, a
missing enclosing scope, and vocabulary buckets for the things that go wrong (secrets and
auth, locks and threads, `unsafe`/`unwrap`/`panic!`, subprocesses and SQL, `TODO`/`FIXME`
markers); test files are halved and documentation files cut to a third — prose *about* locks
and secrets is not the same risk as code that takes them (dogfooding this tool on its own
branch put the README at risk 100 before that rule existed). The review screen shows each
unit's score with the reasons
on hover, and the file picker shows each file's peak risk. It orders attention — the models
still judge every unit on its merits when it is reached, and per-edit line offsets are
tracked individually so an out-of-order walk edits exactly as safely as a linear one.

## The branch pass: cross-cutting findings

Every unit is judged in isolation, which is exactly what a per-unit pass cannot see past.
When the walk finishes, the summary screen offers a **branch pass** (`G`): each enabled model
gets the branch's full diff (truncated past ~60k characters, with the seam marked) and the
run of the repository, and reports only cross-cutting findings — hunks that contradict each
other, half-applied renames, code left dead, the test or doc a change obviously needs,
new logic that duplicates something that already exists. Findings come back with severity,
affected files, and the same clickable evidence chips as unit verdicts; they are recorded in
the database, sorted high-severity-first for human triage, dismissable one by one (the
dismissal is recorded too, not deleted), and exportable as markdown for a PR description.
Nothing is ever edited by this pass — an empty list is an acceptable answer, and the prompt
says so.

## Per-edit validation

Settings takes a **check command** (e.g. `cargo check`, `tsc --noEmit`, `go build ./...`) run
in the repository after every applied edit, whitespace-tokenized with no shell. If the check
fails, the edit is **reverted on the spot** and the command's own output is shown — a bad
model rewrite can never walk the review onto a broken tree. Code edits are always validated
when a command is set; comment-only edits opt in with their own toggle (they rarely break a
build, and checks cost time). The check runs synchronously, so pick a fast one; the timeout
is configurable.

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

The prompt itself stays short — the file, the excerpt with the unit marked, a note that the
repository is readable and the path is relative to its root, and a request for a JSON verdict
(with its evidence list). The models are assumed to already know what good comments and good
code look like; what they cannot know from the excerpt is the code around it, so they are
given the means to go and look. Browsing costs time: the model timeout defaults to 300s.

## Commit provenance

Commits made by **Commit and Continue** carry metadata about the app and where the final
text came from:

```
review(comments): rewrite comment in src/lib.rs:42     (or: review(code): revise code in …)

<model justification, when a candidate was picked>

Reviewed-with: code-review-assistant
Comment-provenance: claude | claude+human-edited | human-authored   (code: Change-provenance)
Co-authored-by: Claude <noreply@anthropic.com>     (when a co-author identity is configured)
```

A batch that mixes comment and code decisions commits as `review: N decisions in <file>`,
with each decision's own verb and justification listed in the body. Flags never commit —
nothing changed — but they are recorded in the database like any other decision, attributed
to the model that raised them.

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
| Review | `1/2/3` pick candidate · `K` keep/approve · `D` delete · `E` edit · `R` re-run models · `P` prev · `N` skip |
| Continue | `Ctrl+S` save + continue · `Ctrl+Enter` commit + continue |
| Summary | `G` run branch pass · `F` files · `B` branches/PRs |

## Build & run

```sh
cargo run --release
```

Requires `git` on PATH; `gh` (authenticated) for the PR picker; and whichever model CLIs you
configure. `cargo test` runs the unit tests (diff parsing, comment and code-unit extraction,
scope detection, provenance, edit application and revert, validation, model-output parsing).

Run the same checks used in pull requests with:

```console
cargo fmt --all --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
```

## Scope notes

- Comment units are runs of **whole-line** comments (line or block style) that the diff added
  or touched; trailing comments sharing a line with code are not yet extracted. Comment
  language coverage is extension-based (Rust, C/C++, JS/TS, Python, Go, Java/Kotlin, shell,
  SQL, HTML/XML, and more — see `src/comments.rs`).
- Code units cover every changed cluster of code, including pure removals (anchored to the
  line the removal followed, with the removed lines shown in context). Files with unknown
  extensions still get hunk units — a `.conf` change is still a change. A cluster longer
  than ~120 lines (a brand-new large file, typically) is split into consecutive windows, and
  the cuts are semantic first: at the tops of the functions and classes the region spans,
  descending into oversized containers so an `impl` splits at its methods and a class at its
  defs. Blank lines are only the fallback — inside a definition that is itself too long, and
  in files the scope heuristics cannot read.
- A cross-cutting concern that spans units is what **flag** is for: the concern is recorded
  and attributed even though no line in the unit moves.
- Edits apply to the working tree of the checked-out branch; the app verifies the on-disk
  lines still match the diff before touching a file, and reverts any edit the configured
  check command rejects.
