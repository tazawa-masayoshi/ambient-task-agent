use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::sync::cache_dir;

const SESSIONS_FILE: &str = "sessions.json";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionsFile {
    pub sessions: HashMap<String, Session>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub session_id: String,
    pub home_cwd: String,
    pub tty: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub active_task: Option<String>,
    #[serde(default)]
    pub tasks_completed: i32,
    #[serde(default)]
    pub tasks_total: i32,
    #[serde(default)]
    pub is_yolo: bool,
}

fn sessions_path() -> PathBuf {
    cache_dir().join(SESSIONS_FILE)
}

pub fn read_session_store() -> SessionsFile {
    let path = sessions_path();
    match std::fs::read_to_string(&path) {
        Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
        Err(_) => SessionsFile::default(),
    }
}

fn write_session_store(store: &SessionsFile) -> Result<()> {
    let path = sessions_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let data = serde_json::to_string_pretty(store)?;
    std::fs::write(path, data)?;
    Ok(())
}

pub fn get_tty_from_ancestors() -> String {
    let mut ppid = std::os::unix::process::parent_id() as i32;

    for _ in 0..5 {
        let output = std::process::Command::new("ps")
            .args(["-o", "tty=", "-p", &ppid.to_string()])
            .output();

        if let Ok(out) = output {
            let tty = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !tty.is_empty() && tty != "??" {
                return format!("/dev/{}", tty);
            }
        }

        let output = std::process::Command::new("ps")
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

pub fn detect_yolo_mode() -> bool {
    let mut ppid = std::os::unix::process::parent_id() as i32;

    for _ in 0..5 {
        let output = std::process::Command::new("ps")
            .args(["-o", "args=", "-p", &ppid.to_string()])
            .output();

        if let Ok(out) = output {
            let args = String::from_utf8_lossy(&out.stdout);
            if args.contains("--dangerously-skip-permissions") {
                return true;
            }
        }

        let output = std::process::Command::new("ps")
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

    false
}

pub fn find_wezterm_pane_by_tty(tty: &str) -> Option<(i32, i32)> {
    if tty.is_empty() {
        return None;
    }

    let output = std::process::Command::new("/opt/homebrew/bin/wezterm")
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

pub fn read_claude_tasks(session_id: &str) -> (Option<String>, i32, i32) {
    let tasks_dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()))
        .join(".claude/tasks")
        .join(session_id);

    let entries = match std::fs::read_dir(&tasks_dir) {
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
            if let Ok(content) = std::fs::read_to_string(entry.path()) {
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

pub fn determine_status(
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

pub fn update_session(
    event_name: &str,
    session_id: &str,
    cwd: &str,
    tty: &str,
    notification_type: Option<&str>,
    is_yolo: bool,
) -> Result<String> {
    let mut store = read_session_store();
    let now_utc = Utc::now();
    let now = now_utc.to_rfc3339();

    if !tty.is_empty() {
        store
            .sessions
            .retain(|k, s| s.tty != tty || k == session_id);
    }

    store.sessions.retain(|_, s| {
        if s.status != "stopped" {
            return true;
        }
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&s.updated_at) {
            let age = now_utc.signed_duration_since(dt.with_timezone(&Utc));
            age < chrono::Duration::hours(12)
        } else {
            true
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
            is_yolo,
        },
    );

    store.updated_at = now;
    write_session_store(&store)?;
    Ok(new_status)
}
