use anyhow::Result;
use chrono::{Local, Utc};
use cron::Schedule;
use std::path::PathBuf;
use std::str::FromStr;

use crate::claude::ClaudeRunner;
use crate::db::{Db, ScheduledJob};
use crate::google::calendar::GoogleCalendarClient;
use crate::repo_config::ReposConfig;
use crate::slack::client::SlackClient;
use crate::sync::{load_cache, TasksCache};

use super::context;

/// スケジューラ固有のルール
const SCHEDULER_RULES: &str = "\
## 出力フォーマット
- Slack mrkdwn で日本語出力。見出し（*太字*）・箇条書き・テーブルを活用
- 数値は具体的に。「いくつか」ではなく「3件」
- 全体で Slack 1メッセージに収まる分量（長くても2000文字以内）
- アクション提案は「何を → いつまでに」を含める
- 分析は結論ファーストで、根拠を箇条書きで補足";

/// soul.md が無い場合のフォールバック
const PM_FALLBACK_SOUL: &str =
    "あなたはサーバント型PMです。チームの成果を最大化するため裏方として支援します。";

fn build_scheduler_system_prompt(ctx: &SchedulerContext) -> String {
    context::build_system_prompt(&ctx.soul, PM_FALLBACK_SOUL, SCHEDULER_RULES, &ctx.skill, None)
}

/// スケジューラが各ジョブ実行時に使うコンテキスト
pub struct SchedulerContext {
    pub db: Db,
    pub slack: SlackClient,
    pub asana_pat: String,
    pub asana_project_id: String,
    pub asana_user_name: String,
    pub google_calendar: Option<GoogleCalendarClient>,
    pub repos_base_dir: String,
    pub stagnation_threshold_hours: i64,
    pub soul: String,
    pub skill: String,
    pub log_dir: PathBuf,
    pub runner_ctx: crate::execution::RunnerContext,
}

/// repos.toml のスケジュール設定を DB に反映（起動時に呼ぶ）
pub fn seed_schedules(db: &Db, config: &ReposConfig) -> Result<()> {
    let default_channel = &config.defaults.slack_channel;

    for entry in &config.schedule {
        if Schedule::from_str(&entry.cron).is_err() {
            tracing::warn!("Invalid cron expression for {}: {}", entry.key, entry.cron);
            continue;
        }

        let next = compute_next_run(&entry.cron);
        let next_str = next.as_deref();
        let channel = entry.slack_channel.as_deref().unwrap_or(default_channel);

        db.upsert_scheduled_job(
            &entry.key,
            &entry.cron,
            &entry.job_type,
            &entry.prompt,
            channel,
            next_str,
        )?;

        tracing::info!(
            "Scheduled job registered: {} (next: {})",
            entry.key,
            next_str.unwrap_or("none")
        );
    }

    Ok(())
}

fn compute_next_run(cron_expr: &str) -> Option<String> {
    let schedule = Schedule::from_str(cron_expr).ok()?;
    let next = schedule.upcoming(Local).next()?;
    Some(
        next.with_timezone(&Utc)
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string(),
    )
}

/// 期限の来たスケジュールジョブがあれば実行
///
/// Pre-advance scheduling: next_run を実行「前」に更新する（OpenFang パターン）。
/// これにより、遅い実行が次のティックと重なっても二重発火しない。
pub async fn check_and_run(ctx: &mut SchedulerContext) -> Result<()> {
    let now = Utc::now();

    while let Some(job) = ctx.db.get_due_job(&now)? {
        tracing::info!(
            "Running scheduled job: {} (type: {})",
            job.job_key,
            job.job_type
        );

        // Pre-advance: 実行前に next_run を更新（二重発火防止）
        let next = compute_next_run(&job.schedule_cron).unwrap_or_default();
        ctx.db.mark_job_run(job.id, &next)?;

        if let Err(e) = execute_job(&job, ctx).await {
            tracing::error!("Scheduled job {} failed: {}", job.job_key, e);
        }

        tracing::info!("Job {} done. Next run: {}", job.job_key, next);
    }

    Ok(())
}

async fn execute_job(job: &ScheduledJob, ctx: &mut SchedulerContext) -> Result<()> {
    match job.job_type.as_str() {
        "morning_briefing" => run_morning_briefing(job, ctx).await,
        "evening_summary" => run_evening_summary(job, ctx).await,
        "meeting_reminder" => run_meeting_reminder(job, ctx).await,
        "stagnation_check" => run_stagnation_check(job, ctx).await,
        "weekly_pm_review" => run_weekly_pm_review(job, ctx).await,
        "self_improvement" => run_self_improvement(job, ctx).await,
        other => {
            tracing::warn!("Unknown job type: {}", other);
            Ok(())
        }
    }
}

// ============================================================================
// Morning Briefing (AI化)
// ============================================================================

