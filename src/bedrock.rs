use anyhow::{Context, Result};
use async_trait::async_trait;
use aws_sdk_bedrockruntime::types::{
    ContentBlock, ConversationRole, InferenceConfiguration, Message, StopReason,
    SystemContentBlock, Tool, ToolConfiguration, ToolInputSchema, ToolResultBlock,
    ToolResultContentBlock, ToolResultStatus, ToolSpecification,
};
use aws_sdk_bedrockruntime::Client;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::claude::{AgentBackend, AgentOutput, AgentRequest};

const BASH_TIMEOUT_SECS: u64 = 60;
const MAX_READ_BYTES: usize = 512_000;

// ============================================================================
// BedrockBackend
// ============================================================================

pub struct BedrockBackend {
    client: Client,
    model_id: String,
}

impl BedrockBackend {
    pub async fn new(model_id: impl Into<String>, region: Option<&str>) -> Result<Self> {
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
        if let Some(r) = region {
            loader = loader.region(aws_config::Region::new(r.to_string()));
        }
        let config = loader.load().await;
        let client = Client::new(&config);

        Ok(Self {
            client,
            model_id: model_id.into(),
        })
    }
}

#[async_trait]
impl AgentBackend for BedrockBackend {
    async fn execute(&self, request: AgentRequest) -> Result<AgentOutput> {
        let start = std::time::Instant::now();
        let max_turns = request.max_turns as usize;
        let cwd = request.cwd.clone().unwrap_or_else(|| PathBuf::from("/tmp"));

        // allowed_tools をパース
        let allowed: Vec<&str> = request
            .allowed_tools
            .as_deref()
            .unwrap_or("Read,Bash")
            .split(',')
            .map(|s| s.trim())
            .collect();

        let mut tools = build_tools(&allowed)?;

        // extra_tool_defs があれば Bedrock ToolSpecification に変換して追加
        for meta in &request.extra_tool_defs {
            let schema_doc = json_to_document(meta.input_schema.clone());
            tools.push(Tool::ToolSpec(
                ToolSpecification::builder()
                    .name(&meta.name)
                    .description(&meta.description)
                    .input_schema(ToolInputSchema::Json(schema_doc))
                    .build()
                    .map_err(|e| anyhow::anyhow!("ExtraTool '{}': {}", meta.name, e))?,
            ));
        }

        let tool_config = ToolConfiguration::builder()
            .set_tools(Some(tools))
            .build()
            .map_err(|e| anyhow::anyhow!("ToolConfiguration build: {}", e))?;

        let system = request
            .system_prompt
            .as_ref()
            .map(|sp| vec![SystemContentBlock::Text(sp.clone())]);

        let mut messages = vec![Message::builder()
            .role(ConversationRole::User)
            .content(ContentBlock::Text(request.prompt.clone()))
            .build()
            .map_err(|e| anyhow::anyhow!("Message build: {}", e))?];

        let mut final_text = String::new();
        let mut turns = 0;

        loop {
            // タイムアウト判定
            if let Some(timeout) = request.timeout_secs {
                if start.elapsed().as_secs() >= timeout {
                    return Ok(AgentOutput {
                        success: false,
                        stdout: final_text,
                        stderr: format!("Bedrock timed out after {}s", timeout),
                        duration: start.elapsed(),
                        truncated: false,
                        usage: None,
                        cost_usd: None,
                        session_id: None,
                    });
                }
            }

            // Converse API 呼び出し
            let mut req_builder = self
                .client
                .converse()
                .model_id(&self.model_id)
                .set_messages(Some(messages.clone()))
                .tool_config(tool_config.clone())
                .inference_config(
                    InferenceConfiguration::builder()
                        .max_tokens(4096)
                        .build(),
                );

            if let Some(ref sys) = system {
                req_builder = req_builder.set_system(Some(sys.clone()));
            }

            let response = req_builder
                .send()
                .await
                .context("Bedrock Converse API failed")?;

            let stop_reason = response.stop_reason().clone();

            // アシスタントメッセージ抽出
            let assistant_msg = match response.output() {
                Some(aws_sdk_bedrockruntime::types::ConverseOutput::Message(msg)) => msg.clone(),
                _ => {
                    return Ok(AgentOutput {
                        success: false,
                        stdout: final_text,
                        stderr: "Unexpected Bedrock response".to_string(),
                        duration: start.elapsed(),
                        truncated: false,
                        usage: None,
                        cost_usd: None,
                        session_id: None,
                    });
                }
            };

            messages.push(assistant_msg.clone());

            match stop_reason {
                StopReason::ToolUse => {
                    let mut tool_results = Vec::new();

                    for block in assistant_msg.content() {
                        match block {
                            ContentBlock::ToolUse(tool_use) => {
                                let input = document_to_json(tool_use.input());

                                tracing::info!(
                                    "Bedrock tool [{}] input: {}",
                                    tool_use.name(),
                                    crate::claude::truncate_str(
                                        &serde_json::to_string(&input).unwrap_or_default(),
                                        200
                                    )
                                );

                                let (result_text, status) = {
                                    let (text, st) =
                                        execute_tool(tool_use.name(), &input, &cwd).await;
                                    // built-in tool で "Unknown tool" なら dispatcher にフォールバック
                                    if st == ToolResultStatus::Error
                                        && text.contains("Unknown tool")
                                    {
                                        if let Some(ref dispatcher) = request.tool_dispatcher {
                                            if let Some((out, ok)) = dispatcher
                                                .dispatch(tool_use.name(), &input, &cwd)
                                                .await
                                            {
                                                let s = if ok {
                                                    ToolResultStatus::Success
                                                } else {
                                                    ToolResultStatus::Error
                                                };
                                                (out, s)
                                            } else {
                                                (text, st)
                                            }
                                        } else {
                                            (text, st)
                                        }
                                    } else {
                                        (text, st)
                                    }
                                };

                                tracing::debug!(
                                    "Bedrock tool [{}] result: {} chars, {:?}",
                                    tool_use.name(),
                                    result_text.len(),
                                    status
                                );

                                tool_results.push(ContentBlock::ToolResult(
                                    ToolResultBlock::builder()
                                        .tool_use_id(tool_use.tool_use_id())
                                        .content(ToolResultContentBlock::Text(result_text))
                                        .status(status)
                                        .build()
                                        .map_err(|e| {
                                            anyhow::anyhow!("ToolResultBlock build: {}", e)
                                        })?,
                                ));
                            }
                            ContentBlock::Text(text) => {
                                tracing::debug!(
                                    "Bedrock thinking: {}",
                                    crate::claude::truncate_str(text, 100)
                                );
                            }
                            _ => {}
                        }
                    }

                    messages.push(
                        Message::builder()
                            .role(ConversationRole::User)
                            .set_content(Some(tool_results))
                            .build()
                            .map_err(|e| anyhow::anyhow!("Tool result message build: {}", e))?,
                    );
                }
                StopReason::EndTurn => {
                    for block in assistant_msg.content() {
                        if let ContentBlock::Text(text) = block {
                            if !final_text.is_empty() {
                                final_text.push('\n');
                            }
                            final_text.push_str(text);
                        }
                    }
                    break;
                }
                StopReason::MaxTokens => {
                    for block in assistant_msg.content() {
                        if let ContentBlock::Text(text) = block {
                            final_text.push_str(text);
                        }
                    }
                    tracing::warn!("Bedrock: max_tokens reached");
                    break;
                }
                _ => {
                    tracing::warn!("Bedrock: unexpected stop_reason: {:?}", stop_reason);
                    break;
                }
            }

            turns += 1;
            if turns >= max_turns {
                tracing::info!("Bedrock: max_turns ({}) reached", max_turns);
                break;
            }
        }

        // 出力切り詰め（UTF-8 境界を考慮した truncate_str を使用）
        let truncated = if let Some(max_bytes) = request.max_output_bytes {
            if final_text.len() > max_bytes {
                let total = final_text.len();
                let safe_end = crate::claude::truncate_str(&final_text, max_bytes).len();
                final_text.truncate(safe_end);
                final_text.push_str(&format!("\n[truncated, {} total bytes]", total));
                true
            } else {
                false
            }
        } else {
            false
        };

        Ok(AgentOutput {
            success: true,
            stdout: final_text,
            stderr: String::new(),
            duration: start.elapsed(),
            truncated,
            usage: None,
            cost_usd: None,
            session_id: None,
        })
    }
}

