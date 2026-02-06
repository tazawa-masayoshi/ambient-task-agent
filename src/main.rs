mod asana;
mod config;
mod slack;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use anyhow::Result;
use chrono::Local;
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use crate::asana::client::AsanaClient;
use crate::config::{load_asana_config, load_slack_config};
use crate::slack::client::SlackClient;

const CACHE_DIR: &str = ".config/ambient-task-agent";
const CACHE_FILE: &str = "tasks-cache.json";

#[derive(Parser)]
#[command(name = "ambient-task-agent")]
#[command(about = "Asanaタスク管理エージェント")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Asanaからタスクを取得してキャッシュに保存
    Sync {
        /// 変更があった場合のみ出力（cron用）
        #[arg(long)]
        quiet: bool,
    },
    /// キャッシュ済みタスクを表示
    Show {
        /// 自分のタスクのみ表示
        #[arg(long)]
        mine: bool,
        /// JSON形式で出力
        #[arg(long)]
        json: bool,
    },
    /// Slackにメッセージ送信
    Notify {
        /// 送信するメッセージ
        #[arg(short, long)]
        message: String,
        /// 送信先チャンネルID（省略時はテストチャンネル）
        #[arg(short, long)]
        channel: Option<String>,
    },
    /// タスク完了をSlackに通知
    Done {
        /// 完了したタスク名
        #[arg(short, long)]
        task: String,
    },
    /// キャッシュの状態を表示
    Status,
}

// tasks-cache.json の構造
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TasksCache {
    synced_at: String,
    project: ProjectInfo,
    tasks: Vec<CachedTask>,
    summary: TaskSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    tasks_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProjectInfo {
    gid: String,
    name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Hash)]
struct CachedTask {
    gid: String,
    name: String,
    assignee: String,
    due_on: Option<String>,
    completed: bool,
    section: Option<String>,
    notes_preview: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TaskSummary {
    total: usize,
    incomplete: usize,
    my_tasks: usize,
    overdue: usize,
}

/// sync結果
struct SyncResult {
    cache: TasksCache,
    changed: bool,
    diff: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Sync { quiet } => cmd_sync(quiet).await?,
        Commands::Show { mine, json } => cmd_show(mine, json)?,
        Commands::Notify { message, channel } => cmd_notify(&message, channel.as_deref()).await?,
        Commands::Done { task } => cmd_done(&task).await?,
        Commands::Status => cmd_status()?,
    }

    Ok(())
}

fn cache_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(CACHE_DIR)
}

fn cache_path() -> PathBuf {
    cache_dir().join(CACHE_FILE)
}

fn load_cache() -> Result<TasksCache> {
    let path = cache_path();
    let content = fs::read_to_string(&path)
        .map_err(|_| anyhow::anyhow!("キャッシュが見つかりません: {}\nambient-task-agent sync を実行してください", path.display()))?;
    let cache: TasksCache = serde_json::from_str(&content)?;
    Ok(cache)
}

/// タスク一覧のハッシュを計算（synced_atは除外）
fn compute_tasks_hash(tasks: &[CachedTask]) -> String {
    let mut hasher = DefaultHasher::new();
    for task in tasks {
        task.hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

/// 前回と今回の差分を検出
fn detect_changes(old: &TasksCache, new_tasks: &[CachedTask]) -> Vec<String> {
    let mut changes = Vec::new();

    let old_map: std::collections::HashMap<&str, &CachedTask> =
        old.tasks.iter().map(|t| (t.gid.as_str(), t)).collect();
    let new_map: std::collections::HashMap<&str, &CachedTask> =
        new_tasks.iter().map(|t| (t.gid.as_str(), t)).collect();

    // 新規タスク
    for (gid, task) in &new_map {
        if !old_map.contains_key(gid) {
            changes.push(format!("[新規] {} ({})", task.name, task.assignee));
        }
    }

    // 削除されたタスク
    for (gid, task) in &old_map {
        if !new_map.contains_key(gid) {
            changes.push(format!("[削除] {}", task.name));
        }
    }

    // 変更されたタスク
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
                changes.push(format!("[担当変更] {} : {} → {}", new_task.name, old_task.assignee, new_task.assignee));
            }
            if old_task.due_on != new_task.due_on {
                let old_due = old_task.due_on.as_deref().unwrap_or("なし");
                let new_due = new_task.due_on.as_deref().unwrap_or("なし");
                changes.push(format!("[期限変更] {} : {} → {}", new_task.name, old_due, new_due));
            }
            if old_task.section != new_task.section {
                let old_sec = old_task.section.as_deref().unwrap_or("なし");
                let new_sec = new_task.section.as_deref().unwrap_or("なし");
                changes.push(format!("[セクション移動] {} : {} → {}", new_task.name, old_sec, new_sec));
            }
            if old_task.name != new_task.name {
                changes.push(format!("[名前変更] {} → {}", old_task.name, new_task.name));
            }
        }
    }

    changes
}

