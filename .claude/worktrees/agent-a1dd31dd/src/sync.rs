use anyhow::Result;
use chrono::Local;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

use crate::asana::client::AsanaClient;
use crate::config::AsanaConfig;

use serde::{Deserialize, Serialize};

// ====== データ構造（main.rs から移動） ======

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TasksCache {
    pub synced_at: String,
    pub project: ProjectInfo,
    pub tasks: Vec<CachedTask>,
    pub summary: TaskSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tasks_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectInfo {
    pub gid: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Hash)]
pub struct CachedTask {
    pub gid: String,
    pub name: String,
    pub assignee: String,
    pub due_on: Option<String>,
    pub completed: bool,
    pub section: Option<String>,
    pub notes_preview: Option<String>,
    #[serde(default = "default_priority")]
    pub priority: i32,
}

fn default_priority() -> i32 {
    3
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSummary {
    pub total: usize,
    pub incomplete: usize,
    pub my_tasks: usize,
    pub overdue: usize,
}

pub struct SyncResult {
    pub cache: TasksCache,
    pub changed: bool,
    pub diff: Vec<String>,
}

// ====== キャッシュパス ======

const CACHE_DIR: &str = ".config/wez-sidebar";
const CACHE_FILE: &str = "tasks-cache.json";

pub fn cache_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(CACHE_DIR)
}

pub fn cache_path() -> PathBuf {
    cache_dir().join(CACHE_FILE)
}

pub fn load_cache() -> Result<TasksCache> {
    let path = cache_path();
    let content = std::fs::read_to_string(&path)
        .map_err(|_| anyhow::anyhow!("キャッシュが見つかりません: {}", path.display()))?;
    let cache: TasksCache = serde_json::from_str(&content)?;
    Ok(cache)
}

// ====== コアロジック ======

/// Asana からタスクを取得してキャッシュを更新。変更があれば diff を返す。
pub async fn run_sync(asana_config: &AsanaConfig) -> Result<SyncResult> {
    let user_name = &asana_config.user_name;
    let project_id = &asana_config.project_id;
    let client = AsanaClient::new(asana_config.clone());

    let raw_tasks = client.fetch_project_tasks().await?;
    let today = Local::now().format("%Y-%m-%d").to_string();

    let mut cached_tasks: Vec<CachedTask> = raw_tasks
        .iter()
        .map(|t| {
            let assignee = t
                .assignee
                .as_ref()
                .map(|a| a.name.clone())
                .unwrap_or_else(|| "未割当".to_string());
            let section = t
                .memberships
                .as_ref()
                .and_then(|m| m.first())
                .and_then(|m| m.section.as_ref())
                .map(|s| s.name.clone());
            let notes_preview = t.notes.as_ref().map(|n| {
                let trimmed: String = n.chars().take(100).collect();
                trimmed
            });
            let priority = calc_priority(&t.due_on, &today);
            CachedTask {
                gid: t.gid.clone(),
                name: t.name.clone(),
                assignee,
                due_on: t.due_on.clone(),
                completed: t.completed,
                section,
                notes_preview,
                priority,
            }
        })
        .collect();

    cached_tasks.sort_by(|a, b| {
        if a.completed != b.completed {
            return a.completed.cmp(&b.completed);
        }
        match a.priority.cmp(&b.priority) {
            std::cmp::Ordering::Equal => {
                let a_due = a.due_on.as_deref().unwrap_or("9999-99-99");
                let b_due = b.due_on.as_deref().unwrap_or("9999-99-99");
                a_due.cmp(b_due)
            }
            other => other,
        }
    });

    let new_hash = compute_tasks_hash(&cached_tasks);

    let old_cache = load_cache().ok();
    let old_hash = old_cache
        .as_ref()
        .and_then(|c| c.tasks_hash.clone())
        .unwrap_or_default();
    let changed = old_hash != new_hash;

    let diff = if changed {
        if let Some(ref old) = old_cache {
            detect_changes(old, &cached_tasks)
        } else {
            vec!["[初回同期]".to_string()]
        }
    } else {
        Vec::new()
    };

    let incomplete = cached_tasks.iter().filter(|t| !t.completed).count();
    let my_tasks = cached_tasks
        .iter()
        .filter(|t| !t.completed && t.assignee.contains(user_name))
        .count();
    let overdue = cached_tasks
        .iter()
        .filter(|t| {
            !t.completed
                && t.due_on
                    .as_ref()
                    .map(|d| d.as_str() < today.as_str())
                    .unwrap_or(false)
        })
        .count();

    let cache = TasksCache {
        synced_at: Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string(),
        project: ProjectInfo {
            gid: project_id.clone(),
            name: "レボリューションズ".to_string(),
        },
        tasks: cached_tasks,
        summary: TaskSummary {
            total: raw_tasks.len(),
            incomplete,
            my_tasks,
            overdue,
        },
        tasks_hash: Some(new_hash),
    };

    // キャッシュ書き出し
    let dir = cache_dir();
    std::fs::create_dir_all(&dir)?;
    let json = serde_json::to_string_pretty(&cache)?;
    std::fs::write(cache_path(), &json)?;

    Ok(SyncResult {
        cache,
        changed,
        diff,
    })
}

