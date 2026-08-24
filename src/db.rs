//! SQLite persistence: settings, activity log, sessions, model suggestions,
//! and human decisions. One global database in the platform data dir
//! (override with the CRA_DB env var).

use rusqlite::{params, Connection};
use std::collections::HashSet;
use std::path::PathBuf;

pub struct Db {
    conn: Connection,
}

/// One model's answer on one unit, as it goes into the record.
pub struct SuggestionRecord<'a> {
    pub session_id: i64,
    pub file: &'a str,
    pub line_start: u32,
    pub line_end: u32,
    pub model: &'a str,
    pub action: Option<&'a str>,
    pub comment: Option<&'a str>,
    pub justification: Option<&'a str>,
    pub latency_ms: i64,
    pub error: Option<&'a str>,
    /// Places the model says it read, as a JSON array.
    pub evidence: Option<&'a str>,
    /// What the call spent. `None` when the CLI reported nothing, which is
    /// stored as null so the evaluation page can tell silence from zero.
    pub usage: Option<crate::models::Usage>,
    /// `(usd, estimated)` — see [`crate::models::Usage::priced`].
    pub cost: Option<(f64, bool)>,
    /// The follow-up question this answer replies to; `None` for a first-round
    /// verdict. This is what ties an answer back to the exact words that
    /// prompted it — without it the question survives only as a truncated
    /// activity-log line.
    pub follow_up_id: Option<i64>,
    /// 1 for the opening verdict, 2 for the answer to the first follow-up,
    /// and so on. Recorded per row because rounds are where verdicts flip,
    /// and mining that flip needs to know which round each answer belongs to.
    pub round: i64,
}

/// One human verdict on one comment.
pub struct DecisionRecord<'a> {
    pub session_id: i64,
    pub file: &'a str,
    pub line_start: u32,
    pub line_end: u32,
    pub original: &'a str,
    pub action: &'a str,
    pub final_text: &'a str,
    pub source: &'a str,
    pub human_edited: bool,
    pub committed: bool,
    pub commit_sha: Option<&'a str>,
    pub justification: Option<&'a str>,
    /// The unit exactly as reviewed, so the judgement can be replayed against
    /// a different model later. `None` for decisions made before this existed.
    pub unit_json: Option<&'a str>,
    /// Whether model identities were hidden when the choice was made.
    pub blinded: bool,
}

/// A labelled example: the unit, and what the human decided about it.
pub struct CorpusRow {
    pub unit_json: String,
    pub action: String,
    pub final_text: String,
    pub source: String,
    pub human_edited: bool,
    pub blinded: bool,
    /// Repository the comment was reviewed in, so a replay can put the models
    /// back in it. Empty for a decision recorded without a session.
    pub repo: String,
}

/// One model's answer inside one contest: everything shown side by side for a
/// single unit, plus what that answer cost to produce.
pub struct ContestRow {
    /// Rows sharing this were competing for the same decision.
    pub decision_id: i64,
    pub repo: String,
    pub human_action: String,
    pub human_source: String,
    pub blinded: bool,
    pub model: String,
    pub model_action: Option<String>,
    pub latency_ms: i64,
    pub error: Option<String>,
}

/// One model's total spend, over every call it made.
pub struct SpendRow {
    pub model: String,
    pub calls: usize,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cost_usd: f64,
    /// Calls that carry a cost at all — the rest are unmeasured, not free.
    pub priced_calls: usize,
    /// Of those, how many were priced from the model configuration's rates rather than by
    /// the CLI itself.
    pub estimated_calls: usize,
}

/// One model's proposal next to the human verdict for the same comment.
pub struct AgreementRow {
    pub model: String,
    pub model_action: Option<String>,
    pub latency_ms: i64,
    pub error: Option<String>,
    pub human_action: String,
    pub human_source: String,
    pub blinded: bool,
}

