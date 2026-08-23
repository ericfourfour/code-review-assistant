//! Risk triage: a fast, local, deterministic score per unit so the walk can
//! visit the changes most worth human attention first.
//!
//! Deliberately *not* a model call. Triage runs at plan build, synchronously,
//! for every unit on the branch — a model pass there would freeze the app for
//! minutes and add a failure mode to the one step that must always work. The
//! models still judge every unit when it is reached; this only decides the
//! order, so cheap static signals are enough: what kind of change it is, how
//! big, whether it deletes code, and whether it touches the vocabulary of
//! things that go wrong (locks, secrets, unsafe blocks, subprocesses…).
//! The score is shown with its reasons, so the ordering is inspectable
//! rather than an oracle.

use crate::units::ReviewUnit;

pub struct Risk {
    /// 0–100. Ordering signal, not a judgement.
    pub score: u32,
    /// Why the score is what it is, in the order the points were added.
    pub reasons: Vec<String>,
}

/// One vocabulary bucket: a label, the points it adds, and the substrings
/// (matched case-insensitively) that trigger it. A bucket fires at most once
/// per unit — five `unwrap`s are not five times as risky as one.
struct Bucket {
    label: &'static str,
    points: u32,
    needles: &'static [&'static str],
}

const BUCKETS: &[Bucket] = &[
    Bucket {
        label: "security-sensitive terms",
        points: 15,
        needles: &["passw", "secret", "credential", "api_key", "apikey", "private_key",
                   "authent", "authoriz", "permission", "crypt", "certif"],
    },
    Bucket {
        label: "concurrency",
        points: 12,
        needles: &["mutex", "rwlock", ".lock(", "atomic", "thread", "spawn", "channel",
                   "semaphore", "concurren", "race "],
    },
    Bucket {
        label: "panics or unsafe code",
        points: 12,
        needles: &["unsafe", ".unwrap(", ".expect(", "panic!", "unimplemented!", "todo!",
                   "unreachable!", "assert!", "raise ", "throw "],
    },
    Bucket {
        label: "external effects",
        points: 10,
        needles: &["subprocess", "command", "exec(", "system(", "eval(", "shell",
                   "sql", "query(", "http", "request(", "socket", "std::fs", "os.remove",
                   "unlink", "rmdir", "delete", "truncate", "drop table", "migrat"],
    },
    Bucket {
        label: "unfinished-work markers",
        points: 8,
        needles: &["todo", "fixme", "hack", "xxx", "workaround", "temporar"],
    },
];

/// Path fragments that mark lower-stakes files. Tests matter, but a bug in a
/// test rarely ships; halving keeps them in the walk without letting a big
/// test file outrank the code it tests.
fn is_test_path(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.split(['/', '\\']).any(|seg| {
        seg == "test" || seg == "tests" || seg == "spec" || seg == "__tests__" || seg == "testdata"
    }) || p.contains("_test.")
        || p.contains(".test.")
        || p.contains(".spec.")
}

/// Documentation. Prose *about* locks and secrets fires the vocabulary
/// buckets exactly as hard as code that touches them — dogfooding this tool
/// on its own branch put the README at risk 100 for that reason — so doc
/// files are cut to a third rather than letting their subject matter outrank
/// the code it describes.
fn is_doc_path(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    [".md", ".markdown", ".rst", ".txt", ".adoc"].iter().any(|ext| p.ends_with(ext))
}

pub fn assess(unit: &ReviewUnit) -> Risk {
    let mut score: u32 = 0;
    let mut reasons: Vec<String> = Vec::new();
    let add = |score: &mut u32, reasons: &mut Vec<String>, pts: u32, why: String| {
        *score += pts;
        reasons.push(format!("+{pts} {why}"));
    };

    match unit {
        ReviewUnit::Comment(_) => add(&mut score, &mut reasons, 5, "comment change".into()),
        ReviewUnit::Code(u) => {
            add(&mut score, &mut reasons, 25, "code change".into());
            if u.scope.is_none() {
                add(&mut score, &mut reasons, 8, "no enclosing scope found".into());
            }
        }
    }

    let changed = match unit {
        ReviewUnit::Comment(u) => u.raw_lines.len(),
        ReviewUnit::Code(u) => u.changed_lines.len().max(1),
    };
    let size_pts = (2 * changed as u32).min(20);
    if size_pts > 0 {
        add(&mut score, &mut reasons, size_pts, format!("{changed} changed line(s)"));
    }

    // Removed lines appear in the context as `-` rows; deleting code is a
    // classic place for a regression to hide.
    let removed = unit.context().lines().filter(|l| l.starts_with('-')).count();
    if removed > 0 {
        let pts = (3 * removed as u32).min(12);
        add(&mut score, &mut reasons, pts, format!("removes {removed} line(s)"));
    }

    let haystack = unit.raw_lines().join("\n").to_ascii_lowercase();
    for bucket in BUCKETS {
        if bucket.needles.iter().any(|n| haystack.contains(n)) {
            add(&mut score, &mut reasons, bucket.points, bucket.label.to_string());
        }
    }

    if is_doc_path(unit.file()) {
        score /= 3;
        reasons.push("cut to a third: documentation".into());
    } else if is_test_path(unit.file()) {
        score /= 2;
        reasons.push("halved: test code".into());
    }

    Risk { score: score.min(100), reasons }
}

