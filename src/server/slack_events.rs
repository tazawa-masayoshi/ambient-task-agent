use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;

use crate::claude::ClaudeRunner;
use super::http::AppState;

#[derive(Debug, Deserialize)]
struct ReactionAddedEvent {
    reaction: String,
    item: ReactionItem,
}

#[derive(Debug, Deserialize)]
struct ReactionItem {
    #[serde(rename = "type")]
    item_type: String,
    channel: String,
    ts: String,
}

/// Slack イベントをディスパッチ
pub async fn dispatch_event(state: &Arc<AppState>, event: &serde_json::Value) -> Result<()> {
    let event_type = event
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or_default();

    match event_type {
        "reaction_added" => {
            let ev: ReactionAddedEvent = serde_json::from_value(event.clone())?;
            handle_reaction_added(state, &ev).await
        }
        "app_mention" => handle_app_mention(state, event).await,
        "message" => handle_message(state, event).await,
        "assistant_thread_started" => handle_assistant_thread_started(state, event).await,
        "assistant_thread_context_changed" => {
            tracing::debug!("assistant_thread_context_changed (ignored)");
            Ok(())
        }
        _ => {
            tracing::debug!("Unhandled Slack event type: {}", event_type);
            Ok(())
        }
    }
}

// ============================================================================
// Phase 1: reaction_added
// ============================================================================

async fn handle_reaction_added(state: &Arc<AppState>, event: &ReactionAddedEvent) -> Result<()> {
    if event.item.item_type != "message" {
        return Ok(());
    }

    let channel = &event.item.channel;
    let message_ts = &event.item.ts;

    tracing::info!(
        "Reaction added: {} on message {} in {}",
        event.reaction,
        message_ts,
        channel
    );

    match event.reaction.as_str() {
        // ⚡ ops 手動実行（ops チャンネルのメッセージに対して）→ キューに追加
        "zap" => {
            if let Some(repo_entry) = state.repos_config.find_repo_by_ops_channel(channel) {
                let slack = state.slack_client();
                match slack.fetch_message(channel, message_ts).await {
                    Ok(msg) => {
                        let text = msg.get("text").and_then(|t| t.as_str()).unwrap_or_default();
                        tracing::info!("⚡ ops manual trigger in {}: {}", channel, crate::claude::truncate_str(text, 100));
                        enqueue_ops_request(state, &msg, channel, message_ts, None, text, repo_entry, "ready")?;
                    }
                    Err(e) => {
                        tracing::warn!("Failed to fetch message for ⚡ ops: {}", e);
                    }
                }
            }
        }

        // 👍 了解（承認操作は Block Kit ボタンに移行済み）
        "+1" => {
            let slack = state.slack_client();
            let task = state.db.find_task_by_slack_ts(channel, message_ts)?;
            let thread_ts = task
                .as_ref()
                .and_then(|t| t.slack_thread_ts.as_deref())
                .unwrap_or(message_ts);
            if let Err(e) = slack.reply_thread(channel, thread_ts, "👍 了解！").await {
                tracing::warn!("Failed to reply for +1 reaction: {}", e);
            }
        }

        _ => {}
    }

    Ok(())
}

// ============================================================================
// Phase 2: app_mention
// ============================================================================

