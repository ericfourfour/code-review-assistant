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

#[derive(Clone)]
pub struct Suggestion {
    pub action: Action,
    pub comment: String,
    pub justification: String,
    pub latency_ms: i64,
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

fn run_model(slot: &ModelSlot, prompt: &str, timeout: Duration) -> Result<Suggestion, String> {
    let (argv, via_stdin) = build_argv(&slot.command, prompt, slot);
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

    let raw = extract_json(&stdout)
        .or_else(|| extract_json(&stderr))
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

pub fn spawn_model(
    seq: u64,
    slot_idx: usize,
    slot: ModelSlot,
    prompt: String,
    timeout_secs: u64,
    send: impl FnOnce(CandidateMsg) + Send + 'static,
    ctx: egui::Context,
) {
    std::thread::spawn(move || {
        let result = run_model(&slot, &prompt, Duration::from_secs(timeout_secs.max(5)));
        send(CandidateMsg { seq, slot_idx, model: slot.name.clone(), result });
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
}