// ====== ヘルパー ======

pub fn compute_tasks_hash(tasks: &[CachedTask]) -> String {
    let mut hasher = DefaultHasher::new();
    for task in tasks {
        task.hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

pub fn calc_priority(due_on: &Option<String>, today: &str) -> i32 {
    let due = match due_on {
        Some(d) => d.as_str(),
        None => return 3,
    };
    if due < today {
        return 1;
    }
    let today_date = chrono::NaiveDate::parse_from_str(today, "%Y-%m-%d");
    let due_date = chrono::NaiveDate::parse_from_str(due, "%Y-%m-%d");
    match (today_date, due_date) {
        (Ok(t), Ok(d)) => {
            let days_left = (d - t).num_days();
            if days_left <= 7 {
                1
            } else if days_left <= 14 {
                2
            } else {
                3
            }
        }
        _ => 3,
    }
}

pub fn detect_changes(old: &TasksCache, new_tasks: &[CachedTask]) -> Vec<String> {
    let mut changes = Vec::new();

    let old_map: std::collections::HashMap<&str, &CachedTask> =
        old.tasks.iter().map(|t| (t.gid.as_str(), t)).collect();
    let new_map: std::collections::HashMap<&str, &CachedTask> =
        new_tasks.iter().map(|t| (t.gid.as_str(), t)).collect();

    for (gid, task) in &new_map {
        if !old_map.contains_key(gid) {
            changes.push(format!("[新規] {} ({})", task.name, task.assignee));
        }
    }
    for (gid, task) in &old_map {
        if !new_map.contains_key(gid) {
            changes.push(format!("[削除] {}", task.name));
        }
    }
    for (gid, new_task) in &new_map {
        if let Some(old_task) = old_map.get(gid) {
            if old_task.completed != new_task.completed {
                if new_task.completed {
                    changes.push(format!("[完了] {}", new_task.name));
                } else {
                    changes.push(format!("[未完了に戻し] {}", new_task.name));
                }
            }
            if old_task.assignee != new_task.assignee {
                changes.push(format!(
                    "[担当変更] {} : {} → {}",
                    new_task.name, old_task.assignee, new_task.assignee
                ));
            }
            if old_task.due_on != new_task.due_on {
                let old_due = old_task.due_on.as_deref().unwrap_or("なし");
                let new_due = new_task.due_on.as_deref().unwrap_or("なし");
                changes.push(format!(
                    "[期限変更] {} : {} → {}",
                    new_task.name, old_due, new_due
                ));
            }
            if old_task.section != new_task.section {
                let old_sec = old_task.section.as_deref().unwrap_or("なし");
                let new_sec = new_task.section.as_deref().unwrap_or("なし");
                changes.push(format!(
                    "[セクション移動] {} : {} → {}",
                    new_task.name, old_sec, new_sec
                ));
            }
            if old_task.name != new_task.name {
                changes.push(format!("[名前変更] {} → {}", old_task.name, new_task.name));
            }
        }
    }

    changes
}