/// Order units riskiest-first (stable: ties keep their line order), and files
/// by the riskiest unit they contain. Called on the assembled plan when the
/// triage setting is on; the offset bookkeeping tolerates any walk order.
pub fn order_riskiest_first(files: &mut Vec<(String, Vec<ReviewUnit>)>) {
    for (_, units) in files.iter_mut() {
        let mut scored: Vec<(u32, ReviewUnit)> =
            units.drain(..).map(|u| (assess(&u).score, u)).collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.start_line().cmp(&b.1.start_line())));
        units.extend(scored.into_iter().map(|(_, u)| u));
    }
    // Units are now sorted riskiest-first, so a file's risk is its first unit.
    let mut scored: Vec<(u32, (String, Vec<ReviewUnit>))> = files
        .drain(..)
        .map(|f| (f.1.first().map(|u| assess(u).score).unwrap_or(0), f))
        .collect();
    // Stable, so equally risky files keep their diff order.
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    files.extend(scored.into_iter().map(|(_, f)| f));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codeunits::CodeUnit;
    use crate::comments::{CommentStyle, CommentUnit};

    fn comment(file: &str, line: u32, text: &str) -> ReviewUnit {
        ReviewUnit::Comment(CommentUnit {
            file: file.into(),
            lang: "Rust".into(),
            start_line: line,
            end_line: line,
            raw_lines: vec![text.to_string()],
            indent: String::new(),
            style: CommentStyle::Line { prefix: "//".into() },
            context: String::new(),
            hunk_header: String::new(),
            has_added: true,
        })
    }

    fn code(file: &str, line: u32, lines: &[&str], scope: Option<&str>, context: &str) -> ReviewUnit {
        ReviewUnit::Code(CodeUnit {
            file: file.into(),
            lang: "Rust".into(),
            start_line: line,
            end_line: line + lines.len() as u32 - 1,
            raw_lines: lines.iter().map(|s| s.to_string()).collect(),
            changed_lines: (line..line + lines.len() as u32).collect(),
            scope: scope.map(|s| s.to_string()),
            context: context.into(),
            hunk_header: String::new(),
            deleted_file: false,
        })
    }

    #[test]
    fn code_outranks_comments_and_risky_vocabulary_outranks_plain() {
        let plain_comment = assess(&comment("src/a.rs", 1, "// explains the retry"));
        let plain_code = assess(&code("src/a.rs", 1, &["    x += 1;"], Some("fn f"), ""));
        let scary_code = assess(&code(
            "src/a.rs",
            1,
            &["    unsafe { *ptr = 1; }", "    mutex.lock();"],
            Some("fn f"),
            "",
        ));
        assert!(plain_code.score > plain_comment.score);
        assert!(scary_code.score > plain_code.score);
        assert!(scary_code.reasons.iter().any(|r| r.contains("unsafe")), "{:?}", scary_code.reasons);
        assert!(scary_code.reasons.iter().any(|r| r.contains("concurrency")));
    }

    #[test]
    fn removals_and_missing_scope_raise_the_score() {
        let with_removal = assess(&code(
            "src/a.rs",
            5,
            &["    keep();"],
            None,
            ">    5| keep();\n-     | gone();\n",
        ));
        let without = assess(&code("src/a.rs", 5, &["    keep();"], Some("fn f"), ">    5| keep();\n"));
        assert!(with_removal.score > without.score);
        assert!(with_removal.reasons.iter().any(|r| r.contains("removes 1 line")));
        assert!(with_removal.reasons.iter().any(|r| r.contains("no enclosing scope")));
    }

    #[test]
    fn prose_about_risky_things_does_not_outrank_the_code() {
        // Found by running the tool on its own branch: the README hit risk
        // 100 because it *describes* the vocabulary buckets.
        let prose = &["This guards secrets with a mutex; unsafe code panics on TODO."];
        let doc = assess(&code("README.md", 1, prose, None, ""));
        let src = assess(&code("src/vault.rs", 1, prose, None, ""));
        assert!(doc.score < src.score / 2, "doc {} vs code {}", doc.score, src.score);
        assert!(doc.reasons.iter().any(|r| r.contains("documentation")), "{:?}", doc.reasons);
    }

    #[test]
    fn test_files_are_halved_and_the_score_is_bounded() {
        let prod = assess(&code("src/auth.rs", 1, &["    check_password(secret);"], Some("fn f"), ""));
        let test = assess(&code("tests/auth.rs", 1, &["    check_password(secret);"], Some("fn f"), ""));
        assert_eq!(test.score, prod.score / 2);
        assert!(test.reasons.iter().any(|r| r.contains("test code")));

        let everything = assess(&code(
            "src/kitchen.rs",
            1,
            &["unsafe passw mutex subprocess todo fixme; ".repeat(30).as_str()],
            None,
            &"-     | gone\n".repeat(10),
        ));
        assert!(everything.score <= 100, "{}", everything.score);
    }
    #[test]
    fn scores_are_deterministic() {
        let u = code("src/a.rs", 1, &["    spawn(worker);"], Some("fn f"), "");
        assert_eq!(assess(&u).score, assess(&u).score);
    }

    #[test]
    fn ordering_walks_riskiest_first_but_keeps_line_order_on_ties() {
        let mut files = vec![
            (
                "src/mild.rs".to_string(),
                vec![comment("src/mild.rs", 2, "// a"), comment("src/mild.rs", 9, "// b")],
            ),
            (
                "src/hot.rs".to_string(),
                vec![
                    comment("src/hot.rs", 3, "// note"),
                    code("src/hot.rs", 40, &["    unsafe { go(); }"], Some("fn f"), ""),
                ],
            ),
        ];
        order_riskiest_first(&mut files);
        assert_eq!(files[0].0, "src/hot.rs", "the file with the riskiest unit leads");
        assert!(files[0].1[0].is_code(), "within a file, the risky code unit leads");
        assert_eq!(files[0].1[1].start_line(), 3);
        // Equal-score units stay in line order, so the tie-break is stable.
        assert_eq!(files[1].1[0].start_line(), 2);
        assert_eq!(files[1].1[1].start_line(), 9);
    }
}
