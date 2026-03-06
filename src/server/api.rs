use std::collections::HashSet;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::worker::decomposer::{self, Subtask};
use crate::worker::task_file;

use super::http::AppState;

#[derive(Debug, Deserialize)]
pub struct TasksQuery {
    pub status: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TasksSummaryResponse {
    pub counts: Vec<StatusCount>,
    pub total: i64,
}

#[derive(Debug, Serialize)]
pub struct StatusCount {
    pub status: String,
    pub count: i64,
}

/// GET /api/sessions
pub async fn list_sessions(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    match state.db.list_active_sessions() {
        Ok(sessions) => Json(serde_json::json!({ "sessions": sessions })),
        Err(e) => {
            tracing::error!("Failed to list sessions: {}", e);
            Json(serde_json::json!({ "error": e.to_string() }))
        }
    }
}

/// GET /api/tasks?status=pending
pub async fn list_tasks(
    State(state): State<Arc<AppState>>,
    Query(query): Query<TasksQuery>,
) -> Json<serde_json::Value> {
    match state.db.list_tasks(query.status.as_deref()) {
        Ok(tasks) => Json(serde_json::json!({ "tasks": tasks })),
        Err(e) => {
            tracing::error!("Failed to list tasks: {}", e);
            Json(serde_json::json!({ "error": e.to_string() }))
        }
    }
}

/// GET /api/tasks/next — 次に着手すべきサブタスクを返す
pub async fn next_task(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let tasks = match state.db.get_tasks_by_priority() {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("Failed to get tasks by priority: {}", e);
            return Json(serde_json::json!({ "error": e.to_string() }));
        }
    };

    for task in &tasks {
        if let Some(ref json) = task.subtasks_json {
            if let Ok(subtasks) = serde_json::from_str::<Vec<Subtask>>(json) {
                let actionable = decomposer::get_actionable_subtasks(&subtasks);
                if let Some(next) = actionable.first() {
                    let reason = if next.depends_on.is_empty() {
                        "依存なし — 即着手可能".to_string()
                    } else {
                        let deps: Vec<String> = next.depends_on.iter().map(|d| format!("#{}", d)).collect();
                        format!("前提タスク {} 完了済み", deps.join(", "))
                    };
                    return Json(serde_json::json!({
                        "task_id": task.id,
                        "task_name": task.asana_task_name,
                        "subtask": {
                            "index": next.index,
                            "title": next.title,
                            "detail": next.detail,
                            "estimated_minutes": next.estimated_minutes,
                        },
                        "reason": reason,
                        "priority_score": task.priority_score.unwrap_or(0.0),
                        "progress_percent": task.progress_percent.unwrap_or(0),
                    }));
                }
            }
        }
    }

    Json(serde_json::json!({ "message": "着手可能なサブタスクがありません" }))
}