async fn run_morning_briefing(job: &ScheduledJob, ctx: &mut SchedulerContext) -> Result<()> {
    // 1. Asana 同期（最新データ取得）
    let asana_config = crate::config::AsanaConfig {
        pat: ctx.asana_pat.clone(),
        project_id: ctx.asana_project_id.clone(),
        user_name: ctx.asana_user_name.clone(),
    };
    if let Err(e) = crate::sync::run_sync(&asana_config).await {
        tracing::warn!("Asana sync failed before morning briefing: {}", e);
    }

    // 2. タスクキャッシュ読み込み
    let cache = match load_cache() {
        Ok(c) => c,
        Err(_) => {
            ctx.slack
                .post_message(
                    &job.slack_channel,
                    ":warning: 朝のブリーフィング: タスクキャッシュが見つかりません。`sync` を実行してください。",
                )
                .await?;
            return Ok(());
        }
    };

    // 3. GCal イベント取得
    let events = fetch_today_events_safe(&mut ctx.google_calendar).await;

    // 4. プロンプト構築 → claude -p
    let tasks_text = format_tasks_for_prompt(&cache);
    let events_text = format_events_for_prompt(&events);
    let today = Local::now().format("%Y-%m-%d (%A)").to_string();

    // 作業履歴・メモを注入
    let work_context = context::read_context(&ctx.repos_base_dir);
    let work_memory = context::read_memory(&ctx.repos_base_dir);

    let mut context_parts = vec![
        format!("## 日付\n{}", today),
        format!("## Asanaタスク\n{}", tasks_text),
        format!("## 今日のカレンダー\n{}", events_text),
    ];
    if !work_context.is_empty() {
        context_parts.push(format!("## 直近の作業履歴\n{}", work_context));
    }
    if !work_memory.is_empty() {
        context_parts.push(format!("## 過去の学び・メモ\n{}", work_memory));
    }

    // アクティブタスクを1回取得し、停滞チェックと見積もり情報の両方に使う
    let threshold = ctx.stagnation_threshold_hours;
    let active_tasks = ctx.db.get_active_tasks().unwrap_or_default();

    // 停滞タスク情報を追加（active の中から threshold 超過を抽出）
    let stagnant_cutoff = (chrono::Utc::now() - chrono::Duration::hours(threshold))
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string();
    let stagnant_lines: Vec<String> = active_tasks
        .iter()
        .filter(|t| {
            matches!(t.status.as_str(), "ready" | "in_progress") && t.updated_at < stagnant_cutoff
        })
        .map(|t| format!("- #{} {} (status: {}, 最終更新: {})", t.id, t.asana_task_name, t.status, t.updated_at))
        .collect();
    if !stagnant_lines.is_empty() {
        context_parts.push(format!(
            "## 停滞タスク（{}時間以上未更新）\n{}",
            threshold,
            stagnant_lines.join("\n")
        ));
    }

    // タイムボクシング: 空き時間にタスクを自動配置
    let work_start = chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap();
    let work_end = chrono::NaiveTime::from_hms_opt(18, 0, 0).unwrap();
    let free_slots = compute_free_slots(&events, work_start, work_end);
    let total_free_minutes: u32 = free_slots.iter().map(|s| s.duration_minutes).sum();
    let mtg_minutes = 540u32.saturating_sub(total_free_minutes);
    let mut timebox_tasks = build_timebox_tasks(&cache, &active_tasks);
    let (timeline_text, allocated_minutes, unallocated_count, allocated_tasks) =
        build_timeboxing_timeline(&events, &free_slots, &mut timebox_tasks);

    // カレンダーにタスクブロックを登録
    if let Some(gcal) = ctx.google_calendar.as_mut() {
        let today_date = Local::now().date_naive();
        let tz = Local::now().timezone();

        // 前回の配置を削除（プレフィックスで識別）
        const TASK_EVENT_PREFIX: &str = "📋 ";
        match gcal.delete_events_by_summary_prefix(TASK_EVENT_PREFIX).await {
            Ok(n) if n > 0 => tracing::info!("Deleted {} old task events from calendar", n),
            Ok(_) => {}
            Err(e) => tracing::warn!("Failed to cleanup old task events: {}", e),
        }

        // 配置済みタスクをカレンダーに登録
        let mut created = 0u32;
        for task in &allocated_tasks {
            let Some(start_dt) = today_date
                .and_time(task.start)
                .and_local_timezone(tz)
                .earliest()
                .map(|dt| dt.with_timezone(&Utc))
            else {
                tracing::warn!("Skipping calendar event for '{}': ambiguous local time", task.name);
                continue;
            };
            let Some(end_dt) = today_date
                .and_time(task.end)
                .and_local_timezone(tz)
                .earliest()
                .map(|dt| dt.with_timezone(&Utc))
            else {
                tracing::warn!("Skipping calendar event for '{}': ambiguous end time", task.name);
                continue;
            };

            let summary = format!("{}{}", TASK_EVENT_PREFIX, task.name);
            let dur = (task.end - task.start).num_minutes();
            let desc = format!("自動配置 by ambient-task-agent ({}分)", dur);

            match gcal.create_event(&summary, start_dt, end_dt, Some(&desc)).await {
                Ok(_) => created += 1,
                Err(e) => tracing::warn!("Failed to create calendar event for '{}': {}", task.name, e),
            }
        }

        if created > 0 {
            tracing::info!("Created {} task events on calendar", created);
        }
    }

    context_parts.push(format!(
        "## 今日のキャパシティ\n\
         作業可能: {}h{}m / MTG: {}h{}m / タスク配置済み: {}h{}m（70%ルール適用、30%バッファ）",
        total_free_minutes / 60, total_free_minutes % 60,
        mtg_minutes / 60, mtg_minutes % 60,
        allocated_minutes / 60, allocated_minutes % 60,
    ));

    context_parts.push(format!(
        "## タイムボクシング（自動配置済み）\n{}",
        timeline_text
    ));

    // 停滞タスクのうち未配置のものを補足
    if unallocated_count > 0 {
        context_parts.push(format!(
            "_※ {}件のタスクが時間不足で未配置_",
            unallocated_count
        ));
    }

    let context_block = context_parts.join("\n\n");

    let system_prompt = build_scheduler_system_prompt(ctx);

    let prompt = if job.prompt_template.is_empty() {
        format!(
            "## 指示: 朝のブリーフィング作成\n\n\
             以下のデータには、自動配置済みのタイムボクシングが含まれています。\n\
             あなたの役割は計画のレビューと補足コメントです。\n\n\
             ### やること\n\
             1. タイムボクシングをそのまま出力に含める（変更不要なら変えない）\n\
             2. 配置に問題がある場合のみ、調整案をコメントで提示\n\
             3. 以下のセクションを追加:\n\n\
             *:rotating_light: 即対応*（期限超過・ブロッカーがあれば）\n\
             • タスク名 — 推奨アクション\n\n\
             *:calendar: 今日のタイムボクシング*\n\
             上記の自動配置をそのまま貼り付け。調整があればコメントを添える。\n\n\
             *:construction: 停滞タスク診断*（あれば）\n\
             • タスク名 — [Tiger/Paper Tiger/Elephant] 推奨アクション\n\n\
             *:bulb: 今日のフォーカス*\n\
             一言で今日の最重要ゴールを宣言\n\n\
             {context_block}"
        )
    } else {
        format!("{}\n\n{}", job.prompt_template, context_block)
    };

    match ClaudeRunner::new("scheduler:morning_briefing", &prompt)
        .system_prompt(&system_prompt)
        .max_turns(3)
        .log_dir(&ctx.log_dir)
        .with_context(&ctx.runner_ctx)
        .run()
        .await
    {
        Ok(result) if result.success => {
            ctx.slack
                .post_message(&job.slack_channel, &result.stdout)
                .await?;
        }
        Ok(result) => {
            tracing::error!("AI morning briefing failed: {}", result.stderr);
            let today_str = Local::now().format("%Y-%m-%d").to_string();
            let message = build_morning_message(&cache, &today_str, &events);
            ctx.slack
                .post_message(&job.slack_channel, &message)
                .await?;
        }
        Err(e) => {
            tracing::error!("AI morning briefing failed: {}", e);
            let today_str = Local::now().format("%Y-%m-%d").to_string();
            let message = build_morning_message(&cache, &today_str, &events);
            ctx.slack
                .post_message(&job.slack_channel, &message)
                .await?;
        }
    }

    Ok(())
}

// ============================================================================
// Evening Summary (AI化)
// ============================================================================

async fn run_evening_summary(job: &ScheduledJob, ctx: &mut SchedulerContext) -> Result<()> {
    // Asana 同期
    let asana_config = crate::config::AsanaConfig {
        pat: ctx.asana_pat.clone(),
        project_id: ctx.asana_project_id.clone(),
        user_name: ctx.asana_user_name.clone(),
    };
    if let Err(e) = crate::sync::run_sync(&asana_config).await {
        tracing::warn!("Asana sync failed before evening summary: {}", e);
    }

    let cache = match load_cache() {
        Ok(c) => c,
        Err(_) => {
            ctx.slack
                .post_message(
                    &job.slack_channel,
                    ":warning: 夕方サマリー: タスクキャッシュが見つかりません。",
                )
                .await?;
            return Ok(());
        }
    };

    let events = fetch_today_events_safe(&mut ctx.google_calendar).await;

    let tasks_text = format_tasks_for_prompt(&cache);
    let events_text = format_events_for_prompt(&events);
    let today = Local::now().format("%Y-%m-%d (%A)").to_string();

    // 作業履歴・メモを注入
    let work_context = context::read_context(&ctx.repos_base_dir);
    let work_memory = context::read_memory(&ctx.repos_base_dir);

    let mut context_parts = vec![
        format!("## 日付\n{}", today),
        format!("## Asanaタスク\n{}", tasks_text),
        format!("## 今日のカレンダー\n{}", events_text),
    ];
    if !work_context.is_empty() {
        context_parts.push(format!("## 直近の作業履歴\n{}", work_context));
    }
    if !work_memory.is_empty() {
        context_parts.push(format!("## 過去の学び・メモ\n{}", work_memory));
    }
    let context_block = context_parts.join("\n\n");

    let system_prompt = build_scheduler_system_prompt(ctx);

    let prompt = if job.prompt_template.is_empty() {
        format!(
            "## 指示: 夕方の振り返り作成\n\n\
             以下のデータを分析し、本日の振り返りを作成してください。\n\n\
             ### Step 1: 実績評価\n\
             タスクの状態変化とカレンダーから、今日の成果を評価:\n\
             - 完了できたタスクは何か\n\
             - 進行中で進捗があったものは何か\n\
             - 手つかずで残ったものはなぜか\n\n\
             ### Step 2: 以下のフォーマットで出力\n\n\
             *:white_check_mark: 今日の成果*\n\
             • 完了タスクを箇条書き（なければ「完了タスクなし」）\n\n\
             *:arrow_forward: 進行中*\n\
             • タスク名 — 進捗メモ（推測で構わない）\n\n\
             *:next_track_button: 明日への引き継ぎ*\n\
             | タスク | 優先度 | 推奨アクション |\n\
             |--------|--------|----------------|\n\
             明日最初に着手すべきものを上に配置\n\n\
             *:brain: 気づき*\n\
             今日のパターンから見える改善ポイント1つ（停滞傾向、見積もり精度、割り込み多さ等）\n\n\
             {context_block}"
        )
    } else {
        format!("{}\n\n{}", job.prompt_template, context_block)
    };

    match ClaudeRunner::new("scheduler:evening_summary", &prompt)
        .system_prompt(&system_prompt)
        .max_turns(3)
        .log_dir(&ctx.log_dir)
        .with_context(&ctx.runner_ctx)
        .run()
        .await
    {
        Ok(result) if result.success => {
            ctx.slack
                .post_message(&job.slack_channel, &result.stdout)
                .await?;
        }
        Ok(result) => {
            tracing::error!("AI evening summary failed: {}", result.stderr);
            let today_str = Local::now().format("%Y-%m-%d").to_string();
            let message = build_evening_message(&cache, &today_str);
            ctx.slack
                .post_message(&job.slack_channel, &message)
                .await?;
        }
        Err(e) => {
            tracing::error!("AI evening summary failed: {}", e);
            let today_str = Local::now().format("%Y-%m-%d").to_string();
            let message = build_evening_message(&cache, &today_str);
            ctx.slack
                .post_message(&job.slack_channel, &message)
                .await?;
        }
    }

    Ok(())
}

