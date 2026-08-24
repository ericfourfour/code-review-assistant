//! The reviewer's standing preferences, distilled from their own review
//! history and prepended to review prompts.
//!
//! The history shows a pattern: models open with unanimous "keep", the
//! reviewer pushes back with a follow-up ("say why, not what", "this is
//! self-explanatory"), and the verdicts flip. The correction works — but it
//! is paid for again on every unit, one follow-up round at a time. The
//! preamble moves that guidance to round one.
//!
//! Everything here is the reviewer's own words: follow-up questions are typed
//! by the human, and the verdict mix counts human decisions. Model output is
//! deliberately not mined — a preamble built from model justifications would
//! teach the models to agree with themselves.

use crate::db::Db;

/// How many past follow-up questions the preamble carries. Enough to show the
/// pattern, few enough that the guidance does not drown the unit under review.
const QUESTION_LIMIT: usize = 8;

/// How many decisions the verdict-mix line needs before it says anything.
/// Below this the percentages are noise dressed up as a preference.
const MIX_FLOOR: i64 = 10;

/// The preference preamble for a review prompt, or `None` when there is no
/// history to speak from. Rebuilt per unit — it is two small queries, and the
/// follow-up asked a minute ago is exactly the guidance the next unit needs.
pub fn preamble(db: &Db) -> Option<String> {
    let questions = db.recent_followup_questions(QUESTION_LIMIT);
    let (kept, rewritten, deleted) = db.action_mix();
    let total = kept + rewritten + deleted;

    let mut out = String::new();
    if !questions.is_empty() {
        out.push_str(
            "Standing guidance from this reviewer, quoted from follow-ups they had to send \
             on earlier units. Apply it from the start — do not wait to be corrected:\n",
        );
        for q in &questions {
            out.push_str("- ");
            out.push_str(q.trim());
            out.push('\n');
        }
    }
    if total >= MIX_FLOOR {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!(
            "Across {total} past verdicts this reviewer kept {kept}, rewrote {rewritten}, and \
             deleted {deleted}. Weigh your verdict on the merits — do not default to keep out \
             of caution.\n"
        ));
    }
    if out.is_empty() {
        return None;
    }
    Some(format!("== Reviewer preferences ==\n{out}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::testkit::TempDir;

    fn db(tag: &str) -> (TempDir, Db) {
        let dir = TempDir::new(tag);
        let db = Db::open_at(&dir.path().join("cra.db")).expect("open test db");
        (dir, db)
    }

    fn decide(db: &Db, line: u32, action: &str) {
        db.log_decision(&crate::db::DecisionRecord {
            session_id: 1,
            file: "src/lib.rs",
            line_start: line,
            line_end: line,
            original: "// x",
            action,
            final_text: "",
            source: "original",
            human_edited: false,
            committed: false,
            commit_sha: None,
            justification: None,
            unit_json: None,
            blinded: false,
        });
    }

    #[test]
    fn an_empty_history_has_no_preamble() {
        let (_dir, db) = db("profile_empty");
        assert_eq!(preamble(&db), None);
    }

    #[test]
    fn follow_up_questions_are_quoted_in_full_and_newest_first() {
        let (_dir, db) = db("profile_questions");
        db.log_follow_up(1, "a.rs", 1, 1, 2, "Why does this comment restate the code?");
        db.log_follow_up(1, "b.rs", 9, 9, 2, "This test needs to say why.");
        let p = preamble(&db).expect("preamble");
        assert!(p.contains("- This test needs to say why.\n- Why does this comment restate the code?"));
    }

    #[test]
    fn a_repeated_question_is_kept_once() {
        let (_dir, db) = db("profile_dedup");
        db.log_follow_up(1, "a.rs", 1, 1, 2, "Say why, not what.");
        db.log_follow_up(1, "b.rs", 5, 5, 2, "Say why, not what.");
        let p = preamble(&db).expect("preamble");
        assert_eq!(p.matches("Say why, not what.").count(), 1);
    }

    #[test]
    fn the_verdict_mix_appears_only_with_enough_history() {
        let (_dir, db) = db("profile_mix");
        for line in 0..9 {
            decide(&db, line, "keep");
        }
        db.log_follow_up(1, "a.rs", 1, 1, 2, "q");
        assert!(
            !preamble(&db).expect("preamble").contains("past verdicts"),
            "nine decisions are not a pattern"
        );
        decide(&db, 9, "rewrite");
        let p = preamble(&db).expect("preamble");
        assert!(p.contains("Across 10 past verdicts this reviewer kept 9, rewrote 1, and deleted 0"));
    }
}
