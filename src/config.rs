use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct SlackConfig {
    pub bot_token: String,
    pub test_channel: String,
}

#[derive(Debug, Clone)]
pub struct AsanaConfig {
    pub pat: String,
    pub project_id: String,
    pub user_name: String,
}

/// Load environment variables from .env files.
/// Priority: ./.env > ~/.credentials/common.env
pub fn load_credentials_env() -> HashMap<String, String> {
    let mut map = HashMap::new();

    let global_path = home_dir().join(".credentials/common.env");
    load_env_file(&global_path, &mut map);

    let local_path = PathBuf::from(".env");
    load_env_file(&local_path, &mut map);

    map
}

fn load_env_file(path: &PathBuf, map: &mut HashMap<String, String>) {
    if let Ok(content) = fs::read_to_string(path) {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                let value = value.trim().trim_matches('"').trim_matches('\'');
                map.insert(key.trim().to_string(), value.to_string());
            }
        }
    }
}

pub fn load_slack_config() -> Result<SlackConfig> {
    let env = load_credentials_env();
    let bot_token = env
        .get("SLACK_BOT_TOKEN")
        .context("SLACK_BOT_TOKEN not found in .env")?
        .clone();
    let test_channel = env
        .get("SLACK_TEST_CHANNEL")
        .context("SLACK_TEST_CHANNEL not found in .env")?
        .clone();

    anyhow::ensure!(!bot_token.is_empty(), "SLACK_BOT_TOKEN is empty");

    Ok(SlackConfig {
        bot_token,
        test_channel,
    })
}

pub fn load_asana_config() -> Result<AsanaConfig> {
    let env = load_credentials_env();
    let pat = env
        .get("ASANA_PAT")
        .context("ASANA_PAT not found in .env")?
        .clone();
    let project_id = env
        .get("ASANA_PROJECT_ID")
        .context("ASANA_PROJECT_ID not found in .env")?
        .clone();
    let user_name = env
        .get("ASANA_USER_NAME")
        .unwrap_or(&"田澤雅義".to_string())
        .clone();

    anyhow::ensure!(!pat.is_empty(), "ASANA_PAT is empty");
    anyhow::ensure!(!project_id.is_empty(), "ASANA_PROJECT_ID is empty");

    Ok(AsanaConfig {
        pat,
        project_id,
        user_name,
    })
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}