// ============================================================================
// Meeting Reminder
// ============================================================================

async fn run_meeting_reminder(
    job: &ScheduledJob,
    ctx: &mut SchedulerContext,
) -> Result<()> {
    use chrono::Duration;

    let gcal = match ctx.google_calendar.as_mut() {
        Some(c) => c,
        None => {
            tracing::debug!("Google Calendar not configured, skipping meeting reminder");
            return Ok(());
        }
    };

    let events = match gcal.fetch_today_events().await {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("Failed to fetch calendar events for meeting reminder: {}", e);
            return Ok(());
        }
    };

    let now = Utc::now();
    let remind_window = Duration::minutes(15);
    let today_str = Local::now().format("%Y-%m-%d").to_string();

    for event in &events {
        if event.is_all_day() {
            continue;
        }

        let start = match event.start_time() {
            Some(t) => t,
            None => continue,
        };

        let until_start = start - now;
        if until_start < Duration::zero() || until_start > remind_window {
            continue;
        }

        // 重複チェック
        if ctx.db.is_meeting_reminded(&event.id, &today_str)? {
            continue;
        }

        // Slack 通知
        let summary = event.summary.as_deref().unwrap_or("(無題)");
        let start_local = start
            .with_timezone(&Local)
            .format("%H:%M")
            .to_string();

        let meet_info = event
            .meet_link()
            .map(|url| format!("\n:video_camera: <{}|参加する>", url))
            .unwrap_or_default();

        let message = format!(
            ":bell: *MTGリマインド*\n>{} ({}〜){}\n_あと{}分で開始_",
            summary,
            start_local,
            meet_info,
            until_start.num_minutes(),
        );

        ctx.slack
            .post_message(&job.slack_channel, &message)
            .await?;

        ctx.db.mark_meeting_reminded(&event.id, &today_str)?;
        tracing::info!("Meeting reminder sent: {} at {}", summary, start_local);
    }

    // 古いリマインド記録を掃除（7日以上前）
    ctx.db.cleanup_old_reminders()?;

    Ok(())
}

// ============================================================================
// Helpers
// ============================================================================

use crate::google::calendar::CalendarEvent;

// ============================================================================
// Free Slots (空き時間計算)
// ============================================================================

#[derive(Debug, Clone)]
struct FreeSlot {
    start: chrono::NaiveTime,
    end: chrono::NaiveTime,
    duration_minutes: u32,
}

/// カレンダーイベントから作業可能な空き時間スロットを計算
fn compute_free_slots(
    events: &[CalendarEvent],
    work_start: chrono::NaiveTime,
    work_end: chrono::NaiveTime,
) -> Vec<FreeSlot> {
    // 終日イベントを除外し、時間指定イベントの開始・終了時刻を収集
    // start_time() は CalendarEvent の既存メソッドを再利用
    let mut busy_periods: Vec<(chrono::NaiveTime, chrono::NaiveTime)> = events
        .iter()
        .filter(|e| !e.is_all_day())
        .filter_map(|e| {
            let start_local = e.start_time()?.with_timezone(&Local).time();
            let end_local = e.end_time()?.with_timezone(&Local).time();
            Some((start_local, end_local))
        })
        .collect();

    // 開始時刻でソート
    busy_periods.sort_by_key(|(s, _)| *s);

    let mut slots = Vec::new();
    let mut cursor = work_start;

    for (busy_start, busy_end) in &busy_periods {
        // 勤務時間外のイベントはスキップ
        if *busy_end <= work_start || *busy_start >= work_end {
            continue;
        }

        if *busy_start > cursor {
            let gap_minutes = (*busy_start - cursor).num_minutes() as u32;
            if gap_minutes >= 30 {
                slots.push(FreeSlot {
                    start: cursor,
                    end: *busy_start,
                    duration_minutes: gap_minutes,
                });
            }
        }

        // cursor を busy_end に進める（重複イベントを考慮して max）
        if *busy_end > cursor {
            cursor = *busy_end;
        }
    }

    // 最後のイベント後から work_end までの空き
    if cursor < work_end {
        let gap_minutes = (work_end - cursor).num_minutes() as u32;
        if gap_minutes >= 30 {
            slots.push(FreeSlot {
                start: cursor,
                end: work_end,
                duration_minutes: gap_minutes,
            });
        }
    }

    slots
}

// ============================================================================
// Time-Boxing (タスク自動配置)
// ============================================================================

const DEFAULT_TASK_MINUTES: u32 = 60;
const MAX_BLOCK_MINUTES: u32 = 90;
const BUFFER_RATIO: f32 = 0.70; // 空き時間の70%を配置、30%バッファ
const LUNCH_START_HOUR: u32 = 12;
const LUNCH_END_HOUR: u32 = 13;

/// 配置対象のタスク（Asana + DB の情報を統合）
struct TimeBoxTask {
    name: String,
    estimated_minutes: u32,
    is_overdue: bool,
    is_today: bool,
    priority: i32, // 低い = 高優先
}

/// タイムライン1行分
enum TimelineEntry {
    Meeting {
        start: chrono::NaiveTime,
        end: chrono::NaiveTime,
        name: String,
    },
    Task {
        start: chrono::NaiveTime,
        end: chrono::NaiveTime,
        task_name: String,
    },
    Buffer {
        start: chrono::NaiveTime,
        end: chrono::NaiveTime,
    },
    Lunch {
        start: chrono::NaiveTime,
        end: chrono::NaiveTime,
    },
}

impl TimelineEntry {
    fn start(&self) -> chrono::NaiveTime {
        match self {
            Self::Meeting { start, .. }
            | Self::Task { start, .. }
            | Self::Buffer { start, .. }
            | Self::Lunch { start, .. } => *start,
        }
    }

