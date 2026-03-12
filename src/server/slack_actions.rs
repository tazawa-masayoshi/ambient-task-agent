use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use super::http::AppState;
use super::slack_webhook::verify_slack_signature;

#[derive(Debug, Deserialize)]
struct SlackActionPayload {
    actions: Vec<SlackAction>,
    channel: Option<ChannelInfo>,
    message: Option<MessageInfo>,
}

#[derive(Debug, Deserialize)]
struct SlackAction {
    action_id: String,
    value: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChannelInfo {
    id: String,
}

#[derive(Debug, Deserialize)]
struct MessageInfo {
    ts: String,
    #[serde(default)]
    thread_ts: Option<String>,
}

/// POST /slack/actions — Slack Block Kit interactivity endpoint
pub async fn handle_slack_action(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // 署名検証（/webhook/slack と同じロジック）
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
            tracing::warn!("Invalid Slack signature on /slack/actions");
            return (StatusCode::UNAUTHORIZED, "invalid signature").into_response();
        }
    }

    // application/x-www-form-urlencoded の payload フィールドを抽出
    let payload_json = match extract_payload_field(&body) {
        Some(p) => p,
        None => {
            tracing::error!("No payload field in Slack action request");
            return (StatusCode::BAD_REQUEST, "missing payload").into_response();
        }
    };

    let payload: SlackActionPayload = match serde_json::from_str(&payload_json) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("Failed to parse Slack action payload: {}", e);
            return (StatusCode::BAD_REQUEST, "invalid payload").into_response();
        }
    };

    let action = match payload.actions.first() {
        Some(a) => a,
        None => return (StatusCode::OK, "no action").into_response(),
    };

    let action_value = action.value.as_deref().unwrap_or("").to_string();
    let action_id = action.action_id.clone();

    let channel_id = payload
        .channel
        .as_ref()
        .map(|c| c.id.as_str())
        .unwrap_or(&state.slack_channel)
        .to_string();

    let message_ts = payload.message.as_ref().map(|m| m.ts.clone());
    let thread_ts = payload
        .message
        .as_ref()
        .and_then(|m| m.thread_ts.clone());

    // 非同期で処理
    let state_clone = state.clone();

    tokio::spawn(async move {
        if let Err(e) = process_action(
            &state_clone,
            &action_id,
            &action_value,
            &channel_id,
            message_ts.as_deref(),
            thread_ts.as_deref(),
        )
        .await
        {
            tracing::error!("Failed to process Slack action: {}", e);
        }
    });

    // 即座に 200 OK を返す（Slack の3秒タイムアウト対策）
    (StatusCode::OK, "").into_response()
}

/// `application/x-www-form-urlencoded` のボディから `payload` フィールドを取得
fn extract_payload_field(body: &[u8]) -> Option<String> {
    let body_str = std::str::from_utf8(body).ok()?;
    for part in body_str.split('&') {
        if let Some(value) = part.strip_prefix("payload=") {
            return urlencoding::decode(value).ok().map(|s| s.into_owned());
        }
    }
    None
}

/// Socket Mode から呼ばれる interactive ペイロードの処理
pub async fn dispatch_action(state: &AppState, payload: &serde_json::Value) -> anyhow::Result<()> {
    let action_payload: SlackActionPayload = serde_json::from_value(payload.clone())?;

    let action = match action_payload.actions.first() {
        Some(a) => a,
        None => return Ok(()),
    };

    let action_value = action.value.as_deref().unwrap_or("");

    let channel = action_payload
        .channel
        .as_ref()
        .map(|c| c.id.as_str())
        .unwrap_or(&state.slack_channel);
    let message_ts = action_payload.message.as_ref().map(|m| m.ts.as_str());
    let thread_ts = action_payload
        .message
        .as_ref()
        .and_then(|m| m.thread_ts.as_deref());

    process_action(
        state,
        &action.action_id,
        action_value,
        channel,
        message_ts,
        thread_ts,
    )
    .await
}

