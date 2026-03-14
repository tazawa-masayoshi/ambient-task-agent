use chrono::{DateTime, Utc};

use crate::db::{CodingTask, Subtask};

/// 優先度スコアを計算 (0.0-100.0)
///
/// | 要素             | 最大pt | 計算                                        |
/// |------------------|--------|---------------------------------------------|
/// | 停滞時間         | 30     | 経過時間 / 72h                              |
/// | 進捗率           | 20     | progress / 100 × 20                        |
/// | ステータス重み   | 15     | in_progress=15, ready=10, approved=5        |
/// | blocked 率（逆） | 10     | (1 - blocked/total) × 10                   |
/// | 予約（期限）     | 10     | 将来 Asana due_on 連携用                    |
pub fn calculate_priority_score(task: &CodingTask, now: &DateTime<Utc>) -> f64 {
    let mut score: f64 = 0.0;

    // 1. 停滞時間 (max 30pt): updated_at からの経過時間 / 72h
    if let Ok(updated) = parse_datetime(&task.updated_at) {
        let hours = now.signed_duration_since(updated).num_hours() as f64;
        score += (hours / 72.0 * 30.0).min(30.0);
    }

    // 2. 進捗率 (max 20pt): 進捗が進んでいるタスクを優先
    let progress = task.progress_percent.unwrap_or(0) as f64;
    score += (progress / 100.0) * 20.0;

    // 3. ステータス重み (max 15pt)
    score += match task.status.as_str() {
        "in_progress" | "executing" => 15.0,
        "ready" => 10.0,
        "approved" | "auto_approved" => 5.0,
        _ => 0.0,
    };

    // 4. blocked 率 (max 10pt): blocked サブタスクが少ないほど高い
    if let Some(ref json) = task.subtasks_json {
        if let Ok(subtasks) = serde_json::from_str::<Vec<Subtask>>(json) {
            if !subtasks.is_empty() {
                let blocked = subtasks.iter().filter(|s| s.status == "blocked").count() as f64;
                let total = subtasks.len() as f64;
                score += (1.0 - blocked / total) * 10.0;
            } else {
                score += 10.0; // サブタスクなし → blocked なし
            }
        }
    }

    // 5. 予約: 将来 Asana due_on 連携用 (max 10pt) — 現在は 0
    // TODO: due_on フィールド追加時に実装

    score.clamp(0.0, 100.0)
}

fn parse_datetime(s: &str) -> Result<DateTime<Utc>, ()> {
    // RFC3339 形式を試行
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }
    // SQLite の strftime フォーマット (%Y-%m-%dT%H:%M:%fZ)
    if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.fZ") {
        return Ok(ndt.and_utc());
    }
    Err(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_task(status: &str, progress: Option<i32>, updated_at: &str) -> CodingTask {
        CodingTask {
            id: 1,
            asana_task_gid: "g1".into(),
            asana_task_name: "Test".into(),
            description: None,
            repo_key: None,
            branch_name: None,
            status: status.into(),
            plan_text: None,
            analysis_text: None,
            subtasks_json: None,
            slack_channel: None,
            slack_thread_ts: None,
            slack_plan_ts: None,
            pr_url: None,
            error_message: None,
            retry_count: 0,
            summary: None,
            memory_note: None,
            priority_score: None,
            progress_percent: progress,
            started_at_task: None,
            completed_at: None,
            estimated_minutes: None,
            actual_minutes: None,
            retrospective_note: None,
            complexity: None,
            claude_session_id: None,
            current_subtask_index: None,
            created_at: "2026-01-01T00:00:00.000Z".into(),
            updated_at: updated_at.into(),
            source: "asana".into(),
            converse_thread_ts: None,
        }
    }

    #[test]
    fn test_in_progress_higher_than_ready() {
        let now = Utc::now();
        let updated = now.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
        let t1 = make_task("in_progress", Some(50), &updated);
        let t2 = make_task("ready", Some(50), &updated);
        let s1 = calculate_priority_score(&t1, &now);
        let s2 = calculate_priority_score(&t2, &now);
        assert!(s1 > s2, "in_progress ({}) should score higher than ready ({})", s1, s2);
    }

    #[test]
    fn test_stagnant_task_scores_higher() {
        let now = Utc::now();
        let recent = now.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
        let old = (now - chrono::Duration::hours(48)).format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
        let t_recent = make_task("ready", Some(0), &recent);
        let t_old = make_task("ready", Some(0), &old);
        let s_recent = calculate_priority_score(&t_recent, &now);
        let s_old = calculate_priority_score(&t_old, &now);
        assert!(s_old > s_recent, "stagnant task ({}) should score higher than recent ({})", s_old, s_recent);
    }

    #[test]
    fn test_score_clamped() {
        let now = Utc::now();
        let very_old = "2020-01-01T00:00:00.000Z";
        let t = make_task("in_progress", Some(100), very_old);
        let s = calculate_priority_score(&t, &now);
        assert!(s <= 100.0, "score should be clamped to 100.0, got {}", s);
        assert!(s >= 0.0);
    }
}
