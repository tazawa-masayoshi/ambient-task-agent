use anyhow::Result;
use std::path::Path;

use crate::claude::ClaudeRunner;
use crate::db::OpsMessage;
use crate::execution::RunnerContext;

const OPS_ALLOWED_TOOLS: &str = "Read,Write,Edit,Bash,Glob,Grep";
const OPS_PLAN_ALLOWED_TOOLS: &str = "Read,Glob,Grep,Bash";

const FALLBACK_OPS_SOUL: &str = "\
あなたは定型保守作業を実行する自律エージェントです。
スキルファイルの手順に従い、正確に作業を完了してください。";

const FALLBACK_OPS_PLAN_SOUL: &str = "\
あなたは依頼内容を分析し、作業計画を立てるエージェントです。
コードを読んで問題を特定し、何をどう修正すべきかを具体的に報告してください。";

const OPS_RULES: &str = "\
## ルール
- 作業手順に従って処理すること
- 不明な点があれば作業を中断し、確認が必要な内容を報告すること

## 出力（重要）
最後に必ずテキストで作業結果を出力すること。ツール操作だけで終了してはいけない。
以下の形式で要約を出力:
- 何を確認/実行したか
- 結果（成功/失敗/対応不要）
- 対応不要の場合はその理由";

const OPS_PLAN_RULES: &str = "\
## ルール
- ファイルの読み取り・検索のみ行い、書き込み・編集は一切行わないこと
- 問題の特定と原因分析を行うこと
- 具体的な修正方針（どのファイルのどの箇所をどう変更すべきか）を報告すること
- 不明な点があれば、その旨を明記すること

## 出力（重要）
最後に必ずテキストで分析結果を出力すること。ツール操作だけで終了してはいけない。
日本語で箇条書きにまとめ、以下を含めること:
- 問題の特定結果
- 原因分析
- 具体的な修正方針（対応不要の場合はその理由）";

pub struct OpsRequest {
    pub message_text: String,
    pub files: Vec<SlackFile>,
}

pub struct SlackFile {
    pub name: String,
    pub url_private_download: String,
}

/// ops プロンプトを構築（履歴 + メッセージ + 添付ファイル）
fn build_ops_prompt(req: &OpsRequest, history: &[OpsMessage], download_dir: Option<&str>) -> String {
    let mut parts = Vec::new();
    if !history.is_empty() {
        let history_text: Vec<String> = history
            .iter()
            .map(|m| format!("[{}] {}", m.role, m.content))
            .collect();
        parts.push(format!("## 前回の会話履歴\n{}", history_text.join("\n\n")));
    }
    parts.push(format!("## Slackメッセージ\n{}", req.message_text));
    if !req.files.is_empty() {
        if let Some(dir) = download_dir {
            let file_list: Vec<String> = req
                .files
                .iter()
                .map(|f| {
                    let safe = std::path::Path::new(&f.name)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("download");
                    format!("- {}/{}", dir, safe)
                })
                .collect();
            parts.push(format!(
                "## 添付ファイル（{}/ にダウンロード済み）\n{}",
                dir,
                file_list.join("\n")
            ));
        } else {
            let file_list: Vec<String> = req
                .files
                .iter()
                .map(|f| {
                    let safe = std::path::Path::new(&f.name)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("download");
                    format!("- {}", safe)
                })
                .collect();
            parts.push(format!(
                "## 添付ファイル（Slackに添付、ローカル未保存）\n{}",
                file_list.join("\n")
            ));
        }
    }
    parts.join("\n\n")
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

/// Slack イベント JSON から SlackFile を抽出
pub fn extract_slack_files_from_json(event: &serde_json::Value) -> Vec<SlackFile> {
    event
        .get("files")
        .and_then(|f| f.as_array())
        .map(|files| {
            files
                .iter()
                .filter_map(|f| {
                    let name = f.get("name")?.as_str()?.to_string();
                    let url = f.get("url_private_download")?.as_str()?.to_string();
                    Some(SlackFile {
                        name,
                        url_private_download: url,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// ops タスクを claude -p で実行
#[allow(clippy::too_many_arguments)]
pub async fn execute_ops(
    req: &OpsRequest,
    repo_path: &Path,
    skill_paths: &[String],
    soul: &str,
    max_turns: u32,
    log_dir: Option<&Path>,
    runner_ctx: &RunnerContext,
    history: &[OpsMessage],
    download_dir: Option<&str>,
    plan_only: bool,
) -> Result<String> {
    let skill_content = read_ops_skills(repo_path, skill_paths);

    // plan モードではスキルファイルが無くても動作可能
    let (base_soul, rules, tools) = if plan_only {
        let soul = if soul.is_empty() { FALLBACK_OPS_PLAN_SOUL } else { soul };
        (soul, OPS_PLAN_RULES, OPS_PLAN_ALLOWED_TOOLS)
    } else {
        if skill_content.is_empty() {
            anyhow::bail!("No skill files found for ops execution");
        }
        let soul = if soul.is_empty() { FALLBACK_OPS_SOUL } else { soul };
        (soul, OPS_RULES, OPS_ALLOWED_TOOLS)
    };

    // システムプロンプト構築
    let system_prompt = if skill_content.is_empty() {
        format!("{}\n\n{}", base_soul, rules)
    } else {
        format!("{}\n\n## 作業手順\n{}\n\n{}", base_soul, skill_content, rules)
    };

    let prompt = build_ops_prompt(req, history, download_dir);

    let result = ClaudeRunner::new("ops", &prompt)
        .system_prompt(&system_prompt)
        .max_turns(max_turns)
        .allowed_tools(tools)
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

