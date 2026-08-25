//! Every model CLI this app starts: what it is, what session it carries, what
//! it has spent, and whether it is still running.
//!
//! Until this module existed a CLI call was fire-and-forget. A thread spawned
//! it, a channel carried the reply back, and the only thing that could stop it
//! was its own deadline. Walk off the review screen with three models reading
//! the repository and all three kept reading — minutes of tokens spent on a
//! verdict nobody would ever see, against sessions the app had already
//! forgotten the ids of. Nothing on screen said so, because nothing knew.
//!
//! So calls are tracked rather than launched. A [`ProcHandle`] is the one
//! object both ends share: the worker writes the pid into it, streams activity
//! through it, and records the session id and usage into it when the process
//! ends; the UI reads it each frame and can ask it to stop. The [`ProcTable`]
//! holds one row per call the app has made, which is what lets a page's
//! processes be addressed as a group — "stop everything the review screen
//! started" is a navigation event, not a per-call one.
//!
//! Two things here are deliberately pessimistic:
//!
//! * A stop is not believed until the process confirms it. `stop` only raises
//!   a flag; the row stays `Stopping` until the worker has killed the child,
//!   reaped it, and written the outcome back. The banner the reviewer sees is
//!   built from those outcomes, so "terminated" on screen means terminated.
//! * A kill goes after the whole tree. The model CLIs are npm shims — killing
//!   `claude.cmd` leaves the node process that is doing the work (and holding
//!   the session, and spending the money) running behind it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::models::Usage;

/// The page a call belongs to. Navigation stops a page's processes, so this is
/// the address a stop is sent to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Owner {
    /// Per-unit verdicts and follow-ups on the review screen — including the
    /// prefetch it starts for the unit after this one, which is the review's
    /// work even though no card is showing it yet.
    Review,
    /// The whole-branch review on the summary screen.
    Branch,
    /// The interactive fix session on the follow-up screen.
    Fix,
}

impl Owner {
    pub fn label(self) -> &'static str {
        match self {
            Owner::Review => "review",
            Owner::Branch => "branch review",
            Owner::Fix => "fix session",
        }
    }

    /// Every owner, so a shutdown can sweep them all without a match arm that
    /// silently misses one added later.
    pub const ALL: [Owner; 3] = [Owner::Review, Owner::Branch, Owner::Fix];
}

/// How a process stopped running.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Ended {
    /// Ran to completion on its own.
    Finished,
    /// Killed because the call outlived its deadline.
    TimedOut,
    /// Killed because we asked it to stop, with the reason a human would read.
    Cancelled(String),
    /// Never became a process at all.
    SpawnFailed,
}

impl Ended {
    pub fn label(&self) -> &'static str {
        match self {
            Ended::Finished => "finished",
            Ended::TimedOut => "timed out",
            Ended::Cancelled(_) => "terminated",
            Ended::SpawnFailed => "never started",
        }
    }

    /// Whether the process was still working when it stopped — the case where
    /// resuming its session has something left to continue.
    pub fn interrupted(&self) -> bool {
        matches!(self, Ended::Cancelled(_) | Ended::TimedOut)
    }
}

/// Where a call is in its life.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RunState {
    /// Asked for, no pid yet. Brief, but a stop pressed inside that window
    /// still has to land, so it is a state rather than an assumption.
    Starting,
    Running,
    /// A stop has been asked for and the process has not confirmed it yet.
    Stopping,
    Done(Ended),
}

impl RunState {
    pub fn is_live(&self) -> bool {
        !matches!(self, RunState::Done(_))
    }

    pub fn label(&self) -> &'static str {
        match self {
            RunState::Starting => "starting",
            RunState::Running => "running",
            RunState::Stopping => "stopping",
            RunState::Done(e) => e.label(),
        }
    }
}

