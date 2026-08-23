//! Invoke reviewer model CLIs (claude / codex / agy / anything configured)
//! on background threads and parse their JSON verdicts.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::settings::ModelSlot;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Keep,
    Rewrite,
    Delete,
    /// Code only: something is wrong that a rewrite of the unit's own lines
    /// cannot fix. The justification is the payload; nothing is edited.
    Flag,
}

impl Action {
    pub fn label(self) -> &'static str {
        match self {
            Action::Keep => "KEEP",
            Action::Rewrite => "REWRITE",
            Action::Delete => "DELETE",
            Action::Flag => "FLAG",
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Keep => "keep",
            Action::Rewrite => "rewrite",
            Action::Delete => "delete",
            Action::Flag => "flag",
        }
    }
}

/// One place a model says it read on its way to a verdict, so the human can
/// look at the same code. Self-reported — it is the only cross-CLI channel
/// there is — which is exactly why the viewer shows the real file at that
/// spot rather than anything the model wrote.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Evidence {
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub lines: String,
    #[serde(default)]
    pub note: String,
}

#[derive(Clone, Debug)]
pub struct Suggestion {
    pub action: Action,
    pub comment: String,
    pub justification: String,
    pub evidence: Vec<Evidence>,
    pub latency_ms: i64,
}

/// One exchange with a model CLI: what we piped in, what it printed back.
/// Recorded only so the inspector can show it — conversational continuity
/// comes from the CLI's own session, not from anything we re-send.
#[derive(Clone)]
pub struct Turn {
    pub prompt: String,
    pub reply: String,
}

/// How much of a CLI's output the inspector keeps. A model that reads its way
/// around the repository prints whole files into its transcript, and the whole
/// thing is laid out again on every repaint of the prompt window. The ends are
/// where anything useful is — the CLI's envelope at the top, the verdict at the
/// bottom — so the middle is what goes.
const TRANSCRIPT_HEAD: usize = 2_000;
const TRANSCRIPT_TAIL: usize = 4_000;

/// The output as the transcript should keep it. Parsing has already happened
/// against the full text by the time this is called; this is only what the
/// human is shown.
pub fn transcript_excerpt(raw: &str) -> String {
    let total = raw.chars().count();
    let kept = TRANSCRIPT_HEAD + TRANSCRIPT_TAIL;
    if total <= kept {
        return raw.to_string();
    }
    let head: String = raw.chars().take(TRANSCRIPT_HEAD).collect();
    let tail: String = raw.chars().skip(total - TRANSCRIPT_TAIL).collect();
    format!("{head}\n\n… {} characters elided …\n\n{tail}", total - kept)
}

/// The answer format both prompts (and every follow-up) ask for. Shared so a
/// follow-up can never drift from the schema the opening turn promised.
/// The evidence list is what lets the human see the larger context the model
/// judged from — which files it went and read, and why they mattered.
pub fn answer_schema(code: bool) -> &'static str {
    if code {
        "Answer with JSON only:\n\
{\"action\":\"approve|revise|flag|delete\",\
\"replacement\":\"full replacement for the lines under review if revise, else empty\",\
\"justification\":\"one or two short sentences — for flag, say exactly what is wrong and where\",\
\"evidence\":[{\"file\":\"path\",\"lines\":\"12-40\",\"note\":\"what you checked there\"}]}\n\
In \"evidence\", list the places you actually read to reach this verdict — the reviewer is \
shown them. Use [] if you judged from the excerpt alone."
    } else {
        "Answer with JSON only:\n\
{\"action\":\"keep|rewrite|delete\",\
\"comment\":\"replacement text if rewrite, else empty\",\
\"justification\":\"one short sentence\",\
\"evidence\":[{\"file\":\"path\",\"lines\":\"12-40\",\"note\":\"what you checked there\"}]}\n\
In \"evidence\", list the places you actually read to reach this verdict — the reviewer is \
shown them. Use [] if you judged from the excerpt alone."
    }
}

/// The prompt for a follow-up turn. The conversation itself lives in the
/// CLI's own session, so only the new message and the answer format go over.
pub fn followup_prompt(message: &str, code: bool) -> String {
    format!("{}\n\n{}", message.trim(), answer_schema(code))
}

#[derive(Deserialize)]
struct RawSuggestion {
    action: String,
    /// The proposed replacement. Comment prompts call it "comment", code
    /// prompts "replacement"; either key lands here.
    #[serde(default, alias = "replacement")]
    comment: String,
    #[serde(default)]
    justification: String,
    #[serde(default)]
    evidence: Vec<Evidence>,
}

/// Message sent back to the UI thread when a model finishes.
pub struct CandidateMsg {
    /// Monotonic id of the review position the request was made for; stale
    /// replies (user already moved on) are logged but not displayed.
    pub seq: u64,
    pub slot_idx: usize,
    pub model: String,
    pub result: Result<Suggestion, String>,
    /// Everything the CLI printed, kept verbatim for the transcript.
    pub raw: String,
}

