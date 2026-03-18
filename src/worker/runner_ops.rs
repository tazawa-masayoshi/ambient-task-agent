//! Worker の ops キュー処理メソッド群（impl 分散）

use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::db::OpsQueueItem;
use crate::repo_config::RepoEntry;

use super::{context, ops::OpsExecMode, runner::{Worker, count_business_days, extract_slack_summary, ERROR_LOG_HINT}};

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
        let repo_entry = match self.resolve_ops_repo_entry(&item, max_retries).await? {
            Some(entry) => entry,
            None => return Ok(()),
        };

        let reply_ts = item.thread_ts.as_deref().unwrap_or(&item.message_ts);
        let slack = self.slack.clone();
        slack.reply_thread(&item.channel, reply_ts, ":gear: 処理中...").await.ok();

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

                self.post_ops_result(&item, &output, exec_result.exec_mode, reply_ts, &admin_mention).await?;
            }
            Err(e) => {
                let err_str = e.to_string();
                if item.retry_count + 1 >= max_retries {
                    let detail = format!(":x: *ops 失敗*（リトライ上限到達）\n```\n{}\n```{}", err_str, ERROR_LOG_HINT);
                    slack.reply_thread(&item.channel, reply_ts, &detail).await.ok();
                    self.db.mark_ops_failed(item.id, &err_str)?;
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
    ) -> Result<Option<RepoEntry>> {
        // 1. repo_key で直接マッチ（DM 等チャンネルに紐づかないケース）
        if let Some(direct) = self.repos_config.find_repo_by_key(&item.repo_key) {
            tracing::info!("ops item {} key-matched to scope: {} ({})",
                item.id, direct.key,
                direct.ops_description.as_deref().unwrap_or("no description"));
            return Ok(Some(direct.clone()));
        }
        // 2. コンテンツベースルーティング（LLM が内容からスコープを判定）
        match self.route_ops(item).await {
            Ok(Some(idx)) => {
                let entry = self.repos_config.repo[idx].clone();
                tracing::info!("ops item {} routed to scope: {} ({})",
                    item.id, entry.key,
                    entry.ops_description.as_deref().unwrap_or("no description"));
                Ok(Some(entry))
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
        let files = super::ops::extract_slack_files_from_json(&event);
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
        let history = self.db.get_ops_context(&item.channel, reply_ts)?;

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
        let output = super::ops::execute_ops(
            &req, &repo_path, &ops_skills, &soul,
            max_turns, Some(&log_dir), &self.runner_ctx, &history, dl_dir_ref,
            exec_mode,
        ).await;

        Ok(OpsExecutionResult { output, exec_mode })
    }

    /// 実行成功時の Slack 投稿: Inception Turn1/Turn2 / Execute / Plan。
    async fn post_ops_result(
        self: &Arc<Self>,
        item: &OpsQueueItem,
        output: &str,
        exec_mode: OpsExecMode,
        reply_ts: &str,
        admin_mention: &str,
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
        let is_no_action = output.contains("対応不要")
            || output.contains("作業対象外")
            || output.contains("スコープ外");
        let emoji = if is_no_action {
            ":information_source:"
        } else if is_plan_only {
            ":memo:"
        } else {
            ":white_check_mark:"
        };
        let label = if is_no_action {
            "対応不要"
        } else if is_plan_only {
            "分析完了"
        } else {
            "ops 完了"
        };
        // 作業結果まとめセクションがあればそこだけ抽出
        let slack_output = extract_slack_summary(output);
        let truncated = crate::claude::truncate_str(slack_output, 2800);
        // 対応不要はボタンなしで即解決、それ以外は完了/タスク化ボタン付き
        if is_no_action {
            let msg = format!("{} *{}*{}\n```\n{}\n```", emoji, label, admin_mention, truncated);
            slack.reply_thread(&item.channel, reply_ts, &msg).await.ok();
            self.db.resolve_ops(item.id).ok();
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

    /// ops メッセージをルーティング（コンテンツベースで最適なopsスコープを選択）
    async fn route_ops(&self, item: &OpsQueueItem) -> Result<Option<usize>> {
        if item.message_text.trim().len() < 5 {
            tracing::debug!("route_ops: message too short, skipping");
            return Ok(None);
        }

        let ops_entries = self.repos_config.get_all_ops_entries();
        if ops_entries.is_empty() {
            tracing::warn!("route_ops: no ops entries found in config");
            return Ok(None);
        }

        // スコープが1つしかない場合は分類不要
        if ops_entries.len() == 1 {
            tracing::info!("route_ops: single scope, auto-selecting: {}", ops_entries[0].1.key);
            return Ok(Some(ops_entries[0].0));
        }

        let scopes: Vec<String> = ops_entries
            .iter()
            .enumerate()
            .map(|(i, (_, entry))| {
                let desc = entry
                    .ops_description
                    .as_deref()
                    .unwrap_or(&entry.key);
                format!("{}. {}", i + 1, desc)
            })
            .collect();

        tracing::info!(
            "route_ops: classifying item {} across {} scopes: [{}]",
            item.id,
            ops_entries.len(),
            scopes.join(", ")
        );

        let prompt = format!(
            "以下のSlackメッセージがどの作業スコープに該当するか判定してください。\n\n\
             ## 作業スコープ一覧\n{}\n\n\
             ## メッセージ\n{}\n\n\
             該当するスコープの番号を scope フィールドに返してください。どれにも該当しない場合は 0 にしてください。",
            scopes.join("\n"),
            item.message_text
        );

        let schema = r#"{"type":"object","properties":{"scope":{"type":"integer"}},"required":["scope"]}"#;

        let log_dir = self.log_dir();
        let result = crate::claude::ClaudeRunner::new("route", &prompt)
            .max_turns(1)
            .allowed_tools("")
            .json_schema(schema)
            .log_dir(&log_dir)
            .with_context(&self.runner_ctx)
            .run()
            .await?;

        if !result.success {
            anyhow::bail!("route claude -p failed: {}", result.stderr);
        }

        let answer = result.stdout.trim();
        tracing::info!("route_ops: Claude answer='{}' for item {}", answer, item.id);

        let num: usize = serde_json::from_str::<serde_json::Value>(answer)
            .ok()
            .and_then(|v| v.get("scope")?.as_u64())
            .unwrap_or(0) as usize;

        if num == 0 || num > ops_entries.len() {
            tracing::info!("route_ops: no match (answer='{}', parsed={})", answer, num);
            return Ok(None);
        }

        let selected = &ops_entries[num - 1].1;
        tracing::info!(
            "route_ops: selected scope {} '{}' for item {}",
            num,
            selected.ops_description.as_deref().unwrap_or(&selected.key),
            item.id
        );

        Ok(Some(ops_entries[num - 1].0))
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
