use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::Notify;

use crate::db::{CodingTask, Db};
use crate::google::calendar::GoogleCalendarClient;
use crate::repo_config::ReposConfig;
use crate::slack::client::SlackClient;

use super::{analyzer, context, decomposer, executor, priority, scheduler, task_file};

/// ハートビート間隔の下限
const MIN_HEARTBEAT_SECS: u64 = 10;

pub struct Worker {
    db: Db,
    repos_config: ReposConfig,
    slack: SlackClient,
    asana_pat: String,
    asana_project_id: String,
    asana_user_name: String,
    google_calendar: Option<GoogleCalendarClient>,
    default_slack_channel: String,
    notify: Arc<Notify>,
}

impl Worker {
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
    ) -> Self {
        Self {
            db,
            repos_config,
            slack,
            asana_pat,
            asana_project_id,
            asana_user_name,
            google_calendar,
            default_slack_channel,
            notify,
        }
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
    /// - ハートビート（60秒）: スケジューラージョブチェック
    /// - イベント駆動: Notify で即時起床してタスク処理
    pub async fn run(mut self) {
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

        let mut consecutive_errors: u32 = 0;

        loop {
            let mut had_error = false;

            // タスク処理
            had_error |= self.process_tasks().await;

            // スケジューラージョブチェック
            had_error |= self.run_scheduler().await;

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
                    _ = self.notify.notified() => {
                        tracing::debug!("Worker woken by event");
                    }
                    _ = tokio::time::sleep(heartbeat) => {
                        tracing::trace!("Worker heartbeat");
                    }
                }
            }
        }
    }

    /// タスクキューを処理。エラーがあれば true を返す
    async fn process_tasks(&self) -> bool {
        let mut had_error = false;

        // 1. new タスク → analyzing → proposed
        match self.db.get_new_task() {
            Ok(Some(task)) => {
                tracing::info!("Analyzing task: {} ({})", task.asana_task_name, task.asana_task_gid);
                if let Err(e) = self.analyze_task(task).await {
                    tracing::error!("Task analysis failed: {}", e);
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::error!("Failed to fetch new task: {}", e);
                had_error = true;
            }
        }

        // 2. approved タスク → decomposing → ready
        match self.db.get_approved_task() {
            Ok(Some(task)) => {
                tracing::info!("Decomposing approved task: {} ({})", task.asana_task_name, task.asana_task_gid);
                if let Err(e) = self.decompose_task(task).await {
                    tracing::error!("Task decomposition failed: {}", e);
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::error!("Failed to fetch approved task: {}", e);
                had_error = true;
            }
        }

        // 2.5. 全アクティブタスクの優先度を再計算
        if let Ok(active_tasks) = self.db.get_active_tasks() {
            let now = chrono::Utc::now();
            for t in &active_tasks {
                let score = priority::calculate_priority_score(t, &now);
                if let Err(e) = self.db.update_priority_score(t.id, score) {
                    tracing::warn!("Failed to update priority for task {}: {}", t.id, e);
                }
            }
        }

        // 3. auto_approved タスク → executing → done
        match self.db.get_auto_approved_task() {
            Ok(Some(task)) => {
                tracing::info!("Auto-executing task: {} ({})", task.asana_task_name, task.asana_task_gid);
                if let Err(e) = self.execute_auto_approved_task(task).await {
                    tracing::error!("Auto-execution failed: {}", e);
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::error!("Failed to fetch auto_approved task: {}", e);
                had_error = true;
            }
        }

        had_error
    }

    /// スケジューラージョブを実行。エラーがあれば true を返す
    async fn run_scheduler(&mut self) -> bool {
        let mut ctx = scheduler::SchedulerContext {
            db: self.db.clone(),
            slack: self.slack.clone(),
            asana_pat: self.asana_pat.clone(),
            asana_project_id: self.asana_project_id.clone(),
            asana_user_name: self.asana_user_name.clone(),
            google_calendar: self.google_calendar.take(),
            repos_base_dir: self.repos_config.defaults.repos_base_dir.clone(),
            stagnation_threshold_hours: self.repos_config.defaults.stagnation_threshold_hours,
        };

        let had_error = if let Err(e) = scheduler::check_and_run(&mut ctx).await {
            tracing::error!("Scheduled job check failed: {}", e);
            true
        } else {
            false
        };

        self.google_calendar = ctx.google_calendar;
        had_error
    }

    /// new → analyzing → proposed: 要件定義を生成して Block Kit ボタン付きで Slack 投稿
    async fn analyze_task(&self, task: CodingTask) -> Result<()> {
        let channel = task
            .slack_channel
            .as_deref()
            .unwrap_or(&self.default_slack_channel);

        // Step 1: Slack 親メッセージ送信（再生成時は既存スレッドを再利用）
        let thread_ts = if let Some(ref existing_ts) = task.slack_thread_ts {
            self.slack
                .reply_thread(channel, existing_ts, ":arrows_counterclockwise: 要件定義を再生成中...")
                .await
                .ok();
            existing_ts.clone()
        } else {
            let parent_msg = format!(
                ":inbox_tray: タスクを受信しました\n*{}*\nhttps://app.asana.com/0/0/{}",
                task.asana_task_name, task.asana_task_gid
            );
            match self.slack.post_message(channel, &parent_msg).await {
                Ok(ts) => {
                    self.db.update_slack_thread(task.id, channel, &ts)?;
                    ts
                }
                Err(e) => {
                    tracing::error!("Failed to post Slack message: {}", e);
                    self.db
                        .set_error(task.id, &format!("Slack post failed: {}", e))?;
                    return Err(e);
                }
            }
        };

        // Step 2: status → analyzing
        self.db.update_status(task.id, "analyzing")?;

        // Step 3: リポジトリパスを解決
        let repo_path = match self.resolve_repo_path(&task) {
            Ok(p) => p,
            Err(e) => {
                self.db.set_error(task.id, &e.to_string())?;
                self.slack
                    .reply_thread(channel, &thread_ts, &format!(":x: エラー: {}", e))
                    .await
                    .ok();
                return Err(e);
            }
        };

        // Step 4: claude -p で要件定義生成
        let notes = task.description.as_deref().unwrap_or("");
        self.slack
            .reply_thread(channel, &thread_ts, ":brain: 要件定義を作成中...")
            .await
            .ok();

        let base_dir = &self.repos_config.defaults.repos_base_dir;
        let soul = context::read_soul(base_dir);
        let skill = context::read_skill(base_dir);
        let work_context = context::read_context(base_dir);
        let work_memory = context::read_memory(base_dir);
        let max_turns = self.repos_config.defaults.claude_max_plan_turns;

        match analyzer::analyze_task(
            &task.asana_task_name,
            notes,
            &repo_path,
            max_turns,
            &soul,
            &skill,
            &work_context,
            &work_memory,
        )
        .await
        {
            Ok(analysis) => {
                self.db.update_analysis(task.id, &analysis)?;
                self.db.update_status(task.id, "proposed")?;

                // Block Kit ボタン付きで Slack に投稿
                let analysis_display = truncate_for_slack(&analysis, 2800);
                let blocks = build_proposal_blocks(task.id, analysis_display);
                let plan_ts = self
                    .slack
                    .post_blocks(channel, &thread_ts, &blocks, "要件定義が完成しました（ボタンで操作してください）")
                    .await?;

                self.db.update_plan_ts(task.id, &plan_ts)?;

                tracing::info!(
                    "Analysis posted for task {} (plan_ts: {})",
                    task.asana_task_gid,
                    plan_ts
                );
            }
            Err(e) => {
                let err_msg = format!("Analysis failed: {}", e);
                self.db.set_error(task.id, &err_msg)?;
                self.slack
                    .reply_thread(
                        channel,
                        &thread_ts,
                        &format!(":x: 要件定義の作成に失敗しました\n```\n{}\n```", e),
                    )
                    .await
                    .ok();
                tracing::error!("{}", err_msg);
            }
        }

        Ok(())
    }

    /// approved → decomposing → ready: タスク分解してファイル書き出し
    async fn decompose_task(&self, task: CodingTask) -> Result<()> {
        let channel = task
            .slack_channel
            .as_deref()
            .unwrap_or(&self.default_slack_channel);
        let thread_ts = task.slack_thread_ts.as_deref().unwrap_or("");

        // Step 1: status → decomposing
        self.db.update_status(task.id, "decomposing")?;
        self.slack
            .reply_thread(channel, thread_ts, ":gear: タスクを分解中...")
            .await
            .ok();

        // Step 2: リポジトリパスを解決
        let repo_path = match self.resolve_repo_path(&task) {
            Ok(p) => p,
            Err(e) => {
                self.db.set_error(task.id, &e.to_string())?;
                return Err(e);
            }
        };

        let analysis = task.analysis_text.as_deref().unwrap_or("");
        let base_dir = &self.repos_config.defaults.repos_base_dir;
        let soul = context::read_soul(base_dir);
        let skill = context::read_skill(base_dir);
        let work_context = context::read_context(base_dir);
        let work_memory = context::read_memory(base_dir);
        let max_turns = self.repos_config.defaults.claude_max_plan_turns;

        // Step 3: claude -p でサブタスク生成
        match decomposer::decompose_task(
            &task.asana_task_name,
            analysis,
            &repo_path,
            max_turns,
            &soul,
            &skill,
            &work_context,
            &work_memory,
        )
        .await
        {
            Ok(mut subtasks) => {
                // ブロック検知
                decomposer::detect_blocked_subtasks(&mut subtasks);

                // DB に subtasks_json 保存
                let json = serde_json::to_string(&subtasks)?;
                self.db.update_subtasks(task.id, &json)?;

                // 進捗率・見積もり時間を DB に保存
                let progress = decomposer::calculate_progress(&subtasks);
                self.db.update_progress(task.id, progress)?;
                let estimated_total: i32 = subtasks
                    .iter()
                    .filter_map(|s| s.estimated_minutes)
                    .sum::<u32>() as i32;
                if estimated_total > 0 {
                    let conn_task = self.db.get_task_by_id(task.id)?;
                    if let Some(t) = conn_task {
                        // estimated_minutes は DB 上で直接更新
                        let now = chrono::Utc::now();
                        let score = priority::calculate_priority_score(&t, &now);
                        self.db.update_priority_score(task.id, score)?;
                    }
                }

                // タスクファイル書き出し（優先度・進捗の更新を反映するため再取得）
                let updated_task = self.db.get_task_by_id(task.id)?.unwrap_or(task.clone());
                task_file::write_task_file(base_dir, &updated_task, &subtasks)?;

                // status → ready
                self.db.update_status(task.id, "ready")?;

                // Slack にサブタスク一覧を投稿
                let subtask_lines: Vec<String> = subtasks
                    .iter()
                    .map(|s| format!("{}. {}", s.index, s.title))
                    .collect();
                let msg = format!(
                    ":white_check_mark: タスクを分解しました（{}件）\n\n{}\n\n`/task {}` で詳細を確認できます",
                    subtasks.len(),
                    subtask_lines.join("\n"),
                    task.id
                );
                self.slack.reply_thread(channel, thread_ts, &msg).await.ok();

                tracing::info!(
                    "Task {} decomposed into {} subtasks",
                    task.asana_task_gid,
                    subtasks.len()
                );
            }
            Err(e) => {
                let err_msg = format!("Decomposition failed: {}", e);
                self.db.set_error(task.id, &err_msg)?;
                self.slack
                    .reply_thread(
                        channel,
                        thread_ts,
                        &format!(":x: タスク分解に失敗しました\n```\n{}\n```", e),
                    )
                    .await
                    .ok();
                tracing::error!("{}", err_msg);
            }
        }

        Ok(())
    }

    /// auto_approved → executing → done: 要件定義をプランとして自動実行
    async fn execute_auto_approved_task(&self, task: CodingTask) -> Result<()> {
        let channel = task
            .slack_channel
            .as_deref()
            .unwrap_or(&self.default_slack_channel);
        let thread_ts = task.slack_thread_ts.as_deref().unwrap_or("");

        // Step 1: executing に更新 + Slack 通知
        self.db.update_status(task.id, "executing")?;
        self.slack
            .reply_thread(channel, thread_ts, ":rocket: 自動実行中...")
            .await
            .ok();

        // Step 2: リポジトリパスとエントリを解決
        let repo_entry = task
            .repo_key
            .as_deref()
            .and_then(|key| self.repos_config.find_repo_by_key(key));

        let repo_path = repo_entry.map(|r| self.repos_config.repo_local_path(r));

        // analysis_text をプランとして使う
        let plan_text = task.analysis_text.as_deref().unwrap_or("");
        let max_turns = self.repos_config.defaults.claude_max_execute_turns;
        let base_dir = &self.repos_config.defaults.repos_base_dir;
        let soul = context::read_soul(base_dir);
        let skill = context::read_skill(base_dir);
        let work_context = context::read_context(base_dir);
        let work_memory = context::read_memory(base_dir);

        // Step 3: executor 呼び出し
        let result = executor::execute_task(
            &task.asana_task_name,
            plan_text,
            repo_entry,
            repo_path.as_deref(),
            max_turns,
            &soul,
            &skill,
            &work_context,
            &work_memory,
        )
        .await?;

        // Step 4: 結果を Slack に投稿
        if result.success {
            self.db.update_status(task.id, "done")?;

            let output_summary = truncate_for_slack(&result.output, 3700);
            let msg = format!(
                ":white_check_mark: 自動実行完了\n```\n{}\n```",
                output_summary
            );
            self.slack
                .reply_thread(channel, thread_ts, &msg)
                .await
                .ok();
        } else {
            self.db
                .set_error(task.id, &truncate_for_slack(&result.output, 500))?;

            let output_summary = truncate_for_slack(&result.output, 3700);
            let msg = format!(
                ":x: 自動実行失敗\n```\n{}\n```",
                output_summary
            );
            self.slack
                .reply_thread(channel, thread_ts, &msg)
                .await
                .ok();
        }

        Ok(())
    }
}

/// Block Kit の承認ボタン付きブロックを構築
fn build_proposal_blocks(task_id: i64, analysis_text: &str) -> serde_json::Value {
    let task_id_str = task_id.to_string();
    serde_json::json!([
        {
            "type": "section",
            "text": {
                "type": "mrkdwn",
                "text": format!(":clipboard: *要件定義*\n\n{}", analysis_text)
            }
        },
        {
            "type": "actions",
            "elements": [
                {
                    "type": "button",
                    "text": { "type": "plain_text", "text": "✅ OK" },
                    "action_id": "approve_task",
                    "value": task_id_str,
                    "style": "primary"
                },
                {
                    "type": "button",
                    "text": { "type": "plain_text", "text": "❌ NG" },
                    "action_id": "reject_task",
                    "value": task_id_str,
                    "style": "danger"
                },
                {
                    "type": "button",
                    "text": { "type": "plain_text", "text": "🔄 再生成" },
                    "action_id": "regenerate_task",
                    "value": task_id_str
                }
            ]
        }
    ])
}

fn truncate_for_slack(text: &str, max_len: usize) -> &str {
    if text.len() <= max_len {
        text
    } else {
        let mut end = max_len;
        while !text.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        &text[..end]
    }
}
