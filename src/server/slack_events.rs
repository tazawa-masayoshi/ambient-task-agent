use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;

use super::http::AppState;
use crate::slack::client::SlackClient;

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

    // message_ts でタスクを検索（slack_thread_ts or slack_plan_ts）
    let task = match state.db.find_task_by_slack_ts(channel, message_ts)? {
        Some(t) => t,
        None => {
            // 👍 のような汎用スタンプはタスク以外にも押される
            if event.reaction == "+1" {
                // タスク外のメッセージ → 「了解」返信
                let slack = build_slack_client(state);
                slack.reply_thread(channel, message_ts, "👍 了解！").await.ok();
            }
            return Ok(());
        }
    };

    let slack = build_slack_client(state);
    let thread_ts = task.slack_thread_ts.as_deref().unwrap_or(message_ts);

    match event.reaction.as_str() {
        // ✅ 承認
        "white_check_mark" => {
            if task.status != "plan_posted" {
                tracing::debug!("Task {} is not in plan_posted status, ignoring ✅", task.id);
                return Ok(());
            }
            state.db.update_status(task.id, "approved")?;
            slack
                .reply_thread(
                    channel,
                    thread_ts,
                    ":white_check_mark: プランが承認されました！",
                )
                .await?;
            tracing::info!("Task {} approved via reaction", task.id);
        }

        // ❌ 却下
        "x" => {
            if task.status != "plan_posted" {
                tracing::debug!("Task {} is not in plan_posted status, ignoring ❌", task.id);
                return Ok(());
            }
            state.db.update_status(task.id, "rejected")?;
            slack
                .reply_thread(channel, thread_ts, ":x: プランが却下されました。")
                .await?;
            tracing::info!("Task {} rejected via reaction", task.id);
        }

        // 🔄 再生成
        "arrows_counterclockwise" => {
            if task.status != "plan_posted" {
                tracing::debug!(
                    "Task {} is not in plan_posted status, ignoring 🔄",
                    task.id
                );
                return Ok(());
            }
            state.db.reset_for_regeneration(task.id)?;
            slack
                .reply_thread(
                    channel,
                    thread_ts,
                    ":arrows_counterclockwise: プランを再生成します...",
                )
                .await?;
            tracing::info!("Task {} queued for regeneration via reaction", task.id);
        }

        // 👍 了解
        "+1" => {
            slack
                .reply_thread(channel, thread_ts, "👍 了解！")
                .await?;
        }

        _ => {
            tracing::debug!("Unhandled reaction: {}", event.reaction);
        }
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
    let thread_ts = event
        .get("thread_ts")
        .or_else(|| event.get("ts"))
        .and_then(|t| t.as_str())
        .unwrap_or_default();

    // メンション部分を除去してコマンドを抽出
    let command = extract_command(text);
    tracing::info!("App mention command: '{}' in {}", command, channel);

    let slack = build_slack_client(state);

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

        // その他: claude -p に投げる
        other if !other.is_empty() => {
            slack
                .reply_thread(channel, thread_ts, ":brain: 考え中...")
                .await?;

            match crate::claude::run_claude_prompt(other, 3).await {
                Ok(response) => {
                    slack.reply_thread(channel, thread_ts, &response).await?;
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
        "あなたはAI Scrum Masterです。以下のタスク情報から今日やるべきことを簡潔に提案してください。Slack mrkdwnで日本語出力。\n\n## 日付\n{}\n\n## タスク\n{}",
        today,
        tasks_text.join("\n")
    );

    crate::claude::run_claude_prompt(&prompt, 3).await
}

// ============================================================================
// Phase 3: message (thread) — sleep/wake/archive
// ============================================================================

async fn handle_message(state: &Arc<AppState>, event: &serde_json::Value) -> Result<()> {
    // bot 自身のメッセージを無視（無限ループ防止）
    if event.get("bot_id").is_some() || event.get("bot_profile").is_some() {
        return Ok(());
    }

    // サブタイプ付きメッセージ（message_changed 等）は無視
    if event.get("subtype").is_some() {
        return Ok(());
    }

    // スレッド返信のみ対象
    let thread_ts = match event.get("thread_ts").and_then(|t| t.as_str()) {
        Some(ts) => ts,
        None => return Ok(()), // トップレベルメッセージは無視
    };

    let channel = event
        .get("channel")
        .and_then(|c| c.as_str())
        .unwrap_or_default();
    let text = event
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .trim()
        .to_lowercase();

    // thread_ts でタスクを検索
    let task = match state.db.find_task_by_thread_ts(channel, thread_ts)? {
        Some(t) => t,
        None => return Ok(()), // タスクスレッドでなければ無視
    };

    let slack = build_slack_client(state);

    match text.as_str() {
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
            state.db.update_status(task.id, "pending")?;
            slack
                .reply_thread(channel, thread_ts, ":sunny: タスクを再開しました")
                .await?;
            tracing::info!("Task {} woken up via thread message", task.id);
        }

        "archive" => {
            state.db.update_status(task.id, "archived")?;
            slack
                .reply_thread(channel, thread_ts, ":file_cabinet: タスクをアーカイブしました")
                .await?;
            tracing::info!("Task {} archived via thread message", task.id);
        }

        _ => {
            // sleep/wake/archive 以外のスレッド返信は無視
        }
    }

    Ok(())
}

// ============================================================================
// Helpers
// ============================================================================

fn build_slack_client(state: &AppState) -> SlackClient {
    let config = crate::config::SlackConfig {
        bot_token: state.slack_bot_token.clone(),
        test_channel: state.slack_channel.clone(),
        signing_secret: state.slack_signing_secret.clone(),
    };
    SlackClient::new(config)
}

/// メンションテキストからコマンド部分を抽出
/// "<@U12345> sync" → "sync"
fn extract_command(text: &str) -> &str {
    // <@UXXXXXXX> を除去
    if let Some(pos) = text.find('>') {
        text[pos + 1..].trim()
    } else {
        text.trim()
    }
}
