use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
pub struct ReposConfig {
    pub defaults: Defaults,
    #[serde(default)]
    pub repo: Vec<RepoEntry>,
    #[serde(default)]
    pub schedule: Vec<ScheduleEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Defaults {
    pub slack_channel: String,
    pub repos_base_dir: String,
    #[serde(default = "default_max_plan_turns")]
    pub claude_max_plan_turns: u32,
    #[allow(dead_code)]
    #[serde(default = "default_max_execute_turns")]
    pub claude_max_execute_turns: u32,
    #[serde(default = "default_heartbeat_secs")]
    pub worker_heartbeat_secs: u64,
    pub google_calendar_id: Option<String>,
    #[serde(default = "default_stagnation_hours")]
    pub stagnation_threshold_hours: i64,
}

fn default_max_plan_turns() -> u32 {
    10
}

fn default_max_execute_turns() -> u32 {
    20
}

fn default_heartbeat_secs() -> u64 {
    60
}

fn default_stagnation_hours() -> i64 {
    24
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct RepoEntry {
    pub key: String,
    pub github: String,
    #[serde(default = "default_branch")]
    pub default_branch: String,
    #[serde(rename = "match")]
    pub match_rule: Option<MatchRule>,
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
    pub max_execute_turns: Option<u32>,
}

fn default_branch() -> String {
    "main".to_string()
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct MatchRule {
    pub project_gid: Option<String>,
    pub section_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct ScheduleEntry {
    pub key: String,
    pub cron: String,
    pub job_type: String,
    #[serde(default)]
    pub prompt: String,
    pub slack_channel: Option<String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

impl ReposConfig {
    pub fn load(path: &PathBuf) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read repos config: {}", path.display()))?;
        let config: ReposConfig =
            toml::from_str(&content).with_context(|| "Failed to parse repos.toml")?;
        Ok(config)
    }

    /// Asana プロジェクトGID からマッチするリポジトリを検索
    pub fn find_repo_by_project(&self, project_gid: &str) -> Option<&RepoEntry> {
        self.repo.iter().find(|r| {
            r.match_rule
                .as_ref()
                .and_then(|m| m.project_gid.as_deref())
                == Some(project_gid)
        })
    }

    pub fn find_repo_by_key(&self, key: &str) -> Option<&RepoEntry> {
        self.repo.iter().find(|r| r.key == key)
    }

    pub fn repo_local_path(&self, repo: &RepoEntry) -> PathBuf {
        PathBuf::from(&self.defaults.repos_base_dir).join(&repo.key)
    }
}
