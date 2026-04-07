//! agent-harness crate のアダプター
//!
//! harness の LlmClient / ToolExecutor を ambient-task-agent 用に実装する。

use std::path::Path;
use std::sync::Arc;

use agent_harness::bash_validation::{validate_command, ValidationResult};
use agent_harness::{HookDecision, PermissionMode, SharedToolHook};
use anyhow::Result;
use async_trait::async_trait;

use super::mcp::{parse_mcp_tool_name, McpManager};
use super::tool_impls;
use super::types::*;

/// MCP の bash 系ツール (`*__bash_command` / `*__execute_command`) の判定。
/// builtin `Bash` は含まない。
fn is_mcp_bash_tool(name: &str) -> bool {
    name.ends_with("__bash_command") || name.ends_with("__execute_command")
}

/// Bash 系ツール全般 (builtin Bash + MCP bash 系) の判定。
/// hook / bash_validation で共通利用。
pub(crate) fn is_bash_tool(name: &str) -> bool {
    name == "Bash" || is_mcp_bash_tool(name)
}

// ============================================================================
// ToolExecutor impl — builtin + MCP + SubAgent
// ============================================================================

pub struct AmbientToolExecutor {
    pub mcp_manager: Option<Arc<McpManager>>,
    pub timeout_secs: u64,
    /// Bash command validation mode. ReadOnly is used for classify/conversing
    /// phases; DangerFullAccess for full ops execution.
    pub permission_mode: PermissionMode,
    /// Optional pre-tool-use hook. Runs before bash_validation and MCP safeguard.
    pub hook: Option<SharedToolHook>,
}

#[async_trait]
impl agent_harness::ToolExecutor for AmbientToolExecutor {
    async fn execute(
        &self,
        name: &str,
        input: &serde_json::Value,
        cwd: &Path,
    ) -> agent_harness::ToolOutput {
        // PreToolUse hook: 最も外側で実行。Deny されたら他の検証はスキップ。
        if let Some(ref hook) = self.hook {
            if let HookDecision::Deny { reason } = hook.pre_tool_use(name, input, cwd) {
                tracing::info!("Tool '{}' blocked by PreToolUse hook: {}", name, reason);
                return agent_harness::ToolOutput::err(format!("Blocked by hook: {}", reason));
            }
        }

        // MCP safeguard: bash 系 MCP ツールに safeguard 適用
        if let Some(reason) = check_mcp_safeguard(name, input) {
            return agent_harness::ToolOutput::err(format!("Blocked by safeguard: {}", reason));
        }

        // bash_validation: builtin Bash と MCP bash 系の両方に対する事前検証
        if let Some(reason) = self.check_bash_validation(name, input, cwd) {
            return agent_harness::ToolOutput::err(reason);
        }

        // MCP ツール
        if parse_mcp_tool_name(name).is_some() {
            return self.execute_mcp(name, input).await;
        }

        // builtin ツール
        let ctx = tool_impls::ToolExecutionContext {
            cwd: cwd.to_path_buf(),
            timeout_secs: self.timeout_secs,
        };
        let result = tool_impls::execute_tool(name, input, &ctx).await;
        agent_harness::ToolOutput {
            output: result.output,
            is_error: result.is_error,
        }
    }

    fn is_read_only(&self, name: &str) -> bool {
        matches!(name, "Read" | "Glob" | "Grep")
            || name.starts_with("mcp__")
                && (name.ends_with("__find_symbol")
                    || name.ends_with("__find_referencing_symbols")
                    || name.ends_with("__get_symbols_overview")
                    || name.ends_with("__list_dir")
                    || name.ends_with("__find_file")
                    || name.ends_with("__search_for_pattern")
                    || name.ends_with("__read_memory")
                    || name.ends_with("__list_memories"))
    }
}

impl AmbientToolExecutor {
    /// Run bash_validation against the command if this tool is a bash invocation.
    /// Returns Some(error message) to block, None to allow.
    fn check_bash_validation(
        &self,
        name: &str,
        input: &serde_json::Value,
        cwd: &Path,
    ) -> Option<String> {
        if !is_bash_tool(name) {
            return None;
        }
        let command = input.get("command").and_then(|v| v.as_str())?;
        match validate_command(command, self.permission_mode, cwd) {
            ValidationResult::Allow => None,
            ValidationResult::Block { reason } => Some(format!("Bash blocked: {reason}")),
            ValidationResult::Warn { message } => {
                tracing::warn!(
                    "bash_validation warning (mode={}): {}",
                    self.permission_mode.as_str(),
                    message
                );
                None
            }
        }
    }

    async fn execute_mcp(
        &self,
        name: &str,
        input: &serde_json::Value,
    ) -> agent_harness::ToolOutput {
        match &self.mcp_manager {
            Some(mgr) => match mgr.call_tool(name, input).await {
                Ok((output, is_error)) => {
                    if is_error {
                        agent_harness::ToolOutput::err(output)
                    } else {
                        agent_harness::ToolOutput::ok(output)
                    }
                }
                Err(e) => {
                    agent_harness::ToolOutput::err(format!("MCP tool '{}' failed: {}", name, e))
                }
            },
            None => agent_harness::ToolOutput::err(format!(
                "MCP tool '{}' called but no MCP manager available",
                name
            )),
        }
    }
}