/// One frame's view of a call.
pub struct ProcSnapshot {
    pub elapsed: Duration,
    /// Non-empty output lines seen so far — proof of life even when none of
    /// them parsed into anything recognisable.
    pub lines: u64,
    /// The most recent recognisable step (a tool call, a shell command), or
    /// empty when the stream has shown none yet.
    pub activity: String,
    /// The operating system's id for the process, once it has one. This is
    /// what makes a claim of termination checkable from outside the app.
    pub pid: Option<u32>,
    /// The CLI conversation this call belongs to, once it is known. For a
    /// model whose id we generate it is known before the process starts; for
    /// one that reports its own it arrives with the reply, which is why a
    /// killed call can leave none behind.
    pub session: Option<String>,
    pub usage: Usage,
    pub state: RunState,
}

impl ProcSnapshot {
    /// Elapsed against the call's deadline: "47s / 240s".
    pub fn clock(&self, timeout_secs: u64) -> String {
        format!("{}s / {timeout_secs}s", self.elapsed.as_secs())
    }

    /// The activity worth a label: the last recognisable step, else how much
    /// the CLI has printed, else nothing (it has been silent so far).
    ///
    /// `blinded` drops the step itself and leaves the count. The step names
    /// the vendor whatever it says — a tool call spelled with claude's
    /// argument names, a shell command in codex's argv style, a path only one
    /// of them would open — so on a review card with the model's identity
    /// hidden it answers the question the blinding is there to withhold. What
    /// is left is proof of life, which is all such a card needs.
    pub fn activity_line(&self, blinded: bool) -> Option<String> {
        if !blinded && !self.activity.is_empty() {
            return Some(self.activity.clone());
        }
        (self.lines > 0).then(|| format!("{} line(s) of output", self.lines))
    }

    /// The process as a human would name it.
    pub fn pid_label(&self) -> String {
        match self.pid {
            Some(pid) => format!("pid {pid}"),
            None => "no pid".to_string(),
        }
    }
}

/// The handle both ends of a call share. Cheap to clone — every clone is the
/// same allocation, which is the point: the UI's copy sees what the worker
/// writes, and the worker sees a stop the moment the UI asks for one.
#[derive(Clone)]
pub struct ProcHandle {
    inner: Arc<Mutex<Inner>>,
    /// Kept outside the mutex so the worker's poll loop can check it without
    /// ever contending with a UI thread reading a snapshot.
    cancel: Arc<AtomicBool>,
}

struct Inner {
    started: Instant,
    /// Frozen when the call ends, so a finished row stops counting up.
    ended_after: Option<Duration>,
    lines: u64,
    activity: String,
    pid: Option<u32>,
    session: Option<String>,
    usage: Usage,
    state: RunState,
    /// Why a stop was asked for, kept from the moment it is asked so the
    /// confirmation can quote it after the process is gone.
    stop_reason: String,
}

