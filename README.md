# Code Review Assistant

A local, dense, keyboard-first GUI (Rust + [egui](https://github.com/emilk/egui)) that helps a
human review **AI-generated changes** one unit at a time — the comments a branch introduced
*and* the code around them. Models like Claude Opus 5 tend to generate comments that restate
the code or bury the point, and code that is plausible without being right; this tool reviews
you through every reviewable unit a branch/PR introduced, asks three reviewer models for a
verdict on each, and leaves the final call (and the final text) to you.

Two kinds of unit share one review:

- **Comment units** — runs of whole-line comments the diff touched. Verdicts:
  **keep / rewrite / delete**.
- **Code units** — clusters of changed code lines. Verdicts: **approve / revise / flag /
  delete**, where *revise* comes with replacement lines and *flag* is a concern the unit's
own lines cannot fix (the justification describes the finding; nothing is edited).

Either kind can be switched off in Settings; both are on by default. A comment run sitting
*between* changed code lines is judged with that code rather than separately — units in a
file stay disjoint and ordered so sequential edits are applied exactly where they should be.

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
Pick Repository  →  Pick Branch / PR (or working tree / staged)  →  Start or Pick File  →  Review
```

1. **Pick repository** — recent repos are remembered; add any path. The picker also discovers
   repositories on its own: a background scan of your home folder for local clones, plus
   `gh repo list` for your GitHub account, streamed in as they are found and cached for an
   hour (refresh on demand). A repository that only exists on GitHub is cloned when opened.
   Repositories inactive past a configurable cutoff are hidden, and any repository can be
   excluded from the list — both editable in settings.
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
   - one continuation button (`Ctrl+S`) writes the working tree and runs your validation
     command, if configured — the **Commit each decision individually** checkbox decides
     whether it also commits that file with provenance metadata, and retitles the button
     **Save and Continue** or **Commit and Continue** to match;
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

The same grammars colour the code. Every excerpt on the review screen — the context pane,
the original, the editable final text, the candidate previews, the evidence viewer — is
syntax-highlighted from each grammar's own `highlights.scm`, so there is no second idea of
what a keyword or a string is. Nothing wraps: indentation is how code says what encloses
what, so long lines scroll sideways instead of losing their leading space.

A code candidate proposing **REVISE** previews as a diff against the original rather than as
a fresh block of code — the verdict is a claim about what should change, and a block of code
leaves the reader to find that change by hand. Comment rewrites still preview as plain
replacement text: prose rewords wholesale, so a line diff there is only the old wording
restated above the new. Configured models are coloured from a deliberately unbranded palette, and
by *settings position* rather than by model identity — blinding shuffles the models, so a
colour that tracked identity would give the answer away.

## Seeing what the models read

The models are told to browse the repository before judging, and their JSON verdict includes
an `evidence` list: the files and line ranges they actually read, with a note on why each
mattered. Every candidate card shows its evidence as clickable chips — clicking one opens the
**real file at that spot** (with margin around the named range), not the model's paraphrase of
it. A model that misreports a path is caught by the same viewer: the failure to open it is
shown, which is itself worth knowing when weighing the verdict. Evidence is stored alongside
every suggestion in the database.

## Triage: riskiest first

The review visits units riskiest-first by default (Settings → review order turns it back to diff
order). Risk is a **local, deterministic heuristic** — deliberately not a model call, which
would stall plan building and add a failure mode to the one step that must always work. The
score (0–100) adds up inspectable signals: code outranks comments, size, removed lines, a
missing enclosing scope, and risk rules for the things that go wrong (secrets and
auth, locks and threads, `unsafe`/`unwrap`/`panic!`, subprocesses and SQL, `TODO`/`FIXME`
markers); test files are halved and documentation files cut to a third — prose *about* locks
and secrets is not the same risk as code that takes them (dogfooding this tool on its own
branch put the README at risk 100 before that rule existed). The review screen shows each
unit's score with the reasons
on hover, and the file picker shows each file's peak risk. It orders attention — the models
still judge every unit on its merits when it is reached, and per-edit line offsets are
tracked individually so an out-of-order review edits exactly as safely as a linear one.

## Prefetch, and disagreement first

Deciding a unit takes minutes; the models answer in seconds. So while you decide, the next
unit's models are already being queried (Settings → prefetch), and by the time you advance
the verdicts are usually waiting — the review stops paying the model latency between units.
Answers that arrive for a unit you never reach are still recorded and billed honestly.

Prefetching also buys a second kind of triage: when every model says **keep** about the
upcoming unit, it is pushed to the end of its file (Settings → defer keeps), so the units the
models *disagree* about — the ones history says are worth your attention — are reviewed
first. Deferred units are still reviewed, last, from their stored answers; each unit
is deferred at most once, so the review always terminates.

## Standing preferences

Every follow-up question you type is recorded in full (`follow_ups` table), and each answer
links back to the exact question it replied to. The recent ones — your own words about what
the models got wrong — are distilled into a preference preamble sent with every review
prompt (Settings → preferences), together with your keep/rewrite/delete mix once there are
enough verdicts to mean something. The observed pattern this attacks: models open with a
polite unanimous "keep", the reviewer pushes back ("say *why*, not *what*"), and the
verdicts flip — a correction that used to be paid for again on every unit, one follow-up
round at a time. The preamble moves it to round one.

## Decisions stick

A unit you have already ruled on does not come back. When a plan is built, every unit the
diff offers is matched against this repository's decision history and dropped if it is
already answered, so reopening the app resumes a review instead of restarting it. Both sides
of a decision count as answered: the text that was judged, and the text the verdict left
behind — otherwise a rewrite you accepted returns as a brand-new unit the moment it is
committed, and the same comment is reviewed forever.

Matching is by **file and exact text**, never by line number: edits above a unit move it down
the file, and its verdict moves with it. Keep, rewrite, delete and flag all count — all four
are you having looked. **Skipping** a unit during a review records nothing, so a skip comes
back next time; that is the difference between "not now" and "decided". History is scoped per
repository, so two checkouts with identical boilerplate are two separate jobs.

The file picker says how many units were held back, so a short plan reads as *the rest is
done* rather than *the extractor lost things*, and a branch with nothing left says so by
name. Settings → **skip decided** turns it off to review everything again, and the ref picker's
**re-check** (`C`) deliberately re-judges past decisions to measure how consistent a reviewer
is with their own earlier self.

## Whole-branch review: cross-cutting findings

Every unit is judged in isolation, which cannot expose interactions between separate changes.
When the review finishes, the summary screen offers a **whole-branch review** (`G`): each enabled model
gets the branch's full diff (truncated past ~60k characters, with the seam marked) and the
run of the repository, and reports only cross-cutting findings — hunks that contradict each
other, half-applied renames, code left dead, the test or doc a change obviously needs,
new logic that duplicates something that already exists. Findings come back with severity,
affected files, and the same clickable evidence chips as unit verdicts; they are recorded in
the database, sorted high-severity-first for human triage, dismissable one by one (the
dismissal is recorded too, not deleted), and exportable as markdown for a PR description.
Nothing is ever edited by this pass — an empty list is an acceptable answer, and the prompt
says so.

## Notes and the follow-up fix session

Sometimes a small unit reveals a large problem — a pattern repeated across files, a missing
abstraction, an error design that swallows causes — and the review screen deliberately cannot
act beyond the unit's own lines. The **NOTE** box (`C`) parks that observation instead: the
note is saved with the unit's file, lines, and the code as it stood, the unit itself still
gets decided normally, and the review goes on uninterrupted.

When the review is done (or any later day — the backlog is per-repository and survives the
session that wrote it), the **follow-up screen** (`N` from the summary or the branch/PR
picker) shows every note still open. Triage is three-way:

- **dismiss** — recorded as such, never shown again;
- **check** — this note is the next fix session's job;
- **leave unchecked** — still open, shown again on the next visit.

Then edit the prompt preamble, pick the model — "done with the review models" is exactly when
a larger one earns its keep — and **begin the fix session**: one interactive conversation,
run in the repository, whose opening prompt is your preamble plus every checked note with its
locus and review-time code. Checked notes are marked resolved the moment the session starts;
the transcript is where their fate is read, and the conversation can be continued turn by
turn (the session resumes through the same `{session}` setup as review follow-ups).
Resolved and dismissed notes never return — only what was left unchecked is offered again.

## Per-edit validation

Settings takes a **check command** (e.g. `cargo check`, `tsc --noEmit`, `go build ./...`) run
in the repository after every applied edit, whitespace-tokenized with no shell. If the check
fails, the edit is **reverted on the spot** and the command's own output is shown — a bad
model rewrite can never review the review onto a broken tree. Code edits are always validated
when a command is set; comment-only edits opt in with their own toggle (they rarely break a
build, and checks cost time). The check runs synchronously, so pick a fast one; the timeout
is configurable.

## Model CLIs

Reviewer models run through the CLIs you already have installed and authenticated. Command
templates are configured in Settings (`Ctrl+,`), tokenized on whitespace — `{prompt}` is
replaced with the prompt; without it the prompt is piped to stdin. No shell is involved.

Each CLI is started **in the repository under review**, with the flags that let it read that
repository and nothing else:

| model | shipped template |
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
*prompt* ends the run with no verdict at all. The git-history question that used to stop the
model now comes back answered.

The prompt itself stays short — the file, the excerpt with the unit marked, a note that the
repository is readable, what each verdict means, and a request for a JSON verdict (with its
evidence list). It deliberately does not tell a reviewer what to look for or in what order:
the models are assumed to know what good comments and good code look like, and what they
cannot know from the excerpt is the code around it, so they are given the means to go and
look rather than a checklist. Browsing costs time: the model timeout defaults to 300s.

## Processes, sessions and usage

A model CLI is a process this app started and a conversation the CLI goes on holding
afterwards. Both are tracked, on every page that runs one.

**The ledger** (`Ctrl+P`, or the badge in the top bar) lists every call this run has made:
state, model, which page started it, **pid**, **session id**, elapsed time, what it spent, and
the last step it was seen taking. Each live row has a stop button, and there is a stop-all.
The badge shows what is running and what is paused without opening anything.

**Leaving a page stops that page's models.** Walking off the review screen used to leave three
CLIs reading the repository for minutes, spending the whole time on a verdict nobody would
ever see, against sessions the app had already forgotten the ids of. Now navigating away
terminates them — the whole process tree, because the CLIs are npm shims and killing
`claude.cmd` leaves the node process underneath it working — and a banner names every process
by pid. It says *terminating* until each process confirms it is gone, and only then
*terminated*: the confirmation is of the kill, not of the request.

**Coming back shows what was paused, and starts nothing.** The unit's cards are where you left
them, each stopped call showing its pid, its session, how long it ran and what it spent, with
two offers:

- **Resume this session** — continues the conversation the CLI still holds, so the model keeps
  everything it had read and worked out before the kill;
- **Ask again** — a new conversation, paying for that work a second time.

Where there is no session id to continue, only the second is possible and the card says why:
a CLI that reports its id in its reply (rather than taking one this app generates, claude's
`--session-id`) leaves nothing behind when it is killed mid-answer. Returning never picks for
you — that is the whole point of pausing rather than cancelling. `R` on the review screen
still re-runs the models outright; asking again is what that key is for.

The same applies to the whole-branch review on the summary screen and to the fix session on
the follow-up screen. Quitting stops everything first: an orphaned CLI has no window left to
report itself in.

**Usage.** Every call's tokens and cost are read from the CLI's own accounting and totalled
for the run — including calls that were stopped part-way, whose spend was real. Settings takes
a **spend limit** and a **token limit** per run; past either, no new call starts and the page
says why and where to change it. Zero means no ceiling, which is the default. Silence about
spend is never counted as zero: a CLI that reports no tokens is *unmeasured*, and a limit that
stopped work on an estimate would be stopping it for the wrong reason.

A stopped call is recorded with what it spent but marked as stopped, so the evaluation page
never scores a model for a call you cut short — walking away says nothing about the model.

Sessions outlive the app that opened them: the CLI still holds the conversation after the
window closes, so each one is written to the database with its model, page, state and running
spend. Conversations an earlier run left paused are listed at the bottom of the ledger.

## Commit provenance

Commits made with **Commit each decision individually** checked carry metadata about the app and where the final
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

## Evaluation: which model you actually listen to

Every review is a blind side-by-side test that already happened. Choosing one model's
suggestion over another's is a label, and **Ctrl+E** is that pile of labels added up —
per model, over every review so far.

| Column | What it means |
|---|---|
| **won** | Decisions whose final text came from that model — the only outcome the tool exists to produce. A suggestion you picked and then reworded still counts (hover for how many); it started you off. |
| **agreed** | Same keep/rewrite/delete verdict as yours. Cheaper to earn: agreeing a comment needs rewriting says nothing about whether the rewrite was any good. |
| **offered / errors** | Contests it answered, and ones it crashed or timed out on. An error is not a loss — it never reached the table — so it has its own column instead of quietly deflating the win rate. |
| **tokens / cost** | What the CLI said the call spent. |

Below the table: **head to head**, because a leaderboard row hides who a model was up
against — two models on the same win rate are not equivalent if one was only ever in the
room with the weakest model. Then the **verdict mix**, the bias check a win rate cannot show:
a model that answers "rewrite" to everything scores well on a branch full of bad comments
and badly on a clean one. Then **spend**, over every call including the ones on units you
skipped — skipping does not refund what it cost to ask.

Two filters, both above everything they affect. **Blinded only** is on by default: a choice
made while the model names were visible measures which model you already trust as much as it
measures the suggestion. **Repository** scopes to one checkout, since a model that suits one
codebase need not suit another. `⧉ copy as markdown` takes the whole page, caveats included.

The page argues with itself on purpose. Every rate carries its denominator — `44% (20/45)`,
never a bare `44%` — and a **How much of this to believe** block sits under the tables
naming what would make the ranking wrong: too few contests, unblinded decisions in the mix,
and how often you agreed with *your own* earlier verdict, which is the ceiling no model can
be scored above. None of it is accuracy. There is no ground truth for whether a comment
earns its place; your judgement is the label, so every number says "agrees with you".

### Tokens and cost

What each call spends is read out of the CLI's own output — the result envelope or the last
event of a JSON stream, whichever that CLI prints — and stored on the suggestion row. A
running total is read once, never summed across events, so a model that took eight turns is
not billed eight times.

A CLI that reports no cost is **unmeasured, not free**: it shows a dash and is named in the
caveats rather than sitting at the top of the cost table. Give the model configuration `$/Mtok` rates in
settings to price it from its token counts; a cost the CLI reports itself always wins over
those rates. Only per-unit review calls are counted — whole-branch reviews and follow-up fix
sessions run the same CLIs and are not in these figures.

### From the command line

The same history is scoreable outside the window, and can be replayed against a model,
effort level or prompt you have not used yet:

```sh
cra export-corpus --out corpus.json     # your past verdicts, as labelled examples
cra replay --corpus corpus.json         # ask every enabled model the same questions
cra report --corpus corpus.json --results replay.json
cra report                              # or just score the whole review history
```

## Storage

All activity — sessions, every model suggestion (with latency, errors, tokens and cost),
every human decision, every commit, and every CLI conversation the app opened (with its
model, page, state and spend) — plus settings live in a local SQLite database at
`~/.local/share/code-review-assistant/cra.db` (platform data dir; override with `CRA_DB`).

## Hotkeys

Every action has one; the bottom bar always shows what's live. Highlights:

| Context | Keys |
|---|---|
| Everywhere | `Ctrl+E` model evaluation · `Ctrl+P` processes & sessions · `Ctrl+,` settings · `Ctrl+Q` quit · `Esc` back |
| Pickers | `↑/↓` select · `Enter` open · `Tab` branches⇄PRs · `W` working tree · `S` staged · `U` toggle untracked · `R` refresh |
| Files | `Enter` start at file · `S` start full review |
| Review | `1/2/3` pick candidate · `K` keep/approve · `D` delete · `E` edit · `C` note for follow-up · `R` re-run models · `P` prev · `N` skip |
| Continue | `Ctrl+S` save + continue · `Ctrl+Enter` commit + continue |
| Summary | `G` run whole-branch review · `N` follow-up notes · `F` files · `B` branches/PRs |
| Evaluation | `B` blinded decisions only · `R` refresh |

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
- Code units cover every changed group of code, including pure removals (represented by the
  line the removal followed, with the removed lines shown in context). Files with unknown
  extensions still get hunk units — a `.conf` change is still a change. A cluster covering
  more than one definition is split into one unit per definition, whatever its length: a
  whole added file arrives as its functions, structs and constants, each with its own
  verdict, rather than as one wall of code. Oversized containers are descended into, so an
  `impl` splits at its methods and a class at its defs. Blank lines are only the fallback —
  inside a single definition longer than ~120 lines, and in files no bundled grammar reads.
- A cross-cutting concern that spans units is what **flag** is for: the concern is recorded
  and attributed even though no line in the unit moves.
- Edits apply to the working tree of the checked-out branch; the app verifies the on-disk
  lines still match the diff before touching a file, and reverts any edit the configured
  check command rejects.
