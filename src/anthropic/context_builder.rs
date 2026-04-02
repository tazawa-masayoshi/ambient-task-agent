//! コンテキスト組立 — claw-code prompt.rs 参考
//!
//! git status + git diff + cwd + 日付 + ツールリストを
//! system prompt に自動注入する。

use std::path::Path;
use std::process::Command;

use super::types::ToolDefinition;

/// プロジェクトコンテキスト（git 状態 + 環境情報）
pub struct ProjectContext {
    pub cwd: String,
    pub current_date: String,
    pub git_status: Option<String>,
    pub git_diff_summary: Option<String>,
    pub git_recent_commits: Option<String>,
}

impl ProjectContext {
    /// cwd から git 情報を自動取得して ProjectContext を構築
    pub fn discover(cwd: &Path) -> Self {
        let current_date = chrono::Local::now().format("%Y-%m-%d").to_string();
        let cwd_str = cwd.to_string_lossy().to_string();

        let git_status = run_git(cwd, &["status", "--short", "--branch"])
            .map(|s| truncate_output(&s, 2000));

        let git_diff_summary = run_git(cwd, &["diff", "--stat"])
            .map(|s| truncate_output(&s, 1000));

        let git_recent_commits = run_git(
            cwd,
            &["log", "--oneline", "--no-decorate", "-5"],
        )
        .map(|s| truncate_output(&s, 500));

        Self {
            cwd: cwd_str,
            current_date,
            git_status,
            git_diff_summary,
            git_recent_commits,
        }
    }
}

/// system prompt にプロジェクトコンテキストを注入
///
/// base_system_prompt の末尾に環境情報セクションを追加する。
/// claw-code の SystemPromptBuilder 相当だが、ops エージェント用に簡略化。
pub fn build_enriched_system_prompt(
    base_prompt: &str,
    ctx: &ProjectContext,
    tools: &[ToolDefinition],
) -> String {
    let mut sections = Vec::new();
    sections.push(base_prompt.to_string());

    // 環境コンテキスト
    let mut env_section = format!(
        "\n\n## 環境コンテキスト\n\
         - 作業ディレクトリ: {}\n\
         - 日付: {}",
        ctx.cwd, ctx.current_date
    );

    if !tools.is_empty() {
        let tool_names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        env_section.push_str(&format!("\n- 利用可能ツール: {}", tool_names.join(", ")));
    }

    sections.push(env_section);

    // Git コンテキスト
    let has_git = ctx.git_status.is_some()
        || ctx.git_diff_summary.is_some()
        || ctx.git_recent_commits.is_some();

    if has_git {
        let mut git_section = String::from("\n\n## プロジェクト状態");

        if let Some(ref status) = ctx.git_status {
            if !status.trim().is_empty() {
                git_section.push_str(&format!("\n### git status\n```\n{}\n```", status.trim()));
            }
        }

        if let Some(ref diff) = ctx.git_diff_summary {
            if !diff.trim().is_empty() {
                git_section
                    .push_str(&format!("\n### 変更ファイル (git diff --stat)\n```\n{}\n```", diff.trim()));
            }
        }

        if let Some(ref commits) = ctx.git_recent_commits {
            if !commits.trim().is_empty() {
                git_section
                    .push_str(&format!("\n### 直近のコミット\n```\n{}\n```", commits.trim()));
            }
        }

        sections.push(git_section);
    }

    sections.join("")
}

/// git コマンドを実行して stdout を返す。失敗時は None。
fn run_git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    if stdout.trim().is_empty() {
        return None;
    }

    Some(stdout)
}

/// 出力を max_chars で切り詰め
fn truncate_output(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars).collect();
        format!("{}...\n[truncated, {} total chars]", truncated, s.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_output_short() {
        let s = "hello world";
        assert_eq!(truncate_output(s, 100), "hello world");
    }

    #[test]
    fn test_truncate_output_long() {
        let s = "a".repeat(200);
        let result = truncate_output(&s, 50);
        assert!(result.contains("[truncated"));
        assert!(result.starts_with(&"a".repeat(50)));
    }

    #[test]
    fn test_build_enriched_system_prompt() {
        let ctx = ProjectContext {
            cwd: "/tmp/test-repo".to_string(),
            current_date: "2026-04-03".to_string(),
            git_status: Some("## main\n M src/main.rs".to_string()),
            git_diff_summary: None,
            git_recent_commits: Some("abc1234 feat: something".to_string()),
        };

        let tools = vec![ToolDefinition {
            name: "Bash".to_string(),
            description: "Execute bash".to_string(),
            input_schema: serde_json::json!({}),
        }];

        let result = build_enriched_system_prompt("Base prompt.", &ctx, &tools);

        assert!(result.starts_with("Base prompt."));
        assert!(result.contains("作業ディレクトリ: /tmp/test-repo"));
        assert!(result.contains("日付: 2026-04-03"));
        assert!(result.contains("利用可能ツール: Bash"));
        assert!(result.contains("## main"));
        assert!(result.contains("abc1234 feat: something"));
    }

    #[test]
    fn test_build_enriched_no_git() {
        let ctx = ProjectContext {
            cwd: "/tmp/no-git".to_string(),
            current_date: "2026-04-03".to_string(),
            git_status: None,
            git_diff_summary: None,
            git_recent_commits: None,
        };

        let result = build_enriched_system_prompt("Base.", &ctx, &[]);
        assert!(result.starts_with("Base."));
        assert!(!result.contains("プロジェクト状態"));
    }
}