async fn handle_app_mention(state: &Arc<AppState>, event: &serde_json::Value) -> Result<()> {
    let channel = event
        .get("channel")
        .and_then(|c| c.as_str())
        .unwrap_or_default();
    let text = event
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or_default();
    let event_ts = event.get("ts").and_then(|t| t.as_str()).unwrap_or_default();
    let event_thread_ts = event.get("thread_ts").and_then(|t| t.as_str());
    // スレッド返信先: thread_ts があればそれ、なければ ts 自体
    let thread_ts = event_thread_ts.unwrap_or(event_ts);

    // メンション部分を除去してコマンドを抽出
    let command = extract_command(text);
    tracing::info!("App mention command: '{}' in {}", command, channel);

    // ops チャンネルでのメンション → キューに追加
    if let Some(repo_entry) = state.repos_config.find_repo_by_ops_channel(channel) {
        return enqueue_ops_request(state, event, channel, event_ts, event_thread_ts, text, repo_entry, "ready");
    }

    let slack = state.slack_client();

    match command.to_lowercase().trim() {
        "sync" => {
            slack
                .reply_thread(channel, thread_ts, ":hourglass_flowing_sand: Asana 同期中...")
                .await?;

            let asana_config = crate::config::AsanaConfig {
                pat: state.asana_pat.clone(),
                project_id: state.asana_project_id.clone(),
                user_name: state.asana_user_name.clone(),
            };
            match crate::sync::run_sync(&asana_config).await {
                Ok(result) => {
                    let msg = if result.changed {
                        format!(
                            ":white_check_mark: Asana 同期完了（{}件変更）\n{}",
                            result.diff.len(),
                            result
                                .diff
                                .iter()
                                .take(10)
                                .map(|d| format!("  • {}", d))
                                .collect::<Vec<_>>()
                                .join("\n")
                        )
                    } else {
                        ":white_check_mark: Asana 同期完了（変更なし）".to_string()
                    };
                    slack.reply_thread(channel, thread_ts, &msg).await?;
                }
                Err(e) => {
                    slack
                        .reply_thread(
                            channel,
                            thread_ts,
                            &format!(":x: 同期失敗: {}", e),
                        )
                        .await?;
                }
            }
        }

        "status" => {
            let counts = state.db.count_tasks_by_status()?;
            let lines: Vec<String> = counts
                .iter()
                .map(|(status, count)| format!("  • {}: {}件", status, count))
                .collect();
            let msg = if lines.is_empty() {
                ":bar_chart: タスクはありません".to_string()
            } else {
                format!(":bar_chart: *タスクステータス*\n{}", lines.join("\n"))
            };
            slack.reply_thread(channel, thread_ts, &msg).await?;
        }

        cmd if cmd.contains("今日のタスク") || cmd.contains("ブリーフィング") => {
            slack
                .reply_thread(channel, thread_ts, ":brain: ブリーフィング生成中...")
                .await?;

            match generate_briefing_response(state).await {
                Ok(response) => {
                    slack.reply_thread(channel, thread_ts, &response).await?;
                }
                Err(e) => {
                    slack
                        .reply_thread(
                            channel,
                            thread_ts,
                            &format!(":x: ブリーフィング生成失敗: {}", e),
                        )
                        .await?;
                }
            }
        }

        // その他: claude -p に投げる（スレッド内なら履歴をコンテキストに含める）
        other if !other.is_empty() => {
            slack
                .reply_thread(channel, thread_ts, ":brain: 考え中...")
                .await?;

            // スレッド内メンションの場合、会話履歴を取得してプロンプトに含める
            let prompt = if event.get("thread_ts").is_some() {
                match slack.fetch_thread_replies(channel, thread_ts).await {
                    Ok(replies) => {
                        let history: Vec<String> = replies.iter()
                            .filter_map(|msg| {
                                let user = msg.get("user").and_then(|u| u.as_str()).unwrap_or("unknown");
                                let text = msg.get("text").and_then(|t| t.as_str())?;
                                Some(format!("<@{}>: {}", user, text))
                            })
                            .collect();
                        if history.is_empty() {
                            other.to_string()
                        } else {
                            format!(
                                "## スレッドの会話履歴\n{}\n\n## 依頼\n{}",
                                history.join("\n"),
                                other
                            )
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to fetch thread replies: {}", e);
                        other.to_string()
                    }
                }
            } else {
                other.to_string()
            };

            let log_dir = log_dir_from_state(state);
            match ClaudeRunner::new("mention", &prompt)
                .max_turns(3)
                .log_dir(&log_dir)
                .with_context(&state.runner_ctx)
                .non_blocking()
                .run()
                .await
            {
                Ok(result) if result.success => {
                    slack.reply_thread(channel, thread_ts, &result.stdout).await?;
                }
                Ok(result) => {
                    slack
                        .reply_thread(
                            channel,
                            thread_ts,
                            &format!(":x: 応答生成失敗: {}", result.stderr),
                        )
                        .await?;
                }
                Err(e) => {
                    slack
                        .reply_thread(
                            channel,
                            thread_ts,
                            &format!(":x: 応答生成失敗: {}", e),
                        )
                        .await?;
                }
            }
        }

        _ => {
            slack
                .reply_thread(
                    channel,
                    thread_ts,
                    "使い方: `@bot sync` / `@bot status` / `@bot 今日のタスク` / `@bot <質問>`",
                )
                .await?;
        }
    }

    Ok(())
}

/// ブリーフィング応答を生成
async fn generate_briefing_response(state: &Arc<AppState>) -> Result<String> {
    use chrono::Local;

    let asana_config = crate::config::AsanaConfig {
        pat: state.asana_pat.clone(),
        project_id: state.asana_project_id.clone(),
        user_name: state.asana_user_name.clone(),
    };
    if let Err(e) = crate::sync::run_sync(&asana_config).await {
        tracing::warn!("Asana sync failed before briefing: {}", e);
    }

    let cache = crate::sync::load_cache()?;
    let today = Local::now().format("%Y-%m-%d (%A)").to_string();

    let incomplete: Vec<_> = cache.tasks.iter().filter(|t| !t.completed).collect();
    let tasks_text: Vec<String> = incomplete
        .iter()
        .map(|t| {
            let due = t
                .due_on
                .as_deref()
                .map(|d| format!(" (期限: {})", d))
                .unwrap_or_default();
            format!("- {}{} (担当: {})", t.name, due, t.assignee)
        })
        .collect();

    let prompt = format!(
        "あなたはサーバント型PMです。以下のタスク情報から今日やるべきことを簡潔に提案してください。Slack mrkdwnで日本語出力。\n\n## 日付\n{}\n\n## タスク\n{}",
        today,
        tasks_text.join("\n")
    );

    let log_dir = log_dir_from_state(state);
    let result = ClaudeRunner::new("briefing", &prompt)
        .max_turns(3)
        .log_dir(&log_dir)
        .with_context(&state.runner_ctx)
        .non_blocking()
        .run()
        .await?;

    if !result.success {
        anyhow::bail!("briefing failed: {}", result.stderr);
    }

    Ok(result.stdout.trim().to_string())
}

// ============================================================================
// Phase 3: message (thread) — sleep/wake/archive
// ============================================================================

async fn handle_message(state: &Arc<AppState>, event: &serde_json::Value) -> Result<()> {
    // Bot 自身のメッセージを無視（無限ループ防止）
    if event.get("bot_id").is_some() || event.get("bot_profile").is_some() {
        return Ok(());
    }

    // サブタイプ付きメッセージ（message_changed 等）は無視
    if event.get("subtype").is_some() {
        return Ok(());
    }

    let channel = event
        .get("channel")
        .and_then(|c| c.as_str())
        .unwrap_or_default();

    // DM (im) チャンネル → Agent/Assistant として処理
    if is_dm_channel(channel) {
        return handle_dm_message(state, event).await;
    }

    let text = event
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or_default();
    let message_ts = event
        .get("ts")
        .and_then(|t| t.as_str())
        .unwrap_or_default();

    // ops チャンネル判定（トップレベル・スレッド返信の両方で使う）
    let thread_ts = event.get("thread_ts").and_then(|t| t.as_str());
    let bot_mention = format!("<@{}", state.bot_user_id);
    let has_mention = !state.bot_user_id.is_empty() && text.contains(&bot_mention);

    if let Some(repo_entry) = state.repos_config.find_repo_by_ops_channel(channel) {
        let sender = event.get("user").and_then(|u| u.as_str()).unwrap_or_default();
        let admin_user_id = state.repos_config.defaults.ops_admin_user.as_deref();
        let is_admin = admin_user_id.is_some_and(|admin| admin == sender);
        // @admin ユーザー宛メンションの検出（@bot 以外で admin 宛の依頼）
        let has_admin_mention = admin_user_id
            .is_some_and(|admin| text.contains(&format!("<@{}", admin)));

        match (thread_ts, has_mention) {
            // スレッド返信 + @bot メンション → admin のみ ops 即実行
            (Some(tts), true) if is_admin => {
                tracing::info!("ops thread mention by admin in {}: {}", channel, crate::claude::truncate_str(text, 100));
                enqueue_ops_request(state, event, channel, message_ts, Some(tts), text, repo_entry, "ready")?;
            }
            (Some(_), true) => {
                tracing::info!("ops thread mention by non-admin {} ignored", sender);
            }
            // トップレベル + @bot メンション → ops_monitor のチャンネルのみ即実行
            (None, true) if repo_entry.ops_monitor => {
                tracing::info!("ops top-level mention in {}: {}", channel, crate::claude::truncate_str(text, 100));
                enqueue_ops_request(state, event, channel, message_ts, None, text, repo_entry, "ready")?;
            }
            (None, true) => {
                tracing::debug!("ops_monitor=false, top-level mention ignored in {}", channel);
            }
            // トップレベル + @admin メンション → admin 宛依頼として即実行
            (None, false) if has_admin_mention => {
                tracing::info!("ops admin-mention in {}: {}", channel, crate::claude::truncate_str(text, 100));
                enqueue_ops_request(state, event, channel, message_ts, None, text, repo_entry, "ready")?;
            }
            // トップレベル + メンションなし → 自動分類
            (None, false) => {
                if repo_entry.ops_monitor {
                    enqueue_ops_request(state, event, channel, message_ts, None, text, repo_entry, "pending")?;
                }
            }
            // スレッド返信 + メンションなし → 通常は無視（人同士の会話）
            // Inception モードはターン2へ継続するためエンキュー
            (Some(tts), false) => {
                if repo_entry.ops_mode == crate::repo_config::OpsMode::Inception {
                    tracing::info!("inception: thread reply enqueued for turn2 in {}", channel);
                    enqueue_ops_request(state, event, channel, message_ts, Some(tts), text, repo_entry, "ready")?;
                }
            }
        }
        return Ok(());
    }

    if thread_ts.is_none() {
        // Asana URL 自動リンク
        if let Some(task_gid) = extract_asana_task_gid(text) {
            handle_asana_url_link(state, channel, message_ts, &task_gid).await?;
        }
        return Ok(());
    }

    let thread_ts = thread_ts.unwrap();
    let text_lower = text.trim().to_lowercase();

    // thread_ts でタスクを検索（slack_thread_ts → converse_thread_ts の順でフォールバック）
    let task = match state.db.find_task_by_thread_ts(channel, thread_ts)? {
        Some(t) => t,
        None => match state.db.find_conversing_task_by_thread(channel, thread_ts)? {
            Some(t) => t,
            None => return Ok(()), // タスクスレッドでなければ無視
        },
    };

    let slack = state.slack_client();

    match text_lower.as_str() {
        "sleep" => {
            if task.status == "sleeping" || task.status == "archived" || task.status == "completed" {
                slack
                    .reply_thread(
                        channel,
                        thread_ts,
                        &format!(":no_entry: 現在のステータスは `{}` のため sleep できません", task.status),
                    )
                    .await?;
                return Ok(());
            }
            state.db.update_status(task.id, "sleeping")?;
            slack
                .reply_thread(channel, thread_ts, ":zzz: タスクをスリープしました")
                .await?;
            tracing::info!("Task {} set to sleeping via thread message", task.id);
        }

        "wake" => {
            if task.status != "sleeping" {
                slack
                    .reply_thread(
                        channel,
                        thread_ts,
                        &format!(":no_entry: 現在のステータスは `{}` のため wake できません（sleeping のみ）", task.status),
                    )
                    .await?;
                return Ok(());
            }
            state.db.update_status(task.id, "new")?;
            slack
                .reply_thread(channel, thread_ts, ":sunny: タスクを再開しました")
                .await?;
            tracing::info!("Task {} woken up via thread message", task.id);
            state.wake_worker();
        }

        "直した" | "fixed" | "修正完了" => {
            if task.status == "manual" {
                state.db.update_status(task.id, "executing")?;
                slack
                    .reply_thread(channel, thread_ts, ":rocket: 確認しました。実行を再開します...")
                    .await?;
                tracing::info!("Task {} resumed from manual mode via thread reply", task.id);
                state.wake_worker();
            }
        }

        "archive" => {
            state.db.update_status(task.id, "archived")?;
            slack
                .reply_thread(channel, thread_ts, ":file_cabinet: タスクをアーカイブしました")
                .await?;
            tracing::info!("Task {} archived via thread message", task.id);
        }

        // 承認/実行開始（conversing → executing）
        "ok" | "承認" | "approve" | "go" | "実行" | "run" => {
            if task.status == "conversing" {
                state.db.update_status(task.id, "executing")?;
                slack
                    .reply_thread(channel, thread_ts, ":rocket: 実行を開始します...")
                    .await?;
                tracing::info!("Task {} → executing via thread reply", task.id);
                state.wake_worker();
            } else {
                slack
                    .reply_thread(
                        channel,
                        thread_ts,
                        &format!(":no_entry: 現在のステータスは `{}` のため実行できません（conversing のみ）", task.status),
                    )
                    .await?;
            }
        }

        "ng" | "却下" | "reject" => {
            if task.status == "conversing" {
                state.db.update_status(task.id, "done")?;
                slack
                    .reply_thread(channel, thread_ts, ":x: タスクを却下しました")
                    .await?;
                tracing::info!("Task {} rejected via thread reply", task.id);
            }
        }

        // 実行中タスクの停止
        "stop" | "cancel" | "中止" | "停止" => {
            match task.status.as_str() {
                "executing" | "ci_pending" | "conversing" => {
                    let prev_status = task.status.clone();
                    state.db.set_error(task.id, &format!("Cancelled by user (was {})", prev_status))?;
                    slack
                        .reply_thread(
                            channel,
                            thread_ts,
                            &format!(":octagonal_sign: タスクを中止しました（`{}` → `error`）\n\
                                      実行中のプロセスは次のターン終了時に停止します", prev_status),
                        )
                        .await?;
                    tracing::info!("Task {} cancelled via thread reply (was {})", task.id, prev_status);
                }
                _ => {
                    slack
                        .reply_thread(
                            channel,
                            thread_ts,
                            &format!(":no_entry: 現在のステータスは `{}` のため中止できません", task.status),
                        )
                        .await?;
                }
            }
        }

        // ステータス確認
        cmd if cmd == "status" || cmd == "状態" || cmd.starts_with("進捗") => {
            let status_emoji = match task.status.as_str() {
                "new" => ":inbox_tray:",
                "conversing" => ":speech_balloon:",
                "executing" => ":rocket:",
                "manual" => ":hammer:",
                "ci_pending" => ":hourglass:",
                "done" => ":tada:",
                "error" => ":x:",
                "sleeping" => ":zzz:",
                _ => ":grey_question:",
            };
            let mut msg = format!(
                "{} *{}*\nステータス: `{}`",
                status_emoji, task.asana_task_name, task.status
            );
            if let Some(ref pr_url) = task.pr_url {
                msg.push_str(&format!("\nPR: {}", pr_url));
            }
            if let Some(ref branch) = task.branch_name {
                msg.push_str(&format!("\nブランチ: `{}`", branch));
            }
            if task.retry_count > 0 {
                msg.push_str(&format!("\nリトライ: {}回", task.retry_count));
            }
            slack.reply_thread(channel, thread_ts, &msg).await?;
        }

        _ => {
            // conversing タスクへのスレッド返信 → ops_contexts に追記して会話継続
            if task.status == "conversing" {
                let converse_ts = task.converse_thread_ts.as_deref()
                    .unwrap_or(thread_ts);
                let repo_key = task.repo_key.as_deref().unwrap_or("default");
                state.db.append_ops_context(channel, converse_ts, repo_key, "user", text)?;
                state.wake_worker();
                tracing::info!("Task {} conversing: user replied, waking worker", task.id);
            }
            // manual 中のその他メッセージは作業メモとして無視
            // それ以外の認識できないスレッド返信も無視
        }
    }

    Ok(())
}

// ============================================================================
// Phase 4: Agent/Assistant DM
// ============================================================================

/// assistant_thread_started: ユーザーが Agent を開いた時
async fn handle_assistant_thread_started(state: &Arc<AppState>, event: &serde_json::Value) -> Result<()> {
    let channel = event
        .get("assistant_thread")
        .and_then(|t| t.get("channel_id"))
        .and_then(|c| c.as_str())
        .unwrap_or_default();
    let thread_ts = event
        .get("assistant_thread")
        .and_then(|t| t.get("thread_ts"))
        .and_then(|t| t.as_str())
        .unwrap_or_default();

    if channel.is_empty() || thread_ts.is_empty() {
        return Ok(());
    }

    // Suggested Prompts を設定
    let prompts = serde_json::json!([
        { "title": "今日のタスク", "message": "今日やるべきタスクの一覧と優先度を教えて" },
        { "title": "新しいタスクを依頼", "message": "このタスクをお願いしたい：" },
        { "title": "進捗確認", "message": "現在進行中のタスクのステータスを教えて" },
        { "title": "今日のブリーフィング", "message": "今日の予定・タスク・注意事項をまとめて" }
    ]);

    // assistant.threads.setSuggestedPrompts
    let resp = reqwest::Client::new()
        .post("https://slack.com/api/assistant.threads.setSuggestedPrompts")
        .header("Authorization", format!("Bearer {}", state.slack_bot_token))
        .json(&serde_json::json!({
            "channel_id": channel,
            "thread_ts": thread_ts,
            "prompts": prompts,
        }))
        .send()
        .await;

    if let Ok(r) = resp {
        let body: serde_json::Value = r.json().await.unwrap_or_default();
        if body.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            tracing::warn!("setSuggestedPrompts failed: {:?}", body.get("error"));
        }
    }

    tracing::info!("Assistant thread started: channel={}, thread_ts={}", channel, thread_ts);
    Ok(())
}

