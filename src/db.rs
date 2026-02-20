use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Debug, Clone)]
pub struct CodingTask {
    pub id: i64,
    pub asana_task_gid: String,
    pub asana_task_name: String,
    pub repo_key: Option<String>,
    pub branch_name: Option<String>,
    pub status: String,
    pub plan_text: Option<String>,
    pub slack_channel: Option<String>,
    pub slack_thread_ts: Option<String>,
    pub pr_url: Option<String>,
    pub error_message: Option<String>,
    pub retry_count: i32,
    pub created_at: String,
    pub updated_at: String,
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
            ",
        )?;
        Ok(())
    }

    pub fn insert_task(&self, asana_task_gid: &str, asana_task_name: &str, repo_key: Option<&str>, slack_channel: Option<&str>) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO coding_tasks (asana_task_gid, asana_task_name, repo_key, slack_channel) VALUES (?1, ?2, ?3, ?4)",
            params![asana_task_gid, asana_task_name, repo_key, slack_channel],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn get_pending_task(&self) -> Result<Option<CodingTask>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, asana_task_gid, asana_task_name, repo_key, branch_name, status, plan_text, slack_channel, slack_thread_ts, pr_url, error_message, retry_count, created_at, updated_at
             FROM coding_tasks WHERE status = 'pending' ORDER BY id ASC LIMIT 1",
        )?;
        let task = stmt
            .query_row([], |row| {
                Ok(CodingTask {
                    id: row.get(0)?,
                    asana_task_gid: row.get(1)?,
                    asana_task_name: row.get(2)?,
                    repo_key: row.get(3)?,
                    branch_name: row.get(4)?,
                    status: row.get(5)?,
                    plan_text: row.get(6)?,
                    slack_channel: row.get(7)?,
                    slack_thread_ts: row.get(8)?,
                    pr_url: row.get(9)?,
                    error_message: row.get(10)?,
                    retry_count: row.get(11)?,
                    created_at: row.get(12)?,
                    updated_at: row.get(13)?,
                })
            })
            .ok();
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

    pub fn insert_webhook_event(&self, event_type: &str, resource_gid: &str, payload: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO webhook_events (event_type, resource_gid, payload) VALUES (?1, ?2, ?3)",
            params![event_type, resource_gid, payload],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn task_exists_for_gid(&self, asana_task_gid: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM coding_tasks WHERE asana_task_gid = ?1 AND status NOT IN ('completed', 'failed')",
            params![asana_task_gid],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }
}
