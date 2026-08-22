//! SQLite persistence: settings, activity log, sessions, model suggestions,
//! and human decisions. One global database in the platform data dir
//! (override with the CRA_DB env var).

use rusqlite::{params, Connection};
use std::path::PathBuf;

pub struct Db {
    conn: Connection,
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

    #[allow(clippy::too_many_arguments)]
    pub fn log_suggestion(
        &self,
        session_id: i64,
        file: &str,
        line_start: u32,
        line_end: u32,
        model: &str,
        action: Option<&str>,
        comment: Option<&str>,
        justification: Option<&str>,
        latency_ms: i64,
        error: Option<&str>,
    ) {
        let _ = self.conn.execute(
            "INSERT INTO suggestions(ts, session_id, file, line_start, line_end, model,
                                     action, comment, justification, latency_ms, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![now(), session_id, file, line_start, line_end, model, action, comment,
                    justification, latency_ms, error],
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn log_decision(
        &self,
        session_id: i64,
        file: &str,
        line_start: u32,
        line_end: u32,
        original: &str,
        action: &str,
        final_text: &str,
        source: &str,
        human_edited: bool,
        committed: bool,
        commit_sha: Option<&str>,
        justification: Option<&str>,
    ) {
        let _ = self.conn.execute(
            "INSERT INTO decisions(ts, session_id, file, line_start, line_end, original, action,
                                   final_text, source, human_edited, committed, commit_sha, justification)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![now(), session_id, file, line_start, line_end, original, action, final_text,
                    source, human_edited as i64, committed as i64, commit_sha, justification],
        );
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
