mod asana;
mod config;
mod slack;

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use anyhow::Result;
use chrono::{Local, Utc};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::process::Command;

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
    /// Claude Code hookイベント処理
    Hook {
        /// イベント名 (PreToolUse, PostToolUse, Stop, UserPromptSubmit, Notification)
        event: String,
    },
    /// 作業タスクを設定
    Start {
        /// タスク名（部分一致検索）
        query: Option<String>,
        /// GID直指定
        #[arg(long)]
        gid: Option<String>,
    },
    /// 現在の作業タスクを表示
    Current,
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
    /// 優先度 (1=高, 2=中, 3=低)。due_on + section から自動計算。
    #[serde(default = "default_priority")]
    priority: i32,
}

fn default_priority() -> i32 {
    3
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
        Commands::Hook { event } => cmd_hook(&event).await?,
        Commands::Start { query, gid } => cmd_start(query, gid)?,
        Commands::Current => cmd_current()?,
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

/// 期限ベースの優先度計算 (1=高, 2=中, 3=低)
fn calc_priority(due_on: &Option<String>, today: &str) -> i32 {
    let due = match due_on {
        Some(d) => d.as_str(),
        None => return 3, // 期限なし → 低
    };

    if due < today {
        return 1; // 期限超過 → 高
    }

    // 日数差を計算
    let today_date = chrono::NaiveDate::parse_from_str(today, "%Y-%m-%d");
    let due_date = chrono::NaiveDate::parse_from_str(due, "%Y-%m-%d");

    match (today_date, due_date) {
        (Ok(t), Ok(d)) => {
            let days_left = (d - t).num_days();
            if days_left <= 7 {
                1 // 1週間以内 → 高
            } else if days_left <= 14 {
                2 // 2週間以内 → 中
            } else {
                3 // それ以降 → 低
            }
        }
        _ => 3,
    }
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

    // 未完了タスクを優先度順にソート (priority昇順 → due_on昇順)
    cached_tasks.sort_by(|a, b| {
        if a.completed != b.completed {
            return a.completed.cmp(&b.completed); // 未完了が先
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

/// current-task.json の構造
#[derive(Debug, Serialize, Deserialize)]
struct CurrentTask {
    gid: String,
    name: String,
}

fn current_task_path() -> PathBuf {
    PathBuf::from(".claude/current-task.json")
}

fn cmd_start(query: Option<String>, gid: Option<String>) -> Result<()> {
    let cache = load_cache()?;
    let incomplete: Vec<&CachedTask> = cache.tasks.iter().filter(|t| !t.completed).collect();

    let task = if let Some(gid) = gid {
        // GID直指定
        incomplete
            .iter()
            .find(|t| t.gid == gid)
            .ok_or_else(|| anyhow::anyhow!("GID {} のタスクが見つかりません", gid))?
    } else if let Some(ref q) = query {
        // 部分一致検索
        let matches: Vec<&&CachedTask> = incomplete
            .iter()
            .filter(|t| t.name.contains(q.as_str()))
            .collect();

        match matches.len() {
            0 => anyhow::bail!("「{}」に一致するタスクが見つかりません", q),
            1 => matches[0],
            _ => {
                // 複数候補 → JSON出力して終了（スキル側で選択してもらう）
                let candidates: Vec<serde_json::Value> = matches
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "gid": t.gid,
                            "name": t.name,
                            "assignee": t.assignee,
                            "due_on": t.due_on,
                        })
                    })
                    .collect();
                let output = serde_json::json!({
                    "status": "multiple",
                    "candidates": candidates,
                });
                println!("{}", serde_json::to_string(&output)?);
                return Ok(());
            }
        }
    } else {
        // 引数なし → 一覧表示
        let candidates: Vec<serde_json::Value> = incomplete
            .iter()
            .map(|t| {
                serde_json::json!({
                    "gid": t.gid,
                    "name": t.name,
                    "assignee": t.assignee,
                    "due_on": t.due_on,
                })
            })
            .collect();
        let output = serde_json::json!({
            "status": "multiple",
            "candidates": candidates,
        });
        println!("{}", serde_json::to_string(&output)?);
        return Ok(());
    };

    // current-task.json 作成
    let ct = CurrentTask {
        gid: task.gid.clone(),
        name: task.name.clone(),
    };
    let path = current_task_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, serde_json::to_string_pretty(&ct)?)?;

    let output = serde_json::json!({
        "status": "ok",
        "task": {
            "gid": task.gid,
            "name": task.name,
        }
    });
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

fn cmd_current() -> Result<()> {
    let path = current_task_path();
    if !path.exists() {
        println!("{}", serde_json::json!({"status": "none"}));
        return Ok(());
    }

    let content = fs::read_to_string(&path)?;
    let task: CurrentTask = serde_json::from_str(&content)?;
    let output = serde_json::json!({
        "status": "ok",
        "task": {
            "gid": task.gid,
            "name": task.name,
        }
    });
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

// ============================================================================
// Session Management
// ============================================================================

const SESSIONS_FILE: &str = "sessions.json";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct SessionsFile {
    sessions: HashMap<String, Session>,
    updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Session {
    session_id: String,
    home_cwd: String,
    tty: String,
    status: String,
    created_at: String,
    updated_at: String,
    #[serde(default)]
    active_task: Option<String>,
    #[serde(default)]
    tasks_completed: i32,
    #[serde(default)]
    tasks_total: i32,
}

#[derive(Debug, Deserialize)]
struct HookPayload {
    session_id: String,
    cwd: Option<String>,
    notification_type: Option<String>,
}

const VALID_HOOK_EVENTS: &[&str] = &[
    "PreToolUse",
    "PostToolUse",
    "Notification",
    "Stop",
    "UserPromptSubmit",
];

fn sessions_path() -> PathBuf {
    cache_dir().join(SESSIONS_FILE)
}

fn read_session_store() -> SessionsFile {
    let path = sessions_path();
    match fs::read_to_string(&path) {
        Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
        Err(_) => SessionsFile::default(),
    }
}

fn write_session_store(store: &SessionsFile) -> Result<()> {
    let path = sessions_path();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let data = serde_json::to_string_pretty(store)?;
    fs::write(path, data)?;
    Ok(())
}

fn get_tty_from_ancestors() -> String {
    let mut ppid = std::os::unix::process::parent_id() as i32;

    for _ in 0..5 {
        let output = Command::new("ps")
            .args(["-o", "tty=", "-p", &ppid.to_string()])
            .output();

        if let Ok(out) = output {
            let tty = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !tty.is_empty() && tty != "??" {
                return format!("/dev/{}", tty);
            }
        }

        let output = Command::new("ps")
            .args(["-o", "ppid=", "-p", &ppid.to_string()])
            .output();

        if let Ok(out) = output {
            if let Ok(new_ppid) = String::from_utf8_lossy(&out.stdout).trim().parse::<i32>() {
                ppid = new_ppid;
            } else {
                break;
            }
        } else {
            break;
        }
    }

    String::new()
}

/// TTYからWezTermの (tab_id, pane_id) を特定
fn find_wezterm_pane_by_tty(tty: &str) -> Option<(i32, i32)> {
    if tty.is_empty() {
        return None;
    }

    let output = Command::new("/opt/homebrew/bin/wezterm")
        .args(["cli", "list", "--format", "json"])
        .output()
        .ok()?;

    #[derive(Deserialize)]
    struct WezPane {
        tab_id: i32,
        pane_id: i32,
        tty_name: String,
    }

    let panes: Vec<WezPane> = serde_json::from_slice(&output.stdout).ok()?;
    panes
        .iter()
        .find(|p| p.tty_name == tty)
        .map(|p| (p.tab_id, p.pane_id))
}

/// ~/.claude/tasks/<session_id>/*.json からタスク進捗を読み取り
fn read_claude_tasks(session_id: &str) -> (Option<String>, i32, i32) {
    let tasks_dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()))
        .join(".claude/tasks")
        .join(session_id);

    let entries = match fs::read_dir(&tasks_dir) {
        Ok(e) => e,
        Err(_) => return (None, 0, 0),
    };

    #[derive(Deserialize)]
    struct TaskItem {
        subject: String,
        status: String,
    }

    let mut items: Vec<TaskItem> = Vec::new();
    for entry in entries.flatten() {
        if entry
            .path()
            .extension()
            .map(|e| e == "json")
            .unwrap_or(false)
        {
            if let Ok(content) = fs::read_to_string(entry.path()) {
                if let Ok(item) = serde_json::from_str::<TaskItem>(&content) {
                    items.push(item);
                }
            }
        }
    }

    if items.is_empty() {
        return (None, 0, 0);
    }

    let total = items.len() as i32;
    let completed = items.iter().filter(|t| t.status == "completed").count() as i32;

    let active = items
        .iter()
        .find(|t| t.status == "in_progress")
        .or_else(|| items.iter().find(|t| t.status == "pending"))
        .map(|t| t.subject.clone());

    (active, completed, total)
}

fn determine_status(
    event_name: &str,
    notification_type: Option<&str>,
    current_status: &str,
) -> String {
    if event_name == "Stop" {
        return "stopped".to_string();
    }
    if event_name == "UserPromptSubmit" {
        return "running".to_string();
    }
    if current_status == "stopped" {
        return "stopped".to_string();
    }
    if event_name == "PreToolUse" {
        return "running".to_string();
    }
    if event_name == "Notification" && notification_type == Some("permission_prompt") {
        return "waiting_input".to_string();
    }
    "running".to_string()
}

fn update_session(
    event_name: &str,
    session_id: &str,
    cwd: &str,
    tty: &str,
    notification_type: Option<&str>,
) -> Result<String> {
    let mut store = read_session_store();
    let now_utc = Utc::now();
    let now = now_utc.to_rfc3339();

    // TTY重複排除: 同じTTY・別session_idのエントリを削除
    if !tty.is_empty() {
        store
            .sessions
            .retain(|k, s| s.tty != tty || k == session_id);
    }

    // 停止済みセッションの自動クリーンアップ (24時間以上前)
    store.sessions.retain(|_, s| {
        if s.status != "stopped" {
            return true;
        }
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&s.updated_at) {
            let age = now_utc.signed_duration_since(dt.with_timezone(&Utc));
            age < chrono::Duration::hours(24)
        } else {
            true // パース失敗は保持
        }
    });

    let existing = store.sessions.get(session_id);
    let current_status = existing.map(|s| s.status.as_str()).unwrap_or("");
    let created_at = existing
        .map(|s| s.created_at.clone())
        .unwrap_or_else(|| now.clone());
    let home_cwd = cwd.to_string();
    let final_tty = existing
        .and_then(|s| {
            if s.tty.is_empty() {
                None
            } else {
                Some(s.tty.clone())
            }
        })
        .unwrap_or_else(|| tty.to_string());

    // タスク進捗読み取り
    let (active_task, tasks_completed, tasks_total) = read_claude_tasks(session_id);

    let new_status = determine_status(event_name, notification_type, current_status);

    store.sessions.insert(
        session_id.to_string(),
        Session {
            session_id: session_id.to_string(),
            home_cwd,
            tty: final_tty,
            status: new_status.clone(),
            created_at,
            updated_at: now.clone(),
            active_task,
            tasks_completed,
            tasks_total,
        },
    );

    store.updated_at = now;
    write_session_store(&store)?;
    Ok(new_status)
}

