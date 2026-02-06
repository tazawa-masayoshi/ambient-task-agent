mod agent;
mod calendar;
mod config;
mod kintone;
mod priority;
mod skills;
mod slack;

use std::sync::Arc;

use anyhow::Result;
use chrono::Local;
use clap::{Parser, Subcommand};

use crate::agent::orchestrator::{AgentContext, AgentOrchestrator};
use crate::config::{load_app_config, load_kintone_config, load_slack_config};
use crate::kintone::client::KintoneClient;
use crate::kintone::poller;
use crate::priority::sort_by_priority;
use crate::skills::mock_skills::*;
use crate::skills::registry::SkillRegistry;
use crate::skills::slack_skills::*;
use crate::slack::client::SlackClient;

#[derive(Parser)]
#[command(name = "ambient-task-agent")]
#[command(about = "kintoneタスク監視・管理エージェント")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// kintoneからアクティブなタスクを取得して表示
    Fetch,
    /// タスクのステータスを更新
    UpdateStatus {
        /// レコードID
        #[arg(short = 'i', long)]
        id: String,
        /// 新しいステータス (todo, in_progress, done)
        #[arg(short, long)]
        status: String,
    },
    /// 新しいタスクを追加
    Add {
        /// タスクタイトル
        #[arg(short, long)]
        title: String,
        /// 優先度 (urgent, this_week, someday)
        #[arg(short, long, default_value = "someday")]
        priority: String,
        /// タスクタイプ (request, todo)
        #[arg(long, default_value = "todo")]
        task_type: String,
        /// 親タスクID
        #[arg(long)]
        parent_id: Option<String>,
        /// 説明
        #[arg(short, long, default_value = "")]
        description: String,
    },
    /// Slackテストチャンネルにメッセージ送信
    SlackTest {
        /// 送信するメッセージ
        #[arg(short, long, default_value = "🤖 ambient-task-agent からのテストメッセージです")]
        message: String,
    },
    /// エージェントを実行（LLM判断ループ）
    Agent {
        /// エージェントへの指示
        #[arg(short, long)]
        message: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("ambient_task_agent=info".parse()?),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Fetch => {
            let config = load_kintone_config()?;
            let client = KintoneClient::new(config);
            cmd_fetch(&client).await?
        }
        Commands::UpdateStatus { id, status } => {
            let config = load_kintone_config()?;
            let client = KintoneClient::new(config);
            cmd_update_status(&client, &id, &status).await?
        }
        Commands::Add {
            title,
            priority,
            task_type,
            parent_id,
            description,
        } => {
            let config = load_kintone_config()?;
            let client = KintoneClient::new(config);
            cmd_add(
                &client,
                &title,
                &priority,
                &task_type,
                parent_id.as_deref(),
                &description,
            )
            .await?
        }
        Commands::SlackTest { message } => {
            let config = load_slack_config()?;
            let client = SlackClient::new(config);
            cmd_slack_test(&client, &message).await?
        }
        Commands::Agent { message } => {
            cmd_agent(&message).await?
        }
    }

    Ok(())
}

async fn cmd_fetch(client: &KintoneClient) -> Result<()> {
    let mut tasks = poller::fetch_active_tasks(client).await?;
    sort_by_priority(&mut tasks);

    if tasks.is_empty() {
        println!("アクティブなタスクはありません");
        return Ok(());
    }

    for task in &tasks {
        let priority_icon = match task.priority.as_str() {
            "urgent" => "🔴",
            "this_week" => "🟡",
            _ => "🟢",
        };
        let status_mark = match task.status.as_str() {
            "in_progress" => "▶",
            "done" => "✓",
            _ => " ",
        };
        let type_tag = if task.task_type.is_empty() {
            String::new()
        } else {
            format!(" [{}]", task.task_type)
        };
        let parent_tag = task
            .parent_id
            .map(|pid| format!(" (parent:{})", pid))
            .unwrap_or_default();

        println!(
            "{} {} #{} {}{}{}",
            priority_icon, status_mark, task.id, task.title, type_tag, parent_tag
        );
    }

    println!("\n合計: {}件", tasks.len());
    Ok(())
}

async fn cmd_update_status(client: &KintoneClient, id: &str, status: &str) -> Result<()> {
    let valid = ["todo", "in_progress", "done"];
    anyhow::ensure!(
        valid.contains(&status),
        "無効なステータス: {}（有効値: {}）",
        status,
        valid.join(", ")
    );

    client.update_status(id, status).await?;
    println!("レコード #{} のステータスを {} に更新しました", id, status);
    Ok(())
}

async fn cmd_add(
    client: &KintoneClient,
    title: &str,
    priority: &str,
    task_type: &str,
    parent_id: Option<&str>,
    description: &str,
) -> Result<()> {
    let mut fields = serde_json::json!({
        "タイトル": { "value": title },
        "status": { "value": "todo" },
        "優先度": { "value": priority },
        "task_type": { "value": task_type },
    });

    if !description.is_empty() {
        fields["description"] = serde_json::json!({ "value": description });
    }
    if let Some(pid) = parent_id {
        fields["parent_id"] = serde_json::json!({ "value": pid });
    }

    let new_id = client.add_record(fields).await?;
    println!("タスクを作成しました: #{} {}", new_id, title);
    Ok(())
}

async fn cmd_slack_test(client: &SlackClient, message: &str) -> Result<()> {
    let ts = client.post_test(message).await?;
    println!("Slack送信成功 (ts: {})", ts);
    Ok(())
}

async fn cmd_agent(message: &str) -> Result<()> {
    let app_config = load_app_config();

    // APIキー確認 (OpenAI)
    let api_key = app_config
        .openai_api_key
        .ok_or_else(|| anyhow::anyhow!("OPENAI_API_KEY not found in .env"))?;

    // Skills登録
    let mut registry = SkillRegistry::new();

    // モックスキル（kintone/calendar代わり）
    registry.register(MockFetchTasks);
    registry.register(MockUpdateTaskStatus);
    registry.register(MockAddTask);
    registry.register(MockGetCalendar);
    registry.register(MockFindFreeSlots);

    // Slackスキル（実際のAPI）
    if let Some(slack_config) = app_config.slack {
        let slack_client = Arc::new(SlackClient::new(slack_config));
        registry.register(PostSlackMessage::new(slack_client.clone()));
        registry.register(ReplySlackThread::new(slack_client));
    }

    println!("登録済みスキル: {:?}", registry.list());

    // Agent実行
    let agent = AgentOrchestrator::new(api_key, registry);
    let context = AgentContext {
        current_time: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        additional_context: None,
    };

    println!("\n🤖 エージェント実行中...\n");
    let response = agent.run(message, &context).await?;
    println!("\n📝 エージェントの応答:\n{}", response);

    Ok(())
}
