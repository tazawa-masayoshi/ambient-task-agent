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
        // 🤖 自動実行（proposed → auto_approved）
        "robot_face" => {
            let task = match state.db.find_task_by_slack_ts(channel, message_ts)? {
                Some(t) => t,
                None => return Ok(()),
            };
            if task.status != "proposed" {
                tracing::debug!(
                    "Task {} is not in proposed status ({}), ignoring 🤖",
                    task.id,
                    task.status
                );
                return Ok(());
            }
            state.db.update_status(task.id, "auto_approved")?;
            let slack = state.slack_client();
            let thread_ts = task.slack_thread_ts.as_deref().unwrap_or(message_ts);
            slack
                .reply_thread(
                    channel,
                    thread_ts,
                    ":robot_face: 自動実行モードで承認されました！実行を開始します...",
                )
                .await?;
            tracing::info!("Task {} auto_approved via 🤖 reaction", task.id);
            state.wake_worker();
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
    let thread_ts = event
        .get("thread_ts")
        .or_else(|| event.get("ts"))
        .and_then(|t| t.as_str())
        .unwrap_or_default();

    // メンション部分を除去してコマンドを抽出
    let command = extract_command(text);
    tracing::info!("App mention command: '{}' in {}", command, channel);

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

        // その他: claude -p に投げる
        other if !other.is_empty() => {
            slack
                .reply_thread(channel, thread_ts, ":brain: 考え中...")
                .await?;

            let log_dir = log_dir_from_state(state);
            match ClaudeRunner::new("mention", other)
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
    // bot 自身のメッセージを無視（無限ループ防止）
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

    // ops チャンネルのメッセージ処理（トップレベル or スレッド返信）
    if let Some(repo_entry) = state.repos_config.find_repo_by_ops_channel(channel) {
        // ops_tools (tool ベース) を優先、なければ ops_skills (skill ベース)
        let use_tools = repo_entry
            .ops_tools
            .as_ref()
            .map(|t| !t.is_empty())
            .unwrap_or(false);

        let ops_skills = if use_tools {
            // tool ベースの場合、ops_skills チェックをスキップ
            Vec::new()
        } else {
            match &repo_entry.ops_skills {
                Some(s) if !s.is_empty() => s.clone(),
                _ => {
                    if thread_ts.is_none() {
                        return Ok(());
                    }
                    Vec::new()
                }
            }
        };

        // tool ベースでも skill ベースでもない場合、スレッド返信以外は無視
        if !use_tools && ops_skills.is_empty() && thread_ts.is_none() {
            return Ok(());
        }

        // スレッド返信の場合: 既存の ops コンテキストがあるかチェック
        let effective_thread_ts = thread_ts.unwrap_or(message_ts);
        let is_followup = thread_ts.is_some()
            && state.db.get_ops_repo_key(channel, effective_thread_ts)?
                .is_some();

        // トップレベル or 既存 ops スレッドへの返信
        if thread_ts.is_none() || is_followup {
            if ops_skills.is_empty() && !is_followup {
                // ops_skills 未設定でトップレベル → 無視
            } else {
                let slack = state.slack_client();
                slack.reply_thread(channel, effective_thread_ts, ":gear: 処理中...").await.ok();

                let files = extract_slack_files(event);
                let repo_path = state.repos_config.repo_local_path(repo_entry);
                let repo_key = repo_entry.key.clone();
                let soul = crate::worker::context::read_soul(&state.repos_config.defaults.repos_base_dir);
                let max_turns = state.repos_config.defaults.claude_max_execute_turns;

                // 会話履歴を読み込み
                let history = state.db.get_ops_context(channel, effective_thread_ts)?;

                let req = crate::worker::ops::OpsRequest {
                    message_text: text.to_string(),
                    files,
                };

                let db = state.db.clone();
                let channel = channel.to_string();
                let thread_ts_owned = effective_thread_ts.to_string();
                let log_dir = log_dir_from_state(state);
                let runner_ctx = state.runner_ctx.clone();
                let ops_tools = repo_entry.ops_tools.clone();

                tokio::spawn(async move {
                    // ファイルダウンロード
                    if !req.files.is_empty() {
                        let images_dir = repo_path.join("images");
                        for f in &req.files {
                            let dest = images_dir.join(&f.name);
                            if let Err(e) = slack.download_file(&f.url_private_download, &dest).await {
                                tracing::warn!("Failed to download file {}: {}", f.name, e);
                            }
                        }
                    }

                    // ユーザーメッセージを保存
                    if let Err(e) = db.append_ops_context(&channel, &thread_ts_owned, &repo_key, "user", &req.message_text) {
                        tracing::warn!("Failed to save ops context (user): {}", e);
                    }

                    // ops_tools (tool ベース) と ops_skills (skill ベース) を分岐
                    let use_tools = ops_tools.as_ref().map(|t| !t.is_empty()).unwrap_or(false);

                    let exec_result = if use_tools {
                        let tools = ops_tools.as_ref().unwrap();
                        crate::worker::ops::execute_ops_with_tools(
                            &req, &repo_path, tools, &soul,
                            Some(&log_dir), &runner_ctx, &history,
                        ).await
                    } else {
                        crate::worker::ops::execute_ops(
                            &req, &repo_path, &ops_skills, &soul,
                            max_turns, Some(&log_dir), &runner_ctx, &history,
                        ).await
                    };

                    match exec_result {
                        Ok(output) => {
                            // アシスタント応答を保存
                            if let Err(e) = db.append_ops_context(&channel, &thread_ts_owned, &repo_key, "assistant", &output) {
                                tracing::warn!("Failed to save ops context (assistant): {}", e);
                            }

                            let truncated = crate::worker::runner::truncate_for_slack(&output, 3700);
                            let suffix = if truncated.len() < output.len() { "\n... (truncated)" } else { "" };
                            let msg = format!(":white_check_mark: 完了\n```\n{}{}\n```", truncated, suffix);
                            slack.reply_thread(&channel, &thread_ts_owned, &msg).await.ok();
                        }
                        Err(e) => {
                            let msg = format!(":x: 失敗: {}", e);
                            slack.reply_thread(&channel, &thread_ts_owned, &msg).await.ok();
                        }
                    }
                });

                return Ok(());
            }
        }
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

    // thread_ts でタスクを検索
    let task = match state.db.find_task_by_thread_ts(channel, thread_ts)? {
        Some(t) => t,
        None => return Ok(()), // タスクスレッドでなければ無視
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
// Helpers
// ============================================================================

/// Slack イベントの files 配列から SlackFile を抽出
fn extract_slack_files(event: &serde_json::Value) -> Vec<crate::worker::ops::SlackFile> {
    event
        .get("files")
        .and_then(|f| f.as_array())
        .map(|files| {
            files
                .iter()
                .filter_map(|f| {
                    let name = f.get("name")?.as_str()?.to_string();
                    let url = f.get("url_private_download")?.as_str()?.to_string();
                    Some(crate::worker::ops::SlackFile {
                        name,
                        url_private_download: url,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// ログ出力先ディレクトリを AppState から計算
fn log_dir_from_state(state: &Arc<AppState>) -> PathBuf {
    PathBuf::from(&state.repos_config.defaults.repos_base_dir)
        .join(".agent")
        .join("logs")
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
