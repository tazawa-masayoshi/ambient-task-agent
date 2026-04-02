use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

use super::http::AppState;
use super::slack_events;

/// Socket Mode で Slack に接続してイベントを受信する
pub async fn run_socket_mode(state: Arc<AppState>, app_token: String) {
    const MAX_RETRIES: u32 = 30;
    const MAX_BACKOFF_SECS: u64 = 60;

    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("failed to build reqwest client");

    let mut consecutive_failures: u32 = 0;

    loop {
        match connect_and_listen(&state, &app_token, &http_client).await {
            Ok(established) => {
                if established {
                    // hello 受信後の正常切断 → カウンタリセット、最低1秒待って再接続
                    consecutive_failures = 0;
                    tracing::info!("Socket Mode connection closed, reconnecting in 1s...");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                } else {
                    // hello 前の即 disconnect → バックオフ（古い接続のローテーション）
                    consecutive_failures += 1;
                    let backoff = std::cmp::min(
                        1u64 << consecutive_failures.min(5),
                        MAX_BACKOFF_SECS,
                    );
                    tracing::info!(
                        "Socket Mode: disconnected before handshake ({}/{}), retrying in {}s...",
                        consecutive_failures, MAX_RETRIES, backoff
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
                }
            }
            Err(e) => {
                consecutive_failures += 1;
                if consecutive_failures >= MAX_RETRIES {
                    tracing::error!(
                        "Socket Mode: {} consecutive failures, giving up. Last error: {}",
                        consecutive_failures, e
                    );
                    return;
                }
                let backoff = std::cmp::min(
                    1u64 << (consecutive_failures - 1),
                    MAX_BACKOFF_SECS,
                );
                tracing::error!(
                    "Socket Mode error ({}/{}): {}, retrying in {}s...",
                    consecutive_failures, MAX_RETRIES, e, backoff
                );
                tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
            }
        }
    }
}

/// apps.connections.open を呼んで WebSocket URL を取得
async fn get_ws_url(client: &reqwest::Client, app_token: &str) -> anyhow::Result<String> {
    let resp = client
        .post("https://slack.com/api/apps.connections.open")
        .header("Authorization", format!("Bearer {}", app_token))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send()
        .await?;

    let body: serde_json::Value = resp.json().await?;
    if body.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        let err = body.get("error").and_then(|e| e.as_str()).unwrap_or("unknown");
        anyhow::bail!("apps.connections.open failed: {}", err);
    }

    body.get("url")
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("No URL in apps.connections.open response"))
}

/// WebSocket に接続してイベントを処理。
/// 戻り値: Ok(true) = hello 受信後の正常切断, Ok(false) = hello 前の disconnect
async fn connect_and_listen(state: &Arc<AppState>, app_token: &str, http_client: &reqwest::Client) -> anyhow::Result<bool> {
    // Slack は通常10秒間隔で ping を送るので、60秒無通信なら接続が死んでいると判断
    const READ_TIMEOUT_SECS: u64 = 60;

    let ws_url = get_ws_url(http_client, app_token).await?;
    tracing::info!("Socket Mode connecting to WebSocket...");

    let (ws_stream, _) = tokio_tungstenite::connect_async(&ws_url).await?;
    tracing::info!("Socket Mode connected");

    let (mut write, mut read) = ws_stream.split();
    let mut established = false;

    loop {
        let msg = tokio::time::timeout(
            std::time::Duration::from_secs(READ_TIMEOUT_SECS),
            read.next(),
        ).await;

        let msg = match msg {
            Ok(Some(Ok(m))) => m,
            Ok(Some(Err(e))) => {
                tracing::error!("WebSocket read error: {}", e);
                break;
            }
            Ok(None) => {
                // ストリーム終了
                break;
            }
            Err(_) => {
                tracing::warn!("Socket Mode: no message for {}s, reconnecting...", READ_TIMEOUT_SECS);
                break;
            }
        };

        match msg {
            Message::Text(text) => {
                let payload: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("Failed to parse Socket Mode message: {}", e);
                        continue;
                    }
                };

                // envelope_id で ack 応答（必須）
                if let Some(envelope_id) = payload.get("envelope_id").and_then(|v| v.as_str()) {
                    let ack = serde_json::json!({ "envelope_id": envelope_id });
                    if let Err(e) = write.send(Message::Text(ack.to_string().into())).await {
                        tracing::error!("Failed to send ack: {}", e);
                        break;
                    }
                }

                let msg_type = payload.get("type").and_then(|t| t.as_str()).unwrap_or("");
                tracing::debug!("Socket Mode received type={}", msg_type);

                match msg_type {
                    "events_api" => {
                        handle_events_api(state, &payload).await;
                    }
                    "interactive" => {
                        handle_interactive(state, &payload).await;
                    }
                    "disconnect" => {
                        tracing::info!("Socket Mode received disconnect (established={}), will reconnect", established);
                        break;
                    }
                    "hello" => {
                        established = true;
                        tracing::info!("Socket Mode handshake complete");
                    }
                    _ => {
                        tracing::debug!("Socket Mode unknown type: {}", msg_type);
                    }
                }
            }
            Message::Ping(data) => {
                if let Err(e) = write.send(Message::Pong(data)).await {
                    tracing::error!("Failed to send pong: {}", e);
                    break;
                }
            }
            Message::Close(_) => {
                tracing::info!("Socket Mode WebSocket closed by server");
                break;
            }
            _ => {}
        }
    }

    Ok(established)
}

/// events_api タイプのペイロードを処理
async fn handle_events_api(state: &Arc<AppState>, payload: &serde_json::Value) {
    // Socket Mode: payload.payload.event にイベント本体がある
    let event = payload
        .get("payload")
        .and_then(|p| p.get("event"));

    let event_type = event
        .and_then(|e| e.get("type"))
        .and_then(|t| t.as_str())
        .unwrap_or("none");
    tracing::debug!("Socket Mode event_type={}", event_type);

    if let Some(event) = event {
        let state = state.clone();
        let event = event.clone();
        tokio::spawn(async move {
            if let Err(e) = slack_events::dispatch_event(&state, &event).await {
                tracing::error!("Socket Mode event processing failed: {}", e);
            }
        });
    }
}

/// interactive タイプ（Block Kit ボタン等）を処理
async fn handle_interactive(state: &Arc<AppState>, payload: &serde_json::Value) {
    let inner = match payload.get("payload") {
        Some(p) => p,
        None => return,
    };

    // Slack actions と同じ処理に委譲
    let state = state.clone();
    let inner = inner.clone();
    tokio::spawn(async move {
        if let Err(e) = super::slack_actions::dispatch_action(&state, &inner).await {
            tracing::error!("Socket Mode interactive processing failed: {}", e);
        }
    });
}
