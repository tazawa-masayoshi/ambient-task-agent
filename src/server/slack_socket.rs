use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

use super::http::AppState;
use super::slack_events;

/// Socket Mode で Slack に接続してイベントを受信する
pub async fn run_socket_mode(state: Arc<AppState>, app_token: String) {
    loop {
        match connect_and_listen(&state, &app_token).await {
            Ok(()) => {
                tracing::info!("Socket Mode connection closed, reconnecting...");
            }
            Err(e) => {
                tracing::error!("Socket Mode error: {}, reconnecting in 5s...", e);
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    }
}

/// apps.connections.open を呼んで WebSocket URL を取得
async fn get_ws_url(app_token: &str) -> anyhow::Result<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
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

/// WebSocket に接続してイベントを処理
async fn connect_and_listen(state: &Arc<AppState>, app_token: &str) -> anyhow::Result<()> {
    let ws_url = get_ws_url(app_token).await?;
    tracing::info!("Socket Mode connecting to WebSocket...");

    let (ws_stream, _) = tokio_tungstenite::connect_async(&ws_url).await?;
    tracing::info!("Socket Mode connected");

    let (mut write, mut read) = ws_stream.split();

    while let Some(msg) = read.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                tracing::error!("WebSocket read error: {}", e);
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

                match msg_type {
                    "events_api" => {
                        handle_events_api(state, &payload).await;
                    }
                    "interactive" => {
                        handle_interactive(state, &payload).await;
                    }
                    "disconnect" => {
                        tracing::info!("Socket Mode received disconnect, will reconnect");
                        break;
                    }
                    "hello" => {
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

    Ok(())
}

/// events_api タイプのペイロードを処理
async fn handle_events_api(state: &Arc<AppState>, payload: &serde_json::Value) {
    // Socket Mode: payload.payload.event にイベント本体がある
    let event = payload
        .get("payload")
        .and_then(|p| p.get("event"));

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
