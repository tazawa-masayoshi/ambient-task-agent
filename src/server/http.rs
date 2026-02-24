use std::sync::Arc;

use anyhow::Result;
use axum::routing::{get, post};
use axum::Router;
use tokio::net::TcpListener;

use crate::db::Db;
use crate::repo_config::ReposConfig;

use super::{slack_webhook, webhook};

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
}

pub async fn run_server(state: AppState, port: u16) -> Result<()> {
    let shared = Arc::new(state);

    let app = Router::new()
        .route("/health", get(health))
        .route("/webhook/asana", post(webhook::handle_asana_webhook))
        .route("/webhook/slack", post(slack_webhook::handle_slack_webhook))
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
