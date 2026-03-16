use anyhow::{Context, Result};
use serde::Serialize;
use std::path::PathBuf;

use crate::db::{CodingTask, Db};

/// タスクファイルのパス: {repos_base_dir}/.agent/tasks/{id}.md
pub fn task_file_path(repos_base_dir: &str, task_id: i64) -> PathBuf {
    PathBuf::from(repos_base_dir)
        .join(".agent")
        .join("tasks")
        .join(format!("{}.md", task_id))
}

/// タスクファイルを読み込み（CLI用）
pub fn read_task_file(repos_base_dir: &str, task_id: i64) -> Result<String> {
    let path = task_file_path(repos_base_dir, task_id);
    std::fs::read_to_string(&path)
        .with_context(|| format!("Task file not found: {}", path.display()))
}

#[allow(dead_code)]
/// 完了済みタスクファイルを削除
pub fn cleanup_done_tasks(repos_base_dir: &str, done_ids: &[i64]) -> Result<()> {
    for id in done_ids {
        let path = task_file_path(repos_base_dir, *id);
        if path.exists() {
            std::fs::remove_file(&path)?;
            tracing::info!("Cleaned up task file: {}", path.display());
        }
    }
    Ok(())
}

// ── wez-sidebar タスクキャッシュ同期 ──

/// wez-sidebar の TasksFile 形式
#[derive(Serialize)]
pub struct WezTasksFile {
    pub tasks: Vec<WezTask>,
}

/// wez-sidebar の Task 形式
#[derive(Serialize)]
pub struct WezTask {
    pub id: String,
    pub title: String,
    pub status: String,
    pub priority: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due_on: Option<String>,
}

/// CodingTask のステータスを wez-sidebar 用に変換
fn to_wez_status(status: &str) -> &str {
    match status {
        "done" => "completed",
        "approved" | "decomposing" | "ready" | "executing" | "auto_approved" => "in_progress",
        _ => "pending", // new, analyzing, proposed, error, rejected
    }
}

/// priority_score を wez-sidebar の priority (1=high, 2=medium, 3=low) に変換
fn to_wez_priority(score: Option<f64>) -> i32 {
    match score {
        Some(s) if s >= 7.0 => 1,
        Some(s) if s >= 4.0 => 2,
        _ => 3,
    }
}

/// CodingTask リストを wez-sidebar 形式に変換
pub fn to_wez_tasks_file(tasks: &[CodingTask]) -> WezTasksFile {
    let wez_tasks = tasks
        .iter()
        .map(|t| WezTask {
            id: t.asana_task_gid.clone(),
            title: t.asana_task_name.clone(),
            status: to_wez_status(&t.status).to_string(),
            priority: to_wez_priority(t.priority_score),
            due_on: None,
        })
        .collect();
    WezTasksFile { tasks: wez_tasks }
}

/// DB のアクティブタスクを wez-sidebar 形式の JSON に書き出す
/// DB が空の場合は既存キャッシュ（Asana 同期結果）を上書きしない
pub fn sync_tasks_cache(db: &Db, cache_path: &str) -> Result<()> {
    let tasks = db.get_active_tasks()?;
    if tasks.is_empty() {
        tracing::debug!("Tasks cache: DB empty, skipping write to preserve Asana cache");
        return Ok(());
    }
    let file = to_wez_tasks_file(&tasks);
    let json = serde_json::to_string_pretty(&file)?;

    let path = PathBuf::from(cache_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, json)
        .with_context(|| format!("Failed to write tasks cache: {}", path.display()))?;

    tracing::debug!("Tasks cache synced: {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_cleanup_done_tasks() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().to_str().unwrap();

        // タスクファイルを手動作成
        let path = task_file_path(base, 1);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "test").unwrap();
        assert!(path.exists());

        cleanup_done_tasks(base, &[1]).unwrap();
        assert!(!path.exists());
    }
}
