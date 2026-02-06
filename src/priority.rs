use chrono::{DateTime, Utc};

use crate::kintone::models::TaskRecord;

/// Calculate priority score for sorting tasks.
/// Higher score = higher priority.
pub fn priority_score(task: &TaskRecord) -> f64 {
    let now = Utc::now();

    let base_score: f64 = match task.priority.as_str() {
        "urgent" => 100.0,
        "this_week" => 50.0,
        _ => 10.0, // someday
    };

    let deadline_score = if !task.deadline.is_empty() {
        parse_days_until(&task.deadline, &now)
            .map(|days_left| {
                if days_left < 0 {
                    30.0 // overdue
                } else if days_left <= 1 {
                    20.0
                } else if days_left <= 3 {
                    10.0
                } else {
                    0.0
                }
            })
            .unwrap_or(0.0)
    } else {
        0.0
    };

    let stale_score = if !task.created_at.is_empty() {
        DateTime::parse_from_rfc3339(&task.created_at)
            .ok()
            .map(|ct| {
                let days_old = now.signed_duration_since(ct).num_days();
                (days_old as f64 * 0.5).min(15.0)
            })
            .unwrap_or(0.0)
    } else {
        0.0
    };

    base_score + deadline_score + stale_score
}

fn parse_days_until(deadline: &str, now: &DateTime<Utc>) -> Option<i64> {
    if let Ok(dl) = DateTime::parse_from_rfc3339(deadline) {
        return Some(dl.signed_duration_since(now).num_days());
    }
    if let Ok(dl) = chrono::NaiveDate::parse_from_str(deadline, "%Y-%m-%d") {
        let today = now.date_naive();
        return Some((dl - today).num_days());
    }
    None
}

/// Sort tasks by priority score descending.
pub fn sort_by_priority(tasks: &mut [TaskRecord]) {
    tasks.sort_by(|a, b| {
        priority_score(b)
            .partial_cmp(&priority_score(a))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}
