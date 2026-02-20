use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::asana::client::AsanaClient;
use crate::config::load_asana_config;
use crate::session;

#[derive(Debug, Deserialize)]
struct HookPayload {
    session_id: String,
    cwd: Option<String>,
    notification_type: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CurrentTask {
    pub gid: String,
    pub name: String,
}

const VALID_HOOK_EVENTS: &[&str] = &[
    "PreToolUse",
    "PostToolUse",
    "Notification",
    "Stop",
    "UserPromptSubmit",
];

pub async fn cmd_hook(event_name: &str) -> Result<()> {
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

    if payload.session_id.is_empty() {
        print!("{{}}");
        return Ok(());
    }

    let tty = session::get_tty_from_ancestors();

    let cwd = payload
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap().to_string_lossy().to_string());

    let new_status = match session::update_session(
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

    // waiting_input 時のデスクトップ通知
    if new_status == "waiting_input" {
        send_desktop_notification(&cwd, &tty);
    }

    // Stop 時: Asana コメント投稿
    if event == "Stop" {
        post_stop_comment(&cwd).await;
    }

    print!("{{}}");
    Ok(())
}

fn send_desktop_notification(cwd: &str, tty: &str) {
    let dir_name = PathBuf::from(cwd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    let (activate_cmd, approve_cmd) = match session::find_wezterm_pane_by_tty(tty) {
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

    let script = format!(
        r#"result=$(/opt/homebrew/bin/terminal-notifier -title 'Claude Code' -message '許可待ち: {}' -sound Tink -actions '承認' -sender com.github.wez.wezterm); if [ "$result" = "@ACTIONCLICKED" ]; then {}; elif [ "$result" = "@CONTENTCLICKED" ]; then {}; fi"#,
        dir_name, approve_cmd, activate_cmd
    );

    let _ = std::process::Command::new("bash")
        .args(["-c", &script])
        .spawn();
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