/// GET /api/tasks/:id/progress — サブタスク単位の進捗詳細
pub async fn task_progress(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Json<serde_json::Value> {
    let task = match state.db.get_task_by_id(id) {
        Ok(Some(t)) => t,
        Ok(None) => return Json(serde_json::json!({ "error": format!("Task {} not found", id) })),
        Err(e) => return Json(serde_json::json!({ "error": e.to_string() })),
    };

    let (subtasks_summary, subtasks_detail) = if let Some(ref json) = task.subtasks_json {
        match serde_json::from_str::<Vec<Subtask>>(json) {
            Ok(subtasks) => {
                let total = subtasks.len();
                let done = subtasks.iter().filter(|s| s.status == "done").count();
                let in_progress = subtasks.iter().filter(|s| s.status == "in_progress").count();
                let blocked = subtasks.iter().filter(|s| s.status == "blocked").count();
                let pending = subtasks.iter().filter(|s| s.status == "pending").count();
                (
                    serde_json::json!({ "total": total, "done": done, "in_progress": in_progress, "blocked": blocked, "pending": pending }),
                    serde_json::json!(subtasks),
                )
            }
            Err(_) => (serde_json::json!({}), serde_json::json!([])),
        }
    } else {
        (serde_json::json!({}), serde_json::json!([]))
    };

    Json(serde_json::json!({
        "task_id": task.id,
        "progress_percent": task.progress_percent.unwrap_or(0),
        "subtasks_summary": subtasks_summary,
        "subtasks": subtasks_detail,
    }))
}

/// GET /api/tasks/validate — 整合性チェック
pub async fn validate_tasks(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let tasks = match state.db.get_active_tasks() {
        Ok(t) => t,
        Err(e) => return Json(serde_json::json!({ "error": e.to_string() })),
    };

    let mut issues: Vec<serde_json::Value> = Vec::new();

    for task in &tasks {
        if let Some(ref json) = task.subtasks_json {
            if let Ok(subtasks) = serde_json::from_str::<Vec<Subtask>>(json) {
                // ステータス矛盾: 全サブタスク done なのに task≠done
                let all_done = !subtasks.is_empty() && subtasks.iter().all(|s| s.status == "done");
                if all_done && task.status != "done" {
                    issues.push(serde_json::json!({
                        "task_id": task.id,
                        "type": "status_mismatch",
                        "message": format!("全サブタスク完了だがタスクステータスは '{}'", task.status),
                    }));
                }

                // 不正 depends_on: 存在しない index
                let valid_indices: HashSet<u32> = subtasks.iter().map(|s| s.index).collect();
                for s in &subtasks {
                    for dep in &s.depends_on {
                        if !valid_indices.contains(dep) {
                            issues.push(serde_json::json!({
                                "task_id": task.id,
                                "type": "invalid_dependency",
                                "message": format!("サブタスク #{} の depends_on #{} は存在しない", s.index, dep),
                            }));
                        }
                    }
                }

                // 循環依存チェック (DFS)
                if let Some(cycle) = detect_cycle(&subtasks) {
                    issues.push(serde_json::json!({
                        "task_id": task.id,
                        "type": "circular_dependency",
                        "message": format!("循環依存を検出: {:?}", cycle),
                    }));
                }
            }
        } else {
            // 孤立タスク: ready/in_progress でサブタスクなし
            if matches!(task.status.as_str(), "ready" | "in_progress") {
                issues.push(serde_json::json!({
                    "task_id": task.id,
                    "type": "orphan_task",
                    "message": format!("ステータス '{}' だがサブタスクが未定義", task.status),
                }));
            }
        }
    }

    Json(serde_json::json!({
        "valid": issues.is_empty(),
        "issues_count": issues.len(),
        "issues": issues,
    }))
}

/// 循環依存を DFS で検出
fn detect_cycle(subtasks: &[Subtask]) -> Option<Vec<u32>> {
    use std::collections::HashMap;
    let deps: HashMap<u32, &Vec<u32>> = subtasks.iter().map(|s| (s.index, &s.depends_on)).collect();
    let mut visited: HashSet<u32> = HashSet::new();
    let mut rec_stack: HashSet<u32> = HashSet::new();

    for s in subtasks {
        if !visited.contains(&s.index) {
            let mut path = Vec::new();
            if dfs_cycle(s.index, &deps, &mut visited, &mut rec_stack, &mut path) {
                return Some(path);
            }
        }
    }
    None
}

fn dfs_cycle(
    node: u32,
    deps: &std::collections::HashMap<u32, &Vec<u32>>,
    visited: &mut HashSet<u32>,
    rec_stack: &mut HashSet<u32>,
    path: &mut Vec<u32>,
) -> bool {
    visited.insert(node);
    rec_stack.insert(node);
    path.push(node);

    if let Some(neighbors) = deps.get(&node) {
        for &n in *neighbors {
            if !visited.contains(&n) {
                if dfs_cycle(n, deps, visited, rec_stack, path) {
                    return true;
                }
            } else if rec_stack.contains(&n) {
                path.push(n);
                return true;
            }
        }
    }

    rec_stack.remove(&node);
    path.pop();
    false
}

/// GET /api/tasks/cache — wez-sidebar 互換の軽量タスク一覧
pub async fn tasks_cache(
    State(state): State<Arc<AppState>>,
) -> Json<task_file::WezTasksFile> {
    match state.db.get_active_tasks() {
        Ok(tasks) => Json(task_file::to_wez_tasks_file(&tasks)),
        Err(e) => {
            tracing::error!("Failed to get tasks for cache: {}", e);
            Json(task_file::WezTasksFile { tasks: vec![] })
        }
    }
}

/// GET /api/tasks/summary
pub async fn tasks_summary(
    State(state): State<Arc<AppState>>,
) -> Json<TasksSummaryResponse> {
    match state.db.count_tasks_by_status() {
        Ok(counts) => {
            let total: i64 = counts.iter().map(|(_, c)| c).sum();
            let status_counts = counts
                .into_iter()
                .map(|(status, count)| StatusCount { status, count })
                .collect();
            Json(TasksSummaryResponse {
                counts: status_counts,
                total,
            })
        }
        Err(e) => {
            tracing::error!("Failed to count tasks: {}", e);
            Json(TasksSummaryResponse {
                counts: vec![],
                total: 0,
            })
        }
    }
}