    fn format_line(&self) -> String {
        match self {
            Self::Meeting { start, end, name } => {
                let dur = (*end - *start).num_minutes();
                format!(
                    "{}-{}  [MTG {}m]  {}",
                    start.format("%H:%M"),
                    end.format("%H:%M"),
                    dur,
                    name,
                )
            }
            Self::Task {
                start,
                end,
                task_name,
            } => {
                let dur = (*end - *start).num_minutes();
                format!(
                    "{}-{}  [{}m]     {}",
                    start.format("%H:%M"),
                    end.format("%H:%M"),
                    dur,
                    task_name,
                )
            }
            Self::Buffer { start, end } => {
                let dur = (*end - *start).num_minutes();
                format!(
                    "{}-{}  [{}m]     ☕ バッファ",
                    start.format("%H:%M"),
                    end.format("%H:%M"),
                    dur,
                )
            }
            Self::Lunch { start, end } => {
                format!(
                    "{}-{}  [---]     🍚 昼休み",
                    start.format("%H:%M"),
                    end.format("%H:%M"),
                )
            }
        }
    }
}

/// Asana タスクと DB タスクを統合し、優先度順にソートした配置用リストを作成
fn build_timebox_tasks(
    cache: &TasksCache,
    db_tasks: &[crate::db::CodingTask],
) -> Vec<TimeBoxTask> {
    let today = Local::now().format("%Y-%m-%d").to_string();

    // DB の見積もりをGIDでルックアップ
    let estimates: std::collections::HashMap<&str, i32> = db_tasks
        .iter()
        .filter_map(|t| t.estimated_minutes.map(|m| (t.asana_task_gid.as_str(), m)))
        .collect();

    let mut tasks: Vec<TimeBoxTask> = cache
        .tasks
        .iter()
        .filter(|t| !t.completed)
        .map(|t| {
            let is_overdue = t
                .due_on
                .as_deref()
                .is_some_and(|d| d < today.as_str());
            let is_today = t.due_on.as_deref() == Some(today.as_str());
            let est = estimates
                .get(t.gid.as_str())
                .map(|&m| m.max(0) as u32)
                .unwrap_or(DEFAULT_TASK_MINUTES);

            TimeBoxTask {
                name: t.name.clone(),
                estimated_minutes: est.min(MAX_BLOCK_MINUTES), // 90分上限
                is_overdue,
                is_today,
                priority: t.priority,
            }
        })
        .collect();

    // 優先度ソート: 期限超過 > 本日期限 > 高priority > 低priority
    tasks.sort_by(|a, b| {
        let rank = |t: &TimeBoxTask| -> (u8, i32) {
            if t.is_overdue {
                (0, t.priority)
            } else if t.is_today {
                (1, t.priority)
            } else {
                (2, t.priority)
            }
        };
        rank(a).cmp(&rank(b))
    });

    tasks
}

/// 配置済みタスクの時間情報（カレンダー登録用）
struct AllocatedTask {
    name: String,
    start: chrono::NaiveTime,
    end: chrono::NaiveTime,
}

/// 空きスロットにタスクを配置し、タイムラインを生成
fn build_timeboxing_timeline(
    events: &[CalendarEvent],
    free_slots: &[FreeSlot],
    tasks: &mut Vec<TimeBoxTask>,
) -> (String, u32, u32, Vec<AllocatedTask>) {
    // 返り値: (timeline_text, allocated_minutes, unallocated_count, allocated_tasks)
    let mut timeline: Vec<TimelineEntry> = Vec::new();
    let mut allocated_tasks: Vec<AllocatedTask> = Vec::new();

    // ① MTG をタイムラインに追加
    for e in events {
        if e.is_all_day() {
            continue;
        }
        let start = match e.start_time() {
            Some(t) => t.with_timezone(&Local).time(),
            None => continue,
        };
        let end = match e.end_time() {
            Some(t) => t.with_timezone(&Local).time(),
            None => continue,
        };

        timeline.push(TimelineEntry::Meeting {
            start,
            end,
            name: e.summary.clone().unwrap_or_else(|| "(無題)".to_string()),
        });
    }

    // ② 各空きスロットにタスクを配置
    let mut total_allocated: u32 = 0;

    for slot in free_slots {
        // 昼休みを空きスロットから切り出す
        let lunch_start =
            chrono::NaiveTime::from_hms_opt(LUNCH_START_HOUR, 0, 0).unwrap();
        let lunch_end =
            chrono::NaiveTime::from_hms_opt(LUNCH_END_HOUR, 0, 0).unwrap();

        // このスロットが昼休みと重なるか
        let sub_slots = split_slot_around_lunch(slot, lunch_start, lunch_end);

        for sub in &sub_slots {
            let usable = (sub.duration_minutes as f32 * BUFFER_RATIO) as u32;
            let mut remaining = usable;
            let mut cursor = sub.start;

            // greedy first-fit: 優先順にタスクを詰める
            let mut i = 0;
            while i < tasks.len() && remaining > 0 {
                if tasks[i].estimated_minutes <= remaining {
                    let task = tasks.remove(i);
                    let task_end = cursor
                        + chrono::Duration::minutes(task.estimated_minutes as i64);

                    allocated_tasks.push(AllocatedTask {
                        name: task.name.clone(),
                        start: cursor,
                        end: task_end,
                    });

                    timeline.push(TimelineEntry::Task {
                        start: cursor,
                        end: task_end,
                        task_name: task.name,
                    });

                    total_allocated += task.estimated_minutes;
                    remaining -= task.estimated_minutes;
                    cursor = task_end;
                    // i は進めない（remove でずれるため）
                } else {
                    i += 1;
                }
            }

            // 残りはバッファ
            if cursor < sub.end {
                timeline.push(TimelineEntry::Buffer {
                    start: cursor,
                    end: sub.end,
                });
            }
        }
    }

    // ③ 昼休みエントリを追加（勤務時間内に収まる場合）
    let lunch_start =
        chrono::NaiveTime::from_hms_opt(LUNCH_START_HOUR, 0, 0).unwrap();
    let lunch_end =
        chrono::NaiveTime::from_hms_opt(LUNCH_END_HOUR, 0, 0).unwrap();
    // MTG が昼休み時間帯を完全に埋めていない場合のみ追加
    let lunch_blocked = events.iter().any(|e| {
        if e.is_all_day() {
            return false;
        }
        let s = e
            .start_time()
            .map(|t| t.with_timezone(&Local).time());
        let en = e
            .end
            .date_time
            .as_ref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Local).time());
        match (s, en) {
            (Some(s), Some(en)) => s <= lunch_start && en >= lunch_end,
            _ => false,
        }
    });
    if !lunch_blocked {
        timeline.push(TimelineEntry::Lunch {
            start: lunch_start,
            end: lunch_end,
        });
    }

    // ④ 時間順ソート & フォーマット
    timeline.sort_by_key(|e| e.start());

    let unallocated = tasks.len() as u32;

    let mut lines = Vec::new();
    lines.push("```".to_string());
    for entry in &timeline {
        lines.push(entry.format_line());
    }
    lines.push("```".to_string());

    // 未配置タスクがあれば列挙
    if !tasks.is_empty() {
        lines.push(String::new());
        lines.push("*未配置タスク*（時間不足）".to_string());
        for t in tasks.iter() {
            let urgency = if t.is_overdue {
                " :rotating_light: 期限超過"
            } else if t.is_today {
                " :calendar: 本日期限"
            } else {
                ""
            };
            lines.push(format!(
                "• {} ({}分){}", t.name, t.estimated_minutes, urgency
            ));
        }
    }

    (lines.join("\n"), total_allocated, unallocated, allocated_tasks)
}

