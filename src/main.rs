mod asana;
mod claude;
mod config;
mod db;
mod google;
mod hook;
mod repo_config;
mod server;
mod session;
mod slack;
mod sync;
mod worker;

use anyhow::Result;
use chrono::Local;
use clap::{Parser, Subcommand};
use std::fs;
use std::path::PathBuf;

use crate::config::{load_asana_config, load_google_calendar_config, load_server_config, load_slack_config};
use crate::hook::CurrentTask;
use crate::slack::client::SlackClient;
use crate::sync::{cache_path, load_cache, CachedTask};

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
        #[arg(long)]
        quiet: bool,
    },
    /// キャッシュ済みタスクを表示
    Show {
        #[arg(long)]
        mine: bool,
        #[arg(long)]
        json: bool,
    },
    /// Slackにメッセージ送信
    Notify {
        #[arg(short, long)]
        message: String,
        #[arg(short, long)]
        channel: Option<String>,
    },
    /// タスク完了をSlackに通知
    Done {
        #[arg(short, long)]
        task: String,
    },
    /// キャッシュの状態を表示
    Status,
    /// Claude Code hookイベント処理
    Hook {
        event: String,
    },
    /// 作業タスクを設定
    Start {
        query: Option<String>,
        #[arg(long)]
        gid: Option<String>,
    },
    /// 現在の作業タスクを表示
    Current,
    /// 自律エージェントサーバーを起動
    Serve {
        #[arg(short, long, default_value = "3000")]
        port: u16,
        #[arg(long)]
        config_dir: Option<String>,
    },
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
        Commands::Hook { event } => hook::cmd_hook(&event).await?,
        Commands::Start { query, gid } => cmd_start(query, gid)?,
        Commands::Current => cmd_current()?,
        Commands::Serve { port, config_dir } => cmd_serve(port, config_dir.as_deref()).await?,
    }

    Ok(())
}

// ============================================================================
// Sync / Show / Status
// ============================================================================

async fn cmd_sync(quiet: bool) -> Result<()> {
    let asana_config = load_asana_config()?;
    let result = sync::run_sync(&asana_config).await?;

    if quiet {
        if result.changed {
            let output = serde_json::json!({
                "changed": true,
                "diff": result.diff,
                "summary": {
                    "total": result.cache.summary.total,
                    "incomplete": result.cache.summary.incomplete,
                    "my_tasks": result.cache.summary.my_tasks,
                    "overdue": result.cache.summary.overdue,
                }
            });
            println!("{}", serde_json::to_string(&output)?);
        }
    } else {
        println!(
            "Asana同期完了: 全{}件 (未完了: {}, 自分: {}, 期限超過: {})",
            result.cache.summary.total,
            result.cache.summary.incomplete,
            result.cache.summary.my_tasks,
            result.cache.summary.overdue
        );
        if result.changed {
            if result.diff.is_empty() {
                println!("変更あり（詳細不明 - 初回同期または構造変更）");
            } else {
                println!("\n変更点:");
                for d in &result.diff {
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
        cache
            .tasks
            .iter()
            .filter(|t| !t.completed && t.assignee.contains(&user_name))
            .collect()
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

// ============================================================================
// Notify / Done
// ============================================================================

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

// ============================================================================
// Start / Current
// ============================================================================

fn current_task_path() -> PathBuf {
    PathBuf::from(".claude/current-task.json")
}

fn cmd_start(query: Option<String>, gid: Option<String>) -> Result<()> {
    let cache = load_cache()?;
    let incomplete: Vec<&CachedTask> = cache.tasks.iter().filter(|t| !t.completed).collect();

    let task = if let Some(gid) = gid {
        incomplete
            .iter()
            .find(|t| t.gid == gid)
            .ok_or_else(|| anyhow::anyhow!("GID {} のタスクが見つかりません", gid))?
    } else if let Some(ref q) = query {
        let matches: Vec<&&CachedTask> = incomplete
            .iter()
            .filter(|t| t.name.contains(q.as_str()))
            .collect();

        match matches.len() {
            0 => anyhow::bail!("「{}」に一致するタスクが見つかりません", q),
            1 => matches[0],
            _ => {
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
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({
                        "status": "multiple",
                        "candidates": candidates,
                    }))?
                );
                return Ok(());
            }
        }
    } else {
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
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "status": "multiple",
                "candidates": candidates,
            }))?
        );
        return Ok(());
    };

    let ct = CurrentTask {
        gid: task.gid.clone(),
        name: task.name.clone(),
    };
    let path = current_task_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, serde_json::to_string_pretty(&ct)?)?;

    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "status": "ok",
            "task": { "gid": task.gid, "name": task.name },
        }))?
    );
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
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "status": "ok",
            "task": { "gid": task.gid, "name": task.name },
        }))?
    );
    Ok(())
}

// ============================================================================
// Serve
// ============================================================================

async fn cmd_serve(port: u16, config_dir: Option<&str>) -> Result<()> {
    tracing_subscriber::fmt::init();

    let server_config = load_server_config(config_dir)?;
    let slack_config = load_slack_config()?;
    let asana_config = load_asana_config()?;

    let db = db::Db::open(&server_config.db_path)?;
    let repos_config = repo_config::ReposConfig::load(&server_config.repos_config_path)?;

    let slack_client = SlackClient::new(slack_config.clone());
    let default_channel = repos_config.defaults.slack_channel.clone();

    let app_state = server::http::AppState {
        db: db.clone(),
        repos_config: repos_config.clone(),
        asana_webhook_secret: server_config.asana_webhook_secret,
        slack_bot_token: slack_config.bot_token.clone(),
        slack_channel: default_channel.clone(),
        slack_signing_secret: slack_config.signing_secret.clone(),
        asana_pat: asana_config.pat.clone(),
        asana_project_id: asana_config.project_id.clone(),
        asana_user_name: asana_config.user_name.clone(),
        slack_workspace: slack_config.workspace.clone(),
    };

    // Google Calendar クライアント初期化
    let gcal_client = load_google_calendar_config().and_then(|gcal_config| {
        let calendar_id = repos_config
            .defaults
            .google_calendar_id
            .as_deref()
            .unwrap_or(&gcal_config.calendar_id);
        match google::calendar::GoogleCalendarClient::new(
            &gcal_config.service_account_key_path,
            calendar_id,
        ) {
            Ok(c) => {
                tracing::info!("Google Calendar client initialized (calendar: {})", calendar_id);
                Some(c)
            }
            Err(e) => {
                tracing::warn!("Google Calendar not available: {}", e);
                None
            }
        }
    });

    // ワーカーを別タスクで起動
    let worker = worker::runner::Worker::new(
        db,
        repos_config,
        slack_client,
        asana_config.pat.clone(),
        asana_config.project_id.clone(),
        asana_config.user_name.clone(),
        gcal_client,
        default_channel,
        slack_config.workspace.clone(),
    );
    tokio::spawn(async move {
        worker.run().await;
    });

    // HTTP サーバー起動
    server::http::run_server(app_state, port).await
}