/// Split the command template into argv, substituting `{prompt}` and `{repo}`
/// and appending the model selection. Returns (argv, prompt_via_stdin).
///
/// Piping is the default: a template without `{prompt}` gets the prompt on
/// stdin, which sidesteps both the command-line length limit and Windows'
/// refusal to pass multi-line arguments to a `.cmd` shim.
///
/// `{repo}` is for CLIs that need the repository named rather than merely
/// being started in it — agy will not read a directory that is not in its
/// workspace, however it was launched. `{cli_home}` is for one whose
/// permissions live in a config file rather than in flags: it points the CLI
/// at a home this app owns, so the rules it runs under are the reviewer's and
/// not the user's own. Both are substituted inside their token, so a path with
/// spaces in it stays one argument.
fn build_argv(
    template: &str,
    prompt: &str,
    repo: &str,
    cli_home: &str,
    slot: &ModelSlot,
) -> (Vec<String>, bool) {
    let mut argv: Vec<String> = Vec::new();
    let mut used_placeholder = false;
    // No repository (a replay whose checkout has gone) leaves the CLI pointed
    // at wherever it was started, which is what it would default to anyway.
    let repo = if repo.trim().is_empty() { "." } else { repo.trim() };
    for tok in template.split_whitespace() {
        if tok == "{prompt}" {
            argv.push(prompt.to_string());
            used_placeholder = true;
        } else {
            argv.push(tok.replace("{repo}", repo).replace("{cli_home}", cli_home));
        }
    }
    // Append rather than splice: inserting ahead of the prompt would land
    // between a flag and its value for templates like `agy -p {prompt}`, where
    // the prompt *is* `-p`'s argument. All three shipped CLIs accept these
    // trailing. A CLI that needs them elsewhere can spell them out in the
    // command template and leave these fields empty.
    for (value, flag, fallback) in [
        (&slot.model, &slot.model_flag, "--model"),
        (&slot.effort, &slot.effort_flag, "--effort"),
    ] {
        if value.trim().is_empty() {
            continue;
        }
        argv.push(if flag.trim().is_empty() { fallback } else { flag.trim() }.to_string());
        argv.push(value.trim().to_string());
    }
    (argv, !used_placeholder)
}

/// Every JSON document in the output: the whole thing if it parses, else
/// line by line for CLIs that emit a JSONL event stream (`codex --json`).
fn json_documents(output: &str) -> Vec<serde_json::Value> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(output.trim()) {
        return vec![v];
    }
    output
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        .collect()
}

fn collect_strings(v: &serde_json::Value, out: &mut Vec<String>) {
    match v {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(a) => a.iter().for_each(|x| collect_strings(x, out)),
        serde_json::Value::Object(o) => o.values().for_each(|x| collect_strings(x, out)),
        _ => {}
    }
}

fn find_string_key(v: &serde_json::Value, key: &str) -> Option<String> {
    match v {
        serde_json::Value::Object(o) => {
            if let Some(serde_json::Value::String(s)) = o.get(key) {
                return Some(s.clone());
            }
            o.values().find_map(|x| find_string_key(x, key))
        }
        serde_json::Value::Array(a) => a.iter().find_map(|x| find_string_key(x, key)),
        _ => None,
    }
}

/// The session id the CLI reported, looked up by key anywhere in its output.
/// Returns None when the slot names no key — those CLIs take an id we choose.
pub fn extract_session_id(output: &str, key: &str) -> Option<String> {
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    json_documents(output).iter().find_map(|v| find_string_key(v, key))
}

/// Pull a typed payload out of whatever the CLI printed. Asking a CLI for
/// machine-readable output (needed to recover the session id) wraps the
/// model's answer in the CLI's own envelope, so the payload arrives as an
/// escaped string inside it rather than as bare text — first scan the output
/// itself, then every string embedded in its JSON documents.
pub(crate) fn extract_payload<T: serde::de::DeserializeOwned>(output: &str) -> Option<T> {
    if let Some(v) = scan_json::<T>(output) {
        return Some(v);
    }
    let mut strings = Vec::new();
    for doc in json_documents(output) {
        collect_strings(&doc, &mut strings);
    }
    strings.iter().rev().find_map(|s| scan_json::<T>(s))
}

fn extract_verdict(output: &str) -> Option<RawSuggestion> {
    extract_payload(output)
}

#[cfg(test)]
fn extract_json(output: &str) -> Option<RawSuggestion> {
    scan_json(output)
}

fn scan_json<T: serde::de::DeserializeOwned>(output: &str) -> Option<T> {
    // Model CLIs often wrap JSON in prose or code fences; scan for the first
    // balanced object that parses into the expected shape.
    let bytes = output.as_bytes();
    let mut start = None;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for (i, &b) in bytes.iter().enumerate() {
        if esc {
            esc = false;
            continue;
        }
        match b {
            b'\\' if in_str => esc = true,
            b'"' => in_str = !in_str,
            b'{' if !in_str => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            b'}' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    if let Some(s) = start {
                        if let Ok(raw) = serde_json::from_str::<T>(&output[s..=i]) {
                            return Some(raw);
                        }
                    }
                    start = None;
                }
            }
            _ => {}
        }
    }
    None
}

