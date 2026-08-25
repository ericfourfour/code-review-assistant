#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agycli;
mod app;
mod codeunits;
mod comments;
mod db;
mod diffparse;
mod discover;
mod eval;
mod findings;
mod gitio;
mod highlight;
mod models;
mod notes;
mod procs;
mod profile;
mod review;
mod scopes;
mod settings;
#[cfg(test)]
mod testkit;
mod triage;
mod ui;
mod units;

use std::path::PathBuf;

const USAGE: &str = "\
Code Review Assistant — review AI-written comments and code, one unit at a time.

    cra                                  open the review window (default)

Evaluation — measuring the models against your own judgements:

    cra export-corpus [--out FILE] [--limit N]
        Write the comments you have already reviewed, with your verdicts, to a
        corpus file. Defaults: --out corpus.json, --limit 500.

    cra replay --corpus FILE [--out FILE]
        Ask every enabled model about every comment in the corpus and write the
        answers. Default: --out replay.json.

    cra report [--corpus FILE --results FILE] [--out FILE]
        Score a replay against the corpus, or with no arguments score the whole
        review history straight out of the database. Default: --out report.txt.

Output also goes to stdout, which a release build on Windows has no console
for; the --out file is the reliable channel there.
";

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        std::process::exit(run_cli(&args));
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1480.0, 940.0])
            .with_min_inner_size([1100.0, 700.0])
            .with_title("Code Review Assistant"),
        ..Default::default()
    };
    eframe::run_native(
        "code-review-assistant",
        options,
        Box::new(|cc| Ok(Box::new(app::CraApp::new(cc)))),
    )
}

/// Value of `--name`, if present.
fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    let at = args.iter().position(|a| a == name)?;
    args.get(at + 1)
        .map(|s| s.as_str())
        .filter(|v| !v.starts_with("--"))
}

fn path_flag(args: &[String], name: &str, default: &str) -> PathBuf {
    PathBuf::from(flag(args, name).unwrap_or(default))
}

/// Report to both stdout and a file. A release build on Windows is a windowed
/// subsystem binary with no console attached, so stdout goes nowhere there and
/// the file is what the user actually reads.
fn emit(text: &str, path: &std::path::Path) -> Result<(), String> {
    print!("{text}");
    std::fs::write(path, text).map_err(|e| format!("write {}: {e}", path.display()))
}

fn run_cli(args: &[String]) -> i32 {
    match execute(args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e}");
            let _ = std::fs::write("cra-error.txt", format!("{e}\n"));
            1
        }
    }
}

fn execute(args: &[String]) -> Result<(), String> {
    match args[0].as_str() {
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(())
        }
        "export-corpus" => {
            let db = db::Db::open()?;
            let limit: usize = flag(args, "--limit")
                .and_then(|v| v.parse().ok())
                .unwrap_or(500);
            let corpus = eval::Corpus::from_db(&db, limit);
            let out = path_flag(args, "--out", "corpus.json");
            corpus.save(&out)?;
            println!(
                "wrote {} labelled comment(s) to {}",
                corpus.entries.len(),
                out.display()
            );
            if corpus.entries.is_empty() {
                println!(
                    "Nothing to export yet: a comment enters the corpus only once you have\n\
                     reviewed it, and only reviews recorded by a build that stores the unit\n\
                     alongside the verdict are eligible."
                );
            }
            Ok(())
        }
        "replay" => {
            let corpus_path = flag(args, "--corpus")
                .ok_or("replay needs --corpus FILE (make one with export-corpus)")?;
            let corpus = eval::Corpus::load(&PathBuf::from(corpus_path))?;
            let db = db::Db::open()?;
            let settings = settings::Settings::load(&db);
            let enabled = settings.enabled_models().len();
            if enabled == 0 {
                return Err("no models are enabled in settings".into());
            }
            println!(
                "replaying {} comment(s) against {enabled} model(s)...",
                corpus.entries.len()
            );
            let results = eval::replay(&settings, &corpus, |done, total, model| {
                println!("  [{done}/{total}] {model}");
            });
            let out = path_flag(args, "--out", "replay.json");
            let text = serde_json::to_string_pretty(&results)
                .map_err(|e| format!("serialize results: {e}"))?;
            std::fs::write(&out, text).map_err(|e| format!("write {}: {e}", out.display()))?;
            println!(
                "wrote {} answer(s) to {}",
                results.answers.len(),
                out.display()
            );
            Ok(())
        }
        "report" => {
            let out = path_flag(args, "--out", "report.txt");
            match (flag(args, "--corpus"), flag(args, "--results")) {
                (Some(c), Some(r)) => {
                    let corpus = eval::Corpus::load(&PathBuf::from(c))?;
                    let text = std::fs::read_to_string(r).map_err(|e| format!("read {r}: {e}"))?;
                    let results: eval::ReplayResults =
                        serde_json::from_str(&text).map_err(|e| format!("parse {r}: {e}"))?;
                    emit(&eval::Report::from_replay(&corpus, &results).render(), &out)
                }
                (None, None) => {
                    let db = db::Db::open()?;
                    emit(&eval::Report::from_db(&db).render(), &out)
                }
                _ => Err("report needs both --corpus and --results, or neither".into()),
            }
        }
        other => Err(format!("unknown command {other:?}\n\n{USAGE}")),
    }
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flags_are_read_by_name() {
        let args = argv(&["replay", "--corpus", "c.json", "--out", "r.json"]);
        assert_eq!(flag(&args, "--corpus"), Some("c.json"));
        assert_eq!(flag(&args, "--out"), Some("r.json"));
        assert_eq!(flag(&args, "--missing"), None);
        assert_eq!(
            path_flag(&args, "--nope", "default.json"),
            PathBuf::from("default.json")
        );
    }

    #[test]
    fn a_flag_with_no_value_does_not_swallow_the_next_flag() {
        // `replay --corpus --out x` must not take "--out" as the corpus path,
        // which would surface much later as a confusing file-read error.
        let args = argv(&["replay", "--corpus", "--out", "r.json"]);
        assert_eq!(flag(&args, "--corpus"), None);
        let trailing = argv(&["replay", "--corpus"]);
        assert_eq!(flag(&trailing, "--corpus"), None);
    }

    #[test]
    fn unknown_commands_explain_themselves() {
        let err = execute(&argv(&["frobnicate"])).unwrap_err();
        assert!(err.contains("unknown command"), "{err}");
        assert!(
            err.contains("export-corpus"),
            "the error should show usage: {err}"
        );
    }

    #[test]
    fn replay_without_a_corpus_says_so() {
        assert!(execute(&argv(&["replay"]))
            .unwrap_err()
            .contains("--corpus"));
    }

    #[test]
    fn report_needs_both_halves_or_neither() {
        assert!(execute(&argv(&["report", "--corpus", "c.json"]))
            .unwrap_err()
            .contains("both"));
    }
}
