//! Worker の ops キュー処理メソッド群（impl 分散）

use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::db::OpsQueueItem;
use crate::repo_config::RepoEntry;

use super::{context, ops::OpsExecMode, runner::{Worker, count_business_days, error_log_hint_for, extract_slack_summary}};

/// `prepare_ops_execution` の戻り値。実行結果 + メタ情報を保持する。
struct OpsExecutionResult {
    output: Result<String, anyhow::Error>,
    exec_mode: OpsExecMode,
}

impl Worker {
    /// ops キューを処理。spawn して即戻る。DB エラー時に true を返す。
    pub(crate) fn process_ops_queue(self: &Arc<Self>) -> bool {
        const MAX_OPS_RETRIES: i64 = 5;

        // 長時間 processing のままのアイテムをリカバリ（running_ops 内は除外）
        {
            let active = self.running_ops.lock().unwrap();
            match self.db.recover_stale_ops(&active) {
                Ok(n) if n > 0 => tracing::warn!("Recovered {} stale ops_queue items", n),
                Err(e) => tracing::warn!("Failed to recover stale ops: {}", e),
                _ => {}
            }
        }

        match self.db.dequeue_ops_item() {
            Ok(Some(item)) => {
                tracing::info!(
                    "Processing ops queue item {} (status={}, channel={}, retry={})",
                    item.id, item.status, item.channel, item.retry_count
                );
                // running_ops に登録（Drop ガードで自動除去）
                self.running_ops.lock().unwrap().insert(item.id);
                let w = Arc::clone(self);
                let ops_id = item.id;
                tokio::spawn(async move {
                    let _guard = RunningOpsGuard {
                        set: Arc::clone(&w.running_ops),
                        ops_id,
                    };
                    if let Err(e) = w.run_ops_item(item, MAX_OPS_RETRIES).await {
                        tracing::error!("ops queue item failed: {}", e);
                    }
                });
                false
            }
            Ok(None) => false,
            Err(e) => {
                tracing::error!("Failed to dequeue ops item: {}", e);
                true
            }
        }
    }

    /// ops キューアイテムを実行
    ///
    /// - pending: classify → actionable なら実行、そうでなければ skipped
    /// - ready: 分類スキップで即実行（⚡手動トリガー、スレッド返信、@メンション）
    async fn run_ops_item(self: &Arc<Self>, item: OpsQueueItem, max_retries: i64) -> Result<()> {
        let route_result = match self.resolve_ops_repo_entry(&item, max_retries).await? {
            Some(r) => r,
            None => return Ok(()),
        };
        let repo_entry = route_result.repo_entry;

        let reply_ts = item.thread_ts.as_deref().unwrap_or(&item.message_ts);
        let slack = self.slack.clone();
        // :gear: の ts を保持して、no_action 時に update でクリーンアップできるようにする
        let processing_msg_ts = slack
            .reply_thread(&item.channel, reply_ts, ":gear: 処理中...")
            .await
            .ok();

        let exec_result = self.prepare_ops_execution(&item, &repo_entry).await?;

        let admin_mention = self.repos_config.defaults.ops_admin_user
            .as_deref()
            .map(|uid| format!(" <@{}>", uid))
            .unwrap_or_default();

        match exec_result.output {
            Ok(raw_output) => {
                let output = if raw_output.trim().is_empty() {
                    tracing::warn!("ops item {}: Claude returned empty output after resume retry", item.id);
                    ":warning: 作業を実行しましたが、結果の要約を取得できませんでした。ログを確認してください。".to_string()
                } else {
                    raw_output
                };
                if let Err(e) = self.db.append_ops_context(&item.channel, reply_ts, &item.repo_key, "assistant", &output) {
                    tracing::warn!("Failed to save ops context (assistant): {}", e);
                }

                self.post_ops_result(
                    &item,
                    &output,
                    exec_result.exec_mode,
                    reply_ts,
                    &admin_mention,
                    processing_msg_ts.as_deref(),
                ).await?;
            }
            Err(e) => {
                let err_str = e.to_string();
                if item.retry_count + 1 >= max_retries {
                    let detail = format!(
                        ":x: *ops 失敗*（リトライ上限到達）\n```\n{}\n```{}",
                        err_str,
                        error_log_hint_for(&err_str)
                    );
                    slack.reply_thread(&item.channel, reply_ts, &detail).await.ok();
                    self.db.mark_ops_failed(item.id, &err_str)?;
                    self.db.set_ops_outcome(item.id, "error").ok();
                } else {
                    tracing::warn!("ops execution failed for item {} (retry {}): {}", item.id, item.retry_count, err_str);
                    self.db.mark_ops_retry(item.id, &err_str)?;
                }
            }
        }

        Ok(())
    }

