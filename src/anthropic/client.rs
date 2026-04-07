//! Anthropic API クライアント — claude-auth crate のアダプター
//!
//! claude-auth の AnthropicClient をラップし、LlmClient trait を実装する。
//! 型変換: claude-auth の型 ↔ types.rs の型

use anyhow::Result;
use async_trait::async_trait;

use super::llm_client::{LlmClient, OnToolUseCallback};
use super::types::*;

/// claude-auth crate の AnthropicClient をラップするアダプター
pub struct AnthropicClient {
    inner: claude_auth::AnthropicClient,
}

impl AnthropicClient {
    /// 内部の claude-auth クライアントへのアクセス
    pub fn inner_client(&self) -> &claude_auth::AnthropicClient {
        &self.inner
    }

    /// API キー認証
    pub fn new(api_key: String) -> Self {
        let model = std::env::var("ANTHROPIC_MODEL")
            .unwrap_or_else(|_| "claude-sonnet-4-20250514".to_string());
        Self {
            inner: claude_auth::AnthropicClient::with_api_key(api_key, model),
        }
    }

    /// OAuth 認証（Claude Code Max プラン）
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            inner: claude_auth::AnthropicClient::from_env()?,
        })
    }
}

#[async_trait]
impl LlmClient for AnthropicClient {
    async fn send_streaming(
        &self,
        request: MessagesRequest,
        _on_tool_use: Option<OnToolUseCallback>,
    ) -> Result<MessagesResponse> {
        let mut body = convert_request(&request);
        body["stream"] = serde_json::Value::Bool(false);
        let resp = self.inner.send_request(&body).await?;
        Ok(convert_response(resp))
    }
}

fn convert_request(req: &MessagesRequest) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": req.model,
        "max_tokens": req.max_tokens,
        "stream": req.stream,
    });

    // OAuth 認証時は Claude Code 識別の system block が必要（rate limit tier に影響）
    let mut system_blocks = vec![serde_json::json!({
        "type": "text",
        "text": "You are Claude Code, Anthropic's official CLI for Claude.",
        "cache_control": { "type": "ephemeral" }
    })];
    if let Some(ref system) = req.system {
        for s in system {
            let mut block = serde_json::json!({
                "type": s.block_type,
                "text": s.text,
            });
            if let Some(ref cc) = s.cache_control {
                block["cache_control"] = serde_json::json!({
                    "type": cc.cache_type,
                });
            }
            system_blocks.push(block);
        }
    }
    body["system"] = serde_json::Value::Array(system_blocks);

    let messages: Vec<serde_json::Value> = req
        .messages
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
    body["messages"] = serde_json::Value::Array(messages);

    if let Some(ref tools) = req.tools {
        let tool_defs: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })
            })
            .collect();
        body["tools"] = serde_json::Value::Array(tool_defs);
    }

    if let Some(ref tc) = req.tool_choice {
        match tc {
            ToolChoice::Auto => {
                body["tool_choice"] = serde_json::json!({"type": "auto"});
            }
            ToolChoice::None => {
                body["tool_choice"] = serde_json::json!({"type": "none"});
            }
        }
    }

    body
}

fn convert_response(resp: claude_auth::MessagesResponse) -> MessagesResponse {
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

    MessagesResponse {
        id: resp.id,
        model: resp.model,
        role: Role::Assistant,
        content,
        stop_reason,
        usage: ApiUsage {
            input_tokens: resp.usage.input_tokens,
            output_tokens: resp.usage.output_tokens,
            cache_creation_input_tokens: resp.usage.cache_creation_input_tokens,
            cache_read_input_tokens: resp.usage.cache_read_input_tokens,
        },
    }
}
