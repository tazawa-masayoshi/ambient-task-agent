use anyhow::{Context, Result};
use serde::Serialize;
use std::path::PathBuf;

use crate::db::{CodingTask, Db};

use super::decomposer::Subtask;

/// タスクファイルのパス: {repos_base_dir}/.agent/tasks/{id}.md
pub fn task_file_path(repos_base_dir: &str, task_id: i64) -> PathBuf {
    PathBuf::from(repos_base_dir)
        .join(".agent")
        .join("tasks")
        .join(format!("{}.md", task_id))
}

/// タスク情報を YAML frontmatter + Markdown ファイルとして書き出し
pub fn write_task_file(
    repos_base_dir: &str,
    task: &CodingTask,
    subtasks: &[Subtask],
) -> Result<PathBuf> {
    let path = task_file_path(repos_base_dir, task.id);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create task directory: {}", parent.display()))?;
    }

    let repo = task.repo_key.as_deref().unwrap_or("unknown");
    let analysis = task.analysis_text.as_deref().unwrap_or("");
    let progress = task.progress_percent.unwrap_or(0);
    let priority = task.priority_score.unwrap_or(0.0);
    let estimated: i32 = subtasks
        .iter()
        .filter_map(|s| s.estimated_minutes)
        .sum::<u32>() as i32;
    let estimated_display = if estimated > 0 || task.estimated_minutes.is_some() {
        let mins = task.estimated_minutes.unwrap_or(estimated);
        format!("{}min", mins)
    } else {
        "unknown".to_string()
    };

    let subtask_lines: Vec<String> = subtasks
        .iter()
        .map(|s| {
            let marker = match s.status.as_str() {
                "done" => "[x]",
                "in_progress" => "[~]",
                "blocked" => "[!]",
                _ => "[ ]",
            };
            let deps = if s.depends_on.is_empty() {
                String::new()
            } else {
                let dep_list: Vec<String> = s.depends_on.iter().map(|d| d.to_string()).collect();
                format!(" (depends: [{}])", dep_list.join(", "))
            };
            let est = s
                .estimated_minutes
                .map(|m| format!(" [est: {}min]", m))
                .unwrap_or_default();
            let blocked_mark = if s.status == "blocked" {
                " — blocked"
            } else {
                ""
            };
            format!(
                "{}. {} {}{}{}\n   {}{}",
                s.index, marker, s.title, deps, est, s.detail, blocked_mark
            )
        })
        .collect();

    let content = format!(
        "---\nid: {}\nrepo: {}\nstatus: {}\nprogress: {}%\npriority: {:.1}\nestimated: {}\n---\n\n# Task #{}: {}\n\n## 要件\n{}\n\n## サブタスク\n{}\n",
        task.id,
        repo,
        task.status,
        progress,
        priority,
        estimated_display,
        task.id,
        task.asana_task_name,
        analysis,
        subtask_lines.join("\n"),
    );

    std::fs::write(&path, &content)
        .with_context(|| format!("Failed to write task file: {}", path.display()))?;

    tracing::info!("Task file written: {}", path.display());
    Ok(path)
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
pub fn sync_tasks_cache(db: &Db, cache_path: &str) -> Result<()> {
    let tasks = db.get_active_tasks()?;
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

    fn make_test_task(id: i64) -> CodingTask {
        CodingTask {
            id,
            asana_task_gid: "12345".to_string(),
            asana_task_name: "テストタスク".to_string(),
            description: None,
            repo_key: Some("my-repo".to_string()),
            branch_name: None,
            status: "ready".to_string(),
            plan_text: None,
            analysis_text: Some("メールアドレス形式チェックを追加する".to_string()),
            subtasks_json: None,
            slack_channel: None,
            slack_thread_ts: None,
            slack_plan_ts: None,
            pr_url: None,
            error_message: None,
            retry_count: 0,
            summary: None,
            memory_note: None,
            priority_score: None,
            progress_percent: None,
            started_at_task: None,
            completed_at: None,
            estimated_minutes: None,
            actual_minutes: None,
            retrospective_note: None,
            complexity: None,
            claude_session_id: None,
            current_subtask_index: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn test_write_and_read_task_file() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().to_str().unwrap();
        let task = make_test_task(42);
        let subtasks = vec![
            Subtask { index: 1, title: "バリデーション関数作成".to_string(), detail: "src/validators.rs".to_string(), depends_on: vec![], estimated_minutes: Some(30), status: "pending".to_string(), started_at: None, completed_at: None, actual_minutes: None },
            Subtask { index: 2, title: "フォームに組み込み".to_string(), detail: "src/pages/Login.tsx".to_string(), depends_on: vec![1], estimated_minutes: Some(45), status: "pending".to_string(), started_at: None, completed_at: None, actual_minutes: None },
        ];

        let path = write_task_file(base, &task, &subtasks).unwrap();
        assert!(path.exists());

        let content = read_task_file(base, 42).unwrap();
        assert!(content.contains("Task #42"));
        assert!(content.contains("テストタスク"));
        assert!(content.contains("バリデーション関数作成"));
    }

    #[test]
    fn test_cleanup_done_tasks() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().to_str().unwrap();
        let task = make_test_task(1);
        write_task_file(base, &task, &[]).unwrap();

        let path = task_file_path(base, 1);
        assert!(path.exists());

        cleanup_done_tasks(base, &[1]).unwrap();
        assert!(!path.exists());
    }
}
