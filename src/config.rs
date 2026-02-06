use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct KintoneConfig {
    pub domain: String,
    pub app_id: String,
    pub api_token: String,
}

#[derive(Debug, Clone)]
pub struct SlackConfig {
    pub bot_token: String,
    pub test_channel: String,
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    #[allow(dead_code)]
    pub kintone: Option<KintoneConfig>,
    pub slack: Option<SlackConfig>,
    #[allow(dead_code)]
    pub anthropic_api_key: Option<String>,
    pub openai_api_key: Option<String>,
}

/// Load environment variables from .env files.
/// Priority: ./env > ~/.credentials/common.env
pub fn load_credentials_env() -> HashMap<String, String> {
    let mut map = HashMap::new();

    // 1. Load from ~/.credentials/common.env (lower priority)
    let global_path = home_dir().join(".credentials/common.env");
    load_env_file(&global_path, &mut map);

    // 2. Load from ./.env (higher priority, overwrites)
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

pub fn load_app_config() -> AppConfig {
    let env = load_credentials_env();

    AppConfig {
        kintone: load_kintone_config_from_env(&env).ok(),
        slack: load_slack_config_from_env(&env).ok(),
        anthropic_api_key: env.get("ANTHROPIC_API_KEY").cloned(),
        openai_api_key: env.get("OPENAI_API_KEY").cloned(),
    }
}

pub fn load_kintone_config() -> Result<KintoneConfig> {
    let env = load_credentials_env();
    load_kintone_config_from_env(&env)
}

fn load_kintone_config_from_env(env: &HashMap<String, String>) -> Result<KintoneConfig> {
    let domain = env
        .get("KINTONE_DOMAIN")
        .context("KINTONE_DOMAIN not found")?
        .clone();
    let app_id = env
        .get("KINTONE_APP_ID_TASKS")
        .context("KINTONE_APP_ID_TASKS not found")?
        .clone();
    let api_token = env
        .get("KINTONE_API_TOKEN")
        .context("KINTONE_API_TOKEN not found")?
        .clone();

    anyhow::ensure!(!domain.is_empty(), "KINTONE_DOMAIN is empty");
    anyhow::ensure!(!app_id.is_empty(), "KINTONE_APP_ID_TASKS is empty");
    anyhow::ensure!(!api_token.is_empty(), "KINTONE_API_TOKEN is empty");

    Ok(KintoneConfig {
        domain,
        app_id,
        api_token,
    })
}

pub fn load_slack_config() -> Result<SlackConfig> {
    let env = load_credentials_env();
    load_slack_config_from_env(&env)
}

fn load_slack_config_from_env(env: &HashMap<String, String>) -> Result<SlackConfig> {
    let bot_token = env
        .get("SLACK_BOT_TOKEN")
        .context("SLACK_BOT_TOKEN not found")?
        .clone();
    let test_channel = env
        .get("SLACK_TEST_CHANNEL")
        .context("SLACK_TEST_CHANNEL not found")?
        .clone();

    anyhow::ensure!(!bot_token.is_empty(), "SLACK_BOT_TOKEN is empty");

    Ok(SlackConfig {
        bot_token,
        test_channel,
    })
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}
