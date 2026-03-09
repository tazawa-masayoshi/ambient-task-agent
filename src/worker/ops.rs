use anyhow::{Context, Result};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::claude::{ClaudeRunner, ExtraToolDispatcher, ToolMeta};
use crate::db::OpsMessage;
use crate::execution::RunnerContext;
use crate::repo_config::OpsToolDef;

const OPS_ALLOWED_TOOLS: &str = "Read,Write,Edit,Bash,Glob,Grep";

const FALLBACK_OPS_SOUL: &str = "\
あなたは定型保守作業を実行する自律エージェントです。
スキルファイルの手順に従い、正確に作業を完了してください。";

const OPS_RULES: &str = "\
## ルール
- 作業手順に従って処理すること
- 完了時に作業内容の要約を出力すること
- 不明な点があれば作業を中断し、確認が必要な内容を報告すること";

const TOOL_OPS_SOUL: &str = "\
あなたは ops チャンネルのアシスタントです。
ユーザーのメッセージを理解し、適切なツールを呼び出してください。

## ルール
- ユーザーの意図を正確に把握すること
- 適切なツールを選んで実行すること
- ツールの結果をユーザーにわかりやすく日本語で要約すること
- 不明な点があれば確認を求めること
- 1つのメッセージに複数の指示がある場合、対応するツールを順に呼ぶこと";

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
                .map(|f| format!("- {}/{}", dir, f.name))
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
                .map(|f| format!("- {}", f.name))
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

    let prompt = build_ops_prompt(req, history, download_dir);

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

// ============================================================================
// Tool ベース ops — LLM はパラメータ抽出のみ、実行は定型コマンド
// ============================================================================

/// OpsToolDef → Bedrock ToolMeta に変換
fn ops_tool_to_meta(tool: &OpsToolDef) -> ToolMeta {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();

    for (name, param) in &tool.params {
        properties.insert(
            name.clone(),
            serde_json::json!({
                "type": param.param_type,
                "description": param.description,
            }),
        );
        if param.required {
            required.push(serde_json::Value::String(name.clone()));
        }
    }

    ToolMeta {
        name: tool.name.clone(),
        description: tool.description.clone(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": required,
        }),
    }
}

/// ops tool を PARAM_xxx 環境変数付きで実行するディスパッチャ
struct OpsToolDispatcher {
    tools: Vec<OpsToolDef>,
    repo_path: PathBuf,
}

#[async_trait]
impl ExtraToolDispatcher for OpsToolDispatcher {
    async fn dispatch(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        _cwd: &Path,
    ) -> Option<(String, bool)> {
        let tool_def = self.tools.iter().find(|t| t.name == tool_name)?;

        match execute_ops_tool(tool_def, input, &self.repo_path).await {
            Ok(output) => Some((output, true)),
            Err(e) => Some((format!("Error: {}", e), false)),
        }
    }
}

/// 単一の ops tool をコマンド実行
async fn execute_ops_tool(
    tool: &OpsToolDef,
    params: &serde_json::Value,
    repo_path: &Path,
) -> Result<String> {
    let command_path = if Path::new(&tool.command).is_absolute() {
        PathBuf::from(&tool.command)
    } else {
        repo_path.join(&tool.command)
    };

    // コマンドファイルの存在確認
    if !command_path.exists() {
        anyhow::bail!("Tool command not found: {}", command_path.display());
    }

    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg(command_path.to_str().unwrap_or(&tool.command));
    cmd.current_dir(repo_path);

    // パラメータを PARAM_xxx 環境変数として注入
    if let Some(obj) = params.as_object() {
        for (key, value) in obj {
            let env_key = format!("PARAM_{}", key.to_uppercase());
            let env_val = match value {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            cmd.env(&env_key, &env_val);
        }
    }

    tracing::info!(
        "ops tool [{}]: command={}, params={}",
        tool.name,
        tool.command,
        crate::claude::truncate_str(
            &serde_json::to_string(params).unwrap_or_default(),
            200
        )
    );

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(tool.timeout_secs),
        cmd.output(),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "Tool '{}' timed out after {}s",
            tool.name,
            tool.timeout_secs
        )
    })?
    .context("Failed to execute tool command")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if output.status.success() {
        Ok(if stdout.is_empty() {
            "(completed, no output)".to_string()
        } else {
            stdout.to_string()
        })
    } else {
        anyhow::bail!(
            "Exit {}\nstdout:\n{}\nstderr:\n{}",
            output.status.code().unwrap_or(-1),
            stdout,
            stderr
        )
    }
}

/// tool ベースで ops を実行 (ops_tools が設定されている場合)
pub async fn execute_ops_with_tools(
    req: &OpsRequest,
    repo_path: &Path,
    tools: &[OpsToolDef],
    soul: &str,
    log_dir: Option<&Path>,
    runner_ctx: &RunnerContext,
    history: &[OpsMessage],
    download_dir: Option<&str>,
) -> Result<String> {
    if tools.is_empty() {
        anyhow::bail!("No ops_tools defined");
    }

    // system_prompt: NLU + routing 特化 (skill.md は含めない)
    let base_soul = if soul.is_empty() { TOOL_OPS_SOUL } else { soul };
    let system_prompt = base_soul.to_string();

    let prompt = build_ops_prompt(req, history, download_dir);

    // OpsToolDef → ToolMeta に変換
    let tool_metas: Vec<ToolMeta> = tools.iter().map(ops_tool_to_meta).collect();

    // ディスパッチャ
    let dispatcher: Arc<dyn ExtraToolDispatcher> = Arc::new(OpsToolDispatcher {
        tools: tools.to_vec(),
        repo_path: repo_path.to_path_buf(),
    });

    // tool ベースでは max_turns を抑える (1 tool call + 要約 = 2-3 ターン)
    let max_turns = 3;

    let result = ClaudeRunner::new("ops", &prompt)
        .system_prompt(&system_prompt)
        .max_turns(max_turns)
        .allowed_tools("") // built-in tools は不要（空にすると build_tools で空リストになる）
        .cwd(repo_path)
        .optional_log_dir(log_dir)
        .extra_tools(tool_metas, dispatcher)
        .with_context(runner_ctx)
        .run()
        .await?;

    if !result.success {
        anyhow::bail!("ops tool execution failed: {}", result.error_output());
    }

    Ok(result.stdout.trim().to_string())
}