/// 昼休み時間帯でスロットを分割する
fn split_slot_around_lunch(
    slot: &FreeSlot,
    lunch_start: chrono::NaiveTime,
    lunch_end: chrono::NaiveTime,
) -> Vec<FreeSlot> {
    // スロットが昼休みと重ならない場合
    if slot.end <= lunch_start || slot.start >= lunch_end {
        return vec![slot.clone()];
    }

    let mut result = Vec::new();

    // 昼前の部分
    if slot.start < lunch_start {
        let dur = (lunch_start - slot.start).num_minutes() as u32;
        if dur >= 30 {
            result.push(FreeSlot {
                start: slot.start,
                end: lunch_start,
                duration_minutes: dur,
            });
        }
    }

    // 昼後の部分
    if slot.end > lunch_end {
        let dur = (slot.end - lunch_end).num_minutes() as u32;
        if dur >= 30 {
            result.push(FreeSlot {
                start: lunch_end,
                end: slot.end,
                duration_minutes: dur,
            });
        }
    }

    result
}

async fn fetch_today_events_safe(
    gcal: &mut Option<GoogleCalendarClient>,
) -> Vec<CalendarEvent> {
    match gcal.as_mut() {
        Some(client) => match client.fetch_today_events().await {
            Ok(events) => events,
            Err(e) => {
                tracing::warn!("Failed to fetch GCal events: {}", e);
                Vec::new()
            }
        },
        None => Vec::new(),
    }
}

fn format_tasks_for_prompt(cache: &TasksCache) -> String {
    let today = Local::now().format("%Y-%m-%d").to_string();
    let incomplete: Vec<_> = cache.tasks.iter().filter(|t| !t.completed).collect();

    if incomplete.is_empty() {
        return "未完了タスクなし".to_string();
    }

    let mut lines = Vec::new();
    for t in &incomplete {
        let due_info = t
            .due_on
            .as_deref()
            .map(|d| {
                if d < today.as_str() {
                    format!(" [期限超過: {}]", d)
                } else if d == today.as_str() {
                    " [本日期限]".to_string()
                } else {
                    format!(" (期限: {})", d)
                }
            })
            .unwrap_or_else(|| " (期限なし)".to_string());

        let section = t
            .section
            .as_deref()
            .map(|s| format!(" [{}]", s))
            .unwrap_or_default();

        lines.push(format!("- {}{}{} (担当: {})", t.name, due_info, section, t.assignee));
    }

    format!(
        "未完了: {}件 / 期限超過: {}件\n{}",
        cache.summary.incomplete,
        cache.summary.overdue,
        lines.join("\n")
    )
}

fn format_events_for_prompt(events: &[CalendarEvent]) -> String {
    if events.is_empty() {
        return "今日のイベントなし".to_string();
    }

    let mut lines = Vec::new();
    for e in events {
        let summary = e.summary.as_deref().unwrap_or("(無題)");
        let time = if e.is_all_day() {
            "終日".to_string()
        } else if let Some(start) = e.start_time() {
            let end_str = e
                .end
                .date_time
                .as_ref()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&Local).format("%H:%M").to_string())
                .unwrap_or_default();
            let start_local = start.with_timezone(&Local).format("%H:%M").to_string();
            format!("{}-{}", start_local, end_str)
        } else {
            "時間不明".to_string()
        };

        let meet = e
            .meet_link()
            .map(|_| " [Meet/Zoom]")
            .unwrap_or_default();

        lines.push(format!("- {} {}{}", time, summary, meet));
    }

    lines.join("\n")
}

// ============================================================================
// Fallback static messages
// ============================================================================

fn build_morning_message(
    cache: &TasksCache,
    today: &str,
    events: &[CalendarEvent],
) -> String {
    let incomplete: Vec<_> = cache.tasks.iter().filter(|t| !t.completed).collect();

    let overdue: Vec<_> = incomplete
        .iter()
        .filter(|t| t.due_on.as_deref().is_some_and(|d| d < today))
        .collect();

    let due_today: Vec<_> = incomplete
        .iter()
        .filter(|t| t.due_on.as_deref() == Some(today))
        .collect();

    let mut msg = format!(
        ":sunrise: *おはようございます！ 本日のタスクブリーフィング* ({})\n\n",
        today
    );

    if !overdue.is_empty() {
        msg.push_str(&format!(
            ":rotating_light: *期限超過 ({}件)*\n",
            overdue.len()
        ));
        for t in &overdue {
            let due = t.due_on.as_deref().unwrap_or("?");
            msg.push_str(&format!("  • {} (期限: {})\n", t.name, due));
        }
        msg.push('\n');
    }

    if !due_today.is_empty() {
        msg.push_str(&format!(
            ":calendar: *本日期限 ({}件)*\n",
            due_today.len()
        ));
        for t in &due_today {
            msg.push_str(&format!("  • {}\n", t.name));
        }
        msg.push('\n');
    }

    // カレンダーイベント
    if !events.is_empty() {
        msg.push_str(":date: *今日のスケジュール*\n");
        for e in events {
            let summary = e.summary.as_deref().unwrap_or("(無題)");
            let time_str = if e.is_all_day() {
                "終日".to_string()
            } else {
                e.start_time()
                    .map(|t| t.with_timezone(&Local).format("%H:%M").to_string())
                    .unwrap_or_default()
            };
            msg.push_str(&format!("  • {} {}\n", time_str, summary));
        }
        msg.push('\n');
    }

    let mut recommendations: Vec<_> = incomplete.clone();
    recommendations.sort_by_key(|t| t.priority);
    let top3: Vec<_> = recommendations.iter().take(3).collect();

    if !top3.is_empty() {
        msg.push_str(":dart: *今日のおすすめ*\n");
        for (i, t) in top3.iter().enumerate() {
            let due_info = t
                .due_on
                .as_deref()
                .map(|d| format!(" (期限: {})", d))
                .unwrap_or_default();
            msg.push_str(&format!("  {}. {}{}\n", i + 1, t.name, due_info));
        }
        msg.push('\n');
    }

    msg.push_str(&format!(
        "_未完了: {}件 | 期限超過: {}件_",
        cache.summary.incomplete, cache.summary.overdue
    ));

    msg
}

// ============================================================================
// Stagnation Check
// ============================================================================

async fn run_stagnation_check(job: &ScheduledJob, ctx: &mut SchedulerContext) -> Result<()> {
    let threshold = ctx.stagnation_threshold_hours;
    let tasks = ctx.db.get_stagnant_tasks(threshold)?;

    if tasks.is_empty() {
        tracing::info!("No stagnant tasks found (threshold: {}h)", threshold);
        return Ok(());
    }

    let tasks_text: Vec<String> = tasks
        .iter()
        .map(|t| {
            format!(
                "- #{} {} (status: {}, 最終更新: {})",
                t.id, t.asana_task_name, t.status, t.updated_at
            )
        })
        .collect();

    let system_prompt = build_scheduler_system_prompt(ctx);

    let prompt = format!(
        "## 指示: 停滞タスク診断\n\n\
         以下のタスクが{}時間以上更新されていません。\n\n\
         ### Step 1: Tiger 分類\n\
         各タスクを以下の3カテゴリに分類:\n\
         - *Tiger*（本物の障害）: 外部依存・技術的ブロッカーがある\n\
         - *Paper Tiger*（見かけだけ）: 着手していないだけ、優先度が曖昧\n\
         - *Elephant*（暗黙の問題）: スコープ過大・要件不明確で誰も触れていない\n\n\
         ### Step 2: 以下のフォーマットで出力\n\n\
         *:tiger: Tiger*（即対応が必要）\n\
         • タスク名 — ブロッカーの仮説 → 推奨アクション\n\n\
         *:cat: Paper Tiger*（リマインドで解決）\n\
         • タスク名 — 次の具体的アクション1つ\n\n\
         *:elephant: Elephant*（再定義が必要）\n\
         • タスク名 — 分割案 or スコープ絞り込み提案\n\n\
         タスクがない分類は省略。\n\n\
         ## 停滞タスク一覧\n{}",
        threshold,
        tasks_text.join("\n")
    );

    match ClaudeRunner::new("scheduler:stagnation_check", &prompt)
        .system_prompt(&system_prompt)
        .max_turns(3)
        .log_dir(&ctx.log_dir)
        .with_context(&ctx.runner_ctx)
        .run()
        .await
    {
        Ok(result) if result.success => {
            let message = format!(":warning: *停滞タスク診断* ({}時間以上未更新)\n\n{}", threshold, result.stdout);
            ctx.slack.post_message(&job.slack_channel, &message).await?;
        }
        Ok(result) => {
            tracing::error!("AI stagnation check failed: {}", result.stderr);
            let message = format!(
                ":warning: *停滞タスク* ({}時間以上未更新: {}件)\n{}",
                threshold,
                tasks.len(),
                tasks_text.join("\n")
            );
            ctx.slack.post_message(&job.slack_channel, &message).await?;
        }
        Err(e) => {
            tracing::error!("AI stagnation check failed: {}", e);
            let message = format!(
                ":warning: *停滞タスク* ({}時間以上未更新: {}件)\n{}",
                threshold,
                tasks.len(),
                tasks_text.join("\n")
            );
            ctx.slack.post_message(&job.slack_channel, &message).await?;
        }
    }

    Ok(())
}