// ============================================================================
// Hook Command (unified handler)
// ============================================================================

async fn cmd_hook(event_name: &str) -> Result<()> {
    // イベント名の正規化: 後方互換性のため小文字"stop"も受け付ける
    let event = if event_name.eq_ignore_ascii_case("stop") && event_name != "Stop" {
        "Stop"
    } else {
        event_name
    };

    if !VALID_HOOK_EVENTS.contains(&event) {
        eprintln!("未知のhookイベント: {}", event);
        print!("{{}}");
        return Ok(());
    }

    // 1. stdinからJSON読み取り
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;

    if input.trim().is_empty() {
        print!("{{}}");
        return Ok(());
    }

    let payload: HookPayload = match serde_json::from_str(&input) {
        Ok(p) => p,
        Err(_) => {
            print!("{{}}");
            return Ok(());
        }
    };

    if payload.session_id.is_empty() {
        print!("{{}}");
        return Ok(());
    }

    // 2. 親プロセスのTTYを取得
    let tty = get_tty_from_ancestors();

    let cwd = payload
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap().to_string_lossy().to_string());

    // 3-7. セッション更新
    let new_status = match update_session(
        event,
        &payload.session_id,
        &cwd,
        &tty,
        payload.notification_type.as_deref(),
    ) {
        Ok(status) => status,
        Err(e) => {
            eprintln!("セッション更新失敗: {}", e);
            String::new()
        }
    };

    // waiting_input時のデスクトップ通知 (クリックでWezTermペインに移動, 承認ボタンでEnter送信)
    if new_status == "waiting_input" {
        let dir_name = PathBuf::from(&cwd)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        // TTYからWezTermペインIDを特定
        let (activate_cmd, approve_cmd) = match find_wezterm_pane_by_tty(&tty) {
            Some((tab_id, pane_id)) => {
                let activate = format!(
                    "/opt/homebrew/bin/wezterm cli activate-tab --tab-id {} && /opt/homebrew/bin/wezterm cli activate-pane --pane-id {}",
                    tab_id, pane_id
                );
                let approve = format!(
                    "{} && /opt/homebrew/bin/wezterm cli send-text --pane-id {} --no-paste $'\\n'",
                    activate, pane_id
                );
                (activate, approve)
            }
            None => ("open -a WezTerm".to_string(), "open -a WezTerm".to_string()),
        };

        // bash -c でterminal-notifierの出力を判定し、ボタンに応じたアクションを実行
        let script = format!(
            r#"result=$(/opt/homebrew/bin/terminal-notifier -title 'Claude Code' -message '許可待ち: {}' -sound Tink -actions '承認' -sender com.github.wez.wezterm); if [ "$result" = "@ACTIONCLICKED" ]; then {}; elif [ "$result" = "@CONTENTCLICKED" ]; then {}; fi"#,
            dir_name, approve_cmd, activate_cmd
        );

        let _ = Command::new("bash")
            .args(["-c", &script])
            .spawn();
    }

    // 8. Stopの場合のみ: Asanaコメント投稿（既存処理）
    if event == "Stop" {
        let cwd_path = PathBuf::from(&cwd);
        let task_file = cwd_path.join(".claude/current-task.json");

        if task_file.exists() {
            if let Ok(content) = fs::read_to_string(&task_file) {
                if let Ok(task) = serde_json::from_str::<CurrentTask>(&content) {
                    let project_name = cwd_path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown");

                    let comment = format!(
                        "Claude Code作業セッション終了\n📁 {}",
                        project_name
                    );

                    match load_asana_config() {
                        Ok(asana_config) => {
                            let client = AsanaClient::new(asana_config);
                            if let Err(e) = client.post_comment(&task.gid, &comment).await {
                                eprintln!("Asanaコメント投稿失敗: {}", e);
                            }
                        }
                        Err(e) => {
                            eprintln!("Asana設定読み込み失敗: {}", e);
                        }
                    }
                }
            }
        }
    }

    // 9. 空のJSONを返す
    print!("{{}}");
    Ok(())
}