/// DM (im) チャンネルかどうかを判定
fn is_dm_channel(channel: &str) -> bool {
    channel.starts_with('D')
}

/// DM メッセージを Agent として処理
async fn handle_dm_message(state: &Arc<AppState>, event: &serde_json::Value) -> Result<()> {
    let channel = event.get("channel").and_then(|c| c.as_str()).unwrap_or_default();
    let text = event.get("text").and_then(|t| t.as_str()).unwrap_or_default();
    let thread_ts = event.get("thread_ts").and_then(|t| t.as_str());
    let message_ts = event.get("ts").and_then(|t| t.as_str()).unwrap_or_default();

    // スレッド内の返信: thread_ts を使う。トップレベル: ts を使う
    let reply_ts = thread_ts.unwrap_or(message_ts);

    let slack = state.slack_client();

    // ローディング表示
    let _ = reqwest::Client::new()
        .post("https://slack.com/api/assistant.threads.setStatus")
        .header("Authorization", format!("Bearer {}", state.slack_bot_token))
        .json(&serde_json::json!({
            "channel_id": channel,
            "thread_ts": reply_ts,
            "status": "考え中...",
        }))
        .send()
        .await;

    // DM → Inception モード（ambient-task-agent）で処理
    // ops_mode=inception のリポを探す
    let repo_entry = state.repos_config.repo.iter()
        .find(|r| r.ops_mode == crate::repo_config::OpsMode::Inception)
        .or_else(|| state.repos_config.find_repo_by_ops_channel(&state.slack_channel));

    if let Some(repo_entry) = repo_entry {
        enqueue_ops_request(state, event, channel, message_ts, thread_ts, text, repo_entry, "ready")?;
    } else {
        slack.reply_thread(channel, reply_ts, ":wave: メッセージを受け取りました。現在 DM からの直接処理は設定されていません。").await?;
    }

    Ok(())
}

