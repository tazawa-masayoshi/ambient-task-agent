//! MCP サーバー設定の動的構築
//!
//! repos.toml のベース設定に対し、実行時にプロジェクトパスや環境変数を注入して
//! McpServerConfig を構築する。

use std::path::Path;

use super::mcp::McpServerConfig;
use crate::repo_config::RepoEntry;

/// RepoEntry + 実行時コンテキストから McpServerConfig[] を構築
pub fn build_mcp_configs(
    repo_entry: &RepoEntry,
    repo_path: &Path,
) -> Vec<McpServerConfig> {
    let mut configs: Vec<McpServerConfig> = repo_entry
        .mcp_servers
        .iter()
        .map(|c| {
            let mut config = c.clone();
            // Serena の --project 引数を動的注入
            if config.name == "serena" {
                inject_serena_project(&mut config, repo_path);
            }
            config.resolve_env();
            config
        })
        .collect();

    // 名前でソート（ツール順序の安定化 → プロンプトキャッシュヒット率向上）
    configs.sort_by(|a, b| a.name.cmp(&b.name));
    configs
}

/// Serena MCP の McpServerConfig をゼロから構築（repos.toml に未設定の場合）
#[allow(dead_code)]
pub fn serena_config(repo_path: &Path) -> McpServerConfig {
    McpServerConfig {
        name: "serena".to_string(),
        command: "uvx".to_string(),
        args: vec![
            "--from".to_string(),
            "git+https://github.com/oraios/serena".to_string(),
            "serena".to_string(),
            "start-mcp-server".to_string(),
            "--context".to_string(),
            "autonomous-agent".to_string(),
            "--project".to_string(),
            repo_path.to_string_lossy().to_string(),
        ],
        env: Default::default(),
    }
}

/// Serena の args に --project が未設定なら追加
fn inject_serena_project(config: &mut McpServerConfig, repo_path: &Path) {
    if !config.args.iter().any(|a| a == "--project") {
        config.args.push("--project".to_string());
        config.args.push(repo_path.to_string_lossy().to_string());
    }
}