/// The most useful error available when the output carried no verdict.
///
/// A CLI that refuses one of its own tools reports it in its envelope and
/// still exits 0, so without this the slot shows a wall of JSON with the one
/// line that explains it buried inside.
pub(crate) fn cli_error(name: &str, stdout: &str, stderr: &str) -> String {
    for out in [stdout, stderr] {
        let reported = json_documents(out).iter().find_map(|v| find_string_key(v, "error"));
        if let Some(msg) = reported.filter(|m| !m.trim().is_empty()) {
            return refused_tool(name, &msg).unwrap_or(msg);
        }
    }
    let snippet: String = stdout.chars().take(400).collect();
    format!("no JSON verdict in output: {}", snippet.trim())
}

/// Rewrite a permission refusal into something actionable. This is not a
/// broken CLI and retrying will not help — its own rules said no — so the
/// message has to name the command it wanted and where that gets decided.
fn refused_tool(name: &str, msg: &str) -> Option<String> {
    if !msg.to_ascii_lowercase().contains("permission") {
        return None;
    }
    // The command is quoted in the refusal; fall back to the whole message
    // rather than guessing if this CLI words it differently.
    let wanted = msg.split('"').nth(1).map(str::trim).filter(|c| !c.is_empty());
    Some(match wanted {
        Some(cmd) => format!(
            "{name} refused a tool it wanted: `{cmd}`. Nothing ran, and retrying will not \
             help until that command is allowed in the CLI's own permission settings. \
             Reviewing needs only file reads, so a slot that keeps hitting this is reaching \
             for shell commands it does not need."
        ),
        None => format!("{name} refused a tool it wanted: {}", msg.trim()),
    })
}

/// Run one model CLI. Returns the parsed verdict and everything the process
/// printed (kept for the transcript and the prompt inspector).
/// `command` is the already-resolved template — the slot's opening command
/// or its resume command with the session id substituted in.
///
/// `repo` is the directory the CLI runs in, which is what makes the rest of
/// the codebase reachable: the prompt names files by their path relative to
/// the repository root, so the model can only open them if that is where it
/// started. Empty means "inherit ours", which is what the unit tests use.
fn run_model(
    slot: &ModelSlot,
    command: &str,
    prompt: &str,
    repo: &str,
    cli_home: &str,
    timeout: Duration,
) -> (Result<Suggestion, String>, String) {
    let mut raw_output = String::new();
    let result = run_inner(slot, command, prompt, repo, cli_home, timeout, &mut raw_output);
    (result, raw_output)
}

/// Run a model CLI to completion and collect what it printed: everything up
/// to — but not including — parsing a verdict out of the output. Shared by
/// the per-unit review and the branch pass, which want different payloads
/// from the same plumbing. Returns (stdout, stderr, latency_ms) and appends
/// the transcript-worthy text to `raw_output`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn capture_cli(
    slot: &ModelSlot,
    command: &str,
    prompt: &str,
    repo: &str,
    cli_home: &str,
    timeout: Duration,
    raw_output: &mut String,
) -> Result<(String, String, i64), String> {
    let (argv, via_stdin) = build_argv(command, prompt, repo, cli_home, slot);
    if argv.is_empty() {
        return Err("empty command template".into());
    }
    let started = Instant::now();
    let mut cmd = crate::gitio::hidden_command(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(if via_stdin { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if !repo.trim().is_empty() {
        cmd.current_dir(repo);
    }
    let mut child = cmd.spawn().map_err(|e| format!("spawn `{}`: {e}", argv[0]))?;
    if via_stdin {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(prompt.as_bytes());
        }
    }

    // Poll with a deadline; kill on timeout so a hung CLI can't wedge a slot.
    let deadline = started + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("timed out after {}s", timeout.as_secs()));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(format!("wait: {e}")),
        }
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let latency_ms = started.elapsed().as_millis() as i64;
    raw_output.push_str(stdout.trim());
    if !stderr.trim().is_empty() {
        if !raw_output.is_empty() {
            raw_output.push('\n');
        }
        raw_output.push_str("[stderr] ");
        raw_output.push_str(stderr.trim());
    }
    Ok((stdout, stderr, latency_ms))
}

#[allow(clippy::too_many_arguments)]
fn run_inner(
    slot: &ModelSlot,
    command: &str,
    prompt: &str,
    repo: &str,
    cli_home: &str,
    timeout: Duration,
    raw_output: &mut String,
) -> Result<Suggestion, String> {
    let (stdout, stderr, latency_ms) =
        capture_cli(slot, command, prompt, repo, cli_home, timeout, raw_output)?;

    let raw = extract_verdict(&stdout)
        .or_else(|| extract_verdict(&stderr))
        .ok_or_else(|| cli_error(&slot.name, &stdout, &stderr))?;
    let action = match raw.action.trim().to_ascii_lowercase().as_str() {
        "keep" | "approve" => Action::Keep,
        "rewrite" | "revise" => Action::Rewrite,
        "delete" | "remove" => Action::Delete,
        "flag" | "concern" => Action::Flag,
        other => return Err(format!("unknown action {other:?}")),
    };
    Ok(Suggestion {
        action,
        comment: raw.comment,
        justification: raw.justification,
        evidence: raw.evidence,
        latency_ms,
    })
}