/// Asana URL からタスク GID を抽出
/// https://app.asana.com/0/{project_id}/{task_gid} or .../f
fn extract_asana_task_gid(text: &str) -> Option<String> {
    // Slack はURLを <url> や <url|label> 形式で囲む場合がある
    for segment in text.split_whitespace() {
        let url = segment
            .trim_start_matches('<')
            .trim_end_matches('>')
            .split('|')
            .next()
            .unwrap_or("");
        if let Some(path) = url.strip_prefix("https://app.asana.com/0/") {
            // path = "{project_id}/{task_gid}" or "{project_id}/{task_gid}/f"
            let parts: Vec<&str> = path.split('/').collect();
            if parts.len() >= 2 && !parts[1].is_empty() && parts[1].chars().all(|c| c.is_ascii_digit()) {
                return Some(parts[1].to_string());
            }
        }
    }
    None
}

/// Asana URL を検出してタスクと Slack スレッドを自動リンク
async fn handle_asana_url_link(
    state: &Arc<AppState>,
    channel: &str,
    message_ts: &str,
    task_gid: &str,
) -> Result<()> {
    let task = match state.db.find_task_by_gid(task_gid)? {
        Some(t) => t,
        None => return Ok(()), // DB にないタスクは無視
    };

    // 既に Slack スレッドがリンク済みなら返信で通知
    if task.slack_thread_ts.is_some() {
        let slack = state.slack_client();
        slack
            .reply_thread(
                channel,
                message_ts,
                &format!(
                    ":link: このタスクは既に Slack スレッドにリンクされています（*{}*）",
                    task.asana_task_name
                ),
            )
            .await?;
        return Ok(());
    }

    // Slack スレッド未設定 → このメッセージをスレッドとしてリンク
    state.db.update_slack_thread(task.id, channel, message_ts)?;

    let slack = state.slack_client();
    slack
        .reply_thread(
            channel,
            message_ts,
            &format!(
                ":link: タスク *{}* をこのスレッドにリンクしました\nステータス: `{}`",
                task.asana_task_name, task.status
            ),
        )
        .await?;

    tracing::info!(
        "Auto-linked task {} ({}) to Slack thread {} in {}",
        task.asana_task_gid,
        task.asana_task_name,
        message_ts,
        channel
    );

    // Asana タスクにも Slack URL をコメント（workspace 設定あり時）
    if let Some(ref workspace) = state.slack_workspace {
        let slack_url = format!(
            "https://{}.slack.com/archives/{}/p{}",
            workspace,
            channel,
            message_ts.replace('.', "")
        );
        let asana_config = crate::config::AsanaConfig {
            pat: state.asana_pat.clone(),
            project_id: String::new(),
            user_name: String::new(),
        };
        let asana_client = crate::asana::client::AsanaClient::new(asana_config);
        if let Err(e) = asana_client
            .post_comment(task_gid, &format!("🔗 Slack スレッド: {}", slack_url))
            .await
        {
            tracing::warn!("Failed to post Asana comment with Slack URL: {}", e);
        }
    }

    Ok(())
}