// ============================================================================
// Tool definitions
// ============================================================================

fn build_tools(allowed: &[&str]) -> Result<Vec<Tool>> {
    let all_tools: Vec<(&str, &str, serde_json::Value)> = vec![
        (
            "Read",
            "ファイルを読み取る。絶対パスを指定すること。",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "読み取るファイルの絶対パス"
                    }
                },
                "required": ["file_path"]
            }),
        ),
        (
            "Write",
            "ファイルに書き込む。親ディレクトリは自動作成される。",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "書き込むファイルの絶対パス"
                    },
                    "content": {
                        "type": "string",
                        "description": "書き込む内容"
                    }
                },
                "required": ["file_path", "content"]
            }),
        ),
        (
            "Edit",
            "ファイル内の文字列を置換する。old_stringはファイル内で一意であること。",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "編集するファイルの絶対パス"
                    },
                    "old_string": {
                        "type": "string",
                        "description": "置換対象の文字列（ユニーク）"
                    },
                    "new_string": {
                        "type": "string",
                        "description": "置換後の文字列"
                    }
                },
                "required": ["file_path", "old_string", "new_string"]
            }),
        ),
        (
            "Bash",
            "bashコマンドを実行する。",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "実行するbashコマンド"
                    }
                },
                "required": ["command"]
            }),
        ),
        (
            "Glob",
            "globパターンでファイルを検索する。",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "ファイル名パターン (例: '*.rs', '*.toml')"
                    },
                    "path": {
                        "type": "string",
                        "description": "検索ディレクトリ（省略時はcwd）"
                    }
                },
                "required": ["pattern"]
            }),
        ),
        (
            "Grep",
            "正規表現でファイル内容を検索する。",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "検索する正規表現パターン"
                    },
                    "path": {
                        "type": "string",
                        "description": "検索パス（省略時はcwd）"
                    },
                    "include": {
                        "type": "string",
                        "description": "ファイルフィルタ (例: '*.rs')"
                    }
                },
                "required": ["pattern"]
            }),
        ),
    ];

    all_tools
        .into_iter()
        .filter(|(name, _, _)| allowed.contains(name))
        .map(|(name, desc, schema)| {
            let schema_doc = json_to_document(schema);
            Ok(Tool::ToolSpec(
                ToolSpecification::builder()
                    .name(name)
                    .description(desc)
                    .input_schema(ToolInputSchema::Json(schema_doc))
                    .build()
                    .map_err(|e| anyhow::anyhow!("ToolSpec '{}': {}", name, e))?,
            ))
        })
        .collect()
}

