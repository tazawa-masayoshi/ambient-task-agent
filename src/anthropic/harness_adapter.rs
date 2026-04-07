//! agent-harness crate のアダプター
//!
//! harness の LlmClient / ToolExecutor を ambient-task-agent 用に実装する。

#![allow(dead_code)] // backend.rs から呼ぶ予定

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use super::mcp::{parse_mcp_tool_name, McpManager};
use super::tool_impls;
use super::types::*;

// ============================================================================
// ToolExecutor impl — builtin + MCP + SubAgent
// ============================================================================

pub struct AmbientToolExecutor {
    pub mcp_manager: Option<Arc<McpManager>>,
    pub timeout_secs: u64,
}

#[async_trait]
impl agent_harness::ToolExecutor for AmbientToolExecutor {
    async fn execute(
        &self,
        name: &str,
        input: &serde_json::Value,
        cwd: &Path,
    ) -> agent_harness::ToolOutput {
        // MCP safeguard: bash 系 MCP ツールに safeguard 適用
        if let Some(reason) = check_mcp_safeguard(name, input) {
            return agent_harness::ToolOutput::err(format!("Blocked by safeguard: {}", reason));
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
// LlmClient impl — claude-auth 経由
// ============================================================================

pub struct AmbientLlmClient {
    pub inner: super::client::AnthropicClient,
}

#[async_trait]
impl agent_harness::LlmClient for AmbientLlmClient {
    async fn send(
        &self,
        model: &str,
        max_tokens: u32,
        system_prompt: Option<&str>,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
    ) -> Result<agent_harness::LlmResponse> {
        // harness 型 → Anthropic API 型に変換
        let mut body = serde_json::json!({
            "model": model,
            "max_tokens": max_tokens,
            "stream": false,
        });

        // Claude Code 識別 system block + ユーザー system prompt
        let mut system_blocks = vec![serde_json::json!({
            "type": "text",
            "text": "You are Claude Code, Anthropic's official CLI for Claude.",
            "cache_control": { "type": "ephemeral" }
        })];
        if let Some(sp) = system_prompt {
            system_blocks.push(serde_json::json!({
                "type": "text",
                "text": sp,
                "cache_control": { "type": "ephemeral" }
            }));
        }
        body["system"] = serde_json::Value::Array(system_blocks);

        // messages
        let msgs: Vec<serde_json::Value> = messages
            .iter()
            .map(|m| {
                let role = match m.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                };
                let content: Vec<serde_json::Value> = m
                    .content
                    .iter()
                    .map(|b| match b {
                        ContentBlock::Text { text } => {
                            serde_json::json!({"type": "text", "text": text})
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            serde_json::json!({"type": "tool_use", "id": id, "name": name, "input": input})
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => {
                            let text = match content {
                                ToolResultContent::Text(t) => t.clone(),
                                ToolResultContent::Blocks(blocks) => blocks
                                    .iter()
                                    .map(|b| match b {
                                        ToolResultBlock::Text { text } => text.as_str(),
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                            };
                            let mut v = serde_json::json!({
                                "type": "tool_result",
                                "tool_use_id": tool_use_id,
                                "content": text,
                            });
                            if let Some(true) = is_error {
                                v["is_error"] = serde_json::Value::Bool(true);
                            }
                            v
                        }
                    })
                    .collect();
                serde_json::json!({"role": role, "content": content})
            })
            .collect();
        body["messages"] = serde_json::Value::Array(msgs);

        // tools
        if let Some(t) = tools {
            let tool_defs: Vec<serde_json::Value> = t
                .iter()
                .map(|td| {
                    serde_json::json!({
                        "name": td.name,
                        "description": td.description,
                        "input_schema": td.input_schema,
                    })
                })
                .collect();
            body["tools"] = serde_json::Value::Array(tool_defs);
            body["tool_choice"] = serde_json::json!({"type": "auto"});
        }

        let resp = self.inner.inner_client().send_request(&body).await?;

        // claude-auth レスポンス → harness LlmResponse に変換
        let content: Vec<ContentBlock> = resp
            .content
            .into_iter()
            .map(|b| match b {
                claude_auth::ContentBlock::Text { text } => ContentBlock::Text { text },
                claude_auth::ContentBlock::ToolUse { id, name, input } => {
                    ContentBlock::ToolUse { id, name, input }
                }
                claude_auth::ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    let text = match &content {
                        serde_json::Value::String(s) => s.clone(),
                        serde_json::Value::Array(arr) => arr
                            .iter()
                            .filter_map(|v| v.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join("\n"),
                        other => other.to_string(),
                    };
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content: ToolResultContent::Text(text),
                        is_error,
                    }
                }
            })
            .collect();

        let stop_reason = resp.stop_reason.as_deref().map(|s| match s {
            "end_turn" => StopReason::EndTurn,
            "tool_use" => StopReason::ToolUse,
            "max_tokens" => StopReason::MaxTokens,
            "stop_sequence" => StopReason::StopSequence,
            _ => StopReason::EndTurn,
        });

        Ok(agent_harness::LlmResponse {
            content,
            stop_reason,
            usage: Usage {
                input_tokens: resp.usage.input_tokens,
                output_tokens: resp.usage.output_tokens,
                cache_creation_input_tokens: resp.usage.cache_creation_input_tokens.unwrap_or(0),
                cache_read_input_tokens: resp.usage.cache_read_input_tokens.unwrap_or(0),
            },
        })
    }
}

// ============================================================================
// MCP safeguard
// ============================================================================

fn check_mcp_safeguard(name: &str, input: &serde_json::Value) -> Option<String> {
    if name.ends_with("__bash_command") || name.ends_with("__execute_command") {
        if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
            return tool_impls::check_dangerous_command(cmd).map(|r| r.to_string());
        }
    }
    None
}