/// Run a model outside the review loop — the evaluation replay uses this so a
/// measurement goes through exactly the same path as a real review.
pub fn run_for_eval(
    slot: &ModelSlot,
    command: &str,
    prompt: &str,
    repo: &str,
    cli_home: &str,
    timeout: Duration,
) -> (Result<Suggestion, String>, String) {
    run_model(slot, command, prompt, repo, cli_home, timeout)
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_model(
    seq: u64,
    slot_idx: usize,
    slot: ModelSlot,
    command: String,
    prompt: String,
    repo: String,
    cli_home: String,
    timeout_secs: u64,
    send: impl FnOnce(CandidateMsg) + Send + 'static,
    ctx: egui::Context,
) {
    std::thread::spawn(move || {
        let (result, raw) =
            run_model(&slot, &command, &prompt, &repo, &cli_home, Duration::from_secs(timeout_secs.max(5)));
        send(CandidateMsg { seq, slot_idx, model: slot.name.clone(), result, raw });
        ctx.request_repaint();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_json_from_noisy_output() {
        let noisy = "Sure! Here's my verdict:\n```json\n{\"action\":\"rewrite\",\"comment\":\"Bump the counter.\",\"justification\":\"Original restates the code.\"}\n```\nDone.";
        let raw = extract_json(noisy).unwrap();
        assert_eq!(raw.action, "rewrite");
        assert_eq!(raw.comment, "Bump the counter.");
    }

    #[test]
    fn a_code_verdict_parses_with_replacement_alias_and_evidence() {
        let out = "{\"action\":\"revise\",\"replacement\":\"    retry(n - 1);\",\
\"justification\":\"off by one\",\
\"evidence\":[{\"file\":\"src/retry.rs\",\"lines\":\"10-30\",\"note\":\"caller counts from 1\"}]}";
        let raw = extract_json(out).unwrap();
        assert_eq!(raw.action, "revise");
        assert_eq!(raw.comment, "    retry(n - 1);", "\"replacement\" must land in comment");
        assert_eq!(raw.evidence.len(), 1);
        assert_eq!(raw.evidence[0].file, "src/retry.rs");
        assert_eq!(raw.evidence[0].lines, "10-30");
    }

    #[test]
    fn evidence_objects_inside_the_verdict_do_not_confuse_the_scanner() {
        // The balanced-object scan must return the verdict, not the first
        // nested {...} it happens to close.
        let out = "prose {\"action\":\"flag\",\"justification\":\"races\",\
\"evidence\":[{\"file\":\"a.rs\"},{\"file\":\"b.rs\"}]} more prose";
        let raw = extract_json(out).unwrap();
        assert_eq!(raw.action, "flag");
        assert_eq!(raw.evidence.len(), 2);
    }

    #[test]
    fn the_schemas_stay_paired_with_the_prompts() {
        assert!(answer_schema(false).contains("keep|rewrite|delete"));
        assert!(answer_schema(false).contains("\"evidence\""));
        assert!(answer_schema(true).contains("approve|revise|flag|delete"));
        assert!(answer_schema(true).contains("\"replacement\""));
        assert!(followup_prompt("why?", true).contains("approve|revise|flag|delete"));
        assert!(followup_prompt("why?", false).contains("keep|rewrite|delete"));
    }

    #[test]
    fn skips_non_matching_objects_and_handles_braces_in_strings() {
        let s = "{\"other\":1} then {\"action\":\"keep\",\"justification\":\"has {braces} and \\\"quotes\\\"\"}";
        let raw = extract_json(s).unwrap();
        assert_eq!(raw.action, "keep");
        assert!(raw.justification.contains("{braces}"));
    }

    fn slot(model: &str, model_flag: &str, effort: &str, effort_flag: &str) -> ModelSlot {
        ModelSlot {
            name: "t".into(),
            command: String::new(),
            coauthor: String::new(),
            enabled: true,
            model: model.into(),
            model_flag: model_flag.into(),
            effort: effort.into(),
            effort_flag: effort_flag.into(),
            resume_command: String::new(),
            session_key: String::new(),
        }
    }

    #[test]
    fn argv_placeholder_vs_stdin() {
        let bare = slot("", "--model", "", "--effort");
        let (argv, stdin) = build_argv("claude -p {prompt}", "hello", "", "", &bare);
        assert_eq!(argv, vec!["claude", "-p", "hello"]);
        assert!(!stdin);
        let (argv, stdin) = build_argv("codex exec", "hello", "", "", &bare);
        assert_eq!(argv, vec!["codex", "exec"]);
        assert!(stdin);
    }

    #[test]
    fn model_and_effort_trail_so_they_never_split_a_flag_from_its_value() {
        let (argv, _) =
            build_argv("claude -p", "hi", "", "", &slot("haiku", "--model", "low", "--effort"));
        assert_eq!(argv, vec!["claude", "-p", "--model", "haiku", "--effort", "low"]);
        // `agy -p {prompt}` passes the prompt as -p's value, so the flags must
        // land after it — inserting ahead would make -p consume "--model".
        let (argv, _) =
            build_argv("agy -p {prompt}", "hi", "", "", &slot("gemini-3.7-flash-low", "--model", "", ""));
        assert_eq!(argv, vec!["agy", "-p", "hi", "--model", "gemini-3.7-flash-low"]);
        // codex routes effort through a config override rather than a flag
        let (argv, _) = build_argv(
            "codex exec",
            "hi",
            "",
            "",
            &slot("gpt-5.6-luna", "--model", "model_reasoning_effort=low", "-c"),
        );
        assert_eq!(
            argv,
            vec!["codex", "exec", "--model", "gpt-5.6-luna", "-c", "model_reasoning_effort=low"]
        );
        // empty values leave argv untouched
        let (argv, _) = build_argv("claude -p", "hi", "", "", &slot("  ", "--model", " ", "--effort"));
        assert_eq!(argv, vec!["claude", "-p"]);
    }

    #[test]
    fn the_cli_home_token_points_a_cli_at_the_config_this_app_owns() {
        let bare = slot("", "--model", "", "--effort");
        let (argv, _) = build_argv(
            "agy --gemini_dir={cli_home} -p {prompt} --add-dir {repo}",
            "hi",
            "C:/code/app",
            "C:/data/agy home",
            &bare,
        );
        // The flag takes its value with an `=`, so the substitution has to
        // happen inside the token — and a space in the path must not split it.
        assert_eq!(
            argv,
            vec!["agy", "--gemini_dir=C:/data/agy home", "-p", "hi", "--add-dir", "C:/code/app"]
        );
    }

    #[test]
    fn the_repo_token_names_the_directory_and_survives_spaces_in_it() {
        let bare = slot("", "--model", "", "--effort");
        let (argv, _) = build_argv("agy -p {prompt} --add-dir {repo}", "hi", "C:/code/app", "", &bare);
        assert_eq!(argv, vec!["agy", "-p", "hi", "--add-dir", "C:/code/app"]);
        // The template is tokenized on whitespace, so a path with a space in
        // it has to come back out as one argument rather than two.
        let (argv, _) =
            build_argv("agy --add-dir {repo}", "hi", "C:/my code/app", "", &bare);
        assert_eq!(argv, vec!["agy", "--add-dir", "C:/my code/app"]);
        // A replay whose checkout is gone still has to produce a valid argv.
        let (argv, _) = build_argv("agy --add-dir {repo}", "hi", "", "", &bare);
        assert_eq!(argv, vec!["agy", "--add-dir", "."]);
    }

    #[test]
    fn session_id_is_found_in_jsonl_and_in_a_single_object() {
        // codex --json emits an event stream
        let codex = "{\"type\":\"thread.started\",\"thread_id\":\"abc-123\"}\n\
{\"type\":\"turn.started\"}";
        assert_eq!(extract_session_id(codex, "thread_id"), Some("abc-123".into()));
        // agy emits one object
        let agy = "{\"conversation_id\":\"xyz-789\",\"response\":\"hi\"}";
        assert_eq!(extract_session_id(agy, "conversation_id"), Some("xyz-789".into()));
        // a slot that names no key never reports one — the id is ours to pick
        assert_eq!(extract_session_id(agy, ""), None);
        assert_eq!(extract_session_id("plain text", "thread_id"), None);
    }

    #[test]
    fn verdict_is_found_inside_a_cli_json_envelope() {
        // bare text, as claude prints it
        let bare = "Sure: {\"action\":\"keep\",\"justification\":\"fine\"}";
        assert_eq!(extract_verdict(bare).unwrap().action, "keep");
        // escaped inside agy's envelope
        let wrapped = "{\"conversation_id\":\"x\",\"response\":\"\
{\\\"action\\\":\\\"delete\\\",\\\"justification\\\":\\\"restates the code\\\"}\"}";
        let v = extract_verdict(wrapped).unwrap();
        assert_eq!(v.action, "delete");
        assert_eq!(v.justification, "restates the code");
        // escaped inside a codex JSONL event
        let jsonl = "{\"type\":\"thread.started\",\"thread_id\":\"t\"}\n\
{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"\
{\\\"action\\\":\\\"rewrite\\\",\\\"comment\\\":\\\"Bump it.\\\",\\\"justification\\\":\\\"clearer\\\"}\"}}";
        let v = extract_verdict(jsonl).unwrap();
        assert_eq!(v.action, "rewrite");
        assert_eq!(v.comment, "Bump it.");
    }

    #[test]
    fn a_verdict_survives_an_envelope_that_also_reports_a_refusal() {
        // What agy returns once it has a rule to bounce off rather than an
        // unanswerable confirmation: it routes around the refused command,
        // answers from files, and reports both. The verdict is the part that
        // matters, so a slot must show a candidate and not an error.
        let mixed = "{\"conversation_id\":\"x\",\"status\":\"ERROR\",\"response\":\"\
{\\\"action\\\":\\\"keep\\\",\\\"comment\\\":\\\"\\\",\\\"justification\\\":\\\"reads fine\\\"}\",\
\"error\":\"permission check failed for command \\\"git log -1\\\": Matches user-configured \
deny rule.\"}";
        let v = extract_verdict(mixed).expect("verdict alongside the refusal");
        assert_eq!(v.action, "keep");
        assert_eq!(v.justification, "reads fine");
    }

    #[test]
    fn a_refused_tool_is_reported_as_itself_rather_than_as_unparseable_output() {
        // agy exits 0 and puts the refusal in its envelope, so the slot would
        // otherwise show a JSON blob whose first 400 characters say nothing.
        let refusal = "{\"conversation_id\":\"x\",\"status\":\"ERROR\",\"response\":\"\",\
\"error\":\"permission check failed for command \\\"git log -1\\\": user denied permission\"}";
        let err = cli_error("agy", refusal, "");
        assert!(err.contains("git log -1"), "the command it wanted must survive: {err}");
        assert!(err.contains("agy"), "the slot must be named: {err}");
        assert!(!err.contains("no JSON verdict"), "{err}");

        // An envelope error that is not about permissions is passed through.
        let other = "{\"status\":\"ERROR\",\"error\":\"model capacity exhausted\"}";
        assert_eq!(cli_error("agy", other, ""), "model capacity exhausted");

        // Output with nothing to go on still says what it saw.
        assert!(cli_error("codex", "garbage", "").contains("no JSON verdict"));
    }

    #[test]
    fn a_long_transcript_keeps_the_envelope_and_the_verdict() {
        let short = "{\"action\":\"keep\"}";
        assert_eq!(transcript_excerpt(short), short, "a normal reply is untouched");

        // What a CLI prints when the model reads a few files on its way to an
        // answer: an opening event, a wall of file content, then the verdict.
        let long = format!(
            "{{\"type\":\"thread.started\",\"thread_id\":\"t-1\"}}\n{}\n{{\"action\":\"delete\"}}",
            "x".repeat(200_000)
        );
        let kept = transcript_excerpt(&long);
        assert!(kept.chars().count() < 7_000, "still {} chars", kept.chars().count());
        assert!(kept.starts_with("{\"type\":\"thread.started\""), "lost the envelope");
        assert!(kept.ends_with("{\"action\":\"delete\"}"), "lost the verdict");
        assert!(kept.contains("characters elided"), "the cut should be visible: {kept:.200}");
    }

    #[test]
    fn followup_prompt_carries_the_message_and_the_format() {
        let p = followup_prompt("  too wordy — one line?  ", false);
        assert!(p.starts_with("too wordy — one line?"));
        assert!(p.contains("\"action\""));
        assert!(p.contains("keep|rewrite|delete"));
    }
}

#[cfg(test)]
mod spawn_tests {
    use super::*;
    use crate::testkit::{FakeCli, FakeCliSpec, TempDir};

    const VERDICT: &str =
        "{\"action\":\"rewrite\",\"comment\":\"Bump it.\",\"justification\":\"clearer\"}";

    fn run(slot: &ModelSlot, prompt: &str) -> (Result<Suggestion, String>, String) {
        let command = slot.command.clone();
        run_model(slot, &command, prompt, "", "", Duration::from_secs(30))
    }

    #[test]
    fn prompt_reaches_the_cli_on_stdin() {
        let dir = TempDir::new("stdin");
        let cli = FakeCli::new(&dir, "fake", FakeCliSpec { reply: VERDICT, ..Default::default() });
        let slot = cli.slot("");

        let (res, _) = run(&slot, "line one\nline two");
        let s = res.expect("verdict");
        assert_eq!(s.action, Action::Rewrite);
        assert_eq!(s.comment, "Bump it.");
        // Multi-line prompts are exactly what argument passing rejects for a
        // .cmd shim, so this is the case that has to go over the pipe.
        let seen = cli.stdin_seen();
        assert!(seen.contains("line one"), "stdin missing first line: {seen:?}");
        assert!(seen.contains("line two"), "stdin missing second line: {seen:?}");
    }

    #[test]
    fn prompt_reaches_the_cli_as_an_argument_when_templated() {
        let dir = TempDir::new("argv");
        let cli = FakeCli::new(&dir, "fake", FakeCliSpec { reply: VERDICT, ..Default::default() });
        let slot = cli.slot("{prompt}");

        let (res, _) = run(&slot, "single-line prompt");
        assert!(res.is_ok());
        assert!(cli.argv_seen().contains("single-line prompt"), "{}", cli.argv_seen());
        assert!(cli.stdin_seen().trim().is_empty(), "stdin should be closed in argv mode");
    }

    #[test]
    fn model_and_effort_reach_the_process_in_order() {
        let dir = TempDir::new("flags");
        let cli = FakeCli::new(&dir, "fake", FakeCliSpec { reply: VERDICT, ..Default::default() });
        let mut slot = cli.slot("--print {prompt}");
        slot.model = "tiny-model".into();
        slot.effort = "low".into();

        let (res, _) = run(&slot, "hello");
        assert!(res.is_ok());
        let argv = cli.argv_seen();
        let prompt_at = argv.find("hello").expect("prompt in argv");
        let model_at = argv.find("--model tiny-model").expect("model flag in argv");
        let effort_at = argv.find("--effort low").expect("effort flag in argv");
        // Both must trail the prompt, or `--print` would swallow the flag
        // instead of the prompt — the bug agy's -p exposed.
        assert!(prompt_at < model_at && model_at < effort_at, "wrong order: {argv:?}");
    }

    #[test]
    fn a_failing_cli_surfaces_as_an_error_with_its_output_kept() {
        let dir = TempDir::new("fail");
        let cli = FakeCli::new(
            &dir,
            "fake",
            FakeCliSpec { reply: "command not recognised", exit_code: 1, ..Default::default() },
        );
        let (res, raw) = run(&cli.slot(""), "hi");
        let err = res.unwrap_err();
        assert!(err.contains("no JSON verdict"), "{err}");
        // The raw output is what the prompt inspector shows, so it must survive.
        assert!(raw.contains("command not recognised"), "{raw:?}");
    }

    #[test]
    fn a_hung_cli_is_killed_at_the_deadline() {
        let dir = TempDir::new("hang");
        let cli = FakeCli::new(
            &dir,
            "fake",
            FakeCliSpec { reply: VERDICT, delay_secs: 30, ..Default::default() },
        );
        let slot = cli.slot("");
        let started = Instant::now();
        let (res, _) = run_model(&slot, &slot.command, "hi", "", "", Duration::from_secs(1));
        let err = res.unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        // A slot that never returns would wedge the review; the poll loop has
        // to give up close to the deadline rather than wait out the child.
        assert!(started.elapsed() < Duration::from_secs(15), "took {:?}", started.elapsed());
    }

    #[test]
    fn session_id_survives_a_round_trip_through_a_resume_command() {
        let dir = TempDir::new("session");
        // Turn 1 answers in an envelope carrying the id, the way codex does.
        let first = FakeCli::new(
            &dir,
            "first",
            FakeCliSpec {
                reply: "{\"type\":\"thread.started\",\"thread_id\":\"sess-42\"}\n\
{\"type\":\"item.completed\",\"item\":{\"text\":\"{\\\"action\\\":\\\"keep\\\",\\\"justification\\\":\\\"ok\\\"}\"}}",
                ..Default::default()
            },
        );
        let mut slot = first.slot("");
        slot.session_key = "thread_id".into();

        let (res, raw) = run(&slot, "first turn");
        assert_eq!(res.unwrap().action, Action::Keep, "verdict must survive the envelope");
        let session = extract_session_id(&raw, &slot.session_key).expect("session id");
        assert_eq!(session, "sess-42");

        // Turn 2 resumes: the id has to land on the child's command line.
        let second =
            FakeCli::new(&dir, "second", FakeCliSpec { reply: VERDICT, ..Default::default() });
        slot.resume_command = format!("{} resume {{session}}", second.command());
        let resume = slot.resume_command.replace("{session}", &session);
        let (res2, _) =
            run_model(&slot, &resume, &followup_prompt("why?", false), "", "", Duration::from_secs(30));
        assert!(res2.is_ok());
        assert!(second.argv_seen().contains("resume sess-42"), "{}", second.argv_seen());
        assert!(second.stdin_seen().contains("why?"), "{}", second.stdin_seen());
    }

    #[test]
    fn the_cli_runs_in_the_repository_it_is_reviewing() {
        let dir = TempDir::new("cwd");
        let cli = FakeCli::new(&dir, "fake", FakeCliSpec { reply: VERDICT, ..Default::default() });
        let repo = TempDir::new("cwd_repo");
        let slot = cli.slot("");

        let (res, _) = run_model(
            &slot,
            &slot.command,
            "hi",
            &repo.path().to_string_lossy(),
            "",
            Duration::from_secs(30),
        );
        assert!(res.is_ok());
        // The prompt names files by their path relative to the repository
        // root, so anywhere else the model has nothing it can open.
        let seen = std::fs::canonicalize(cli.cwd_seen()).expect("child cwd");
        let want = std::fs::canonicalize(repo.path()).expect("repo path");
        assert_eq!(seen, want);
    }

    #[test]
    fn a_missing_program_is_reported_rather_than_panicking() {
        let dir = TempDir::new("missing");
        let mut slot = FakeCli::new(&dir, "fake", FakeCliSpec::default()).slot("");
        slot.command = "cra-no-such-program-anywhere".into();
        let (res, _) = run(&slot, "hi");
        assert!(res.unwrap_err().contains("spawn"), "should name the spawn failure");
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::settings::Settings;
    use crate::testkit::TempRepo;

    /// A value the model can only report by opening a file the prompt never
    /// quotes, so a CLI that answered from the prompt alone is distinguishable
    /// from one that actually browsed the repository.
    const RETRY_LIMIT: &str = "4291";

    /// A repository whose comment cannot be judged from the diff alone: what
    /// "the configured limit" means lives in a file next door.
    fn browsable_repo() -> TempRepo {
        let repo = TempRepo::new("live_browse");
        repo.write("src/config.rs", &format!("pub const RETRY_LIMIT: u32 = {RETRY_LIMIT};\n"));
        repo.write(
            "src/net.rs",
            "use crate::config::RETRY_LIMIT;\n\npub fn connect() {\n    \
             // Retry up to the configured limit.\n    for _ in 0..RETRY_LIMIT {}\n}\n",
        );
        repo.commit("seed");
        repo
    }

    fn verdict_prompt(extra: &str) -> String {
        format!(
            "File: src/net.rs (Rust)\n\n\
>    4|     // Retry up to the configured limit.\n \
    5|     for _ in 0..RETRY_LIMIT {{}}\n\n\
Review the comment marked with '>' (lines 4-4). \
Should it be kept, rewritten, or deleted?{extra}\n\
Answer with JSON only:\n\
{{\"action\":\"keep|rewrite|delete\",\"comment\":\"replacement text if rewrite, else empty\",\"justification\":\"one short sentence\"}}"
        )
    }

    /// Two real turns per configured CLI, resuming by session id between them.
    /// Exercises argv building, PATHEXT resolution, stdin piping, verdict
    /// extraction from each CLI's JSON envelope, session-id capture, and the
    /// resume command.
    ///
    /// Both turns are answerable only from the repository. Turn 1 asks for a
    /// constant defined in a file the prompt does not quote, so a slot whose
    /// tools are switched off — or one started in the wrong directory — fails
    /// here instead of quietly reviewing on the diff alone. Turn 2 asks for
    /// the same number back, so a lost session shows up as a wrong answer.
    ///
    /// Ignored by default; costs six model calls:
    ///     cargo test -- --ignored --nocapture
    #[test]
    #[ignore]
    fn configured_clis_browse_the_repo_and_hold_a_session() {
        let timeout = Duration::from_secs(300);
        let repo = browsable_repo();
        let repo_path = repo.path();
        // A slot whose permissions come from a config file gets the home this
        // app writes; the shipped agy template will not read the repo without
        // it, so the test would be measuring the wrong thing.
        let cli_home = crate::agycli::configure(&repo_path)
            .expect("write CLI permissions")
            .to_string_lossy()
            .to_string();
        let mut failures: Vec<String> = Vec::new();

        for (_, slot) in Settings::default().enabled_models() {
            // Turn 1 — either we name the session or the CLI reports one.
            let (command, mut session) = if slot.session_key.trim().is_empty() {
                let id = uuid::Uuid::new_v4().to_string();
                (slot.command.replace("{session}", &id), Some(id))
            } else {
                (slot.command.clone(), None)
            };
            let first = verdict_prompt(
                " This repository defines RETRY_LIMIT somewhere; open the file that \
                 defines it and start your justification with its value.",
            );
            let (res, raw) = run_model(&slot, &command, &first, &repo_path, &cli_home, timeout);
            match &res {
                Ok(v) => {
                    println!("{:>6} turn 1: {} — {}", slot.name, v.action.label(), v.justification);
                    if !v.justification.contains(RETRY_LIMIT) {
                        failures.push(format!(
                            "{}: did not read the repo — turn 1 said {:?}",
                            slot.name, v.justification
                        ));
                    }
                }
                Err(e) => {
                    println!("{:>6} turn 1: FAILED {e}", slot.name);
                    failures.push(format!("{} turn 1: {e}", slot.name));
                    continue;
                }
            }
            if !slot.session_key.trim().is_empty() {
                session = extract_session_id(&raw, &slot.session_key);
            }
            let Some(session) = session else {
                println!("{:>6}: no session id in output", slot.name);
                failures.push(format!("{}: no session id reported", slot.name));
                continue;
            };
            println!("{:>6} session: {session}", slot.name);

            // Turn 2 — resume, and ask for something only turn 1 established.
            let resume = slot.resume_command.replace("{session}", &session);
            let second = followup_prompt(
                "What was the value of RETRY_LIMIT you found? \
                 Put just that number in \"justification\".",
                false,
            );
            let (res2, raw2) = run_model(&slot, &resume, &second, &repo_path, &cli_home, timeout);
            match res2 {
                Ok(v) => {
                    println!("{:>6} turn 2: justification = {}", slot.name, v.justification);
                    if !v.justification.contains(RETRY_LIMIT) {
                        failures.push(format!(
                            "{}: session lost — turn 2 said {:?}",
                            slot.name, v.justification
                        ));
                    }
                }
                Err(e) => {
                    println!(
                        "{:>6} turn 2: FAILED {e}\n  raw: {}",
                        slot.name,
                        raw2.chars().take(300).collect::<String>()
                    );
                    failures.push(format!("{} turn 2: {e}", slot.name));
                }
            }
        }
        assert!(failures.is_empty(), "live round-trip failed:\n  {}", failures.join("\n  "));
    }
}
