use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::Notify;

use crate::db::{CodingTask, Db};
use crate::google::calendar::GoogleCalendarClient;
use crate::repo_config::ReposConfig;
use crate::slack::client::SlackClient;

use super::{analyzer, context, executor, priority, scheduler, task_file, workflow, workspace};

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

        loop {
            let mut had_error = false;

            // タスク処理（個別に spawn、ループはブロックしない）
            had_error |= worker.process_tasks();

            // スケジューラージョブチェック（軽量なので直接 await）
            had_error |= worker.run_scheduler().await;

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

        // 1. new → planning（spawn 前にステータスを claim して二重 pickup 防止）
        match self.db.get_new_task() {
            Ok(Some(task)) => {
                tracing::info!("Planning task: {} ({})", task.asana_task_name, task.asana_task_gid);
                if self.db.update_status(task.id, "planning").is_ok() {
                    let w = Arc::clone(self);
                    tokio::spawn(async move {
                        if let Err(e) = w.plan_task(task).await {
                            tracing::error!("Task planning failed: {}", e);
                        }
                    });
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::error!("Failed to fetch new task: {}", e);
                had_error = true;
            }
        }

        // 2. approved → executing（手動承認後、--resume で実行）
        match self.db.get_approved_task() {
            Ok(Some(task)) => {
                tracing::info!("Executing approved task: {} ({})", task.asana_task_name, task.asana_task_gid);
                if self.db.update_status(task.id, "executing").is_ok() {
                    let w = Arc::clone(self);
                    tokio::spawn(async move {
                        if let Err(e) = w.execute_approved_task(task).await {
                            tracing::error!("Approved task execution failed: {}", e);
                        }
                    });
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

        // 3. auto_approved → executing（自動実行、--resume で実行）
        match self.db.get_auto_approved_task() {
            Ok(Some(task)) => {
                tracing::info!("Auto-executing task: {} ({})", task.asana_task_name, task.asana_task_gid);
                if self.db.update_status(task.id, "executing").is_ok() {
                    let w = Arc::clone(self);
                    tokio::spawn(async move {
                        if let Err(e) = w.execute_approved_task(task).await {
                            tracing::error!("Auto-execution failed: {}", e);
                        }
                    });
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::error!("Failed to fetch auto_approved task: {}", e);
                had_error = true;
            }
        }

        // 4. ci_pending タスク → CI 結果確認 → done or リトライ
        match self.db.get_ci_pending_task() {
            Ok(Some(task)) => {
                tracing::debug!("Checking CI for task: {} ({})", task.asana_task_name, task.id);
                let w = Arc::clone(self);
                tokio::spawn(async move {
                    if let Err(e) = w.check_ci_and_handle(task).await {
                        tracing::error!("CI check failed: {}", e);
                    }
                });
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

    /// new → planning → proposed/auto_approved: 実装計画を生成して Slack 投稿
    async fn plan_task(&self, task: CodingTask) -> Result<()> {
        let channel = task
            .slack_channel
            .as_deref()
            .unwrap_or(&self.default_slack_channel);

        // Step 1: Slack 親メッセージ送信（再生成時は既存スレッドを再利用）
        let thread_ts = if let Some(ref existing_ts) = task.slack_thread_ts {
            self.slack
                .reply_thread(channel, existing_ts, ":arrows_counterclockwise: 計画を再生成中...")
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

        // Step 2: リポジトリパスを解決（status は spawn 前に "planning" に更新済み）
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

        // Step 4: claude -p で実装計画生成（Plan mode）
        let notes = task.description.as_deref().unwrap_or("");
        self.slack
            .reply_thread(channel, &thread_ts, ":brain: 実装計画を作成中...")
            .await
            .ok();

        let base_dir = &self.repos_config.defaults.repos_base_dir;
        let (work_context, work_memory) = prepare_repo_context(base_dir, &repo_path);
        let wc = context::WorkContext {
            repo_path: repo_path.clone(),
            max_turns: self.repos_config.defaults.claude_max_plan_turns,
            soul: context::merged_soul(base_dir, Some(&repo_path)),
            skill: context::read_skill(base_dir),
            context: work_context,
            memory: work_memory,
        };

        let log_dir = self.log_dir();
        match analyzer::plan_task(
            &task.asana_task_name,
            notes,
            &wc,
            Some(&log_dir),
            &self.runner_ctx,
        )
        .await
        {
            Ok(plan_result) => {
                self.db.update_analysis(task.id, &plan_result.plan_text)?;
                if let Some(ref c) = plan_result.complexity {
                    self.db.update_complexity(task.id, c)?;
                    tracing::info!("Task {} complexity: {}", task.id, c);
                }

                // session_id を保存（--resume で Act mode に使う）
                if let Some(ref sid) = plan_result.session_id {
                    self.db.update_session_id(task.id, sid)?;
                    tracing::info!("Task {} session_id saved: {}", task.id, sid);
                }

                // auto_execute 判定
                let is_auto_execute = task
                    .repo_key
                    .as_deref()
                    .and_then(|key| self.repos_config.find_repo_by_key(key))
                    .map(|r| r.auto_execute)
                    .unwrap_or(false);

                if is_auto_execute {
                    // ボタンなしで情報投稿 → auto_approved へ（即実行）
                    let plan_display = truncate_for_slack(&plan_result.plan_text, 2800);
                    let blocks = build_info_blocks(plan_display);
                    let plan_ts = self
                        .slack
                        .post_blocks(channel, &thread_ts, &blocks, "実装計画が完成しました（自動実行されます）")
                        .await?;
                    self.db.update_plan_ts(task.id, &plan_ts)?;
                    self.db.update_status(task.id, "auto_approved")?;

                    tracing::info!(
                        "Plan posted for task {} (auto_execute, plan_ts: {})",
                        task.asana_task_gid,
                        plan_ts
                    );
                } else {
                    // 承認待ち: ボタン付き投稿 → proposed
                    self.db.update_status(task.id, "proposed")?;

                    let plan_display = truncate_for_slack(&plan_result.plan_text, 2800);
                    let blocks = build_proposal_blocks(plan_display);
                    let plan_ts = self
                        .slack
                        .post_blocks(channel, &thread_ts, &blocks, "実装計画が完成しました（操作してください）")
                        .await?;
                    self.db.update_plan_ts(task.id, &plan_ts)?;

                    tracing::info!(
                        "Plan posted for task {} (plan_ts: {})",
                        task.asana_task_gid,
                        plan_ts
                    );
                }
            }
            Err(e) => {
                let err_msg = format!("Planning failed: {}", e);
                self.db.set_error(task.id, &err_msg)?;
                self.slack
                    .reply_thread(
                        channel,
                        &thread_ts,
                        &format!(":x: 実装計画の作成に失敗しました\n```\n{}\n```", e),
                    )
                    .await
                    .ok();
                tracing::error!("{}", err_msg);
            }
        }

        Ok(())
    }

    /// approved/auto_approved → executing → ci_pending/done: Plan mode の続きを Act mode で実行
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

        if result.success {
            self.db.update_status(task.id, "done")?;
            context::append_completed_task(base_dir, &task, None);

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
                ":x: 実行失敗\n```\n{}\n```",
                output_summary
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
                // finalize_worktree が remove まで担当
                self.finalize_worktree(task, repo_entry, ws).await?;
            }
            Ok(exec_result) => {
                self.db
                    .set_error(task.id, truncate_for_slack(&exec_result.output, 500))?;
                let output_summary = truncate_for_slack(&exec_result.output, 3700);
                let msg = format!(":x: 実行失敗\n```\n{}\n```", output_summary);
                self.slack
                    .reply_thread(channel, thread_ts, &msg)
                    .await
                    .ok();
                workspace::remove(ws).await.ok();
            }
            Err(e) => {
                self.db
                    .set_error(task.id, &format!("Execution error: {}", e))?;
                let msg = format!(":x: 実行エラー\n```\n{}\n```", e);
                self.slack
                    .reply_thread(channel, thread_ts, &msg)
                    .await
                    .ok();
                workspace::remove(ws).await.ok();
            }
        }
        Ok(())
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
                let msg = format!(":gear: PR を作成しました — CI 結果を監視中...\n{}", pr_url);
                self.slack
                    .reply_thread(channel, thread_ts, &msg)
                    .await
                    .ok();
            }
            Err(e) => {
                self.db.update_status(task.id, "done")?;
                let repo_path = self.repos_config.repo_local_path(repo_entry);
                context::append_completed_task(base_dir, task, Some(&repo_path));
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
                context::append_completed_task(base_dir, &task, Some(&repo_path));

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
                context::append_completed_task(base_dir, &task, Some(&repo_path));

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
                        ":x: CI 失敗（リトライ上限 {} 回に到達）\n```\n{}\n```",
                        max_retry, summary
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
                        &format!(":x: CI 修正の実行中にエラー: {}", e),
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

/// Block Kit の計画表示ブロック（承認はスレッド返信で行う）
fn build_proposal_blocks(plan_text: &str) -> serde_json::Value {
    serde_json::json!([
        {
            "type": "section",
            "text": {
                "type": "mrkdwn",
                "text": format!(":clipboard: *実装計画*\n\n{}", plan_text)
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
fn build_info_blocks(plan_text: &str) -> serde_json::Value {
    serde_json::json!([
        {
            "type": "section",
            "text": {
                "type": "mrkdwn",
                "text": format!(":clipboard: *実装計画*\n\n{}", plan_text)
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

/// リポジトリの初期セットアップ + merged context/memory を返す
fn prepare_repo_context(base_dir: &str, repo_path: &Path) -> (String, String) {
    setup_repo_dirs(repo_path);
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

## 基本原則
- CLAUDE.md に記載されたプロジェクト規約に従うこと
- 既存のコードパターン・命名規則・ディレクトリ構造を尊重すること
- スコープ外の変更は禁止（依頼された範囲のみ変更すること）
- 変更後はテストを実行して通ることを確認すること

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