// ============================================================================
// Tool execution
// ============================================================================

async fn execute_tool(
    name: &str,
    input: &serde_json::Value,
    cwd: &Path,
) -> (String, ToolResultStatus) {
    match execute_tool_inner(name, input, cwd).await {
        Ok(output) => (output, ToolResultStatus::Success),
        Err(e) => (format!("Error: {}", e), ToolResultStatus::Error),
    }
}

async fn execute_tool_inner(
    name: &str,
    input: &serde_json::Value,
    cwd: &Path,
) -> Result<String> {
    match name {
        "Read" => {
            let path = input["file_path"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("file_path is required"))?;
            let content = tokio::fs::read_to_string(path)
                .await
                .with_context(|| format!("Failed to read: {}", path))?;
            if content.len() > MAX_READ_BYTES {
                Ok(format!(
                    "{}\n[truncated at {} bytes, total {} bytes]",
                    &content[..MAX_READ_BYTES],
                    MAX_READ_BYTES,
                    content.len()
                ))
            } else {
                Ok(content)
            }
        }
        "Write" => {
            let path = input["file_path"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("file_path is required"))?;
            let content = input["content"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("content is required"))?;
            if let Some(parent) = Path::new(path).parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(path, content).await?;
            Ok(format!("Written {} bytes to {}", content.len(), path))
        }
        "Edit" => {
            let path = input["file_path"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("file_path is required"))?;
            let old = input["old_string"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("old_string is required"))?;
            let new = input["new_string"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("new_string is required"))?;
            let content = tokio::fs::read_to_string(path).await?;
            let count = content.matches(old).count();
            if count == 0 {
                anyhow::bail!("old_string not found in {}", path);
            }
            if count > 1 {
                anyhow::bail!("old_string found {} times (must be unique)", count);
            }
            let updated = content.replacen(old, new, 1);
            tokio::fs::write(path, &updated).await?;
            Ok(format!("Edit applied to {}", path))
        }
        "Bash" => {
            let command = input["command"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("command is required"))?;
            let output = tokio::time::timeout(
                std::time::Duration::from_secs(BASH_TIMEOUT_SECS),
                tokio::process::Command::new("bash")
                    .arg("-c")
                    .arg(command)
                    .current_dir(cwd)
                    .output(),
            )
            .await
            .map_err(|_| anyhow::anyhow!("Command timed out after {}s", BASH_TIMEOUT_SECS))?
            .context("Failed to execute command")?;

            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            if output.status.success() {
                Ok(if stdout.is_empty() {
                    "(no output)".to_string()
                } else {
                    stdout.to_string()
                })
            } else {
                Ok(format!(
                    "Exit {}\nstdout:\n{}\nstderr:\n{}",
                    output.status.code().unwrap_or(-1),
                    stdout,
                    stderr
                ))
            }
        }
        "Glob" => {
            let pattern = input["pattern"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("pattern is required"))?;
            let search_path = input["path"]
                .as_str()
                .unwrap_or(cwd.to_str().unwrap_or("."));
            // パターンからファイル名部分を抽出 (e.g. "**/*.rs" → "*.rs")
            let name_part = pattern.rsplit('/').next().unwrap_or(pattern);
            let cmd = format!(
                "find {} -name {} -type f 2>/dev/null | sort | head -100",
                shell_escape(search_path),
                shell_escape(name_part)
            );
            let output = tokio::process::Command::new("bash")
                .arg("-c")
                .arg(&cmd)
                .current_dir(cwd)
                .output()
                .await?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            Ok(if stdout.is_empty() {
                "No files matched".to_string()
            } else {
                stdout.to_string()
            })
        }
        "Grep" => {
            let pattern = input["pattern"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("pattern is required"))?;
            let search_path = input["path"].as_str().unwrap_or(".");
            let include = input["include"].as_str();
            let mut cmd = format!(
                "grep -rn {} {}",
                shell_escape(pattern),
                shell_escape(search_path)
            );
            if let Some(inc) = include {
                cmd = format!("{} --include={}", cmd, shell_escape(inc));
            }
            cmd.push_str(" 2>/dev/null | head -100");
            let output = tokio::process::Command::new("bash")
                .arg("-c")
                .arg(&cmd)
                .current_dir(cwd)
                .output()
                .await?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            Ok(if stdout.is_empty() {
                "No matches found".to_string()
            } else {
                stdout.to_string()
            })
        }
        _ => anyhow::bail!("Unknown tool: {}", name),
    }
}

fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ============================================================================
// Document <-> serde_json 変換
// ============================================================================

fn json_to_document(value: serde_json::Value) -> aws_smithy_types::Document {
    match value {
        serde_json::Value::Null => aws_smithy_types::Document::Null,
        serde_json::Value::Bool(b) => aws_smithy_types::Document::Bool(b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                if i >= 0 {
                    aws_smithy_types::Document::Number(aws_smithy_types::Number::PosInt(i as u64))
                } else {
                    aws_smithy_types::Document::Number(aws_smithy_types::Number::NegInt(i))
                }
            } else if let Some(f) = n.as_f64() {
                aws_smithy_types::Document::Number(aws_smithy_types::Number::Float(f))
            } else {
                aws_smithy_types::Document::Null
            }
        }
        serde_json::Value::String(s) => aws_smithy_types::Document::String(s),
        serde_json::Value::Array(arr) => {
            aws_smithy_types::Document::Array(arr.into_iter().map(json_to_document).collect())
        }
        serde_json::Value::Object(map) => {
            let hm: HashMap<String, aws_smithy_types::Document> = map
                .into_iter()
                .map(|(k, v)| (k, json_to_document(v)))
                .collect();
            aws_smithy_types::Document::Object(hm)
        }
    }
}

fn document_to_json(doc: &aws_smithy_types::Document) -> serde_json::Value {
    match doc {
        aws_smithy_types::Document::Object(map) => {
            let obj: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), document_to_json(v)))
                .collect();
            serde_json::Value::Object(obj)
        }
        aws_smithy_types::Document::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(document_to_json).collect())
        }
        aws_smithy_types::Document::Number(n) => match n {
            aws_smithy_types::Number::PosInt(i) => serde_json::json!(*i),
            aws_smithy_types::Number::NegInt(i) => serde_json::json!(*i),
            aws_smithy_types::Number::Float(f) => serde_json::json!(*f),
        },
        aws_smithy_types::Document::String(s) => serde_json::Value::String(s.clone()),
        aws_smithy_types::Document::Bool(b) => serde_json::Value::Bool(*b),
        aws_smithy_types::Document::Null => serde_json::Value::Null,
    }
}
