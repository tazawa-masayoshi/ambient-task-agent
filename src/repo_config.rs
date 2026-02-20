use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
pub struct ReposConfig {
    pub defaults: Defaults,
    #[serde(default)]
    pub repo: Vec<RepoEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Defaults {
    pub slack_channel: String,
    pub repos_base_dir: String,
    #[serde(default = "default_max_plan_turns")]
    pub claude_max_plan_turns: u32,
}

fn default_max_plan_turns() -> u32 {
    10
}

#[derive(Debug, Clone, Deserialize)]
pub struct RepoEntry {
    pub key: String,
    pub github: String,
    #[serde(default = "default_branch")]
    pub default_branch: String,
    #[serde(rename = "match")]
    pub match_rule: Option<MatchRule>,
}

fn default_branch() -> String {
    "main".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct MatchRule {
    pub project_gid: Option<String>,
    pub section_name: Option<String>,
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

    pub fn repo_local_path(&self, repo: &RepoEntry) -> PathBuf {
        PathBuf::from(&self.defaults.repos_base_dir).join(&repo.key)
    }
}