// ============================================================================
// Ops enqueue
// ============================================================================

/// ops リクエストをキューに追加してワーカーを起床
///
/// - status = "pending": 分類が必要（ops_monitor 自動検出）
/// - status = "ready": 分類不要で即実行（⚡手動トリガー、@メンション、スレッド返信）
#[allow(clippy::too_many_arguments)]
fn enqueue_ops_request(
    state: &Arc<AppState>,
    event: &serde_json::Value,
    channel: &str,
    message_ts: &str,
    thread_ts: Option<&str>,
    text: &str,
    repo_entry: &crate::repo_config::RepoEntry,
    status: &str,
) -> Result<()> {
    let event_json = serde_json::to_string(event).unwrap_or_default();
    let id = state.db.enqueue_ops(
        channel, message_ts, thread_ts, &repo_entry.key, text, &event_json, status,
    )?;
    tracing::info!("Enqueued ops item {} (status={}, channel={})", id, status, channel);
    state.wake_worker();
    Ok(())
}

// ============================================================================
// Helpers
// ============================================================================

/// ログ出力先ディレクトリを AppState から計算
fn log_dir_from_state(state: &Arc<AppState>) -> PathBuf {
    PathBuf::from(&state.repos_config.defaults.repos_base_dir)
        .join(".agent")
        .join("logs")
}

/// メンションテキストからコマンド部分を抽出
/// "<@U12345> sync" → "sync"
pub fn extract_command(text: &str) -> &str {
    // <@UXXXXXXX> を除去
    if let Some(pos) = text.find('>') {
        text[pos + 1..].trim()
    } else {
        text.trim()
    }
}
