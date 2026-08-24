//! Minimal unified-diff parser (output of `git diff --no-color`).

#[derive(Clone)]
pub struct HunkLine {
    pub origin: char, // ' ', '+', '-'
    pub text: String,
    pub new_lineno: Option<u32>,
    #[allow(dead_code)]
    pub old_lineno: Option<u32>,
}

#[derive(Clone)]
pub struct Hunk {
    pub header: String,
    #[allow(dead_code)]
    pub new_start: u32,
    pub lines: Vec<HunkLine>,
}

#[derive(Clone)]
pub struct DiffFile {
    pub path: String,
    pub hunks: Vec<Hunk>,
}

impl DiffFile {
    /// Lines this diff adds and removes in the file, as `(added, removed)`.
    /// Context lines are not counted, however many of them the diff was asked
    /// for — this is the `+/-` a reviewer expects, not the size of the hunks.
    pub fn line_changes(&self) -> (usize, usize) {
        let mut added = 0;
        let mut removed = 0;
        for line in self.hunks.iter().flat_map(|h| &h.lines) {
            match line.origin {
                '+' => added += 1,
                '-' => removed += 1,
                _ => {}
            }
        }
        (added, removed)
    }
}

pub fn parse(diff: &str) -> Vec<DiffFile> {
    let mut files: Vec<DiffFile> = Vec::new();
    let mut cur_file: Option<DiffFile> = None;
    let mut cur_hunk: Option<Hunk> = None;
    let mut old_no = 0u32;
    let mut new_no = 0u32;

    let close_hunk = |file: &mut Option<DiffFile>, hunk: &mut Option<Hunk>| {
        if let (Some(f), Some(h)) = (file.as_mut(), hunk.take()) {
            f.hunks.push(h);
        }
    };

    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            close_hunk(&mut cur_file, &mut cur_hunk);
            if let Some(f) = cur_file.take() {
                if !f.hunks.is_empty() {
                    files.push(f);
                }
            }
            cur_file = None;
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            let path = rest.trim();
            if path == "/dev/null" {
                cur_file = None;
            } else {
                let path = path.strip_prefix("b/").unwrap_or(path);
                cur_file = Some(DiffFile { path: path.to_string(), hunks: Vec::new() });
            }
        } else if line.starts_with("@@") {
            close_hunk(&mut cur_file, &mut cur_hunk);
            if cur_file.is_none() {
                continue;
            }
            // @@ -old_start[,old_len] +new_start[,new_len] @@ context
            let parse_start = |tok: &str| -> u32 {
                tok.trim_start_matches(['-', '+'])
                    .split(',')
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(1)
            };
            let mut parts = line.split_whitespace();
            parts.next(); // @@
            let old_tok = parts.next().unwrap_or("-1");
            let new_tok = parts.next().unwrap_or("+1");
            old_no = parse_start(old_tok);
            new_no = parse_start(new_tok);
            cur_hunk = Some(Hunk {
                header: line.to_string(),
                new_start: new_no,
                lines: Vec::new(),
            });
        } else if let Some(h) = cur_hunk.as_mut() {
            let (origin, text) = match line.chars().next() {
                Some('+') => ('+', &line[1..]),
                Some('-') => ('-', &line[1..]),
                Some(' ') => (' ', &line[1..]),
                Some('\\') => continue, // "\ No newline at end of file"
                _ => {
                    // end of hunk (e.g. next file header handled above)
                    continue;
                }
            };
            let (old_lineno, new_lineno) = match origin {
                '+' => {
                    let n = new_no;
                    new_no += 1;
                    (None, Some(n))
                }
                '-' => {
                    let n = old_no;
                    old_no += 1;
                    (Some(n), None)
                }
                _ => {
                    let (o, n) = (old_no, new_no);
                    old_no += 1;
                    new_no += 1;
                    (Some(o), Some(n))
                }
            };
            h.lines.push(HunkLine { origin, text: text.to_string(), new_lineno, old_lineno });
        }
    }
    close_hunk(&mut cur_file, &mut cur_hunk);
    if let Some(f) = cur_file.take() {
        if !f.hunks.is_empty() {
            files.push(f);
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "\
diff --git a/src/lib.rs b/src/lib.rs
index 111..222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,4 +1,6 @@
 fn main() {
+    // Increment the counter by one to increment it
+    counter += 1;
     println!(\"hi\");
-    let x = 1;
+    let x = 2;
 }
";

    #[test]
    fn parses_files_hunks_and_line_numbers() {
        let files = parse(DIFF);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "src/lib.rs");
        assert_eq!(files[0].hunks.len(), 1);
        let h = &files[0].hunks[0];
        assert_eq!(h.new_start, 1);
        let added: Vec<_> = h.lines.iter().filter(|l| l.origin == '+').collect();
        assert_eq!(added.len(), 3);
        assert_eq!(added[0].new_lineno, Some(2));
        assert_eq!(added[0].text, "    // Increment the counter by one to increment it");
        // context line after the removal keeps correct numbering
        let last = h.lines.iter().rev().find(|l| l.origin == ' ').unwrap();
        assert_eq!(last.new_lineno, Some(6));
    }

    #[test]
    fn line_changes_counts_only_the_changed_lines() {
        // The diff is generated with context, so the count has to ignore it:
        // this hunk is six lines of which three are added and one removed.
        assert_eq!(parse(DIFF)[0].line_changes(), (3, 1));
    }

    #[test]
    fn skips_deleted_files() {
        let diff = "\
diff --git a/gone.rs b/gone.rs
--- a/gone.rs
+++ /dev/null
@@ -1,2 +0,0 @@
-// old
-fn f() {}
";
        assert!(parse(diff).is_empty());
    }
}
