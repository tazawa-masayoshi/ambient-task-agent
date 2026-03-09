use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

static CREDENTIALS_ENV: OnceLock<HashMap<String, String>> = OnceLock::new();

#[derive(Debug, Clone)]
pub struct SlackConfig {
    pub bot_token: String,
    pub test_channel: String,
    pub signing_secret: Option<String>,
    pub workspace: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AsanaConfig {
    pub pat: String,
    pub project_id: String,
    pub user_name: String,
}

/// Load environment variables from .env files + process env.
/// Priority: ./.env > ~/.credentials/ambient-task-agent.env > ~/.credentials/common.env > process env
/// Also sets loaded values into process env so std::env::var() can find them.
pub fn load_credentials_env() -> HashMap<String, String> {
    CREDENTIALS_ENV
        .get_or_init(|| {
            let mut map: HashMap<String, String> = std::env::vars().collect();

            let global_path = home_dir().join(".credentials/common.env");
            load_env_file(&global_path, &mut map);

            let agent_path = home_dir().join(".credentials/ambient-task-agent.env");
            load_env_file(&agent_path, &mut map);

            let local_path = PathBuf::from(".env");
            load_env_file(&local_path, &mut map);

            // .env ファイルから読んだ値を process env にも反映
            for (key, value) in &map {
                if std::env::var(key).is_err() {
                    std::env::set_var(key, value);
                }
            }

            map
        })
        .clone()
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

    let signing_secret = env.get("SLACK_SIGNING_SECRET").cloned();
    let workspace = env.get("SLACK_WORKSPACE").cloned();

    anyhow::ensure!(!bot_token.is_empty(), "SLACK_BOT_TOKEN is empty");

    Ok(SlackConfig {
        bot_token,
        test_channel,
        signing_secret,
        workspace,
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

#[derive(Debug, Clone)]
pub struct GoogleCalendarConfig {
    pub service_account_key_path: String,
    pub calendar_id: String,
}

pub fn load_google_calendar_config() -> Option<GoogleCalendarConfig> {
    let env = load_credentials_env();

    let key_path = env
        .get("GOOGLE_SERVICE_ACCOUNT_KEY")
        .cloned()
        .unwrap_or_else(|| {
            home_dir()
                .join(".credentials/Masayoshi.json")
                .to_string_lossy()
                .to_string()
        });

    if !std::path::Path::new(&key_path).exists() {
        return None;
    }

    let calendar_id = env.get("GOOGLE_CALENDAR_ID").cloned()?;

    Some(GoogleCalendarConfig {
        service_account_key_path: key_path,
        calendar_id,
    })
}

pub struct ServerConfig {
    pub asana_webhook_secret: Option<String>,
    pub repos_config_path: PathBuf,
    pub db_path: PathBuf,
}

pub fn load_server_config(config_dir: Option<&str>) -> Result<ServerConfig> {
    let env = load_credentials_env();

    let base = config_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config/ambient-task-agent"));

    let repos_config_path = base.join("repos.toml");
    let db_path = base.join("agent.db");

    let asana_webhook_secret = env.get("ASANA_WEBHOOK_SECRET").cloned();

    Ok(ServerConfig {
        asana_webhook_secret,
        repos_config_path,
        db_path,
    })
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}