fn db_path() -> PathBuf {
    if let Ok(p) = std::env::var("CRA_DB") {
        return PathBuf::from(p);
    }
    let mut dir = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    dir.push("code-review-assistant");
    let _ = std::fs::create_dir_all(&dir);
    dir.push("cra.db");
    dir
}

/// Add a column if the table does not already have it. SQLite has no
/// `ADD COLUMN IF NOT EXISTS`, and blindly retrying the ALTER would mask real
/// errors, so ask first.
fn add_column(conn: &Connection, table: &str, column: &str, decl: &str) {
    let Ok(mut stmt) = conn.prepare(&format!("PRAGMA table_info({table})")) else { return };
    let existing: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default();
    if !existing.iter().any(|c| c == column) {
        let _ = conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"), []);
    }
}

fn now() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string()
}

impl Db {
    pub fn open() -> Result<Self, String> {
        Self::open_at(&db_path())
    }

    /// Open a database at an explicit path. The `CRA_DB` env var does the same
    /// job for a whole process; this exists so a test can have its own file
    /// without a process-global that would race other tests.
    pub fn open_at(path: &std::path::Path) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS settings(
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS events(
                 id     INTEGER PRIMARY KEY AUTOINCREMENT,
                 ts     TEXT NOT NULL,
                 kind   TEXT NOT NULL,
                 detail TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS sessions(
                 id       INTEGER PRIMARY KEY AUTOINCREMENT,
                 ts       TEXT NOT NULL,
                 repo     TEXT NOT NULL,
                 ref_kind TEXT NOT NULL,
                 ref_name TEXT NOT NULL,
                 base_ref TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS suggestions(
                 id            INTEGER PRIMARY KEY AUTOINCREMENT,
                 ts            TEXT NOT NULL,
                 session_id    INTEGER,
                 file          TEXT NOT NULL,
                 line_start    INTEGER NOT NULL,
                 line_end      INTEGER NOT NULL,
                 model         TEXT NOT NULL,
                 action        TEXT,
                 comment       TEXT,
                 justification TEXT,
                 latency_ms    INTEGER,
                 error         TEXT
             );
             CREATE TABLE IF NOT EXISTS decisions(
                 id            INTEGER PRIMARY KEY AUTOINCREMENT,
                 ts            TEXT NOT NULL,
                 session_id    INTEGER,
                 file          TEXT NOT NULL,
                 line_start    INTEGER NOT NULL,
                 line_end      INTEGER NOT NULL,
                 original      TEXT NOT NULL,
                 action        TEXT NOT NULL,
                 final_text    TEXT NOT NULL,
                 source        TEXT NOT NULL,
                 human_edited  INTEGER NOT NULL,
                 committed     INTEGER NOT NULL,
                 commit_sha    TEXT,
                 justification TEXT
             );",
        )
        .map_err(|e| e.to_string())?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS notes(
                 id         INTEGER PRIMARY KEY AUTOINCREMENT,
                 ts         TEXT NOT NULL,
                 session_id INTEGER,
                 repo       TEXT NOT NULL,
                 file       TEXT NOT NULL,
                 line_start INTEGER NOT NULL,
                 line_end   INTEGER NOT NULL,
                 excerpt    TEXT NOT NULL,
                 text       TEXT NOT NULL,
                 status     TEXT NOT NULL DEFAULT 'open',
                 status_ts  TEXT
             );
             CREATE TABLE IF NOT EXISTS findings(
                 id         INTEGER PRIMARY KEY AUTOINCREMENT,
                 ts         TEXT NOT NULL,
                 session_id INTEGER,
                 model      TEXT NOT NULL,
                 severity   TEXT NOT NULL,
                 title      TEXT NOT NULL,
                 detail     TEXT NOT NULL,
                 files      TEXT NOT NULL,
                 evidence   TEXT,
                 status     TEXT NOT NULL DEFAULT 'open'
             );
             CREATE TABLE IF NOT EXISTS follow_ups(
                 id         INTEGER PRIMARY KEY AUTOINCREMENT,
                 ts         TEXT NOT NULL,
                 session_id INTEGER,
                 file       TEXT NOT NULL,
                 line_start INTEGER NOT NULL,
                 line_end   INTEGER NOT NULL,
                 round      INTEGER NOT NULL,
                 question   TEXT NOT NULL
             );",
        )
        .map_err(|e| e.to_string())?;
        // Added after the first release; existing databases get them here so a
        // review history recorded before evaluation existed stays usable.
        add_column(&conn, "decisions", "unit_json", "TEXT");
        add_column(&conn, "decisions", "blinded", "INTEGER NOT NULL DEFAULT 0");
        // What the model says it read to reach the verdict, as a JSON array.
        add_column(&conn, "suggestions", "evidence", "TEXT");
        // What the call spent, as the CLI accounted for it. Null, not zero,
        // when the CLI said nothing — a silent CLI is unmeasured, not free.
        add_column(&conn, "suggestions", "input_tokens", "INTEGER");
        add_column(&conn, "suggestions", "output_tokens", "INTEGER");
        add_column(&conn, "suggestions", "cache_read_tokens", "INTEGER");
        add_column(&conn, "suggestions", "cost_usd", "REAL");
        // Whether that cost came from the CLI (0) or from the model configuration's
        // configured rates (1). Kept per row because a rate change later must
        // not silently rewrite what earlier calls are said to have cost.
        add_column(&conn, "suggestions", "cost_estimated", "INTEGER");
        // Which follow-up question an answer replies to, and which round of
        // the conversation it belongs to. Null on rows from before this
        // existed — those answers' questions survive only as truncated
        // activity-log lines, which is exactly what these columns fix.
        add_column(&conn, "suggestions", "follow_up_id", "INTEGER");
        add_column(&conn, "suggestions", "round", "INTEGER");
        Ok(Db { conn })
    }

