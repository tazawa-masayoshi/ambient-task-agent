use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::Notify;

use crate::db::{CodingTask, Db};
use crate::google::calendar::GoogleCalendarClient;
use crate::repo_config::ReposConfig;
use crate::slack::client::SlackClient;

use super::{analyzer, context, decomposer, executor, priority, scheduler, task_file, workspace};

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
            google_calendar,
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

        // wez-sidebar タスクキャッシュ同期
        if let Some(ref cache_path) = self.repos_config.defaults.tasks_cache_file {
            if let Err(e) = task_file::sync_tasks_cache(&self.db, cache_path) {
                tracing::warn!("Failed to sync tasks cache: {}", e);
            }
        }

        had_error
    }

    /// スケジューラージョブを実行。エラーがあれば true を返す
    async fn run_scheduler(&mut self) -> bool {
        let base_dir = &self.repos_config.defaults.repos_base_dir;
        let mut ctx = scheduler::SchedulerContext {
            db: self.db.clone(),
            slack: self.slack.clone(),
            asana_pat: self.asana_pat.clone(),
            asana_project_id: self.asana_project_id.clone(),
            asana_user_name: self.asana_user_name.clone(),
            google_calendar: self.google_calendar.take(),
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
        let (work_context, work_memory) = prepare_repo_context(base_dir, &repo_path);
        let wc = context::WorkContext {
            repo_path: repo_path.clone(),
            max_turns: self.repos_config.defaults.claude_max_plan_turns,
            soul: context::read_soul(base_dir),
            skill: context::read_skill(base_dir),
            context: work_context,
            memory: work_memory,
        };

        let log_dir = self.log_dir();
        match analyzer::analyze_task(
            &task.asana_task_name,
            notes,
            &wc,
            Some(&log_dir),
            &self.runner_ctx,
        )
        .await
        {
            Ok((analysis, complexity)) => {
                self.db.update_analysis(task.id, &analysis)?;
                if let Some(ref c) = complexity {
                    self.db.update_complexity(task.id, c)?;
                    tracing::info!("Task {} complexity: {}", task.id, c);
                }

                // auto_execute 判定
                let is_auto_execute = task
                    .repo_key
                    .as_deref()
                    .and_then(|key| self.repos_config.find_repo_by_key(key))
                    .map(|r| r.auto_execute)
                    .unwrap_or(false);

                if is_auto_execute {
                    // ボタンなしで情報投稿 → auto_approved へ
                    let analysis_display = truncate_for_slack(&analysis, 2800);
                    let blocks = build_info_blocks(task.id, analysis_display);
                    let plan_ts = self
                        .slack
                        .post_blocks(channel, &thread_ts, &blocks, "要件定義が完成しました（自動実行されます）")
                        .await?;
                    self.db.update_plan_ts(task.id, &plan_ts)?;
                    self.db.update_status(task.id, "auto_approved")?;

                    tracing::info!(
                        "Analysis posted for task {} (auto_execute, plan_ts: {})",
                        task.asana_task_gid,
                        plan_ts
                    );
                } else {
                    // 既存フロー: ボタン付き投稿 → proposed
                    self.db.update_status(task.id, "proposed")?;

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

        // simple タスクは分解をスキップして直接 ready に
        if task.complexity.as_deref() == Some("simple") {
            tracing::info!("Task {} is simple, skipping decomposition", task.id);
            self.db.update_status(task.id, "decomposing")?;

            let auto_subtask = vec![decomposer::Subtask {
                index: 1,
                title: task.asana_task_name.clone(),
                detail: task.analysis_text.clone().unwrap_or_default(),
                depends_on: vec![],
                estimated_minutes: task.estimated_minutes.and_then(|m| u32::try_from(m).ok()),
                status: "pending".to_string(),
                started_at: None,
                completed_at: None,
                actual_minutes: None,
            }];
            let json = serde_json::to_string(&auto_subtask)?;
            self.db.update_subtasks(task.id, &json)?;
            self.db.update_status(task.id, "ready")?;

            self.slack
                .reply_thread(
                    channel,
                    thread_ts,
                    ":zap: simpleタスクのため分解をスキップしました",
                )
                .await
                .ok();

            return Ok(());
        }

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
        let (work_context, work_memory) = prepare_repo_context(base_dir, &repo_path);

        // complex タスクは max_turns を増やす
        let plan_turns = match task.complexity.as_deref() {
            Some("complex") => self.repos_config.defaults.claude_max_plan_turns.saturating_mul(2),
            _ => self.repos_config.defaults.claude_max_plan_turns,
        };

        let wc = context::WorkContext {
            repo_path: repo_path.clone(),
            max_turns: plan_turns,
            soul: context::read_soul(base_dir),
            skill: context::read_skill(base_dir),
            context: work_context,
            memory: work_memory,
        };

        // Step 3: claude -p でサブタスク生成
        let log_dir = self.log_dir();
        match decomposer::decompose_task(
            &task.asana_task_name,
            analysis,
            &wc,
            Some(&log_dir),
            &self.runner_ctx,
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

                // per-repo context にも記録
                let entry = format!(
                    "[DECOMPOSED] #{} {} ({}件のサブタスク)",
                    task.id, task.asana_task_name, subtasks.len()
                );
                if let Err(e) = context::append_repo_context(&repo_path, &entry) {
                    tracing::warn!("Failed to append to repo context: {}", e);
                }

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
        let repo_entry = task
            .repo_key
            .as_deref()
            .and_then(|key| self.repos_config.find_repo_by_key(key));

        // auto_execute リポジトリなら worktree 実行パスへ分岐
        if repo_entry.map(|r| r.auto_execute).unwrap_or(false) {
            return self.execute_in_worktree(task, repo_entry.unwrap()).await;
        }

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

        // Step 2: リポジトリパスを解決
        let repo_path = repo_entry.map(|r| self.repos_config.repo_local_path(r));

        // analysis_text をプランとして使う
        let plan_text = task.analysis_text.as_deref().unwrap_or("");
        let base_dir = &self.repos_config.defaults.repos_base_dir;
        let (work_context, work_memory) = if let Some(ref rp) = repo_path {
            prepare_repo_context(base_dir, rp)
        } else {
            (
                context::merged_context(base_dir, None),
                context::merged_memory(base_dir, None),
            )
        };
        // complex タスクは max_turns を増やす
        let execute_turns = match task.complexity.as_deref() {
            Some("complex") => self.repos_config.defaults.claude_max_execute_turns.saturating_mul(2),
            _ => self.repos_config.defaults.claude_max_execute_turns,
        };

        let wc = context::WorkContext {
            repo_path: repo_path.clone().unwrap_or_else(|| std::path::PathBuf::from(base_dir)),
            max_turns: execute_turns,
            soul: context::read_soul(base_dir),
            skill: context::read_skill(base_dir),
            context: work_context,
            memory: work_memory,
        };

        // Step 3: executor 呼び出し
        let log_dir = self.log_dir();
        let result = executor::execute_task(
            &task.asana_task_name,
            plan_text,
            repo_entry,
            repo_path.as_deref(),
            &wc,
            Some(&log_dir),
            &self.runner_ctx,
        )
        .await?;

        // Step 4: 結果を Slack に投稿
        if result.success {
            self.db.update_status(task.id, "done")?;

            // context.md に完了記録（global + per-repo）
            let base_dir = &self.repos_config.defaults.repos_base_dir;
            context::append_completed_task(base_dir, &task, repo_path.as_deref());

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
                .set_error(task.id, truncate_for_slack(&result.output, 500))?;

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

    /// worktree 隔離実行: worktree 作成 → executor → PR 作成
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

        // Step 3: status → executing
        self.db.update_status(task.id, "executing")?;
        self.slack
            .reply_thread(
                channel,
                thread_ts,
                &format!(":rocket: worktree で自動実行中... (branch: `{}`)", ws.branch_name),
            )
            .await
            .ok();

        // Step 4: executor 実行 (cwd = worktree_path)
        let plan_text = task.analysis_text.as_deref().unwrap_or("");
        let (work_context, work_memory) = prepare_repo_context(base_dir, &ws.worktree_path);
        let execute_turns = match task.complexity.as_deref() {
            Some("complex") => self
                .repos_config
                .defaults
                .claude_max_execute_turns
                .saturating_mul(2),
            _ => self.repos_config.defaults.claude_max_execute_turns,
        };

        let wc = context::WorkContext {
            repo_path: ws.worktree_path.clone(),
            max_turns: execute_turns,
            soul: context::read_soul(base_dir),
            skill: context::read_skill(base_dir),
            context: work_context,
            memory: work_memory,
        };

        let log_dir = self.log_dir();
        let result = executor::execute_task(
            &task.asana_task_name,
            plan_text,
            Some(repo_entry),
            Some(ws.worktree_path.as_path()),
            &wc,
            Some(&log_dir),
            &self.runner_ctx,
        )
        .await;

        // Step 5: 結果に応じて PR 作成 or cleanup
        match result {
            Ok(exec_result) if exec_result.success => {
                // PR 作成を試みる
                match workspace::finalize(
                    &ws,
                    &task.asana_task_name,
                    &repo_entry.default_branch,
                    &repo_entry.github,
                )
                .await
                {
                    Ok(pr_url) => {
                        self.db.update_pr_url(task.id, &pr_url)?;
                        self.db.update_status(task.id, "done")?;

                        let repo_path = self.repos_config.repo_local_path(repo_entry);
                        context::append_completed_task(base_dir, &task, Some(&repo_path));

                        let output_summary = truncate_for_slack(&exec_result.output, 2800);
                        let msg = format!(
                            ":white_check_mark: 自動実行完了 — PR を作成しました\n{}\n```\n{}\n```",
                            pr_url, output_summary
                        );
                        self.slack
                            .reply_thread(channel, thread_ts, &msg)
                            .await
                            .ok();
                    }
                    Err(e) => {
                        // PR 作成失敗（変更なし含む）
                        self.db.update_status(task.id, "done")?;

                        let repo_path = self.repos_config.repo_local_path(repo_entry);
                        context::append_completed_task(base_dir, &task, Some(&repo_path));

                        let output_summary = truncate_for_slack(&exec_result.output, 2800);
                        let msg = format!(
                            ":white_check_mark: 自動実行完了（PR作成スキップ: {}）\n```\n{}\n```",
                            e, output_summary
                        );
                        self.slack
                            .reply_thread(channel, thread_ts, &msg)
                            .await
                            .ok();
                    }
                }
            }
            Ok(exec_result) => {
                // executor 失敗
                self.db
                    .set_error(task.id, truncate_for_slack(&exec_result.output, 500))?;

                let output_summary = truncate_for_slack(&exec_result.output, 3700);
                let msg = format!(":x: 自動実行失敗\n```\n{}\n```", output_summary);
                self.slack
                    .reply_thread(channel, thread_ts, &msg)
                    .await
                    .ok();
            }
            Err(e) => {
                self.db
                    .set_error(task.id, &format!("Execution error: {}", e))?;

                let msg = format!(":x: 実行エラー\n```\n{}\n```", e);
                self.slack
                    .reply_thread(channel, thread_ts, &msg)
                    .await
                    .ok();
            }
        }

        // Step 6: worktree cleanup（PR push 済みなので不要）
        workspace::remove(&ws).await.ok();

        Ok(())
    }
}

/// Block Kit の要件定義表示ブロック（承認はスレッド返信で行う）
fn build_proposal_blocks(_task_id: i64, analysis_text: &str) -> serde_json::Value {
    serde_json::json!([
        {
            "type": "section",
            "text": {
                "type": "mrkdwn",
                "text": format!(":clipboard: *要件定義*\n\n{}", analysis_text)
            }
        },
        {
            "type": "context",
            "elements": [
                {
                    "type": "mrkdwn",
                    "text": "スレッドに返信して操作: `ok` 承認 / `go` 即実行 / `ng` 却下 / `再生成` やり直し"
                }
            ]
        }
    ])
}

/// Block Kit の情報表示ブロック（ボタンなし、auto_execute 用）
fn build_info_blocks(_task_id: i64, analysis_text: &str) -> serde_json::Value {
    serde_json::json!([
        {
            "type": "section",
            "text": {
                "type": "mrkdwn",
                "text": format!(":clipboard: *要件定義*\n\n{}", analysis_text)
            }
        },
        {
            "type": "context",
            "elements": [
                {
                    "type": "mrkdwn",
                    "text": ":gear: auto_execute が有効なため、worktree で自動実行されます"
                }
            ]
        }
    ])
}

/// リポジトリの初期セットアップ + merged context/memory を返す
fn prepare_repo_context(base_dir: &str, repo_path: &Path) -> (String, String) {
    // .agent/ ディレクトリ作成（create_dir_all は冪等）
    let agent_dir = repo_path.join(".agent");
    if let Err(e) = std::fs::create_dir_all(&agent_dir) {
        tracing::warn!("Failed to create repo .agent dir: {}", e);
    }

    // .claude/rules/agent.md が無ければデフォルトルールを生成
    ensure_repo_agent_rules(repo_path);

    (
        context::merged_context(base_dir, Some(repo_path)),
        context::merged_memory(base_dir, Some(repo_path)),
    )
}

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

- CLAUDE.md に記載されたプロジェクト規約に従うこと
- 既存のコードパターン・命名規則・ディレクトリ構造を尊重すること
- スコープ外の変更は禁止（依頼された範囲のみ変更すること）
- 変更後はテストを実行して通ることを確認すること
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
