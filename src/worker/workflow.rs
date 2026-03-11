use anyhow::Result;
use serde::Deserialize;
use std::path::Path;

/// WORKFLOW.md の YAML front matter で指定可能な設定
#[derive(Debug, Clone, Default, Deserialize)]
#[allow(dead_code)]
pub struct WorkflowConfig {
    /// executor の最大ターン数（RepoEntry.max_execute_turns を上書き）
    pub max_execute_turns: Option<u32>,
    /// 許可ツール（RepoEntry.allowed_tools を上書き）
    pub allowed_tools: Option<Vec<String>>,
    /// 自動実行フラグ（RepoEntry.auto_execute を上書き）
    pub auto_execute: Option<bool>,
    /// CI 最大リトライ回数（RepoEntry.ci_max_retry を上書き）
    pub ci_max_retry: Option<u32>,
}

/// WORKFLOW.md のパース結果
#[derive(Debug, Clone)]
pub struct Workflow {
    /// YAML front matter から取得した設定
    pub config: WorkflowConfig,
    /// Markdown 本文（per-repo soul として利用）
    pub body: String,
}

/// WORKFLOW.md のパス
fn workflow_path(repo_path: &Path) -> std::path::PathBuf {
    repo_path.join("WORKFLOW.md")
}

/// WORKFLOW.md を読み込んでパース。ファイルがなければ None
pub fn load(repo_path: &Path) -> Option<Workflow> {
    let path = workflow_path(repo_path);
    let content = std::fs::read_to_string(&path).ok()?;
    match parse(&content) {
        Ok(wf) => Some(wf),
        Err(e) => {
            tracing::warn!("Failed to parse WORKFLOW.md at {}: {}", path.display(), e);
            None
        }
    }
}

/// YAML front matter + Markdown body をパース
fn parse(content: &str) -> Result<Workflow> {
    let trimmed = content.trim_start();

    if !trimmed.starts_with("---") {
        // front matter なし → 全体が body
        return Ok(Workflow {
            config: WorkflowConfig::default(),
            body: content.to_string(),
        });
    }

    // 最初の --- を飛ばして、次の --- を探す
    let after_first = &trimmed[3..];
    let end_pos = after_first
        .find("\n---")
        .map(|p| p + 1) // \n の直後
        .or_else(|| {
            // ファイル末尾に --- がある場合
            if after_first.trim_end().ends_with("---") {
                Some(after_first.trim_end().len() - 3)
            } else {
                None
            }
        });

    match end_pos {
        Some(pos) => {
            let yaml_str = &after_first[..pos].trim();
            let body_start = pos + 3; // "---" の長さ
            let body = if body_start < after_first.len() {
                after_first[body_start..].trim_start_matches('\n').to_string()
            } else {
                String::new()
            };

            let config: WorkflowConfig = if yaml_str.is_empty() {
                WorkflowConfig::default()
            } else {
                serde_yaml::from_str(yaml_str)?
            };

            Ok(Workflow { config, body })
        }
        None => {
            // 閉じ --- がない → 全体が body
            Ok(Workflow {
                config: WorkflowConfig::default(),
                body: content.to_string(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_with_front_matter() {
        let content = "---\nmax_execute_turns: 30\nauto_execute: true\n---\n# Instructions\nUse TypeScript.";
        let wf = parse(content).unwrap();
        assert_eq!(wf.config.max_execute_turns, Some(30));
        assert_eq!(wf.config.auto_execute, Some(true));
        assert!(wf.body.contains("# Instructions"));
        assert!(wf.body.contains("Use TypeScript."));
    }

    #[test]
    fn test_parse_without_front_matter() {
        let content = "# Just a body\nNo YAML here.";
        let wf = parse(content).unwrap();
        assert!(wf.config.max_execute_turns.is_none());
        assert!(wf.body.contains("# Just a body"));
    }

    #[test]
    fn test_parse_empty_front_matter() {
        let content = "---\n---\n# Body only";
        let wf = parse(content).unwrap();
        assert!(wf.config.max_execute_turns.is_none());
        assert!(wf.body.contains("# Body only"));
    }

    #[test]
    fn test_parse_with_allowed_tools() {
        let content = "---\nallowed_tools:\n  - Bash\n  - Read\n  - Write\nci_max_retry: 5\n---\n# Rules";
        let wf = parse(content).unwrap();
        let tools = wf.config.allowed_tools.unwrap();
        assert_eq!(tools, vec!["Bash", "Read", "Write"]);
        assert_eq!(wf.config.ci_max_retry, Some(5));
    }

    #[test]
    fn test_parse_no_closing_delimiter() {
        let content = "---\nmax_execute_turns: 10\nSome body text";
        let wf = parse(content).unwrap();
        // 閉じ --- がない → 全体が body
        assert!(wf.config.max_execute_turns.is_none());
    }

    #[test]
    fn test_load_missing_file() {
        let result = load(Path::new("/nonexistent/path"));
        assert!(result.is_none());
    }
}
