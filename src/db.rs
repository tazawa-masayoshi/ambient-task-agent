use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Debug, Clone, Serialize)]
#[allow(dead_code)]
pub struct CodingTask {
    pub id: i64,
    pub asana_task_gid: String,
    pub asana_task_name: String,
    pub description: Option<String>,
    pub repo_key: Option<String>,
    pub branch_name: Option<String>,
    pub status: String,
    pub plan_text: Option<String>,
    pub analysis_text: Option<String>,
    pub subtasks_json: Option<String>,
    pub slack_channel: Option<String>,
    pub slack_thread_ts: Option<String>,
    pub slack_plan_ts: Option<String>,
    pub pr_url: Option<String>,
    pub error_message: Option<String>,
    pub retry_count: i32,
    pub summary: Option<String>,
    pub memory_note: Option<String>,
    pub priority_score: Option<f64>,
    pub progress_percent: Option<i32>,
    pub started_at_task: Option<String>,
    pub completed_at: Option<String>,
    pub estimated_minutes: Option<i32>,
    pub actual_minutes: Option<i32>,
    pub retrospective_note: Option<String>,
    pub complexity: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRow {
    pub session_id: String,
    pub home_cwd: String,
    pub tty: String,
    pub status: String,
    pub active_task: Option<String>,
    pub tasks_completed: i32,
    pub tasks_total: i32,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ScheduledJob {
    pub id: i64,
    pub job_key: String,
    pub schedule_cron: String,
    pub job_type: String,
    pub prompt_template: String,
    pub slack_channel: String,
    pub enabled: bool,
    pub last_run_at: Option<String>,
    pub next_run_at: Option<String>,
}

const TASK_COLUMNS: &str = "id, asana_task_gid, asana_task_name, description, repo_key, branch_name, status, plan_text, analysis_text, subtasks_json, slack_channel, slack_thread_ts, slack_plan_ts, pr_url, error_message, retry_count, summary, memory_note, priority_score, progress_percent, started_at, completed_at, estimated_minutes, actual_minutes, retrospective_note, complexity, created_at, updated_at";

fn row_to_task(row: &rusqlite::Row) -> rusqlite::Result<CodingTask> {
    Ok(CodingTask {
        id: row.get(0)?,
        asana_task_gid: row.get(1)?,
        asana_task_name: row.get(2)?,
        description: row.get(3)?,
        repo_key: row.get(4)?,
        branch_name: row.get(5)?,
        status: row.get(6)?,
        plan_text: row.get(7)?,
        analysis_text: row.get(8)?,
        subtasks_json: row.get(9)?,
        slack_channel: row.get(10)?,
        slack_thread_ts: row.get(11)?,
        slack_plan_ts: row.get(12)?,
        pr_url: row.get(13)?,
        error_message: row.get(14)?,
        retry_count: row.get(15)?,
        summary: row.get(16)?,
        memory_note: row.get(17)?,
        priority_score: row.get(18)?,
        progress_percent: row.get(19)?,
        started_at_task: row.get(20)?,
        completed_at: row.get(21)?,
        estimated_minutes: row.get(22)?,
        actual_minutes: row.get(23)?,
        retrospective_note: row.get(24)?,
        complexity: row.get(25)?,
        created_at: row.get(26)?,
        updated_at: row.get(27)?,
    })
}

impl Db {
    pub fn open(path: &PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create db directory: {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("Failed to open database: {}", path.display()))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS coding_tasks (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                asana_task_gid  TEXT NOT NULL,
                asana_task_name TEXT NOT NULL,
                repo_key        TEXT,
                branch_name     TEXT,
                status          TEXT NOT NULL DEFAULT 'pending',
                plan_text       TEXT,
                slack_channel   TEXT,
                slack_thread_ts TEXT,
                slack_plan_ts   TEXT,
                pr_url          TEXT,
                error_message   TEXT,
                retry_count     INTEGER NOT NULL DEFAULT 0,
                created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                updated_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            );

            CREATE TABLE IF NOT EXISTS webhook_events (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                event_type  TEXT NOT NULL,
                resource_gid TEXT NOT NULL,
                payload     TEXT NOT NULL,
                created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            );

            CREATE TABLE IF NOT EXISTS meeting_reminders (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id    TEXT NOT NULL,
                event_date  TEXT NOT NULL,
                notified_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                UNIQUE(event_id, event_date)
            );

            CREATE TABLE IF NOT EXISTS sessions (
                session_id      TEXT PRIMARY KEY,
                home_cwd        TEXT NOT NULL,
                tty             TEXT NOT NULL DEFAULT '',
                status          TEXT NOT NULL DEFAULT 'running',
                active_task     TEXT,
                tasks_completed INTEGER NOT NULL DEFAULT 0,
                tasks_total     INTEGER NOT NULL DEFAULT 0,
                created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                updated_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            );

            CREATE TABLE IF NOT EXISTS scheduled_jobs (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                job_key         TEXT NOT NULL UNIQUE,
                schedule_cron   TEXT NOT NULL,
                job_type        TEXT NOT NULL,
                prompt_template TEXT NOT NULL DEFAULT '',
                slack_channel   TEXT NOT NULL,
                enabled         INTEGER NOT NULL DEFAULT 1,
                last_run_at     TEXT,
                next_run_at     TEXT,
                created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                updated_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            );

            CREATE TABLE IF NOT EXISTS ops_contexts (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                channel     TEXT NOT NULL,
                thread_ts   TEXT NOT NULL,
                repo_key    TEXT NOT NULL,
                role        TEXT NOT NULL,
                content     TEXT NOT NULL,
                created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            );
            CREATE INDEX IF NOT EXISTS idx_ops_contexts_thread
                ON ops_contexts(channel, thread_ts);
            ",
        )?;

        // v4-v8: 不足カラムを一括追加（PRAGMA table_info は1回のみ）
        self.add_missing_columns(&conn, "coding_tasks", &[
            ("slack_plan_ts", "TEXT"),      // v4
            ("summary", "TEXT"),            // v5
            ("memory_note", "TEXT"),        // v5
            ("description", "TEXT"),        // v6
            ("analysis_text", "TEXT"),      // v7
            ("subtasks_json", "TEXT"),      // v7
            ("priority_score", "REAL"),     // v8
            ("progress_percent", "INTEGER"),// v8
            ("started_at", "TEXT"),         // v8
            ("completed_at", "TEXT"),       // v8
            ("estimated_minutes", "INTEGER"),// v8
            ("actual_minutes", "INTEGER"),  // v8
            ("retrospective_note", "TEXT"), // v8
            ("complexity", "TEXT"),         // v9
        ])?;

        // v7: 既存ステータスのマイグレーション（レガシーステータスが残っている場合のみ）
        let legacy_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM coding_tasks WHERE status IN ('pending','plan_posted','planning')",
            [],
            |r| r.get(0),
        )?;
        if legacy_count > 0 {
            conn.execute_batch(
                "
                UPDATE coding_tasks SET status = 'new' WHERE status = 'pending';
                UPDATE coding_tasks SET status = 'proposed' WHERE status = 'plan_posted';
                UPDATE coding_tasks SET status = 'analyzing' WHERE status = 'planning';
                ",
            )?;
            tracing::info!("Migrated {} legacy status values", legacy_count);
        }

        Ok(())
    }

