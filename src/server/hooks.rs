use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::asana::client::AsanaClient;
use crate::config::{AsanaConfig, SlackConfig};
use crate::db::SessionRow;
use crate::session;
use crate::slack::client::SlackClient;

use super::http::AppState;

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct HookEventPayload {
    pub session_id: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub hook_event_name: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_input: Option<serde_json::Value>,
    #[serde(default)]
    pub transcript_path: Option<String>,
    // Notification イベント用
    #[serde(default)]
    pub notification_type: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct HookEventResponse {}

/// POST /hooks/event
pub async fn handle_hook_event(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<HookEventPayload>,
) -> Json<HookEventResponse> {
    let event_name = payload.hook_event_name.as_deref().unwrap_or("Unknown");
    let cwd = payload
        .cwd
        .as_deref()
        .unwrap_or("/tmp");

    tracing::info!(
        "Hook event: {} session={} cwd={}",
        event_name,
        payload.session_id,
        cwd
    );

    // 既存セッション取得（ステータス判定用）
    let current_status = state
        .db
        .get_session(&payload.session_id)
        .ok()
        .flatten()
        .map(|s| s.status)
        .unwrap_or_default();

    let new_status = session::determine_status(
        event_name,
        payload.notification_type.as_deref(),
        &current_status,
    );

    // stale セッション掃除
    if let Err(e) = state.db.cleanup_stale_sessions() {
        tracing::warn!("Failed to cleanup stale sessions: {}", e);
    }

    // セッション upsert
    let now = chrono::Utc::now().to_rfc3339();
    let session_row = SessionRow {
        session_id: payload.session_id.clone(),
        home_cwd: cwd.to_string(),
        tty: String::new(),
        status: new_status.clone(),
        active_task: None,
        tasks_completed: 0,
        tasks_total: 0,
        created_at: now,
        updated_at: String::new(), // DB が自動設定
    };

    if let Err(e) = state.db.upsert_session(&session_row) {
        tracing::error!("Failed to upsert session: {}", e);
    }

    // waiting_input → Slack 通知
    if new_status == "waiting_input" {
        let project_name = PathBuf::from(cwd)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();
        let session_suffix = if payload.session_id.len() > 6 {
            &payload.session_id[payload.session_id.len() - 6..]
        } else {
            &payload.session_id
        };
        let message = format!(
            ":bell: 許可待ち: {}\nセッション: {}",
            project_name, session_suffix
        );

        let slack_config = SlackConfig {
            bot_token: state.slack_bot_token.clone(),
            test_channel: state.slack_channel.clone(),
            signing_secret: None,
            workspace: None,
        };
        let slack = SlackClient::new(slack_config);
        let channel = state.slack_channel.clone();
        tokio::spawn(async move {
            if let Err(e) = slack.post_message(&channel, &message).await {
                tracing::warn!("Slack notification failed: {}", e);
            }
        });
    }

    // Stop → Asana コメント投稿
    if event_name == "Stop" {
        let cwd_owned = cwd.to_string();
        let asana_pat = state.asana_pat.clone();
        let asana_project_id = state.asana_project_id.clone();
        let asana_user_name = state.asana_user_name.clone();
        tokio::spawn(async move {
            post_stop_comment(&cwd_owned, &asana_pat, &asana_project_id, &asana_user_name).await;
        });
    }

    Json(HookEventResponse {})
}

async fn post_stop_comment(cwd: &str, asana_pat: &str, _project_id: &str, _user_name: &str) {
    let cwd_path = PathBuf::from(cwd);
    let task_file = cwd_path.join(".claude/current-task.json");

    if !task_file.exists() {
        return;
    }

    let content = match std::fs::read_to_string(&task_file) {
        Ok(c) => c,
        Err(_) => return,
    };

    let task: crate::hook::CurrentTask = match serde_json::from_str(&content) {
        Ok(t) => t,
        Err(_) => return,
    };

    let project_name = cwd_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    let comment = format!("Claude Code作業セッション終了\n📁 {}", project_name);

    let asana_config = AsanaConfig {
        pat: asana_pat.to_string(),
        project_id: String::new(),
        user_name: String::new(),
    };
    let client = AsanaClient::new(asana_config);
    if let Err(e) = client.post_comment(&task.gid, &comment).await {
        tracing::warn!("Asana comment failed: {}", e);
    }
}