// ============================================================================
// LlmClient impl — Bedrock Converse API 経由
// ============================================================================
//
// 注: Anthropic API (OAuth/API Key) 用の LlmClient は claude_auth::AnthropicLlmClient
// に統合済み。共通実装は claude-auth crate を参照。
//
// 既知の差異 (要追跡): claude_auth::AnthropicLlmClient は送信時に
// "You are Claude Code, Anthropic's official CLI for Claude." の identity block を
// 自動 prepend する。BedrockLlmClient はこれを行わないため、同じ system_prompt を
// 渡しても 2 backend で実効プロンプトが異なる。Bedrock パスを本格運用する際は
// identity block の扱いを統一すべき (TODO: backend.rs の system_prompt 組み立て層に
// 移動する案あり)。

pub struct BedrockLlmClient {
    pub client: Arc<super::bedrock_client::BedrockClient>,
}

#[async_trait]
impl agent_harness::LlmClient for BedrockLlmClient {
    async fn send(
        &self,
        _model: &str,
        max_tokens: u32,
        system_prompt: Option<&str>,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
    ) -> Result<agent_harness::LlmResponse> {
        use super::bedrock_convert;
        use super::types::{MessagesRequest, ToolChoice};

        // harness 型 → Bedrock 型に変換して呼び出し
        let request = MessagesRequest {
            model: String::new(), // Bedrock は model_id を内部で持つ
            max_tokens,
            system: system_prompt.map(|sp| {
                vec![super::types::SystemBlock {
                    block_type: "text".to_string(),
                    text: sp.to_string(),
                    cache_control: None,
                }]
            }),
            messages: messages.to_vec(),
            tools: tools.map(|t| t.to_vec()),
            tool_choice: tools.map(|_| ToolChoice::Auto),
            stream: false,
        };

        // Bedrock Converse API 呼び出し
        let br_messages = bedrock_convert::convert_messages(&request.messages);
        let br_system = request.system.as_ref().map(|s| bedrock_convert::convert_system_blocks(s));
        let br_tool_config = bedrock_convert::convert_tools(
            request.tools.as_deref().unwrap_or(&[]),
            &request.tool_choice,
        );
        let inference_config = aws_sdk_bedrockruntime::types::InferenceConfiguration::builder()
            .max_tokens(max_tokens as i32)
            .build();

        let mut builder = self.client.raw_client()
            .converse()
            .model_id(self.client.model_id())
            .set_messages(Some(br_messages))
            .inference_config(inference_config);

        if let Some(system) = br_system {
            for block in system {
                builder = builder.system(block);
            }
        }
        if let Some(tool_config) = br_tool_config {
            builder = builder.tool_config(tool_config);
        }

        let output = builder.send().await
            .map_err(|e| anyhow::anyhow!("Bedrock Converse failed: {}", e))?;

        // Bedrock レスポンス → harness LlmResponse
        let br_output = output.output()
            .ok_or_else(|| anyhow::anyhow!("Bedrock: no output"))?;

        let br_message = match br_output {
            aws_sdk_bedrockruntime::types::ConverseOutput::Message(m) => m,
            _ => anyhow::bail!("Bedrock: unexpected output type"),
        };

        let content: Vec<ContentBlock> = br_message.content().iter().map(|block| {
            match block {
                aws_sdk_bedrockruntime::types::ContentBlock::Text(text) => {
                    ContentBlock::Text { text: text.clone() }
                }
                aws_sdk_bedrockruntime::types::ContentBlock::ToolUse(tool) => {
                    let input = bedrock_convert::document_to_serde_value(tool.input().clone());
                    ContentBlock::ToolUse {
                        id: tool.tool_use_id().to_string(),
                        name: tool.name().to_string(),
                        input,
                    }
                }
                _ => ContentBlock::Text { text: "[unsupported block]".to_string() },
            }
        }).collect();

        let stop_reason = Some(bedrock_convert::map_stop_reason(output.stop_reason()));

        let usage = if let Some(u) = output.usage() {
            Usage {
                input_tokens: u.input_tokens() as u64,
                output_tokens: u.output_tokens() as u64,
                cache_creation_input_tokens: u.cache_write_input_tokens().map(|v| v as u64).unwrap_or(0),
                cache_read_input_tokens: u.cache_read_input_tokens().map(|v| v as u64).unwrap_or(0),
            }
        } else {
            Usage::default()
        };

        Ok(agent_harness::LlmResponse { content, stop_reason, usage })
    }
}

// ============================================================================
// MCP safeguard
// ============================================================================

/// MCP bash 系ツール (builtin Bash は対象外: tool_impls 内で別途チェック) に対する
/// dangerous-command pattern safeguard。
///
/// 意図的に builtin Bash は除外: builtin は execute_bash 内で同じ check_dangerous_command
/// を呼ぶため、ここで2重実行する必要がない。
fn check_mcp_safeguard(name: &str, input: &serde_json::Value) -> Option<String> {
    if is_mcp_bash_tool(name) {
        if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
            return tool_impls::check_dangerous_command(cmd).map(|r| r.to_string());
        }
    }
    None
}
