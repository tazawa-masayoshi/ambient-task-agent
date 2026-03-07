use anyhow::Result;
use std::path::Path;

use crate::claude::ClaudeRunner;
use crate::db::OpsMessage;
use crate::execution::RunnerContext;

const OPS_ALLOWED_TOOLS: &str = "Read,Write,Edit,Bash,Glob,Grep";

const FALLBACK_OPS_SOUL: &str = "\
あなたは定型保守作業を実行する自律エージェントです。
スキルファイルの手順に従い、正確に作業を完了してください。";

const OPS_RULES: &str = "\
## ルール
- 作業手順に従って処理すること
- 完了時に作業内容の要約を出力すること
- 不明な点があれば作業を中断し、確認が必要な内容を報告すること";

pub struct OpsRequest {
    pub message_text: String,
    pub files: Vec<SlackFile>,
}

pub struct SlackFile {
    pub name: String,
    pub url_private_download: String,
}

/// スキルファイルを読み込んで結合
fn read_ops_skills(repo_path: &Path, skill_paths: &[String]) -> String {
    skill_paths
        .iter()
        .filter_map(|p| {
            let full = repo_path.join(p);
            match std::fs::read_to_string(&full) {
                Ok(content) => Some(content),
                Err(e) => {
                    tracing::warn!("Failed to read skill file {}: {}", full.display(), e);
                    None
                }
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n---\n\n")
}

/// ops タスクを claude -p で実行
pub async fn execute_ops(
    req: &OpsRequest,
    repo_path: &Path,
    skill_paths: &[String],
    soul: &str,
    max_turns: u32,
    log_dir: Option<&Path>,
    runner_ctx: &RunnerContext,
    history: &[OpsMessage],
) -> Result<String> {
    let skill_content = read_ops_skills(repo_path, skill_paths);
    if skill_content.is_empty() {
        anyhow::bail!("No skill files found for ops execution");
    }

    // システムプロンプト構築
    let base_soul = if soul.is_empty() { FALLBACK_OPS_SOUL } else { soul };
    let system_prompt = format!(
        "{}\n\n## 作業手順\n{}\n\n{}",
        base_soul, skill_content, OPS_RULES
    );

    // プロンプト構築
    let mut prompt_parts = Vec::new();

    // 会話履歴があれば先頭に含める
    if !history.is_empty() {
        let history_text: Vec<String> = history
            .iter()
            .map(|m| format!("[{}] {}", m.role, m.content))
            .collect();
        prompt_parts.push(format!(
            "## 前回の会話履歴\n{}",
            history_text.join("\n\n")
        ));
    }

    prompt_parts.push(format!("## Slackメッセージ\n{}", req.message_text));
    if !req.files.is_empty() {
        let file_list: Vec<String> = req
            .files
            .iter()
            .map(|f| format!("- images/{}", f.name))
            .collect();
        prompt_parts.push(format!(
            "## 添付ファイル（images/ にダウンロード済み）\n{}",
            file_list.join("\n")
        ));
    }
    let prompt = prompt_parts.join("\n\n");

    let result = ClaudeRunner::new("ops", &prompt)
        .system_prompt(&system_prompt)
        .max_turns(max_turns)
        .allowed_tools(OPS_ALLOWED_TOOLS)
        .cwd(repo_path)
        .optional_log_dir(log_dir)
        .with_context(runner_ctx)
        .run()
        .await?;

    if !result.success {
        anyhow::bail!("claude -p ops failed: {}", result.error_output());
    }

    Ok(result.stdout.trim().to_string())
}
