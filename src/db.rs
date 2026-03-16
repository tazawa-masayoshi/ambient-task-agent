use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
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
    /// claude -p セッション継続用 ID
    pub claude_session_id: Option<String>,
    /// サブタスクループの現在インデックス（1-based）
    pub current_subtask_index: Option<i32>,
    pub created_at: String,
    pub updated_at: String,
    /// タスク入口識別: 'asana' | 'slack' | 'manual'
    pub source: String,
    /// conversing フェーズの会話スレッド TS（ops_contexts のキーに使う）
    pub converse_thread_ts: Option<String>,
    /// 初回分類結果: "execute" | "converse"
    pub initial_classification: Option<String>,
    /// 分類の実際の結果: "correct" | "needed_converse" | "needed_manual"
    pub classification_outcome: Option<String>,
}

/// 分類履歴レコード（few-shot 学習用）
pub struct ClassificationRecord {
    pub task_name: String,
    pub description: String,
    pub classification: String,
    pub outcome: String,
}

/// サブタスク定義（旧 decomposer で使用、DB の subtasks_json に保存済みデータの読み取り用に保持）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subtask {
    pub index: u32,
    pub title: String,
    pub detail: String,
    #[serde(default)]
    pub depends_on: Vec<u32>,
    #[serde(default)]
    pub estimated_minutes: Option<u32>,
    #[serde(default = "default_subtask_status")]
    pub status: String,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub completed_at: Option<String>,
    #[serde(default)]
    pub actual_minutes: Option<u32>,
}

fn default_subtask_status() -> String {
    "pending".to_string()
}