    /// 不足カラムを一括追加（内部利用専用: table/col/col_type は定数のみ渡すこと）
    fn add_missing_columns(
        &self,
        conn: &Connection,
        table: &str,
        cols: &[(&str, &str)],
    ) -> Result<()> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", table))?;
        let existing: std::collections::HashSet<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))?
            .filter_map(|r| r.ok())
            .collect();
        for (col, col_type) in cols {
            if !existing.contains(*col) {
                conn.execute_batch(&format!(
                    "ALTER TABLE {} ADD COLUMN {} {}",
                    table, col, col_type
                ))?;
                tracing::info!("Added column {}.{}", table, col);
            }
        }
        Ok(())
    }

    pub fn insert_task(&self, asana_task_gid: &str, asana_task_name: &str, description: Option<&str>, repo_key: Option<&str>, slack_channel: Option<&str>) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO coding_tasks (asana_task_gid, asana_task_name, description, repo_key, slack_channel, status) VALUES (?1, ?2, ?3, ?4, ?5, 'new')",
            params![asana_task_gid, asana_task_name, description, repo_key, slack_channel],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn get_new_task(&self) -> Result<Option<CodingTask>> {
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "SELECT {} FROM coding_tasks WHERE status = 'new' ORDER BY id ASC LIMIT 1",
            TASK_COLUMNS
        );
        let mut stmt = conn.prepare(&sql)?;
        let task = stmt.query_row([], row_to_task).ok();
        Ok(task)
    }

    pub fn get_approved_task(&self) -> Result<Option<CodingTask>> {
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "SELECT {} FROM coding_tasks WHERE status = 'approved' ORDER BY id ASC LIMIT 1",
            TASK_COLUMNS
        );
        let mut stmt = conn.prepare(&sql)?;
        let task = stmt.query_row([], row_to_task).ok();
        Ok(task)
    }

    pub fn get_auto_approved_task(&self) -> Result<Option<CodingTask>> {
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "SELECT {} FROM coding_tasks WHERE status = 'auto_approved' ORDER BY id ASC LIMIT 1",
            TASK_COLUMNS
        );
        let mut stmt = conn.prepare(&sql)?;
        let task = stmt.query_row([], row_to_task).ok();
        Ok(task)
    }

    pub fn update_status(&self, id: i64, status: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE coding_tasks SET status = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?2",
            params![status, id],
        )?;
        Ok(())
    }

    pub fn update_complexity(&self, id: i64, complexity: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE coding_tasks SET complexity = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?2",
            params![complexity, id],
        )?;
        Ok(())
    }

    pub fn update_analysis(&self, id: i64, analysis_text: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE coding_tasks SET analysis_text = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?2",
            params![analysis_text, id],
        )?;
        Ok(())
    }

    pub fn update_subtasks(&self, id: i64, subtasks_json: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE coding_tasks SET subtasks_json = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?2",
            params![subtasks_json, id],
        )?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn update_plan(&self, id: i64, plan_text: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE coding_tasks SET plan_text = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?2",
            params![plan_text, id],
        )?;
        Ok(())
    }

    pub fn update_slack_thread(&self, id: i64, channel: &str, thread_ts: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE coding_tasks SET slack_channel = ?1, slack_thread_ts = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?3",
            params![channel, thread_ts, id],
        )?;
        Ok(())
    }

    pub fn set_error(&self, id: i64, error: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE coding_tasks SET status = 'failed', error_message = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?2",
            params![error, id],
        )?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn update_summary(&self, id: i64, summary: &str, memory_note: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE coding_tasks SET summary = ?1, memory_note = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?3",
            params![summary, memory_note, id],
        )?;
        Ok(())
    }

    pub fn insert_webhook_event(&self, event_type: &str, resource_gid: &str, payload: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO webhook_events (event_type, resource_gid, payload) VALUES (?1, ?2, ?3)",
            params![event_type, resource_gid, payload],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn update_plan_ts(&self, id: i64, plan_ts: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE coding_tasks SET slack_plan_ts = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?2",
            params![plan_ts, id],
        )?;
        Ok(())
    }

    /// slack_thread_ts または slack_plan_ts でタスクを検索
    pub fn find_task_by_slack_ts(&self, channel: &str, ts: &str) -> Result<Option<CodingTask>> {
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "SELECT {} FROM coding_tasks WHERE slack_channel = ?1 AND (slack_thread_ts = ?2 OR slack_plan_ts = ?2) ORDER BY id DESC LIMIT 1",
            TASK_COLUMNS
        );
        let mut stmt = conn.prepare(&sql)?;
        let task = stmt.query_row(params![channel, ts], row_to_task).ok();
        Ok(task)
    }

    /// 再生成用: status=new, analysis_text=NULL にリセット
    pub fn reset_for_regeneration(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE coding_tasks SET status = 'new', analysis_text = NULL, plan_text = NULL, subtasks_json = NULL, slack_plan_ts = NULL, error_message = NULL, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// slack_thread_ts でタスクを検索（スレッド内メッセージ用）
    pub fn find_task_by_thread_ts(&self, channel: &str, thread_ts: &str) -> Result<Option<CodingTask>> {
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "SELECT {} FROM coding_tasks WHERE slack_channel = ?1 AND slack_thread_ts = ?2 ORDER BY id DESC LIMIT 1",
            TASK_COLUMNS
        );
        let mut stmt = conn.prepare(&sql)?;
        let task = stmt.query_row(params![channel, thread_ts], row_to_task).ok();
        Ok(task)
    }

    /// coding_tasks の集計（status ごとの件数）
    pub fn count_tasks_by_status(&self) -> Result<Vec<(String, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT status, COUNT(*) FROM coding_tasks GROUP BY status ORDER BY status",
        )?;
        let rows = stmt
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// asana_task_gid でアクティブなタスクを検索
    pub fn find_task_by_gid(&self, asana_task_gid: &str) -> Result<Option<CodingTask>> {
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "SELECT {} FROM coding_tasks WHERE asana_task_gid = ?1 AND status NOT IN ('completed', 'failed', 'archived', 'done') ORDER BY id DESC LIMIT 1",
            TASK_COLUMNS
        );
        let mut stmt = conn.prepare(&sql)?;
        let task = stmt.query_row(params![asana_task_gid], row_to_task).ok();
        Ok(task)
    }

    pub fn task_exists_for_gid(&self, asana_task_gid: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM coding_tasks WHERE asana_task_gid = ?1 AND status NOT IN ('completed', 'failed', 'done')",
            params![asana_task_gid],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    // ========================================================================
    // Scheduled Jobs
    // ========================================================================

    /// スケジュールジョブを upsert（key が存在すれば更新、なければ挿入）
    pub fn upsert_scheduled_job(
        &self,
        job_key: &str,
        schedule_cron: &str,
        job_type: &str,
        prompt_template: &str,
        slack_channel: &str,
        next_run_at: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO scheduled_jobs (job_key, schedule_cron, job_type, prompt_template, slack_channel, next_run_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(job_key) DO UPDATE SET
                schedule_cron = excluded.schedule_cron,
                job_type = excluded.job_type,
                prompt_template = excluded.prompt_template,
                slack_channel = excluded.slack_channel,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
            params![job_key, schedule_cron, job_type, prompt_template, slack_channel, next_run_at],
        )?;
        Ok(())
    }

    /// 実行期限が来ているジョブを1件取得
    pub fn get_due_job(&self, now: &DateTime<Utc>) -> Result<Option<ScheduledJob>> {
        let conn = self.conn.lock().unwrap();
        let now_str = now.format("%Y-%m-%dT%H:%M:%S").to_string();
        let mut stmt = conn.prepare(
            "SELECT id, job_key, schedule_cron, job_type, prompt_template, slack_channel, enabled, last_run_at, next_run_at
             FROM scheduled_jobs
             WHERE enabled = 1 AND next_run_at IS NOT NULL AND next_run_at <= ?1
             ORDER BY next_run_at ASC LIMIT 1",
        )?;
        let job = stmt
            .query_row(params![now_str], |row| {
                Ok(ScheduledJob {
                    id: row.get(0)?,
                    job_key: row.get(1)?,
                    schedule_cron: row.get(2)?,
                    job_type: row.get(3)?,
                    prompt_template: row.get(4)?,
                    slack_channel: row.get(5)?,
                    enabled: row.get::<_, i32>(6)? != 0,
                    last_run_at: row.get(7)?,
                    next_run_at: row.get(8)?,
                })
            })
            .ok();
        Ok(job)
    }

    /// ジョブの last_run_at と next_run_at を更新
    pub fn mark_job_run(&self, id: i64, next_run_at: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE scheduled_jobs SET
                last_run_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                next_run_at = ?1,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?2",
            params![next_run_at, id],
        )?;
        Ok(())
    }

    // ========================================================================
    // Meeting Reminders
    // ========================================================================

    pub fn is_meeting_reminded(&self, event_id: &str, event_date: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM meeting_reminders WHERE event_id = ?1 AND event_date = ?2",
            params![event_id, event_date],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn mark_meeting_reminded(&self, event_id: &str, event_date: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO meeting_reminders (event_id, event_date) VALUES (?1, ?2)",
            params![event_id, event_date],
        )?;
        Ok(())
    }

    pub fn cleanup_old_reminders(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM meeting_reminders WHERE notified_at < strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-7 days')",
            [],
        )?;
        Ok(())
    }

    // ========================================================================
    // Sessions
    // ========================================================================

    pub fn upsert_session(&self, session: &SessionRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sessions (session_id, home_cwd, tty, status, active_task, tasks_completed, tasks_total, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
             ON CONFLICT(session_id) DO UPDATE SET
                home_cwd = excluded.home_cwd,
                tty = CASE WHEN excluded.tty = '' THEN sessions.tty ELSE excluded.tty END,
                status = excluded.status,
                active_task = excluded.active_task,
                tasks_completed = excluded.tasks_completed,
                tasks_total = excluded.tasks_total,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
            params![
                session.session_id,
                session.home_cwd,
                session.tty,
                session.status,
                session.active_task,
                session.tasks_completed,
                session.tasks_total,
                session.created_at,
            ],
        )?;
        Ok(())
    }

    pub fn get_session(&self, session_id: &str) -> Result<Option<SessionRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT session_id, home_cwd, tty, status, active_task, tasks_completed, tasks_total, created_at, updated_at
             FROM sessions WHERE session_id = ?1",
        )?;
        let row = stmt
            .query_row(params![session_id], |row| {
                Ok(SessionRow {
                    session_id: row.get(0)?,
                    home_cwd: row.get(1)?,
                    tty: row.get(2)?,
                    status: row.get(3)?,
                    active_task: row.get(4)?,
                    tasks_completed: row.get(5)?,
                    tasks_total: row.get(6)?,
                    created_at: row.get(7)?,
                    updated_at: row.get(8)?,
                })
            })
            .ok();
        Ok(row)
    }

    /// アクティブ + 2h以内の stopped セッションを返す
    pub fn list_active_sessions(&self) -> Result<Vec<SessionRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT session_id, home_cwd, tty, status, active_task, tasks_completed, tasks_total, created_at, updated_at
             FROM sessions
             WHERE status != 'stopped'
                OR updated_at > strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-2 hours')
             ORDER BY updated_at DESC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(SessionRow {
                    session_id: row.get(0)?,
                    home_cwd: row.get(1)?,
                    tty: row.get(2)?,
                    status: row.get(3)?,
                    active_task: row.get(4)?,
                    tasks_completed: row.get(5)?,
                    tasks_total: row.get(6)?,
                    created_at: row.get(7)?,
                    updated_at: row.get(8)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// 2h超の stopped セッションを削除
    pub fn cleanup_stale_sessions(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let deleted = conn.execute(
            "DELETE FROM sessions WHERE status = 'stopped' AND updated_at < strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-2 hours')",
            [],
        )?;
        Ok(deleted)
    }

    // ========================================================================
    // Coding Tasks (list)
    // ========================================================================

    /// coding_tasks をフィルタ付きで一覧取得
    pub fn list_tasks(&self, status_filter: Option<&str>) -> Result<Vec<CodingTask>> {
        let conn = self.conn.lock().unwrap();
        let (sql, filter_params): (String, Vec<Box<dyn rusqlite::types::ToSql>>) = match status_filter {
            Some(status) => (
                format!("SELECT {} FROM coding_tasks WHERE status = ?1 ORDER BY updated_at DESC", TASK_COLUMNS),
                vec![Box::new(status.to_string())],
            ),
            None => (
                format!("SELECT {} FROM coding_tasks ORDER BY updated_at DESC", TASK_COLUMNS),
                vec![],
            ),
        };
        let mut stmt = conn.prepare(&sql)?;
        let params_refs: Vec<&dyn rusqlite::types::ToSql> = filter_params.iter().map(|p| p.as_ref()).collect();
        let rows = stmt
            .query_map(params_refs.as_slice(), row_to_task)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// 指定時間以上 updated_at が更新されていないアクティブタスクを取得
    pub fn get_stagnant_tasks(&self, threshold_hours: i64) -> Result<Vec<CodingTask>> {
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "SELECT {} FROM coding_tasks WHERE status IN ('ready', 'in_progress') AND updated_at < strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-{} hours') ORDER BY updated_at ASC",
            TASK_COLUMNS, threshold_hours
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map([], row_to_task)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// 指定日時以降に完了したタスク数
    pub fn count_completed_since(&self, since: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM coding_tasks WHERE status = 'done' AND updated_at >= ?1",
            params![since],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// アクティブなタスク一覧（new〜in_progress）
    pub fn get_active_tasks(&self) -> Result<Vec<CodingTask>> {
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "SELECT {} FROM coding_tasks WHERE status NOT IN ('done', 'failed', 'archived') ORDER BY updated_at DESC",
            TASK_COLUMNS
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map([], row_to_task)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    // ========================================================================
    // PM Layer: Priority / Progress / Subtask Management
    // ========================================================================

    /// サブタスクのステータスを更新し、progress_percent を再計算
    pub fn update_subtask_status(&self, id: i64, subtask_index: u32, status: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let json_str: Option<String> = conn.query_row(
            "SELECT subtasks_json FROM coding_tasks WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )?;

        let json_str = json_str.ok_or_else(|| anyhow::anyhow!("No subtasks_json for task {}", id))?;
        let mut subtasks: Vec<serde_json::Value> = serde_json::from_str(&json_str)?;

        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let mut found = false;
        for s in subtasks.iter_mut() {
            if s.get("index").and_then(|v| v.as_u64()) == Some(subtask_index as u64) {
                s["status"] = serde_json::Value::String(status.to_string());
                if status == "in_progress" && s.get("started_at").and_then(|v| v.as_str()).is_none() {
                    s["started_at"] = serde_json::Value::String(now.clone());
                }
                if status == "done" {
                    s["completed_at"] = serde_json::Value::String(now.clone());
                }
                found = true;
                break;
            }
        }
        if !found {
            anyhow::bail!("Subtask index {} not found in task {}", subtask_index, id);
        }

        let done_count = subtasks.iter().filter(|s| s.get("status").and_then(|v| v.as_str()) == Some("done")).count();
        let progress = if subtasks.is_empty() { 0 } else { ((done_count as f64 / subtasks.len() as f64) * 100.0).round() as i32 };

        let new_json = serde_json::to_string(&subtasks)?;
        conn.execute(
            "UPDATE coding_tasks SET subtasks_json = ?1, progress_percent = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?3",
            params![new_json, progress, id],
        )?;
        Ok(())
    }

    /// 優先度スコアを更新
    pub fn update_priority_score(&self, id: i64, score: f64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE coding_tasks SET priority_score = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?2",
            params![score, id],
        )?;
        Ok(())
    }

    /// 進捗率を更新
    pub fn update_progress(&self, id: i64, percent: i32) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE coding_tasks SET progress_percent = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?2",
            params![percent, id],
        )?;
        Ok(())
    }

    /// タスクを開始（started_at を記録、COALESCE で既存値を保持）
    pub fn start_task(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE coding_tasks SET status = 'in_progress', started_at = COALESCE(started_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')), updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// タスクを完了（actual_minutes を自動計算、retrospective_note を記録）
    pub fn complete_task_with_retrospective(&self, id: i64, note: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        // started_at から actual_minutes を計算
        let started_at: Option<String> = conn.query_row(
            "SELECT started_at FROM coding_tasks WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )?;

        let actual_minutes: Option<i32> = started_at.and_then(|started| {
            DateTime::parse_from_rfc3339(&started).ok().or_else(|| {
                chrono::NaiveDateTime::parse_from_str(&started, "%Y-%m-%dT%H:%M:%S%.fZ")
                    .ok()
                    .map(|ndt| ndt.and_utc().fixed_offset())
            })
        }).map(|started| {
            let elapsed = Utc::now().signed_duration_since(started);
            elapsed.num_minutes() as i32
        });

        conn.execute(
            "UPDATE coding_tasks SET status = 'done', completed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), actual_minutes = ?1, progress_percent = 100, retrospective_note = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?3",
            params![actual_minutes, note, id],
        )?;
        Ok(())
    }

    /// 優先度スコア降順でタスクを取得
    pub fn get_tasks_by_priority(&self) -> Result<Vec<CodingTask>> {
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "SELECT {} FROM coding_tasks WHERE status NOT IN ('done', 'failed', 'archived') ORDER BY COALESCE(priority_score, 0) DESC",
            TASK_COLUMNS
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map([], row_to_task)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// ID でタスクを取得
    pub fn get_task_by_id(&self, id: i64) -> Result<Option<CodingTask>> {
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "SELECT {} FROM coding_tasks WHERE id = ?1",
            TASK_COLUMNS
        );
        let mut stmt = conn.prepare(&sql)?;
        let task = stmt.query_row(params![id], row_to_task).ok();
        Ok(task)
    }

    pub fn update_branch_name(&self, id: i64, branch_name: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE coding_tasks SET branch_name = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?2",
            params![branch_name, id],
        )?;
        Ok(())
    }

    pub fn update_pr_url(&self, id: i64, pr_url: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE coding_tasks SET pr_url = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?2",
            params![pr_url, id],
        )?;
        Ok(())
    }

    /// ci_pending 状態のタスクを1件取得
    pub fn get_ci_pending_task(&self) -> Result<Option<CodingTask>> {
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "SELECT {} FROM coding_tasks WHERE status = 'ci_pending' ORDER BY id ASC LIMIT 1",
            TASK_COLUMNS
        );
        let mut stmt = conn.prepare(&sql)?;
        let task = stmt.query_row([], row_to_task).ok();
        Ok(task)
    }

    /// retry_count をインクリメントして新しい値を返す
    pub fn increment_retry_count(&self, id: i64) -> Result<i32> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE coding_tasks SET retry_count = retry_count + 1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1",
            params![id],
        )?;
        let count: i32 = conn.query_row(
            "SELECT retry_count FROM coding_tasks WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    // ── ops_contexts ──

    /// ops 会話メッセージを保存
    pub fn append_ops_context(
        &self,
        channel: &str,
        thread_ts: &str,
        repo_key: &str,
        role: &str,
        content: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO ops_contexts (channel, thread_ts, repo_key, role, content) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![channel, thread_ts, repo_key, role, content],
        )?;
        Ok(())
    }

    /// ops スレッドの会話履歴を取得（時系列順）
    pub fn get_ops_context(&self, channel: &str, thread_ts: &str) -> Result<Vec<OpsMessage>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT role, content, created_at FROM ops_contexts WHERE channel = ?1 AND thread_ts = ?2 ORDER BY id ASC",
        )?;
        let rows = stmt
            .query_map(params![channel, thread_ts], |row| {
                Ok(OpsMessage {
                    role: row.get(0)?,
                    content: row.get(1)?,
                    created_at: row.get(2)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// ops スレッドの repo_key を取得
    pub fn get_ops_repo_key(&self, channel: &str, thread_ts: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let result = conn.query_row(
            "SELECT repo_key FROM ops_contexts WHERE channel = ?1 AND thread_ts = ?2 LIMIT 1",
            params![channel, thread_ts],
            |row| row.get(0),
        );
        match result {
            Ok(key) => Ok(Some(key)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct OpsMessage {
    pub role: String,
    pub content: String,
    pub created_at: String,
}
