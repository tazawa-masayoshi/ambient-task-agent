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
    user: Option<UserInfo>,
}

#[derive(Debug, Deserialize)]
struct UserInfo {
    id: String,
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
    let user_id = payload.user.map(|u| u.id);

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
            user_id.as_deref(),
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
    let user_id = action_payload.user.as_ref().map(|u| u.id.as_str());

    process_action(
        state,
        &action.action_id,
        action_value,
        channel,
        message_ts,
        thread_ts,
        user_id,
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
    user_id: Option<&str>,
) -> anyhow::Result<()> {
    // ops 系アクションは task_id ではなく ops_queue の id を使う
    if action_id == "ops_resolve" {
        return process_ops_resolve(state, action_value, channel, message_ts).await;
    }
    if action_id == "ops_escalate" {
        return process_ops_escalate(state, action_value, channel, message_ts, thread_ts).await;
    }
    // Inception モード 承認ゲート（admin 権限チェック）
    if action_id.starts_with("ops_inception_") {
        if let Some(admin) = state.repos_config.defaults.ops_admin_user.as_deref() {
            let actor = user_id.unwrap_or_default();
            if actor != admin {
                tracing::warn!(
                    "inception action denied: user {} is not admin {} for action {}",
                    actor, admin, action_id,
                );
                if let Some(msg_ts) = message_ts {
                    let slack = state.slack_client();
                    slack.reply_thread(
                        channel,
                        msg_ts,
                        &format!(":no_entry: この操作は管理者 (<@{}>) のみ実行できます。", admin),
                    ).await.ok();
                }
                return Ok(());
            }
        }
    }
    if action_id == "ops_inception_approve" {
        return process_ops_inception_approve(state, action_value, channel, message_ts).await;
    }
    if action_id == "ops_inception_revise" {
        return process_ops_inception_revise(state, action_value, channel, message_ts).await;
    }
    if action_id == "ops_inception_cancel" {
        return process_ops_inception_cancel(state, action_value, channel, message_ts).await;
    }
    if action_id == "ops_inception_asana" {
        return process_ops_inception_asana(state, action_value, channel, message_ts).await;
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

        "task_execute" => {
            // conversing/proposed → approved（worker ループが executing に遷移して実行）
            if task.status == "conversing" || task.status == "proposed" || task.status == "manual" {
                state.db.update_status(task.id, "approved")?;
                slack
                    .reply_thread(channel, reply_ts, ":white_check_mark: 実行を開始します...")
                    .await?;
                tracing::info!("Task {} approved via task_execute button", task.id);
                state.wake_worker();
            }
        }

        "task_converse" => {
            // ステータス変更なし（次のスレッド返信を待つ）
            slack
                .reply_thread(
                    channel,
                    reply_ts,
                    ":speech_balloon: スレッドに追加の指示を入力してください",
                )
                .await?;
        }

        "task_manual" => {
            state.db.update_status(task.id, "manual")?;
            let branch_info = task
                .branch_name
                .as_deref()
                .map(|b| format!("\nブランチ: `{}`", b))
                .unwrap_or_default();
            slack
                .reply_thread(
                    channel,
                    reply_ts,
                    &format!(
                        ":hammer: 手動修正モードに入りました{}\n完了したら `直した` と返信してください",
                        branch_info
                    ),
                )
                .await?;
            tracing::info!("Task {} set to manual mode via task_manual button", task.id);
        }

        "task_skip" => {
            state.db.update_status(task.id, "done")?;
            slack
                .reply_thread(channel, reply_ts, ":fast_forward: タスクをスキップしました")
                .await?;
            tracing::info!("Task {} skipped via task_skip button", task.id);
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
        task_name,
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

/// ops_inception_approve ボタンの処理: タスク分解結果を Asana に登録
async fn process_ops_inception_approve(
    state: &AppState,
    action_value: &str,
    channel: &str,
    message_ts: Option<&str>,
) -> anyhow::Result<()> {
    let ops_id: i64 = action_value
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid ops_id: {}", action_value))?;

    let item = match state.db.get_ops_item(ops_id)? {
        Some(i) => i,
        None => {
            tracing::warn!("inception approve: ops item {} not found", ops_id);
            return Ok(());
        }
    };

    // IDOR 防止: ボタンが押されたチャンネルと ops アイテムのチャンネルが一致するか検証
    if item.channel != channel {
        tracing::warn!("inception approve: channel mismatch ops_id={} (expected={}, got={})", ops_id, item.channel, channel);
        return Ok(());
    }

    // ボタンを更新
    if let Some(msg_ts) = message_ts {
        let slack = state.slack_client();
        let updated_blocks = serde_json::json!([
            {
                "type": "section",
                "text": {
                    "type": "mrkdwn",
                    "text": ":white_check_mark: *承認されました — Asana 登録中...*"
                }
            }
        ]);
        slack
            .update_blocks(channel, msg_ts, &updated_blocks, "Asana 登録中...")
            .await
            .ok();
    }

    // ops_contexts から最新の assistant 出力（ターン2の要件定義）を取得
    // runner.rs と同じロジックで reply_ts を決定（item ベース統一、Slack payload は使わない）
    let reply_ts = item.thread_ts.as_deref().unwrap_or(&item.message_ts);
    let history = state.db.get_ops_context(channel, reply_ts)?;
    let last_output = history
        .iter()
        .rev()
        .find(|m| m.role == "assistant")
        .map(|m| m.content.as_str())
        .unwrap_or(&item.message_text);

    // TASKS_JSON を抽出して各タスクを登録
    let tasks = crate::worker::ops::extract_tasks_json(last_output);
    let slack = state.slack_client();

    // タスク数の上限ガード（Claude 出力暴走時の安全弁）
    if tasks.len() > 50 {
        tracing::warn!("inception approve: too many tasks ({}) from ops item {}, truncating to 50", tasks.len(), ops_id);
    }
    let tasks = if tasks.len() > 50 { &tasks[..50] } else { &tasks[..] };

    if tasks.is_empty() {
        // TASKS_JSON がない場合は単一タスクとして登録
        let task_name = crate::claude::truncate_str(&item.message_text, 100);
        let task_id = state.db.create_task_from_ops(
            ops_id,
            task_name,
            last_output,
            &item.repo_key,
            channel,
            reply_ts,
        )?;
        tracing::info!("inception: registered task #{} (single) from ops item {}", task_id, ops_id);
        slack
            .reply_thread(
                channel,
                reply_ts,
                &format!(":clipboard: タスクを登録しました (task #{})\n計画 → 実行のフローに入ります", task_id),
            )
            .await
            .ok();
    } else {
        // 複数タスクを登録
        let mut registered_ids = Vec::new();
        for task_json in tasks {
            let title = task_json.get("title").and_then(|v: &serde_json::Value| v.as_str()).unwrap_or("Inception task");
            let description = task_json.get("description").and_then(|v: &serde_json::Value| v.as_str()).unwrap_or("");
            let task_id = state.db.create_task_from_ops(
                ops_id,
                title,
                description,
                &item.repo_key,
                channel,
                reply_ts,
            )?;
            registered_ids.push(task_id);
            tracing::info!("inception: registered task #{} '{}' from ops item {}", task_id, title, ops_id);
        }
        let ids_str: Vec<String> = registered_ids.iter().map(|id| format!("#{}", id)).collect();
        slack
            .reply_thread(
                channel,
                reply_ts,
                &format!(
                    ":clipboard: {} 件のタスクを登録しました ({})\n計画 → 実行のフローに入ります",
                    registered_ids.len(),
                    ids_str.join(", ")
                ),
            )
            .await
            .ok();
    }

    state.db.resolve_ops(ops_id)?;
    state.wake_worker();
    Ok(())
}

/// ops_inception_revise ボタンの処理: ops_contexts をリセットしてターン1からやり直し
async fn process_ops_inception_revise(
    state: &AppState,
    action_value: &str,
    channel: &str,
    message_ts: Option<&str>,
) -> anyhow::Result<()> {
    let ops_id: i64 = action_value
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid ops_id: {}", action_value))?;

    let item = match state.db.get_ops_item(ops_id)? {
        Some(i) => i,
        None => {
            tracing::warn!("inception revise: ops item {} not found", ops_id);
            return Ok(());
        }
    };

    // IDOR 防止: ボタンが押されたチャンネルと ops アイテムのチャンネルが一致するか検証
    if item.channel != channel {
        tracing::warn!("inception revise: channel mismatch ops_id={} (expected={}, got={})", ops_id, item.channel, channel);
        return Ok(());
    }

    // ボタンを更新（修正待ち状態を表示）
    if let Some(msg_ts) = message_ts {
        let slack = state.slack_client();
        let updated_blocks = serde_json::json!([
            {
                "type": "section",
                "text": {
                    "type": "mrkdwn",
                    "text": ":arrows_counterclockwise: *修正リクエスト — スレッドに修正内容を返信してください*"
                }
            }
        ]);
        slack
            .update_blocks(channel, msg_ts, &updated_blocks, "修正待ち...")
            .await
            .ok();
    }

    let reply_ts = item.thread_ts.as_deref().unwrap_or(&item.message_ts);

    // ops_contexts は保持（Turn2 再実行時に履歴＋修正フィードバックをコンテキストとして使う）
    // 再エンキューもしない — ユーザーのスレッド返信を待ち、
    // slack_events.rs の Inception スレッド返信ハンドリングで自動エンキューされる

    state.db.resolve_ops(ops_id)?;

    let slack = state.slack_client();
    slack
        .reply_thread(
            channel,
            reply_ts,
            ":pencil2: 修正内容をこのスレッドに返信してください。返信をもとに要件定義をやり直します。",
        )
        .await
        .ok();

    Ok(())
}

/// ops_inception_asana ボタンの処理: TASKS_JSON をローカル DB に登録（自動実行なし）
async fn process_ops_inception_asana(
    state: &AppState,
    action_value: &str,
    channel: &str,
    message_ts: Option<&str>,
) -> anyhow::Result<()> {
    let ops_id: i64 = action_value
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid ops_id: {}", action_value))?;

    let item = match state.db.get_ops_item(ops_id)? {
        Some(i) => i,
        None => {
            tracing::warn!("inception asana: ops item {} not found", ops_id);
            return Ok(());
        }
    };

    if item.channel != channel {
        tracing::warn!("inception asana: channel mismatch ops_id={}", ops_id);
        return Ok(());
    }

    // ボタンを更新
    if let Some(msg_ts) = message_ts {
        let slack = state.slack_client();
        let updated_blocks = serde_json::json!([
            {
                "type": "section",
                "text": {
                    "type": "mrkdwn",
                    "text": ":clipboard: *Asana 登録中...*"
                }
            }
        ]);
        slack
            .update_blocks(channel, msg_ts, &updated_blocks, "Asana 登録中...")
            .await
            .ok();
    }

    let reply_ts = item.thread_ts.as_deref().unwrap_or(&item.message_ts);
    let history = state.db.get_ops_context(channel, reply_ts)?;
    let last_output = history
        .iter()
        .rev()
        .find(|m| m.role == "assistant")
        .map(|m| m.content.as_str())
        .unwrap_or(&item.message_text);

    let tasks = crate::worker::ops::extract_tasks_json(last_output);
    let slack = state.slack_client();

    if tasks.len() > 50 {
        tracing::warn!("inception asana: too many tasks ({}) from ops item {}", tasks.len(), ops_id);
    }
    let tasks = if tasks.len() > 50 { &tasks[..50] } else { &tasks[..] };

    if tasks.is_empty() {
        let task_name = crate::claude::truncate_str(&item.message_text, 100);
        let task_id = state.db.create_task_from_ops(
            ops_id, task_name, last_output, &item.repo_key, channel, reply_ts,
        )?;
        // 'registered' にして worker に拾わせない
        state.db.update_status(task_id, "registered")?;
        tracing::info!("inception asana: registered task #{} (single, no-exec) from ops item {}", task_id, ops_id);
        slack
            .reply_thread(
                channel,
                reply_ts,
                &format!(":clipboard: タスクを登録しました (task #{})\n_※ 自動実行なし — Asana 連携は今後実装予定_", task_id),
            )
            .await
            .ok();
    } else {
        let mut registered_ids = Vec::new();
        for task_json in tasks {
            let title = task_json.get("title").and_then(|v: &serde_json::Value| v.as_str()).unwrap_or("Inception task");
            let description = task_json.get("description").and_then(|v: &serde_json::Value| v.as_str()).unwrap_or("");
            let task_id = state.db.create_task_from_ops(
                ops_id, title, description, &item.repo_key, channel, reply_ts,
            )?;
            state.db.update_status(task_id, "registered")?;
            registered_ids.push((task_id, title.to_string()));
            tracing::info!("inception asana: registered task #{} '{}' (no-exec) from ops item {}", task_id, title, ops_id);
        }
        let task_list: Vec<String> = registered_ids
            .iter()
            .map(|(id, title)| format!("• #{} {}", id, title))
            .collect();
        slack
            .reply_thread(
                channel,
                reply_ts,
                &format!(
                    ":clipboard: {} 件のタスクを登録しました:\n{}\n_※ 自動実行なし — Asana 連携は今後実装予定_",
                    registered_ids.len(),
                    task_list.join("\n")
                ),
            )
            .await
            .ok();
    }

    state.db.resolve_ops(ops_id)?;
    Ok(())
}

/// ops_inception_cancel ボタンの処理
async fn process_ops_inception_cancel(
    state: &AppState,
    action_value: &str,
    channel: &str,
    message_ts: Option<&str>,
) -> anyhow::Result<()> {
    let ops_id: i64 = action_value
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid ops_id: {}", action_value))?;

    state.db.resolve_ops(ops_id)?;
    tracing::info!("inception: ops item {} cancelled", ops_id);

    if let Some(msg_ts) = message_ts {
        let slack = state.slack_client();
        let updated_blocks = serde_json::json!([
            {
                "type": "section",
                "text": {
                    "type": "mrkdwn",
                    "text": ":x: *キャンセルされました*"
                }
            }
        ]);
        slack
            .update_blocks(channel, msg_ts, &updated_blocks, "キャンセルされました")
            .await
            .ok();
    }

    Ok(())
}
