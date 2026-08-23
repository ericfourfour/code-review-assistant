//! Invoke reviewer model CLIs (claude / codex / agy / anything configured)
//! on background threads and parse their JSON verdicts.

use serde::Deserialize;
use std::io::Write;
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::settings::ModelSlot;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Keep,
    Rewrite,
    Delete,
}

impl Action {
    pub fn label(self) -> &'static str {
        match self {
            Action::Keep => "KEEP",
            Action::Rewrite => "REWRITE",
            Action::Delete => "DELETE",
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Keep => "keep",
            Action::Rewrite => "rewrite",
            Action::Delete => "delete",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Suggestion {
    pub action: Action,
    pub comment: String,
    pub justification: String,
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

/// The prompt for a follow-up turn. The conversation itself lives in the
/// CLI's own session, so only the new message and the answer format go over.
pub fn followup_prompt(message: &str) -> String {
    format!(
        "{}\n\nAnswer with JSON only:\n\
{{\"action\":\"keep|rewrite|delete\",\"comment\":\"replacement text if rewrite, else empty\",\"justification\":\"one short sentence\"}}",
        message.trim()
    )
}

#[derive(Deserialize)]
struct RawSuggestion {
    action: String,
    #[serde(default)]
    comment: String,
    #[serde(default)]
    justification: String,
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

/// Split the command template into argv, substituting `{prompt}` and
/// appending the model selection. Returns (argv, prompt_via_stdin).
///
/// Piping is the default: a template without `{prompt}` gets the prompt on
/// stdin, which sidesteps both the command-line length limit and Windows'
/// refusal to pass multi-line arguments to a `.cmd` shim.
fn build_argv(template: &str, prompt: &str, slot: &ModelSlot) -> (Vec<String>, bool) {
    let mut argv: Vec<String> = Vec::new();
    let mut used_placeholder = false;
    for tok in template.split_whitespace() {
        if tok == "{prompt}" {
            argv.push(prompt.to_string());
            used_placeholder = true;
        } else {
            argv.push(tok.to_string());
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

/// Pull the verdict out of whatever the CLI printed. Asking a CLI for
/// machine-readable output (needed to recover the session id) wraps the
/// model's answer in the CLI's own envelope, so the verdict arrives as an
/// escaped string inside it rather than as bare text.
fn extract_verdict(output: &str) -> Option<RawSuggestion> {
    if let Some(raw) = extract_json(output) {
        return Some(raw);
    }
    let mut strings = Vec::new();
    for doc in json_documents(output) {
        collect_strings(&doc, &mut strings);
    }
    strings.iter().rev().find_map(|s| extract_json(s))
}

fn extract_json(output: &str) -> Option<RawSuggestion> {
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
                        if let Ok(raw) = serde_json::from_str::<RawSuggestion>(&output[s..=i]) {
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

/// Run one model CLI. Returns the parsed verdict and everything the process
/// printed (kept for the transcript and the prompt inspector).
/// `command` is the already-resolved template — the slot's opening command
/// or its resume command with the session id substituted in.
fn run_model(
    slot: &ModelSlot,
    command: &str,
    prompt: &str,
    timeout: Duration,
) -> (Result<Suggestion, String>, String) {
    let mut raw_output = String::new();
    let result = run_inner(slot, command, prompt, timeout, &mut raw_output);
    (result, raw_output)
}

fn run_inner(
    slot: &ModelSlot,
    command: &str,
    prompt: &str,
    timeout: Duration,
    raw_output: &mut String,
) -> Result<Suggestion, String> {
    let (argv, via_stdin) = build_argv(command, prompt, slot);
    if argv.is_empty() {
        return Err("empty command template".into());
    }
    let started = Instant::now();
    let mut cmd = crate::gitio::hidden_command(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(if via_stdin { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
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
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let latency_ms = started.elapsed().as_millis() as i64;
    raw_output.push_str(stdout.trim());
    if !stderr.trim().is_empty() {
        if !raw_output.is_empty() {
            raw_output.push('\n');
        }
        raw_output.push_str("[stderr] ");
        raw_output.push_str(stderr.trim());
    }

    let raw = extract_verdict(&stdout)
        .or_else(|| extract_verdict(&stderr))
        .ok_or_else(|| {
            let snippet: String = stdout.chars().take(400).collect();
            format!("no JSON verdict in output: {}", snippet.trim())
        })?;
    let action = match raw.action.to_ascii_lowercase().as_str() {
        "keep" => Action::Keep,
        "rewrite" => Action::Rewrite,
        "delete" | "remove" => Action::Delete,
        other => return Err(format!("unknown action {other:?}")),
    };
    Ok(Suggestion {
        action,
        comment: raw.comment,
        justification: raw.justification,
        latency_ms,
    })
}

/// Run a model outside the review loop — the evaluation replay uses this so a
/// measurement goes through exactly the same path as a real review.
pub fn run_for_eval(
    slot: &ModelSlot,
    command: &str,
    prompt: &str,
    timeout: Duration,
) -> (Result<Suggestion, String>, String) {
    run_model(slot, command, prompt, timeout)
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_model(
    seq: u64,
    slot_idx: usize,
    slot: ModelSlot,
    command: String,
    prompt: String,
    timeout_secs: u64,
    send: impl FnOnce(CandidateMsg) + Send + 'static,
    ctx: egui::Context,
) {
    std::thread::spawn(move || {
        let (result, raw) =
            run_model(&slot, &command, &prompt, Duration::from_secs(timeout_secs.max(5)));
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
        let (argv, stdin) = build_argv("claude -p {prompt}", "hello", &bare);
        assert_eq!(argv, vec!["claude", "-p", "hello"]);
        assert!(!stdin);
        let (argv, stdin) = build_argv("codex exec", "hello", &bare);
        assert_eq!(argv, vec!["codex", "exec"]);
        assert!(stdin);
    }

    #[test]
    fn model_and_effort_trail_so_they_never_split_a_flag_from_its_value() {
        let (argv, _) = build_argv("claude -p", "hi", &slot("haiku", "--model", "low", "--effort"));
        assert_eq!(argv, vec!["claude", "-p", "--model", "haiku", "--effort", "low"]);
        // `agy -p {prompt}` passes the prompt as -p's value, so the flags must
        // land after it — inserting ahead would make -p consume "--model".
        let (argv, _) =
            build_argv("agy -p {prompt}", "hi", &slot("gemini-3.7-flash-low", "--model", "", ""));
        assert_eq!(argv, vec!["agy", "-p", "hi", "--model", "gemini-3.7-flash-low"]);
        // codex routes effort through a config override rather than a flag
        let (argv, _) = build_argv(
            "codex exec",
            "hi",
            &slot("gpt-5.6-luna", "--model", "model_reasoning_effort=low", "-c"),
        );
        assert_eq!(
            argv,
            vec!["codex", "exec", "--model", "gpt-5.6-luna", "-c", "model_reasoning_effort=low"]
        );
        // empty values leave argv untouched
        let (argv, _) = build_argv("claude -p", "hi", &slot("  ", "--model", " ", "--effort"));
        assert_eq!(argv, vec!["claude", "-p"]);
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
    fn followup_prompt_carries_the_message_and_the_format() {
        let p = followup_prompt("  too wordy — one line?  ");
        assert!(p.starts_with("too wordy — one line?"));
        assert!(p.contains("\"action\""));
        assert!(p.trim_end().ends_with("}"));
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
        run_model(slot, &command, prompt, Duration::from_secs(30))
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
        let (res, _) = run_model(&slot, &slot.command, "hi", Duration::from_secs(1));
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
        let (res2, _) = run_model(&slot, &resume, &followup_prompt("why?"), Duration::from_secs(30));
        assert!(res2.is_ok());
        assert!(second.argv_seen().contains("resume sess-42"), "{}", second.argv_seen());
        assert!(second.stdin_seen().contains("why?"), "{}", second.stdin_seen());
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

    fn verdict_prompt(extra: &str) -> String {
        format!(
            "File: src/lib.rs (Rust)\n\n\
>    2|     // Increment the counter by one\n \
    3|     counter += 1;\n\n\
Review the comment marked with '>' (lines 2-2). \
Should it be kept, rewritten, or deleted?{extra}\n\
Answer with JSON only:\n\
{{\"action\":\"keep|rewrite|delete\",\"comment\":\"replacement text if rewrite, else empty\",\"justification\":\"one short sentence\"}}"
        )
    }

    /// Two real turns per configured CLI, resuming by session id between them.
    /// Exercises argv building, PATHEXT resolution, stdin piping, verdict
    /// extraction from each CLI's JSON envelope, session-id capture, and the
    /// resume command. The second turn asks for a number that only exists in
    /// the first turn, so a broken session shows up as a wrong answer rather
    /// than a passing test.
    ///
    /// Ignored by default; costs six model calls:
    ///     cargo test -- --ignored --nocapture
    #[test]
    #[ignore]
    fn configured_clis_hold_a_session_across_two_turns() {
        let timeout = Duration::from_secs(180);
        let mut failures: Vec<String> = Vec::new();

        for (_, slot) in Settings::default().enabled_models() {
            // Turn 1 — either we name the session or the CLI reports one.
            let (command, mut session) = if slot.session_key.trim().is_empty() {
                let id = uuid::Uuid::new_v4().to_string();
                (slot.command.replace("{session}", &id), Some(id))
            } else {
                (slot.command.clone(), None)
            };
            let first = verdict_prompt(" Also remember the number 4291 for later.");
            let (res, raw) = run_model(&slot, &command, &first, timeout);
            match &res {
                Ok(v) => println!("{:>6} turn 1: {} — {}", slot.name, v.action.label(), v.justification),
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
                "What number did I ask you to remember? Put just that number in \"justification\".",
            );
            let (res2, raw2) = run_model(&slot, &resume, &second, timeout);
            match res2 {
                Ok(v) => {
                    println!("{:>6} turn 2: justification = {}", slot.name, v.justification);
                    if !v.justification.contains("4291") {
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
        assert!(failures.is_empty(), "session round-trip failed:\n  {}", failures.join("\n  "));
    }
}