async fn process_action(
    state: &AppState,
    action_id: &str,
    action_value: &str,
    channel: &str,
    message_ts: Option<&str>,
    thread_ts: Option<&str>,
) -> anyhow::Result<()> {
    // ops 系アクションは task_id ではなく ops_queue の id を使う
    if action_id == "ops_resolve" {
        return process_ops_resolve(state, action_value, channel, message_ts).await;
    }
    if action_id == "ops_escalate" {
        return process_ops_escalate(state, action_value, channel, message_ts, thread_ts).await;
    }

    let task_id: i64 = action_value
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid task_id: {}", action_value))?;

    let task = match state.db.get_task_by_id(task_id)? {
        Some(t) => t,
        None => {
            tracing::warn!("Task {} not found for action {}", task_id, action_id);
            return Ok(());
        }
    };

    let slack = state.slack_client();
    let reply_ts = thread_ts
        .or(task.slack_thread_ts.as_deref())
        .unwrap_or("");

    match action_id {
        "approve_task" => {
            if task.status != "proposed" {
                tracing::debug!(
                    "Task {} is not in proposed status ({}), ignoring approve",
                    task.id,
                    task.status
                );
                return Ok(());
            }
            state.db.update_status(task.id, "approved")?;

            // ボタンを無効化（メッセージ更新）
            if let Some(msg_ts) = message_ts {
                let updated_blocks = serde_json::json!([
                    {
                        "type": "section",
                        "text": {
                            "type": "mrkdwn",
                            "text": ":white_check_mark: *承認されました*"
                        }
                    }
                ]);
                slack
                    .update_blocks(channel, msg_ts, &updated_blocks, "承認されました")
                    .await
                    .ok();
            }

            slack
                .reply_thread(
                    channel,
                    reply_ts,
                    ":white_check_mark: 要件定義が承認されました！タスクを分解します...",
                )
                .await?;
            tracing::info!("Task {} approved via Block Kit button", task.id);
            state.wake_worker();
        }

        "reject_task" => {
            if task.status != "proposed" {
                return Ok(());
            }
            state.db.update_status(task.id, "rejected")?;

            if let Some(msg_ts) = message_ts {
                let updated_blocks = serde_json::json!([
                    {
                        "type": "section",
                        "text": {
                            "type": "mrkdwn",
                            "text": ":x: *却下されました*"
                        }
                    }
                ]);
                slack
                    .update_blocks(channel, msg_ts, &updated_blocks, "却下されました")
                    .await
                    .ok();
            }

            slack
                .reply_thread(
                    channel,
                    reply_ts,
                    ":x: 要件定義が却下されました。スレッドでフィードバックを送信してください。",
                )
                .await?;
            tracing::info!("Task {} rejected via Block Kit button", task.id);
        }

        "regenerate_task" => {
            if task.status != "proposed" {
                return Ok(());
            }
            state.db.reset_for_regeneration(task.id)?;

            if let Some(msg_ts) = message_ts {
                let updated_blocks = serde_json::json!([
                    {
                        "type": "section",
                        "text": {
                            "type": "mrkdwn",
                            "text": ":arrows_counterclockwise: *再生成中...*"
                        }
                    }
                ]);
                slack
                    .update_blocks(channel, msg_ts, &updated_blocks, "再生成中...")
                    .await
                    .ok();
            }

            slack
                .reply_thread(
                    channel,
                    reply_ts,
                    ":arrows_counterclockwise: 要件定義を再生成します...",
                )
                .await?;
            tracing::info!("Task {} queued for regeneration via Block Kit button", task.id);
            state.wake_worker();
        }

        "stop_task" => {
            match task.status.as_str() {
                "executing" | "ci_pending" | "planning" => {
                    let prev_status = task.status.clone();
                    state.db.set_error(task.id, &format!("Cancelled by user (was {})", prev_status))?;

                    // ボタンを無効化
                    if let Some(msg_ts) = message_ts {
                        let updated_blocks = serde_json::json!([
                            {
                                "type": "section",
                                "text": {
                                    "type": "mrkdwn",
                                    "text": ":octagonal_sign: *中止されました*"
                                }
                            }
                        ]);
                        slack
                            .update_blocks(channel, msg_ts, &updated_blocks, "中止されました")
                            .await
                            .ok();
                    }

                    slack
                        .reply_thread(
                            channel,
                            reply_ts,
                            &format!(
                                ":octagonal_sign: タスクを中止しました（`{}` → `error`）\n\
                                 実行中のプロセスは次のターン終了時に停止します",
                                prev_status
                            ),
                        )
                        .await?;
                    tracing::info!("Task {} stopped via Block Kit button (was {})", task.id, prev_status);
                }
                _ => {
                    slack
                        .reply_thread(
                            channel,
                            reply_ts,
                            &format!(":no_entry: 現在のステータスは `{}` のため中止できません", task.status),
                        )
                        .await
                        .ok();
                }
            }
        }

        _ => {
            tracing::debug!("Unknown action_id: {}", action_id);
        }
    }

    Ok(())
}

