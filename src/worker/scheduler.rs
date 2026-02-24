use anyhow::Result;
use chrono::{Local, Utc};
use cron::Schedule;
use std::str::FromStr;

use crate::db::{Db, ScheduledJob};
use crate::google::calendar::GoogleCalendarClient;
use crate::repo_config::ReposConfig;
use crate::slack::client::SlackClient;
use crate::sync::{load_cache, TasksCache};

/// スケジューラが各ジョブ実行時に使うコンテキスト
pub struct SchedulerContext {
    pub db: Db,
    pub slack: SlackClient,
    pub asana_pat: String,
    pub asana_project_id: String,
    pub asana_user_name: String,
    pub google_calendar: Option<GoogleCalendarClient>,
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

    let context_block = format!(
        "## 日付\n{}\n\n## Asanaタスク\n{}\n\n## 今日のカレンダー\n{}",
        today, tasks_text, events_text,
    );

    let prompt = if job.prompt_template.is_empty() {
        format!(
            "あなたはAI Scrum Masterです。以下の情報を分析し、今日やるべきことを提案してください。\n期限超過→本日期限→MTG準備→優先度順。簡潔にSlack mrkdwnで日本語出力。\n\n{context_block}"
        )
    } else {
        format!("{}\n\n{}", job.prompt_template, context_block)
    };

    match crate::claude::run_claude_prompt(&prompt, 3).await {
        Ok(ai_output) => {
            ctx.slack
                .post_message(&job.slack_channel, &ai_output)
                .await?;
        }
        Err(e) => {
            tracing::error!("AI morning briefing failed: {}", e);
            // フォールバック: 静的メッセージ
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

    let context_block = format!(
        "## 日付\n{}\n\n## Asanaタスク\n{}\n\n## 今日のカレンダー\n{}",
        today, tasks_text, events_text,
    );

    let prompt = if job.prompt_template.is_empty() {
        format!(
            "あなたはAI Scrum Masterです。以下の情報から本日の振り返りをまとめてください。\n完了タスク、進行中の作業、明日に持ち越すものを整理。簡潔にSlack mrkdwnで日本語出力。\n\n{context_block}"
        )
    } else {
        format!("{}\n\n{}", job.prompt_template, context_block)
    };

    match crate::claude::run_claude_prompt(&prompt, 3).await {
        Ok(ai_output) => {
            ctx.slack
                .post_message(&job.slack_channel, &ai_output)
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
