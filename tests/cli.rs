//! End-to-end tests for the headless evaluation commands.
//!
//! These run the real binary with `CRA_DB` pointed at a throwaway database, so
//! they cover argument handling, the schema migration on a fresh file, and the
//! report text a user actually sees — none of which a unit test reaches.

use std::path::{Path, PathBuf};
use std::process::Command;

fn work_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cra_cli_{}_{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create work dir");
    dir
}

/// Run the binary in `dir` with its own database.
fn run(dir: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_code-review-assistant"))
        .args(args)
        .current_dir(dir)
        .env("CRA_DB", dir.join("cra.db"))
        .output()
        .expect("run binary");
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

#[test]
fn help_lists_the_evaluation_commands() {
    let dir = work_dir("help");
    let (ok, text) = run(&dir, &["--help"]);
    assert!(ok, "{text}");
    for expected in ["export-corpus", "replay", "report"] {
        assert!(text.contains(expected), "help is missing {expected}: {text}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_unknown_command_fails_loudly() {
    let dir = work_dir("unknown");
    let (ok, text) = run(&dir, &["frobnicate"]);
    assert!(!ok, "an unknown command must not exit zero");
    assert!(text.contains("unknown command"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn exporting_from_an_empty_history_says_so_rather_than_writing_junk() {
    let dir = work_dir("empty");
    let (ok, text) = run(&dir, &["export-corpus"]);
    assert!(ok, "{text}");
    assert!(text.contains("wrote 0 labelled comment"), "{text}");
    assert!(text.contains("Nothing to export yet"), "the emptiness needs explaining: {text}");

    // It still writes a well-formed corpus, so a replay against it is a no-op
    // rather than a parse error.
    let corpus = std::fs::read_to_string(dir.join("corpus.json")).expect("corpus written");
    assert!(corpus.contains("\"entries\""), "{corpus}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn reporting_an_empty_history_states_what_it_cannot_know() {
    let dir = work_dir("report");
    let (ok, text) = run(&dir, &["report"]);
    assert!(ok, "{text}");
    // The two caveats that keep the numbers honest have to survive to the
    // output a user reads, not just live in the source.
    assert!(text.contains("self-agreement: unknown"), "{text}");
    assert!(text.contains("not with any ground truth"), "{text}");

    let written = std::fs::read_to_string(dir.join("report.txt")).expect("report written");
    assert_eq!(written.trim(), text.trim(), "the file and stdout should agree");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_replay_needs_a_corpus_and_says_which_flag() {
    let dir = work_dir("noflag");
    let (ok, text) = run(&dir, &["replay"]);
    assert!(!ok);
    assert!(text.contains("--corpus"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn reporting_a_replay_scores_it_against_the_corpus() {
    let dir = work_dir("score");
    // A hand-written corpus and replay: two comments, one model that agrees
    // with the reviewer on one of them.
    let corpus = r#"{
      "entries": [
        {
          "unit": {
            "file": "src/lib.rs", "lang": "Rust", "start_line": 2, "end_line": 2,
            "raw_lines": ["    // restates the code"], "indent": "    ",
            "style": {"Line": {"prefix": "//"}},
            "context": "", "hunk_header": "", "has_added": true
          },
          "action": "delete", "final_text": "", "source": "human-authored", "blinded": true
        },
        {
          "unit": {
            "file": "src/lib.rs", "lang": "Rust", "start_line": 9, "end_line": 9,
            "raw_lines": ["    // explains why"], "indent": "    ",
            "style": {"Line": {"prefix": "//"}},
            "context": "", "hunk_header": "", "has_added": true
          },
          "action": "keep", "final_text": "    // explains why", "source": "original",
          "blinded": true
        }
      ]
    }"#;
    let replay = r#"{
      "answers": [
        {"model": "m1", "entry": 0, "action": "delete", "comment": "",
         "latency_ms": 120, "error": null},
        {"model": "m1", "entry": 1, "action": "delete", "comment": "",
         "latency_ms": 80, "error": null}
      ]
    }"#;
    std::fs::write(dir.join("corpus.json"), corpus).unwrap();
    std::fs::write(dir.join("replay.json"), replay).unwrap();

    let (ok, text) =
        run(&dir, &["report", "--corpus", "corpus.json", "--results", "replay.json"]);
    assert!(ok, "{text}");
    assert!(text.contains("m1"), "{text}");
    // Agreed on one of two: the score must read 50%, not 100% or 0%.
    assert!(text.contains("50%"), "expected 50% agreement in:\n{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_malformed_corpus_is_rejected_with_the_file_named() {
    let dir = work_dir("malformed");
    std::fs::write(dir.join("corpus.json"), "{ not json").unwrap();
    std::fs::write(dir.join("replay.json"), r#"{"answers": []}"#).unwrap();
    let (ok, text) =
        run(&dir, &["report", "--corpus", "corpus.json", "--results", "replay.json"]);
    assert!(!ok);
    assert!(text.contains("corpus.json"), "the error should name the file: {text}");
    let _ = std::fs::remove_dir_all(&dir);
}