/// ops_escalate ボタンの処理: ops アイテムを coding_task に昇格
async fn process_ops_escalate(
    state: &AppState,
    action_value: &str,
    channel: &str,
    message_ts: Option<&str>,
    thread_ts: Option<&str>,
) -> anyhow::Result<()> {
    let ops_id: i64 = action_value
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid ops_id: {}", action_value))?;

    // ops アイテムの情報を取得
    let item = state.db.get_ops_item(ops_id)?;
    let item = match item {
        Some(i) => i,
        None => {
            tracing::warn!("ops item {} not found for escalation", ops_id);
            return Ok(());
        }
    };

    // coding_task を作成（asana_task_gid は ops_{id} でダミー、後で Asana 連携可能）
    let task_name = crate::claude::truncate_str(&item.message_text, 100);
    let task_id = state.db.create_task_from_ops(
        ops_id,
        &task_name,
        &item.message_text,
        &item.repo_key,
        channel,
        thread_ts.or(item.thread_ts.as_deref()).unwrap_or(&item.message_ts),
    )?;

    // ops 側を解決済みに
    state.db.resolve_ops(ops_id)?;
    tracing::info!("ops item {} escalated to task {}", ops_id, task_id);

    let slack = state.slack_client();

    // ボタンを更新
    if let Some(msg_ts) = message_ts {
        let updated_blocks = serde_json::json!([
            {
                "type": "section",
                "text": {
                    "type": "mrkdwn",
                    "text": format!(":clipboard: *タスク化済み* (task #{})", task_id)
                }
            }
        ]);
        slack
            .update_blocks(channel, msg_ts, &updated_blocks, &format!("タスク化済み (task #{})", task_id))
            .await
            .ok();
    }

    // スレッドに通知
    let reply_ts = thread_ts
        .or(item.thread_ts.as_deref())
        .unwrap_or(&item.message_ts);
    slack
        .reply_thread(
            channel,
            reply_ts,
            &format!(":clipboard: タスクとして登録しました (task #{})\n計画 → 実行のフローに入ります", task_id),
        )
        .await
        .ok();

    state.wake_worker();
    Ok(())
}

/// ops_resolve ボタンの処理
async fn process_ops_resolve(
    state: &AppState,
    action_value: &str,
    channel: &str,
    message_ts: Option<&str>,
) -> anyhow::Result<()> {
    let ops_id: i64 = action_value
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid ops_id: {}", action_value))?;

    state.db.resolve_ops(ops_id)?;
    tracing::info!("ops item {} resolved via button", ops_id);

    // ボタンを除去してメッセージを更新
    if let Some(msg_ts) = message_ts {
        let slack = state.slack_client();
        let updated_blocks = serde_json::json!([
            {
                "type": "section",
                "text": {
                    "type": "mrkdwn",
                    "text": ":white_check_mark: *対応完了*"
                }
            }
        ]);
        slack
            .update_blocks(channel, msg_ts, &updated_blocks, "対応完了")
            .await
            .ok();
    }

    Ok(())
}
