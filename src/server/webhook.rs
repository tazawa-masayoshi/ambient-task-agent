use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use super::http::AppState;

type HmacSha256 = Hmac<Sha256>;

/// Asana Webhook エンドポイント
///
/// 初回: X-Hook-Secret ヘッダーを受け取り、同じ値を返してハンドシェイク完了
/// 以降: HMAC-SHA256 署名検証 + イベント処理
pub async fn handle_asana_webhook(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // ハンドシェイク: 初回登録時
    if let Some(hook_secret) = headers.get("x-hook-secret") {
        let secret = hook_secret.to_str().unwrap_or_default().to_string();
        tracing::info!("Asana webhook handshake received");
        return (StatusCode::OK, [("x-hook-secret", secret)], String::new()).into_response();
    }

    // 署名検証
    if let Some(ref webhook_secret) = state.asana_webhook_secret {
        let signature = headers
            .get("x-hook-signature")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();

        if !verify_signature(webhook_secret, &body, signature) {
            tracing::warn!("Invalid webhook signature");
            return (StatusCode::UNAUTHORIZED, "invalid signature").into_response();
        }
    }

    // イベント処理
    let payload: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("Failed to parse webhook payload: {}", e);
            return (StatusCode::BAD_REQUEST, "invalid json").into_response();
        }
    };

    let events = match payload.get("events").and_then(|e| e.as_array()) {
        Some(events) => events.clone(),
        None => {
            tracing::debug!("No events in payload");
            return (StatusCode::OK, "no events").into_response();
        }
    };

    for event in &events {
        if let Err(e) = process_event(&state, event).await {
            tracing::error!("Failed to process event: {}", e);
        }
    }

    // Webhook 受信をトリガーに tasks-cache.json を更新
    tokio::spawn({
        let state = state.clone();
        async move {
            if let Err(e) = sync_tasks_cache(&state).await {
                tracing::error!("Failed to sync tasks cache: {}", e);
            }
        }
    });

    (StatusCode::OK, "ok").into_response()
}

async fn process_event(state: &AppState, event: &serde_json::Value) -> anyhow::Result<()> {
    let action = event
        .get("action")
        .and_then(|a| a.as_str())
        .unwrap_or_default();
    let resource_type = event
        .get("resource")
        .and_then(|r| r.get("resource_type"))
        .and_then(|t| t.as_str())
        .unwrap_or_default();
    let resource_gid = event
        .get("resource")
        .and_then(|r| r.get("gid"))
        .and_then(|g| g.as_str())
        .unwrap_or_default();

    tracing::info!(
        "Webhook event: action={}, type={}, gid={}",
        action,
        resource_type,
        resource_gid
    );

    // イベントをDBに記録
    state.db.insert_webhook_event(
        action,
        resource_gid,
        &serde_json::to_string(event).unwrap_or_default(),
    )?;

    // タスク変更イベントのみ処理
    if resource_type != "task" {
        return Ok(());
    }

    // added / changed イベントでタスクをキューイング
    match action {
        "added" | "changed" => {
            enqueue_task(state, resource_gid).await?;
        }
        _ => {
            tracing::debug!("Ignoring action: {}", action);
        }
    }

    Ok(())
}

async fn enqueue_task(state: &AppState, task_gid: &str) -> anyhow::Result<()> {
    // 重複チェック
    if state.db.task_exists_for_gid(task_gid)? {
        tracing::info!("Task {} already queued, skipping", task_gid);
        return Ok(());
    }

    // Asana API でタスク詳細を取得
    let asana_config = crate::config::AsanaConfig {
        pat: state.asana_pat.clone(),
        project_id: String::new(),
        user_name: String::new(),
    };
    let client = crate::asana::client::AsanaClient::new(asana_config);
    let task = client.get_task(task_gid).await?;

    // タスク名が空なら無視（セクション見出し等）
    if task.name.trim().is_empty() {
        return Ok(());
    }

    // プロジェクトGIDからリポジトリを特定
    let project_gid = task
        .memberships
        .as_ref()
        .and_then(|m| m.first())
        .and_then(|m| m.project.as_ref())
        .map(|p| p.gid.as_str());

    let repo_key = project_gid.and_then(|gid| {
        state
            .repos_config
            .find_repo_by_project(gid)
            .map(|r| r.key.as_str())
    });

    let id = state.db.insert_task(
        task_gid,
        &task.name,
        task.notes.as_deref(),
        repo_key,
        Some(&state.slack_channel),
    )?;
    state.wake_worker();

    tracing::info!(
        "Queued task: id={}, gid={}, name={}, repo={:?}",
        id,
        task_gid,
        task.name,
        repo_key
    );

    Ok(())
}

/// Asana からタスクを取得して tasks-cache.json を更新（wez-sidebar 向け）
async fn sync_tasks_cache(state: &AppState) -> anyhow::Result<()> {
    let config = crate::config::AsanaConfig {
        pat: state.asana_pat.clone(),
        project_id: state.asana_project_id.clone(),
        user_name: state.asana_user_name.clone(),
    };
    let result = crate::sync::run_sync(&config).await?;
    if result.changed {
        tracing::info!("Tasks cache updated ({} changes)", result.diff.len());
    }
    Ok(())
}

fn verify_signature(secret: &str, body: &[u8], expected: &str) -> bool {
    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    let result = hex::encode(mac.finalize().into_bytes());
    result == expected
}
