//! The one place that draws source code.
//!
//! Two rules hold everywhere code appears. It is syntax-coloured, through the
//! grammars in [`crate::highlight`]. And it is never wrapped: indentation is
//! how code says what encloses what, and a wrapped row drops the leading
//! space that carries it, so long lines scroll sideways instead.

use std::sync::Arc;

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId, Galley, TextStyle, Ui};

/// One display row: a gutter (diff marker and line number, or nothing), the
/// code itself, and how to colour what the grammar does not claim.
pub struct Line {
    pub gutter: String,
    pub gutter_color: Color32,
    pub text: String,
    pub base: Color32,
    pub background: Option<Color32>,
    /// Whether this row is part of the text the parser sees. Removed lines
    /// are not: they do not exist in the file being reviewed, and feeding
    /// them in would break the structure the parse is there to find.
    pub syntax: bool,
}

impl Line {
    pub fn code(text: impl Into<String>, base: Color32) -> Self {
        Self {
            gutter: String::new(),
            gutter_color: base,
            text: text.into(),
            base,
            background: None,
            syntax: true,
        }
    }

    pub fn gutter(mut self, gutter: impl Into<String>, color: Color32) -> Self {
        self.gutter = gutter.into();
        self.gutter_color = color;
        self
    }

    pub fn background(mut self, color: Color32) -> Self {
        self.background = Some(color);
        self
    }

    /// Mark the row as outside the parsed snippet — it keeps `base` whole.
    pub fn unparsed(mut self) -> Self {
        self.syntax = false;
        self
    }
}

/// Split a block of text into rows. `split('\n')` rather than `lines()` so
/// the rows rejoin into exactly the original string, which the text editor's
/// cursor positions depend on.
pub fn rows(text: &str, base: Color32) -> Vec<Line> {
    text.split('\n').map(|l| Line::code(l, base)).collect()
}

/// Lay the rows out as one unwrapped galley, coloured for the language `path`
/// implies.
pub fn galley(ui: &Ui, path: &str, lines: &[Line]) -> Arc<Galley> {
    let parsed: Vec<&str> =
        lines.iter().filter(|l| l.syntax).map(|l| l.text.as_str()).collect();
    let spans = crate::highlight::line_spans(path, &parsed);
    let font = TextStyle::Monospace.resolve(ui.style());

    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    let mut nth_parsed = 0usize;
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            push(&mut job, "\n", &font, line.base, None);
        }
        if !line.gutter.is_empty() {
            push(&mut job, &line.gutter, &font, line.gutter_color, line.background);
        }
        let runs = if line.syntax {
            nth_parsed += 1;
            spans.get(nth_parsed - 1).map(|r| r.as_slice()).unwrap_or(&[])
        } else {
            &[]
        };
        let mut cursor = 0usize;
        for (start, end, color) in runs {
            let (start, end) = (*start, *end);
            let usable = start >= cursor
                && end <= line.text.len()
                && line.text.is_char_boundary(start)
                && line.text.is_char_boundary(end);
            if !usable {
                continue; // a span that does not fit costs colour, not a panic
            }
            if start > cursor {
                push(&mut job, &line.text[cursor..start], &font, line.base, line.background);
            }
            push(&mut job, &line.text[start..end], &font, *color, line.background);
            cursor = end;
        }
        if cursor < line.text.len() {
            push(&mut job, &line.text[cursor..], &font, line.base, line.background);
        }
    }
    ui.fonts(|f| f.layout_job(job))
}

fn push(job: &mut LayoutJob, text: &str, font: &FontId, color: Color32, bg: Option<Color32>) {
    if text.is_empty() {
        return;
    }
    job.append(
        text,
        0.0,
        TextFormat {
            font_id: font.clone(),
            color,
            background: bg.unwrap_or(Color32::TRANSPARENT),
            ..Default::default()
        },
    );
}

/// Draw the rows. The caller owns the scrolling — every code view here sits
/// in a [`egui::ScrollArea`] that can go both ways, because nothing wraps.
pub fn show(ui: &mut Ui, path: &str, lines: &[Line]) {
    let galley = galley(ui, path, lines);
    ui.add(egui::Label::new(galley));
}

/// Draw a plain block of code — no gutter, one colour under the highlighting.
pub fn show_block(ui: &mut Ui, path: &str, text: &str, base: Color32) {
    show(ui, path, &rows(text, base));
}

