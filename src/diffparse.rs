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
    pub deleted: bool,
}

/// Decode the C-style quoting Git applies to paths containing non-ASCII or
/// otherwise special bytes. Diff headers are line-oriented, so quoted paths
/// are the only reliable way for Git to carry those bytes here.
fn git_path(raw: &str) -> String {
    let raw = raw.trim();
    let Some(inner) = raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        return raw.to_string();
    };
    let bytes = inner.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            decoded.push(bytes[i]);
            i += 1;
            continue;
        }
        i += 1;
        if i >= bytes.len() {
            decoded.push(b'\\');
            break;
        }
        match bytes[i] {
            b'a' => decoded.push(0x07),
            b'b' => decoded.push(0x08),
            b't' => decoded.push(b'\t'),
            b'n' => decoded.push(b'\n'),
            b'v' => decoded.push(0x0b),
            b'f' => decoded.push(0x0c),
            b'r' => decoded.push(b'\r'),
            b'\\' => decoded.push(b'\\'),
            b'"' => decoded.push(b'"'),
            b'0'..=b'7' => {
                let mut value = 0u8;
                let mut digits = 0;
                while i < bytes.len() && digits < 3 && matches!(bytes[i], b'0'..=b'7') {
                    value = value.wrapping_mul(8).wrapping_add(bytes[i] - b'0');
                    i += 1;
                    digits += 1;
                }
                decoded.push(value);
                continue;
            }
            other => decoded.push(other),
        }
        i += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

pub fn parse(diff: &str) -> Vec<DiffFile> {
    let mut files: Vec<DiffFile> = Vec::new();
    let mut cur_file: Option<DiffFile> = None;
    let mut cur_hunk: Option<Hunk> = None;
    let mut old_no = 0u32;
    let mut new_no = 0u32;
    let mut old_path: Option<String> = None;

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
            old_path = None;
        } else if let Some(rest) = line.strip_prefix("--- ") {
            let path = git_path(rest);
            if path != "/dev/null" {
                old_path = Some(path.strip_prefix("a/").unwrap_or(&path).to_string());
            }
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            let path = git_path(rest);
            if path == "/dev/null" {
                cur_file = old_path.clone().map(|path| DiffFile {
                    path,
                    hunks: Vec::new(),
                    deleted: true,
                });
            } else {
                let path = path.strip_prefix("b/").unwrap_or(&path);
                cur_file = Some(DiffFile {
                    path: path.to_string(),
                    hunks: Vec::new(),
                    deleted: false,
                });
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
            h.lines.push(HunkLine {
                origin,
                text: text.to_string(),
                new_lineno,
                old_lineno,
            });
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
        assert_eq!(
            added[0].text,
            "    // Increment the counter by one to increment it"
        );
        // context line after the removal keeps correct numbering
        let last = h.lines.iter().rev().find(|l| l.origin == ' ').unwrap();
        assert_eq!(last.new_lineno, Some(6));
    }

    #[test]
    fn retains_deleted_files_with_their_old_path() {
        let diff = "\
diff --git a/gone.rs b/gone.rs
--- a/gone.rs
+++ /dev/null
@@ -1,2 +0,0 @@
-// old
-fn f() {}
";
        let files = parse(diff);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "gone.rs");
        assert!(files[0].deleted);
    }

    #[test]
    fn decodes_git_quoted_paths_before_extension_matching() {
        let diff = "\
diff --git \"a/src/\\303\\251.rs\" \"b/src/\\303\\251.rs\"
--- \"a/src/\\303\\251.rs\"
+++ \"b/src/\\303\\251.rs\"
@@ -1 +1 @@
-// old
+// new
";
        let files = parse(diff);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "src/é.rs");
    }
}
