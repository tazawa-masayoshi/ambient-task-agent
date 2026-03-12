use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ExecMode {
    Deny,
    #[default]
    Normal,
    DryRun,
}

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
    /// ops スレッド返信を許可する Slack ユーザーID
    pub ops_admin_user: Option<String>,
    /// wez-sidebar 用タスクキャッシュファイルのパス
    pub tasks_cache_file: Option<String>,
    #[serde(default = "default_stagnation_hours")]
    pub stagnation_threshold_hours: i64,
    #[serde(default = "default_timeout_secs")]
    pub claude_timeout_secs: u64,
    #[serde(default = "default_max_output_bytes")]
    pub claude_max_output_bytes: usize,
    #[serde(default)]
    pub claude_exec_mode: ExecMode,
    #[serde(default = "default_max_concurrent")]
    pub claude_max_concurrent: usize,
    #[serde(default = "default_allowed_env")]
    pub claude_allowed_env: Vec<String>,
    #[serde(default)]
    pub module_policy: HashMap<String, ModulePolicy>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ModulePolicy {
    pub exec_mode: Option<ExecMode>,
    pub timeout_secs: Option<u64>,
}

fn default_max_plan_turns() -> u32 {
    10
}

fn default_max_execute_turns() -> u32 {
    20
}

fn default_heartbeat_secs() -> u64 {
    15
}

fn default_stagnation_hours() -> i64 {
    24
}

fn default_timeout_secs() -> u64 {
    600
}

fn default_max_output_bytes() -> usize {
    100_000
}

fn default_max_concurrent() -> usize {
    2
}

fn default_allowed_env() -> Vec<String> {
    vec![
        "PATH".to_string(),
        "HOME".to_string(),
        "USER".to_string(),
        "SHELL".to_string(),
        "LANG".to_string(),
        "LC_ALL".to_string(),
    ]
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
    pub ops_channel: Option<String>,
    #[serde(default)]
    pub ops_skills: Option<Vec<String>>,
    /// Slack 添付ファイルの保存先ディレクトリ（repo_path からの相対パス）
    /// 未設定の場合はファイルをダウンロードしない
    pub ops_download_dir: Option<String>,
    /// true: チャンネルのトップレベルメッセージを自動監視して作業対象か判定
    /// false/未設定: ⚡リアクションによる手動トリガーのみ
    #[serde(default)]
    pub ops_monitor: bool,
    /// true: 分析後に自動実行 + PR 作成（承認スキップ）
    #[serde(default)]
    pub auto_execute: bool,
    /// CI 失敗時の最大リトライ回数（デフォルト: 3）
    #[serde(default = "default_ci_max_retry")]
    pub ci_max_retry: u32,
}

fn default_ci_max_retry() -> u32 {
    3
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

impl Defaults {
    /// モジュール固有ポリシーを解決して (ExecMode, timeout_secs) を返す
    pub fn resolve_for_module(&self, module: &str) -> (ExecMode, u64) {
        let mp = self.module_policy.get(module);
        // global Deny は常に勝つ
        let exec_mode = if self.claude_exec_mode == ExecMode::Deny {
            ExecMode::Deny
        } else {
            mp.and_then(|p| p.exec_mode.clone())
                .unwrap_or(self.claude_exec_mode.clone())
        };
        let timeout = mp
            .and_then(|p| p.timeout_secs)
            .unwrap_or(self.claude_timeout_secs);
        (exec_mode, timeout)
    }
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

    /// ops_channel から対応するリポジトリを検索
    pub fn find_repo_by_ops_channel(&self, channel: &str) -> Option<&RepoEntry> {
        self.repo
            .iter()
            .find(|r| r.ops_channel.as_deref() == Some(channel))
    }

    pub fn find_repo_by_key(&self, key: &str) -> Option<&RepoEntry> {
        self.repo.iter().find(|r| r.key == key)
    }

    pub fn repo_local_path(&self, repo: &RepoEntry) -> PathBuf {
        PathBuf::from(&self.defaults.repos_base_dir).join(&repo.key)
    }

    /// ops_channel のチャンネル名を Slack チャンネルID に解決する。
    /// channel_map: チャンネル名 → チャンネルID のマッピング（Slack API から取得）
    pub fn resolve_ops_channels(&mut self, channel_map: &HashMap<String, String>) {
        for repo in &mut self.repo {
            if let Some(ref name) = repo.ops_channel {
                // 既に Slack チャンネルID（C + 大文字英数字、9文字以上）の場合はスキップ
                if name.starts_with('C')
                    && name.len() >= 9
                    && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
                {
                    continue;
                }
                if let Some(id) = channel_map.get(name) {
                    tracing::info!("Resolved ops_channel: {} -> {}", name, id);
                    repo.ops_channel = Some(id.clone());
                } else {
                    tracing::warn!("ops_channel '{}' not found in bot's channels (repo: {})", name, repo.key);
                }
            }
        }
    }
}