impl ProcHandle {
    pub fn new() -> ProcHandle {
        ProcHandle {
            inner: Arc::new(Mutex::new(Inner {
                started: Instant::now(),
                ended_after: None,
                lines: 0,
                activity: String::new(),
                pid: None,
                session: None,
                usage: Usage::default(),
                state: RunState::Starting,
                stop_reason: String::new(),
            })),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn snapshot(&self) -> ProcSnapshot {
        let g = self.lock();
        ProcSnapshot {
            elapsed: g.ended_after.unwrap_or_else(|| g.started.elapsed()),
            lines: g.lines,
            activity: g.activity.clone(),
            pid: g.pid,
            session: g.session.clone(),
            usage: g.usage,
            state: g.state.clone(),
        }
    }

    /// Ask the call to stop. Raises the flag the worker polls and moves the
    /// row to `Stopping`; it does not claim the process is gone, because from
    /// here it is not known to be. A repeat stop keeps the first reason: what
    /// started the shutdown is more informative than what echoed it.
    pub fn stop(&self, reason: &str) {
        {
            let mut g = self.lock();
            if !g.state.is_live() {
                return;
            }
            if g.stop_reason.is_empty() {
                g.stop_reason = reason.to_string();
            }
            g.state = RunState::Stopping;
        }
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// Whether a stop has been asked for. Polled by the worker between waits.
    pub(crate) fn stop_requested(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    pub(crate) fn stop_reason(&self) -> String {
        self.lock().stop_reason.clone()
    }

    /// The process exists; here is its id. Called the instant `spawn` returns,
    /// so a stop raised while the CLI was still starting has a pid to kill.
    pub(crate) fn started_process(&self, pid: u32) {
        let mut g = self.lock();
        g.pid = Some(pid);
        // A stop asked for during startup already moved the row to `Stopping`,
        // and the process finally appearing must not undo that.
        if g.state == RunState::Starting {
            g.state = RunState::Running;
        }
    }

    /// The session id this call runs under, as soon as it is known: before the
    /// process starts for a model whose id we generate, and from the reply for
    /// one that reports its own.
    pub(crate) fn set_session(&self, session: Option<String>) {
        if let Some(id) = session.filter(|s| !s.trim().is_empty()) {
            self.lock().session = Some(id);
        }
    }

    /// The call is over. Freezes the clock and records what it spent, which is
    /// what keeps a killed call's usage on the books instead of losing it.
    pub(crate) fn ended(&self, how: Ended, usage: Usage) {
        let mut g = self.lock();
        g.ended_after = Some(g.started.elapsed());
        g.usage = usage;
        g.state = RunState::Done(how);
    }

    /// Whether two handles are the same call. Every clone shares one
    /// allocation, so pointer identity is exactly the question.
    pub fn is(&self, other: &ProcHandle) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub(crate) fn on_line(&self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        // Parse outside the lock; the UI reads snapshots while this runs.
        let activity = live_activity(line);
        let mut g = self.lock();
        g.lines += 1;
        if let Some(a) = activity {
            g.activity = a;
        }
    }
}

impl Default for ProcHandle {
    fn default() -> Self {
        Self::new()
    }
}

/// One tracked call: the handle, plus who started it and what for. The
/// descriptive half lives here rather than in the handle because the worker
/// thread never needs it — only the ledger and the UI do.
pub struct Tracked {
    pub id: u64,
    pub owner: Owner,
    pub model: String,
    /// Index into the settings model list, so a resume can go back to the same
    /// CLI even if the picker has moved since.
    pub model_index: usize,
    /// What the call was asked to do — "src/lib.rs:42 · opening verdict".
    pub what: String,
    pub handle: ProcHandle,
    /// When the row stopped being live, so finished rows can be aged out
    /// without asking the handle every frame.
    pub done_at: Option<Instant>,
}

impl Tracked {
    pub fn snapshot(&self) -> ProcSnapshot {
        self.handle.snapshot()
    }
}

impl ProcTable {
    /// The row a handle belongs to. Used when a page has the handle in hand —
    /// on a candidate card, say — and wants what the ledger knows about it.
    pub fn row_for(&self, handle: &ProcHandle) -> Option<&Tracked> {
        self.rows.iter().find(|r| r.handle.is(handle))
    }
}

/// What a stop did to one call, as the confirmation banner reads it. Holding
/// the handle rather than a copy of its state is what makes the confirmation
/// live: a row starts as "terminating" and becomes "terminated" only when the
/// process itself says so.
#[derive(Clone)]
pub struct StopReceipt {
    pub owner: Owner,
    /// Index into the settings model list, so a caller that has to name this
    /// model under blinding can resolve the same stand-in its card uses.
    pub model_index: usize,
    pub model: String,
    pub what: String,
    /// The pid as it was when the stop was asked for. Held by value because
    /// the point of a receipt is to name the process that was killed, and that
    /// has to survive the row being swept.
    pub pid: Option<u32>,
    pub session: Option<String>,
    pub(crate) handle: ProcHandle,
}

impl StopReceipt {
    /// Whether the process has confirmed it is gone.
    pub fn confirmed(&self) -> bool {
        !self.handle.snapshot().state.is_live()
    }

    /// The line the reviewer reads: what was killed, and whether the kill has
    /// landed yet. `name` is what to call the model, which under blinding is
    /// its stand-in rather than the name in settings.
    pub fn line(&self, name: &str) -> String {
        let pid = match self.pid {
            Some(pid) => format!("pid {pid}"),
            None => "no pid (it had not started)".to_string(),
        };
        let state = if self.confirmed() {
            "terminated"
        } else {
            "terminating…"
        };
        format!("{name} · {} · {pid} {state}", self.what)
    }
}

/// How long a finished row stays in the table. Long enough to answer "what
/// just happened", short enough that a long review does not accumulate a
/// thousand rows nobody will read.
const KEEP_FINISHED: Duration = Duration::from_secs(180);
/// A floor under the age rule, so the most recent history is always there
/// however fast the calls are coming.
const KEEP_AT_LEAST: usize = 40;

/// A call that has just ended, handed to the app so the conversation it was
/// holding can be written down before the row is aged out of memory.
pub struct Completed {
    pub owner: Owner,
    pub model: String,
    pub what: String,
    pub session: Option<String>,
    pub pid: Option<u32>,
    pub usage: Usage,
    pub ended: Ended,
}

/// The ledger of every call this run of the app has made.
#[derive(Default)]
pub struct ProcTable {
    rows: Vec<Tracked>,
    next_id: u64,
    /// Everything ever recorded, rows since swept included, so the totals a
    /// limit is checked against never fall as history is aged out.
    spent: Usage,
    /// Calls that have ended, of any kind.
    completed: u64,
}

impl ProcTable {
    pub fn new() -> ProcTable {
        ProcTable::default()
    }

    /// Track a new call and hand back the handle to give the worker.
    pub fn register(
        &mut self,
        owner: Owner,
        model_index: usize,
        model: &str,
        what: &str,
    ) -> ProcHandle {
        self.next_id += 1;
        let handle = ProcHandle::new();
        self.rows.push(Tracked {
            id: self.next_id,
            owner,
            model: model.to_string(),
            model_index,
            what: what.to_string(),
            handle: handle.clone(),
            done_at: None,
        });
        handle
    }

    /// Fold newly-finished calls into the running totals and age out old rows,
    /// returning the calls that ended since the last sweep.
    ///
    /// Called once a frame: the alternative — accounting inside the worker —
    /// would put the whole table behind a lock for the sake of four additions.
    /// The returned list is how a conversation reaches the database before the
    /// row carrying its id is aged out of memory.
    pub fn sweep(&mut self) -> Vec<Completed> {
        let now = Instant::now();
        let mut just_ended = Vec::new();
        for row in &mut self.rows {
            if row.done_at.is_some() {
                continue;
            }
            let snap = row.handle.snapshot();
            if snap.state.is_live() {
                continue;
            }
            row.done_at = Some(now);
            self.completed += 1;
            let ended = match &snap.state {
                RunState::Done(e) => e.clone(),
                // Unreachable: `is_live` already sent every other state on.
                _ => Ended::Finished,
            };
            just_ended.push(Completed {
                owner: row.owner,
                model: row.model.clone(),
                what: row.what.clone(),
                session: snap.session.clone(),
                pid: snap.pid,
                usage: snap.usage,
                ended,
            });
            let u = snap.usage;
            self.spent.input_tokens += u.input_tokens;
            self.spent.output_tokens += u.output_tokens;
            self.spent.cache_read_tokens += u.cache_read_tokens;
            if let Some(c) = u.cost_usd {
                self.spent.cost_usd = Some(self.spent.cost_usd.unwrap_or(0.0) + c);
            }
        }
        if self.rows.len() > KEEP_AT_LEAST {
            let mut droppable = self.rows.len() - KEEP_AT_LEAST;
            self.rows.retain(|r| {
                let stale = r
                    .done_at
                    .is_some_and(|t| now.duration_since(t) > KEEP_FINISHED);
                if stale && droppable > 0 {
                    droppable -= 1;
                    return false;
                }
                true
            });
        }
        just_ended
    }

    pub fn rows(&self) -> &[Tracked] {
        &self.rows
    }

    /// Live rows, newest first — the order a "what is running" list wants.
    ///
    /// The handle is asked, not `done_at`. `done_at` is only set by [`sweep`],
    /// so a count taken from it would be as old as the last sweep: a call that
    /// failed to spawn a moment ago would still be counted as running, and
    /// anything gating on "nothing is running" would be answered wrongly.
    ///
    /// [`sweep`]: ProcTable::sweep
    pub fn live(&self) -> impl Iterator<Item = &Tracked> {
        self.rows
            .iter()
            .rev()
            .filter(|r| r.done_at.is_none() && r.handle.snapshot().state.is_live())
    }

    pub fn running(&self, owner: Owner) -> usize {
        self.live().filter(|r| r.owner == owner).count()
    }

    pub fn running_total(&self) -> usize {
        self.live().count()
    }

    /// Everything the tracked calls have reported spending, finished rows
    /// included. This is what a usage limit is measured against.
    pub fn spent(&self) -> Usage {
        self.spent
    }

    pub fn completed(&self) -> u64 {
        self.completed
    }

    /// Stop every live call one page owns, and return a receipt per process so
    /// the reviewer can be shown exactly what was killed. An empty result
    /// means nothing was running, which is itself worth saying.
    pub fn stop(&mut self, owner: Owner, reason: &str) -> Vec<StopReceipt> {
        self.stop_where(reason, |r| r.owner == owner)
    }

    pub fn stop_all(&mut self, reason: &str) -> Vec<StopReceipt> {
        self.stop_where(reason, |_| true)
    }

    /// Stop one tracked call by its row id.
    pub fn stop_one(&mut self, id: u64, reason: &str) -> Vec<StopReceipt> {
        self.stop_where(reason, |r| r.id == id)
    }

    fn stop_where(&mut self, reason: &str, want: impl Fn(&Tracked) -> bool) -> Vec<StopReceipt> {
        let mut out = Vec::new();
        for row in self.rows.iter().filter(|r| r.done_at.is_none() && want(r)) {
            let snap = row.handle.snapshot();
            if !snap.state.is_live() {
                continue;
            }
            row.handle.stop(reason);
            out.push(StopReceipt {
                owner: row.owner,
                model_index: row.model_index,
                model: row.model.clone(),
                what: row.what.clone(),
                pid: snap.pid,
                session: snap.session,
                handle: row.handle.clone(),
            });
        }
        out
    }
}

/// Kill a process and everything it started.
///
/// [`std::process::Child::kill`] reaches the process this app spawned and no
/// further. That is not enough here: the model CLIs are npm shims, so what was
/// spawned is `claude.cmd`, and the node process underneath it is the one
/// reading the repository, holding the session and spending the tokens.
/// Killing only the shim leaves that behind — still working, and still holding
/// the inherited stdout pipe, which is the very case the reader threads have
/// to fall back to a timeout for.
pub fn kill_tree(child: &mut std::process::Child) {
    let pid = child.id();
    #[cfg(windows)]
    {
        // `taskkill /T` walks the tree from this pid down. Without it the node
        // process behind the shim survives; the alternative — a job object —
        // would have to be set up at spawn time for every CLI call.
        let _ = crate::gitio::hidden_command("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(unix)]
    {
        // `models::run_model` asks the child to lead a new process group. Do
        // not trust that assumption at the point where a negative pid could
        // otherwise signal this app's own group: verify it against the OS.
        // Calling libc directly also avoids platform-specific parsing of a
        // negative pid by the external `kill` utility.
        if let Ok(pid) = libc::pid_t::try_from(pid) {
            let owns_group = unsafe { libc::getpgid(pid) } == pid;
            if owns_group {
                // TERM first for orderly cleanup, then make certain inherited
                // helpers cannot outlive the review that started them.
                unsafe {
                    libc::kill(-pid, libc::SIGTERM);
                    libc::kill(-pid, libc::SIGKILL);
                }
            }
        }
    }
    // Whatever the platform sweep managed, make sure the process this app is
    // actually waiting on is dead: the wait loop is what unblocks the review,
    // and it only ends when this one does.
    let _ = child.kill();
}

/// What one streamed event says the model is doing, if it says anything a
/// human would recognise. The streaming CLIs (`claude --output-format
/// stream-json`, `codex --json`) emit one JSON event per line; a tool call is
/// the reportable step in either stream. Everything else — text deltas,
/// lifecycle events, non-JSON noise — is counted but not shown.
fn live_activity(line: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if let Some(s) = tool_use_activity(&v) {
        return Some(s);
    }
    // codex spells a shell step as a `command` field on the item rather than
    // as a tool_use block.
    find_command(&v).map(|c| format!("$ {}", clip_head(&c, 56)))
}

/// The first `tool_use` block anywhere in the event: the tool's name plus the
/// most target-like thing in its input.
fn tool_use_activity(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Object(o) => {
            if o.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                if let Some(name) = o.get("name").and_then(|n| n.as_str()) {
                    let target = o.get("input").and_then(|input| {
                        ["file_path", "path", "pattern", "command", "url", "query"]
                            .iter()
                            .find_map(|k| input.get(*k).and_then(|s| s.as_str()))
                    });
                    return Some(match target {
                        Some(t) => format!("{name} {}", clip_tail(t, 48)),
                        None => name.to_string(),
                    });
                }
            }
            o.values().find_map(tool_use_activity)
        }
        serde_json::Value::Array(a) => a.iter().find_map(tool_use_activity),
        _ => None,
    }
}

/// The first `command` value anywhere in the event — a string, or an argv
/// array joined back into one.
fn find_command(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Object(o) => {
            match o.get("command") {
                Some(serde_json::Value::String(s)) if !s.trim().is_empty() => {
                    return Some(s.clone())
                }
                Some(serde_json::Value::Array(a)) => {
                    let parts: Vec<&str> = a.iter().filter_map(|x| x.as_str()).collect();
                    if !parts.is_empty() {
                        return Some(parts.join(" "));
                    }
                }
                _ => {}
            }
            o.values().find_map(find_command)
        }
        serde_json::Value::Array(a) => a.iter().find_map(find_command),
        _ => None,
    }
}

/// Keep the start of a string — a command's program and first arguments are
/// its informative end.
fn clip_head(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let head: String = s.chars().take(n - 1).collect();
    format!("{head}…")
}

/// Keep the end of a string — a path's file name is its informative end.
fn clip_tail(s: &str, n: usize) -> String {
    let total = s.chars().count();
    if total <= n {
        return s.to_string();
    }
    let tail: String = s.chars().skip(total - (n - 1)).collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(tokens: i64, cost: Option<f64>) -> Usage {
        Usage {
            input_tokens: tokens,
            output_tokens: 0,
            cache_read_tokens: 0,
            cost_usd: cost,
        }
    }

    /// A stop is a request, not an outcome. Until the worker confirms it, the
    /// row must not claim the process is gone — the whole point of the banner
    /// is that "terminated" on screen can be trusted.
    #[test]
    fn a_stop_is_not_a_termination_until_the_process_says_so() {
        let mut table = ProcTable::new();
        let handle = table.register(Owner::Review, 0, "claude", "src/a.rs:1");
        handle.started_process(4242);

        let receipts = table.stop(Owner::Review, "left the review screen");
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].pid, Some(4242));
        assert!(!receipts[0].confirmed(), "nothing has killed anything yet");
        let line = receipts[0].line("claude");
        assert!(line.contains("pid 4242 terminating"), "{line}");
        assert_eq!(handle.snapshot().state, RunState::Stopping);
        assert!(handle.stop_requested(), "the worker's poll loop reads this");

        // The worker kills the child and writes the outcome back.
        handle.ended(
            Ended::Cancelled("left the review screen".into()),
            usage(10, Some(0.5)),
        );
        assert!(receipts[0].confirmed());
        let line = receipts[0].line("model A");
        assert!(line.contains("pid 4242 terminated"), "{line}");
        // Named by whatever the caller passes, so a blinded card's stand-in
        // reaches the banner instead of the name in settings.
        assert!(line.starts_with("model A"), "{line}");
    }

    /// A stop raised before the CLI has a pid still has to land, or a model
    /// that is slow to start survives the navigation that was meant to end it.
    #[test]
    fn a_stop_during_startup_survives_the_process_appearing() {
        let mut table = ProcTable::new();
        let handle = table.register(Owner::Fix, 1, "codex", "fix session");
        assert_eq!(handle.snapshot().state, RunState::Starting);

        let receipts = table.stop(Owner::Fix, "left the follow-up screen");
        assert_eq!(receipts[0].pid, None, "there was no process to name yet");
        handle.started_process(99);
        assert_eq!(
            handle.snapshot().state,
            RunState::Stopping,
            "a late pid must not put the row back to running"
        );
        assert!(handle.stop_requested());
    }

    /// Stopping is addressed to one page. The others are mid-flight work the
    /// reviewer has not walked away from.
    #[test]
    fn stopping_one_page_leaves_the_others_running() {
        let mut table = ProcTable::new();
        let review = table.register(Owner::Review, 0, "claude", "src/a.rs:1");
        let fix = table.register(Owner::Fix, 0, "claude", "fix session");
        review.started_process(1);
        fix.started_process(2);

        let receipts = table.stop(Owner::Review, "navigated away");
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].owner, Owner::Review);
        assert!(
            fix.snapshot().state.is_live(),
            "the fix session was not left"
        );
        assert_eq!(table.running(Owner::Fix), 1);
    }

    /// A killed call still spent what it spent. Dropping its usage would let a
    /// reviewer who navigates away often run indefinitely under a limit.
    #[test]
    fn a_killed_calls_usage_stays_on_the_books() {
        let mut table = ProcTable::new();
        let a = table.register(Owner::Review, 0, "claude", "src/a.rs:1");
        let b = table.register(Owner::Review, 1, "codex", "src/a.rs:1");
        a.started_process(1);
        b.started_process(2);

        table.stop(Owner::Review, "navigated away");
        a.ended(
            Ended::Cancelled("navigated away".into()),
            usage(1_000, Some(0.02)),
        );
        b.ended(Ended::Finished, usage(500, Some(0.01)));
        table.sweep();

        let spent = table.spent();
        assert_eq!(spent.input_tokens, 1_500, "the killed call counts too");
        assert_eq!(spent.cost_usd, Some(0.03));
        assert_eq!(table.completed(), 2);
        assert_eq!(table.running_total(), 0);
        // Sweeping twice must not bill the same call twice.
        table.sweep();
        assert_eq!(table.spent().input_tokens, 1_500);
    }

    /// A session id known before the process starts is what makes a resume
    /// possible after an interruption; one that only arrives with the reply
    /// leaves a killed call with nothing to resume, and the receipt has to say
    /// so rather than offer a resume that cannot work.
    #[test]
    fn a_receipt_carries_the_session_only_when_there_is_one() {
        let mut table = ProcTable::new();
        let known = table.register(Owner::Review, 0, "claude", "src/a.rs:1");
        known.set_session(Some("11111111-2222".into()));
        known.started_process(7);
        let unknown = table.register(Owner::Review, 1, "codex", "src/a.rs:1");
        unknown.started_process(8);

        let receipts = table.stop(Owner::Review, "navigated away");
        let claude = receipts.iter().find(|r| r.model == "claude").unwrap();
        let codex = receipts.iter().find(|r| r.model == "codex").unwrap();
        assert_eq!(claude.session.as_deref(), Some("11111111-2222"));
        assert_eq!(codex.session, None);
        // Blank is not a session id: it would offer a resume that resumes
        // nothing.
        unknown.set_session(Some("   ".into()));
        assert_eq!(unknown.snapshot().session, None);
    }

    /// A call that has ended is not running, whether or not the table has got
    /// round to sweeping it. Anything that gates on "nothing is running" — a
    /// quit, a test, a button — reads this between sweeps.
    #[test]
    fn a_finished_call_stops_counting_as_running_before_the_next_sweep() {
        let mut table = ProcTable::new();
        let handle = table.register(Owner::Review, 0, "claude", "src/a.rs:1");
        handle.started_process(5);
        assert_eq!(table.running_total(), 1);

        handle.ended(Ended::SpawnFailed, Usage::default());
        assert_eq!(
            table.running_total(),
            0,
            "no sweep has run yet, and it must not have to"
        );
        assert_eq!(table.running(Owner::Review), 0);
        // The row is still there to be read; it is only no longer live.
        assert_eq!(table.rows().len(), 1);
    }

    /// The clock has to stop when the call does, or a paused card sits there
    /// counting up as though it were still working.
    #[test]
    fn the_clock_freezes_when_the_call_ends() {
        let handle = ProcHandle::new();
        handle.started_process(3);
        handle.ended(Ended::Finished, Usage::default());
        let first = handle.snapshot().elapsed;
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(handle.snapshot().elapsed, first);
    }

    #[test]
    fn live_activity_reads_a_tool_call_out_of_a_stream_event() {
        // claude --output-format stream-json: an assistant message event
        // carrying a tool_use block.
        let ev = "{\"type\":\"assistant\",\"message\":{\"content\":[\
{\"type\":\"tool_use\",\"name\":\"Read\",\"input\":{\"file_path\":\"src/config.rs\"}}]}}";
        assert_eq!(live_activity(ev).unwrap(), "Read src/config.rs");
        // codex --json: the step is a command on the item, argv-style.
        let ev = "{\"type\":\"item.started\",\"item\":{\"type\":\"command_execution\",\
\"command\":[\"bash\",\"-lc\",\"cargo test\"]}}";
        assert_eq!(live_activity(ev).unwrap(), "$ bash -lc cargo test");
        // Lifecycle noise and plain text are counted, never shown.
        assert_eq!(live_activity("{\"type\":\"turn.started\"}"), None);
        assert_eq!(live_activity("plain text, not an event"), None);
    }

    #[test]
    fn a_live_handle_keeps_the_last_step_and_counts_the_stream() {
        let live = ProcHandle::new();
        live.on_line("{\"type\":\"system\",\"subtype\":\"init\"}");
        live.on_line(
            "{\"type\":\"assistant\",\"message\":{\"content\":[\
{\"type\":\"tool_use\",\"name\":\"Grep\",\"input\":{\"pattern\":\"RETRY_LIMIT\"}}]}}",
        );
        live.on_line("   ");
        let snap = live.snapshot();
        assert_eq!(snap.lines, 2, "blank lines are not output");
        assert_eq!(snap.activity, "Grep RETRY_LIMIT");
        assert_eq!(snap.activity_line(false).unwrap(), "Grep RETRY_LIMIT");
        // Blinded, the step goes: which tool a CLI reaches for, and how it
        // spells the call, is a name by another route.
        assert_eq!(snap.activity_line(true).unwrap(), "2 line(s) of output");
        // Output with no recognisable step in it still shows proof of life.
        let quiet = ProcHandle::new();
        quiet.on_line("warming up");
        assert_eq!(
            quiet.snapshot().activity_line(false).unwrap(),
            "1 line(s) of output"
        );
    }
}
