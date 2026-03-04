use std::sync::Arc;

use anyhow::Result;
use axum::routing::{get, post};
use axum::Router;
use tokio::net::TcpListener;

use crate::db::Db;
use crate::repo_config::ReposConfig;

use super::{api, hooks, slack_actions, slack_webhook, webhook};

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub repos_config: ReposConfig,
    pub asana_webhook_secret: Option<String>,
    pub slack_bot_token: String,
    pub slack_channel: String,
    pub slack_signing_secret: Option<String>,
    pub asana_pat: String,
    pub asana_project_id: String,
    pub asana_user_name: String,
    pub slack_workspace: Option<String>,
    pub worker_notify: std::sync::Arc<tokio::sync::Notify>,
    pub runner_ctx: crate::execution::RunnerContext,
}

impl AppState {
    /// ワーカーを即時起床させる
    pub fn wake_worker(&self) {
        self.worker_notify.notify_one();
    }

    pub fn slack_client(&self) -> crate::slack::client::SlackClient {
        let config = crate::config::SlackConfig {
            bot_token: self.slack_bot_token.clone(),
            test_channel: self.slack_channel.clone(),
            signing_secret: self.slack_signing_secret.clone(),
            workspace: self.slack_workspace.clone(),
        };
        crate::slack::client::SlackClient::new(config)
    }
}

pub async fn run_server(state: AppState, port: u16) -> Result<()> {
    let shared = Arc::new(state);

    let app = Router::new()
        .route("/health", get(health))
        .route("/hooks/event", post(hooks::handle_hook_event))
        .route("/api/sessions", get(api::list_sessions))
        .route("/api/tasks", get(api::list_tasks))
        .route("/api/tasks/next", get(api::next_task))
        .route("/api/tasks/summary", get(api::tasks_summary))
        .route("/api/tasks/validate", get(api::validate_tasks))
        .route("/api/tasks/{id}/progress", get(api::task_progress))
        .route("/webhook/asana", post(webhook::handle_asana_webhook))
        .route("/webhook/slack", post(slack_webhook::handle_slack_webhook))
        .route("/slack/actions", post(slack_actions::handle_slack_action))
        .with_state(shared);

    let addr = format!("0.0.0.0:{}", port);
    tracing::info!("Starting server on {}", addr);

    let listener = TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health() -> &'static str {
    "ok"
}