// ============================================================================
// Weekly PM Review
// ============================================================================

async fn run_weekly_pm_review(job: &ScheduledJob, ctx: &mut SchedulerContext) -> Result<()> {
    let since = (Utc::now() - chrono::Duration::days(7))
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string();
    let completed = ctx.db.count_completed_since(&since)?;
    let active = ctx.db.get_active_tasks()?;
    let threshold = ctx.stagnation_threshold_hours;
    let stagnant = ctx.db.get_stagnant_tasks(threshold)?;

    let active_lines: Vec<String> = active
        .iter()
        .take(20)
        .map(|t| format!("- #{} {} (status: {})", t.id, t.asana_task_name, t.status))
        .collect();

    let stagnant_lines: Vec<String> = stagnant
        .iter()
        .map(|t| {
            format!(
                "- #{} {} (status: {}, 最終更新: {})",
                t.id, t.asana_task_name, t.status, t.updated_at
            )
        })
        .collect();

    let system_prompt = build_scheduler_system_prompt(ctx);

    let prompt = format!(
        "## 指示: 週次PMレビュー作成\n\n\
         今週のプロジェクト状況を分析し、来週の戦略を立ててください。\n\n\
         ### 数値サマリー\n\
         - 完了: {}件 / アクティブ: {}件 / 停滞: {}件\n\n\
         ### Step 1: 分析\n\
         - 今週の完了ペース（ベロシティ）は適切か\n\
         - 停滞タスクに共通するパターンはあるか\n\
         - アクティブタスクの構成は健全か（WIP過多ではないか）\n\n\
         ### Step 2: 以下のフォーマットで出力\n\n\
         *:chart_with_upwards_trend: 今週の成果*\n\
         完了{}件。主な成果を1-3行で要約\n\n\
         *:traffic_light: プロジェクト健全性*\n\
         | 指標 | 状態 | コメント |\n\
         |------|------|----------|\n\
         | ベロシティ | :green_circle:/:yellow_circle:/:red_circle: | 前週比の所感 |\n\
         | WIP数 | :green_circle:/:yellow_circle:/:red_circle: | 適正 or 過多 |\n\
         | 停滞率 | :green_circle:/:yellow_circle:/:red_circle: | 停滞/アクティブ比 |\n\n\
         *:warning: ボトルネック*\n\
         問題と改善案を箇条書き（なければ「特になし」）\n\n\
         *:dart: 来週のフォーカス*\n\
         優先順位付きで3つまで。各項目に理由を添える\n\n\
         *:crystal_ball: リスク・懸念*\n\
         来週以降に顕在化しそうなリスク（なければ省略）\n\n\
         ## アクティブタスク\n{}\n\n\
         ## 停滞タスク\n{}",
        completed,
        active.len(),
        stagnant.len(),
        completed,
        if active_lines.is_empty() { "なし".to_string() } else { active_lines.join("\n") },
        if stagnant_lines.is_empty() { "なし".to_string() } else { stagnant_lines.join("\n") },
    );

    match ClaudeRunner::new("scheduler:weekly_pm_review", &prompt)
        .system_prompt(&system_prompt)
        .max_turns(3)
        .log_dir(&ctx.log_dir)
        .with_context(&ctx.runner_ctx)
        .run()
        .await
    {
        Ok(result) if result.success => {
            let message = format!(":bar_chart: *週次PMレビュー*\n\n{}", result.stdout);
            ctx.slack.post_message(&job.slack_channel, &message).await?;
        }
        Ok(result) => {
            tracing::error!("AI weekly PM review failed: {}", result.stderr);
            let message = format!(
                ":bar_chart: *週次PMレビュー*\n\
                 - 完了: {}件\n\
                 - アクティブ: {}件\n\
                 - 停滞: {}件\n\n\
                 _AI分析は利用できませんでした_",
                completed,
                active.len(),
                stagnant.len()
            );
            ctx.slack.post_message(&job.slack_channel, &message).await?;
        }
        Err(e) => {
            tracing::error!("AI weekly PM review failed: {}", e);
            let message = format!(
                ":bar_chart: *週次PMレビュー*\n\
                 - 完了: {}件\n\
                 - アクティブ: {}件\n\
                 - 停滞: {}件\n\n\
                 _AI分析は利用できませんでした_",
                completed,
                active.len(),
                stagnant.len()
            );
            ctx.slack.post_message(&job.slack_channel, &message).await?;
        }
    }

    Ok(())
}

// ============================================================================
// Fallback static messages
// ============================================================================

fn build_evening_message(cache: &TasksCache, today: &str) -> String {
    let completed_today: Vec<_> = cache.tasks.iter().filter(|t| t.completed).collect();
    let incomplete: Vec<_> = cache.tasks.iter().filter(|t| !t.completed).collect();

    let overdue: Vec<_> = incomplete
        .iter()
        .filter(|t| t.due_on.as_deref().is_some_and(|d| d < today))
        .collect();

    let mut msg = format!(":city_sunset: *本日の進捗サマリー* ({})\n\n", today);

    msg.push_str(&format!(
        ":white_check_mark: *完了タスク*: {}件（全期間）\n\n",
        completed_today.len()
    ));

    if !overdue.is_empty() {
        msg.push_str(&format!(
            ":warning: *明日に持ち越し (期限超過: {}件)*\n",
            overdue.len()
        ));
        for t in overdue.iter().take(5) {
            let due = t.due_on.as_deref().unwrap_or("?");
            msg.push_str(&format!("  • {} (期限: {})\n", t.name, due));
        }
        if overdue.len() > 5 {
            msg.push_str(&format!("  ... 他{}件\n", overdue.len() - 5));
        }
        msg.push('\n');
    }

    msg.push_str(&format!(
        "_残タスク: {}件 | お疲れ様でした :wave:_",
        cache.summary.incomplete
    ));

    msg
}

// ============================================================================
// Self-Improvement（自己改善）
// ============================================================================

