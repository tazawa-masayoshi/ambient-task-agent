use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::{Datelike, DateTime, Timelike, Utc};
use tokio::sync::Notify;

use crate::db::{CodingTask, Db, OpsQueueItem};
use crate::google::calendar::GoogleCalendarClient;
use crate::repo_config::ReposConfig;
use crate::slack::client::SlackClient;

use super::{context, executor, priority, scheduler, task_file, workflow, workspace};

/// ハートビート間隔の下限
const MIN_HEARTBEAT_SECS: u64 = 10;

pub struct Worker {
    db: Db,
    repos_config: ReposConfig,
    slack: SlackClient,
    asana_pat: String,
    asana_project_id: String,
    asana_user_name: String,
    google_calendar: tokio::sync::Mutex<Option<GoogleCalendarClient>>,
    default_slack_channel: String,
    notify: Arc<Notify>,
    runner_ctx: crate::execution::RunnerContext,
}

impl Worker {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: Db,
        repos_config: ReposConfig,
        slack: SlackClient,
        asana_pat: String,
        asana_project_id: String,
        asana_user_name: String,
        google_calendar: Option<GoogleCalendarClient>,
        default_slack_channel: String,
        notify: Arc<Notify>,
        runner_ctx: crate::execution::RunnerContext,
    ) -> Self {
        Self {
            db,
            repos_config,
            slack,
            asana_pat,
            asana_project_id,
            asana_user_name,
            google_calendar: tokio::sync::Mutex::new(google_calendar),
            default_slack_channel,
            notify,
            runner_ctx,
        }
    }

    /// 実行ログの出力先ディレクトリ
    fn log_dir(&self) -> PathBuf {
        PathBuf::from(&self.repos_config.defaults.repos_base_dir)
            .join(".agent")
            .join("logs")
    }

    /// リポジトリパスを解決（共通ヘルパー）
    fn resolve_repo_path(&self, task: &CodingTask) -> Result<std::path::PathBuf> {
        match task.repo_key.as_deref() {
            Some(key) => match self.repos_config.find_repo_by_key(key) {
                Some(r) => Ok(self.repos_config.repo_local_path(r)),
                None => anyhow::bail!("Unknown repo key: {}", key),
            },
            None => anyhow::bail!("No repo_key assigned to task"),
        }
    }

    /// メインワーカーループ
    ///
    /// - ハートビート（15秒）: DB からタスクを取得して tokio::spawn で並列実行
    /// - イベント駆動: Notify で即時起床してタスク処理
    /// - 各タスクは spawn されるため、heartbeat ループはブロックしない
    pub async fn run(self) {
        let heartbeat_secs = std::cmp::max(
            self.repos_config.defaults.worker_heartbeat_secs,
            MIN_HEARTBEAT_SECS,
        );
        let heartbeat = Duration::from_secs(heartbeat_secs);
        tracing::info!("Worker started (heartbeat={}s)", heartbeat_secs);

        // スケジュールジョブを DB に seed
        if let Err(e) = scheduler::seed_schedules(&self.db, &self.repos_config) {
            tracing::error!("Failed to seed schedules: {}", e);
        }

        let worker = Arc::new(self);
        let mut consecutive_errors: u32 = 0;
        let mut last_followup_check: Option<DateTime<Utc>> = None;

        loop {
            let mut had_error = false;

            // タスク処理（個別に spawn、ループはブロックしない）
            had_error |= worker.process_tasks();

            // ops キュー処理（個別に spawn）
            had_error |= worker.process_ops_queue();

            // スケジューラージョブチェック（軽量なので直接 await）
            had_error |= worker.run_scheduler().await;

            // ops フォローアップチェック（1時間ごと、業務時間 9-18 JST のみ）
            let now = Utc::now();
            let jst_hour = (now.hour() + 9) % 24; // UTC → JST 簡易変換
            let jst_weekday = (now + chrono::Duration::hours(9)).weekday();
            let is_weekday = !matches!(jst_weekday, chrono::Weekday::Sat | chrono::Weekday::Sun);
            #[allow(clippy::manual_range_contains)]
            let should_check_followup = is_weekday
                && jst_hour >= 9 && jst_hour < 18
                && last_followup_check
                    .map(|last| (now - last).num_minutes() >= 60)
                    .unwrap_or(true);
            if should_check_followup {
                worker.check_ops_followups().await;
                worker.timeout_stale_conversing_tasks().await;
                last_followup_check = Some(now);
            }

            // エラー時バックオフ、通常時はハートビートまたは Notify 待ち
            if had_error {
                consecutive_errors = consecutive_errors.saturating_add(1);
                let backoff = std::cmp::min(5 * (1u64 << consecutive_errors), 120);
                if consecutive_errors >= 3 {
                    tracing::warn!(
                        "Worker: {} consecutive errors, backing off {}s",
                        consecutive_errors,
                        backoff
                    );
                }
                tokio::time::sleep(Duration::from_secs(backoff)).await;
            } else {
                consecutive_errors = 0;
                // Notify またはハートビートタイムアウトで起床
                tokio::select! {
                    _ = worker.notify.notified() => {
                        tracing::debug!("Worker woken by event");
                    }
                    _ = tokio::time::sleep(heartbeat) => {
                        tracing::trace!("Worker heartbeat");
                    }
                }
            }
        }
    }

    /// タスクキューを処理。各タスクを tokio::spawn で並列実行する。
    /// DB フェッチエラーがあれば true を返す。
    fn process_tasks(self: &Arc<Self>) -> bool {
        let mut had_error = false;

        // 1. new → classify (LLM + heuristics fallback) → executing / conversing
        match self.db.get_new_task() {
            Ok(Some(task)) => {
                tracing::info!("Classifying new task: {} ({})", task.asana_task_name, task.asana_task_gid);
                // heuristics で即判定してステータスを claim（二重 pickup 防止）
                let heuristic = classify_new_task_heuristic(&task, &self.repos_config);
                let initial_status = match heuristic {
                    TaskClassification::Execute => "executing",
                    TaskClassification::Converse => "conversing",
                };
                if self.db.update_status(task.id, initial_status).is_ok() {
                    let task_id = task.id;
                    self.spawn_task(task_id, |w| async move {
                        w.classify_and_dispatch(task).await
                    });
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::error!("Failed to fetch new task: {}", e);
                had_error = true;
            }
        }

        // 2. conversing タスクの継続処理（ユーザー返信があれば次の Claude ターンを実行）
        had_error |= self.process_conversing_tasks();

        // 2.5 (並存期間) approved / auto_approved → executing
        // v12 マイグレーション完了後に削除
        match self.db.get_approved_task() {
            Ok(Some(task)) => {
                tracing::info!("Executing approved task: {} ({})", task.asana_task_name, task.asana_task_gid);
                if self.db.update_status(task.id, "executing").is_ok() {
                    let task_id = task.id;
                    self.spawn_task(task_id, |w| async move { w.execute_approved_task(task).await });
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::error!("Failed to fetch approved task: {}", e);
                had_error = true;
            }
        }
        match self.db.get_auto_approved_task() {
            Ok(Some(task)) => {
                tracing::info!("Auto-executing task: {} ({})", task.asana_task_name, task.asana_task_gid);
                if self.db.update_status(task.id, "executing").is_ok() {
                    let task_id = task.id;
                    self.spawn_task(task_id, |w| async move { w.execute_approved_task(task).await });
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::error!("Failed to fetch auto_approved task: {}", e);
                had_error = true;
            }
        }

        // 3. 全アクティブタスクの優先度を再計算
        if let Ok(active_tasks) = self.db.get_active_tasks() {
            let now = chrono::Utc::now();
            for t in &active_tasks {
                let score = priority::calculate_priority_score(t, &now);
                if let Err(e) = self.db.update_priority_score(t.id, score) {
                    tracing::warn!("Failed to update priority for task {}: {}", t.id, e);
                }
            }
        }

        // 4. ci_pending タスク → CI 結果確認 → done or リトライ
        match self.db.get_ci_pending_task() {
            Ok(Some(task)) => {
                tracing::debug!("Checking CI for task: {} ({})", task.asana_task_name, task.id);
                let task_id = task.id;
                self.spawn_task(task_id, |w| async move { w.check_ci_and_handle(task).await });
            }
            Ok(None) => {}
            Err(e) => {
                tracing::error!("Failed to fetch ci_pending task: {}", e);
                had_error = true;
            }
        }

        // wez-sidebar タスクキャッシュ同期
        if let Some(ref cache_path) = self.repos_config.defaults.tasks_cache_file {
            if let Err(e) = task_file::sync_tasks_cache(&self.db, cache_path) {
                tracing::warn!("Failed to sync tasks cache: {}", e);
            }
        }

        had_error
    }

    /// ops キューを処理。spawn して即戻る。DB エラー時に true を返す。
    fn process_ops_queue(self: &Arc<Self>) -> bool {
        const MAX_OPS_RETRIES: i64 = 5;

        // 長時間 processing のままのアイテムをリカバリ
        match self.db.recover_stale_ops() {
            Ok(n) if n > 0 => tracing::warn!("Recovered {} stale ops_queue items", n),
            Err(e) => tracing::warn!("Failed to recover stale ops: {}", e),
            _ => {}
        }

        match self.db.dequeue_ops_item() {
            Ok(Some(item)) => {
                tracing::info!(
                    "Processing ops queue item {} (status={}, channel={}, retry={})",
                    item.id, item.status, item.channel, item.retry_count
                );
                if self.db.mark_ops_processing(item.id).is_ok() {
                    let w = Arc::clone(self);
                    tokio::spawn(async move {
                        if let Err(e) = w.run_ops_item(item, MAX_OPS_RETRIES).await {
                            tracing::error!("ops queue item failed: {}", e);
                        }
                    });
                }
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
        // チャンネルが ops_channel に紐づいている場合はそのエントリを直接使う（ルーティング不要）
        // 紐づいていない場合のみコンテンツベースルーティングにフォールバック
        let repo_entry = if let Some(direct) = self.repos_config.find_repo_by_ops_channel(&item.channel) {
            tracing::info!("ops item {} channel-matched to scope: {} ({})",
                item.id, direct.key,
                direct.ops_description.as_deref().unwrap_or("no description"));
            direct.clone()
        } else {
            match self.route_ops(&item).await {
            Ok(Some(idx)) => {
                let entry = self.repos_config.repo[idx].clone();
                tracing::info!("ops item {} routed to scope: {} ({})",
                    item.id, entry.key,
                    entry.ops_description.as_deref().unwrap_or("no description"));
                entry
            }
            Ok(None) if item.status == "pending" => {
                tracing::debug!("ops item {} classified as non-actionable", item.id);
                self.db.mark_ops_skipped(item.id)?;
                return Ok(());
            }
            Ok(None) => {
                let err = format!("No matching ops scope for item {}", item.id);
                self.db.mark_ops_failed(item.id, &err)?;
                return Ok(());
            }
            Err(e) => {
                tracing::warn!("ops routing failed for item {}: {}", item.id, e);
                if item.retry_count + 1 >= max_retries {
                    self.db.mark_ops_failed(item.id, &e.to_string())?;
                } else {
                    self.db.mark_ops_retry(item.id, &e.to_string())?;
                }
                return Ok(());
            }
        }
        };

        // 実行
        let event: serde_json::Value =
            serde_json::from_str(&item.event_json).unwrap_or_default();

        // スレッド返信先: thread_ts があればそちら、なければ message_ts 自体がスレッドの起点
        let reply_ts = item.thread_ts.as_deref().unwrap_or(&item.message_ts);

        let slack = self.slack.clone();
        slack.reply_thread(&item.channel, reply_ts, ":gear: 処理中...").await.ok();

        // ファイルダウンロード
        let files = super::ops::extract_slack_files_from_json(&event);
        let repo_path = self.repos_config.repo_local_path(&repo_entry);
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

        let repo_key = &item.repo_key;
        let message_text = crate::server::slack_events::extract_command(&item.message_text).to_string();

        // 会話履歴を保存 & 取得（スレッドの ts で管理）
        if let Err(e) = self.db.append_ops_context(&item.channel, reply_ts, repo_key, "user", &message_text) {
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
            crate::repo_config::OpsMode::Plan => super::ops::OpsExecMode::PlanOnly,
            crate::repo_config::OpsMode::Inception => {
                if history.iter().any(|m| m.role == "assistant") {
                    super::ops::OpsExecMode::InceptionTurn2
                } else {
                    super::ops::OpsExecMode::InceptionTurn1
                }
            }
            crate::repo_config::OpsMode::Execute => super::ops::OpsExecMode::Execute,
        };
        let is_plan_only = exec_mode == super::ops::OpsExecMode::PlanOnly;
        let is_inception_turn1 = exec_mode == super::ops::OpsExecMode::InceptionTurn1;
        let is_inception_turn2 = exec_mode == super::ops::OpsExecMode::InceptionTurn2;

        let dl_dir_ref = ops_download_dir.as_deref();
        let exec_result = super::ops::execute_ops(
            &req, &repo_path, &ops_skills, &soul,
            max_turns, Some(&log_dir), &self.runner_ctx, &history, dl_dir_ref,
            exec_mode,
        ).await;

        // admin ユーザーへのメンション（完了通知に含める）
        let admin_mention = self.repos_config.defaults.ops_admin_user
            .as_deref()
            .map(|uid| format!(" <@{}>", uid))
            .unwrap_or_default();

        match exec_result {
            Ok(raw_output) => {
                let output = if raw_output.trim().is_empty() {
                    tracing::warn!("ops item {}: Claude returned empty output, using fallback", item.id);
                    "（作業完了 — Claude からのテキスト出力なし。ツール操作のみ実行された可能性があります）".to_string()
                } else {
                    raw_output
                };
                if let Err(e) = self.db.append_ops_context(&item.channel, reply_ts, repo_key, "assistant", &output) {
                    tracing::warn!("Failed to save ops context (assistant): {}", e);
                }

                // Inception ターン1: 質問を投稿してユーザー返信待ち
                if is_inception_turn1 {
                    let truncated = crate::claude::truncate_str(&output, 2800);
                    let msg = format!(":bulb: *要件ヒアリング*{}\n{}", admin_mention, truncated);
                    slack.reply_thread(&item.channel, reply_ts, &msg).await.ok();
                    self.db.mark_ops_done(item.id)?;
                    tracing::info!("inception turn1 done for ops item {}, waiting for user reply", item.id);
                    return Ok(());
                }

                // Inception ターン2: 要件整理 + タスク分解完了 → 承認ゲートボタン
                if is_inception_turn2 {
                    let truncated = crate::claude::truncate_str(&output, 2800);
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
                let truncated = crate::claude::truncate_str(&output, 2800);
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

    /// ops フォローアップチェック: 未解決アイテムにリマインドを送信
    ///
    /// - 営業日1日後: 1回目リマインド
    /// - 営業日3日後: 2回目リマインド
    /// - 営業日5日後: 3回目リマインド + 保留に移行
    ///
    /// 土日はカウント対象外（営業日ベース）。
    async fn check_ops_followups(self: &Arc<Self>) {
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

            // リマインドのタイミング判定（営業日ベース）
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
                // 営業日5日後: 保留に移行
                let msg = format!(
                    ":file_folder: *保留に移行* {}\n営業日5日未対応のため保留にしました: _{}_",
                    admin_mention, short_text
                );
                slack.reply_thread(&item.channel, reply_ts, &msg).await.ok();
                self.db.mark_ops_on_hold(item.id).ok();
                tracing::info!("ops item {} moved to on_hold after {} business days", item.id, business_days);
            } else {
                // 営業日1日 / 3日後: リマインド
                let label = if item.reminder_count == 0 { "1営業日" } else { "3営業日" };
                let msg = format!(
                    ":bell: *リマインド* {}\n{}経過: _{}_",
                    admin_mention, label, short_text
                );
                slack.reply_thread(&item.channel, reply_ts, &msg).await.ok();
                self.db.increment_ops_reminder(item.id).ok();
                tracing::info!("ops item {} reminder {} sent ({}bd elapsed)", item.id, item.reminder_count + 1, business_days);
            }
        }
    }

    /// ops メッセージをルーティング（コンテンツベースで最適なopsスコープを選択）
    ///
    /// 全opsスコープの説明を提示し、Claude に最適なスコープを選ばせる。
    /// 該当なしの場合は None を返す。
    /// conversing 状態で営業日5日以上返信がないタスクを sleeping に遷移
    async fn timeout_stale_conversing_tasks(self: &Arc<Self>) {
        // 営業日5日 ≈ 暦日7日（+バッファ）として120時間で検索
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

        // ops_entries[num-1] の .0 がグローバル repo 配列のインデックス
        Ok(Some(ops_entries[num - 1].0))
    }

    /// スケジューラージョブを実行。エラーがあれば true を返す
    async fn run_scheduler(&self) -> bool {
        let base_dir = &self.repos_config.defaults.repos_base_dir;
        let gcal = self.google_calendar.lock().await.take();
        let mut ctx = scheduler::SchedulerContext {
            db: self.db.clone(),
            slack: self.slack.clone(),
            asana_pat: self.asana_pat.clone(),
            asana_project_id: self.asana_project_id.clone(),
            asana_user_name: self.asana_user_name.clone(),
            google_calendar: gcal,
            repos_base_dir: base_dir.clone(),
            stagnation_threshold_hours: self.repos_config.defaults.stagnation_threshold_hours,
            soul: context::read_soul(base_dir),
            skill: context::read_skill(base_dir),
            log_dir: self.log_dir(),
            runner_ctx: self.runner_ctx.clone(),
        };

        let had_error = if let Err(e) = scheduler::check_and_run(&mut ctx).await {
            tracing::error!("Scheduled job check failed: {}", e);
            true
        } else {
            false
        };

        *self.google_calendar.lock().await = ctx.google_calendar;
        had_error
    }

    /// approved/auto_approved/executing → ci_pending/done: タスクを実行
    ///
    /// session_id があれば --resume で Plan セッションを継続、なければフルプロンプトで実行。
    /// repo_entry があれば worktree 隔離実行、なければ直接実行。
    async fn execute_approved_task(&self, task: CodingTask) -> Result<()> {
        let repo_entry = task
            .repo_key
            .as_deref()
            .and_then(|key| self.repos_config.find_repo_by_key(key));

        // worktree 隔離実行（PR作成つき）
        if let Some(entry) = repo_entry {
            return self.execute_in_worktree(task, entry).await;
        }

        // フォールバック: worktree なし直接実行（status は spawn 前に "executing" に更新済み）
        let channel = task
            .slack_channel
            .as_deref()
            .unwrap_or(&self.default_slack_channel);
        let thread_ts = task.slack_thread_ts.as_deref().unwrap_or("");

        let exec_blocks = build_executing_blocks(task.id, ":rocket: 実行中...");
        self.slack
            .post_blocks(channel, thread_ts, &exec_blocks, "実行中...")
            .await
            .ok();

        let plan_text = task.analysis_text.as_deref().unwrap_or("");
        let base_dir = &self.repos_config.defaults.repos_base_dir;
        let (work_context, work_memory) = (
            context::merged_context(base_dir, None),
            context::merged_memory(base_dir, None),
        );

        let base_turns = self.repos_config.defaults.claude_max_execute_turns;
        let max_turns = match task.complexity.as_deref() {
            Some("complex") => base_turns.saturating_mul(2),
            _ => base_turns,
        };
        let wc = context::WorkContext {
            repo_path: std::path::PathBuf::from(base_dir),
            max_turns,
            soul: context::merged_soul(base_dir, None),
            skill: context::read_skill(base_dir),
            context: work_context,
            memory: work_memory,
        };

        let log_dir = self.log_dir();
        let session_id = task.claude_session_id.as_deref();
        let result = executor::execute_task_with_session(
            &task.asana_task_name,
            plan_text,
            None,
            None,
            &wc,
            Some(&log_dir),
            &self.runner_ctx,
            session_id,
        )
        .await?;

        // MEMORY 永続化
        self.persist_learnings(&result.output, &task, None);

        // ブロッカー検知: executing → conversing に遷移
        if let Some(ref blocker) = result.blocker {
            tracing::info!("Task {} blocker detected, reverting to conversing", task.id);
            self.db.update_status(task.id, "conversing")?;
            let blocks = build_conversing_blocks(task.id, &format!(":warning: *ブロッカーが検出されました*\n{}", blocker));
            self.slack.post_blocks(channel, thread_ts, &blocks,
                &format!("ブロッカー: {}", blocker)).await.ok();
            return Ok(());
        }

        if result.success {
            self.db.update_status(task.id, "done")?;
            context::append_completed_task(base_dir, &task, None, Some(&result.output));

            let output_summary = truncate_for_slack(&result.output, 3700);
            let msg = format!(
                ":white_check_mark: 実行完了\n```\n{}\n```",
                output_summary
            );
            self.slack
                .reply_thread(channel, thread_ts, &msg)
                .await
                .ok();
        } else {
            self.db
                .set_error(task.id, truncate_for_slack(&result.output, 500))?;

            let output_summary = truncate_for_slack(&result.output, 3700);
            let msg = format!(
                ":x: 実行失敗\n```\n{}\n```{}",
                output_summary, ERROR_LOG_HINT
            );
            self.slack
                .reply_thread(channel, thread_ts, &msg)
                .await
                .ok();
        }

        Ok(())
    }

    /// worktree 隔離実行: worktree 作成 → Act mode 実行 → PR 作成
    async fn execute_in_worktree(
        &self,
        task: CodingTask,
        repo_entry: &crate::repo_config::RepoEntry,
    ) -> Result<()> {
        let channel = task
            .slack_channel
            .as_deref()
            .unwrap_or(&self.default_slack_channel);
        let thread_ts = task.slack_thread_ts.as_deref().unwrap_or("");
        let base_dir = &self.repos_config.defaults.repos_base_dir;

        // Step 1: worktree 作成
        self.slack
            .reply_thread(channel, thread_ts, ":file_folder: worktree を作成中...")
            .await
            .ok();

        let ws = match workspace::create(
            base_dir,
            &repo_entry.key,
            task.id,
            &repo_entry.default_branch,
        )
        .await
        {
            Ok(ws) => ws,
            Err(e) => {
                let err_msg = format!("Worktree creation failed: {}", e);
                self.db.set_error(task.id, &err_msg)?;
                self.slack
                    .reply_thread(channel, thread_ts, &format!(":x: {}", err_msg))
                    .await
                    .ok();
                return Err(e);
            }
        };

        // Step 2: DB に branch_name を記録
        self.db
            .update_branch_name(task.id, &ws.branch_name)?;

        // Step 3: ストップボタン付き通知（status は spawn 前に "executing" に更新済み）
        let exec_msg = format!(":rocket: worktree で実行中... (branch: `{}`)", ws.branch_name);
        let exec_blocks = build_executing_blocks(task.id, &exec_msg);
        self.slack
            .post_blocks(channel, thread_ts, &exec_blocks, &exec_msg)
            .await
            .ok();

        // Step 4: Act mode 実行（--resume で Plan セッションを継続）
        self.execute_worktree_act(&task, repo_entry, &ws).await
    }

    /// worktree Act mode 実行: --resume で Plan セッションを継続
    async fn execute_worktree_act(
        &self,
        task: &CodingTask,
        repo_entry: &crate::repo_config::RepoEntry,
        ws: &workspace::Workspace,
    ) -> Result<()> {
        let plan_text = task.analysis_text.as_deref().unwrap_or("");
        let max_turns = self.resolve_execute_turns(&ws.worktree_path, task.complexity.as_deref());
        let has_session = task.claude_session_id.is_some();
        let wc = self.build_worktree_context(ws, max_turns, has_session);

        let log_dir = self.log_dir();
        let session_id = task.claude_session_id.as_deref();
        let result = executor::execute_task_with_session(
            &task.asana_task_name,
            plan_text,
            Some(repo_entry),
            Some(ws.worktree_path.as_path()),
            &wc,
            Some(&log_dir),
            &self.runner_ctx,
            session_id,
        )
        .await;

        self.handle_worktree_result(task, repo_entry, ws, result)
            .await
    }

    /// worktree 実行結果の共通処理: PR 作成 or エラー
    ///
    /// 成功時: `finalize_worktree`（PR作成 + remove）に委譲
    /// 失敗時: ここで remove する
    async fn handle_worktree_result(
        &self,
        task: &CodingTask,
        repo_entry: &crate::repo_config::RepoEntry,
        ws: &workspace::Workspace,
        result: Result<executor::ExecutionResult>,
    ) -> Result<()> {
        let channel = task
            .slack_channel
            .as_deref()
            .unwrap_or(&self.default_slack_channel);
        let thread_ts = task.slack_thread_ts.as_deref().unwrap_or("");

        match result {
            Ok(exec_result) if exec_result.success => {
                // MEMORY 永続化（worktree 削除前に main repo に保存）
                self.persist_learnings(&exec_result.output, task, Some(repo_entry));

                // ブロッカー検知: executing → conversing に遷移
                if let Some(ref blocker) = exec_result.blocker {
                    tracing::info!("Task {} blocker detected, reverting to conversing", task.id);
                    self.db.update_status(task.id, "conversing")?;
                    self.db.set_classification_outcome(task.id, "needed_converse").ok();
                    // claude_session_id は保持（--resume で再開可能）
                    let blocks = build_conversing_blocks(task.id, &format!(":warning: *ブロッカーが検出されました*\n{}", blocker));
                    self.slack.post_blocks(channel, thread_ts, &blocks,
                        &format!("ブロッカー: {}", blocker)).await.ok();
                    workspace::remove(ws).await.ok();
                    return Ok(());
                }

                // git-ratchet: 品質メトリクスが悪化していないか検証
                if let Err(ratchet_err) = quality_ratchet_check(&ws.worktree_path).await {
                    tracing::warn!("Task {} ratchet check failed: {}", task.id, ratchet_err);
                    self.db.set_error(task.id, &format!("Ratchet check failed: {}", ratchet_err))?;
                    let msg = format!(
                        ":no_entry: *品質ラチェット不合格* — PR を作成しません\n```\n{}\n```",
                        ratchet_err
                    );
                    self.slack.reply_thread(channel, thread_ts, &msg).await.ok();
                    workspace::remove(ws).await.ok();
                    return Ok(());
                }

                // finalize_worktree が remove まで担当
                self.finalize_worktree(task, repo_entry, ws).await?;
            }
            Ok(exec_result) => {
                // 失敗時も学びがあれば保存
                self.persist_learnings(&exec_result.output, task, Some(repo_entry));
                self.db
                    .set_error(task.id, truncate_for_slack(&exec_result.output, 500))?;
                let output_summary = truncate_for_slack(&exec_result.output, 3700);
                let msg = format!(":x: 実行失敗\n```\n{}\n```{}", output_summary, ERROR_LOG_HINT);
                self.slack
                    .reply_thread(channel, thread_ts, &msg)
                    .await
                    .ok();
                workspace::remove(ws).await.ok();
            }
            Err(e) => {
                self.db
                    .set_error(task.id, &format!("Execution error: {}", e))?;
                let msg = format!(":x: 実行エラー\n```\n{}\n```{}", e, ERROR_LOG_HINT);
                self.slack
                    .reply_thread(channel, thread_ts, &msg)
                    .await
                    .ok();
                workspace::remove(ws).await.ok();
            }
        }
        Ok(())
    }

    /// executor 出力から MEMORY + SKILL_CANDIDATE を抽出して永続化
    fn persist_learnings(
        &self,
        output: &str,
        task: &CodingTask,
        repo_entry: Option<&crate::repo_config::RepoEntry>,
    ) {
        // MEMORY 永続化
        if let Some(memory) = context::extract_memory(output) {
            let base_dir = &self.repos_config.defaults.repos_base_dir;
            let entry = format!("[{}] {}", task.asana_task_name, memory);

            if let Err(e) = context::append_memory(base_dir, &entry) {
                tracing::warn!("Failed to persist global memory: {}", e);
            }
            if let Some(re) = repo_entry {
                let repo_path = self.repos_config.repo_local_path(re);
                if let Err(e) = context::append_repo_memory(&repo_path, &entry) {
                    tracing::warn!("Failed to persist repo memory: {}", e);
                }
            }
            tracing::info!("Persisted learning for task {}: {}", task.id, memory);
        }

        // SKILL_CANDIDATE 蓄積
        let candidates = context::extract_skill_candidates(output);
        for (name, description) in &candidates {
            let repo_key = repo_entry.map(|r| r.key.as_str());
            if let Err(e) = self.db.upsert_skill_candidate(name, description, repo_key, Some(task.id)) {
                tracing::warn!("Failed to upsert skill candidate '{}': {}", name, e);
            }
        }
    }

    /// タスク処理を spawn し、panic/エラー時に DB を error 状態に復帰させる
    fn spawn_task<F, Fut>(self: &Arc<Self>, task_id: i64, f: F)
    where
        F: FnOnce(Arc<Worker>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<()>> + Send,
    {
        let w = Arc::clone(self);
        let db = self.db.clone();
        tokio::spawn(async move {
            match f(w).await {
                Ok(()) => {}
                Err(e) => {
                    tracing::error!("Task {} failed: {}", task_id, e);
                    db.set_error(task_id, &format!("Task failed: {}", e)).ok();
                }
            }
        });
    }

    /// WORKFLOW.md → defaults → complex*2 の順で max_turns を解決
    fn resolve_execute_turns(&self, worktree_path: &Path, complexity: Option<&str>) -> u32 {
        let wf = workflow::load(worktree_path);
        let base = wf
            .as_ref()
            .and_then(|w| w.config.max_execute_turns)
            .unwrap_or(self.repos_config.defaults.claude_max_execute_turns);
        match complexity {
            Some("complex") => base.saturating_mul(2),
            _ => base,
        }
    }

    /// worktree 用 WorkContext を構築
    ///
    /// - `has_session=true` の場合: ディレクトリ設定のみ行い context/memory の読み込みをスキップ
    ///   （--resume 時は Plan セッションにコンテキストが既にある）
    fn build_worktree_context(
        &self,
        ws: &workspace::Workspace,
        max_turns: u32,
        has_session: bool,
    ) -> context::WorkContext {
        let base_dir = &self.repos_config.defaults.repos_base_dir;
        setup_repo_dirs(&ws.worktree_path);
        let (work_context, work_memory) = if has_session {
            (String::new(), String::new())
        } else {
            (
                context::merged_context(base_dir, Some(&ws.worktree_path)),
                context::merged_memory(base_dir, Some(&ws.worktree_path)),
            )
        };
        context::WorkContext {
            repo_path: ws.worktree_path.clone(),
            max_turns,
            soul: context::merged_soul(base_dir, Some(&ws.worktree_path)),
            skill: context::read_skill(base_dir),
            context: work_context,
            memory: work_memory,
        }
    }

    /// worktree → PR 作成 → ci_pending or done
    async fn finalize_worktree(
        &self,
        task: &CodingTask,
        repo_entry: &crate::repo_config::RepoEntry,
        ws: &workspace::Workspace,
    ) -> Result<()> {
        let channel = task
            .slack_channel
            .as_deref()
            .unwrap_or(&self.default_slack_channel);
        let thread_ts = task.slack_thread_ts.as_deref().unwrap_or("");
        let base_dir = &self.repos_config.defaults.repos_base_dir;

        match workspace::finalize(
            ws,
            &task.asana_task_name,
            &repo_entry.default_branch,
            &repo_entry.github,
        )
        .await
        {
            Ok(pr_url) => {
                self.db.update_pr_url(task.id, &pr_url)?;
                self.db.update_status(task.id, "ci_pending")?;
                self.db.set_classification_outcome(task.id, "correct").ok();
                // ラチェットベースラインを更新（PR 作成成功 = 品質チェック通過済み）
                update_quality_baseline(&ws.worktree_path).await;
                let msg = format!(":gear: PR を作成しました — CI 結果を監視中...\n{}", pr_url);
                self.slack
                    .reply_thread(channel, thread_ts, &msg)
                    .await
                    .ok();
            }
            Err(e) => {
                self.db.update_status(task.id, "done")?;
                let repo_path = self.repos_config.repo_local_path(repo_entry);
                context::append_completed_task(base_dir, task, Some(&repo_path), None);
                let msg = format!(
                    ":white_check_mark: 自動実行完了（PR作成スキップ: {}）",
                    e
                );
                self.slack
                    .reply_thread(channel, thread_ts, &msg)
                    .await
                    .ok();
            }
        }

        workspace::remove(ws).await.ok();
        Ok(())
    }

    /// conversing タスクの継続処理。ユーザー返信があれば次の Claude ターンを spawn する。
    fn process_conversing_tasks(self: &Arc<Self>) -> bool {
        match self.db.get_conversing_tasks_needing_response() {
            Ok(tasks) => {
                for task in tasks {
                    let task_id = task.id;
                    self.spawn_task(task_id, |w| async move {
                        w.continue_conversing_task(task).await
                    });
                }
                false
            }
            Err(e) => {
                tracing::error!("Failed to fetch conversing tasks: {}", e);
                true
            }
        }
    }

    /// LLM で分類し、結果に基づいて executing or conversing にディスパッチ
    ///
    /// heuristics で暫定ステータスが設定済み。LLM 結果で変更が必要なら更新する。
    async fn classify_and_dispatch(&self, task: CodingTask) -> Result<()> {
        let log_dir = self.log_dir();
        let classification = classify_new_task_llm(
            &task, &self.repos_config, &self.db, &self.runner_ctx, &log_dir,
        ).await;

        // 分類結果を記録
        let class_str = match classification {
            TaskClassification::Execute => "execute",
            TaskClassification::Converse => "converse",
        };
        self.db.set_initial_classification(task.id, class_str).ok();

        match classification {
            TaskClassification::Execute => {
                // heuristics で conversing にしていた場合は executing に修正
                if task.status != "executing" {
                    self.db.update_status(task.id, "executing")?;
                }
                self.execute_approved_task(task).await
            }
            TaskClassification::Converse => {
                // heuristics で executing にしていた場合は conversing に修正
                if task.status != "conversing" {
                    self.db.update_status(task.id, "conversing")?;
                }
                self.start_conversing_task(task).await
            }
        }
    }

    /// conversing 開始: Slack スレッド作成 + 要件ヒアリング質問生成
    async fn start_conversing_task(&self, task: CodingTask) -> Result<()> {
        let channel = task
            .slack_channel
            .as_deref()
            .unwrap_or(&self.default_slack_channel);

        // 1. Slack スレッドを作成（まだなければ）
        let thread_ts = if let Some(ref existing_ts) = task.slack_thread_ts {
            existing_ts.clone()
        } else {
            let msg = format!(
                ":speech_balloon: *要件ヒアリング開始*\n*{}*",
                task.asana_task_name
            );
            let ts = self.slack.post_message(channel, &msg).await?;
            self.db.update_slack_thread(task.id, channel, &ts)?;
            ts
        };

        // 2. converse_thread_ts を設定
        self.db.update_converse_thread_ts(task.id, &thread_ts)?;

        // 3. ops_contexts にタスク情報を追加
        let repo_key = task.repo_key.as_deref().unwrap_or("default");
        let initial_context = format!(
            "タスク: {}\n説明: {}",
            task.asana_task_name,
            task.description.as_deref().unwrap_or("(なし)")
        );
        self.db.append_ops_context(channel, &thread_ts, repo_key, "user", &initial_context)?;

        // 4. Inception Turn1 相当のプロンプトで Claude を実行（要件ヒアリング質問生成）
        let repo_path = self.resolve_repo_path(&task).unwrap_or_else(|_| {
            std::path::PathBuf::from(&self.repos_config.defaults.repos_base_dir)
        });
        let history = self.db.get_ops_context(channel, &thread_ts)?;
        let soul = context::read_soul(&self.repos_config.defaults.repos_base_dir);
        let log_dir = self.log_dir();

        let req = super::ops::OpsRequest {
            message_text: initial_context,
            files: vec![],
        };

        let output = super::ops::execute_ops(
            &req, &repo_path, &[], &soul,
            5, Some(&log_dir), &self.runner_ctx, &history, None,
            super::ops::OpsExecMode::InceptionTurn1,
        ).await?;

        // 5. assistant 出力を保存
        self.db.append_ops_context(channel, &thread_ts, repo_key, "assistant", &output)?;

        // 6. 質問を Slack スレッドに投稿（conversing ボタン付き）
        let truncated = crate::claude::truncate_str(&output, 2800);
        let blocks = build_conversing_blocks(task.id, truncated);
        self.slack.post_blocks(channel, &thread_ts, &blocks,
            &format!(":speech_balloon: {}", truncated)).await?;

        tracing::info!("Task {} is now conversing, waiting for user reply", task.id);
        Ok(())
    }

    /// conversing 継続: ユーザー返信を受けて次の Claude ターンを実行
    async fn continue_conversing_task(&self, task: CodingTask) -> Result<()> {
        let channel = task
            .slack_channel
            .as_deref()
            .unwrap_or(&self.default_slack_channel);
        let thread_ts = task.converse_thread_ts.as_deref()
            .or(task.slack_thread_ts.as_deref())
            .ok_or_else(|| anyhow::anyhow!("No converse_thread_ts for task {}", task.id))?;

        let repo_key = task.repo_key.as_deref().unwrap_or("default");
        let history = self.db.get_ops_context(channel, thread_ts)?;

        let latest_user_msg = history.iter().rev()
            .find(|m| m.role == "user")
            .map(|m| m.content.as_str())
            .unwrap_or("");

        let repo_path = self.resolve_repo_path(&task).unwrap_or_else(|_| {
            std::path::PathBuf::from(&self.repos_config.defaults.repos_base_dir)
        });
        let soul = context::read_soul(&self.repos_config.defaults.repos_base_dir);
        let log_dir = self.log_dir();

        let req = super::ops::OpsRequest {
            message_text: latest_user_msg.to_string(),
            files: vec![],
        };

        // 常に InceptionTurn2 相当（要件確認の継続）
        let output = super::ops::execute_ops(
            &req, &repo_path, &[], &soul,
            5, Some(&log_dir), &self.runner_ctx, &history, None,
            super::ops::OpsExecMode::InceptionTurn2,
        ).await?;

        // assistant 出力を保存
        self.db.append_ops_context(channel, thread_ts, repo_key, "assistant", &output)?;

        // REQUIREMENTS_CONFIRMED: 検出 → 自動的に executing に遷移
        if output.contains("REQUIREMENTS_CONFIRMED:") {
            self.db.update_status(task.id, "executing")?;
            self.db.update_analysis(task.id, &output)?;
            self.slack.reply_thread(channel, thread_ts,
                ":white_check_mark: 要件が確定しました。実行を開始します...").await.ok();
            return Ok(());
        }

        // 通常の会話継続: 応答を Slack に投稿（conversing ボタン付き）
        let truncated = crate::claude::truncate_str(&output, 2800);
        let blocks = build_conversing_blocks(task.id, truncated);
        self.slack.post_blocks(channel, thread_ts, &blocks,
            &format!(":speech_balloon: {}", truncated)).await?;

        Ok(())
    }

    /// ci_pending タスクの CI 結果を確認し、完了 or リトライする
    async fn check_ci_and_handle(&self, task: CodingTask) -> Result<()> {
        let channel = task
            .slack_channel
            .as_deref()
            .unwrap_or(&self.default_slack_channel);
        let thread_ts = task.slack_thread_ts.as_deref().unwrap_or("");
        let base_dir = &self.repos_config.defaults.repos_base_dir;

        let repo_entry = match task
            .repo_key
            .as_deref()
            .and_then(|key| self.repos_config.find_repo_by_key(key))
        {
            Some(r) => r,
            None => {
                tracing::warn!("No repo_entry for ci_pending task {}", task.id);
                return Ok(());
            }
        };

        let branch_name = match task.branch_name.as_deref() {
            Some(b) => b,
            None => {
                tracing::warn!("No branch_name for ci_pending task {}", task.id);
                self.db.update_status(task.id, "done")?;
                return Ok(());
            }
        };

        // CI ステータスを確認
        let ci_status = workspace::check_ci(
            base_dir,
            &repo_entry.key,
            &repo_entry.github,
            branch_name,
        )
        .await?;

        match ci_status {
            workspace::CiStatus::Pending => {
                // まだ実行中 — 次のループで再チェック
                tracing::trace!("CI still pending for task {}", task.id);
            }
            workspace::CiStatus::NotFound => {
                // CI ワークフローがない — そのまま done に
                tracing::info!("No CI workflow found for task {}, marking done", task.id);
                self.db.update_status(task.id, "done")?;

                let repo_path = self.repos_config.repo_local_path(repo_entry);
                context::append_completed_task(base_dir, &task, Some(&repo_path), None);

                self.slack
                    .reply_thread(
                        channel,
                        thread_ts,
                        ":white_check_mark: 完了（CI ワークフローなし）",
                    )
                    .await
                    .ok();
            }
            workspace::CiStatus::Passed => {
                // CI 通過 — done
                self.db.update_status(task.id, "done")?;

                let repo_path = self.repos_config.repo_local_path(repo_entry);
                context::append_completed_task(base_dir, &task, Some(&repo_path), None);

                let pr_url = task.pr_url.as_deref().unwrap_or("(no URL)");
                let msg = format!(
                    ":white_check_mark: CI 通過 — 完了\n{}",
                    pr_url
                );
                self.slack
                    .reply_thread(channel, thread_ts, &msg)
                    .await
                    .ok();
            }
            workspace::CiStatus::Failed { summary } => {
                // CI 失敗 — リトライ可能か判定
                let new_count = self.db.increment_retry_count(task.id)?;
                let max_retry = repo_entry.ci_max_retry;

                if (new_count as u32) > max_retry {
                    // リトライ上限到達
                    self.db
                        .set_error(task.id, &format!("CI failed after {} retries: {}", new_count, summary))?;

                    let msg = format!(
                        ":x: CI 失敗（リトライ上限 {} 回に到達）\n```\n{}\n```{}",
                        max_retry, summary, ERROR_LOG_HINT
                    );
                    self.slack
                        .reply_thread(channel, thread_ts, &msg)
                        .await
                        .ok();
                } else {
                    // リトライ実行
                    tracing::info!(
                        "CI failed for task {} (retry {}/{}), attempting fix",
                        task.id, new_count, max_retry
                    );
                    if let Err(e) = self
                        .retry_ci_failed(&task, repo_entry, &summary)
                        .await
                    {
                        tracing::error!("CI retry failed for task {}: {}", task.id, e);
                        self.db.set_error(task.id, &format!("CI retry error: {}", e))?;
                    }
                }
            }
        }

        Ok(())
    }

    /// CI 失敗時のリトライ: worktree を再作成 → CI エラーをフィードバック → 再実行 → push
    async fn retry_ci_failed(
        &self,
        task: &CodingTask,
        repo_entry: &crate::repo_config::RepoEntry,
        ci_summary: &str,
    ) -> Result<()> {
        let channel = task
            .slack_channel
            .as_deref()
            .unwrap_or(&self.default_slack_channel);
        let thread_ts = task.slack_thread_ts.as_deref().unwrap_or("");
        let base_dir = &self.repos_config.defaults.repos_base_dir;
        let branch_name = task.branch_name.as_deref().unwrap_or("");
        let retry_count = task.retry_count;

        self.slack
            .reply_thread(
                channel,
                thread_ts,
                &format!(
                    ":recycle: CI 失敗を検出 — 自動修正中 (リトライ {})...\n```\n{}\n```",
                    retry_count + 1,
                    truncate_for_slack(ci_summary, 500)
                ),
            )
            .await
            .ok();

        // CI の失敗ログを取得（エージェントへのフィードバック用）
        let ci_log = workspace::get_ci_failure_log(
            base_dir,
            &repo_entry.key,
            &repo_entry.github,
            branch_name,
        )
        .await
        .unwrap_or_else(|_| ci_summary.to_string());

        // 既存ブランチから worktree を再作成
        let ws = workspace::create_for_retry(
            base_dir,
            &repo_entry.key,
            task.id,
            branch_name,
        )
        .await?;

        // CI エラーをフィードバックとしてプロンプトに注入
        let ci_fix_prompt = format!(
            "CI が失敗しました。以下のエラーログを読んで修正してください。\n\
             コードを修正し、テストが通ることを確認してから完了してください。\n\
             リンター設定やテスト設定を変更してはいけません。コードを直してください。\n\n\
             ## CI エラーログ\n```\n{}\n```",
            truncate_for_slack(&ci_log, 2500)
        );

        // executor 実行（CI エラーをプロンプトに含める）
        let max_turns = self.resolve_execute_turns(&ws.worktree_path, task.complexity.as_deref());
        let wc = self.build_worktree_context(&ws, max_turns, false);

        let log_dir = self.log_dir();
        let result = executor::execute_task(
            &format!("[CI FIX] {}", task.asana_task_name),
            &ci_fix_prompt,
            Some(repo_entry),
            Some(ws.worktree_path.as_path()),
            &wc,
            Some(&log_dir),
            &self.runner_ctx,
        )
        .await;

        match result {
            Ok(exec_result) if exec_result.success => {
                // 修正を push
                match workspace::push_retry(&ws).await {
                    Ok(()) => {
                        // ci_pending に戻す（次のループで CI を再チェック）
                        self.db.update_status(task.id, "ci_pending")?;

                        self.slack
                            .reply_thread(
                                channel,
                                thread_ts,
                                ":gear: CI 修正を push しました — CI 結果を再監視中...",
                            )
                            .await
                            .ok();
                    }
                    Err(e) => {
                        // push 失敗（変更なし等）
                        self.db.update_status(task.id, "ci_pending")?;
                        tracing::warn!("Push retry failed for task {}: {}", task.id, e);

                        self.slack
                            .reply_thread(
                                channel,
                                thread_ts,
                                &format!(":warning: CI 修正の push に失敗: {}", e),
                            )
                            .await
                            .ok();
                    }
                }
            }
            Ok(exec_result) => {
                // executor は完了したが成功ではない
                self.db.update_status(task.id, "ci_pending")?;

                let output_summary = truncate_for_slack(&exec_result.output, 500);
                self.slack
                    .reply_thread(
                        channel,
                        thread_ts,
                        &format!(":warning: CI 修正の実行結果が不明 — 再監視中\n```\n{}\n```", output_summary),
                    )
                    .await
                    .ok();
            }
            Err(e) => {
                // executor エラー
                self.db.update_status(task.id, "ci_pending")?;
                tracing::error!("CI fix executor error for task {}: {}", task.id, e);

                self.slack
                    .reply_thread(
                        channel,
                        thread_ts,
                        &format!(":x: CI 修正の実行中にエラー: {}{}", e, ERROR_LOG_HINT),
                    )
                    .await
                    .ok();
            }
        }

        // worktree cleanup
        workspace::remove(&ws).await.ok();

        Ok(())
    }
}

// 旧 build_proposal_blocks / build_info_blocks は削除（conversing フローに置き換え済み）

/// Block Kit の実行中ブロック（ストップボタン付き）
fn build_executing_blocks(task_id: i64, message: &str) -> serde_json::Value {
    serde_json::json!([
        {
            "type": "section",
            "text": {
                "type": "mrkdwn",
                "text": message
            }
        },
        {
            "type": "actions",
            "elements": [
                {
                    "type": "button",
                    "text": {
                        "type": "plain_text",
                        "text": ":octagonal_sign: 中止",
                        "emoji": true
                    },
                    "style": "danger",
                    "action_id": "stop_task",
                    "value": format!("{}", task_id)
                }
            ]
        }
    ])
}

/// リポジトリの初期セットアップ（ディレクトリ作成 + デフォルトルール生成）
fn setup_repo_dirs(repo_path: &Path) {
    let agent_dir = repo_path.join(".agent");
    if let Err(e) = std::fs::create_dir_all(&agent_dir) {
        tracing::warn!("Failed to create repo .agent dir: {}", e);
    }
    ensure_repo_agent_rules(repo_path);
}

// 旧 prepare_repo_context は削除（start_conversing_task で直接構築）

/// .claude/rules/agent.md が無ければデフォルトルールを生成
fn ensure_repo_agent_rules(repo_path: &Path) {
    let agent_rules = repo_path.join(".claude").join("rules").join("agent.md");
    if agent_rules.exists() {
        return;
    }
    if let Some(parent) = agent_rules.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::warn!("Failed to create .claude/rules dir: {}", e);
            return;
        }
    }

    let default_rules = "\
# エージェント向けルール

## 基本原則
- CLAUDE.md に記載されたプロジェクト規約に従うこと
- 既存のコードパターン・命名規則・ディレクトリ構造を尊重すること
- スコープ外の変更は禁止（依頼された範囲のみ変更すること）

## 実行スタイル（重要）
- **確認を求めて止まるな。** 計画に従って最後まで自律的に実行すること
- エラーが出たらコードを修正して再試行。3回修正しても解決しなければ SUMMARY に記録して完了
- 不明点は合理的に推測して進め、推測した内容を SUMMARY に記録すること

## 品質チェック（完了前に必須）
- テストがあれば実行して全パス確認
- リンターがあれば実行してエラーゼロ確認
- 型チェックがあれば実行してエラーゼロ確認

## 知識活用
- `.agent/memory.md` があれば作業開始時に読み、過去の学びを活用すること
- 作業中に発見したパターン・注意点があれば `.agent/memory.md` に追記すること

## Worktree 安全ルール
- 専用 worktree 内でのみ作業する（共有 workspace を触らない）
- git stash / git checkout / git switch は禁止（ブランチ管理はランタイムが行う）
- git worktree の作成・削除は禁止（ランタイムが管理する）
- 現在のタスクスコープ外のファイルを変更しない

## Harness ルール
- リンター設定・フォーマッター設定・テスト設定を変更してはいけない
- テストやリンターのエラーは、コードを修正して解決すること
- #[allow(...)] / @ts-ignore / noqa 等でエラーを黙らせてはいけない
- CI が失敗した場合はコードを直すこと（CI 設定を変えない）
";

    if let Err(e) = std::fs::write(&agent_rules, default_rules) {
        tracing::warn!("Failed to write default agent rules: {}", e);
    } else {
        tracing::info!("Generated default .claude/rules/agent.md at {}", agent_rules.display());
    }
}

pub(crate) fn truncate_for_slack(text: &str, max_len: usize) -> &str {
    crate::claude::truncate_str(text, max_len)
}

/// from → to 間の営業日数をカウント（土日を除外、JST ベース）
fn count_business_days(from: DateTime<Utc>, to: DateTime<Utc>) -> i64 {
    let jst_offset = chrono::Duration::hours(9);
    let start = (from + jst_offset).date_naive();
    let end = (to + jst_offset).date_naive();
    let mut count = 0i64;
    let mut d = start.succ_opt().unwrap_or(start); // 翌日からカウント開始
    while d <= end {
        let wd = d.weekday();
        if !matches!(wd, chrono::Weekday::Sat | chrono::Weekday::Sun) {
            count += 1;
        }
        d = d.succ_opt().unwrap_or(d);
    }
    count
}

const ERROR_LOG_HINT: &str = "\n_詳細ログ: `journalctl --user -u sdtab-ambient-task-agent -n 50`_";

// ============================================================================
// タスク分類（new → executing / conversing）
// ============================================================================

enum TaskClassification {
    Execute,
    Converse,
}

/// new タスクを executing（即実行）か conversing（要件ヒアリング）に分類（heuristics 版）
fn classify_new_task_heuristic(task: &CodingTask, repos_config: &ReposConfig) -> TaskClassification {
    let is_slack_origin = task.asana_task_gid.starts_with("slack_")
        || task.asana_task_gid.starts_with("ops_");

    let has_repo = task.repo_key.is_some();

    let auto_execute = task.repo_key.as_deref()
        .and_then(|key| repos_config.find_repo_by_key(key))
        .map(|r| r.auto_execute)
        .unwrap_or(false);

    if is_slack_origin {
        // Slack/ops 入口: analysis_text に要件定義が入っていれば即実行
        if task.analysis_text.is_some() {
            TaskClassification::Execute
        } else {
            TaskClassification::Converse
        }
    } else {
        // Asana 入口: auto_execute フラグを尊重
        if auto_execute && has_repo {
            TaskClassification::Execute
        } else {
            TaskClassification::Converse
        }
    }
}

/// new タスクを LLM で分類（past examples を few-shot で渡す）
/// LLM 呼び出しに失敗した場合は heuristics にフォールバック
async fn classify_new_task_llm(
    task: &CodingTask,
    repos_config: &ReposConfig,
    db: &crate::db::Db,
    runner_ctx: &crate::execution::RunnerContext,
    log_dir: &Path,
) -> TaskClassification {
    // 過去の分類履歴を取得（few-shot 用）
    let history = db.get_recent_classification_history(10).unwrap_or_default();
    let examples = if history.is_empty() {
        String::new()
    } else {
        let lines: Vec<String> = history.iter().map(|r| {
            format!("- 「{}」({}文字) → {} → 結果: {}", r.task_name, r.description.len(), r.classification, r.outcome)
        }).collect();
        format!("## 過去の分類履歴\n{}\n\n", lines.join("\n"))
    };

    let desc = task.description.as_deref().unwrap_or("(なし)");
    let prompt = format!(
        "タスクを execute（即実行可能）か converse（要件確認が必要）に分類してください。\n\n\
         {}## 新しいタスク\n名前: {}\n説明: {}\nリポジトリ: {}\n入口: {}\n\n\
         判断基準:\n- 要件が明確で実装可能 → execute\n- 曖昧・不足・確認事項あり → converse",
        examples,
        task.asana_task_name,
        crate::claude::truncate_str(desc, 500),
        task.repo_key.as_deref().unwrap_or("なし"),
        task.source,
    );

    let schema = r#"{"type":"object","properties":{"classification":{"type":"string","enum":["execute","converse"]},"reason":{"type":"string"}},"required":["classification"]}"#;

    let result = crate::claude::ClaudeRunner::new("classify", &prompt)
        .max_turns(1)
        .allowed_tools("")
        .json_schema(schema)
        .log_dir(log_dir)
        .with_context(runner_ctx)
        .run()
        .await;

    match result {
        Ok(r) if r.success => {
            let answer: Option<String> = serde_json::from_str::<serde_json::Value>(&r.stdout)
                .ok()
                .and_then(|v| v.get("classification")?.as_str().map(|s| s.to_string()));

            match answer.as_deref() {
                Some("execute") => {
                    tracing::info!("LLM classify task {}: execute", task.id);
                    TaskClassification::Execute
                }
                Some("converse") => {
                    tracing::info!("LLM classify task {}: converse", task.id);
                    TaskClassification::Converse
                }
                _ => {
                    tracing::warn!("LLM classify: unexpected answer '{}', falling back to heuristics", r.stdout);
                    classify_new_task_heuristic(task, repos_config)
                }
            }
        }
        Ok(r) => {
            tracing::warn!("LLM classify failed (non-success): {}, falling back", r.stderr);
            classify_new_task_heuristic(task, repos_config)
        }
        Err(e) => {
            tracing::warn!("LLM classify error: {}, falling back to heuristics", e);
            classify_new_task_heuristic(task, repos_config)
        }
    }
}

// ============================================================================
// Block Kit ヘルパー（conversing / manual）
// ============================================================================

/// conversing 状態のボタンレイアウト: [実行開始] [指示追加] [手動修正] [スキップ]
fn build_conversing_blocks(task_id: i64, message: &str) -> serde_json::Value {
    serde_json::json!([
        {
            "type": "section",
            "text": {
                "type": "mrkdwn",
                "text": format!(":speech_balloon: {}", message)
            }
        },
        {
            "type": "actions",
            "elements": [
                {
                    "type": "button",
                    "text": { "type": "plain_text", "text": "実行開始" },
                    "style": "primary",
                    "action_id": "task_execute",
                    "value": task_id.to_string()
                },
                {
                    "type": "button",
                    "text": { "type": "plain_text", "text": "指示追加" },
                    "action_id": "task_add_instruction",
                    "value": task_id.to_string()
                },
                {
                    "type": "button",
                    "text": { "type": "plain_text", "text": "手動修正" },
                    "action_id": "task_manual",
                    "value": task_id.to_string()
                },
                {
                    "type": "button",
                    "text": { "type": "plain_text", "text": "スキップ" },
                    "style": "danger",
                    "action_id": "task_skip",
                    "value": task_id.to_string()
                }
            ]
        }
    ])
}

/// manual 状態のボタンレイアウト: [再開] [完了]
// ============================================================================
// Git Ratchet（品質ラチェット）
// ============================================================================

#[derive(serde::Serialize, serde::Deserialize, Debug)]
struct QualityBaseline {
    test_count: u32,
    clippy_warnings: u32,
    #[serde(default)]
    updated_at: String,
}

/// ベースラインファイルのパス（worktree の親リポジトリの .agent/ に保存）
fn baseline_path(worktree_path: &Path) -> PathBuf {
    // worktree_path は .claude/worktrees/xxx/ の下にあるので、
    // main repo の .agent/ に保存する
    worktree_path
        .ancestors()
        .find(|p| p.join(".agent").is_dir())
        .unwrap_or(worktree_path)
        .join(".agent")
        .join("quality-baseline.json")
}

/// worktree で cargo test + clippy を実行してメトリクスを取得
async fn capture_quality_metrics(worktree_path: &Path) -> Result<(u32, u32)> {
    // cargo test
    let test_output = tokio::process::Command::new("cargo")
        .arg("test")
        .arg("--")
        .arg("--format=terse")
        .current_dir(worktree_path)
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("cargo test failed to start: {}", e))?;

    let test_stdout = String::from_utf8_lossy(&test_output.stdout);
    // "test result: ok. 36 passed" をパース
    let test_count = test_stdout.lines()
        .find(|l| l.contains("test result:"))
        .and_then(|l| {
            l.split_whitespace()
                .find(|w| w.parse::<u32>().is_ok())
                .and_then(|w| w.parse::<u32>().ok())
        })
        .unwrap_or(0);

    // cargo clippy
    let clippy_output = tokio::process::Command::new("cargo")
        .args(["clippy", "--message-format=short"])
        .current_dir(worktree_path)
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("cargo clippy failed to start: {}", e))?;

    let clippy_stderr = String::from_utf8_lossy(&clippy_output.stderr);
    let clippy_warnings = clippy_stderr.lines()
        .filter(|l| l.contains("warning:") && !l.contains("warning: `"))
        .count() as u32;

    Ok((test_count, clippy_warnings))
}

/// ラチェット検証: テスト数が減少 or clippy warnings が増加 → エラー
async fn quality_ratchet_check(worktree_path: &Path) -> Result<()> {
    let bp = baseline_path(worktree_path);
    let baseline = if bp.exists() {
        let content = std::fs::read_to_string(&bp)
            .map_err(|e| anyhow::anyhow!("Failed to read baseline: {}", e))?;
        serde_json::from_str::<QualityBaseline>(&content)
            .map_err(|e| anyhow::anyhow!("Failed to parse baseline: {}", e))?
    } else {
        tracing::info!("No quality baseline found, skipping ratchet check");
        return Ok(());
    };

    let (test_count, clippy_warnings) = capture_quality_metrics(worktree_path).await?;

    tracing::info!(
        "Ratchet check: tests={}/{} (baseline), clippy_warnings={}/{} (baseline)",
        test_count, baseline.test_count, clippy_warnings, baseline.clippy_warnings
    );

    let mut violations = Vec::new();
    if test_count < baseline.test_count {
        violations.push(format!(
            "テスト数が減少: {} → {} ({}件減)",
            baseline.test_count, test_count, baseline.test_count - test_count
        ));
    }
    if clippy_warnings > baseline.clippy_warnings {
        violations.push(format!(
            "clippy warnings が増加: {} → {} ({}件増)",
            baseline.clippy_warnings, clippy_warnings, clippy_warnings - baseline.clippy_warnings
        ));
    }

    if violations.is_empty() {
        tracing::info!("Ratchet check passed");
        Ok(())
    } else {
        Err(anyhow::anyhow!("{}", violations.join("\n")))
    }
}

/// ベースラインを更新（PR 作成成功後に呼ぶ）
async fn update_quality_baseline(worktree_path: &Path) {
    match capture_quality_metrics(worktree_path).await {
        Ok((test_count, clippy_warnings)) => {
            let baseline = QualityBaseline {
                test_count,
                clippy_warnings,
                updated_at: chrono::Utc::now().to_rfc3339(),
            };
            let bp = baseline_path(worktree_path);
            if let Some(parent) = bp.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            match serde_json::to_string_pretty(&baseline) {
                Ok(json) => {
                    if let Err(e) = std::fs::write(&bp, json) {
                        tracing::warn!("Failed to write quality baseline: {}", e);
                    } else {
                        tracing::info!("Quality baseline updated: tests={}, clippy={}", test_count, clippy_warnings);
                    }
                }
                Err(e) => tracing::warn!("Failed to serialize quality baseline: {}", e),
            }
        }
        Err(e) => tracing::warn!("Failed to capture quality metrics for baseline: {}", e),
    }
}

#[allow(dead_code)]
fn build_manual_blocks(task_id: i64) -> serde_json::Value {
    serde_json::json!([
        {
            "type": "section",
            "text": {
                "type": "mrkdwn",
                "text": ":wrench: *手動対応モード*\nターミナルで作業を行い、完了後に以下のボタンを押してください:"
            }
        },
        {
            "type": "actions",
            "elements": [
                {
                    "type": "button",
                    "text": { "type": "plain_text", "text": "再開" },
                    "style": "primary",
                    "action_id": "task_resume",
                    "value": task_id.to_string()
                },
                {
                    "type": "button",
                    "text": { "type": "plain_text", "text": "完了" },
                    "action_id": "task_done",
                    "value": task_id.to_string()
                }
            ]
        }
    ])
}

