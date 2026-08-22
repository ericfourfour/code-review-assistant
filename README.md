# Code Review Assistant

A local, dense, keyboard-first GUI (Rust + [egui](https://github.com/emilk/egui)) that helps a
human review **AI-generated code comments** one at a time. Models like Claude Opus 5 tend to
generate comments that restate the code or bury the point; this tool walks you through every
comment a branch/PR introduced and asks three reviewer models whether each one should be
**kept, rewritten, or deleted** — then leaves the final call (and the final wording) to you.

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

Defaults: `claude -p {prompt}`, `codex exec {prompt}`, `agy -p {prompt}`.

Prompts are deliberately **minimalist** — the file, the surrounding code with the comment
marked, and a request for a JSON verdict. The models are assumed to already know what a good
comment looks like; we don't lecture them about it.

## Commit provenance

Commits made by **Commit and Continue** carry metadata about the app and where the final
text came from:

```
review(comments): rewrite comment in src/lib.rs:42

<model justification, when a candidate was picked>

Reviewed-with: code-review-assistant
Comment-provenance: claude | claude+human-edited | human-authored
Co-authored-by: Claude <noreply@anthropic.com>     (when a model's suggestion was picked)
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

## Scope notes (v1)

- Reviewable units are runs of **whole-line** comments (line or block style) that the diff
  added or touched; trailing comments sharing a line with code are not yet extracted.
- Language coverage is extension-based (Rust, C/C++, JS/TS, Python, Go, Java/Kotlin, shell,
  SQL, HTML/XML, and more — see `src/comments.rs`).
- Edits apply to the working tree of the checked-out branch; the app verifies the on-disk
  lines still match the diff before touching a file.