    /// ops アイテムのルーティング: key → content-based の2段階で RepoEntry を解決。
    /// `None` = non-actionable（skipped/failed 処理済み）。
    async fn resolve_ops_repo_entry(
        &self,
        item: &OpsQueueItem,
        max_retries: i64,
    ) -> Result<Option<super::content_router::RouteResult>> {
        let router = super::content_router::ContentRouter::new(
            &self.repos_config,
            &self.runner_ctx,
        );

        match router.route(item).await {
            Ok(Some(result)) => {
                tracing::info!(
                    "ops item {} routed to scope: {} ({}, {} MCP servers)",
                    item.id,
                    result.repo_entry.key,
                    result.repo_entry.ops_description.as_deref().unwrap_or("no description"),
                    result.mcp_configs.len(),
                );
                Ok(Some(result))
            }
            Ok(None) if item.status == "pending" => {
                tracing::debug!("ops item {} classified as non-actionable", item.id);
                self.db.mark_ops_skipped(item.id)?;
                Ok(None)
            }
            Ok(None) => {
                let err = format!("No matching ops scope for item {}", item.id);
                self.db.mark_ops_failed(item.id, &err)?;
                Ok(None)
            }
            Err(e) => {
                tracing::warn!("ops routing failed for item {}: {}", item.id, e);
                if item.retry_count + 1 >= max_retries {
                    self.db.mark_ops_failed(item.id, &e.to_string())?;
                } else {
                    self.db.mark_ops_retry(item.id, &e.to_string())?;
                }
                Ok(None)
            }
        }
    }