/// 着手可能なサブタスク（pending + 依存解決済み）を返す
pub fn get_actionable_subtasks(subtasks: &[Subtask]) -> Vec<&Subtask> {
    let done_indices: HashSet<u32> = subtasks
        .iter()
        .filter(|s| s.status == "done")
        .map(|s| s.index)
        .collect();

    subtasks
        .iter()
        .filter(|s| {
            s.status == "pending"
                && (s.depends_on.is_empty() || s.depends_on.iter().all(|dep| done_indices.contains(dep)))
        })
        .collect()
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

const TASK_COLUMNS: &str = "id, asana_task_gid, asana_task_name, description, repo_key, branch_name, status, plan_text, analysis_text, subtasks_json, slack_channel, slack_thread_ts, slack_plan_ts, pr_url, error_message, retry_count, summary, memory_note, priority_score, progress_percent, started_at, completed_at, estimated_minutes, actual_minutes, retrospective_note, complexity, claude_session_id, current_subtask_index, created_at, updated_at, source, converse_thread_ts, initial_classification, classification_outcome";

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
        claude_session_id: row.get(26)?,
        current_subtask_index: row.get(27)?,
        created_at: row.get(28)?,
        updated_at: row.get(29)?,
        source: row.get(30).unwrap_or_else(|_| "asana".to_string()),
        converse_thread_ts: row.get(31).unwrap_or(None),
        initial_classification: row.get(32).unwrap_or(None),
        classification_outcome: row.get(33).unwrap_or(None),
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

            CREATE TABLE IF NOT EXISTS ops_queue (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                channel       TEXT NOT NULL,
                message_ts    TEXT NOT NULL,
                thread_ts     TEXT,
                repo_key      TEXT NOT NULL,
                message_text  TEXT NOT NULL,
                event_json    TEXT NOT NULL DEFAULT '{}',
                status        TEXT NOT NULL DEFAULT 'pending',
                retry_count   INTEGER NOT NULL DEFAULT 0,
                error_message TEXT,
                created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                updated_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            );
            CREATE INDEX IF NOT EXISTS idx_ops_queue_status
                ON ops_queue(status, created_at);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_ops_queue_channel_ts
                ON ops_queue(channel, message_ts);

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
            ("claude_session_id", "TEXT"),  // v10: セッション継続用
            ("current_subtask_index", "INTEGER"), // v10: サブタスクループ進捗
            ("source", "TEXT NOT NULL DEFAULT 'asana'"), // v11: タスク入口識別 (asana/slack/manual)
            ("converse_thread_ts", "TEXT"),              // v11: conversing フェーズのスレッド TS
            ("initial_classification", "TEXT"),          // v13: 初回分類結果 (execute/converse)
            ("classification_outcome", "TEXT"),          // v13: 分類の実際の結果 (correct/needed_converse/needed_manual)
        ])?;

        // ops_queue: 追加カラム
        self.add_missing_columns(&conn, "ops_queue", &[
            ("thread_ts", "TEXT"),
            ("done_at", "TEXT"),
            ("resolved_at", "TEXT"),
            ("reminder_count", "INTEGER DEFAULT 0"),
            ("notify_ts", "TEXT"),
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

        // v12: 旧ステータスモデルから新ステータスモデルへのマイグレーション
        // proposed/analyzing → conversing、approved/auto_approved → executing
        let v12_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM coding_tasks WHERE status IN ('proposed','analyzing','approved','auto_approved')",
            [],
            |r| r.get(0),
        )?;
        if v12_count > 0 {
            conn.execute_batch(
                "
                UPDATE coding_tasks SET status = 'conversing' WHERE status IN ('proposed', 'analyzing');
                UPDATE coding_tasks SET status = 'executing' WHERE status IN ('approved', 'auto_approved');
                ",
            )?;
            tracing::info!("v12: Migrated {} tasks to new status model", v12_count);
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

    /// Slack 起点のタスクを登録する（source='slack'、ダミー GID を自動生成）
    #[allow(dead_code)]
    pub fn insert_task_from_slack(
        &self,
        task_name: &str,
        description: Option<&str>,
        repo_key: Option<&str>,
        slack_channel: Option<&str>,
        slack_thread_ts: Option<&str>,
    ) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let dummy_gid = format!("slack_task_{}", chrono::Utc::now().timestamp_millis());
        conn.execute(
            "INSERT INTO coding_tasks (asana_task_gid, asana_task_name, description, repo_key, slack_channel, slack_thread_ts, status, source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'new', 'slack')",
            params![dummy_gid, task_name, description, repo_key, slack_channel, slack_thread_ts],
        )?;
        Ok(conn.last_insert_rowid())
    }

    fn get_task_by_status(&self, status: &str) -> Result<Option<CodingTask>> {
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "SELECT {} FROM coding_tasks WHERE status = ?1 ORDER BY id ASC LIMIT 1",
            TASK_COLUMNS
        );
        let mut stmt = conn.prepare(&sql)?;
        Ok(stmt.query_row(params![status], row_to_task).ok())
    }

    pub fn get_new_task(&self) -> Result<Option<CodingTask>> {
        self.get_task_by_status("new")
    }

    pub fn get_approved_task(&self) -> Result<Option<CodingTask>> {
        self.get_task_by_status("approved")
    }

    pub fn get_auto_approved_task(&self) -> Result<Option<CodingTask>> {
        self.get_task_by_status("auto_approved")
    }

    /// conversing 状態のタスクを1件取得（会話フェーズ処理用）
    #[allow(dead_code)]
    pub fn get_conversing_task(&self) -> Result<Option<CodingTask>> {
        self.get_task_by_status("conversing")
    }

    /// executing 状態のタスクを1件取得（実行フェーズ処理用）
    #[allow(dead_code)]
    pub fn get_executing_task(&self) -> Result<Option<CodingTask>> {
        self.get_task_by_status("executing")
    }

    /// manual 状態のタスクを1件取得（手動承認待ち）
    #[allow(dead_code)]
    pub fn get_manual_task(&self) -> Result<Option<CodingTask>> {
        self.get_task_by_status("manual")
    }

    pub fn update_status(&self, id: i64, status: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE coding_tasks SET status = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?2",
            params![status, id],
        )?;
        Ok(())
    }

    pub fn update_description(&self, id: i64, description: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE coding_tasks SET description = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?2",
            params![description, id],
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
            "UPDATE coding_tasks SET status = 'new', analysis_text = NULL, plan_text = NULL, subtasks_json = NULL, slack_plan_ts = NULL, error_message = NULL, claude_session_id = NULL, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1",
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
    // PM Layer: Priority
    // ========================================================================

    /// 優先度スコアを更新
    pub fn update_priority_score(&self, id: i64, score: f64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE coding_tasks SET priority_score = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?2",
            params![score, id],
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
        self.get_task_by_status("ci_pending")
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

    /// claude -p セッション ID を保存
    pub fn update_session_id(&self, id: i64, session_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE coding_tasks SET claude_session_id = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?2",
            params![session_id, id],
        )?;
        Ok(())
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

    /// ops スレッドの会話履歴を削除（Inception リビジョン時にターン1からやり直す）
    #[allow(dead_code)]
    pub fn clear_ops_context(&self, channel: &str, thread_ts: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM ops_contexts WHERE channel = ?1 AND thread_ts = ?2",
            params![channel, thread_ts],
        )?;
        Ok(())
    }

    /// ops スレッドの repo_key を取得
    #[allow(dead_code)]
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

    // ========================================================================
    // ops_queue
    // ========================================================================

    /// ops キューにアイテムを追加（同一 channel+message_ts の重複は無視）
    #[allow(clippy::too_many_arguments)]
    pub fn enqueue_ops(
        &self,
        channel: &str,
        message_ts: &str,
        thread_ts: Option<&str>,
        repo_key: &str,
        message_text: &str,
        event_json: &str,
        status: &str,
    ) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        // 同じメッセージが既にキューにある場合はスキップ（done/failed 含む）
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM ops_queue WHERE channel = ?1 AND message_ts = ?2)",
            params![channel, message_ts],
            |row| row.get(0),
        )?;
        if exists {
            tracing::debug!("ops_queue: duplicate skipped (channel={}, ts={})", channel, message_ts);
            return Ok(0);
        }
        conn.execute(
            "INSERT INTO ops_queue (channel, message_ts, thread_ts, repo_key, message_text, event_json, status) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![channel, message_ts, thread_ts, repo_key, message_text, event_json, status],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// 10分以上 processing のまま放置されたアイテムを ready に戻す
    pub fn recover_stale_ops(&self) -> Result<u64> {
        let conn = self.conn.lock().unwrap();
        let count = conn.execute(
            "UPDATE ops_queue SET status = 'ready', \
             error_message = 'recovered from stale processing', \
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') \
             WHERE status = 'processing' \
             AND updated_at < strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-10 minutes')",
            [],
        )?;
        Ok(count as u64)
    }

    /// 処理対象のキューアイテムを1件取得（pending/ready、同一 repo_key の processing がなければ）
    pub fn dequeue_ops_item(&self) -> Result<Option<OpsQueueItem>> {
        let conn = self.conn.lock().unwrap();
        let result = conn.query_row(
            "SELECT id, channel, message_ts, thread_ts, repo_key, message_text, event_json, status, retry_count \
             FROM ops_queue WHERE status IN ('pending', 'ready') \
             AND repo_key NOT IN (SELECT DISTINCT repo_key FROM ops_queue WHERE status = 'processing') \
             ORDER BY created_at ASC LIMIT 1",
            [],
            |row| {
                Ok(OpsQueueItem {
                    id: row.get(0)?,
                    channel: row.get(1)?,
                    message_ts: row.get(2)?,
                    thread_ts: row.get(3)?,
                    repo_key: row.get(4)?,
                    message_text: row.get(5)?,
                    event_json: row.get(6)?,
                    status: row.get(7)?,
                    retry_count: row.get(8)?,
                })
            },
        );
        match result {
            Ok(item) => Ok(Some(item)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// キューアイテムのステータスを processing に更新
    pub fn mark_ops_processing(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE ops_queue SET status = 'processing', updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// キューアイテムを完了にする
    pub fn mark_ops_done(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE ops_queue SET status = 'done', \
             done_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), \
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// 完了通知メッセージの ts を記録（ボタン更新用）
    pub fn set_ops_notify_ts(&self, id: i64, ts: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE ops_queue SET notify_ts = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1",
            params![id, ts],
        )?;
        Ok(())
    }

    /// ops アイテムを解決済みにする（ボタン押下時）
    pub fn resolve_ops(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE ops_queue SET resolved_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), \
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// フォローアップが必要な ops アイテムを取得
    /// （done + resolved_at IS NULL + done_at が指定時間以上経過）
    pub fn get_ops_needing_followup(&self) -> Result<Vec<OpsFollowupItem>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, channel, message_ts, thread_ts, repo_key, message_text, \
             done_at, reminder_count, notify_ts \
             FROM ops_queue \
             WHERE status = 'done' AND resolved_at IS NULL AND done_at IS NOT NULL \
             ORDER BY done_at ASC"
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(OpsFollowupItem {
                id: row.get(0)?,
                channel: row.get(1)?,
                message_ts: row.get(2)?,
                thread_ts: row.get(3)?,
                repo_key: row.get(4)?,
                message_text: row.get(5)?,
                done_at: row.get(6)?,
                reminder_count: row.get(7)?,
                notify_ts: row.get(8)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// リマインダー送信後にカウントを更新
    pub fn increment_ops_reminder(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE ops_queue SET reminder_count = reminder_count + 1, \
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// ops アイテムを保留にする（7日後）
    pub fn mark_ops_on_hold(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE ops_queue SET status = 'on_hold', \
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// ops アイテムを ID で取得
    pub fn get_ops_item(&self, id: i64) -> Result<Option<OpsQueueItem>> {
        let conn = self.conn.lock().unwrap();
        let result = conn.query_row(
            "SELECT id, channel, message_ts, thread_ts, repo_key, message_text, event_json, status, retry_count \
             FROM ops_queue WHERE id = ?1",
            params![id],
            |row| {
                Ok(OpsQueueItem {
                    id: row.get(0)?,
                    channel: row.get(1)?,
                    message_ts: row.get(2)?,
                    thread_ts: row.get(3)?,
                    repo_key: row.get(4)?,
                    message_text: row.get(5)?,
                    event_json: row.get(6)?,
                    status: row.get(7)?,
                    retry_count: row.get(8)?,
                })
            },
        );
        match result {
            Ok(item) => Ok(Some(item)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// ops アイテムから coding_task を作成（エスカレーション）
    pub fn create_task_from_ops(
        &self,
        ops_id: i64,
        task_name: &str,
        description: &str,
        repo_key: &str,
        channel: &str,
        thread_ts: &str,
    ) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO coding_tasks (asana_task_gid, asana_task_name, description, repo_key, \
             status, slack_channel, slack_thread_ts) \
             VALUES (?1, ?2, ?3, ?4, 'new', ?5, ?6)",
            params![
                format!("ops_{}", ops_id),
                task_name,
                description,
                repo_key,
                channel,
                thread_ts,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// ops アイテムから coding_task を指定ステータスで作成（Inception 承認後に executing で直接登録するために使用）
    #[allow(dead_code, clippy::too_many_arguments)]
    pub fn create_task_from_ops_with_status(
        &self,
        ops_id: i64,
        task_name: &str,
        description: &str,
        repo_key: &str,
        channel: &str,
        thread_ts: &str,
        status: &str,
    ) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO coding_tasks (asana_task_gid, asana_task_name, description, repo_key, \
             status, slack_channel, slack_thread_ts, source) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'slack')",
            params![
                format!("ops_{}", ops_id),
                task_name,
                description,
                repo_key,
                status,
                channel,
                thread_ts,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// conversing フェーズの会話スレッド TS を更新
    #[allow(dead_code)]
    pub fn update_converse_thread_ts(&self, id: i64, converse_thread_ts: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE coding_tasks SET converse_thread_ts = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?2",
            params![converse_thread_ts, id],
        )?;
        Ok(())
    }

    /// conversing 状態のタスクのうち、指定スレッドに対応するものを取得
    #[allow(dead_code)]
    pub fn find_conversing_task_by_thread(
        &self,
        channel: &str,
        thread_ts: &str,
    ) -> Result<Option<CodingTask>> {
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "SELECT {} FROM coding_tasks \
             WHERE status = 'conversing' \
             AND slack_channel = ?1 \
             AND (slack_thread_ts = ?2 OR converse_thread_ts = ?2) \
             ORDER BY id DESC LIMIT 1",
            TASK_COLUMNS
        );
        let mut stmt = conn.prepare(&sql)?;
        let task = stmt.query_row(params![channel, thread_ts], row_to_task).ok();
        Ok(task)
    }

    /// conversing 状態のタスクのうち、ユーザーが返信済み（ops_contexts の最新が user ロール）のものを取得
    pub fn get_conversing_tasks_needing_response(&self) -> Result<Vec<CodingTask>> {
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "SELECT {} FROM coding_tasks ct \
             WHERE ct.status = 'conversing' \
               AND ct.converse_thread_ts IS NOT NULL \
               AND EXISTS ( \
                 SELECT 1 FROM ops_contexts oc \
                 WHERE oc.channel = ct.slack_channel \
                   AND oc.thread_ts = ct.converse_thread_ts \
                   AND oc.role = 'user' \
                   AND oc.id = ( \
                     SELECT MAX(id) FROM ops_contexts \
                     WHERE channel = oc.channel AND thread_ts = oc.thread_ts \
                   ) \
               )",
            TASK_COLUMNS
        );
        let mut stmt = conn.prepare(&sql)?;
        let tasks = stmt.query_map([], row_to_task)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(tasks)
    }

    /// conversing 状態で updated_at が cutoff_hours 以上前のタスクを取得（タイムアウト対象）
    pub fn get_stale_conversing_tasks(&self, cutoff_hours: i64) -> Result<Vec<CodingTask>> {
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "SELECT {} FROM coding_tasks \
             WHERE status = 'conversing' \
               AND updated_at < strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-{} hours') \
             ORDER BY id ASC",
            TASK_COLUMNS, cutoff_hours
        );
        let mut stmt = conn.prepare(&sql)?;
        let tasks = stmt.query_map([], row_to_task)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(tasks)
    }

    /// 初回分類結果を記録
    pub fn set_initial_classification(&self, id: i64, classification: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE coding_tasks SET initial_classification = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?2",
            params![classification, id],
        )?;
        Ok(())
    }

    /// 分類結果の実際の outcome を記録
    pub fn set_classification_outcome(&self, id: i64, outcome: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE coding_tasks SET classification_outcome = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?2",
            params![outcome, id],
        )?;
        Ok(())
    }

    /// 直近の分類履歴を取得（few-shot 学習用）
    pub fn get_recent_classification_history(&self, limit: usize) -> Result<Vec<ClassificationRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT asana_task_name, description, initial_classification, classification_outcome \
             FROM coding_tasks \
             WHERE initial_classification IS NOT NULL AND classification_outcome IS NOT NULL \
             ORDER BY id DESC LIMIT ?1"
        )?;
        let records = stmt.query_map(params![limit as i64], |row| {
            Ok(ClassificationRecord {
                task_name: row.get(0)?,
                description: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                classification: row.get::<_, String>(2)?,
                outcome: row.get::<_, String>(3)?,
            })
        })?.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(records)
    }

    /// キューアイテムをスキップ（対応不要と分類）
    pub fn mark_ops_skipped(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE ops_queue SET status = 'skipped', updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// キューアイテムをリトライ待ちに戻す
    pub fn mark_ops_retry(&self, id: i64, error: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE ops_queue SET status = 'ready', retry_count = retry_count + 1, \
             error_message = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1",
            params![id, error],
        )?;
        Ok(())
    }

    /// キューアイテムを失敗にする
    pub fn mark_ops_failed(&self, id: i64, error: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE ops_queue SET status = 'failed', error_message = ?2, \
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1",
            params![id, error],
        )?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct OpsMessage {
    pub role: String,
    pub content: String,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct OpsQueueItem {
    pub id: i64,
    pub channel: String,
    pub message_ts: String,
    /// スレッド返信先の ts（None ならトップレベル → message_ts をスレッドに使う）
    pub thread_ts: Option<String>,
    pub repo_key: String,
    pub message_text: String,
    pub event_json: String,
    pub status: String,
    pub retry_count: i64,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct OpsFollowupItem {
    pub id: i64,
    pub channel: String,
    pub message_ts: String,
    pub thread_ts: Option<String>,
    pub repo_key: String,
    pub message_text: String,
    pub done_at: String,
    pub reminder_count: i64,
    pub notify_ts: Option<String>,
}
