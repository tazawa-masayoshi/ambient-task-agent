use std::sync::Arc;

use axum::extract::{Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

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
