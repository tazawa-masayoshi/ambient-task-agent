use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::{Datelike, DateTime, Timelike, Utc};
use tokio::sync::Notify;

use crate::db::{CodingTask, Db};
use crate::google::calendar::GoogleCalendarClient;
use crate::repo_config::ReposConfig;
use crate::slack::client::SlackClient;

use super::classify::{TaskClassification, classify_new_task_heuristic};
use super::{context, executor, priority, scheduler, task_file, workflow, workspace};

/// ハートビート間隔の下限
const MIN_HEARTBEAT_SECS: u64 = 10;

pub struct Worker {
    pub(crate) db: Db,
    pub(crate) repos_config: ReposConfig,
    pub(crate) slack: SlackClient,
    pub(crate) asana_pat: String,
    pub(crate) asana_project_id: String,
    pub(crate) asana_user_name: String,
    pub(crate) google_calendar: tokio::sync::Mutex<Option<GoogleCalendarClient>>,
    pub(crate) default_slack_channel: String,
    pub(crate) notify: Arc<Notify>,
    pub(crate) runner_ctx: crate::execution::RunnerContext,
    /// spawn 中のタスク ID セット（二重実行防止）
    pub(crate) running_tasks: Arc<std::sync::Mutex<HashSet<i64>>>,
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
            running_tasks: Arc::new(std::sync::Mutex::new(HashSet::new())),
        }
    }

    /// 実行ログの出力先ディレクトリ
    pub(crate) fn log_dir(&self) -> PathBuf {
        PathBuf::from(&self.repos_config.defaults.repos_base_dir)
            .join(".agent")
            .join("logs")
    }

    /// リポジトリパスを解決（共通ヘルパー）
    pub(crate) fn resolve_repo_path(&self, task: &CodingTask) -> Result<std::path::PathBuf> {
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

        // 2.5 executing タスクを拾って実行（classify_and_dispatch で executing に設定済み）
        match self.db.get_task_by_status("executing") {
            Ok(Some(task)) if task.started_at_task.is_none() => {
                // started_at が未設定 = まだ実行開始していない
                tracing::info!("Executing task: {} ({})", task.asana_task_name, task.asana_task_gid);
                let task_id = task.id;
                self.spawn_task(task_id, |w| async move { w.execute_task(task).await });
            }
            Ok(_) => {}
            Err(e) => {
                tracing::error!("Failed to fetch executing task: {}", e);
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

    // process_ops_queue は runner_ops.rs に移動

    // run_ops_item 〜 route_ops は runner_ops.rs に移動済み
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
    pub(crate) async fn execute_task(&self, task: CodingTask) -> Result<()> {
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
            // MEMORY 永続化（成功時のみ — 失敗時は文脈欠落による汚染リスクがあるため除外）
            self.persist_learnings(&result.output, &task, None);
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
                // ブロッカー検知: executing → conversing に遷移（persist_learnings は呼ばない）
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

                // MEMORY 永続化（ブロッカーなし・成功時のみ — 途中出力の汚染防止）
                self.persist_learnings(&exec_result.output, task, Some(repo_entry));

                // git-ratchet: 品質メトリクスが悪化していないか検証
                if let Err(ratchet_err) = super::ratchet::quality_ratchet_check(&ws.worktree_path).await {
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
                // 失敗時は persist_learnings を呼ばない（文脈欠落した MEMORY: による汚染防止）
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

    /// タスク処理を spawn し、panic/エラー時に DB を error 状態に復帰させる。
    /// 同一 task_id が既に実行中なら spawn しない（二重実行防止）。
    /// Drop ガードにより panic 時も running_tasks から確実に除去される。
    pub(crate) fn spawn_task<F, Fut>(self: &Arc<Self>, task_id: i64, f: F)
    where
        F: FnOnce(Arc<Worker>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<()>> + Send,
    {
        {
            let mut running = self.running_tasks.lock().unwrap();
            if !running.insert(task_id) {
                tracing::debug!("Task {} already running, skipping spawn", task_id);
                return;
            }
        }
        let w = Arc::clone(self);
        let db = self.db.clone();
        tokio::spawn(async move {
            // Drop ガード: panic 時も running_tasks から task_id を確実に除去
            let _guard = RunningTaskGuard {
                set: Arc::clone(&w.running_tasks),
                task_id,
            };
            match f(Arc::clone(&w)).await {
                Ok(()) => {}
                Err(e) => {
                    tracing::error!("Task {} failed: {}", task_id, e);
                    db.set_error(task_id, &format!("Task failed: {}", e)).ok();
                }
            }
        });
    }

    /// WORKFLOW.md → defaults → complex*2 の順で max_turns を解決
    pub(crate) fn resolve_execute_turns(&self, worktree_path: &Path, complexity: Option<&str>) -> u32 {
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
    pub(crate) fn build_worktree_context(
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
                super::ratchet::update_quality_baseline(&ws.worktree_path).await;
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
pub(crate) fn count_business_days(from: DateTime<Utc>, to: DateTime<Utc>) -> i64 {
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

pub(crate) const ERROR_LOG_HINT: &str = "\n_詳細ログ: `journalctl --user -u sdtab-ambient-task-agent -n 50`_";

// ============================================================================
// Block Kit ヘルパー（conversing / manual）
// ============================================================================

/// ops 出力から Slack 向けサマリを抽出
/// 「作業結果まとめ」「結果」セクションがあればそこだけ返す。なければ全文。
pub(crate) fn extract_slack_summary(output: &str) -> &str {
    // 「作業結果まとめ」「**作業結果」「## 結果」など
    let markers = ["作業結果まとめ", "作業結果:", "**作業結果", "## 結果", "## まとめ"];
    for marker in markers {
        if let Some(pos) = output.find(marker) {
            return output[pos..].trim();
        }
    }
    output
}

/// conversing 状態のボタンレイアウト: [実行開始] [指示追加] [手動修正] [スキップ]
pub(crate) fn build_conversing_blocks(task_id: i64, message: &str) -> serde_json::Value {
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
/// spawn_task の Drop ガード。panic 時も running_tasks から task_id を確実に除去する。
/// Mutex が poisoned でも除去を試みる（into_inner で回復）。
struct RunningTaskGuard {
    set: Arc<std::sync::Mutex<HashSet<i64>>>,
    task_id: i64,
}

impl Drop for RunningTaskGuard {
    fn drop(&mut self) {
        match self.set.lock() {
            Ok(mut set) => { set.remove(&self.task_id); }
            Err(poisoned) => { poisoned.into_inner().remove(&self.task_id); }
        }
    }
}