    /// ファイルダウンロード → 会話履歴保存 → OpsExecMode 判定 → execute_ops 実行。
    async fn prepare_ops_execution(
        self: &Arc<Self>,
        item: &OpsQueueItem,
        repo_entry: &RepoEntry,
    ) -> Result<OpsExecutionResult> {
        let event: serde_json::Value =
            serde_json::from_str(&item.event_json).unwrap_or_default();

        let reply_ts = item.thread_ts.as_deref().unwrap_or(&item.message_ts);
        let slack = self.slack.clone();

        // ファイルダウンロード
        let mut files = super::ops::extract_slack_files_from_json(&event);
        let repo_path = self.repos_config.repo_local_path(repo_entry);
        if !files.is_empty() {
            if let Some(ref dl_dir) = repo_entry.ops_download_dir {
                let download_dir = repo_path.join(dl_dir);
                for f in &files {
                    let safe_name = std::path::Path::new(&f.name)
                        .file_name()
                        .unwrap_or_else(|| std::ffi::OsStr::new("download"));
                    let dest = download_dir.join(safe_name);
                    if let Err(e) = slack.download_file(&f.url_private_download, &dest).await {
                        tracing::warn!("Failed to download file {}: {}", f.name, e);
                    }
                }
            }
        }

        let message_text = crate::server::slack_events::extract_command(&item.message_text).to_string();

        // 会話履歴を保存 & 取得（スレッドの ts で管理）
        if let Err(e) = self.db.append_ops_context(&item.channel, reply_ts, &item.repo_key, "user", &message_text) {
            tracing::warn!("Failed to save ops context (user): {}", e);
        }
        let mut history = self.db.get_ops_context(&item.channel, reply_ts)?;

        // Slack スレッドの全メッセージを取得してコンテキストに追加
        if let Some(thread_ts) = item.thread_ts.as_deref() {
            match slack.fetch_thread_replies(&item.channel, thread_ts).await {
                Ok(replies) => {
                    let thread_messages: Vec<crate::db::OpsMessage> = replies
                        .iter()
                        .filter_map(|msg| {
                            let text = msg.get("text").and_then(|t| t.as_str()).unwrap_or("");
                            if text.is_empty() {
                                return None;
                            }
                            let ts = msg.get("ts").and_then(|t| t.as_str()).unwrap_or("");
                            let is_bot = msg.get("bot_id").is_some()
                                || msg.get("subtype").and_then(|s| s.as_str()) == Some("bot_message");
                            let role = if is_bot { "assistant" } else { "user" };
                            Some(crate::db::OpsMessage {
                                role: role.to_string(),
                                content: text.to_string(),
                                created_at: ts.to_string(),
                            })
                        })
                        .collect();
                    if !thread_messages.is_empty() {
                        tracing::info!(
                            "ops item {}: loaded {} Slack thread messages as context",
                            item.id,
                            thread_messages.len()
                        );
                        let mut merged = thread_messages;
                        merged.extend(history);
                        history = merged;
                    }

                    // スレッド内の全メッセージから添付ファイルをダウンロード
                    if let Some(ref dl_dir) = repo_entry.ops_download_dir {
                        let download_dir = repo_path.join(dl_dir);
                        for msg in &replies {
                            let thread_files = super::ops::extract_slack_files_from_json(msg);
                            for f in &thread_files {
                                let safe_name = std::path::Path::new(&f.name)
                                    .file_name()
                                    .unwrap_or_else(|| std::ffi::OsStr::new("download"));
                                let dest = download_dir.join(safe_name);
                                if dest.exists() {
                                    continue; // 既にダウンロード済み
                                }
                                match slack.download_file(&f.url_private_download, &dest).await {
                                    Ok(()) => {
                                        tracing::info!("Downloaded thread file: {} → {}", f.name, dest.display());
                                        files.push(f.clone());
                                    }
                                    Err(e) => {
                                        tracing::warn!("Failed to download thread file {}: {}", f.name, e);
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to fetch Slack thread replies for ops {}: {}", item.id, e);
                }
            }
        }

        let ops_skills = repo_entry.ops_skills.clone().unwrap_or_default();
        let ops_download_dir = repo_entry.ops_download_dir.clone();
        let soul = context::read_soul(&self.repos_config.defaults.repos_base_dir);
        let max_turns = self.repos_config.defaults.claude_max_execute_turns;
        let log_dir = self.log_dir();

        let req = super::ops::OpsRequest {
            message_text,
            files,
        };

        // OpsMode → OpsExecMode に変換（Inception は2ターン固定設計）
        // ターン判定: assistant の応答履歴の有無で Turn1/Turn2 を決定。
        // 3ターン以上の返信が来ても常に Turn2 として処理される（設計上の上限）。
        // 注意: append_ops_context("user") 後に get_ops_context を呼んでいるため
        // history には既に今回の user メッセージが含まれている。
        let exec_mode = match repo_entry.ops_mode {
            crate::repo_config::OpsMode::Plan => OpsExecMode::PlanOnly,
            crate::repo_config::OpsMode::Inception => {
                if history.iter().any(|m| m.role == "assistant") {
                    OpsExecMode::InceptionTurn2
                } else {
                    OpsExecMode::InceptionTurn1
                }
            }
            crate::repo_config::OpsMode::Execute => OpsExecMode::Execute,
        };

        let dl_dir_ref = ops_download_dir.as_deref();

        // 失敗パターン注入: 3件以上あればシステムプロンプト末尾に追加
        let failure_context = {
            let patterns = self.db.get_active_failure_patterns(&item.repo_key, 5)
                .unwrap_or_default();
            if patterns.len() >= 3 {
                let mut section = String::from(
                    "\n\n## 過去の失敗パターン（参考情報）\n\
                     以下は同リポジトリで過去に失敗した際のサマリです。同じミスを避けてください。\n"
                );
                for p in &patterns {
                    let date = if p.created_at.len() >= 10 { &p.created_at[..10] } else { &p.created_at };
                    // サニタイズ: 改行除去 + 200文字制限（プロンプト注入防止）
                    let sanitized: String = p.failure_summary
                        .chars()
                        .filter(|c| *c != '\n' && *c != '\r')
                        .take(200)
                        .collect();
                    section.push_str(&format!("\n- [{}] {}", date, sanitized));
                }
                Some(section)
            } else {
                None
            }
        };

        // 提案承認済みフラグ: 前回 proposal → 承認 → 再キューされた場合、
        // 「承認済みなので実行せよ」という追加指示を注入する
        let is_proposal_approved = item.error_message.as_deref() == Some("proposal approved");
        let proposal_override = if is_proposal_approved {
            Some("\n\n## 提案承認済み\n前回この依頼に対して提案を出し、管理者が承認しました。\
                  今回は提案ではなく **実際に実行** してください。`OPS_RESULT: completed` で完了すること。")
        } else {
            None
        };

        // failure_context と proposal_override を結合
        let combined_context = match (&failure_context, &proposal_override) {
            (Some(f), Some(p)) => Some(format!("{}{}", f, p)),
            (Some(f), None) => Some(f.clone()),
            (None, Some(p)) => Some(p.to_string()),
            (None, None) => None,
        };

        // MCP サーバー設定の動的構築
        let mcp_configs = crate::anthropic::mcp_config::build_mcp_configs(repo_entry, &repo_path);

        // prompt_evolution で承認済みの prompt があれば override として渡す。
        // 未承認なら None で、従来の soul + skills + rules 構築が使われる。
        let approved_prompt = self
            .db
            .get_approved_prompt_for_repo(&item.repo_key)
            .ok()
            .flatten();

        let output = super::ops::execute_ops(
            &req, &repo_path, &ops_skills, &soul,
            max_turns, Some(&log_dir), &self.runner_ctx, &history, dl_dir_ref,
            exec_mode, None,
            combined_context.as_deref(),
            mcp_configs,
            approved_prompt.as_deref(),
        ).await;

        Ok(OpsExecutionResult { output, exec_mode })
    }

    /// 実行成功時の Slack 投稿: Inception Turn1/Turn2 / Execute / Plan。
    ///
    /// `processing_msg_ts`: `:gear: 処理中...` メッセージの ts。no_action 時に
    /// このメッセージを update で上書きして、別途 reply を投稿しない形にする。
    async fn post_ops_result(
        self: &Arc<Self>,
        item: &OpsQueueItem,
        output: &str,
        exec_mode: OpsExecMode,
        reply_ts: &str,
        admin_mention: &str,
        processing_msg_ts: Option<&str>,
    ) -> Result<()> {
        let slack = self.slack.clone();

        // Inception ターン1: 質問を投稿してユーザー返信待ち
        if exec_mode == OpsExecMode::InceptionTurn1 {
            let truncated = crate::claude::truncate_str(output, 2800);
            let msg = format!(":bulb: *要件ヒアリング*{}\n{}", admin_mention, truncated);
            slack.reply_thread(&item.channel, reply_ts, &msg).await.ok();
            self.db.mark_ops_done(item.id)?;
            tracing::info!("inception turn1 done for ops item {}, waiting for user reply", item.id);
            return Ok(());
        }

        // Inception ターン2: 要件整理 + タスク分解完了 → 承認ゲートボタン
        if exec_mode == OpsExecMode::InceptionTurn2 {
            let truncated = crate::claude::truncate_str(output, 2800);
            let blocks = serde_json::json!([
                {
                    "type": "section",
                    "text": {
                        "type": "mrkdwn",
                        "text": format!(":memo: *要件定義完了*{}\n```\n{}\n```", admin_mention, truncated)
                    }
                },
                {
                    "type": "actions",
                    "elements": [
                        {
                            "type": "button",
                            "text": { "type": "plain_text", "text": "\u{2705} 承認（自動実行）" },
                            "style": "primary",
                            "action_id": "ops_inception_approve",
                            "value": item.id.to_string()
                        },
                        {
                            "type": "button",
                            "text": { "type": "plain_text", "text": "\u{1f4cb} Asana登録のみ" },
                            "action_id": "ops_inception_asana",
                            "value": item.id.to_string()
                        },
                        {
                            "type": "button",
                            "text": { "type": "plain_text", "text": "\u{1f527} 修正して" },
                            "action_id": "ops_inception_revise",
                            "value": item.id.to_string()
                        },
                        {
                            "type": "button",
                            "text": { "type": "plain_text", "text": "\u{274c} キャンセル" },
                            "style": "danger",
                            "action_id": "ops_inception_cancel",
                            "value": item.id.to_string()
                        }
                    ]
                }
            ]);
            let fallback = format!(":memo: *要件定義完了*{}\n{}", admin_mention, truncated);
            match slack.post_blocks(&item.channel, reply_ts, &blocks, &fallback).await {
                Ok(ts) => {
                    self.db.set_ops_notify_ts(item.id, &ts).ok();
                }
                Err(e) => {
                    tracing::warn!("Failed to post inception blocks: {}", e);
                    slack.reply_thread(&item.channel, reply_ts, &fallback).await.ok();
                }
            }
            self.db.mark_ops_done(item.id)?;
            tracing::info!("inception turn2 done for ops item {}, awaiting approval", item.id);
            return Ok(());
        }

        // 通常モード（Execute / Plan）
        let is_plan_only = exec_mode == OpsExecMode::PlanOnly;
        // OPS_RESULT マーカーで判定: 最終非空行のみ検査（本文中の誤検知を防止）。
        // フォールバック: マーカーがない場合のみ、先頭200文字のキーワード検索。
        let last_line = output.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
        let is_proposal = last_line.contains("OPS_RESULT: proposal");
        let is_no_action = if is_proposal {
            false
        } else if last_line.contains("OPS_RESULT: no_action") {
            true
        } else if last_line.contains("OPS_RESULT: completed") || last_line.contains("OPS_RESULT: failed") {
            false
        } else {
            // レガシー: マーカーなし → 先頭部分のみでキーワード検索（誤検知防止）
            let head: String = output.chars().take(200).collect();
            head.contains("対応不要")
                || head.contains("作業対象外")
                || head.contains("スコープ外")
        };
        let emoji = if is_no_action {
            ":information_source:"
        } else if is_proposal {
            ":bulb:"
        } else if is_plan_only {
            ":memo:"
        } else {
            ":white_check_mark:"
        };
        let label = if is_no_action {
            "対応不要"
        } else if is_proposal {
            "提案"
        } else if is_plan_only {
            "分析完了"
        } else {
            "ops 完了"
        };
        // 作業結果まとめセクションがあればそこだけ抽出
        let slack_output = extract_slack_summary(output);
        let truncated = crate::claude::truncate_str(slack_output, 2800);
        // outcome を記録（self_improvement 分析用）
        let is_failed = last_line.contains("OPS_RESULT: failed");
        let outcome = if is_no_action {
            "no_action"
        } else if is_proposal {
            "proposal"
        } else if is_failed {
            "failed"
        } else {
            "completed"
        };
        self.db.set_ops_outcome(item.id, outcome).ok();

        // 失敗パターンを DB に保存（次回実行時のプロンプト注入用）
        if is_failed {
            let summary: String = output.chars().rev().take(500).collect::<Vec<_>>()
                .into_iter().rev().collect();
            if let Err(e) = self.db.insert_ops_failure_pattern(
                &item.repo_key,
                "[]", // skill_paths は post_ops_result からは不明なので空配列
                &summary,
            ) {
                tracing::warn!("Failed to save failure pattern: {}", e);
            }
        }

        // 対応不要は :gear: 処理中... メッセージを削除して完全 silent にする。
        // 会話締め (「対応済みです」「ありがとう」等) で bot が発火しても、
        // Slack UI には一切痕跡を残さない。ログは journalctl に残る。
        //
        // 権限エラー等で delete が失敗した場合は update_text で "対応不要" 1 行に
        // 上書きしてフォールバック（静音性を維持）。
        if is_no_action {
            if let Some(ts) = processing_msg_ts {
                if let Err(e) = slack.delete_message(&item.channel, ts).await {
                    tracing::warn!(
                        "Failed to delete processing message for no_action ({}), falling back to update",
                        e
                    );
                    let brief = format!(
                        "{} *対応不要*{}（スレッドの会話に作業対象なし）",
                        emoji, admin_mention
                    );
                    slack.update_text(&item.channel, ts, &brief).await.ok();
                }
            }
            self.db.resolve_ops(item.id).ok();
        } else if is_proposal {
            // 提案モード: 実行せず提案のみ → 承認ボタンで実行可能
            let blocks = serde_json::json!([
                {
                    "type": "section",
                    "text": {
                        "type": "mrkdwn",
                        "text": format!("{} *{}*{}\n```\n{}\n```", emoji, label, admin_mention, truncated)
                    }
                },
                {
                    "type": "actions",
                    "elements": [
                        {
                            "type": "button",
                            "text": { "type": "plain_text", "text": "\u{1f680} 承認して実行" },
                            "style": "primary",
                            "action_id": "ops_approve_proposal",
                            "value": item.id.to_string()
                        },
                        {
                            "type": "button",
                            "text": { "type": "plain_text", "text": "\u{1f4cb} タスク化" },
                            "action_id": "ops_escalate",
                            "value": item.id.to_string()
                        },
                        {
                            "type": "button",
                            "text": { "type": "plain_text", "text": "\u{274c} 却下" },
                            "action_id": "ops_resolve",
                            "value": item.id.to_string()
                        }
                    ]
                }
            ]);
            let fallback = format!("{} *{}*{}\n{}", emoji, label, admin_mention, truncated);
            match slack.post_blocks(&item.channel, reply_ts, &blocks, &fallback).await {
                Ok(ts) => {
                    self.db.set_ops_notify_ts(item.id, &ts).ok();
                }
                Err(e) => {
                    tracing::warn!("Failed to post ops blocks: {}", e);
                    slack.reply_thread(&item.channel, reply_ts, &fallback).await.ok();
                }
            }
        } else {
            let blocks = serde_json::json!([
                {
                    "type": "section",
                    "text": {
                        "type": "mrkdwn",
                        "text": format!("{} *{}*{}\n```\n{}\n```", emoji, label, admin_mention, truncated)
                    }
                },
                {
                    "type": "actions",
                    "elements": [
                        {
                            "type": "button",
                            "text": { "type": "plain_text", "text": "\u{2705} 完了" },
                            "style": "primary",
                            "action_id": "ops_resolve",
                            "value": item.id.to_string()
                        },
                        {
                            "type": "button",
                            "text": { "type": "plain_text", "text": "\u{1f4cb} タスク化" },
                            "action_id": "ops_escalate",
                            "value": item.id.to_string()
                        }
                    ]
                }
            ]);
            let fallback = format!("{} *{}*{}\n{}", emoji, label, admin_mention, truncated);
            match slack.post_blocks(&item.channel, reply_ts, &blocks, &fallback).await {
                Ok(ts) => {
                    self.db.set_ops_notify_ts(item.id, &ts).ok();
                }
                Err(e) => {
                    tracing::warn!("Failed to post ops blocks: {}", e);
                    slack.reply_thread(&item.channel, reply_ts, &fallback).await.ok();
                }
            }
        }
        self.db.mark_ops_done(item.id)?;

        Ok(())
    }

    /// ops フォローアップチェック: 未解決アイテムにリマインドを送信
    pub(crate) async fn check_ops_followups(self: &Arc<Self>) {
        let items = match self.db.get_ops_needing_followup() {
            Ok(items) => items,
            Err(e) => {
                tracing::warn!("Failed to get ops followups: {}", e);
                return;
            }
        };

        let now = chrono::Utc::now();
        let admin_mention = self.repos_config.defaults.ops_admin_user
            .as_deref()
            .map(|uid| format!("<@{}>", uid))
            .unwrap_or_default();

        for item in items {
            let done_at = match item.done_at.parse::<DateTime<Utc>>() {
                Ok(dt) => dt,
                Err(_) => continue,
            };
            let business_days = count_business_days(done_at, now);

            let should_remind = match item.reminder_count {
                0 => business_days >= 1,
                1 => business_days >= 3,
                2 => business_days >= 5,
                _ => false,
            };

            if !should_remind {
                continue;
            }

            let reply_ts = item.thread_ts.as_deref().unwrap_or(&item.message_ts);
            let slack = self.slack.clone();
            let short_text = crate::claude::truncate_str(&item.message_text, 80);

            if item.reminder_count >= 2 {
                let msg = format!(
                    ":file_folder: *保留に移行* {}\n営業日5日未対応のため保留にしました: _{}_",
                    admin_mention, short_text
                );
                slack.reply_thread(&item.channel, reply_ts, &msg).await.ok();
                self.db.mark_ops_on_hold(item.id).ok();
                tracing::info!("ops item {} moved to on_hold after {} business days", item.id, business_days);
            } else {
                let label = if item.reminder_count == 0 { "1営業日" } else { "3営業日" };
                let msg = format!(
                    ":bell: *リマインド* {}\n{}経過: _{}_",
                    admin_mention, label, short_text
                );
                let blocks = serde_json::json!([
                    {
                        "type": "section",
                        "text": {
                            "type": "mrkdwn",
                            "text": msg
                        }
                    },
                    {
                        "type": "actions",
                        "elements": [
                            {
                                "type": "button",
                                "text": { "type": "plain_text", "text": "\u{2705} 完了" },
                                "style": "primary",
                                "action_id": "ops_resolve",
                                "value": item.id.to_string()
                            },
                            {
                                "type": "button",
                                "text": { "type": "plain_text", "text": "\u{1f4cb} タスク化" },
                                "action_id": "ops_escalate",
                                "value": item.id.to_string()
                            }
                        ]
                    }
                ]);
                slack.post_blocks(&item.channel, reply_ts, &blocks, &msg).await.ok();
                self.db.increment_ops_reminder(item.id).ok();
                tracing::info!("ops item {} reminder {} sent ({}bd elapsed)", item.id, item.reminder_count + 1, business_days);
            }
        }
    }

    /// conversing 状態で営業日5日以上返信がないタスクを sleeping に遷移
    pub(crate) async fn timeout_stale_conversing_tasks(self: &Arc<Self>) {
        let stale_tasks = match self.db.get_stale_conversing_tasks(120) {
            Ok(tasks) => tasks,
            Err(e) => {
                tracing::warn!("Failed to get stale conversing tasks: {}", e);
                return;
            }
        };

        for task in stale_tasks {
            let channel = task.slack_channel.as_deref()
                .unwrap_or(&self.default_slack_channel);
            let thread_ts = task.slack_thread_ts.as_deref().unwrap_or("");

            self.db.update_status(task.id, "sleeping").ok();
            self.slack.reply_thread(channel, thread_ts,
                ":zzz: 5営業日以上返信がないため、タスクをスリープに移行しました。`wake` で再開できます。",
            ).await.ok();
            tracing::info!("Task {} conversing timeout → sleeping", task.id);
        }
    }

}

/// process_ops_queue の Drop ガード。panic 時も running_ops から ops_id を確実に除去する。
struct RunningOpsGuard {
    set: Arc<std::sync::Mutex<std::collections::HashSet<i64>>>,
    ops_id: i64,
}

impl Drop for RunningOpsGuard {
    fn drop(&mut self) {
        match self.set.lock() {
            Ok(mut set) => { set.remove(&self.ops_id); }
            Err(poisoned) => { poisoned.into_inner().remove(&self.ops_id); }
        }
    }
}
