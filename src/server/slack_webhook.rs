use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use super::http::AppState;
use super::slack_events;

type HmacSha256 = Hmac<Sha256>;

/// Slack Events API エンドポイント
///
/// 1. x-slack-retry-num ヘッダがある場合は即 200 返却（リトライスキップ）
/// 2. 署名検証 (SLACK_SIGNING_SECRET)
/// 3. url_verification → challenge 返却
/// 4. event_callback → tokio::spawn で非同期処理、即 200 OK
pub async fn handle_slack_webhook(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // リトライスキップ: Slack の3秒タイムアウト対策
    if headers.get("x-slack-retry-num").is_some() {
        tracing::debug!("Skipping Slack retry");
        return (StatusCode::OK, "ok").into_response();
    }

    // 署名検証
    if let Some(ref signing_secret) = state.slack_signing_secret {
        let timestamp = headers
            .get("x-slack-request-timestamp")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        let signature = headers
            .get("x-slack-signature")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();

        if !verify_slack_signature(signing_secret, timestamp, &body, signature) {
            tracing::warn!("Invalid Slack signature");
            return (StatusCode::UNAUTHORIZED, "invalid signature").into_response();
        }
    }

    // JSON パース
    let payload: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("Failed to parse Slack webhook payload: {}", e);
            return (StatusCode::BAD_REQUEST, "invalid json").into_response();
        }
    };

    let event_type = payload
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or_default();

    match event_type {
        // Slack App 登録時の URL 検証チャレンジ
        "url_verification" => {
            let challenge = payload
                .get("challenge")
                .and_then(|c| c.as_str())
                .unwrap_or_default()
                .to_string();
            tracing::info!("Slack URL verification challenge received");
            (
                StatusCode::OK,
                [("content-type", "text/plain")],
                challenge,
            )
                .into_response()
        }

        // イベントコールバック: 非同期処理して即 200 返却
        "event_callback" => {
            let event = payload.get("event").cloned();
            if let Some(event) = event {
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = slack_events::dispatch_event(&state, &event).await {
                        tracing::error!("Slack event processing failed: {}", e);
                    }
                });
            }
            (StatusCode::OK, "ok").into_response()
        }

        _ => {
            tracing::debug!("Unknown Slack event type: {}", event_type);
            (StatusCode::OK, "ok").into_response()
        }
    }
}

/// Slack の署名を検証
/// sig_basestring = "v0:{timestamp}:{body}"
/// expected = "v0=" + hmac_sha256(secret, sig_basestring)
fn verify_slack_signature(secret: &str, timestamp: &str, body: &[u8], expected: &str) -> bool {
    // タイムスタンプが5分以上古い場合は拒否
    if let Ok(ts) = timestamp.parse::<i64>() {
        let now = chrono::Utc::now().timestamp();
        if (now - ts).abs() > 300 {
            tracing::warn!("Slack request timestamp too old: {} (now: {})", ts, now);
            return false;
        }
    }

    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
        return false;
    };

    let sig_basestring = format!("v0:{}:", timestamp);
    mac.update(sig_basestring.as_bytes());
    mac.update(body);

    let computed = format!("v0={}", hex::encode(mac.finalize().into_bytes()));
    computed == expected
}
