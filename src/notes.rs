//! Reviewer notes: issues too big for the unit they were spotted on.
//!
//! Reviewing one unit at a time deliberately narrows the blast radius of every
//! edit — which also means a unit that *reveals* a larger problem offers no
//! way to act on it. A note parks that observation: it names the spot, keeps
//! the code as it stood, and waits on the follow-up screen. There the human
//! triages the backlog — dismiss what no longer matters, check what a model
//! with room to make bigger changes should tackle — and hands the checked
//! notes to one model in a single interactive fix session. Resolved and
//! dismissed notes never come back; unchecked ones wait for the next visit.

/// One note as the database hands it back. `excerpt` is the unit's text as it
/// stood when the note was written — the working tree may have moved on, and
/// the note is about what the reviewer was looking at, not about whatever sits
/// on those lines today.
#[derive(Clone)]
pub struct Note {
    pub id: i64,
    pub ts: String,
    pub file: String,
    pub line_start: u32,
    pub line_end: u32,
    pub excerpt: String,
    pub text: String,
}

impl Note {
    /// `file:12-18`, or `file:12` when the note covers one line.
    pub fn locus(&self) -> String {
        if self.line_end > self.line_start {
            format!("{}:{}-{}", self.file, self.line_start, self.line_end)
        } else {
            format!("{}:{}", self.file, self.line_start)
        }
    }
}

/// The editable opening of the fix-session prompt. A starting point, not a
/// contract — the human rewrites it on the follow-up screen before launching.
pub fn default_preamble() -> &'static str {
    "Resolve the review notes below. Each was written during a unit-by-unit code \
review at a spot where the reviewer saw an issue larger than that unit — a pattern, \
a design problem, a missing abstraction — that could not be fixed within the unit \
itself. Work through them one at a time: read enough of the surrounding code to see \
the whole issue, make the change it calls for, and say what you changed and why. If \
a note is mistaken or already resolved, say so and move on instead of forcing an edit."
}

/// The opening prompt of a fix session: the (human-edited) preamble, the
/// ground rules, then every checked note with its locus and the code it was
/// written against.
pub fn build_fix_prompt(preamble: &str, notes: &[Note]) -> String {
    let mut out = String::new();
    let preamble = preamble.trim();
    if !preamble.is_empty() {
        out.push_str(preamble);
        out.push_str("\n\n");
    }
    out.push_str(
        "You are running in the root of the repository these notes are about, and every \
path below is relative to it. Line numbers and excerpts are from review time — the \
files may have shifted since, so find the code, don't trust the numbers.\n",
    );
    for (i, n) in notes.iter().enumerate() {
        out.push_str(&format!("\n--- note {} · {} ---\n{}\n", i + 1, n.locus(), n.text.trim()));
        if !n.excerpt.trim().is_empty() {
            out.push_str("The code the note was written on, as it stood then:\n");
            for line in n.excerpt.lines() {
                out.push_str("    ");
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(id: i64, file: &str, start: u32, end: u32, text: &str, excerpt: &str) -> Note {
        Note {
            id,
            ts: "2026-08-23 12:00:00.000".into(),
            file: file.into(),
            line_start: start,
            line_end: end,
            excerpt: excerpt.into(),
            text: text.into(),
        }
    }

    #[test]
    fn locus_collapses_single_line_ranges() {
        assert_eq!(note(1, "src/a.rs", 12, 18, "", "").locus(), "src/a.rs:12-18");
        assert_eq!(note(1, "src/a.rs", 12, 12, "", "").locus(), "src/a.rs:12");
    }

    #[test]
    fn the_fix_prompt_carries_every_note_in_order_with_its_code() {
        let notes = vec![
            note(1, "src/a.rs", 10, 12, "This retry loop appears in four files — extract it.", "    retry();"),
            note(2, "src/b.rs", 5, 5, "Error type swallows the cause.", ""),
        ];
        let p = build_fix_prompt("Fix these.", &notes);
        assert!(p.starts_with("Fix these."));
        let n1 = p.find("note 1 · src/a.rs:10-12").expect("first note present");
        let n2 = p.find("note 2 · src/b.rs:5").expect("second note present");
        assert!(n1 < n2, "notes must keep review order");
        assert!(p.contains("extract it."));
        // The excerpt is indented so it reads as quoted code, and a note
        // without one gets no empty quotation block.
        assert!(p.contains("\n        retry();\n"));
        assert_eq!(p.matches("as it stood then").count(), 1);
        // The model is told where it is and not to trust stale line numbers.
        assert!(p.contains("root of the repository"));
        assert!(p.contains("don't trust the numbers"));
    }

    #[test]
    fn an_emptied_preamble_still_yields_a_usable_prompt() {
        let p = build_fix_prompt("   ", &[note(1, "a.rs", 1, 1, "x", "")]);
        assert!(p.starts_with("You are running in the root"));
        assert!(p.contains("note 1 · a.rs:1"));
    }
}