    pub fn log(&self, kind: &str, detail: &str) {
        let _ = self.conn.execute(
            "INSERT INTO events(ts, kind, detail) VALUES (?1, ?2, ?3)",
            params![now(), kind, detail],
        );
    }

    pub fn get_setting(&self, key: &str) -> Option<String> {
        self.conn
            .query_row("SELECT value FROM settings WHERE key = ?1", params![key], |r| r.get(0))
            .ok()
    }

    pub fn set_setting(&self, key: &str, value: &str) {
        let _ = self.conn.execute(
            "INSERT INTO settings(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        );
    }

    pub fn new_session(&self, repo: &str, ref_kind: &str, ref_name: &str, base_ref: &str) -> i64 {
        let _ = self.conn.execute(
            "INSERT INTO sessions(ts, repo, ref_kind, ref_name, base_ref) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![now(), repo, ref_kind, ref_name, base_ref],
        );
        self.conn.last_insert_rowid()
    }

    pub fn log_suggestion(&self, s: &SuggestionRecord) {
        let _ = self.conn.execute(
            "INSERT INTO suggestions(ts, session_id, file, line_start, line_end, model,
                                     action, comment, justification, latency_ms, error, evidence,
                                     input_tokens, output_tokens, cache_read_tokens, cost_usd,
                                     cost_estimated, follow_up_id, round)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                     ?18, ?19)",
            params![
                now(), s.session_id, s.file, s.line_start, s.line_end, s.model, s.action,
                s.comment, s.justification, s.latency_ms, s.error, s.evidence,
                s.usage.map(|u| u.input_tokens), s.usage.map(|u| u.output_tokens),
                s.usage.map(|u| u.cache_read_tokens), s.cost.map(|(usd, _)| usd),
                s.cost.map(|(_, estimated)| estimated as i64), s.follow_up_id, s.round
            ],
        );
    }

    /// Record one follow-up question, in full, at the moment it is asked.
    /// Returns its row id so the answers can carry it — the question and its
    /// answers are one conversation, and only this link keeps them one.
    pub fn log_follow_up(
        &self,
        session_id: i64,
        file: &str,
        line_start: u32,
        line_end: u32,
        round: i64,
        question: &str,
    ) -> i64 {
        let _ = self.conn.execute(
            "INSERT INTO follow_ups(ts, session_id, file, line_start, line_end, round, question)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![now(), session_id, file, line_start, line_end, round, question],
        );
        self.conn.last_insert_rowid()
    }

    /// The reviewer's most recent follow-up questions, newest first, each one
    /// kept once. These are the reviewer's own words about what the models got
    /// wrong — the raw material the preference preamble is distilled from.
    pub fn recent_followup_questions(&self, limit: usize) -> Vec<String> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT question FROM follow_ups GROUP BY question ORDER BY MAX(id) DESC LIMIT ?1",
        ) else {
            return Vec::new();
        };
        stmt.query_map(params![limit as i64], |r| r.get::<_, String>(0))
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    /// How past human verdicts split, as (kept, rewritten, deleted). Flags
    /// count as kept — the text was left alone.
    pub fn action_mix(&self) -> (i64, i64, i64) {
        self.conn
            .query_row(
                "SELECT SUM(action IN ('keep', 'flag')), SUM(action = 'rewrite'),
                        SUM(action = 'delete')
                 FROM decisions",
                [],
                |r| {
                    Ok((
                        r.get::<_, Option<i64>>(0)?.unwrap_or(0),
                        r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                        r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    ))
                },
            )
            .unwrap_or((0, 0, 0))
    }

    /// Returns the new row's id, so a decision saved without committing can
    /// later be updated once it is included in a commit (see
    /// [`Db::mark_committed`]).
    pub fn log_decision(&self, d: &DecisionRecord) -> i64 {
        let _ = self.conn.execute(
            "INSERT INTO decisions(ts, session_id, file, line_start, line_end, original, action,
                                   final_text, source, human_edited, committed, commit_sha,
                                   justification, unit_json, blinded)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                now(), d.session_id, d.file, d.line_start, d.line_end, d.original, d.action,
                d.final_text, d.source, d.human_edited as i64, d.committed as i64, d.commit_sha,
                d.justification, d.unit_json, d.blinded as i64
            ],
        );
        self.conn.last_insert_rowid()
    }

    /// Back-fills `committed`/`commit_sha` on a decision that was saved
    /// without committing, once a later `Commit and Continue` folds it into a
    /// commit alongside the decision that triggered it.
    pub fn mark_committed(&self, id: i64, sha: &str) {
        let _ = self.conn.execute(
            "UPDATE decisions SET committed = 1, commit_sha = ?1 WHERE id = ?2",
            params![sha, id],
        );
    }

    /// Every unit text this repository already has a verdict on, as
    /// `(file, text)`. Both sides of a decision are listed: the text that was
    /// judged, and the text the verdict left behind. The second matters as
    /// much as the first — a rewrite changes the file, so the next diff offers
    /// the *new* text back as a fresh unit, and only recognising it as the
    /// outcome of a decision stops the same comment being asked about forever.
    ///
    /// Scoped by repository path: identical boilerplate in two checkouts is
    /// two separate review jobs.
    pub fn decided_units(&self, repo: &str) -> HashSet<(String, String)> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT d.file, d.original FROM decisions d
               JOIN sessions s ON s.id = d.session_id
              WHERE s.repo = ?1
             UNION
             SELECT d.file, d.final_text FROM decisions d
               JOIN sessions s ON s.id = d.session_id
              WHERE s.repo = ?1 AND d.final_text <> ''",
        ) else {
            return HashSet::new();
        };
        stmt.query_map(params![repo], |r| Ok((r.get(0)?, r.get(1)?)))
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    /// Reviewed comments that carry the unit they were judged on, newest
    /// first. These are the labelled examples a replay is scored against.
    pub fn corpus(&self, limit: usize) -> Vec<CorpusRow> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT d.unit_json, d.action, d.final_text, d.source, d.human_edited, d.blinded,
                    COALESCE(s.repo, '')
             FROM decisions d
             LEFT JOIN sessions s ON s.id = d.session_id
             WHERE d.unit_json IS NOT NULL AND d.unit_json <> ''
             ORDER BY d.id DESC LIMIT ?1",
        ) else {
            return Vec::new();
        };
        stmt.query_map(params![limit as i64], |r| {
            Ok(CorpusRow {
                unit_json: r.get(0)?,
                action: r.get(1)?,
                final_text: r.get(2)?,
                source: r.get(3)?,
                human_edited: r.get::<_, i64>(4)? != 0,
                blinded: r.get::<_, i64>(5)? != 0,
                repo: r.get(6)?,
            })
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
    }

    /// Labelled decisions from one repository only. Re-checking runs models
    /// in the selected checkout, so mixing another repository's relative
    /// paths into that plan would show them unrelated code.
    pub fn corpus_for_repo(&self, repo: &str, limit: usize) -> Vec<CorpusRow> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT d.unit_json, d.action, d.final_text, d.source, d.human_edited, d.blinded,
                    s.repo
             FROM decisions d
             JOIN sessions s ON s.id = d.session_id
             WHERE d.unit_json IS NOT NULL AND d.unit_json <> '' AND s.repo = ?1
             ORDER BY d.id DESC LIMIT ?2",
        ) else {
            return Vec::new();
        };
        stmt.query_map(params![repo, limit as i64], |r| {
            Ok(CorpusRow {
                unit_json: r.get(0)?,
                action: r.get(1)?,
                final_text: r.get(2)?,
                source: r.get(3)?,
                human_edited: r.get::<_, i64>(4)? != 0,
                blinded: r.get::<_, i64>(5)? != 0,
                repo: r.get(6)?,
            })
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
    }

    /// Every model suggestion paired with the decision the human made on the
    /// same comment. This join is the whole evaluation dataset.
    pub fn agreement_rows(&self) -> Vec<AgreementRow> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT s.model, s.action, s.latency_ms, s.error,
                    d.action, d.source, d.blinded
             FROM suggestions s
             JOIN decisions d
               ON s.session_id = d.session_id
              AND s.file = d.file
              AND s.line_start = d.line_start
             WHERE NOT EXISTS (
                 SELECT 1 FROM suggestions newer
                 WHERE newer.session_id = s.session_id
                   AND newer.file = s.file
                   AND newer.line_start = s.line_start
                   AND newer.line_end = s.line_end
                   AND newer.model = s.model
                   AND newer.id > s.id
             )",
        ) else {
            return Vec::new();
        };
        stmt.query_map([], |r| {
            Ok(AgreementRow {
                model: r.get(0)?,
                model_action: r.get(1)?,
                latency_ms: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                error: r.get(3)?,
                human_action: r.get(4)?,
                human_source: r.get(5)?,
                blinded: r.get::<_, i64>(6)? != 0,
            })
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
    }

    /// Every model answer paired with the decision it was competing for, plus
    /// what the call spent. One row per (decision, model answer); a unit whose
    /// models were re-run contributes a row per run, which
    /// [`crate::eval::Leaderboard`] collapses to the last one.
    ///
    /// This is the same join [`Db::agreement_rows`] makes. It is kept separate
    /// rather than widened because it carries the decision id, and that is
    /// what turns a flat list of agreements into contests: rows sharing an id
    /// were shown side by side, and exactly one of them could be chosen.
    pub fn contest_rows(&self) -> Vec<ContestRow> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT d.id, COALESCE(sess.repo, ''), d.action, d.source, d.blinded,
                    s.model, s.action, s.latency_ms, s.error
             FROM suggestions s
             JOIN decisions d
               ON s.session_id = d.session_id
              AND s.file = d.file
              AND s.line_start = d.line_start
             LEFT JOIN sessions sess ON sess.id = d.session_id
             ORDER BY s.id ASC",
        ) else {
            return Vec::new();
        };
        stmt.query_map([], |r| {
            Ok(ContestRow {
                decision_id: r.get(0)?,
                repo: r.get(1)?,
                human_action: r.get(2)?,
                human_source: r.get(3)?,
                blinded: r.get::<_, i64>(4)? != 0,
                model: r.get(5)?,
                model_action: r.get(6)?,
                latency_ms: r.get::<_, Option<i64>>(7)?.unwrap_or(0),
                error: r.get(8)?,
            })
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
    }

    /// What every model call spent, decided or not. Separate from
    /// [`Db::contest_rows`] because spending is not conditional on the review
    /// reaching a verdict: a unit that was skipped, or one whose models are
    /// still being compared, cost money all the same, and a cost table that
    /// only counted finished contests would understate every model equally.
    pub fn spend_rows(&self, repo: Option<&str>) -> Vec<SpendRow> {
        let sql = "SELECT s.model, COUNT(*),
                          COALESCE(SUM(s.input_tokens), 0), COALESCE(SUM(s.output_tokens), 0),
                          COALESCE(SUM(s.cache_read_tokens), 0),
                          COALESCE(SUM(s.cost_usd), 0),
                          SUM(s.cost_usd IS NOT NULL),
                          SUM(COALESCE(s.cost_estimated, 0))
                   FROM suggestions s
                   LEFT JOIN sessions sess ON sess.id = s.session_id
                   WHERE (?1 IS NULL OR sess.repo = ?1)
                   GROUP BY s.model";
        let Ok(mut stmt) = self.conn.prepare(sql) else { return Vec::new() };
        stmt.query_map(params![repo], |r| {
            Ok(SpendRow {
                model: r.get(0)?,
                calls: r.get::<_, i64>(1)? as usize,
                input_tokens: r.get(2)?,
                output_tokens: r.get(3)?,
                cache_read_tokens: r.get(4)?,
                cost_usd: r.get(5)?,
                priced_calls: r.get::<_, Option<i64>>(6)?.unwrap_or(0) as usize,
                estimated_calls: r.get::<_, Option<i64>>(7)?.unwrap_or(0) as usize,
            })
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
    }

    /// Every repository that has review history, newest activity first, for
    /// the evaluation page's scope filter.
    pub fn repos_with_history(&self) -> Vec<String> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT repo, MAX(id) AS last FROM sessions
              WHERE repo <> '' GROUP BY repo ORDER BY last DESC",
        ) else {
            return Vec::new();
        };
        stmt.query_map([], |r| r.get::<_, String>(0))
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    /// Comments judged more than once, oldest verdict first. Comparing a
    /// reviewer against their own earlier self is the only way to know how
    /// much of a model's disagreement is noise rather than error.
    pub fn repeated_decisions(&self) -> Vec<(String, Vec<String>)> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT COALESCE(s.repo, '') || ':' ||
                    COALESCE(NULLIF(d.unit_json, ''),
                             d.file || ':' || d.line_start || ':' || d.line_end || ':' || d.original)
                    AS key,
                    d.action
             FROM decisions d
             LEFT JOIN sessions s ON s.id = d.session_id
             ORDER BY d.id ASC",
        ) else {
            return Vec::new();
        };
        let rows: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default();
        let mut grouped: Vec<(String, Vec<String>)> = Vec::new();
        for (key, action) in rows {
            match grouped.iter_mut().find(|(k, _)| *k == key) {
                Some((_, actions)) => actions.push(action),
                None => grouped.push((key, vec![action])),
            }
        }
        grouped.retain(|(_, actions)| actions.len() > 1);
        grouped
    }

    /// Record one reviewer note — an issue larger than the unit it was
    /// spotted on, parked for a follow-up fix session. Returns its row id.
    #[allow(clippy::too_many_arguments)]
    pub fn log_note(
        &self,
        session_id: i64,
        repo: &str,
        file: &str,
        line_start: u32,
        line_end: u32,
        excerpt: &str,
        text: &str,
    ) -> i64 {
        let _ = self.conn.execute(
            "INSERT INTO notes(ts, session_id, repo, file, line_start, line_end, excerpt, text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![now(), session_id, repo, file, line_start, line_end, excerpt, text],
        );
        self.conn.last_insert_rowid()
    }

    /// Notes for this repository still waiting on a follow-up session, oldest
    /// first so the list reads in the order the review raised them. Resolved
    /// and dismissed notes stay on the record but are never offered again.
    pub fn open_notes(&self, repo: &str) -> Vec<crate::notes::Note> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT id, ts, file, line_start, line_end, excerpt, text
             FROM notes WHERE repo = ?1 AND status = 'open' ORDER BY id ASC",
        ) else {
            return Vec::new();
        };
        stmt.query_map(params![repo], |r| {
            Ok(crate::notes::Note {
                id: r.get(0)?,
                ts: r.get(1)?,
                file: r.get(2)?,
                line_start: r.get(3)?,
                line_end: r.get(4)?,
                excerpt: r.get(5)?,
                text: r.get(6)?,
            })
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
    }

    /// Human triage of a note: 'open', 'resolved', or 'dismissed'. Stamps when
    /// the status changed, so the record shows how long a note waited.
    pub fn set_note_status(&self, id: i64, status: &str) {
        let _ = self.conn.execute(
            "UPDATE notes SET status = ?1, status_ts = ?2 WHERE id = ?3",
            params![status, now(), id],
        );
    }

    pub fn count_open_notes(&self, repo: &str) -> i64 {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM notes WHERE repo = ?1 AND status = 'open'",
                params![repo],
                |r| r.get(0),
            )
            .unwrap_or(0)
    }

    /// Every suggestion's conversation coordinates, oldest first:
    /// (file, action, follow_up_id, round).
    #[cfg(test)]
    pub fn suggestion_links(&self) -> Vec<(String, Option<String>, Option<i64>, Option<i64>)> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT file, action, follow_up_id, round FROM suggestions ORDER BY id ASC",
        ) else {
            return Vec::new();
        };
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub fn followup_row(&self, id: i64) -> Option<(i64, String)> {
        self.conn
            .query_row(
                "SELECT round, question FROM follow_ups WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok()
    }

    #[cfg(test)]
    pub fn note_status(&self, id: i64) -> Option<String> {
        self.conn
            .query_row("SELECT status FROM notes WHERE id = ?1", params![id], |r| r.get(0))
            .ok()
    }

    /// Record one whole-branch finding; returns its row id so a later human
    /// dismissal can be written back.
    #[allow(clippy::too_many_arguments)]
    pub fn log_finding(
        &self,
        session_id: i64,
        model: &str,
        severity: &str,
        title: &str,
        detail: &str,
        files_json: &str,
        evidence_json: Option<&str>,
    ) -> i64 {
        let _ = self.conn.execute(
            "INSERT INTO findings(ts, session_id, model, severity, title, detail, files, evidence)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![now(), session_id, model, severity, title, detail, files_json, evidence_json],
        );
        self.conn.last_insert_rowid()
    }

    /// Human triage of a finding: 'open' or 'dismissed'.
    pub fn set_finding_status(&self, id: i64, status: &str) {
        let _ = self.conn.execute(
            "UPDATE findings SET status = ?1 WHERE id = ?2",
            params![status, id],
        );
    }

    #[cfg(test)]
    pub fn finding_status(&self, id: i64) -> Option<String> {
        self.conn
            .query_row("SELECT status FROM findings WHERE id = ?1", params![id], |r| r.get(0))
            .ok()
    }

    pub fn decision_counts(&self, session_id: i64) -> (i64, i64) {
        let total: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM decisions WHERE session_id = ?1",
                params![session_id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let committed: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM decisions WHERE session_id = ?1 AND committed = 1",
                params![session_id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        (total, committed)
    }
}