/// Rows for a line-level diff of `old` against `new`: what a proposal would
/// take out, what it would put in, and the untouched lines around them. A
/// revision only means something against the text it replaces, so this is
/// what a proposed rewrite previews as rather than a fresh block of code.
///
/// Only the surviving text is parsed for syntax — removed lines are not in the
/// version being proposed, and mixing them into the parse would break the
/// structure the highlighter is there to find.
pub fn diff_rows(old: &str, new: &str) -> Vec<Line> {
    use crate::ui::theme;

    let (a, b): (Vec<&str>, Vec<&str>) =
        (old.split('\n').collect(), new.split('\n').collect());
    diff_ops(&a, &b)
        .into_iter()
        .map(|(op, text)| match op {
            Op::Del => Line::code(text, theme::REMOVED)
                .gutter("- ", theme::REMOVED)
                .background(theme::DEL_BG)
                .unparsed(),
            Op::Add => Line::code(text, theme::CODE)
                .gutter("+ ", theme::ADDED)
                .background(theme::ADD_BG),
            Op::Same => Line::code(text, theme::CODE_CTX).gutter("  ", theme::GUTTER),
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    Same,
    Del,
    Add,
}

/// How many cells of the longest-common-subsequence table are worth filling.
/// A candidate preview is a unit-sized excerpt, so this is never reached in
/// practice; it exists so a pathological pair of texts costs a coarse diff
/// rather than a frame that takes seconds to lay out.
const LCS_CELLS: usize = 250_000;

/// Line ops taking `a` to `b`. The matching head and tail are peeled off
/// first, which is what keeps the table small for the usual case — a revision
/// touching a couple of lines in the middle of a unit.
fn diff_ops<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<(Op, &'a str)> {
    let mut head = 0;
    while head < a.len() && head < b.len() && a[head] == b[head] {
        head += 1;
    }
    let mut tail = 0;
    while tail < a.len() - head
        && tail < b.len() - head
        && a[a.len() - 1 - tail] == b[b.len() - 1 - tail]
    {
        tail += 1;
    }
    let (mid_a, mid_b) = (&a[head..a.len() - tail], &b[head..b.len() - tail]);

    let mut out: Vec<(Op, &str)> = a[..head].iter().map(|l| (Op::Same, *l)).collect();
    if mid_a.len().saturating_mul(mid_b.len()) > LCS_CELLS {
        out.extend(mid_a.iter().map(|l| (Op::Del, *l)));
        out.extend(mid_b.iter().map(|l| (Op::Add, *l)));
    } else {
        out.extend(lcs_ops(mid_a, mid_b));
    }
    out.extend(a[a.len() - tail..].iter().map(|l| (Op::Same, *l)));
    out
}

/// Classic longest-common-subsequence diff. Ties go to the deletion, so a
/// replaced run reads as its old lines then its new ones rather than as the
/// two interleaved.
fn lcs_ops<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<(Op, &'a str)> {
    let (n, m) = (a.len(), b.len());
    // `t[i][j]` is the length of the longest common subsequence of the
    // suffixes `a[i..]` and `b[j..]`, flattened into one row-major buffer.
    let mut t = vec![0u32; (n + 1) * (m + 1)];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            t[at(i, j)] = if a[i] == b[j] {
                t[at(i + 1, j + 1)] + 1
            } else {
                t[at(i + 1, j)].max(t[at(i, j + 1)])
            };
        }
    }

    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < n && j < m {
        if a[i] == b[j] {
            out.push((Op::Same, a[i]));
            i += 1;
            j += 1;
        } else if t[at(i + 1, j)] >= t[at(i, j + 1)] {
            out.push((Op::Del, a[i]));
            i += 1;
        } else {
            out.push((Op::Add, b[j]));
            j += 1;
        }
    }
    out.extend(a[i..].iter().map(|l| (Op::Del, *l)));
    out.extend(b[j..].iter().map(|l| (Op::Add, *l)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> egui::Context {
        let ctx = egui::Context::default();
        crate::ui::theme::apply(&ctx);
        ctx
    }

    /// The galley's text must equal the editor's text byte for byte, or the
/// text cursor appears somewhere other than where it is drawn.
    #[test]
    fn a_block_lays_out_to_exactly_the_text_it_was_given() {
        let ctx = ctx();
        for text in ["fn f() {\n    x();\n}", "trailing newline\n", "", "\n\n", "héllo — ok"] {
            let _ = ctx.run(Default::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let g = galley(ui, "a.rs", &rows(text, Color32::WHITE));
                    assert_eq!(g.text(), text, "{text:?}");
                });
            });
        }
    }

    #[test]
    fn gutters_and_removed_lines_still_round_trip() {
        let ctx = ctx();
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let lines = vec![
                    Line::code("    let x = 1;", Color32::WHITE).gutter(">    1| ", Color32::GRAY),
                    Line::code("    let y = 2;", Color32::WHITE).unparsed().gutter("-     | ", Color32::RED),
                ];
                let g = galley(ui, "a.rs", &lines);
                assert_eq!(g.text(), ">    1|     let x = 1;\n-     |     let y = 2;");
            });
        });
    }

    /// Compact rendering of a diff for assertions: the gutter marker plus the
    /// line, so the shape of the diff is readable in the test itself.
    fn shape(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| format!("{}{}", l.gutter.trim_end(), l.text)).collect()
    }

    #[test]
    fn a_diff_marks_what_leaves_what_arrives_and_what_stays() {
        let old = "fn f() {\n    old();\n}";
        let new = "fn f() {\n    new();\n    more();\n}";
        let rows = diff_rows(old, new);
        assert_eq!(
            shape(&rows),
            vec!["fn f() {", "-    old();", "+    new();", "+    more();", "}"]
        );
        // Diff role lives in the background tint, leaving colour for syntax.
        assert_eq!(rows[1].background, Some(crate::ui::theme::DEL_BG));
        assert_eq!(rows[2].background, Some(crate::ui::theme::ADD_BG));
        assert!(rows[0].background.is_none() && rows[4].background.is_none());
        // Only the proposed text is parsed — the removed line is not in it.
        assert!(!rows[1].syntax, "a removed line must stay out of the parse");
        assert!(rows.iter().enumerate().all(|(i, r)| i == 1 || r.syntax));
    }

    #[test]
    fn a_diff_of_identical_text_is_all_context() {
        let rows = diff_rows("a\nb\nc", "a\nb\nc");
        assert_eq!(shape(&rows), vec!["a", "b", "c"]);
        assert!(rows.iter().all(|r| r.background.is_none()));
    }

    #[test]
    fn pure_inserts_and_pure_deletes_keep_the_lines_that_did_not_move() {
        // An insert in the middle does not restate the lines around it...
        assert_eq!(shape(&diff_rows("a\nc", "a\nb\nc")), vec!["a", "+b", "c"]);
        // ...and neither does a deletion.
        assert_eq!(shape(&diff_rows("a\nb\nc", "a\nc")), vec!["a", "-b", "c"]);
        // Empty on either side is the whole other side.
        assert_eq!(shape(&diff_rows("", "a")), vec!["-", "+a"]);
        assert_eq!(shape(&diff_rows("a\nb", "")), vec!["-a", "-b", "+"]);
    }

    #[test]
    fn a_replaced_run_reads_as_old_lines_then_new_ones() {
        // Not the two interleaved, which is unreadable in a narrow card.
        let rows = diff_rows("keep\nx1\nx2\ntail", "keep\ny1\ny2\ntail");
        assert_eq!(shape(&rows), vec!["keep", "-x1", "-x2", "+y1", "+y2", "tail"]);
    }

    #[test]
    fn a_pathological_pair_still_diffs_without_filling_the_table() {
        // Past the table cap the diff goes coarse — every old line out, every
        // new line in — rather than costing a frame that takes seconds.
        let old: String = (0..600).map(|i| format!("old{i}\n")).collect();
        let new: String = (0..600).map(|i| format!("new{i}\n")).collect();
        let rows = diff_rows(&old, &new);
        assert!(rows.iter().any(|r| r.gutter.starts_with('-')));
        assert!(rows.iter().any(|r| r.gutter.starts_with('+')));
        // The trailing empty line both texts end with is still shared.
        assert_eq!(shape(rows.last().map(std::slice::from_ref).unwrap()), vec![""]);
    }

    #[test]
    fn a_diff_lays_out_with_its_gutters_intact() {
        let ctx = ctx();
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let g = galley(ui, "a.rs", &diff_rows("let x = 1;", "let x = 2;"));
                assert_eq!(g.text(), "- let x = 1;\n+ let x = 2;");
            });
        });
    }

    #[test]
    fn nothing_wraps_however_narrow_the_ui_is() {
        let ctx = ctx();
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.set_max_width(40.0);
                let long = "                let very_long_identifier = another_long_one(a, b, c);";
                let g = galley(ui, "a.rs", &rows(long, Color32::WHITE));
                assert_eq!(g.rows.len(), 1, "one source line must stay one row");
            });
        });
    }
}