async fn cmd_sync(quiet: bool) -> Result<()> {
    let asana_config = load_asana_config()?;
    let user_name = asana_config.user_name.clone();
    let project_id = asana_config.project_id.clone();
    let client = AsanaClient::new(asana_config);

    let raw_tasks = client.fetch_project_tasks().await?;
    let today = Local::now().format("%Y-%m-%d").to_string();

    // CachedTask に変換
    let cached_tasks: Vec<CachedTask> = raw_tasks
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
            CachedTask {
                gid: t.gid.clone(),
                name: t.name.clone(),
                assignee,
                due_on: t.due_on.clone(),
                completed: t.completed,
                section,
                notes_preview,
            }
        })
        .collect();

    // ハッシュ計算
    let new_hash = compute_tasks_hash(&cached_tasks);

    // 前回キャッシュと比較
    let old_cache = load_cache().ok();
    let old_hash = old_cache
        .as_ref()
        .and_then(|c| c.tasks_hash.clone())
        .unwrap_or_default();
    let changed = old_hash != new_hash;

    // 差分検出
    let diff = if changed {
        if let Some(ref old) = old_cache {
            detect_changes(old, &cached_tasks)
        } else {
            vec!["[初回同期]".to_string()]
        }
    } else {
        Vec::new()
    };

    // サマリー計算
    let incomplete = cached_tasks.iter().filter(|t| !t.completed).count();
    let my_tasks = cached_tasks
        .iter()
        .filter(|t| !t.completed && t.assignee.contains(&user_name))
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
            gid: project_id,
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
    fs::create_dir_all(&dir)?;
    let json = serde_json::to_string_pretty(&cache)?;
    fs::write(cache_path(), &json)?;

    // 出力
    if quiet {
        if changed {
            // 差分情報をJSON出力（cron→claude -p の入力用）
            let output = serde_json::json!({
                "changed": true,
                "diff": diff,
                "summary": {
                    "total": cache.summary.total,
                    "incomplete": cache.summary.incomplete,
                    "my_tasks": cache.summary.my_tasks,
                    "overdue": cache.summary.overdue,
                }
            });
            println!("{}", serde_json::to_string(&output)?);
        }
        // 変更なしなら何も出力しない（exit code 0）
    } else {
        println!(
            "Asana同期完了: 全{}件 (未完了: {}, 自分: {}, 期限超過: {})",
            cache.summary.total, cache.summary.incomplete, cache.summary.my_tasks, cache.summary.overdue
        );
        if changed {
            if diff.is_empty() {
                println!("変更あり（詳細不明 - 初回同期または構造変更）");
            } else {
                println!("\n変更点:");
                for d in &diff {
                    println!("  {}", d);
                }
            }
        } else {
            println!("変更なし");
        }
    }

    Ok(())
}

fn cmd_show(mine: bool, json: bool) -> Result<()> {
    let cache = load_cache()?;
    let user_name = load_asana_config()
        .map(|c| c.user_name)
        .unwrap_or_else(|_| "田澤".to_string());

    if json {
        println!("{}", serde_json::to_string_pretty(&cache)?);
        return Ok(());
    }

    let today = Local::now().format("%Y-%m-%d").to_string();

    let tasks: Vec<&CachedTask> = if mine {
        cache.tasks.iter().filter(|t| !t.completed && t.assignee.contains(&user_name)).collect()
    } else {
        cache.tasks.iter().filter(|t| !t.completed).collect()
    };

    if tasks.is_empty() {
        println!("表示するタスクがありません");
        return Ok(());
    }

    for task in &tasks {
        let due_mark = match &task.due_on {
            Some(due) if due < &today => " [期限超過]",
            Some(due) if due == &today => " [本日期限]",
            _ => "",
        };
        let section = task.section.as_deref().unwrap_or("");
        let due = task.due_on.as_deref().unwrap_or("期限なし");

        println!(
            "  {} | {} | {} | {}{}",
            task.assignee, section, task.name, due, due_mark
        );
    }

    println!(
        "\n同期: {} | 全{}件 (未完了: {}, 期限超過: {})",
        cache.synced_at, cache.summary.total, cache.summary.incomplete, cache.summary.overdue
    );
    Ok(())
}

fn cmd_status() -> Result<()> {
    let path = cache_path();
    if !path.exists() {
        println!("キャッシュなし: {}", path.display());
        println!("ambient-task-agent sync を実行してタスクを同期してください");
        return Ok(());
    }

    let cache = load_cache()?;
    println!("キャッシュパス: {}", path.display());
    println!("最終同期: {}", cache.synced_at);
    println!("プロジェクト: {}", cache.project.name);
    println!(
        "タスク: 全{}件 (未完了: {}, 自分: {}, 期限超過: {})",
        cache.summary.total,
        cache.summary.incomplete,
        cache.summary.my_tasks,
        cache.summary.overdue
    );
    if let Some(hash) = &cache.tasks_hash {
        println!("ハッシュ: {}", hash);
    }
    Ok(())
}

async fn cmd_notify(message: &str, channel: Option<&str>) -> Result<()> {
    let config = load_slack_config()?;
    let client = SlackClient::new(config.clone());

    let ch = channel.unwrap_or(&config.test_channel);
    let ts = client.post_message(ch, message).await?;
    println!("Slack送信成功 (ts: {})", ts);
    Ok(())
}

async fn cmd_done(task_name: &str) -> Result<()> {
    let message = format!("✅ タスク完了: {}", task_name);
    cmd_notify(&message, None).await
}
