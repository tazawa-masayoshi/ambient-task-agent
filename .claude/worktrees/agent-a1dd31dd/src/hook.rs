use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::asana::client::AsanaClient;
use crate::config::load_asana_config;

#[derive(Debug, Deserialize)]
struct HookPayload {
    #[allow(dead_code)]
    session_id: String,
    cwd: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CurrentTask {
    pub gid: String,
    pub name: String,
}

pub async fn cmd_hook(event_name: &str) -> Result<()> {
    // Stop イベント以外は何もしない（セッション管理は wez-sidebar 側で処理）
    if !event_name.eq_ignore_ascii_case("stop") {
        print!("{{}}");
        return Ok(());
    }

    let mut input = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)?;

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

    let cwd = payload
        .cwd
        .unwrap_or_else(|| std::env::current_dir().unwrap().to_string_lossy().to_string());

    post_stop_comment(&cwd).await;

    print!("{{}}");
    Ok(())
}

async fn post_stop_comment(cwd: &str) {
    let cwd_path = PathBuf::from(cwd);
    let task_file = cwd_path.join(".claude/current-task.json");

    if !task_file.exists() {
        return;
    }

    let content = match std::fs::read_to_string(&task_file) {
        Ok(c) => c,
        Err(_) => return,
    };

    let task: CurrentTask = match serde_json::from_str(&content) {
        Ok(t) => t,
        Err(_) => return,
    };

    let project_name = cwd_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    let comment = format!("Claude Code作業セッション終了\n📁 {}", project_name);

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