async fn run_self_improvement(
    _job: &ScheduledJob,
    ctx: &mut SchedulerContext,
) -> Result<()> {
    // 1. 分類履歴を分析
    let classification_history = ctx.db.get_recent_classification_history(20).unwrap_or_default();
    let total = classification_history.len();
    let incorrect: Vec<_> = classification_history.iter()
        .filter(|r| r.outcome != "correct")
        .collect();

    // 2. 直近のエラータスクを取得
    let error_tasks = ctx.db.get_tasks_by_status("error").unwrap_or_default();

    // 3. memory.md を読む
    let memory = context::read_memory(&ctx.repos_base_dir);

    // 分析材料がなければスキップ
    if total == 0 && error_tasks.is_empty() && memory.is_empty() {
        tracing::debug!("self_improvement: no data to analyze, skipping");
        return Ok(());
    }

    // 4. 分析プロンプト構築
    let mut analysis_parts = vec![
        "あなたは ambient-task-agent の自己改善アナリストです。\n\
         以下のデータを分析し、改善提案をタスクとして出力してください。".to_string(),
    ];

    if total > 0 {
        let accuracy = if total > 0 {
            ((total - incorrect.len()) as f64 / total as f64 * 100.0) as u32
        } else { 100 };
        let mut section = format!("## 分類精度\n正解率: {}% ({}/{}件)\n", accuracy, total - incorrect.len(), total);
        if !incorrect.is_empty() {
            section.push_str("\n誤分類:\n");
            for r in &incorrect {
                section.push_str(&format!("- 「{}」→ {} だったが {} が必要だった\n",
                    r.task_name, r.classification, r.outcome));
            }
        }
        analysis_parts.push(section);
    }

    if !error_tasks.is_empty() {
        let mut section = format!("## エラータスク ({}件)\n", error_tasks.len());
        for t in error_tasks.iter().take(5) {
            let err = t.error_message.as_deref().unwrap_or("不明");
            section.push_str(&format!("- #{} {} — {}\n", t.id, t.asana_task_name,
                crate::claude::truncate_str(err, 100)));
        }
        analysis_parts.push(section);
    }

    if !memory.is_empty() {
        analysis_parts.push(format!("## 学習メモ\n{}", crate::claude::truncate_str(&memory, 500)));
    }

    analysis_parts.push(
        "## 出力フォーマット\n\
         改善提案を以下の JSON 形式で出力してください:\n\
         ```json\n\
         {\"proposals\": [{\"title\": \"改善タイトル\", \"description\": \"具体的な改善内容\", \"priority\": \"high/medium/low\"}]}\n\
         ```\n\
         提案がなければ空配列 `{\"proposals\": []}` を返してください。\n\
         最大3件まで。最も効果的な改善のみ提案してください。".to_string()
    );

    let prompt = analysis_parts.join("\n\n");
    let schema = r#"{"type":"object","properties":{"proposals":{"type":"array","items":{"type":"object","properties":{"title":{"type":"string"},"description":{"type":"string"},"priority":{"type":"string","enum":["high","medium","low"]}},"required":["title","description","priority"]}}},"required":["proposals"]}"#;

    let result = ClaudeRunner::new("self_improvement", &prompt)
        .system_prompt("あなたは ambient-task-agent の改善を分析するエージェントです。コードの品質、分類精度、エラーパターンを分析して具体的な改善提案をしてください。")
        .max_turns(1)
        .allowed_tools("")
        .json_schema(schema)
        .log_dir(&ctx.log_dir)
        .with_context(&ctx.runner_ctx)
        .run()
        .await?;

    if !result.success {
        tracing::warn!("self_improvement: Claude analysis failed: {}", result.stderr);
        return Ok(());
    }

    // 5. 提案をパースしてタスクとして登録
    let proposals: Vec<serde_json::Value> = serde_json::from_str::<serde_json::Value>(&result.stdout)
        .ok()
        .and_then(|v| v.get("proposals")?.as_array().cloned())
        .unwrap_or_default();

    if proposals.is_empty() {
        tracing::info!("self_improvement: no proposals this week");
        return Ok(());
    }

    let slack_channel = &ctx.db.get_default_slack_channel().unwrap_or_default();
    let channel = if slack_channel.is_empty() {
        // フォールバック: repos_base_dir から設定を読む
        return Ok(());
    } else {
        slack_channel.as_str()
    };

    let mut registered = Vec::new();
    for p in &proposals {
        let title = p.get("title").and_then(|v| v.as_str()).unwrap_or("自己改善タスク");
        let desc = p.get("description").and_then(|v| v.as_str()).unwrap_or("");
        let priority = p.get("priority").and_then(|v| v.as_str()).unwrap_or("medium");

        let task_name = format!("[self-improvement][{}] {}", priority, title);
        let task_id = ctx.db.insert_task(
            &format!("self_{}", chrono::Utc::now().timestamp_millis()),
            &task_name,
            Some(desc),
            Some("self"),
            None,
        )?;
        registered.push((task_id, task_name.clone()));
        tracing::info!("self_improvement: registered task #{} '{}'", task_id, task_name);
    }

    // 6. Slack に通知
    let task_list: Vec<String> = registered.iter()
        .map(|(id, name)| format!("• #{} {}", id, name))
        .collect();
    let msg = format!(
        ":bulb: *自己改善提案* ({}件)\n{}\n\n_conversing フローで確認後、承認すると PR が作成されます_",
        registered.len(),
        task_list.join("\n")
    );
    ctx.slack.post_message(channel, &msg).await.ok();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::google::calendar::{CalendarEvent, EventDateTime};

    fn make_event(start_time: &str, end_time: &str, summary: &str) -> CalendarEvent {
        CalendarEvent {
            id: summary.to_string(),
            summary: Some(summary.to_string()),
            start: EventDateTime {
                date_time: Some(start_time.to_string()),
                date: None,
            },
            end: EventDateTime {
                date_time: Some(end_time.to_string()),
                date: None,
            },
            html_link: None,
            conference_data: None,
            status: None,
        }
    }

    fn make_all_day_event(date: &str, summary: &str) -> CalendarEvent {
        CalendarEvent {
            id: summary.to_string(),
            summary: Some(summary.to_string()),
            start: EventDateTime {
                date_time: None,
                date: Some(date.to_string()),
            },
            end: EventDateTime {
                date_time: None,
                date: Some(date.to_string()),
            },
            html_link: None,
            conference_data: None,
            status: None,
        }
    }

    #[test]
    fn test_compute_free_slots_no_events() {
        let work_start = chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap();
        let work_end = chrono::NaiveTime::from_hms_opt(18, 0, 0).unwrap();
        let slots = compute_free_slots(&[], work_start, work_end);
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].start, work_start);
        assert_eq!(slots[0].end, work_end);
        assert_eq!(slots[0].duration_minutes, 540); // 9h
    }

    #[test]
    fn test_compute_free_slots_with_meetings() {
        let events = vec![
            make_event("2026-03-05T10:00:00+09:00", "2026-03-05T11:00:00+09:00", "朝会"),
            make_event("2026-03-05T14:00:00+09:00", "2026-03-05T15:00:00+09:00", "1on1"),
        ];
        let work_start = chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap();
        let work_end = chrono::NaiveTime::from_hms_opt(18, 0, 0).unwrap();
        let slots = compute_free_slots(&events, work_start, work_end);

        assert_eq!(slots.len(), 3);
        // 09:00-10:00 (60分)
        assert_eq!(slots[0].start, chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap());
        assert_eq!(slots[0].end, chrono::NaiveTime::from_hms_opt(10, 0, 0).unwrap());
        assert_eq!(slots[0].duration_minutes, 60);
        // 11:00-14:00 (180分)
        assert_eq!(slots[1].start, chrono::NaiveTime::from_hms_opt(11, 0, 0).unwrap());
        assert_eq!(slots[1].end, chrono::NaiveTime::from_hms_opt(14, 0, 0).unwrap());
        assert_eq!(slots[1].duration_minutes, 180);
        // 15:00-18:00 (180分)
        assert_eq!(slots[2].start, chrono::NaiveTime::from_hms_opt(15, 0, 0).unwrap());
        assert_eq!(slots[2].end, chrono::NaiveTime::from_hms_opt(18, 0, 0).unwrap());
        assert_eq!(slots[2].duration_minutes, 180);
    }

    #[test]
    fn test_compute_free_slots_ignores_all_day() {
        let events = vec![
            make_all_day_event("2026-03-05", "祝日"),
            make_event("2026-03-05T10:00:00+09:00", "2026-03-05T10:30:00+09:00", "短い会議"),
        ];
        let work_start = chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap();
        let work_end = chrono::NaiveTime::from_hms_opt(18, 0, 0).unwrap();
        let slots = compute_free_slots(&events, work_start, work_end);

        assert_eq!(slots.len(), 2);
        // 09:00-10:00 (60分)
        assert_eq!(slots[0].duration_minutes, 60);
        // 10:30-18:00 (450分)
        assert_eq!(slots[1].duration_minutes, 450);
    }

    #[test]
    fn test_compute_free_slots_short_gap_ignored() {
        let events = vec![
            make_event("2026-03-05T09:00:00+09:00", "2026-03-05T09:20:00+09:00", "朝礼"),
            make_event("2026-03-05T09:40:00+09:00", "2026-03-05T10:00:00+09:00", "確認"),
        ];
        let work_start = chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap();
        let work_end = chrono::NaiveTime::from_hms_opt(18, 0, 0).unwrap();
        let slots = compute_free_slots(&events, work_start, work_end);

        // 09:20-09:40 は20分なので無視される
        // 10:00-18:00 のみ
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].start, chrono::NaiveTime::from_hms_opt(10, 0, 0).unwrap());
        assert_eq!(slots[0].duration_minutes, 480);
    }

    #[test]
    fn test_compute_free_slots_overlapping_events() {
        let events = vec![
            make_event("2026-03-05T10:00:00+09:00", "2026-03-05T12:00:00+09:00", "長い会議"),
            make_event("2026-03-05T11:00:00+09:00", "2026-03-05T11:30:00+09:00", "重複会議"),
        ];
        let work_start = chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap();
        let work_end = chrono::NaiveTime::from_hms_opt(18, 0, 0).unwrap();
        let slots = compute_free_slots(&events, work_start, work_end);

        assert_eq!(slots.len(), 2);
        // 09:00-10:00
        assert_eq!(slots[0].duration_minutes, 60);
        // 12:00-18:00
        assert_eq!(slots[1].start, chrono::NaiveTime::from_hms_opt(12, 0, 0).unwrap());
        assert_eq!(slots[1].duration_minutes, 360);
    }

    // ================================================================
    // Time-Boxing tests
    // ================================================================

    #[test]
    fn test_split_slot_around_lunch_no_overlap() {
        let slot = FreeSlot {
            start: chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
            end: chrono::NaiveTime::from_hms_opt(11, 0, 0).unwrap(),
            duration_minutes: 120,
        };
        let lunch_s = chrono::NaiveTime::from_hms_opt(12, 0, 0).unwrap();
        let lunch_e = chrono::NaiveTime::from_hms_opt(13, 0, 0).unwrap();
        let result = split_slot_around_lunch(&slot, lunch_s, lunch_e);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].duration_minutes, 120);
    }

    #[test]
    fn test_split_slot_around_lunch_spans_lunch() {
        let slot = FreeSlot {
            start: chrono::NaiveTime::from_hms_opt(11, 0, 0).unwrap(),
            end: chrono::NaiveTime::from_hms_opt(15, 0, 0).unwrap(),
            duration_minutes: 240,
        };
        let lunch_s = chrono::NaiveTime::from_hms_opt(12, 0, 0).unwrap();
        let lunch_e = chrono::NaiveTime::from_hms_opt(13, 0, 0).unwrap();
        let result = split_slot_around_lunch(&slot, lunch_s, lunch_e);
        assert_eq!(result.len(), 2);
        // 11:00-12:00 (60分)
        assert_eq!(result[0].start, chrono::NaiveTime::from_hms_opt(11, 0, 0).unwrap());
        assert_eq!(result[0].end, lunch_s);
        assert_eq!(result[0].duration_minutes, 60);
        // 13:00-15:00 (120分)
        assert_eq!(result[1].start, lunch_e);
        assert_eq!(result[1].end, chrono::NaiveTime::from_hms_opt(15, 0, 0).unwrap());
        assert_eq!(result[1].duration_minutes, 120);
    }

    #[test]
    fn test_split_slot_entirely_within_lunch() {
        let slot = FreeSlot {
            start: chrono::NaiveTime::from_hms_opt(12, 15, 0).unwrap(),
            end: chrono::NaiveTime::from_hms_opt(12, 45, 0).unwrap(),
            duration_minutes: 30,
        };
        let lunch_s = chrono::NaiveTime::from_hms_opt(12, 0, 0).unwrap();
        let lunch_e = chrono::NaiveTime::from_hms_opt(13, 0, 0).unwrap();
        let result = split_slot_around_lunch(&slot, lunch_s, lunch_e);
        assert_eq!(result.len(), 0); // 全部昼休み内
    }

    #[test]
    fn test_build_timeboxing_timeline_basic() {
        let events = vec![
            make_event("2026-03-05T10:00:00+09:00", "2026-03-05T11:00:00+09:00", "朝会"),
        ];
        let work_start = chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap();
        let work_end = chrono::NaiveTime::from_hms_opt(18, 0, 0).unwrap();
        let free_slots = compute_free_slots(&events, work_start, work_end);

        let mut tasks = vec![
            TimeBoxTask {
                name: "タスクA".to_string(),
                estimated_minutes: 60,
                is_overdue: true,
                is_today: false,
                priority: 1,
            },
            TimeBoxTask {
                name: "タスクB".to_string(),
                estimated_minutes: 45,
                is_overdue: false,
                is_today: true,
                priority: 2,
            },
        ];

        let (timeline, allocated, unallocated, allocated_list) =
            build_timeboxing_timeline(&events, &free_slots, &mut tasks);

        // タスクが配置された
        assert!(allocated > 0);
        // タイムラインにタスク名が含まれる
        assert!(timeline.contains("タスクA"));
        assert!(timeline.contains("タスクB"));
        // MTG が含まれる
        assert!(timeline.contains("朝会"));
        // 昼休みが含まれる
        assert!(timeline.contains("昼休み"));
        // 未配置なし
        assert_eq!(unallocated, 0);
        // allocated_list にタスクが入っている
        assert_eq!(allocated_list.len(), 2);
        assert_eq!(allocated_list[0].name, "タスクA");
    }

    #[test]
    fn test_build_timeboxing_overflows_to_unallocated() {
        // 空きが非常に少ないケース
        let events = vec![
            make_event("2026-03-05T09:00:00+09:00", "2026-03-05T17:30:00+09:00", "終日ワークショップ"),
        ];
        let work_start = chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap();
        let work_end = chrono::NaiveTime::from_hms_opt(18, 0, 0).unwrap();
        let free_slots = compute_free_slots(&events, work_start, work_end);
        // free_slots: 17:30-18:00 (30分) → usable = 21分

        let mut tasks = vec![
            TimeBoxTask {
                name: "大きなタスク".to_string(),
                estimated_minutes: 90,
                is_overdue: false,
                is_today: false,
                priority: 1,
            },
        ];

        let (timeline, _allocated, unallocated, allocated_list) =
            build_timeboxing_timeline(&events, &free_slots, &mut tasks);

        // 90分のタスクは21分のスロットに入らない
        assert_eq!(unallocated, 1);
        assert!(timeline.contains("未配置タスク"));
        assert!(timeline.contains("大きなタスク"));
        assert!(allocated_list.is_empty());
    }
}
