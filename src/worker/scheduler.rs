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
## ルール
- 簡潔にSlack mrkdwnで日本語出力すること
- 箇条書きを活用し、読みやすいフォーマットにすること";

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
pub async fn check_and_run(ctx: &mut SchedulerContext) -> Result<()> {
    let now = Utc::now();

    while let Some(job) = ctx.db.get_due_job(&now)? {
        tracing::info!(
            "Running scheduled job: {} (type: {})",
            job.job_key,
            job.job_type
        );

        if let Err(e) = execute_job(&job, ctx).await {
            tracing::error!("Scheduled job {} failed: {}", job.job_key, e);
        }

        let next = compute_next_run(&job.schedule_cron).unwrap_or_default();
        ctx.db.mark_job_run(job.id, &next)?;

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

    // 空き時間スロットを計算
    let work_start = chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap();
    let work_end = chrono::NaiveTime::from_hms_opt(18, 0, 0).unwrap();
    let free_slots = compute_free_slots(&events, work_start, work_end);
    if !free_slots.is_empty() {
        let slots_text = free_slots
            .iter()
            .map(|s| {
                format!(
                    "- {}-{} ({}分)",
                    s.start.format("%H:%M"),
                    s.end.format("%H:%M"),
                    s.duration_minutes
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        context_parts.push(format!("## 作業可能な時間帯\n{}", slots_text));
    }

    // DB タスクの見積もり時間・複雑度を追加
    let estimates_text: Vec<String> = active_tasks
        .iter()
        .filter(|t| t.estimated_minutes.is_some())
        .map(|t| {
            format!(
                "- #{} {} (見積: {}分, 複雑度: {})",
                t.id,
                t.asana_task_name,
                t.estimated_minutes.unwrap(),
                t.complexity.as_deref().unwrap_or("未判定")
            )
        })
        .collect();
    if !estimates_text.is_empty() {
        context_parts.push(format!("## タスク見積もり\n{}", estimates_text.join("\n")));
    }

    let context_block = context_parts.join("\n\n");

    let system_prompt = build_scheduler_system_prompt(ctx);

    let prompt = if job.prompt_template.is_empty() {
        format!(
            "以下の情報を分析し、今日やるべきことを提案してください。\n\
             期限超過→本日期限→MTG準備→停滞タスク対処→優先度順。\n\
             **作業可能な時間帯とタスク見積もりがある場合は、具体的な時間配置を提案してください。**\n\
             例: \"09:00-10:30 → #42 認証機能追加（見積90分）\"\n\
             バッファ時間（休憩・予備）を確保し、空き時間の80%程度を目安に配置。\n\n\
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
            "以下の情報から本日の振り返りをまとめてください。\n完了タスク、進行中の作業、明日に持ち越すものを整理。\n\n{context_block}"
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
            let end_local = e.end.date_time.as_ref()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())?
                .with_timezone(&Local)
                .time();
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
        "以下のタスクが{}時間以上停滞しています。各タスクについて：\n\
         1. 考えられる停滞原因（ブロッカー、優先度不明、スコープ過大等）\n\
         2. 推奨アクション（分割、優先度変更、キャンセル等）\n\
         を簡潔に診断してください。\n\n{}",
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
        "週次プロジェクトレビューを作成してください。\n\
         - 今週の完了タスク: {}件\n\
         - アクティブタスク: {}件\n\
         - 停滞タスク: {}件\n\n\
         ## アクティブタスク\n{}\n\n\
         ## 停滞タスク\n{}\n\n\
         以下の観点で分析してください:\n\
         1. 今週の成果サマリー\n\
         2. ボトルネックと改善案\n\
         3. 来週の優先事項提案",
        completed,
        active.len(),
        stagnant.len(),
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
}
